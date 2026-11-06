//! Server-Sent Events 增量解析。
//!
//! 只实现 `text/event-stream` 里真正用得到的部分：`event:` / `data:` / 注释行 /
//! 空行分发。不引入 SSE 库（架构文档 §2.6 明确不新增依赖）。
//!
//! **按字节缓冲、按行切分**：一个事件经常被 TCP 拆成两半，甚至拆在
//! 一个中文字符的中间。这里先攒字节，遇到 `\n` 才解码——`\n` 是 ASCII，
//! 不可能出现在 UTF-8 多字节序列的中间，所以按完整行解码不会切坏字符。

use me_core::{AiErrorKind, CoreError};

/// 单行上限（含字段名与值）。
pub const MAX_LINE_BYTES: usize = 256 * 1024;
/// 整个流的上限。
pub const MAX_TOTAL_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    /// `event:` 字段；缺省时按 `message` 处理（Anthropic 有，OpenAI 没有）。
    pub name: Option<String>,
    /// 多行 `data:` 用 `\n` 拼接后的内容。
    pub data: String,
}

impl SseEvent {
    pub fn is_done(&self) -> bool {
        self.data.trim() == "[DONE]"
    }
}

pub struct SseDecoder {
    buffer: Vec<u8>,
    data: String,
    name: Option<String>,
    total: usize,
    max_line: usize,
    max_total: usize,
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::with_limits(MAX_LINE_BYTES, MAX_TOTAL_BYTES)
    }

    pub fn with_limits(max_line: usize, max_total: usize) -> Self {
        Self {
            buffer: Vec::new(),
            data: String::new(),
            name: None,
            total: 0,
            max_line,
            max_total,
        }
    }

    /// 喂一段字节，返回这次能完整解析出的事件（可能 0 个或多个）。
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, CoreError> {
        self.total = self.total.saturating_add(chunk.len());
        if self.total > self.max_total {
            return Err(CoreError::ai(
                AiErrorKind::Protocol,
                None::<String>,
                "AI 响应超过大小上限",
            ));
        }
        self.buffer.extend_from_slice(chunk);

        let mut events = Vec::new();
        loop {
            let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') else {
                if self.buffer.len() > self.max_line {
                    return Err(CoreError::ai(
                        AiErrorKind::Protocol,
                        None::<String>,
                        "AI 响应单行超过上限",
                    ));
                }
                break;
            };
            if position > self.max_line {
                return Err(CoreError::ai(
                    AiErrorKind::Protocol,
                    None::<String>,
                    "AI 响应单行超过上限",
                ));
            }
            let mut line: Vec<u8> = self.buffer.drain(..=position).collect();
            line.pop(); // '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            // 完整行：UTF-8 边界安全。
            let text = String::from_utf8_lossy(&line).into_owned();
            if let Some(event) = self.line(&text) {
                events.push(event);
            }
        }
        Ok(events)
    }

    /// 流结束时把残留的 `data:` 交出去（服务商没发最后一个空行时）。
    pub fn finish(&mut self) -> Option<SseEvent> {
        if self.buffer.len() > self.max_line {
            self.buffer.clear();
            self.data.clear();
            self.name = None;
            return None;
        }
        let text = String::from_utf8_lossy(&self.buffer).into_owned();
        self.buffer.clear();
        // 残留内容按普通行处理；通常为空。
        if !text.is_empty() {
            let _ = self.line(&text);
        }
        self.dispatch()
    }

    fn line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None; // 注释 / 心跳
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "data" => {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(value);
            }
            "event" => self.name = Some(value.to_string()),
            // id / retry 与本次交互无关。
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        if self.data.is_empty() && self.name.is_none() {
            return None;
        }
        let event = SseEvent {
            name: self.name.take(),
            data: std::mem::take(&mut self.data),
        };
        if event.data.is_empty() && event.name.is_none() {
            return None;
        }
        Some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_data_line() {
        let mut decoder = SseDecoder::new();
        let events = decoder.push(b"data: {\"a\":1}\n\n").expect("解析成功");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"a\":1}");
        assert!(events[0].name.is_none());
    }

    #[test]
    fn keeps_event_name() {
        let mut decoder = SseDecoder::new();
        let events = decoder
            .push(b"event: content_block_delta\ndata: {}\n\n")
            .expect("解析成功");
        assert_eq!(events[0].name.as_deref(), Some("content_block_delta"));
    }

    #[test]
    fn joins_multiline_data() {
        let mut decoder = SseDecoder::new();
        let events = decoder.push(b"data: a\ndata: b\n\n").expect("解析成功");
        assert_eq!(events[0].data, "a\nb");
    }

    #[test]
    fn handles_crlf_and_comments() {
        let mut decoder = SseDecoder::new();
        let events = decoder
            .push(b": keep-alive\r\ndata: x\r\n\r\n")
            .expect("解析成功");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "x");
    }

    #[test]
    fn event_split_across_chunks_is_reassembled() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"data: {\"te").expect("ok").is_empty());
        assert!(decoder.push(b"xt\":\"hi\"}").expect("ok").is_empty());
        let events = decoder.push(b"\n\n").expect("ok");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"text\":\"hi\"}");
    }

    #[test]
    fn multibyte_char_split_across_chunks() {
        let payload = "data: 中文\n\n".as_bytes();
        // 故意从「中」的中间切开。
        let split = payload.len() - 5;
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(&payload[..split]).expect("ok").is_empty());
        let events = decoder.push(&payload[split..]).expect("ok");
        assert_eq!(events[0].data, "中文");
    }

    #[test]
    fn detects_done_sentinel() {
        let mut decoder = SseDecoder::new();
        let events = decoder.push(b"data: [DONE]\n\n").expect("ok");
        assert!(events[0].is_done());
    }

    #[test]
    fn single_line_limit_is_enforced() {
        let mut decoder = SseDecoder::with_limits(16, 1024);
        let err = decoder
            .push(b"data: 0123456789012345678901234567890")
            .unwrap_err();
        assert!(matches!(
            err,
            CoreError::Ai {
                kind: AiErrorKind::Protocol,
                ..
            }
        ));
    }

    #[test]
    fn total_limit_is_enforced() {
        let mut decoder = SseDecoder::with_limits(64, 32);
        let _ = decoder.push(b"data: a\n\n").expect("ok");
        let err = decoder.push(&[b'x'; 40]).unwrap_err();
        assert!(matches!(
            err,
            CoreError::Ai {
                kind: AiErrorKind::Protocol,
                ..
            }
        ));
    }

    #[test]
    fn finish_flushes_pending_data() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"data: tail").expect("ok").is_empty());
        let event = decoder.finish().expect("有残留事件");
        assert_eq!(event.data, "tail");
    }
}
