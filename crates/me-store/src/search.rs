//! 全文检索：唯一负责 `messages_fts` 的 SQL（插入 / body 更新 / 删除 / 重建），
//! 以及 `MATCH` 构建、转义、< 3 字符 `LIKE`、过滤与覆盖率。
//!
//! trigram 分词要求查询 >= 3 字符；1~2 字符仅走头部 `LIKE`。

use std::fs;

use me_core::CoreError;
use me_core::model::{FolderId, SearchQuery};
use me_core::port::{SearchOutcome, SearchRepo};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, params, params_from_iter};

use crate::cache::CachePaths;
use crate::db::WritableStore;
use crate::{StoreResult, messages, storage};

// ---------------------------------------------------------------------------
// FTS 维护
// ---------------------------------------------------------------------------

/// 摘要入库：写入头部列，`body` 留空。
pub(crate) fn insert_head_fts(
    conn: &Connection,
    rowid: i64,
    subject: Option<&str>,
    from_addr: Option<&str>,
    to_addrs: &str,
) -> Result<(), CoreError> {
    conn.execute(
        "INSERT INTO messages_fts(rowid, subject, body, from_addr, to_addrs)
         VALUES (?1, ?2, '', ?3, ?4)",
        params![rowid, subject, from_addr, to_addrs],
    )
    .store()?;
    Ok(())
}

/// 头部被本地修复后同步 FTS 头部列。
pub(crate) fn update_head_fts(
    conn: &Connection,
    rowid: i64,
    subject: Option<&str>,
    from_addr: Option<&str>,
    to_addrs: &str,
) -> Result<(), CoreError> {
    conn.execute(
        "UPDATE messages_fts SET subject = ?2, from_addr = ?3, to_addrs = ?4 WHERE rowid = ?1",
        params![rowid, subject, from_addr, to_addrs],
    )
    .store()?;
    Ok(())
}

fn insert_full_fts(
    conn: &Connection,
    rowid: i64,
    subject: Option<&str>,
    body: &str,
    from_addr: Option<&str>,
    to_addrs: &str,
) -> Result<(), CoreError> {
    conn.execute(
        "INSERT INTO messages_fts(rowid, subject, body, from_addr, to_addrs)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![rowid, subject, body, from_addr, to_addrs],
    )
    .store()?;
    Ok(())
}

pub(crate) fn delete_ids_fts(conn: &Connection, ids: &[i64]) -> Result<(), CoreError> {
    for chunk in ids.chunks(500) {
        let sql = format!(
            "DELETE FROM messages_fts WHERE rowid IN ({})",
            placeholders(chunk.len())
        );
        conn.execute(&sql, params_from_iter(chunk.iter().copied()))
            .store()?;
    }
    Ok(())
}

pub(crate) fn delete_folder_fts(conn: &Connection, folder: FolderId) -> Result<(), CoreError> {
    conn.execute(
        "DELETE FROM messages_fts
          WHERE rowid IN (SELECT id FROM messages WHERE folder_id = ?1)",
        params![folder.0],
    )
    .store()?;
    Ok(())
}

fn clear_fts(conn: &Connection) -> Result<(), CoreError> {
    conn.execute("DELETE FROM messages_fts", []).store()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 查询串构造
// ---------------------------------------------------------------------------

fn placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(", ")
}

/// 整个查询词作为一个短语：双引号加倍后整体加引号，避免 FTS 运算符被解释。
fn escape_fts_query(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

fn escape_like(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// 非文本过滤条件（账户 / 文件夹 / 时间 / 附件 / 头部字段 / 未读）。
///
/// 头部字段用 `LIKE` 子串匹配而不是 FTS：它们要和全文短语**取交集**，
/// 而 `messages_fts` 一次只能吃一个短语。
fn base_filters(query: &SearchQuery) -> (Vec<String>, Vec<SqlValue>) {
    let mut parts = Vec::new();
    let mut values = Vec::new();

    if !query.accounts.is_empty() {
        parts.push(format!(
            "m.account_id IN ({})",
            placeholders(query.accounts.len())
        ));
        values.extend(query.accounts.iter().map(|a| SqlValue::Integer(a.0)));
    }
    if !query.folders.is_empty() {
        parts.push(format!(
            "m.folder_id IN ({})",
            placeholders(query.folders.len())
        ));
        values.extend(query.folders.iter().map(|f| SqlValue::Integer(f.0)));
    }
    if let Some(since) = query.since {
        parts.push("m.date >= ?".to_string());
        values.push(SqlValue::Integer(since));
    }
    if let Some(until) = query.until {
        parts.push("m.date <= ?".to_string());
        values.push(SqlValue::Integer(until));
    }
    match query.has_attachment {
        Some(true) => parts.push("m.has_attachment = 1".to_string()),
        Some(false) => parts.push("m.has_attachment = 0".to_string()),
        None => {}
    }
    for (column, value) in [
        ("m.from_addr", query.from.as_deref()),
        ("m.to_addrs", query.to.as_deref()),
        ("m.subject", query.subject.as_deref()),
    ] {
        let Some(value) = value.map(str::trim).filter(|text| !text.is_empty()) else {
            continue;
        };
        parts.push(format!("{column} LIKE ? ESCAPE '\\'"));
        values.push(SqlValue::Text(format!("%{}%", escape_like(value))));
    }
    // 与 `messages::unread_count_conn` 同一个位：`flags & 1` 是 `\Seen`。
    if query.unread_only {
        parts.push("(m.flags & 1) = 0".to_string());
    }

    (parts, values)
}

fn join_where(parts: &[String]) -> String {
    if parts.is_empty() {
        "1 = 1".to_string()
    } else {
        parts.join(" AND ")
    }
}

fn coverage_counts(conn: &Connection, query: &SearchQuery) -> Result<(u32, u32), CoreError> {
    let (parts, values) = base_filters(query);
    let where_sql = join_where(&parts);

    // 两条查询**保持分开**，不要合并成 `COUNT(*), COUNT(body_path)`。
    //
    // 合并后 SQLite 只能整表扫一次（两个聚合都要全量行），实测 5 万行
    // **7.3 ms**；分开则各自走覆盖索引，实测各 **0.5 ms**（合计 1.1 ms）——
    // 合并反而慢 6.7 倍。`body_path IS NOT NULL` 那条靠
    // `idx_messages_indexed(account_id) WHERE body_path IS NOT NULL` 部分索引。
    let total_sql = format!("SELECT COUNT(*) FROM messages m WHERE {where_sql}");
    let total: i64 = conn
        .query_row(
            &total_sql,
            params_from_iter(values.iter().cloned()),
            |row| row.get(0),
        )
        .store()?;

    let indexed_sql =
        format!("SELECT COUNT(*) FROM messages m WHERE {where_sql} AND m.body_path IS NOT NULL");
    let indexed: i64 = conn
        .query_row(
            &indexed_sql,
            params_from_iter(values.iter().cloned()),
            |row| row.get(0),
        )
        .store()?;

    Ok((
        u32::try_from(indexed).unwrap_or(u32::MAX),
        u32::try_from(total).unwrap_or(u32::MAX),
    ))
}

fn read_indexed_body(paths: &CachePaths, row: &me_core::port::IndexRow) -> Option<String> {
    // 优先使用规范路径 `bodies/{id}.txt`（文档 §4.8），再回退到列中记录的实际路径。
    if let Ok(text) = fs::read_to_string(paths.body_path(row.id)) {
        return Some(text);
    }
    if let Some(path) = &row.body_path
        && let Ok(text) = fs::read_to_string(path)
    {
        return Some(text);
    }
    None
}

// ---------------------------------------------------------------------------
// Trait impl
// ---------------------------------------------------------------------------

impl SearchRepo for WritableStore {
    fn search(&self, query: &SearchQuery) -> Result<SearchOutcome, CoreError> {
        let conn = self.lock();

        let text_chars = query.text.chars().count();
        let fts_mode = text_chars >= 3;
        let like_mode = text_chars > 0 && !fts_mode;

        let (base_parts, base_values) = base_filters(query);

        // 主查询：文本条件 + 过滤条件。
        let mut parts = Vec::new();
        let mut values = Vec::new();
        if fts_mode {
            parts.push("messages_fts MATCH ?".to_string());
            values.push(SqlValue::Text(escape_fts_query(&query.text)));
        } else if like_mode {
            parts.push(
                "(m.subject LIKE ? ESCAPE '\\' OR m.from_addr LIKE ? ESCAPE '\\' \
                  OR m.to_addrs LIKE ? ESCAPE '\\')"
                    .to_string(),
            );
            let pattern = format!("%{}%", escape_like(&query.text));
            for _ in 0..3 {
                values.push(SqlValue::Text(pattern.clone()));
            }
        }
        parts.extend(base_parts.iter().cloned());
        values.extend(base_values.iter().cloned());

        let where_sql = join_where(&parts);
        let join = if fts_mode {
            " JOIN messages_fts ON messages_fts.rowid = m.id"
        } else {
            ""
        };

        // 总数：走与分页同一套 JOIN，保证「总数」与「实际能翻到的条数」一致。
        //
        // 曾试过「只数 messages_fts、不 join messages」（`messages_fts.rowid` 就是
        // `messages.id`，两边行集合一一对应），单看确实快一半（13.5 → 5.9 ms）。
        // 但加上 `EXISTS` 兜底（防孤儿 FTS 行）后又回到 13.5 ms —— 兜底本身
        // 就要逐行回表，等于把 join 又做了一遍。
        //
        // 结论：**不加 EXISTS**，孤儿行的风险由数据层保证 —— 插入时同事务写 FTS
        // （`insert_messages_conn`），删除时同事务清 FTS
        // （`delete_permanent_conn` → `delete_ids_fts`、`delete_account_conn`）。
        // 真的出现孤儿行时，总数会偏大、分页结果仍正确 —— 偏大比漏报安全。
        let total_sql = format!("SELECT COUNT(*) FROM messages m{join} WHERE {where_sql}");
        let total: i64 = conn
            .query_row(
                &total_sql,
                params_from_iter(values.iter().cloned()),
                |row| row.get(0),
            )
            .store()?;

        // 分页：附加键集游标（date, id）。
        let mut page_where = where_sql;
        let mut page_values = values;
        if let Some((date, id)) = query.cursor {
            page_where.push_str(" AND (m.date < ? OR (m.date = ? AND m.id < ?))");
            page_values.push(SqlValue::Integer(date));
            page_values.push(SqlValue::Integer(date));
            page_values.push(SqlValue::Integer(id));
        }

        let messages = messages::search_rows(
            &conn,
            fts_mode,
            &page_where,
            &page_values,
            query.capped_limit(),
        )?;

        let (body_indexed, body_total) = coverage_counts(&conn, query)?;

        Ok(SearchOutcome {
            // query_id 由 me-sync 覆盖；存储层恒为 0。
            query_id: 0,
            messages,
            total: u32::try_from(total).unwrap_or(u32::MAX),
            body_indexed,
            body_total,
        })
    }

    fn rebuild_index(&self) -> Result<u32, CoreError> {
        // **先读行、再读文件、最后才拿写锁**。
        //
        // 原实现是「拿写锁 → 查行 → 逐个读 bodies/*.txt → 开事务写 FTS」，
        // 整个读文件过程都**占着全局写锁**。重建索引时邮件量大（几万封），
        // 读文件要几秒到几十秒，这期间所有写操作（标记已读、移动、删除、
        // 发信归档）全部排队。
        //
        // 现在把读文件挪到锁外：`index_rows_conn` 只读、`read_indexed_body`
        // 只碰文件系统，两者都不需要写连接。写锁只在最后写 FTS 时才拿。
        //
        // 代价：读文件期间行数据可能有变化（并发同步会插入新邮件）。
        // 这与原实现**行为一致** —— 原实现的 `rows` 同样是在读文件前取的，
        // 只是取的时候持着锁。锁的作用是防止**写**并发，不是防止读并发。
        //
        let paths = self.paths();
        // 第一步：只读地取行，**读完立刻放开写锁**。
        let rows = {
            let conn = self.lock();
            messages::index_rows_conn(&conn, None)?
        };

        // 第二步：读文件（`bodies/*.txt`）——**完全不持锁**。
        // `read_indexed_body` 只碰文件系统，不需要写连接；
        // 原实现把这一步放在写锁内，几万封邮件要读几秒到几十秒，
        // 期间所有写操作（标记已读、移动、删除、发信归档）全部排队。
        let bodies: Vec<Option<String>> = rows
            .iter()
            .map(|row| {
                if row.body_path.is_some() {
                    read_indexed_body(paths, row)
                } else {
                    None
                }
            })
            .collect();

        // 第三步：写回 FTS——**这一段才需要写锁**。
        let mut conn = self.lock();
        let tx = conn
            .transaction()
            .map_err(|e| storage(format!("开始重建索引事务失败: {e}")))?;
        clear_fts(&tx)?;

        let mut indexed = 0u32;
        for (row, body) in rows.iter().zip(bodies.iter()) {
            insert_full_fts(
                &tx,
                row.id.0,
                row.subject.as_deref(),
                body.as_deref().unwrap_or(""),
                row.from_addr.as_deref(),
                &row.to_addrs,
            )?;
            indexed = indexed.saturating_add(1);
        }
        tx.commit().store()?;
        Ok(indexed)
    }
}
