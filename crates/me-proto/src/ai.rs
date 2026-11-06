//! AI 服务商传输层：Anthropic Messages 与 OpenAI 兼容两种协议。
//!
//! 职责非常窄：**把一次请求发出去，把一次模型轮次的增量搬回来**。
//! 它不执行工具、不触库、不发事件、不持有密钥（key 只在 [`AiChatRequest`] 里活一次）。
//!
//! 约束（`doc/v03架构设计.md` §6.3）：
//! - 只允许 HTTPS；OpenAI 兼容允许用户配置的本机 http 端点；
//! - 禁止重定向、不携带 Cookie / Referer；
//! - 连接 15s / 单轮 120s；流式响应 ≤ 4 MiB、单行 ≤ 256 KiB；
//! - 请求体与响应体**都不写日志**；错误 `detail` 只含状态码与错误类别。
//!
//! 一处有意的口径：模型返回**白名单外**的工具名视为协议错误而不是「工具错误结果」。
//! 工具清单是固定的（§2.3 第 8 条），出现别的名字只可能是端点/提示词不匹配，
//! 静默继续会让用户看到一段「AI 假装调用了某个工具」的回复。

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::StreamExt;
use me_core::ai::{
    AiChatRequest, AiDelta, AiDeltaSink, AiFuture, AiProvider, AiProviderKind, AiToolCall,
    AiToolName, AiTurnResult, AiWireMessage, CancelFlag,
};
use me_core::{AiErrorKind, CoreError};
use serde_json::{Value, json};

use crate::sse::{SseDecoder, SseEvent};

/// 建立连接的超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 单轮模型调用总超时。
const TURN_TIMEOUT: Duration = Duration::from_secs(120);
/// 错误响应体最多读这么多（只用于分类，不进 detail）。
const MAX_ERROR_BODY: usize = 8 * 1024;
/// Anthropic 要求固定版本头。
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// 生产实现。一个实例可同时服务两种协议：按 `config.kind` 分支。
#[derive(Clone)]
pub struct HttpAiProvider {
    client: reqwest::Client,
}

impl HttpAiProvider {
    pub fn new() -> Result<Self, CoreError> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TURN_TIMEOUT)
            // 重定向会把 Authorization 带到别的主机；一律拒绝。
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| {
                CoreError::ai(
                    AiErrorKind::Config,
                    None::<String>,
                    "创建 AI HTTP 客户端失败",
                )
            })?;
        Ok(Self { client })
    }
}

/// 实际请求地址。`base_url` 末尾的 `/v1` 会与 Anthropic 的 `/v1/messages` 去重。
pub fn endpoint(config: &me_core::ai::AiProviderConfig) -> Result<String, CoreError> {
    let base = config.effective_base_url().trim_end_matches('/');
    if base.is_empty() {
        return Err(CoreError::ai(
            AiErrorKind::Config,
            None::<String>,
            "请先在设置中填写 Base URL",
        ));
    }
    match config.kind {
        AiProviderKind::Anthropic => {
            let base = base.strip_suffix("/v1").unwrap_or(base);
            Ok(format!("{base}/v1/messages"))
        }
        AiProviderKind::OpenAiCompatible => Ok(format!("{base}/chat/completions")),
    }
}

// ---------------------------------------------------------------------------
// 请求体（纯函数，可脱离网络单测）
// ---------------------------------------------------------------------------

/// Anthropic 的 `messages`：相邻同角色消息必须合并，`tool_result` 只能出现在 `user` 里。
fn anthropic_messages(messages: &[AiWireMessage]) -> Vec<Value> {
    let mut out: Vec<(String, Vec<Value>)> = Vec::new();

    let push = |role: &str, block: Value, out: &mut Vec<(String, Vec<Value>)>| match out.last_mut()
    {
        Some((last_role, blocks)) if last_role == role => blocks.push(block),
        _ => out.push((role.to_string(), vec![block])),
    };

    for message in messages {
        match message {
            AiWireMessage::User { content } => {
                push("user", json!({"type": "text", "text": content}), &mut out);
            }
            AiWireMessage::Assistant {
                content,
                tool_calls,
            } => {
                if !content.is_empty() {
                    push(
                        "assistant",
                        json!({"type": "text", "text": content}),
                        &mut out,
                    );
                }
                for call in tool_calls {
                    let input = if call.parse_ok {
                        call.arguments.clone()
                    } else {
                        json!({})
                    };
                    push(
                        "assistant",
                        json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.name.as_str(),
                            "input": input,
                        }),
                        &mut out,
                    );
                }
                // 只有工具调用、没有文本的轮次也要占一个 assistant 位置。
                if content.is_empty() && tool_calls.is_empty() {
                    push(
                        "assistant",
                        json!({"type": "text", "text": "(no content)"}),
                        &mut out,
                    );
                }
            }
            AiWireMessage::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                push(
                    "user",
                    json!({
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": content,
                        "is_error": is_error,
                    }),
                    &mut out,
                );
            }
        }
    }

    out.into_iter()
        .map(|(role, blocks)| json!({"role": role, "content": blocks}))
        .collect()
}

pub fn anthropic_body(request: &AiChatRequest) -> Value {
    let mut body = json!({
        "model": request.config.model,
        "max_tokens": request.max_output_tokens,
        "system": request.system,
        "messages": anthropic_messages(&request.messages),
        "stream": true,
    });
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|spec| {
                json!({
                    "name": spec.name.as_str(),
                    "description": spec.description,
                    "input_schema": spec.input_schema,
                })
            })
            .collect();
        body["tools"] = Value::Array(tools);
    }
    body
}

pub fn openai_body(request: &AiChatRequest) -> Value {
    let mut messages: Vec<Value> = vec![json!({"role": "system", "content": request.system})];
    for message in &request.messages {
        match message {
            AiWireMessage::User { content } => {
                messages.push(json!({"role": "user", "content": content}));
            }
            AiWireMessage::Assistant {
                content,
                tool_calls,
            } => {
                let mut entry = json!({
                    "role": "assistant",
                    "content": if content.is_empty() { Value::Null } else { Value::String(content.clone()) },
                });
                if !tool_calls.is_empty() {
                    let calls: Vec<Value> = tool_calls
                        .iter()
                        .map(|call| {
                            let arguments = if call.parse_ok {
                                serde_json::to_string(&call.arguments)
                                    .unwrap_or_else(|_| "{}".into())
                            } else {
                                "{}".to_string()
                            };
                            json!({
                                "id": call.id,
                                "type": "function",
                                "function": {"name": call.name.as_str(), "arguments": arguments},
                            })
                        })
                        .collect();
                    entry["tool_calls"] = Value::Array(calls);
                }
                messages.push(entry);
            }
            AiWireMessage::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                // OpenAI 的 tool 消息没有 is_error 字段：用前缀让模型知道失败了。
                let content = if *is_error {
                    format!("[工具执行失败] {content}")
                } else {
                    content.clone()
                };
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": content,
                }));
            }
        }
    }

    let mut body = json!({
        "model": request.config.model,
        "max_tokens": request.max_output_tokens,
        "messages": messages,
        "stream": true,
        // 不显式要求的话多数兼容端点不会在流里给 usage。
        "stream_options": {"include_usage": true},
    });
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|spec| {
                json!({
                    "type": "function",
                    "function": {
                        "name": spec.name.as_str(),
                        "description": spec.description,
                        "parameters": spec.input_schema,
                    }
                })
            })
            .collect();
        body["tools"] = Value::Array(tools);
    }
    body
}

// ---------------------------------------------------------------------------
// 错误映射
// ---------------------------------------------------------------------------

/// 把 HTTP 状态映射成 `CoreError::Ai`。**不把响应体带进 detail**。
pub fn map_status(status: u16, body: &[u8]) -> CoreError {
    let code = status.to_string();
    let hint = String::from_utf8_lossy(&body[..body.len().min(MAX_ERROR_BODY)]);
    let lower = hint.to_ascii_lowercase();
    let context_too_long = lower.contains("context length")
        || lower.contains("context_length")
        || lower.contains("maximum context")
        || lower.contains("too many tokens");

    let (kind, detail) = match status {
        401 | 403 => (AiErrorKind::Auth, "API Key 无效或无权限"),
        429 => (AiErrorKind::RateLimited, "请求过于频繁"),
        400 | 413 | 422 if context_too_long => (AiErrorKind::Request, "上下文超出模型上限"),
        400 | 404 | 413 | 422 => (AiErrorKind::Request, "请求被拒绝（端点或模型可能不存在）"),
        408 | 504 => (AiErrorKind::Transient, "服务响应超时"),
        s if s >= 500 => (AiErrorKind::Transient, "服务端错误"),
        _ => (AiErrorKind::Protocol, "未预期的响应状态"),
    };
    CoreError::ai(kind, Some(code), detail)
}

fn map_transport(err: reqwest::Error) -> CoreError {
    if err.is_timeout() {
        return CoreError::ai(AiErrorKind::Transient, None::<String>, "AI 请求超时");
    }
    if err.is_connect() {
        return CoreError::ai(AiErrorKind::Transient, None::<String>, "无法连接 AI 服务");
    }
    if err.is_redirect() {
        return CoreError::ai(AiErrorKind::Protocol, None::<String>, "AI 服务返回了重定向");
    }
    if err.is_body() || err.is_decode() {
        return CoreError::ai(AiErrorKind::Protocol, None::<String>, "读取 AI 响应失败");
    }
    CoreError::ai(AiErrorKind::Transient, None::<String>, "AI 请求失败")
}

/// 服务商在流里塞的 `error` 事件。
fn provider_error(payload: &Value) -> CoreError {
    let kind = payload["error"]["type"]
        .as_str()
        .or_else(|| payload["type"].as_str())
        .unwrap_or("");
    let mapped = match kind {
        "authentication_error" | "permission_error" | "invalid_api_key" => AiErrorKind::Auth,
        "rate_limit_error" | "insufficient_quota" => AiErrorKind::RateLimited,
        "overloaded_error" | "api_error" | "server_error" => AiErrorKind::Transient,
        "invalid_request_error" | "not_found_error" => AiErrorKind::Request,
        _ => AiErrorKind::Protocol,
    };
    CoreError::ai(mapped, None::<String>, "AI 服务返回错误")
}

fn parse_data(data: &str) -> Result<Value, CoreError> {
    serde_json::from_str(data).map_err(|_| {
        CoreError::ai(
            AiErrorKind::Protocol,
            None::<String>,
            "AI 响应不是合法 JSON",
        )
    })
}

// ---------------------------------------------------------------------------
// 轮次累积
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct PartialTool {
    id: String,
    name: String,
    args: String,
}

#[derive(Default)]
struct TurnState {
    text: String,
    tools: BTreeMap<usize, PartialTool>,
    order: Vec<usize>,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    stop_reason: Option<String>,
}

impl TurnState {
    fn start_tool(&mut self, index: usize, id: &str, name: &str) {
        let slot = self.tools.entry(index).or_insert_with(|| {
            self.order.push(index);
            PartialTool::default()
        });
        if !id.is_empty() {
            slot.id = id.to_string();
        }
        if !name.is_empty() {
            slot.name.push_str(name);
        }
    }

    fn append_args(&mut self, index: usize, part: &str) {
        if !self.tools.contains_key(&index) {
            self.start_tool(index, "", "");
        }
        if let Some(slot) = self.tools.get_mut(&index) {
            slot.args.push_str(part);
        }
    }

    fn finish(self) -> Result<AiTurnResult, CoreError> {
        let mut tool_calls = Vec::new();
        for index in &self.order {
            let Some(partial) = self.tools.get(index) else {
                continue;
            };
            if partial.name.trim().is_empty() {
                continue;
            }
            let Some(name) = AiToolName::parse(&partial.name) else {
                return Err(CoreError::ai(
                    AiErrorKind::Protocol,
                    None::<String>,
                    "模型返回了白名单外的工具",
                ));
            };
            let trimmed = partial.args.trim();
            let (arguments, parse_ok) = if trimmed.is_empty() {
                (json!({}), true)
            } else {
                match serde_json::from_str::<Value>(trimmed) {
                    Ok(value) if value.is_object() => (value, true),
                    _ => (Value::Null, false),
                }
            };
            tool_calls.push(AiToolCall {
                id: partial.id.clone(),
                name,
                arguments,
                parse_ok,
            });
        }
        Ok(AiTurnResult {
            text: self.text,
            tool_calls,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            stop_reason: self.stop_reason,
        })
    }
}

fn apply_anthropic(
    state: &mut TurnState,
    event: &SseEvent,
    sink: &AiDeltaSink,
) -> Result<(), CoreError> {
    match event.name.as_deref().unwrap_or("message") {
        "message_start" => {
            let value = parse_data(&event.data)?;
            state.input_tokens = value["message"]["usage"]["input_tokens"]
                .as_u64()
                .map(|n| n as u32);
            state.output_tokens = value["message"]["usage"]["output_tokens"]
                .as_u64()
                .map(|n| n as u32);
        }
        "content_block_start" => {
            let value = parse_data(&event.data)?;
            let index = value["index"].as_u64().unwrap_or(0) as usize;
            let block = &value["content_block"];
            if block["type"].as_str() == Some("tool_use") {
                let id = block["id"].as_str().unwrap_or_default();
                let name = block["name"].as_str().unwrap_or_default();
                state.start_tool(index, id, name);
                // 有些端点会在 start 里直接给完整 input；按 JSON 追加进缓冲。
                if let Some(input) = block.get("input")
                    && input.is_object()
                    && input.as_object().is_some_and(|o| !o.is_empty())
                {
                    state.append_args(index, &input.to_string());
                }
            }
        }
        "content_block_delta" => {
            let value = parse_data(&event.data)?;
            let index = value["index"].as_u64().unwrap_or(0) as usize;
            match value["delta"]["type"].as_str() {
                Some("text_delta") => {
                    let text = value["delta"]["text"].as_str().unwrap_or("");
                    if !text.is_empty() {
                        state.text.push_str(text);
                        sink(AiDelta::Text(text.to_string()));
                    }
                }
                Some("input_json_delta") => {
                    let part = value["delta"]["partial_json"].as_str().unwrap_or("");
                    state.append_args(index, part);
                }
                _ => {}
            }
        }
        "message_delta" => {
            let value = parse_data(&event.data)?;
            if let Some(reason) = value["delta"]["stop_reason"].as_str() {
                state.stop_reason = Some(reason.to_string());
            }
            if let Some(tokens) = value["usage"]["output_tokens"].as_u64() {
                state.output_tokens = Some(tokens as u32);
            }
            if let Some(tokens) = value["usage"]["input_tokens"].as_u64() {
                state.input_tokens = Some(tokens as u32);
            }
        }
        "error" => {
            let value = parse_data(&event.data)?;
            return Err(provider_error(&value));
        }
        _ => {}
    }
    Ok(())
}

fn apply_openai(
    state: &mut TurnState,
    event: &SseEvent,
    sink: &AiDeltaSink,
) -> Result<(), CoreError> {
    if event.is_done() {
        return Ok(());
    }
    let value = parse_data(&event.data)?;
    if value.get("error").is_some() {
        return Err(provider_error(&value));
    }
    if let Some(usage) = value.get("usage")
        && !usage.is_null()
    {
        state.input_tokens = usage["prompt_tokens"].as_u64().map(|n| n as u32);
        state.output_tokens = usage["completion_tokens"].as_u64().map(|n| n as u32);
    }
    let Some(choice) = value["choices"]
        .as_array()
        .and_then(|choices| choices.first())
    else {
        return Ok(());
    };
    if let Some(reason) = choice["finish_reason"].as_str()
        && !reason.is_empty()
    {
        state.stop_reason = Some(reason.to_string());
    }
    let delta = &choice["delta"];
    if let Some(text) = delta["content"].as_str()
        && !text.is_empty()
    {
        state.text.push_str(text);
        sink(AiDelta::Text(text.to_string()));
    }
    if let Some(calls) = delta["tool_calls"].as_array() {
        for call in calls {
            let index = call["index"].as_u64().unwrap_or(0) as usize;
            let id = call["id"].as_str().unwrap_or("");
            let name = call["function"]["name"].as_str().unwrap_or("");
            if !id.is_empty() || !name.is_empty() {
                state.start_tool(index, id, name);
            }
            if let Some(args) = call["function"]["arguments"].as_str()
                && !args.is_empty()
            {
                state.append_args(index, args);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 端口实现
// ---------------------------------------------------------------------------

impl AiProvider for HttpAiProvider {
    fn stream_chat(
        &self,
        request: AiChatRequest,
        sink: AiDeltaSink,
        cancel: CancelFlag,
    ) -> AiFuture<Result<AiTurnResult, CoreError>> {
        let client = self.client.clone();
        Box::pin(async move { stream_turn(client, request, sink, cancel).await })
    }
}

async fn stream_turn(
    client: reqwest::Client,
    request: AiChatRequest,
    sink: AiDeltaSink,
    cancel: CancelFlag,
) -> Result<AiTurnResult, CoreError> {
    let url = endpoint(&request.config)?;
    let mut builder = client
        .post(&url)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream");

    match request.config.kind {
        AiProviderKind::Anthropic => {
            let credential = request.credential.as_ref().ok_or_else(|| {
                CoreError::ai(
                    AiErrorKind::Config,
                    None::<String>,
                    "尚未配置 Anthropic API Key",
                )
            })?;
            builder = builder
                .header("x-api-key", credential.expose())
                .header("anthropic-version", ANTHROPIC_VERSION);
        }
        AiProviderKind::OpenAiCompatible => {
            if let Some(credential) = request.credential.as_ref() {
                builder =
                    builder.header("authorization", format!("Bearer {}", credential.expose()));
            }
        }
    }

    let body = match request.config.kind {
        AiProviderKind::Anthropic => anthropic_body(&request),
        AiProviderKind::OpenAiCompatible => openai_body(&request),
    };

    let payload = serde_json::to_vec(&body)
        .map_err(|_| CoreError::ai(AiErrorKind::Protocol, None::<String>, "AI 请求体无法序列化"))?;
    let response = builder.body(payload).send().await.map_err(map_transport)?;
    let status = response.status();
    if !status.is_success() {
        let bytes = response.bytes().await.unwrap_or_default();
        return Err(map_status(status.as_u16(), &bytes));
    }

    let mut stream = response.bytes_stream();
    let mut decoder = SseDecoder::new();
    let mut state = TurnState::default();

    let handle = |state: &mut TurnState, event: &SseEvent| -> Result<(), CoreError> {
        match request.config.kind {
            AiProviderKind::Anthropic => apply_anthropic(state, event, &sink),
            AiProviderKind::OpenAiCompatible => apply_openai(state, event, &sink),
        }
    };

    while let Some(chunk) = stream.next().await {
        if cancel.is_cancelled() {
            return Err(CoreError::cancelled());
        }
        let chunk = chunk.map_err(map_transport)?;
        for event in decoder.push(&chunk)? {
            cancel.check()?;
            handle(&mut state, &event)?;
        }
    }

    if let Some(event) = decoder.finish() {
        handle(&mut state, &event)?;
    }

    if let (Some(input), Some(output)) = (state.input_tokens, state.output_tokens) {
        sink(AiDelta::Usage {
            input: Some(input),
            output: Some(output),
        });
    }

    state.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_core::ai::{
        AiChatRequest, AiCredential, AiProviderConfig, AiToolPermissions, tool_specs,
    };
    use std::sync::{Arc, Mutex};

    fn config(kind: AiProviderKind) -> AiProviderConfig {
        AiProviderConfig {
            kind,
            base_url: if kind == AiProviderKind::Anthropic {
                "https://api.anthropic.com".into()
            } else {
                "http://127.0.0.1:11434/v1".into()
            },
            model: "test-model".into(),
            max_output_tokens: 512,
        }
    }

    fn request(kind: AiProviderKind, messages: Vec<AiWireMessage>) -> AiChatRequest {
        AiChatRequest {
            config: config(kind),
            credential: Some(AiCredential::new("sk-test")),
            system: "sys".into(),
            messages,
            tools: tool_specs(&AiToolPermissions::default()),
            max_output_tokens: 512,
        }
    }

    fn sink() -> (AiDeltaSink, Arc<Mutex<String>>) {
        let buffer = Arc::new(Mutex::new(String::new()));
        let clone = buffer.clone();
        let sink: AiDeltaSink = Arc::new(move |delta| {
            if let AiDelta::Text(text) = delta {
                clone.lock().unwrap().push_str(&text);
            }
        });
        (sink, buffer)
    }

    #[test]
    fn endpoint_avoids_duplicate_v1() {
        let mut ant = config(AiProviderKind::Anthropic);
        assert_eq!(
            endpoint(&ant).unwrap(),
            "https://api.anthropic.com/v1/messages"
        );
        ant.base_url = "https://proxy.example.com/v1".into();
        assert_eq!(
            endpoint(&ant).unwrap(),
            "https://proxy.example.com/v1/messages"
        );

        let openai = config(AiProviderKind::OpenAiCompatible);
        assert_eq!(
            endpoint(&openai).unwrap(),
            "http://127.0.0.1:11434/v1/chat/completions"
        );
    }

    #[test]
    fn anthropic_body_merges_tool_results_into_user_turn() {
        let req = request(
            AiProviderKind::Anthropic,
            vec![
                AiWireMessage::User {
                    content: "查邮件".into(),
                },
                AiWireMessage::Assistant {
                    content: String::new(),
                    tool_calls: vec![AiToolCall {
                        id: "call_1".into(),
                        name: AiToolName::EmailSearch,
                        arguments: json!({"query": "周报"}),
                        parse_ok: true,
                    }],
                },
                AiWireMessage::ToolResult {
                    call_id: "call_1".into(),
                    content: "找到 1 封".into(),
                    is_error: false,
                },
                AiWireMessage::ToolResult {
                    call_id: "call_2".into(),
                    content: "超时".into(),
                    is_error: true,
                },
            ],
        );
        let body = anthropic_body(&req);
        let messages = body["messages"].as_array().unwrap();
        // user(问题) → assistant(tool_use) → user(两个 tool_result 合并)
        assert_eq!(messages.len(), 3, "两个 tool_result 必须合并成一条 user");
        assert_eq!(messages[2]["role"], "user");
        let blocks = messages[2]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[1]["is_error"], true);
        // 工具数量跟着 `TOOL_SPECS` 走，别写死：每次加工具都会有人忘了改这里。
        assert_eq!(
            body["tools"].as_array().unwrap().len(),
            me_core::ai::TOOL_SPECS.len()
        );
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn anthropic_merges_consecutive_user_messages() {
        let req = request(
            AiProviderKind::Anthropic,
            vec![
                AiWireMessage::User {
                    content: "a".into(),
                },
                AiWireMessage::User {
                    content: "b".into(),
                },
            ],
        );
        let body = anthropic_body(&req);
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn openai_body_marks_tool_errors_and_requests_usage() {
        let req = request(
            AiProviderKind::OpenAiCompatible,
            vec![
                AiWireMessage::User {
                    content: "问".into(),
                },
                AiWireMessage::ToolResult {
                    call_id: "c1".into(),
                    content: "失败".into(),
                    is_error: true,
                },
            ],
        );
        let body = openai_body(&req);
        assert_eq!(body["stream_options"]["include_usage"], true);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert!(
            messages[2]["content"]
                .as_str()
                .unwrap()
                .contains("工具执行失败")
        );
    }

    #[test]
    fn status_mapping_covers_the_table() {
        assert!(matches!(
            map_status(401, b""),
            CoreError::Ai {
                kind: AiErrorKind::Auth,
                ..
            }
        ));
        assert!(matches!(
            map_status(429, b""),
            CoreError::Ai {
                kind: AiErrorKind::RateLimited,
                ..
            }
        ));
        assert!(matches!(
            map_status(529, b""),
            CoreError::Ai {
                kind: AiErrorKind::Transient,
                retryable: true,
                ..
            }
        ));
        let long = map_status(400, b"{\"error\":\"maximum context length is 128000\"}");
        match long {
            CoreError::Ai { kind, detail, .. } => {
                assert_eq!(kind, AiErrorKind::Request);
                assert!(detail.contains("上下文"), "detail 应指出上下文超限");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn anthropic_text_deltas_reach_the_sink() {
        let (sink, buffer) = sink();
        let mut state = TurnState::default();
        for data in [
            r#"{"index":0,"delta":{"type":"text_delta","text":"你"}}"#,
            r#"{"index":0,"delta":{"type":"text_delta","text":"好"}}"#,
        ] {
            apply_anthropic(
                &mut state,
                &SseEvent {
                    name: Some("content_block_delta".into()),
                    data: data.into(),
                },
                &sink,
            )
            .unwrap();
        }
        assert_eq!(*buffer.lock().unwrap(), "你好");
        assert_eq!(state.finish().unwrap().text, "你好");
    }

    #[test]
    fn anthropic_tool_use_accumulates_partial_json() {
        let (sink, _) = sink();
        let mut state = TurnState::default();
        apply_anthropic(
            &mut state,
            &SseEvent {
                name: Some("content_block_start".into()),
                data: r#"{"index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"weather_get"}}"#.into(),
            },
            &sink,
        )
        .unwrap();
        for part in [r#"{"location":"#, r#""成都","days":3}"#] {
            apply_anthropic(
                &mut state,
                &SseEvent {
                    name: Some("content_block_delta".into()),
                    data: format!(
                        r#"{{"index":1,"delta":{{"type":"input_json_delta","partial_json":{}}}}}"#,
                        serde_json::to_string(part).unwrap()
                    ),
                },
                &sink,
            )
            .unwrap();
        }
        let turn = state.finish().unwrap();
        assert_eq!(turn.tool_calls.len(), 1);
        let call = &turn.tool_calls[0];
        assert_eq!(call.id, "toolu_1");
        assert_eq!(call.name, AiToolName::WeatherGet);
        assert!(call.parse_ok);
        assert_eq!(call.arguments["location"], "成都");
    }

    #[test]
    fn openai_stream_accumulates_tool_calls_by_index() {
        let (sink, buffer) = sink();
        let mut state = TurnState::default();
        let chunks = [
            r#"{"choices":[{"delta":{"content":"好的"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"email_search","arguments":"{\"query\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"周报\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":12,"completion_tokens":7}}"#,
        ];
        for data in chunks {
            apply_openai(
                &mut state,
                &SseEvent {
                    name: None,
                    data: data.into(),
                },
                &sink,
            )
            .unwrap();
        }
        assert_eq!(*buffer.lock().unwrap(), "好的");
        let turn = state.finish().unwrap();
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].arguments["query"], "周报");
        assert_eq!(turn.input_tokens, Some(12));
        assert_eq!(turn.output_tokens, Some(7));
        assert_eq!(turn.stop_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn unknown_tool_name_is_a_protocol_error() {
        let (sink, _) = sink();
        let mut state = TurnState::default();
        apply_openai(
            &mut state,
            &SseEvent {
                name: None,
                data: r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"send_mail","arguments":"{}"}}]}}]}"#.into(),
            },
            &sink,
        )
        .unwrap();
        let err = state.finish().unwrap_err();
        assert!(matches!(
            err,
            CoreError::Ai {
                kind: AiErrorKind::Protocol,
                ..
            }
        ));
    }

    #[test]
    fn invalid_arguments_are_marked_not_dropped() {
        let (sink, _) = sink();
        let mut state = TurnState::default();
        apply_openai(
            &mut state,
            &SseEvent {
                name: None,
                data: r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"email_search","arguments":"{oops"}}]}}]}"#.into(),
            },
            &sink,
        )
        .unwrap();
        let turn = state.finish().unwrap();
        assert_eq!(turn.tool_calls.len(), 1);
        assert!(!turn.tool_calls[0].parse_ok);
    }

    #[test]
    fn stream_error_event_maps_to_ai_error() {
        let (sink, _) = sink();
        let mut state = TurnState::default();
        let err = apply_anthropic(
            &mut state,
            &SseEvent {
                name: Some("error".into()),
                data: r#"{"type":"error","error":{"type":"overloaded_error"}}"#.into(),
            },
            &sink,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            CoreError::Ai {
                kind: AiErrorKind::Transient,
                ..
            }
        ));
    }

    #[test]
    fn error_details_never_contain_the_key() {
        let err = map_status(401, b"{\"error\":\"bad key sk-test\"}");
        let text = format!("{err:?} {}", err.user_message());
        assert!(!text.contains("sk-test"));
    }
}
