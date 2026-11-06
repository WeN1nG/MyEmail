//! 正文后台补全与按需下载。
//!
//! - 队列来源：`body_path IS NULL AND uid IS NOT NULL`，按 `(folder_id, uid)` 升序。
//! - 每封使用 `BODY.PEEK[]`，解析后写 `bodies/` + `raw/`，同事务写 FTS 与 snippet。
//! - 断网、手动同步或关闭开关即暂停；进度持久化在 `body_path`。

use std::sync::atomic::Ordering;

use me_core::model::{MessageBody, make_snippet};
use me_core::port::{BodyJob, ImapSession, ImapSessionFactory};
use me_core::{CoreError, ErrorScope, Event, FlagOp, Flags, MessageId};
use tokio_util::sync::CancellationToken;

use crate::SyncService;

impl<S, F, M> SyncService<S, F, M>
where
    S: me_core::port::StorePort + Clone,
    F: ImapSessionFactory + 'static,
    M: me_core::port::SmtpSender + 'static,
{
    pub(crate) async fn backfill(&self, account_id: me_core::AccountId, token: CancellationToken) {
        if !self.inner.backfill.load(Ordering::Relaxed) {
            return;
        }
        let mut disk_warned = false;
        if self.disk_is_low(&mut disk_warned).await {
            return;
        }
        let account = match self.account(account_id).await {
            Ok(account) => account,
            Err(_) => return,
        };
        let secret = match self.credential(&account).await {
            Ok(secret) => secret,
            Err(err) => {
                tracing::warn!(account = account_id.0, error = %err.user_message(), "backfill skipped: no credential");
                return;
            }
        };
        let _guard = match self.inner.governor.acquire(account_id).await {
            Ok(guard) => guard,
            Err(_) => return,
        };
        let mut session = match self.inner.factory.connect(&account, &secret).await {
            Ok(session) => session,
            Err(err) => {
                tracing::warn!(account = account_id.0, error = %err.user_message(), "backfill connect failed");
                return;
            }
        };

        let total = self
            .db(move |s| s.pending_body_count(account_id))
            .await
            .unwrap_or(0);
        let mut indexed = 0u32;
        let mut throttle = crate::events::ProgressThrottle::default();
        let paths = self.inner.paths.clone();
        // 本轮已失败的邮件不再重试，避免服务器返回空结果时原地打转。
        let mut failed: std::collections::HashSet<MessageId> = std::collections::HashSet::new();

        'outer: loop {
            if token.is_cancelled() || !self.inner.backfill.load(Ordering::Relaxed) {
                break;
            }
            // 每页检查一次剩余空间：磁盘写满时立即停止并提示。
            if self.disk_is_low(&mut disk_warned).await {
                break;
            }
            let jobs = match self.db(move |s| s.body_jobs(account_id, 200)).await {
                Ok(jobs) => jobs,
                Err(err) => {
                    tracing::warn!(account = account_id.0, error = %err.user_message(), "backfill queue failed");
                    break;
                }
            };
            let jobs: Vec<BodyJob> = jobs
                .into_iter()
                .filter(|job| !failed.contains(&job.message))
                .collect();
            if jobs.is_empty() {
                break;
            }

            let mut index = 0usize;
            while index < jobs.len() {
                if token.is_cancelled() || !self.inner.backfill.load(Ordering::Relaxed) {
                    break 'outer;
                }
                let folder_id = jobs[index].folder;
                let folder = self
                    .db(move |s| s.get_folder(folder_id))
                    .await
                    .ok()
                    .flatten();
                let Some(folder) = folder else {
                    while index < jobs.len() && jobs[index].folder == folder_id {
                        failed.insert(jobs[index].message);
                        index += 1;
                    }
                    continue;
                };
                if folder.deleted_on_server {
                    while index < jobs.len() && jobs[index].folder == folder_id {
                        failed.insert(jobs[index].message);
                        index += 1;
                    }
                    continue;
                }
                if let Err(err) = session.select(&folder.name).await {
                    tracing::warn!(folder = %folder.name, error = %err.user_message(), "backfill select failed");
                    while index < jobs.len() && jobs[index].folder == folder_id {
                        failed.insert(jobs[index].message);
                        index += 1;
                    }
                    continue;
                }

                while index < jobs.len() && jobs[index].folder == folder_id {
                    if token.is_cancelled() || !self.inner.backfill.load(Ordering::Relaxed) {
                        break 'outer;
                    }
                    let job = jobs[index];
                    match self.fetch_body_job(&mut session, job, &paths).await {
                        Ok(()) => {
                            indexed = indexed.saturating_add(1);
                            if throttle.tick() {
                                self.emit(Event::IndexProgress {
                                    account: account_id,
                                    indexed,
                                    total,
                                })
                                .await;
                            }
                        }
                        Err(err) if err.is_cancelled() => break 'outer,
                        Err(err) => {
                            failed.insert(job.message);
                            tracing::warn!(
                                message = job.message.0,
                                uid = job.uid,
                                error = %err.user_message(),
                                "body fetch failed"
                            );
                        }
                    }
                    index += 1;
                }
            }
        }

        if throttle.flush_if_due(true) {
            self.emit(Event::IndexProgress {
                account: account_id,
                indexed,
                total,
            })
            .await;
        }
        let _ = session.logout().await;
    }

    /// 剩余空间不足时提示一次并返回 `true`。
    async fn disk_is_low(&self, warned: &mut bool) -> bool {
        let available = self
            .db(|store| store.available_bytes())
            .await
            .ok()
            .flatten();
        match available {
            Some(bytes) if bytes < me_core::MIN_FREE_BYTES => {
                if !*warned {
                    *warned = true;
                    self.emit(Event::DiskSpaceLow {
                        available_bytes: Some(bytes),
                    })
                    .await;
                }
                true
            }
            _ => false,
        }
    }

    async fn fetch_body_job(
        &self,
        session: &mut F::Session,
        job: BodyJob,
        paths: &me_store::cache::CachePaths,
    ) -> Result<(), CoreError> {
        let raw = session.fetch_body(job.uid).await?;
        let parsed = me_proto::mime::parse_body(&raw)?;
        if parsed.raw_len > me_core::MAX_PARSE_BYTES {
            return Err(CoreError::limit(me_core::LimitKind::ParseSize));
        }

        // 联系人累积：**在正文解析之前**把收发件人取出来。
        // 解析失败不阻断补全——联系人是**附带行为**，
        // 一封畸形邮件不该让这封邮件的正文永远补不上。
        let contacts = me_proto::mime::parse_addresses(&raw)
            .ok()
            .map(|(from, to, cc)| collect_contacts(from, to, cc))
            .unwrap_or_default();

        let text = parsed.text;
        let attachments = parsed.attachments;
        let paths = paths.clone();
        let message = job.message;
        let text_for_file = text.clone();
        let raw_for_file = raw;
        let (body_path, raw_cached) = tokio::task::spawn_blocking(move || {
            let body_path = me_store::cache::write_body(&paths, message, &text_for_file)?;
            let raw_written = me_store::cache::write_raw_gz(&paths, message, &raw_for_file)?;
            Ok::<_, CoreError>((body_path, raw_written))
        })
        .await
        .map_err(|join| CoreError::storage(format!("正文写入失败：{join}")))??;

        let snippet = make_snippet(&text);
        let body_path_str = body_path.to_string_lossy().to_string();
        let text_for_db = text.clone();
        self.db(move |s| s.set_body_cached(message, &body_path_str, &text_for_db, Some(&snippet)))
            .await?;

        let attachments_for_db = attachments.clone();
        self.db(move |s| s.upsert_attachments(message, &attachments_for_db))
            .await?;

        // 联系人累积：**放在正文落库之后**——
        // 它是额外的一笔写，放在后面不会拖慢「正文能搜到」这条主线。
        // 失败也**不报错**——联系人是附带行为。
        if !contacts.is_empty() {
            let _ = self.db(move |s| s.upsert_contacts(&contacts)).await;
        }

        let _ = raw_cached;
        Ok(())
    }

    pub(crate) async fn open_message(&self, id: MessageId) {
        let message = match self.db(move |s| s.get_message(id)).await {
            Ok(Some(message)) => message,
            Ok(None) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Message(id),
                    message: "邮件不存在".into(),
                })
                .await;
                return;
            }
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Message(id),
                    message: err.user_message(),
                })
                .await;
                return;
            }
        };

        // 打开即已读：本地先写，服务器异步回写。
        if !message.flags.is_seen() {
            let changed = self
                .db(move |s| s.apply_local_flags(&[id], FlagOp::Set(Flags::SEEN)))
                .await;
            if let Ok(items) = changed
                && !items.is_empty()
            {
                self.emit(Event::FlagsChanged { items }).await;
            }
            let account = message.account;
            let service = self.clone();
            let token = CancellationToken::new();
            tokio::spawn(async move { service.flush_flags(account, token).await });
        }

        let paths = self.inner.paths.clone();
        if message.body_cached {
            let id = message.id;
            let read = tokio::task::spawn_blocking({
                let paths = paths.clone();
                move || me_store::cache::read_body(&paths, id)
            })
            .await
            .map_err(|join| CoreError::storage(format!("正文读取失败：{join}")));
            if let Ok(Ok(Some(text))) = read {
                let attachments = self
                    .db(move |s| s.list_attachments(id))
                    .await
                    .unwrap_or_default();
                let raw_cached = paths.raw_path(id).is_file();
                // 已缓存正文时从原始邮件恢复 HTML，供阅读页富文本渲染。
                let (html, inline_images) = if raw_cached {
                    let paths_for_html = paths.clone();
                    tokio::task::spawn_blocking(move || {
                        me_store::cache::read_raw_gz(&paths_for_html, id)
                            .ok()
                            .flatten()
                            .and_then(|raw| me_proto::mime::parse_body(&raw).ok())
                            .map(|parsed| (parsed.html, parsed.inline_images))
                    })
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or((None, Vec::new()))
                } else {
                    (None, Vec::new())
                };
                self.emit(Event::MessageBodyLoaded {
                    id,
                    body: MessageBody {
                        message: id,
                        text,
                        html,
                        inline_images,
                        attachments,
                        raw_cached,
                    },
                })
                .await;
                return;
            }
            // 文件缺失：继续走下载路径（覆盖 FTS 与 body_path）。
        }

        let Some(uid) = message.uid else {
            self.emit(Event::Error {
                scope: ErrorScope::Message(id),
                message: "该邮件的服务器 UID 尚未确认，暂时无法下载正文".into(),
            })
            .await;
            return;
        };

        match self.fetch_and_cache_body(&message, uid, &paths).await {
            Ok(()) => {}
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Message(id),
                    message: format!("正文下载失败：{}", err.user_message()),
                })
                .await;
            }
        }
    }

    async fn fetch_and_cache_body(
        &self,
        message: &me_core::MessageSummary,
        uid: u32,
        paths: &me_store::cache::CachePaths,
    ) -> Result<(), CoreError> {
        let account = self.account(message.account).await?;
        let message_id = message.id;
        let folder_id = message.folder;
        let secret = self.credential(&account).await?;
        let folder = self
            .db(move |s| s.get_folder(folder_id))
            .await?
            .ok_or_else(|| CoreError::storage("文件夹不存在"))?;

        let _guard = self.inner.governor.acquire(account.id).await?;
        let mut session = self.inner.factory.connect(&account, &secret).await?;
        let result = async {
            session.select(&folder.name).await?;
            let raw = session.fetch_body(uid).await?;
            let parsed = me_proto::mime::parse_body(&raw)?;
            let text = parsed.text;
            let html = parsed.html;
            let inline_images = parsed.inline_images;
            let attachments = parsed.attachments;
            let paths = paths.clone();
            let text_for_file = text.clone();
            let body_path = tokio::task::spawn_blocking(move || {
                let body_path = me_store::cache::write_body(&paths, message_id, &text_for_file)?;
                let _ = me_store::cache::write_raw_gz(&paths, message_id, &raw)?;
                Ok::<_, CoreError>(body_path)
            })
            .await
            .map_err(|join| CoreError::storage(format!("正文写入失败：{join}")))??;

            let snippet = make_snippet(&text);
            let body_path_str = body_path.to_string_lossy().to_string();
            let text_for_db = text.clone();
            self.db(move |s| {
                s.set_body_cached(message_id, &body_path_str, &text_for_db, Some(&snippet))
            })
            .await?;
            let attachments_for_db = attachments.clone();
            self.db(move |s| s.upsert_attachments(message_id, &attachments_for_db))
                .await?;

            self.emit(Event::MessageBodyLoaded {
                id: message_id,
                body: MessageBody {
                    message: message_id,
                    text,
                    html,
                    inline_images,
                    attachments,
                    raw_cached: true,
                },
            })
            .await;
            Ok(())
        }
        .await;
        let _ = session.logout().await;
        result
    }

    pub(crate) async fn download_attachment(&self, message_id: MessageId, part_index: u32) {
        let attachment = match self
            .db(move |s| s.get_attachment(message_id, part_index))
            .await
        {
            Ok(Some(attachment)) => attachment,
            Ok(None) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Message(message_id),
                    message: "附件不存在".into(),
                })
                .await;
                return;
            }
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Message(message_id),
                    message: err.user_message(),
                })
                .await;
                return;
            }
        };

        if let Some(path) = &attachment.local_path
            && path.is_file()
        {
            self.emit(Event::AttachmentDownloaded {
                message: message_id,
                part_index,
                local_path: path.clone(),
            })
            .await;
            return;
        }

        let message = match self.db(move |s| s.get_message(message_id)).await {
            Ok(Some(message)) => message,
            _ => return,
        };
        let Some(uid) = message.uid else {
            self.emit(Event::Error {
                scope: ErrorScope::Message(message_id),
                message: "该邮件的服务器 UID 尚未确认，暂时无法下载附件".into(),
            })
            .await;
            return;
        };

        match self.fetch_part_and_cache(&message, uid, part_index).await {
            Ok(path) => {
                self.emit(Event::AttachmentDownloaded {
                    message: message_id,
                    part_index,
                    local_path: path,
                })
                .await;
            }
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Message(message_id),
                    message: format!("附件下载失败：{}", err.user_message()),
                })
                .await;
            }
        }
    }

    async fn fetch_part_and_cache(
        &self,
        message: &me_core::MessageSummary,
        uid: u32,
        part_index: u32,
    ) -> Result<std::path::PathBuf, CoreError> {
        let account = self.account(message.account).await?;
        let message_id = message.id;
        let folder_id = message.folder;
        let secret = self.credential(&account).await?;
        let folder = self
            .db(move |s| s.get_folder(folder_id))
            .await?
            .ok_or_else(|| CoreError::storage("文件夹不存在"))?;
        let attachment = self
            .db(move |s| s.list_attachments(message_id))
            .await?
            .into_iter()
            .find(|a| a.part_index == part_index);

        let _guard = self.inner.governor.acquire(account.id).await?;
        let mut session = self.inner.factory.connect(&account, &secret).await?;
        let result = async {
            session.select(&folder.name).await?;
            let bytes = session.fetch_part(uid, &part_index.to_string()).await?;
            if bytes.is_empty() {
                return Err(CoreError::parse("服务器返回空附件"));
            }
            // 与附件目录迁移/缓存清理串行，确保写入当前生效的目录。
            let _gate = self.inner.attach_gate.lock().await;
            let attach_dir = self.attachment_dir();
            let part = part_index;
            let filename = attachment.as_ref().and_then(|a| a.filename.clone());
            let path = tokio::task::spawn_blocking(move || {
                me_store::cache::write_attachment_in(
                    &attach_dir,
                    message_id,
                    part,
                    filename.as_deref(),
                    &bytes,
                )
            })
            .await
            .map_err(|join| CoreError::storage(format!("附件写入失败：{join}")))??;

            let path_for_db = path.clone();
            self.db(move |s| s.set_attachment_path(message_id, part_index, &path_for_db))
                .await?;
            Ok(path)
        }
        .await;
        let _ = session.logout().await;
        result
    }
}

/// 从一封邮件的收发件人构造待累积的联系人列表。
///
/// **去重在这一层做**（同一个地址在一封邮件里出现多次只留一条），
/// 存储层再按 `normalized` 跨邮件去重。
fn collect_contacts(
    from: Option<me_core::Address>,
    to: Vec<me_core::Address>,
    cc: Vec<me_core::Address>,
) -> Vec<me_core::Contact> {
    let now = me_core::now_unix();
    let mut out: Vec<me_core::Contact> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut push = |address: me_core::Address, out: &mut Vec<me_core::Contact>| {
        let normalized = me_core::Contact::normalize(&address.addr);
        if normalized.is_empty() || !seen.insert(normalized.clone()) {
            return;
        }
        out.push(me_core::Contact {
            id: me_core::ContactId(0),
            addr: address.addr.trim().to_string(),
            normalized,
            display_name: address.name,
            last_seen: now,
            seen_count: 1,
        });
    };

    if let Some(from) = from {
        push(from, &mut out);
    }
    for address in to {
        push(address, &mut out);
    }
    for address in cc {
        push(address, &mut out);
    }
    out
}
