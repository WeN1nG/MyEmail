//! UI 侧状态。退出即丢；未保存草稿需先经 `SaveDraft` 落库。

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use me_core::{
    Account, AccountId, Address, AuthMethod, ConnectionState, Contact, Draft, DraftId, FlagOp,
    Flags, Folder, FolderId, HtmlRendererMode, MessageBody, MessageId, MessageSummary, ParsedQuery,
    SavedSearch, ThemeMode, TlsMode, parse_search_syntax,
};

// ---------------------------------------------------------------------------
// 页面导航
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Mail,
    Calendar,
    Weather,
    AiChat,
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
            Page::AiChat => "AI",
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
/// AI 音乐工具正在等待的目标状态。
///
/// UI 是 immediate 模式，没有后台任务可等，所以「等状态达成」只能按帧轮询：
/// `poll_ai_music` 每帧拿当前状态与这里比一次，达成或超时就回执。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MusicExpect {
    /// 指定索引的那首正在播放。
    Playing(usize),
    /// 已经不在播放（暂停或停止）。
    Paused,
    /// 已经停止且没有当前曲目。
    Stopped,
    /// 当前曲目不再是这个索引（切歌了）。
    Switched(Option<usize>),
}

/// 等待回执的音乐操作。同一时刻只保留一条：AI 一轮里连发两个播放命令没有意义。
#[derive(Debug, Clone)]
pub struct PendingAiMusic {
    pub call_id: String,
    pub expect: MusicExpect,
    pub deadline: Instant,
    /// 给用户看的一句话（动作描述），回执时补上结果状态。
    pub action_label: String,
}

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
    /// AI 音乐工具等待中的操作；`None` = 没有待回执的调用。
    pub ai_pending: Option<PendingAiMusic>,
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
            ai_pending: None,
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
    /// 统一收件箱的未读数（跨账户合计）。
    pub unified_unread_count: u32,
    pub connection: BTreeMap<AccountId, ConnectionState>,
    /// 实时同步状态（WP-08 在设置/账户页展示）。
    pub realtime: BTreeMap<AccountId, RealtimeView>,

    pub selected_account: Option<AccountId>,
    pub selected_folder: Option<FolderId>,
    /// 是否在看**统一收件箱**（各账户收件箱的并集）。
    ///
    /// **与 `selected_folder` 互斥**：统一收件箱是**视图层的虚拟项**，
    /// 不是数据库里的文件夹行——`folders.role` 有 CHECK 约束且
    /// `account_id NOT NULL`，插不进一行「属于所有账户的收件箱」。
    ///
    /// 为什么用 `bool` 而不是把 `selected_folder` 改成
    /// `enum MailScope { Folder(FolderId), UnifiedInbox }`：后者更「干净」，
    /// 但 `selected_folder` 被十几处引用，改它会波及整个 `me-app`——
    /// **收益不抵风险**。互斥关系**集中在这两个方法里维护**，
    /// 禁止别处直接写字段。
    pub unified_inbox: bool,
    pub messages: Vec<MessageSummary>,
    /// 键集分页游标：上一页最后一行的 `(date, id)`。
    pub messages_cursor: Option<(i64, i64)>,
    pub messages_has_more: bool,

    pub selected_message: Option<MessageId>,
    /// 多选集合（批量操作）。**与 `selected_message` 并存**——
    /// 单选点击保留 `selected_message`（阅读窗靠它），`Ctrl`/`Shift` 点击进集合。
    ///
    /// 集合里的 id **必须都属于当前列表**——切文件夹 / 切账户 / 退出搜索时清空，
    /// 否则会出现「选了 INBOX 的 3 封 + Archive 的 2 封」这种语义混乱。
    pub selected_messages: HashSet<MessageId>,
    /// `Shift` 连选的锚点。`Shift` 点击时选「锚点到目标」之间的连续区间。
    pub list_selection_anchor: Option<MessageId>,
    /// 批量删除的确认卡是否打开（`None` = 不打开）。
    ///
    /// 沿用 AI 页 `pending_delete` 的先例——删除是破坏性操作，要先确认。
    pub pending_batch_delete: Option<Vec<MessageId>>,
    /// 阅读窗是否打开。**不能**用 `selected_message.is_some()` 代替：
    /// 切页时 `selected_message` 不清（方便回到列表继续看），那会让阅读窗
    /// 跟着用户跑到每一个页面上；AI 页的卡片又需要它在 AI 页原地弹出。
    pub reader_open: bool,
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
    /// AI 对话页状态（会话/消息/流缓冲/设置草稿）。
    pub ai: crate::views::ai::AiPageState,
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
    /// AI：会话列表 / 当前会话消息 / 实体注册表。
    pub ai_conversations_dirty: bool,
    pub ai_messages_dirty: bool,
    pub ai_entities_dirty: bool,
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
            unified_inbox: false,
            unified_unread_count: 0,
            messages: Vec::new(),
            messages_cursor: None,
            messages_has_more: false,
            selected_message: None,
            selected_messages: HashSet::new(),
            list_selection_anchor: None,
            pending_batch_delete: None,
            reader_open: false,
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
            ai: crate::views::ai::AiPageState::default(),
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
            ai_conversations_dirty: true,
            ai_messages_dirty: true,
            ai_entities_dirty: true,
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
    // --- 多选（批次 2）---

    /// 裸点击：清空多选集合，设单选。
    pub fn select_only(&mut self, id: MessageId) {
        self.selected_messages.clear();
        self.selected_message = Some(id);
        self.list_selection_anchor = Some(id);
    }

    /// `Ctrl` 点击：切换单封的选中态。
    pub fn toggle_selected(&mut self, id: MessageId) {
        if !self.selected_messages.remove(&id) {
            self.selected_messages.insert(id);
        }
        // 锚点跟着最后一次操作走——`Shift` 连选以它为起点。
        self.list_selection_anchor = Some(id);
        // 单选值跟着走：集合非空时，单选值应该是集合里的一封。
        self.selected_message = Some(id);
    }

    /// `Shift` 点击：选「锚点到目标」之间的连续区间。
    ///
    /// **区间按当前列表的行序**算——与用户看到的顺序一致。
    /// 锚点不在当前列表里（切了文件夹没清）时退化成「只选这一封」。
    pub fn select_range(&mut self, id: MessageId, ordered: &[MessageId]) {
        let Some(anchor) = self.list_selection_anchor else {
            self.selected_messages.insert(id);
            self.selected_message = Some(id);
            return;
        };
        let (Some(start), Some(end)) = (
            ordered.iter().position(|m| *m == anchor),
            ordered.iter().position(|m| *m == id),
        ) else {
            // 锚点不在列表里——退化成单选。
            self.selected_messages.clear();
            self.selected_messages.insert(id);
            self.selected_message = Some(id);
            return;
        };
        let (lo, hi) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        self.selected_messages.clear();
        for m in &ordered[lo..=hi] {
            self.selected_messages.insert(*m);
        }
        self.selected_message = Some(id);
    }

    /// 清空多选集合与锚点。
    ///
    /// 切文件夹 / 切账户 / 退出搜索时**必须**调用——
    /// 否则集合里会留下不属于当前列表的 id。
    pub fn clear_multi_selection(&mut self) {
        self.selected_messages.clear();
        self.list_selection_anchor = None;
        self.pending_batch_delete = None;
    }

    /// 批量操作的目标 id 列表。
    ///
    /// 集合为空时退化为单选值——这样「选中 1 封」与「没选但有单选」
    /// 两种状态都能用同一套批量按钮。
    pub fn batch_targets(&self) -> Vec<MessageId> {
        if self.selected_messages.is_empty() {
            return self.selected_message.into_iter().collect();
        }
        // **按列表顺序**返回——用户期望操作顺序与看到的一致。
        let mut ids: Vec<MessageId> = self
            .messages
            .iter()
            .map(|m| m.id)
            .filter(|id| self.selected_messages.contains(id))
            .collect();
        // 列表里没有的（理论上不该有）也要带上——不能静默漏掉。
        let missing: Vec<MessageId> = self
            .selected_messages
            .iter()
            .filter(|id| !ids.contains(id))
            .copied()
            .collect();
        ids.extend(missing);
        ids
    }

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

    /// 撰写页状态（可写）。
    pub fn compose_mut(&mut self) -> Option<&mut ComposeState> {
        match &mut self.dialog {
            Dialog::Compose(compose) => Some(compose),
            _ => None,
        }
    }

    // --- 统一收件箱（批次 4）---

    /// 选中统一收件箱。
    ///
    /// **互斥关系的唯一入口**——所有切到统一收件箱的地方都要走这里，
    /// 否则会出现「`unified_inbox = true` 且 `selected_folder = Some(x)`」
    /// 的矛盾态，列表取数会二义。
    pub fn select_unified_inbox(&mut self) {
        self.unified_inbox = true;
        self.selected_folder = None;
    }

    /// 选中某个文件夹（退出统一收件箱）。
    ///
    /// **互斥关系的唯一入口**。
    pub fn select_folder(&mut self, folder: FolderId) {
        self.unified_inbox = false;
        self.selected_folder = Some(folder);
    }

    /// 退出文件夹选择（初始状态 / 切账户）。
    pub fn clear_folder_selection(&mut self) {
        self.unified_inbox = false;
        self.selected_folder = None;
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

    /// 切换 AI 会话：清掉上一个会话的临时 UI，并让主区重查一次消息。
    ///
    /// 置脏必须和选中一起做。视图里只调 `ai.select()` 的话，页内切换会话后
    /// 消息既不重查也不报错，主区就是一片空白——只有切到别的页再回来时，
    /// `switch_page` 顺手置脏才把它盖住。
    pub fn select_ai_conversation(&mut self, id: Option<me_core::ai::AiConversationId>) {
        let changed = self.ai.selected != id;
        self.ai.select(id);
        if changed {
            self.ai_messages_dirty = true;
        }
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

    /// 统一收件箱的未读数（跨账户合计）。
    ///
    /// 单独一个字段而不是把各账户的加总——**各账户的未读数是按 folder 缓存的**，
    /// 统一收件箱的聚合范围（`role='inbox'` 的所有文件夹）与之不同，
    /// 只能单独查。刷新时填入。
    pub fn unified_unread(&self) -> u32 {
        self.unified_unread_count
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
    // --- 速记语法与条件面板（批次 1a） ---
    /// 条件面板里的发件人（`from`）。
    pub from: String,
    /// 条件面板里的收件人（`to`）。
    pub to: String,
    /// 条件面板里的主题（`subject`）。
    pub subject: String,
    /// 条件面板里的「仅未读」。
    pub unread_only: bool,
    /// 上一次解析的结果。**搜索框内容变了但没回车时，界面显示的还是旧条件**——
    /// 这个不一致必须避免，所以 chips 由 `parsed` 派生（见 `sync_parsed`）。
    pub parsed: ParsedQuery,
    /// 已解析条件的展示列表（chips），每项可单独删除。
    pub conditions: Vec<ConditionChip>,
    /// 保存的搜索列表（`Event::SavedSearchesChanged` 的快照）。
    pub saved_searches: Vec<SavedSearch>,
    /// 「保存此搜索」弹窗的输入框内容（空 = 未打开）。
    pub save_search_name: String,
    /// 「保存此搜索」弹窗是否打开。
    pub show_save_search: bool,
}

/// 条件标签的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipKind {
    From,
    To,
    Subject,
    HasAttachment,
    Before,
    After,
    Unread,
}

/// 单个条件标签。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionChip {
    /// 展示文案，如「发件人含 zhang」。
    pub label: String,
    pub kind: ChipKind,
    /// 条件的值。删除标签时用它回填条件面板。
    pub value: String,
}

impl SearchState {
    /// 「1～2 字仅匹配头部」的提示是否该显示。
    ///
    /// **只看正文部分的字符数**（`parsed.text`），不是原始输入——
    /// `from:ab` 走的是 `from` 的 `LIKE`，与正文全文检索无关，
    /// 用原始输入判定会误报。
    pub fn is_short_query(&self) -> bool {
        (1..3).contains(&self.parsed.text_len())
    }

    /// 是否是空查询。
    ///
    /// **任何条件存在即非空**——速记语法下 `from:zhang` 的 `text` 为空，
    /// 只看 `text` 会把它判成空查询，点搜索直接清空搜索框、什么都不搜。
    pub fn is_empty_query(&self) -> bool {
        self.parsed.is_empty()
            && self.from.trim().is_empty()
            && self.to.trim().is_empty()
            && self.subject.trim().is_empty()
            && !self.unread_only
            && self.has_attachment.is_none()
            && self.since_text.trim().is_empty()
            && self.until_text.trim().is_empty()
    }

    /// 用当前输入重新解析速记语法，并重建 chips。
    ///
    /// 由调用方在**搜索框内容变化后**调用，保证界面显示的条件与输入一致。
    pub fn sync_parsed(&mut self, today: i64) {
        self.parsed = parse_search_syntax(&self.text, today);
        self.conditions = build_chips(&self.parsed, self);
    }

    /// 删除某个条件标签：同时清掉对应的面板输入。
    pub fn remove_chip(&mut self, index: usize) {
        if index >= self.conditions.len() {
            return;
        }
        let chip = self.conditions.remove(index);
        match chip.kind {
            ChipKind::From => self.from.clear(),
            ChipKind::To => self.to.clear(),
            ChipKind::Subject => self.subject.clear(),
            ChipKind::HasAttachment => self.has_attachment = None,
            ChipKind::Before => self.until_text.clear(),
            ChipKind::After => self.since_text.clear(),
            ChipKind::Unread => self.unread_only = false,
        }
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
        self.from.clear();
        self.to.clear();
        self.subject.clear();
        self.unread_only = false;
        self.parsed = ParsedQuery::default();
        self.conditions.clear();
        self.save_search_name.clear();
        self.show_save_search = false;
    }

    pub fn coverage_label(&self) -> String {
        format!("正文索引覆盖 {}/{} 封", self.body_indexed, self.body_total)
    }

    /// 附件条件的展示文案（`None` 时返回空串）。
    pub fn has_attachment_label(&self) -> String {
        match self.has_attachment {
            Some(true) => "仅含附件".to_string(),
            Some(false) => "不含附件".to_string(),
            None => String::new(),
        }
    }
}

/// 由解析结果 + 面板状态生成条件标签。
///
/// 面板里的条件（`from` / `to` / `subject` / 时间 / 附件 / 仅未读）与
/// 速记语法解析出的条件**合并展示**——用户在面板里填的也是条件，
/// 两者都该看得见。
fn build_chips(parsed: &ParsedQuery, state: &SearchState) -> Vec<ConditionChip> {
    let mut chips = Vec::new();
    let push = |chips: &mut Vec<ConditionChip>, kind: ChipKind, value: &str, label: String| {
        if value.trim().is_empty() {
            return;
        }
        chips.push(ConditionChip {
            label,
            kind,
            value: value.trim().to_string(),
        });
    };

    // 面板里的条件优先（面板是显式操作，速记语法是输入框里的文本）。
    push(
        &mut chips,
        ChipKind::From,
        &state.from,
        format!("发件人含 {}", state.from.trim()),
    );
    push(
        &mut chips,
        ChipKind::To,
        &state.to,
        format!("收件人含 {}", state.to.trim()),
    );
    push(
        &mut chips,
        ChipKind::Subject,
        &state.subject,
        format!("主题含 {}", state.subject.trim()),
    );
    push(
        &mut chips,
        ChipKind::HasAttachment,
        &state.has_attachment_label(),
        state.has_attachment_label(),
    );
    push(
        &mut chips,
        ChipKind::After,
        &state.since_text,
        format!("晚于 {}", state.since_text.trim()),
    );
    push(
        &mut chips,
        ChipKind::Before,
        &state.until_text,
        format!("早于 {}", state.until_text.trim()),
    );
    if state.unread_only {
        push(&mut chips, ChipKind::Unread, "unread", "仅未读".to_string());
    }

    // 速记语法解析出的、面板里没有的条件（避免同一个条件显示两次）。
    if state.from.trim().is_empty()
        && let Some(v) = &parsed.from
    {
        push(&mut chips, ChipKind::From, v, format!("发件人含 {v}"));
    }
    if state.to.trim().is_empty()
        && let Some(v) = &parsed.to
    {
        push(&mut chips, ChipKind::To, v, format!("收件人含 {v}"));
    }
    if state.subject.trim().is_empty()
        && let Some(v) = &parsed.subject
    {
        push(&mut chips, ChipKind::Subject, v, format!("主题含 {v}"));
    }
    chips
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
    /// 收件人输入框的补全候选（撰写页）。
    ///
    /// 由 `refresh_contact_suggestions` 按当前输入的前缀查出——
    /// **不持久化、不缓存**，每次输入变化重查（联系人表很小，读一次几微秒）。
    pub contact_suggestions: Vec<Contact>,
    /// 补全候选是针对哪个输入框的（收件人 / 抄送 / 密送）。
    pub contact_suggestion_field: ContactField,
    /// 补全候选上次查询时用的前缀——变了才需要重查。
    pub contact_suggestion_prefix: String,
    /// 哪个地址框最近被编辑过（决定补全针对哪个）。
    pub contact_focus: ContactField,
}

/// 补全针对哪个地址输入框。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactField {
    To,
    Cc,
    Bcc,
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
    /// 补全候选的前缀：从当前地址输入框里取出**最后一段**。
    ///
    /// 地址框里的内容是「a@example.com, Name <b@example.com>」这样的列表，
    /// 补全只对**正在输入的那一段**有意义——所以取最后一个逗号之后的部分。
    pub fn suggestion_prefix(&self, field: ContactField) -> String {
        let raw = match field {
            ContactField::To => &self.to,
            ContactField::Cc => &self.cc,
            ContactField::Bcc => &self.bcc,
        };
        let last = raw.rsplit(',').next().unwrap_or("");
        // `Name <prefix` 形态里只取尖括号内的部分。
        let prefix = match last.find('<') {
            Some(idx) => &last[idx + 1..],
            None => last,
        };
        prefix.trim().to_string()
    }

    /// 把选中的联系人填回地址框。
    ///
    /// **替换最后一段**（正在输入的那一段），前面的部分保持不变。
    pub fn apply_suggestion(&mut self, field: ContactField, contact: &Contact) {
        let display = match &contact.display_name {
            Some(name) if !name.trim().is_empty() => {
                format!("{} <{}>", name.trim(), contact.addr)
            }
            _ => contact.addr.clone(),
        };
        let target = match field {
            ContactField::To => &mut self.to,
            ContactField::Cc => &mut self.cc,
            ContactField::Bcc => &mut self.bcc,
        };
        match target.rfind(',') {
            Some(idx) => {
                target.truncate(idx + 1);
                target.push(' ');
                target.push_str(&display);
            }
            None => {
                target.clear();
                target.push_str(&display);
            }
        }
    }

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
            contact_suggestions: Vec::new(),
            contact_suggestion_field: ContactField::To,
            contact_suggestion_prefix: String::new(),
            contact_focus: ContactField::To,
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

/// 测试用：构造一个最小可用的账户。
///
/// 只填渲染需要的字段（`id` / `email`），端点之类给占位值——
/// `Account` 有十几个字段，逐个手写会让每个渲染测试都变成噪音。
#[cfg(test)]
pub(crate) fn sample_account(id: i64, email: &str) -> Account {
    Account {
        id: AccountId(id),
        email: email.to_string(),
        display_name: None,
        imap: me_core::Endpoint::new("imap.example.com", 993, TlsMode::Implicit),
        smtp: me_core::Endpoint::new("smtp.example.com", 465, TlsMode::Implicit),
        auth_user: email.to_string(),
        auth_method: AuthMethod::AppPassword,
        oauth_provider: None,
        oauth_subject: None,
        needs_reauth: false,
        idle_enabled: true,
        poll_interval_secs: None,
        token_expires_at: None,
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

    #[test]
    fn switching_ai_conversation_marks_messages_for_reload() {
        let mut state = UiState::default();
        use me_core::ai::AiConversationId;

        state.ai_messages_dirty = false;
        state.select_ai_conversation(Some(AiConversationId(1)));
        assert!(
            state.ai_messages_dirty,
            "页内切换会话必须重查消息，否则主区一直空白"
        );

        state.ai_messages_dirty = false;
        state.select_ai_conversation(Some(AiConversationId(1)));
        assert!(!state.ai_messages_dirty, "点同一条不用白跑一次查询");

        state.select_ai_conversation(Some(AiConversationId(2)));
        assert!(state.ai_messages_dirty);
        assert!(state.ai.messages.is_empty(), "切会话要清掉上一条的缓冲");
        assert!(state.ai.loading, "等查询回来之前先显示加载中");
    }

    // --- 联系人补全（批次 3）---

    #[test]
    fn suggestion_prefix_takes_the_last_segment() {
        let mut compose = ComposeState::new(AccountId(1), "me@example.com");
        compose.to = "a@example.com, b@example.com".to_string();
        assert_eq!(compose.suggestion_prefix(ContactField::To), "b@example.com");

        compose.to = "a@example.com, zhang".to_string();
        assert_eq!(compose.suggestion_prefix(ContactField::To), "zhang");
    }

    #[test]
    fn suggestion_prefix_handles_the_name_angle_bracket_form() {
        let mut compose = ComposeState::new(AccountId(1), "me@example.com");
        compose.to = "Alice <alice@example.com>, Zh".to_string();
        assert_eq!(compose.suggestion_prefix(ContactField::To), "Zh");

        compose.to = "Alice <ali".to_string();
        assert_eq!(compose.suggestion_prefix(ContactField::To), "ali");
    }

    #[test]
    fn applying_a_suggestion_replaces_the_last_segment() {
        let mut compose = ComposeState::new(AccountId(1), "me@example.com");
        compose.to = "a@example.com, zhang".to_string();
        compose.apply_suggestion(
            ContactField::To,
            &Contact {
                id: me_core::ContactId(1),
                addr: "zhang@example.com".to_string(),
                normalized: "zhang@example.com".to_string(),
                display_name: Some("张三".to_string()),
                last_seen: 0,
                seen_count: 1,
            },
        );
        assert_eq!(compose.to, "a@example.com, 张三 <zhang@example.com>");
    }

    #[test]
    fn applying_a_suggestion_without_a_name_uses_the_plain_address() {
        let mut compose = ComposeState::new(AccountId(1), "me@example.com");
        compose.to = "zhang".to_string();
        compose.apply_suggestion(
            ContactField::To,
            &Contact {
                id: me_core::ContactId(1),
                addr: "zhang@example.com".to_string(),
                normalized: "zhang@example.com".to_string(),
                display_name: None,
                last_seen: 0,
                seen_count: 1,
            },
        );
        assert_eq!(compose.to, "zhang@example.com");
    }

    #[test]
    fn applying_a_suggestion_to_cc_and_bcc_uses_the_right_field() {
        let mut compose = ComposeState::new(AccountId(1), "me@example.com");
        compose.cc = "old@example.com".to_string();
        compose.bcc = "secret@example.com".to_string();
        let contact = Contact {
            id: me_core::ContactId(1),
            addr: "new@example.com".to_string(),
            normalized: "new@example.com".to_string(),
            display_name: None,
            last_seen: 0,
            seen_count: 1,
        };
        compose.apply_suggestion(ContactField::Cc, &contact);
        compose.apply_suggestion(ContactField::Bcc, &contact);
        assert_eq!(compose.cc, "new@example.com");
        assert_eq!(compose.bcc, "new@example.com");
        // 收件人没被动过。
        assert!(compose.to.is_empty());
    }

    // --- 统一收件箱（批次 4）---

    #[test]
    fn unified_inbox_and_folder_selection_are_mutually_exclusive() {
        let mut state = UiState::default();
        state.select_folder(FolderId(1));
        assert!(!state.unified_inbox);
        assert_eq!(state.selected_folder, Some(FolderId(1)));

        state.select_unified_inbox();
        assert!(state.unified_inbox);
        assert_eq!(
            state.selected_folder, None,
            "选统一收件箱必须清掉文件夹选择"
        );

        state.select_folder(FolderId(2));
        assert!(!state.unified_inbox, "选文件夹必须退出统一收件箱");
        assert_eq!(state.selected_folder, Some(FolderId(2)));
    }

    #[test]
    fn clearing_folder_selection_also_exits_unified_inbox() {
        let mut state = UiState::default();
        state.select_unified_inbox();
        state.clear_folder_selection();
        assert!(!state.unified_inbox);
        assert_eq!(state.selected_folder, None);
    }

    #[test]
    fn unified_unread_count_starts_at_zero() {
        let state = UiState::default();
        assert_eq!(state.unified_unread(), 0);
    }
}
