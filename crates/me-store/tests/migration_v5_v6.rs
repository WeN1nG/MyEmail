//! v5 → v6 迁移测试（纯索引调整）。
//!
//! v6 不改任何列类型 / CHECK / NOT NULL，只调整索引 —— 目的���让
//! `ORDER BY <时间列> DESC, id DESC` 的查询不再退化成临时 B 树排序或整表扫描。
//!
//! 验证：
//! - v5 形态的库（索引还是旧列序）打开后升级到 v6，且索引定义与新建库**逐条一致**；
//! - 升级**幂等**（新建库本身已经是 v6 形态，再跑一遍迁移不报错、结果不变）；
//! - 关键查询的 `EXPLAIN QUERY PLAN` 不再出现 `USE TEMP B-TREE FOR ORDER BY`；
//! - 数据本身不受影响（迁移只动索引）。

use std::path::Path;

use me_core::port::MetaRepo;
use me_store::Store;
use rusqlite::{Connection, params};

/// v6 期望的索引定义。**这是 schema v6 的索引事实源**，
/// 与 `crates/me-store/src/migrations.rs` 的 DDL 和迁移函数必须一致。
const V6_INDEXES: &[(&str, &str)] = &[
    (
        "idx_messages_folder_date",
        "CREATE INDEX idx_messages_folder_date ON messages(folder_id, date DESC, id DESC)",
    ),
    (
        "idx_messages_body_pending",
        "CREATE INDEX idx_messages_body_pending \
         ON messages(account_id, folder_id, date DESC, id DESC) WHERE body_path IS NULL",
    ),
    (
        "idx_messages_unread",
        "CREATE INDEX idx_messages_unread ON messages(folder_id, (flags & 1))",
    ),
    (
        "idx_messages_indexed",
        "CREATE INDEX idx_messages_indexed ON messages(account_id) WHERE body_path IS NOT NULL",
    ),
    (
        "idx_drafts_updated",
        "CREATE INDEX idx_drafts_updated ON drafts(account_id, updated_at DESC, id DESC)",
    ),
    (
        "ix_ai_conversations_updated",
        "CREATE INDEX ix_ai_conversations_updated ON ai_conversations(updated_at DESC, id DESC)",
    ),
    (
        "ix_ai_entities_last_seen",
        "CREATE INDEX ix_ai_entities_last_seen ON ai_entities(last_seen_at DESC, id DESC)",
    ),
];

/// v5 形态的索引定义（旧列序）。迁移必须把它们换成 `V6_INDEXES` 里的形状。
const V5_INDEXES: &[(&str, &str)] = &[
    (
        "idx_messages_folder_date",
        "CREATE INDEX idx_messages_folder_date ON messages(folder_id, date DESC)",
    ),
    (
        "idx_messages_body_pending",
        "CREATE INDEX idx_messages_body_pending \
         ON messages(account_id, folder_id, uid) WHERE body_path IS NULL",
    ),
    (
        "idx_drafts_updated",
        "CREATE INDEX idx_drafts_updated ON drafts(account_id, updated_at DESC, id DESC)",
    ),
    (
        "ix_ai_conversations_updated",
        "CREATE INDEX ix_ai_conversations_updated ON ai_conversations(updated_at DESC)",
    ),
    (
        "ix_ai_entities_last_seen",
        "CREATE INDEX ix_ai_entities_last_seen ON ai_entities(last_seen_at DESC)",
    ),
];

/// 读出某个索引的建表语句（SQLite 存的是建索引时的原文）。
fn index_sql(conn: &Connection, name: &str) -> Option<String> {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?1",
        params![name],
        |row| row.get(0),
    )
    .ok()
}

/// 归一化 SQL：压掉所有空白差异。
///
/// 建索引语句的原文会带上「DDL 里怎么写」的重排痕迹（换行 vs 单行），
/// 而 DDL 与迁移函数是**两处独立书写**的 —— 比对语义而不是格式。
fn normalize(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 建一个「v5 形态」的库：先按当前 DDL 建好库（表结构与 v5 相同），
/// 再把索引换回 v5 的旧列序，最后把版本号改回 5。
///
/// 这样构造比手工维护一份冻结的 v5 DDL 更不容易漂移 —— v5 与 v6 的**表结构完全相同**，
/// 差别只在索引。
fn create_v5_db(dir: &Path) -> Connection {
    drop(Store::open(dir).expect("create v5-shaped db"));
    let conn = Connection::open(dir.join("myemail.db")).expect("open");

    // 把当前（v6）索引换回 v5 的旧列序。
    for (name, _) in V6_INDEXES {
        conn.execute_batch(&format!("DROP INDEX IF EXISTS {name};"))
            .expect("drop index");
    }
    for (_, sql) in V5_INDEXES {
        conn.execute_batch(sql).expect("create v5 index");
    }

    conn.execute(
        "UPDATE meta SET value = '5' WHERE key = 'schema_version'",
        [],
    )
    .expect("rewind version");

    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint");
    conn
}

fn query_plan(conn: &Connection, sql: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("prepare");
    stmt.query_map([], |row| row.get::<_, String>(3))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect")
}

#[test]
fn v5_db_upgrades_to_v6_and_indexes_match_fresh_db() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let conn = create_v5_db(dir.path());
        // 前置条件：迁移前确实是旧列序。
        let before = index_sql(&conn, "idx_messages_body_pending").expect("index exists");
        assert!(
            before.contains("folder_id, uid"),
            "前置条件不成立：迁移前 body_pending 应是旧列序，实际为 {before}"
        );
        assert!(
            index_sql(&conn, "idx_messages_unread").is_none(),
            "前置条件不成立：v5 不该有 idx_messages_unread"
        );
    }

    let store = Store::open(dir.path()).expect("migrate v5 -> v6");
    assert_eq!(
        store.writable().schema_version().unwrap(),
        me_core::SCHEMA_VERSION,
        "升级后版本号应等于 SCHEMA_VERSION"
    );

    let conn = Connection::open(dir.path().join("myemail.db")).expect("open");

    // 逐条核对：索引存在，且建表语句与事实源一致。
    for (name, expected) in V6_INDEXES {
        let actual =
            index_sql(&conn, name).unwrap_or_else(|| panic!("索引 {name} 应存在，实际不存在"));
        assert_eq!(
            normalize(&actual),
            normalize(expected),
            "索引 {name} 的定义与事实源不一致"
        );
    }

    // 与新建库对比：索引集合必须完全相同。
    let fresh_dir = tempfile::tempdir().expect("tempdir");
    drop(Store::open(fresh_dir.path()).expect("create fresh"));
    let fresh = Connection::open(fresh_dir.path().join("myemail.db")).expect("open fresh");

    let index_names = |conn: &Connection| -> Vec<String> {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'index' \
                  AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .expect("prepare");
        stmt.query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect")
    };
    assert_eq!(
        index_names(&conn),
        index_names(&fresh),
        "迁移库与新建库的索引集合不一致"
    );
}

#[test]
fn migration_is_idempotent() {
    // 新建库本身已经是 v6 形态；再走一遍迁移不能报错，结果也不能变。
    let dir = tempfile::tempdir().expect("tempdir");
    drop(Store::open(dir.path()).expect("create"));
    let conn = Connection::open(dir.path().join("myemail.db")).expect("open");
    conn.execute(
        "UPDATE meta SET value = '5' WHERE key = 'schema_version'",
        [],
    )
    .expect("rewind version");
    let before: Vec<String> = V6_INDEXES
        .iter()
        .map(|(name, _)| index_sql(&conn, name).expect("index"))
        .collect();
    drop(conn);

    let store = Store::open(dir.path()).expect("re-migrate");
    assert_eq!(
        store.writable().schema_version().unwrap(),
        me_core::SCHEMA_VERSION
    );

    let conn = Connection::open(dir.path().join("myemail.db")).expect("open");
    for (i, (name, _)) in V6_INDEXES.iter().enumerate() {
        assert_eq!(
            normalize(&index_sql(&conn, name).expect("index")),
            normalize(&before[i]),
            "重复迁移后索引 {name} 的定义变了"
        );
    }
}

#[test]
fn key_queries_no_longer_use_temp_btree_sort() {
    // 这几条查询在 v5 的索引下会退化成「USE TEMP B-TREE FOR ORDER BY」，
    // v6 之后必须走索引本身的顺序。
    let dir = tempfile::tempdir().expect("tempdir");
    drop(Store::open(dir.path()).expect("create"));
    let conn = Connection::open(dir.path().join("myemail.db")).expect("open");

    let cases: &[(&str, &str)] = &[
        (
            "邮件列表（键集分页）",
            "SELECT id, subject, date FROM messages m \
             WHERE folder_id = 1 AND (date < 100 OR (date = 100 AND id < 50)) \
             ORDER BY date DESC, id DESC LIMIT 200",
        ),
        (
            "正文补全队列",
            "SELECT id, folder_id, uid FROM messages \
             WHERE account_id = 1 AND body_path IS NULL AND uid IS NOT NULL \
               AND folder_id NOT IN (SELECT id FROM folders WHERE role = 'drafts') \
             ORDER BY folder_id, date DESC, id DESC LIMIT 200",
        ),
        (
            "草稿列表",
            "SELECT id FROM drafts WHERE account_id = 1 AND deleted_at IS NULL \
               AND conflict_of IS NULL ORDER BY updated_at DESC, id DESC LIMIT 200",
        ),
        (
            "AI 实体列表",
            "SELECT id FROM ai_entities e ORDER BY e.last_seen_at DESC, e.id DESC LIMIT 64",
        ),
    ];

    for (label, sql) in cases {
        let plan = query_plan(&conn, sql);
        assert!(
            !plan.iter().any(|step| step.contains("TEMP B-TREE")),
            "{label} 仍在用临时 B 树排序：{plan:#?}"
        );
    }
}

#[test]
fn unread_count_query_uses_the_expression_index() {
    // 未读计数用 `(flags & 1) = 0`，普通索引帮不上忙；
    // v6 的表达式索引让它走覆盖扫描。
    let dir = tempfile::tempdir().expect("tempdir");
    drop(Store::open(dir.path()).expect("create"));
    let conn = Connection::open(dir.path().join("myemail.db")).expect("open");

    let plan = query_plan(
        &conn,
        "SELECT COUNT(*) FROM messages WHERE folder_id = 1 AND (flags & 1) = 0",
    );
    assert!(
        plan.iter().any(|step| step.contains("idx_messages_unread")),
        "未读计数没有走 idx_messages_unread：{plan:#?}"
    );
}

#[test]
fn coverage_count_query_uses_the_partial_index() {
    // 搜索结果页显示「正文索引覆盖 N/M 封」，其中 N 走
    // `idx_messages_indexed(account_id) WHERE body_path IS NOT NULL` 部分索引。
    let dir = tempfile::tempdir().expect("tempdir");
    drop(Store::open(dir.path()).expect("create"));
    let conn = Connection::open(dir.path().join("myemail.db")).expect("open");

    let plan = query_plan(
        &conn,
        "SELECT COUNT(*) FROM messages m WHERE 1 = 1 AND m.body_path IS NOT NULL",
    );
    assert!(
        plan.iter()
            .any(|step| step.contains("idx_messages_indexed")),
        "正文覆盖率查询没有走 idx_messages_indexed：{plan:#?}"
    );
}

#[test]
fn migration_preserves_data() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let conn = create_v5_db(dir.path());
        conn.execute_batch(
            "INSERT INTO accounts(id, email, display_name, imap_host, imap_port, imap_tls,
                                  smtp_host, smtp_port, smtp_tls, auth_user, auth_method, created_at)
                 VALUES (1, 'a@example.com', 'A', 'imap.example.com', 993, 'implicit',
                         'smtp.example.com', 465, 'implicit', 'a@example.com', 'password', 0);
             INSERT INTO folders(id, account_id, name, delimiter, role, uid_validity, last_uid)
                 VALUES (1, 1, 'INBOX', '/', 'inbox', 10, 1);
             INSERT INTO messages(id, account_id, folder_id, uid, message_id, subject, date,
                                  flags, has_attachment, body_path)
                 VALUES (1, 1, 1, 1, 'm1@example.com', 'hello', 1000, 0, 0, NULL),
                        (2, 1, 1, 2, 'm2@example.com', 'world', 2000, 1, 0, 'bodies/2.txt');",
        )
        .expect("fixtures");
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .expect("checkpoint");
    }

    drop(Store::open(dir.path()).expect("migrate"));
    let conn = Connection::open(dir.path().join("myemail.db")).expect("open");

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .expect("count");
    assert_eq!(count, 2, "迁移后邮件数应不变");

    let unread: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE folder_id = 1 AND (flags & 1) = 0",
            [],
            |row| row.get(0),
        )
        .expect("unread");
    assert_eq!(unread, 1, "迁移后未读数应不变");

    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE body_path IS NULL AND uid IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("pending");
    assert_eq!(pending, 1, "迁移后待补全数应不变");
}
