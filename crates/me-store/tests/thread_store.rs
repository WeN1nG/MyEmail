//! 线程两列（`in_reply_to` / `"references"`）与回填队列的存储层测试。

use me_core::command::AccountDraft;
use me_core::model::{
    AccountId, AuthMethod, DateSource, Endpoint, Flags, FolderId, MessageId, NewMessage, TlsMode,
};
use me_core::port::{AccountRepo, FolderInfo, FolderRepo, MessageRepo, ReadOnlyRepo};
use me_store::Store;

fn seed(store: &Store) -> (AccountId, FolderId) {
    let writable = store.writable();
    let account = writable
        .create_account(&AccountDraft {
            email: "a@example.com".into(),
            display_name: None,
            imap: Endpoint::new("imap.invalid", 993, TlsMode::Implicit),
            smtp: Endpoint::new("smtp.invalid", 465, TlsMode::Implicit),
            auth_user: "a@example.com".into(),
            auth_secret: None,
            auth_method: AuthMethod::Password,
            oauth_provider: None,
            oauth_subject: None,
            plaintext_tls_ack: false,
        })
        .expect("account");
    let inbox = writable
        .upsert_folder(
            account.id,
            &FolderInfo {
                name: "INBOX".into(),
                delimiter: Some('/'),
                attributes: vec!["\\Inbox".into()],
            },
        )
        .expect("folder");
    (account.id, inbox.id)
}

fn new_message(
    uid: u32,
    message_id: &str,
    in_reply_to: Option<&str>,
    references: &[&str],
) -> NewMessage {
    NewMessage {
        uid: Some(uid),
        message_id: Some(message_id.to_string()),
        subject: Some(format!("邮件 {uid}")),
        from: None,
        to: Vec::new(),
        cc: Vec::new(),
        date: Some(1000 + i64::from(uid)),
        date_source: DateSource::Header,
        size: None,
        flags: Flags::NONE,
        has_attachment: false,
        in_reply_to: in_reply_to.map(str::to_string),
        references: references.iter().map(|r| r.to_string()).collect(),
    }
}

fn insert(store: &Store, account: AccountId, folder: FolderId, message: NewMessage) -> MessageId {
    store
        .writable()
        .insert_messages(account, folder, &[message], 100)
        .expect("insert")[0]
        .id
}

#[test]
fn thread_headers_survive_the_roundtrip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");
    let (account, folder) = seed(&store);

    let id = insert(
        &store,
        account,
        folder,
        new_message(1, "c@x", Some("b@x"), &["a@x", "b@x"]),
    );

    let summary = store
        .read_only()
        .get_message_readonly(id)
        .expect("read")
        .expect("exists");
    assert_eq!(summary.in_reply_to.as_deref(), Some("b@x"));
    assert_eq!(
        summary.references,
        vec!["a@x".to_string(), "b@x".to_string()]
    );

    // 列表查询走的是另一组列（`SUMMARY_COLUMNS_M`），也要带出来。
    let listed = store
        .read_only()
        .list_messages_readonly(folder, None, 10)
        .expect("list");
    assert_eq!(listed[0].in_reply_to.as_deref(), Some("b@x"));
    assert_eq!(listed[0].references.len(), 2, "列表路径也要读到引用链");
}

#[test]
fn empty_references_are_stored_as_null_on_insert() {
    // 入库时没有回复头 → NULL（= 待回填）。回填之后才写 "[]"。
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");
    let (account, folder) = seed(&store);
    let id = insert(&store, account, folder, new_message(1, "solo@x", None, &[]));

    assert!(
        store
            .writable()
            .thread_backlog(0, 10)
            .expect("backlog")
            .is_empty(),
        "还没有 body_path，不该进队列"
    );
    store
        .writable()
        .set_body_cached(id, "bodies/1.txt", "正文", None)
        .expect("body");
    assert_eq!(
        store.writable().thread_backlog(0, 10).expect("backlog"),
        vec![id],
        "正文补全 + 两列 NULL → 进回填队列"
    );
}

#[test]
fn backlog_skips_processed_and_bodyless_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");
    let (account, folder) = seed(&store);

    let pending = insert(&store, account, folder, new_message(1, "p@x", None, &[]));
    let done = insert(
        &store,
        account,
        folder,
        new_message(2, "d@x", Some("p@x"), &["p@x"]),
    );
    let no_body = insert(&store, account, folder, new_message(3, "n@x", None, &[]));

    store
        .writable()
        .set_body_cached(pending, "bodies/1.txt", "正文", None)
        .expect("body pending");
    store
        .writable()
        .set_body_cached(done, "bodies/2.txt", "正文", None)
        .expect("body done");
    // `no_body` 不写 body_path。

    let backlog = store.writable().thread_backlog(0, 100).expect("backlog");
    assert_eq!(
        backlog,
        vec![pending],
        "只有「有正文 + 两列都是 NULL」的进队列；入库时已带引用链的不进"
    );
    assert!(!backlog.contains(&no_body));
}

#[test]
fn backlog_cursor_advances_past_unreadable_rows() {
    // 读不到 raw 的邮件会一直留在队列里——游标必须往下走，否则整轮卡死。
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");
    let (account, folder) = seed(&store);

    let ids: Vec<MessageId> = (1..=3)
        .map(|uid| {
            let id = insert(&store, account, folder, new_message(uid, "x@x", None, &[]));
            store
                .writable()
                .set_body_cached(id, "bodies/x.txt", "正文", None)
                .expect("body");
            id
        })
        .collect();

    let first = store.writable().thread_backlog(0, 2).expect("first");
    assert_eq!(first, ids[..2].to_vec());
    let second = store
        .writable()
        .thread_backlog(first.last().unwrap().0, 2)
        .expect("second");
    assert_eq!(second, ids[2..].to_vec(), "游标之后才是下一批");
}

#[test]
fn backfill_marks_empty_chain_as_json_array() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");
    let (account, folder) = seed(&store);
    let id = insert(&store, account, folder, new_message(1, "solo@x", None, &[]));
    store
        .writable()
        .set_body_cached(id, "bodies/1.txt", "正文", None)
        .expect("body");

    store
        .writable()
        .set_thread_headers(id, None, &[])
        .expect("write headers");

    assert!(
        store
            .writable()
            .thread_backlog(0, 10)
            .expect("backlog")
            .is_empty(),
        "写回 \"[]\" 之后不该再入队"
    );
    let summary = store
        .read_only()
        .get_message_readonly(id)
        .expect("read")
        .expect("exists");
    assert!(summary.references.is_empty());
    assert!(summary.in_reply_to.is_none());
}
