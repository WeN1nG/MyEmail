//! 保存的搜索的仓储（`saved_searches` 表）。
//!
//! 只做存取与结构校验；「只存条件、不做自动动作」是**产品决策**（D42），
//! 由领域层的 `SavedSearchQuery` 类型保证——表里根本没有「动作」这个概念。
//!
//! 名称唯一由 `ux_saved_searches_name` 唯一索引保证（存储层约束，
//! 防竞态——见 `doc/v03架构设计.md` §2.7 的两类存储层约束）。

use me_core::CoreError;
use me_core::model::{
    MAX_SAVED_SEARCH_NAME, SavedSearch, SavedSearchId, SavedSearchQuery, now_unix,
};
use me_core::port::SavedSearchRepo;
use rusqlite::{Connection, Row, params};

use crate::StoreResult;
use crate::db::WritableStore;
use crate::storage;

const COLUMNS: &str = "id, name, query_json, created_at, last_used_at";

/// 判断是不是唯一约束冲突（名称重复）。
///
/// SQLite 的 `extended_code` 是 `SQLITE_CONSTRAINT_UNIQUE`（2067）或
/// `SQLITE_CONSTRAINT_PRIMARYKEY`（1555）；两者都表示「撞了」。
fn is_unique_violation(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::ConstraintViolation,
                extended_code,
            },
            _,
        ) if *extended_code == 2067 || *extended_code == 1555
    )
}

fn map_search(row: &Row<'_>) -> rusqlite::Result<SavedSearch> {
    let id = SavedSearchId(row.get(0)?);
    let name: String = row.get(1)?;
    let query_json: String = row.get(2)?;
    let created_at: i64 = row.get(3)?;
    let last_used_at: i64 = row.get(4)?;
    // **反序列化失败不吞掉**——存进去的 JSON 坏了就是真 bug，
    // 静默跳过会让用户看到「保存的搜索少了一条」却查不出原因。
    let query: SavedSearchQuery = serde_json::from_str(&query_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(SavedSearch {
        id,
        name,
        query,
        created_at,
        last_used_at,
    })
}

impl SavedSearchRepo for WritableStore {
    fn list_saved_searches(&self) -> Result<Vec<SavedSearch>, CoreError> {
        let conn = self.lock();
        list_saved_searches_conn(&conn)
    }

    fn get_saved_search(&self, id: SavedSearchId) -> Result<Option<SavedSearch>, CoreError> {
        let conn = self.lock();
        get_saved_search_conn(&conn, id)
    }

    fn insert_saved_search(&self, search: &SavedSearch) -> Result<SavedSearchId, CoreError> {
        let name = search.name.trim();
        if name.is_empty() {
            return Err(CoreError::storage("保存的搜索名称不能为空"));
        }
        if name.chars().count() > MAX_SAVED_SEARCH_NAME {
            return Err(CoreError::storage(format!(
                "保存的搜索名称不能超过 {MAX_SAVED_SEARCH_NAME} 个字符"
            )));
        }
        let query_json = serde_json::to_string(&search.query)
            .map_err(|e| crate::storage(format!("序列化保存的搜索失败: {e}")))?;

        let conn = self.lock();
        let inserted = conn.execute(
            "INSERT INTO saved_searches(name, query_json, created_at, last_used_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![name, query_json, search.created_at, search.last_used_at],
        );
        match inserted {
            Ok(_) => Ok(SavedSearchId(conn.last_insert_rowid())),
            Err(err) => {
                // 名称重复由唯一索引拦下。**在仓储层翻译成可读的错误**——
                // `CoreError::Storage` 的 `user_message()` 是通用文案
                //（「本地数据库操作失败」），UI 拿不到「名字重复」这个信息。
                if is_unique_violation(&err) {
                    Err(CoreError::validation(format!(
                        "保存的搜索「{name}」已经存在了，换个名字吧"
                    )))
                } else {
                    Err(storage(err))
                }
            }
        }
    }

    fn delete_saved_search(&self, id: SavedSearchId) -> Result<(), CoreError> {
        let conn = self.lock();
        // 不存在时**不报错**（幂等）——删除是破坏性操作，
        // 重复点删除不该弹错误。
        conn.execute("DELETE FROM saved_searches WHERE id = ?1", params![id.0])
            .store()?;
        Ok(())
    }

    fn touch_saved_search(&self, id: SavedSearchId) -> Result<(), CoreError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE saved_searches SET last_used_at = ?1 WHERE id = ?2",
            params![now_unix(), id.0],
        )
        .store()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 内部：给 me-sync 用的连接级函数
// ---------------------------------------------------------------------------

/// 列出全部保存的搜索（连接级）。
pub(crate) fn list_saved_searches_conn(conn: &Connection) -> Result<Vec<SavedSearch>, CoreError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {COLUMNS} FROM saved_searches ORDER BY last_used_at DESC, id DESC"
        ))
        .store()?;
    let rows = stmt
        .query_map([], map_search)
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    Ok(rows)
}

/// 读出一条保存的搜索（连接级）。
pub(crate) fn get_saved_search_conn(
    conn: &Connection,
    id: SavedSearchId,
) -> Result<Option<SavedSearch>, CoreError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {COLUMNS} FROM saved_searches WHERE id = ?1"
        ))
        .store()?;
    let mut rows = stmt.query(params![id.0]).store()?;
    match rows.next().store()? {
        Some(row) => Ok(Some(map_search(row).store()?)),
        None => Ok(None),
    }
}
