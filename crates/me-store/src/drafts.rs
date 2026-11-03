//! 草稿仓储。`raw_path` 指向唯一不可重建的数据。
//!
//! v0.2：Bcc 只存 `bcc_addrs`（不写入 RFC822）；同步状态、基线与删除墓碑
//! 只做存取与枚举校验，冲突决策在 `me-sync`。

use std::fs;
use std::path::PathBuf;

use me_core::CoreError;
use me_core::model::{
    AccountId, Address, Draft, DraftContent, DraftId, DraftStatus, DraftSyncState, now_unix,
};
use me_core::port::{DraftRepo, DraftSyncItem, NewDraft};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::db::WritableStore;
use crate::{StoreResult, storage};

const SELECT_COLUMNS: &str = "id, account_id, raw_path, message_id, subject, to_addrs, bcc_addrs, \
                              status, server_uid, server_uid_validity, sync_state, conflict_of, \
                              last_synced_at, last_error, updated_at";

fn parse_sync_state(value: &str) -> DraftSyncState {
    // 合法值由写入路径保证；旧/损坏数据保守回落到 local_only。
    DraftSyncState::parse(value).unwrap_or_default()
}

fn map_draft(row: &Row<'_>) -> rusqlite::Result<Draft> {
    let to_addrs: Option<String> = row.get(5)?;
    let bcc_addrs: Option<String> = row.get(6)?;
    let status: String = row.get(7)?;
    let sync_state: String = row.get(10)?;
    Ok(Draft {
        id: DraftId(row.get(0)?),
        account: AccountId(row.get(1)?),
        message_id: row.get(3)?,
        subject: row.get(4)?,
        to: parse_addresses(to_addrs.as_deref()),
        bcc: parse_addresses(bcc_addrs.as_deref()),
        // raw_path 不在 Draft 模型中；调用方通过 `CachePaths::draft_raw_path` 获取。
        status: DraftStatus::parse(&status),
        server_uid: row.get::<_, Option<i64>>(8)?.map(|v| v as u32),
        server_uid_validity: row.get::<_, Option<i64>>(9)?.map(|v| v as u32),
        sync_state: parse_sync_state(&sync_state),
        conflict_of: row.get::<_, Option<i64>>(11)?.map(DraftId),
        last_synced_at: row.get(12)?,
        last_error: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

fn map_sync_item(row: &Row<'_>) -> rusqlite::Result<DraftSyncItem> {
    let state: String = row.get(6)?;
    Ok(DraftSyncItem {
        id: DraftId(row.get(0)?),
        message_id: row.get(1)?,
        server_uid: row.get::<_, Option<i64>>(2)?.map(|v| v as u32),
        server_uid_validity: row.get::<_, Option<i64>>(3)?.map(|v| v as u32),
        local_hash: row.get(4)?,
        server_hash: row.get(5)?,
        state: parse_sync_state(&state),
        deleted: row.get::<_, i64>(8)? != 0,
        sync_attempts: row.get::<_, i64>(7)?.max(0) as u32,
    })
}

fn parse_addresses(json: Option<&str>) -> Vec<Address> {
    json.and_then(|raw| serde_json::from_str::<Vec<Address>>(raw).ok())
        .unwrap_or_default()
}

fn encode_addresses(addresses: &[Address]) -> String {
    serde_json::to_string(addresses).unwrap_or_else(|_| "[]".to_string())
}

pub(crate) fn get_draft_conn(conn: &Connection, id: DraftId) -> Result<Option<Draft>, CoreError> {
    let sql = format!("SELECT {SELECT_COLUMNS} FROM drafts WHERE id = ?1");
    conn.query_row(&sql, params![id.0], map_draft)
        .optional()
        .store()
}

pub(crate) fn list_drafts_conn(
    conn: &Connection,
    account: Option<AccountId>,
) -> Result<Vec<Draft>, CoreError> {
    // 墓碑与冲突副本不进入普通草稿列表。
    let where_clause = "deleted_at IS NULL AND conflict_of IS NULL";
    let (sql, param): (String, Option<i64>) = match account {
        Some(account) => (
            format!(
                "SELECT {SELECT_COLUMNS} FROM drafts \
                 WHERE account_id = ?1 AND {where_clause} \
                 ORDER BY updated_at DESC, id DESC"
            ),
            Some(account.0),
        ),
        None => (
            format!(
                "SELECT {SELECT_COLUMNS} FROM drafts WHERE {where_clause} \
                 ORDER BY updated_at DESC, id DESC"
            ),
            None,
        ),
    };
    let mut stmt = conn.prepare(&sql).store()?;
    let rows = match param {
        Some(account_id) => stmt
            .query_map(params![account_id], map_draft)
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?,
        None => stmt
            .query_map([], map_draft)
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?,
    };
    Ok(rows)
}

pub(crate) fn create_draft_conn(conn: &Connection, draft: &NewDraft) -> Result<Draft, CoreError> {
    conn.execute(
        "INSERT INTO drafts(
             account_id, raw_path, message_id, subject, to_addrs, bcc_addrs,
             status, server_uid, sync_state, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'draft', NULL, 'local_only', ?7)",
        params![
            draft.account.0,
            draft.raw_path,
            draft.message_id,
            draft.subject,
            encode_addresses(&draft.to),
            encode_addresses(&draft.bcc),
            now_unix(),
        ],
    )
    .store()?;
    let id = DraftId(conn.last_insert_rowid());
    get_draft_conn(conn, id)?.ok_or_else(|| storage("草稿写入后无法读取"))
}

pub(crate) fn update_draft_conn(
    conn: &Connection,
    id: DraftId,
    subject: Option<&str>,
    to: &[Address],
    bcc: &[Address],
    raw_path: &str,
) -> Result<Draft, CoreError> {
    let changed = conn
        .execute(
            "UPDATE drafts
                SET subject = ?2, to_addrs = ?3, bcc_addrs = ?4, raw_path = ?5, updated_at = ?6
              WHERE id = ?1",
            params![
                id.0,
                subject,
                encode_addresses(to),
                encode_addresses(bcc),
                raw_path,
                now_unix(),
            ],
        )
        .store()?;
    if changed == 0 {
        return Err(storage(format!("草稿不存在: {}", id.0)));
    }
    get_draft_conn(conn, id)?.ok_or_else(|| storage("草稿更新后无法读取"))
}

pub(crate) fn set_draft_server_uid_conn(
    conn: &Connection,
    id: DraftId,
    uid: Option<u32>,
) -> Result<(), CoreError> {
    let changed = conn
        .execute(
            "UPDATE drafts SET server_uid = ?2, updated_at = ?3 WHERE id = ?1",
            params![id.0, uid.map(i64::from), now_unix()],
        )
        .store()?;
    if changed == 0 {
        return Err(storage(format!("草稿不存在: {}", id.0)));
    }
    Ok(())
}

pub(crate) fn set_draft_status_conn(
    conn: &Connection,
    id: DraftId,
    status: DraftStatus,
    last_error: Option<&str>,
) -> Result<(), CoreError> {
    let changed = conn
        .execute(
            "UPDATE drafts SET status = ?2, last_error = ?3, updated_at = ?4 WHERE id = ?1",
            params![id.0, status.as_str(), last_error, now_unix()],
        )
        .store()?;
    if changed == 0 {
        return Err(storage(format!("草稿不存在: {}", id.0)));
    }
    Ok(())
}

pub(crate) fn delete_draft_conn(conn: &Connection, id: DraftId) -> Result<(), CoreError> {
    let changed = conn
        .execute("DELETE FROM drafts WHERE id = ?1", params![id.0])
        .store()?;
    if changed == 0 {
        return Err(storage(format!("草稿不存在: {}", id.0)));
    }
    Ok(())
}

pub(crate) fn list_draft_sync_candidates_conn(
    conn: &Connection,
    account: AccountId,
) -> Result<Vec<DraftSyncItem>, CoreError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, message_id, server_uid, server_uid_validity, local_hash, server_hash,
                    sync_state, sync_attempts, deleted_at IS NOT NULL
               FROM drafts
              WHERE account_id = ?1
                AND status = 'draft'
                AND (
                     (deleted_at IS NULL
                      AND sync_state IN ('local_only','local_dirty','conflict'))
                  OR (deleted_at IS NOT NULL AND sync_state = 'deleted_local')
                )
              ORDER BY updated_at, id",
        )
        .store()?;
    let rows = stmt
        .query_map(params![account.0], map_sync_item)
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    Ok(rows)
}

pub(crate) fn get_draft_sync_item_conn(
    conn: &Connection,
    id: DraftId,
) -> Result<Option<DraftSyncItem>, CoreError> {
    conn.query_row(
        "SELECT id, message_id, server_uid, server_uid_validity, local_hash, server_hash,
                sync_state, sync_attempts, deleted_at IS NOT NULL
           FROM drafts WHERE id = ?1",
        params![id.0],
        map_sync_item,
    )
    .optional()
    .store()
}

pub(crate) fn find_draft_by_server_uid_conn(
    conn: &Connection,
    account: AccountId,
    uid_validity: u32,
    uid: u32,
) -> Result<Option<Draft>, CoreError> {
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM drafts
          WHERE account_id = ?1 AND server_uid_validity = ?2 AND server_uid = ?3
            AND deleted_at IS NULL"
    );
    conn.query_row(
        &sql,
        params![account.0, i64::from(uid_validity), i64::from(uid)],
        map_draft,
    )
    .optional()
    .store()
}

pub(crate) fn find_drafts_by_message_id_conn(
    conn: &Connection,
    account: AccountId,
    message_id: &str,
) -> Result<Vec<Draft>, CoreError> {
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM drafts
          WHERE account_id = ?1 AND message_id = ?2 AND deleted_at IS NULL
          LIMIT 2"
    );
    let mut stmt = conn.prepare(&sql).store()?;
    let rows = stmt
        .query_map(params![account.0, message_id], map_draft)
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    Ok(rows)
}

pub(crate) fn mark_draft_synced_conn(
    conn: &Connection,
    id: DraftId,
    server_uid: Option<u32>,
    uid_validity: Option<u32>,
    local_hash: &str,
    server_hash: &str,
) -> Result<(), CoreError> {
    let changed = conn
        .execute(
            "UPDATE drafts
                SET server_uid = ?2, server_uid_validity = ?3,
                    local_hash = ?4, server_hash = ?5,
                    sync_state = 'synced', sync_attempts = 0,
                    last_synced_at = ?6, last_error = NULL
              WHERE id = ?1",
            params![
                id.0,
                server_uid.map(i64::from),
                uid_validity.map(i64::from),
                local_hash,
                server_hash,
                now_unix(),
            ],
        )
        .store()?;
    if changed == 0 {
        return Err(storage(format!("草稿不存在: {}", id.0)));
    }
    Ok(())
}

pub(crate) fn set_draft_sync_state_conn(
    conn: &Connection,
    id: DraftId,
    state: DraftSyncState,
    error: Option<&str>,
) -> Result<(), CoreError> {
    // 同步状态不是内容变更：不更新 updated_at，避免草稿列表重新排序。
    let changed = conn
        .execute(
            "UPDATE drafts SET sync_state = ?2, last_error = ?3 WHERE id = ?1",
            params![id.0, state.as_str(), error],
        )
        .store()?;
    if changed == 0 {
        return Err(storage(format!("草稿不存在: {}", id.0)));
    }
    Ok(())
}

pub(crate) fn bump_draft_sync_attempt_conn(
    conn: &Connection,
    id: DraftId,
    error: &str,
) -> Result<u32, CoreError> {
    let changed = conn
        .execute(
            "UPDATE drafts
                SET sync_attempts = sync_attempts + 1, last_error = ?2
              WHERE id = ?1",
            params![id.0, error],
        )
        .store()?;
    if changed == 0 {
        return Err(storage(format!("草稿不存在: {}", id.0)));
    }
    let attempts: i64 = conn
        .query_row(
            "SELECT sync_attempts FROM drafts WHERE id = ?1",
            params![id.0],
            |row| row.get(0),
        )
        .store()?;
    Ok(attempts.max(0) as u32)
}

pub(crate) fn clear_draft_sync_attempts_conn(
    conn: &Connection,
    id: DraftId,
) -> Result<(), CoreError> {
    let changed = conn
        .execute(
            "UPDATE drafts SET sync_attempts = 0, last_error = NULL WHERE id = ?1",
            params![id.0],
        )
        .store()?;
    if changed == 0 {
        return Err(storage(format!("草稿不存在: {}", id.0)));
    }
    Ok(())
}

pub(crate) fn create_conflict_copy_conn(
    conn: &Connection,
    source: DraftId,
    content: &DraftContent,
) -> Result<DraftId, CoreError> {
    let source_account: Option<i64> = conn
        .query_row(
            "SELECT account_id FROM drafts WHERE id = ?1",
            params![source.0],
            |row| row.get(0),
        )
        .optional()
        .store()?;
    if source_account.is_none() {
        return Err(storage(format!("草稿不存在: {}", source.0)));
    }

    let subject = if content.subject.starts_with("[冲突副本]") {
        content.subject.clone()
    } else {
        format!("[冲突副本] {}", content.subject)
    };
    conn.execute(
        "INSERT INTO drafts(
             account_id, raw_path, message_id, subject, to_addrs, bcc_addrs,
             status, server_uid, server_uid_validity, sync_state, conflict_of, updated_at
         ) VALUES (?1, '', ?2, ?3, ?4, ?5, 'draft', NULL, NULL, 'conflict', ?6, ?7)",
        params![
            content.account.0,
            content.message_id,
            subject,
            encode_addresses(&content.to),
            encode_addresses(&content.bcc),
            source.0,
            now_unix(),
        ],
    )
    .store()?;
    Ok(DraftId(conn.last_insert_rowid()))
}

pub(crate) fn tombstone_draft_conn(conn: &Connection, id: DraftId) -> Result<(), CoreError> {
    let changed = conn
        .execute(
            "UPDATE drafts
                SET deleted_at = ?2, sync_state = 'deleted_local', updated_at = ?2
              WHERE id = ?1",
            params![id.0, now_unix()],
        )
        .store()?;
    if changed == 0 {
        return Err(storage(format!("草稿不存在: {}", id.0)));
    }
    Ok(())
}

pub(crate) fn reset_draft_server_identity_conn(
    conn: &Connection,
    account: AccountId,
) -> Result<u32, CoreError> {
    let changed = conn
        .execute(
            "UPDATE drafts
                SET server_uid = NULL, server_uid_validity = NULL, server_hash = NULL
              WHERE account_id = ?1 AND status = 'draft' AND deleted_at IS NULL",
            params![account.0],
        )
        .store()?;
    Ok(changed as u32)
}

pub(crate) fn promote_local_only_to_dirty_conn(
    conn: &Connection,
    account: Option<AccountId>,
) -> Result<u32, CoreError> {
    let changed = match account {
        Some(account) => conn
            .execute(
                "UPDATE drafts SET sync_state = 'local_dirty'
                  WHERE account_id = ?1 AND status = 'draft'
                    AND deleted_at IS NULL AND sync_state = 'local_only'",
                params![account.0],
            )
            .store()?,
        None => conn
            .execute(
                "UPDATE drafts SET sync_state = 'local_dirty'
                  WHERE status = 'draft'
                    AND deleted_at IS NULL AND sync_state = 'local_only'",
                [],
            )
            .store()?,
    };
    Ok(changed as u32)
}

pub(crate) fn promote_conflict_copy_conn(conn: &Connection, id: DraftId) -> Result<(), CoreError> {
    let changed = conn
        .execute(
            "UPDATE drafts SET conflict_of = NULL, sync_state = 'local_only' WHERE id = ?1",
            params![id.0],
        )
        .store()?;
    if changed == 0 {
        return Err(storage(format!("草稿不存在: {}", id.0)));
    }
    Ok(())
}

impl DraftRepo for WritableStore {
    fn create_draft(&self, draft: &NewDraft) -> Result<Draft, CoreError> {
        let conn = self.lock();
        create_draft_conn(&conn, draft)
    }

    fn update_draft(
        &self,
        id: DraftId,
        subject: Option<&str>,
        to: &[Address],
        bcc: &[Address],
        raw_path: &str,
    ) -> Result<Draft, CoreError> {
        let conn = self.lock();
        update_draft_conn(&conn, id, subject, to, bcc, raw_path)
    }

    fn get_draft(&self, id: DraftId) -> Result<Option<Draft>, CoreError> {
        let conn = self.lock();
        get_draft_conn(&conn, id)
    }

    fn list_drafts(&self, account: Option<AccountId>) -> Result<Vec<Draft>, CoreError> {
        let conn = self.lock();
        list_drafts_conn(&conn, account)
    }

    fn set_draft_server_uid(&self, id: DraftId, uid: Option<u32>) -> Result<(), CoreError> {
        let conn = self.lock();
        set_draft_server_uid_conn(&conn, id, uid)
    }

    fn set_draft_status(
        &self,
        id: DraftId,
        status: DraftStatus,
        last_error: Option<&str>,
    ) -> Result<(), CoreError> {
        let conn = self.lock();
        set_draft_status_conn(&conn, id, status, last_error)
    }

    fn delete_draft(&self, id: DraftId) -> Result<(), CoreError> {
        let conn = self.lock();
        delete_draft_conn(&conn, id)
    }

    /// 扫描 `drafts/` 下形如 `{id}` 的目录，用于清理孤儿目录。
    fn list_draft_dirs(&self) -> Result<Vec<(DraftId, PathBuf)>, CoreError> {
        let dir = self.paths().drafts_dir();
        let entries = fs::read_dir(dir).map_err(|e| storage(format!("读取草稿目录失败: {e}")))?;
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if !is_dir {
                continue;
            }
            if let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i64>().ok())
            {
                out.push((DraftId(id), entry.path()));
            }
        }
        out.sort_by_key(|(id, _)| id.0);
        Ok(out)
    }

    fn list_draft_sync_candidates(
        &self,
        account: AccountId,
    ) -> Result<Vec<DraftSyncItem>, CoreError> {
        let conn = self.lock();
        list_draft_sync_candidates_conn(&conn, account)
    }

    fn get_draft_sync_item(&self, id: DraftId) -> Result<Option<DraftSyncItem>, CoreError> {
        let conn = self.lock();
        get_draft_sync_item_conn(&conn, id)
    }

    fn find_draft_by_server_uid(
        &self,
        account: AccountId,
        uid_validity: u32,
        uid: u32,
    ) -> Result<Option<Draft>, CoreError> {
        let conn = self.lock();
        find_draft_by_server_uid_conn(&conn, account, uid_validity, uid)
    }

    fn find_drafts_by_message_id(
        &self,
        account: AccountId,
        message_id: &str,
    ) -> Result<Vec<Draft>, CoreError> {
        let conn = self.lock();
        find_drafts_by_message_id_conn(&conn, account, message_id)
    }

    fn mark_draft_synced(
        &self,
        id: DraftId,
        server_uid: Option<u32>,
        uid_validity: Option<u32>,
        local_hash: &str,
        server_hash: &str,
    ) -> Result<(), CoreError> {
        let conn = self.lock();
        mark_draft_synced_conn(&conn, id, server_uid, uid_validity, local_hash, server_hash)
    }

    fn set_draft_sync_state(
        &self,
        id: DraftId,
        state: DraftSyncState,
        error: Option<&str>,
    ) -> Result<(), CoreError> {
        let conn = self.lock();
        set_draft_sync_state_conn(&conn, id, state, error)
    }

    fn bump_draft_sync_attempt(&self, id: DraftId, error: &str) -> Result<u32, CoreError> {
        let conn = self.lock();
        bump_draft_sync_attempt_conn(&conn, id, error)
    }

    fn clear_draft_sync_attempts(&self, id: DraftId) -> Result<(), CoreError> {
        let conn = self.lock();
        clear_draft_sync_attempts_conn(&conn, id)
    }

    fn create_conflict_copy(
        &self,
        source: DraftId,
        content: &DraftContent,
    ) -> Result<DraftId, CoreError> {
        let conn = self.lock();
        create_conflict_copy_conn(&conn, source, content)
    }

    fn tombstone_draft(&self, id: DraftId) -> Result<(), CoreError> {
        let conn = self.lock();
        tombstone_draft_conn(&conn, id)
    }

    fn finalize_deleted_draft(&self, id: DraftId) -> Result<(), CoreError> {
        let conn = self.lock();
        delete_draft_conn(&conn, id)
    }

    fn reset_draft_server_identity(&self, account: AccountId) -> Result<u32, CoreError> {
        let conn = self.lock();
        reset_draft_server_identity_conn(&conn, account)
    }

    fn promote_local_only_to_dirty(&self, account: Option<AccountId>) -> Result<u32, CoreError> {
        let conn = self.lock();
        promote_local_only_to_dirty_conn(&conn, account)
    }

    fn promote_conflict_copy(&self, id: DraftId) -> Result<(), CoreError> {
        let conn = self.lock();
        promote_conflict_copy_conn(&conn, id)
    }
}
