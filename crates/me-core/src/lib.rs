//! MyEmail 领域层。
//!
//! 本 crate 是依赖图的唯一汇点：不依赖 tokio / rusqlite / 任何协议库，
//! 领域规则可脱离运行时单测。

pub mod ai;
pub mod ai_plan;
pub mod calendar;
pub mod command;
pub mod error;
pub mod model;
pub mod music;
pub mod port;
pub mod search_syntax;
pub mod weather;

pub use ai::{
    AiChatRequest, AiContextMode, AiContextRef, AiContextRequest, AiConversation, AiConversationId,
    AiCredential, AiDelta, AiDeltaSink, AiEntity, AiEntityKind, AiEntitySource, AiFuture,
    AiMemoryRepo, AiMessage, AiMessageId, AiMessageStatus, AiProvider, AiProviderConfig,
    AiProviderKind, AiRepo, AiRole, AiToolCall, AiToolName, AiToolPermissions, AiToolResult,
    AiToolSpec, AiToolStatus, AiToolTrace, AiTurnResult, AiWireMessage, CancelFlag, ChatBuildInput,
    MAX_CONTEXT_REFS, MAX_CONTEXT_TOTAL_CHARS, MAX_CURRENT_CHARS, MAX_ENTITIES, MAX_ENTITY_CONTEXT,
    MAX_INPUT_CHARS, MAX_MESSAGE_CHARS, MAX_TOOL_CALLS, MAX_TOOL_RESULT_CHARS, MAX_TOOL_ROUNDS,
    MAX_USER_INPUT_CHARS, MailCardRef, MotiveSource, NewAiEntity, NewAiMessage, SYSTEM_PROMPT,
    TOOL_SPECS, build_chat_request, escape_context, estimate_tokens, normalize_base_url,
    normalize_entity_name, render_context_refs, render_entity_context, render_user_message,
    select_entities, strip_control, tool_motive, tool_specs, trim_history, truncate_chars,
    validate_api_key, validate_config,
};
pub use ai_plan::{
    Capability, MAX_PLAN_GOAL_CHARS, MAX_PLAN_STEPS, MAX_STEP_EVIDENCE_CHARS, MAX_STEP_TITLE_CHARS,
    Plan, PlanStatus, PlanStep, validate_plan,
};

pub use calendar::{
    Calendar, CalendarEvent, CalendarEventDraft, CalendarEventId, CalendarEventStatus,
    CalendarException, CalendarId, CalendarSeries, CalendarSource, CalendarViewKind, DueReminder,
    EventOccurrence, ExpandedEvent, RecurrenceEditScope, RecurrenceError, RecurrenceLimits,
    RecurrenceWarning, due_reminders, expand_event, is_valid_timezone, recurrence_until,
    validate_draft,
};
pub use music::{MusicAction, MusicOp, MusicOrder, MusicOutcome, MusicQueue};
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
pub use error::{AiErrorKind, BoxError, CoreError, LimitKind};
pub use model::{
    Account, AccountId, Address, Attachment, AuthCredential, AuthMethod, Contact, ContactId,
    DateSource, Draft, DraftContent, DraftId, DraftStatus, DraftSyncState, Endpoint, FlagOp, Flags,
    Folder, FolderId, FolderRole, InlineImage, LogSafeMessageSummary, MAX_ATTACHMENT_BYTES,
    MAX_ATTACHMENT_TOTAL_BYTES, MAX_PAGE_SIZE, MAX_PARSE_BYTES, MAX_SAVED_SEARCH_NAME, MessageBody,
    MessageId, MessageSummary, NewMessage, OAuthClientConfig, OAuthProvider, OutgoingMessage,
    SavedSearch, SavedSearchId, SavedSearchQuery, SearchQuery, TlsMode, check_attachment_limits,
    effective_date, format_local_datetime, generate_message_id, make_snippet, mask_email,
    normalize_message_id, now_unix, sort_newest_first,
};
pub use port::{
    AccountRepo, AttachmentRepo, BodyCoverage, BodyJob, BrowserOpener, CalendarRepo,
    DeleteAccountOutcome, DraftRepo, DraftSyncItem, FlagStoreMode, FolderInfo, FolderRepo,
    HeaderFetch, IdleOutcome, ImapCapabilities, ImapSession, ImapSessionFactory, IndexRow,
    MessageRepo, MetaRepo, MoveOrigin, NewDraft, PendingOp, PendingOpKind, PendingRepo,
    ReadOnlyRepo, SearchOutcome, SearchRepo, SelectedFolder, SmtpSender, StorePort, UidRange,
};
pub use search_syntax::{ParsedQuery, parse_search_syntax};

/// 当前 schema 版本。v3 的表结构冻结（`doc/v03架构设计.md` §8.3），
/// v4 / v5 都是**纯加列**（`ai_conversations.model_id`、`ai_messages.plan`），
/// v6 是**纯索引调整**（不改列类型/CHECK/NOT NULL，也不改任何表结构）：
/// 补齐 `ORDER BY` 与索引的列序，让邮件列表、未读计数、正文补全队列、
/// 草稿列表、AI 会话/实体列表都不再退化成临时 B 树排序或全表扫描。
/// v7 是 v0.4 的唯一 DDL 级变更：新增 `contacts` / `saved_searches` 两张表，
/// 给 `messages` 补 `in_reply_to` / `references` 两列。
pub const SCHEMA_VERSION: u32 = 7;

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
    /// 日历功能开关（`doc/v03架构设计.md` D26）。
    ///
    /// **预留键，v0.3 未接线**：本仓库里没有任何读取方，设置页也没有开关，
    /// 写了 `1`/`0` 都不会改变行为。真正的「关掉某模块即隐藏入口并拒命令」
    /// 需要改 `SettingsLoaded` 契约，留给 v0.4（见 `doc/v3收尾项清单.md` D4）。
    /// 保留常量是为了将来接线时键名不用再变。
    pub const FEATURE_CALENDAR: &str = "feature_calendar";
    /// 音乐目录（绝对路径）；缺失表示尚未选择。
    pub const MUSIC_DIR: &str = "music_dir";
    /// 播放顺序：`sequential` / `shuffle`。
    pub const MUSIC_ORDER: &str = "music_order";
    /// 和风天气天气接口 Host。
    pub const WEATHER_API_HOST: &str = "weather_api_host";
    /// 和风天气 API Key（**明文**，与 Host 一起存在 `meta` 里）。
    ///
    /// 用户明确要求：Key 不走系统密钥库，而是跟 Host 一起留在本机数据库，
    /// 这样退出应用再进来仍然有效。代价与 `AI_MODELS` 相同——读得到
    /// `myemail.db` 就读得到 key；补偿手段只有「不进日志、不进事件载荷、
    /// 界面只用暗文显示」这几条。
    pub const WEATHER_API_KEY: &str = "weather_api_key";
    /// 模型档案列表（JSON 数组，元素为 `AiModel`）。
    ///
    /// **含 API Key（明文）**：用户要求 key 与其他调用参数一起保存，所以它就在这个
    /// JSON 里，不走系统密钥库。读得到 `myemail.db` 就读得到 key；补偿手段只有
    /// `AiModel` 的 `Debug` 脱敏与「不得进日志」两条，别在这里加 `Debug` 之外的回显。
    pub const AI_MODELS: &str = "ai_models";
    /// 当前选中的模型档案 id（新对话默认用它）。
    pub const AI_ACTIVE_MODEL: &str = "ai_active_model";
    /// v0.3 早期的单份服务商配置，仅作迁移来源。
    pub const AI_CONFIG: &str = "ai_config";
    /// v0.3 早期的「各协议是否已配置 key」，仅作迁移来源。
    pub const AI_KEY_PRESENT_ANTHROPIC: &str = "ai_key_present_anthropic";
    pub const AI_KEY_PRESENT_OPENAI: &str = "ai_key_present_openai";
    /// 各协议隐私确认时间（JSON：`{"anthropic":<unix>}`）。
    pub const AI_PRIVACY_ACK: &str = "ai_privacy_ack";
    /// 全局工具开关；模型档案自带 `tools` 时以档案为准。
    pub const AI_TOOL_PERMISSIONS: &str = "ai_tool_permissions";
    /// 实体注册表上限。
    pub const AI_MEMORY_MAX_ENTITIES: &str = "ai_memory_max_entities";
    /// AI 功能开关（`doc/v03架构设计.md` D26）。
    ///
    /// **预留键，v0.3 未接线**，与 [`FEATURE_CALENDAR`] 同状态：无读取方、
    /// 无设置入口，改它不会关掉 AI 页。接线工作见 `doc/v3收尾项清单.md` D4。
    pub const FEATURE_AI: &str = "feature_ai";
}
