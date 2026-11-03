//! OAuth 回环授权流程集成测试：真实 HTTP 回调 + mock 令牌/用户信息端点。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use me_core::command::AccountDraft;
use me_core::model::{AuthMethod, Endpoint, OAuthProvider, TlsMode};
use me_core::port::AccountRepo;
use me_core::{Command, Event, OAuthFlowMode};
use me_proto::oauth::ProviderConfig;
use me_store::Store;
use me_sync::SyncService;
use me_sync::events::EventSender;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

async fn spawn_oauth_server() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let captured = captured.clone();
            tokio::spawn(async move {
                let (read_half, mut writer) = socket.into_split();
                let mut reader = BufReader::new(read_half);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                    return;
                }
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        break;
                    }
                    headers.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if name.eq_ignore_ascii_case("content-length") {
                            value.trim().parse::<usize>().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                let mut body = vec![0u8; content_length];
                let _ = reader.read_exact(&mut body).await;
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                captured
                    .lock()
                    .unwrap()
                    .push(format!("{path} {}", String::from_utf8_lossy(&body)));

                let response_body = if path.starts_with("/token") {
                    r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#
                } else if path.starts_with("/userinfo") {
                    r#"{"sub":"subject-1","email":"alice@example.com"}"#
                } else {
                    r#"{"error":"not_found"}"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                let _ = writer.write_all(response.as_bytes()).await;
                let _ = writer.shutdown().await;
            });
        }
    });
    (addr, requests)
}

fn provider_config(addr: SocketAddr) -> ProviderConfig {
    let mut config = ProviderConfig::for_provider(OAuthProvider::Google, "client-id");
    config.token_url = format!("http://{addr}/token");
    config.userinfo_url = format!("http://{addr}/userinfo");
    config
}

async fn wait_for_event(
    event_rx: &mut mpsc::Receiver<Event>,
    timeout: Duration,
    mut match_event: impl FnMut(&Event) -> bool,
) -> Event {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("timeout waiting for event");
        }
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Ok(Some(event)) if match_event(&event) => return event,
            Ok(Some(_)) | Err(_) => continue,
            Ok(None) => panic!("event channel closed"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loopback_oauth_creates_tokens_and_updates_account() {
    let (oauth_addr, requests) = spawn_oauth_server().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let account = store
        .writable()
        .create_account(&AccountDraft {
            email: "alice@example.com".into(),
            display_name: None,
            imap: Endpoint::new("127.0.0.1", 9, TlsMode::Implicit),
            smtp: Endpoint::new("127.0.0.1", 9, TlsMode::Implicit),
            auth_user: "alice@example.com".into(),
            auth_secret: None,
            auth_method: AuthMethod::OAuth2,
            oauth_provider: Some(OAuthProvider::Google),
            oauth_subject: None,
            plaintext_tls_ack: false,
        })
        .unwrap();

    let (command_tx, command_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(1024);
    let events = EventSender::new(event_tx, Arc::new(|| {}));
    let service = SyncService::new(
        store.writable(),
        me_proto::ProtoFactory,
        me_proto::ProtoSmtp,
        store.paths(),
        events,
        Arc::new(me_sync::NoopBrowser),
    );
    service.configure_oauth_provider(provider_config(oauth_addr));
    let runner = service.clone();
    tokio::spawn(async move { runner.run(command_rx).await });

    let account_id = account.id;
    command_tx
        .send(Command::StartOAuth {
            provider: OAuthProvider::Google,
            mode: OAuthFlowMode::Loopback,
            account: Some(account_id),
            email_hint: Some("alice@example.com".into()),
        })
        .unwrap();

    let started = wait_for_event(&mut event_rx, Duration::from_secs(5), |event| {
        matches!(event, Event::OAuthFlowStarted { .. })
    })
    .await;
    let Event::OAuthFlowStarted {
        flow_id,
        verification_uri,
        ..
    } = started
    else {
        unreachable!()
    };

    let authorize = url::Url::parse(&verification_uri).expect("authorize url");
    let params: std::collections::HashMap<_, _> = authorize.query_pairs().into_owned().collect();
    let redirect_uri = params.get("redirect_uri").expect("redirect_uri").clone();
    let state = params.get("state").expect("state").clone();
    assert!(params.contains_key("code_challenge"));

    // 模拟浏览器回调。
    let callback = format!("{redirect_uri}?code=code-1&state={state}");
    let callback_url = url::Url::parse(&callback).unwrap();
    let host = callback_url.host_str().unwrap().to_string();
    let port = callback_url.port().unwrap();
    let mut socket = TcpStream::connect((host.as_str(), port)).await.unwrap();
    let path = format!(
        "{}?{}",
        callback_url.path(),
        callback_url.query().unwrap_or("")
    );
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    socket.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    let _ = socket.read_to_string(&mut response).await;
    assert!(response.starts_with("HTTP/1.1 200"), "response: {response}");

    let completed = wait_for_event(&mut event_rx, Duration::from_secs(8), |event| {
        matches!(
            event,
            Event::OAuthCompleted { .. } | Event::OAuthFailed { .. }
        )
    })
    .await;
    match completed {
        Event::OAuthCompleted {
            flow_id: completed_id,
            account,
            created,
        } => {
            assert_eq!(completed_id, flow_id);
            assert!(!created);
            assert_eq!(account.id, account_id);
            assert_eq!(account.oauth_subject.as_deref(), Some("subject-1"));
        }
        Event::OAuthFailed { message, .. } => panic!("oauth failed: {message}"),
        _ => unreachable!(),
    }

    let updated = store.writable().get_account(account_id).unwrap().unwrap();
    assert!(!updated.needs_reauth);
    assert!(updated.token_expires_at.is_some());
    assert_eq!(updated.oauth_subject.as_deref(), Some("subject-1"));

    let requests = requests.lock().unwrap().clone().join("\n");
    assert!(requests.contains("/token"));
    assert!(requests.contains("code=code-1"));
    assert!(requests.contains("code_verifier="));
    assert!(requests.contains("/userinfo"));
}

async fn spawn_device_server() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let token_calls = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let captured = captured.clone();
            let token_calls = token_calls.clone();
            tokio::spawn(async move {
                let (read_half, mut writer) = socket.into_split();
                let mut reader = BufReader::new(read_half);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                    return;
                }
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        break;
                    }
                    headers.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if name.eq_ignore_ascii_case("content-length") {
                            value.trim().parse::<usize>().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                let mut body = vec![0u8; content_length];
                let _ = reader.read_exact(&mut body).await;
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                captured
                    .lock()
                    .unwrap()
                    .push(format!("{path} {}", String::from_utf8_lossy(&body)));

                let (status, response_body) = if path.starts_with("/device") {
                    (
                        200,
                        r#"{"device_code":"device-1","user_code":"ABCD-1234","verification_uri":"https://example.com/device","expires_in":900,"interval":1}"#.to_string(),
                    )
                } else if path.starts_with("/token") {
                    let call = token_calls.fetch_add(1, Ordering::SeqCst);
                    if call == 0 {
                        (400, r#"{"error":"authorization_pending"}"#.to_string())
                    } else {
                        (
                            200,
                            r#"{"access_token":"access-device","refresh_token":"refresh-device","expires_in":3600,"token_type":"Bearer"}"#.to_string(),
                        )
                    }
                } else if path.starts_with("/userinfo") {
                    (
                        200,
                        r#"{"sub":"subject-2","email":"alice@example.com"}"#.to_string(),
                    )
                } else {
                    (404, r#"{"error":"not_found"}"#.to_string())
                };
                let reason = if status == 200 { "OK" } else { "Error" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                let _ = writer.write_all(response.as_bytes()).await;
                let _ = writer.shutdown().await;
            });
        }
    });
    (addr, requests)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn device_code_oauth_completes_after_pending() {
    let (oauth_addr, requests) = spawn_device_server().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let account = store
        .writable()
        .create_account(&AccountDraft {
            email: "alice@example.com".into(),
            display_name: None,
            imap: Endpoint::new("127.0.0.1", 9, TlsMode::Implicit),
            smtp: Endpoint::new("127.0.0.1", 9, TlsMode::Implicit),
            auth_user: "alice@example.com".into(),
            auth_secret: None,
            auth_method: AuthMethod::OAuth2,
            oauth_provider: Some(OAuthProvider::Microsoft),
            oauth_subject: None,
            plaintext_tls_ack: false,
        })
        .unwrap();
    let account_id = account.id;

    let (command_tx, command_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(1024);
    let events = EventSender::new(event_tx, Arc::new(|| {}));
    let service = SyncService::new(
        store.writable(),
        me_proto::ProtoFactory,
        me_proto::ProtoSmtp,
        store.paths(),
        events,
        Arc::new(me_sync::NoopBrowser),
    );
    let mut config = ProviderConfig::for_provider(OAuthProvider::Microsoft, "client-id");
    config.token_url = format!("http://{oauth_addr}/token");
    config.device_code_url = Some(format!("http://{oauth_addr}/device"));
    config.userinfo_url = format!("http://{oauth_addr}/userinfo");
    service.configure_oauth_provider(config);
    let runner = service.clone();
    tokio::spawn(async move { runner.run(command_rx).await });

    command_tx
        .send(Command::StartOAuth {
            provider: OAuthProvider::Microsoft,
            mode: OAuthFlowMode::DeviceCode,
            account: Some(account_id),
            email_hint: Some("alice@example.com".into()),
        })
        .unwrap();

    let started = wait_for_event(&mut event_rx, Duration::from_secs(5), |event| {
        matches!(event, Event::OAuthFlowStarted { .. })
    })
    .await;
    let Event::OAuthFlowStarted { user_code, .. } = started else {
        unreachable!()
    };
    assert_eq!(user_code.as_deref(), Some("ABCD-1234"));

    let completed = wait_for_event(&mut event_rx, Duration::from_secs(10), |event| {
        matches!(
            event,
            Event::OAuthCompleted { .. } | Event::OAuthFailed { .. }
        )
    })
    .await;
    match completed {
        Event::OAuthCompleted { created, .. } => assert!(!created),
        Event::OAuthFailed { message, .. } => panic!("device flow failed: {message}"),
        _ => unreachable!(),
    }
    let updated = store.writable().get_account(account_id).unwrap().unwrap();
    assert!(!updated.needs_reauth);
    assert!(updated.token_expires_at.is_some());

    let requests = requests.lock().unwrap().clone().join("\n");
    assert!(requests.contains("/device"));
    assert!(requests.contains("/token"));
    let token_requests = requests
        .lines()
        .filter(|line| line.starts_with("/token"))
        .count();
    assert!(
        token_requests >= 2,
        "expected pending then success, got {requests}"
    );
}
