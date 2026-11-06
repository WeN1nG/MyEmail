//! 联系人仓储（`contacts` 表）。
//!
//! 联系人是**本地派生数据**：由正文补全落库 / 发信归档时累积。
//! 永不出网、不参与 AI 请求；`ClearCache` **不清理**它（与 D30 一致）。
//!
//! 去重规则：`contacts.normalized`（小写）唯一——邮箱大小写不敏感，
//! 但 `addr` 保留原样用于显示。重复出现只更新 `last_seen` 与 `seen_count`，
//! 不新增行。
//!
//! **必须批量 UPSERT**：累积发生在**正文补全的写库路径**上，
//! 一封一封写会把补全吞吐拖垮（见 `doc/versiondoc/v4开发指南.md` §10）。

use me_core::CoreError;
use me_core::model::{Contact, ContactId};
use me_core::port::ContactRepo;
use rusqlite::{Connection, Row, params};

use crate::StoreResult;
use crate::db::WritableStore;

const COLUMNS: &str = "id, addr, normalized, display_name, last_seen, seen_count";

/// 补全下拉一次取多少条。
const MAX_LIST_LIMIT: u32 = 50;

fn map_contact(row: &Row<'_>) -> rusqlite::Result<Contact> {
    Ok(Contact {
        id: ContactId(row.get(0)?),
        addr: row.get(1)?,
        normalized: row.get(2)?,
        display_name: row.get(3)?,
        last_seen: row.get(4)?,
        seen_count: row.get::<_, i64>(5)? as u32,
    })
}

impl ContactRepo for WritableStore {
    fn upsert_contacts(&self, contacts: &[Contact]) -> Result<(), CoreError> {
        if contacts.is_empty() {
            return Ok(());
        }
        let conn = self.lock();
        upsert_contacts_conn(&conn, contacts)
    }

    fn list_contacts(&self, prefix: &str, limit: u32) -> Result<Vec<Contact>, CoreError> {
        let conn = self.lock();
        list_contacts_conn(&conn, prefix, limit)
    }

    fn remove_contact(&self, id: ContactId) -> Result<(), CoreError> {
        let conn = self.lock();
        conn.execute("DELETE FROM contacts WHERE id = ?1", params![id.0])
            .store()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 内部：给 me-sync 用的连接级函数
// ---------------------------------------------------------------------------

/// 批量 UPSERT 联系人（连接级）。
///
/// 用 `INSERT ... ON CONFLICT(normalized) DO UPDATE`——一次语句处理一行，
/// **不逐条 SELECT**（那会让 N 封邮件变成 2N 次查询）。
/// `seen_count` 累加 1，`last_seen` 取新的时间戳，
/// `display_name` **只在原来为空时**才填（后来的空名不该抹掉已有的名字）。
pub(crate) fn upsert_contacts_conn(
    conn: &Connection,
    contacts: &[Contact],
) -> Result<(), CoreError> {
    // 用 `prepare` + `execute` 逐条跑 UPSERT，**不显式开事务**——
    // 单条 UPSERT 本身是原子的；显式事务需要 `&mut Connection`，
    // 而这里的调用点在写库路径上已经持有锁。
    // 性能上：一次 `prepare` 复用语句，比 N 次 `execute` 拼 SQL 快得多。
    let mut stmt = conn
        .prepare(
            "INSERT INTO contacts(addr, normalized, display_name, last_seen, seen_count)
             VALUES (?1, ?2, ?3, ?4, 1)
             ON CONFLICT(normalized) DO UPDATE SET
                 last_seen = MAX(contacts.last_seen, excluded.last_seen),
                 seen_count = contacts.seen_count + 1,
                 display_name = COALESCE(contacts.display_name, excluded.display_name)",
        )
        .store()?;
    for c in contacts {
        let normalized = Contact::normalize(&c.addr);
        if normalized.is_empty() {
            continue;
        }
        stmt.execute(params![
            c.addr.trim(),
            normalized,
            c.display_name,
            c.last_seen
        ])
        .store()?;
    }
    Ok(())
}

/// 按前缀列出联系人（连接级）。
///
/// 排序：`last_seen` 最近 > `seen_count` 高 > 姓名包含匹配 > 邮箱前缀匹配。
/// 前缀匹配对**邮箱**与**显示名**都试——用户可能记的是「张」而不是邮箱前缀。
pub(crate) fn list_contacts_conn(
    conn: &Connection,
    prefix: &str,
    limit: u32,
) -> Result<Vec<Contact>, CoreError> {
    let limit = limit.clamp(1, MAX_LIST_LIMIT);
    let prefix = prefix.trim();
    if prefix.is_empty() {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM contacts
                 ORDER BY last_seen DESC, seen_count DESC, id DESC
                 LIMIT ?1"
            ))
            .store()?;
        let rows = stmt
            .query_map(params![limit], map_contact)
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?;
        return Ok(rows);
    }

    // `LIKE` 的通配符要转义，否则用户输入 `%` 会匹配全部。
    let like = format!(
        "%{}%",
        prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {COLUMNS} FROM contacts
             WHERE addr LIKE ?1 ESCAPE '\\' OR COALESCE(display_name, '') LIKE ?1 ESCAPE '\\'
             ORDER BY
                 -- 姓名包含匹配优先于邮箱前缀匹配
                 CASE WHEN COALESCE(display_name, '') LIKE ?1 ESCAPE '\\' THEN 0 ELSE 1 END,
                 last_seen DESC, seen_count DESC, id DESC
             LIMIT ?2"
        ))
        .store()?;
    let rows = stmt
        .query_map(params![like, limit], map_contact)
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    Ok(rows)
}

/// 列出某前缀匹配的联系人总数（用于诊断）。
///
/// 只在测试里用——生产路径不需要「匹配到多少条」这个数。
#[cfg(test)]
pub(crate) fn count_contacts_matching_conn(
    conn: &Connection,
    prefix: &str,
) -> Result<u32, CoreError> {
    let like = format!("%{}%", prefix.replace('%', "\\%").replace('_', "\\_"));
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM contacts
             WHERE addr LIKE ?1 ESCAPE '\\' OR COALESCE(display_name, '') LIKE ?1 ESCAPE '\\'",
            params![like],
            |row| row.get(0),
        )
        .store()?;
    Ok(n as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory");
        conn.execute_batch(
            "CREATE TABLE contacts (
                 id            INTEGER PRIMARY KEY,
                 addr          TEXT NOT NULL,
                 normalized    TEXT NOT NULL,
                 display_name  TEXT,
                 last_seen     INTEGER NOT NULL,
                 seen_count    INTEGER NOT NULL DEFAULT 1
             );
             CREATE UNIQUE INDEX ux_contacts_normalized ON contacts(normalized);",
        )
        .expect("create table");
        conn
    }

    fn c(addr: &str, name: Option<&str>, seen: i64) -> Contact {
        Contact {
            id: ContactId(0),
            addr: addr.to_string(),
            normalized: Contact::normalize(addr),
            display_name: name.map(|s| s.to_string()),
            last_seen: seen,
            seen_count: 1,
        }
    }

    #[test]
    fn upsert_is_idempotent_on_normalized_address() {
        let conn = conn();
        upsert_contacts_conn(&conn, &[c("Alice@Example.com", Some("爱丽丝"), 100)]).expect("first");
        upsert_contacts_conn(&conn, &[c("alice@example.com", None, 200)]).expect("second");

        assert_eq!(count_contacts_matching_conn(&conn, "").unwrap(), 1);
        let list = list_contacts_conn(&conn, "", 10).expect("list");
        assert_eq!(list[0].seen_count, 2);
        assert_eq!(list[0].last_seen, 200);
        // 显示名不该被后来的空值抹掉。
        assert_eq!(list[0].display_name.as_deref(), Some("爱丽丝"));
    }

    #[test]
    fn prefix_match_covers_both_address_and_display_name() {
        let conn = conn();
        upsert_contacts_conn(
            &conn,
            &[
                c("zhang@example.com", Some("张三"), 100),
                c("alice@example.com", Some("爱丽丝"), 200),
            ],
        )
        .expect("upsert");

        assert_eq!(count_contacts_matching_conn(&conn, "zhang").unwrap(), 1);
        assert_eq!(count_contacts_matching_conn(&conn, "张").unwrap(), 1);
        assert_eq!(
            count_contacts_matching_conn(&conn, "@example.com").unwrap(),
            2
        );
    }
}
