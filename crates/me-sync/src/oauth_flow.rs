//! OAuth 授权流程：回环重定向、设备码、账户创建与重新授权。
//!
//! 安全约束：
//! - 回环监听只绑定 `127.0.0.1:0`；`state` 单次、常量时间比较；5 分钟超时。
//! - 令牌只留在内存 / keyring；事件与日志不包含令牌、授权码或设备码。
//! - 浏览器打开经 `BrowserOpener` 注入，`me-sync` 不直接调用平台 API。
//! - 流程结束（成功、失败、取消）都必须从 registry 清理。

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use me_core::port::{ImapSessionFactory, SmtpSender, StorePort};
use me_core::{
    Account, AccountDraft, AccountId, AuthMethod, CoreError, Event, OAuthFlowMode, OAuthProvider,
    now_unix,
};
use me_proto::oauth::{DeviceTokenOutcome, OAuthClient, Pkce, TokenResponse, random_urlsafe};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use url::form_urlencoded;

use crate::SyncService;

const LOOPBACK_TIMEOUT_SECS: u64 = 300;

/// 正在进行的 OAuth 流程。`std::sync::Mutex` 只在同步代码中短暂持有。
#[derive(Default)]
pub struct OAuthFlowRegistry {
    next: AtomicU64,
    active: Mutex<HashMap<u64, CancellationToken>>,
}

impl OAuthFlowRegistry {
    fn allocate(&self) -> (u64, CancellationToken) {
        let flow_id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let token = CancellationToken::new();
        self.active
            .lock()
            .expect("oauth flows poisoned")
            .insert(flow_id, token.clone());
        (flow_id, token)
    }

    pub fn cancel(&self, flow_id: u64) -> bool {
        if let Some(token) = self
            .active
            .lock()
            .expect("oauth flows poisoned")
            .get(&flow_id)
        {
            token.cancel();
            true
        } else {
            false
        }
    }

    pub fn cancel_all(&self) {
        for token in self.active.lock().expect("oauth flows poisoned").values() {
            token.cancel();
        }
    }

    fn finish(&self, flow_id: u64) {
        self.active
            .lock()
            .expect("oauth flows poisoned")
            .remove(&flow_id);
    }
}

impl<S, F, M> SyncService<S, F, M>
where
    S: StorePort + Clone,
    F: ImapSessionFactory + 'static,
    M: SmtpSender + 'static,
{
    pub(crate) async fn handle_start_oauth(
        &self,
        provider: OAuthProvider,
        mode: OAuthFlowMode,
        account: Option<AccountId>,
        email_hint: Option<String>,
    ) {
        let (flow_id, token) = self.inner.oauth_flows.allocate();
        let result = self
            .run_oauth_flow(flow_id, token.clone(), provider, mode, account, email_hint)
            .await;
        self.inner.oauth_flows.finish(flow_id);

        match result {
            Ok((account, created, keyring_ok)) => {
                if !keyring_ok {
                    self.emit(Event::CredentialNotPersisted {
                        account: account.id,
                        created,
                    })
                    .await;
                }
                self.emit(Event::OAuthCompleted {
                    flow_id,
                    account: account.clone(),
                    created,
                })
                .await;
                self.sync_account_task(account.id, None);
                self.start_watcher(account.id);
            }
            Err(err) => {
                let message = if token.is_cancelled() {
                    "授权已取消".to_string()
                } else {
                    err.user_message()
                };
                self.emit(Event::OAuthFailed {
                    flow_id,
                    message,
                    reauth_required: err.needs_reauth(),
                })
                .await;
            }
        }
    }

    async fn run_oauth_flow(
        &self,
        flow_id: u64,
        token: CancellationToken,
        provider: OAuthProvider,
        mode: OAuthFlowMode,
        account_id: Option<AccountId>,
        email_hint: Option<String>,
    ) -> Result<(Account, bool, bool), CoreError> {
        let existing = match account_id {
            Some(id) => Some(self.account(id).await?),
            None => None,
        };
        if let Some(account) = &existing
            && let Some(existing_provider) = account.oauth_provider
            && existing_provider != provider
        {
            return Err(CoreError::oauth(
                None::<String>,
                "账户的 OAuth 服务商与重新授权请求不一致",
                false,
            ));
        }

        let config = self.inner.tokens.provider_config(provider).ok_or_else(|| {
            CoreError::oauth(
                None::<String>,
                format!("未配置 {} OAuth 客户端", provider.label()),
                false,
            )
        })?;
        let client = OAuthClient::new(config.clone())?;

        let token_response = match mode {
            OAuthFlowMode::Loopback => self.loopback_flow(flow_id, &token, &client).await?,
            OAuthFlowMode::DeviceCode => self.device_code_flow(flow_id, &token, &client).await?,
        };

        let userinfo = client
            .fetch_userinfo(&token_response.access_token)
            .await
            .ok();
        let email = userinfo
            .as_ref()
            .and_then(|info| info.email.clone())
            .or(email_hint)
            .or_else(|| existing.as_ref().map(|account| account.email.clone()))
            .filter(|email| !email.trim().is_empty())
            .ok_or_else(|| {
                CoreError::oauth(
                    None::<String>,
                    "无法获取账户邮箱地址；请在登录窗口中填写邮箱",
                    false,
                )
            })?;
        let subject = userinfo.and_then(|info| info.subject);
        let scopes = config.scopes.clone();
        if token.is_cancelled() {
            return Err(CoreError::cancelled());
        }

        match existing {
            Some(account) => {
                let keyring_ok = self
                    .inner
                    .tokens
                    .store_oauth(&account, provider, subject.clone(), scopes, &token_response)
                    .await?;
                let id = account.id;
                let subject_for_db = subject.clone();
                self.db(move |s| {
                    s.set_account_oauth_identity(id, provider, subject_for_db.as_deref())?;
                    s.set_account_reauth(id, false)?;
                    s.set_account_token_expiry(id, Some(token_response.expires_at))
                })
                .await?;
                let updated = self.account(account.id).await?;
                Ok((updated, false, keyring_ok))
            }
            None => {
                let draft = AccountDraft {
                    email: email.clone(),
                    display_name: None,
                    imap: provider.default_imap(),
                    smtp: provider.default_smtp(),
                    auth_user: email,
                    auth_secret: None,
                    auth_method: AuthMethod::OAuth2,
                    oauth_provider: Some(provider),
                    oauth_subject: subject.clone(),
                    plaintext_tls_ack: false,
                };
                let account = self.db(move |s| s.create_account(&draft)).await?;
                let keyring_ok = match self
                    .inner
                    .tokens
                    .store_oauth(&account, provider, subject, scopes, &token_response)
                    .await
                {
                    Ok(value) => value,
                    Err(err) => {
                        let id = account.id;
                        let _ = self.db(move |s| s.delete_account(id, true)).await;
                        return Err(err);
                    }
                };
                let id = account.id;
                let expires_at = token_response.expires_at;
                self.db(move |s| {
                    s.set_account_reauth(id, false)?;
                    s.set_account_token_expiry(id, Some(expires_at))
                })
                .await?;
                let created = self.account(id).await?;
                Ok((created, true, keyring_ok))
            }
        }
    }

    async fn loopback_flow(
        &self,
        flow_id: u64,
        token: &CancellationToken,
        client: &OAuthClient,
    ) -> Result<TokenResponse, CoreError> {
        let pkce = Pkce::generate();
        let state = random_urlsafe(32);
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|err| CoreError::network(false, format!("无法启动授权回调监听：{err}")))?;
        let port = listener
            .local_addr()
            .map_err(|err| CoreError::network(false, format!("无法读取回调端口：{err}")))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");
        let authorize_url = client.authorize_url(&redirect_uri, &state, &pkce)?;

        self.emit(Event::OAuthFlowStarted {
            flow_id,
            provider: client.config().provider,
            verification_uri: authorize_url.clone(),
            user_code: None,
            expires_at: now_unix() + LOOPBACK_TIMEOUT_SECS as i64,
        })
        .await;
        if let Err(err) = self.inner.browser.open(&authorize_url) {
            tracing::warn!(error = %err.user_message(), "failed to open system browser for OAuth");
        }

        let code = self.wait_for_callback(listener, &state, token).await?;
        client
            .exchange_code(&code, &redirect_uri, pkce.verifier())
            .await
    }

    async fn wait_for_callback(
        &self,
        listener: TcpListener,
        expected_state: &str,
        token: &CancellationToken,
    ) -> Result<String, CoreError> {
        let started = std::time::Instant::now();
        loop {
            if token.is_cancelled() {
                return Err(CoreError::cancelled());
            }
            let remaining =
                Duration::from_secs(LOOPBACK_TIMEOUT_SECS).saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(CoreError::oauth(
                    None::<String>,
                    "授权超时，请重新发起登录",
                    false,
                ));
            }
            let accepted = tokio::select! {
                _ = token.cancelled() => return Err(CoreError::cancelled()),
                accepted = tokio::time::timeout(remaining, listener.accept()) => accepted,
            };
            let (mut socket, _) = match accepted {
                Ok(Ok(value)) => value,
                Ok(Err(err)) => {
                    return Err(CoreError::network(
                        false,
                        format!("授权回调监听失败：{err}"),
                    ));
                }
                Err(_) => {
                    return Err(CoreError::oauth(
                        None::<String>,
                        "授权超时，请重新发起登录",
                        false,
                    ));
                }
            };

            let mut buffer = [0u8; 8192];
            let read = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut buffer))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("");
            if !path.starts_with("/callback") {
                respond(&mut socket, 404, "Not Found").await;
                continue;
            }

            let query = path.split_once('?').map(|(_, query)| query).unwrap_or("");
            let mut code = None;
            let mut state = None;
            let mut error = None;
            for (key, value) in form_urlencoded::parse(query.as_bytes()) {
                match key.as_ref() {
                    "code" => code = Some(value.into_owned()),
                    "state" => state = Some(value.into_owned()),
                    "error" => error = Some(value.into_owned()),
                    _ => {}
                }
            }

            if !state
                .as_deref()
                .is_some_and(|state| constant_time_eq(state, expected_state))
            {
                respond(&mut socket, 400, "授权状态校验失败，请重新发起登录。").await;
                return Err(CoreError::oauth(
                    None::<String>,
                    "授权 state 校验失败",
                    false,
                ));
            }
            if let Some(error) = error {
                respond(&mut socket, 400, "授权被拒绝或已取消。").await;
                let reauth = error == "invalid_grant";
                return Err(CoreError::oauth(Some(error), "用户取消了授权", reauth));
            }
            let Some(code) = code else {
                respond(&mut socket, 400, "授权回调缺少 code。").await;
                return Err(CoreError::oauth(None::<String>, "授权回调缺少 code", false));
            };
            respond(&mut socket, 200, "授权完成，可以关闭此窗口并返回 MyEmail。").await;
            return Ok(code);
        }
    }

    async fn device_code_flow(
        &self,
        flow_id: u64,
        token: &CancellationToken,
        client: &OAuthClient,
    ) -> Result<TokenResponse, CoreError> {
        let device = client.request_device_code().await?;
        let verification_uri = device
            .verification_uri_complete
            .clone()
            .unwrap_or_else(|| device.verification_uri.clone());
        self.emit(Event::OAuthFlowStarted {
            flow_id,
            provider: client.config().provider,
            verification_uri: verification_uri.clone(),
            user_code: Some(device.user_code.clone()),
            expires_at: device.expires_at,
        })
        .await;
        if let Err(err) = self.inner.browser.open(&verification_uri) {
            tracing::warn!(error = %err.user_message(), "failed to open system browser for device code");
        }

        let mut interval = device.interval_secs.max(1);
        loop {
            if token.is_cancelled() {
                return Err(CoreError::cancelled());
            }
            if now_unix() >= device.expires_at {
                return Err(CoreError::oauth(
                    Some("expired_token"),
                    "设备码已过期，请重新发起授权",
                    true,
                ));
            }
            tokio::select! {
                _ = token.cancelled() => return Err(CoreError::cancelled()),
                _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
            }
            match client.poll_device_token(&device.device_code).await? {
                DeviceTokenOutcome::Pending => {}
                DeviceTokenOutcome::SlowDown => interval = interval.saturating_add(5),
                DeviceTokenOutcome::Token(token) => return Ok(token),
            }
        }
    }
}

async fn respond(socket: &mut tokio::net::TcpStream, status: u16, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>MyEmail</title>\
         <body style=\"font-family:sans-serif;text-align:center;padding-top:80px\">\
         <h2>MyEmail</h2><p>{message}</p></body>"
    );
    let reason = if status == 200 { "OK" } else { "Bad Request" };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_allocates_and_cancels_all() {
        let registry = OAuthFlowRegistry::default();
        let (first_id, first) = registry.allocate();
        let (second_id, second) = registry.allocate();
        assert_ne!(first_id, second_id);
        registry.cancel_all();
        assert!(first.is_cancelled());
        assert!(second.is_cancelled());
        registry.finish(first_id);
        registry.finish(second_id);
    }

    #[test]
    fn constant_time_compare_matches_exact_only() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
    }
}
