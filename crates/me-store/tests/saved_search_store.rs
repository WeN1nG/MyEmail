//! 保存的搜索与联系人的仓储测试。
//!
//! 覆盖：
//! - 保存 / 读取 / 删除 / `touch` 更新 `last_used_at`；
//! - JSON 往返等价（`SavedSearchQuery` ↔ `query_json`）；
//! - **持久化子集不含分页态**（`cursor` / `limit` 不进 JSON）；
//! - 联系人去重（大小写归一化）、排序、删除。

use std::path::Path;

use me_core::model::{AccountId, Contact, ContactId, FolderId, SavedSearch, SavedSearchId};
use me_core::port::{ContactRepo, MetaRepo, SavedSearchRepo};
use me_store::Store;
use rusqlite::Connection;

/// 建一个带账户与文件夹的库（保存的搜索的 `accounts` / `folders` 要有值可存）。
fn seed(dir: &Path) {
    drop(Store::open(dir).expect("open"));
    let conn = Connection::open(dir.join("myemail.db")).expect("open");
    conn.execute_batch(
        "INSERT INTO accounts(email, imap_host, imap_port, imap_tls, smtp_host, smtp_port, smtp_tls,
                             auth_user, created_at)
         VALUES ('a@example.com', 'h', 993, 'implicit', 's', 465, 'implicit',
                 'a@example.com', 1700000000);
         INSERT INTO folders(account_id, name, role)
         SELECT id, 'INBOX', 'inbox' FROM accounts;",
    )
    .expect("seed");
}

/// 建一个空库（联系人测试不需要账户）。
fn empty(dir: &Path) {
    drop(Store::open(dir).expect("open"));
}

fn sample_query() -> me_core::model::SavedSearchQuery {
    me_core::model::SavedSearchQuery {
        text: "报价".to_string(),
        accounts: vec![AccountId(1)],
        folders: vec![FolderId(1)],
        since: Some(1_700_000_000),
        until: Some(1_800_000_000),
        has_attachment: Some(true),
        from: Some("zhang".to_string()),
        to: None,
        subject: Some("预算".to_string()),
        unread_only: true,
    }
}

// ---------------------------------------------------------------------------
// 保存的搜索
// ---------------------------------------------------------------------------

#[test]
fn saved_search_round_trips_through_storage() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed(dir.path());
    let store = Store::open(dir.path()).expect("open");

    let query = sample_query();
    let id = store
        .writable()
        .insert_saved_search(&SavedSearch {
            id: SavedSearchId(0),
            name: "张老板的报价".to_string(),
            query: query.clone(),
            created_at: 1_700_000_000,
            last_used_at: 1_700_000_000,
        })
        .expect("insert");

    let loaded = store
        .writable()
        .get_saved_search(id)
        .expect("get")
        .expect("exists");
    assert_eq!(loaded.name, "张老板的报价");
    assert_eq!(loaded.query, query, "JSON 往返后条件应完全等价");
    assert_eq!(loaded.created_at, 1_700_000_000);
    assert_eq!(loaded.last_used_at, 1_700_000_000);
}

#[test]
fn saved_search_list_is_ordered_by_last_used() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed(dir.path());
    let store = Store::open(dir.path()).expect("open");

    let mut ids = Vec::new();
    for (i, name) in ["第一个", "第二个", "第三个"].iter().enumerate() {
        let id = store
            .writable()
            .insert_saved_search(&SavedSearch {
                id: SavedSearchId(0),
                name: name.to_string(),
                query: sample_query(),
                created_at: 1_700_000_000 + i as i64,
                last_used_at: 1_700_000_000 + i as i64,
            })
            .expect("insert");
        ids.push(id);
    }

    let list = store.writable().list_saved_searches().expect("list");
    assert_eq!(list.len(), 3);
    // last_used_at 倒序 → 最后插入的排最前。
    assert_eq!(list[0].id, ids[2]);
    assert_eq!(list[2].id, ids[0]);
}

#[test]
fn touch_updates_last_used_at() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed(dir.path());
    let store = Store::open(dir.path()).expect("open");

    let id = store
        .writable()
        .insert_saved_search(&SavedSearch {
            id: SavedSearchId(0),
            name: "旧的".to_string(),
            query: sample_query(),
            created_at: 1_700_000_000,
            last_used_at: 1_700_000_000,
        })
        .expect("insert");

    store.writable().touch_saved_search(id).expect("touch");

    let loaded = store
        .writable()
        .get_saved_search(id)
        .expect("get")
        .expect("exists");
    assert!(
        loaded.last_used_at > 1_700_000_000,
        "touch 后 last_used_at 应更新，实际 {}",
        loaded.last_used_at
    );
    // touch 不该动 created_at。
    assert_eq!(loaded.created_at, 1_700_000_000);
}

#[test]
fn delete_is_idempotent() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed(dir.path());
    let store = Store::open(dir.path()).expect("open");

    let id = store
        .writable()
        .insert_saved_search(&SavedSearch {
            id: SavedSearchId(0),
            name: "待删".to_string(),
            query: sample_query(),
            created_at: 1_700_000_000,
            last_used_at: 1_700_000_000,
        })
        .expect("insert");

    store.writable().delete_saved_search(id).expect("delete");
    // 再删一次**不该报错**（幂等）。
    store
        .writable()
        .delete_saved_search(id)
        .expect("delete again");

    assert!(
        store
            .writable()
            .get_saved_search(id)
            .expect("get")
            .is_none()
    );
    assert!(
        store
            .writable()
            .list_saved_searches()
            .expect("list")
            .is_empty()
    );
}

#[test]
fn duplicate_name_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed(dir.path());
    let store = Store::open(dir.path()).expect("open");

    let make = |name: &str| SavedSearch {
        id: SavedSearchId(0),
        name: name.to_string(),
        query: sample_query(),
        created_at: 1_700_000_000,
        last_used_at: 1_700_000_000,
    };
    store
        .writable()
        .insert_saved_search(&make("同名"))
        .expect("first insert");
    // 同名必须被唯一索引拦下。
    assert!(
        store.writable().insert_saved_search(&make("同名")).is_err(),
        "同名保存的搜索应被拒绝"
    );
}

#[test]
fn empty_name_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed(dir.path());
    let store = Store::open(dir.path()).expect("open");

    let result = store.writable().insert_saved_search(&SavedSearch {
        id: SavedSearchId(0),
        name: "   ".to_string(),
        query: sample_query(),
        created_at: 1_700_000_000,
        last_used_at: 1_700_000_000,
    });
    assert!(result.is_err(), "空名称应被拒绝");
}

/// **持久化子集不含分页态**——这是 `SavedSearchQuery` 存在的全部理由。
#[test]
fn persisted_query_excludes_pagination_state() {
    let mut query = me_core::model::SearchQuery {
        text: "报价".to_string(),
        accounts: vec![AccountId(1)],
        folders: vec![FolderId(1)],
        since: Some(1),
        until: Some(2),
        has_attachment: Some(true),
        from: Some("zhang".to_string()),
        to: None,
        subject: Some("预算".to_string()),
        unread_only: true,
        // **分页态**——不该被存。
        limit: 50,
        cursor: Some((100, 200)),
    };
    let saved = me_core::model::SavedSearchQuery::from_query(&query);

    let json = serde_json::to_string(&saved).expect("serialize");
    assert!(!json.contains("cursor"), "cursor 不该进 JSON：{json}");
    assert!(!json.contains("limit"), "limit 不该进 JSON：{json}");

    // 但**条件**都要在。
    assert!(json.contains("报价"));
    assert!(json.contains("zhang"));
    assert!(json.contains("预算"));
    assert!(json.contains("unread_only"));

    // 还原时分页从头开始。
    let restored = saved.to_query();
    assert_eq!(restored.cursor, None);
    assert_eq!(restored.limit, me_core::MAX_PAGE_SIZE);
    query.cursor = None;
    assert_eq!(restored.text, query.text);
    assert_eq!(restored.from, query.from);
    assert_eq!(restored.unread_only, query.unread_only);
}

// ---------------------------------------------------------------------------
// 联系人
// ---------------------------------------------------------------------------

fn contact(addr: &str, name: Option<&str>, seen: i64) -> Contact {
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
fn contacts_are_deduplicated_case_insensitively() {
    let dir = tempfile::tempdir().expect("tempdir");
    empty(dir.path());
    let store = Store::open(dir.path()).expect("open");

    store
        .writable()
        .upsert_contacts(&[contact("Alice@Example.com", Some("爱丽丝"), 100)])
        .expect("first");
    store
        .writable()
        .upsert_contacts(&[contact("alice@example.com", None, 200)])
        .expect("second");

    let list = store.writable().list_contacts("", 50).expect("list");
    assert_eq!(list.len(), 1, "大小写不同应只占一行");
    assert_eq!(list[0].seen_count, 2, "重复出现应累加 seen_count");
    assert_eq!(list[0].last_seen, 200, "last_seen 应更新为较新的一次");
    // 显示时保留原样（第一次的写法）。
    assert_eq!(list[0].addr, "Alice@Example.com");
    // 后来的空名不该抹掉已有的名字。
    assert_eq!(list[0].display_name.as_deref(), Some("爱丽丝"));
}

#[test]
fn contacts_are_listed_newest_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    empty(dir.path());
    let store = Store::open(dir.path()).expect("open");

    store
        .writable()
        .upsert_contacts(&[
            contact("old@example.com", None, 100),
            contact("new@example.com", None, 300),
            contact("mid@example.com", None, 200),
        ])
        .expect("upsert");

    let list = store.writable().list_contacts("", 50).expect("list");
    assert_eq!(list[0].addr, "new@example.com");
    assert_eq!(list[1].addr, "mid@example.com");
    assert_eq!(list[2].addr, "old@example.com");
}

#[test]
fn contacts_can_be_found_by_name_or_address_prefix() {
    let dir = tempfile::tempdir().expect("tempdir");
    empty(dir.path());
    let store = Store::open(dir.path()).expect("open");

    store
        .writable()
        .upsert_contacts(&[
            contact("zhang@example.com", Some("张三"), 100),
            contact("alice@example.com", Some("爱丽丝"), 200),
        ])
        .expect("upsert");

    // 按邮箱前缀。
    let by_addr = store.writable().list_contacts("zhang", 50).expect("list");
    assert_eq!(by_addr.len(), 1);
    assert_eq!(by_addr[0].addr, "zhang@example.com");

    // 按显示名——用户记的可能是名字而不是邮箱前缀。
    let by_name = store.writable().list_contacts("张", 50).expect("list");
    assert_eq!(by_name.len(), 1);
    assert_eq!(by_name[0].addr, "zhang@example.com");
}

#[test]
fn contact_prefix_wildcards_are_escaped() {
    let dir = tempfile::tempdir().expect("tempdir");
    empty(dir.path());
    let store = Store::open(dir.path()).expect("open");

    store
        .writable()
        .upsert_contacts(&[contact("a@example.com", None, 100)])
        .expect("upsert");

    // `%` 是 LIKE 通配符——用户输入 `%` 不该匹配全部。
    let list = store.writable().list_contacts("%", 50).expect("list");
    assert!(
        list.is_empty(),
        "通配符应被转义，实际匹配到 {} 条",
        list.len()
    );
}

#[test]
fn contact_can_be_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    empty(dir.path());
    let store = Store::open(dir.path()).expect("open");

    store
        .writable()
        .upsert_contacts(&[contact("gone@example.com", None, 100)])
        .expect("upsert");
    let list = store.writable().list_contacts("", 50).expect("list");
    assert_eq!(list.len(), 1);

    store.writable().remove_contact(list[0].id).expect("remove");
    assert!(
        store
            .writable()
            .list_contacts("", 50)
            .expect("list")
            .is_empty()
    );
}

#[test]
fn empty_upsert_is_a_no_op() {
    let dir = tempfile::tempdir().expect("tempdir");
    empty(dir.path());
    let store = Store::open(dir.path()).expect("open");

    store.writable().upsert_contacts(&[]).expect("empty upsert");
    assert!(
        store
            .writable()
            .list_contacts("", 50)
            .expect("list")
            .is_empty()
    );
}

#[test]
fn schema_version_is_7_after_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    empty(dir.path());
    let store = Store::open(dir.path()).expect("open");
    assert_eq!(
        store.writable().schema_version().unwrap(),
        me_core::SCHEMA_VERSION
    );
    assert_eq!(me_core::SCHEMA_VERSION, 7);
}
