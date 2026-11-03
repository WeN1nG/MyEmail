//! 真实 IMAP/SMTP 端到端测试（Docker + GreenMail）。
//!
//! 默认忽略。运行方式：
//! ```text
//! scripts/e2e.sh
//! ```
//! 或手动启动 `tests/e2e/docker-compose.yml` 后：
//! ```text
//! cargo test -p me-sync --test e2e_real_imap -- --ignored --nocapture
//! ```
//!
//! 覆盖：SMTP 投递 → 启动同步拉取头部 → 后台补全正文 → 本地缓存可读。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use me_core::model::{Address, AuthMethod, Endpoint, FolderRole, OutgoingMessage, TlsMode};
use me_core::port::{AccountRepo, FolderRepo, MessageRepo, SmtpSender};
use me_core::{AccountDraft, AuthCredential, Command};
use me_store::Store;
use me_sync::SyncService;
use me_sync::events::EventSender;
use tokio::sync::mpsc;

fn addr_from_env(key: &str, default: &str) -> SocketAddr {
    std::env::var(key)
        .unwrap_or_else(|_| default.to_string())
        .parse()
        .unwrap_or_else(|err| panic!("{key} 不是有效地址: {err}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "需要 Docker：scripts/e2e.sh 会启动 GreenMail"]
async fn real_imap_smtp_roundtrip() {
    let imap = addr_from_env("MYEMAIL_E2E_IMAP_ADDR", "127.0.0.1:3143");
    let smtp = addr_from_env("MYEMAIL_E2E_SMTP_ADDR", "127.0.0.1:3025");
    let user = std::env::var("MYEMAIL_E2E_USER").unwrap_or_else(|_| "alice@example.com".into());
    let password = std::env::var("MYEMAIL_E2E_PASSWORD").unwrap_or_else(|_| "secret".into());

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let writable = store.writable();

    let account = writable
        .create_account(&AccountDraft {
            email: user.clone(),
            display_name: Some("E2E".into()),
            imap: Endpoint::new("127.0.0.1", imap.port(), TlsMode::None),
            smtp: Endpoint::new("127.0.0.1", smtp.port(), TlsMode::None),
            auth_user: user.clone(),
            auth_secret: Some(password.clone()),
            auth_method: AuthMethod::Password,
            oauth_provider: None,
            oauth_subject: None,
            plaintext_tls_ack: true,
        })
        .unwrap();

    // 1. 通过真实 SMTP 给自己发一封邮件，作为同步源。
    let subject = format!("E2E {}", std::process::id());
    let outgoing = OutgoingMessage {
        account: account.id,
        draft_id: None,
        message_id: me_core::generate_message_id(&user),
        from: Address::bare(&user),
        to: vec![Address::bare(&user)],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: subject.clone(),
        body_text: "E2E 正文 hello".into(),
        attachments: Vec::new(),
    };

    let credential = AuthCredential::password(password.clone());
    let mut last_error = None;
    let mut sent = false;
    for _ in 0..30 {
        match me_proto::ProtoSmtp
            .send(&account, &credential, &outgoing)
            .await
        {
            Ok(()) => {
                sent = true;
                break;
            }
            Err(err) => {
                last_error = Some(err.user_message());
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    assert!(sent, "SMTP 投递失败：{last_error:?}");

    // 2. 启动同步服务，等待头部与正文补全。
    let (command_tx, command_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, mut event_rx) = mpsc::channel(1024);
    tokio::spawn(async move { while event_rx.recv().await.is_some() {} });
    let events = EventSender::new(event_tx, Arc::new(|| {}));
    let service = SyncService::new(
        writable.clone(),
        me_proto::ProtoFactory,
        me_proto::ProtoSmtp,
        store.paths(),
        events,
        Arc::new(me_sync::NoopBrowser),
    );
    service.set_session_credential(account.id, AuthCredential::password(password.clone()));
    let runner = service.clone();
    tokio::spawn(async move { runner.run(command_rx).await });

    let mut synced = None;
    for _ in 0..300 {
        let folders = writable.list_folders(account.id).unwrap_or_default();
        if let Some(inbox) = folders
            .iter()
            .find(|folder| folder.role == FolderRole::Inbox)
        {
            let messages = writable
                .list_messages(inbox.id, None, 50)
                .unwrap_or_default();
            if let Some(message) = messages
                .iter()
                .find(|message| message.subject.as_deref() == Some(subject.as_str()))
            {
                if message.body_cached {
                    synced = Some(message.clone());
                    break;
                }
                synced = Some(message.clone());
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let message = synced.expect("同步超时：未在 INBOX 找到测试邮件");
    assert!(message.body_cached, "正文补全超时：{message:?}");

    let body =
        std::fs::read_to_string(store.paths().body_path(message.id)).expect("读取正文缓存失败");
    assert!(body.contains("E2E 正文"), "正文内容不符：{body:?}");

    command_tx.send(Command::CancelSync(account.id)).unwrap();
    drop(command_tx);
}
