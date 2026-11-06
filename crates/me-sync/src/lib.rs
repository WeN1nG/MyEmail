//! 服务门面：Command 接收循环、账户/草稿/发送/搜索/维护命令编排。
//!
//! 不依赖 egui，不持有 UI 状态；只通过 `EventSender` 向上层发事件。

pub mod ai;
mod ai_music;
pub mod ai_tools;
pub mod auth;
pub mod calendar;
pub mod drafts;
pub mod engine;
pub mod events;
pub mod indexer;
pub mod oauth_flow;
pub mod tasks;
pub mod watcher;
mod weather;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use me_core::model::{check_attachment_limits, generate_message_id, now_unix};
use me_core::port::{BrowserOpener, ImapSession, SmtpSender, StorePort};
use me_core::{
    APP_NAME, Account, AccountId, AuthCredential, Command, ConnectionState, CoreError,
    DEFAULT_POLL_SECS, DraftContent, DraftId, DraftStatus, ErrorScope, Event, FlagOp, Flags,
    FolderRole, HtmlRendererMode, MAX_POLL_SECS, MIN_POLL_SECS, MessageId, MusicOrder,
    OAuthClientConfig, OAuthProvider, OutgoingMessage, SavedSearch, SavedSearchId,
    SavedSearchQuery, SearchQuery, SendFailureKind, ThemeMode,
};
use me_store::cache::{self, CachePaths};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::ai::AiState;
use crate::auth::TokenManager;
use crate::events::EventSender;
use crate::oauth_flow::OAuthFlowRegistry;
use crate::tasks::{GLOBAL_WATCH_CONNECTIONS, Governor, TaskIds, TaskRegistry};
use crate::weather::WeatherState;

const HEADER_BATCH: u32 = 200;
const FLAG_BATCH: u32 = 500;

pub struct Inner<S, F, M> {
    pub(crate) store: S,
    pub(crate) factory: F,
    pub(crate) smtp: M,
    pub(crate) paths: CachePaths,
    /// 当前生效的附件根目录（可被用户重定位），默认 `paths.attach_dir()`。
    pub(crate) attachment_dir: Mutex<PathBuf>,
    pub(crate) events: EventSender,
    pub(crate) browser: Arc<dyn BrowserOpener>,
    pub(crate) oauth_flows: OAuthFlowRegistry,
    pub(crate) tasks: TaskRegistry,
    pub(crate) governor: Governor,
    /// IDLE 监听连接预算，与操作连接独立计费。
    pub(crate) watch_governor: Governor,
    pub(crate) watchers: Mutex<HashMap<AccountId, CancellationToken>>,
    /// IDLE/轮询触发的同步正在运行；新一轮只置 pending。
    pub(crate) realtime_syncing: Mutex<HashSet<AccountId>>,
    pub(crate) realtime_pending: Mutex<HashSet<AccountId>>,
    pub(crate) manual_syncing: Mutex<HashSet<AccountId>>,
    pub(crate) realtime_enabled: AtomicBool,
    pub(crate) poll_interval_secs: AtomicU64,
    pub(crate) draft_server_sync: AtomicBool,
    pub(crate) draft_bulk_upload_pending: AtomicBool,
    pub(crate) draft_bulk_upload_declined: AtomicBool,
    pub(crate) tokens: TokenManager,
    pub(crate) query_seq: AtomicU64,
    pub(crate) backfill: AtomicBool,
    pub(crate) task_ids: TaskIds,
    pub(crate) sync_gate: Arc<tokio::sync::Mutex<()>>,
    /// 草稿拉取/推送串行，避免并发 APPEND 产生重复版本。
    pub(crate) draft_gate: Arc<tokio::sync::Mutex<()>>,
    /// 附件写入、清理与目录迁移串行，避免迁移期间写入旧目录。
    pub(crate) attach_gate: Arc<tokio::sync::Mutex<()>>,
    pub(crate) backfills: Mutex<HashMap<AccountId, CancellationToken>>,
    /// 天气数据源；未注入时天气命令只提示「尚未初始化」。
    pub(crate) weather_provider: RwLock<Option<Arc<dyn me_core::weather::WeatherProvider>>>,
    /// 会话内的天气状态：Key 兜底、单飞、退避与最小间隔。
    pub(crate) weather_state: Mutex<WeatherState>,
    /// AI 传输层；未注入时 AI 命令提示「尚未初始化」。
    pub(crate) ai_provider: RwLock<Option<Arc<dyn me_core::ai::AiProvider>>>,
    /// 会话内 AI 状态：配置、密钥兜底、隐私确认、进行中的流。
    pub(crate) ai_state: Mutex<AiState>,
    /// AI 音乐工具的等待表：`call_id -> 回执通道`。
    ///
    /// 音乐是唯一不由 `me-sync` 执行的工具（播放器住在 UI 线程侧），
    /// 所以这里存的是「等 UI 回话」的通道，而不是执行结果。
    pub(crate) ai_music_pending:
        Mutex<HashMap<String, tokio::sync::oneshot::Sender<me_core::MusicOutcome>>>,
}

/// 用于比较目录归属：优先规范化（Windows 下同时统一大小写与分隔符），
/// 目标尚不存在时规范化其父目录。
fn comparison_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(canonical_parent) = std::fs::canonicalize(parent)
    {
        return canonical_parent.join(name);
    }
    path.to_path_buf()
}

pub struct SyncService<S, F, M>
where
    S: StorePort + Clone,
    F: me_core::port::ImapSessionFactory + 'static,
    M: SmtpSender + 'static,
{
    inner: Arc<Inner<S, F, M>>,
}

impl<S, F, M> Clone for SyncService<S, F, M>
where
    S: StorePort + Clone,
    F: me_core::port::ImapSessionFactory + 'static,
    M: SmtpSender + 'static,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<S, F, M> SyncService<S, F, M>
where
    S: StorePort + Clone,
    F: me_core::port::ImapSessionFactory + 'static,
    M: SmtpSender + 'static,
{
    pub fn new(
        store: S,
        factory: F,
        smtp: M,
        paths: CachePaths,
        events: EventSender,
        browser: Arc<dyn BrowserOpener>,
    ) -> Self {
        let attachment_dir = paths.attach_dir().to_path_buf();
        Self {
            inner: Arc::new(Inner {
                store,
                factory,
                smtp,
                paths,
                attachment_dir: Mutex::new(attachment_dir),
                events,
                browser,
                oauth_flows: OAuthFlowRegistry::default(),
                tasks: TaskRegistry::default(),
                governor: Governor::default(),
                watch_governor: Governor::new(GLOBAL_WATCH_CONNECTIONS),
                watchers: Mutex::new(HashMap::new()),
                realtime_syncing: Mutex::new(HashSet::new()),
                realtime_pending: Mutex::new(HashSet::new()),
                manual_syncing: Mutex::new(HashSet::new()),
                realtime_enabled: AtomicBool::new(true),
                poll_interval_secs: AtomicU64::new(u64::from(DEFAULT_POLL_SECS)),
                draft_server_sync: AtomicBool::new(true),
                draft_bulk_upload_pending: AtomicBool::new(false),
                draft_bulk_upload_declined: AtomicBool::new(false),
                tokens: TokenManager::new(),
                query_seq: AtomicU64::new(0),
                backfill: AtomicBool::new(true),
                task_ids: TaskIds::default(),
                sync_gate: Arc::new(tokio::sync::Mutex::new(())),
                draft_gate: Arc::new(tokio::sync::Mutex::new(())),
                attach_gate: Arc::new(tokio::sync::Mutex::new(())),
                backfills: Mutex::new(HashMap::new()),
                weather_provider: RwLock::new(None),
                weather_state: Mutex::new(WeatherState::default()),
                ai_provider: RwLock::new(None),
                ai_state: Mutex::new(AiState::default()),
                ai_music_pending: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// 启动命令循环。通道关闭时取消全部任务。
    pub async fn run(self, mut command_rx: mpsc::UnboundedReceiver<Command>) {
        self.load_settings().await;
        self.load_oauth_configs().await;
        self.load_ai_settings().await;
        self.startup();

        while let Some(command) = command_rx.recv().await {
            self.dispatch(command);
        }
        self.inner.tasks.cancel_all();
    }

    async fn load_settings(&self) {
        let loaded = self
            .db(|s| {
                Ok((
                    s.get_setting(me_core::settings_key::BODY_BACKFILL)?,
                    s.get_setting(me_core::settings_key::THEME)?,
                    s.get_setting(me_core::settings_key::REMOTE_IMAGES)?,
                    s.get_setting(me_core::settings_key::REALTIME_SYNC)?,
                    s.get_setting(me_core::settings_key::POLL_INTERVAL_SECS)?,
                    s.get_setting(me_core::settings_key::DRAFT_SERVER_SYNC)?,
                    s.get_setting(me_core::settings_key::DRAFT_BULK_UPLOAD_PENDING)?,
                    s.get_setting(me_core::settings_key::DRAFT_BULK_UPLOAD_DECLINED)?,
                    s.get_setting(me_core::settings_key::PLAINTEXT_NOTICE_ACK)?,
                    s.get_setting(me_core::settings_key::HTML_RENDERER)?,
                    s.get_setting(me_core::settings_key::OAUTH_CLIENT_GOOGLE)?,
                    s.get_setting(me_core::settings_key::OAUTH_CLIENT_MICROSOFT)?,
                ))
            })
            .await
            .ok();
        let (
            backfill,
            theme,
            remote_images,
            realtime_sync,
            poll_interval,
            draft_server_sync,
            bulk_upload_pending,
            bulk_upload_declined,
            plaintext_notice_ack,
            html_renderer,
            oauth_google,
            oauth_microsoft,
        ) = loaded.unwrap_or_default();

        let body_backfill = backfill.as_deref() != Some("false");
        let theme = theme
            .as_deref()
            .map(ThemeMode::parse)
            .unwrap_or(ThemeMode::System);
        let remote_images = remote_images.as_deref() != Some("false");
        let realtime_sync = realtime_sync.as_deref() != Some("false");
        let poll_interval_secs = poll_interval
            .as_deref()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(DEFAULT_POLL_SECS)
            .clamp(MIN_POLL_SECS, MAX_POLL_SECS);
        let draft_server_sync = draft_server_sync.as_deref() != Some("false");
        let draft_bulk_upload_pending = bulk_upload_pending.as_deref() == Some("true");
        let draft_bulk_upload_declined = bulk_upload_declined.as_deref() == Some("true");
        let plaintext_notice_ack = plaintext_notice_ack.as_deref() == Some("true");
        let html_renderer = HtmlRendererMode::parse(html_renderer.as_deref().unwrap_or("enhanced"));
        let parse_client = |raw: Option<String>| {
            raw.and_then(|value| serde_json::from_str::<OAuthClientConfig>(&value).ok())
                .unwrap_or_default()
        };
        let google_client = parse_client(oauth_google);
        let microsoft_client = parse_client(oauth_microsoft);
        let attachment_dir = self
            .db(|s| s.get_setting(me_core::settings_key::ATTACHMENT_DIR))
            .await
            .ok()
            .flatten()
            .map(|value| PathBuf::from(value.trim()))
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| self.inner.paths.attach_dir().to_path_buf());
        if let Ok(mut current) = self.inner.attachment_dir.lock() {
            *current = attachment_dir.clone();
        }

        self.inner.backfill.store(body_backfill, Ordering::Relaxed);
        self.inner
            .realtime_enabled
            .store(realtime_sync, Ordering::Relaxed);
        self.inner
            .poll_interval_secs
            .store(u64::from(poll_interval_secs), Ordering::Relaxed);
        self.inner
            .draft_server_sync
            .store(draft_server_sync, Ordering::Relaxed);
        self.inner
            .draft_bulk_upload_pending
            .store(draft_bulk_upload_pending, Ordering::Relaxed);
        self.inner
            .draft_bulk_upload_declined
            .store(draft_bulk_upload_declined, Ordering::Relaxed);

        let music = self
            .db(|s| {
                Ok((
                    s.get_setting(me_core::settings_key::MUSIC_DIR)?,
                    s.get_setting(me_core::settings_key::MUSIC_ORDER)?,
                ))
            })
            .await
            .ok()
            .unwrap_or_default();
        let music_dir = music
            .0
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty());
        let music_order = music
            .1
            .as_deref()
            .and_then(MusicOrder::parse)
            .unwrap_or_default();

        // 天气：读回 Host 与 Key（两者都存在本机 `meta`，退出再进来照样有效）。
        // 只有一个 Host：官方已把城市搜索与天气收进同一个专属域名。
        let stored_host = self
            .db(|s| s.get_setting(me_core::settings_key::WEATHER_API_HOST))
            .await
            .ok()
            .flatten();
        let api_host = stored_host
            .as_deref()
            .and_then(|value| me_core::weather::normalize_host(value).ok())
            .unwrap_or_else(|| me_proto::qweather::DEFAULT_API_HOST.to_string());
        // 记录会话里生效的 Host：之后每次请求都用它。
        if let Ok(mut state) = self.inner.weather_state.lock() {
            state.api_host = api_host.clone();
        }
        // 已经注入过数据源（例如测试或自定义实现）时不要覆盖。
        let provider_ready = self
            .inner
            .weather_provider
            .read()
            .map(|guard| guard.is_some())
            .unwrap_or(false);
        if !provider_ready
            && let Ok(provider) = me_proto::qweather::QWeatherProvider::new()
            && let Ok(mut guard) = self.inner.weather_provider.write()
        {
            *guard = Some(Arc::new(provider));
        }
        let weather_key_present = self.weather_credential().await.is_some();

        self.emit(Event::SettingsLoaded {
            theme,
            body_backfill,
            remote_images,
            realtime_sync,
            poll_interval_secs,
            draft_server_sync,
            draft_bulk_upload_pending,
            plaintext_notice_ack,
            html_renderer,
            oauth_google_client_id: google_client.client_id,
            oauth_microsoft_client_id: microsoft_client.client_id,
            oauth_microsoft_tenant: microsoft_client.tenant.unwrap_or_default(),
            attachment_dir,
            music_dir,
            music_order,
        })
        .await;

        // 天气配置状态单独发一个事件：界面据此决定显示「配置引导」还是天气内容。
        self.emit(Event::WeatherConfigChanged {
            key_present: weather_key_present,
            api_host,
        })
        .await;
    }

    /// 构建期客户端 + meta 中的 `client_id` / tenant 覆盖。
    async fn load_oauth_configs(&self) {
        let loaded = self
            .db(|s| {
                Ok((
                    s.get_setting(me_core::settings_key::OAUTH_CLIENT_GOOGLE)?,
                    s.get_setting(me_core::settings_key::OAUTH_CLIENT_MICROSOFT)?,
                ))
            })
            .await
            .ok();
        let (google, microsoft) = loaded.unwrap_or_default();
        for (provider, raw) in [
            (OAuthProvider::Google, google),
            (OAuthProvider::Microsoft, microsoft),
        ] {
            let override_config = raw.and_then(|value| match serde_json::from_str(&value) {
                Ok(config) => Some(config),
                Err(err) => {
                    tracing::warn!(provider = provider.as_str(), %err, "invalid OAuth client override; ignoring");
                    None
                }
            });
            self.apply_oauth_provider(provider, override_config);
        }
    }

    fn apply_oauth_provider(
        &self,
        provider: OAuthProvider,
        override_config: Option<OAuthClientConfig>,
    ) {
        let mut config = me_proto::oauth::ProviderConfig::from_build_time(provider);
        if config.is_none()
            && let Some(client_id) = override_config
                .as_ref()
                .map(|value| value.client_id.trim())
                .filter(|value| !value.is_empty())
        {
            config = Some(me_proto::oauth::ProviderConfig::for_provider(
                provider, client_id,
            ));
        }
        let Some(mut config) = config else {
            tracing::info!(
                provider = provider.as_str(),
                "no build-time OAuth client and no advanced override"
            );
            return;
        };
        if let Some(override_config) = override_config
            && let Err(err) = config.apply_override(&override_config)
        {
            tracing::warn!(
                provider = provider.as_str(),
                error = %err.user_message(),
                "OAuth client override rejected"
            );
            return;
        }
        self.inner.tokens.configure_provider(config);
    }

    async fn handle_set_oauth_client(&self, provider: OAuthProvider, config: OAuthClientConfig) {
        let mut base =
            me_proto::oauth::ProviderConfig::from_build_time(provider).unwrap_or_else(|| {
                me_proto::oauth::ProviderConfig::for_provider(
                    provider,
                    config.client_id.trim().to_string(),
                )
            });
        if let Err(err) = base.apply_override(&config) {
            self.emit(Event::Error {
                scope: ErrorScope::Global,
                message: err.user_message(),
            })
            .await;
            return;
        }

        let key = match provider {
            OAuthProvider::Google => me_core::settings_key::OAUTH_CLIENT_GOOGLE,
            OAuthProvider::Microsoft => me_core::settings_key::OAUTH_CLIENT_MICROSOFT,
        };
        let value = match serde_json::to_string(&config) {
            Ok(value) => value,
            Err(_) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Global,
                    message: "OAuth 客户端设置无法序列化".into(),
                })
                .await;
                return;
            }
        };
        if let Err(err) = self.db(move |s| s.set_setting(key, &value)).await {
            self.emit(Event::Error {
                scope: ErrorScope::Global,
                message: err.user_message(),
            })
            .await;
            return;
        }
        self.inner.tokens.configure_provider(base);
    }

    /// 启动时清理没有对应 drafts 行的目录（崩溃残留）。
    async fn cleanup_orphan_drafts(&self) {
        let drafts = match self.db(|s| s.list_drafts(None)).await {
            Ok(drafts) => drafts,
            Err(_) => return,
        };
        let valid: Vec<DraftId> = drafts.into_iter().map(|d| d.id).collect();
        let paths = self.inner.paths.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let freed = cache::cleanup_orphan_draft_dirs(&paths, &valid);
            if freed > 0 {
                tracing::info!(freed, "removed orphan draft directories");
            }
        })
        .await;
    }

    fn startup(&self) {
        // 日历与邮件无关：不等账户同步，先保证默认日历存在并启动提醒循环。
        self.start_reminder_loop();
        {
            let service = self.clone();
            tokio::spawn(async move { service.calendar_bootstrap().await });
        }

        let service = self.clone();
        tokio::spawn(async move {
            match service.db(|s| s.list_accounts()).await {
                Ok(accounts) => {
                    service
                        .emit(Event::AccountsLoaded {
                            accounts: accounts.clone(),
                        })
                        .await;
                    service.repair_legacy_headers().await;
                    service.cleanup_orphan_drafts().await;
                    // 多账户串行同步；sync_gate 之外再加账户级取消令牌。
                    for account in accounts {
                        service.cancel_backfill(account.id);
                        let token = service.inner.tasks.begin(account.id);
                        service.sync_account(account.id, None, token).await;
                        // 首次同步完成后再启动 watcher，避免同账户同时建两条连接。
                        service.start_watcher(account.id);
                    }
                }
                Err(err) => {
                    service
                        .emit(Event::Error {
                            scope: ErrorScope::Global,
                            message: err.user_message(),
                        })
                        .await;
                }
            }
        });
    }

    /// 修复旧版本遗留的头部/正文/附件名乱码。
    ///
    /// v0.2.0 早期构建未启用 `mail-parser` 的 `full_encoding`，GBK/GB18030 邮件
    /// 会被按 UTF-8 解码成 U+FFFD。这里只处理摘要或附件名中确实含替换字符、且仍有
    /// 原始邮件缓存的记录：重新解析头部与正文，覆盖摘要、正文文件、snippet、FTS
    /// 与附件元数据（`upsert_attachments` 不会覆盖已下载的 `local_path`）。
    async fn repair_legacy_headers(&self) {
        let Ok(candidates) = self.db(|s| s.messages_with_broken_headers(500)).await else {
            return;
        };
        if candidates.is_empty() {
            return;
        }

        let mut updated = Vec::new();
        for summary in candidates {
            let id = summary.id;
            let paths = self.inner.paths.clone();
            let parsed = tokio::task::spawn_blocking(move || {
                let raw = cache::read_raw_gz(&paths, id).ok().flatten()?;
                let header = me_proto::mime::parse_headers(&raw).ok()?;
                let body = me_proto::mime::parse_body(&raw).ok()?;
                Some((header, body))
            })
            .await
            .ok()
            .flatten();
            let Some((header, body)) = parsed else {
                continue;
            };

            let snippet = me_core::make_snippet(&body.text);
            let text = body.text.clone();
            let attachments = body.attachments;
            let paths = self.inner.paths.clone();
            let write =
                tokio::task::spawn_blocking(move || cache::write_body(&paths, id, &text)).await;
            let Ok(Ok(body_path)) = write else {
                continue;
            };
            let body_path = body_path.to_string_lossy().to_string();

            let result = self
                .db(move |s| {
                    s.update_message_headers(
                        id,
                        header.subject.as_deref(),
                        header.from.as_ref(),
                        &header.to,
                        &header.cc,
                    )?;
                    if !attachments.is_empty() {
                        s.upsert_attachments(id, &attachments)?;
                    }
                    s.set_body_cached(id, &body_path, &body.text, Some(&snippet))?;
                    s.get_message(id)
                })
                .await;
            if let Ok(Some(summary)) = result {
                updated.push(summary);
            }
        }

        if !updated.is_empty() {
            self.emit(Event::MessagesUpdated { messages: updated })
                .await;
        }
    }

    // -----------------------------------------------------------------------
    // 工具
    // -----------------------------------------------------------------------

    /// 当前生效的附件根目录（默认或用户重定位后的目录）。
    pub(crate) fn attachment_dir(&self) -> PathBuf {
        self.inner
            .attachment_dir
            .lock()
            .map(|dir| dir.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    pub(crate) async fn db<T>(
        &self,
        f: impl FnOnce(S) -> Result<T, CoreError> + Send + 'static,
    ) -> Result<T, CoreError>
    where
        T: Send + 'static,
    {
        let store = self.inner.store.clone();
        match tokio::task::spawn_blocking(move || f(store)).await {
            Ok(result) => result,
            Err(join) => Err(CoreError::storage(format!("数据库任务失败：{join}"))),
        }
    }

    pub(crate) async fn emit(&self, event: Event) {
        if event.is_progress() {
            self.inner.events.send_progress(event);
        } else {
            self.inner.events.send(event).await;
        }
    }

    pub(crate) async fn status(&self, account: AccountId, status: ConnectionState) {
        self.emit(Event::AccountStatus {
            id: account,
            status,
        })
        .await;
    }

    pub(crate) async fn account(&self, id: AccountId) -> Result<Account, CoreError> {
        self.db(move |s| s.get_account(id))
            .await?
            .ok_or_else(|| CoreError::storage(format!("账户 {id} 不存在")))
    }

    pub(crate) async fn credential(&self, account: &Account) -> Result<AuthCredential, CoreError> {
        match self.inner.tokens.resolve(account).await {
            Ok(credential) => {
                if let AuthCredential::OAuth2 { expires_at, .. } = &credential {
                    let id = account.id;
                    let expires_at = *expires_at;
                    let _ = self
                        .db(move |s| {
                            s.set_account_reauth(id, false)?;
                            s.set_account_token_expiry(id, Some(expires_at))
                        })
                        .await;
                }
                Ok(credential)
            }
            Err(err) if err.needs_reauth() => {
                let id = account.id;
                let _ = self.db(move |s| s.set_account_reauth(id, true)).await;
                if !account.needs_reauth {
                    self.emit(Event::OAuthReauthRequired { account: id }).await;
                }
                Err(err)
            }
            Err(err) => Err(err),
        }
    }

    /// 保存密码/授权码凭证：先写会话内存；返回是否成功写入系统密钥库。
    pub(crate) async fn store_credential(&self, account: &Account, secret: &str) -> bool {
        self.inner.tokens.store_password(account, secret).await
    }

    async fn delete_credential(&self, account: &Account) {
        self.inner.tokens.delete(account).await;
    }

    // -----------------------------------------------------------------------
    // 命令分发
    // -----------------------------------------------------------------------

    fn dispatch(&self, command: Command) {
        match command {
            Command::AddAccount(draft) => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_add_account(draft).await });
            }
            Command::UpdateAccount { id, draft } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_update_account(id, draft).await });
            }
            Command::DeleteAccount { id, delete_drafts } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_delete_account(id, delete_drafts).await });
            }
            Command::TestConnection(id) => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_test_connection(id).await });
            }
            Command::SyncAccount(id) => self.sync_account_task(id, None),
            Command::SyncFolder { account, folder } => {
                self.sync_account_task(account, Some(folder))
            }
            Command::CancelSync(id) => self.inner.tasks.cancel(id),
            Command::OpenMessage(id) => {
                let service = self.clone();
                tokio::spawn(async move { service.open_message(id).await });
            }
            Command::DownloadAttachment {
                message,
                part_index,
            } => {
                let service = self.clone();
                tokio::spawn(async move { service.download_attachment(message, part_index).await });
            }
            Command::SetFlags { ids, flags } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_set_flags(ids, flags).await });
            }
            Command::MoveToTrash(ids) => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_move_to_trash(ids).await });
            }
            Command::EmptyTrash(folder) => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_empty_trash(folder).await });
            }
            Command::SaveDraft(outgoing) => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_save_draft(outgoing).await });
            }
            Command::SaveDraftToServer(id) => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_save_draft_to_server(id).await });
            }
            Command::LoadDraft(id) => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_load_draft(id).await });
            }
            Command::Send(outgoing) => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_send(outgoing).await });
            }
            Command::RetryArchive(id) => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_retry_archive(id).await });
            }
            Command::Search(query) => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_search(query).await });
            }
            Command::SaveSearch { name, query } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_save_search(name, query).await });
            }
            Command::DeleteSearch { id } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_delete_search(id).await });
            }
            Command::RunSavedSearch { id } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_run_saved_search(id).await });
            }
            Command::SetTheme(mode) => {
                let service = self.clone();
                tokio::spawn(async move {
                    let _ = service
                        .db(move |s| s.set_setting(me_core::settings_key::THEME, mode.as_str()))
                        .await;
                });
            }
            Command::SetBodyBackfill { enabled } => {
                self.inner.backfill.store(enabled, Ordering::Relaxed);
                let service = self.clone();
                tokio::spawn(async move {
                    let value = if enabled { "true" } else { "false" };
                    let _ = service
                        .db(move |s| s.set_setting(me_core::settings_key::BODY_BACKFILL, value))
                        .await;
                });
            }
            Command::SetRemoteImages { enabled } => {
                let service = self.clone();
                tokio::spawn(async move {
                    let value = if enabled { "true" } else { "false" };
                    let _ = service
                        .db(move |s| s.set_setting(me_core::settings_key::REMOTE_IMAGES, value))
                        .await;
                });
            }
            Command::AcknowledgePlaintextNotice => {
                let service = self.clone();
                tokio::spawn(async move {
                    let _ = service
                        .db(|s| s.set_setting(me_core::settings_key::PLAINTEXT_NOTICE_ACK, "true"))
                        .await;
                });
            }
            Command::RebuildIndex => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_rebuild_index().await });
            }
            Command::ClearCache(account) => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_clear_cache(account).await });
            }
            Command::SetAttachmentDir { path } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_set_attachment_dir(path).await });
            }
            Command::SetFolderRole { folder, role } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_set_folder_role(folder, role).await });
            }
            Command::AcknowledgeFolderRemoval(folder) => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_acknowledge_folder(folder).await });
            }
            Command::SetRealtimeSync { enabled } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_set_realtime(enabled).await });
            }
            Command::SetPollInterval { secs } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_set_poll_interval(secs).await });
            }
            Command::SetAccountRealtime {
                account,
                enabled,
                poll_interval_secs,
            } => {
                let service = self.clone();
                tokio::spawn(async move {
                    service
                        .handle_set_account_realtime(account, enabled, poll_interval_secs)
                        .await
                });
            }
            Command::SetDraftServerSync { enabled } => {
                self.inner
                    .draft_server_sync
                    .store(enabled, Ordering::Relaxed);
                let service = self.clone();
                tokio::spawn(async move {
                    let value = if enabled { "true" } else { "false" };
                    let _ = service
                        .db(move |s| s.set_setting(me_core::settings_key::DRAFT_SERVER_SYNC, value))
                        .await;
                });
            }
            Command::SetHtmlRenderer { mode } => {
                let service = self.clone();
                tokio::spawn(async move {
                    let value = mode.as_str();
                    let _ = service
                        .db(move |s| s.set_setting(me_core::settings_key::HTML_RENDERER, value))
                        .await;
                });
            }
            Command::SetOAuthClient { provider, config } => {
                let service = self.clone();
                tokio::spawn(
                    async move { service.handle_set_oauth_client(provider, config).await },
                );
            }
            Command::StartOAuth {
                provider,
                mode,
                account,
                email_hint,
            } => {
                let service = self.clone();
                tokio::spawn(async move {
                    service
                        .handle_start_oauth(provider, mode, account, email_hint)
                        .await
                });
            }
            Command::CancelOAuth { flow_id } => {
                if flow_id == 0 {
                    self.inner.oauth_flows.cancel_all();
                } else {
                    self.inner.oauth_flows.cancel(flow_id);
                }
            }
            Command::DeleteDraft { id } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_delete_draft(id).await });
            }
            Command::RetryDraftSync { draft } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_retry_draft_sync(draft).await });
            }
            Command::ResolveDraftConflict { draft, resolution } => {
                let service = self.clone();
                tokio::spawn(
                    async move { service.handle_resolve_conflict(draft, resolution).await },
                );
            }
            Command::ResolveDraftDeletion { draft, resolution } => {
                let service = self.clone();
                tokio::spawn(
                    async move { service.handle_resolve_deletion(draft, resolution).await },
                );
            }
            Command::ConfirmDraftBulkUpload { accept } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_confirm_bulk_upload(accept).await });
            }
            // 日历：统一转到用例入口，dispatch 不做任何业务判断（§2.7）。
            cmd @ (Command::CreateCalendarEvent { .. }
            | Command::UpdateCalendarEvent { .. }
            | Command::DeleteCalendarEvent { .. }
            | Command::RestoreCalendarEvent { .. }
            | Command::CreateCalendar { .. }
            | Command::SetCalendarVisible { .. }) => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_calendar_command(cmd).await });
            }
            // 音乐设置只落 meta；播放由 UI 直接驱动音频线程（§5.5.3）。
            Command::AiMusicResult { call_id, outcome } => {
                self.complete_music_op(&call_id, outcome);
            }
            Command::SetMusicSettings { dir, order } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_set_music_settings(dir, order).await });
            }

            // ---- 天气（v0.3）----
            // 天气命令都要等网络响应，统一 spawn 出去，不阻塞命令循环。
            Command::SetWeatherConfig { host, key } => {
                let service = self.clone();
                tokio::spawn(async move { service.set_weather_config(host, key).await });
            }
            Command::TestWeatherConnection { host, key } => {
                let service = self.clone();
                tokio::spawn(async move { service.test_weather_connection(host, key).await });
            }
            Command::SearchWeatherLocations { request_id, query } => {
                let service = self.clone();
                // 派发时同步记录请求号：先发的请求可能先返回，
                // 只有同步记录才能保证旧结果被丢弃。
                service.note_weather_search(request_id);
                tokio::spawn(
                    async move { service.search_weather_locations(request_id, query).await },
                );
            }
            Command::AddWeatherLocation { candidate } => {
                let service = self.clone();
                tokio::spawn(async move { service.add_weather_location(candidate).await });
            }
            Command::RemoveWeatherLocation { id } => {
                let service = self.clone();
                tokio::spawn(async move { service.remove_weather_location(id).await });
            }
            Command::SetDefaultWeatherLocation { id } => {
                let service = self.clone();
                tokio::spawn(async move { service.set_default_weather_location(id).await });
            }
            Command::RefreshWeather { id, force } => {
                let service = self.clone();
                tokio::spawn(async move { service.refresh_weather(id, force).await });
            }

            // ---- AI（v0.3）----
            // 与天气同理：可能等网络，一律 spawn，不阻塞命令循环。
            Command::CreateAiConversation { model } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_create_ai_conversation(model).await });
            }
            Command::RenameAiConversation { id, title } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_rename_ai_conversation(id, title).await });
            }
            Command::DeleteAiConversation { id } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_delete_ai_conversation(id).await });
            }
            Command::ClearAiConversations => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_clear_ai_conversations().await });
            }
            Command::SendAiMessage {
                conversation,
                content,
                context,
            } => {
                let service = self.clone();
                tokio::spawn(async move {
                    service
                        .handle_send_ai_message(conversation, content, context)
                        .await
                });
            }
            Command::CancelAiStream { conversation } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_cancel_ai_stream(conversation).await });
            }
            Command::RegenerateAiMessage { conversation } => {
                let service = self.clone();
                tokio::spawn(
                    async move { service.handle_regenerate_ai_message(conversation).await },
                );
            }
            Command::SaveAiModel { model } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_save_ai_model(model).await });
            }
            Command::DeleteAiModel { id } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_delete_ai_model(id).await });
            }
            Command::SetActiveAiModel { id } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_set_active_ai_model(id).await });
            }
            Command::SetConversationAiModel {
                conversation,
                model,
            } => {
                let service = self.clone();
                tokio::spawn(async move {
                    service
                        .handle_set_conversation_ai_model(conversation, model)
                        .await
                });
            }
            Command::TestAiModel { model } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_test_ai_model(model).await });
            }
            Command::SetAiPrivacyAck { provider, accepted } => {
                let service = self.clone();
                tokio::spawn(
                    async move { service.handle_set_ai_privacy_ack(provider, accepted).await },
                );
            }
            Command::UndoAiCalendarCreate { event } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_undo_ai_calendar_create(event).await });
            }
            Command::DeleteAiEntity { entity } => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_delete_ai_entity(entity).await });
            }
            Command::ClearAiEntities => {
                let service = self.clone();
                tokio::spawn(async move { service.handle_clear_ai_entities().await });
            }
        }
    }

    /// 写音乐目录/顺序。失败只提示，不影响播放（播放不依赖 DB）。
    async fn handle_set_music_settings(&self, dir: Option<PathBuf>, order: Option<MusicOrder>) {
        // `db` 的闭包要求 'static，先把值转成可 move 的字符串。
        let dir = dir.map(|path| path.to_string_lossy().to_string());
        let order = order.map(|order| order.as_str().to_string());
        let result = self
            .db(move |s| {
                if let Some(dir) = &dir {
                    s.set_setting(me_core::settings_key::MUSIC_DIR, dir)?;
                }
                if let Some(order) = &order {
                    s.set_setting(me_core::settings_key::MUSIC_ORDER, order)?;
                }
                Ok(())
            })
            .await;
        if let Err(err) = result {
            self.emit(Event::Error {
                scope: ErrorScope::Global,
                message: err.user_message(),
            })
            .await;
        }
    }

    fn sync_account_task(&self, account: AccountId, only_folder: Option<me_core::FolderId>) {
        // 手动同步幂等合并：同账户已在同步时不再叠加任务。
        {
            let mut manual = self
                .inner
                .manual_syncing
                .lock()
                .expect("manual syncing poisoned");
            if !manual.insert(account) {
                return;
            }
        }
        // 手动/启动同步优先于后台补全：先停掉该账户的补全，腾出连接。
        self.cancel_backfill(account);
        let token = self.inner.tasks.begin(account);
        let task_id = self.inner.task_ids.next();
        let service = self.clone();
        tokio::spawn(async move {
            tracing::info!(task = %task_id, account = account.0, "sync task started");
            service.sync_account(account, only_folder, token).await;
            service
                .inner
                .manual_syncing
                .lock()
                .expect("manual syncing poisoned")
                .remove(&account);
        });
    }

    pub(crate) fn start_backfill(&self, account: AccountId) {
        let token = CancellationToken::new();
        if let Some(old) = self
            .inner
            .backfills
            .lock()
            .expect("backfills poisoned")
            .insert(account, token.clone())
        {
            old.cancel();
        }
        let service = self.clone();
        tokio::spawn(async move { service.backfill(account, token).await });
    }

    pub(crate) fn cancel_backfill(&self, account: AccountId) {
        if let Some(token) = self
            .inner
            .backfills
            .lock()
            .expect("backfills poisoned")
            .get(&account)
        {
            token.cancel();
        }
    }

    // -----------------------------------------------------------------------
    // 账户
    // -----------------------------------------------------------------------

    async fn handle_add_account(&self, draft: me_core::AccountDraft) {
        if let Err(err) = draft.validate() {
            self.emit(Event::Error {
                scope: ErrorScope::Global,
                message: err.user_message(),
            })
            .await;
            return;
        }
        let secret = draft.auth_secret.clone();
        match self.db(move |s| s.create_account(&draft)).await {
            Ok(account) => {
                let mut keyring_ok = true;
                if let Some(secret) = secret {
                    keyring_ok = self.store_credential(&account, &secret).await;
                }
                let account_id = account.id;
                self.emit(Event::AccountAdded { account }).await;
                if !keyring_ok {
                    self.emit(Event::CredentialNotPersisted {
                        account: account_id,
                        created: true,
                    })
                    .await;
                }
            }
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Global,
                    message: format!("添加账户失败：{}", err.user_message()),
                })
                .await;
            }
        }
    }

    async fn handle_update_account(&self, id: AccountId, draft: me_core::AccountDraft) {
        if let Err(err) = draft.validate() {
            self.emit(Event::Error {
                scope: ErrorScope::Account(id),
                message: err.user_message(),
            })
            .await;
            return;
        }
        let secret = draft.auth_secret.clone();
        match self.db(move |s| s.update_account(id, &draft)).await {
            Ok(account) => {
                let mut keyring_ok = true;
                if let Some(secret) = secret {
                    keyring_ok = self.store_credential(&account, &secret).await;
                }
                let account_id = account.id;
                self.emit(Event::AccountUpdated { account }).await;
                if !keyring_ok {
                    self.emit(Event::CredentialNotPersisted {
                        account: account_id,
                        created: false,
                    })
                    .await;
                }
            }
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Account(id),
                    message: format!("更新账户失败：{}", err.user_message()),
                })
                .await;
            }
        }
    }

    async fn handle_delete_account(&self, id: AccountId, delete_drafts: bool) {
        let account = self.account(id).await.ok();
        match self.db(move |s| s.delete_account(id, delete_drafts)).await {
            Ok(me_core::port::DeleteAccountOutcome::Deleted) => {
                self.inner.tasks.cancel(id);
                self.stop_watcher(id);
                if let Some(account) = &account {
                    self.delete_credential(account).await;
                }
                self.emit(Event::AccountRemoved { id }).await;
            }
            Ok(me_core::port::DeleteAccountOutcome::RefusedDraftsExist(count)) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Account(id),
                    message: format!("该账户仍有 {count} 封草稿，已拒绝删除。请先导出或清理草稿。"),
                })
                .await;
            }
            Ok(me_core::port::DeleteAccountOutcome::NotFound) => {
                self.emit(Event::AccountRemoved { id }).await;
            }
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Account(id),
                    message: err.user_message(),
                })
                .await;
            }
        }
    }

    async fn handle_test_connection(&self, id: AccountId) {
        let account = match self.account(id).await {
            Ok(account) => account,
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Account(id),
                    message: err.user_message(),
                })
                .await;
                return;
            }
        };
        let secret = match self.credential(&account).await {
            Ok(secret) => secret,
            Err(err) => {
                self.emit(Event::ConnectionTested {
                    account: id,
                    imap_ok: false,
                    smtp_ok: false,
                    detail: Some(err.user_message()),
                })
                .await;
                return;
            }
        };

        let imap = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.inner.factory.connect(&account, &secret),
        )
        .await;
        let (imap_ok, imap_detail) = match imap {
            Ok(Ok(mut session)) => {
                let _ = session.logout().await;
                (true, None)
            }
            Ok(Err(err)) => (false, Some(err.user_message())),
            Err(_) => (false, Some("IMAP 连接超时".to_string())),
        };

        let smtp = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            me_proto::smtp::verify(&account, &secret),
        )
        .await;
        let (smtp_ok, smtp_detail) = match smtp {
            Ok(Ok(())) => (true, None),
            Ok(Err(err)) => (false, Some(err.user_message())),
            Err(_) => (false, Some("SMTP 连接超时".to_string())),
        };

        let detail = [imap_detail, smtp_detail]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        self.emit(Event::ConnectionTested {
            account: id,
            imap_ok,
            smtp_ok,
            detail: if detail.is_empty() {
                None
            } else {
                Some(detail.join("；"))
            },
        })
        .await;
    }

    // -----------------------------------------------------------------------
    // 标志 / 删除
    // -----------------------------------------------------------------------

    async fn handle_set_flags(&self, ids: Vec<MessageId>, op: FlagOp) {
        if ids.is_empty() {
            return;
        }
        let account = match self
            .db({
                let ids = ids.clone();
                move |s| {
                    let mut account = None;
                    for id in ids {
                        if let Some(message) = s.get_message(id)? {
                            account = Some(message.account);
                            break;
                        }
                    }
                    Ok(account)
                }
            })
            .await
        {
            Ok(account) => account,
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Global,
                    message: err.user_message(),
                })
                .await;
                return;
            }
        };

        match self.db(move |s| s.apply_local_flags(&ids, op)).await {
            Ok(changed) => {
                if !changed.is_empty() {
                    self.emit(Event::FlagsChanged { items: changed }).await;
                }
                if let Some(account) = account {
                    let service = self.clone();
                    let token = CancellationToken::new();
                    tokio::spawn(async move { service.flush_flags(account, token).await });
                }
            }
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Global,
                    message: err.user_message(),
                })
                .await;
            }
        }
    }

    async fn handle_move_to_trash(&self, ids: Vec<MessageId>) {
        if ids.is_empty() {
            return;
        }

        let mut account = None;
        let mut messages = Vec::new();
        for &id in &ids {
            match self.db(move |s| s.get_message(id)).await {
                Ok(Some(message)) => {
                    account = Some(message.account);
                    messages.push(message);
                }
                Ok(None) => {}
                Err(err) => {
                    self.emit(Event::Error {
                        scope: ErrorScope::Message(id),
                        message: err.user_message(),
                    })
                    .await;
                }
            }
        }
        let Some(account) = account else { return };

        let folders = match self.db(move |s| s.list_folders(account)).await {
            Ok(folders) => folders,
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Account(account),
                    message: err.user_message(),
                })
                .await;
                return;
            }
        };
        let trash = folders
            .iter()
            .find(|f| f.role == FolderRole::Trash)
            .cloned();
        let role_of = |folder: me_core::FolderId| {
            folders
                .iter()
                .find(|f| f.id == folder)
                .map(|f| f.role)
                .unwrap_or(FolderRole::Other)
        };

        // 废件箱内的再次删除 = 永久删除。
        let permanent: Vec<MessageId> = messages
            .iter()
            .filter(|m| role_of(m.folder) == FolderRole::Trash)
            .map(|m| m.id)
            .collect();
        let move_messages: Vec<_> = messages
            .iter()
            .filter(|m| role_of(m.folder) != FolderRole::Trash)
            .cloned()
            .collect();

        if !permanent.is_empty() {
            match self.supports_uidplus(account).await {
                Ok(true) => {}
                Ok(false) => {
                    self.emit(Event::Error {
                        scope: ErrorScope::Account(account),
                        message: "服务器不支持 UIDPLUS，已禁用永久删除".into(),
                    })
                    .await;
                    return;
                }
                Err(err) => {
                    self.emit(Event::Error {
                        scope: ErrorScope::Account(account),
                        message: format!("无法确认服务器能力：{}", err.user_message()),
                    })
                    .await;
                    return;
                }
            }
            let permanent_folder = messages
                .iter()
                .find(|m| role_of(m.folder) == FolderRole::Trash)
                .map(|m| m.folder);
            let permanent_for_db = permanent.clone();
            match self
                .db(move |s| s.delete_permanent(&permanent_for_db))
                .await
            {
                Ok(_) => {
                    if let Some(folder) = permanent_folder {
                        self.emit(Event::MessagesRemoved {
                            folder,
                            ids: permanent.clone(),
                        })
                        .await;
                    }
                    let token = CancellationToken::new();
                    let service = self.clone();
                    tokio::spawn(async move { service.execute_pending_ops(account, token).await });
                }
                Err(err) => {
                    self.emit(Event::Error {
                        scope: ErrorScope::Account(account),
                        message: err.user_message(),
                    })
                    .await;
                }
            }
        }

        if move_messages.is_empty() {
            return;
        }
        let Some(trash) = trash else {
            self.emit(Event::Error {
                scope: ErrorScope::Account(account),
                message:
                    "该账户没有废件箱角色，无法删除。请先在服务器端创建废件箱，或手动指定角色。"
                        .into(),
            })
            .await;
            return;
        };

        let origins: Vec<me_core::port::MoveOrigin> = move_messages
            .iter()
            .filter_map(|message| {
                let uid = message.uid?;
                let folder = folders.iter().find(|f| f.id == message.folder);
                Some(me_core::port::MoveOrigin {
                    message: message.id,
                    source_folder: message.folder,
                    source_folder_name: folder.map(|f| f.name.clone()).unwrap_or_default(),
                    source_uid: uid,
                })
            })
            .collect();
        let source_folders: Vec<me_core::FolderId> =
            origins.iter().map(|o| o.source_folder).collect();

        match self.db(move |s| s.move_to_trash(&origins, trash.id)).await {
            Ok(()) => {
                let moved_ids: Vec<MessageId> = move_messages.iter().map(|m| m.id).collect();
                for folder in source_folders {
                    self.emit(Event::MessagesRemoved {
                        folder,
                        ids: moved_ids.clone(),
                    })
                    .await;
                }
                let token = CancellationToken::new();
                let service = self.clone();
                tokio::spawn(async move { service.execute_pending_ops(account, token).await });
            }
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Account(account),
                    message: err.user_message(),
                })
                .await;
            }
        }
    }

    /// 连接一次服务器确认是否支持 UIDPLUS（永久删除的前置条件）。
    async fn supports_uidplus(&self, account_id: AccountId) -> Result<bool, CoreError> {
        let account = self.account(account_id).await?;
        let secret = self.credential(&account).await?;
        let _guard = self.inner.governor.acquire(account_id).await?;
        let mut session = self.inner.factory.connect(&account, &secret).await?;
        let supported = session.capabilities().uidplus;
        let _ = session.logout().await;
        Ok(supported)
    }

    async fn handle_empty_trash(&self, folder: me_core::FolderId) {
        let folder_struct = match self.db(move |s| s.get_folder(folder)).await {
            Ok(Some(folder)) => folder,
            Ok(None) => return,
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Folder(folder),
                    message: err.user_message(),
                })
                .await;
                return;
            }
        };
        if folder_struct.role != FolderRole::Trash {
            self.emit(Event::Error {
                scope: ErrorScope::Folder(folder),
                message: "只能清空废件箱文件夹".into(),
            })
            .await;
            return;
        }

        match self.supports_uidplus(folder_struct.account).await {
            Ok(true) => {}
            Ok(false) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Folder(folder),
                    message: "服务器不支持 UIDPLUS，已禁用清空废件箱".into(),
                })
                .await;
                return;
            }
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Folder(folder),
                    message: format!("无法确认服务器能力：{}", err.user_message()),
                })
                .await;
                return;
            }
        }

        let mut ids = Vec::new();
        let mut cursor = None;
        loop {
            let page = self
                .db(move |s| me_core::port::MessageRepo::list_messages(&s, folder, cursor, 200))
                .await;
            match page {
                Ok(messages) if messages.is_empty() => break,
                Ok(messages) => {
                    if messages.len() < 200 {
                        ids.extend(messages.into_iter().map(|m| m.id));
                        break;
                    }
                    cursor = messages.last().map(|m| (m.date.unwrap_or(0), m.id.0));
                    ids.extend(messages.into_iter().map(|m| m.id));
                }
                Err(err) => {
                    self.emit(Event::Error {
                        scope: ErrorScope::Folder(folder),
                        message: err.user_message(),
                    })
                    .await;
                    return;
                }
            }
        }

        if ids.is_empty() {
            return;
        }
        let ids_for_db = ids.clone();
        match self.db(move |s| s.delete_permanent(&ids_for_db)).await {
            Ok(_) => {
                self.emit(Event::MessagesRemoved {
                    folder,
                    ids: ids.clone(),
                })
                .await;
                let token = CancellationToken::new();
                let service = self.clone();
                tokio::spawn(async move {
                    service
                        .execute_pending_ops(folder_struct.account, token)
                        .await
                });
            }
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Folder(folder),
                    message: err.user_message(),
                })
                .await;
            }
        }
    }

    // -----------------------------------------------------------------------
    // 草稿与发送
    // -----------------------------------------------------------------------

    async fn handle_save_draft(&self, outgoing: OutgoingMessage) {
        match self.save_draft_inner(&outgoing).await {
            Ok((id, _raw)) => {
                self.mark_local_draft_dirty_after_save(id).await;
                self.emit(Event::DraftSaved {
                    id,
                    synced_to_server: false,
                })
                .await;
            }
            Err(err) => {
                self.emit(Event::Error {
                    scope: outgoing
                        .draft_id
                        .map(ErrorScope::Draft)
                        .unwrap_or(ErrorScope::Account(outgoing.account)),
                    message: format!("保存草稿失败：{}", err.user_message()),
                })
                .await;
            }
        }
    }

    /// 读取草稿原文并解析为撰写界面可用的字段。
    ///
    /// 密送地址从本地 `drafts.bcc_addrs` 恢复；RFC 原文仍不含 Bcc。
    async fn handle_load_draft(&self, id: DraftId) {
        let draft = match self.db(move |s| s.get_draft(id)).await {
            Ok(Some(draft)) => draft,
            Ok(None) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Draft(id),
                    message: "草稿不存在".into(),
                })
                .await;
                return;
            }
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Draft(id),
                    message: format!("读取草稿失败：{}", err.user_message()),
                })
                .await;
                return;
            }
        };

        let paths = self.inner.paths.clone();
        let fallback = draft.clone();
        let result = tokio::task::spawn_blocking(move || {
            let raw = cache::read_draft_raw(&paths, id)?
                .ok_or_else(|| CoreError::storage("草稿原文缺失，无法继续编辑"))?;
            let header = me_proto::mime::parse_headers(&raw).ok();
            let body_text = me_proto::mime::parse_body(&raw)
                .map(|body| body.text)
                .unwrap_or_default();

            let subject = header
                .as_ref()
                .and_then(|header| header.subject.clone())
                .or_else(|| fallback.subject.clone())
                .unwrap_or_default();
            let to = header
                .as_ref()
                .map(|header| header.to.clone())
                .filter(|to| !to.is_empty())
                .unwrap_or_else(|| fallback.to.clone());
            let cc = header
                .as_ref()
                .map(|header| header.cc.clone())
                .unwrap_or_default();
            let message_id = header
                .as_ref()
                .and_then(|header| header.message_id.clone())
                .unwrap_or_else(|| fallback.message_id.clone());

            let mut attachments = Vec::new();
            if let Ok(entries) = std::fs::read_dir(paths.draft_attachment_dir(id)) {
                for entry in entries.flatten() {
                    if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                        attachments.push(entry.path());
                    }
                }
            }
            attachments.sort();

            Ok::<_, CoreError>(DraftContent {
                account: fallback.account,
                draft_id: fallback.id,
                message_id,
                subject,
                to,
                cc,
                bcc: fallback.bcc.clone(),
                body_text,
                attachments,
            })
        })
        .await
        .map_err(|join| CoreError::storage(format!("草稿读取任务失败：{join}")));

        match result.and_then(|inner| inner) {
            Ok(draft) => self.emit(Event::DraftLoaded { draft }).await,
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Draft(id),
                    message: format!("打开草稿失败：{}", err.user_message()),
                })
                .await;
            }
        }
    }

    /// 写入草稿原文与托管附件，返回草稿 id 与 RFC822 字节。
    async fn save_draft_inner(
        &self,
        outgoing: &OutgoingMessage,
    ) -> Result<(DraftId, Vec<u8>), CoreError> {
        let raw = me_proto::mime::build_rfc822(outgoing)?;
        self.check_outgoing_attachments(outgoing)?;

        let subject = Some(outgoing.subject.clone()).filter(|s| !s.trim().is_empty());
        let to = outgoing.to.clone();
        let bcc = outgoing.bcc.clone();
        let message_id = if outgoing.message_id.trim().is_empty() {
            generate_message_id(&outgoing.from.addr)
        } else {
            outgoing.message_id.clone()
        };
        let account = outgoing.account;
        let paths = self.inner.paths.clone();
        let attachments = outgoing.attachments.clone();
        let raw_for_file = raw.clone();

        let id = match outgoing.draft_id {
            Some(id) => id,
            None => {
                let subject_for_new = subject.clone();
                let to_for_new = to.clone();
                let bcc_for_new = bcc.clone();
                let draft = self
                    .db(move |s| {
                        s.create_draft(&me_core::port::NewDraft {
                            account,
                            message_id: message_id.clone(),
                            subject: subject_for_new,
                            to: to_for_new,
                            bcc: bcc_for_new,
                            raw_path: String::new(),
                        })
                    })
                    .await?;
                draft.id
            }
        };

        let paths_for_file = paths.clone();
        let draft_id = id;
        let stored_path = tokio::task::spawn_blocking(move || {
            let path = cache::write_draft_raw(&paths_for_file, draft_id, &raw_for_file)?;
            copy_draft_attachments(&paths_for_file, draft_id, &attachments)?;
            Ok::<_, CoreError>(path.to_string_lossy().to_string())
        })
        .await
        .map_err(|join| CoreError::storage(format!("草稿文件写入失败：{join}")))??;

        let stored_path_for_db = stored_path.clone();
        let subject_for_db = subject.clone();
        let to_for_db = to.clone();
        let bcc_for_db = bcc.clone();
        self.db(move |s| {
            s.update_draft(
                id,
                subject_for_db.as_deref(),
                &to_for_db,
                &bcc_for_db,
                &stored_path_for_db,
            )
        })
        .await?;

        Ok((id, raw))
    }

    fn check_outgoing_attachments(&self, outgoing: &OutgoingMessage) -> Result<(), CoreError> {
        let mut sizes = Vec::new();
        for path in &outgoing.attachments {
            let meta = std::fs::metadata(path).map_err(|err| {
                CoreError::storage(format!("无法读取附件 {}：{err}", path.display()))
            })?;
            sizes.push(meta.len());
        }
        check_attachment_limits(&sizes)
    }

    async fn handle_send(&self, outgoing: OutgoingMessage) {
        let (draft_id, raw) = match self.save_draft_inner(&outgoing).await {
            Ok(result) => result,
            Err(err) => {
                let scope = outgoing
                    .draft_id
                    .map(ErrorScope::Draft)
                    .unwrap_or(ErrorScope::Account(outgoing.account));
                self.emit(Event::Error {
                    scope,
                    message: format!("发送前保存草稿失败：{}", err.user_message()),
                })
                .await;
                return;
            }
        };

        let account = match self.account(outgoing.account).await {
            Ok(account) => account,
            Err(err) => {
                self.fail_send(draft_id, classify_send_error(&err)).await;
                return;
            }
        };
        let secret = match self.credential(&account).await {
            Ok(secret) => secret,
            Err(err) => {
                self.fail_send(draft_id, classify_send_error(&err)).await;
                return;
            }
        };

        // 发信优先：停掉该账户的后台补全，尽快腾出连接预算。
        self.cancel_backfill(account.id);
        self.status(account.id, ConnectionState::Connecting).await;
        let smtp_guard = match self.inner.governor.acquire(account.id).await {
            Ok(guard) => guard,
            Err(err) => {
                self.fail_send(draft_id, classify_send_error(&err)).await;
                return;
            }
        };
        let send_result = self.inner.smtp.send(&account, &secret, &outgoing).await;
        drop(smtp_guard);
        if let Err(err) = send_result {
            self.status(
                account.id,
                ConnectionState::Error {
                    detail: err.user_message(),
                },
            )
            .await;
            self.fail_send(draft_id, classify_send_error(&err)).await;
            return;
        }

        // SMTP 成功：归档到 Sent。
        let account_for_db = account.clone();
        let sent_folder = self
            .db(move |s| s.list_folders(account_for_db.id))
            .await
            .ok()
            .and_then(|folders| folders.into_iter().find(|f| f.role == FolderRole::Sent));

        let mut archive_error = None;
        if let Some(folder) = sent_folder {
            match self
                .append_raw(&account, &secret, &folder.name, &raw, Flags::SEEN)
                .await
            {
                Ok(_) => {}
                Err(err) => archive_error = Some(err.user_message()),
            }
        } else {
            archive_error = Some("账户没有“已发送”文件夹".to_string());
        }

        let archived = archive_error.is_none();
        let error_for_db = archive_error.clone();
        let _ = self
            .db(move |s| s.set_draft_status(draft_id, DraftStatus::Sent, error_for_db.as_deref()))
            .await;

        self.status(account.id, ConnectionState::Online).await;
        self.emit(Event::SendCompleted {
            draft: draft_id,
            archived,
            archive_error,
        })
        .await;

        // 尝试删除服务器草稿副本，失败仅提示。
        if let Ok(Some(draft)) = self.db(move |s| s.get_draft(draft_id)).await
            && draft.server_uid.is_some()
            && let Err(err) = self.delete_server_draft(&account, &secret, &draft).await
        {
            tracing::warn!(draft = draft_id.0, error = %err.user_message(), "failed to remove server draft copy");
        }
    }

    async fn fail_send(&self, draft: DraftId, reason: SendFailureKind) {
        let message = reason.user_message();
        let message_for_db = message.clone();
        let _ = self
            .db(move |s| s.set_draft_status(draft, DraftStatus::Draft, Some(&message_for_db)))
            .await;
        self.emit(Event::SendFailed { draft, reason }).await;
    }

    async fn handle_save_draft_to_server(&self, id: DraftId) {
        match self.push_draft_to_server(id).await {
            Ok(()) => {
                self.emit(Event::DraftSaved {
                    id,
                    synced_to_server: true,
                })
                .await;
            }
            Err(err) => {
                let message = err.user_message();
                let message_for_db = message.clone();
                let _ = self
                    .db(move |s| {
                        s.set_draft_sync_state(
                            id,
                            me_core::DraftSyncState::LocalDirty,
                            Some(&message_for_db),
                        )
                    })
                    .await;
                self.emit(Event::Error {
                    scope: ErrorScope::Draft(id),
                    message,
                })
                .await;
            }
        }
    }

    async fn handle_retry_archive(&self, id: DraftId) {
        let draft = match self.db(move |s| s.get_draft(id)).await {
            Ok(Some(draft)) => draft,
            _ => return,
        };
        if draft.status != DraftStatus::Sent {
            return;
        }
        let account = match self.account(draft.account).await {
            Ok(account) => account,
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Draft(id),
                    message: err.user_message(),
                })
                .await;
                return;
            }
        };
        let secret = match self.credential(&account).await {
            Ok(secret) => secret,
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Draft(id),
                    message: err.user_message(),
                })
                .await;
                return;
            }
        };
        let raw = match cache::read_draft_raw(&self.inner.paths, id) {
            Ok(Some(raw)) => raw,
            Ok(None) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Draft(id),
                    message: "草稿原文缺失，无法重试归档".into(),
                })
                .await;
                return;
            }
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Draft(id),
                    message: err.user_message(),
                })
                .await;
                return;
            }
        };
        let account_for_db = account.clone();
        let sent = self
            .db(move |s| s.list_folders(account_for_db.id))
            .await
            .ok()
            .and_then(|folders| folders.into_iter().find(|f| f.role == FolderRole::Sent));
        let Some(folder) = sent else {
            self.emit(Event::SendCompleted {
                draft: id,
                archived: false,
                archive_error: Some("账户没有“已发送”文件夹".into()),
            })
            .await;
            return;
        };

        match self
            .append_raw(&account, &secret, &folder.name, &raw, Flags::SEEN)
            .await
        {
            Ok(_) => {
                let _ = self
                    .db(move |s| s.set_draft_status(id, DraftStatus::Sent, None))
                    .await;
                self.emit(Event::SendCompleted {
                    draft: id,
                    archived: true,
                    archive_error: None,
                })
                .await;
            }
            Err(err) => {
                let message = err.user_message();
                let message_for_db = message.clone();
                let _ = self
                    .db(move |s| s.set_draft_status(id, DraftStatus::Sent, Some(&message_for_db)))
                    .await;
                self.emit(Event::SendCompleted {
                    draft: id,
                    archived: false,
                    archive_error: Some(message),
                })
                .await;
            }
        }
    }

    async fn append_raw(
        &self,
        account: &Account,
        credential: &AuthCredential,
        folder: &str,
        raw: &[u8],
        flags: Flags,
    ) -> Result<Option<u32>, CoreError> {
        let _guard = self.inner.governor.acquire(account.id).await?;
        let mut session = self.inner.factory.connect(account, credential).await?;
        let result = session.append(folder, raw, flags).await;
        let _ = session.logout().await;
        result
    }

    async fn delete_server_draft(
        &self,
        account: &Account,
        credential: &AuthCredential,
        draft: &me_core::Draft,
    ) -> Result<(), CoreError> {
        let Some(uid) = draft.server_uid else {
            return Ok(());
        };
        let account_id = account.id;
        let folders = self.db(move |s| s.list_folders(account_id)).await?;
        let Some(folder) = folders.into_iter().find(|f| f.role == FolderRole::Drafts) else {
            return Ok(());
        };
        let _guard = self.inner.governor.acquire(account.id).await?;
        let mut session = self.inner.factory.connect(account, credential).await?;
        let result = async {
            session.select(&folder.name).await?;
            session
                .store_flags(&[uid], Flags::DELETED, me_core::port::FlagStoreMode::Add)
                .await?;
            if session.capabilities().uidplus {
                session.uid_expunge(&[uid]).await?;
            }
            Ok(())
        }
        .await;
        let _ = session.logout().await;
        result
    }

    // -----------------------------------------------------------------------
    // 搜索与维护
    // -----------------------------------------------------------------------

    /// 搜索框入口。与 `email_search` 工具共用 [`Self::search_mail`]：
    /// 两套查询一定会漂移成「AI 说没有、用户一搜就有」。
    async fn handle_search(&self, query: SearchQuery) {
        match self.search_mail(query, me_core::MAX_PAGE_SIZE).await {
            Ok(outcome) => {
                let query_id = outcome.query_id;
                self.emit(Event::SearchResults {
                    query_id,
                    messages: outcome.messages,
                    total: outcome.total,
                    body_indexed: outcome.body_indexed,
                    body_total: outcome.body_total,
                })
                .await;
            }
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Global,
                    message: format!("搜索失败：{}", err.user_message()),
                })
                .await;
            }
        }
    }

    /// 保存一条命名搜索。
    async fn handle_save_search(&self, name: String, query: SavedSearchQuery) {
        let trimmed = name.trim().to_string();
        if trimmed.is_empty() {
            self.emit(Event::Error {
                scope: ErrorScope::Global,
                message: "保存的搜索需要一个名称".to_string(),
            })
            .await;
            return;
        }
        let result = self
            .db(move |s| {
                s.insert_saved_search(&SavedSearch {
                    id: SavedSearchId(0),
                    name: trimmed,
                    query,
                    created_at: now_unix(),
                    last_used_at: now_unix(),
                })
            })
            .await;
        match result {
            Ok(_) => self.emit_saved_searches().await,
            Err(err) => {
                // 名称重复在**仓储层**已翻译成可读的 `Validation` 错误
                // （`CoreError::Storage` 的 `user_message()` 是通用文案，
                // 拿不到「名字重复」这个信息）。
                self.emit(Event::Error {
                    scope: ErrorScope::Global,
                    message: err.user_message(),
                })
                .await;
            }
        }
    }

    /// 删除一条保存的搜索。
    async fn handle_delete_search(&self, id: SavedSearchId) {
        let result = self.db(move |s| s.delete_saved_search(id)).await;
        if let Err(err) = result {
            self.emit(Event::Error {
                scope: ErrorScope::Global,
                message: format!("删除失败：{}", err.user_message()),
            })
            .await;
            return;
        }
        self.emit_saved_searches().await;
    }

    /// 执行一条保存的搜索。
    ///
    /// **反序列化在 UI 侧做**（`SavedSearchQuery` → `SearchQuery` 是领域层的活），
    /// 这里只负责：读出 → touch（更新 `last_used_at`）→ 转成查询 → 搜索。
    async fn handle_run_saved_search(&self, id: SavedSearchId) {
        let found = self.db(move |s| s.get_saved_search(id)).await;
        let search = match found {
            Ok(Some(search)) => search,
            Ok(None) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Global,
                    message: "这条保存的搜索已经不存在了".to_string(),
                })
                .await;
                return;
            }
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Global,
                    message: format!("读取失败：{}", err.user_message()),
                })
                .await;
                return;
            }
        };
        // touch 后要重新下发列表——排序变了（最近使用的排前面）。
        let _ = self.db(move |s| s.touch_saved_search(id)).await;
        self.emit_saved_searches().await;
        self.handle_search(search.query.to_query()).await;
    }

    /// 下发保存的搜索列表（全量快照）。
    async fn emit_saved_searches(&self) {
        let result = self.db(|s| s.list_saved_searches()).await;
        match result {
            Ok(searches) => self.emit(Event::SavedSearchesChanged { searches }).await,
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Global,
                    message: format!("读取保存的搜索失败：{}", err.user_message()),
                })
                .await;
            }
        }
    }

    async fn handle_rebuild_index(&self) {
        match self.db(|s| s.rebuild_index()).await {
            Ok(indexed) => self.emit(Event::IndexRebuilt { indexed }).await,
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Global,
                    message: err.user_message(),
                })
                .await;
            }
        }
    }

    /// 重定位附件缓存目录。
    ///
    /// 顺序：校验目标 → 递归复制 → 持久化设置 → 切换运行时目录 → 重写数据库
    /// `local_path` → 删除旧目录。任一步失败都保留旧目录，避免数据丢失。
    async fn handle_set_attachment_dir(&self, requested: Option<PathBuf>) {
        let _gate = self.inner.attach_gate.lock().await;
        let default_dir = self.inner.paths.attach_dir().to_path_buf();
        let target = requested.unwrap_or_else(|| default_dir.clone());
        if !target.is_absolute() {
            self.emit(Event::Error {
                scope: ErrorScope::Global,
                message: "附件下载目录必须是绝对路径".into(),
            })
            .await;
            return;
        }

        let old = self.attachment_dir();
        if target == old {
            self.emit(Event::AttachmentDirChanged { dir: target }).await;
            return;
        }
        if target.starts_with(&old) || old.starts_with(&target) || {
            // 同一个目录可能以不同大小写/分隔符传入；规范化后再比较。
            let old_cmp = comparison_path(&old);
            let target_cmp = comparison_path(&target);
            target_cmp == old_cmp
                || target_cmp.starts_with(&old_cmp)
                || old_cmp.starts_with(&target_cmp)
        } {
            self.emit(Event::Error {
                scope: ErrorScope::Global,
                message: "新附件目录不能与原目录相同或互相包含".into(),
            })
            .await;
            return;
        }

        let marker = target.join(cache::ATTACHMENT_MARKER);
        if target.exists() && !target.is_dir() {
            self.emit(Event::Error {
                scope: ErrorScope::Global,
                message: format!("{} 不是文件夹", target.display()),
            })
            .await;
            return;
        }
        if !cache::dir_is_empty(&target) && !marker.exists() {
            self.emit(Event::Error {
                scope: ErrorScope::Global,
                message: "请选择空文件夹，或此前由 MyEmail 使用的附件目录".into(),
            })
            .await;
            return;
        }

        // 1) 先复制，旧目录保持不动；失败时无需回滚。
        let copy_old = old.clone();
        let copy_new = target.clone();
        let copied = tokio::task::spawn_blocking(move || -> Result<u64, CoreError> {
            let bytes = cache::copy_dir_tree(&copy_old, &copy_new)?;
            std::fs::write(
                copy_new.join(cache::ATTACHMENT_MARKER),
                b"myemail attachment cache\n",
            )
            .map_err(|e| CoreError::storage(format!("写入附件目录标记失败：{e}")))?;
            Ok(bytes)
        })
        .await;
        let copied = match copied {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(err)) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Global,
                    message: format!("附件目录迁移失败：{}", err.user_message()),
                })
                .await;
                return;
            }
            Err(join) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Global,
                    message: format!("附件目录迁移失败：{join}"),
                })
                .await;
                return;
            }
        };

        // 2) 持久化设置；失败则维持原目录。
        let stored = if target == default_dir {
            String::new()
        } else {
            target.to_string_lossy().into_owned()
        };
        let stored_for_db = stored.clone();
        if let Err(err) = self
            .db(move |s| s.set_setting(me_core::settings_key::ATTACHMENT_DIR, &stored_for_db))
            .await
        {
            self.emit(Event::Error {
                scope: ErrorScope::Global,
                message: format!("保存附件目录设置失败：{}", err.user_message()),
            })
            .await;
            return;
        }

        // 3) 切换运行时目录，之后的新下载写入新位置。
        if let Ok(mut dir) = self.inner.attachment_dir.lock() {
            *dir = target.clone();
        }

        // 4) 重写已缓存附件的绝对路径；失败时保留旧目录，旧路径仍然有效。
        let rewrite_old = old.clone();
        let rewrite_new = target.clone();
        let rewritten = self
            .db(move |s| s.rewrite_attachment_paths(&rewrite_old, &rewrite_new))
            .await;
        let rewrite_ok = match rewritten {
            Ok(count) => {
                tracing::info!(
                    count,
                    from = %old.display(),
                    to = %target.display(),
                    "attachment cache relocated"
                );
                true
            }
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Global,
                    message: format!("附件路径更新失败，旧文件已保留：{}", err.user_message()),
                })
                .await;
                false
            }
        };

        // 5) 旧目录仅在其归属可确认（默认目录或带标记）时删除。
        let mut freed = 0u64;
        if rewrite_ok {
            let owned = old == default_dir || old.join(cache::ATTACHMENT_MARKER).exists();
            if owned {
                let delete_old = old.clone();
                freed = tokio::task::spawn_blocking(move || {
                    let size = cache::dir_size(&delete_old);
                    match std::fs::remove_dir_all(&delete_old) {
                        Ok(()) => size,
                        Err(_) => 0,
                    }
                })
                .await
                .unwrap_or(0);
            }
        }
        tracing::info!(
            copied_bytes = copied,
            freed_bytes = freed,
            "attachment directory migration finished"
        );

        self.emit(Event::AttachmentDirChanged { dir: target }).await;
    }

    async fn handle_clear_cache(&self, account: Option<AccountId>) {
        let freed = match self.clear_cache_inner(account).await {
            Ok(freed) => freed,
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Global,
                    message: err.user_message(),
                })
                .await;
                return;
            }
        };
        self.emit(Event::CacheCleared {
            account,
            freed_bytes: freed,
        })
        .await;
    }

    async fn clear_cache_inner(&self, account: Option<AccountId>) -> Result<u64, CoreError> {
        let paths = self.inner.paths.clone();
        let accounts: Vec<Account> = match account {
            Some(id) => vec![self.account(id).await?],
            None => self.db(|s| s.list_accounts()).await?,
        };

        let _ = self.db(move |s| s.reset_body_cache(account)).await?;

        let _gate = self.inner.attach_gate.lock().await;
        let attach_dir = self.attachment_dir();
        let mut freed = 0u64;
        for account in accounts {
            let account_id = account.id;
            let ids = self.db(move |s| s.message_ids(account_id)).await?;
            let paths = paths.clone();
            let attach_dir = attach_dir.clone();
            let bytes = tokio::task::spawn_blocking(move || {
                cache::remove_message_files_in(&paths, &attach_dir, &ids)
            })
            .await
            .map_err(|join| CoreError::storage(format!("缓存清理失败：{join}")))?;
            freed = freed.saturating_add(bytes);
        }
        Ok(freed)
    }

    async fn handle_set_folder_role(&self, folder: me_core::FolderId, role: FolderRole) {
        let set = self.db(move |s| s.set_folder_role(folder, role)).await;
        if let Err(err) = set {
            self.emit(Event::Error {
                scope: ErrorScope::Folder(folder),
                message: err.user_message(),
            })
            .await;
            return;
        }
        let account = self
            .db(move |s| s.get_folder(folder))
            .await
            .ok()
            .flatten()
            .map(|f| f.account);
        if let Some(account) = account
            && let Ok(folders) = self.db(move |s| s.list_folders(account)).await
        {
            self.emit(Event::FoldersUpdated { account, folders }).await;
        }
    }

    async fn handle_acknowledge_folder(&self, folder: me_core::FolderId) {
        let account = self
            .db(move |s| s.get_folder(folder))
            .await
            .ok()
            .flatten()
            .map(|f| f.account);
        // 源文件夹消失后，待同步操作已无意义；先清理再删文件夹。
        let _ = self.db(move |s| s.cleanup_ops_for_folder(folder)).await;
        match self.db(move |s| s.delete_folder(folder)).await {
            Ok(()) => {
                self.emit(Event::FolderRemoved { folder }).await;
                if let Some(account) = account
                    && let Ok(folders) = self.db(move |s| s.list_folders(account)).await
                {
                    self.emit(Event::FoldersUpdated { account, folders }).await;
                }
            }
            Err(err) => {
                self.emit(Event::Error {
                    scope: ErrorScope::Folder(folder),
                    message: err.user_message(),
                })
                .await;
            }
        }
    }
}

fn classify_send_error(err: &CoreError) -> SendFailureKind {
    match err {
        CoreError::Auth { .. } => SendFailureKind::Auth {
            detail: err.user_message(),
        },
        CoreError::OAuth {
            reauth_required, ..
        } => SendFailureKind::Auth {
            detail: if *reauth_required {
                "发送失败：授权已失效，请重新登录".into()
            } else {
                err.user_message()
            },
        },
        CoreError::Network {
            retryable, detail, ..
        } => {
            let lower = detail.to_ascii_lowercase();
            if lower.contains("timeout") || lower.contains("timed out") || !retryable {
                if lower.contains("tls") || lower.contains("certificate") {
                    SendFailureKind::Tls {
                        detail: detail.clone(),
                    }
                } else if lower.contains("timeout") || lower.contains("timed out") {
                    SendFailureKind::Timeout
                } else {
                    SendFailureKind::NetworkUnreachable
                }
            } else if lower.contains("tls") || lower.contains("certificate") {
                SendFailureKind::Tls {
                    detail: detail.clone(),
                }
            } else {
                SendFailureKind::NetworkUnreachable
            }
        }
        CoreError::Protocol { code, detail } => SendFailureKind::Rejected {
            code: code.as_deref().and_then(|c| c.parse().ok()),
            detail: detail.clone(),
        },
        CoreError::Parse { detail } => SendFailureKind::Rejected {
            code: None,
            detail: detail.clone(),
        },
        CoreError::Limit { what } => SendFailureKind::Rejected {
            code: None,
            detail: what.to_string(),
        },
        CoreError::Storage { .. }
        | CoreError::Validation { .. }
        | CoreError::Cancelled
        | CoreError::Ai { .. } => SendFailureKind::Rejected {
            code: None,
            detail: err.user_message(),
        },
    }
}

fn copy_draft_attachments(
    paths: &CachePaths,
    id: DraftId,
    attachments: &[PathBuf],
) -> Result<(), CoreError> {
    let dir = paths.draft_attachment_dir(id);
    if attachments.is_empty() {
        // 重新编辑时移除了全部附件：清空托管目录，避免下次打开又出现。
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        return Ok(());
    }
    std::fs::create_dir_all(&dir).map_err(CoreError::from)?;
    let mut used: HashMap<String, u32> = HashMap::new();
    let mut keep: std::collections::HashSet<String> = std::collections::HashSet::new();
    for path in attachments {
        let base = path
            .file_name()
            .map(|name| cache::safe_name(&name.to_string_lossy()))
            .unwrap_or_else(|| "attachment".to_string());
        let count = used.entry(base.clone()).or_insert(0);
        let name = if *count == 0 {
            base.clone()
        } else {
            format!("{base}-{count}")
        };
        *count += 1;
        keep.insert(name.clone());
        let dest = dir.join(&name);
        // 重新保存已有草稿时，附件源路径就是托管路径；不能把文件复制到自身
        // （`fs::copy` 会先截断目标，导致附件变空）。
        if same_file(path, &dest) {
            continue;
        }
        std::fs::copy(path, &dest).map_err(CoreError::from)?;
    }

    // 删除本次已不存在的旧附件。
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !keep.contains(&name) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

fn same_file(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// 由 `me-app` 注入的路径工具：确保缓存目录存在。
pub fn ensure_cache_paths(root: impl AsRef<Path>) -> Result<CachePaths, CoreError> {
    CachePaths::new(root.as_ref().to_path_buf()).map_err(CoreError::from)
}

impl<S, F, M> SyncService<S, F, M>
where
    S: StorePort + Clone,
    F: me_core::port::ImapSessionFactory + 'static,
    M: SmtpSender + 'static,
{
    /// 把凭证放入本次会话内存（keyring 不可用或测试时的降级路径）。
    pub fn set_session_credential(&self, account: AccountId, credential: AuthCredential) {
        self.inner.tokens.set_session(account, credential);
    }

    /// 注入 OAuth 服务商客户端配置（client_id / 端点 / scope）。
    pub fn configure_oauth_provider(&self, config: me_proto::oauth::ProviderConfig) {
        self.inner.tokens.configure_provider(config);
    }
}

/// 原生实现组合：SQLite 写句柄 + 真实 IMAP/SMTP 适配。
///
/// 这样 `me-app` 无需直接依赖 `me-proto`，也不会 `use` 协议库符号。
pub type NativeService =
    SyncService<me_store::WritableStore, me_proto::ProtoFactory, me_proto::ProtoSmtp>;

pub fn start_native(
    store: me_store::WritableStore,
    paths: CachePaths,
    events: EventSender,
    browser: Arc<dyn BrowserOpener>,
) -> NativeService {
    let service = SyncService::new(
        store,
        me_proto::ProtoFactory,
        me_proto::ProtoSmtp,
        paths,
        events,
        browser,
    );
    // AI 传输层在这里注入：`me-app` 因此不需要依赖 `me-proto`（§2.3 第 2 条）。
    if let Ok(provider) = me_proto::ai::HttpAiProvider::new() {
        service.with_ai_provider(Arc::new(provider));
    }
    service
}

/// 无头/测试用浏览器打开端口：不执行任何外部程序。
pub struct NoopBrowser;

impl BrowserOpener for NoopBrowser {
    fn open(&self, _url: &str) -> Result<(), CoreError> {
        Ok(())
    }
}

/// 应用名，便于上层展示。
pub const NAME: &str = APP_NAME;
