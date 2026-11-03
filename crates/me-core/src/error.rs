//! 分层错误模型。
//!
//! - `me-core` / `me-proto` / `me-store` 使用本枚举向上传播，保留 `source`。
//! - `me-sync` 把 `CoreError` 翻译成 `SendFailureKind` / 退避决策。
//! - `me-app` 只在顶层用 `anyhow` 兜底。
//!
//! 约束：错误消息不得包含正文与凭证；协议文本须先脱敏。

use std::error::Error;
use std::fmt;

/// 上游错误（boxed，避免在 `me-core` 引入任何协议/存储类型）。
pub type BoxError = Box<dyn Error + Send + Sync + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    /// 单文件附件超过 25 MB。
    AttachmentFile,
    /// 单封附件总量超过 25 MB。
    AttachmentTotal,
    /// 单封邮件解析超过大小上限。
    ParseSize,
}

impl LimitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LimitKind::AttachmentFile => "附件单文件超过 25 MB",
            LimitKind::AttachmentTotal => "附件总量超过 25 MB",
            LimitKind::ParseSize => "邮件体积超过解析上限",
        }
    }
}

impl fmt::Display for LimitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("网络错误: {detail}")]
    Network {
        retryable: bool,
        detail: String,
        #[source]
        source: Option<BoxError>,
    },

    /// 已脱敏的协议错误。`code` 为 IMAP 状态标签或 SMTP 数字码的字符串形式。
    #[error("协议错误{}: {detail}", code.as_ref().map(|c| format!(" [{c}]")).unwrap_or_default())]
    Protocol {
        code: Option<String>,
        detail: String,
    },

    #[error("认证失败: {detail}")]
    Auth { detail: String },

    /// OAuth2 令牌/授权失败。`error_code` 只允许 OAuth 错误码（如 `invalid_grant`），
    /// 不得包含 token、authorization code 或响应体原文。
    #[error("授权失败{}: {detail}", error_code.as_ref().map(|c| format!(" [{c}]")).unwrap_or_default())]
    OAuth {
        error_code: Option<String>,
        detail: String,
        reauth_required: bool,
    },

    #[error("存储错误: {detail}")]
    Storage { detail: String },

    #[error("解析错误: {detail}")]
    Parse { detail: String },

    #[error("超出限制: {what}")]
    Limit { what: LimitKind },

    /// 用户输入或本地规则校验失败。`message` 可直接展示给用户。
    #[error("校验失败: {message}")]
    Validation { message: String },

    #[error("操作已取消")]
    Cancelled,
}

impl CoreError {
    pub fn network(retryable: bool, detail: impl Into<String>) -> Self {
        CoreError::Network {
            retryable,
            detail: detail.into(),
            source: None,
        }
    }

    pub fn network_with_source(
        retryable: bool,
        detail: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        CoreError::Network {
            retryable,
            detail: detail.into(),
            source: Some(Box::new(source)),
        }
    }

    pub fn protocol(code: Option<impl Into<String>>, detail: impl Into<String>) -> Self {
        CoreError::Protocol {
            code: code.map(Into::into),
            detail: detail.into(),
        }
    }

    pub fn auth(detail: impl Into<String>) -> Self {
        CoreError::Auth {
            detail: detail.into(),
        }
    }

    pub fn oauth(
        error_code: Option<impl Into<String>>,
        detail: impl Into<String>,
        reauth_required: bool,
    ) -> Self {
        CoreError::OAuth {
            error_code: error_code.map(Into::into),
            detail: detail.into(),
            reauth_required,
        }
    }

    /// OAuth 错误是否要求用户重新授权。
    pub fn needs_reauth(&self) -> bool {
        matches!(
            self,
            CoreError::OAuth {
                reauth_required: true,
                ..
            }
        )
    }

    pub fn storage(detail: impl Into<String>) -> Self {
        CoreError::Storage {
            detail: detail.into(),
        }
    }

    pub fn parse(detail: impl Into<String>) -> Self {
        CoreError::Parse {
            detail: detail.into(),
        }
    }

    /// 是否是「资源不存在」类的协议错误（HTTP 404 / IMAP `NONEXISTENT`）。
    ///
    /// 用于需要区分「路径不对」与「地点不对」的场景：
    /// 和风天气的城市搜索在统一 Host 下用 `/geo/v2/city/lookup`、
    /// 在旧公共 Host 下用 `/v2/city/lookup`，404 意味着该换路径而不是地点失效。
    pub fn is_not_found(&self) -> bool {
        match self {
            CoreError::Protocol { code, .. } => code.as_deref().is_some_and(|code| code == "404"),
            _ => false,
        }
    }

    pub fn limit(what: LimitKind) -> Self {
        CoreError::Limit { what }
    }

    /// 校验失败：`message` 会原样展示给用户，因此必须是面向用户的文案。
    pub fn validation(message: impl Into<String>) -> Self {
        CoreError::Validation {
            message: message.into(),
        }
    }

    pub fn cancelled() -> Self {
        CoreError::Cancelled
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self, CoreError::Cancelled)
    }

    /// 是否值得原样重试。认证失败、解析失败、超限与取消都不重试。
    pub fn is_retryable(&self) -> bool {
        match self {
            CoreError::Network { retryable, .. } => *retryable,
            CoreError::Protocol { detail, .. } => {
                // 仅把明确的临时性协议错误视为可重试。
                let lower = detail.to_ascii_lowercase();
                lower.contains("try again")
                    || lower.contains("temporarily")
                    || lower.contains("rate limit")
                    || lower.contains("too many")
                    || lower.contains("bye")
            }
            CoreError::Auth { .. }
            | CoreError::OAuth { .. }
            | CoreError::Storage { .. }
            | CoreError::Parse { .. }
            | CoreError::Validation { .. }
            | CoreError::Limit { .. }
            | CoreError::Cancelled => false,
        }
    }

    /// 面向用户的一句话说明（不含敏感信息）。
    pub fn user_message(&self) -> String {
        match self {
            CoreError::Network { retryable, .. } => {
                if *retryable {
                    "网络连接失败，请稍后重试".to_string()
                } else {
                    "网络不可达".to_string()
                }
            }
            CoreError::Protocol { detail, .. } => format!("服务器返回错误：{detail}"),
            CoreError::Auth { .. } => "认证失败，请检查用户名与授权码".to_string(),
            CoreError::OAuth {
                reauth_required, ..
            } => {
                if *reauth_required {
                    "授权已失效，请重新登录".to_string()
                } else {
                    "授权失败，请稍后重试".to_string()
                }
            }
            CoreError::Storage { .. } => "本地数据库操作失败".to_string(),
            CoreError::Parse { .. } => "邮件内容无法解析".to_string(),
            CoreError::Validation { message } => message.clone(),
            CoreError::Limit { what } => what.to_string(),
            CoreError::Cancelled => "操作已取消".to_string(),
        }
    }

    pub fn scope_is_retryable(&self) -> bool {
        self.is_retryable()
    }
}

impl From<std::io::Error> for CoreError {
    fn from(value: std::io::Error) -> Self {
        let retryable = matches!(
            value.kind(),
            std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::Interrupted
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
        );
        CoreError::network_with_source(retryable, value.to_string(), value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryability() {
        assert!(CoreError::network(true, "timeout").is_retryable());
        assert!(!CoreError::network(false, "dns").is_retryable());
        assert!(!CoreError::auth("bad password").is_retryable());
        assert!(CoreError::protocol(None::<String>, "server too many connections").is_retryable());
        assert!(!CoreError::protocol(None::<String>, "no such mailbox").is_retryable());
    }

    #[test]
    fn user_messages_are_safe() {
        let err = CoreError::auth("user=alice@example.com password=hunter2");
        // `user_message` 不转发 detail，避免凭证进入 UI/日志。
        assert!(!err.user_message().contains("hunter2"));
        assert!(!err.user_message().contains("alice"));
    }
}
