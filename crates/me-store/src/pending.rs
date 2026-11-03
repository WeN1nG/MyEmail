//! 待同步队列：`pending_flags`（目标态）与 `pending_ops`（本地已生效操作）。

use me_core::CoreError;
use me_core::model::{AccountId, Flags, FolderId, MessageId, now_unix};
use me_core::port::{PendingOp, PendingOpKind, PendingRepo};
use rusqlite::{Connection, Row, params};

use crate::StoreResult;
use crate::db::WritableStore;

// ---------------------------------------------------------------------------
// pending_flags
// ---------------------------------------------------------------------------

/// UPSERT 目标态；不累加、不叠加。新变更会把 attempts / last_error 重置。
pub(crate) fn enqueue_flags_conn(
    conn: &Connection,
    message: MessageId,
    target: Flags,
) -> Result<(), CoreError> {
    conn.execute(
        "INSERT INTO pending_flags(message_id, target_flags, attempts, last_error, updated_at)
         VALUES (?1, ?2, 0, NULL, ?3)
         ON CONFLICT(message_id) DO UPDATE SET
             target_flags = excluded.target_flags,
             attempts = 0,
             last_error = NULL,
             updated_at = excluded.updated_at",
        params![message.0, i64::from(target.bits()), now_unix()],
    )
    .store()?;
    Ok(())
}

pub(crate) fn pending_flags_conn(
    conn: &Connection,
    limit: u32,
) -> Result<Vec<(MessageId, Flags, u32)>, CoreError> {
    let mut stmt = conn
        .prepare(
            "SELECT message_id, target_flags, attempts
               FROM pending_flags
              ORDER BY updated_at, message_id
              LIMIT ?1",
        )
        .store()?;
    let rows = stmt
        .query_map(params![i64::from(limit)], |row| {
            Ok((
                MessageId(row.get(0)?),
                Flags::from_bits(row.get::<_, i64>(1)? as u32),
                row.get::<_, i64>(2)? as u32,
            ))
        })
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    Ok(rows)
}

pub(crate) fn clear_pending_flags_conn(
    conn: &Connection,
    message: MessageId,
) -> Result<(), CoreError> {
    conn.execute(
        "DELETE FROM pending_flags WHERE message_id = ?1",
        params![message.0],
    )
    .store()?;
    Ok(())
}

pub(crate) fn bump_pending_flags_attempt_conn(
    conn: &Connection,
    message: MessageId,
    error: &str,
) -> Result<(), CoreError> {
    conn.execute(
        "UPDATE pending_flags
            SET attempts = attempts + 1, last_error = ?2, updated_at = ?3
          WHERE message_id = ?1",
        params![message.0, error, now_unix()],
    )
    .store()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// pending_ops
// ---------------------------------------------------------------------------

pub(crate) fn enqueue_op_conn(conn: &Connection, op: &PendingOp) -> Result<i64, CoreError> {
    conn.execute(
        "INSERT INTO pending_ops(
             account_id, kind, message_id, source_folder_id, source_folder_name,
             source_uid, target_folder_id, attempts, last_error, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            op.account.0,
            op.kind.as_str(),
            op.message.map(|m| m.0),
            op.source_folder.map(|f| f.0),
            op.source_folder_name,
            op.source_uid.map(i64::from),
            op.target_folder.map(|f| f.0),
            i64::from(op.attempts),
            op.last_error,
            now_unix(),
        ],
    )
    .store()?;
    Ok(conn.last_insert_rowid())
}

fn map_pending_op(row: &Row<'_>) -> rusqlite::Result<PendingOp> {
    let kind_str: String = row.get(2)?;
    let kind = PendingOpKind::parse(&kind_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("未知 pending_ops.kind: {kind_str}"),
            )),
        )
    })?;
    Ok(PendingOp {
        id: row.get(0)?,
        account: AccountId(row.get(1)?),
        kind,
        message: row.get::<_, Option<i64>>(3)?.map(MessageId),
        source_folder: row.get::<_, Option<i64>>(4)?.map(FolderId),
        source_folder_name: row.get(5)?,
        source_uid: row.get::<_, Option<i64>>(6)?.map(|v| v as u32),
        target_folder: row.get::<_, Option<i64>>(7)?.map(FolderId),
        attempts: row.get::<_, i64>(8)? as u32,
        last_error: row.get(9)?,
    })
}

pub(crate) fn pending_ops_conn(
    conn: &Connection,
    account: AccountId,
    limit: u32,
) -> Result<Vec<PendingOp>, CoreError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, account_id, kind, message_id, source_folder_id, source_folder_name,
                    source_uid, target_folder_id, attempts, last_error
               FROM pending_ops
              WHERE account_id = ?1
              ORDER BY id
              LIMIT ?2",
        )
        .store()?;
    let rows = stmt
        .query_map(params![account.0, i64::from(limit)], map_pending_op)
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    Ok(rows)
}

pub(crate) fn complete_op_conn(conn: &Connection, id: i64) -> Result<(), CoreError> {
    conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![id])
        .store()?;
    Ok(())
}

pub(crate) fn bump_op_attempt_conn(
    conn: &Connection,
    id: i64,
    error: &str,
) -> Result<(), CoreError> {
    conn.execute(
        "UPDATE pending_ops SET attempts = attempts + 1, last_error = ?2 WHERE id = ?1",
        params![id, error],
    )
    .store()?;
    Ok(())
}

/// 源文件夹或目标文件夹消失后，相关操作已无意义，直接清理。
pub(crate) fn cleanup_ops_for_folder_conn(
    conn: &Connection,
    folder: FolderId,
) -> Result<u32, CoreError> {
    let changed = conn
        .execute(
            "DELETE FROM pending_ops WHERE source_folder_id = ?1 OR target_folder_id = ?1",
            params![folder.0],
        )
        .store()?;
    Ok(u32::try_from(changed).unwrap_or(u32::MAX))
}

// ---------------------------------------------------------------------------
// Trait impl
// ---------------------------------------------------------------------------

impl PendingRepo for WritableStore {
    fn enqueue_flags(&self, message: MessageId, target: Flags) -> Result<(), CoreError> {
        let conn = self.lock();
        enqueue_flags_conn(&conn, message, target)
    }

    fn pending_flags(&self, limit: u32) -> Result<Vec<(MessageId, Flags, u32)>, CoreError> {
        let conn = self.lock();
        pending_flags_conn(&conn, limit)
    }

    fn clear_pending_flags(&self, message: MessageId) -> Result<(), CoreError> {
        let conn = self.lock();
        clear_pending_flags_conn(&conn, message)
    }

    fn bump_pending_flags_attempt(&self, message: MessageId, error: &str) -> Result<(), CoreError> {
        let conn = self.lock();
        bump_pending_flags_attempt_conn(&conn, message, error)
    }

    fn enqueue_op(&self, op: &PendingOp) -> Result<i64, CoreError> {
        let conn = self.lock();
        enqueue_op_conn(&conn, op)
    }

    fn pending_ops(&self, account: AccountId, limit: u32) -> Result<Vec<PendingOp>, CoreError> {
        let conn = self.lock();
        pending_ops_conn(&conn, account, limit)
    }

    fn complete_op(&self, id: i64) -> Result<(), CoreError> {
        let conn = self.lock();
        complete_op_conn(&conn, id)
    }

    fn bump_op_attempt(&self, id: i64, error: &str) -> Result<(), CoreError> {
        let conn = self.lock();
        bump_op_attempt_conn(&conn, id, error)
    }

    fn cleanup_ops_for_folder(&self, folder: FolderId) -> Result<u32, CoreError> {
        let conn = self.lock();
        cleanup_ops_for_folder_conn(&conn, folder)
    }
}
