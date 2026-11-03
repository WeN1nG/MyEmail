//! 文件夹仓储。
//!
//! 角色判定在 `me-core`（`FolderRole::from_special_use`）完成，存储层只落库。
//! `role='trash'` 由部分唯一索引保证同账户唯一。

use me_core::CoreError;
use me_core::model::{AccountId, Folder, FolderId, FolderRole};
use me_core::port::{FolderInfo, FolderRepo};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::db::WritableStore;
use crate::search;
use crate::{StoreResult, storage};

const SELECT_COLUMNS: &str =
    "id, account_id, name, delimiter, role, uid_validity, last_uid, deleted_on_server";

fn map_folder(row: &Row<'_>) -> rusqlite::Result<Folder> {
    let delimiter: Option<String> = row.get(3)?;
    let role: String = row.get(4)?;
    Ok(Folder {
        id: FolderId(row.get(0)?),
        account: AccountId(row.get(1)?),
        name: row.get(2)?,
        delimiter: delimiter.and_then(|s| s.chars().next()),
        role: FolderRole::parse(&role),
        uid_validity: row.get::<_, i64>(5)? as u32,
        last_uid: row.get::<_, i64>(6)? as u32,
        deleted_on_server: row.get::<_, i64>(7)? != 0,
    })
}

pub(crate) fn list_folders_conn(
    conn: &Connection,
    account: AccountId,
) -> Result<Vec<Folder>, CoreError> {
    let sql = format!("SELECT {SELECT_COLUMNS} FROM folders WHERE account_id = ?1 ORDER BY name");
    let mut stmt = conn.prepare(&sql).store()?;
    let folders = stmt
        .query_map(params![account.0], map_folder)
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    Ok(folders)
}

pub(crate) fn get_folder_conn(
    conn: &Connection,
    id: FolderId,
) -> Result<Option<Folder>, CoreError> {
    let sql = format!("SELECT {SELECT_COLUMNS} FROM folders WHERE id = ?1");
    conn.query_row(&sql, params![id.0], map_folder)
        .optional()
        .store()
}

pub(crate) fn find_folder_conn(
    conn: &Connection,
    account: AccountId,
    name: &str,
) -> Result<Option<Folder>, CoreError> {
    let sql = format!("SELECT {SELECT_COLUMNS} FROM folders WHERE account_id = ?1 AND name = ?2");
    conn.query_row(&sql, params![account.0, name], map_folder)
        .optional()
        .store()
}

pub(crate) fn upsert_folder_conn(
    conn: &mut Connection,
    account: AccountId,
    info: &FolderInfo,
) -> Result<Folder, CoreError> {
    let derived = FolderRole::from_special_use(&info.attributes, &info.name);
    let delimiter = info.delimiter.map(|c| c.to_string());

    let tx = conn
        .transaction()
        .map_err(|e| storage(format!("开始文件夹事务失败: {e}")))?;

    let existing: Option<(i64, String)> = tx
        .query_row(
            "SELECT id, role FROM folders WHERE account_id = ?1 AND name = ?2",
            params![account.0, info.name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .store()?;

    let id = match existing {
        Some((id, current_role)) => {
            // 派生结果为 Other 时不覆盖已有角色（用户可能手动指定过）。
            let role = if derived == FolderRole::Other {
                FolderRole::parse(&current_role)
            } else {
                derived
            };
            tx.execute(
                "UPDATE folders
                    SET delimiter = ?2, role = ?3, deleted_on_server = 0
                  WHERE id = ?1",
                params![id, delimiter, role.as_str()],
            )
            .store()?;
            id
        }
        None => {
            tx.execute(
                "INSERT INTO folders(account_id, name, delimiter, role)
                 VALUES (?1, ?2, ?3, ?4)",
                params![account.0, info.name, delimiter, derived.as_str()],
            )
            .store()?;
            tx.last_insert_rowid()
        }
    };

    let folder =
        get_folder_conn(&tx, FolderId(id))?.ok_or_else(|| storage("文件夹写入后无法读取"))?;
    tx.commit().store()?;
    Ok(folder)
}

pub(crate) fn rename_folder_conn(
    conn: &Connection,
    id: FolderId,
    new_name: &str,
) -> Result<(), CoreError> {
    let changed = conn
        .execute(
            "UPDATE folders SET name = ?2 WHERE id = ?1",
            params![id.0, new_name],
        )
        .store()?;
    if changed == 0 {
        return Err(storage(format!("文件夹不存在: {}", id.0)));
    }
    Ok(())
}

pub(crate) fn set_folder_role_conn(
    conn: &Connection,
    id: FolderId,
    role: FolderRole,
) -> Result<(), CoreError> {
    let changed = conn
        .execute(
            "UPDATE folders SET role = ?2 WHERE id = ?1",
            params![id.0, role.as_str()],
        )
        .store()?;
    if changed == 0 {
        return Err(storage(format!("文件夹不存在: {}", id.0)));
    }
    Ok(())
}

pub(crate) fn set_folder_uid_state_conn(
    conn: &Connection,
    id: FolderId,
    uid_validity: u32,
    last_uid: u32,
) -> Result<(), CoreError> {
    let changed = conn
        .execute(
            "UPDATE folders SET uid_validity = ?2, last_uid = ?3 WHERE id = ?1",
            params![id.0, i64::from(uid_validity), i64::from(last_uid)],
        )
        .store()?;
    if changed == 0 {
        return Err(storage(format!("文件夹不存在: {}", id.0)));
    }
    Ok(())
}

pub(crate) fn set_folder_deleted_conn(
    conn: &Connection,
    id: FolderId,
    deleted: bool,
) -> Result<(), CoreError> {
    let changed = conn
        .execute(
            "UPDATE folders SET deleted_on_server = ?2 WHERE id = ?1",
            params![id.0, i64::from(deleted)],
        )
        .store()?;
    if changed == 0 {
        return Err(storage(format!("文件夹不存在: {}", id.0)));
    }
    Ok(())
}

/// 删除文件夹及其邮件；FTS 行与邮件一并显式清理。
///
/// `pending_ops.source_folder_id` 由外键 `ON DELETE SET NULL` 保留操作行。
pub(crate) fn delete_folder_conn(conn: &mut Connection, id: FolderId) -> Result<(), CoreError> {
    let tx = conn
        .transaction()
        .map_err(|e| storage(format!("开始删除文件夹事务失败: {e}")))?;
    search::delete_folder_fts(&tx, id)?;
    let changed = tx
        .execute("DELETE FROM folders WHERE id = ?1", params![id.0])
        .store()?;
    if changed == 0 {
        return Err(storage(format!("文件夹不存在: {}", id.0)));
    }
    tx.commit().store()?;
    Ok(())
}

impl FolderRepo for WritableStore {
    fn list_folders(&self, account: AccountId) -> Result<Vec<Folder>, CoreError> {
        let conn = self.lock();
        list_folders_conn(&conn, account)
    }

    fn get_folder(&self, id: FolderId) -> Result<Option<Folder>, CoreError> {
        let conn = self.lock();
        get_folder_conn(&conn, id)
    }

    fn find_folder(&self, account: AccountId, name: &str) -> Result<Option<Folder>, CoreError> {
        let conn = self.lock();
        find_folder_conn(&conn, account, name)
    }

    fn upsert_folder(&self, account: AccountId, info: &FolderInfo) -> Result<Folder, CoreError> {
        let mut conn = self.lock();
        upsert_folder_conn(&mut conn, account, info)
    }

    fn rename_folder(&self, id: FolderId, new_name: &str) -> Result<(), CoreError> {
        let conn = self.lock();
        rename_folder_conn(&conn, id, new_name)
    }

    fn set_folder_role(&self, id: FolderId, role: FolderRole) -> Result<(), CoreError> {
        let conn = self.lock();
        set_folder_role_conn(&conn, id, role)
    }

    fn set_folder_uid_state(
        &self,
        id: FolderId,
        uid_validity: u32,
        last_uid: u32,
    ) -> Result<(), CoreError> {
        let conn = self.lock();
        set_folder_uid_state_conn(&conn, id, uid_validity, last_uid)
    }

    fn set_folder_deleted(&self, id: FolderId, deleted: bool) -> Result<(), CoreError> {
        let conn = self.lock();
        set_folder_deleted_conn(&conn, id, deleted)
    }

    fn delete_folder(&self, id: FolderId) -> Result<(), CoreError> {
        let mut conn = self.lock();
        delete_folder_conn(&mut conn, id)
    }
}
