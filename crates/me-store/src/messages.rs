//! 邮件摘要仓储：批量入库、去重补 UID、键集分页、标志、正文缓存与删除。

use me_core::CoreError;
use me_core::MAX_PAGE_SIZE;
use me_core::model::{
    AccountId, Address, DateSource, FlagOp, Flags, FolderId, MessageId, MessageSummary, NewMessage,
    now_unix,
};
use me_core::port::{BodyCoverage, BodyJob, IndexRow, MessageRepo, MoveOrigin};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use std::path::PathBuf;

use crate::db::WritableStore;
use crate::{StoreResult, pending, search, storage};

const SUMMARY_COLUMNS: &str = "id, account_id, folder_id, uid, message_id, subject, from_addr, \
     from_name, to_addrs, cc_addrs, date, date_source, size, flags, snippet, has_attachment, \
     body_path";

const SUMMARY_COLUMNS_M: &str = "m.id, m.account_id, m.folder_id, m.uid, m.message_id, \
     m.subject, m.from_addr, m.from_name, m.to_addrs, m.cc_addrs, m.date, m.date_source, \
     m.size, m.flags, m.snippet, m.has_attachment, m.body_path";

pub(crate) fn parse_addresses(json: Option<&str>) -> Vec<Address> {
    json.and_then(|raw| serde_json::from_str::<Vec<Address>>(raw).ok())
        .unwrap_or_default()
}

pub(crate) fn encode_addresses(addresses: &[Address]) -> String {
    serde_json::to_string(addresses).unwrap_or_else(|_| "[]".to_string())
}

pub(crate) fn row_to_summary(row: &Row<'_>) -> rusqlite::Result<MessageSummary> {
    let from_addr: Option<String> = row.get(6)?;
    let from_name: Option<String> = row.get(7)?;
    let to_addrs: Option<String> = row.get(8)?;
    let cc_addrs: Option<String> = row.get(9)?;
    let date_source: String = row.get(11)?;
    let body_path: Option<String> = row.get(16)?;
    let from = from_addr.map(|addr| Address {
        name: from_name,
        addr,
    });
    Ok(MessageSummary {
        id: MessageId(row.get(0)?),
        account: AccountId(row.get(1)?),
        folder: FolderId(row.get(2)?),
        uid: row.get::<_, Option<i64>>(3)?.map(|v| v as u32),
        message_id: row.get(4)?,
        subject: row.get(5)?,
        from,
        to: parse_addresses(to_addrs.as_deref()),
        cc: parse_addresses(cc_addrs.as_deref()),
        date: row.get(10)?,
        date_source: DateSource::parse(&date_source),
        size: row.get::<_, Option<i64>>(12)?.map(|v| v as u32),
        flags: Flags::from_bits(row.get::<_, i64>(13)? as u32),
        snippet: row.get(14)?,
        has_attachment: row.get::<_, i64>(15)? != 0,
        body_cached: body_path.is_some(),
    })
}

// ---------------------------------------------------------------------------
// 查询
// ---------------------------------------------------------------------------

pub(crate) fn get_message_conn(
    conn: &Connection,
    id: MessageId,
) -> Result<Option<MessageSummary>, CoreError> {
    let sql = format!("SELECT {SUMMARY_COLUMNS} FROM messages WHERE id = ?1");
    conn.query_row(&sql, params![id.0], row_to_summary)
        .optional()
        .store()
}

fn summary_by_id(conn: &Connection, id: i64) -> Result<Option<MessageSummary>, CoreError> {
    get_message_conn(conn, MessageId(id))
}

/// 头部文本或附件名中含 U+FFFD 的摘要：旧版本把 GBK 等非 UTF-8 邮件按 UTF-8 解码。
pub(crate) fn broken_header_summaries_conn(
    conn: &Connection,
    limit: u32,
) -> Result<Vec<MessageSummary>, CoreError> {
    let limit = limit.clamp(1, 2_000);
    let sql = format!(
        "SELECT {SUMMARY_COLUMNS} FROM messages
          WHERE instr(subject, char(65533)) > 0
             OR instr(COALESCE(from_name, ''), char(65533)) > 0
             OR instr(COALESCE(from_addr, ''), char(65533)) > 0
             OR instr(COALESCE(to_addrs, ''), char(65533)) > 0
             OR instr(COALESCE(cc_addrs, ''), char(65533)) > 0
             OR instr(COALESCE(snippet, ''), char(65533)) > 0
             OR EXISTS (
                    SELECT 1 FROM attachments
                     WHERE attachments.message_id = messages.id
                       AND instr(COALESCE(attachments.filename, ''), char(65533)) > 0
                )
          ORDER BY id
          LIMIT ?1"
    );
    let mut stmt = conn.prepare(&sql).store()?;
    let rows = stmt
        .query_map(params![i64::from(limit)], row_to_summary)
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    Ok(rows)
}

/// 覆盖摘要头部并同步 FTS 头部列（正文由 `set_body_cached` 单独更新）。
pub(crate) fn update_headers_conn(
    conn: &mut Connection,
    id: MessageId,
    subject: Option<&str>,
    from: Option<&Address>,
    to: &[Address],
    cc: &[Address],
) -> Result<(), CoreError> {
    let from_addr = from.map(|address| address.addr.as_str());
    let from_name = from.and_then(|address| address.name.as_deref());
    let to_addrs = encode_addresses(to);
    let cc_addrs = encode_addresses(cc);
    let tx = conn
        .transaction()
        .map_err(|e| storage(format!("开始头部修复事务失败: {e}")))?;
    tx.execute(
        "UPDATE messages
            SET subject = ?2, from_addr = ?3, from_name = ?4, to_addrs = ?5, cc_addrs = ?6
          WHERE id = ?1",
        params![id.0, subject, from_addr, from_name, to_addrs, cc_addrs],
    )
    .store()?;
    search::update_head_fts(&tx, id.0, subject, from_addr, &to_addrs)?;
    tx.commit().store()?;
    Ok(())
}

pub(crate) fn list_messages_conn(
    conn: &Connection,
    folder: FolderId,
    cursor: Option<(i64, i64)>,
    limit: u32,
) -> Result<Vec<MessageSummary>, CoreError> {
    let limit = limit.clamp(1, MAX_PAGE_SIZE);
    let rows = match cursor {
        None => {
            let sql = format!(
                "SELECT {SUMMARY_COLUMNS} FROM messages
                  WHERE folder_id = ?1
                  ORDER BY date DESC, id DESC
                  LIMIT ?2"
            );
            let mut stmt = conn.prepare(&sql).store()?;
            stmt.query_map(params![folder.0, i64::from(limit)], row_to_summary)
                .store()?
                .collect::<Result<Vec<_>, _>>()
                .store()?
        }
        Some((date, id)) => {
            let sql = format!(
                "SELECT {SUMMARY_COLUMNS} FROM messages
                  WHERE folder_id = ?1 AND (date < ?2 OR (date = ?3 AND id < ?4))
                  ORDER BY date DESC, id DESC
                  LIMIT ?5"
            );
            let mut stmt = conn.prepare(&sql).store()?;
            stmt.query_map(
                params![folder.0, date, date, id, i64::from(limit)],
                row_to_summary,
            )
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?
        }
    };
    Ok(rows)
}

pub(crate) fn unread_count_conn(conn: &Connection, folder: FolderId) -> Result<u32, CoreError> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE folder_id = ?1 AND (flags & 1) = 0",
            params![folder.0],
            |row| row.get(0),
        )
        .store()?;
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

fn placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn body_coverage_conn(
    conn: &Connection,
    accounts: &[AccountId],
    folders: &[FolderId],
) -> Result<BodyCoverage, CoreError> {
    let mut parts = Vec::new();
    let mut values: Vec<i64> = Vec::new();
    if !accounts.is_empty() {
        parts.push(format!("account_id IN ({})", placeholders(accounts.len())));
        values.extend(accounts.iter().map(|a| a.0));
    }
    if !folders.is_empty() {
        parts.push(format!("folder_id IN ({})", placeholders(folders.len())));
        values.extend(folders.iter().map(|f| f.0));
    }

    let total_sql = if parts.is_empty() {
        "SELECT COUNT(*) FROM messages".to_string()
    } else {
        format!(
            "SELECT COUNT(*) FROM messages WHERE {}",
            parts.join(" AND ")
        )
    };
    let total: i64 = conn
        .query_row(&total_sql, params_from_iter(values.iter()), |row| {
            row.get(0)
        })
        .store()?;

    let indexed_sql = if parts.is_empty() {
        "SELECT COUNT(*) FROM messages WHERE body_path IS NOT NULL".to_string()
    } else {
        format!(
            "SELECT COUNT(*) FROM messages WHERE {} AND body_path IS NOT NULL",
            parts.join(" AND ")
        )
    };
    let indexed: i64 = conn
        .query_row(&indexed_sql, params_from_iter(values.iter()), |row| {
            row.get(0)
        })
        .store()?;

    Ok(BodyCoverage {
        indexed: u32::try_from(indexed).unwrap_or(u32::MAX),
        total: u32::try_from(total).unwrap_or(u32::MAX),
    })
}

pub(crate) fn body_jobs_conn(
    conn: &Connection,
    account: AccountId,
    limit: u32,
) -> Result<Vec<BodyJob>, CoreError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, folder_id, uid FROM messages
              WHERE account_id = ?1 AND body_path IS NULL AND uid IS NOT NULL
                AND folder_id NOT IN (SELECT id FROM folders WHERE role = 'drafts')
              ORDER BY folder_id, uid
              LIMIT ?2",
        )
        .store()?;
    let rows = stmt
        .query_map(params![account.0, i64::from(limit)], |row| {
            Ok(BodyJob {
                message: MessageId(row.get(0)?),
                folder: FolderId(row.get(1)?),
                uid: row.get::<_, i64>(2)? as u32,
            })
        })
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    Ok(rows)
}

pub(crate) fn pending_body_count_conn(
    conn: &Connection,
    account: AccountId,
) -> Result<u32, CoreError> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages
              WHERE account_id = ?1 AND body_path IS NULL AND uid IS NOT NULL
                AND folder_id NOT IN (SELECT id FROM folders WHERE role = 'drafts')",
            params![account.0],
            |row| row.get(0),
        )
        .store()?;
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

pub(crate) fn index_rows_conn(
    conn: &Connection,
    account: Option<AccountId>,
) -> Result<Vec<IndexRow>, CoreError> {
    let (sql, param) = match account {
        Some(_) => (
            "SELECT id, subject, from_addr, COALESCE(to_addrs, ''), body_path
               FROM messages WHERE account_id = ?1 ORDER BY id"
                .to_string(),
            Some(account.unwrap().0),
        ),
        None => (
            "SELECT id, subject, from_addr, COALESCE(to_addrs, ''), body_path
               FROM messages ORDER BY id"
                .to_string(),
            None,
        ),
    };
    let mut stmt = conn.prepare(&sql).store()?;
    let map_row = |row: &Row<'_>| {
        let body_path: Option<String> = row.get(4)?;
        Ok(IndexRow {
            id: MessageId(row.get(0)?),
            subject: row.get(1)?,
            from_addr: row.get(2)?,
            to_addrs: row.get(3)?,
            body_path: body_path.map(PathBuf::from),
        })
    };
    let rows = match param {
        Some(account_id) => stmt
            .query_map(params![account_id], map_row)
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?,
        None => stmt
            .query_map([], map_row)
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?,
    };
    Ok(rows)
}

pub(crate) fn message_ids_conn(
    conn: &Connection,
    account: AccountId,
) -> Result<Vec<MessageId>, CoreError> {
    let mut stmt = conn
        .prepare("SELECT id FROM messages WHERE account_id = ?1 ORDER BY id")
        .store()?;
    let rows = stmt
        .query_map(params![account.0], |row| Ok(MessageId(row.get(0)?)))
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// 批量入库
// ---------------------------------------------------------------------------

pub(crate) fn insert_messages_conn(
    conn: &mut Connection,
    account: AccountId,
    folder: FolderId,
    messages: &[NewMessage],
    new_last_uid: u32,
) -> Result<Vec<MessageSummary>, CoreError> {
    let tx = conn
        .transaction()
        .map_err(|e| storage(format!("开始邮件入库事务失败: {e}")))?;

    let mut result_ids: Vec<i64> = Vec::with_capacity(messages.len());

    {
        let mut insert = tx
            .prepare(
                "INSERT INTO messages(
                     account_id, folder_id, uid, message_id, subject, from_addr, from_name,
                     to_addrs, cc_addrs, date, date_source, size, flags, snippet,
                     has_attachment, body_path
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, NULL, ?14, NULL)",
            )
            .store()?;

        for message in messages {
            // 去重：同 folder + message_id 且 uid IS NULL 的占位行 → 补 UID，不插入。
            if let (Some(uid), Some(message_id)) = (message.uid, message.message_id.as_deref()) {
                let existing: Option<i64> = tx
                    .query_row(
                        "SELECT id FROM messages
                          WHERE folder_id = ?1 AND message_id = ?2 AND uid IS NULL
                          LIMIT 1",
                        params![folder.0, message_id],
                        |row| row.get(0),
                    )
                    .optional()
                    .store()?;
                if let Some(existing_id) = existing {
                    tx.execute(
                        "UPDATE messages SET uid = ?2 WHERE id = ?1",
                        params![existing_id, i64::from(uid)],
                    )
                    .store()?;
                    // 服务器出现新 UID 即说明移动已生效，相关待同步操作不再需要。
                    tx.execute(
                        "DELETE FROM pending_ops WHERE message_id = ?1",
                        params![existing_id],
                    )
                    .store()?;
                    result_ids.push(existing_id);
                    continue;
                }
            }

            let from_addr = message.from.as_ref().map(|a| a.addr.as_str());
            let from_name = message.from.as_ref().and_then(|a| a.name.as_deref());
            insert
                .execute(params![
                    account.0,
                    folder.0,
                    message.uid.map(i64::from),
                    message.message_id,
                    message.subject,
                    from_addr,
                    from_name,
                    encode_addresses(&message.to),
                    encode_addresses(&message.cc),
                    message.date,
                    message.date_source.as_str(),
                    message.size.map(i64::from),
                    i64::from(message.flags.bits()),
                    i64::from(message.has_attachment),
                ])
                .store()?;
            let id = tx.last_insert_rowid();
            search::insert_head_fts(
                &tx,
                id,
                message.subject.as_deref(),
                from_addr,
                &encode_addresses(&message.to),
            )?;
            result_ids.push(id);
        }
    }

    // 只在一批数据完整写入后推进 `last_uid`；用 MAX 防止回退。
    tx.execute(
        "UPDATE folders SET last_uid = MAX(last_uid, ?2) WHERE id = ?1",
        params![folder.0, i64::from(new_last_uid)],
    )
    .store()?;

    let mut summaries = Vec::with_capacity(result_ids.len());
    for id in result_ids {
        if let Some(summary) = summary_by_id(&tx, id)? {
            summaries.push(summary);
        }
    }
    tx.commit().store()?;
    Ok(summaries)
}

// ---------------------------------------------------------------------------
// 标志
// ---------------------------------------------------------------------------

pub(crate) fn apply_local_flags_conn(
    conn: &mut Connection,
    ids: &[MessageId],
    op: FlagOp,
) -> Result<Vec<(MessageId, Flags)>, CoreError> {
    let tx = conn
        .transaction()
        .map_err(|e| storage(format!("开始标志事务失败: {e}")))?;
    let mut changed = Vec::new();
    for &id in ids {
        let current: Option<i64> = tx
            .query_row(
                "SELECT flags FROM messages WHERE id = ?1",
                params![id.0],
                |row| row.get(0),
            )
            .optional()
            .store()?;
        let Some(bits) = current else { continue };
        let new_flags = op.apply(Flags::from_bits(bits as u32));
        tx.execute(
            "UPDATE messages SET flags = ?2 WHERE id = ?1",
            params![id.0, i64::from(new_flags.bits())],
        )
        .store()?;
        pending::enqueue_flags_conn(&tx, id, new_flags)?;
        changed.push((id, new_flags));
    }
    tx.commit().store()?;
    Ok(changed)
}

pub(crate) fn apply_server_flags_conn(
    conn: &mut Connection,
    folder: FolderId,
    items: &[(u32, Flags)],
) -> Result<Vec<(MessageId, Flags)>, CoreError> {
    let tx = conn
        .transaction()
        .map_err(|e| storage(format!("开始服务器标志事务失败: {e}")))?;
    let mut changed = Vec::new();
    for &(uid, flags) in items {
        let row: Option<(i64, i64)> = tx
            .query_row(
                "SELECT id, flags FROM messages WHERE folder_id = ?1 AND uid = ?2",
                params![folder.0, i64::from(uid)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .store()?;
        let Some((id, bits)) = row else { continue };

        // 本地有未同步标志时以本地目标态为准，忽略服务器值。
        let has_pending: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pending_flags WHERE message_id = ?1)",
                params![id],
                |row| row.get(0),
            )
            .store()?;
        if has_pending {
            continue;
        }

        let current = Flags::from_bits(bits as u32);
        if current == flags {
            continue;
        }
        tx.execute(
            "UPDATE messages SET flags = ?2 WHERE id = ?1",
            params![id, i64::from(flags.bits())],
        )
        .store()?;
        changed.push((MessageId(id), flags));
    }
    tx.commit().store()?;
    Ok(changed)
}

// ---------------------------------------------------------------------------
// 删除 / 移动
// ---------------------------------------------------------------------------

pub(crate) fn clear_folder_conn(conn: &mut Connection, folder: FolderId) -> Result<u32, CoreError> {
    let tx = conn
        .transaction()
        .map_err(|e| storage(format!("开始清空文件夹事务失败: {e}")))?;
    let count: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE folder_id = ?1",
            params![folder.0],
            |row| row.get(0),
        )
        .store()?;
    search::delete_folder_fts(&tx, folder)?;
    tx.execute(
        "DELETE FROM messages WHERE folder_id = ?1",
        params![folder.0],
    )
    .store()?;
    tx.commit().store()?;
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

pub(crate) fn move_to_trash_conn(
    conn: &mut Connection,
    origins: &[MoveOrigin],
    trash: FolderId,
) -> Result<(), CoreError> {
    let tx = conn
        .transaction()
        .map_err(|e| storage(format!("开始移入废件箱事务失败: {e}")))?;
    for origin in origins {
        let account: Option<i64> = tx
            .query_row(
                "SELECT account_id FROM messages WHERE id = ?1",
                params![origin.message.0],
                |row| row.get(0),
            )
            .optional()
            .store()?;
        let Some(account) = account else { continue };

        let updated = tx
            .execute(
                "UPDATE messages SET folder_id = ?2, uid = NULL WHERE id = ?1",
                params![origin.message.0, trash.0],
            )
            .store()?;
        if updated == 0 {
            continue;
        }
        tx.execute(
            "INSERT INTO pending_ops(
                 account_id, kind, message_id, source_folder_id, source_folder_name,
                 source_uid, target_folder_id, attempts, last_error, created_at
             ) VALUES (?1, 'move_to_trash', ?2, ?3, ?4, ?5, ?6, 0, NULL, ?7)",
            params![
                account,
                origin.message.0,
                origin.source_folder.0,
                origin.source_folder_name,
                i64::from(origin.source_uid),
                trash.0,
                now_unix(),
            ],
        )
        .store()?;
    }
    tx.commit().store()?;
    Ok(())
}

pub(crate) fn delete_permanent_conn(
    conn: &mut Connection,
    ids: &[MessageId],
) -> Result<Vec<MoveOrigin>, CoreError> {
    let tx = conn
        .transaction()
        .map_err(|e| storage(format!("开始永久删除事务失败: {e}")))?;
    let mut origins = Vec::new();
    let mut deleted_ids = Vec::new();

    for &id in ids {
        let row: Option<(i64, i64, Option<i64>, String)> = tx
            .query_row(
                "SELECT m.account_id, m.folder_id, m.uid, f.name
                   FROM messages m JOIN folders f ON f.id = m.folder_id
                  WHERE m.id = ?1",
                params![id.0],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .store()?;
        let Some((account, folder_id, source_uid, folder_name)) = row else {
            continue;
        };

        // 先入队再删行：`pending_ops.message_id` 随后被置 NULL，操作本身保留。
        tx.execute(
            "INSERT INTO pending_ops(
                 account_id, kind, message_id, source_folder_id, source_folder_name,
                 source_uid, target_folder_id, attempts, last_error, created_at
             ) VALUES (?1, 'delete_permanent', ?2, ?3, ?4, ?5, NULL, 0, NULL, ?6)",
            params![
                account,
                id.0,
                folder_id,
                folder_name,
                source_uid,
                now_unix(),
            ],
        )
        .store()?;

        origins.push(MoveOrigin {
            message: id,
            source_folder: FolderId(folder_id),
            source_folder_name: folder_name,
            // `MoveOrigin::source_uid` 是非可空的；`uid IS NULL`（本地已移动、待确认）
            // 的行用 0 表示“无服务器 UID”。真正的空值已写入 `pending_ops.source_uid`。
            source_uid: source_uid.map(|v| v as u32).unwrap_or(0),
        });
        deleted_ids.push(id.0);
    }

    search::delete_ids_fts(&tx, &deleted_ids)?;
    for id in deleted_ids {
        tx.execute("DELETE FROM messages WHERE id = ?1", params![id])
            .store()?;
    }
    tx.commit().store()?;
    Ok(origins)
}

// ---------------------------------------------------------------------------
// 正文缓存
// ---------------------------------------------------------------------------

pub(crate) fn set_body_cached_conn(
    conn: &mut Connection,
    id: MessageId,
    body_path: &str,
    text: &str,
    snippet: Option<&str>,
) -> Result<(), CoreError> {
    let tx = conn
        .transaction()
        .map_err(|e| storage(format!("开始正文缓存事务失败: {e}")))?;
    // 不变量：body_path 非空 ⇔ FTS body 已写入。先确保 FTS 行存在（幂等）。
    tx.execute(
        "INSERT OR IGNORE INTO messages_fts(rowid, subject, body, from_addr, to_addrs)
         SELECT id, subject, '', from_addr, to_addrs FROM messages WHERE id = ?1",
        params![id.0],
    )
    .store()?;
    tx.execute(
        "UPDATE messages_fts SET body = ?2 WHERE rowid = ?1",
        params![id.0, text],
    )
    .store()?;
    tx.execute(
        "UPDATE messages SET body_path = ?2, snippet = ?3 WHERE id = ?1",
        params![id.0, body_path, snippet],
    )
    .store()?;
    tx.commit().store()?;
    Ok(())
}

fn reset_body_cache_sql(account: Option<AccountId>) -> (&'static str, &'static str) {
    match account {
        Some(_) => (
            "SELECT id FROM messages WHERE account_id = ?1 AND body_path IS NOT NULL",
            "UPDATE messages SET body_path = NULL WHERE account_id = ?1 AND body_path IS NOT NULL",
        ),
        None => (
            "SELECT id FROM messages WHERE body_path IS NOT NULL",
            "UPDATE messages SET body_path = NULL WHERE body_path IS NOT NULL",
        ),
    }
}

pub(crate) fn reset_body_cache_conn(
    conn: &mut Connection,
    account: Option<AccountId>,
) -> Result<u32, CoreError> {
    let tx = conn
        .transaction()
        .map_err(|e| storage(format!("开始清理正文缓存事务失败: {e}")))?;
    let (select_sql, update_sql) = reset_body_cache_sql(account);

    let ids: Vec<i64> = {
        let mut stmt = tx.prepare(select_sql).store()?;
        match account {
            Some(account) => stmt
                .query_map(params![account.0], |row| row.get(0))
                .store()?
                .collect::<Result<Vec<_>, _>>()
                .store()?,
            None => stmt
                .query_map([], |row| row.get(0))
                .store()?
                .collect::<Result<Vec<_>, _>>()
                .store()?,
        }
    };

    for chunk in ids.chunks(500) {
        let sql = format!(
            "UPDATE messages_fts SET body = '' WHERE rowid IN ({})",
            placeholders(chunk.len())
        );
        tx.execute(&sql, params_from_iter(chunk.iter())).store()?;

        // 缓存清理后附件文件已删除，元数据必须同步失效，
        // 否则 UI 会认为附件仍在本机、点击打开才报错。
        let sql = format!(
            "UPDATE attachments SET local_path = NULL WHERE message_id IN ({})",
            placeholders(chunk.len())
        );
        tx.execute(&sql, params_from_iter(chunk.iter())).store()?;
    }

    let changed = match account {
        Some(account) => tx.execute(update_sql, params![account.0]).store()?,
        None => tx.execute(update_sql, []).store()?,
    };
    tx.commit().store()?;
    Ok(u32::try_from(changed).unwrap_or(u32::MAX))
}

// ---------------------------------------------------------------------------
// 移动确认
// ---------------------------------------------------------------------------

pub(crate) fn confirm_moved_uid_conn(
    conn: &Connection,
    folder: FolderId,
    message_id: &str,
    new_uid: u32,
) -> Result<Option<MessageId>, CoreError> {
    let changed = conn
        .execute(
            "UPDATE messages SET uid = ?3
              WHERE folder_id = ?1 AND message_id = ?2 AND uid IS NULL",
            params![folder.0, message_id, i64::from(new_uid)],
        )
        .store()?;
    if changed != 1 {
        return Ok(None);
    }
    let id: Option<i64> = conn
        .query_row(
            "SELECT id FROM messages
              WHERE folder_id = ?1 AND message_id = ?2 AND uid = ?3",
            params![folder.0, message_id, i64::from(new_uid)],
            |row| row.get(0),
        )
        .optional()
        .store()?;
    Ok(id.map(MessageId))
}

// ---------------------------------------------------------------------------
// SearchRepo 需要的未分类查询辅助
// ---------------------------------------------------------------------------

/// 供 `search.rs` 复用：按动态 WHERE 列出摘要。
pub(crate) fn search_rows(
    conn: &Connection,
    join_fts: bool,
    where_sql: &str,
    values: &[SqlValue],
    limit: u32,
) -> Result<Vec<MessageSummary>, CoreError> {
    let join = if join_fts {
        " JOIN messages_fts ON messages_fts.rowid = m.id"
    } else {
        ""
    };
    let sql = format!(
        "SELECT {SUMMARY_COLUMNS_M} FROM messages m{join}
          WHERE {where_sql}
          ORDER BY m.date DESC, m.id DESC
          LIMIT ?"
    );
    let mut all: Vec<SqlValue> = values.to_vec();
    all.push(SqlValue::Integer(i64::from(limit)));
    let mut stmt = conn.prepare(&sql).store()?;
    let rows = stmt
        .query_map(params_from_iter(all.iter()), row_to_summary)
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Trait impl
// ---------------------------------------------------------------------------

impl MessageRepo for WritableStore {
    fn insert_messages(
        &self,
        account: AccountId,
        folder: FolderId,
        messages: &[NewMessage],
        new_last_uid: u32,
    ) -> Result<Vec<MessageSummary>, CoreError> {
        let mut conn = self.lock();
        insert_messages_conn(&mut conn, account, folder, messages, new_last_uid)
    }

    fn list_messages(
        &self,
        folder: FolderId,
        cursor: Option<(i64, i64)>,
        limit: u32,
    ) -> Result<Vec<MessageSummary>, CoreError> {
        let conn = self.lock();
        list_messages_conn(&conn, folder, cursor, limit)
    }

    fn get_message(&self, id: MessageId) -> Result<Option<MessageSummary>, CoreError> {
        let conn = self.lock();
        get_message_conn(&conn, id)
    }

    fn messages_with_broken_headers(&self, limit: u32) -> Result<Vec<MessageSummary>, CoreError> {
        let conn = self.lock();
        broken_header_summaries_conn(&conn, limit)
    }

    fn update_message_headers(
        &self,
        id: MessageId,
        subject: Option<&str>,
        from: Option<&Address>,
        to: &[Address],
        cc: &[Address],
    ) -> Result<(), CoreError> {
        let mut conn = self.lock();
        update_headers_conn(&mut conn, id, subject, from, to, cc)
    }

    fn unread_count(&self, folder: FolderId) -> Result<u32, CoreError> {
        let conn = self.lock();
        unread_count_conn(&conn, folder)
    }

    fn body_coverage(
        &self,
        accounts: &[AccountId],
        folders: &[FolderId],
    ) -> Result<BodyCoverage, CoreError> {
        let conn = self.lock();
        body_coverage_conn(&conn, accounts, folders)
    }

    fn apply_local_flags(
        &self,
        ids: &[MessageId],
        op: FlagOp,
    ) -> Result<Vec<(MessageId, Flags)>, CoreError> {
        let mut conn = self.lock();
        apply_local_flags_conn(&mut conn, ids, op)
    }

    fn apply_server_flags(
        &self,
        folder: FolderId,
        items: &[(u32, Flags)],
    ) -> Result<Vec<(MessageId, Flags)>, CoreError> {
        let mut conn = self.lock();
        apply_server_flags_conn(&mut conn, folder, items)
    }

    fn clear_folder(&self, folder: FolderId) -> Result<u32, CoreError> {
        let mut conn = self.lock();
        clear_folder_conn(&mut conn, folder)
    }

    fn move_to_trash(&self, origins: &[MoveOrigin], trash: FolderId) -> Result<(), CoreError> {
        let mut conn = self.lock();
        move_to_trash_conn(&mut conn, origins, trash)
    }

    fn delete_permanent(&self, ids: &[MessageId]) -> Result<Vec<MoveOrigin>, CoreError> {
        let mut conn = self.lock();
        delete_permanent_conn(&mut conn, ids)
    }

    fn body_jobs(&self, account: AccountId, limit: u32) -> Result<Vec<BodyJob>, CoreError> {
        let conn = self.lock();
        body_jobs_conn(&conn, account, limit)
    }

    fn pending_body_count(&self, account: AccountId) -> Result<u32, CoreError> {
        let conn = self.lock();
        pending_body_count_conn(&conn, account)
    }

    fn set_body_cached(
        &self,
        id: MessageId,
        body_path: &str,
        text: &str,
        snippet: Option<&str>,
    ) -> Result<(), CoreError> {
        let mut conn = self.lock();
        set_body_cached_conn(&mut conn, id, body_path, text, snippet)
    }

    fn reset_body_cache(&self, account: Option<AccountId>) -> Result<u32, CoreError> {
        let mut conn = self.lock();
        reset_body_cache_conn(&mut conn, account)
    }

    fn index_rows(&self, account: Option<AccountId>) -> Result<Vec<IndexRow>, CoreError> {
        let conn = self.lock();
        index_rows_conn(&conn, account)
    }

    fn message_ids(&self, account: AccountId) -> Result<Vec<MessageId>, CoreError> {
        let conn = self.lock();
        message_ids_conn(&conn, account)
    }

    fn confirm_moved_uid(
        &self,
        folder: FolderId,
        message_id: &str,
        new_uid: u32,
    ) -> Result<Option<MessageId>, CoreError> {
        let conn = self.lock();
        confirm_moved_uid_conn(&conn, folder, message_id, new_uid)
    }
}
