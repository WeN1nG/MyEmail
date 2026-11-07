//! eframe 主循环：抽取事件（每帧 ≤ 256）→ 懒加载脏视图 → 渲染 → 分发 Command。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::audio::{AudioCommand, AudioEvent, AudioPlayer};
use eframe::egui;
use me_core::MusicOrder;
use me_core::calendar::{CalendarEventId, RecurrenceLimits};
use me_core::{Command, CoreError, ErrorScope, Event, ReadOnlyRepo, ThemeMode};
use tokio::sync::mpsc;

use crate::state::{Dialog, Page, ToastKind, UiError, UiState};
use crate::views;
use crate::views::calendar::OccurrenceView;

/// 每帧事件处理上限，防止十万级事件涌入时单帧卡死。
const MAX_EVENTS_PER_FRAME: usize = 256;
const PAGE_SIZE: u32 = 200;

/// UI ↔ 后端的通道句柄。
pub struct BackendHandle {
    command_tx: mpsc::UnboundedSender<Command>,
    event_rx: mpsc::Receiver<Event>,
}

impl BackendHandle {
    pub fn new(
        command_tx: mpsc::UnboundedSender<Command>,
        event_rx: mpsc::Receiver<Event>,
    ) -> Self {
        Self {
            command_tx,
            event_rx,
        }
    }

    pub fn send(&self, command: Command) -> bool {
        self.command_tx.send(command).is_ok()
    }

    pub fn try_recv(&mut self) -> Option<Event> {
        self.event_rx.try_recv().ok()
    }
}

pub struct MyEmailApp<R: ReadOnlyRepo + Clone + 'static> {
    pub state: UiState,
    pub backend: BackendHandle,
    pub repo: R,
    pub cache_dir: Option<PathBuf>,
    pub images: crate::images::RemoteImageLoader,
    pub html: crate::views::html::HtmlRenderer,
    /// 第二个实例请求激活已有窗口时由 IPC 线程置位。
    pub activation: Arc<AtomicBool>,
    /// 上次渲染时应用的视觉效果，避免每帧重复设置。
    applied_theme: Option<ThemeMode>,
    /// 音频线程门面：首次进入音乐页惰性启动，切换页面不停止播放。
    pub player: Option<AudioPlayer>,
}

impl<R: ReadOnlyRepo + Clone + 'static> MyEmailApp<R> {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        backend: BackendHandle,
        repo: R,
        cache_dir: Option<PathBuf>,
        images: crate::images::RemoteImageLoader,
        html: crate::views::html::HtmlRenderer,
        activation: Arc<AtomicBool>,
    ) -> Self {
        crate::fonts::install(&cc.egui_ctx);

        let state = UiState::default();

        let mut app = Self {
            state,
            backend,
            repo,
            cache_dir,
            images,
            html,
            activation,
            applied_theme: None,
            player: None,
        };
        app.refresh_accounts();
        app.refresh_settings();
        app
    }

    // -----------------------------------------------------------------------
    // 查询（只读、键集分页、LIMIT ≤ 200）
    // -----------------------------------------------------------------------

    fn refresh_settings(&mut self) {
        // 设置读取经只读接口不可用；由 `AccountsLoaded` 之前的默认值兜底，
        // 后续版本再加只读 meta 接口。
    }

    fn refresh_accounts(&mut self) {
        match self.repo.list_accounts_readonly() {
            Ok(accounts) => {
                if self.state.selected_account.is_none() {
                    self.state.selected_account = accounts.first().map(|a| a.id);
                }
                self.state.accounts = accounts;
                self.state.folders_dirty = true;
            }
            Err(err) => self.report_error(ErrorScope::Global, err),
        }
    }

    fn refresh_folders(&mut self) {
        let Some(account) = self.state.selected_account else {
            self.state.folders_dirty = false;
            return;
        };
        match self.repo.list_folders_readonly(account) {
            Ok(folders) => {
                for folder in &folders {
                    if let Ok(count) = self.repo.unread_count_readonly(folder.id) {
                        self.state.unread.insert(folder.id, count);
                    }
                }
                // 统一收件箱的未读数：**跨账户合计**，
                // 范围是所有账户 `role='inbox'` 的文件夹——与单个 folder 的
                // 未读数不是一回事，只能单独查。
                if let Ok(count) = self.repo.unified_unread_count_readonly() {
                    self.state.unified_unread_count = count;
                }
                let selected_missing = self
                    .state
                    .selected_folder
                    .map(|selected| !folders.iter().any(|f| f.id == selected))
                    .unwrap_or(true);
                // **统一收件箱模式下不自动选文件夹**——那会把视图切走。
                // 统一收件箱的未读数是跨账户的，单独查。
                if !self.state.unified_inbox
                    && (self.state.selected_folder.is_none() || selected_missing)
                {
                    // **走集中方法**——它会同时把 `unified_inbox` 置 false，
                    // 保持两者互斥。
                    if let Some(folder) = folders
                        .iter()
                        .find(|f| f.role == me_core::FolderRole::Inbox)
                        .or_else(|| folders.first())
                    {
                        self.state.select_folder(folder.id);
                    }
                    self.state.messages.clear();
                    self.state.selected_message = None;
                    // 列表换了 → 多选集合里的 id 不再属于当前列表，必须清空。
                    self.state.clear_multi_selection();
                    self.state.open_body = None;
                }
                self.state.folders.insert(account, folders);
                self.state.folders_dirty = false;
                self.state.messages_dirty = true;
            }
            Err(err) => {
                self.state.folders_dirty = false;
                self.report_error(ErrorScope::Account(account), err);
            }
        }
    }

    fn refresh_messages(&mut self) {
        if self.state.search.active {
            self.state.messages_dirty = false;
            return;
        }
        // **统一收件箱**：跨账户聚合，在 store 层一次查完
        // （UI 端拉完各账户再合并会破坏键集分页，也违反 LIMIT ≤ 200）。
        if self.state.unified_inbox {
            match self.repo.list_unified_inbox_readonly(None, PAGE_SIZE) {
                Ok(messages) => {
                    self.state.messages_has_more = messages.len() as u32 == PAGE_SIZE;
                    self.state.messages_cursor = messages.last().map(cursor_of);
                    self.state.messages = messages;
                    self.state.selected_message = None;
                    self.state.clear_multi_selection();
                    self.state.open_body = None;
                    self.state.messages_dirty = false;
                }
                Err(err) => {
                    self.state.messages_dirty = false;
                    self.report_error(ErrorScope::Global, err);
                }
            }
            return;
        }
        let Some(folder) = self.state.selected_folder else {
            self.state.messages.clear();
            self.state.messages_dirty = false;
            return;
        };
        match self.repo.list_messages_readonly(folder, None, PAGE_SIZE) {
            Ok(messages) => {
                self.state.messages_has_more = messages.len() as u32 == PAGE_SIZE;
                self.state.messages_cursor = messages.last().map(cursor_of);
                self.state.messages = messages;
                self.state.selected_message = None;
                // 列表换了 → 多选集合里的 id 不再属于当前列表，必须清空。
                self.state.clear_multi_selection();
                self.state.open_body = None;
                self.state.messages_dirty = false;
            }
            Err(err) => {
                self.state.messages_dirty = false;
                self.report_error(ErrorScope::Folder(folder), err);
            }
        }
    }

    fn load_more_messages(&mut self) {
        if self.state.search.active || !self.state.messages_has_more {
            return;
        }
        let cursor = self.state.messages_cursor;
        if self.state.unified_inbox {
            match self.repo.list_unified_inbox_readonly(cursor, PAGE_SIZE) {
                Ok(more) => {
                    self.state.messages_has_more = more.len() as u32 == PAGE_SIZE;
                    if let Some(last) = more.last() {
                        self.state.messages_cursor = Some(cursor_of(last));
                    }
                    self.state.messages.extend(more);
                }
                Err(err) => self.report_error(ErrorScope::Global, err),
            }
            return;
        }
        let Some(folder) = self.state.selected_folder else {
            return;
        };
        match self.repo.list_messages_readonly(folder, cursor, PAGE_SIZE) {
            Ok(more) => {
                self.state.messages_has_more = more.len() as u32 == PAGE_SIZE;
                if let Some(last) = more.last() {
                    self.state.messages_cursor = Some(cursor_of(last));
                }
                for message in more {
                    if !self.state.messages.iter().any(|m| m.id == message.id) {
                        self.state.messages.push(message);
                    }
                }
                me_core::sort_newest_first(&mut self.state.messages);
            }
            Err(err) => self.report_error(ErrorScope::Folder(folder), err),
        }
    }

    fn refresh_drafts(&mut self) {
        match self.repo.list_drafts_readonly(None) {
            Ok(drafts) => {
                self.state.drafts = drafts;
                self.state.drafts_dirty = false;
            }
            Err(err) => {
                self.state.drafts_dirty = false;
                self.report_error(ErrorScope::Global, err);
            }
        }
    }

    /// 重查撰写页的联系人补全候选。
    ///
    /// **前缀没变就不查**——每次按键都查库没必要（联系人表虽小，
    /// 但这是主线程路径）。
    fn refresh_contact_suggestions(&mut self) {
        let Some(compose) = self.state.compose_mut() else {
            return;
        };
        let field = compose.contact_focus;
        let prefix = compose.suggestion_prefix(field);
        if prefix == compose.contact_suggestion_prefix {
            return;
        }
        // 前缀太短时**不查**——「a」能匹配所有人，候选列表没意义。
        let contacts = if prefix.len() < 2 {
            Vec::new()
        } else {
            // 查失败**不报错**——补全是辅助功能，不该弹错误打扰用户。
            self.repo
                .list_contacts_readonly(&prefix, 8)
                .unwrap_or_default()
        };
        if let Some(compose) = self.state.compose_mut() {
            compose.contact_suggestion_prefix = prefix;
            compose.contact_suggestion_field = field;
            compose.contact_suggestions = contacts;
        }
    }

    /// 重查天气页数据：地点列表 + 当前地点的快照。
    fn refresh_ai_conversations(&mut self) {
        self.state.ai_conversations_dirty = false;
        let list = match self.repo.list_ai_conversations_readonly(None, 100) {
            Ok(list) => list,
            Err(err) => {
                self.report_error(ErrorScope::Global, err);
                return;
            }
        };
        let selected_missing = self
            .state
            .ai
            .selected
            .is_none_or(|id| !list.iter().any(|item| item.id == id));
        if selected_missing {
            // 没有选中或选中的会话已被删除：落到第一条。
            match list.first() {
                Some(first) => {
                    let id = first.id;
                    self.state.select_ai_conversation(Some(id));
                }
                None => self.state.select_ai_conversation(None),
            }
        }
        self.state.ai.conversations = list;
    }

    fn refresh_ai_messages(&mut self) {
        self.state.ai_messages_dirty = false;
        let Some(conversation) = self.state.ai.selected else {
            self.state.ai.messages.clear();
            return;
        };
        match self.repo.list_ai_messages_readonly(conversation, None, 200) {
            Ok(messages) => {
                // 清单跟着最后一条带清单的助手消息走：它是这一轮需求的最新进度，
                // 上一条消息的清单已经过期，继续显示会让人以为任务还在跑。
                self.state.ai.plan = messages.iter().rev().find_map(|item| item.plan.clone());
                self.state.ai.messages = messages;
                self.state.ai.loading = false;
            }
            Err(err) => self.report_error(ErrorScope::Global, err),
        }
    }

    fn refresh_ai_entities(&mut self) {
        self.state.ai_entities_dirty = false;
        match self.repo.list_ai_entities_readonly(64) {
            Ok(list) => self.state.ai.entities = list,
            Err(err) => self.report_error(ErrorScope::Global, err),
        }
    }

    fn refresh_weather(&mut self) {
        self.state.weather.dirty = false;
        let locations = match self.repo.list_weather_locations_readonly() {
            Ok(locations) => locations,
            Err(err) => {
                self.report_error(ErrorScope::Global, err);
                return;
            }
        };
        self.state.weather.locations = locations;
        // 认领刚添加的地点：它入库后才有 id，按服务商地点 id 匹配。
        if let Some(pending) = self.state.weather.pending_select.clone()
            && let Some(location) = self
                .state
                .weather
                .locations
                .iter()
                .find(|item| item.provider_location_id == pending)
        {
            self.state.weather.selected = Some(location.id);
            self.state.weather.pending_select = None;
        }
        let active = self.state.weather.active_location_id();
        match active {
            Some(id) => match self.repo.get_weather_snapshot_readonly(id) {
                Ok(snapshot) => {
                    self.state.weather.snapshot = snapshot;
                    // 记下这份快照属于哪个地点：切换地点后，
                    // 拿旧地点的快照去判断「是否过期」会得出错误结论。
                    self.state.weather.snapshot_location = Some(id);
                }
                Err(err) => self.report_error(ErrorScope::Global, err),
            },
            None => {
                self.state.weather.snapshot = None;
                self.state.weather.snapshot_location = None;
            }
        }
    }

    /// 页面停在天气页时，当前地点的缓存缺失或过期就自动刷新一次（每地点 60 秒节流）。
    fn maybe_auto_refresh_weather(&mut self, commands: &mut Vec<Command>) {
        if !self.state.weather.key_present {
            return;
        }
        let Some(location_id) = self.state.weather.active_location_id() else {
            return;
        };
        let now = me_core::now_unix();
        // 每个地点各自节流：刚刷新过 A 城，不该挡住切换到 B 城后的首次请求。
        if let Some(last) = self.state.weather.auto_refresh_at.get(&location_id)
            && now - last < 60
        {
            return;
        }
        // 只有属于当前地点的快照才能用来判断是否过期。
        let expired = match (
            &self.state.weather.snapshot,
            self.state.weather.snapshot_location,
        ) {
            (Some(snapshot), Some(owner)) if owner == location_id => {
                me_core::weather::is_expired(now, snapshot.expires_at)
            }
            _ => true,
        };
        if !expired {
            return;
        }
        self.state.weather.auto_refresh_at.insert(location_id, now);
        self.state.weather.refreshing.insert(location_id);
        // 刷的是**当前显示的地点**：切到第二个城市后，自动刷新动的也应该是它。
        commands.push(Command::RefreshWeather {
            id: location_id,
            force: false,
        });
    }

    /// 重查当前视图的日程。
    ///
    /// 读的是「主行 + 例外」，展开交给 `me_core::expand_event`：
    /// 界面与提醒循环共用同一份规则实现，不会各算一套。
    fn refresh_calendar(&mut self) {
        self.state.calendar.dirty = false;

        match self.repo.list_calendars_readonly() {
            Ok(calendars) => self.state.calendar.calendars = calendars,
            Err(err) => {
                self.report_error(ErrorScope::Global, err);
                return;
            }
        }

        let visible = self.state.calendar.visible_calendars();
        if visible.is_empty() {
            self.state.calendar.occurrences.clear();
            self.state.calendar.loaded_range = None;
            return;
        }

        let (start, end) = self.state.calendar.range_days();
        let from = views::calendar::day_start_utc(start);
        let to = views::calendar::day_start_utc(end + chrono::Duration::days(1));

        let series = match self
            .repo
            .list_calendar_series_readonly(&visible, from, to, PAGE_SIZE)
        {
            Ok(series) => series,
            Err(err) => {
                self.report_error(ErrorScope::Global, err);
                return;
            }
        };

        let ids = series.iter().map(|item| item.id).collect::<Vec<_>>();
        let exceptions = self
            .repo
            .list_calendar_exceptions_readonly(&ids)
            .unwrap_or_default();

        let mut occurrences = Vec::new();
        let mut truncated = false;
        for item in &series {
            let own = exceptions
                .iter()
                .filter(|(id, _)| *id == item.id)
                .map(|(_, exception)| exception.clone())
                .collect::<Vec<_>>();
            let expanded = me_core::expand_event(item, &own, from, to, RecurrenceLimits::default());
            truncated |= expanded.truncated;
            for occ in expanded.occurrences {
                occurrences.push(OccurrenceView {
                    event: item.id,
                    calendar: item.calendar_id,
                    summary: occ.summary.clone().unwrap_or_else(|| item.summary.clone()),
                    start_utc: occ.start_utc,
                    end_utc: occ.end_utc,
                    all_day: occ.all_day,
                    recurring: item.rrule.is_some(),
                    source: item.source,
                });
            }
        }
        occurrences.sort_by_key(|occ| (occ.all_day, occ.start_utc));
        self.state.calendar.occurrences = occurrences;
        self.state.calendar.loaded_range = Some((from, to));

        if truncated {
            self.state
                .toast(ToastKind::Warn, "重复日程过多，已截断显示");
        }
    }

    /// 打开已有事件的编辑器：需要完整行（描述、地点、来源）。
    fn open_calendar_event(&mut self, id: CalendarEventId) {
        match self.repo.get_calendar_event_readonly(id) {
            Ok(Some(event)) => {
                self.state.calendar.editor =
                    Some(views::calendar::CalendarEditor::from_event(&event));
            }
            Ok(None) => self.state.toast(ToastKind::Warn, "该日程已被删除"),
            Err(err) => self.report_error(ErrorScope::Global, err),
        }
    }

    // -----------------------------------------------------------------------
    // 音乐
    // -----------------------------------------------------------------------

    /// 惰性启动音频线程并回收播放状态。
    ///
    /// 音频线程只在首次进入音乐页时创建，切换页面不销毁——否则「切页不停止播放」
    /// 就成了空话。
    fn poll_music(&mut self, ctx: &egui::Context) {
        if self.state.page == Page::Music {
            self.ensure_player();
        }

        // 先取出事件，避免同时借用 player 与 state。
        let mut events = Vec::new();
        if let Some(player) = &self.player {
            while let Some(event) = player.try_recv() {
                events.push(event);
            }
        }

        for event in events {
            apply_audio_event(&mut self.state, event);
        }

        // 频谱只在音乐页取：样本是"最新一块覆盖旧块"，不离页也取没有意义。
        let frame = if self.state.page == Page::Music {
            self.player.as_ref().and_then(|player| player.spectrum())
        } else {
            None
        };
        let dt = ctx.input(|input| input.stable_dt);
        self.state.music.analyzer.update(frame.as_ref(), dt);

        // 音乐页在播放、或柱子还在衰减时用 66ms（≈15fps）跑频谱；
        // 30fps 太快，柱子每帧都在变，看着晃眼。其余情况退回 250ms 慢轮询，
        // 不在别的页面上空转重绘。
        let animating = (self.state.music.playing && self.state.page == Page::Music)
            || self.state.music.analyzer.is_active();
        if animating {
            ctx.request_repaint_after(Duration::from_millis(66));
        } else if self.state.music.playing || self.state.music.scanning {
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        // AI 音乐工具的等待：每帧比一次目标状态，达成或超时就回执。
        self.poll_ai_music(ctx);
    }

    /// 惰性启动音频线程。
    ///
    /// 音乐页与 AI 音乐执行器**共用**这一处启动逻辑：AI 侧要能在从没进过音乐页的
    /// 情况下直接放歌；两份启动代码只会越改越不一致。
    fn ensure_player(&mut self) {
        if self.player.is_some() {
            return;
        }
        let player = AudioPlayer::start();
        // 有已保存目录就自动扫描；扫描不等于播放。
        if let Some(dir) = self.state.music.dir.clone() {
            self.state.music.scanning = true;
            player.send(AudioCommand::LoadDirectory(dir));
        }
        if self.state.music.order == MusicOrder::Shuffle {
            player.send(AudioCommand::SetOrder(MusicOrder::Shuffle));
        }
        self.player = Some(player);
    }

    fn reply_ai_music(&self, call_id: &str, outcome: me_core::MusicOutcome) {
        self.backend.send(Command::AiMusicResult {
            call_id: call_id.to_string(),
            outcome,
        });
    }

    /// 当前播放状态的一句话（给模型、也给活动条）。
    fn music_state_summary(&self) -> String {
        let music = &self.state.music;
        let order = match music.order {
            MusicOrder::Shuffle => "随机",
            MusicOrder::Sequential => "顺序",
        };
        if let Some(index) = music.current {
            let title = music
                .tracks
                .get(index)
                .map(|track| track.title.clone())
                .unwrap_or_else(|| "未知曲目".to_string());
            if music.playing {
                format!(
                    "正在播放「{title}」（第 {} / {} 首，顺序：{order}）",
                    index + 1,
                    music.tracks.len()
                )
            } else {
                format!(
                    "已暂停：当前是「{title}」（第 {} / {} 首）",
                    index + 1,
                    music.tracks.len()
                )
            }
        } else if music.scanning {
            "还没有开始播放，媒体库正在扫描".to_string()
        } else if music.tracks.is_empty() {
            "曲库为空：还没有扫描到可播放的音频文件".to_string()
        } else {
            format!("当前没有在播放（曲库共 {} 首）", music.tracks.len())
        }
    }

    /// 执行一次 AI 音乐操作。
    ///
    /// 只有「需要等状态变化」的动作才挂 `ai_pending`；搜索/查询/失败路径当场回执，
    /// 免得模型白等 2 秒。
    fn begin_ai_music(&mut self, call_id: String, op: me_core::MusicOp) {
        use crate::state::{MusicExpect, PendingAiMusic};
        use me_core::music::{MusicAction, MusicOp};

        // 只读查询先答，不看播放器。
        if matches!(op, MusicOp::State) {
            let text = self.music_state_summary();
            self.reply_ai_music(&call_id, me_core::MusicOutcome::ok(text));
            return;
        }

        if let Some(error) = self.state.music.device_error.clone() {
            self.reply_ai_music(
                &call_id,
                me_core::MusicOutcome::fail(format!("音频设备不可用：{error}")),
            );
            return;
        }
        if self.state.music.dir.is_none() {
            self.reply_ai_music(
                &call_id,
                me_core::MusicOutcome::fail("还没有选择音乐目录：请先到音乐页选一个文件夹"),
            );
            return;
        }

        self.ensure_player();

        if self.state.music.tracks.is_empty() {
            let text = if self.state.music.scanning {
                "正在扫描媒体库，稍等几秒再试（大目录可能要十几秒）"
            } else {
                "音乐目录里没有找到可播放的音频文件"
            };
            self.reply_ai_music(&call_id, me_core::MusicOutcome::fail(text));
            return;
        }

        let tracks = self.state.music.tracks.clone();
        let mut expect: Option<MusicExpect> = None;
        let mut label = String::new();
        let mut outcome: Option<me_core::MusicOutcome> = None;

        match op {
            MusicOp::Search { query } => {
                let hits = crate::views::music::find_tracks(&tracks, &query);
                if hits.is_empty() {
                    outcome = Some(me_core::MusicOutcome::ok(format!(
                        "曲库里没有匹配「{query}」的曲目（共 {} 首）",
                        tracks.len()
                    )));
                } else {
                    let list = hits
                        .iter()
                        .take(8)
                        .map(|&index| format!("{}. {}", index + 1, tracks[index].title))
                        .collect::<Vec<_>>()
                        .join("、");
                    outcome = Some(me_core::MusicOutcome::ok(format!(
                        "匹配到 {} 首：{list}",
                        hits.len()
                    )));
                }
            }
            MusicOp::Play { query, index } => {
                let target = match (index, query.as_deref()) {
                    (Some(raw), _) if raw >= 1 && raw <= tracks.len() => Some(raw - 1),
                    (Some(raw), _) => {
                        outcome = Some(me_core::MusicOutcome::fail(format!(
                            "序号 {raw} 超出范围（曲库共 {} 首，序号从 1 开始）",
                            tracks.len()
                        )));
                        None
                    }
                    (None, Some(word)) => {
                        let hits = crate::views::music::find_tracks(&tracks, word);
                        match hits.len() {
                            0 => {
                                outcome = Some(me_core::MusicOutcome::ok(format!(
                                    "曲库里没有匹配「{word}」的曲目"
                                )));
                                None
                            }
                            1 => Some(hits[0]),
                            _ => {
                                let list = hits
                                    .iter()
                                    .take(8)
                                    .map(|&index| format!("{}. {}", index + 1, tracks[index].title))
                                    .collect::<Vec<_>>()
                                    .join("、");
                                outcome = Some(me_core::MusicOutcome::ok(format!(
                                    "「{word}」匹配到 {} 首，没有自动播放。候选：{list}。\
请让用户选一首（或用 index 指定序号）",
                                    hits.len()
                                )));
                                None
                            }
                        }
                    }
                    (None, None) => Some(0),
                };

                if let Some(target) = target
                    && let (Some(player), Some(track)) = (&self.player, tracks.get(target))
                {
                    player.send(AudioCommand::Play {
                        index: target,
                        path: track.path.clone(),
                    });
                    label = format!("播放「{}」", track.title);
                    expect = Some(MusicExpect::Playing(target));
                }
            }
            MusicOp::Control { action } => match action {
                MusicAction::Play => {
                    if self.state.music.playing {
                        outcome = Some(me_core::MusicOutcome::ok("已经在播放"));
                    } else if let Some(current) = self.state.music.current {
                        if let Some(player) = &self.player {
                            player.send(AudioCommand::Toggle);
                        }
                        label = "继续播放".to_string();
                        expect = Some(MusicExpect::Playing(current));
                    } else if let (Some(player), Some(track)) = (&self.player, tracks.first()) {
                        player.send(AudioCommand::Play {
                            index: 0,
                            path: track.path.clone(),
                        });
                        label = format!("播放「{}」", track.title);
                        expect = Some(MusicExpect::Playing(0));
                    }
                }
                MusicAction::Pause => {
                    if !self.state.music.playing {
                        outcome = Some(me_core::MusicOutcome::ok("当前没有在播放"));
                    } else {
                        if let Some(player) = &self.player {
                            player.send(AudioCommand::Toggle);
                        }
                        label = "暂停".to_string();
                        expect = Some(MusicExpect::Paused);
                    }
                }
                MusicAction::Next | MusicAction::Prev => {
                    let before = self.state.music.current;
                    if let Some(player) = &self.player {
                        player.send(if action == MusicAction::Next {
                            AudioCommand::Next
                        } else {
                            AudioCommand::Prev
                        });
                    }
                    label = action.label().to_string();
                    expect = Some(MusicExpect::Switched(before));
                }
                MusicAction::Stop => {
                    if let Some(player) = &self.player {
                        player.send(AudioCommand::Stop);
                    }
                    label = "停止播放".to_string();
                    expect = Some(MusicExpect::Stopped);
                }
            },
            MusicOp::State => {}
        }

        if let Some(outcome) = outcome {
            self.reply_ai_music(&call_id, outcome);
            return;
        }
        let Some(expect) = expect else {
            self.reply_ai_music(
                &call_id,
                me_core::MusicOutcome::fail("这条音乐指令没有可执行的动作"),
            );
            return;
        };
        // 新的等待覆盖旧的：上一轮多半已经超时，留着只会把回执张冠李戴。
        self.state.music.ai_pending = Some(PendingAiMusic {
            call_id,
            expect,
            deadline: Instant::now() + Duration::from_millis(2500),
            action_label: label,
        });
    }

    /// 每帧检查 AI 音乐操作是否达成；达成或超时都回执一次。
    fn poll_ai_music(&mut self, ctx: &egui::Context) {
        use crate::state::MusicExpect;
        let Some(pending) = self.state.music.ai_pending.clone() else {
            return;
        };
        let music = &self.state.music;
        let satisfied = match pending.expect {
            MusicExpect::Playing(index) => music.playing && music.current == Some(index),
            MusicExpect::Paused => !music.playing,
            MusicExpect::Stopped => !music.playing,
            MusicExpect::Switched(before) => music.current != before,
        };
        let outcome = if satisfied {
            Some(me_core::MusicOutcome::ok(format!(
                "{}：{}",
                pending.action_label,
                self.music_state_summary()
            )))
        } else if Instant::now() >= pending.deadline {
            let error = self
                .state
                .music
                .error
                .clone()
                .unwrap_or_else(|| "播放器没有响应".to_string());
            Some(me_core::MusicOutcome::fail(format!(
                "{}没有在 2 秒内完成：{error}",
                pending.action_label
            )))
        } else {
            // 等待期间保持重绘，否则状态变了也轮询不到。
            ctx.request_repaint_after(Duration::from_millis(100));
            None
        };
        if let Some(outcome) = outcome {
            self.state.music.ai_pending = None;
            self.reply_ai_music(&pending.call_id, outcome);
        }
    }

    /// 把本帧界面产生的播放命令转交给音频线程。
    fn flush_music_commands(&mut self) {
        if self.state.music.pending.is_empty() {
            return;
        }
        match &self.player {
            Some(player) => {
                for command in self.state.music.pending.drain(..) {
                    player.send(command);
                }
            }
            // 还没创建音频线程：丢弃而不是堆积，下次进音乐页会重新扫描。
            None => self.state.music.pending.clear(),
        }
    }

    // -----------------------------------------------------------------------
    // 事件
    // -----------------------------------------------------------------------

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::SettingsLoaded {
                theme,
                body_backfill,
                remote_images,
                realtime_sync,
                poll_interval_secs,
                draft_server_sync,
                draft_bulk_upload_pending,
                plaintext_notice_ack,
                html_renderer,
                oauth_google_client_id,
                oauth_microsoft_client_id,
                oauth_microsoft_tenant,
                attachment_dir,
                music_dir,
                music_order,
            } => {
                self.state.theme = theme;
                self.state.music.dir = music_dir;
                self.state.music.order = music_order;
                self.state.body_backfill = body_backfill;
                self.state.remote_images = remote_images;
                self.state.realtime_sync = realtime_sync;
                self.state.poll_interval_secs = poll_interval_secs;
                self.state.draft_server_sync = draft_server_sync;
                self.state.draft_bulk_upload_pending = draft_bulk_upload_pending;
                // 明文缓存告知只提示一次；本次会话内已确认就不再因事件重发而出现。
                if !plaintext_notice_ack && !self.state.plaintext_notice_ack {
                    self.state.show_plaintext_notice = true;
                }
                self.state.html_renderer = html_renderer;
                self.state.oauth_google_client_id = oauth_google_client_id;
                self.state.oauth_microsoft_client_id = oauth_microsoft_client_id;
                self.state.oauth_microsoft_tenant = oauth_microsoft_tenant;
                self.state.attachment_dir = Some(attachment_dir);
                self.applied_theme = None;
            }
            Event::AttachmentDirChanged { dir } => {
                // 已打开邮件里记录的绝对路径同步换到新目录，避免“打开”指向旧位置。
                if let Some(old) = self.state.attachment_dir.clone()
                    && let Some(body) = &mut self.state.open_body
                {
                    for attachment in &mut body.attachments {
                        if let Some(path) = attachment.local_path.clone()
                            && let Ok(rest) = path.strip_prefix(&old)
                        {
                            attachment.local_path = Some(dir.join(rest));
                        }
                    }
                }
                self.state.attachment_dir = Some(dir.clone());
                self.state.toast(
                    ToastKind::Info,
                    format!("附件下载目录已切换为 {}", dir.display()),
                );
            }
            Event::AccountsLoaded { accounts } => {
                if self.state.selected_account.is_none() {
                    self.state.selected_account = accounts.first().map(|a| a.id);
                }
                self.state.accounts = accounts;
                self.state.folders_dirty = true;
                self.state.drafts_dirty = true;
            }
            Event::AccountAdded { account } => {
                let id = account.id;
                self.state.accounts.retain(|a| a.id != id);
                self.state.accounts.push(account);
                self.state.selected_account = Some(id);
                self.state.clear_folder_selection();
                self.state.folders_dirty = true;
                self.state.drafts_dirty = true;
                if let Dialog::AccountEditor(editor) = &mut self.state.dialog {
                    editor.busy = false;
                }
                self.state.dialog.close();
                self.state.toast(ToastKind::Success, "账户已添加");
            }
            Event::AccountUpdated { account } => {
                if let Some(slot) = self.state.accounts.iter_mut().find(|a| a.id == account.id) {
                    *slot = account;
                } else {
                    self.state.accounts.push(account);
                }
                self.state.folders_dirty = true;
                if let Dialog::AccountEditor(editor) = &mut self.state.dialog {
                    editor.busy = false;
                }
                self.state.dialog.close();
                self.state.toast(ToastKind::Success, "账户已更新");
            }
            Event::AccountRemoved { id } => {
                self.state.accounts.retain(|a| a.id != id);
                self.state.folders.remove(&id);
                if self.state.selected_account == Some(id) {
                    self.state.selected_account = self.state.accounts.first().map(|a| a.id);
                    self.state.clear_folder_selection();
                    self.state.messages.clear();
                    self.state.selected_message = None;
                    // 列表换了 → 多选集合里的 id 不再属于当前列表，必须清空。
                    self.state.clear_multi_selection();
                    self.state.open_body = None;
                }
                self.state.folders_dirty = true;
                self.state.drafts_dirty = true;
                self.state.dialog.close();
                self.state.toast(ToastKind::Success, "账户已删除");
            }
            Event::OAuthFlowStarted {
                flow_id,
                provider,
                verification_uri,
                user_code,
                expires_at,
            } => {
                if let Dialog::OAuthLogin(login) = &mut self.state.dialog
                    && login.provider == provider
                    && login.flow_id.is_none_or(|id| id == flow_id)
                {
                    login.flow_id = Some(flow_id);
                    login.verification_uri = verification_uri;
                    login.user_code = user_code;
                    login.expires_at = expires_at;
                    login.message = if login.user_code.is_some() {
                        "请在浏览器中完成设备码授权。".to_string()
                    } else {
                        "已在浏览器打开授权页面，完成后会自动返回。".to_string()
                    };
                    login.busy = true;
                }
            }
            Event::OAuthCompleted {
                flow_id,
                account,
                created,
            } => {
                if let Dialog::OAuthLogin(login) = &self.state.dialog
                    && login.flow_id.is_none_or(|id| id == flow_id)
                {
                    self.state.dialog.close();
                }
                let id = account.id;
                if let Some(existing) = self
                    .state
                    .accounts
                    .iter_mut()
                    .find(|existing| existing.id == id)
                {
                    *existing = account;
                } else {
                    self.state.accounts.push(account);
                    self.state.selected_account = Some(id);
                    self.state.clear_folder_selection();
                }
                self.state.folders_dirty = true;
                self.state.drafts_dirty = true;
                self.state.toast(
                    ToastKind::Success,
                    if created {
                        "OAuth 账户已添加，正在同步"
                    } else {
                        "OAuth 授权已更新"
                    },
                );
            }
            Event::OAuthFailed {
                flow_id,
                message,
                reauth_required,
            } => {
                if let Dialog::OAuthLogin(login) = &mut self.state.dialog
                    && login.flow_id.is_none_or(|id| id == flow_id)
                {
                    login.flow_id = Some(flow_id);
                    login.busy = false;
                    login.message = message.clone();
                } else if message != "授权已取消" {
                    self.state.toast(
                        ToastKind::Warn,
                        format!(
                            "OAuth 授权失败：{message}{}",
                            if reauth_required {
                                "（需要重新登录）"
                            } else {
                                ""
                            }
                        ),
                    );
                }
            }
            Event::OAuthReauthRequired { account } => {
                if let Some(existing) = self
                    .state
                    .accounts
                    .iter_mut()
                    .find(|existing| existing.id == account)
                {
                    existing.needs_reauth = true;
                }
                self.state
                    .toast(ToastKind::Warn, "授权已失效，请在账户页点击“重新授权”。");
            }
            Event::CredentialNotPersisted { account, created } => {
                if created {
                    // 新增账户时让用户明确选择：仅本次会话，或取消添加。
                    self.state.dialog = Dialog::CredentialFallback { account };
                } else {
                    self.state.toast(
                        ToastKind::Warn,
                        "系统密钥库不可用：新凭证仅保存在本次会话，退出后需重新输入。",
                    );
                }
            }
            Event::DiskSpaceLow { available_bytes } => {
                self.state.disk_low = true;
                let text = match available_bytes {
                    Some(bytes) => format!("剩余磁盘仅 {}，已暂停后台正文补全", human_bytes(bytes)),
                    None => "剩余磁盘空间不足，已暂停后台正文补全".to_string(),
                };
                self.state.toast(ToastKind::Warn, text);
            }
            Event::AccountStatus { id, status } => {
                let finished = matches!(
                    status,
                    me_core::ConnectionState::Online
                        | me_core::ConnectionState::Disconnected
                        | me_core::ConnectionState::Error { .. }
                        | me_core::ConnectionState::Backoff { .. }
                );
                if finished {
                    self.state.sync_progress = None;
                }
                self.state.connection.insert(id, status);
            }
            Event::RealtimeStatus {
                account,
                mode,
                connected,
                last_event_at,
            } => {
                self.state.realtime.insert(
                    account,
                    crate::state::RealtimeView {
                        mode,
                        connected,
                        last_event_at,
                    },
                );
            }
            Event::ConnectionTested {
                account,
                imap_ok,
                smtp_ok,
                detail,
            } => {
                if let Dialog::AccountEditor(editor) = &mut self.state.dialog {
                    editor.busy = false;
                    editor.test_result = Some(format!(
                        "IMAP：{}；SMTP：{}{}",
                        if imap_ok { "成功" } else { "失败" },
                        if smtp_ok { "成功" } else { "失败" },
                        detail
                            .as_deref()
                            .map(|d| format!("（{d}）"))
                            .unwrap_or_default()
                    ));
                }
                self.state.toast(
                    if imap_ok && smtp_ok {
                        ToastKind::Success
                    } else {
                        ToastKind::Warn
                    },
                    format!("账户 {} 连接测试完成", account.0),
                );
            }
            Event::FoldersUpdated { account, folders } => {
                self.state.folders.insert(account, folders);
                self.state.folders_dirty = true;
                if self.state.selected_folder.is_none() {
                    self.state.messages_dirty = true;
                }
            }
            Event::MessagesAppended { folder, messages } => {
                if self.state.search.active {
                    return;
                }
                if self.state.selected_folder == Some(folder) {
                    // 增量插入并保持 (date DESC, id DESC) 排序。
                    for message in messages {
                        if !self.state.messages.iter().any(|m| m.id == message.id) {
                            self.state.messages.push(message);
                        }
                    }
                    me_core::sort_newest_first(&mut self.state.messages);
                }
                self.state.folders_dirty = true;
            }
            Event::MessagesUpdated { messages } => {
                for updated in messages {
                    if let Some(existing) = self
                        .state
                        .messages
                        .iter_mut()
                        .find(|message| message.id == updated.id)
                    {
                        *existing = updated;
                    }
                }
                self.state.folders_dirty = true;
            }
            Event::MessagesRemoved { folder, ids } => {
                if self.state.selected_folder == Some(folder) {
                    let removed: std::collections::HashSet<_> = ids.iter().copied().collect();
                    self.state.messages.retain(|m| !removed.contains(&m.id));
                    if self
                        .state
                        .selected_message
                        .map(|id| removed.contains(&id))
                        .unwrap_or(false)
                    {
                        self.state.selected_message = None;
                        self.state.clear_multi_selection();
                        self.state.open_body = None;
                    }
                }
                self.state.folders_dirty = true;
            }
            Event::FlagsChanged { items } => {
                for (id, flags) in items {
                    if let Some(message) = self.state.messages.iter_mut().find(|m| m.id == id) {
                        message.flags = flags;
                    }
                }
                self.state.folders_dirty = true;
            }
            Event::ThreadsBackfilled { updated } => {
                // 线程头写回来了：列表要重画才能把新成串的邮件按线程折叠。
                if updated > 0 {
                    self.state.messages_dirty = true;
                }
            }
            Event::MessageBodyLoaded { id, body } => {
                if self.state.selected_message == Some(id) {
                    let missing = !body.raw_cached;
                    self.state.open_body = Some(body);
                    self.state.reader_blocks = None;
                    self.state.html_rendered = None;
                    self.state.html_job = None;
                    self.state.reader_textures.clear();
                    self.state.reader_texture_message = None;
                    self.state.body_loading = false;
                    if missing {
                        // 仅在正文文件缺失时提示，正常路径保持安静。
                        self.state.body_loading = false;
                    }
                    if let Some(message) = self.state.messages.iter().find(|m| m.id == id)
                        && !message.flags.is_seen()
                    {
                        let mut updated = message.clone();
                        updated.flags = updated.flags.set_seen(true);
                        if let Some(slot) = self.state.messages.iter_mut().find(|m| m.id == id) {
                            slot.flags = updated.flags;
                        }
                    }
                }
            }
            Event::AttachmentDownloaded {
                message,
                part_index,
                local_path,
            } => {
                if let Some(body) = &mut self.state.open_body
                    && body.message == message
                    && let Some(attachment) = body
                        .attachments
                        .iter_mut()
                        .find(|a| a.part_index == part_index)
                {
                    attachment.local_path = Some(local_path.clone());
                }
                self.state.toast(
                    ToastKind::Success,
                    format!("附件已下载：{}", local_path.display()),
                );
            }
            Event::SavedSearchesChanged { searches } => {
                // **全量快照**，不是增量——条数很少，直接替换。
                self.state.search.saved_searches = searches;
            }
            Event::SearchResults {
                query_id,
                messages,
                total,
                body_indexed,
                body_total,
            } => {
                if query_id < self.state.search.query_id {
                    return;
                }
                self.state.search.query_id = query_id;
                self.state.search.active = true;
                self.state.search.body_indexed = body_indexed;
                self.state.search.body_total = body_total;
                self.state.messages = messages;
                self.state.messages_cursor = None;
                self.state.messages_has_more = false;
                self.state.selected_message = None;
                // 搜索结果换了 → 多选集合里的 id 不再属于当前列表，必须清空。
                self.state.clear_multi_selection();
                self.state.open_body = None;
                self.state.messages_dirty = false;
                if total == 0 {
                    self.state.toast(ToastKind::Info, "没有匹配的邮件");
                }
            }
            Event::SyncProgress {
                folder,
                done,
                total,
            } => {
                self.state.sync_progress = Some(crate::state::SyncProgressView {
                    folder,
                    done,
                    total,
                });
            }
            Event::IndexProgress {
                account,
                indexed,
                total,
            } => {
                self.state.index_progress = Some(crate::state::IndexProgressView {
                    account,
                    indexed,
                    total,
                });
            }
            Event::IndexRebuilt { indexed } => {
                self.state
                    .toast(ToastKind::Success, format!("索引已重建，共 {indexed} 封"));
                self.state.messages_dirty = true;
            }
            Event::DraftSaved {
                id,
                synced_to_server,
            } => {
                let mut exit_after_save = false;
                let mut toast_text: Option<&str> = None;
                if let Dialog::Compose(compose) = &mut self.state.dialog {
                    if synced_to_server {
                        toast_text = Some("已保存到服务器草稿箱");
                    } else {
                        if compose.draft_id.is_none() {
                            compose.draft_id = Some(id);
                        }
                        compose.mark_saved();
                        toast_text = Some("草稿已保存");
                    }
                    if compose.exit_after_save && !synced_to_server {
                        exit_after_save = true;
                    }
                }
                if let Some(text) = toast_text {
                    self.state.toast(ToastKind::Success, text);
                }
                if exit_after_save {
                    self.state.dialog.close();
                    self.state.exit_requested = true;
                }
                self.state.drafts_dirty = true;
            }
            Event::DraftLoaded { draft } => {
                let compose = crate::state::ComposeState::from_draft(draft.account, &draft);
                self.state.dialog = Dialog::Compose(compose);
            }
            Event::SendCompleted {
                draft,
                archived,
                archive_error,
            } => {
                let matches = matches!(&self.state.dialog, Dialog::Compose(c) if c.draft_id == Some(draft) || c.draft_id.is_none());
                if matches {
                    self.state.dialog.close();
                }
                self.state.drafts_dirty = true;
                self.state.messages_dirty = true;
                if archived {
                    self.state.toast(ToastKind::Success, "邮件已发送");
                } else {
                    self.state.toast(
                        ToastKind::Warn,
                        format!(
                            "已发送，但未保存到已发送文件夹{}",
                            archive_error.map(|e| format!("：{e}")).unwrap_or_default()
                        ),
                    );
                }
            }
            Event::SendFailed { draft, reason } => {
                if let Dialog::Compose(compose) = &mut self.state.dialog {
                    if compose.draft_id.is_none() || compose.draft_id == Some(draft) {
                        compose.draft_id = Some(draft);
                    }
                    compose.status = crate::state::ComposeStatus::Editing;
                    compose.may_have_been_sent = true;
                    compose.last_failure = Some(reason.user_message());
                    compose.error = Some(reason.user_message());
                }
                self.state.toast(ToastKind::Error, reason.user_message());
                self.state.drafts_dirty = true;
            }
            Event::CacheCleared {
                account,
                freed_bytes,
            } => {
                self.state.toast(
                    ToastKind::Success,
                    format!("缓存已清理，释放 {}", human_bytes(freed_bytes)),
                );
                if let Some(account) = account {
                    if self.state.selected_account == Some(account) {
                        self.state.messages_dirty = true;
                    }
                } else {
                    self.state.messages_dirty = true;
                }
            }
            Event::DraftChanged { .. } | Event::DraftDeleted { .. } => {
                self.state.drafts_dirty = true;
            }
            Event::DraftSyncStateChanged {
                draft,
                state,
                error,
            } => {
                self.state.drafts_dirty = true;
                if let Some(error) = error {
                    self.state.toast(
                        ToastKind::Warn,
                        format!("草稿 {} 同步失败（{}）：{error}", draft.0, state.label()),
                    );
                }
            }
            Event::DraftConflictDetected {
                draft,
                conflict_copy,
            } => {
                self.state.drafts_dirty = true;
                self.state.toast(
                    ToastKind::Warn,
                    format!(
                        "草稿 {} 存在冲突，已保留两份（副本 {}）",
                        draft.0, conflict_copy.0
                    ),
                );
            }
            Event::FolderRemoved { folder } => {
                if self.state.selected_folder == Some(folder) {
                    self.state.clear_folder_selection();
                    self.state.messages.clear();
                }
                self.state.folders_dirty = true;
            }
            Event::CalendarsChanged { calendars } => {
                self.state.calendar.calendars = calendars;
                self.state.calendar.dirty = true;
            }
            Event::CalendarEventsChanged { .. } => {
                self.state.calendar.dirty = true;
            }
            Event::WeatherConfigChanged {
                key_present,
                api_host,
            } => {
                self.state.weather.key_present = key_present;
                self.state.weather.api_host = api_host.clone();
                self.state.weather.dirty = true;
                // 只有用户主动保存过才提示：启动时读回配置也会发这个事件。
                if self.state.weather.save_requested {
                    self.state.weather.save_requested = false;
                    self.state
                        .toast(ToastKind::Info, format!("天气配置已保存（{api_host}）"));
                }
            }
            Event::WeatherConnectionTested { ok, message } => {
                self.state.weather.testing = false;
                self.state.weather.test_result = Some((ok, message.clone()));
                let kind = if ok { ToastKind::Info } else { ToastKind::Warn };
                self.state.toast(kind, message);
            }
            Event::WeatherKeyStored { persisted } => {
                self.state.weather.key_present = true;
                self.state.weather.dirty = true;
                let message = if persisted {
                    "和风天气 API Key 已保存到本机配置"
                } else {
                    "API Key 仅在本次会话有效"
                };
                self.state.toast(ToastKind::Info, message);
            }
            Event::WeatherLocationCandidates {
                request_id,
                candidates,
            } => {
                // 只接受当前请求号的结果，旧的直接丢弃。
                if request_id == self.state.weather.search_request_id {
                    self.state.weather.candidates = candidates;
                }
            }
            Event::WeatherLocationsChanged => {
                self.state.weather.dirty = true;
            }
            Event::WeatherUpdated { location_id, .. } => {
                // 只结束**这个地点**的刷新状态：别的城市还在刷不算完成。
                self.state.weather.refreshing.remove(&location_id);
                // 错误提示也是按地点记的：清掉属于该地点的那条。
                if self.state.weather.error_location == Some(location_id) {
                    self.state.weather.error = None;
                    self.state.weather.error_location = None;
                }
                self.state.weather.dirty = true;
            }
            Event::WeatherFailed {
                location_id,
                message,
                has_cache,
            } => {
                if let Some(id) = location_id {
                    self.state.weather.refreshing.remove(&id);
                }
                self.state.weather.dirty = true;
                // 保存失败：不再等待「已保存」提示。
                self.state.weather.save_requested = false;
                self.state.weather.error = Some(if has_cache {
                    format!("{message}（显示的是缓存数据）")
                } else {
                    message.clone()
                });
                self.state.weather.error_location = location_id;
                self.state.toast(ToastKind::Warn, message);
            }
            // ---- AI ----
            Event::AiConversationsChanged => {
                self.state.ai_conversations_dirty = true;
            }
            Event::AiStreamStarted {
                conversation,
                trimmed_turns,
                ..
            } => {
                if self.state.ai.selected == Some(conversation) {
                    self.state.ai.on_stream_started(conversation);
                    if trimmed_turns > 0 {
                        self.state.toast(
                            ToastKind::Info,
                            format!("已省略较早的 {trimmed_turns} 轮对话"),
                        );
                    }
                }
                self.state.ai_messages_dirty = true;
            }
            Event::AiStreamDelta {
                conversation,
                seq,
                delta,
                ..
            } => {
                if self.state.ai.selected == Some(conversation) {
                    self.state.ai.on_delta(conversation, seq, &delta);
                }
            }
            Event::AiMessageCompleted {
                conversation,
                status,
                ..
            } => {
                if self.state.ai.selected == Some(conversation) {
                    self.state.ai.on_finished();
                    if status == me_core::ai::AiMessageStatus::Cancelled {
                        self.state.toast(ToastKind::Info, "已停止生成");
                    }
                }
                self.state.ai_messages_dirty = true;
                self.state.ai_conversations_dirty = true;
            }
            Event::AiStreamFailed {
                conversation,
                error_text,
                retryable,
                ..
            } => {
                if self.state.ai.selected == Some(conversation) {
                    self.state.ai.on_finished();
                }
                self.state.ai_messages_dirty = true;
                let suffix = if retryable { "（可重试）" } else { "" };
                self.state
                    .toast(ToastKind::Warn, format!("{error_text}{suffix}"));
            }
            Event::AiPlanUpdated {
                conversation, plan, ..
            } => {
                if self.state.ai.selected == Some(conversation) {
                    self.state.ai.plan = Some(plan);
                }
            }
            Event::AiToolStarted { .. } => {
                // 工具开始不单独展示：完成事件带摘要，活动条一次成型。
            }
            Event::AiToolWeatherResult { .. } => {
                // 天气卡片由天气页负责；AI 页只保留文字摘要。
            }
            Event::AiToolCompleted {
                call_id,
                tool,
                status,
                summary,
                duration_ms,
                motive,
                motive_source,
                ..
            } => {
                self.state.ai.activity.push(me_core::ai::AiToolTrace {
                    call_id,
                    tool,
                    status,
                    summary,
                    duration_ms,
                    motive,
                    motive_source,
                });
            }
            Event::AiToolEmailResults {
                conversation,
                cards,
                ..
            } => {
                if self.state.ai.selected == Some(conversation) {
                    let mut built = Vec::new();
                    for card in cards {
                        if let Ok(Some(message)) = self.repo.get_message_readonly(card.message) {
                            built.push(views::ai::AiCard {
                                message: card.message,
                                reason: card.reason,
                                subject: message.subject.unwrap_or_else(|| "(无主题)".to_string()),
                                from: message
                                    .from
                                    .as_ref()
                                    .map(|address| address.display())
                                    .unwrap_or_default(),
                                date: message.date,
                                snippet: message.snippet.unwrap_or_default(),
                            });
                        }
                    }
                    self.state.ai.cards = built;
                }
            }
            Event::AiMusicCommand {
                call_id,
                op: music_op,
                ..
            } => {
                self.begin_ai_music(call_id, music_op);
            }
            Event::AiToolCalendarCreated { event, .. } => {
                self.state.ai.last_calendar_event = Some(event);
            }
            Event::AiToolCalendarDeletePending {
                conversation,
                event,
                title,
                start_utc,
                ..
            } => {
                if self.state.ai.selected == Some(conversation) {
                    self.state.ai.pending_delete = Some(crate::views::ai::PendingDelete {
                        event,
                        title,
                        start_utc,
                    });
                }
            }
            Event::AiEntitiesChanged => {
                self.state.ai_entities_dirty = true;
            }
            Event::AiConfigChanged {
                models,
                active,
                privacy_ack_anthropic,
                privacy_ack_openai,
                entity_count,
            } => {
                self.state.ai.apply_config(
                    models,
                    active,
                    (privacy_ack_anthropic, privacy_ack_openai),
                    entity_count,
                );
            }
            Event::AiConnectionTested { ok, detail, .. } => {
                let text = detail.unwrap_or_default();
                self.state.ai.test_result = Some((ok, text.clone()));
                let kind = if ok { ToastKind::Info } else { ToastKind::Warn };
                if !text.is_empty() {
                    self.state.toast(kind, text);
                }
            }
            Event::CalendarReminder {
                summary,
                start_utc,
                minutes_before,
                ..
            } => {
                let when = views::calendar::local_from_utc(start_utc);
                let lead = if minutes_before == 0 {
                    "现在开始".to_string()
                } else {
                    format!("{minutes_before} 分钟后开始")
                };
                self.state.toast(
                    ToastKind::Info,
                    format!("{summary}：{} {lead}", when.format("%H:%M")),
                );
            }
            Event::Error { scope, message } => {
                self.state.last_error = Some(UiError {
                    scope,
                    message: message.clone(),
                    detail: None,
                });
                self.state.toast(ToastKind::Error, message);
                if let Dialog::AccountEditor(editor) = &mut self.state.dialog {
                    editor.busy = false;
                    editor.error = Some(
                        self.state
                            .last_error
                            .as_ref()
                            .map(|e| e.message.clone())
                            .unwrap_or_default(),
                    );
                } else if let Dialog::Compose(compose) = &mut self.state.dialog {
                    compose.status = crate::state::ComposeStatus::Editing;
                }
            }
        }
    }

    fn report_error(&mut self, scope: ErrorScope, err: CoreError) {
        let message = err.user_message();
        self.state.last_error = Some(UiError {
            scope,
            message: message.clone(),
            detail: Some(err.to_string()),
        });
        self.state.toast(ToastKind::Error, message);
    }

    // -----------------------------------------------------------------------
    // 命令
    // -----------------------------------------------------------------------

    pub fn submit(&mut self, command: Command) {
        if !self.backend.send(command) {
            self.state
                .toast(ToastKind::Error, "后端已停止，无法执行操作");
        }
    }

    fn dispatch(&mut self, commands: Vec<Command>) {
        for command in commands {
            self.submit(command);
        }
    }

    fn apply_theme(&mut self, ctx: &egui::Context) {
        if self.applied_theme == Some(self.state.theme) {
            return;
        }
        self.applied_theme = Some(self.state.theme);
        match self.state.theme {
            ThemeMode::Light => ctx.set_visuals(egui::Visuals::light()),
            ThemeMode::Dark => ctx.set_visuals(egui::Visuals::dark()),
            ThemeMode::System => {
                // eframe 默认跟随系统；System 下不覆盖。
            }
        }
    }
}

fn cursor_of(message: &me_core::MessageSummary) -> (i64, i64) {
    (message.date.unwrap_or(0), message.id.0)
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

impl<R: ReadOnlyRepo + Clone + 'static> eframe::App for MyEmailApp<R> {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 0. 第二个实例请求激活：把已有窗口带到前台。
        if self.activation.swap(false, Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }

        // 1. 抽取事件，每帧上限 256 条。
        let mut processed = 0;
        while processed < MAX_EVENTS_PER_FRAME {
            match self.backend.try_recv() {
                Some(event) => {
                    self.handle_event(event);
                    processed += 1;
                }
                None => break,
            }
        }
        let has_pending_events = processed == MAX_EVENTS_PER_FRAME;

        // 1b. 回收后台 HTML 渲染结果；旧 generation 直接丢弃。
        while let Some(result) = self.html.try_recv() {
            let current = self.state.html_job.is_some_and(|job| {
                job.message == result.message && job.generation == result.generation
            });
            if !current {
                continue;
            }
            self.state.html_job = None;
            if let Err(err) = &result.result {
                self.state.toast(
                    ToastKind::Warn,
                    format!("富文本渲染失败，已回退到简化视图：{err}"),
                );
            }
            self.state.html_rendered = Some(result);
        }

        // 2. 懒加载脏视图。
        if self.state.folders_dirty {
            self.refresh_folders();
        }
        if self.state.messages_dirty {
            self.refresh_messages();
        }
        if self.state.drafts_dirty {
            self.refresh_drafts();
        }
        // 日历按需刷新：只有停在日历页时才查库，避免非日历用户白付查询成本。
        if self.state.page == Page::Calendar && self.state.calendar.dirty {
            self.refresh_calendar();
        }
        // 天气同理：停在天气页才查库；缓存过期时自动刷新一次。
        if self.state.page == Page::Weather && self.state.weather.dirty {
            self.refresh_weather();
        }
        // AI 同理：停在 AI 页才查库。
        if self.state.page == Page::AiChat {
            if self.state.ai_conversations_dirty {
                self.refresh_ai_conversations();
            }
            if self.state.ai_messages_dirty {
                self.refresh_ai_messages();
            }
            if self.state.ai_entities_dirty {
                self.refresh_ai_entities();
            }
        }
        if let Some(event) = self.state.calendar.pending_open.take() {
            self.open_calendar_event(event);
        }
        // 联系人补全：**只在撰写窗口开着时**查。
        // 联系人表很小（几十到几百行），前缀查询走 `ux_contacts_normalized`，
        // 但没必要在没开撰写窗口时付这个成本。
        self.refresh_contact_suggestions();
        self.state.prune_toasts();
        self.apply_theme(ctx);

        // 2a. 音乐：惰性启动音频线程、回收状态、必要时请求重绘。
        self.poll_music(ctx);

        // 2b. 页面快捷键：Ctrl+1..5。输入控件聚焦时不抢键。
        if !ctx.wants_keyboard_input() {
            // 顶栏顺序 = 编号顺序；插入 AI 后音乐/账户/设置各后移一位。
            let shortcuts = [
                (egui::Key::Num1, Page::Mail),
                (egui::Key::Num2, Page::Calendar),
                (egui::Key::Num3, Page::Weather),
                (egui::Key::Num4, Page::AiChat),
                (egui::Key::Num5, Page::Music),
                (egui::Key::Num6, Page::Accounts),
                (egui::Key::Num7, Page::Settings),
            ];
            for (key, page) in shortcuts {
                if ctx.input(|i| i.modifiers.command && i.key_pressed(key)) {
                    views::switch_page(&mut self.state, page);
                }
            }
        }

        // 3. 渲染。
        let mut commands: Vec<Command> = Vec::new();
        views::top_bar(&mut self.state, &mut commands, ctx);

        if matches!(
            self.state.page,
            Page::Mail | Page::Calendar | Page::Weather | Page::AiChat | Page::Music
        ) {
            views::folder_sidebar(&mut self.state, &mut commands, ctx);
        }
        if self.state.page == Page::Mail
            && self.state.messages_has_more
            && !self.state.search.active
        {
            // 列表滚动到底时加载下一页。
            if ctx.input(|i| i.key_pressed(egui::Key::End)) {
                self.load_more_messages();
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| match self.state.page {
            Page::Mail => views::mailbox::show(&mut self.state, &mut commands, ui),
            Page::Calendar => views::calendar::show(&mut self.state, &mut commands, ui),
            Page::Weather => views::weather::show(&mut self.state, &mut commands, ui),
            Page::AiChat => views::ai::show(&mut self.state, &mut commands, ui),
            Page::Music => views::music::show(&mut self.state, &mut commands, ui),
            Page::Accounts => views::accounts::show(&mut self.state, &mut commands, ui),
            Page::Settings => views::settings::show(
                &mut self.state,
                &mut commands,
                self.cache_dir.as_deref(),
                ui,
            ),
        });

        if self.state.reader_open && self.state.selected_message.is_some() {
            views::reader::show_window(
                &mut self.state,
                &mut commands,
                &self.images,
                &self.html,
                ctx,
            );
        }
        views::accounts::show_editor(&mut self.state, &mut commands, ctx);
        views::compose::show_window(&mut self.state, &mut commands, ctx);
        views::mailbox::show_save_search_dialog(&mut self.state, &mut commands, ctx);
        if self.state.page == Page::Calendar {
            views::calendar::show_editor(&mut self.state, &mut commands, ctx);
        }
        views::calendar::new_calendar_dialog(&mut self.state, &mut commands, ctx);
        views::dialogs::show(&mut self.state, &mut commands, ctx);
        // AI 设置是弹窗：只在 AI 页渲染，切走后自动收掉。
        if self.state.page == Page::AiChat {
            views::ai::settings_modal(&mut self.state, &mut commands, ctx);
        }
        views::toasts(&mut self.state, ctx);

        // 4. 分发命令。
        if self.state.load_more_requested {
            self.state.load_more_requested = false;
            self.load_more_messages();
        }
        // 天气：缓存缺失或过期时自动刷新一次（内部有 60 秒节流）。
        self.maybe_auto_refresh_weather(&mut commands);

        self.dispatch(commands);
        self.flush_music_commands();

        // 5. 有剩余事件时请求下一帧。
        if has_pending_events {
            ctx.request_repaint();
        }
        if self.state.sync_progress.is_some() || self.state.index_progress.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if self.state.exit_requested {
            self.state.exit_requested = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

/// 把音频线程的事件并入 UI 状态。
///
/// 抽成自由函数是为了能直接测：这些分支里最容易错的是「拖动进度条时不能被
/// 250ms 的状态回传覆盖」。
fn apply_audio_event(state: &mut UiState, event: AudioEvent) {
    match event {
        AudioEvent::Playlist {
            tracks,
            skipped,
            truncated,
        } => {
            state.music.tracks = tracks;
            state.music.skipped = skipped;
            state.music.truncated = truncated;
            state.music.scanning = false;
            state.music.current = None;
            state.music.position_ms = 0;
            state.music.duration_ms = None;
            // 换目录等于换队列：上一首的标签/封面必须一起清掉。
            state.music.now_playing = None;
            state.music.cover_texture = None;
            state.music.analyzer.reset();
        }
        AudioEvent::Durations(list) => {
            for (index, ms) in list {
                if let Some(track) = state.music.tracks.get_mut(index) {
                    track.duration_ms = Some(ms);
                }
            }
        }
        AudioEvent::State(snapshot) => {
            state.music.playing = snapshot.playing;
            state.music.current = snapshot.current;
            state.music.duration_ms = snapshot.duration_ms;
            state.music.scanning = snapshot.scanning;
            state.music.error = snapshot.error;
            if state.music.scrubbing.is_none() {
                state.music.position_ms = snapshot.position_ms;
            }
            state.music.order = snapshot.order;
        }
        AudioEvent::Error(message) => {
            state.toast(ToastKind::Warn, message);
        }
        AudioEvent::DeviceError(message) => {
            state.music.device_error = Some(message.clone());
            state.toast(ToastKind::Warn, message);
        }
        AudioEvent::NowPlaying(info) => {
            // 纹理在视图里按需创建（那里才有 `egui::Context`），这里只落数据。
            let stale = state
                .music
                .cover_texture
                .as_ref()
                .is_some_and(|(key, _)| *key != info.path);
            if stale {
                state.music.cover_texture = None;
            }
            state.music.now_playing = Some(info);
            state.music.analyzer.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::playlist::Track;
    use std::path::PathBuf;

    fn track(title: &str) -> Track {
        Track {
            path: PathBuf::from(format!("C:/m/{title}.mp3")),
            title: title.to_string(),
            duration_ms: None,
        }
    }

    fn snapshot(position_ms: u64) -> crate::audio::AudioState {
        crate::audio::AudioState {
            current: Some(0),
            playing: true,
            position_ms,
            duration_ms: Some(120_000),
            order: MusicOrder::Sequential,
            error: None,
            scanning: false,
        }
    }

    #[test]
    fn state_event_updates_position_while_not_scrubbing() {
        let mut state = UiState::default();
        apply_audio_event(&mut state, AudioEvent::State(snapshot(5_000)));
        assert_eq!(state.music.position_ms, 5_000);
        assert!(state.music.playing);
    }

    #[test]
    fn state_event_does_not_override_scrubbing_position() {
        let mut state = UiState::default();
        state.music.position_ms = 1_000;
        state.music.scrubbing = Some(42_000);
        apply_audio_event(&mut state, AudioEvent::State(snapshot(9_000)));
        assert_eq!(
            state.music.position_ms, 1_000,
            "拖动期间不能用回传覆盖本地位置"
        );
        assert_eq!(state.music.display_position_ms(), 42_000);
    }

    #[test]
    fn playlist_event_resets_selection_and_scanning() {
        let mut state = UiState::default();
        state.music.scanning = true;
        state.music.current = Some(3);
        state.music.tracks = vec![track("old")];
        apply_audio_event(
            &mut state,
            AudioEvent::Playlist {
                tracks: vec![track("a"), track("b")],
                skipped: 2,
                truncated: true,
            },
        );
        assert_eq!(state.music.tracks.len(), 2);
        assert_eq!(state.music.current, None, "换目录后旧索引必须作废");
        assert_eq!(state.music.position_ms, 0);
        assert!(!state.music.scanning);
        assert_eq!(state.music.skipped, 2);
        assert!(state.music.truncated);
    }

    #[test]
    fn duration_event_fills_matching_track_only() {
        let mut state = UiState::default();
        state.music.tracks = vec![track("a"), track("b")];
        apply_audio_event(&mut state, AudioEvent::Durations(vec![(1, 90_000), (9, 1)]));
        assert_eq!(state.music.tracks[0].duration_ms, None);
        assert_eq!(state.music.tracks[1].duration_ms, Some(90_000));
    }

    #[test]
    fn device_error_is_recorded_without_losing_list() {
        let mut state = UiState::default();
        state.music.tracks = vec![track("a")];
        apply_audio_event(
            &mut state,
            AudioEvent::DeviceError("没有可用的音频设备".into()),
        );
        assert_eq!(
            state.music.device_error.as_deref(),
            Some("没有可用的音频设备")
        );
        assert_eq!(state.music.tracks.len(), 1, "设备错误不清空已加载的列表");
    }
}
