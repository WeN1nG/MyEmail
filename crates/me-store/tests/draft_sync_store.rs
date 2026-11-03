//! v0.2 草稿同步仓储测试：Bcc、同步状态、候选查询、服务器身份、墓碑与冲突副本。

use me_core::command::AccountDraft;
use me_core::model::{
    AccountId, Address, AuthMethod, Draft, DraftContent, DraftSyncState, Endpoint, TlsMode,
};
use me_core::port::{AccountRepo, DraftRepo, NewDraft};
use me_store::Store;

fn open_store() -> (tempfile::TempDir, Store, AccountId) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open store");
    let account = store
        .writable()
        .create_account(&AccountDraft {
            email: "alice@example.com".into(),
            display_name: None,
            imap: Endpoint::new("imap.example.com", 993, TlsMode::Implicit),
            smtp: Endpoint::new("smtp.example.com", 465, TlsMode::Implicit),
            auth_user: "alice@example.com".into(),
            auth_secret: Some("secret".into()),
            auth_method: AuthMethod::Password,
            oauth_provider: None,
            oauth_subject: None,
            plaintext_tls_ack: false,
        })
        .expect("account")
        .id;
    (dir, store, account)
}

fn create_draft(
    store: &Store,
    account: AccountId,
    message_id: &str,
    to: &[Address],
    bcc: &[Address],
) -> Draft {
    store
        .writable()
        .create_draft(&NewDraft {
            account,
            message_id: message_id.into(),
            subject: Some("主题".into()),
            to: to.to_vec(),
            bcc: bcc.to_vec(),
            raw_path: "drafts/1/message.eml".into(),
        })
        .expect("create draft")
}

#[test]
fn bcc_round_trips_but_is_not_part_of_rfc822() {
    let (_dir, store, account) = open_store();
    let writable = store.writable();
    let bcc = vec![Address::bare("secret@example.com")];
    let draft = create_draft(
        &store,
        account,
        "d1@example.com",
        &[Address::bare("bob@example.com")],
        &bcc,
    );
    assert_eq!(draft.bcc, bcc);
    assert_eq!(draft.sync_state, DraftSyncState::LocalOnly);
    assert_eq!(draft.server_uid_validity, None);

    let updated = writable
        .update_draft(
            draft.id,
            Some("新主题"),
            &[Address::bare("bob@example.com")],
            &[],
            "drafts/1/message.eml",
        )
        .unwrap();
    assert!(updated.bcc.is_empty());

    let fetched = writable.get_draft(draft.id).unwrap().unwrap();
    assert_eq!(fetched.subject.as_deref(), Some("新主题"));
    assert!(fetched.bcc.is_empty());
}

#[test]
fn sync_candidates_exclude_synced_and_include_tombstones() {
    let (_dir, store, account) = open_store();
    let writable = store.writable();
    let local = create_draft(&store, account, "local@example.com", &[], &[]);
    let dirty = create_draft(&store, account, "dirty@example.com", &[], &[]);
    let tombstone = create_draft(&store, account, "gone@example.com", &[], &[]);

    writable
        .set_draft_sync_state(dirty.id, DraftSyncState::LocalDirty, None)
        .unwrap();
    writable
        .mark_draft_synced(local.id, Some(10), Some(1), "lh", "sh")
        .unwrap();
    writable.tombstone_draft(tombstone.id).unwrap();

    let candidates = writable.list_draft_sync_candidates(account).unwrap();
    assert_eq!(candidates.len(), 2);
    assert!(candidates.iter().any(|c| c.id == dirty.id && !c.deleted));
    assert!(candidates.iter().any(|c| c.id == tombstone.id && c.deleted));

    // 普通草稿列表隐藏墓碑；同步成功清空失败计数。
    assert_eq!(writable.list_drafts(Some(account)).unwrap().len(), 2);
    let synced = writable.get_draft(local.id).unwrap().unwrap();
    assert_eq!(synced.sync_state, DraftSyncState::Synced);
    assert_eq!(synced.server_uid, Some(10));

    writable.finalize_deleted_draft(tombstone.id).unwrap();
    assert!(writable.get_draft(tombstone.id).unwrap().is_none());
}

#[test]
fn failure_attempts_increment_and_reset() {
    let (_dir, store, account) = open_store();
    let writable = store.writable();
    let draft = create_draft(&store, account, "retry@example.com", &[], &[]);

    for expected in 1..=3 {
        assert_eq!(
            writable
                .bump_draft_sync_attempt(draft.id, "network down")
                .unwrap(),
            expected
        );
    }
    let fetched = writable.get_draft(draft.id).unwrap().unwrap();
    assert_eq!(fetched.last_error.as_deref(), Some("network down"));

    writable.clear_draft_sync_attempts(draft.id).unwrap();
    let item = writable
        .list_draft_sync_candidates(account)
        .unwrap()
        .into_iter()
        .find(|item| item.id == draft.id)
        .unwrap();
    assert_eq!(item.sync_attempts, 0);
}

#[test]
fn server_identity_is_unique_and_lookupable() {
    let (_dir, store, account) = open_store();
    let writable = store.writable();
    let first = create_draft(&store, account, "one@example.com", &[], &[]);
    let second = create_draft(&store, account, "two@example.com", &[], &[]);

    writable
        .mark_draft_synced(first.id, Some(42), Some(7), "lh", "sh")
        .unwrap();
    let found = writable
        .find_draft_by_server_uid(account, 7, 42)
        .unwrap()
        .expect("server identity");
    assert_eq!(found.id, first.id);

    // 同一服务器身份不能绑定到第二行。
    assert!(
        writable
            .mark_draft_synced(second.id, Some(42), Some(7), "lh", "sh")
            .is_err()
    );

    // 按 Message-ID 兜底最多返回 2 条。
    assert_eq!(
        writable
            .find_drafts_by_message_id(account, "one@example.com")
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn conflict_copy_is_hidden_from_main_list() {
    let (_dir, store, account) = open_store();
    let writable = store.writable();
    let source = create_draft(
        &store,
        account,
        "conflict@example.com",
        &[Address::bare("bob@example.com")],
        &[Address::bare("bcc@example.com")],
    );
    let content = DraftContent {
        account,
        draft_id: source.id,
        message_id: "conflict@example.com".into(),
        subject: "服务器版本".into(),
        to: vec![Address::bare("bob@example.com")],
        cc: vec![],
        bcc: vec![],
        body_text: "server body".into(),
        attachments: vec![],
    };

    let copy_id = writable.create_conflict_copy(source.id, &content).unwrap();
    let copy = writable.get_draft(copy_id).unwrap().unwrap();
    assert_eq!(copy.conflict_of, Some(source.id));
    assert_eq!(copy.sync_state, DraftSyncState::Conflict);
    assert_eq!(copy.server_uid, None);
    assert!(copy.subject.as_deref().unwrap().starts_with("[冲突副本]"));

    // 主列表只显示原草稿；两份可通过 Message-ID 查到。
    assert_eq!(writable.list_drafts(Some(account)).unwrap().len(), 1);
    assert_eq!(
        writable
            .find_drafts_by_message_id(account, "conflict@example.com")
            .unwrap()
            .len(),
        2
    );
}
