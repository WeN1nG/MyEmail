//! MyEmail 领域层。
//!
//! 本 crate 是依赖图的唯一汇点：不依赖 tokio / rusqlite / 任何协议库，
//! 领域规则可脱离运行时单测。

pub mod calendar;
pub mod command;
pub mod error;
pub mod model;
pub mod music;
pub mod port;
pub mod weather;

pub use calendar::{
    Calendar, CalendarEvent, CalendarEventDraft, CalendarEventId, CalendarEventStatus,
    CalendarException, CalendarId, CalendarSeries, CalendarSource, CalendarViewKind, DueReminder,
    EventOccurrence, ExpandedEvent, RecurrenceEditScope, RecurrenceError, RecurrenceLimits,
    RecurrenceWarning, due_reminders, expand_event, is_valid_timezone, recurrence_until,
    validate_draft,
};
pub use music::{MusicOrder, MusicQueue};
pub use weather::{
    CurrentWeather, DEFAULT_API_HOST, DailyForecast, HOST_HINT, HourlyForecast,
    MAX_WEATHER_LOCATIONS, PATH_7D, PATH_24H, PATH_CITY_LOOKUP, PATH_CITY_LOOKUP_LEGACY, PATH_NOW,
    SeriesPoint, StoredWeatherSnapshot, WeatherCredential, WeatherForecast, WeatherFuture,
    WeatherKind, WeatherLocation, WeatherLocationCandidate, WeatherLocationId, WeatherProvider,
    WeatherRepo, WeatherSnapshot, format_humidity, format_precip, format_temp, format_wind,
    hourly_series, hourly_temp_range, is_expired, is_public_ip, normalize_host,
    preview_request_url, validate_candidate, validate_forecast, validate_query,
};

pub use command::{
    AccountDraft, Command, ConnectionState, DraftConflictResolution, DraftDeletionResolution,
    ErrorScope, Event, HtmlRendererMode, OAuthFlowMode, RealtimeMode, RecipientList,
    SendFailureKind, TaskId, ThemeMode,
};
pub use error::{BoxError, CoreError, LimitKind};
pub use model::{
    Account, AccountId, Address, Attachment, AuthCredential, AuthMethod, DateSource, Draft,
    DraftContent, DraftId, DraftStatus, DraftSyncState, Endpoint, FlagOp, Flags, Folder, FolderId,
    FolderRole, InlineImage, LogSafeMessageSummary, MAX_ATTACHMENT_BYTES,
    MAX_ATTACHMENT_TOTAL_BYTES, MAX_PAGE_SIZE, MAX_PARSE_BYTES, MessageBody, MessageId,
    MessageSummary, NewMessage, OAuthClientConfig, OAuthProvider, OutgoingMessage, SearchQuery,
    TlsMode, check_attachment_limits, effective_date, format_local_datetime, generate_message_id,
    make_snippet, mask_email, normalize_message_id, now_unix, sort_newest_first,
};
pub use port::{
    AccountRepo, AttachmentRepo, BodyCoverage, BodyJob, BrowserOpener, CalendarRepo,
    DeleteAccountOutcome, DraftRepo, DraftSyncItem, FlagStoreMode, FolderInfo, FolderRepo,
    HeaderFetch, IdleOutcome, ImapCapabilities, ImapSession, ImapSessionFactory, IndexRow,
    MessageRepo, MetaRepo, MoveOrigin, NewDraft, PendingOp, PendingOpKind, PendingRepo,
    ReadOnlyRepo, SearchOutcome, SearchRepo, SelectedFolder, SmtpSender, StorePort, UidRange,
};

/// 当前 schema 版本。v3 起冻结（`doc/v03架构设计.md` §8.3）。
pub const SCHEMA_VERSION: u32 = 3;

/// 应用显示名。
pub const APP_NAME: &str = "MyEmail";

/// 后台补全要求的最低剩余磁盘空间（500 MB）。
pub const MIN_FREE_BYTES: u64 = 500 * 1024 * 1024;

/// 轮询间隔范围与默认值（秒）。
pub const MIN_POLL_SECS: u32 = 60;
pub const MAX_POLL_SECS: u32 = 3600;
pub const DEFAULT_POLL_SECS: u32 = 300;

/// 默认设置键。
pub mod settings_key {
    pub const SCHEMA_VERSION: &str = "schema_version";
    pub const THEME: &str = "theme";
    pub const BODY_BACKFILL: &str = "body_backfill";
    pub const REMOTE_IMAGES: &str = "remote_images";
    pub const LAST_ACCOUNT: &str = "last_account";
    pub const REALTIME_SYNC: &str = "realtime_sync";
    pub const POLL_INTERVAL_SECS: &str = "poll_interval_secs";
    pub const DRAFT_SERVER_SYNC: &str = "draft_server_sync";
    pub const DRAFT_BULK_UPLOAD_PENDING: &str = "draft_bulk_upload_pending";
    pub const DRAFT_BULK_UPLOAD_DECLINED: &str = "draft_bulk_upload_declined";
    pub const HTML_RENDERER: &str = "html_renderer";
    /// 附件缓存根目录（绝对路径）；空串或缺失表示使用 `<数据目录>/attach`。
    pub const ATTACHMENT_DIR: &str = "attachment_dir";
    /// 用户已确认「本地缓存为明文保存」告知；只提示一次。
    pub const PLAINTEXT_NOTICE_ACK: &str = "plaintext_notice_ack";
    pub const OAUTH_CLIENT_GOOGLE: &str = "oauth_client_google";
    pub const OAUTH_CLIENT_MICROSOFT: &str = "oauth_client_microsoft";
    /// 日历默认视图：`month` / `week` / `agenda`。
    pub const CALENDAR_DEFAULT_VIEW: &str = "calendar_default_view";
    /// 日历功能开关。
    pub const FEATURE_CALENDAR: &str = "feature_calendar";
    /// 音乐目录（绝对路径）；缺失表示尚未选择。
    pub const MUSIC_DIR: &str = "music_dir";
    /// 播放顺序：`sequential` / `shuffle`。
    pub const MUSIC_ORDER: &str = "music_order";
    /// 和风天气天气接口 Host。
    pub const WEATHER_API_HOST: &str = "weather_api_host";
}
