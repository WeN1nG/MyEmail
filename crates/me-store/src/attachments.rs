//! 附件元数据仓储（文件本体由 `cache` 模块负责）。

use me_core::CoreError;
use me_core::model::{Attachment, MessageId};
use me_core::port::AttachmentRepo;
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::path::{Path, PathBuf};

use crate::db::WritableStore;
use crate::{StoreResult, storage};

fn map_attachment(row: &Row<'_>) -> rusqlite::Result<Attachment> {
    let local_path: Option<String> = row.get(6)?;
    Ok(Attachment {
        part_index: row.get::<_, i64>(0)? as u32,
        filename: row.get(1)?,
        mime_type: row.get(2)?,
        size: row.get::<_, Option<i64>>(3)?.map(|v| v as u32),
        is_inline: row.get::<_, i64>(4)? != 0,
        content_id: row.get(5)?,
        local_path: local_path.map(PathBuf::from),
    })
}

const SELECT_COLUMNS: &str =
    "part_index, filename, mime_type, size, is_inline, content_id, local_path";

pub(crate) fn list_attachments_conn(
    conn: &Connection,
    message: MessageId,
) -> Result<Vec<Attachment>, CoreError> {
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM attachments WHERE message_id = ?1 ORDER BY part_index"
    );
    let mut stmt = conn.prepare(&sql).store()?;
    let rows = stmt
        .query_map(params![message.0], map_attachment)
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    Ok(rows)
}

pub(crate) fn upsert_attachments_conn(
    conn: &mut Connection,
    message: MessageId,
    attachments: &[Attachment],
) -> Result<(), CoreError> {
    let tx = conn
        .transaction()
        .map_err(|e| storage(format!("开始附件事务失败: {e}")))?;
    for attachment in attachments {
        tx.execute(
            "INSERT INTO attachments(
                 message_id, part_index, filename, mime_type, size, local_path, is_inline, content_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(message_id, part_index) DO UPDATE SET
                 filename = excluded.filename,
                 mime_type = excluded.mime_type,
                 size = excluded.size,
                 is_inline = excluded.is_inline,
                 content_id = excluded.content_id",
            params![
                message.0,
                i64::from(attachment.part_index),
                attachment.filename,
                attachment.mime_type,
                attachment.size.map(i64::from),
                attachment.local_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                i64::from(attachment.is_inline),
                attachment.content_id,
            ],
        )
        .store()?;
    }
    tx.commit().store()?;
    Ok(())
}

pub(crate) fn set_attachment_path_conn(
    conn: &Connection,
    message: MessageId,
    part_index: u32,
    local_path: &Path,
) -> Result<(), CoreError> {
    let changed = conn
        .execute(
            "UPDATE attachments SET local_path = ?3 WHERE message_id = ?1 AND part_index = ?2",
            params![
                message.0,
                i64::from(part_index),
                local_path.to_string_lossy()
            ],
        )
        .store()?;
    if changed == 0 {
        return Err(storage(format!(
            "附件不存在: message={}, part={part_index}",
            message.0
        )));
    }
    Ok(())
}

pub(crate) fn rewrite_attachment_paths_conn(
    conn: &mut Connection,
    old_dir: &Path,
    new_dir: &Path,
) -> Result<u32, CoreError> {
    let tx = conn
        .transaction()
        .map_err(|e| storage(format!("开始附件路径重写事务失败: {e}")))?;
    let rows: Vec<(i64, i64, String)> = {
        let mut stmt = tx
            .prepare(
                "SELECT message_id, part_index, local_path FROM attachments
                  WHERE local_path IS NOT NULL",
            )
            .store()?;
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?
    };
    let mut updated = 0u32;
    for (message_id, part_index, local_path) in rows {
        let path = PathBuf::from(&local_path);
        let Ok(rest) = path.strip_prefix(old_dir) else {
            continue;
        };
        let rewritten = new_dir.join(rest);
        tx.execute(
            "UPDATE attachments SET local_path = ?3 WHERE message_id = ?1 AND part_index = ?2",
            params![
                message_id,
                part_index,
                rewritten.to_string_lossy().into_owned()
            ],
        )
        .store()?;
        updated += 1;
    }
    tx.commit().store()?;
    Ok(updated)
}

pub(crate) fn get_attachment_conn(
    conn: &Connection,
    message: MessageId,
    part_index: u32,
) -> Result<Option<Attachment>, CoreError> {
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM attachments WHERE message_id = ?1 AND part_index = ?2"
    );
    conn.query_row(
        &sql,
        params![message.0, i64::from(part_index)],
        map_attachment,
    )
    .optional()
    .store()
}

impl AttachmentRepo for WritableStore {
    fn list_attachments(&self, message: MessageId) -> Result<Vec<Attachment>, CoreError> {
        let conn = self.lock();
        list_attachments_conn(&conn, message)
    }

    fn upsert_attachments(
        &self,
        message: MessageId,
        attachments: &[Attachment],
    ) -> Result<(), CoreError> {
        let mut conn = self.lock();
        upsert_attachments_conn(&mut conn, message, attachments)
    }

    fn set_attachment_path(
        &self,
        message: MessageId,
        part_index: u32,
        local_path: &Path,
    ) -> Result<(), CoreError> {
        let conn = self.lock();
        set_attachment_path_conn(&conn, message, part_index, local_path)
    }

    fn rewrite_attachment_paths(&self, old_dir: &Path, new_dir: &Path) -> Result<u32, CoreError> {
        let mut conn = self.lock();
        rewrite_attachment_paths_conn(&mut conn, old_dir, new_dir)
    }

    fn get_attachment(
        &self,
        message: MessageId,
        part_index: u32,
    ) -> Result<Option<Attachment>, CoreError> {
        let conn = self.lock();
        get_attachment_conn(&conn, message, part_index)
    }
}
