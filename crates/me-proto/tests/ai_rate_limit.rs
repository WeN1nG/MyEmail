//! AI 传输层的限流处理：429 退避后重试。
//!
//! 2026-11-07 的两次真实对话都以「请求过于频繁」收场：模型前几轮因为参数
//! 写错反复重试，请求密度顶到限流线上，后面的请求全被 429 打掉，整轮失败
//! ——而模型其实已经查到了 20 封邮件。
//!
//! 这里验证：429 会退避重试，而不是立刻把整轮对话判死。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use me_core::ai::{
    AiChatRequest, AiCredential, AiDelta, AiDeltaSink, AiProvider, AiProviderConfig,
    AiProviderKind, AiWireMessage,
};
use me_proto::ai::HttpAiProvider;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 起一个 mock HTTP 服务：前 `rate_limited_times` 次回 429，之后回正常 SSE。
///
/// 返回请求次数的计数器——测试要确认「确实重试过」，而不只是「最后成功了」。
async fn spawn_mock(rate_limited_times: usize) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let counter = counter.clone();
            tokio::spawn(async move {
                // 把整个请求读完（头 + 体）：读一半就回响应，
                // 客户端还在发体时会看到连接被重置。
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let Ok(read) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..read]);
                    // 头结束，且 Content-Length 的体也收齐了，才算读完。
                    let text = String::from_utf8_lossy(&buf);
                    let Some(head_end) = text.find("\r\n\r\n") else {
                        continue;
                    };
                    let head = &text[..head_end];
                    let length = head
                        .lines()
                        .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
                        .and_then(|line| line.split(':').nth(1))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= head_end + 4 + length {
                        break;
                    }
                }

                let attempt = counter.fetch_add(1, Ordering::SeqCst);
                if attempt < rate_limited_times {
                    socket
                        .write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await
                        .ok();
                    return;
                }
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\n\
data: [DONE]\n\n";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(response.as_bytes()).await.ok();
            });
        }
    });

    (format!("http://{addr}/v1"), hits)
}

fn request(base_url: String) -> AiChatRequest {
    AiChatRequest {
        config: AiProviderConfig {
            kind: AiProviderKind::OpenAiCompatible,
            base_url,
            model: "test-model".into(),
            max_output_tokens: 512,
        },
        credential: Some(AiCredential::new("sk-test")),
        system: "sys".into(),
        messages: vec![AiWireMessage::User {
            content: "你好".into(),
        }],
        tools: Vec::new(),
        max_output_tokens: 512,
    }
}

fn sink() -> (AiDeltaSink, Arc<std::sync::Mutex<String>>) {
    let buffer = Arc::new(std::sync::Mutex::new(String::new()));
    let clone = buffer.clone();
    let sink: AiDeltaSink = Arc::new(move |delta| {
        if let AiDelta::Text(text) = delta {
            clone.lock().unwrap().push_str(&text);
        }
    });
    (sink, buffer)
}

/// 撞一次 429 之后应当重试并成功，而不是整轮失败。
///
/// 退避是真的（生产值 2 秒起步），这里换成毫秒级——
/// 测试要验的是「会不会重试」，不是「等多久」。
#[tokio::test]
async fn rate_limit_is_retried_and_the_turn_succeeds() {
    me_proto::ai::override_rate_limit_backoff([1, 1, 1]);
    let (base_url, hits) = spawn_mock(1).await;
    let provider = HttpAiProvider::new().expect("provider");
    let (sink, buffer) = sink();

    let turn = provider
        .stream_chat(request(base_url), sink, me_core::ai::CancelFlag::new())
        .await
        .expect("429 之后重试，不该失败");

    assert_eq!(turn.text, "你好");
    assert_eq!(*buffer.lock().unwrap(), "你好");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "应当发了两次：一次被限流、一次成功"
    );
}

/// 连续被限流到退避用完，才把错误抛给上层。
///
/// 这一条要钉死：退避不是无限的，否则模型会一直挂着。
#[tokio::test]
async fn persistent_rate_limit_eventually_fails() {
    me_proto::ai::override_rate_limit_backoff([1, 1, 1]);
    // 比退避次数多一次，确保用完仍被限流。
    let (base_url, hits) = spawn_mock(99).await;
    let provider = HttpAiProvider::new().expect("provider");
    let (sink, _buffer) = sink();

    let err = provider
        .stream_chat(request(base_url), sink, me_core::ai::CancelFlag::new())
        .await
        .expect_err("一直限流就该失败");

    assert!(
        matches!(
            err,
            me_core::CoreError::Ai {
                kind: me_core::AiErrorKind::RateLimited,
                ..
            }
        ),
        "应当报限流：{err:?}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 4, "初次 + 3 次退避");
}
