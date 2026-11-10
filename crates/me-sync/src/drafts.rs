//! 草稿双向同步（WP-10）。
//!
//! 规则见 `doc/v02架构设计.md` §5.4：
//! - 草稿使用 Drafts 文件夹全量 UID 扫描（不做 UIDNEXT 增量）。
//! - 本地与服务器各自维护基线哈希；**不直接比较两侧哈希**，只判断各自是否变化。
//! - 双方都改 → 保留两份，冲突副本 `conflict_of` 指向原草稿，不静默覆盖。
//! - 本地删除写墓碑；服务器删除确认后才物理删除。
//! - 单封失败只影响该草稿，不阻塞整账户邮件同步。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use me_core::port::{FlagStoreMode, ImapSession, ImapSessionFactory, SmtpSender, StorePort};
use me_core::{
    Account, AccountId, Address, CoreError, Draft, DraftConflictResolution, DraftContent,
    DraftDeletionResolution, DraftId, DraftSyncState, ErrorScope, Event, Flags, Folder, FolderRole,
    OutgoingMessage, generate_message_id,
};
use me_store::cache::{self, CachePaths};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::SyncService;

#[derive(Clone)]
struct SnapshotAttachment {
    part_index: u32,
    filename: String,
    data: Vec<u8>,
    path: Option<PathBuf>,
}

#[derive(Clone)]
struct DraftSnapshot {
    message_id: String,
    subject: String,
    to: Vec<Address>,
    cc: Vec<Address>,
    bcc: Vec<Address>,
    body_text: String,
    attachments: Vec<SnapshotAttachment>,
}

struct ServerDraft<'a> {
    uid: u32,
    uid_validity: u32,
    raw: &'a [u8],
    snapshot: &'a DraftSnapshot,
}

impl DraftSnapshot {
    fn without_bcc(&self) -> Self {
        let mut clone = self.clone();
        clone.bcc.clear();
        clone
    }
}

fn hex_sha(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn snapshot_hash(snapshot: &DraftSnapshot) -> String {
    let address = |addr: &Address| serde_json::json!([addr.name, addr.addr.to_ascii_lowercase()]);
    let mut attachments: Vec<serde_json::Value> = snapshot
        .attachments
        .iter()
        .map(|attachment| {
            // 文件名可能因本地托管命名而不同，哈希只认内容。
            serde_json::json!({
                "size": attachment.data.len(),
                "sha": hex_sha(&attachment.data),
            })
        })
        .collect();
    attachments.sort_by_key(|value| value.to_string());

    let value = serde_json::json!({
        "subject": snapshot.subject,
        "to": snapshot.to.iter().map(address).collect::<Vec<_>>(),
        "cc": snapshot.cc.iter().map(address).collect::<Vec<_>>(),
        "bcc": snapshot.bcc.iter().map(address).collect::<Vec<_>>(),
        "body": snapshot.body_text.replace("\r\n", "\n").trim_end(),
        "attachments": attachments,
    });
    hex_sha(value.to_string().as_bytes())
}

fn load_local_snapshot(paths: &CachePaths, draft: &Draft) -> Result<DraftSnapshot, CoreError> {
    let raw = cache::read_draft_raw(paths, draft.id)?
        .ok_or_else(|| CoreError::storage("草稿原文缺失，无法同步"))?;
    let parsed = me_proto::mime::parse_draft(&raw)?;
    let mut attachments = Vec::new();
    if let Ok(entries) = std::fs::read_dir(paths.draft_attachment_dir(draft.id)) {
        for entry in entries.flatten() {
            if !entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
            {
                continue;
            }
            let Some(filename) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let data = std::fs::read(entry.path())
                .map_err(|err| CoreError::storage(format!("读取草稿附件失败：{err}")))?;
            attachments.push(SnapshotAttachment {
                part_index: attachments.len() as u32 + 1,
                filename,
                data,
                path: Some(entry.path()),
            });
        }
    }
    attachments.sort_by(|left, right| left.filename.cmp(&right.filename));

    Ok(DraftSnapshot {
        message_id: draft.message_id.clone(),
        subject: draft
            .subject
            .clone()
            .or_else(|| parsed.header.subject.clone())
            .unwrap_or_default(),
        to: draft.to.clone(),
        cc: parsed.header.cc.clone(),
        bcc: draft.bcc.clone(),
        body_text: parsed.body.text,
        attachments,
    })
}

fn server_snapshot(account: &Account, parsed: &me_proto::mime::ParsedDraft) -> DraftSnapshot {
    let mut attachments: Vec<SnapshotAttachment> = parsed
        .attachments
        .iter()
        .map(|decoded| SnapshotAttachment {
            part_index: decoded.attachment.part_index,
            filename: decoded.attachment.display_name(),
            data: decoded.data.clone(),
            path: None,
        })
        .collect();
    attachments.sort_by(|left, right| left.filename.cmp(&right.filename));
    DraftSnapshot {
        message_id: parsed
            .header
            .message_id
            .clone()
            .unwrap_or_else(|| generate_message_id(&account.email)),
        subject: parsed.header.subject.clone().unwrap_or_default(),
        to: parsed.header.to.clone(),
        cc: parsed.header.cc.clone(),
        bcc: Vec::new(),
        body_text: parsed.body.text.clone(),
        attachments,
    }
}

fn write_snapshot_files(
    paths: &CachePaths,
    id: DraftId,
    raw: &[u8],
    snapshot: &DraftSnapshot,
) -> Result<String, CoreError> {
    cache::clear_draft_attachments(paths, id);
    let path = cache::write_draft_raw(paths, id, raw)?;
    for attachment in &snapshot.attachments {
        cache::write_draft_attachment_bytes(
            paths,
            id,
            attachment.part_index,
            Some(&attachment.filename),
            &attachment.data,
        )?;
    }
    Ok(path.to_string_lossy().to_string())
}

impl<S, F, M> SyncService<S, F, M>
where
    S: StorePort + Clone,
    F: ImapSessionFactory + 'static,
    M: SmtpSender + 'static,
{
    async fn emit_draft_state(&self, draft: DraftId, state: DraftSyncState, error: Option<String>) {
        self.emit(Event::DraftSyncStateChanged {
            draft,
            state,
            error,
        })
        .await;
    }

    /// 完整账户同步后调用（或手动同步 Drafts）。串行化，避免并发 APPEND。
    pub(crate) async fn sync_drafts(
        &self,
        account_id: AccountId,
        token: &CancellationToken,
    ) -> Result<(), CoreError> {
        if token.is_cancelled() {
            return Ok(());
        }
        let _gate = self.inner.draft_gate.lock().await;
        let account = self.account(account_id).await?;
        let folder = {
            let folders = self.db(move |s| s.list_folders(account_id)).await?;
            match folders
                .into_iter()
                .find(|folder| folder.role == FolderRole::Drafts)
            {
                Some(folder) => folder,
                None => {
                    self.emit(Event::Error {
                        scope: ErrorScope::Account(account_id),
                        message: "账户没有草稿箱文件夹，跳过草稿同步".into(),
                    })
                    .await;
                    return Ok(());
                }
            }
        };

        let credential = self.credential(&account).await?;
        let _guard = self.inner.governor.acquire(account_id).await?;
        let mut session = self.inner.factory.connect(&account, &credential).await?;
        let result = self
            .sync_drafts_session(&account, &folder, &mut session, token)
            .await;
        let _ = session.logout().await;
        result
    }

    async fn sync_drafts_session(
        &self,
        account: &Account,
        folder: &Folder,
        session: &mut F::Session,
        token: &CancellationToken,
    ) -> Result<(), CoreError> {
        let account_id = account.id;
        let selected = session.select(&folder.name).await?;
        let uid_validity = selected.uid_validity;
        if folder.uid_validity != uid_validity {
            let folder_id = folder.id;
            let account_id = account.id;
            self.db(move |s| {
                s.set_folder_uid_state(folder_id, uid_validity, 0)?;
                s.reset_draft_server_identity(account_id)
            })
            .await?;
            tracing::info!(
                account = account.id.0,
                uid_validity,
                "draft UIDVALIDITY changed; server identities reset"
            );
        }

        let server_uids = session.search_all_uids().await?;
        let server_set: HashSet<u32> = server_uids.iter().copied().collect();
        let local = self.db(move |s| s.list_drafts(Some(account_id))).await?;
        let mapped: HashSet<u32> = local
            .iter()
            .filter(|draft| draft.server_uid_validity == Some(uid_validity))
            .filter_map(|draft| draft.server_uid)
            .collect();

        // 1) 新增 UID：拉取原文 → 匹配本地 → 导入/覆盖/冲突。
        for uid in server_uids {
            if token.is_cancelled() {
                return Ok(());
            }
            if mapped.contains(&uid) {
                continue;
            }
            let raw = match session.fetch_body(uid).await {
                Ok(raw) => raw,
                Err(err) => {
                    tracing::warn!(account = account.id.0, uid, error = %err.user_message(), "draft fetch failed");
                    continue;
                }
            };
            let parsed = match me_proto::mime::parse_draft(&raw) {
                Ok(parsed) => parsed,
                Err(err) => {
                    tracing::warn!(account = account.id.0, uid, error = %err.user_message(), "draft parse failed");
                    continue;
                }
            };
            if let Err(err) = self
                .import_server_draft(account, uid, uid_validity, &raw, &parsed)
                .await
            {
                tracing::warn!(account = account.id.0, uid, error = %err.user_message(), "draft import failed");
                self.emit(Event::Error {
                    scope: ErrorScope::Account(account.id),
                    message: format!("拉取服务器草稿失败：{}", err.user_message()),
                })
                .await;
            }
        }

        // 2) 服务器 UID 消失 / 墓碑确认。
        let local_after = self.db(move |s| s.list_drafts(Some(account_id))).await?;
        for draft in local_after {
            let Some(uid) = draft.server_uid else {
                continue;
            };
            if server_set.contains(&uid) {
                continue;
            }
            let id = draft.id;
            match draft.sync_state {
                DraftSyncState::Synced => {
                    self.db(move |s| {
                        s.set_draft_sync_state(id, DraftSyncState::DeletedServer, None)
                    })
                    .await?;
                    self.emit_draft_state(id, DraftSyncState::DeletedServer, None)
                        .await;
                }
                DraftSyncState::LocalDirty => {
                    // 服务器副本已不存在：按 LocalOnly 路径重新 APPEND。
                    self.db(move |s| s.set_draft_server_uid(id, None)).await?;
                }
                _ => {}
            }
        }

        let candidates = self
            .db(move |s| s.list_draft_sync_candidates(account_id))
            .await?;
        for item in candidates.iter().filter(|item| item.deleted) {
            if token.is_cancelled() {
                return Ok(());
            }
            let Some(uid) = item.server_uid.filter(|uid| server_set.contains(uid)) else {
                self.finalize_deleted_draft(account.id, item.id).await;
                continue;
            };
            if let Err(err) = self
                .delete_server_tombstone(account, session, item.id, uid, uid_validity)
                .await
            {
                let item_id = item.id;
                let message = err.user_message();
                let message_for_db = message.clone();
                let attempts = self
                    .db(move |s| s.bump_draft_sync_attempt(item_id, &message_for_db))
                    .await
                    .unwrap_or(1);
                self.emit_draft_state(item_id, DraftSyncState::DeletedLocal, Some(message))
                    .await;
                if attempts >= 3 {
                    tracing::warn!(
                        account = account.id.0,
                        draft = item.id.0,
                        "draft deletion retry limit reached"
                    );
                }
            }
        }

        // 3) 推送本地修改。
        if !self.inner.draft_server_sync.load(Ordering::Relaxed) {
            return Ok(());
        }
        let bulk_pending = self.inner.draft_bulk_upload_pending.load(Ordering::Relaxed);
        let bulk_declined = self
            .inner
            .draft_bulk_upload_declined
            .load(Ordering::Relaxed);
        let candidates = self
            .db(move |s| s.list_draft_sync_candidates(account_id))
            .await?;
        for item in candidates {
            if token.is_cancelled() {
                return Ok(());
            }
            if item.deleted || item.sync_attempts >= 3 {
                continue;
            }
            let should_push = match item.state {
                DraftSyncState::LocalDirty => true,
                DraftSyncState::LocalOnly => !bulk_pending && !bulk_declined,
                DraftSyncState::Conflict => false,
                _ => false,
            };
            if !should_push {
                continue;
            }
            let id = item.id;
            let Some(draft) = self.db(move |s| s.get_draft(id)).await? else {
                continue;
            };
            match self
                .push_local_draft(account, &draft, session, &folder.name, uid_validity)
                .await
            {
                Ok(()) => {}
                Err(err) => {
                    let message = err.user_message();
                    let message_for_db = message.clone();
                    self.db(move |s| s.bump_draft_sync_attempt(id, &message_for_db))
                        .await
                        .ok();
                    self.emit_draft_state(id, item.state, Some(message)).await;
                }
            }
        }

        Ok(())
    }

    async fn import_server_draft(
        &self,
        account: &Account,
        uid: u32,
        uid_validity: u32,
        raw: &[u8],
        parsed: &me_proto::mime::ParsedDraft,
    ) -> Result<(), CoreError> {
        let snapshot = server_snapshot(account, parsed);
        let server_hash = snapshot_hash(&snapshot);
        let matches = {
            let account_id = account.id;
            let message_id = snapshot.message_id.clone();
            self.db(move |s| s.find_drafts_by_message_id(account_id, &message_id))
                .await?
        };

        match matches.as_slice() {
            [] => {
                let id = self
                    .create_local_draft_from_snapshot(account, &snapshot, raw)
                    .await?;
                let local_hash = server_hash.clone();
                self.db(move |s| {
                    s.mark_draft_synced(
                        id,
                        Some(uid),
                        Some(uid_validity),
                        &local_hash,
                        &server_hash,
                    )
                })
                .await?;
                self.emit(Event::DraftChanged { id }).await;
                self.emit_draft_state(id, DraftSyncState::Synced, None)
                    .await;
            }
            [local] => {
                self.merge_server_into_local(account, local, uid, uid_validity, raw, &snapshot)
                    .await?;
            }
            _ => {
                // Message-ID 重复：不自动合并，服务器内容作为冲突副本导入。
                let local = &matches[0];
                let id = self
                    .create_conflict_copy_from_snapshot(account, local.id, &snapshot, raw)
                    .await?;
                self.emit(Event::DraftConflictDetected {
                    draft: local.id,
                    conflict_copy: id,
                })
                .await;
            }
        }
        Ok(())
    }

    async fn create_local_draft_from_snapshot(
        &self,
        account: &Account,
        snapshot: &DraftSnapshot,
        raw: &[u8],
    ) -> Result<DraftId, CoreError> {
        let account_id = account.id;
        let message_id = snapshot.message_id.clone();
        let subject = Some(snapshot.subject.clone()).filter(|value| !value.is_empty());
        let to = snapshot.to.clone();
        let draft = self
            .db(move |s| {
                s.create_draft(&me_core::port::NewDraft {
                    account: account_id,
                    message_id,
                    subject,
                    to,
                    bcc: Vec::new(),
                    raw_path: String::new(),
                })
            })
            .await?;
        let id = draft.id;
        let paths = self.inner.paths.clone();
        let snapshot_for_files = snapshot.clone();
        let raw_for_files = raw.to_vec();
        let raw_path = tokio::task::spawn_blocking(move || {
            write_snapshot_files(&paths, id, &raw_for_files, &snapshot_for_files)
        })
        .await
        .map_err(|join| CoreError::storage(format!("草稿文件写入失败：{join}")))??;
        let subject_for_db = Some(snapshot.subject.clone()).filter(|value| !value.is_empty());
        let to_for_db = snapshot.to.clone();
        self.db(move |s| s.update_draft(id, subject_for_db.as_deref(), &to_for_db, &[], &raw_path))
            .await?;
        Ok(id)
    }

    async fn create_conflict_copy_from_snapshot(
        &self,
        account: &Account,
        source: DraftId,
        snapshot: &DraftSnapshot,
        raw: &[u8],
    ) -> Result<DraftId, CoreError> {
        let content = DraftContent {
            account: account.id,
            draft_id: source,
            message_id: snapshot.message_id.clone(),
            subject: snapshot.subject.clone(),
            to: snapshot.to.clone(),
            cc: snapshot.cc.clone(),
            bcc: Vec::new(),
            body_text: snapshot.body_text.clone(),
            attachments: Vec::new(),
        };
        let copy = self
            .db(move |s| s.create_conflict_copy(source, &content))
            .await?;
        let paths = self.inner.paths.clone();
        let snapshot_for_files = snapshot.clone();
        let raw_for_files = raw.to_vec();
        let raw_path = tokio::task::spawn_blocking(move || {
            write_snapshot_files(&paths, copy, &raw_for_files, &snapshot_for_files)
        })
        .await
        .map_err(|join| CoreError::storage(format!("冲突副本写入失败：{join}")))??;
        let copy_subject = format!("[冲突副本] {}", snapshot.subject);
        let to = snapshot.to.clone();
        self.db(move |s| s.update_draft(copy, Some(&copy_subject), &to, &[], &raw_path))
            .await?;
        self.emit(Event::DraftChanged { id: copy }).await;
        Ok(copy)
    }

    async fn merge_server_into_local(
        &self,
        account: &Account,
        local: &Draft,
        uid: u32,
        uid_validity: u32,
        raw: &[u8],
        server_snapshot: &DraftSnapshot,
    ) -> Result<(), CoreError> {
        let item = {
            let id = local.id;
            self.db(move |s| s.get_draft_sync_item(id))
                .await?
                .ok_or_else(|| CoreError::storage("草稿同步候选消失"))?
        };
        let paths = self.inner.paths.clone();
        let local_for_snapshot = local.clone();
        let local_snapshot =
            tokio::task::spawn_blocking(move || load_local_snapshot(&paths, &local_for_snapshot))
                .await
                .map_err(|join| CoreError::storage(format!("读取本地草稿失败：{join}")))??;

        let local_hash = snapshot_hash(&local_snapshot);
        let local_equiv_hash = snapshot_hash(&local_snapshot.without_bcc());
        let server_hash = snapshot_hash(server_snapshot);
        let baseline_local = item.local_hash.clone();
        let baseline_server = item.server_hash.clone();

        let local_changed = baseline_local.as_deref() != Some(local_hash.as_str());
        let server_changed = baseline_server.as_deref() != Some(server_hash.as_str());
        let only_server_changed = !local_changed && server_changed;
        let only_local_changed = local_changed && !server_changed;
        let no_baseline = baseline_local.is_none() && baseline_server.is_none();
        let both_changed = local_changed && server_changed;

        if no_baseline && local_equiv_hash == server_hash {
            self.bind_synced(local.id, uid, uid_validity, &local_hash, &server_hash)
                .await?;
            return Ok(());
        }
        if only_server_changed {
            return self
                .overwrite_local_with_server(local.id, uid, uid_validity, raw, server_snapshot)
                .await;
        }
        if only_local_changed {
            self.bind_synced(local.id, uid, uid_validity, &local_hash, &server_hash)
                .await?;
            let id = local.id;
            self.db(move |s| s.set_draft_sync_state(id, DraftSyncState::LocalDirty, None))
                .await?;
            self.emit_draft_state(id, DraftSyncState::LocalDirty, None)
                .await;
            return Ok(());
        }
        if both_changed || no_baseline {
            return self
                .create_conflict(
                    account,
                    local,
                    ServerDraft {
                        uid,
                        uid_validity,
                        raw,
                        snapshot: server_snapshot,
                    },
                    &local_hash,
                )
                .await;
        }

        // 两侧都没有变化（或变化相同）：只更新服务器身份。
        self.bind_synced(local.id, uid, uid_validity, &local_hash, &server_hash)
            .await
    }

    async fn bind_synced(
        &self,
        id: DraftId,
        uid: u32,
        uid_validity: u32,
        local_hash: &str,
        server_hash: &str,
    ) -> Result<(), CoreError> {
        let local_hash = local_hash.to_string();
        let server_hash = server_hash.to_string();
        self.db(move |s| {
            s.mark_draft_synced(id, Some(uid), Some(uid_validity), &local_hash, &server_hash)
        })
        .await?;
        self.emit_draft_state(id, DraftSyncState::Synced, None)
            .await;
        Ok(())
    }

    async fn overwrite_local_with_server(
        &self,
        id: DraftId,
        uid: u32,
        uid_validity: u32,
        raw: &[u8],
        snapshot: &DraftSnapshot,
    ) -> Result<(), CoreError> {
        let paths = self.inner.paths.clone();
        let snapshot_for_files = snapshot.clone();
        let raw_for_files = raw.to_vec();
        let raw_path = tokio::task::spawn_blocking(move || {
            write_snapshot_files(&paths, id, &raw_for_files, &snapshot_for_files)
        })
        .await
        .map_err(|join| CoreError::storage(format!("草稿覆盖失败：{join}")))??;
        let subject = Some(snapshot.subject.clone()).filter(|value| !value.is_empty());
        let to = snapshot.to.clone();
        self.db(move |s| s.update_draft(id, subject.as_deref(), &to, &[], &raw_path))
            .await?;
        let hash = snapshot_hash(snapshot);
        self.bind_synced(id, uid, uid_validity, &hash, &hash).await
    }

    async fn create_conflict(
        &self,
        account: &Account,
        local: &Draft,
        server: ServerDraft<'_>,
        local_hash: &str,
    ) -> Result<(), CoreError> {
        let server_hash = snapshot_hash(server.snapshot);
        // 顺序是**先落副本、再标冲突**，反过来会留下一个真实的坏状态：
        // 「原草稿已 Conflict，但副本文件还没写完」。UI 收到 `DraftConflictDetected`
        // 就弹「已保留两份（副本 X）」，用户点进去却读不到原文；点「保留两份」时
        // `resolve_conflict` 读副本原文得到 None，直接报「冲突副本原文缺失」。
        // CI 上 `spawn_blocking` 与 DB 往返都比本机慢，这个窗口被
        // `draft_sync_mock::both_sides_changed_creates_conflict_copy` 抓到了。
        let copy = self
            .create_conflict_copy_from_snapshot(account, local.id, server.snapshot, server.raw)
            .await?;
        // 基线对齐到「服务器版本」，但**不发 Synced 事件**：它只是中间态，
        // 发出去只会让界面闪一下 Synced → Conflict。
        let local_id = local.id;
        let local_hash = local_hash.to_string();
        let server_uid = server.uid;
        let uid_validity = server.uid_validity;
        let baseline_server = server_hash;
        self.db(move |s| {
            s.mark_draft_synced(
                local_id,
                Some(server_uid),
                Some(uid_validity),
                &local_hash,
                &baseline_server,
            )
        })
        .await?;
        self.db(move |s| s.set_draft_sync_state(local_id, DraftSyncState::Conflict, None))
            .await?;
        self.emit_draft_state(local.id, DraftSyncState::Conflict, None)
            .await;
        self.emit(Event::DraftConflictDetected {
            draft: local.id,
            conflict_copy: copy,
        })
        .await;
        Ok(())
    }

    async fn push_local_draft(
        &self,
        account: &Account,
        draft: &Draft,
        session: &mut F::Session,
        folder_name: &str,
        uid_validity: u32,
    ) -> Result<(), CoreError> {
        let paths = self.inner.paths.clone();
        let draft_for_snapshot = draft.clone();
        let snapshot =
            tokio::task::spawn_blocking(move || load_local_snapshot(&paths, &draft_for_snapshot))
                .await
                .map_err(|join| CoreError::storage(format!("读取本地草稿失败：{join}")))??;

        let local_hash = snapshot_hash(&snapshot);
        let server_hash = snapshot_hash(&snapshot.without_bcc());
        let attachments: Vec<PathBuf> = snapshot
            .attachments
            .iter()
            .filter_map(|attachment| attachment.path.clone())
            .collect();
        let outgoing = OutgoingMessage {
            account: account.id,
            draft_id: Some(draft.id),
            message_id: snapshot.message_id.clone(),
            from: account.from_address(),
            to: snapshot.to.clone(),
            cc: snapshot.cc.clone(),
            bcc: snapshot.bcc.clone(),
            subject: snapshot.subject.clone(),
            body_text: snapshot.body_text.clone(),
            attachments,
        };
        let raw = me_proto::mime::build_rfc822(&outgoing)?;

        let old_uid = draft.server_uid;
        let mut new_uid = session.append(folder_name, &raw, Flags::DRAFT).await?;
        if new_uid.is_none() {
            let mut found = session.search_message_id(&snapshot.message_id).await?;
            found.sort_unstable();
            new_uid = found.pop();
        }
        let Some(new_uid) = new_uid else {
            return Err(CoreError::protocol(
                None::<String>,
                "APPEND 成功但无法确认服务器 UID",
            ));
        };

        let mut cleanup_error = None;
        if let Some(old_uid) = old_uid
            && old_uid != new_uid
        {
            let result = self
                .remove_old_server_version(account, session, old_uid)
                .await;
            cleanup_error = result.err();
        }

        let id = draft.id;
        let local_hash_for_db = local_hash;
        let server_hash_for_db = server_hash;
        self.db(move |s| {
            s.mark_draft_synced(
                id,
                Some(new_uid),
                Some(uid_validity),
                &local_hash_for_db,
                &server_hash_for_db,
            )?;
            s.clear_draft_sync_attempts(id)
        })
        .await?;
        if let Some(error) = cleanup_error {
            let message = error.user_message();
            let message_for_db = message.clone();
            self.db(move |s| {
                s.set_draft_sync_state(id, DraftSyncState::Synced, Some(&message_for_db))
            })
            .await?;
            self.emit_draft_state(id, DraftSyncState::Synced, Some(message))
                .await;
        } else {
            self.emit_draft_state(id, DraftSyncState::Synced, None)
                .await;
        }
        self.emit(Event::DraftChanged { id }).await;
        Ok(())
    }

    async fn remove_old_server_version(
        &self,
        account: &Account,
        session: &mut F::Session,
        old_uid: u32,
    ) -> Result<(), CoreError> {
        session
            .store_flags(&[old_uid], Flags::DELETED, FlagStoreMode::Add)
            .await?;
        let capabilities = session.capabilities();
        if capabilities.uidplus {
            session.uid_expunge(&[old_uid]).await?;
            return Ok(());
        }
        if capabilities.move_supported {
            let account_id = account.id;
            let folders = self.db(move |s| s.list_folders(account_id)).await?;
            if let Some(trash) = folders
                .iter()
                .find(|folder| folder.role == FolderRole::Trash)
            {
                session.move_to(&[old_uid], &trash.name).await?;
                return Ok(());
            }
        }
        Err(CoreError::protocol(
            None::<String>,
            "服务器不支持精确删除旧草稿，已保留 \\Deleted 副本",
        ))
    }

    async fn delete_server_tombstone(
        &self,
        account: &Account,
        session: &mut F::Session,
        id: DraftId,
        uid: u32,
        _uid_validity: u32,
    ) -> Result<(), CoreError> {
        session
            .store_flags(&[uid], Flags::DELETED, FlagStoreMode::Add)
            .await?;
        if session.capabilities().uidplus {
            session.uid_expunge(&[uid]).await?;
            self.finalize_deleted_draft(account.id, id).await;
            Ok(())
        } else {
            Err(CoreError::protocol(
                None::<String>,
                "服务器不支持 UIDPLUS，无法精确删除草稿",
            ))
        }
    }

    async fn finalize_deleted_draft(&self, account: AccountId, id: DraftId) {
        let deleted = self.db(move |s| s.finalize_deleted_draft(id)).await.is_ok();
        if !deleted {
            self.emit(Event::Error {
                scope: ErrorScope::Account(account),
                message: "删除本地草稿失败".into(),
            })
            .await;
            return;
        }
        let paths = self.inner.paths.clone();
        let _ = tokio::task::spawn_blocking(move || cache::remove_draft_dir(&paths, id)).await;
        self.emit(Event::DraftDeleted { id }).await;
    }

    /// 手动「保存到服务器」/重试：忽略自动镜像与批量上传确认，但仍遵守连接预算。
    pub(crate) async fn push_draft_to_server(&self, id: DraftId) -> Result<(), CoreError> {
        let _gate = self.inner.draft_gate.lock().await;
        let draft = self
            .db(move |s| s.get_draft(id))
            .await?
            .ok_or_else(|| CoreError::storage("草稿不存在"))?;
        let account = self.account(draft.account).await?;
        let folder = {
            let account_id = account.id;
            let folders = self.db(move |s| s.list_folders(account_id)).await?;
            folders
                .into_iter()
                .find(|folder| folder.role == FolderRole::Drafts)
                .ok_or_else(|| CoreError::storage("账户没有草稿箱文件夹"))?
        };
        let credential = self.credential(&account).await?;
        let _guard = self.inner.governor.acquire(account.id).await?;
        let mut session = self.inner.factory.connect(&account, &credential).await?;
        let result = async {
            let selected = session.select(&folder.name).await?;
            self.push_local_draft(
                &account,
                &draft,
                &mut session,
                &folder.name,
                selected.uid_validity,
            )
            .await
        }
        .await;
        let _ = session.logout().await;
        result
    }

    pub(crate) async fn mark_local_draft_dirty_after_save(&self, id: DraftId) {
        if !self.inner.draft_server_sync.load(Ordering::Relaxed) {
            return;
        }
        let Some(draft) = self.db(move |s| s.get_draft(id)).await.ok().flatten() else {
            return;
        };
        if matches!(
            draft.sync_state,
            DraftSyncState::Conflict | DraftSyncState::DeletedLocal | DraftSyncState::DeletedServer
        ) {
            return;
        }
        let _ = self
            .db(move |s| s.set_draft_sync_state(id, DraftSyncState::LocalDirty, None))
            .await;
    }

    fn trigger_drafts_sync(&self, account: AccountId) {
        let service = self.clone();
        tokio::spawn(async move {
            let token = CancellationToken::new();
            if let Err(err) = service.sync_drafts(account, &token).await {
                tracing::warn!(account = account.0, error = %err.user_message(), "draft sync failed");
            }
        });
    }

    pub(crate) async fn handle_delete_draft(&self, id: DraftId) {
        let Some(draft) = self.db(move |s| s.get_draft(id)).await.ok().flatten() else {
            return;
        };
        if matches!(draft.sync_state, DraftSyncState::DeletedServer)
            || (draft.server_uid.is_none() && draft.sync_state == DraftSyncState::LocalOnly)
        {
            self.finalize_deleted_draft(draft.account, id).await;
            return;
        }
        if let Err(err) = self.db(move |s| s.tombstone_draft(id)).await {
            self.emit(Event::Error {
                scope: ErrorScope::Draft(id),
                message: err.user_message(),
            })
            .await;
            return;
        }
        self.emit_draft_state(id, DraftSyncState::DeletedLocal, None)
            .await;
        self.trigger_drafts_sync(draft.account);
    }

    pub(crate) async fn handle_retry_draft_sync(&self, id: DraftId) {
        let Ok(Some(draft)) = self.db(move |s| s.get_draft(id)).await else {
            return;
        };
        let _ = self.db(move |s| s.clear_draft_sync_attempts(id)).await;
        self.emit_draft_state(id, draft.sync_state, None).await;
        self.trigger_drafts_sync(draft.account);
    }

    pub(crate) async fn handle_resolve_conflict(
        &self,
        draft_id: DraftId,
        resolution: DraftConflictResolution,
    ) {
        let Ok(Some(draft)) = self.db(move |s| s.get_draft(draft_id)).await else {
            return;
        };
        let account_id = draft.account;
        let matches = {
            let message_id = draft.message_id.clone();
            self.db(move |s| s.find_drafts_by_message_id(account_id, &message_id))
                .await
                .unwrap_or_default()
        };
        let copy = matches
            .iter()
            .find(|candidate| candidate.conflict_of == Some(draft_id))
            .cloned();
        let Some(copy) = copy else {
            // 没有冲突副本：按本地内容重新推送。
            let _ = self
                .db(move |s| s.set_draft_sync_state(draft_id, DraftSyncState::LocalDirty, None))
                .await;
            self.emit_draft_state(draft_id, DraftSyncState::LocalDirty, None)
                .await;
            self.trigger_drafts_sync(account_id);
            return;
        };

        match resolution {
            DraftConflictResolution::KeepLocal => {
                self.delete_draft_local(copy.id).await;
                let _ = self
                    .db(move |s| s.set_draft_sync_state(draft_id, DraftSyncState::LocalDirty, None))
                    .await;
                self.emit_draft_state(draft_id, DraftSyncState::LocalDirty, None)
                    .await;
                self.trigger_drafts_sync(account_id);
            }
            DraftConflictResolution::UseServer => {
                let hash = match self.adopt_draft_content(draft_id, &copy).await {
                    Ok(hash) => hash,
                    Err(err) => {
                        self.emit(Event::Error {
                            scope: ErrorScope::Draft(draft_id),
                            message: err.user_message(),
                        })
                        .await;
                        return;
                    }
                };
                self.delete_draft_local(copy.id).await;
                if let (Some(uid), Some(uid_validity)) =
                    (draft.server_uid, draft.server_uid_validity)
                {
                    let _ = self
                        .bind_synced(draft_id, uid, uid_validity, &hash, &hash)
                        .await;
                } else {
                    let _ = self
                        .db(move |s| s.set_draft_sync_state(draft_id, DraftSyncState::Synced, None))
                        .await;
                    self.emit_draft_state(draft_id, DraftSyncState::Synced, None)
                        .await;
                }
            }
            DraftConflictResolution::KeepBoth => {
                let _ = self.db(move |s| s.promote_conflict_copy(copy.id)).await;
                let _ = self
                    .db(move |s| s.set_draft_sync_state(draft_id, DraftSyncState::LocalDirty, None))
                    .await;
                self.emit_draft_state(copy.id, DraftSyncState::LocalOnly, None)
                    .await;
                self.emit_draft_state(draft_id, DraftSyncState::LocalDirty, None)
                    .await;
                self.trigger_drafts_sync(account_id);
            }
        }
    }

    async fn adopt_draft_content(
        &self,
        target: DraftId,
        source: &Draft,
    ) -> Result<String, CoreError> {
        let paths = self.inner.paths.clone();
        let source_for_load = source.clone();
        let snapshot =
            tokio::task::spawn_blocking(move || load_local_snapshot(&paths, &source_for_load))
                .await
                .map_err(|join| CoreError::storage(format!("读取冲突副本失败：{join}")))??;
        let raw = {
            let paths = self.inner.paths.clone();
            let source_id = source.id;
            tokio::task::spawn_blocking(move || cache::read_draft_raw(&paths, source_id))
                .await
                .map_err(|join| CoreError::storage(format!("读取冲突副本原文失败：{join}")))??
                .ok_or_else(|| CoreError::storage("冲突副本原文缺失"))?
        };
        let paths = self.inner.paths.clone();
        let raw_for_files = raw;
        let snapshot_for_files = snapshot.clone();
        let raw_path = tokio::task::spawn_blocking(move || {
            write_snapshot_files(&paths, target, &raw_for_files, &snapshot_for_files)
        })
        .await
        .map_err(|join| CoreError::storage(format!("应用冲突内容失败：{join}")))??;
        let subject = Some(snapshot.subject.clone()).filter(|value| !value.is_empty());
        let to = snapshot.to.clone();
        self.db(move |s| s.update_draft(target, subject.as_deref(), &to, &[], &raw_path))
            .await?;
        self.emit(Event::DraftChanged { id: target }).await;
        Ok(snapshot_hash(&snapshot.without_bcc()))
    }

    async fn delete_draft_local(&self, id: DraftId) {
        let deleted = self.db(move |s| s.finalize_deleted_draft(id)).await.is_ok();
        if deleted {
            let paths = self.inner.paths.clone();
            let _ = tokio::task::spawn_blocking(move || cache::remove_draft_dir(&paths, id)).await;
            self.emit(Event::DraftDeleted { id }).await;
        }
    }

    pub(crate) async fn handle_resolve_deletion(
        &self,
        draft: DraftId,
        resolution: DraftDeletionResolution,
    ) {
        let Ok(Some(existing)) = self.db(move |s| s.get_draft(draft)).await else {
            return;
        };
        match resolution {
            DraftDeletionResolution::KeepLocal => {
                let _ = self.db(move |s| s.set_draft_server_uid(draft, None)).await;
                let _ = self
                    .db(move |s| s.set_draft_sync_state(draft, DraftSyncState::LocalOnly, None))
                    .await;
                self.emit_draft_state(draft, DraftSyncState::LocalOnly, None)
                    .await;
            }
            DraftDeletionResolution::DeleteLocal => {
                let _ = self.db(move |s| s.tombstone_draft(draft)).await;
                self.emit_draft_state(draft, DraftSyncState::DeletedLocal, None)
                    .await;
                self.trigger_drafts_sync(existing.account);
            }
        }
    }

    pub(crate) async fn handle_confirm_bulk_upload(&self, accept: bool) {
        self.inner
            .draft_bulk_upload_pending
            .store(false, Ordering::Relaxed);
        let _ = self
            .db(|s| s.set_setting(me_core::settings_key::DRAFT_BULK_UPLOAD_PENDING, "false"))
            .await;
        if !accept {
            return;
        }
        let _ = self.db(|s| s.promote_local_only_to_dirty(None)).await;
        let accounts = self.db(|s| s.list_accounts()).await.unwrap_or_default();
        for account in accounts {
            self.trigger_drafts_sync(account.id);
        }
    }
}
