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

/// 遇到 429（请求过于频繁）时的退避间隔。
///
/// 429 是**服务端在说「你太快了」**，不是请求本身有问题。工具循环里
/// 每一轮都是一次新的模型请求，密集请求很容易撞上限流——
/// 2026-11-07 的两次真实对话就是这么收场的：前几轮因为参数写错反复重试，
/// 请求密度顶上去，后面的请求全被 429 打掉，整轮失败。
///
/// 退避后重试而不是直接失败：请求还没被处理，重发是安全的。
const RATE_LIMIT_BACKOFF: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(12),
];

/// 退避表；测试会把它换成毫秒级的。
///
/// 抽成静态而不是常量：集成测试要跑「一直限流」那条路径，
/// 真等 19 秒太慢了。**只给测试用**，生产值固定。
static RATE_LIMIT_BACKOFF_SLOTS: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// 读当前的退避表。生产走 [`RATE_LIMIT_BACKOFF`]，测试可以覆盖。
fn rate_limit_backoff() -> Vec<Duration> {
    let overridden = RATE_LIMIT_BACKOFF_SLOTS
        .iter()
        .map(|slot| slot.load(std::sync::atomic::Ordering::Relaxed))
        .collect::<Vec<_>>();
    if overridden.iter().all(|value| *value > 0) {
        return overridden.into_iter().map(Duration::from_millis).collect();
    }
    RATE_LIMIT_BACKOFF.to_vec()
}

/// 把退避表换成毫秒级。**只给测试用**。
///
/// 放在 `#[doc(hidden)]` 的公开函数里，是因为集成测试是独立的 crate，
/// 摸不到模块内部的私有状态。
#[doc(hidden)]
pub fn override_rate_limit_backoff(millis: [u64; 3]) {
    for (slot, value) in RATE_LIMIT_BACKOFF_SLOTS.iter().zip(millis) {
        slot.store(value, std::sync::atomic::Ordering::Relaxed);
    }
}

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
    /// 服务商返回思考内容时在这里累积。**不进 `AiWireMessage::Assistant`**，
    /// 也不回灌上下文——只用于展示。
    thinking: String,
    tools: BTreeMap<usize, PartialTool>,
    order: Vec<usize>,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    stop_reason: Option<String>,
    /// 正文扫描器：把「正文」与「文本形态的工具调用」分开。
    scanner: TextToolScanner,
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
        // 文本形态的工具调用排在结构化调用之后：模型先正经调工具、
        // 又在正文里补写一段标签时，正文里的那份是「补话」，不该抢在前面。
        for (offset, call) in self.scanner.calls.iter().enumerate() {
            let Some(name) = AiToolName::parse(&call.name) else {
                tracing::warn!(
                    tool = %call.name,
                    "正文里的工具调用不在白名单内，已忽略"
                );
                continue;
            };
            tool_calls.push(AiToolCall {
                id: format!("text_call_{}", offset + 1),
                name,
                arguments: Value::Object(call.arguments.clone()),
                parse_ok: true,
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

// ---------------------------------------------------------------------------
// 文本形态的工具调用
// ---------------------------------------------------------------------------

/// 标签名：`<tool_call>` … `</tool_call>`。
const TEXT_TOOL_TAG: &str = "tool_call";

/// 文本形态的一次工具调用：工具名 + 参数。
struct TextToolCall {
    name: String,
    arguments: serde_json::Map<String, Value>,
}

/// 正文扫描器：把「正文」与「文本形态的工具调用」分开。
///
/// ## 为什么需要它
///
/// 2026-11-07 的真实事故（模型 `agnes-2.5-flash`）：收尾阶段引擎没下发任何
/// 工具，模型却仍想调 `plan_update`，于是把调用写进了正文：
///
/// ```text
/// <tool_call>
/// <function=plan_update>
/// <parameter=goal>
/// 检索近期邮件并基于内容生成用户画像
/// </parameter>
/// ...
/// </function>
/// </tool_call>
/// ```
///
/// 协议层只认 `delta.tool_calls`，于是这段 XML 原样落进正文：用户看到的是一坨
/// 标签，真正的答案（模型已经写进思考区）一个字都没显示。
///
/// 这里把标签从正文里剥掉，并还原成一次**真正的**工具调用。
#[derive(Default)]
struct TextToolScanner {
    /// 还没判定归属的原文。
    buf: String,
    /// 从正文里认出来的工具调用。
    calls: Vec<TextToolCall>,
}

impl TextToolScanner {
    /// 吃进一段正文增量，返回**可以对外发**的那部分。
    ///
    /// 标签是分片到达的（`<` 和 `tool_call>` 可能落在两个 delta 里），
    /// 所以没看到结束标签之前不能把后半截放出去。
    fn feed(&mut self, chunk: &str) -> String {
        self.buf.push_str(chunk);
        let mut out = String::new();
        loop {
            let Some((open_start, open_end)) = find_text_tool_tag(&self.buf, 0, false) else {
                // 没有开始标签：留一个可能是不完整标签的尾巴，其余交出去。
                let cut = self.buf.len() - partial_tag_len(&self.buf);
                out.push_str(&self.buf[..cut]);
                self.buf.drain(..cut);
                break;
            };
            let Some((_, close_end)) = find_text_tool_tag(&self.buf, open_end, true) else {
                // 开始标签到了、结束标签还没来：先把前面的正文交出去，剩下的等。
                out.push_str(&self.buf[..open_start]);
                self.buf.drain(..open_start);
                break;
            };
            out.push_str(&self.buf[..open_start]);
            if let Some(call) = parse_text_tool_call(&self.buf[open_start..close_end]) {
                self.calls.push(call);
            }
            self.buf.drain(..close_end);
        }
        out
    }

    /// 收尾：把剩下的原文交出去。
    ///
    /// 没收完的标签（只有 `<tool_call>` 没有结束）不是正文，直接丢掉——
    /// 宁可少一句话，也不能让一段 XML 标签落到用户眼前。
    fn flush(&mut self) -> String {
        let mut out = String::new();
        if let Some((open_start, _)) = find_text_tool_tag(&self.buf, 0, false) {
            out.push_str(&self.buf[..open_start]);
        } else {
            out.push_str(&self.buf);
        }
        self.buf.clear();
        out
    }
}

/// 找到形如 `<tool_call>` / `</tool_call>` 的标签。
///
/// 返回 `(标签起点, 标签终点)`，终点是 `>` 之后的位置。
///
/// 标签里可能夹着零宽字符（U+200B / U+FEFF / U+2060）——那是模型把
/// 结构化调用「写出来」时最常见的花样，匹配时必须跳过。
fn find_text_tool_tag(buf: &str, from: usize, closing: bool) -> Option<(usize, usize)> {
    let bytes = buf.as_bytes();
    let mut at = from;
    while at < bytes.len() {
        let rel = buf[at..].find('<')?;
        let start = at + rel;
        let mut k = start + 1;
        if closing {
            if !buf[k..].starts_with('/') {
                at = start + 1;
                continue;
            }
            k += 1;
        }
        while let Some(len) = zero_width_len(bytes, k) {
            k += len;
        }
        if buf[k..].starts_with(TEXT_TOOL_TAG) {
            let mut j = k + TEXT_TOOL_TAG.len();
            while let Some(len) = zero_width_len(bytes, j) {
                j += len;
            }
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'>' {
                return Some((start, j + 1));
            }
        }
        at = start + 1;
    }
    None
}

/// 零宽字符的字节长度。三个候选都是 3 字节的 UTF-8 序列，
/// 跳过它们不会切到字符中间。
fn zero_width_len(bytes: &[u8], at: usize) -> Option<usize> {
    let rest = bytes.get(at..)?;
    if rest.starts_with(&[0xE2, 0x80, 0x8B]) {
        return Some(3); // U+200B ZERO WIDTH SPACE
    }
    if rest.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Some(3); // U+FEFF ZERO WIDTH NO-BREAK SPACE
    }
    if rest.starts_with(&[0xE2, 0x81, 0xA0]) {
        return Some(3); // U+2060 WORD JOINER
    }
    None
}

/// 缓冲区末尾有多长的一段可能是**不完整**的开始标签。
///
/// 差一个 `>` 的 `<tool_ca` 必须留住，等下一段补齐；否则会把
/// 半截标签当成正文发出去。
fn partial_tag_len(buf: &str) -> usize {
    const PREFIX: &str = "<tool_call";
    let bytes = buf.as_bytes();
    let max = PREFIX.len().min(bytes.len());
    for len in (1..=max).rev() {
        let start = buf.len() - len;
        if !buf.is_char_boundary(start) {
            continue;
        }
        if PREFIX.as_bytes().starts_with(&bytes[start..]) {
            return len;
        }
    }
    0
}

/// 把一段文本形态的工具调用解析成「工具名 + 参数」。
///
/// 格式是模型自己约定的：
///
/// ```text
/// <function=plan_update>
/// <parameter=goal>
/// 检索近期邮件
/// </parameter>
/// </function>
/// ```
fn parse_text_tool_call(block: &str) -> Option<TextToolCall> {
    let fn_at = block.find("<function=")?;
    let name_start = fn_at + "<function=".len();
    let name_end = name_start + block[name_start..].find('>')?;
    let name = block[name_start..name_end].trim().to_string();
    if name.is_empty() {
        return None;
    }

    let mut arguments = serde_json::Map::new();
    let mut at = name_end;
    while let Some(rel) = block[at..].find("<parameter=") {
        let key_start = at + rel + "<parameter=".len();
        let Some(rel) = block[key_start..].find('>') else {
            break;
        };
        let key_end = key_start + rel;
        let key = block[key_start..key_end].trim().to_string();
        let value_start = key_end + 1;
        let Some(rel) = block[value_start..].find("</parameter>") else {
            break;
        };
        let value_end = value_start + rel;
        let value = coerce_value(block[value_start..value_end].trim());
        if !key.is_empty() {
            // 有些模型把整包参数塞进 `arguments` / `input` 里，
            // 那是一个对象，应该摊平到参数表，而不是当成一个叫
            // `arguments` 的参数。
            if is_argument_wrapper(&key) && value.is_object() {
                if let Some(object) = value.as_object() {
                    for (inner_key, inner_value) in object {
                        arguments.insert(inner_key.clone(), inner_value.clone());
                    }
                }
            } else {
                arguments.insert(key, value);
            }
        }
        at = value_end + "</parameter>".len();
    }
    Some(TextToolCall { name, arguments })
}

/// 参数名是「整包参数」的包装名。
fn is_argument_wrapper(key: &str) -> bool {
    matches!(
        key,
        "arguments" | "args" | "parameters" | "params" | "input" | "tool_input"
    )
}

/// 参数值：能当 JSON 就当 JSON，否则当字符串。
///
/// `steps` 这种数组参数在文本形态里就是一段 JSON 数组；
/// `goal` 这种就是一句话。
fn coerce_value(raw: &str) -> Value {
    if raw.is_empty() {
        return Value::String(String::new());
    }
    let looks_json = raw.starts_with('{')
        || raw.starts_with('[')
        || raw.starts_with('"')
        || matches!(raw, "true" | "false" | "null")
        || raw.parse::<f64>().is_ok();
    if looks_json && let Ok(value) = serde_json::from_str::<Value>(raw) {
        return value;
    }
    Value::String(raw.to_string())
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
                        // 与 OpenAI 路径同一把尺子：正文里混进来的「文本形态
                        // 工具调用」也要剥掉，不能原样落到用户眼前。
                        let clean = state.scanner.feed(text);
                        if !clean.is_empty() {
                            state.text.push_str(&clean);
                            sink(AiDelta::Text(clean));
                        }
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
    // 思考字段名各家不同：DeepSeek-R1 用 reasoning_content，部分网关用 reasoning。
    // 只认字符串——有些端点把 reasoning 塞成对象（含 effort / summary），
    // 那不是正文，取不到就当没有，不能 panic 也不能把 JSON 当文字展示。
    let reasoning = delta["reasoning_content"]
        .as_str()
        .or_else(|| delta["reasoning"].as_str());
    if let Some(text) = reasoning.filter(|text| !text.is_empty()) {
        state.thinking.push_str(text);
        sink(AiDelta::Thinking(text.to_string()));
    }
    if let Some(text) = delta["content"].as_str()
        && !text.is_empty()
    {
        // 正文里可能混着「文本形态的工具调用」：模型有时不填 `tool_calls`
        // 字段，而是把调用直接写成标签。扫描器把标签剥掉，还原成真正的调用，
        // 绝不能让这段 XML 原样落进用户看到的答复。
        let clean = state.scanner.feed(text);
        if !clean.is_empty() {
            state.text.push_str(&clean);
            sink(AiDelta::Text(clean));
        }
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

    // 每次发请求都要重建 builder：`RequestBuilder` 不支持 clone，
    // 而 429 退避后要重发同一个请求。
    let build = || -> Result<reqwest::RequestBuilder, CoreError> {
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
        Ok(builder)
    };

    let body = match request.config.kind {
        AiProviderKind::Anthropic => anthropic_body(&request),
        AiProviderKind::OpenAiCompatible => openai_body(&request),
    };

    let payload = serde_json::to_vec(&body)
        .map_err(|_| CoreError::ai(AiErrorKind::Protocol, None::<String>, "AI 请求体无法序列化"))?;

    // 429（请求过于频繁）退避后重试。
    //
    // 请求还没被服务端处理，重发是安全的；而直接失败会让整轮对话白费——
    // 前面的工具调用、已经拿到的结果全都没了。
    //
    // 退避次数用完仍被限流，才把错误抛给上层。
    let backoff = rate_limit_backoff();
    let mut response = None;
    for attempt in 0..=backoff.len() {
        cancel.check()?;
        let sent = build()?
            .body(payload.clone())
            .send()
            .await
            .map_err(map_transport)?;
        let status = sent.status();
        if status.as_u16() != 429 {
            response = Some(sent);
            break;
        }
        // 读掉响应体，否则连接不会释放。
        let _ = sent.bytes().await;
        let Some(backoff) = backoff.get(attempt) else {
            // 退避用完仍然 429：按限流失败返回。
            return Err(CoreError::ai(
                AiErrorKind::RateLimited,
                Some("429"),
                "请求过于频繁",
            ));
        };
        tokio::time::sleep(*backoff).await;
    }
    let Some(response) = response else {
        return Err(CoreError::ai(
            AiErrorKind::RateLimited,
            Some("429"),
            "请求过于频繁",
        ));
    };
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

    // 收尾：把扫描器里还没判定的尾巴交出去（没收完的标签已经被丢掉）。
    let tail = state.scanner.flush();
    if !tail.is_empty() {
        state.text.push_str(&tail);
        sink(AiDelta::Text(tail));
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

    // -----------------------------------------------------------------------
    // 思考内容（v1 只做 OpenAI 兼容端点的响应侧解析）
    // -----------------------------------------------------------------------

    /// 收集全部增量，用来验证思考与回答各自走对通道。
    fn collecting_sink() -> (AiDeltaSink, Arc<Mutex<Vec<AiDelta>>>) {
        let all = Arc::new(Mutex::new(Vec::new()));
        let clone = all.clone();
        let sink: AiDeltaSink = Arc::new(move |delta| clone.lock().unwrap().push(delta));
        (sink, all)
    }

    fn openai_event(delta: &str) -> SseEvent {
        SseEvent {
            name: None,
            data: format!(r#"{{"choices":[{{"index":0,"delta":{delta}}}]}}"#),
        }
    }

    #[test]
    fn openai_reasoning_content_reaches_sink() {
        let (sink, all) = collecting_sink();
        let mut state = TurnState::default();
        apply_openai(
            &mut state,
            &openai_event(r#"{"reasoning_content":"先想一下"}"#),
            &sink,
        )
        .unwrap();
        let deltas = all.lock().unwrap().clone();
        assert_eq!(deltas, vec![AiDelta::Thinking("先想一下".to_string())]);
        assert_eq!(state.thinking, "先想一下");
        assert_eq!(state.text, "", "思考不能混进正文");
    }

    /// 部分网关用 `reasoning` 而不是 `reasoning_content`。
    #[test]
    fn openai_reasoning_fallback_field() {
        let (sink, all) = collecting_sink();
        let mut state = TurnState::default();
        apply_openai(
            &mut state,
            &openai_event(r#"{"reasoning":"换个字段名"}"#),
            &sink,
        )
        .unwrap();
        assert_eq!(
            *all.lock().unwrap(),
            vec![AiDelta::Thinking("换个字段名".to_string())]
        );
    }

    /// 有些端点把 reasoning 塞成对象（effort/summary），不是正文。
    /// `.as_str()` 拿 None 就够了，绝不能 panic 或把 JSON 当文字展示。
    #[test]
    fn openai_reasoning_object_form_is_ignored_safely() {
        let (sink, all) = collecting_sink();
        let mut state = TurnState::default();
        apply_openai(
            &mut state,
            &openai_event(r#"{"reasoning":{"effort":"high","summary":"略"}}"#),
            &sink,
        )
        .unwrap();
        assert!(all.lock().unwrap().is_empty(), "不该产出任何增量");
        assert_eq!(state.thinking, "");
    }

    /// 思考与正文交错到达时，两条通道各自正确、顺序不乱。
    #[test]
    fn openai_text_and_thinking_do_not_mix() {
        let (sink, all) = collecting_sink();
        let mut state = TurnState::default();
        for delta in [
            r#"{"reasoning_content":"第一步"}"#,
            r#"{"content":"答案开头"}"#,
            r#"{"reasoning_content":"，再想一步"}"#,
            r#"{"content":"，答案结尾"}"#,
        ] {
            apply_openai(&mut state, &openai_event(delta), &sink).unwrap();
        }
        assert_eq!(state.thinking, "第一步，再想一步");
        assert_eq!(state.text, "答案开头，答案结尾");
        assert_eq!(
            *all.lock().unwrap(),
            vec![
                AiDelta::Thinking("第一步".to_string()),
                AiDelta::Text("答案开头".to_string()),
                AiDelta::Thinking("，再想一步".to_string()),
                AiDelta::Text("，答案结尾".to_string()),
            ]
        );
    }

    /// 普通端点只给 content：不该产生空的 Thinking 增量。
    #[test]
    fn openai_without_reasoning_emits_no_thinking() {
        let (sink, all) = collecting_sink();
        let mut state = TurnState::default();
        apply_openai(&mut state, &openai_event(r#"{"content":"你好"}"#), &sink).unwrap();
        assert_eq!(
            *all.lock().unwrap(),
            vec![AiDelta::Text("你好".to_string())]
        );
        assert_eq!(state.thinking, "");
    }

    // -----------------------------------------------------------------------
    // 文本形态的工具调用（2026-11-07 的真实事故）
    // -----------------------------------------------------------------------

    /// 事故原文：**生产库里那条失败回复的正文，一字不改**（2026-11-07，
    /// `ai_messages.id = 6`）。模型在收尾阶段把 `plan_update` / `plan_submit`
    /// 写成了正文里的标签，用户看到的就是这一坨。
    const INCIDENT_ANSWER: &str = r#"








从搜索结果看，你的邮件涉及较多校园招聘信息。让我读取几封关键邮件的细节来获取更完整的画像。





<tool_call>
<function=plan_update>
<parameter=goal>
检索近期邮件并基于内容生成用户画像
</parameter>
<parameter=motive>
已完成第二步：读取代表性邮件正文
</parameter>
<parameter=steps>
[{"capability": "mail", "evidence": "获取到近30天共95封邮件，读取了星际悦动拒绝通知、中国石油AI面试邀请、提交作业邮件等关键内容", "id": 1, "status": "done", "title": "检索近30天邮件"}, {"capability": "mail", "evidence": "读取了3封代表性邮件：星际悦动校招拒绝（AI应用工程师岗）、中国石油AI面试通知（含考试时间链接）、提交Demo和方案作业（闻凝），以及东软集团职业测评邀请、西藏电信校招等", "id": 2, "status": "done", "title": "读取代表性邮件正文"}]
</parameter>
</function>
</tool_call>
<tool_call>
<function=plan_submit>
<parameter=goal>
检索近期邮件并基于内容生成用户画像
</parameter>
<parameter=motive>
已完成两步操作，现在总结用户画像
</parameter>
<parameter=steps>
[]
</parameter>
</function>
</tool_call>"#;

    /// 事故的复现：正文里出现工具调用标签时，标签**不能**落进用户可见的答复，
    /// 而要被还原成一次真正的工具调用。
    #[test]
    fn text_form_tool_call_is_parsed_not_shown() {
        let (sink, buffer) = sink();
        let mut state = TurnState::default();
        apply_openai(
            &mut state,
            &openai_event(&format!(
                r#"{{"content":{}}}"#,
                serde_json::to_string(INCIDENT_ANSWER).unwrap()
            )),
            &sink,
        )
        .unwrap();
        let tail = state.scanner.flush();
        if !tail.is_empty() {
            state.text.push_str(&tail);
        }
        let turn = state.finish().unwrap();

        // 用户看到的正文里不能有标签。
        assert!(
            !turn.text.contains("tool_call"),
            "工具调用标签漏进了正文：{}",
            turn.text
        );
        assert!(
            !buffer.lock().unwrap().contains("tool_call"),
            "工具调用标签漏进了流式增量"
        );
        // 正文只剩那句过程性叙述（750 字 → 61 字）。
        assert!(
            turn.text
                .contains("从搜索结果看，你的邮件涉及较多校园招聘信息"),
            "叙述要留下：{}",
            turn.text
        );
        assert!(
            turn.text.chars().count() <= 80,
            "剥完只剩叙述，不该有别的东西：{}",
            turn.text
        );

        // 而它必须变成两次真正的工具调用。
        assert_eq!(turn.tool_calls.len(), 2);
        let call = &turn.tool_calls[0];
        assert_eq!(call.name, AiToolName::PlanUpdate);
        assert!(call.parse_ok);
        assert_eq!(call.arguments["goal"], "检索近期邮件并基于内容生成用户画像");
        assert_eq!(call.arguments["motive"], "已完成第二步：读取代表性邮件正文");
        let steps = call.arguments["steps"].as_array().expect("steps 应是数组");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0]["id"], 1);
        assert_eq!(steps[0]["status"], "done");
        assert_eq!(turn.tool_calls[1].name, AiToolName::PlanSubmit);
    }

    /// 标签是**分片**到达的：`<` 与 `tool_call>` 落在两个 delta 里也得认出来。
    #[test]
    fn text_form_tool_call_survives_chunked_deltas() {
        let (sink, buffer) = sink();
        let mut state = TurnState::default();
        let full = INCIDENT_ANSWER;
        // 每次只喂 7 个**字符**：标签、参数名、结束标签全都会被切开。
        // （按字符切，不能按字节切——正文里有中文。）
        let chars: Vec<char> = full.chars().collect();
        let mut at = 0;
        while at < chars.len() {
            let end = (at + 7).min(chars.len());
            let piece: String = chars[at..end].iter().collect();
            at = end;
            apply_openai(
                &mut state,
                &openai_event(&format!(
                    r#"{{"content":{}}}"#,
                    serde_json::to_string(&piece).unwrap()
                )),
                &sink,
            )
            .unwrap();
        }
        let tail = state.scanner.flush();
        if !tail.is_empty() {
            state.text.push_str(&tail);
        }
        let turn = state.finish().unwrap();
        assert!(
            !turn.text.contains("tool_call"),
            "分片到达时标签也不该漏进正文：{}",
            turn.text
        );
        assert!(
            !buffer.lock().unwrap().contains("tool_call"),
            "分片到达时标签漏进了流式增量"
        );
        assert_eq!(turn.tool_calls.len(), 2);
        assert_eq!(turn.tool_calls[0].name, AiToolName::PlanUpdate);
        assert_eq!(turn.tool_calls[1].name, AiToolName::PlanSubmit);
    }

    /// 标签里夹零宽字符（模型把结构化调用「写出来」时的常见花样）也要认。
    #[test]
    fn text_form_tool_call_tolerates_zero_width_chars() {
        let (sink, _) = sink();
        let mut state = TurnState::default();
        // 显式拼出零宽字符：源码里直接写它太容易被人手的编辑器悄悄吃掉。
        let text = "<\u{200B}tool_call>\n<function=plan_submit>\n\
                    <parameter=goal>\n看看我的邮件\n</parameter>\n</function>\n</\u{200B}tool_call>";
        apply_openai(
            &mut state,
            &openai_event(&format!(
                r#"{{"content":{}}}"#,
                serde_json::to_string(&text).unwrap()
            )),
            &sink,
        )
        .unwrap();
        let turn = state.finish().unwrap();
        assert_eq!(turn.text, "");
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].name, AiToolName::PlanSubmit);
        assert_eq!(turn.tool_calls[0].arguments["goal"], "看看我的邮件");
    }

    /// 没收到结束标签的残缺标签不是正文：宁可少一句，也不能让 XML 落到用户眼前。
    #[test]
    fn unfinished_text_tool_call_is_dropped() {
        let (sink, buffer) = sink();
        let mut state = TurnState::default();
        apply_openai(
            &mut state,
            &openai_event(r#"{"content":"<tool_call>\n<function=plan_update>"}"#),
            &sink,
        )
        .unwrap();
        let tail = state.scanner.flush();
        if !tail.is_empty() {
            state.text.push_str(&tail);
        }
        let turn = state.finish().unwrap();
        assert_eq!(turn.text, "", "残缺标签必须被丢掉");
        assert!(!buffer.lock().unwrap().contains("tool_call"));
        assert!(turn.tool_calls.is_empty());
    }

    /// 正文里提到「tool_call」这个词（而不是真的在调工具）不能被误伤。
    #[test]
    fn prose_mentioning_tool_call_survives() {
        let (sink, _) = sink();
        let mut state = TurnState::default();
        let text = "模型可以用 tool_call 这个形式发起调用，我也可以。";
        apply_openai(
            &mut state,
            &openai_event(&format!(
                r#"{{"content":{}}}"#,
                serde_json::to_string(text).unwrap()
            )),
            &sink,
        )
        .unwrap();
        let turn = state.finish().unwrap();
        assert_eq!(turn.text, text);
        assert!(turn.tool_calls.is_empty());
    }

    /// 结构化调用与文本形态调用同时出现时，两条都要认出来。
    #[test]
    fn structured_and_text_form_calls_coexist() {
        let (sink, _) = sink();
        let mut state = TurnState::default();
        apply_openai(
            &mut state,
            &SseEvent {
                name: None,
                data: r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"email_search","arguments":"{\"query\":\"周报\"}"}}]}}]}"#.into(),
            },
            &sink,
        )
        .unwrap();
        apply_openai(
            &mut state,
            &openai_event(&format!(
                r#"{{"content":{}}}"#,
                serde_json::to_string(INCIDENT_ANSWER).unwrap()
            )),
            &sink,
        )
        .unwrap();
        let turn = state.finish().unwrap();
        assert_eq!(turn.tool_calls.len(), 3);
        assert_eq!(turn.tool_calls[0].name, AiToolName::EmailSearch);
        assert_eq!(turn.tool_calls[1].name, AiToolName::PlanUpdate);
        assert_eq!(turn.tool_calls[2].name, AiToolName::PlanSubmit);
        assert!(!turn.text.contains("tool_call"));
    }
}
