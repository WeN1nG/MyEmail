//! 同步主流程：文件夹合并 → 增量拉取 → 标志回拉 → 待同步标志/操作执行。

use std::collections::HashSet;

use me_core::model::{FolderRole, NewMessage, effective_date};
use me_core::port::{
    FlagStoreMode, FolderInfo, ImapSession, ImapSessionFactory, PendingOp, PendingOpKind, UidRange,
};
use me_core::{Account, AccountId, CoreError, ErrorScope, Event, Flags, Folder, FolderId};
use tokio_util::sync::CancellationToken;

use crate::tasks::BACKOFF_SECS;
use crate::{FLAG_BATCH, HEADER_BATCH, SyncService};

impl<S, F, M> SyncService<S, F, M>
where
    S: me_core::port::StorePort + Clone,
    F: ImapSessionFactory + 'static,
    M: me_core::port::SmtpSender + 'static,
{
    pub(crate) async fn sync_account(
        &self,
        account_id: AccountId,
        only_folder: Option<FolderId>,
        token: CancellationToken,
    ) {
        let account = match self.account(account_id).await {
            Ok(account) => account,
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Account(account_id),
                    message: err.user_message(),
                })
                .await;
                return;
            }
        };

        // 账户串行同步闸门：多账户不同时跑同步。
        let _gate = tokio::select! {
            gate = self.inner.sync_gate.lock() => gate,
            _ = token.cancelled() => {
                self.status(account_id, me_core::ConnectionState::Disconnected)
                    .await;
                return;
            }
        };

        self.status(account_id, me_core::ConnectionState::Connecting)
            .await;
        let mut attempt = 0usize;
        loop {
            if token.is_cancelled() {
                self.status(account_id, me_core::ConnectionState::Disconnected)
                    .await;
                return;
            }
            match self.sync_once(&account, only_folder, &token).await {
                Ok(()) => {
                    self.status(account_id, me_core::ConnectionState::Online)
                        .await;
                    break;
                }
                Err(err) if err.is_cancelled() => {
                    self.status(account_id, me_core::ConnectionState::Disconnected)
                        .await;
                    return;
                }
                Err(err) if err.is_retryable() && attempt < BACKOFF_SECS.len() => {
                    let delay = BACKOFF_SECS[attempt.min(BACKOFF_SECS.len() - 1)];
                    attempt += 1;
                    self.status(
                        account_id,
                        me_core::ConnectionState::Backoff {
                            retry_at: me_core::now_unix() + delay as i64,
                        },
                    )
                    .await;
                    tracing::warn!(
                        account = account_id.0,
                        delay,
                        error = %err.user_message(),
                        "sync failed, backing off"
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(delay)) => {}
                        _ = token.cancelled() => {
                            self.status(account_id, me_core::ConnectionState::Disconnected).await;
                            return;
                        }
                    }
                }
                Err(err) => {
                    self.status(
                        account_id,
                        me_core::ConnectionState::Error {
                            detail: err.user_message(),
                        },
                    )
                    .await;
                    self.emit(Event::Error {
                        scope: ErrorScope::Account(account_id),
                        message: err.user_message(),
                    })
                    .await;
                    return;
                }
            }
        }

        // 草稿双向同步：完整同步后执行；仅同步单个非 Drafts 文件夹时跳过。
        if !token.is_cancelled() {
            let should_sync = match only_folder {
                None => true,
                Some(folder_id) => self
                    .db(move |s| s.get_folder(folder_id))
                    .await
                    .ok()
                    .flatten()
                    .map(|folder| folder.role == FolderRole::Drafts)
                    .unwrap_or(false),
            };
            if should_sync && let Err(err) = self.sync_drafts(account_id, &token).await {
                // 草稿失败不得让整账户邮件同步进入退避。
                tracing::warn!(
                    account = account_id.0,
                    error = %err.user_message(),
                    "draft sync failed"
                );
                self.emit(Event::Error {
                    scope: ErrorScope::Account(account_id),
                    message: format!("草稿同步失败：{}", err.user_message()),
                })
                .await;
            }
        }
        if self
            .inner
            .backfill
            .load(std::sync::atomic::Ordering::Relaxed)
            && !token.is_cancelled()
            && only_folder.is_none()
        {
            self.start_backfill(account_id);
        }
    }

    async fn sync_once(
        &self,
        account: &Account,
        only_folder: Option<FolderId>,
        token: &CancellationToken,
    ) -> Result<(), CoreError> {
        let secret = self.credential(account).await?;
        let _guard = self.inner.governor.acquire(account.id).await?;
        let mut session = self.inner.factory.connect(account, &secret).await?;

        let result = self
            .sync_session(account, &mut session, only_folder, token)
            .await;
        let _ = session.logout().await;
        result
    }

    async fn sync_session(
        &self,
        account: &Account,
        session: &mut F::Session,
        only_folder: Option<FolderId>,
        token: &CancellationToken,
    ) -> Result<(), CoreError> {
        let folders = self.merge_folders(account.id, session).await?;
        self.emit(Event::FoldersUpdated {
            account: account.id,
            folders: folders.clone(),
        })
        .await;

        let targets: Vec<Folder> = folders
            .iter()
            .filter(|f| !f.deleted_on_server)
            .filter(|f| f.role != FolderRole::Drafts)
            .filter(|f| only_folder.map(|id| f.id == id).unwrap_or(true))
            .cloned()
            .collect();

        for folder in &targets {
            if token.is_cancelled() {
                return Ok(());
            }
            self.sync_folder_headers(account.id, session, folder, token)
                .await?;
        }

        // 标志回拉：所有未被服务器删除的文件夹（即使本次只同步单个文件夹也刷新其标志）。
        let flag_targets: Vec<Folder> = if only_folder.is_some() {
            targets.clone()
        } else {
            folders
                .iter()
                .filter(|f| !f.deleted_on_server)
                .filter(|f| f.role != FolderRole::Drafts)
                .cloned()
                .collect()
        };
        for folder in &flag_targets {
            if token.is_cancelled() {
                return Ok(());
            }
            self.pull_flags(session, folder).await?;
        }

        // 本地待同步标志优先，随后覆盖服务器。
        self.flush_flags_with_session(account, session).await;

        // 待同步操作（移动 / 永久删除）。
        self.execute_ops_with_session(account, session).await;

        Ok(())
    }

    async fn sync_folder_headers(
        &self,
        account_id: AccountId,
        session: &mut F::Session,
        folder: &Folder,
        token: &CancellationToken,
    ) -> Result<(), CoreError> {
        let selected = session.select(&folder.name).await?;

        let mut last_uid = folder.last_uid;
        let folder_id = folder.id;
        if folder.uid_validity != 0 && folder.uid_validity != selected.uid_validity {
            // UIDVALIDITY 变化：清空本地邮件与 FTS，全量重同步。
            self.db(move |s| s.clear_folder(folder_id)).await?;
            // UID 空间已重建，旧的移动/删除操作不再可执行。
            let _ = self.db(move |s| s.cleanup_ops_for_folder(folder_id)).await;
            let uid_validity = selected.uid_validity;
            self.db(move |s| s.set_folder_uid_state(folder_id, uid_validity, 0))
                .await?;
            last_uid = 0;
        } else if folder.uid_validity == 0 {
            let uid_validity = selected.uid_validity;
            let previous = folder.last_uid;
            self.db(move |s| s.set_folder_uid_state(folder_id, uid_validity, previous))
                .await?;
        }

        if selected.uid_next <= last_uid.saturating_add(1) {
            return Ok(());
        }

        let initial_last_uid = last_uid;
        let total = selected.uid_next.saturating_sub(last_uid.saturating_add(1));
        self.emit(Event::SyncProgress {
            folder: folder.id,
            done: 0,
            total: Some(total),
        })
        .await;

        let mut start = last_uid.saturating_add(1);
        while start < selected.uid_next {
            if token.is_cancelled() {
                return Ok(());
            }
            let end = start
                .saturating_add(HEADER_BATCH)
                .saturating_sub(1)
                .min(selected.uid_next.saturating_sub(1));
            let fetched = session.fetch_headers(UidRange::bounded(start, end)).await?;

            let mut batch_max = last_uid;
            let mut new_messages: Vec<NewMessage> = Vec::new();
            for fetch in fetched {
                if fetch.uid <= last_uid {
                    continue;
                }
                batch_max = batch_max.max(fetch.uid);
                match me_proto::mime::parse_headers(&fetch.header) {
                    Ok(parsed) => {
                        let (date, date_source) = effective_date(parsed.date, fetch.internal_date);
                        new_messages.push(NewMessage {
                            uid: Some(fetch.uid),
                            message_id: parsed.message_id,
                            subject: parsed.subject,
                            from: parsed.from,
                            to: parsed.to,
                            cc: parsed.cc,
                            date,
                            date_source,
                            size: fetch.size,
                            flags: fetch.flags,
                            has_attachment: fetch.has_attachment,
                            in_reply_to: parsed.in_reply_to,
                            references: parsed.references.unwrap_or_default(),
                        });
                    }
                    Err(err) => {
                        tracing::warn!(
                            folder = folder.id.0,
                            uid = fetch.uid,
                            error = %err.user_message(),
                            "skipping unparsable message header"
                        );
                    }
                }
            }

            if batch_max > last_uid {
                let folder_id = folder.id;
                let batch = new_messages.clone();
                let new_last = batch_max;
                let inserted = self
                    .db(move |s| s.insert_messages(account_id, folder_id, &batch, new_last))
                    .await?;
                if !inserted.is_empty() {
                    self.emit(Event::MessagesAppended {
                        folder: folder.id,
                        messages: inserted,
                    })
                    .await;
                }
                last_uid = batch_max;
                self.emit(Event::SyncProgress {
                    folder: folder.id,
                    done: last_uid.saturating_sub(initial_last_uid),
                    total: Some(total),
                })
                .await;
            }

            if end == u32::MAX {
                break;
            }
            start = end.saturating_add(1);
        }

        Ok(())
    }

    async fn pull_flags(&self, session: &mut F::Session, folder: &Folder) -> Result<(), CoreError> {
        let selected = session.select(&folder.name).await?;
        if selected.uid_next <= 1 {
            return Ok(());
        }
        let max_uid = selected.uid_next.saturating_sub(1);
        let mut start = 1;
        while start <= max_uid {
            let end = start
                .saturating_add(FLAG_BATCH)
                .saturating_sub(1)
                .min(max_uid);
            let items = session.fetch_flags(UidRange::bounded(start, end)).await?;
            if !items.is_empty() {
                let folder_id = folder.id;
                let items_for_db = items.clone();
                let changed = self
                    .db(move |s| s.apply_server_flags(folder_id, &items_for_db))
                    .await?;
                if !changed.is_empty() {
                    self.emit(Event::FlagsChanged { items: changed }).await;
                }
            }
            if end == u32::MAX {
                break;
            }
            start = end.saturating_add(1);
        }
        Ok(())
    }

    async fn merge_folders(
        &self,
        account_id: AccountId,
        session: &mut F::Session,
    ) -> Result<Vec<Folder>, CoreError> {
        let infos = session.list_folders().await?;
        let local = self.db(move |s| s.list_folders(account_id)).await?;

        let local_names: HashSet<String> = local.iter().map(|f| f.name.clone()).collect();
        let server_names: HashSet<String> = infos.iter().map(|i| i.name.clone()).collect();
        let missing: Vec<&Folder> = local
            .iter()
            .filter(|f| !server_names.contains(&f.name))
            .collect();
        let new_infos: Vec<&FolderInfo> = infos
            .iter()
            .filter(|i| !local_names.contains(&i.name))
            .collect();

        // 改名识别：恰好一旧一新且角色一致时，只 UPDATE name，不删建。
        if missing.len() == 1 && new_infos.len() == 1 {
            let old = missing[0];
            let new = new_infos[0];
            let new_role = FolderRole::from_special_use(&new.attributes, &new.name);
            if old.role != FolderRole::Other && old.role == new_role {
                let id = old.id;
                let name = new.name.clone();
                self.db(move |s| s.rename_folder(id, &name)).await?;
                tracing::info!(folder = old.id.0, old = %old.name, new = %new.name, "folder renamed on server");
            }
        }

        for info in &infos {
            let info = info.clone();
            let _ = self.db(move |s| s.upsert_folder(account_id, &info)).await?;
        }

        // 仍在服务器上不存在的本地文件夹：标记待确认，不立即删数据。
        let local_after = self.db(move |s| s.list_folders(account_id)).await?;
        for folder in local_after {
            if !server_names.contains(&folder.name) && !folder.deleted_on_server {
                let id = folder.id;
                self.db(move |s| s.set_folder_deleted(id, true)).await?;
            }
        }

        self.db(move |s| s.list_folders(account_id)).await
    }

    // -----------------------------------------------------------------------
    // 待同步标志
    // -----------------------------------------------------------------------

    pub(crate) async fn flush_flags(&self, account_id: AccountId, token: CancellationToken) {
        if token.is_cancelled() {
            return;
        }
        let account = match self.account(account_id).await {
            Ok(account) => account,
            Err(_) => return,
        };
        let secret = match self.credential(&account).await {
            Ok(secret) => secret,
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Account(account_id),
                    message: err.user_message(),
                })
                .await;
                return;
            }
        };
        let guard = match self.inner.governor.acquire(account_id).await {
            Ok(guard) => guard,
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Account(account_id),
                    message: err.user_message(),
                })
                .await;
                return;
            }
        };
        let mut session = match self.inner.factory.connect(&account, &secret).await {
            Ok(session) => session,
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Account(account_id),
                    message: err.user_message(),
                })
                .await;
                return;
            }
        };
        self.flush_flags_with_session(&account, &mut session).await;
        let _ = session.logout().await;
        drop(guard);
    }

    async fn flush_flags_with_session(&self, account: &Account, session: &mut F::Session) {
        let pending = match self.db(|s| s.pending_flags(500)).await {
            Ok(pending) => pending,
            Err(_) => return,
        };
        let mut current_folder: Option<String> = None;
        for (message_id, target, _attempts) in pending {
            let message = match self.db(move |s| s.get_message(message_id)).await {
                Ok(Some(message)) => message,
                _ => continue,
            };
            let Some(uid) = message.uid else { continue };
            if message.account != account.id {
                continue;
            }
            let folder = match self.db(move |s| s.get_folder(message.folder)).await {
                Ok(Some(folder)) => folder,
                _ => continue,
            };
            if current_folder.as_deref() != Some(folder.name.as_str()) {
                if let Err(err) = session.select(&folder.name).await {
                    tracing::warn!(folder = %folder.name, error = %err.user_message(), "select failed while flushing flags");
                    continue;
                }
                current_folder = Some(folder.name.clone());
            }
            match session
                .store_flags(&[uid], target, FlagStoreMode::Replace)
                .await
            {
                Ok(()) => {
                    let _ = self.db(move |s| s.clear_pending_flags(message_id)).await;
                }
                Err(err) => {
                    let message = err.user_message();
                    let message_for_db = message.clone();
                    let _ = self
                        .db(move |s| s.bump_pending_flags_attempt(message_id, &message_for_db))
                        .await;
                    tracing::warn!(message = message_id.0, error = %message, "failed to push flags");
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // 待同步操作（移动 / 永久删除）
    // -----------------------------------------------------------------------

    pub(crate) async fn execute_pending_ops(
        &self,
        account_id: AccountId,
        token: CancellationToken,
    ) {
        if token.is_cancelled() {
            return;
        }
        let account = match self.account(account_id).await {
            Ok(account) => account,
            Err(_) => return,
        };
        let secret = match self.credential(&account).await {
            Ok(secret) => secret,
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Account(account_id),
                    message: err.user_message(),
                })
                .await;
                return;
            }
        };
        let guard = match self.inner.governor.acquire(account_id).await {
            Ok(guard) => guard,
            Err(_) => return,
        };
        let mut session = match self.inner.factory.connect(&account, &secret).await {
            Ok(session) => session,
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Account(account_id),
                    message: err.user_message(),
                })
                .await;
                return;
            }
        };
        self.execute_ops_with_session(&account, &mut session).await;
        let _ = session.logout().await;
        drop(guard);
    }

    async fn execute_ops_with_session(&self, account: &Account, session: &mut F::Session) {
        let account_id = account.id;
        let ops = match self.db(move |s| s.pending_ops(account_id, 100)).await {
            Ok(ops) => ops,
            Err(_) => return,
        };
        for op in ops {
            let result = self.execute_one_op(session, &op).await;
            match result {
                Ok(()) => {
                    let id = op.id;
                    let _ = self.db(move |s| s.complete_op(id)).await;
                }
                Err(err) => {
                    let message = err.user_message();
                    let message_for_db = message.clone();
                    let id = op.id;
                    let _ = self
                        .db(move |s| s.bump_op_attempt(id, &message_for_db))
                        .await;
                    tracing::warn!(op = op.id, error = %message, "pending op failed");
                }
            }
        }
    }

    async fn execute_one_op(
        &self,
        session: &mut F::Session,
        op: &PendingOp,
    ) -> Result<(), CoreError> {
        let Some(source_uid) = op.source_uid else {
            return Err(CoreError::protocol(None::<String>, "待同步操作缺少源 UID"));
        };
        let source_name = match op.source_folder_name.clone() {
            Some(name) if !name.is_empty() => name,
            _ => {
                let Some(folder_id) = op.source_folder else {
                    return Err(CoreError::protocol(
                        None::<String>,
                        "待同步操作缺少源文件夹",
                    ));
                };
                self.db(move |s| s.get_folder(folder_id))
                    .await?
                    .map(|f| f.name)
                    .ok_or_else(|| CoreError::protocol(None::<String>, "源文件夹已不存在"))?
            }
        };

        match op.kind {
            PendingOpKind::MoveToTrash => {
                let Some(target_id) = op.target_folder else {
                    return Err(CoreError::protocol(
                        None::<String>,
                        "待同步操作缺少目标文件夹",
                    ));
                };
                let target = self
                    .db(move |s| s.get_folder(target_id))
                    .await?
                    .ok_or_else(|| CoreError::protocol(None::<String>, "目标文件夹已不存在"))?;

                session.select(&source_name).await?;
                if session.capabilities().move_supported {
                    let mapping = session.move_to(&[source_uid], &target.name).await?;
                    self.confirm_move(session, op, &target, source_uid, mapping)
                        .await;
                } else {
                    let mapping = session.copy_to(&[source_uid], &target.name).await?;
                    session
                        .store_flags(&[source_uid], Flags::DELETED, FlagStoreMode::Add)
                        .await?;
                    if session.capabilities().uidplus {
                        session.uid_expunge(&[source_uid]).await?;
                    }
                    self.confirm_move(session, op, &target, source_uid, mapping)
                        .await;
                }
                Ok(())
            }
            PendingOpKind::DeletePermanent => {
                if !session.capabilities().uidplus {
                    return Err(CoreError::protocol(
                        None::<String>,
                        "服务器不支持 UIDPLUS，已禁用永久删除",
                    ));
                }
                session.select(&source_name).await?;
                session
                    .store_flags(&[source_uid], Flags::DELETED, FlagStoreMode::Add)
                    .await?;
                session.uid_expunge(&[source_uid]).await?;
                Ok(())
            }
            PendingOpKind::EmptyFolder => {
                // 逐封 delete_permanent 已覆盖；empty_folder 仅作为占位类型。
                Ok(())
            }
        }
    }

    async fn confirm_move(
        &self,
        session: &mut F::Session,
        op: &PendingOp,
        target: &Folder,
        source_uid: u32,
        mapping: Option<Vec<(u32, u32)>>,
    ) -> bool {
        let message_id = match op.message {
            Some(id) => match self.db(move |s| s.get_message(id)).await {
                Ok(Some(message)) => message.message_id,
                _ => None,
            },
            None => None,
        };
        let Some(message_id) = message_id else {
            return false;
        };

        let mut target_uid = mapping.as_ref().and_then(|pairs| {
            pairs
                .iter()
                .find(|(src, _)| *src == source_uid)
                .map(|(_, dst)| *dst)
        });

        if target_uid.is_none()
            && session.select(&target.name).await.is_ok()
            && let Ok(found) = session.search_message_id(&message_id).await
        {
            target_uid = found.first().copied();
        }

        if let Some(uid) = target_uid {
            let target_id = target.id;
            return self
                .db(move |s| s.confirm_moved_uid(target_id, &message_id, uid))
                .await
                .ok()
                .flatten()
                .is_some();
        }
        false
    }
}
