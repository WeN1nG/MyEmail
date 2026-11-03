//! OAuth2 协议适配（Google / Microsoft）。
//!
//! 约束：
//! - 本模块是唯一允许向 OAuth 端点发 HTTP 的地方；对外不暴露 `reqwest` 类型。
//! - 令牌端点响应体上限 64 KB；禁止重定向；连接/总超时 20s；固定 User-Agent。
//! - 任何 `Debug` 输出都不得包含 access token / refresh token / authorization code /
//!   device code / PKCE verifier。
//! - `invalid_grant` / 401 映射为 `CoreError::OAuth { reauth_required: true }`；
//!   5xx 与传输错误映射为可重试的 `CoreError::Network`。

use std::fmt;
use std::time::Duration;

use base64::Engine as _;
use futures_util::StreamExt;
use me_core::model::{OAuthClientConfig, OAuthProvider};
use me_core::{CoreError, now_unix};
use rand::RngCore;
use rand::rngs::OsRng;
use reqwest::StatusCode;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MAX_BODY_BYTES: usize = 64 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const USER_AGENT: &str = "MyEmail/0.2";

// ---------------------------------------------------------------------------
// 服务商配置
// ---------------------------------------------------------------------------

/// 服务商端点配置。字段公开，测试与高级设置可覆盖为本地 mock / 自定义端点。
#[derive(Clone)]
pub struct ProviderConfig {
    pub provider: OAuthProvider,
    pub client_id: String,
    /// 公共客户端可省略；Google 桌面客户端通常需要构建期注入。
    pub client_secret: Option<String>,
    pub authorize_url: String,
    pub token_url: String,
    /// `None` = 该服务商不提供设备码授权。
    pub device_code_url: Option<String>,
    pub userinfo_url: String,
    pub scopes: Vec<String>,
    pub extra_authorize_params: Vec<(String, String)>,
}

impl ProviderConfig {
    /// 生产默认端点。`client_id` 由构建期配置或用户提供的客户端注入。
    pub fn for_provider(provider: OAuthProvider, client_id: impl Into<String>) -> Self {
        let (authorize_url, token_url, device_code_url, userinfo_url, extra) = match provider {
            OAuthProvider::Google => (
                "https://accounts.google.com/o/oauth2/v2/auth",
                "https://oauth2.googleapis.com/token",
                None,
                "https://openidconnect.googleapis.com/v1/userinfo",
                vec![
                    ("access_type".to_string(), "offline".to_string()),
                    ("prompt".to_string(), "consent".to_string()),
                ],
            ),
            OAuthProvider::Microsoft => (
                "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
                "https://login.microsoftonline.com/common/oauth2/v2.0/token",
                Some("https://login.microsoftonline.com/common/oauth2/v2.0/devicecode"),
                "https://graph.microsoft.com/oidc/userinfo",
                vec![("response_mode".to_string(), "query".to_string())],
            ),
        };

        Self {
            provider,
            client_id: client_id.into(),
            client_secret: None,
            authorize_url: authorize_url.to_string(),
            token_url: token_url.to_string(),
            device_code_url: device_code_url.map(str::to_string),
            userinfo_url: userinfo_url.to_string(),
            scopes: provider
                .default_scopes()
                .iter()
                .map(|scope| (*scope).to_string())
                .collect(),
            extra_authorize_params: extra,
        }
    }
}

impl fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("provider", &self.provider)
            .field("client_id", &"<redacted>")
            .field("client_secret", &"<redacted>")
            .field("authorize_url", &self.authorize_url)
            .field("token_url", &self.token_url)
            .field("device_code_url", &self.device_code_url)
            .field("userinfo_url", &self.userinfo_url)
            .field("scopes", &self.scopes)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// 构建期客户端注入
// ---------------------------------------------------------------------------

/// 构建期注入的公共客户端配置。
///
/// 通过编译期环境变量注入，不写入仓库：
/// - `MYEMAIL_GOOGLE_CLIENT_ID` / `MYEMAIL_GOOGLE_CLIENT_SECRET`
/// - `MYEMAIL_MICROSOFT_CLIENT_ID` / `MYEMAIL_MICROSOFT_CLIENT_SECRET`
///
/// `client_secret` 只允许构建期注入；高级设置只能覆盖 `client_id` / tenant。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildTimeClient {
    pub client_id: String,
    pub client_secret: Option<String>,
}

fn non_empty(value: Option<&'static str>) -> Option<&'static str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

pub fn build_time_client(provider: OAuthProvider) -> Option<BuildTimeClient> {
    let (client_id, client_secret) = match provider {
        OAuthProvider::Google => (
            non_empty(option_env!("MYEMAIL_GOOGLE_CLIENT_ID")),
            non_empty(option_env!("MYEMAIL_GOOGLE_CLIENT_SECRET")),
        ),
        OAuthProvider::Microsoft => (
            non_empty(option_env!("MYEMAIL_MICROSOFT_CLIENT_ID")),
            non_empty(option_env!("MYEMAIL_MICROSOFT_CLIENT_SECRET")),
        ),
    };
    Some(BuildTimeClient {
        client_id: client_id?.to_string(),
        client_secret: client_secret.map(str::to_string),
    })
}

impl ProviderConfig {
    /// 从构建期环境变量创建配置；未注入时返回 `None`。
    pub fn from_build_time(provider: OAuthProvider) -> Option<Self> {
        let client = build_time_client(provider)?;
        let mut config = Self::for_provider(provider, client.client_id);
        config.client_secret = client.client_secret;
        Some(config)
    }

    /// 应用高级设置覆盖：只允许 `client_id` 与 Microsoft tenant。
    pub fn apply_override(&mut self, config: &OAuthClientConfig) -> Result<(), CoreError> {
        let client_id = config.client_id.trim();
        if !client_id.is_empty() {
            if client_id.len() > 512 {
                return Err(CoreError::oauth(
                    None::<String>,
                    "OAuth client_id 过长",
                    false,
                ));
            }
            // 构建期 client_secret 只属于构建期 client_id；覆盖为其他客户端时不能继续使用。
            if client_id != self.client_id {
                self.client_secret = None;
            }
            self.client_id = client_id.to_string();
        }

        if let Some(tenant) = config
            .tenant
            .as_deref()
            .map(str::trim)
            .filter(|tenant| !tenant.is_empty())
        {
            if self.provider != OAuthProvider::Microsoft {
                return Err(CoreError::oauth(
                    None::<String>,
                    "只有 Microsoft 支持 tenant 覆盖",
                    false,
                ));
            }
            if tenant.len() > 128
                || !tenant
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
            {
                return Err(CoreError::oauth(
                    None::<String>,
                    "Microsoft tenant 含非法字符",
                    false,
                ));
            }
            self.replace_microsoft_tenant(tenant);
        }

        if self.client_id.trim().is_empty() {
            return Err(CoreError::oauth(
                None::<String>,
                "未配置 OAuth client_id（构建期或高级设置）",
                false,
            ));
        }
        Ok(())
    }

    fn replace_microsoft_tenant(&mut self, tenant: &str) {
        let replace = |url: &mut String| {
            *url = url.replace("/common/", &format!("/{tenant}/"));
        };
        replace(&mut self.authorize_url);
        replace(&mut self.token_url);
        if let Some(url) = &mut self.device_code_url {
            replace(url);
        }
    }
}

// ---------------------------------------------------------------------------
// PKCE
// ---------------------------------------------------------------------------

/// PKCE S256 参数。`Debug` 只显示 challenge。
#[derive(Clone, PartialEq, Eq)]
pub struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    pub fn generate() -> Self {
        // 32 字节 → 43 个 base64url 字符，满足 RFC 7636 的 43～128 长度要求。
        Self::from_verifier(random_urlsafe(32)).expect("generated PKCE verifier is valid")
    }

    pub fn from_verifier(verifier: impl Into<String>) -> Result<Self, CoreError> {
        let verifier = verifier.into();
        let valid_chars = verifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'));
        if !(43..=128).contains(&verifier.len()) || !valid_chars {
            return Err(CoreError::oauth(
                None::<String>,
                "PKCE verifier 必须是 43～128 个 URL-safe 字符",
                false,
            ));
        }
        let digest = Sha256::digest(verifier.as_bytes());
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
        Ok(Self {
            verifier,
            challenge,
        })
    }

    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    pub fn challenge(&self) -> &str {
        &self.challenge
    }
}

impl fmt::Debug for Pkce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pkce")
            .field("verifier", &"<redacted>")
            .field("challenge", &self.challenge)
            .finish()
    }
}

/// 生成 URL-safe、无填充的随机串。`state` 与 PKCE verifier 共用。
pub fn random_urlsafe(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buffer);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buffer)
}

// ---------------------------------------------------------------------------
// 响应类型
// ---------------------------------------------------------------------------

/// 令牌端点返回。`Debug` 脱敏。
#[derive(Clone, PartialEq, Eq)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// unix 秒。
    pub expires_at: i64,
    pub token_type: String,
    pub scope: Option<String>,
    pub id_token: Option<String>,
}

impl TokenResponse {
    pub fn is_expired(&self, now: i64, skew_secs: i64) -> bool {
        self.expires_at > 0 && self.expires_at - now <= skew_secs
    }
}

impl fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenResponse")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("id_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("token_type", &self.token_type)
            .field("scope", &self.scope)
            .finish()
    }
}

/// 设备码授权响应。`Debug` 脱敏设备码，用户码可展示。
#[derive(Clone, PartialEq, Eq)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_at: i64,
    pub interval_secs: u64,
}

impl fmt::Debug for DeviceCodeResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceCodeResponse")
            .field("device_code", &"<redacted>")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("expires_at", &self.expires_at)
            .field("interval_secs", &self.interval_secs)
            .finish()
    }
}

/// 设备码轮询的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceTokenOutcome {
    /// `authorization_pending`：继续按 interval 轮询。
    Pending,
    /// `slow_down`：调用方应把 interval 增加 5 秒。
    SlowDown,
    Token(TokenResponse),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    pub subject: Option<String>,
    pub email: Option<String>,
}

#[derive(Deserialize)]
struct TokenWire {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Deserialize)]
struct DeviceCodeWire {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

fn default_interval() -> u64 {
    5
}

#[derive(Deserialize, Default)]
struct ErrorWire {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Deserialize, Default)]
struct UserInfoWire {
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

// ---------------------------------------------------------------------------
// 客户端
// ---------------------------------------------------------------------------

/// OAuth2 客户端。所有端点 URL 来自 `ProviderConfig`，函数内不硬编码域名。
pub struct OAuthClient {
    http: reqwest::Client,
    config: ProviderConfig,
}

impl OAuthClient {
    pub fn new(config: ProviderConfig) -> Result<Self, CoreError> {
        if config.client_id.trim().is_empty() {
            return Err(CoreError::oauth(
                None::<String>,
                "未配置 OAuth client_id（构建期或高级设置）",
                false,
            ));
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(HTTP_TIMEOUT)
            .connect_timeout(HTTP_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .map_err(|err| {
                CoreError::network(false, format!("OAuth HTTP 客户端初始化失败：{err}"))
            })?;
        Ok(Self { http, config })
    }

    pub fn config(&self) -> &ProviderConfig {
        &self.config
    }

    /// 组装授权 URL。`state` 由调用方生成并单次校验。
    pub fn authorize_url(
        &self,
        redirect_uri: &str,
        state: &str,
        pkce: &Pkce,
    ) -> Result<String, CoreError> {
        let mut url = reqwest::Url::parse(&self.config.authorize_url)
            .map_err(|_| CoreError::oauth(None::<String>, "授权端点 URL 非法", false))?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("response_type", "code")
                .append_pair("client_id", &self.config.client_id)
                .append_pair("redirect_uri", redirect_uri)
                .append_pair("scope", &self.config.scopes.join(" "))
                .append_pair("state", state)
                .append_pair("code_challenge", pkce.challenge())
                .append_pair("code_challenge_method", "S256");
            for (key, value) in &self.config.extra_authorize_params {
                query.append_pair(key, value);
            }
        }
        Ok(url.into())
    }

    pub async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<TokenResponse, CoreError> {
        let mut params: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", &self.config.client_id),
            ("code_verifier", verifier),
        ];
        if let Some(secret) = &self.config.client_secret {
            params.push(("client_secret", secret));
        }
        self.token_request(&params).await
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<TokenResponse, CoreError> {
        let mut params: Vec<(&str, &str)> = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &self.config.client_id),
        ];
        if let Some(secret) = &self.config.client_secret {
            params.push(("client_secret", secret));
        }
        self.token_request(&params).await
    }

    pub async fn request_device_code(&self) -> Result<DeviceCodeResponse, CoreError> {
        let device_url = self.config.device_code_url.as_deref().ok_or_else(|| {
            CoreError::oauth(
                None::<String>,
                format!("{} 不提供设备码授权", self.config.provider.label()),
                false,
            )
        })?;
        let scope = self.config.scopes.join(" ");
        let params: Vec<(&str, &str)> =
            vec![("client_id", &self.config.client_id), ("scope", &scope)];
        let response = self
            .http
            .post(device_url)
            .form(&params)
            .send()
            .await
            .map_err(transport_error)?;
        let (status, body) = read_body(response).await?;
        if !status.is_success() {
            return Err(map_error(status, &body));
        }
        let wire: DeviceCodeWire = serde_json::from_slice(&body)
            .map_err(|_| CoreError::oauth(None::<String>, "设备码响应无法解析", false))?;
        Ok(DeviceCodeResponse {
            device_code: wire.device_code,
            user_code: wire.user_code,
            verification_uri: wire.verification_uri,
            verification_uri_complete: wire.verification_uri_complete,
            expires_at: now_unix() + wire.expires_in as i64,
            interval_secs: wire.interval.max(1),
        })
    }

    /// 单次设备码轮询。调用方按 `interval_secs`（`SlowDown` 时 +5s）循环，
    /// 直到 `Token` 或截止时间。
    pub async fn poll_device_token(
        &self,
        device_code: &str,
    ) -> Result<DeviceTokenOutcome, CoreError> {
        let params: Vec<(&str, &str)> = vec![
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code),
            ("client_id", &self.config.client_id),
        ];
        let response = self
            .http
            .post(&self.config.token_url)
            .form(&params)
            .send()
            .await
            .map_err(transport_error)?;
        let (status, body) = read_body(response).await?;
        if status.is_success() {
            return Ok(DeviceTokenOutcome::Token(parse_token(&body)?));
        }
        if status.is_server_error() {
            return Err(CoreError::network(
                true,
                format!("OAuth 令牌端点暂时不可用（HTTP {}）", status.as_u16()),
            ));
        }
        let wire: ErrorWire = serde_json::from_slice(&body).unwrap_or_default();
        match wire.error.as_deref() {
            Some("authorization_pending") => Ok(DeviceTokenOutcome::Pending),
            Some("slow_down") => Ok(DeviceTokenOutcome::SlowDown),
            Some("expired_token") => Err(CoreError::oauth(
                Some("expired_token"),
                "设备码已过期，请重新发起授权",
                true,
            )),
            Some("access_denied") => Err(CoreError::oauth(
                Some("access_denied"),
                "用户取消了授权",
                false,
            )),
            _ => Err(map_error(status, &body)),
        }
    }

    /// 预填邮箱/用户标识。失败不应阻断账户创建流程的其余部分。
    pub async fn fetch_userinfo(&self, access_token: &str) -> Result<UserInfo, CoreError> {
        let response = self
            .http
            .get(&self.config.userinfo_url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(transport_error)?;
        let (status, body) = read_body(response).await?;
        if !status.is_success() {
            return Err(map_error(status, &body));
        }
        let wire: UserInfoWire = serde_json::from_slice(&body)
            .map_err(|_| CoreError::oauth(None::<String>, "用户信息响应无法解析", false))?;
        Ok(UserInfo {
            subject: wire.sub,
            email: wire.email,
        })
    }

    async fn token_request(&self, params: &[(&str, &str)]) -> Result<TokenResponse, CoreError> {
        let response = self
            .http
            .post(&self.config.token_url)
            .form(params)
            .send()
            .await
            .map_err(transport_error)?;
        let (status, body) = read_body(response).await?;
        if status.is_success() {
            parse_token(&body)
        } else {
            Err(map_error(status, &body))
        }
    }
}

impl fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthClient")
            .field("provider", &self.config.provider)
            .field("client_id", &"<redacted>")
            .field("client_secret", &"<redacted>")
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// 内部辅助
// ---------------------------------------------------------------------------

fn parse_token(body: &[u8]) -> Result<TokenResponse, CoreError> {
    let wire: TokenWire = serde_json::from_slice(body)
        .map_err(|_| CoreError::oauth(None::<String>, "OAuth 令牌响应无法解析", false))?;
    if wire.access_token.is_empty() {
        return Err(CoreError::oauth(
            None::<String>,
            "OAuth 令牌响应缺少 access_token",
            false,
        ));
    }
    Ok(TokenResponse {
        access_token: wire.access_token,
        refresh_token: wire.refresh_token,
        expires_at: now_unix() + wire.expires_in.unwrap_or(3600) as i64,
        token_type: wire.token_type.unwrap_or_else(|| "Bearer".to_string()),
        scope: wire.scope,
        id_token: wire.id_token,
    })
}

fn transport_error(err: reqwest::Error) -> CoreError {
    if err.is_timeout() {
        CoreError::network(true, "OAuth 请求超时")
    } else {
        CoreError::network(true, "OAuth 网络请求失败")
    }
}

async fn read_body(response: reqwest::Response) -> Result<(StatusCode, Vec<u8>), CoreError> {
    let status = response.status();
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| CoreError::network(true, "读取 OAuth 响应失败"))?;
        if body.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
            return Err(CoreError::oauth(
                None::<String>,
                "OAuth 响应体超过 64 KB",
                false,
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((status, body))
}

fn map_error(status: StatusCode, body: &[u8]) -> CoreError {
    if status.is_server_error() {
        return CoreError::network(
            true,
            format!("OAuth 令牌端点暂时不可用（HTTP {}）", status.as_u16()),
        );
    }
    let wire: ErrorWire = serde_json::from_slice(body).unwrap_or_default();
    let Some(code) = wire.error else {
        return CoreError::oauth(
            None::<String>,
            format!("授权端点返回 HTTP {}", status.as_u16()),
            false,
        );
    };
    match code.as_str() {
        "invalid_grant" => CoreError::oauth(Some(code), "授权已失效，请重新登录", true),
        "access_denied" => CoreError::oauth(Some(code), "用户取消了授权", false),
        _ => {
            let detail = sanitize_detail(
                wire.error_description
                    .as_deref()
                    .unwrap_or("授权请求被拒绝"),
            );
            CoreError::oauth(Some(code), detail, false)
        }
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

    #[test]
    fn pkce_matches_rfc7636_vector() {
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk").unwrap();
        assert_eq!(
            pkce.challenge(),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert!(!format!("{pkce:?}").contains(pkce.verifier()));
    }

    #[test]
    fn pkce_rejects_invalid_verifier() {
        assert!(Pkce::from_verifier("short").is_err());
        assert!(Pkce::from_verifier("!".repeat(43)).is_err());
    }

    #[test]
    fn token_and_device_debug_are_redacted() {
        let token = TokenResponse {
            access_token: "ya29.secret".into(),
            refresh_token: Some("1//secret".into()),
            expires_at: 1,
            token_type: "Bearer".into(),
            scope: None,
            id_token: Some("id.secret".into()),
        };
        let text = format!("{token:?}");
        assert!(!text.contains("ya29.secret"));
        assert!(!text.contains("1//secret"));
        assert!(!text.contains("id.secret"));

        let device = DeviceCodeResponse {
            device_code: "device-secret".into(),
            user_code: "ABCD-1234".into(),
            verification_uri: "https://example.com/device".into(),
            verification_uri_complete: None,
            expires_at: 900,
            interval_secs: 5,
        };
        let text = format!("{device:?}");
        assert!(!text.contains("device-secret"));
        assert!(text.contains("ABCD-1234"));
    }

    #[test]
    fn authorize_url_contains_pkce_and_provider_params() {
        let client = OAuthClient::new(ProviderConfig::for_provider(
            OAuthProvider::Google,
            "client-id",
        ))
        .unwrap();
        let pkce = Pkce::from_verifier("a".repeat(43)).unwrap();
        let url = client
            .authorize_url("http://127.0.0.1:1234/callback", "state-1", &pkce)
            .unwrap();
        let parsed = reqwest::Url::parse(&url).unwrap();
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            pairs.get("client_id").map(String::as_str),
            Some("client-id")
        );
        assert_eq!(
            pairs.get("code_challenge").map(String::as_str),
            Some(pkce.challenge())
        );
        assert_eq!(
            pairs.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(pairs.get("state").map(String::as_str), Some("state-1"));
        assert_eq!(
            pairs.get("access_type").map(String::as_str),
            Some("offline")
        );
        assert!(!url.contains(pkce.verifier()));
    }

    #[test]
    fn provider_config_uses_default_endpoints() {
        let google = ProviderConfig::for_provider(OAuthProvider::Google, "id");
        assert!(google.device_code_url.is_none());
        assert!(google.authorize_url.starts_with("https://"));
        let microsoft = ProviderConfig::for_provider(OAuthProvider::Microsoft, "id");
        assert!(microsoft.device_code_url.is_some());
        assert!(
            microsoft
                .scopes
                .iter()
                .any(|scope| scope.contains("IMAP.AccessAsUser.All"))
        );
    }

    #[test]
    fn advanced_override_sets_client_id_and_microsoft_tenant() {
        let mut config = ProviderConfig::for_provider(OAuthProvider::Microsoft, "default-id");
        config
            .apply_override(&OAuthClientConfig {
                client_id: "override-id".into(),
                tenant: Some("contoso.onmicrosoft.com".into()),
            })
            .expect("override");
        assert_eq!(config.client_id, "override-id");
        assert!(config.authorize_url.contains("/contoso.onmicrosoft.com/"));
        assert!(config.token_url.contains("/contoso.onmicrosoft.com/"));
        assert!(!config.authorize_url.contains("/common/"));
        assert!(
            config
                .device_code_url
                .as_deref()
                .unwrap()
                .contains("/contoso.onmicrosoft.com/")
        );
    }

    #[test]
    fn overriding_client_id_drops_build_time_secret() {
        let mut config = ProviderConfig::for_provider(OAuthProvider::Google, "default-id");
        config.client_secret = Some("default-secret".into());
        config
            .apply_override(&OAuthClientConfig {
                client_id: "other-id".into(),
                tenant: None,
            })
            .unwrap();
        assert_eq!(config.client_id, "other-id");
        assert_eq!(config.client_secret, None);
    }

    #[test]
    fn advanced_override_rejects_tenant_for_google_and_bad_chars() {
        let mut google = ProviderConfig::for_provider(OAuthProvider::Google, "id");
        assert!(
            google
                .apply_override(&OAuthClientConfig {
                    client_id: String::new(),
                    tenant: Some("contoso".into()),
                })
                .is_err()
        );

        let mut microsoft = ProviderConfig::for_provider(OAuthProvider::Microsoft, "id");
        assert!(
            microsoft
                .apply_override(&OAuthClientConfig {
                    client_id: String::new(),
                    tenant: Some("bad/tenant".into()),
                })
                .is_err()
        );
    }

    #[test]
    fn build_time_client_is_optional_and_never_empty() {
        if let Some(client) = build_time_client(OAuthProvider::Google) {
            assert!(!client.client_id.trim().is_empty());
        }
    }
}
