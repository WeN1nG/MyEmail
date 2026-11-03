//! OAuth 端点 mock：用最小 HTTP/1.1 服务器验证令牌交换、刷新、设备码与错误分类。
//!
//! 每个 mock 只接受一次请求，返回固定状态码与 JSON；请求体通过 oneshot 回传，
//! 便于断言表单参数与 Authorization 头。

use std::net::SocketAddr;

use me_core::CoreError;
use me_core::model::OAuthProvider;
use me_proto::oauth::{DeviceTokenOutcome, OAuthClient, ProviderConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

struct CapturedRequest {
    method: String,
    path: String,
    headers: String,
    body: String,
}

async fn spawn_http(
    status: u16,
    response_body: impl Into<String>,
) -> (SocketAddr, oneshot::Receiver<CapturedRequest>) {
    let response_body = response_body.into();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = oneshot::channel();

    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let read = socket.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                return;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break position;
            }
        };
        let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
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
        while buffer.len() < header_end + 4 + content_length {
            let read = socket.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        let request_body = String::from_utf8_lossy(&buffer[header_end + 4..]).to_string();
        let mut lines = headers.lines();
        let request_line = lines.next().unwrap_or("");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        let _ = tx.send(CapturedRequest {
            method,
            path,
            headers,
            body: request_body,
        });

        let reason = if (200..300).contains(&status) {
            "OK"
        } else {
            "Error"
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
    });

    (addr, rx)
}

fn config(token_addr: SocketAddr) -> ProviderConfig {
    let mut config = ProviderConfig::for_provider(OAuthProvider::Google, "client-id");
    config.client_secret = Some("client-secret".into());
    config.token_url = format!("http://{token_addr}/token");
    config
}

const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

#[tokio::test]
async fn exchange_code_sends_pkce_and_parses_token() {
    let (addr, request) = spawn_http(
        200,
        r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer","scope":"openid"}"#,
    )
    .await;
    let client = OAuthClient::new(config(addr)).expect("client");
    let token = client
        .exchange_code("code-1", "http://127.0.0.1:9/cb", VERIFIER)
        .await
        .expect("exchange");
    assert_eq!(token.access_token, "access-1");
    assert_eq!(token.refresh_token.as_deref(), Some("refresh-1"));
    assert_eq!(token.token_type, "Bearer");
    assert!(token.expires_at > me_core::now_unix());
    assert!(!format!("{token:?}").contains("access-1"));

    let request = request.await.expect("request");
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/token");
    assert!(request.body.contains("grant_type=authorization_code"));
    assert!(request.body.contains("code=code-1"));
    assert!(request.body.contains("code_verifier="));
    assert!(request.body.contains("client_secret=client-secret"));
}

#[tokio::test]
async fn invalid_grant_requires_reauth_and_hides_description() {
    let (addr, _request) = spawn_http(
        400,
        r#"{"error":"invalid_grant","error_description":"token was revoked"}"#,
    )
    .await;
    let client = OAuthClient::new(config(addr)).expect("client");
    let err = client
        .exchange_code("code", "http://127.0.0.1:9/cb", VERIFIER)
        .await
        .expect_err("must fail");
    assert!(err.needs_reauth());
    assert!(!err.user_message().contains("token was revoked"));
    assert!(matches!(
        err,
        CoreError::OAuth {
            reauth_required: true,
            ..
        }
    ));
}

#[tokio::test]
async fn server_error_is_retryable_network() {
    let (addr, _request) = spawn_http(503, r#"{"error":"temporarily_unavailable"}"#).await;
    let client = OAuthClient::new(config(addr)).expect("client");
    let err = client
        .exchange_code("code", "http://127.0.0.1:9/cb", VERIFIER)
        .await
        .expect_err("must fail");
    assert!(matches!(
        err,
        CoreError::Network {
            retryable: true,
            ..
        }
    ));
}

#[tokio::test]
async fn refresh_uses_refresh_token_grant() {
    let (addr, request) = spawn_http(
        200,
        r#"{"access_token":"access-2","expires_in":1800,"token_type":"Bearer"}"#,
    )
    .await;
    let client = OAuthClient::new(config(addr)).expect("client");
    let token = client.refresh("refresh-2").await.expect("refresh");
    assert_eq!(token.access_token, "access-2");
    assert_eq!(token.refresh_token, None);

    let request = request.await.expect("request");
    assert!(request.body.contains("grant_type=refresh_token"));
    assert!(request.body.contains("refresh_token=refresh-2"));
}

#[tokio::test]
async fn device_code_flow_reports_pending_slow_down_and_token() {
    let (device_addr, device_request) = spawn_http(
        200,
        r#"{"device_code":"device-1","user_code":"ABCD-1234","verification_uri":"https://example.com/device","expires_in":900,"interval":5}"#,
    )
    .await;
    let mut device_config = config(device_addr);
    device_config.device_code_url = Some(format!("http://{device_addr}/device"));
    let device_client = OAuthClient::new(device_config).expect("client");
    let device = device_client
        .request_device_code()
        .await
        .expect("device code");
    assert_eq!(device.user_code, "ABCD-1234");
    assert_eq!(device.interval_secs, 5);
    assert!(device.expires_at > me_core::now_unix());
    assert!(!format!("{device:?}").contains("device-1"));
    let request = device_request.await.expect("request");
    assert_eq!(request.path, "/device");
    assert!(request.body.contains("client_id=client-id"));

    let (pending_addr, _pending_request) =
        spawn_http(400, r#"{"error":"authorization_pending"}"#).await;
    let pending_client = OAuthClient::new(config(pending_addr)).expect("client");
    assert_eq!(
        pending_client
            .poll_device_token("device-1")
            .await
            .expect("pending"),
        DeviceTokenOutcome::Pending
    );

    let (slow_addr, _slow_request) = spawn_http(400, r#"{"error":"slow_down"}"#).await;
    let slow_client = OAuthClient::new(config(slow_addr)).expect("client");
    assert_eq!(
        slow_client
            .poll_device_token("device-1")
            .await
            .expect("slow down"),
        DeviceTokenOutcome::SlowDown
    );

    let (token_addr, token_request) = spawn_http(
        200,
        r#"{"access_token":"access-3","expires_in":900,"token_type":"Bearer"}"#,
    )
    .await;
    let token_client = OAuthClient::new(config(token_addr)).expect("client");
    match token_client
        .poll_device_token("device-1")
        .await
        .expect("token")
    {
        DeviceTokenOutcome::Token(token) => assert_eq!(token.access_token, "access-3"),
        other => panic!("unexpected outcome: {other:?}"),
    }
    let request = token_request.await.expect("request");
    assert!(
        request
            .body
            .contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code")
    );
}

#[tokio::test]
async fn userinfo_uses_bearer_token() {
    let (addr, request) = spawn_http(200, r#"{"sub":"123456","email":"alice@example.com"}"#).await;
    let mut oauth_config = config(addr);
    oauth_config.userinfo_url = format!("http://{addr}/userinfo");
    let client = OAuthClient::new(oauth_config).expect("client");
    let user = client.fetch_userinfo("access-4").await.expect("userinfo");
    assert_eq!(user.subject.as_deref(), Some("123456"));
    assert_eq!(user.email.as_deref(), Some("alice@example.com"));

    let request = request.await.expect("request");
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/userinfo");
    assert!(
        request.headers.contains("authorization: Bearer access-4")
            || request.headers.contains("Authorization: Bearer access-4")
    );
}

#[tokio::test]
async fn oversized_response_body_is_rejected() {
    let huge = format!(r#"{{"access_token":"{}"}}"#, "x".repeat(70 * 1024));
    let (addr, _request) = spawn_http(200, huge).await;
    let client = OAuthClient::new(config(addr)).expect("client");
    let err = client
        .exchange_code("code", "http://127.0.0.1:9/cb", VERIFIER)
        .await
        .expect_err("must reject oversized body");
    assert!(format!("{err:?}").contains("64 KB"));
}
