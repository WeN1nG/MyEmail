//! 联系人累积的集成测试。
//!
//! 验证的是**累积时机与去重**：
//! - 正文补全落库后，收发件人进入 `contacts`；
//! - 同一地址重复出现只累加 `seen_count`，不新增行；
//! - `ClearCache` **不清理**联系人（与 D30 一致）；
//! - 联系人**永不出网**、不参与 AI 请求（靠「不新增依赖 / 不调网络端口」保证，
//!   这里断言累积路径只碰存储层）。

use me_core::model::{Address, Contact};
use me_core::port::{ContactRepo, MessageRepo};
use me_store::Store;

/// 构造一个联系人条目（与 me-sync 的累积逻辑同形）。
fn contact(addr: &str, name: Option<&str>) -> Contact {
    Contact {
        id: me_core::ContactId(0),
        addr: addr.to_string(),
        normalized: Contact::normalize(addr),
        display_name: name.map(|s| s.to_string()),
        last_seen: 1_700_000_000,
        seen_count: 1,
    }
}

#[test]
fn accumulating_contacts_creates_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");

    store
        .writable()
        .upsert_contacts(&[
            contact("alice@example.com", Some("爱丽丝")),
            contact("bob@example.com", None),
        ])
        .expect("upsert");

    let list = store.writable().list_contacts("", 50).expect("list");
    assert_eq!(list.len(), 2);
}

#[test]
fn accumulating_the_same_contact_twice_increments_the_counter() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");

    store
        .writable()
        .upsert_contacts(&[contact("alice@example.com", Some("爱丽丝"))])
        .expect("first");
    store
        .writable()
        .upsert_contacts(&[contact("alice@example.com", Some("爱丽丝"))])
        .expect("second");

    let list = store.writable().list_contacts("", 50).expect("list");
    assert_eq!(list.len(), 1, "重复累积不该新增行");
    assert_eq!(list[0].seen_count, 2, "重复累积应累加 seen_count");
}

#[test]
fn clear_cache_does_not_remove_contacts() {
    // 与 D30 一致：联系人是**本地派生数据**，
    // `ClearCache` 只清正文 / 原始邮件 / 附件 / FTS 正文列，不清联系人。
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");

    store
        .writable()
        .upsert_contacts(&[contact("alice@example.com", Some("爱丽丝"))])
        .expect("upsert");
    assert_eq!(
        store.writable().list_contacts("", 50).expect("list").len(),
        1
    );

    // 清缓存（走既有的 `reset_body_cache` 路径——它清正文 / 原始邮件 / 附件 / FTS）。
    store
        .writable()
        .reset_body_cache(None)
        .expect("clear cache");

    let list = store.writable().list_contacts("", 50).expect("list");
    assert_eq!(list.len(), 1, "ClearCache 不该清理联系人");
}

#[test]
fn contact_normalization_is_case_insensitive() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");

    store
        .writable()
        .upsert_contacts(&[contact("Alice@Example.COM", None)])
        .expect("first");
    store
        .writable()
        .upsert_contacts(&[contact("  alice@example.com  ", None)])
        .expect("second");

    let list = store.writable().list_contacts("", 50).expect("list");
    assert_eq!(list.len(), 1, "大小写与空白不同应归一到同一行");
}

#[test]
fn removing_a_contact_is_possible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");

    store
        .writable()
        .upsert_contacts(&[contact("alice@example.com", None)])
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
fn addresses_without_a_valid_local_part_are_skipped() {
    // 空地址不该进表——累积时会被 `Contact::normalize` 过滤掉。
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");

    let mut empty = contact("", None);
    empty.normalized = String::new();
    store.writable().upsert_contacts(&[empty]).expect("upsert");

    assert!(
        store
            .writable()
            .list_contacts("", 50)
            .expect("list")
            .is_empty()
    );
}

#[test]
fn a_contact_carries_its_display_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");

    let address = Address::new(Some("张三".to_string()), "zhang@example.com");
    store
        .writable()
        .upsert_contacts(&[Contact {
            id: me_core::ContactId(0),
            addr: address.addr.clone(),
            normalized: Contact::normalize(&address.addr),
            display_name: address.name.clone(),
            last_seen: 1_700_000_000,
            seen_count: 1,
        }])
        .expect("upsert");

    let list = store.writable().list_contacts("", 50).expect("list");
    assert_eq!(list[0].display_name.as_deref(), Some("张三"));
}
