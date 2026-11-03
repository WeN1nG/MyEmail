//! UI ↔ 后端契约。
//!
//! - `Command`：UI → me-sync，无界通道。
//! - `Event`：me-sync → UI，有界通道（1024）；进度事件可合并，关键事件不丢弃。
//! - 事件载荷是**结果快照**，UI 无需回查即可渲染常见场景；真相仍在数据库。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::calendar::{CalendarEventDraft, CalendarEventId, CalendarId};
use crate::error::CoreError;
use crate::model::{
    Account, AccountId, Address, AuthMethod, DraftId, DraftSyncState, Endpoint, FlagOp, Flags,
    Folder, FolderId, MessageBody, MessageId, MessageSummary, OAuthClientConfig, OAuthProvider,
    OutgoingMessage, SearchQuery, TlsMode,
};
use crate::music::MusicOrder;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TaskId(pub u64);

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "task#{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Command
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    AddAccount(AccountDraft),
    UpdateAccount {
        id: AccountId,
        draft: AccountDraft,
    },
    DeleteAccount {
        id: AccountId,
        delete_drafts: bool,
    },
    TestConnection(AccountId),

    /// 启动同步与手动刷新共用。
    SyncAccount(AccountId),
    SyncFolder {
        account: AccountId,
        folder: FolderId,
    },
    CancelSync(AccountId),

    /// 触发按需下载正文并置已读。
    OpenMessage(MessageId),
    DownloadAttachment {
        message: MessageId,
        part_index: u32,
    },
    SetFlags {
        ids: Vec<MessageId>,
        flags: FlagOp,
    },
    MoveToTrash(Vec<MessageId>),
    EmptyTrash(FolderId),

    SaveDraft(OutgoingMessage),
    SaveDraftToServer(DraftId),
    /// 读取草稿原文，供 UI 重新编辑。
    LoadDraft(DraftId),
    Send(OutgoingMessage),
    RetryArchive(DraftId),

    Search(SearchQuery),
    SetTheme(ThemeMode),
    SetBodyBackfill {
        enabled: bool,
    },
    /// 是否自动加载正文中的远程图片（默认开启）。
    SetRemoteImages {
        enabled: bool,
    },
    /// 用户已确认「本地缓存为明文保存」告知，落库后不再提示。
    AcknowledgePlaintextNotice,
    RebuildIndex,
    ClearCache(Option<AccountId>),
    /// 附件缓存根目录；`None` 表示恢复默认 `<数据目录>/attach`。
    /// 变更会迁移已有附件文件并更新数据库中的本地路径。
    SetAttachmentDir {
        path: Option<PathBuf>,
    },

    /// 指定文件夹角色（SPECIAL-USE 不可用或用户想覆盖启发式结果时）。
    SetFolderRole {
        folder: FolderId,
        role: crate::model::FolderRole,
    },
    /// 确认删除服务器上已删除、本地待确认的文件夹。
    AcknowledgeFolderRemoval(FolderId),

    /// 全局实时同步开关（IDLE / 轮询）。
    SetRealtimeSync {
        enabled: bool,
    },
    /// 全局默认轮询间隔，60～3600 秒。
    SetPollInterval {
        secs: u32,
    },
    /// 单账户覆盖：是否启用 IDLE / 该账户的轮询间隔。
    /// `poll_interval_secs = None` 表示清除账户覆盖、跟随全局默认。
    SetAccountRealtime {
        account: AccountId,
        enabled: bool,
        poll_interval_secs: Option<u32>,
    },
    /// 草稿自动镜像开关（v0.2 WP-10 接入）。
    SetDraftServerSync {
        enabled: bool,
    },
    /// HTML 渲染模式（v0.2 WP-12 接入）。
    SetHtmlRenderer {
        mode: HtmlRendererMode,
    },
    /// 高级设置覆盖 OAuth 客户端 `client_id` / Microsoft tenant。
    /// `client_secret` 只能构建期注入，不经过此命令。
    SetOAuthClient {
        provider: OAuthProvider,
        config: OAuthClientConfig,
    },
    /// 开始 OAuth 授权。`account = Some(id)` 表示重新授权。
    StartOAuth {
        provider: OAuthProvider,
        mode: OAuthFlowMode,
        account: Option<AccountId>,
        email_hint: Option<String>,
    },
    /// 取消正在进行的 OAuth 授权流程。
    CancelOAuth {
        flow_id: u64,
    },

    /// 删除本地草稿；有服务器身份时写入墓碑并传播到服务器。
    DeleteDraft {
        id: DraftId,
    },
    /// 手动重试某条草稿的服务器同步（清零失败计数）。
    RetryDraftSync {
        draft: DraftId,
    },
    /// 冲突处理。v0.2 默认「保留两份」，该命令用于用户显式选择。
    ResolveDraftConflict {
        draft: DraftId,
        resolution: DraftConflictResolution,
    },
    /// 处置「服务器已删除、本地保留」的草稿。
    ResolveDraftDeletion {
        draft: DraftId,
        resolution: DraftDeletionResolution,
    },
    /// 迁移后一次性确认是否上传从未上传过的旧草稿（作用于所有账户）。
    ConfirmDraftBulkUpload {
        accept: bool,
    },

    // ---- 日历（v0.3）----
    /// 新建日程。
    CreateCalendarEvent {
        draft: CalendarEventDraft,
    },
    /// 更新日程本体。v0.3 只支持 `scope = AllEvents`（整条系列）。
    UpdateCalendarEvent {
        id: CalendarEventId,
        draft: CalendarEventDraft,
    },
    /// 软删除日程。
    DeleteCalendarEvent {
        id: CalendarEventId,
    },
    CreateCalendar {
        name: String,
        color: String,
    },
    /// 显示 / 隐藏日历；隐藏的日历不参与可视范围查询。
    SetCalendarVisible {
        id: CalendarId,
        visible: bool,
    },
    /// 音乐设置。只写 meta，**不驱动播放**：播放由 UI 直接发命令给音频线程。
    ///
    /// `None` 表示该项不修改；`dir = Some(path)` 立即生效（下次启动据此自动扫描）。
    SetMusicSettings {
        dir: Option<PathBuf>,
        order: Option<MusicOrder>,
    },

    // ---- 天气（v0.3）----
    /// 保存和风天气配置：**只有一个 Host**（官方已把城市搜索与天气收进同一域名）
    /// 与 API Key。`key` 为空表示不改动已保存的 Key。
    ///
    /// Key 只进密钥库或会话内存，**不写数据库、不写日志**。
    SetWeatherConfig {
        host: String,
        key: Option<String>,
    },
    /// 测试连接：用给定的 Host 与 Key 发一次真实请求，验证配置是否可用。
    ///
    /// **不落库**——测试用的 Host 与 Key 只在本次请求里生效。
    TestWeatherConnection {
        host: String,
        key: String,
    },
    /// 按城市名搜索候选地点。`request_id` 用于丢弃过期响应。
    SearchWeatherLocations {
        request_id: u64,
        query: String,
    },
    /// 添加一个候选地点。
    AddWeatherLocation {
        candidate: crate::weather::WeatherLocationCandidate,
    },
    /// 删除地点。
    RemoveWeatherLocation {
        id: crate::weather::WeatherLocationId,
    },
    /// 设为默认地点（只有默认地点会被自动刷新）。
    SetDefaultWeatherLocation {
        id: crate::weather::WeatherLocationId,
    },
    /// 刷新默认地点的天气。`force` 只跳过「未过期直接复用」，
    /// 最小间隔与退避仍然生效。
    RefreshWeather {
        force: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountDraft {
    pub email: String,
    pub display_name: Option<String>,
    pub imap: Endpoint,
    pub smtp: Endpoint,
    pub auth_user: String,
    /// `Add` 时必须 `Some`；`Update` 时 `None` = 保留现有凭证。
    pub auth_secret: Option<String>,
    pub auth_method: AuthMethod,
    /// OAuth2 账户必须为 `Some`；密码/授权码账户为 `None`。
    #[serde(default)]
    pub oauth_provider: Option<crate::model::OAuthProvider>,
    /// 由授权结果填充；仅 OAuth2 使用。
    #[serde(default)]
    pub oauth_subject: Option<String>,
    /// TLS 为 `None` 时必须为 `true`。
    pub plaintext_tls_ack: bool,
}

impl AccountDraft {
    /// 校验明文 TLS 风险确认。返回 `Err` 时 UI 应阻止提交。
    pub fn validate_tls_ack(&self) -> Result<(), CoreError> {
        let plaintext = self.imap.tls == TlsMode::None || self.smtp.tls == TlsMode::None;
        if plaintext && !self.plaintext_tls_ack {
            return Err(CoreError::auth("明文 TLS 需要显式风险确认"));
        }
        Ok(())
    }

    /// v0.2 账户写入前的完整校验：
    /// - 明文 TLS 风险确认；
    /// - `auth_method = oauth2` ⇔ `oauth_provider.is_some()`；
    /// - OAuth2 账户不得使用明文 TLS。
    pub fn validate(&self) -> Result<(), CoreError> {
        self.validate_tls_ack()?;
        match (self.auth_method.is_oauth(), self.oauth_provider) {
            (true, None) => Err(CoreError::auth("OAuth2 账户必须指定服务商")),
            (false, Some(_)) => Err(CoreError::auth("密码/授权码账户不能指定 OAuth 服务商")),
            (true, Some(_)) if self.uses_plaintext_tls() => {
                Err(CoreError::auth("OAuth2 账户不得使用明文 TLS"))
            }
            _ => Ok(()),
        }
    }

    pub fn uses_plaintext_tls(&self) -> bool {
        self.imap.tls == TlsMode::None || self.smtp.tls == TlsMode::None
    }
}

/// 草稿冲突处理方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftConflictResolution {
    KeepLocal,
    UseServer,
    KeepBoth,
}

/// 「服务器已删除、本地保留」草稿的处理方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftDeletionResolution {
    KeepLocal,
    DeleteLocal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeMode {
    System,
    Light,
    Dark,
}

/// 实时同步模式。`Off` = 只保留启动与手动同步。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RealtimeMode {
    Idle,
    Polling,
    #[default]
    Off,
}

impl RealtimeMode {
    pub fn label(self) -> &'static str {
        match self {
            RealtimeMode::Idle => "实时",
            RealtimeMode::Polling => "轮询",
            RealtimeMode::Off => "关闭",
        }
    }
}

/// HTML 阅读渲染器模式（v0.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HtmlRendererMode {
    #[default]
    Enhanced,
    Legacy,
}

impl HtmlRendererMode {
    pub fn as_str(self) -> &'static str {
        match self {
            HtmlRendererMode::Enhanced => "enhanced",
            HtmlRendererMode::Legacy => "legacy",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "legacy" => HtmlRendererMode::Legacy,
            _ => HtmlRendererMode::Enhanced,
        }
    }
}

/// OAuth 授权模式：回环重定向为主，设备码为兜底。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthFlowMode {
    Loopback,
    DeviceCode,
}

impl ThemeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ThemeMode::System => "system",
            ThemeMode::Light => "light",
            ThemeMode::Dark => "dark",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "light" => ThemeMode::Light,
            "dark" => ThemeMode::Dark,
            _ => ThemeMode::System,
        }
    }
}

// ---------------------------------------------------------------------------
// Event
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    SettingsLoaded {
        theme: ThemeMode,
        body_backfill: bool,
        remote_images: bool,
        realtime_sync: bool,
        poll_interval_secs: u32,
        draft_server_sync: bool,
        draft_bulk_upload_pending: bool,
        /// 是否已经确认过「本地缓存为明文保存」告知。
        plaintext_notice_ack: bool,
        html_renderer: HtmlRendererMode,
        /// 非敏感 OAuth 客户端覆盖；空串表示使用构建期默认值。
        oauth_google_client_id: String,
        oauth_microsoft_client_id: String,
        oauth_microsoft_tenant: String,
        /// 当前生效的附件缓存根目录（默认或用户自定义）。
        attachment_dir: PathBuf,
        /// 音乐目录；`None` 表示尚未选择。
        music_dir: Option<PathBuf>,
        music_order: MusicOrder,
    },
    /// 附件缓存目录已迁移到新位置；旧文件已搬移、数据库路径已更新。
    AttachmentDirChanged {
        dir: PathBuf,
    },
    AccountsLoaded {
        accounts: Vec<Account>,
    },
    AccountAdded {
        account: Account,
    },
    AccountUpdated {
        account: Account,
    },
    AccountRemoved {
        id: AccountId,
    },
    AccountStatus {
        id: AccountId,
        status: ConnectionState,
    },
    /// 实时同步（IDLE / 轮询）状态。事件稀疏，不参与进度合并。
    RealtimeStatus {
        account: AccountId,
        mode: RealtimeMode,
        connected: bool,
        last_event_at: Option<i64>,
    },
    ConnectionTested {
        account: AccountId,
        imap_ok: bool,
        smtp_ok: bool,
        detail: Option<String>,
    },
    FoldersUpdated {
        account: AccountId,
        folders: Vec<Folder>,
    },
    MessagesAppended {
        folder: FolderId,
        messages: Vec<MessageSummary>,
    },
    /// 本地删除/移动后从列表移除（服务器待同步）。
    MessagesRemoved {
        folder: FolderId,
        ids: Vec<MessageId>,
    },
    FlagsChanged {
        items: Vec<(MessageId, Flags)>,
    },
    /// 本地修复了邮件摘要（如历史 GBK 邮件按正确字符集重新解码）。
    MessagesUpdated {
        messages: Vec<MessageSummary>,
    },
    MessageBodyLoaded {
        id: MessageId,
        body: MessageBody,
    },
    AttachmentDownloaded {
        message: MessageId,
        part_index: u32,
        local_path: PathBuf,
    },
    SearchResults {
        query_id: u64,
        messages: Vec<MessageSummary>,
        total: u32,
        body_indexed: u32,
        body_total: u32,
    },
    SyncProgress {
        folder: FolderId,
        done: u32,
        total: Option<u32>,
    },
    IndexProgress {
        account: AccountId,
        indexed: u32,
        total: u32,
    },
    IndexRebuilt {
        indexed: u32,
    },
    DraftSaved {
        id: DraftId,
        synced_to_server: bool,
    },
    /// 草稿原文已解析完成，可直接填入撰写界面。
    DraftLoaded {
        draft: crate::model::DraftContent,
    },
    /// 草稿同步状态变化（导入 / 推送 / 删除 / 失败）。
    DraftSyncStateChanged {
        draft: DraftId,
        state: DraftSyncState,
        error: Option<String>,
    },
    /// 双方都修改，已保留两份；UI 提供「查看两份 / 保留本地 / 使用服务器 / 保留两份」。
    DraftConflictDetected {
        draft: DraftId,
        conflict_copy: DraftId,
    },
    /// OAuth 流程已开始；回环模式 `user_code = None`，设备码模式提供用户码。
    OAuthFlowStarted {
        flow_id: u64,
        provider: OAuthProvider,
        verification_uri: String,
        user_code: Option<String>,
        expires_at: i64,
    },
    OAuthCompleted {
        flow_id: u64,
        account: Account,
        created: bool,
    },
    OAuthFailed {
        flow_id: u64,
        message: String,
        reauth_required: bool,
    },
    /// 刷新令牌失效，需要重新授权；UI 在账户行显示入口。
    OAuthReauthRequired {
        account: AccountId,
    },
    /// 凭证无法写入系统密钥库，目前仅保存在会话内存。
    /// `created = true` 表示新增账户；UI 可据此让用户选择“仅本次会话”或取消添加。
    CredentialNotPersisted {
        account: AccountId,
        created: bool,
    },
    /// 剩余磁盘低于阈值，后台补全已暂停。
    DiskSpaceLow {
        available_bytes: Option<u64>,
    },
    SendCompleted {
        draft: DraftId,
        archived: bool,
        archive_error: Option<String>,
    },
    SendFailed {
        draft: DraftId,
        reason: SendFailureKind,
    },
    CacheCleared {
        account: Option<AccountId>,
        freed_bytes: u64,
    },
    DraftChanged {
        id: DraftId,
    },
    DraftDeleted {
        id: DraftId,
    },
    FolderRemoved {
        folder: FolderId,
    },
    /// 日历集合变化（新建 / 改名 / 显示隐藏）。
    CalendarsChanged {
        calendars: Vec<crate::calendar::Calendar>,
    },
    /// 某个日历里的事件发生变化；UI 重新拉取当前可视范围。
    CalendarEventsChanged {
        calendar: crate::calendar::CalendarId,
    },
    /// 提醒到点。应用内 toast；v0.3 不调用系统通知。
    CalendarReminder {
        event: crate::calendar::CalendarEventId,
        summary: String,
        start_utc: i64,
        minutes_before: u32,
    },

    // ---- 天气（v0.3）----
    /// 天气配置状态：是否已配置 Key、当前生效的 Host。
    WeatherConfigChanged {
        key_present: bool,
        api_host: String,
    },
    /// 连接测试结果。`ok` 为真时 `message` 是成功描述，否则是失败原因。
    WeatherConnectionTested {
        ok: bool,
        message: String,
    },
    /// API Key 已保存。`persisted = false` 表示只保存在本次会话。
    WeatherKeyStored {
        persisted: bool,
    },
    /// 城市搜索结果。UI 只接受当前 `request_id`。
    WeatherLocationCandidates {
        request_id: u64,
        candidates: Vec<crate::weather::WeatherLocationCandidate>,
    },
    /// 地点列表变化（新增 / 删除 / 改默认）。
    WeatherLocationsChanged,
    /// 天气刷新成功。UI 据此重新读取快照。
    WeatherUpdated {
        location_id: crate::weather::WeatherLocationId,
        fetched_at: i64,
        expires_at: i64,
    },
    /// 天气刷新失败。`has_cache` 表示本地仍有缓存可显示。
    WeatherFailed {
        message: String,
        has_cache: bool,
    },
    Error {
        scope: ErrorScope,
        message: String,
    },
}

impl Event {
    /// 可合并/可丢弃的进度事件。
    pub fn is_progress(&self) -> bool {
        matches!(
            self,
            Event::SyncProgress { .. } | Event::IndexProgress { .. }
        )
    }

    /// 关键事件不得丢弃。
    pub fn is_critical(&self) -> bool {
        !self.is_progress()
    }

    /// 是否影响邮件列表的渲染（用于视图脏标记）。
    pub fn affects_message_list(&self) -> bool {
        matches!(
            self,
            Event::MessagesAppended { .. }
                | Event::MessagesUpdated { .. }
                | Event::MessagesRemoved { .. }
                | Event::FlagsChanged { .. }
                | Event::AccountRemoved { .. }
                | Event::FoldersUpdated { .. }
                | Event::SearchResults { .. }
                | Event::FolderRemoved { .. }
                | Event::CacheCleared { .. }
                | Event::MessageBodyLoaded { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Online,
    Syncing,
    Backoff { retry_at: i64 },
    Error { detail: String },
}

impl ConnectionState {
    pub fn label(&self) -> &'static str {
        match self {
            ConnectionState::Disconnected => "未连接",
            ConnectionState::Connecting => "连接中",
            ConnectionState::Online => "在线",
            ConnectionState::Syncing => "同步中",
            ConnectionState::Backoff { .. } => "稍后重试",
            ConnectionState::Error { .. } => "错误",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendFailureKind {
    NetworkUnreachable,
    Timeout,
    Rejected { code: Option<u16>, detail: String },
    Tls { detail: String },
    Auth { detail: String },
}

impl SendFailureKind {
    pub fn user_message(&self) -> String {
        match self {
            SendFailureKind::NetworkUnreachable => "无网络连接，已保存到草稿箱".to_string(),
            SendFailureKind::Timeout => "发送超时，已保存到草稿箱".to_string(),
            SendFailureKind::Rejected { code, detail } => match code {
                Some(code) => format!("发送失败（{code}）：{detail}，已保存到草稿箱"),
                None => format!("发送失败：{detail}，已保存到草稿箱"),
            },
            SendFailureKind::Tls { detail } => {
                format!("TLS 握手失败：{detail}，已保存到草稿箱")
            }
            SendFailureKind::Auth { detail } => {
                format!("SMTP 认证失败：{detail}，已保存到草稿箱")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorScope {
    Global,
    Account(AccountId),
    Folder(FolderId),
    Message(MessageId),
    Draft(DraftId),
}

// ---------------------------------------------------------------------------
// 契约辅助类型
// ---------------------------------------------------------------------------

/// SMTP 发送所需的地址与正文快照（由 `OutgoingMessage` 派生）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipientList {
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
}

impl RecipientList {
    pub fn from_outgoing(outgoing: &crate::model::OutgoingMessage) -> Self {
        Self {
            to: outgoing.to.clone(),
            cc: outgoing.cc.clone(),
            bcc: outgoing.bcc.clone(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.to.is_empty() && self.cc.is_empty() && self.bcc.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_tls_requires_ack() {
        let draft = AccountDraft {
            email: "a@example.com".into(),
            display_name: None,
            imap: Endpoint::new("imap.example.com", 143, TlsMode::None),
            smtp: Endpoint::new("smtp.example.com", 25, TlsMode::StartTls),
            auth_user: "a@example.com".into(),
            auth_secret: Some("secret".into()),
            auth_method: AuthMethod::Password,
            oauth_provider: None,
            oauth_subject: None,
            plaintext_tls_ack: false,
        };
        assert!(draft.validate_tls_ack().is_err());
        assert!(draft.uses_plaintext_tls());

        let ok = AccountDraft {
            plaintext_tls_ack: true,
            ..draft
        };
        assert!(ok.validate_tls_ack().is_ok());
    }

    #[test]
    fn command_and_event_are_serde_roundtrippable() {
        let command = Command::SetFlags {
            ids: vec![MessageId(7)],
            flags: FlagOp::Set(Flags::SEEN),
        };
        let json = serde_json::to_string(&command).expect("serialize command");
        let back: Command = serde_json::from_str(&json).expect("deserialize command");
        match back {
            Command::SetFlags { ids, flags } => {
                assert_eq!(ids, vec![MessageId(7)]);
                assert_eq!(flags, FlagOp::Set(Flags::SEEN));
            }
            _ => panic!("wrong command variant"),
        }

        let event = Event::IndexProgress {
            account: AccountId(1),
            indexed: 3,
            total: 9,
        };
        let json = serde_json::to_string(&event).expect("serialize event");
        let back: Event = serde_json::from_str(&json).expect("deserialize event");
        assert!(matches!(
            back,
            Event::IndexProgress {
                indexed: 3,
                total: 9,
                ..
            }
        ));
    }

    #[test]
    fn progress_and_critical_events() {
        let progress = Event::SyncProgress {
            folder: FolderId(1),
            done: 1,
            total: Some(2),
        };
        assert!(progress.is_progress());
        assert!(!progress.is_critical());

        let critical = Event::IndexRebuilt { indexed: 1 };
        assert!(!critical.is_progress());
        assert!(critical.is_critical());
        assert!(!critical.affects_message_list());
    }

    #[test]
    fn realtime_commands_and_settings_roundtrip() {
        let command = Command::SetAccountRealtime {
            account: AccountId(1),
            enabled: false,
            poll_interval_secs: Some(120),
        };
        let json = serde_json::to_string(&command).unwrap();
        let back: Command = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            Command::SetAccountRealtime {
                account: AccountId(1),
                enabled: false,
                poll_interval_secs: Some(120)
            }
        ));

        let status = Event::RealtimeStatus {
            account: AccountId(1),
            mode: RealtimeMode::Polling,
            connected: false,
            last_event_at: Some(10),
        };
        let json = serde_json::to_string(&status).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            Event::RealtimeStatus {
                mode: RealtimeMode::Polling,
                connected: false,
                ..
            }
        ));

        let settings = Event::SettingsLoaded {
            theme: ThemeMode::System,
            body_backfill: true,
            remote_images: false,
            realtime_sync: true,
            poll_interval_secs: 300,
            draft_server_sync: true,
            draft_bulk_upload_pending: false,
            plaintext_notice_ack: false,
            html_renderer: HtmlRendererMode::Enhanced,
            oauth_google_client_id: String::new(),
            oauth_microsoft_client_id: String::new(),
            oauth_microsoft_tenant: String::new(),
            attachment_dir: PathBuf::from("C:/data/attach"),
            music_dir: Some(PathBuf::from("C:/music")),
            music_order: MusicOrder::Shuffle,
        };
        let json = serde_json::to_string(&settings).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            Event::SettingsLoaded {
                poll_interval_secs: 300,
                html_renderer: HtmlRendererMode::Enhanced,
                ..
            }
        ));
    }
}
