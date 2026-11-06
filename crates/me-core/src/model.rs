//! 领域模型与纯规则。
//!
//! 约束：本模块不做 IO、不依赖 tokio / rusqlite / 任何协议库。
//! 所有跨层转换（IMAP 标志、时间、附件上限）只在这里实现一处。

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, LimitKind};

// ---------------------------------------------------------------------------
// 标识
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AccountId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FolderId(pub i64);

/// 本地主键，对应 `messages.id`，**不是**服务器 UID。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MessageId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DraftId(pub i64);

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "account#{}", self.0)
    }
}
impl fmt::Display for FolderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "folder#{}", self.0)
    }
}
impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "message#{}", self.0)
    }
}
impl fmt::Display for DraftId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "draft#{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// 账户与端点
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    Implicit,
    StartTls,
    /// 明文。默认不可选，需 `AccountDraft::plaintext_tls_ack` 明确确认。
    None,
}

impl TlsMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TlsMode::Implicit => "implicit",
            TlsMode::StartTls => "starttls",
            TlsMode::None => "none",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "implicit" | "ssl" | "tls" => Some(TlsMode::Implicit),
            "starttls" => Some(TlsMode::StartTls),
            "none" | "plain" => Some(TlsMode::None),
            _ => None,
        }
    }

    pub fn is_plaintext(self) -> bool {
        matches!(self, TlsMode::None)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub tls: TlsMode,
}

impl Endpoint {
    pub fn new(host: impl Into<String>, port: u16, tls: TlsMode) -> Self {
        Self {
            host: host.into(),
            port,
            tls,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    Password,
    AppPassword,
    /// OAuth2（XOAUTH2）。令牌不入库，见 `AuthCredential`。
    OAuth2,
}

impl AuthMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMethod::Password => "password",
            AuthMethod::AppPassword => "app_password",
            AuthMethod::OAuth2 => "oauth2",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "app_password" | "app-password" | "apppassword" => AuthMethod::AppPassword,
            "oauth2" | "oauth" => AuthMethod::OAuth2,
            _ => AuthMethod::Password,
        }
    }

    pub fn is_oauth(self) -> bool {
        matches!(self, AuthMethod::OAuth2)
    }
}

/// v0.2 预置的 OAuth2 服务商。只支持这两家，不做通用注册。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OAuthProvider {
    Google,
    Microsoft,
}

impl OAuthProvider {
    pub const ALL: [OAuthProvider; 2] = [OAuthProvider::Google, OAuthProvider::Microsoft];

    pub fn as_str(self) -> &'static str {
        match self {
            OAuthProvider::Google => "google",
            OAuthProvider::Microsoft => "microsoft",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "google" | "gmail" => Some(OAuthProvider::Google),
            "microsoft" | "outlook" | "microsoft365" => Some(OAuthProvider::Microsoft),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            OAuthProvider::Google => "Google",
            OAuthProvider::Microsoft => "Microsoft",
        }
    }

    /// 默认 IMAP 端点（用户可在高级选项覆盖主机/端口，但不能改明文）。
    pub fn default_imap(self) -> Endpoint {
        match self {
            OAuthProvider::Google => Endpoint::new("imap.gmail.com", 993, TlsMode::Implicit),
            OAuthProvider::Microsoft => {
                Endpoint::new("outlook.office365.com", 993, TlsMode::Implicit)
            }
        }
    }

    pub fn default_smtp(self) -> Endpoint {
        match self {
            OAuthProvider::Google => Endpoint::new("smtp.gmail.com", 465, TlsMode::Implicit),
            OAuthProvider::Microsoft => Endpoint::new("smtp.office365.com", 587, TlsMode::StartTls),
        }
    }

    pub fn userinfo_url(self) -> &'static str {
        match self {
            OAuthProvider::Google => "https://openidconnect.googleapis.com/v1/userinfo",
            OAuthProvider::Microsoft => "https://graph.microsoft.com/oidc/userinfo",
        }
    }

    /// 没有标准撤销端点时返回 `None`，删除账户只删本地令牌。
    pub fn revoke_url(self) -> Option<&'static str> {
        match self {
            OAuthProvider::Google => Some("https://oauth2.googleapis.com/revoke"),
            OAuthProvider::Microsoft => None,
        }
    }

    /// Gmail 设备码授权视客户端类型而定，v0.2 默认不提供。
    pub fn supports_device_code(self) -> bool {
        matches!(self, OAuthProvider::Microsoft)
    }

    pub fn default_scopes(self) -> &'static [&'static str] {
        match self {
            OAuthProvider::Google => &["openid", "email", "https://mail.google.com/"],
            OAuthProvider::Microsoft => &[
                "openid",
                "profile",
                "email",
                "offline_access",
                "https://outlook.office.com/IMAP.AccessAsUser.All",
                "https://outlook.office.com/SMTP.Send",
            ],
        }
    }
}

/// 高级设置中的 OAuth 客户端覆盖。`client_secret` 不在此结构中，只能构建期注入。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OAuthClientConfig {
    /// 为空表示继续使用构建期默认值。
    #[serde(default)]
    pub client_id: String,
    /// 仅 Microsoft 使用；`None` = `common`。
    #[serde(default)]
    pub tenant: Option<String>,
}

/// 进程内凭证。**不得**派生 `Debug`/`Serialize`：令牌原文只能存在于
/// 系统密钥库（刷新令牌）或进程内存（访问令牌）。
#[derive(Clone, PartialEq, Eq)]
pub enum AuthCredential {
    Password {
        secret: String,
    },
    OAuth2 {
        email: String,
        access_token: String,
        /// unix 秒；0 表示未知。
        expires_at: i64,
    },
}

impl AuthCredential {
    pub fn password(secret: impl Into<String>) -> Self {
        AuthCredential::Password {
            secret: secret.into(),
        }
    }

    pub fn oauth2(
        email: impl Into<String>,
        access_token: impl Into<String>,
        expires_at: i64,
    ) -> Self {
        AuthCredential::OAuth2 {
            email: email.into(),
            access_token: access_token.into(),
            expires_at,
        }
    }

    pub fn is_oauth(&self) -> bool {
        matches!(self, AuthCredential::OAuth2 { .. })
    }

    /// 剩余有效期是否已不足 `skew_secs`；`expires_at = 0` 视为未知，交给服务端判定。
    pub fn expires_within(&self, now: i64, skew_secs: i64) -> bool {
        match self {
            AuthCredential::Password { .. } => false,
            AuthCredential::OAuth2 { expires_at, .. } => {
                *expires_at > 0 && *expires_at - now <= skew_secs
            }
        }
    }
}

impl fmt::Debug for AuthCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthCredential::Password { .. } => f
                .debug_struct("AuthCredential::Password")
                .field("secret", &"<redacted>")
                .finish(),
            AuthCredential::OAuth2 { expires_at, .. } => f
                .debug_struct("AuthCredential::OAuth2")
                .field("email", &"<redacted>")
                .field("access_token", &"<redacted>")
                .field("expires_at", expires_at)
                .finish(),
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub email: String,
    pub display_name: Option<String>,
    pub imap: Endpoint,
    pub smtp: Endpoint,
    pub auth_user: String,
    pub auth_method: AuthMethod,
    /// OAuth2 账户必须为 `Some`；密码/授权码账户为 `None`。
    #[serde(default)]
    pub oauth_provider: Option<OAuthProvider>,
    /// 服务商稳定用户标识（可选）。
    #[serde(default)]
    pub oauth_subject: Option<String>,
    /// 刷新令牌失效，需要重新授权。
    #[serde(default)]
    pub needs_reauth: bool,
    #[serde(default = "default_true")]
    pub idle_enabled: bool,
    /// `None` = 跟随全局 `meta.poll_interval_secs`。
    #[serde(default)]
    pub poll_interval_secs: Option<u32>,
    /// 仅用于展示/诊断，不参与令牌有效性判断。
    #[serde(default)]
    pub token_expires_at: Option<i64>,
}

impl Account {
    /// 发信时使用的发件地址。显示名可为空。
    pub fn from_address(&self) -> Address {
        Address {
            name: self.display_name.clone(),
            addr: self.email.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// 地址
// ---------------------------------------------------------------------------

/// 序列化为 `{ "name": "...", "addr": "..." }`。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Address {
    pub name: Option<String>,
    pub addr: String,
}

impl Address {
    pub fn new(name: Option<String>, addr: impl Into<String>) -> Self {
        Self {
            name: name.filter(|n| !n.trim().is_empty()),
            addr: addr.into(),
        }
    }

    /// 仅地址部分。
    pub fn bare(addr: impl Into<String>) -> Self {
        Self {
            name: None,
            addr: addr.into(),
        }
    }

    /// `Name <addr>` 或 `addr`。
    pub fn display(&self) -> String {
        match self
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            Some(name) => format!("{name} <{}>", self.addr),
            None => self.addr.clone(),
        }
    }

    /// 日志用：`a***@domain`。
    pub fn masked(&self) -> String {
        mask_email(&self.addr)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

// ---------------------------------------------------------------------------
// 文件夹
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FolderRole {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Junk,
    Archive,
    #[default]
    Other,
}

impl FolderRole {
    pub const ALL: [FolderRole; 7] = [
        FolderRole::Inbox,
        FolderRole::Sent,
        FolderRole::Drafts,
        FolderRole::Trash,
        FolderRole::Junk,
        FolderRole::Archive,
        FolderRole::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            FolderRole::Inbox => "inbox",
            FolderRole::Sent => "sent",
            FolderRole::Drafts => "drafts",
            FolderRole::Trash => "trash",
            FolderRole::Junk => "junk",
            FolderRole::Archive => "archive",
            FolderRole::Other => "other",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "inbox" => FolderRole::Inbox,
            "sent" => FolderRole::Sent,
            "drafts" => FolderRole::Drafts,
            "trash" => FolderRole::Trash,
            "junk" => FolderRole::Junk,
            "archive" => FolderRole::Archive,
            _ => FolderRole::Other,
        }
    }

    /// 用户可读名称。
    pub fn label(self) -> &'static str {
        match self {
            FolderRole::Inbox => "收件箱",
            FolderRole::Sent => "已发送",
            FolderRole::Drafts => "草稿箱",
            FolderRole::Trash => "废件箱",
            FolderRole::Junk => "垃圾邮件",
            FolderRole::Archive => "归档",
            FolderRole::Other => "其他",
        }
    }

    /// 优先使用 `SPECIAL-USE`（RFC 6154）属性；属性为 `\Inbox`/`\Sent` 等。
    /// 无可用属性时退回名称启发式。
    pub fn from_special_use(attributes: &[String], name: &str) -> Self {
        for attr in attributes {
            let normalized = attr.trim().trim_start_matches('\\').to_ascii_lowercase();
            let role = match normalized.as_str() {
                "inbox" => FolderRole::Inbox,
                "sent" | "sentmail" => FolderRole::Sent,
                "drafts" => FolderRole::Drafts,
                "trash" => FolderRole::Trash,
                "junk" | "spam" => FolderRole::Junk,
                "archive" | "all" => FolderRole::Archive,
                _ => FolderRole::Other,
            };
            if role != FolderRole::Other {
                return role;
            }
        }
        folder_role_heuristic(name)
    }
}

/// 中英文名称启发式。只看最后一段（去掉父文件夹前缀）。
pub fn folder_role_heuristic(name: &str) -> FolderRole {
    let leaf = name
        .rsplit(['/', '\\', '|', '.'])
        .next()
        .unwrap_or(name)
        .trim()
        .to_ascii_lowercase();

    if leaf == "inbox" || leaf.contains("收件箱") || leaf == "in" {
        return FolderRole::Inbox;
    }
    if leaf == "sent"
        || leaf == "sent items"
        || leaf == "sent mail"
        || leaf == "sent messages"
        || leaf.contains("已发送")
        || leaf.contains("发件箱")
    {
        return FolderRole::Sent;
    }
    if leaf == "drafts" || leaf == "draft" || leaf.contains("草稿") || leaf.contains("草稿箱")
    {
        return FolderRole::Drafts;
    }
    if leaf == "trash"
        || leaf == "deleted items"
        || leaf == "deleted messages"
        || leaf.contains("废件箱")
        || leaf.contains("已删除")
        || leaf.contains("垃圾箱")
    {
        return FolderRole::Trash;
    }
    if leaf == "junk"
        || leaf == "junk email"
        || leaf == "spam"
        || leaf.contains("垃圾邮件")
        || leaf.contains("广告邮件")
    {
        return FolderRole::Junk;
    }
    if leaf == "archive" || leaf == "archives" || leaf.contains("归档") {
        return FolderRole::Archive;
    }
    FolderRole::Other
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    pub id: FolderId,
    pub account: AccountId,
    /// 解码后的 UTF-8 名称；与服务器通信时再编码为 modified UTF-7。
    pub name: String,
    pub delimiter: Option<char>,
    pub role: FolderRole,
    pub uid_validity: u32,
    pub last_uid: u32,
    /// 服务器已删除、本地待用户确认。
    pub deleted_on_server: bool,
}

// ---------------------------------------------------------------------------
// 标志位
// ---------------------------------------------------------------------------

pub const FLAG_SEEN: u32 = 1 << 0;
pub const FLAG_ANSWERED: u32 = 1 << 1;
pub const FLAG_FLAGGED: u32 = 1 << 2;
pub const FLAG_DELETED: u32 = 1 << 3;
pub const FLAG_DRAFT: u32 = 1 << 4;

/// IMAP 标志的位掩码。`\Recent` 不入库。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Flags(pub u32);

impl Flags {
    pub const NONE: Flags = Flags(0);
    pub const SEEN: Flags = Flags(FLAG_SEEN);
    pub const ANSWERED: Flags = Flags(FLAG_ANSWERED);
    pub const FLAGGED: Flags = Flags(FLAG_FLAGGED);
    pub const DELETED: Flags = Flags(FLAG_DELETED);
    pub const DRAFT: Flags = Flags(FLAG_DRAFT);

    pub fn from_bits(bits: u32) -> Self {
        Flags(bits)
    }

    pub fn bits(self) -> u32 {
        self.0
    }

    /// 解析 IMAP 标志列表（大小写不敏感，忽略 `\Recent` 与自定义标志）。
    pub fn from_imap<I, S>(items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut bits = 0u32;
        for item in items {
            match item
                .as_ref()
                .trim()
                .trim_start_matches('\\')
                .to_ascii_lowercase()
                .as_str()
            {
                "seen" => bits |= FLAG_SEEN,
                "answered" => bits |= FLAG_ANSWERED,
                "flagged" => bits |= FLAG_FLAGGED,
                "deleted" => bits |= FLAG_DELETED,
                "draft" => bits |= FLAG_DRAFT,
                _ => {}
            }
        }
        Flags(bits)
    }

    /// 转回 IMAP 标志列表，用于 `STORE`。
    pub fn to_imap(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.contains(Flags::SEEN) {
            out.push("\\Seen");
        }
        if self.contains(Flags::ANSWERED) {
            out.push("\\Answered");
        }
        if self.contains(Flags::FLAGGED) {
            out.push("\\Flagged");
        }
        if self.contains(Flags::DELETED) {
            out.push("\\Deleted");
        }
        if self.contains(Flags::DRAFT) {
            out.push("\\Draft");
        }
        out
    }

    pub fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn with(self, other: Flags) -> Self {
        Flags(self.0 | other.0)
    }

    pub fn without(self, other: Flags) -> Self {
        Flags(self.0 & !other.0)
    }

    pub fn is_seen(self) -> bool {
        self.contains(Flags::SEEN)
    }

    pub fn is_flagged(self) -> bool {
        self.contains(Flags::FLAGGED)
    }

    pub fn is_deleted(self) -> bool {
        self.contains(Flags::DELETED)
    }

    pub fn is_draft(self) -> bool {
        self.contains(Flags::DRAFT)
    }

    pub fn set_seen(self, seen: bool) -> Self {
        if seen {
            self.with(Flags::SEEN)
        } else {
            self.without(Flags::SEEN)
        }
    }
}

impl std::ops::BitOr for Flags {
    type Output = Flags;
    fn bitor(self, rhs: Flags) -> Flags {
        self.with(rhs)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FlagOp {
    Set(Flags),
    Clear(Flags),
}

impl FlagOp {
    pub fn apply(self, current: Flags) -> Flags {
        match self {
            FlagOp::Set(flags) => current.with(flags),
            FlagOp::Clear(flags) => current.without(flags),
        }
    }

    /// 按开关生成 Set / Clear。
    pub fn toggle(flag: Flags, on: bool) -> Self {
        if on {
            FlagOp::Set(flag)
        } else {
            FlagOp::Clear(flag)
        }
    }
}

// ---------------------------------------------------------------------------
// 邮件
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DateSource {
    #[default]
    Header,
    Internal,
}

impl DateSource {
    pub fn as_str(self) -> &'static str {
        match self {
            DateSource::Header => "header",
            DateSource::Internal => "internal",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "internal" => DateSource::Internal,
            _ => DateSource::Header,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSummary {
    pub id: MessageId,
    pub account: AccountId,
    pub folder: FolderId,
    /// `None` = 本地已移动、服务器身份待确认。
    pub uid: Option<u32>,
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub from: Option<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    /// unix 秒。
    pub date: Option<i64>,
    pub date_source: DateSource,
    pub size: Option<u32>,
    pub flags: Flags,
    /// 补全完成时由正文前 200 字符生成；补全前为 `None`。
    pub snippet: Option<String>,
    pub has_attachment: bool,
    /// ⇔ `body_path.is_some()`。
    pub body_cached: bool,
}

impl MessageSummary {
    pub fn is_unread(&self) -> bool {
        !self.flags.is_seen()
    }

    pub fn display_subject(&self) -> &str {
        self.subject
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("(无主题)")
    }

    pub fn display_from(&self) -> String {
        self.from
            .as_ref()
            .map(Address::display)
            .unwrap_or_else(|| "(未知发件人)".into())
    }
}

/// 日志安全版本：去掉 `snippet`，地址脱敏。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogSafeMessageSummary {
    pub id: MessageId,
    pub account: AccountId,
    pub folder: FolderId,
    pub uid: Option<u32>,
    pub subject: Option<String>,
    pub from: Option<String>,
    pub date: Option<i64>,
    pub flags: Flags,
    pub has_attachment: bool,
    pub body_cached: bool,
}

impl From<&MessageSummary> for LogSafeMessageSummary {
    fn from(m: &MessageSummary) -> Self {
        Self {
            id: m.id,
            account: m.account,
            folder: m.folder,
            uid: m.uid,
            subject: m.subject.clone(),
            from: m.from.as_ref().map(Address::masked),
            date: m.date,
            flags: m.flags,
            has_attachment: m.has_attachment,
            body_cached: m.body_cached,
        }
    }
}

/// 待入库的新邮件摘要（来自 IMAP 头部 + BODYSTRUCTURE）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewMessage {
    pub uid: Option<u32>,
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub from: Option<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub date: Option<i64>,
    pub date_source: DateSource,
    pub size: Option<u32>,
    pub flags: Flags,
    pub has_attachment: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub part_index: u32,
    pub filename: Option<String>,
    pub mime_type: Option<String>,
    pub size: Option<u32>,
    pub is_inline: bool,
    pub content_id: Option<String>,
    /// `None` = 未下载。
    pub local_path: Option<PathBuf>,
}

impl Attachment {
    pub fn display_name(&self) -> String {
        self.filename
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("attachment-{}", self.part_index))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// 内嵌图片（`cid:` / inline 部件），随正文事件送到阅读页。
pub struct InlineImage {
    /// 去掉尖括号后的 Content-ID。
    pub content_id: Option<String>,
    pub filename: Option<String>,
    pub mime_type: Option<String>,
    /// 已由 MIME 解码的图片字节。
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageBody {
    pub message: MessageId,
    /// `text/plain`，或 `html2text` 转换结果。索引与展示共用。
    pub text: String,
    /// 原始 HTML 正文（若邮件含 `text/html` 部件）。仅用于阅读页富文本渲染，
    /// 不进入全文索引；缺省时阅读页回退到 `text`。
    #[serde(default)]
    pub html: Option<String>,
    /// HTML 中 `cid:` 引用的内嵌图片。
    #[serde(default)]
    pub inline_images: Vec<InlineImage>,
    pub attachments: Vec<Attachment>,
    /// `raw/*.eml.gz` 是否存在。
    pub raw_cached: bool,
}

// ---------------------------------------------------------------------------
// 草稿与发信
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DraftStatus {
    Draft,
    Sent,
}

impl DraftStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DraftStatus::Draft => "draft",
            DraftStatus::Sent => "sent",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "sent" => DraftStatus::Sent,
            _ => DraftStatus::Draft,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftSyncState {
    /// 仅本地存在，从未上过服务器。
    #[default]
    LocalOnly,
    /// 本地与服务器基线一致。
    Synced,
    /// 本地有未推送修改。
    LocalDirty,
    /// 双方都改过，已保留两份，等待用户处理。
    Conflict,
    /// 本地已删，待服务器删除确认。
    DeletedLocal,
    /// 服务器已删，本地保留副本待用户决定。
    DeletedServer,
}

impl DraftSyncState {
    pub const ALL: [DraftSyncState; 6] = [
        DraftSyncState::LocalOnly,
        DraftSyncState::Synced,
        DraftSyncState::LocalDirty,
        DraftSyncState::Conflict,
        DraftSyncState::DeletedLocal,
        DraftSyncState::DeletedServer,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            DraftSyncState::LocalOnly => "local_only",
            DraftSyncState::Synced => "synced",
            DraftSyncState::LocalDirty => "local_dirty",
            DraftSyncState::Conflict => "conflict",
            DraftSyncState::DeletedLocal => "deleted_local",
            DraftSyncState::DeletedServer => "deleted_server",
        }
    }

    /// 非法值返回 `None`；`me-store` 写入前用它校验。
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "local_only" => Some(DraftSyncState::LocalOnly),
            "synced" => Some(DraftSyncState::Synced),
            "local_dirty" => Some(DraftSyncState::LocalDirty),
            "conflict" => Some(DraftSyncState::Conflict),
            "deleted_local" => Some(DraftSyncState::DeletedLocal),
            "deleted_server" => Some(DraftSyncState::DeletedServer),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DraftSyncState::LocalOnly => "仅本地",
            DraftSyncState::Synced => "已同步",
            DraftSyncState::LocalDirty => "待同步",
            DraftSyncState::Conflict => "冲突",
            DraftSyncState::DeletedLocal => "待服务器删除",
            DraftSyncState::DeletedServer => "服务器已删除",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    pub id: DraftId,
    pub account: AccountId,
    /// 首次保存生成并固定，重发复用。
    pub message_id: String,
    pub subject: Option<String>,
    pub to: Vec<Address>,
    /// 仅本地保存，不写入 RFC822。
    #[serde(default)]
    pub bcc: Vec<Address>,
    pub status: DraftStatus,
    pub server_uid: Option<u32>,
    /// `None` = 服务器身份尚未建立基线（v1 迁移行或 UIDVALIDITY 重置后）。
    #[serde(default)]
    pub server_uid_validity: Option<u32>,
    #[serde(default)]
    pub sync_state: DraftSyncState,
    /// 冲突副本指向原草稿。
    #[serde(default)]
    pub conflict_of: Option<DraftId>,
    #[serde(default)]
    pub last_synced_at: Option<i64>,
    pub last_error: Option<String>,
    pub updated_at: i64,
}

/// 重新打开草稿时交给 UI 的完整内容。
///
/// 密送地址不写入草稿原文，只保存在 `drafts.bcc_addrs`（本地明文库）；
/// 重新打开时从该列恢复。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftContent {
    pub account: AccountId,
    pub draft_id: DraftId,
    pub message_id: String,
    pub subject: String,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub body_text: String,
    /// 托管在 `drafts/<id>/attachments/` 下的附件路径。
    pub attachments: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingMessage {
    pub account: AccountId,
    pub draft_id: Option<DraftId>,
    /// 重发复用。
    pub message_id: String,
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub subject: String,
    /// UTF-8 `text/plain`。
    pub body_text: String,
    pub attachments: Vec<PathBuf>,
}

impl OutgoingMessage {
    pub fn all_recipients(&self) -> impl Iterator<Item = &Address> {
        self.to.iter().chain(self.cc.iter()).chain(self.bcc.iter())
    }
}

// ---------------------------------------------------------------------------
// 搜索
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SearchQuery {
    pub text: String,
    pub accounts: Vec<AccountId>,
    pub folders: Vec<FolderId>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub has_attachment: Option<bool>,
    /// 头部字段的 AND 过滤（子串匹配）。AI 的 `email_search` 用它把
    /// 「关键词 + 发件人 + 主题」合成一次查询；UI 搜索框留空。
    pub from: Option<String>,
    pub to: Option<String>,
    pub subject: Option<String>,
    /// 只要未读。与其它条件取交集。
    pub unread_only: bool,
    /// 分页大小，后端强制 ≤ 200。
    pub limit: u32,
    /// `(date, id)` 键集游标。
    pub cursor: Option<(i64, i64)>,
}

impl SearchQuery {
    pub fn capped_limit(&self) -> u32 {
        self.limit.clamp(1, crate::MAX_PAGE_SIZE)
    }
}

// ---------------------------------------------------------------------------
// 保存的搜索
// ---------------------------------------------------------------------------

/// 保存的搜索的 id，对应 `saved_searches.id`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SavedSearchId(pub i64);

/// `SearchQuery` 的**持久化子集**——只存条件，不存分页态。
///
/// **不能直接 `serde_json::to_string(&SearchQuery)`**：`SearchQuery` 带
/// `cursor`（键集分页游标）与 `limit`（分页大小），那是**一次查询的状态**，
/// 不是**条件**。存进去会让「保存的搜索」在下次执行时带着上次的翻页位置，
/// 点开只能看到第二页。
///
/// `accounts` / `folders` 是范围条件，**要存**——「只在收件箱搜」是有意义的条件。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SavedSearchQuery {
    /// 自由文本（全文检索）。
    #[serde(default)]
    pub text: String,
    /// 账户范围。空 = 全部。
    #[serde(default)]
    pub accounts: Vec<AccountId>,
    /// 文件夹范围。空 = 全部。
    #[serde(default)]
    pub folders: Vec<FolderId>,
    #[serde(default)]
    pub since: Option<i64>,
    #[serde(default)]
    pub until: Option<i64>,
    #[serde(default)]
    pub has_attachment: Option<bool>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub unread_only: bool,
}

impl SavedSearchQuery {
    /// 从一次实际查询**提取条件**（丢弃分页态）。
    pub fn from_query(query: &SearchQuery) -> Self {
        Self {
            text: query.text.clone(),
            accounts: query.accounts.clone(),
            folders: query.folders.clone(),
            since: query.since,
            until: query.until,
            has_attachment: query.has_attachment,
            from: query.from.clone(),
            to: query.to.clone(),
            subject: query.subject.clone(),
            unread_only: query.unread_only,
        }
    }

    /// 还原成可执行的查询（分页从头开始）。
    pub fn to_query(&self) -> SearchQuery {
        SearchQuery {
            text: self.text.clone(),
            accounts: self.accounts.clone(),
            folders: self.folders.clone(),
            since: self.since,
            until: self.until,
            has_attachment: self.has_attachment,
            from: self.from.clone(),
            to: self.to.clone(),
            subject: self.subject.clone(),
            unread_only: self.unread_only,
            limit: crate::MAX_PAGE_SIZE,
            cursor: None,
        }
    }
}

/// 一条保存的搜索。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSearch {
    pub id: SavedSearchId,
    /// 用户给的名称。**唯一**（`ux_saved_searches_name`）。
    pub name: String,
    /// 条件本身。
    pub query: SavedSearchQuery,
    /// 创建时间，unix 秒。
    pub created_at: i64,
    /// 最近一次执行时间，unix 秒。
    pub last_used_at: i64,
}

/// 保存的搜索的名称长度上限。
pub const MAX_SAVED_SEARCH_NAME: usize = 60;

// ---------------------------------------------------------------------------
// 联系人
// ---------------------------------------------------------------------------

/// 联系人的 id，对应 `contacts.id`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContactId(pub i64);

/// 一个联系人。
///
/// **本地派生数据**：由正文补全落库 / 发信归档时累积，
/// 永不出网、不参与 AI 请求（见 [v04开发指南](doc/versiondoc/v4开发指南.md) §5.3）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contact {
    pub id: ContactId,
    /// 原始地址（保留大小写，用于显示）。
    pub addr: String,
    /// 归一化后的小写地址（唯一性判据）。
    pub normalized: String,
    /// 显示名（从邮件头里取的 `Name <addr>` 的 Name 部分）。
    pub display_name: Option<String>,
    /// 最近一次出现的时间，unix 秒。
    pub last_seen: i64,
    /// 累计出现次数。
    pub seen_count: u32,
}

impl Contact {
    /// 归一化邮箱地址：去空白 + 转小写。
    ///
    /// 邮箱的**本地部分**理论上大小写敏感，但实践中没有这么用的；
    /// 统一小写才能让 `Alice@` 与 `alice@` 落到同一行。
    pub fn normalize(addr: &str) -> String {
        addr.trim().to_lowercase()
    }
}

// ---------------------------------------------------------------------------
// 纯规则
// ---------------------------------------------------------------------------

/// 正文预览长度（字符）。
pub const SNIPPET_CHARS: usize = 200;

/// 单文件附件上限 25 MB。
pub const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;

/// 单封附件总量上限 25 MB。
pub const MAX_ATTACHMENT_TOTAL_BYTES: u64 = 25 * 1024 * 1024;

/// 分页硬上限。
pub const MAX_PAGE_SIZE: u32 = 200;

/// 日志与错误消息中的正文解析上限（字节）。超过直接判为 `Limit(ParseSize)`。
pub const MAX_PARSE_BYTES: usize = 50 * 1024 * 1024;

/// 取正文前 200 字符，压缩空白。
pub fn make_snippet(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.chars().take(SNIPPET_CHARS).collect()
}

/// `Date` 头解析失败或缺失时回退 `INTERNALDATE`。
pub fn effective_date(header: Option<i64>, internal: Option<i64>) -> (Option<i64>, DateSource) {
    match header {
        Some(date) => (Some(date), DateSource::Header),
        None => (internal, DateSource::Internal),
    }
}

/// 附件上限校验：单文件 ≤ 25 MB，单封总量 ≤ 25 MB。
pub fn check_attachment_limits(sizes: &[u64]) -> Result<(), CoreError> {
    let mut total = 0u64;
    for &size in sizes {
        if size > MAX_ATTACHMENT_BYTES {
            return Err(CoreError::Limit {
                what: LimitKind::AttachmentFile,
            });
        }
        total = total.saturating_add(size);
    }
    if total > MAX_ATTACHMENT_TOTAL_BYTES {
        return Err(CoreError::Limit {
            what: LimitKind::AttachmentTotal,
        });
    }
    Ok(())
}

/// `a***@domain`。无法解析时返回 `***`。
pub fn mask_email(email: &str) -> String {
    let email = email.trim();
    match email.rsplit_once('@') {
        Some((local, domain)) if !local.is_empty() && !domain.is_empty() => {
            let head = local.chars().next().unwrap_or('*');
            format!("{head}***@{domain}")
        }
        _ => "***".to_string(),
    }
}

/// 去掉 Message-ID 两侧的 `<`、`>` 与空白，统一入库形式（不含尖括号）。
pub fn normalize_message_id(raw: &str) -> Option<String> {
    let trimmed = raw
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 生成固定 `Message-ID` 的裸值（不含尖括号），格式 `<uuid>@<发信域名>`。
pub fn generate_message_id(from_addr: &str) -> String {
    let domain = from_addr
        .rsplit_once('@')
        .map(|(_, domain)| domain.trim())
        .filter(|domain| !domain.is_empty() && !domain.contains(char::is_whitespace))
        .unwrap_or("myemail.local");
    format!("{}@{}", uuid::Uuid::new_v4(), domain)
}

/// 当前 unix 秒。
pub fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// 本地时区展示：`YYYY-MM-DD HH:MM`。
pub fn format_local_datetime(unix: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(unix, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        _ => "—".to_string(),
    }
}

/// 在列表/事件中按 `(date DESC, id DESC)` 排序。
pub fn sort_newest_first(messages: &mut [MessageSummary]) {
    messages.sort_by(|a, b| {
        b.date
            .unwrap_or(0)
            .cmp(&a.date.unwrap_or(0))
            .then_with(|| b.id.0.cmp(&a.id.0))
    });
}

/// 把地址列表按地址去重（保留首次出现的显示名）。
pub fn dedupe_addresses(addresses: Vec<Address>) -> Vec<Address> {
    let mut seen = BTreeMap::new();
    for address in addresses {
        let key = address.addr.to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        seen.entry(key).or_insert(address);
    }
    seen.into_values().collect()
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_mapping_round_trips() {
        let flags = Flags::from_imap(["\\Seen", "\\Flagged", "\\Recent", "custom"]);
        assert_eq!(flags.bits(), FLAG_SEEN | FLAG_FLAGGED);
        assert!(flags.contains(Flags::SEEN));
        assert!(!flags.contains(Flags::DRAFT));

        let imap = flags.to_imap();
        assert_eq!(imap, vec!["\\Seen", "\\Flagged"]);
    }

    #[test]
    fn flag_op_applies_to_target_state() {
        let current = Flags::SEEN;
        assert_eq!(
            FlagOp::Set(Flags::FLAGGED).apply(current),
            Flags(FLAG_SEEN | FLAG_FLAGGED)
        );
        assert_eq!(FlagOp::Clear(Flags::SEEN).apply(current), Flags::NONE);
    }

    #[test]
    fn snippet_takes_first_200_chars_and_collapses_space() {
        let text = format!("  hello\n\n world \t{}", "x".repeat(300));
        let snippet = make_snippet(&text);
        assert!(snippet.starts_with("hello world "));
        assert_eq!(snippet.chars().count(), 200);
    }

    #[test]
    fn snippet_handles_cjk_by_chars_not_bytes() {
        let snippet = make_snippet(&"中".repeat(500));
        assert_eq!(snippet.chars().count(), 200);
    }

    #[test]
    fn date_falls_back_to_internal() {
        assert_eq!(
            effective_date(Some(10), Some(20)),
            (Some(10), DateSource::Header)
        );
        assert_eq!(
            effective_date(None, Some(20)),
            (Some(20), DateSource::Internal)
        );
        assert_eq!(effective_date(None, None), (None, DateSource::Internal));
    }

    #[test]
    fn attachment_limits() {
        assert!(check_attachment_limits(&[1024, 2048]).is_ok());
        assert!(matches!(
            check_attachment_limits(&[MAX_ATTACHMENT_BYTES + 1]),
            Err(CoreError::Limit {
                what: LimitKind::AttachmentFile
            })
        ));
        let two_big = [MAX_ATTACHMENT_BYTES / 2 + 1, MAX_ATTACHMENT_BYTES / 2 + 1];
        assert!(matches!(
            check_attachment_limits(&two_big),
            Err(CoreError::Limit {
                what: LimitKind::AttachmentTotal
            })
        ));
    }

    #[test]
    fn email_masking() {
        assert_eq!(mask_email("alice@example.com"), "a***@example.com");
        assert_eq!(mask_email(""), "***");
        assert_eq!(mask_email("not-an-email"), "***");
    }

    #[test]
    fn message_id_normalization() {
        assert_eq!(
            normalize_message_id(" <abc@example.com> "),
            Some("abc@example.com".to_string())
        );
        assert_eq!(normalize_message_id("<>"), None);
        assert_eq!(normalize_message_id(""), None);
    }

    #[test]
    fn generated_message_id_uses_domain() {
        let id = generate_message_id("alice@example.com");
        assert!(id.ends_with("@example.com"));
        assert!(!id.contains('<'));
        assert!(id.len() > "@example.com".len());
    }

    #[test]
    fn folder_role_heuristics() {
        assert_eq!(folder_role_heuristic("INBOX"), FolderRole::Inbox);
        assert_eq!(folder_role_heuristic("已发送"), FolderRole::Sent);
        assert_eq!(folder_role_heuristic("工作/草稿箱"), FolderRole::Drafts);
        assert_eq!(folder_role_heuristic("Deleted Items"), FolderRole::Trash);
        assert_eq!(folder_role_heuristic("random"), FolderRole::Other);

        let attrs = vec!["\\Sent".to_string()];
        assert_eq!(
            FolderRole::from_special_use(&attrs, "something-else"),
            FolderRole::Sent
        );
    }

    #[test]
    fn oauth_provider_parsing_and_defaults() {
        assert_eq!(OAuthProvider::parse("google"), Some(OAuthProvider::Google));
        assert_eq!(
            OAuthProvider::parse("Outlook"),
            Some(OAuthProvider::Microsoft)
        );
        assert_eq!(OAuthProvider::parse("unknown"), None);
        assert_eq!(OAuthProvider::Google.default_imap().tls, TlsMode::Implicit);
        assert_eq!(
            OAuthProvider::Microsoft.default_smtp().tls,
            TlsMode::StartTls
        );
        assert!(!OAuthProvider::Google.supports_device_code());
        assert!(OAuthProvider::Microsoft.supports_device_code());
        assert!(AuthMethod::parse("oauth2").is_oauth());
        assert_eq!(AuthMethod::OAuth2.as_str(), "oauth2");
    }

    #[test]
    fn auth_credential_debug_is_redacted() {
        let password = AuthCredential::password("hunter2");
        let oauth = AuthCredential::oauth2("alice@example.com", "ya29.secret", 100);
        for credential in [password, oauth] {
            let text = format!("{credential:?}");
            assert!(!text.contains("hunter2"));
            assert!(!text.contains("ya29.secret"));
            assert!(!text.contains("alice@example.com"));
            assert!(text.contains("<redacted>"));
        }
    }

    #[test]
    fn auth_credential_expiry_uses_skew() {
        let credential = AuthCredential::oauth2("a@example.com", "token", 1000);
        assert!(credential.expires_within(950, 60));
        assert!(!credential.expires_within(900, 60));
        assert!(!AuthCredential::password("x").expires_within(1000, 60));
    }

    #[test]
    fn draft_sync_state_round_trips_and_rejects_unknown() {
        for state in DraftSyncState::ALL {
            assert_eq!(DraftSyncState::parse(state.as_str()), Some(state));
        }
        assert_eq!(DraftSyncState::parse("bogus"), None);
        assert_eq!(DraftSyncState::default(), DraftSyncState::LocalOnly);
    }

    #[test]
    fn sorting_is_newest_first_then_id() {
        fn sample(id: i64, date: i64) -> MessageSummary {
            MessageSummary {
                id: MessageId(id),
                account: AccountId(1),
                folder: FolderId(1),
                uid: Some(id as u32),
                message_id: None,
                subject: None,
                from: None,
                to: vec![],
                cc: vec![],
                date: Some(date),
                date_source: DateSource::Header,
                size: None,
                flags: Flags::NONE,
                snippet: None,
                has_attachment: false,
                body_cached: false,
            }
        }

        let mut messages = vec![sample(1, 100), sample(2, 100), sample(3, 200)];
        sort_newest_first(&mut messages);
        assert_eq!(messages[0].id, MessageId(3));
        assert_eq!(messages[1].id, MessageId(2));
        assert_eq!(messages[2].id, MessageId(1));
    }
}
