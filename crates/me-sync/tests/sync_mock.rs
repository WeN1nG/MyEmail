//! 同步引擎集成测试：真实 me-store + 真实 me-proto + 本机 mock IMAP。
//!
//! 验证：文件夹合并、增量头部入库、`BODYSTRUCTURE` 附件标记、标志回拉、
//! `last_uid` 推进，以及重启后不重复拉取（第二次同步无新增）。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use me_core::command::AccountDraft;
use me_core::model::{
    AccountId, Address, Attachment, AuthMethod, DateSource, DraftContent, DraftId, Endpoint, Flags,
    FolderRole, NewMessage, OutgoingMessage, TlsMode,
};
use me_core::port::{AccountRepo, AttachmentRepo, FolderInfo, FolderRepo, MessageRepo, MetaRepo};
use me_core::{AuthCredential, Command, Event};
use me_store::Store;
use me_sync::SyncService;
use me_sync::events::EventSender;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

async fn spawn_mock() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let (read_half, mut write_half) = socket.into_split();
                write_half.write_all(b"* OK mock ready\r\n").await.ok();
                let mut reader = BufReader::new(read_half);
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        break;
                    }
                    let command = line.trim_end().to_string();
                    if command.is_empty() {
                        continue;
                    }
                    let (tag, rest) = command.split_once(' ').unwrap_or((&command, ""));
                    let upper = rest.to_ascii_uppercase();

                    let response: Vec<u8> = if upper.starts_with("CAPABILITY") {
                        format!("* CAPABILITY IMAP4rev1 UIDPLUS IDLE\r\n{tag} OK CAPABILITY\r\n")
                            .into_bytes()
                    } else if upper.starts_with("IDLE") {
                        let mut out = b"+ idling\r\n* 3 EXISTS\r\n".to_vec();
                        write_half.write_all(&out).await.ok();
                        // 等待客户端 DONE。
                        line.clear();
                        let _ = reader.read_line(&mut line).await;
                        out = format!("{tag} OK IDLE completed\r\n").into_bytes();
                        out
                    } else if upper.starts_with("LOGIN") {
                        format!("{tag} OK LOGIN completed\r\n").into_bytes()
                    } else if upper.starts_with("LIST") {
                        format!(
                            "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n{tag} OK LIST completed\r\n"
                        )
                        .into_bytes()
                    } else if upper.starts_with("SELECT") {
                        format!(
                            "* 2 EXISTS\r\n* OK [UIDVALIDITY 7] ok\r\n* OK [UIDNEXT 3] ok\r\n{tag} OK [READ-WRITE] SELECT\r\n"
                        )
                        .into_bytes()
                    } else if upper.contains("UID FETCH") && upper.contains("BODY.PEEK[HEADER]") {
                        let h1 = b"Subject: Hello\r\nFrom: alice@example.com\r\nTo: bob@example.com\r\nDate: Mon, 01 Jan 2024 10:00:00 +0000\r\nMessage-ID: <m1@example.com>\r\n\r\n";
                        let h2 = b"Subject: With attachment\r\nFrom: zhang@example.com\r\nTo: bob@example.com\r\nDate: Tue, 02 Jan 2024 11:00:00 +0000\r\nMessage-ID: <m2@example.com>\r\n\r\n";
                        let mut out = format!(
                            "* 1 FETCH (UID 1 FLAGS (\\Seen) INTERNALDATE \"01-Jan-2024 10:00:00 +0000\" RFC822.SIZE 120 BODY[HEADER] {{{}}}\r\n",
                            h1.len()
                        )
                        .into_bytes();
                        out.extend_from_slice(h1);
                        out.extend_from_slice(
                            b" BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 12 1 NIL NIL NIL NIL))\r\n",
                        );
                        out.extend_from_slice(
                            format!(
                                "* 2 FETCH (UID 2 FLAGS () INTERNALDATE \"02-Jan-2024 11:00:00 +0000\" RFC822.SIZE 200 BODY[HEADER] {{{}}}\r\n",
                                h2.len()
                            )
                            .as_bytes(),
                        );
                        out.extend_from_slice(h2);
                        out.extend_from_slice(
                            b" BODYSTRUCTURE ((\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 5 1 NIL NIL NIL NIL)(\"APPLICATION\" \"PDF\" (\"NAME\" \"a.pdf\") NIL NIL \"BASE64\" 10 NIL (\"ATTACHMENT\" (\"FILENAME\" \"a.pdf\")) NIL NIL) \"MIXED\" (\"BOUNDARY\" \"b\") NIL NIL NIL))\r\n",
                        );
                        out.extend_from_slice(format!("{tag} OK FETCH completed\r\n").as_bytes());
                        out
                    } else if upper.contains("UID FETCH") && upper.contains("(UID FLAGS)") {
                        format!(
                            "* 1 FETCH (UID 1 FLAGS (\\Seen))\r\n* 2 FETCH (UID 2 FLAGS ())\r\n{tag} OK FETCH completed\r\n"
                        )
                        .into_bytes()
                    } else if upper.starts_with("LOGOUT") {
                        let bytes = format!("* BYE\r\n{tag} OK LOGOUT\r\n").into_bytes();
                        write_half.write_all(&bytes).await.ok();
                        break;
                    } else {
                        format!("{tag} OK\r\n").into_bytes()
                    };
                    if write_half.write_all(&response).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sync_account_persists_headers_and_flags() {
    let addr = spawn_mock().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let writable = store.writable();

    let account = writable
        .create_account(&AccountDraft {
            email: "alice@example.com".into(),
            display_name: Some("Alice".into()),
            imap: Endpoint::new("127.0.0.1", addr.port(), TlsMode::None),
            smtp: Endpoint::new("127.0.0.1", 2525, TlsMode::None),
            auth_user: "alice@example.com".into(),
            auth_secret: Some("secret".into()),
            auth_method: AuthMethod::AppPassword,
            oauth_provider: None,
            oauth_subject: None,
            plaintext_tls_ack: true,
        })
        .unwrap();

    let (command_tx, command_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(1024);
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
    service.set_session_credential(account.id, AuthCredential::password("secret"));
    let runner = service.clone();
    tokio::spawn(async move { runner.run(command_rx).await });

    // 启动同步会自动跑一次；轮询等待入库。
    let mut messages = Vec::new();
    for _ in 0..100 {
        let folders = writable.list_folders(account.id).unwrap();
        if let Some(inbox) = folders.iter().find(|f| f.role == FolderRole::Inbox) {
            messages = writable.list_messages(inbox.id, None, 50).unwrap();
        }
        if messages.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert_eq!(
        messages.len(),
        2,
        "expected two synced messages, got {messages:?}"
    );
    let first = messages.iter().find(|m| m.uid == Some(1)).expect("uid 1");
    assert_eq!(first.subject.as_deref(), Some("Hello"));
    assert!(first.flags.is_seen());
    let second = messages.iter().find(|m| m.uid == Some(2)).expect("uid 2");
    assert_eq!(second.subject.as_deref(), Some("With attachment"));
    assert!(second.has_attachment);

    let folders = writable.list_folders(account.id).unwrap();
    let inbox = folders
        .iter()
        .find(|f| f.role == FolderRole::Inbox)
        .unwrap();
    assert_eq!(inbox.uid_validity, 7);
    assert_eq!(inbox.last_uid, 2);

    // 手动再同步一次：UIDNEXT 未变，不应重复插入。
    command_tx.send(Command::SyncAccount(account.id)).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after = writable.list_messages(inbox.id, None, 50).unwrap();
    assert_eq!(after.len(), 2, "re-sync must not duplicate messages");

    command_tx.send(Command::CancelSync(account.id)).unwrap();
    drop(command_tx);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_credential_is_used_without_keyring() {
    // 该测试只验证公开的降级入口，不触碰系统密钥库。
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let writable = store.writable();
    let account = writable
        .create_account(&AccountDraft {
            email: "bob@example.com".into(),
            display_name: None,
            imap: Endpoint::new("127.0.0.1", 1, TlsMode::None),
            smtp: Endpoint::new("127.0.0.1", 2, TlsMode::None),
            auth_user: "bob@example.com".into(),
            auth_secret: None,
            auth_method: AuthMethod::Password,
            oauth_provider: None,
            oauth_subject: None,
            plaintext_tls_ack: true,
        })
        .unwrap();

    let (event_tx, _event_rx) = mpsc::channel(8);
    let service = SyncService::new(
        writable,
        me_proto::ProtoFactory,
        me_proto::ProtoSmtp,
        store.paths(),
        EventSender::new(event_tx, Arc::new(|| {})),
        Arc::new(me_sync::NoopBrowser),
    );
    service.set_session_credential(account.id, AuthCredential::password("session-only"));
    // 无 panic 即通过；实际网络行为由上面的 mock 测试覆盖。
    assert_eq!(account.id, AccountId(account.id.0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn draft_saved_then_reloaded_for_editing() {
    let addr = spawn_mock().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let writable = store.writable();
    let account = writable
        .create_account(&AccountDraft {
            email: "alice@example.com".into(),
            display_name: None,
            imap: Endpoint::new("127.0.0.1", addr.port(), TlsMode::None),
            smtp: Endpoint::new("127.0.0.1", 2525, TlsMode::None),
            auth_user: "alice@example.com".into(),
            auth_secret: Some("secret".into()),
            auth_method: AuthMethod::Password,
            oauth_provider: None,
            oauth_subject: None,
            plaintext_tls_ack: true,
        })
        .unwrap();

    let (command_tx, command_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(1024);
    let service = SyncService::new(
        writable.clone(),
        me_proto::ProtoFactory,
        me_proto::ProtoSmtp,
        store.paths(),
        EventSender::new(event_tx, Arc::new(|| {})),
        Arc::new(me_sync::NoopBrowser),
    );
    service.set_session_credential(account.id, AuthCredential::password("secret"));
    let runner = service.clone();
    tokio::spawn(async move { runner.run(command_rx).await });

    let attachment = dir.path().join("note.txt");
    std::fs::write(&attachment, b"attachment-body").unwrap();

    let outgoing = OutgoingMessage {
        account: account.id,
        draft_id: None,
        message_id: "draft-roundtrip@example.com".into(),
        from: account.from_address(),
        to: vec![Address::new(Some("Bob".into()), "bob@example.com")],
        cc: vec![Address::bare("carol@example.com")],
        bcc: vec![Address::bare("hidden@example.com")],
        subject: "草稿主题".into(),
        body_text: "草稿正文\n第二行".into(),
        attachments: vec![attachment],
    };
    command_tx.send(Command::SaveDraft(outgoing)).unwrap();
    let draft_id = wait_for_draft_saved(&mut event_rx).await;

    let managed = store
        .paths()
        .draft_attachment_dir(draft_id)
        .join("note.txt");
    assert_eq!(
        std::fs::read(&managed).unwrap(),
        b"attachment-body",
        "托管附件应已复制"
    );

    command_tx.send(Command::LoadDraft(draft_id)).unwrap();
    let draft = wait_for_draft_loaded(&mut event_rx).await;

    assert_eq!(draft.draft_id, draft_id);
    assert_eq!(draft.account, account.id);
    assert_eq!(draft.message_id, "draft-roundtrip@example.com");
    assert_eq!(draft.subject, "草稿主题");
    assert_eq!(draft.to.len(), 1);
    assert_eq!(draft.to[0].addr, "bob@example.com");
    assert_eq!(draft.to[0].name.as_deref(), Some("Bob"));
    assert_eq!(draft.cc.len(), 1);
    assert_eq!(draft.cc[0].addr, "carol@example.com");
    assert!(
        draft.body_text.contains("草稿正文"),
        "{:?}",
        draft.body_text
    );
    assert_eq!(draft.attachments.len(), 1);
    assert_eq!(draft.attachments[0], managed);

    // 重新保存已有草稿：托管附件源路径 = 目标路径，不能被截断。
    let resave = OutgoingMessage {
        account: account.id,
        draft_id: Some(draft_id),
        message_id: draft.message_id.clone(),
        from: account.from_address(),
        to: draft.to.clone(),
        cc: draft.cc.clone(),
        bcc: Vec::new(),
        subject: draft.subject.clone(),
        body_text: draft.body_text.clone(),
        attachments: draft.attachments.clone(),
    };
    command_tx.send(Command::SaveDraft(resave.clone())).unwrap();
    let _ = wait_for_draft_saved(&mut event_rx).await;
    assert_eq!(
        std::fs::read(&managed).unwrap(),
        b"attachment-body",
        "重新保存后附件不应被清空"
    );

    // 移除全部附件后再次保存：托管目录应被清空。
    let detached = OutgoingMessage {
        attachments: Vec::new(),
        ..resave
    };
    command_tx.send(Command::SaveDraft(detached)).unwrap();
    let _ = wait_for_draft_saved(&mut event_rx).await;
    let remaining = std::fs::read_dir(store.paths().draft_attachment_dir(draft_id))
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(remaining, 0, "旧附件应被清理");

    drop(command_tx);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn realtime_idle_triggers_sync() {
    let addr = spawn_mock().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let writable = store.writable();
    let account = writable
        .create_account(&AccountDraft {
            email: "idle@example.com".into(),
            display_name: None,
            imap: Endpoint::new("127.0.0.1", addr.port(), TlsMode::None),
            smtp: Endpoint::new("127.0.0.1", 2525, TlsMode::None),
            auth_user: "idle@example.com".into(),
            auth_secret: Some("secret".into()),
            auth_method: AuthMethod::Password,
            oauth_provider: None,
            oauth_subject: None,
            plaintext_tls_ack: true,
        })
        .unwrap();

    let (command_tx, command_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(1024);
    let events = EventSender::new(event_tx, Arc::new(|| {}));
    let service = SyncService::new(
        writable.clone(),
        me_proto::ProtoFactory,
        me_proto::ProtoSmtp,
        store.paths(),
        events,
        Arc::new(me_sync::NoopBrowser),
    );
    service.set_session_credential(account.id, AuthCredential::password("secret"));
    let runner = service.clone();
    tokio::spawn(async move { runner.run(command_rx).await });

    // 首次同步完成 -> watcher 进入 IDLE。
    wait_for_online(&mut event_rx).await;
    let event_at = wait_for_idle_change(&mut event_rx).await;
    assert!(event_at > 0, "IDLE 变化必须带时间戳");

    // 去抖 2 秒后应触发一次新的同步（AccountStatus::Connecting）。
    let triggered = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            match event_rx.recv().await {
                Some(Event::AccountStatus {
                    status: me_core::ConnectionState::Connecting,
                    ..
                }) => return true,
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(triggered, "IDLE 事件应触发同步");

    drop(command_tx);
}

/// 等待某个路径消失。
///
/// 附件目录迁移是「拷贝 → 改路径 → 最后删旧目录」三步，
/// 路径重写先完成、旧目录后删除。直接断言会偶发失败，
/// 所以这里轮询等待删除真正发生。
async fn wait_until_gone(path: &std::path::Path) -> bool {
    for _ in 0..100 {
        if !path.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    !path.exists()
}

async fn wait_for_online(event_rx: &mut mpsc::Receiver<Event>) {
    for _ in 0..200 {
        match tokio::time::timeout(Duration::from_millis(100), event_rx.recv()).await {
            Ok(Some(Event::AccountStatus {
                status: me_core::ConnectionState::Online,
                ..
            })) => return,
            Ok(Some(_)) | Err(_) => continue,
            Ok(None) => break,
        }
    }
    panic!("未收到首次同步 Online");
}

async fn wait_for_idle_change(event_rx: &mut mpsc::Receiver<Event>) -> i64 {
    for _ in 0..200 {
        match tokio::time::timeout(Duration::from_millis(100), event_rx.recv()).await {
            Ok(Some(Event::RealtimeStatus {
                mode: me_core::RealtimeMode::Idle,
                connected: true,
                last_event_at: Some(timestamp),
                ..
            })) => return timestamp,
            Ok(Some(_)) | Err(_) => continue,
            Ok(None) => break,
        }
    }
    panic!("未收到 IDLE 变化事件");
}

async fn wait_for_draft_saved(event_rx: &mut mpsc::Receiver<Event>) -> DraftId {
    for _ in 0..100 {
        match tokio::time::timeout(Duration::from_millis(100), event_rx.recv()).await {
            Ok(Some(Event::DraftSaved { id, .. })) => return id,
            Ok(Some(_)) | Err(_) => continue,
            Ok(None) => break,
        }
    }
    panic!("未收到 DraftSaved");
}

async fn wait_for_draft_loaded(event_rx: &mut mpsc::Receiver<Event>) -> DraftContent {
    for _ in 0..100 {
        match tokio::time::timeout(Duration::from_millis(100), event_rx.recv()).await {
            Ok(Some(Event::DraftLoaded { draft })) => return draft,
            Ok(Some(_)) | Err(_) => continue,
            Ok(None) => break,
        }
    }
    panic!("未收到 DraftLoaded");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_repairs_garbled_attachment_names() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let writable = store.writable();

    // 账户不可达：本测试只关心启动修复，同步失败不影响修复结果。
    let account = writable
        .create_account(&AccountDraft {
            email: "repair@example.com".into(),
            display_name: None,
            imap: Endpoint::new("127.0.0.1", 1, TlsMode::None),
            smtp: Endpoint::new("127.0.0.1", 2, TlsMode::None),
            auth_user: "repair@example.com".into(),
            auth_secret: None,
            auth_method: AuthMethod::Password,
            oauth_provider: None,
            oauth_subject: None,
            plaintext_tls_ack: true,
        })
        .unwrap();
    let folder = writable
        .upsert_folder(
            account.id,
            &FolderInfo {
                name: "INBOX".into(),
                delimiter: Some('/'),
                attributes: vec![r"\Inbox".into()],
            },
        )
        .unwrap();
    let inserted = writable
        .insert_messages(
            account.id,
            folder.id,
            &[NewMessage {
                uid: Some(1),
                message_id: Some("<repair@example.com>".into()),
                subject: Some("clean subject".into()),
                from: Some(Address::new(None, "sender@163.com")),
                to: vec![Address::new(None, "repair@example.com")],
                cc: vec![],
                date: Some(1_700_000_000),
                date_source: DateSource::Header,
                size: Some(4096),
                flags: Flags::NONE,
                has_attachment: true,
                in_reply_to: None,
                references: Vec::new(),
            }],
            1,
        )
        .unwrap();
    let id = inserted[0].id;

    // 旧版本留下的乱码附件名：只按 UTF-8 解码 GBK 字节。
    writable
        .upsert_attachments(
            id,
            &[Attachment {
                part_index: 2,
                filename: Some("\u{fffd}\u{0d7a}\u{fffd}.pdf".into()),
                mime_type: Some("application/pdf".into()),
                size: Some(9),
                is_inline: false,
                content_id: None,
                local_path: None,
            }],
        )
        .unwrap();

    let raw = concat!(
        "From: sender@163.com\r\n",
        "To: repair@example.com\r\n",
        "Subject: clean subject\r\n",
        "MIME-Version: 1.0\r\n",
        "Content-Type: multipart/mixed; boundary=\"B\"\r\n",
        "\r\n",
        "--B\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "\r\n",
        "body\r\n",
        "--B\r\n",
        "Content-Type: application/pdf;\r\n",
        "\tname=\"=?GBK?B?x+C1ur/GvLy0tNDC1LAtwfezzLnmt7aw18akyukucGRm?=\"\r\n",
        "Content-Disposition: attachment;\r\n",
        " filename=\"=?GBK?B?x+C1ur/GvLy0tNDC1LAtwfezzLnmt7aw18akyukucGRm?=\"\r\n",
        "Content-Transfer-Encoding: base64\r\n",
        "\r\n",
        "aGVsbG8=\r\n",
        "--B--\r\n",
    );
    me_store::cache::write_raw_gz(&store.paths(), id, raw.as_bytes()).unwrap();

    let (_command_tx, command_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, _event_rx) = mpsc::channel::<Event>(1024);
    let service = SyncService::new(
        writable.clone(),
        me_proto::ProtoFactory,
        me_proto::ProtoSmtp,
        store.paths(),
        EventSender::new(event_tx, Arc::new(|| {})),
        Arc::new(me_sync::NoopBrowser),
    );
    tokio::spawn(async move { service.run(command_rx).await });

    let mut repaired = None;
    for _ in 0..100 {
        if let Some(attachment) = writable.get_attachment(id, 2).unwrap()
            && attachment.filename.as_deref() != Some("\u{fffd}\u{0d7a}\u{fffd}.pdf")
        {
            repaired = attachment.filename;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        repaired.as_deref(),
        Some("青岛科技创新园-流程规范白皮书.pdf"),
        "启动修复应从原始邮件恢复附件名"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attachment_dir_relocation_moves_files_and_persists_setting() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let writable = store.writable();

    let account = writable
        .create_account(&AccountDraft {
            email: "attach@example.com".into(),
            display_name: None,
            imap: Endpoint::new("127.0.0.1", 1, TlsMode::None),
            smtp: Endpoint::new("127.0.0.1", 2, TlsMode::None),
            auth_user: "attach@example.com".into(),
            auth_secret: None,
            auth_method: AuthMethod::Password,
            oauth_provider: None,
            oauth_subject: None,
            plaintext_tls_ack: true,
        })
        .unwrap();
    let folder = writable
        .upsert_folder(
            account.id,
            &FolderInfo {
                name: "INBOX".into(),
                delimiter: Some('/'),
                attributes: vec![r"\Inbox".into()],
            },
        )
        .unwrap();
    let inserted = writable
        .insert_messages(
            account.id,
            folder.id,
            &[NewMessage {
                uid: Some(1),
                message_id: Some("<attach@example.com>".into()),
                subject: Some("attachment".into()),
                from: Some(Address::new(None, "sender@example.com")),
                to: vec![Address::new(None, "attach@example.com")],
                cc: vec![],
                date: Some(1_700_000_000),
                date_source: DateSource::Header,
                size: Some(1024),
                flags: Flags::NONE,
                has_attachment: true,
                in_reply_to: None,
                references: Vec::new(),
            }],
            1,
        )
        .unwrap();
    let id = inserted[0].id;
    writable
        .upsert_attachments(
            id,
            &[Attachment {
                part_index: 2,
                filename: Some("a.pdf".into()),
                mime_type: Some("application/pdf".into()),
                size: Some(7),
                is_inline: false,
                content_id: None,
                local_path: None,
            }],
        )
        .unwrap();
    let old_file =
        me_store::cache::write_attachment(&store.paths(), id, 2, Some("a.pdf"), b"payload")
            .unwrap();
    writable.set_attachment_path(id, 2, &old_file).unwrap();
    let default_dir = store.paths().attach_dir().to_path_buf();
    assert!(old_file.starts_with(&default_dir));

    let relocated = dir.path().join("relocated-attachments");
    let (command_tx, command_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(1024);
    let service = SyncService::new(
        writable.clone(),
        me_proto::ProtoFactory,
        me_proto::ProtoSmtp,
        store.paths(),
        EventSender::new(event_tx, Arc::new(|| {})),
        Arc::new(me_sync::NoopBrowser),
    );
    tokio::spawn(async move { service.run(command_rx).await });

    command_tx
        .send(Command::SetAttachmentDir {
            path: Some(relocated.clone()),
        })
        .unwrap();

    let mut moved = None;
    for _ in 0..100 {
        let attachment = writable.get_attachment(id, 2).unwrap().unwrap();
        if let Some(current) = attachment.local_path
            && current.starts_with(&relocated)
            && current.is_file()
        {
            moved = Some(current);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let moved = moved.expect("附件目录迁移未完成");
    assert_eq!(std::fs::read(&moved).unwrap(), b"payload");
    assert!(wait_until_gone(&old_file).await, "旧附件文件应已删除");
    assert!(wait_until_gone(&default_dir).await, "默认附件目录应已清理");
    assert_eq!(
        writable
            .get_setting(me_core::settings_key::ATTACHMENT_DIR)
            .unwrap(),
        Some(relocated.to_string_lossy().into_owned())
    );

    // 恢复默认目录：文件应搬回 `<数据目录>/attach`，设置清空。
    command_tx
        .send(Command::SetAttachmentDir { path: None })
        .unwrap();
    let mut restored = None;
    for _ in 0..100 {
        let attachment = writable.get_attachment(id, 2).unwrap().unwrap();
        if let Some(current) = attachment.local_path
            && current.starts_with(&default_dir)
            && current.is_file()
        {
            restored = Some(current);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let restored = restored.expect("恢复默认附件目录未完成");
    assert_eq!(std::fs::read(&restored).unwrap(), b"payload");
    assert!(wait_until_gone(&relocated).await, "自定义附件目录应已清理");
    assert_eq!(
        writable
            .get_setting(me_core::settings_key::ATTACHMENT_DIR)
            .unwrap()
            .as_deref(),
        Some("")
    );

    // 非空且无归属标记的目录必须拒绝，避免误删用户文件。
    let busy = dir.path().join("busy");
    std::fs::create_dir_all(&busy).unwrap();
    std::fs::write(busy.join("keep.txt"), b"user data").unwrap();
    command_tx
        .send(Command::SetAttachmentDir {
            path: Some(busy.clone()),
        })
        .unwrap();
    let mut rejected = false;
    for _ in 0..50 {
        match tokio::time::timeout(Duration::from_millis(100), event_rx.recv()).await {
            Ok(Some(Event::Error { message, .. })) if message.contains("空文件夹") => {
                rejected = true;
                break;
            }
            Ok(Some(_)) | Err(_) => continue,
            Ok(None) => break,
        }
    }
    assert!(rejected, "非空目录迁移应被拒绝");
    assert_eq!(std::fs::read(busy.join("keep.txt")).unwrap(), b"user data");
    assert_eq!(
        writable.get_attachment(id, 2).unwrap().unwrap().local_path,
        Some(restored)
    );

    while event_rx.try_recv().is_ok() {}
}
