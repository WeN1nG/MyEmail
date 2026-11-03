//! 凭证解析与 OAuth 令牌管理。
//!
//! 约束：
//! - 访问令牌只存在于进程内存；刷新令牌只存在于系统密钥库（或会话内存降级）。
//! - 刷新令牌的 keyring 值只存 JSON 元数据 + refresh_token，不存 access token。
//! - 同一账户的刷新由 per-account 异步锁单飞；并发请求只发一次刷新。
//! - 任何日志/错误都不能包含令牌原文；邮箱按 `mask_email` 脱敏。

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use me_core::model::{AuthMethod, OAuthProvider, mask_email};
use me_core::{Account, AccountId, AuthCredential, CoreError, now_unix};
use me_proto::oauth::{OAuthClient, ProviderConfig, TokenResponse};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;

pub const KEYRING_SERVICE: &str = "MyEmail";

/// keyring 中的 OAuth 值。`Debug` 脱敏刷新令牌与邮箱。
#[derive(Clone, Serialize, Deserialize)]
struct StoredRefreshToken {
    v: u32,
    provider: String,
    #[serde(default)]
    subject: Option<String>,
    email: String,
    #[serde(default)]
    scopes: Vec<String>,
    refresh_token: String,
    obtained_at: i64,
}

impl fmt::Debug for StoredRefreshToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredRefreshToken")
            .field("v", &self.v)
            .field("provider", &self.provider)
            .field("subject", &self.subject)
            .field("email", &mask_email(&self.email))
            .field("scopes", &self.scopes)
            .field("refresh_token", &"<redacted>")
            .field("obtained_at", &self.obtained_at)
            .finish()
    }
}

/// 进程内凭证缓存 + OAuth 刷新。
#[derive(Default)]
pub struct TokenManager {
    cached: Mutex<HashMap<AccountId, AuthCredential>>,
    /// keyring 不可用时的刷新令牌降级存储；退出即失效。
    session_oauth: Mutex<HashMap<AccountId, StoredRefreshToken>>,
    oauth_configs: Mutex<HashMap<OAuthProvider, ProviderConfig>>,
    refresh_locks: Mutex<HashMap<AccountId, Arc<AsyncMutex<()>>>>,
}

impl TokenManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cached(&self, account: AccountId) -> Option<AuthCredential> {
        self.cached
            .lock()
            .expect("token cache poisoned")
            .get(&account)
            .cloned()
    }

    pub fn set_session(&self, account: AccountId, credential: AuthCredential) {
        self.cached
            .lock()
            .expect("token cache poisoned")
            .insert(account, credential);
    }

    pub fn remove(&self, account: AccountId) {
        self.cached
            .lock()
            .expect("token cache poisoned")
            .remove(&account);
        self.session_oauth
            .lock()
            .expect("oauth session cache poisoned")
            .remove(&account);
    }

    /// 注入服务商客户端配置（client_id / client_secret / 端点）。
    pub fn configure_provider(&self, config: ProviderConfig) {
        self.oauth_configs
            .lock()
            .expect("oauth configs poisoned")
            .insert(config.provider, config);
    }

    pub fn provider_config(&self, provider: OAuthProvider) -> Option<ProviderConfig> {
        self.oauth_configs
            .lock()
            .expect("oauth configs poisoned")
            .get(&provider)
            .cloned()
    }

    /// keyring 不可用或测试时的会话级刷新令牌降级路径。
    pub fn set_session_oauth_refresh(
        &self,
        account: AccountId,
        provider: OAuthProvider,
        email: impl Into<String>,
        refresh_token: impl Into<String>,
    ) {
        let record = StoredRefreshToken {
            v: 1,
            provider: provider.as_str().to_string(),
            subject: None,
            email: email.into(),
            scopes: provider
                .default_scopes()
                .iter()
                .map(|scope| (*scope).to_string())
                .collect(),
            refresh_token: refresh_token.into(),
            obtained_at: now_unix(),
        };
        self.session_oauth
            .lock()
            .expect("oauth session cache poisoned")
            .insert(account, record);
        // 强制下次 resolve 走刷新。
        self.cached
            .lock()
            .expect("token cache poisoned")
            .remove(&account);
    }

    /// 解析账户当前可用的凭证。
    ///
    /// 密码/授权码账户：内存缓存 → keyring。
    /// OAuth2 账户：内存 access token（未临近过期）→ 单飞刷新。
    pub async fn resolve(&self, account: &Account) -> Result<AuthCredential, CoreError> {
        if let Some(credential) = self.cached_valid(account.id) {
            return Ok(credential);
        }

        match account.auth_method {
            AuthMethod::Password | AuthMethod::AppPassword => {
                let email = account.email.clone();
                let masked = mask_email(&email);
                let secret = tokio::task::spawn_blocking(move || {
                    let entry = keyring::Entry::new(KEYRING_SERVICE, &email)
                        .map_err(|err| CoreError::auth(format!("无法访问系统密钥库：{err}")))?;
                    entry
                        .get_password()
                        .map_err(|err| CoreError::auth(format!("无法读取凭证：{err}")))
                })
                .await
                .map_err(|join| CoreError::storage(format!("密钥库任务失败：{join}")))??;

                tracing::debug!(account = %masked, "loaded password credential from keyring");
                let credential = AuthCredential::password(secret);
                self.set_session(account.id, credential.clone());
                Ok(credential)
            }
            AuthMethod::OAuth2 => {
                let lock = self.refresh_lock(account.id);
                let _guard = lock.lock().await;
                // 等待锁期间可能已有任务完成刷新。
                if let Some(credential) = self.cached_valid(account.id) {
                    return Ok(credential);
                }
                self.refresh_oauth(account).await
            }
        }
    }

    fn cached_valid(&self, account: AccountId) -> Option<AuthCredential> {
        let credential = self.cached(account)?;
        // 提前 60 秒刷新，避免请求途中过期。
        if credential.expires_within(now_unix(), 60) {
            None
        } else {
            Some(credential)
        }
    }

    fn refresh_lock(&self, account: AccountId) -> Arc<AsyncMutex<()>> {
        self.refresh_locks
            .lock()
            .expect("refresh locks poisoned")
            .entry(account)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    async fn refresh_oauth(&self, account: &Account) -> Result<AuthCredential, CoreError> {
        let provider = account
            .oauth_provider
            .ok_or_else(|| CoreError::oauth(None::<String>, "OAuth2 账户缺少服务商", true))?;
        let config = self
            .oauth_configs
            .lock()
            .expect("oauth configs poisoned")
            .get(&provider)
            .cloned()
            .ok_or_else(|| {
                CoreError::oauth(
                    None::<String>,
                    format!("未配置 {} OAuth 客户端", provider.label()),
                    false,
                )
            })?;

        let session_record = self
            .session_oauth
            .lock()
            .expect("oauth session cache poisoned")
            .get(&account.id)
            .cloned();
        let record = match session_record {
            Some(record) => record,
            None => self.read_keyring_record(account).await?,
        };

        if record.provider != provider.as_str() {
            return Err(CoreError::oauth(
                None::<String>,
                "OAuth 服务商与已保存的刷新令牌不匹配，请重新授权",
                true,
            ));
        }

        let client = OAuthClient::new(config)?;
        let token = match client.refresh(&record.refresh_token).await {
            Ok(token) => token,
            Err(err) => return Err(err),
        };
        let credential = AuthCredential::oauth2(
            record.email.clone(),
            token.access_token.clone(),
            token.expires_at,
        );
        self.set_session(account.id, credential.clone());

        if let Some(new_refresh) = token.refresh_token {
            let updated = StoredRefreshToken {
                refresh_token: new_refresh,
                obtained_at: now_unix(),
                ..record
            };
            self.session_oauth
                .lock()
                .expect("oauth session cache poisoned")
                .insert(account.id, updated.clone());
            self.write_keyring_record(account, &updated).await;
        }

        Ok(credential)
    }

    /// 保存密码凭证：先写会话内存；返回是否成功写入系统密钥库。
    pub async fn store_password(&self, account: &Account, secret: &str) -> bool {
        self.set_session(account.id, AuthCredential::password(secret));
        // 密码账户不应保留旧的 OAuth 刷新令牌。
        self.session_oauth
            .lock()
            .expect("oauth session cache poisoned")
            .remove(&account.id);

        let email = account.email.clone();
        let secret = secret.to_string();
        let result = tokio::task::spawn_blocking(move || {
            let entry =
                keyring::Entry::new(KEYRING_SERVICE, &email).map_err(|err| err.to_string())?;
            entry.set_password(&secret).map_err(|err| err.to_string())
        })
        .await;
        match result {
            Ok(Ok(())) => true,
            Ok(Err(err)) => {
                tracing::warn!(account = %mask_email(&account.email), %err, "keyring unavailable; credential kept in session memory only");
                false
            }
            Err(join) => {
                tracing::warn!(%join, "keyring task failed; credential kept in session memory only");
                false
            }
        }
    }

    /// OAuth 授权成功后保存令牌。返回是否成功写入 keyring；失败时会话内存仍可用。
    pub async fn store_oauth(
        &self,
        account: &Account,
        provider: OAuthProvider,
        subject: Option<String>,
        scopes: Vec<String>,
        token: &TokenResponse,
    ) -> Result<bool, CoreError> {
        let refresh_token = token.refresh_token.clone().ok_or_else(|| {
            CoreError::oauth(None::<String>, "服务商未返回刷新令牌，请重新授权", true)
        })?;
        let record = StoredRefreshToken {
            v: 1,
            provider: provider.as_str().to_string(),
            subject,
            email: account.email.clone(),
            scopes,
            refresh_token,
            obtained_at: now_unix(),
        };
        self.set_session(
            account.id,
            AuthCredential::oauth2(&account.email, &token.access_token, token.expires_at),
        );
        self.session_oauth
            .lock()
            .expect("oauth session cache poisoned")
            .insert(account.id, record.clone());
        Ok(self.write_keyring_record(account, &record).await)
    }

    async fn read_keyring_record(
        &self,
        account: &Account,
    ) -> Result<StoredRefreshToken, CoreError> {
        let email = account.email.clone();
        let value = tokio::task::spawn_blocking(move || {
            let entry = keyring::Entry::new(KEYRING_SERVICE, &email)
                .map_err(|err| format!("无法访问系统密钥库：{err}"))?;
            match entry.get_password() {
                Ok(value) => Ok(Some(value)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(err) => Err(format!("无法读取刷新令牌：{err}")),
            }
        })
        .await
        .map_err(|join| CoreError::storage(format!("密钥库任务失败：{join}")))?;

        let value = value.map_err(|detail| CoreError::oauth(None::<String>, detail, true))?;
        let Some(value) = value else {
            return Err(CoreError::oauth(
                None::<String>,
                "缺少 OAuth 刷新令牌，请重新授权",
                true,
            ));
        };
        serde_json::from_str::<StoredRefreshToken>(&value).map_err(|_| {
            CoreError::oauth(
                None::<String>,
                "已保存的 OAuth 凭证无法解析，请重新授权",
                true,
            )
        })
    }

    /// 写入失败只记日志：会话内仍可继续使用。
    async fn write_keyring_record(&self, account: &Account, record: &StoredRefreshToken) -> bool {
        let email = account.email.clone();
        let json = match serde_json::to_string(record) {
            Ok(json) => json,
            Err(err) => {
                tracing::warn!(%err, "failed to serialize OAuth refresh token");
                return false;
            }
        };
        let result = tokio::task::spawn_blocking(move || {
            let entry =
                keyring::Entry::new(KEYRING_SERVICE, &email).map_err(|err| err.to_string())?;
            entry.set_password(&json).map_err(|err| err.to_string())
        })
        .await;
        match result {
            Ok(Ok(())) => true,
            Ok(Err(err)) => {
                tracing::warn!(account = %mask_email(&account.email), %err, "keyring unavailable; refresh token kept in session memory only");
                false
            }
            Err(join) => {
                tracing::warn!(%join, "keyring task failed; refresh token kept in session memory only");
                false
            }
        }
    }

    pub async fn delete(&self, account: &Account) {
        self.remove(account.id);
        let email = account.email.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &email) {
                let _ = entry.delete_credential();
            }
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_core::model::{Endpoint, TlsMode};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn account(method: AuthMethod, provider: Option<OAuthProvider>) -> Account {
        Account {
            id: AccountId(1),
            email: "alice@example.com".to_string(),
            display_name: None,
            imap: Endpoint::new("imap.example.com", 993, TlsMode::Implicit),
            smtp: Endpoint::new("smtp.example.com", 465, TlsMode::Implicit),
            auth_user: "alice@example.com".to_string(),
            auth_method: method,
            oauth_provider: provider,
            oauth_subject: None,
            needs_reauth: false,
            idle_enabled: true,
            poll_interval_secs: None,
            token_expires_at: None,
        }
    }

    async fn token_server(status: u16, body: &'static str) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let reason = if (200..300).contains(&status) {
                "OK"
            } else {
                "Error"
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        addr
    }

    fn provider_config(addr: std::net::SocketAddr) -> ProviderConfig {
        let mut config = ProviderConfig::for_provider(OAuthProvider::Google, "client-id");
        config.token_url = format!("http://{addr}/token");
        config
    }

    #[test]
    fn session_credentials_are_cached_by_account() {
        let tokens = TokenManager::new();
        tokens.set_session(AccountId(1), AuthCredential::password("secret"));
        let cached = tokens.cached(AccountId(1)).expect("cached");
        assert!(matches!(cached, AuthCredential::Password { .. }));
        tokens.remove(AccountId(1));
        assert!(tokens.cached(AccountId(1)).is_none());
    }

    #[tokio::test]
    async fn oauth_without_provider_config_is_a_config_error() {
        let tokens = TokenManager::new();
        let err = tokens
            .resolve(&account(AuthMethod::OAuth2, Some(OAuthProvider::Google)))
            .await
            .expect_err("oauth client is not configured");
        // 缺少客户端配置不是用户授权失效，不应把账户标记为 needs_reauth。
        assert!(!err.needs_reauth());
        assert!(!err.user_message().contains("token"));
    }

    #[tokio::test]
    async fn oauth_session_credential_is_used_before_refresh() {
        let tokens = TokenManager::new();
        tokens.set_session(
            AccountId(1),
            AuthCredential::oauth2("alice@example.com", "ya29.secret", now_unix() + 3600),
        );
        let credential = tokens
            .resolve(&account(AuthMethod::OAuth2, Some(OAuthProvider::Google)))
            .await
            .unwrap();
        assert!(credential.is_oauth());
        assert!(!format!("{credential:?}").contains("ya29.secret"));
    }

    #[tokio::test]
    async fn oauth_refresh_uses_session_refresh_token_and_caches() {
        let addr = token_server(
            200,
            r#"{"access_token":"access-refreshed","expires_in":3600,"token_type":"Bearer"}"#,
        )
        .await;
        let tokens = TokenManager::new();
        tokens.configure_provider(provider_config(addr));
        tokens.set_session_oauth_refresh(
            AccountId(1),
            OAuthProvider::Google,
            "alice@example.com",
            "refresh-1",
        );

        let first = tokens
            .resolve(&account(AuthMethod::OAuth2, Some(OAuthProvider::Google)))
            .await
            .expect("refresh");
        match &first {
            AuthCredential::OAuth2 {
                access_token,
                expires_at,
                ..
            } => {
                assert_eq!(access_token, "access-refreshed");
                assert!(*expires_at > now_unix());
            }
            _ => panic!("expected oauth credential"),
        }

        // 服务端只接受一次连接；第二次 resolve 必须命中缓存。
        let second = tokens
            .resolve(&account(AuthMethod::OAuth2, Some(OAuthProvider::Google)))
            .await
            .expect("cached");
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn invalid_grant_marks_reauth() {
        let addr = token_server(400, r#"{"error":"invalid_grant"}"#).await;
        let tokens = TokenManager::new();
        tokens.configure_provider(provider_config(addr));
        tokens.set_session_oauth_refresh(
            AccountId(1),
            OAuthProvider::Google,
            "alice@example.com",
            "refresh-bad",
        );
        let err = tokens
            .resolve(&account(AuthMethod::OAuth2, Some(OAuthProvider::Google)))
            .await
            .expect_err("must fail");
        assert!(err.needs_reauth());
    }

    #[tokio::test]
    async fn missing_provider_config_is_not_reauth() {
        let tokens = TokenManager::new();
        tokens.set_session_oauth_refresh(
            AccountId(1),
            OAuthProvider::Google,
            "alice@example.com",
            "refresh-1",
        );
        let err = tokens
            .resolve(&account(AuthMethod::OAuth2, Some(OAuthProvider::Google)))
            .await
            .expect_err("must fail");
        assert!(!err.needs_reauth());
    }

    #[test]
    fn stored_refresh_token_debug_is_redacted() {
        let record = StoredRefreshToken {
            v: 1,
            provider: "google".into(),
            subject: Some("sub".into()),
            email: "alice@example.com".into(),
            scopes: vec![],
            refresh_token: "1//secret".into(),
            obtained_at: 0,
        };
        let text = format!("{record:?}");
        assert!(!text.contains("1//secret"));
        assert!(!text.contains("alice@example.com"));
    }
}
