//! UI 侧状态。退出即丢；未保存草稿需先经 `SaveDraft` 落库。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use me_core::{
    Account, AccountId, Address, AuthMethod, ConnectionState, Draft, DraftId, FlagOp, Flags,
    Folder, FolderId, HtmlRendererMode, MessageBody, MessageId, MessageSummary, ThemeMode, TlsMode,
};

// ---------------------------------------------------------------------------
// 页面导航
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Mail,
    Calendar,
    Weather,
    Music,
    Accounts,
    Settings,
}

impl Page {
    pub fn label(self) -> &'static str {
        match self {
            Page::Mail => "邮件",
            Page::Calendar => "日历",
            Page::Weather => "天气",
            Page::Music => "音乐",
            Page::Accounts => "账户",
            Page::Settings => "设置",
        }
    }
}

// ---------------------------------------------------------------------------
// 顶层 UI 状态
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReaderMode {
    /// HTML 富文本（无 HTML 时回退纯文本）。
    #[default]
    Rich,
    Plain,
    /// 按 Markdown 子集渲染纯文本正文。
    Markdown,
}

/// 阅读页解析结果的缓存：避免每一帧重新解析 HTML/Markdown。
pub struct RenderedBlocks {
    pub message: MessageId,
    pub mode: ReaderMode,
    pub blocks: Vec<crate::views::richtext::Block>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RealtimeView {
    pub mode: me_core::RealtimeMode,
    pub connected: bool,
    pub last_event_at: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
pub struct HtmlJobView {
    pub message: MessageId,
    pub generation: u64,
    pub started: Instant,
}

/// 音乐页状态。播放位置与时长**只来自音频线程**，界面不自己计时。
pub struct MusicState {
    /// 已保存的目录（来自 meta，或用户本次选择）。
    pub dir: Option<PathBuf>,
    pub order: me_core::MusicOrder,
    pub tracks: Vec<crate::audio::playlist::Track>,
    pub current: Option<usize>,
    pub playing: bool,
    pub position_ms: u64,
    pub duration_ms: Option<u64>,
    /// 拖动进度条时的本地值；`Some` 期间忽略后端回传，避免滑块被拽回。
    pub scrubbing: Option<u64>,
    pub scanning: bool,
    pub error: Option<String>,
    /// 设备打不开：控件禁用，但页面照常渲染。
    pub device_error: Option<String>,
    pub skipped: usize,
    pub truncated: bool,
    /// 当前曲目的标签与封面（切歌时由音频线程读出）。
    pub now_playing: Option<crate::audio::tags::NowPlaying>,
    /// 与 `now_playing` 对应的封面纹理；路径变了才重建。
    pub cover_texture: Option<(std::path::PathBuf, eframe::egui::TextureHandle)>,
    /// 频谱分析器（FFT、分带、平滑），只在 UI 线程使用。
    pub analyzer: crate::audio::spectrum::SpectrumAnalyzer,
    /// 本帧产生的音频命令，由 app 统一转交给音频线程。
    pub pending: Vec<crate::audio::AudioCommand>,
}

impl Default for MusicState {
    fn default() -> Self {
        Self {
            dir: None,
            order: me_core::MusicOrder::Sequential,
            tracks: Vec::new(),
            current: None,
            playing: false,
            position_ms: 0,
            duration_ms: None,
            scrubbing: None,
            scanning: false,
            error: None,
            device_error: None,
            skipped: 0,
            truncated: false,
            now_playing: None,
            cover_texture: None,
            analyzer: crate::audio::spectrum::SpectrumAnalyzer::new(),
            pending: Vec::new(),
        }
    }
}

impl MusicState {
    pub fn current_track(&self) -> Option<&crate::audio::playlist::Track> {
        self.current.and_then(|index| self.tracks.get(index))
    }

    /// 进度条显示值：拖动时用本地值，否则用音频线程回传的位置。
    pub fn display_position_ms(&self) -> u64 {
        self.scrubbing.unwrap_or(self.position_ms)
    }

    /// 柱高，`0.0..=1.0`。
    pub fn levels(&self) -> &[f32; crate::audio::spectrum::BANDS] {
        self.analyzer.levels()
    }

    /// 列表与封面用的曲名：标签优先，回退文件名。
    pub fn display_title(&self) -> Option<String> {
        let track = self.current_track()?;
        Some(crate::audio::tags::display_title(
            &track.title,
            self.now_playing.as_ref(),
        ))
    }

    /// 艺术家 · 专辑。
    pub fn subtitle(&self) -> String {
        crate::audio::tags::subtitle(self.now_playing.as_ref())
    }
}

pub struct UiState {
    pub accounts: Vec<Account>,
    pub folders: BTreeMap<AccountId, Vec<Folder>>,
    pub unread: BTreeMap<FolderId, u32>,
    pub connection: BTreeMap<AccountId, ConnectionState>,
    /// 实时同步状态（WP-08 在设置/账户页展示）。
    pub realtime: BTreeMap<AccountId, RealtimeView>,

    pub selected_account: Option<AccountId>,
    pub selected_folder: Option<FolderId>,
    pub messages: Vec<MessageSummary>,
    /// 键集分页游标：上一页最后一行的 `(date, id)`。
    pub messages_cursor: Option<(i64, i64)>,
    pub messages_has_more: bool,

    pub selected_message: Option<MessageId>,
    pub open_body: Option<MessageBody>,
    pub body_loading: bool,
    pub reader_mode: ReaderMode,
    pub reader_blocks: Option<RenderedBlocks>,
    /// HTML 渲染器 v2 的最新结果（可能属于其他邮件，渲染时按 message 过滤）。
    pub html_rendered: Option<crate::views::html::HtmlRenderResult>,
    /// 正在后台解析的 HTML 任务。
    pub html_job: Option<HtmlJobView>,
    pub next_html_generation: u64,
    /// 阅读页已解码的图片纹理，按 `src` 索引。
    pub reader_textures: std::collections::HashMap<String, eframe::egui::TextureHandle>,
    pub reader_texture_message: Option<MessageId>,
    /// 正在下载 / 下载失败的远程图片 URL（按正文里的原始 src 索引）。
    pub remote_pending: std::collections::HashSet<String>,
    pub remote_failed: std::collections::HashSet<String>,

    pub drafts: Vec<Draft>,

    pub page: Page,
    /// 日历页状态（数据由 app 按脏标记重查）。
    pub calendar: crate::views::calendar::CalendarState,
    /// 天气页状态（数据由 app 按脏标记重查）。
    pub weather: crate::views::weather::WeatherState,
    /// 音乐页状态：播放位置只在播放中由音频线程刷新。
    pub music: crate::state::MusicState,
    pub search: SearchState,
    pub sync_progress: Option<SyncProgressView>,
    pub index_progress: Option<IndexProgressView>,
    pub last_error: Option<UiError>,
    pub toasts: Vec<Toast>,

    pub theme: ThemeMode,
    pub body_backfill: bool,
    /// 是否自动加载远程图片（设置项，默认开启）。
    pub remote_images: bool,
    /// 全局实时同步（IDLE / 轮询）。
    pub realtime_sync: bool,
    /// 全局默认轮询间隔（秒）。
    pub poll_interval_secs: u32,
    /// 草稿自动镜像（WP-10 接入）。
    pub draft_server_sync: bool,
    /// 迁移产生的一次性旧草稿上传确认（WP-10 接入）。
    pub draft_bulk_upload_pending: bool,
    /// HTML 阅读渲染器模式（WP-12 接入）。
    pub html_renderer: HtmlRendererMode,
    /// 非敏感 OAuth 客户端覆盖；空串表示使用构建期默认值。
    pub oauth_google_client_id: String,
    pub oauth_microsoft_client_id: String,
    pub oauth_microsoft_tenant: String,
    /// 当前生效的附件缓存根目录；`None` 表示尚未从后端收到设置。
    pub attachment_dir: Option<PathBuf>,
    /// 剩余磁盘低于 500 MB 时置位，顶部常驻提示。
    pub disk_low: bool,

    /// 事件驱动脏标记：懒加载，不在事件处理里立刻查询。
    pub folders_dirty: bool,
    pub messages_dirty: bool,
    pub drafts_dirty: bool,
    /// 视图请求加载下一页；由 app 在渲染后处理。
    pub load_more_requested: bool,
    /// 保存草稿后关闭窗口。
    pub exit_requested: bool,

    pub dialog: Dialog,

    /// 首次启动的明文缓存告知是否已确认。
    pub plaintext_notice_ack: bool,
    pub show_plaintext_notice: bool,
    pub cache_size_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct SyncProgressView {
    pub folder: FolderId,
    pub done: u32,
    pub total: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
pub struct IndexProgressView {
    pub account: AccountId,
    pub indexed: u32,
    pub total: u32,
}

#[derive(Debug, Clone)]
pub struct UiError {
    pub scope: me_core::ErrorScope,
    pub message: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub created: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warn,
    Error,
}

impl Toast {
    pub fn is_expired(&self) -> bool {
        self.created.elapsed() > Duration::from_secs(6)
    }
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            folders: BTreeMap::new(),
            unread: BTreeMap::new(),
            connection: BTreeMap::new(),
            realtime: BTreeMap::new(),
            selected_account: None,
            selected_folder: None,
            messages: Vec::new(),
            messages_cursor: None,
            messages_has_more: false,
            selected_message: None,
            open_body: None,
            body_loading: false,
            reader_mode: ReaderMode::Rich,
            reader_blocks: None,
            html_rendered: None,
            html_job: None,
            next_html_generation: 1,
            reader_textures: std::collections::HashMap::new(),
            reader_texture_message: None,
            remote_pending: std::collections::HashSet::new(),
            remote_failed: std::collections::HashSet::new(),
            drafts: Vec::new(),
            page: Page::Mail,
            calendar: crate::views::calendar::CalendarState::default(),
            weather: crate::views::weather::WeatherState::default(),
            music: MusicState::default(),
            search: SearchState::default(),
            sync_progress: None,
            index_progress: None,
            last_error: None,
            toasts: Vec::new(),
            theme: ThemeMode::System,
            body_backfill: true,
            remote_images: true,
            realtime_sync: true,
            poll_interval_secs: me_core::DEFAULT_POLL_SECS,
            draft_server_sync: true,
            draft_bulk_upload_pending: false,
            html_renderer: HtmlRendererMode::Enhanced,
            oauth_google_client_id: String::new(),
            oauth_microsoft_client_id: String::new(),
            oauth_microsoft_tenant: String::new(),
            attachment_dir: None,
            disk_low: false,
            folders_dirty: true,
            messages_dirty: true,
            drafts_dirty: true,
            load_more_requested: false,
            exit_requested: false,
            dialog: Dialog::None,
            plaintext_notice_ack: false,
            show_plaintext_notice: false,
            cache_size_bytes: 0,
        }
    }
}

impl UiState {
    pub fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        self.toasts.push(Toast {
            text: text.into(),
            kind,
            created: Instant::now(),
        });
    }

    pub fn prune_toasts(&mut self) {
        self.toasts.retain(|toast| !toast.is_expired());
    }

    pub fn account(&self, id: AccountId) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn folders_of(&self, account: AccountId) -> &[Folder] {
        self.folders.get(&account).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn folder(&self, id: FolderId) -> Option<&Folder> {
        self.folders.values().flatten().find(|f| f.id == id)
    }

    pub fn selected_folder_struct(&self) -> Option<&Folder> {
        self.selected_folder.and_then(|id| self.folder(id))
    }

    pub fn selected_account_struct(&self) -> Option<&Account> {
        self.selected_account.and_then(|id| self.account(id))
    }

    pub fn is_offline(&self) -> bool {
        if self.accounts.is_empty() || self.connection.is_empty() {
            // 尚未收到任何连接状态时不做“离线”判断。
            return false;
        }
        self.connection.values().all(|state| {
            matches!(
                state,
                ConnectionState::Disconnected
                    | ConnectionState::Backoff { .. }
                    | ConnectionState::Error { .. }
            )
        })
    }

    pub fn unread_of(&self, folder: FolderId) -> u32 {
        self.unread.get(&folder).copied().unwrap_or(0)
    }

    /// 文件夹按角色排序：Inbox 优先，其余按名称。
    pub fn sorted_folders(&self, account: AccountId) -> Vec<Folder> {
        let mut folders = self.folders_of(account).to_vec();
        folders.sort_by_key(|f| {
            let rank = match f.role {
                me_core::FolderRole::Inbox => 0,
                me_core::FolderRole::Drafts => 1,
                me_core::FolderRole::Sent => 2,
                me_core::FolderRole::Archive => 3,
                me_core::FolderRole::Junk => 4,
                me_core::FolderRole::Trash => 5,
                me_core::FolderRole::Other => 6,
            };
            (rank, f.name.to_lowercase())
        });
        folders
    }

    pub fn trash_folder(&self, account: AccountId) -> Option<&Folder> {
        self.folders_of(account)
            .iter()
            .find(|f| f.role == me_core::FolderRole::Trash && !f.deleted_on_server)
    }
}

// ---------------------------------------------------------------------------
// 搜索
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct SearchState {
    pub text: String,
    pub active: bool,
    pub query_id: u64,
    pub body_indexed: u32,
    pub body_total: u32,
    /// `None` = 不限。
    pub has_attachment: Option<bool>,
    pub since_text: String,
    pub until_text: String,
    pub accounts: Vec<AccountId>,
    pub folders: Vec<FolderId>,
    pub show_filters: bool,
}

impl SearchState {
    pub fn is_short_query(&self) -> bool {
        let count = self.text.trim().chars().count();
        (1..3).contains(&count)
    }

    pub fn is_empty_query(&self) -> bool {
        self.text.trim().is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.active = false;
        self.query_id = 0;
        self.body_indexed = 0;
        self.body_total = 0;
        self.accounts.clear();
        self.folders.clear();
        self.has_attachment = None;
        self.since_text.clear();
        self.until_text.clear();
    }

    pub fn coverage_label(&self) -> String {
        format!("正文索引覆盖 {}/{} 封", self.body_indexed, self.body_total)
    }
}

// ---------------------------------------------------------------------------
// 对话框
// ---------------------------------------------------------------------------

pub enum Dialog {
    None,
    Confirm(ConfirmDialog),
    AccountEditor(AccountEditor),
    OAuthLogin(OAuthLoginState),
    Compose(ComposeState),
    /// 系统密钥库不可用：选择“仅本次会话记忆”或取消添加。
    CredentialFallback {
        account: AccountId,
    },
    /// 和风天气配置（Host + API Key）。由天气页的「配置密钥」按钮打开。
    WeatherConfig,
}

#[derive(Clone)]
pub struct OAuthLoginState {
    pub flow_id: Option<u64>,
    pub provider: me_core::OAuthProvider,
    pub mode: me_core::OAuthFlowMode,
    pub verification_uri: String,
    pub user_code: Option<String>,
    pub expires_at: i64,
    pub email_hint: String,
    pub reauth_account: Option<AccountId>,
    pub message: String,
    pub busy: bool,
}

impl OAuthLoginState {
    pub fn new(
        provider: me_core::OAuthProvider,
        mode: me_core::OAuthFlowMode,
        reauth_account: Option<AccountId>,
        email_hint: String,
    ) -> Self {
        Self {
            flow_id: None,
            provider,
            mode,
            verification_uri: String::new(),
            user_code: None,
            expires_at: 0,
            email_hint,
            reauth_account,
            message: "正在请求授权…".to_string(),
            busy: true,
        }
    }
}

#[derive(Clone)]
pub struct ConfirmDialog {
    pub title: String,
    pub body: String,
    pub action: ConfirmAction,
    pub danger: bool,
}

#[derive(Clone)]
pub enum ConfirmAction {
    DeleteAccount { id: AccountId, delete_drafts: bool },
    DeleteDraft { id: DraftId },
    EmptyTrash { folder: FolderId },
    MoveToTrash { ids: Vec<MessageId> },
    OpenAttachment { path: PathBuf },
    ClearCacheAll,
    ClearCacheAccount(AccountId),
}

impl Dialog {
    pub fn close(&mut self) {
        *self = Dialog::None;
    }
}

// ---------------------------------------------------------------------------
// 账户编辑
// ---------------------------------------------------------------------------

pub struct AccountEditor {
    pub editing: Option<AccountId>,
    /// 已保存账户才能测试连接。
    pub can_test: bool,
    pub email: String,
    pub display_name: String,
    pub imap_host: String,
    pub imap_port: String,
    pub imap_tls: TlsMode,
    pub smtp_host: String,
    pub smtp_port: String,
    pub smtp_tls: TlsMode,
    pub auth_user: String,
    pub auth_secret: String,
    pub auth_method: AuthMethod,
    /// OAuth 账户元数据；密码/授权码账户为 `None`。v0.2 WP-05 接入 OAuth UI。
    pub oauth_provider: Option<me_core::OAuthProvider>,
    pub oauth_subject: Option<String>,
    pub advanced_open: bool,
    pub plaintext_ack: bool,
    pub error: Option<String>,
    pub test_result: Option<String>,
    pub busy: bool,
}

impl AccountEditor {
    pub fn new() -> Self {
        Self {
            editing: None,
            can_test: false,
            email: String::new(),
            display_name: String::new(),
            imap_host: String::new(),
            imap_port: "993".to_string(),
            imap_tls: TlsMode::Implicit,
            smtp_host: String::new(),
            smtp_port: "465".to_string(),
            smtp_tls: TlsMode::Implicit,
            auth_user: String::new(),
            auth_secret: String::new(),
            auth_method: AuthMethod::Password,
            oauth_provider: None,
            oauth_subject: None,
            advanced_open: false,
            plaintext_ack: false,
            error: None,
            test_result: None,
            busy: false,
        }
    }

    pub fn from_account(account: &Account) -> Self {
        Self {
            editing: Some(account.id),
            can_test: true,
            email: account.email.clone(),
            display_name: account.display_name.clone().unwrap_or_default(),
            imap_host: account.imap.host.clone(),
            imap_port: account.imap.port.to_string(),
            imap_tls: account.imap.tls,
            smtp_host: account.smtp.host.clone(),
            smtp_port: account.smtp.port.to_string(),
            smtp_tls: account.smtp.tls,
            auth_user: account.auth_user.clone(),
            auth_secret: String::new(),
            auth_method: account.auth_method,
            oauth_provider: account.oauth_provider,
            oauth_subject: account.oauth_subject.clone(),
            advanced_open: account.imap.tls.is_plaintext() || account.smtp.tls.is_plaintext(),
            plaintext_ack: false,
            error: None,
            test_result: None,
            busy: false,
        }
    }

    pub fn uses_plaintext_tls(&self) -> bool {
        self.imap_tls.is_plaintext() || self.smtp_tls.is_plaintext()
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.email.trim().is_empty() || !self.email.contains('@') {
            return Err("请填写有效邮箱地址".into());
        }
        if self.imap_host.trim().is_empty() {
            return Err("请填写 IMAP 服务器".into());
        }
        if self.smtp_host.trim().is_empty() {
            return Err("请填写 SMTP 服务器".into());
        }
        let imap_port = self
            .imap_port
            .trim()
            .parse::<u16>()
            .map_err(|_| "IMAP 端口无效")?;
        let smtp_port = self
            .smtp_port
            .trim()
            .parse::<u16>()
            .map_err(|_| "SMTP 端口无效")?;
        if imap_port == 0 || smtp_port == 0 {
            return Err("端口必须大于 0".into());
        }
        if self.editing.is_none() && self.auth_secret.is_empty() {
            return Err("请填写密码或授权码".into());
        }
        if self.uses_plaintext_tls() && !self.plaintext_ack {
            return Err("明文 TLS 需要勾选风险确认".into());
        }
        Ok(())
    }

    pub fn to_draft(&self) -> me_core::AccountDraft {
        me_core::AccountDraft {
            email: self.email.trim().to_string(),
            display_name: Some(self.display_name.trim().to_string()).filter(|s| !s.is_empty()),
            imap: me_core::Endpoint::new(
                self.imap_host.trim(),
                self.imap_port.trim().parse().unwrap_or(993),
                self.imap_tls,
            ),
            smtp: me_core::Endpoint::new(
                self.smtp_host.trim(),
                self.smtp_port.trim().parse().unwrap_or(465),
                self.smtp_tls,
            ),
            auth_user: if self.auth_user.trim().is_empty() {
                self.email.trim().to_string()
            } else {
                self.auth_user.trim().to_string()
            },
            auth_secret: if self.auth_secret.is_empty() {
                None
            } else {
                Some(self.auth_secret.clone())
            },
            auth_method: self.auth_method,
            oauth_provider: self.oauth_provider,
            oauth_subject: self.oauth_subject.clone(),
            plaintext_tls_ack: self.plaintext_ack,
        }
    }
}

// ---------------------------------------------------------------------------
// 撰写
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeStatus {
    Editing,
    Saving,
    Sending,
}

#[derive(Clone)]
pub struct ComposeState {
    pub account: AccountId,
    pub draft_id: Option<DraftId>,
    pub message_id: String,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub attachments: Vec<PathBuf>,
    pub status: ComposeStatus,
    pub error: Option<String>,
    pub may_have_been_sent: bool,
    pub last_failure: Option<String>,
    /// 退出时先落草稿，DraftSaved 后关闭窗口。
    pub exit_after_save: bool,
    /// 关闭前的未保存确认。
    pub confirm_close: bool,
    /// 重发前确认“可能已发送”。
    pub confirm_resend: bool,
    /// 打开时的草稿快照，用于判断是否有未保存修改。
    pub initial: ComposeSnapshot,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ComposeSnapshot {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub attachments: Vec<PathBuf>,
}

impl ComposeState {
    pub fn new(account: AccountId, from_email: &str) -> Self {
        let mut state = Self {
            account,
            draft_id: None,
            message_id: me_core::generate_message_id(from_email),
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            subject: String::new(),
            body: String::new(),
            attachments: Vec::new(),
            status: ComposeStatus::Editing,
            error: None,
            may_have_been_sent: false,
            last_failure: None,
            exit_after_save: false,
            confirm_close: false,
            confirm_resend: false,
            initial: ComposeSnapshot {
                to: String::new(),
                cc: String::new(),
                bcc: String::new(),
                subject: String::new(),
                body: String::new(),
                attachments: Vec::new(),
            },
        };
        state.initial = state.snapshot();
        state
    }

    /// 由已保存草稿重建撰写状态；Bcc 从本地 `drafts.bcc_addrs` 恢复。
    pub fn from_draft(account: AccountId, draft: &me_core::DraftContent) -> Self {
        let mut state = Self::new(account, "");
        state.draft_id = Some(draft.draft_id);
        state.message_id = draft.message_id.clone();
        state.to = join_addresses(&draft.to);
        state.cc = join_addresses(&draft.cc);
        state.bcc = join_addresses(&draft.bcc);
        state.subject = draft.subject.clone();
        state.body = draft.body_text.clone();
        state.attachments = draft.attachments.clone();
        state.initial = state.snapshot();
        state
    }

    pub fn snapshot(&self) -> ComposeSnapshot {
        ComposeSnapshot {
            to: self.to.clone(),
            cc: self.cc.clone(),
            bcc: self.bcc.clone(),
            subject: self.subject.clone(),
            body: self.body.clone(),
            attachments: self.attachments.clone(),
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.snapshot() != self.initial
    }

    pub fn is_empty(&self) -> bool {
        self.to.trim().is_empty()
            && self.cc.trim().is_empty()
            && self.bcc.trim().is_empty()
            && self.subject.trim().is_empty()
            && self.body.trim().is_empty()
            && self.attachments.is_empty()
    }

    pub fn mark_saved(&mut self) {
        self.initial = self.snapshot();
        self.error = None;
        self.status = ComposeStatus::Editing;
    }
}

/// 地址列表转回输入框文本，保留显示名。
fn join_addresses(addrs: &[Address]) -> String {
    addrs
        .iter()
        .map(Address::display)
        .collect::<Vec<_>>()
        .join(", ")
}

/// 解析逗号/分号分隔的地址列表，支持 `Doe, John <john@example.com>` 的引号形式。
pub fn parse_address_list(input: &str) -> Vec<Address> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut angle_depth = 0i32;

    for ch in input.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            '<' => {
                angle_depth += 1;
                current.push(ch);
            }
            '>' => {
                angle_depth = (angle_depth - 1).max(0);
                current.push(ch);
            }
            ',' | ';' if !in_quotes && angle_depth == 0 => {
                push_address(&mut out, &current);
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    push_address(&mut out, &current);
    out
}

fn push_address(out: &mut Vec<Address>, raw: &str) {
    let raw = raw.trim();
    if raw.is_empty() {
        return;
    }
    if let (Some(start), Some(end)) = (raw.rfind('<'), raw.rfind('>'))
        && end > start
    {
        let addr = raw[start + 1..end].trim();
        let name = raw[..start].trim().trim_matches('"').trim();
        out.push(Address::new(
            if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            },
            addr,
        ));
        return;
    }
    out.push(Address::bare(raw));
}

pub fn is_valid_addr(addr: &str) -> bool {
    let addr = addr.trim();
    let Some((local, domain)) = addr.rsplit_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && !addr.contains(char::is_whitespace)
        && !addr.contains('<')
        && !addr.contains('>')
}

// ---------------------------------------------------------------------------
// 标志辅助
// ---------------------------------------------------------------------------

pub fn toggle_seen_op(message: &MessageSummary) -> FlagOp {
    if message.flags.is_seen() {
        FlagOp::Clear(Flags::SEEN)
    } else {
        FlagOp::Set(Flags::SEEN)
    }
}

pub fn toggle_star_op(message: &MessageSummary) -> FlagOp {
    if message.flags.is_flagged() {
        FlagOp::Clear(Flags::FLAGGED)
    } else {
        FlagOp::Set(Flags::FLAGGED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_list_parsing() {
        let list = parse_address_list(
            "Alice <alice@example.com>, \"Doe, John\" <john@example.com>; bob@example.com",
        );
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].addr, "alice@example.com");
        assert_eq!(list[1].name.as_deref(), Some("Doe, John"));
        assert_eq!(list[2].addr, "bob@example.com");
    }

    #[test]
    fn address_validation() {
        assert!(is_valid_addr("a@example.com"));
        assert!(!is_valid_addr("a@localhost"));
        assert!(!is_valid_addr("nope"));
    }
}
