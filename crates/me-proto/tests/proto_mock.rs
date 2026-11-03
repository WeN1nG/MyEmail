//! me-proto 协议时序测试：对本机 mock IMAP 服务器跑真实命令。
//!
//! 覆盖：CAPABILITY、SELECT、LIST(SPECIAL-USE)、分页标志回拉、
//! `BODY.PEEK[]`（不得使用 `BODY[]`）、STORE、MOVE 的 COPYUID 解析。

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use me_core::model::{Account, AccountId, AuthCredential, AuthMethod, Endpoint, Flags, TlsMode};
use me_core::port::{FlagStoreMode, IdleOutcome, ImapSession, UidRange};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

struct Mock {
    addr: SocketAddr,
    commands: Arc<Mutex<Vec<String>>>,
}

async fn spawn_mock() -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().unwrap();
    let commands = Arc::new(Mutex::new(Vec::new()));
    let recorded = commands.clone();

    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept");
        let (read_half, mut write_half) = socket.into_split();
        write_half.write_all(b"* OK mock ready\r\n").await.unwrap();
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
            recorded.lock().unwrap().push(command.clone());
            let (tag, rest) = command.split_once(' ').unwrap_or((&command, ""));
            let upper = rest.to_ascii_uppercase();

            if upper.starts_with("CAPABILITY") {
                reply(
                    &mut write_half,
                    &format!(
                        "* CAPABILITY IMAP4rev1 UIDPLUS MOVE SPECIAL-USE IDLE AUTH=XOAUTH2\r\n{tag} OK CAPABILITY\r\n"
                    ),
                )
                .await;
            } else if upper.starts_with("LOGIN") {
                reply(&mut write_half, &format!("{tag} OK LOGIN completed\r\n")).await;
            } else if upper.starts_with("LIST") {
                reply(
                    &mut write_half,
                    &format!(
                        "* LIST (\\HasNoChildren \\Inbox) \"/\" \"INBOX\"\r\n\
                         * LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"\r\n\
                         * LIST (\\HasNoChildren) \"/\" \"&ZeVnLIqe-\"\r\n\
                         {tag} OK LIST completed\r\n"
                    ),
                )
                .await;
            } else if upper.starts_with("SELECT") {
                reply(
                    &mut write_half,
                    &format!(
                        "* 3 EXISTS\r\n\
                         * OK [UIDVALIDITY 42] ok\r\n\
                         * OK [UIDNEXT 4] ok\r\n\
                         {tag} OK [READ-WRITE] SELECT completed\r\n"
                    ),
                )
                .await;
            } else if upper.contains("UID FETCH") && upper.contains("BODY.PEEK[]") {
                let body = b"Subject: mock\r\n\r\nhello\r\n";
                let mut out =
                    format!("* 1 FETCH (UID 5 BODY[] {{{}}}\r\n", body.len()).into_bytes();
                out.extend_from_slice(body);
                out.extend_from_slice(format!(")\r\n{tag} OK FETCH completed\r\n").as_bytes());
                write_half.write_all(&out).await.unwrap();
            } else if upper.contains("UID FETCH") && upper.contains("(UID FLAGS)") {
                reply(
                    &mut write_half,
                    &format!(
                        "* 1 FETCH (UID 1 FLAGS (\\Seen))\r\n\
                         * 2 FETCH (UID 2 FLAGS ())\r\n\
                         {tag} OK FETCH completed\r\n"
                    ),
                )
                .await;
            } else if upper.contains("UID STORE") {
                reply(&mut write_half, &format!("{tag} OK STORE completed\r\n")).await;
            } else if upper.contains("UID MOVE") {
                reply(
                    &mut write_half,
                    &format!("{tag} OK [COPYUID 42 2 9] MOVE completed\r\n"),
                )
                .await;
            } else if upper.contains("UID SEARCH") {
                reply(
                    &mut write_half,
                    &format!("* SEARCH 5 6\r\n{tag} OK SEARCH completed\r\n"),
                )
                .await;
            } else if upper.starts_with("IDLE") {
                reply(&mut write_half, "+ idling\r\n").await;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                reply(&mut write_half, "* 2 EXISTS\r\n").await;
                // 等待客户端发送 DONE，再结束本轮 IDLE。
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                    recorded.lock().unwrap().push(line.trim_end().to_string());
                }
                reply(&mut write_half, &format!("{tag} OK IDLE completed\r\n")).await;
            } else if upper.starts_with("LOGOUT") {
                reply(
                    &mut write_half,
                    &format!("* BYE logging out\r\n{tag} OK LOGOUT\r\n"),
                )
                .await;
                break;
            } else {
                reply(&mut write_half, &format!("{tag} OK\r\n")).await;
            }
        }
    });

    Mock { addr, commands }
}

async fn reply(writer: &mut tokio::net::tcp::OwnedWriteHalf, text: &str) {
    writer.write_all(text.as_bytes()).await.unwrap();
}

fn account(port: u16) -> Account {
    Account {
        id: AccountId(1),
        email: "alice@example.com".into(),
        display_name: Some("Alice".into()),
        imap: Endpoint::new("127.0.0.1", port, TlsMode::None),
        smtp: Endpoint::new("127.0.0.1", 2525, TlsMode::None),
        auth_user: "alice@example.com".into(),
        auth_method: AuthMethod::AppPassword,
        oauth_provider: None,
        oauth_subject: None,
        needs_reauth: false,
        idle_enabled: true,
        poll_interval_secs: None,
        token_expires_at: None,
    }
}

#[tokio::test]
async fn imap_session_flow() {
    let mock = spawn_mock().await;
    let mut session = me_proto::connect_imap(
        &account(mock.addr.port()),
        &AuthCredential::password("secret"),
    )
    .await
    .expect("connect");

    let capabilities = session.capabilities();
    assert!(capabilities.uidplus && capabilities.move_supported && capabilities.special_use);
    assert!(capabilities.idle && capabilities.xoauth2);

    let selected = session.select("INBOX").await.expect("select");
    assert_eq!(selected.uid_validity, 42);
    assert_eq!(selected.uid_next, 4);
    assert_eq!(selected.exists, 3);

    let folders = session.list_folders().await.expect("list");
    assert_eq!(folders.len(), 3);
    assert_eq!(folders[0].name, "INBOX");
    assert!(folders[0].attributes.iter().any(|a| a == "\\Inbox"));
    // modified UTF-7 解码。
    assert_eq!(folders[2].name, "日本語");

    let flags = session
        .fetch_flags(UidRange::bounded(1, 2))
        .await
        .expect("flags");
    assert_eq!(flags.len(), 2);
    assert!(flags[0].1.is_seen());
    assert_eq!(flags[1].1, Flags::NONE);

    session
        .store_flags(&[1], Flags::FLAGGED, FlagStoreMode::Add)
        .await
        .expect("store");

    let body = session.fetch_body(5).await.expect("body");
    assert!(body.starts_with(b"Subject: mock"));

    let mapping = session.move_to(&[2], "Trash").await.expect("move");
    assert_eq!(mapping, Some(vec![(2, 9)]));

    let found = session
        .search_message_id("abc@example.com")
        .await
        .expect("search");
    assert_eq!(found, vec![5, 6]);

    session.logout().await.expect("logout");

    let commands = mock.commands.lock().unwrap().clone();
    let joined = commands.join("\n").to_ascii_uppercase();
    // 禁止 BODY[]：只允许 BODY.PEEK[]。
    assert!(!joined.contains("BODY[]"), "must never issue BODY[]");
    assert!(joined.contains("BODY.PEEK[]"));
    assert!(joined.contains("+FLAGS.SILENT"));
    assert!(joined.contains("UID MOVE"));
    assert!(joined.contains("RETURN (SPECIAL-USE)"));
    // 标志回拉按 UID 区间分页，不是 1:*。
    assert!(joined.contains("UID FETCH 1:2 (UID FLAGS)"));
}

#[tokio::test]
async fn idle_reports_change_and_returns_usable_session() {
    let mock = spawn_mock().await;
    let mut session = me_proto::connect_imap(
        &account(mock.addr.port()),
        &AuthCredential::password("secret"),
    )
    .await
    .expect("connect");
    session.select("INBOX").await.expect("select");

    let outcome = session
        .wait_for_changes(std::time::Duration::from_secs(2))
        .await
        .expect("idle");
    assert!(matches!(
        outcome,
        IdleOutcome::Changed {
            exists: Some(2),
            recent: None
        }
    ));

    // DONE 之后连接仍可复用。
    assert!(session.capabilities().idle);
    session.logout().await.expect("logout");

    let commands = mock.commands.lock().unwrap().clone();
    assert!(
        commands
            .iter()
            .any(|command| command.eq_ignore_ascii_case("DONE")),
        "client must send DONE after IDLE change"
    );
}

#[tokio::test]
async fn login_failure_is_auth_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = socket.into_split();
        write_half.write_all(b"* OK mock\r\n").await.unwrap();
        let mut reader = BufReader::new(read_half);
        let mut line = String::with_capacity(128);
        let _ = reader.read_line(&mut line).await;
        let tag = line.split_once(' ').map(|(tag, _)| tag).unwrap_or("A0001");
        write_half
            .write_all(
                format!("{tag} NO [AUTHENTICATIONFAILED] invalid credentials\r\n").as_bytes(),
            )
            .await
            .unwrap();
        // 保持连接片刻，避免客户端看到 EOF 而非 NO。
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _: io::Result<()> = Ok(());
    });

    let result =
        me_proto::connect_imap(&account(addr.port()), &AuthCredential::password("wrong")).await;
    let err = match result {
        Ok(_) => panic!("must fail"),
        Err(err) => err,
    };
    assert!(
        matches!(err, me_core::CoreError::Auth { .. }),
        "got {err:?}"
    );
}
