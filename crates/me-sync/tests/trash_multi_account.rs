//! 批量删除的**跨账户**行为（统一收件箱场景）。
//!
//! 回归点：旧实现拿「最后一封邮件的账户」当作整批邮件的账户，
//! 只查那一个账户的文件夹树，再把它自己的废件箱 id 写到**所有**邮件上。
//! 结果是从统一收件箱里批量删除时，别的账户的邮件被移动到一个
//! 不属于它的文件夹 id（跨账户的外键），本地视图直接错乱。
//!
//! 这里不联网：`move_to_trash` 只写库 + 落 `pending_ops`，
//! 真正的服务器动作由后台 pending ops 执行（本测试把 IMAP host 指到
//! 一个不存在的域名，连接必然失败，正好不影响断言）。
//! 也不建凭证（`auth_secret: None`）——不碰系统密钥库。

use std::sync::Arc;
use std::time::{Duration, Instant};

use me_core::command::AccountDraft;
use me_core::model::{
    AccountId, Address, AuthMethod, DateSource, Endpoint, Flags, FolderId, MessageId, NewMessage,
    TlsMode,
};
use me_core::port::{AccountRepo, FolderInfo, FolderRepo, MessageRepo, ReadOnlyRepo};
use me_core::{Command, Event};
use me_store::Store;
use me_sync::SyncService;
use me_sync::events::EventSender;
use tokio::sync::mpsc;

struct Seeded {
    account: AccountId,
    inbox: FolderId,
    trash: FolderId,
    message: MessageId,
}

/// 建账户 + INBOX / Trash 两个文件夹 + 收件箱里一封邮件。
fn seed_account(store: &Store, email: &str) -> Seeded {
    let writable = store.writable();
    let account = writable
        .create_account(&AccountDraft {
            email: email.into(),
            display_name: None,
            imap: Endpoint::new("imap.invalid", 993, TlsMode::Implicit),
            smtp: Endpoint::new("smtp.invalid", 465, TlsMode::Implicit),
            auth_user: email.into(),
            auth_secret: None,
            auth_method: AuthMethod::Password,
            oauth_provider: None,
            oauth_subject: None,
            plaintext_tls_ack: false,
        })
        .expect("create account");

    // 角色由 `SPECIAL-USE` 属性派生——不要手写 role。
    let inbox = writable
        .upsert_folder(
            account.id,
            &FolderInfo {
                name: "INBOX".into(),
                delimiter: Some('/'),
                attributes: vec!["\\Inbox".into()],
            },
        )
        .expect("inbox");
    let trash = writable
        .upsert_folder(
            account.id,
            &FolderInfo {
                name: "Trash".into(),
                delimiter: Some('/'),
                attributes: vec!["\\Trash".into()],
            },
        )
        .expect("trash");

    let inserted = writable
        .insert_messages(
            account.id,
            inbox.id,
            &[NewMessage {
                uid: Some(1),
                message_id: Some(format!("<{email}>")),
                subject: Some(format!("来自 {email}")),
                from: Some(Address {
                    name: None,
                    addr: email.into(),
                }),
                to: Vec::new(),
                cc: Vec::new(),
                date: Some(100),
                date_source: DateSource::Header,
                size: None,
                flags: Flags::NONE,
                has_attachment: false,
            }],
            1,
        )
        .expect("insert message");

    Seeded {
        account: account.id,
        inbox: inbox.id,
        trash: trash.id,
        message: inserted[0].id,
    }
}

fn start_service(store: &Store) -> mpsc::UnboundedSender<Command> {
    let (command_tx, command_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, _event_rx) = mpsc::channel::<Event>(1024);
    let events = EventSender::new(event_tx, Arc::new(|| {}));
    let service = SyncService::new(
        store.writable(),
        me_proto::ProtoFactory,
        me_proto::ProtoSmtp,
        store.paths(),
        events,
        Arc::new(me_sync::NoopBrowser),
    );
    tokio::spawn(async move { service.run(command_rx).await });
    command_tx
}

fn folder_of(store: &Store, id: MessageId) -> Option<FolderId> {
    store
        .read_only()
        .get_message_readonly(id)
        .expect("get message")
        .map(|m| m.folder)
}

#[tokio::test]
async fn batch_delete_moves_each_message_into_its_own_account_trash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");

    let a = seed_account(&store, "a@example.com");
    let b = seed_account(&store, "b@example.com");

    let tx = start_service(&store);
    // 一次删除横跨两个账户——这正是统一收件箱里的批量删除。
    tx.send(Command::MoveToTrash(vec![a.message, b.message]))
        .expect("send");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if folder_of(&store, a.message) == Some(a.trash)
            && folder_of(&store, b.message) == Some(b.trash)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "等待跨账户删除超时：a={:?}（期望 {:?}）b={:?}（期望 {:?}）",
            folder_of(&store, a.message),
            a.trash,
            folder_of(&store, b.message),
            b.trash
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 两个收件箱都空了。
    assert!(
        store
            .read_only()
            .list_messages_readonly(a.inbox, None, 50)
            .expect("list a")
            .is_empty(),
        "账户一的收件箱应已空"
    );
    assert!(
        store
            .read_only()
            .list_messages_readonly(b.inbox, None, 50)
            .expect("list b")
            .is_empty(),
        "账户二的收件箱应已空"
    );

    // 账户归属没被改写，每封都在**自己账户**的废件箱里。
    let moved_a = store
        .read_only()
        .get_message_readonly(a.message)
        .expect("a")
        .expect("a exists");
    let moved_b = store
        .read_only()
        .get_message_readonly(b.message)
        .expect("b")
        .expect("b exists");
    assert_eq!(moved_a.account, a.account);
    assert_eq!(moved_b.account, b.account);
    assert_eq!(moved_a.folder, a.trash, "账户一的邮件进了账户一的废件箱");
    assert_eq!(moved_b.folder, b.trash, "账户二的邮件进了账户二的废件箱");
}
