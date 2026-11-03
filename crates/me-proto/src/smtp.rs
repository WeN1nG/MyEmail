//! SMTP 投递适配。单次发送、不重试、不排队。

use std::future::Future;
use std::str::FromStr;
use std::time::Duration;

use lettre::address::Envelope;
use lettre::transport::smtp::Error as SmtpError;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use me_core::CoreError;
use me_core::model::{Account, AuthCredential, OutgoingMessage, TlsMode};
use me_core::port::SmtpSender;

use crate::mime::build_rfc822;

const SEND_TIMEOUT: Duration = Duration::from_secs(120);

/// SMTP 发送器。
#[derive(Debug, Clone, Copy, Default)]
pub struct ProtoSmtp;

impl ProtoSmtp {
    pub fn new() -> Self {
        Self
    }
}

impl SmtpSender for ProtoSmtp {
    fn send(
        &self,
        account: &Account,
        credential: &AuthCredential,
        message: &OutgoingMessage,
    ) -> impl Future<Output = Result<(), CoreError>> + Send {
        send_smtp(account, credential, message)
    }
}

async fn send_smtp(
    account: &Account,
    credential: &AuthCredential,
    message: &OutgoingMessage,
) -> Result<(), CoreError> {
    let raw = build_rfc822(message)?;
    let envelope = build_envelope(message)?;
    let transport = build_transport(account, credential)?;

    match tokio::time::timeout(SEND_TIMEOUT, transport.send_raw(&envelope, &raw)).await {
        Ok(Ok(_response)) => Ok(()),
        Ok(Err(err)) => Err(map_smtp_error(err)),
        Err(_) => Err(CoreError::network(true, "SMTP 发送超时")),
    }
}

/// 连接测试：打开一次 SMTP 会话并认证，不发送邮件。
pub async fn verify(account: &Account, credential: &AuthCredential) -> Result<(), CoreError> {
    let transport = build_transport(account, credential)?;
    match tokio::time::timeout(Duration::from_secs(20), transport.test_connection()).await {
        Ok(Ok(true)) => Ok(()),
        Ok(Ok(false)) => Err(CoreError::network(false, "SMTP 服务器拒绝连接")),
        Ok(Err(err)) => Err(map_smtp_error(err)),
        Err(_) => Err(CoreError::network(true, "SMTP 连接超时")),
    }
}

fn build_transport(
    account: &Account,
    credential: &AuthCredential,
) -> Result<AsyncSmtpTransport<Tokio1Executor>, CoreError> {
    let tls = match account.smtp.tls {
        TlsMode::Implicit => Tls::Wrapper(tls_parameters(&account.smtp.host)?),
        TlsMode::StartTls => Tls::Required(tls_parameters(&account.smtp.host)?),
        TlsMode::None => Tls::None,
    };

    // 密码账户保持 lettre 默认机制（PLAIN/LOGIN 等），避免破坏只支持 LOGIN 的服务器；
    // OAuth2 账户显式限定 XOAUTH2。
    let (user, secret, oauth) = match credential {
        AuthCredential::Password { secret } => (account.auth_user.clone(), secret.clone(), false),
        AuthCredential::OAuth2 {
            email,
            access_token,
            ..
        } => (email.clone(), access_token.clone(), true),
    };

    let mut builder =
        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(account.smtp.host.clone())
            .port(account.smtp.port)
            .tls(tls);
    if oauth {
        builder = builder.authentication(vec![Mechanism::Xoauth2]);
    }

    Ok(builder
        .credentials(Credentials::new(user, secret))
        .build::<Tokio1Executor>())
}

fn tls_parameters(host: &str) -> Result<TlsParameters, CoreError> {
    TlsParameters::new(host.to_string())
        .map_err(|err| CoreError::network(false, format!("SMTP TLS 配置失败: {err}")))
}

fn build_envelope(message: &OutgoingMessage) -> Result<Envelope, CoreError> {
    let from = lettre::Address::from_str(message.from.addr.trim())
        .map_err(|_| CoreError::parse("发件人地址无效"))?;

    let mut recipients = Vec::new();
    for address in message.all_recipients() {
        let parsed = lettre::Address::from_str(address.addr.trim())
            .map_err(|_| CoreError::parse("收件人地址无效"))?;
        recipients.push(parsed);
    }

    Envelope::new(Some(from), recipients).map_err(|_| CoreError::parse("收件人为空"))
}

fn map_smtp_error(err: SmtpError) -> CoreError {
    let status = err.status().map(|code| code.to_string());
    let transient = err.is_transient() || err.is_timeout();
    let permanent = err.is_permanent();
    let tls = err.is_tls();
    let detail = sanitize_detail(&err.to_string());

    match status.as_deref() {
        Some("530") | Some("534") | Some("535") | Some("538") => {
            CoreError::auth("SMTP 认证失败，请检查用户名与授权码")
        }
        Some(code) => CoreError::protocol(Some(code.to_string()), detail),
        None if transient => CoreError::network_with_source(true, "SMTP 网络错误", err),
        None if tls => CoreError::network_with_source(false, "SMTP TLS 握手失败", err),
        None if permanent => CoreError::protocol(None::<String>, detail),
        None => CoreError::network_with_source(true, "SMTP 连接失败", err),
    }
}

fn sanitize_detail(input: &str) -> String {
    input
        .chars()
        .map(|ch| if ch == '\r' || ch == '\n' { ' ' } else { ch })
        .take(200)
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_core::model::{AccountId, Address, AuthMethod, Endpoint};

    fn account() -> Account {
        Account {
            id: AccountId(1),
            email: "alice@example.com".to_string(),
            display_name: Some("Alice".to_string()),
            imap: Endpoint::new("imap.example.com", 993, TlsMode::Implicit),
            smtp: Endpoint::new("smtp.example.com", 465, TlsMode::Implicit),
            auth_user: "alice@example.com".to_string(),
            auth_method: AuthMethod::AppPassword,
            oauth_provider: None,
            oauth_subject: None,
            needs_reauth: false,
            idle_enabled: true,
            poll_interval_secs: None,
            token_expires_at: None,
        }
    }

    fn outgoing(to: Vec<Address>) -> OutgoingMessage {
        OutgoingMessage {
            account: AccountId(1),
            draft_id: None,
            message_id: "fixed@example.com".to_string(),
            from: Address::new(Some("Alice".to_string()), "alice@example.com"),
            to,
            cc: vec![],
            bcc: vec![Address::bare("hidden@example.com")],
            subject: "Hello".to_string(),
            body_text: "body".to_string(),
            attachments: vec![],
        }
    }

    #[test]
    fn envelope_includes_bcc_but_message_does_not() {
        let message = outgoing(vec![Address::bare("bob@example.com")]);
        let envelope = build_envelope(&message).unwrap();
        assert_eq!(envelope.to().len(), 2);
        assert_eq!(
            envelope.from().map(|a| a.to_string()),
            Some("alice@example.com".to_string())
        );

        let raw = String::from_utf8(build_rfc822(&message).unwrap()).unwrap();
        assert!(!raw.contains("\r\nBcc:"));
    }

    #[test]
    fn empty_recipients_is_error() {
        let mut message = outgoing(vec![]);
        message.bcc.clear();
        assert!(build_envelope(&message).is_err());
    }

    #[test]
    fn credentials_never_appear_in_debug() {
        let credentials = Credentials::new("alice".to_string(), "top-secret".to_string());
        assert!(!format!("{credentials:?}").contains("top-secret"));
    }

    #[test]
    fn account_endpoint_is_plain_data() {
        let account = account();
        assert_eq!(account.smtp.port, 465);
        assert!(account.smtp.tls == TlsMode::Implicit);
    }
}
