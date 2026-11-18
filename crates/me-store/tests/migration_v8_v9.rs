//! v8 → v9 迁移测试。
//!
//! v9 是 v0.5 的**纯建表**：新增 `kb_documents` / `kb_chunks` / `kb_chunks_fts` /
//! `kb_doc_entities` 四个对象（外加独立文件里的 `kbvec.vec_chunks` 虚表），
//! 不 ALTER 任何既有表。
//!
//! 验证：
//! - v8 形态的库打开后升级到 v9，四个对象都在；
//! - **既有表行数不变**（纯建表不该碰数据）；
//! - 升完能直接用：写文档 → 写语块 → FTS 能搜到中文 2 字词；
//! - 全新库一次建到 v9；
//! - **删掉 `kb.sqlite` 后重启不崩**，向量标记被重置（可丢弃物的硬回退线）。

use std::path::Path;

use me_core::knowledge::{
    KbKind, KbOrigin, KnowledgeDocId, KnowledgeRepo, NewChunk, NewKnowledgeDoc,
};
use me_core::port::MetaRepo;
use me_store::Store;
use rusqlite::Connection;

/// v9 期望存在的四个对象（`sqlite_master` 里的 `name`）。
const V9_OBJECTS: &[&str] = &[
    "kb_documents",
    "kb_chunks",
    "kb_chunks_fts",
    "kb_doc_entities",
];

fn exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1)",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        != 0
}

/// 建一个「v8 形态」的库：先按当前 DDL 建好（已是 v9），再删掉 v9 加的东西，
/// 最后把版本号改回 8。比手工维护一份冻结的 v8 DDL 更不容易漂移。
fn create_v8_db(dir: &Path) -> Connection {
    drop(Store::open(dir).expect("create v9-shaped db"));
    let conn = Connection::open(dir.join("myemail.db")).expect("open");

    // 顺序：先删子表（外键），再删父表，最后删虚拟表。
    conn.execute_batch(
        "DROP TABLE IF EXISTS kb_doc_entities;
         DROP TABLE IF EXISTS kb_chunks;
         DROP TABLE IF EXISTS kb_chunks_fts;
         DROP TABLE IF EXISTS kb_documents;
         UPDATE meta SET value = '8' WHERE key = 'schema_version';",
    )
    .expect("rewind to v8");
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint");
    conn
}

fn insert_message(conn: &Connection) -> i64 {
    let now = me_core::now_unix();
    conn.execute(
        "INSERT INTO accounts
           (email, display_name, imap_host, imap_port, imap_tls, smtp_host, smtp_port,
            smtp_tls, auth_user, auth_method, created_at)
         VALUES ('t@example.com', 'T', 'imap.example.com', 993, 'implicit',
                 'smtp.example.com', 465, 'implicit', 't@example.com', 'password', ?1)",
        [now],
    )
    .expect("insert account");
    let account: i64 = conn
        .query_row("SELECT id FROM accounts LIMIT 1", [], |row| row.get(0))
        .expect("account id");
    conn.execute(
        "INSERT INTO folders (account_id, name, role, uid_validity, last_uid)
         VALUES (?1, 'INBOX', 'inbox', 1, 0)",
        [account],
    )
    .expect("insert folder");
    let folder: i64 = conn
        .query_row("SELECT id FROM folders LIMIT 1", [], |row| row.get(0))
        .expect("folder id");
    conn.execute(
        "INSERT INTO messages (account_id, folder_id, uid, subject, date_source, flags, date)
         VALUES (?1, ?2, 1, '合同审查', 'header', 0, ?3)",
        rusqlite::params![account, folder, now],
    )
    .expect("insert message");
    conn.query_row("SELECT MAX(id) FROM messages", [], |row| row.get(0))
        .expect("message id")
}

#[test]
fn v8_db_upgrades_to_v9_with_four_objects() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let conn = create_v8_db(dir.path());
        for name in V9_OBJECTS {
            assert!(!exists(&conn, name), "前置条件：{name} 不该存在");
        }
        // 迁移不动既有数据，但为了验证「行数不变」先塞一封邮件。
        let id = insert_message(&conn);
        assert!(id > 0);
    }

    let store = Store::open(dir.path()).expect("upgrade to v9");
    assert_eq!(
        store.writable().schema_version().unwrap(),
        me_core::SCHEMA_VERSION,
        "升级后版本号应等于 SCHEMA_VERSION"
    );
    assert_eq!(me_core::SCHEMA_VERSION, 9);

    let conn = Connection::open(dir.path().join("myemail.db")).expect("open");
    for name in V9_OBJECTS {
        assert!(exists(&conn, name), "迁移后应当存在 {name}");
    }

    // 既有数据原样保留。
    let messages: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(messages, 1, "纯建表不该动既有数据");

    // 升完就能直接用：写一份文档 + 语块，中文 2 字词必须能搜到。
    let writable = store.writable();
    let doc = writable
        .upsert_document(&NewKnowledgeDoc {
            origin: KbOrigin::File,
            source_ref: "/tmp/采购合同.md".to_string(),
            title: "采购合同".to_string(),
            kind: KbKind::Md,
            mail_id: None,
            size_bytes: 32,
            content_hash: "hash".to_string(),
        })
        .expect("upsert doc");
    writable
        .replace_chunks(
            doc,
            &[NewChunk::new(
                "合同审查由法务部门负责，预算另行审批。",
                None,
            )],
        )
        .expect("replace chunks");
    assert_eq!(writable.fts("合同", 5).expect("fts").len(), 1);
    assert_eq!(writable.fts("预算", 5).expect("fts").len(), 1);
}

#[test]
fn fresh_db_is_created_at_v9() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open fresh");
    assert_eq!(store.writable().schema_version().unwrap(), 9);
    let conn = Connection::open(dir.path().join("myemail.db")).expect("open");
    for name in V9_OBJECTS {
        assert!(exists(&conn, name), "新库里应当有 {name}");
    }
    // 向量表建在独立文件里。
    assert!(
        dir.path().join("kb.sqlite").exists(),
        "新库应当同时创建 kb.sqlite"
    );
}

#[test]
fn deleting_kb_file_is_safe_and_resets_embedded_flags() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ids: Vec<KnowledgeDocId>;
    {
        let store = Store::open(dir.path()).expect("open");
        let writable = store.writable();
        let doc = writable
            .upsert_document(&NewKnowledgeDoc {
                origin: KbOrigin::File,
                source_ref: "/tmp/可丢弃.md".to_string(),
                title: "可丢弃".to_string(),
                kind: KbKind::Md,
                mail_id: None,
                size_bytes: 8,
                content_hash: "h".to_string(),
            })
            .expect("doc");
        writable
            .replace_chunks(doc, &[NewChunk::new("这一块应当有向量。", None)])
            .expect("chunks");
        let chunks = writable.chunks(doc, 0, 10).expect("read chunks");
        ids = vec![doc];
        writable
            .put_vectors(&[(chunks[0].id, vec![0.5; me_core::EMBED_DIM])])
            .expect("vectors");
        writable
            .mark_embedded(&[chunks[0].id])
            .expect("mark embedded");
        assert!(writable.chunks(doc, 0, 10).unwrap()[0].embedded);
    }

    // 删掉向量库（可丢弃物），重启必须不崩，并把「标了完成但没向量」的块重置。
    std::fs::remove_file(dir.path().join("kb.sqlite")).expect("remove kb.sqlite");
    let store = Store::open(dir.path()).expect("reopen after deleting kb.sqlite");
    let writable = store.writable();
    let chunks = writable.chunks(ids[0], 0, 10).expect("chunks after reopen");
    assert_eq!(chunks.len(), 1, "主库语块不该被删掉");
    assert!(
        !chunks[0].embedded,
        "向量没了就必须重置标记，否则这一块永远不会被重跑"
    );
}
