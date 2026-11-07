//! 线程头离线回填的端到端测试（v0.4 的 C5）。
//!
//! 覆盖：正文补全过的历史邮件 → 本地 `raw/*.eml.gz` → 解压解析 → 写回两列
//! → 发 `ThreadsBackfilled`。**全程不联网**（IMAP host 指向不存在的域名，
//! 回填本身也不碰网络）。
//!
//! 还要验证「跑完之后不再重复入队」：没有回复头的邮件写 `"[]"` 之后，
//! 第二次触发应当一条事件都不发。

use std::sync::Arc;
use std::time::Duration;

use me_core::command::AccountDraft;
use me_core::model::{AuthMethod, DateSource, Endpoint, Flags, NewMessage, TlsMode};
use me_core::port::{AccountRepo, FolderInfo, FolderRepo, MessageRepo, ReadOnlyRepo};
use me_core::{Command, Event};
use me_store::{Store, cache};
use me_sync::SyncService;
use me_sync::events::EventSender;
use tokio::sync::mpsc;

fn start_service(store: &Store) -> (mpsc::UnboundedSender<Command>, mpsc::Receiver<Event>) {
    let (command_tx, command_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, event_rx) = mpsc::channel::<Event>(1024);
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
    (command_tx, event_rx)
}

/// 建账户 + INBOX，插一封**没有回复头**的邮件，并把正文缓存「补全」掉。
fn seed(store: &Store) -> (me_core::MessageId, me_core::AccountId) {
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

    let inserted = writable
        .insert_messages(
            account.id,
            inbox.id,
            &[NewMessage {
                uid: Some(1),
                message_id: Some("<child@example.com>".into()),
                subject: Some("Re: 报价".into()),
                from: None,
                to: Vec::new(),
                cc: Vec::new(),
                date: Some(200),
                date_source: DateSource::Header,
                size: Some(120),
                flags: Flags::NONE,
                has_attachment: false,
                // 入库时确实还没有回复头 → 两列都是 NULL（= 待回填）。
                in_reply_to: None,
                references: Vec::new(),
            }],
            1,
        )
        .expect("insert");
    let message = inserted[0].id;

    // 模拟「正文已补全」：body_path 非空才会进入回填队列。
    writable
        .set_body_cached(message, "bodies/1.txt", "正文", None)
        .expect("body cached");
    (message, account.id)
}

/// 一封带 `In-Reply-To` / `References` 的原始报文。
fn raw_with_thread_headers() -> Vec<u8> {
    b"Subject: Re: \xe6\x8a\xa5\xe4\xbb\xb7\r\n\
      From: bob@example.com\r\n\
      To: alice@example.com\r\n\
      Date: Mon, 01 Jan 2024 10:00:00 +0000\r\n\
      Message-ID: <child@example.com>\r\n\
      In-Reply-To: <parent@example.com>\r\n\
      References: <root@example.com> <parent@example.com>\r\n\
      \r\n\
      body\r\n"
        .to_vec()
}

#[tokio::test]
async fn backfill_writes_thread_headers_from_local_raw() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");
    let (message, _account) = seed(&store);
    // 原始报文已经落在 raw/ 里（真实流程由正文补全写入）。
    cache::write_raw_gz(&store.paths(), message, &raw_with_thread_headers()).expect("raw");

    let (tx, mut rx) = start_service(&store);
    tx.send(Command::BackfillThreads).expect("send");

    let mut written = None;
    for _ in 0..100 {
        match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
            Ok(Some(Event::ThreadsBackfilled { updated })) => {
                written = Some(updated);
                break;
            }
            Ok(Some(_)) => continue,
            Ok(None) => panic!("事件通道提前关闭"),
            Err(_) => continue,
        }
    }
    assert_eq!(written, Some(1), "应当写回一封并发事件");

    let summary = store
        .read_only()
        .get_message_readonly(message)
        .expect("read")
        .expect("exists");
    assert_eq!(
        summary.in_reply_to.as_deref(),
        Some("parent@example.com"),
        "In-Reply-To 应写回裸 id"
    );
    assert_eq!(
        summary.references,
        vec![
            "root@example.com".to_string(),
            "parent@example.com".to_string()
        ],
        "References 应保留整条链"
    );
}

#[tokio::test]
async fn backfilled_message_does_not_enter_the_queue_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");
    let (message, _account) = seed(&store);
    // 完全没有回复头的邮件：回填写 `"[]"`，不能一直留在队列里。
    cache::write_raw_gz(
        &store.paths(),
        message,
        b"Subject: no threads\r\nMessage-ID: <solo@example.com>\r\n\r\nbody\r\n",
    )
    .expect("raw");

    let (tx, mut rx) = start_service(&store);
    // 启动时服务自己会跑一次回填，这里不重复触发，只等队列清空——
    // 断言的是**队列语义**，不是「哪一次调用写的」，避免和启动任务抢时序。
    let mut drained = false;
    for _ in 0..150 {
        let backlog = store
            .writable()
            .thread_backlog(0, 10)
            .expect("thread_backlog");
        if backlog.is_empty() {
            drained = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        drained,
        "回填后队列应被清空（已回填的写 \"[]\" 而不是 NULL）"
    );

    // **先把通道里积压的事件读掉**：启动那次回填的 `ThreadsBackfilled`
    // 还躺在队列里，不清空会把「上一轮的事件」误判成本轮的结果。
    while tokio::time::timeout(Duration::from_millis(50), rx.recv())
        .await
        .is_ok()
    {}

    // 再手动触发一次：队列已空，不该再发事件。
    tx.send(Command::BackfillThreads).expect("send");
    let again = tokio::time::timeout(Duration::from_millis(400), async {
        loop {
            match rx.recv().await {
                Some(Event::ThreadsBackfilled { updated }) => return updated,
                Some(_) => continue,
                None => return u32::MAX,
            }
        }
    })
    .await;
    assert!(
        again.is_err(),
        "已回填的邮件不该再次入队（拿到 updated = {again:?}）"
    );

    let summary = store
        .read_only()
        .get_message_readonly(message)
        .expect("read")
        .expect("exists");
    assert!(summary.references.is_empty());
    assert!(summary.in_reply_to.is_none());
}
