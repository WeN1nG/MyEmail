//! me-store 集成测试：使用临时目录与真实 SQLite（FTS5 trigram）。

use me_core::Attachment;
use me_core::command::AccountDraft;
use me_core::model::{
    AccountId, Address, AuthMethod, DateSource, DraftId, DraftStatus, Endpoint, FlagOp, Flags,
    Folder, FolderId, FolderRole, MessageId, NewMessage, OAuthProvider, SearchQuery, TlsMode,
};
use me_core::port::{
    AccountRepo, AttachmentRepo, BodyCoverage, DraftRepo, FolderInfo, FolderRepo, MessageRepo,
    MetaRepo, MoveOrigin, NewDraft, PendingOpKind, PendingRepo, ReadOnlyRepo, SearchRepo,
};
use me_store::{CachePaths, ReadOnlyStore, Store, WritableStore, cache};

// ---------------------------------------------------------------------------
// 帮助函数
// ---------------------------------------------------------------------------

fn open_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open store");
    (dir, store)
}

fn account_draft(email: &str) -> AccountDraft {
    AccountDraft {
        email: email.to_string(),
        display_name: Some("测试用户".to_string()),
        imap: Endpoint::new("imap.example.com", 993, TlsMode::Implicit),
        smtp: Endpoint::new("smtp.example.com", 465, TlsMode::Implicit),
        auth_user: email.to_string(),
        auth_secret: Some("should-not-be-stored".to_string()),
        auth_method: AuthMethod::Password,
        oauth_provider: None,
        oauth_subject: None,
        plaintext_tls_ack: false,
    }
}

fn upsert_folder(store: &Store, account: AccountId, name: &str, attributes: &[&str]) -> Folder {
    store
        .writable()
        .upsert_folder(
            account,
            &FolderInfo {
                name: name.to_string(),
                delimiter: Some('/'),
                attributes: attributes.iter().map(|s| s.to_string()).collect(),
            },
        )
        .expect("upsert folder")
}

fn setup(store: &Store) -> (AccountId, FolderId) {
    let account = store
        .writable()
        .create_account(&account_draft("alice@example.com"))
        .expect("create account");
    let inbox = upsert_folder(store, account.id, "INBOX", &["\\Inbox"]);
    (account.id, inbox.id)
}

fn msg(uid: u32, message_id: &str, subject: &str, date: i64) -> NewMessage {
    NewMessage {
        uid: Some(uid),
        message_id: Some(message_id.to_string()),
        subject: Some(subject.to_string()),
        from: Some(Address::new(Some("Bob".into()), "bob@example.com")),
        to: vec![Address::new(Some("Alice".into()), "alice@example.com")],
        cc: vec![],
        date: Some(date),
        date_source: DateSource::Header,
        size: Some(1024 + uid),
        flags: Flags::NONE,
        has_attachment: false,
        in_reply_to: None,
        references: Vec::new(),
    }
}

fn insert(
    store: &Store,
    account: AccountId,
    folder: FolderId,
    messages: &[NewMessage],
) -> Vec<me_core::MessageSummary> {
    let last_uid = messages.iter().filter_map(|m| m.uid).max().unwrap_or(0);
    store
        .writable()
        .insert_messages(account, folder, messages, last_uid)
        .expect("insert messages")
}

// ---------------------------------------------------------------------------
// schema / 连接
// ---------------------------------------------------------------------------

#[test]
fn schema_creation_and_version() {
    let (dir, store) = open_store();
    let writable = store.writable();
    // 只断言「跟随常量」：写死数字的那行会在每次加迁移时变成噪音。
    assert_eq!(writable.schema_version().unwrap(), me_core::SCHEMA_VERSION);

    let paths = store.paths();
    assert!(paths.db_file().exists());
    for dir in [
        paths.bodies_dir(),
        paths.raw_dir(),
        paths.attach_dir(),
        paths.drafts_dir(),
        paths.logs_dir(),
    ] {
        assert!(dir.is_dir(), "missing {}", dir.display());
    }
    // 子目录都应位于临时根下。
    assert!(paths.root().starts_with(dir.path()));

    // 关闭后重开：schema 不重复创建，版本不变。
    drop(writable);
    drop(store);
    let reopened = Store::open(dir.path()).expect("reopen");
    assert_eq!(
        reopened.writable().schema_version().unwrap(),
        me_core::SCHEMA_VERSION
    );
}

#[test]
fn foreign_keys_are_enforced() {
    let (_dir, store) = open_store();
    let writable = store.writable();
    let bad_account = AccountId(9999);
    let draft = NewDraft {
        account: bad_account,
        message_id: "x@example.com".into(),
        subject: Some("s".into()),
        to: vec![],
        bcc: vec![],
        raw_path: "drafts/1/message.eml".into(),
    };
    assert!(
        writable.create_draft(&draft).is_err(),
        "FK must reject unknown account"
    );

    // 反向确认：合法账户可以写入。
    let account = writable
        .create_account(&account_draft("ok@example.com"))
        .unwrap();
    let draft = NewDraft {
        account: account.id,
        ..draft
    };
    assert!(writable.create_draft(&draft).is_ok());
}

#[test]
fn writable_and_readonly_are_send_sync_store_port() {
    fn assert_store_port<T: me_core::port::StorePort>() {}
    fn assert_send_sync<T: Send + Sync + 'static>() {}
    assert_store_port::<WritableStore>();
    assert_send_sync::<WritableStore>();
    assert_send_sync::<ReadOnlyStore>();
}

// ---------------------------------------------------------------------------
// 账户 / 文件夹
// ---------------------------------------------------------------------------

#[test]
fn account_crud_and_email_uniqueness() {
    let (_dir, store) = open_store();
    let writable = store.writable();

    let created = writable
        .create_account(&account_draft("a@example.com"))
        .unwrap();
    assert_eq!(created.email, "a@example.com");
    assert_eq!(created.imap.port, 993);
    assert_eq!(created.smtp.port, 465);
    assert_eq!(writable.list_accounts().unwrap().len(), 1);

    // 唯一约束
    assert!(
        writable
            .create_account(&account_draft("a@example.com"))
            .is_err()
    );

    let mut changed = account_draft("a@example.com");
    changed.display_name = Some("新名字".into());
    let updated = writable.update_account(created.id, &changed).unwrap();
    assert_eq!(updated.display_name.as_deref(), Some("新名字"));

    assert_eq!(writable.count_drafts(created.id).unwrap(), 0);
    assert_eq!(
        writable.delete_account(created.id, false).unwrap(),
        me_core::port::DeleteAccountOutcome::Deleted
    );
    assert!(writable.get_account(created.id).unwrap().is_none());
    assert_eq!(
        writable.delete_account(created.id, false).unwrap(),
        me_core::port::DeleteAccountOutcome::NotFound
    );
}

#[test]
fn delete_account_refuses_when_drafts_exist() {
    let (_dir, store) = open_store();
    let writable = store.writable();
    let account = writable
        .create_account(&account_draft("draft@example.com"))
        .unwrap();
    writable
        .create_draft(&NewDraft {
            account: account.id,
            message_id: "m@example.com".into(),
            subject: Some("草稿".into()),
            to: vec![],
            bcc: vec![],
            raw_path: "drafts/1/message.eml".into(),
        })
        .unwrap();

    assert_eq!(
        writable.delete_account(account.id, false).unwrap(),
        me_core::port::DeleteAccountOutcome::RefusedDraftsExist(1)
    );
    assert_eq!(
        writable.delete_account(account.id, true).unwrap(),
        me_core::port::DeleteAccountOutcome::Deleted
    );
    assert!(writable.list_drafts(Some(account.id)).unwrap().is_empty());
}

#[test]
fn folder_upsert_keeps_role_and_trash_is_unique() {
    let (_dir, store) = open_store();
    let writable = store.writable();
    let account = writable
        .create_account(&account_draft("folders@example.com"))
        .unwrap();

    let custom = upsert_folder(&store, account.id, "自定义/归档", &["\\Archive"]);
    assert_eq!(custom.role, FolderRole::Archive);

    // 再次 upsert：名称启发式为 Other，保留已有角色。
    let again = store
        .writable()
        .upsert_folder(
            account.id,
            &FolderInfo {
                name: "自定义/归档".into(),
                delimiter: Some('/'),
                attributes: vec![],
            },
        )
        .unwrap();
    assert_eq!(again.id, custom.id);
    assert_eq!(again.role, FolderRole::Archive);

    let trash = upsert_folder(&store, account.id, "废件箱", &["\\Trash"]);
    assert_eq!(trash.role, FolderRole::Trash);

    // 第二个 trash 角色违反部分唯一索引。
    assert!(
        store
            .writable()
            .upsert_folder(
                account.id,
                &FolderInfo {
                    name: "另一个废件箱".into(),
                    delimiter: Some('/'),
                    attributes: vec!["\\Trash".into()],
                },
            )
            .is_err()
    );

    // 服务器删除标记在重新出现时清除。
    writable.set_folder_deleted(trash.id, true).unwrap();
    assert!(
        writable
            .get_folder(trash.id)
            .unwrap()
            .unwrap()
            .deleted_on_server
    );
    let reappeared = upsert_folder(&store, account.id, "废件箱", &["\\Trash"]);
    assert!(!reappeared.deleted_on_server);

    assert_eq!(
        writable
            .find_folder(account.id, "废件箱")
            .unwrap()
            .unwrap()
            .id,
        trash.id
    );
    assert!(
        writable
            .find_folder(account.id, "不存在")
            .unwrap()
            .is_none()
    );
    assert_eq!(writable.list_folders(account.id).unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// 邮件入库 / 分页 / 去重
// ---------------------------------------------------------------------------

#[test]
fn insert_batch_advances_last_uid_and_counts() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let writable = store.writable();

    let messages = vec![
        msg(1, "m1@example.com", "hello one", 100),
        msg(2, "m2@example.com", "hello two", 200),
        msg(3, "m3@example.com", "hello three", 300),
    ];
    let inserted = insert(&store, account, inbox, &messages);
    assert_eq!(inserted.len(), 3);
    assert_eq!(inserted[0].id, MessageId(1));
    assert_eq!(inserted[2].uid, Some(3));
    assert!(inserted[0].from.is_some());
    assert_eq!(inserted[0].to.len(), 1);

    assert_eq!(writable.get_folder(inbox).unwrap().unwrap().last_uid, 3);
    assert_eq!(writable.unread_count(inbox).unwrap(), 3);
    assert_eq!(writable.list_messages(inbox, None, 50).unwrap().len(), 3);

    // 摘要只含头部，正文未缓存。
    assert!(!inserted[0].body_cached);
    assert!(inserted[0].snippet.is_none());

    // 标志入库后未读计数变化。
    let seen = writable
        .apply_local_flags(&[inserted[0].id], FlagOp::Set(Flags::SEEN))
        .unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(writable.unread_count(inbox).unwrap(), 2);
}

#[test]
fn keyset_pagination_has_no_overlap() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let messages: Vec<_> = (1..=5)
        .map(|i| {
            msg(
                i,
                &format!("m{i}@example.com"),
                &format!("subject {i}"),
                i as i64 * 10,
            )
        })
        .collect();
    insert(&store, account, inbox, &messages);
    let writable = store.writable();

    let page1 = writable.list_messages(inbox, None, 2).unwrap();
    assert_eq!(page1.len(), 2);
    assert_eq!(page1[0].date, Some(50));
    assert_eq!(page1[1].date, Some(40));

    let cursor = (page1[1].date.unwrap(), page1[1].id.0);
    let page2 = writable.list_messages(inbox, Some(cursor), 2).unwrap();
    assert_eq!(page2.len(), 2);
    assert_eq!(page2[0].date, Some(30));
    assert_eq!(page2[1].date, Some(20));

    let cursor = (page2[1].date.unwrap(), page2[1].id.0);
    let page3 = writable.list_messages(inbox, Some(cursor), 2).unwrap();
    assert_eq!(page3.len(), 1);
    assert_eq!(page3[0].date, Some(10));

    let ids: Vec<_> = page1
        .iter()
        .chain(page2.iter())
        .chain(page3.iter())
        .map(|m| m.id)
        .collect();
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(ids.len(), unique.len());

    // 超出硬上限时被夹到 200（此处数据量小，仅验证不报错）。
    assert_eq!(
        writable.list_messages(inbox, None, 10_000).unwrap().len(),
        5
    );
}

#[test]
fn inserted_message_with_null_uid_is_deduped_on_server_reappearance() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let trash = upsert_folder(&store, account, "Trash", &["\\Trash"]);
    let writable = store.writable();

    let inserted = insert(
        &store,
        account,
        inbox,
        &[msg(7, "dup@example.com", "移动我", 10)],
    );
    let original_id = inserted[0].id;

    writable
        .move_to_trash(
            &[MoveOrigin {
                message: original_id,
                source_folder: inbox,
                source_folder_name: "INBOX".into(),
                source_uid: 7,
            }],
            trash.id,
        )
        .unwrap();
    let moved = writable.get_message(original_id).unwrap().unwrap();
    assert_eq!(moved.folder, trash.id);
    assert_eq!(moved.uid, None);
    assert_eq!(writable.pending_ops(account, 10).unwrap().len(), 1);

    // 服务器以新 UID 再次出现同一 Message-ID：补 UID，不新增行。
    let returned = writable
        .insert_messages(
            account,
            trash.id,
            &[msg(42, "dup@example.com", "移动我", 10)],
            42,
        )
        .unwrap();
    assert_eq!(returned.len(), 1);
    assert_eq!(returned[0].id, original_id);
    assert_eq!(returned[0].uid, Some(42));
    assert_eq!(writable.list_messages(trash.id, None, 10).unwrap().len(), 1);
    assert!(writable.pending_ops(account, 10).unwrap().is_empty());
}

#[test]
fn null_message_id_is_never_deduped() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let writable = store.writable();

    let mut first = msg(1, "unused", "无 Message-ID", 1);
    first.message_id = None;
    insert(&store, account, inbox, &[first]);

    // 先移动到 trash 使其 uid 为 NULL。
    let trash = upsert_folder(&store, account, "Trash", &["\\Trash"]);
    let id = writable.list_messages(inbox, None, 1).unwrap()[0].id;
    writable
        .move_to_trash(
            &[MoveOrigin {
                message: id,
                source_folder: inbox,
                source_folder_name: "INBOX".into(),
                source_uid: 1,
            }],
            trash.id,
        )
        .unwrap();

    let mut duplicate = msg(2, "unused", "无 Message-ID", 1);
    duplicate.message_id = None;
    writable
        .insert_messages(account, trash.id, &[duplicate], 2)
        .unwrap();
    assert_eq!(writable.list_messages(trash.id, None, 10).unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// 标志
// ---------------------------------------------------------------------------

/// 星标是「重要」在本地唯一可靠的显式标注：检索必须能只按它过滤，
/// 且与未读、时间窗这些既有条件**取交集**而不是互相覆盖。
#[test]
fn search_flagged_only_returns_only_starred() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let messages = vec![
        msg(1, "starred1@example.com", "starred one", 10),
        msg(2, "plain@example.com", "plain two", 20),
        msg(3, "starred3@example.com", "starred three", 30),
    ];
    let inserted = insert(&store, account, inbox, &messages);
    let writable = store.writable();
    writable
        .apply_local_flags(
            &[inserted[0].id, inserted[2].id],
            FlagOp::Set(Flags::FLAGGED),
        )
        .unwrap();

    let starred = writable
        .search(&SearchQuery {
            flagged_only: true,
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(starred.total, 2, "只应命中两封星标邮件");
    assert!(
        starred.messages.iter().all(|m| m.id != inserted[1].id),
        "未星标的邮件不能混进来"
    );

    // 与未读取交集：三封都没有 \Seen，所以两封星标仍然都命中。
    let starred_unread = writable
        .search(&SearchQuery {
            flagged_only: true,
            unread_only: true,
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(starred_unread.total, 2, "两个条件应取交集");

    // 关掉星标条件后回到全量，证明它没有污染其它查询。
    let all = writable
        .search(&SearchQuery {
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(all.total, 3);
}

#[test]
fn local_flag_toggles_collapse_to_one_target_state() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let messages = vec![msg(1, "f1@example.com", "flags", 10)];
    let id = insert(&store, account, inbox, &messages)[0].id;
    let writable = store.writable();

    writable
        .apply_local_flags(&[id], FlagOp::Set(Flags::FLAGGED))
        .unwrap();
    writable
        .apply_local_flags(&[id], FlagOp::Set(Flags::SEEN))
        .unwrap();
    writable
        .apply_local_flags(&[id], FlagOp::Clear(Flags::SEEN))
        .unwrap();

    let pending = writable.pending_flags(10).unwrap();
    assert_eq!(pending.len(), 1, "多次变更只应保留一行");
    assert_eq!(pending[0].0, id);
    assert_eq!(pending[0].1, Flags::FLAGGED);
    assert_eq!(pending[0].2, 0, "目标态不累加 attempts");

    let stored = writable.get_message(id).unwrap().unwrap();
    assert_eq!(stored.flags, Flags::FLAGGED);
}

#[test]
fn server_flag_pull_skips_rows_with_pending_changes() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let messages = vec![
        msg(1, "s1@example.com", "one", 10),
        msg(2, "s2@example.com", "two", 20),
        msg(3, "s3@example.com", "three", 30),
    ];
    let inserted = insert(&store, account, inbox, &messages);
    let writable = store.writable();

    // 第 2 封有本地未同步标志（星标）。
    writable
        .apply_local_flags(&[inserted[1].id], FlagOp::Set(Flags::FLAGGED))
        .unwrap();

    let changes = writable
        .apply_server_flags(
            inbox,
            &[
                (1, Flags::SEEN),
                (2, Flags::SEEN),
                (3, Flags::SEEN | Flags::ANSWERED),
            ],
        )
        .unwrap();
    assert_eq!(changes.len(), 2, "挂起行必须跳过");
    assert!(changes.iter().all(|(id, _)| *id != inserted[1].id));

    assert_eq!(
        writable.get_message(inserted[0].id).unwrap().unwrap().flags,
        Flags::SEEN
    );
    assert_eq!(
        writable.get_message(inserted[1].id).unwrap().unwrap().flags,
        Flags::FLAGGED,
        "本地目标态优先"
    );
    assert_eq!(writable.pending_flags(10).unwrap().len(), 1);

    // 再次回拉相同值不应报告变更。
    let again = writable
        .apply_server_flags(inbox, &[(1, Flags::SEEN)])
        .unwrap();
    assert!(again.is_empty());
}

// ---------------------------------------------------------------------------
// FTS / 搜索
// ---------------------------------------------------------------------------

#[test]
fn fts_head_search_body_update_and_delete() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let writable = store.writable();
    let inserted = insert(
        &store,
        account,
        inbox,
        &[msg(1, "q@example.com", "Quarterly report", 10)],
    );
    let id = inserted[0].id;

    // 头部搜索（trigram 需 >= 3 字符）。
    let outcome = writable
        .search(&SearchQuery {
            text: "Quarterly".into(),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(outcome.messages.len(), 1);
    assert_eq!(outcome.total, 1);
    assert_eq!(outcome.query_id, 0);

    // 正文写入后可按正文搜索。
    let paths = store.paths();
    let file = cache::write_body(&paths, id, "The quick brown fox jumps").unwrap();
    writable
        .set_body_cached(
            id,
            &file.to_string_lossy(),
            "The quick brown fox jumps",
            Some("The quick"),
        )
        .unwrap();

    let outcome = writable
        .search(&SearchQuery {
            text: "brown".into(),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(outcome.messages.len(), 1);
    assert_eq!(outcome.body_indexed, 1);
    assert_eq!(outcome.body_total, 1);

    // 摘要与 body_cached 同步。
    let summary = writable.get_message(id).unwrap().unwrap();
    assert!(summary.body_cached);
    assert_eq!(summary.snippet.as_deref(), Some("The quick"));

    // 删除后 FTS 行一并清除。
    writable.delete_permanent(&[id]).unwrap();
    let outcome = writable
        .search(&SearchQuery {
            text: "brown".into(),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert!(outcome.messages.is_empty());
    let outcome = writable
        .search(&SearchQuery {
            text: "Quarterly".into(),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert!(outcome.messages.is_empty());
}

#[test]
fn short_query_uses_header_like_only() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let writable = store.writable();
    insert(
        &store,
        account,
        inbox,
        &[msg(1, "xy@example.com", "XY 标记", 10)],
    );

    let outcome = writable
        .search(&SearchQuery {
            text: "XY".into(),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(outcome.messages.len(), 1);

    // 2 字符词不查正文：正文含 "XY" 但头部不含时不应命中。
    let mut other = msg(2, "other@example.com", "zebra", 20);
    other.uid = Some(2);
    let second = insert(&store, account, inbox, &[other])[0].id;
    let paths = store.paths();
    let file = cache::write_body(&paths, second, "XY only in body").unwrap();
    writable
        .set_body_cached(second, &file.to_string_lossy(), "XY only in body", None)
        .unwrap();

    let outcome = writable
        .search(&SearchQuery {
            text: "XY".into(),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(outcome.messages.len(), 1, "1~2 字符只走头部 LIKE");
    assert_eq!(outcome.messages[0].id.0, 1);
}

#[test]
fn search_applies_filters_and_cursor() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let other_folder = upsert_folder(&store, account, "Archive", &["\\Archive"]);
    let writable = store.writable();

    let mut with_attachment = msg(1, "att@example.com", "附件邮件", 100);
    with_attachment.has_attachment = true;
    insert(&store, account, inbox, &[with_attachment]);
    insert(
        &store,
        account,
        other_folder.id,
        &[msg(2, "arch@example.com", "归档邮件", 200)],
    );

    // 文件夹过滤
    let outcome = writable
        .search(&SearchQuery {
            text: String::new(),
            folders: vec![other_folder.id],
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(outcome.total, 1);
    assert_eq!(outcome.messages[0].folder, other_folder.id);

    // 账户过滤
    let outcome = writable
        .search(&SearchQuery {
            accounts: vec![account],
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(outcome.total, 2);

    // 时间范围
    let outcome = writable
        .search(&SearchQuery {
            since: Some(150),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(outcome.total, 1);

    // 附件过滤
    let outcome = writable
        .search(&SearchQuery {
            has_attachment: Some(true),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(outcome.total, 1);

    // 键集游标：limit=1 后取下一页。
    let first = writable
        .search(&SearchQuery {
            limit: 1,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(first.messages.len(), 1);
    let cursor = (first.messages[0].date.unwrap(), first.messages[0].id.0);
    let second = writable
        .search(&SearchQuery {
            limit: 1,
            cursor: Some(cursor),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(second.messages.len(), 1);
    assert_ne!(second.messages[0].id, first.messages[0].id);
    // total 不受游标影响。
    assert_eq!(second.total, 2);
}

#[test]
fn fts_query_operators_are_escaped() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let writable = store.writable();
    insert(
        &store,
        account,
        inbox,
        &[msg(1, "op@example.com", "hello OR world", 10)],
    );

    // 若未整体加引号，`OR` 会被解释为运算符；这里应作为短语且不 panic。
    let outcome = writable
        .search(&SearchQuery {
            text: "hello OR world".into(),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(outcome.total, 1);

    // 引号加倍后仍是合法查询。
    let outcome = writable
        .search(&SearchQuery {
            text: "\"hello\"".into(),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert!(outcome.messages.len() <= 1);
}

// ---------------------------------------------------------------------------
// UIDVALIDITY / 删除 / 移动
// ---------------------------------------------------------------------------

#[test]
fn clear_folder_removes_messages_and_fts() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let writable = store.writable();
    let messages = vec![
        msg(1, "c1@example.com", "alpha", 10),
        msg(2, "c2@example.com", "alpha", 20),
        msg(3, "c3@example.com", "alpha", 30),
    ];
    insert(&store, account, inbox, &messages);

    assert_eq!(writable.clear_folder(inbox).unwrap(), 3);
    assert!(writable.list_messages(inbox, None, 10).unwrap().is_empty());
    assert_eq!(writable.unread_count(inbox).unwrap(), 0);

    let outcome = writable
        .search(&SearchQuery {
            text: "alpha".into(),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(outcome.total, 0, "FTS 行必须同步清除");

    // 重建后仍可继续写入。
    insert(
        &store,
        account,
        inbox,
        &[msg(4, "c4@example.com", "beta", 40)],
    );
    assert_eq!(writable.list_messages(inbox, None, 10).unwrap().len(), 1);
}

#[test]
fn move_to_trash_keeps_pending_op_and_nulls_uid() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let trash = upsert_folder(&store, account, "Trash", &["\\Trash"]);
    let inserted = insert(
        &store,
        account,
        inbox,
        &[msg(5, "mv@example.com", "move", 10)],
    );
    let id = inserted[0].id;
    let writable = store.writable();

    writable
        .move_to_trash(
            &[MoveOrigin {
                message: id,
                source_folder: inbox,
                source_folder_name: "INBOX".into(),
                source_uid: 5,
            }],
            trash.id,
        )
        .unwrap();

    let summary = writable.get_message(id).unwrap().unwrap();
    assert_eq!(summary.folder, trash.id);
    assert_eq!(summary.uid, None);
    assert_eq!(summary.account, account);

    let ops = writable.pending_ops(account, 10).unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].kind, PendingOpKind::MoveToTrash);
    assert_eq!(ops[0].message, Some(id));
    assert_eq!(ops[0].source_folder, Some(inbox));
    assert_eq!(ops[0].source_folder_name.as_deref(), Some("INBOX"));
    assert_eq!(ops[0].source_uid, Some(5));
    assert_eq!(ops[0].target_folder, Some(trash.id));

    // 服务器确认后回填 UID。
    let confirmed = writable
        .confirm_moved_uid(trash.id, "mv@example.com", 99)
        .unwrap();
    assert_eq!(confirmed, Some(id));
    assert_eq!(writable.get_message(id).unwrap().unwrap().uid, Some(99));
    assert!(
        writable
            .confirm_moved_uid(trash.id, "mv@example.com", 100)
            .unwrap()
            .is_none()
    );
}

#[test]
fn delete_permanent_keeps_pending_op_after_message_removed() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let writable = store.writable();
    let inserted = insert(
        &store,
        account,
        inbox,
        &[msg(8, "del@example.com", "delete", 10)],
    );
    let id = inserted[0].id;

    writable
        .upsert_attachments(
            id,
            &[Attachment {
                part_index: 1,
                filename: Some("a.txt".into()),
                mime_type: Some("text/plain".into()),
                size: Some(3),
                is_inline: false,
                content_id: None,
                local_path: None,
            }],
        )
        .unwrap();

    let origins = writable.delete_permanent(&[id]).unwrap();
    assert_eq!(origins.len(), 1);
    assert_eq!(origins[0].message, id);
    assert_eq!(origins[0].source_folder, inbox);
    assert_eq!(origins[0].source_folder_name, "INBOX");
    assert_eq!(origins[0].source_uid, 8);

    assert!(writable.get_message(id).unwrap().is_none());
    assert!(
        writable.list_attachments(id).unwrap().is_empty(),
        "附件级联删除"
    );

    let ops = writable.pending_ops(account, 10).unwrap();
    assert_eq!(ops.len(), 1, "pending_ops 必须比本地行活得更久");
    assert_eq!(ops[0].kind, PendingOpKind::DeletePermanent);
    assert_eq!(ops[0].message, None, "ON DELETE SET NULL");
    assert_eq!(ops[0].source_folder, Some(inbox));
    assert_eq!(ops[0].source_folder_name.as_deref(), Some("INBOX"));
    assert_eq!(ops[0].source_uid, Some(8));

    let outcome = writable
        .search(&SearchQuery {
            text: "delete".into(),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(outcome.total, 0);
}

#[test]
fn deleting_account_or_folder_clears_fts_rows() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let writable = store.writable();
    insert(
        &store,
        account,
        inbox,
        &[
            msg(1, "ftsf1@example.com", "accountalpha", 10),
            msg(2, "ftsf2@example.com", "inboxalpha", 20),
        ],
    );

    // 删除文件夹：消息级联删除，FTS 同步清理。
    let other = upsert_folder(&store, account, "Other", &[]);
    insert(
        &store,
        account,
        other.id,
        &[msg(3, "ftsf3@example.com", "folderalpha", 30)],
    );
    writable.delete_folder(other.id).unwrap();
    let outcome = writable
        .search(&SearchQuery {
            text: "folderalpha".into(),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(outcome.total, 0, "文件夹删除后不应残留 FTS 行");

    // 删除账户：messages 级联删除，FTS 同步清理。
    assert_eq!(
        writable.delete_account(account, true).unwrap(),
        me_core::port::DeleteAccountOutcome::Deleted
    );
    let outcome = writable
        .search(&SearchQuery {
            text: "accountalpha".into(),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(outcome.total, 0, "账户删除后不应残留 FTS 行");
}

// ---------------------------------------------------------------------------
// 正文队列 / 覆盖率 / 清理 / 重建
// ---------------------------------------------------------------------------

#[test]
fn body_jobs_and_coverage_respect_uid_and_filters() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let trash = upsert_folder(&store, account, "Trash", &["\\Trash"]);
    let writable = store.writable();
    let inserted = insert(
        &store,
        account,
        inbox,
        &[
            msg(1, "b1@example.com", "one", 10),
            msg(2, "b2@example.com", "two", 20),
        ],
    );
    let moved = inserted[1].id;
    writable
        .move_to_trash(
            &[MoveOrigin {
                message: moved,
                source_folder: inbox,
                source_folder_name: "INBOX".into(),
                source_uid: 2,
            }],
            trash.id,
        )
        .unwrap();

    // 本地移动后的 uid IS NULL 行不进入补全队列。
    let jobs = writable.body_jobs(account, 10).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].message, inserted[0].id);
    assert_eq!(writable.pending_body_count(account).unwrap(), 1);

    let coverage = writable.body_coverage(&[account], &[inbox]).unwrap();
    assert_eq!(
        coverage,
        BodyCoverage {
            indexed: 0,
            total: 1
        }
    );

    let paths = store.paths();
    let file = cache::write_body(&paths, inserted[0].id, "covered body").unwrap();
    writable
        .set_body_cached(
            inserted[0].id,
            &file.to_string_lossy(),
            "covered body",
            None,
        )
        .unwrap();

    assert!(writable.body_jobs(account, 10).unwrap().is_empty());
    assert_eq!(
        writable.body_coverage(&[account], &[inbox]).unwrap(),
        BodyCoverage {
            indexed: 1,
            total: 1
        }
    );
}

#[test]
fn body_jobs_are_ordered_newest_first_by_date_not_uid() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    // UID 递增但 date 与之相反：最新那封的 UID 最小。
    // 旧实现按 `ORDER BY folder_id, uid` 升序会先给出 UID 最小的（最旧的）那封。
    insert(
        &store,
        account,
        inbox,
        &[
            msg(3, "c@example.com", "newest", 300),
            msg(2, "b@example.com", "middle", 200),
            msg(1, "a@example.com", "oldest", 100),
        ],
    );

    let jobs = store.writable().body_jobs(account, 10).unwrap();
    assert_eq!(jobs.len(), 3);
    assert_eq!(jobs[0].uid, 3, "最新的一封应最先补全");
    assert_eq!(jobs[1].uid, 2);
    assert_eq!(jobs[2].uid, 1, "最旧的一封最后补全");
}

#[test]
fn body_jobs_put_undated_last_and_keep_same_date_stable() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let mut undated = msg(1, "a@example.com", "no date", 0);
    undated.date = None;
    insert(
        &store,
        account,
        inbox,
        &[
            msg(3, "c@example.com", "same day first inserted", 500),
            msg(4, "d@example.com", "same day second inserted", 500),
            undated,
        ],
    );

    let jobs = store.writable().body_jobs(account, 10).unwrap();
    assert_eq!(jobs.len(), 3);
    // 同一日期内按 `id DESC`（后插入的先补，符合「最近优先」），次序稳定不抖动。
    assert_eq!(jobs[0].uid, 4);
    assert_eq!(jobs[1].uid, 3);
    // 缺 Date 头的邮件排在最后（SQLite 在 DESC 下把 NULL 排末位）。
    assert_eq!(jobs[2].uid, 1);
}

#[test]
fn body_jobs_keep_folders_grouped_while_sorting_newest_first_inside_each() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let archive = upsert_folder(&store, account, "Archive", &["\\Archive"]);
    let writable = store.writable();
    // 两个文件夹的邮件日期刻意交错。
    // indexer 是游程式消费：取 `jobs[index].folder` 后只吃「连续同 folder」的一段，
    // 每段做一次 SELECT 再批量 FETCH。若排序不再按 folder_id 分组，
    // 同一文件夹会被拆成多段 → IMAP SELECT 往返成倍增加。
    writable
        .insert_messages(
            account,
            inbox,
            &[msg(1, "i1@example.com", "inbox old", 100)],
            1,
        )
        .unwrap();
    writable
        .insert_messages(
            account,
            archive.id,
            &[msg(1, "a1@example.com", "archive new", 900)],
            1,
        )
        .unwrap();
    writable
        .insert_messages(
            account,
            inbox,
            &[msg(2, "i2@example.com", "inbox new", 800)],
            2,
        )
        .unwrap();

    let jobs = writable.body_jobs(account, 10).unwrap();
    assert_eq!(jobs.len(), 3);

    // 每个 folder 只应占一段（不出现 A,B,A 这种交错）。
    let folders: Vec<FolderId> = jobs.iter().map(|job| job.folder).collect();
    let mut segments = 1;
    for pair in folders.windows(2) {
        if pair[0] != pair[1] {
            segments += 1;
        }
    }
    assert_eq!(
        segments, 2,
        "同一文件夹必须连续，否则 indexer 会反复 SELECT"
    );

    // 同一文件夹内部仍是时间倒序（inbox 段里 date=800 的先于 date=100）。
    let inbox_dates: Vec<u32> = jobs
        .iter()
        .filter(|job| job.folder == inbox)
        .map(|job| job.uid)
        .collect();
    assert_eq!(inbox_dates, vec![2, 1], "文件夹内部应按时间倒序");
}

#[test]
fn reset_body_cache_keeps_body_path_and_fts_invariant() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let writable = store.writable();
    let inserted = insert(
        &store,
        account,
        inbox,
        &[
            msg(1, "r1@example.com", "one", 10),
            msg(2, "r2@example.com", "two", 20),
        ],
    );
    let paths = store.paths();
    for summary in &inserted {
        let file = cache::write_body(&paths, summary.id, "zebra crossing").unwrap();
        writable
            .set_body_cached(
                summary.id,
                &file.to_string_lossy(),
                "zebra crossing",
                Some("zebra"),
            )
            .unwrap();
    }
    let outcome = writable
        .search(&SearchQuery {
            text: "zebra".into(),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(outcome.messages.len(), 2);

    // 附件元数据：清理缓存后 local_path 必须失效。
    writable
        .upsert_attachments(
            inserted[0].id,
            &[Attachment {
                part_index: 2,
                filename: Some("a.bin".into()),
                mime_type: Some("application/octet-stream".into()),
                size: Some(1),
                is_inline: false,
                content_id: None,
                local_path: None,
            }],
        )
        .unwrap();
    writable
        .set_attachment_path(inserted[0].id, 2, &std::path::PathBuf::from("/tmp/a.bin"))
        .unwrap();

    assert_eq!(writable.reset_body_cache(Some(account)).unwrap(), 2);

    assert!(
        writable
            .get_attachment(inserted[0].id, 2)
            .unwrap()
            .unwrap()
            .local_path
            .is_none()
    );

    for summary in &inserted {
        let current = writable.get_message(summary.id).unwrap().unwrap();
        assert!(!current.body_cached);
    }
    // 不变量：body_path 为 NULL 时 FTS body 已清空（头部仍可搜）。
    let outcome = writable
        .search(&SearchQuery {
            text: "zebra".into(),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(outcome.total, 0);
    let outcome = writable
        .search(&SearchQuery {
            text: "one".into(),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(outcome.total, 1);
    assert_eq!(
        writable.body_coverage(&[account], &[]).unwrap(),
        BodyCoverage {
            indexed: 0,
            total: 2
        }
    );
}

#[test]
fn rebuild_index_recreates_head_and_body_entries() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let writable = store.writable();
    let inserted = insert(
        &store,
        account,
        inbox,
        &[
            msg(1, "i1@example.com", "rebuild one", 10),
            msg(2, "i2@example.com", "rebuild two", 20),
        ],
    );
    let paths = store.paths();
    let file = cache::write_body(&paths, inserted[0].id, "uniqueowl in body").unwrap();
    writable
        .set_body_cached(
            inserted[0].id,
            &file.to_string_lossy(),
            "uniqueowl in body",
            None,
        )
        .unwrap();

    // 索引行应包含原始 JSON 字符串的 to_addrs。
    let rows = writable.index_rows(Some(account)).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows[0].to_addrs.starts_with('['));
    assert!(rows[0].to_addrs.contains("alice@example.com"));
    assert!(rows[0].body_path.is_some());
    assert!(rows[1].body_path.is_none());

    let indexed = writable.rebuild_index().unwrap();
    assert_eq!(indexed, 2);

    let body_hit = writable
        .search(&SearchQuery {
            text: "uniqueowl".into(),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(body_hit.messages.len(), 1);
    assert_eq!(body_hit.messages[0].id, inserted[0].id);
    let head_hit = writable
        .search(&SearchQuery {
            text: "rebuild".into(),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(head_hit.messages.len(), 2);
}

// ---------------------------------------------------------------------------
// 附件 / 草稿
// ---------------------------------------------------------------------------

#[test]
fn attachment_metadata_upsert_and_path() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let writable = store.writable();
    let id = insert(
        &store,
        account,
        inbox,
        &[msg(1, "a@example.com", "att", 10)],
    )[0]
    .id;

    let first = Attachment {
        part_index: 1,
        filename: Some("doc.pdf".into()),
        mime_type: Some("application/pdf".into()),
        size: Some(10),
        is_inline: false,
        content_id: None,
        local_path: None,
    };
    let second = Attachment {
        part_index: 2,
        filename: Some("pic.png".into()),
        mime_type: Some("image/png".into()),
        size: Some(20),
        is_inline: true,
        content_id: Some("<cid>".into()),
        local_path: None,
    };
    writable
        .upsert_attachments(id, &[first.clone(), second])
        .unwrap();

    let listed = writable.list_attachments(id).unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].part_index, 1);
    assert!(!listed[0].is_inline);

    let path = std::path::PathBuf::from("attach/1/1_doc.pdf");
    writable.set_attachment_path(id, 1, &path).unwrap();
    let fetched = writable.get_attachment(id, 1).unwrap().unwrap();
    assert_eq!(fetched.local_path.as_deref(), Some(path.as_path()));
    assert!(writable.get_attachment(id, 9).unwrap().is_none());

    // 再次 upsert 不覆盖 local_path。
    writable
        .upsert_attachments(
            id,
            &[Attachment {
                size: Some(99),
                ..first.clone()
            }],
        )
        .unwrap();
    let refetched = writable.get_attachment(id, 1).unwrap().unwrap();
    assert_eq!(refetched.local_path.as_deref(), Some(path.as_path()));
    assert_eq!(refetched.size, Some(99));

    assert!(
        writable
            .set_attachment_path(id, 9, &std::path::PathBuf::from("x"))
            .is_err()
    );
}

#[test]
fn drafts_crud_and_dirs() {
    let (_dir, store) = open_store();
    let writable = store.writable();
    let account = writable
        .create_account(&account_draft("drafts@example.com"))
        .unwrap();
    let paths = store.paths();

    let draft = writable
        .create_draft(&NewDraft {
            account: account.id,
            message_id: "draft@example.com".into(),
            subject: Some("原始主题".into()),
            to: vec![Address::bare("bob@example.com")],
            bcc: vec![],
            raw_path: paths
                .draft_raw_path(DraftId(1))
                .to_string_lossy()
                .into_owned(),
        })
        .unwrap();
    assert_eq!(draft.status, DraftStatus::Draft);
    assert_eq!(draft.to.len(), 1);

    let file = cache::write_draft_raw(&paths, draft.id, b"Subject: hi\n\nbody").unwrap();
    assert_eq!(file, paths.draft_raw_path(draft.id));
    assert_eq!(
        cache::read_draft_raw(&paths, draft.id).unwrap().as_deref(),
        Some(&b"Subject: hi\n\nbody"[..])
    );

    let dirs = writable.list_draft_dirs().unwrap();
    assert_eq!(dirs.len(), 1);
    assert_eq!(dirs[0].0, draft.id);

    let updated = writable
        .update_draft(
            draft.id,
            Some("新主题"),
            &[Address::bare("carol@example.com")],
            &[Address::bare("secret@example.com")],
            "drafts/x/message.eml",
        )
        .unwrap();
    assert_eq!(updated.subject.as_deref(), Some("新主题"));
    assert_eq!(updated.to[0].addr, "carol@example.com");

    writable.set_draft_server_uid(draft.id, Some(55)).unwrap();
    writable
        .set_draft_status(draft.id, DraftStatus::Sent, Some("归档失败"))
        .unwrap();
    let fetched = writable.get_draft(draft.id).unwrap().unwrap();
    assert_eq!(fetched.server_uid, Some(55));
    assert_eq!(fetched.status, DraftStatus::Sent);
    assert_eq!(fetched.last_error.as_deref(), Some("归档失败"));

    assert_eq!(writable.list_drafts(None).unwrap().len(), 1);
    assert_eq!(writable.list_drafts(Some(account.id)).unwrap().len(), 1);

    writable.delete_draft(draft.id).unwrap();
    assert!(writable.get_draft(draft.id).unwrap().is_none());
    assert!(writable.list_drafts(None).unwrap().is_empty());
    assert!(writable.delete_draft(draft.id).is_err());
}

// ---------------------------------------------------------------------------
// 只读句柄
// ---------------------------------------------------------------------------

#[test]
fn readonly_store_serves_lists_and_keyset_pages() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    insert(
        &store,
        account,
        inbox,
        &[
            msg(1, "ro1@example.com", "one", 10),
            msg(2, "ro2@example.com", "two", 20),
        ],
    );
    let draft = store
        .writable()
        .create_draft(&NewDraft {
            account,
            message_id: "ro-draft@example.com".into(),
            subject: Some("draft".into()),
            to: vec![],
            bcc: vec![],
            raw_path: "drafts/1/message.eml".into(),
        })
        .unwrap();

    let readonly = store.read_only();
    assert_eq!(readonly.list_accounts_readonly().unwrap().len(), 1);
    assert_eq!(readonly.list_folders_readonly(account).unwrap().len(), 1);
    let page = readonly.list_messages_readonly(inbox, None, 1).unwrap();
    assert_eq!(page.len(), 1);
    let cursor = (page[0].date.unwrap(), page[0].id.0);
    let page2 = readonly
        .list_messages_readonly(inbox, Some(cursor), 1000)
        .unwrap();
    assert_eq!(page2.len(), 1);
    assert_eq!(readonly.unread_count_readonly(inbox).unwrap(), 2);
    assert!(readonly.get_message_readonly(page[0].id).unwrap().is_some());
    assert_eq!(
        readonly.list_drafts_readonly(Some(account)).unwrap().len(),
        1
    );
    assert!(readonly.get_draft_readonly(draft.id).unwrap().is_some());
}

// ---------------------------------------------------------------------------
// 文件缓存
// ---------------------------------------------------------------------------

#[test]
fn safe_name_rules() {
    assert_eq!(cache::safe_name("report.pdf"), "report.pdf");
    assert_eq!(cache::safe_name("a/b:c*d"), "a_b_c_d");
    assert_eq!(cache::safe_name("../../etc/passwd"), "_.._etc_passwd");
    assert_eq!(cache::safe_name(".hidden"), "hidden");
    assert_eq!(cache::safe_name("...."), "attachment");
    assert_eq!(cache::safe_name("  "), "__");
    assert_eq!(cache::safe_name(""), "attachment");
    assert_eq!(cache::safe_name("你好 世界.txt"), "你好_世界.txt");
    assert_eq!(cache::safe_name(&"x".repeat(250)).chars().count(), 100);
}

#[test]
fn cache_roundtrip_gzip_and_cleanup() {
    let (_dir, store) = open_store();
    let paths: CachePaths = store.paths();
    let id = MessageId(42);

    // 正文
    let body = cache::write_body(&paths, id, "Hello 正文").unwrap();
    assert_eq!(body, paths.body_path(id));
    assert_eq!(
        cache::read_body(&paths, id).unwrap().as_deref(),
        Some("Hello 正文")
    );
    assert_eq!(cache::read_body(&paths, MessageId(43)).unwrap(), None);

    // 原始邮件：必须真正 gzip（魔数 1f 8b），且可解压还原。
    let raw = b"From: a@example.com\r\nSubject: test\r\n\r\nbody\x00\xff";
    let raw_path = cache::write_raw_gz(&paths, id, raw).unwrap();
    assert_eq!(raw_path, paths.raw_path(id));
    let compressed = std::fs::read(&raw_path).unwrap();
    assert_eq!(compressed[0], 0x1f);
    assert_eq!(compressed[1], 0x8b);
    assert_eq!(
        cache::read_raw_gz(&paths, id).unwrap().as_deref(),
        Some(&raw[..])
    );
    assert_eq!(cache::read_raw_gz(&paths, MessageId(43)).unwrap(), None);

    // 附件：文件名清洗，命中 attach/{id}/
    let attachment_path =
        cache::write_attachment(&paths, id, 3, Some("../evil name.exe"), b"payload").unwrap();
    assert_eq!(
        attachment_path.parent().unwrap(),
        paths.attachment_dir(id).as_path()
    );
    assert_eq!(
        attachment_path.file_name().unwrap().to_str().unwrap(),
        "3__evil_name.exe"
    );
    assert_eq!(
        std::fs::read(&attachment_path).unwrap(),
        b"payload".to_vec()
    );

    // 草稿原文
    let draft_id = DraftId(7);
    let draft_file = cache::write_draft_raw(&paths, draft_id, b"draft bytes").unwrap();
    assert_eq!(draft_file, paths.draft_raw_path(draft_id));
    assert_eq!(
        cache::read_draft_raw(&paths, draft_id).unwrap().as_deref(),
        Some(&b"draft bytes"[..])
    );

    // 占用统计与清理
    assert!(cache::dir_size(paths.attach_dir()) > 0);
    assert!(cache::total_size(&paths) > 0);
    let freed = cache::remove_message_files(&paths, &[id]);
    assert!(freed > 0);
    assert!(!paths.body_path(id).exists());
    assert!(!paths.raw_path(id).exists());
    assert!(!paths.attachment_dir(id).exists());

    assert!(cache::remove_draft_dir(&paths, draft_id) > 0);
    assert!(!paths.draft_dir(draft_id).exists());
    assert_eq!(cache::remove_draft_dir(&paths, draft_id), 0);
}

#[test]
fn cleanup_orphan_draft_dirs_keeps_valid_ids() {
    let (_dir, store) = open_store();
    let paths = store.paths();

    cache::write_draft_raw(&paths, DraftId(1), b"keep").unwrap();
    cache::write_draft_raw(&paths, DraftId(2), b"orphan").unwrap();
    cache::write_draft_raw(&paths, DraftId(3), b"orphan2").unwrap();
    // 非数字目录不应被删除。
    let junk = paths.drafts_dir().join("not-a-draft");
    std::fs::create_dir_all(&junk).unwrap();

    let freed = cache::cleanup_orphan_draft_dirs(&paths, &[DraftId(1)]);
    assert!(freed > 0);
    assert!(paths.draft_dir(DraftId(1)).exists());
    assert!(!paths.draft_dir(DraftId(2)).exists());
    assert!(!paths.draft_dir(DraftId(3)).exists());
    assert!(junk.exists(), "非 DraftId 目录由用户处理");
}

// ---------------------------------------------------------------------------
// meta
// ---------------------------------------------------------------------------

#[test]
fn meta_settings_roundtrip() {
    let (_dir, store) = open_store();
    let writable = store.writable();
    assert_eq!(writable.get_setting("theme").unwrap(), None);
    writable.set_setting("theme", "dark").unwrap();
    assert_eq!(
        writable.get_setting("theme").unwrap().as_deref(),
        Some("dark")
    );
    writable.set_setting("theme", "light").unwrap();
    assert_eq!(
        writable.get_setting("theme").unwrap().as_deref(),
        Some("light")
    );
    // schema_version 不因用户设置而改变。
    writable.set_setting("other", "x").unwrap();
    assert_eq!(writable.schema_version().unwrap(), me_core::SCHEMA_VERSION);
}

#[test]
fn available_disk_space_is_reported() {
    let (_dir, store) = open_store();
    let available = store.available_bytes().expect("临时目录应支持剩余空间查询");
    assert!(available > 0);
    assert!(store.disk_space_ok(), "临时目录剩余空间应高于阈值");
    assert!(store.writable().available_bytes().unwrap().is_some());
}

#[test]
fn oauth_account_columns_round_trip_and_invariants_are_enforced() {
    let (_dir, store) = open_store();
    let writable = store.writable();

    let oauth = AccountDraft {
        email: "oauth@example.com".into(),
        display_name: None,
        imap: Endpoint::new("imap.gmail.com", 993, TlsMode::Implicit),
        smtp: Endpoint::new("smtp.gmail.com", 465, TlsMode::Implicit),
        auth_user: "oauth@example.com".into(),
        auth_secret: None,
        auth_method: AuthMethod::OAuth2,
        oauth_provider: Some(OAuthProvider::Google),
        oauth_subject: Some("subject-1".into()),
        plaintext_tls_ack: false,
    };
    let created = writable.create_account(&oauth).expect("oauth account");
    assert_eq!(created.auth_method, AuthMethod::OAuth2);
    assert_eq!(created.oauth_provider, Some(OAuthProvider::Google));
    assert_eq!(created.oauth_subject.as_deref(), Some("subject-1"));
    assert!(created.idle_enabled);
    assert!(!created.needs_reauth);
    assert_eq!(created.poll_interval_secs, None);
    assert_eq!(created.token_expires_at, None);

    // OAuth2 ⇔ provider；OAuth2 不允许明文 TLS。
    let mut missing_provider = oauth.clone();
    missing_provider.email = "bad1@example.com".into();
    missing_provider.oauth_provider = None;
    assert!(writable.create_account(&missing_provider).is_err());

    let mut password_with_provider = oauth.clone();
    password_with_provider.email = "bad2@example.com".into();
    password_with_provider.auth_method = AuthMethod::Password;
    assert!(writable.create_account(&password_with_provider).is_err());

    let mut plaintext = oauth.clone();
    plaintext.email = "bad3@example.com".into();
    plaintext.imap = Endpoint::new("imap.gmail.com", 143, TlsMode::None);
    assert!(writable.create_account(&plaintext).is_err());
}

#[test]
fn broken_headers_are_repaired_and_reindexed() {
    let (_dir, store) = open_store();
    let (account, folder) = setup(&store);
    let inserted = insert(
        &store,
        account,
        folder,
        &[msg(1, "<1@example.com>", "broken \u{fffd}\u{fffd}", 100)],
    );
    let id = inserted[0].id;
    let writable = store.writable();
    assert_eq!(writable.messages_with_broken_headers(10).unwrap().len(), 1);

    let from = Address::new(Some("鲜丽华".into()), "xianlihua1021@163.com");
    let to = vec![Address::new(None, "alice@example.com")];
    writable
        .update_message_headers(
            id,
            Some("登元跃迁（成都市）数字科技有限公司-FDE岗位测试题"),
            Some(&from),
            &to,
            &[],
        )
        .unwrap();

    let repaired = writable.get_message(id).unwrap().unwrap();
    assert_eq!(
        repaired.subject.as_deref(),
        Some("登元跃迁（成都市）数字科技有限公司-FDE岗位测试题")
    );
    assert_eq!(
        repaired
            .from
            .as_ref()
            .and_then(|address| address.name.as_deref()),
        Some("鲜丽华")
    );
    assert_eq!(repaired.to.len(), 1);
    assert!(
        writable
            .messages_with_broken_headers(10)
            .unwrap()
            .is_empty()
    );

    // FTS 头部列同步更新，修复后的主题可被检索。
    let outcome = writable
        .search(&SearchQuery {
            text: "测试题".into(),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(outcome.messages.len(), 1);
}

#[test]
fn broken_attachment_names_are_detected_and_repaired() {
    let (_dir, store) = open_store();
    let (account, folder) = setup(&store);
    let inserted = insert(
        &store,
        account,
        folder,
        &[msg(1, "<1@example.com>", "clean subject", 100)],
    );
    let id = inserted[0].id;
    let writable = store.writable();
    let attachment = |filename: &str| Attachment {
        part_index: 2,
        filename: Some(filename.to_string()),
        mime_type: Some("application/pdf".into()),
        size: Some(1024),
        is_inline: false,
        content_id: None,
        local_path: None,
    };

    // 摘要本身干净，仅附件名含 U+FFFD，也必须进入修复候选。
    writable
        .upsert_attachments(id, &[attachment("\u{fffd}\u{0d7a}.pdf")])
        .unwrap();
    assert_eq!(writable.messages_with_broken_headers(10).unwrap().len(), 1);

    // 修复只覆盖元数据，已下载的 local_path 保留。
    let local = std::path::PathBuf::from("C:/cache/attach/2.pdf");
    writable.set_attachment_path(id, 2, &local).unwrap();
    writable
        .upsert_attachments(id, &[attachment("青岛科技创新园-流程规范白皮书.pdf")])
        .unwrap();

    let repaired = writable.get_attachment(id, 2).unwrap().unwrap();
    assert_eq!(
        repaired.filename.as_deref(),
        Some("青岛科技创新园-流程规范白皮书.pdf")
    );
    assert_eq!(repaired.local_path.as_deref(), Some(local.as_path()));
    assert!(
        writable
            .messages_with_broken_headers(10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn attachment_paths_are_rewritten_for_relocation() {
    let (_dir, store) = open_store();
    let (account, folder) = setup(&store);
    let inserted = insert(
        &store,
        account,
        folder,
        &[msg(1, "<1@example.com>", "attachment relocation", 100)],
    );
    let id = inserted[0].id;
    let writable = store.writable();
    let attachment = |part_index: u32, name: &str| Attachment {
        part_index,
        filename: Some(name.to_string()),
        mime_type: Some("application/octet-stream".into()),
        size: Some(4),
        is_inline: false,
        content_id: None,
        local_path: None,
    };
    writable
        .upsert_attachments(
            id,
            &[
                attachment(2, "a.pdf"),
                attachment(3, "b.pdf"),
                attachment(4, "outside.pdf"),
            ],
        )
        .unwrap();

    let old_dir = tempfile::tempdir().unwrap();
    let new_dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let old_a = old_dir.path().join(id.0.to_string()).join("2_a.pdf");
    let old_b = old_dir.path().join(id.0.to_string()).join("3_b.pdf");
    let outside_path = outside.path().join("4_outside.pdf");
    writable.set_attachment_path(id, 2, &old_a).unwrap();
    writable.set_attachment_path(id, 3, &old_b).unwrap();
    writable.set_attachment_path(id, 4, &outside_path).unwrap();

    let rewritten = writable
        .rewrite_attachment_paths(old_dir.path(), new_dir.path())
        .unwrap();
    assert_eq!(rewritten, 2);

    let a = writable.get_attachment(id, 2).unwrap().unwrap();
    let b = writable.get_attachment(id, 3).unwrap().unwrap();
    let untouched = writable.get_attachment(id, 4).unwrap().unwrap();
    assert_eq!(
        a.local_path.as_deref(),
        Some(
            new_dir
                .path()
                .join(id.0.to_string())
                .join("2_a.pdf")
                .as_path()
        )
    );
    assert_eq!(
        b.local_path.as_deref(),
        Some(
            new_dir
                .path()
                .join(id.0.to_string())
                .join("3_b.pdf")
                .as_path()
        )
    );
    assert_eq!(
        untouched.local_path.as_deref(),
        Some(outside_path.as_path())
    );
}

/// 头部字段过滤与未读过滤：与全文短语**取交集**，不是互相覆盖。
#[test]
fn search_filters_combine_header_fields_and_unread() {
    let (_dir, store) = open_store();
    let (account, inbox) = setup(&store);
    let writable = store.writable();

    let mut read = msg(1, "a@example.com", "Quarterly report", 10);
    read.from = Some(Address::new(Some("张三".into()), "zhang@example.com"));
    read.flags = Flags::SEEN;
    let mut unread = msg(2, "b@example.com", "Quarterly report", 11);
    unread.from = Some(Address::new(Some("张三".into()), "zhang@example.com"));
    unread.has_attachment = true;
    let mut other = msg(3, "c@example.com", "Weekly report", 12);
    other.from = Some(Address::new(Some("李四".into()), "li@example.com"));
    insert(&store, account, inbox, &[read, unread, other]);

    let by_from = writable
        .search(&SearchQuery {
            text: "Quarterly".into(),
            from: Some("zhang@example.com".into()),
            limit: 20,
            ..Default::default()
        })
        .expect("search");
    assert_eq!(by_from.total, 2, "关键词 + 发件人取交集");

    let unread_only = writable
        .search(&SearchQuery {
            text: "Quarterly".into(),
            from: Some("zhang@example.com".into()),
            unread_only: true,
            limit: 20,
            ..Default::default()
        })
        .expect("search");
    assert_eq!(unread_only.total, 1, "再加未读只剩一封");
    let only = &unread_only.messages[0];
    assert_eq!(only.uid, Some(2));
    assert!(only.has_attachment, "结果里要带附件标记");

    let by_subject = writable
        .search(&SearchQuery {
            subject: Some("Weekly".into()),
            limit: 20,
            ..Default::default()
        })
        .expect("search");
    assert_eq!(by_subject.total, 1, "没有全文词时也能按主题子串查");

    let by_to = writable
        .search(&SearchQuery {
            to: Some("alice@example.com".into()),
            limit: 20,
            ..Default::default()
        })
        .expect("search");
    assert_eq!(by_to.total, 3, "收件人过滤命中全部三封");

    let no_hit = writable
        .search(&SearchQuery {
            text: "Quarterly".into(),
            from: Some("li@example.com".into()),
            limit: 20,
            ..Default::default()
        })
        .expect("search");
    assert!(no_hit.messages.is_empty(), "两个条件不匹配时不应有结果");
}
