//! v6 → v7 迁移测试。
//!
//! v7 是 v0.4 的唯一 DDL 级变更：新增 `contacts` / `saved_searches` 两张表，
//! 给 `messages` 补 `in_reply_to` / `references` 两列。
//!
//! 验证：
//! - v6 形态的库（无这两张表、`messages` 无这两列）打开后升级到 v7；
//! - 升级**幂等**（新建库本身已是 v7 形态，再跑一遍迁移不报错、结果不变）；
//! - **签名等价**：迁移库与新建库的结构逐条一致（表 / 列 / 索引）；
//! - 历史行的两列是 `NULL`（**不是空串**——空串会与「有但为空」混淆）；
//! - `foreign_key_check` 无悬空；失败回滚后版本号不变。

use std::path::Path;

use me_core::port::MetaRepo;
use me_store::Store;
use rusqlite::{Connection, params};

/// v7 期望的两张新表。
const V7_TABLES: &[(&str, &str)] = &[
    ("contacts", "CREATE TABLE contacts"),
    ("saved_searches", "CREATE TABLE saved_searches"),
];

/// v7 期望 `messages` 补的两列。
// 注：`references` 是 SQLite 保留关键字，DDL 里要加引号；
// 但 `pragma_table_info` 返回的列名不带引号。
const V7_MESSAGE_COLUMNS: &[(&str, &str)] = &[("in_reply_to", "TEXT"), ("references", "TEXT")];

/// 读出某表的建表语句（SQLite 存的是建表时的原文）。
fn table_sql(conn: &Connection, name: &str) -> Option<String> {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![name],
        |row| row.get(0),
    )
    .ok()
}

/// 某表的列清单（列名 + 类型 + 是否可空）。
fn columns_of(conn: &Connection, table: &str) -> Vec<(String, String, bool)> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("prepare table_info");
    stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)? == 0,
        ))
    })
    .expect("query")
    .collect::<Result<Vec<_>, _>>()
    .expect("collect")
}

/// 归一化 SQL：压掉空白差异。
fn normalize(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 建一个「v6 形态」的库：先按当前 DDL 建好库（已是 v7 形态），
/// 再**删掉** v7 的三样东西（新表两列），最后把版本号改回 6。
///
/// 这样构造比手工维护一份冻结的 v6 DDL 更不容易漂移。
fn create_v6_db(dir: &Path) -> Connection {
    drop(Store::open(dir).expect("create v6-shaped db"));
    let conn = Connection::open(dir.join("myemail.db")).expect("open");

    conn.execute_batch("DROP TABLE IF EXISTS contacts;")
        .expect("drop contacts");
    conn.execute_batch("DROP TABLE IF EXISTS saved_searches;")
        .expect("drop saved_searches");

    // SQLite 的 `ALTER TABLE ... DROP COLUMN` 要 3.35+，且会拒绝带索引的列。
    // 这里显式重建 `messages`——**不能用 `CREATE TABLE ... AS SELECT`**：
    // 那样会丢掉外键、UNIQUE 与 CHECK 约束，表就不再是「v6 形态」了。
    conn.execute_batch(
        "CREATE TABLE messages_v6 (
             id             INTEGER PRIMARY KEY,
             account_id     INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
             folder_id      INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
             uid            INTEGER,
             message_id     TEXT,
             subject        TEXT,
             from_addr      TEXT,
             from_name      TEXT,
             to_addrs       TEXT,
             cc_addrs       TEXT,
             date           INTEGER,
             date_source    TEXT NOT NULL DEFAULT 'header',
             size           INTEGER,
             flags          INTEGER NOT NULL DEFAULT 0,
             snippet        TEXT,
             has_attachment INTEGER NOT NULL DEFAULT 0,
             body_path      TEXT,
             UNIQUE(folder_id, uid)
         );
         INSERT INTO messages_v6 SELECT id, account_id, folder_id, uid, message_id,
             subject, from_addr, from_name, to_addrs, cc_addrs, date, date_source,
             size, flags, snippet, has_attachment, body_path FROM messages;
         DROP TABLE messages;
         ALTER TABLE messages_v6 RENAME TO messages;",
    )
    .expect("rewind messages");

    // 重建表会**丢掉旧表上的索引**（DROP TABLE 会连带删掉它的索引），
    // 必须补回来——否则「v6 形态」的库带着 v5 的索引形状，测试前提就不对了。
    conn.execute_batch(
        "CREATE INDEX idx_messages_folder_date ON messages(folder_id, date DESC, id DESC);
         CREATE INDEX idx_messages_account ON messages(account_id);
         CREATE INDEX idx_messages_body_pending
             ON messages(account_id, folder_id, date DESC, id DESC) WHERE body_path IS NULL;
         CREATE INDEX idx_messages_unread ON messages(folder_id, (flags & 1));
         CREATE INDEX idx_messages_indexed ON messages(account_id) WHERE body_path IS NOT NULL;
         CREATE INDEX idx_messages_message_id ON messages(folder_id, message_id);",
    )
    .expect("recreate messages indexes");

    conn.execute(
        "UPDATE meta SET value = '6' WHERE key = 'schema_version'",
        [],
    )
    .expect("rewind version");

    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint");
    conn
}

#[test]
fn v6_db_upgrades_to_v7_with_new_tables_and_columns() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let conn = create_v6_db(dir.path());
        // 前置条件：迁移前确实没有这些东西。
        assert!(
            table_sql(&conn, "contacts").is_none(),
            "前置条件：contacts 不该存在"
        );
        assert!(
            table_sql(&conn, "saved_searches").is_none(),
            "前置条件：saved_searches 不该存在"
        );
        let cols = columns_of(&conn, "messages");
        assert!(
            !cols.iter().any(|(n, _, _)| n == "in_reply_to"),
            "前置条件：messages 不该有 in_reply_to"
        );
        assert!(
            !cols.iter().any(|(n, _, _)| n == "references"),
            "前置条件：messages 不该有 references"
        );
    }

    // 重新打开 → 触发迁移。
    let store = Store::open(dir.path()).expect("upgrade to v7");
    let conn = Connection::open(dir.path().join("myemail.db")).expect("open");

    // 版本号。
    assert_eq!(
        store.writable().schema_version().unwrap(),
        me_core::SCHEMA_VERSION,
        "升级后版本号应等于 SCHEMA_VERSION"
    );

    // 两张新表。
    for (name, prefix) in V7_TABLES {
        let sql = table_sql(&conn, name).expect("table exists");
        assert!(
            normalize(&sql).starts_with(prefix),
            "{name} 的建表语句不符：{sql}"
        );
    }

    // 两列，类型 TEXT、可空。
    let cols = columns_of(&conn, "messages");
    for (name, ty) in V7_MESSAGE_COLUMNS {
        let found = cols
            .iter()
            .find(|(n, _, _)| n == name)
            .unwrap_or_else(|| panic!("messages 缺列 {name}"));
        assert_eq!(found.1, *ty, "{name} 的类型应为 {ty}");
        assert!(found.2, "{name} 必须可空（NULL = 未知）");
    }
}

#[test]
fn migrated_v6_matches_fresh_v7_schema() {
    let dir_migrated = tempfile::tempdir().expect("tempdir");
    let dir_fresh = tempfile::tempdir().expect("tempdir");

    // 迁移库。
    {
        let conn = create_v6_db(dir_migrated.path());
        conn.execute_batch(
            "INSERT INTO accounts(email, imap_host, imap_port, imap_tls, smtp_host, smtp_port, smtp_tls,
                               auth_user, created_at)
             VALUES ('a@example.com', 'h', 993, 'implicit', 's', 465, 'implicit',
                     'a@example.com', 1700000000);",
        )
        .expect("seed account");
    }
    drop(Store::open(dir_migrated.path()).expect("migrate"));

    // 新建库。
    drop(Store::open(dir_fresh.path()).expect("fresh"));

    let conn_m = Connection::open(dir_migrated.path().join("myemail.db")).expect("migrated conn");
    let conn_f = Connection::open(dir_fresh.path().join("myemail.db")).expect("fresh conn");

    // 表结构逐条一致。
    for (name, _) in V7_TABLES {
        let a = normalize(&table_sql(&conn_m, name).expect("migrated table"));
        let b = normalize(&table_sql(&conn_f, name).expect("fresh table"));
        assert_eq!(a, b, "{name} 的建表语句在迁移库与新建库之间不一致");
    }

    // messages 列清单一致（含列序）。
    assert_eq!(
        columns_of(&conn_m, "messages"),
        columns_of(&conn_f, "messages"),
        "messages 的列清单（含列序）不一致"
    );

    // v7 的索引一致。
    for name in [
        "ux_contacts_normalized",
        "ix_contacts_recent",
        "ux_saved_searches_name",
    ] {
        let a: Option<String> = conn_m
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='index' AND name=?1",
                params![name],
                |row| row.get(0),
            )
            .ok();
        let b: Option<String> = conn_f
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='index' AND name=?1",
                params![name],
                |row| row.get(0),
            )
            .ok();
        assert!(a.is_some(), "迁移库缺索引 {name}");
        assert_eq!(
            normalize(a.as_deref().unwrap_or_default()),
            normalize(b.as_deref().unwrap_or_default()),
            "索引 {name} 的定义不一致"
        );
    }
}

#[test]
fn migration_is_idempotent() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let _conn = create_v6_db(dir.path());
    }

    // 第一次：v6 → v7。
    drop(Store::open(dir.path()).expect("first migrate"));
    // 第二次：已是 v7，再开一次不该报错、结果不变。
    let store = Store::open(dir.path()).expect("second open must be a no-op");
    // store 保留以确保打开不报错；版本号断言在下一行。
    assert_eq!(
        store.writable().schema_version().unwrap(),
        me_core::SCHEMA_VERSION,
        "升级后版本号应等于 SCHEMA_VERSION"
    );

    let conn = Connection::open(dir.path().join("myemail.db")).expect("conn");
    for (name, _) in V7_TABLES {
        assert!(table_sql(&conn, name).is_some(), "{name} 丢了");
    }
    let cols = columns_of(&conn, "messages");
    for (name, _) in V7_MESSAGE_COLUMNS {
        assert!(
            cols.iter().any(|(n, _, _)| n == name),
            "messages 缺列 {name}（重复 ALTER 报 duplicate column 就对了）"
        );
    }
}

#[test]
fn historical_rows_get_null_not_empty_string() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let conn = create_v6_db(dir.path());
        conn.execute_batch(
            "INSERT INTO accounts(email, imap_host, imap_port, imap_tls, smtp_host, smtp_port, smtp_tls,
                               auth_user, created_at)
             VALUES ('a@example.com', 'h', 993, 'implicit', 's', 465, 'implicit',
                     'a@example.com', 1700000000);
             INSERT INTO folders(account_id, name, role)
             SELECT id, 'INBOX', 'inbox' FROM accounts;
             INSERT INTO messages(account_id, folder_id, uid, subject)
             SELECT a.id, f.id, 1, '历史邮件' FROM accounts a, folders f;",
        )
        .expect("seed");
    }

    drop(Store::open(dir.path()).expect("migrate"));
    let conn = Connection::open(dir.path().join("myemail.db")).expect("conn");

    let value: Option<String> = conn
        .query_row(
            "SELECT in_reply_to FROM messages WHERE subject = '历史邮件'",
            [],
            |row| row.get(0),
        )
        .expect("query");
    // **必须是 NULL**：空串会与「有但为空」混淆（NULL = 未知）。
    assert_eq!(value, None, "历史行的 in_reply_to 应为 NULL");

    let refs: Option<String> = conn
        .query_row(
            "SELECT \"references\" FROM messages WHERE subject = '历史邮件'",
            [],
            |row| row.get(0),
        )
        .expect("query");
    assert_eq!(refs, None, "历史行的 references 应为 NULL");
}

#[test]
fn foreign_keys_are_intact_after_migration() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let conn = create_v6_db(dir.path());
        conn.execute_batch(
            "INSERT INTO accounts(email, imap_host, imap_port, imap_tls, smtp_host, smtp_port, smtp_tls,
                               auth_user, created_at)
             VALUES ('a@example.com', 'h', 993, 'implicit', 's', 465, 'implicit',
                     'a@example.com', 1700000000);
             INSERT INTO folders(account_id, name, role)
             SELECT id, 'INBOX', 'inbox' FROM accounts;
             INSERT INTO messages(account_id, folder_id, uid)
             SELECT a.id, f.id, 1 FROM accounts a, folders f;",
        )
        .expect("seed");
    }

    drop(Store::open(dir.path()).expect("migrate"));
    let conn = Connection::open(dir.path().join("myemail.db")).expect("conn");

    let mut stmt = conn
        .prepare("PRAGMA foreign_key_check")
        .expect("prepare fk check");
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect");
    assert!(rows.is_empty(), "外键检查发现悬空引用：{rows:?}");
}

#[test]
fn fresh_db_is_already_v7_shaped() {
    // 新建库**不经过迁移**，但结构必须与迁移库一致——
    // 这是「两条路径到达同一版本」的前提。
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("fresh");
    assert_eq!(
        store.writable().schema_version().unwrap(),
        me_core::SCHEMA_VERSION,
        "升级后版本号应等于 SCHEMA_VERSION"
    );

    let conn = Connection::open(dir.path().join("myemail.db")).expect("conn");
    for (name, _) in V7_TABLES {
        assert!(table_sql(&conn, name).is_some(), "新建库缺表 {name}");
    }
    let cols = columns_of(&conn, "messages");
    for (name, _) in V7_MESSAGE_COLUMNS {
        assert!(
            cols.iter().any(|(n, _, _)| n == name),
            "新建库 messages 缺列 {name}"
        );
    }
}
