//! v1 → v2 迁移测试。
//!
//! 冻结一份 v1 DDL（不引用当前 `SCHEMA_DDL`），验证：
//! 备份、版本、纯加列、索引、旧草稿状态、Drafts 历史邮件清理、外键级联、
//! `foreign_key_check` 与失败回滚。

use std::path::Path;

use me_core::port::MetaRepo;
use me_store::Store;
use rusqlite::{Connection, params};

/// v0.1 发布时的 DDL（冻结副本）。
const V1_DDL: &str = r#"
CREATE TABLE accounts (
    id            INTEGER PRIMARY KEY,
    email         TEXT NOT NULL UNIQUE,
    display_name  TEXT,
    imap_host     TEXT NOT NULL,
    imap_port     INTEGER NOT NULL,
    imap_tls      TEXT NOT NULL,
    smtp_host     TEXT NOT NULL,
    smtp_port     INTEGER NOT NULL,
    smtp_tls      TEXT NOT NULL,
    auth_user     TEXT NOT NULL,
    auth_method   TEXT NOT NULL DEFAULT 'password',
    created_at    INTEGER NOT NULL
);
CREATE TABLE folders (
    id           INTEGER PRIMARY KEY,
    account_id   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    delimiter    TEXT,
    role         TEXT NOT NULL DEFAULT 'other'
                 CHECK (role IN ('inbox','sent','drafts','trash','junk','archive','other')),
    uid_validity INTEGER NOT NULL DEFAULT 0,
    last_uid     INTEGER NOT NULL DEFAULT 0,
    deleted_on_server INTEGER NOT NULL DEFAULT 0,
    UNIQUE(account_id, name)
);
CREATE UNIQUE INDEX idx_folders_trash ON folders(account_id) WHERE role = 'trash';
CREATE TABLE messages (
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
CREATE INDEX idx_messages_folder_date ON messages(folder_id, date DESC);
CREATE INDEX idx_messages_account ON messages(account_id);
CREATE INDEX idx_messages_body_pending ON messages(account_id, folder_id, uid) WHERE body_path IS NULL;
CREATE INDEX idx_messages_message_id ON messages(folder_id, message_id);
CREATE TABLE attachments (
    id         INTEGER PRIMARY KEY,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    part_index INTEGER NOT NULL,
    filename   TEXT,
    mime_type  TEXT,
    size       INTEGER,
    local_path TEXT,
    is_inline  INTEGER NOT NULL DEFAULT 0,
    content_id TEXT,
    UNIQUE(message_id, part_index)
);
CREATE TABLE drafts (
    id           INTEGER PRIMARY KEY,
    account_id   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    raw_path     TEXT NOT NULL,
    message_id   TEXT NOT NULL,
    subject      TEXT,
    to_addrs     TEXT,
    status       TEXT NOT NULL CHECK (status IN ('draft','sent')),
    server_uid   INTEGER,
    last_error   TEXT,
    updated_at   INTEGER NOT NULL
);
CREATE VIRTUAL TABLE messages_fts USING fts5(
    subject, body, from_addr, to_addrs,
    tokenize='trigram'
);
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE pending_flags (
    message_id   INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    target_flags INTEGER NOT NULL,
    attempts     INTEGER NOT NULL DEFAULT 0,
    last_error   TEXT,
    updated_at   INTEGER NOT NULL
);
CREATE TABLE pending_ops (
    id               INTEGER PRIMARY KEY,
    account_id       INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind             TEXT NOT NULL,
    message_id       INTEGER REFERENCES messages(id) ON DELETE SET NULL,
    source_folder_id INTEGER REFERENCES folders(id) ON DELETE SET NULL,
    source_folder_name TEXT,
    source_uid       INTEGER,
    target_folder_id INTEGER REFERENCES folders(id) ON DELETE SET NULL,
    attempts         INTEGER NOT NULL DEFAULT 0,
    last_error       TEXT,
    created_at       INTEGER NOT NULL
);
CREATE INDEX idx_pending_ops_account ON pending_ops(account_id, attempts);
"#;

fn create_v1_db(dir: &Path) -> Connection {
    let conn = Connection::open(dir.join("myemail.db")).expect("open v1 db");
    conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")
        .expect("pragmas");
    conn.execute_batch(V1_DDL).expect("v1 ddl");
    conn.execute(
        "INSERT INTO meta(key, value) VALUES ('schema_version', '1')",
        [],
    )
    .expect("version");
    conn.execute(
        "INSERT INTO accounts(
             id, email, display_name, imap_host, imap_port, imap_tls,
             smtp_host, smtp_port, smtp_tls, auth_user, auth_method, created_at
         ) VALUES (1, 'alice@example.com', 'Alice', 'imap.example.com', 993, 'implicit',
                   'smtp.example.com', 465, 'implicit', 'alice@example.com', 'password', 0)",
        [],
    )
    .expect("account");
    conn.execute_batch(
        "INSERT INTO folders(id, account_id, name, delimiter, role, uid_validity, last_uid)
             VALUES (1, 1, 'INBOX', '/', 'inbox', 10, 1),
                    (2, 1, 'Drafts', '/', 'drafts', 20, 3);
         INSERT INTO messages(id, account_id, folder_id, uid, message_id, subject, flags, has_attachment, body_path)
             VALUES (100, 1, 1, 1, 'inbox@example.com', 'inbox', 0, 0, NULL),
                    (200, 1, 2, 1, 'draft1@example.com', 'draft1', 0, 1, 'bodies/200.txt'),
                    (201, 1, 2, 2, 'draft2@example.com', 'draft2', 0, 0, NULL),
                    (202, 1, 2, 3, 'draft3@example.com', 'draft3', 0, 0, NULL);
         INSERT INTO attachments(message_id, part_index, filename, size)
             VALUES (200, 1, 'a.txt', 10);
         INSERT INTO pending_flags(message_id, target_flags, attempts, updated_at)
             VALUES (201, 1, 0, 0);
         INSERT INTO pending_ops(account_id, kind, message_id, source_folder_id,
                                 source_folder_name, source_uid, attempts, created_at)
             VALUES (1, 'move_to_trash', 202, 2, 'Drafts', 3, 0, 0);
         INSERT INTO messages_fts(rowid, subject, body, from_addr, to_addrs)
             VALUES (100, 'inbox', 'body', 'a@example.com', 'b@example.com'),
                    (200, 'draft1', '', '', ''),
                    (201, 'draft2', '', '', ''),
                    (202, 'draft3', '', '', '');
         INSERT INTO drafts(id, account_id, raw_path, message_id, subject, to_addrs,
                            status, server_uid, last_error, updated_at)
             VALUES (1, 1, 'drafts/1/message.eml', 'local1@example.com', 'local1', '[]', 'draft', 77, NULL, 0),
                    (2, 1, 'drafts/2/message.eml', 'local2@example.com', 'local2', '[]', 'draft', NULL, NULL, 0),
                    (3, 1, 'drafts/3/message.eml', 'sent@example.com', 'sent', '[]', 'sent', 88, NULL, 0);",
    )
    .expect("fixtures");
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint");
    conn
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).expect(sql)
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("table_info");
    let mut rows = stmt.query([]).expect("table_info rows");
    while let Some(row) = rows.next().expect("row") {
        let name: String = row.get(1).expect("column name");
        if name == column {
            return true;
        }
    }
    false
}

#[test]
fn v1_to_v2_migration_preserves_data_and_cleans_drafts_messages() {
    let dir = tempfile::tempdir().expect("tempdir");
    drop(create_v1_db(dir.path()));

    let store = Store::open(dir.path()).expect("migrate");
    assert_eq!(
        store.writable().schema_version().unwrap(),
        me_core::SCHEMA_VERSION
    );
    assert_eq!(
        store
            .writable()
            .get_setting("draft_bulk_upload_pending")
            .unwrap()
            .as_deref(),
        Some("true")
    );
    drop(store);

    assert!(
        dir.path().join("myemail.db.bak-v1").is_file(),
        "migration backup must exist"
    );

    let conn = Connection::open(dir.path().join("myemail.db")).expect("reopen");
    assert!(column_exists(&conn, "accounts", "oauth_provider"));
    assert!(column_exists(&conn, "accounts", "poll_interval_secs"));
    assert!(column_exists(&conn, "drafts", "bcc_addrs"));
    assert!(column_exists(&conn, "drafts", "deleted_at"));

    // Drafts 历史摘要被清理，INBOX 不受影响。
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages"), 1);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM messages WHERE folder_id = 2"),
        0
    );
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM attachments"), 0);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM pending_flags"), 0);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages_fts"), 1);

    // pending_ops 行保留，message_id 置 NULL。
    let pending_message: Option<i64> = conn
        .query_row("SELECT message_id FROM pending_ops", [], |row| row.get(0))
        .expect("pending_ops");
    assert!(pending_message.is_none());

    // 外键无悬空引用。
    let violations: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .expect("foreign_key_check");
    assert_eq!(violations, 0);

    // 旧草稿保留，并初始化同步状态。
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM drafts"), 3);
    let states: Vec<(i64, String)> = {
        let mut stmt = conn
            .prepare("SELECT id, sync_state FROM drafts ORDER BY id")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(
        states,
        vec![
            (1, "local_dirty".to_string()),
            (2, "local_only".to_string()),
            (3, "local_only".to_string())
        ]
    );

    // v1 草稿行仍可读；Bcc 列为空。
    let bcc: Option<String> = conn
        .query_row("SELECT bcc_addrs FROM drafts WHERE id = 1", [], |row| {
            row.get(0)
        })
        .expect("bcc");
    assert_eq!(bcc, None);
}

#[test]
fn failed_migration_rolls_back_and_keeps_version_1() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = create_v1_db(dir.path());
    // 同名索引让迁移脚本在 CREATE INDEX 处失败，验证事务回滚。
    conn.execute_batch("CREATE INDEX idx_drafts_account_state ON drafts(status);")
        .expect("conflicting index");
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint");
    drop(conn);

    let err = Store::open(dir.path()).err().expect("migration must fail");
    assert!(
        format!("{err:?}").contains("迁移"),
        "unexpected error: {err:?}"
    );
    assert!(dir.path().join("myemail.db.bak-v1").is_file());

    let conn = Connection::open(dir.path().join("myemail.db")).expect("reopen");
    let version: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .expect("version");
    assert_eq!(version, "1");
    assert!(!column_exists(&conn, "accounts", "oauth_provider"));
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages"), 4);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM drafts"), 3);
}

#[test]
fn new_database_is_created_at_current_schema_directly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("create");
    assert_eq!(
        store.writable().schema_version().unwrap(),
        me_core::SCHEMA_VERSION
    );
    drop(store);
    assert!(!dir.path().join("myemail.db.bak-v1").exists());
}

#[test]
fn v2_database_is_rejected_by_newer_schema_guard() {
    let dir = tempfile::tempdir().expect("tempdir");
    drop(create_v1_db(dir.path()));
    {
        let conn = Connection::open(dir.path().join("myemail.db")).unwrap();
        conn.execute(
            "UPDATE meta SET value = '99' WHERE key = 'schema_version'",
            params![],
        )
        .unwrap();
    }
    let err = Store::open(dir.path())
        .err()
        .expect("future schema must be rejected");
    assert!(format!("{err:?}").contains("高于本程序支持"));
}

/// 从建表语句中提取 `CHECK (...)` 表达式（`table_info` 不暴露约束）。
fn check_constraints(table_sql: &str) -> Vec<String> {
    let upper = table_sql.to_ascii_uppercase();
    let mut checks = Vec::new();
    let mut cursor = 0;

    while let Some(offset) = upper[cursor..].find("CHECK") {
        let start = cursor + offset + "CHECK".len();
        let Some(open) = table_sql[start..].find('(') else {
            break;
        };
        let mut depth = 0usize;
        let mut end = None;
        for (index, ch) in table_sql[start + open..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(start + open + index);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else { break };
        checks.push(
            table_sql[start + open..=end]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
        );
        cursor = end + 1;
    }

    checks
}

/// 表 / 列 / 外键 / 索引 / CHECK 的结构签名。
///
/// 列按名字排序，因此**对物理列序不敏感**：新建库按 §4.2 的逻辑顺序建表，
/// 而 v1→v2 迁移用 `ALTER TABLE ADD COLUMN` 把新列追加到表尾，两者的
/// `PRAGMA table_info` 顺序必然不同。这里校验的是语义等价性。
///
/// FTS5 的影子表（`messages_fts_*`）由 SQLite 版本决定，不参与比较。
fn schema_signature(conn: &Connection) -> Vec<String> {
    let tables: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master
                  WHERE type = 'table'
                    AND name NOT LIKE 'sqlite_%'
                    AND name NOT LIKE 'messages_fts_%'
                  ORDER BY name",
            )
            .expect("prepare tables");
        stmt.query_map([], |row| row.get(0))
            .expect("query tables")
            .collect::<Result<_, _>>()
            .expect("table names")
    };

    let mut signature = Vec::new();

    for table in &tables {
        let table_sql: String = conn
            .query_row(
                "SELECT COALESCE(sql, '') FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![table],
                |row| row.get(0),
            )
            .expect("table ddl");
        let mut checks = check_constraints(&table_sql);
        checks.sort();
        signature.extend(
            checks
                .into_iter()
                .map(|check| format!("check {table}. {check}")),
        );

        let mut columns: Vec<String> = {
            let mut stmt = conn
                .prepare(&format!("PRAGMA table_info({table})"))
                .expect("prepare table_info");
            stmt.query_map([], |row| {
                let name: String = row.get(1)?;
                let ty: String = row.get(2)?;
                let not_null: i64 = row.get(3)?;
                let default: Option<String> = row.get(4)?;
                let pk: i64 = row.get(5)?;
                Ok(format!(
                    "{name} {ty} NOT NULL={not_null} DEFAULT={default:?} PK={pk}"
                ))
            })
            .expect("query table_info")
            .collect::<Result<_, _>>()
            .expect("columns")
        };
        columns.sort();
        signature.extend(
            columns
                .into_iter()
                .map(|column| format!("column {table}. {column}")),
        );

        let mut foreign_keys: Vec<String> = {
            let mut stmt = conn
                .prepare(&format!("PRAGMA foreign_key_list({table})"))
                .expect("prepare foreign_key_list");
            stmt.query_map([], |row| {
                let target: String = row.get(2)?;
                let from: String = row.get(3)?;
                let to: Option<String> = row.get(4)?;
                let on_update: String = row.get(5)?;
                let on_delete: String = row.get(6)?;
                Ok(format!(
                    "{from} -> {target}.{to:?} ON UPDATE {on_update} ON DELETE {on_delete}"
                ))
            })
            .expect("query foreign_key_list")
            .collect::<Result<_, _>>()
            .expect("foreign keys")
        };
        foreign_keys.sort();
        signature.extend(
            foreign_keys
                .into_iter()
                .map(|foreign_key| format!("foreign_key {table}. {foreign_key}")),
        );

        let indexes: Vec<(String, i64, String, i64)> = {
            let mut stmt = conn
                .prepare(&format!("PRAGMA index_list({table})"))
                .expect("prepare index_list");
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .expect("query index_list")
            .collect::<Result<_, _>>()
            .expect("indexes")
        };

        for (name, unique, origin, partial) in indexes {
            // `index_xinfo` 才有 DESC 与 `key` 列：`key = 1` 是索引自身列，
            // `key = 0` 是 SQLite 附加的 rowid，不参与比较。
            let entries: Vec<(i64, String)> = {
                let mut stmt = conn
                    .prepare(&format!("PRAGMA index_xinfo({name})"))
                    .expect("prepare index_xinfo");
                stmt.query_map([], |row| {
                    let column: Option<String> = row.get(2)?;
                    let desc: i64 = row.get(3)?;
                    let key: i64 = row.get(5)?;
                    Ok((
                        key,
                        format!("{} DESC={desc}", column.unwrap_or_else(|| "<expr>".into())),
                    ))
                })
                .expect("query index_xinfo")
                .collect::<Result<Vec<_>, _>>()
                .expect("index columns")
            };
            let entries: Vec<String> = entries
                .into_iter()
                .filter(|(key, _)| *key == 1)
                .map(|(_, entry)| entry)
                .collect();
            signature.push(format!(
                "index {table}.{name} UNIQUE={unique} ORIGIN={origin} PARTIAL={partial} COLUMNS={entries:?}"
            ));
        }
    }

    signature.sort();
    signature
}

/// v1 库迁移后的结构与新建库必须**语义等价**。
///
/// 二者唯一的已知差异是 `accounts` / `drafts` 的物理列序（`ALTER TABLE ADD
/// COLUMN` 只能追加到表尾）。本测试因此按列名比较：任何类型、`NOT NULL`、
/// `DEFAULT`、外键动作、索引列或 DESC 的漂移都会失败。
#[test]
fn migrated_matches_fresh_schema() {
    let fresh_dir = tempfile::tempdir().expect("tempdir");
    drop(Store::open(fresh_dir.path()).expect("create fresh"));
    let fresh = Connection::open(fresh_dir.path().join("myemail.db")).expect("open fresh");

    let migrated_dir = tempfile::tempdir().expect("tempdir");
    drop(create_v1_db(migrated_dir.path()));
    drop(Store::open(migrated_dir.path()).expect("migrate v1"));
    let migrated = Connection::open(migrated_dir.path().join("myemail.db")).expect("open migrated");

    let fresh_signature = schema_signature(&fresh);
    let migrated_signature = schema_signature(&migrated);

    // 防止签名退化为空导致断言恒真（当前约 116 条）。
    assert!(
        fresh_signature.len() >= 90,
        "结构签名条目过少（{}），签名逻辑可能失效",
        fresh_signature.len()
    );
    // `table_info` 不暴露 CHECK，这条断言防止约束提取静默失效。
    assert!(
        fresh_signature
            .iter()
            .any(|item| item.starts_with("check drafts.")),
        "CHECK 约束未纳入签名"
    );

    let only_fresh: Vec<&String> = fresh_signature
        .iter()
        .filter(|item| !migrated_signature.contains(item))
        .collect();
    let only_migrated: Vec<&String> = migrated_signature
        .iter()
        .filter(|item| !fresh_signature.contains(item))
        .collect();

    assert!(
        only_fresh.is_empty() && only_migrated.is_empty(),
        "迁移库与新建库结构不等价\n\
         仅在新建库出现：{only_fresh:#?}\n\
         仅在迁移库出现：{only_migrated:#?}"
    );
    assert_eq!(
        fresh_signature.len(),
        migrated_signature.len(),
        "结构签名长度不同，可能存在重复条目掩盖差异"
    );
}

/// v2 库（没有 v3 的表）打开后应就地升级到 v3，且结构与新建库一致。
///
/// 用「新建 v3 库 → 删掉 v3 对象 → 把版本改回 2」构造 v2 形态：这比手工维护
/// 一份冻结的 v2 DDL 更不容易与真实 v2 漂移（v2 的表结构本就由迁移测试守护）。
#[test]
fn v2_database_upgrades_to_latest_in_place() {
    const V3_OBJECTS: [&str; 11] = [
        "ai_conversation_entities",
        "ai_entity_aliases",
        "ai_entities",
        "ai_messages",
        "ai_conversations",
        "weather_snapshots",
        "weather_locations",
        "calendar_reminder_fires",
        "calendar_event_exceptions",
        "calendar_events",
        "calendars",
    ];

    let dir = tempfile::tempdir().expect("tempdir");
    drop(Store::open(dir.path()).expect("create"));
    {
        let conn = Connection::open(dir.path().join("myemail.db")).expect("open");
        for table in V3_OBJECTS {
            conn.execute_batch(&format!("DROP TABLE {table};"))
                .expect("drop v3 table");
        }
        conn.execute(
            "UPDATE meta SET value = '2' WHERE key = 'schema_version'",
            [],
        )
        .expect("rewind version");
    }

    let store = Store::open(dir.path()).expect("migrate v2 -> latest");
    // 用常量而不是写死版本号：v2 路径要一路升到最新，不能停在 v3。
    assert_eq!(
        store.writable().schema_version().unwrap(),
        me_core::SCHEMA_VERSION
    );

    let conn = Connection::open(dir.path().join("myemail.db")).expect("open");
    for table in V3_OBJECTS {
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                params![table],
                |row| row.get(0),
            )
            .expect("sqlite_master");
        assert!(exists, "迁移后缺少表 {table}");
    }
    // 默认日历由迁移种子写入，且重复打开不会产生第二个默认日历。
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM calendars WHERE is_default = 1"),
        1
    );
    drop(conn);
    drop(store);

    let reopened = Store::open(dir.path()).expect("reopen");
    assert_eq!(
        reopened.writable().schema_version().unwrap(),
        me_core::SCHEMA_VERSION
    );
    drop(reopened);
    let conn = Connection::open(dir.path().join("myemail.db")).expect("open");
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM calendars WHERE is_default = 1"),
        1
    );
}
