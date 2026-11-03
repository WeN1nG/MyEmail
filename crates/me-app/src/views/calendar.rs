//! 日历页：月 / 周 / 议程三种视图 + 事件编辑器。
//!
//! 展开重复规则一律调 `me_core::expand_event`，界面不自己实现规则解析。
//! 写入走 `Command`，不在界面里碰仓储（§2.7）。

use chrono::{
    Datelike, Duration as ChrDuration, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc,
};
use eframe::egui::{self, Align, Color32, Layout, RichText};
use me_core::Command;
use me_core::calendar::{
    Calendar, CalendarEvent, CalendarEventDraft, CalendarEventId, CalendarId, CalendarSource,
    CalendarViewKind,
};

use crate::state::{ToastKind, UiState};

/// 一个已展开的 occurrence（界面视角）。
#[derive(Debug, Clone)]
pub struct OccurrenceView {
    pub event: CalendarEventId,
    pub calendar: CalendarId,
    pub summary: String,
    pub start_utc: i64,
    pub end_utc: i64,
    pub all_day: bool,
    pub recurring: bool,
    pub source: CalendarSource,
}

/// 日历页状态。数据由 `app` 在脏标记置位时重新拉取。
pub struct CalendarState {
    pub calendars: Vec<Calendar>,
    /// `None` = 显示所有可见日历。
    pub selected: Option<CalendarId>,
    pub view: CalendarViewKind,
    /// 当前视图的基准日（本地日期）。
    pub anchor: NaiveDate,
    pub occurrences: Vec<OccurrenceView>,
    /// 已加载的时间范围（UTC 秒），用于判断是否需要重查。
    pub loaded_range: Option<(i64, i64)>,
    pub dirty: bool,
    pub editor: Option<CalendarEditor>,
    /// 新建日历的输入缓冲；`Some` 时显示输入框。
    pub new_calendar: Option<String>,
    /// 视图请求打开某事件的编辑器；由 `app` 读取完整行后填充 `editor`。
    pub pending_open: Option<CalendarEventId>,
}

impl Default for CalendarState {
    fn default() -> Self {
        Self {
            calendars: Vec::new(),
            selected: None,
            view: CalendarViewKind::Month,
            anchor: Local::now().date_naive(),
            occurrences: Vec::new(),
            loaded_range: None,
            dirty: true,
            editor: None,
            new_calendar: None,
            pending_open: None,
        }
    }
}

impl CalendarState {
    pub fn visible_calendars(&self) -> Vec<CalendarId> {
        match self.selected {
            Some(id) => vec![id],
            None => self
                .calendars
                .iter()
                .filter(|calendar| calendar.visible)
                .map(|calendar| calendar.id)
                .collect(),
        }
    }

    pub fn calendar_of(&self, id: CalendarId) -> Option<&Calendar> {
        self.calendars.iter().find(|calendar| calendar.id == id)
    }

    /// 当前视图需要覆盖的本地日期区间（含首尾）。
    pub fn range_days(&self) -> (NaiveDate, NaiveDate) {
        match self.view {
            CalendarViewKind::Month => {
                let first = self.anchor.with_day(1).unwrap_or(self.anchor);
                let offset = first.weekday().num_days_from_monday() as i64;
                let start = first - ChrDuration::days(offset);
                (start, start + ChrDuration::days(41))
            }
            CalendarViewKind::Week => {
                let offset = self.anchor.weekday().num_days_from_monday() as i64;
                let start = self.anchor - ChrDuration::days(offset);
                (start, start + ChrDuration::days(6))
            }
            CalendarViewKind::Agenda => (self.anchor, self.anchor + ChrDuration::days(30)),
        }
    }
}

/// 重复规则预设。`Custom` 保留原文，避免编辑时丢掉编辑器不认识的规则。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatPreset {
    Never,
    Daily,
    Weekly,
    Monthly,
    Yearly,
    Custom,
}

impl RepeatPreset {
    fn label(self) -> &'static str {
        match self {
            RepeatPreset::Never => "不重复",
            RepeatPreset::Daily => "每天",
            RepeatPreset::Weekly => "每周",
            RepeatPreset::Monthly => "每月",
            RepeatPreset::Yearly => "每年",
            RepeatPreset::Custom => "自定义",
        }
    }
}

/// 事件编辑器。时间用「本地民用时间」编辑，保存时换算成 UTC。
pub struct CalendarEditor {
    /// `None` = 新建。
    pub event: Option<CalendarEventId>,
    pub calendar: CalendarId,
    pub summary: String,
    pub location: String,
    pub description: String,
    pub all_day: bool,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    pub preset: RepeatPreset,
    pub interval: u32,
    /// 原始 RRULE；`Custom` 时由用户直接编辑。
    pub raw_rrule: String,
    /// 提醒提前分钟数；`None` = 不提醒。
    pub reminder: Option<u32>,
    /// 编辑既有事件时保留 exdates，界面不提供编辑入口。
    pub exdates: Vec<i64>,
    pub source: CalendarSource,
    pub error: Option<String>,
}

impl CalendarEditor {
    /// 从某个本地日期新建。
    pub fn new(calendar: CalendarId, date: NaiveDate, start_hour: u32) -> Self {
        let hour = start_hour.min(23);
        let start = date
            .and_hms_opt(hour, 0, 0)
            .unwrap_or_else(|| date.and_hms_opt(9, 0, 0).expect("合法时间"));
        Self {
            event: None,
            calendar,
            summary: String::new(),
            location: String::new(),
            description: String::new(),
            all_day: false,
            start,
            end: start + ChrDuration::hours(1),
            preset: RepeatPreset::Never,
            interval: 1,
            raw_rrule: String::new(),
            reminder: Some(15),
            exdates: Vec::new(),
            source: CalendarSource::Local,
            error: None,
        }
    }

    pub fn from_event(event: &CalendarEvent) -> Self {
        let start = local_from_utc(event.start_utc);
        let end = local_from_utc(event.end_utc);
        let (preset, interval) = match event.rrule.as_deref() {
            None => (RepeatPreset::Never, 1),
            Some(rule) => classify_rule(rule),
        };
        Self {
            event: Some(event.id),
            calendar: event.calendar_id,
            summary: event.summary.clone(),
            location: event.location.clone().unwrap_or_default(),
            description: event.description.clone().unwrap_or_default(),
            all_day: event.all_day,
            start,
            end,
            preset,
            interval,
            raw_rrule: event.rrule.clone().unwrap_or_default(),
            reminder: event.reminders.first().copied(),
            exdates: event.exdates.clone(),
            source: event.source,
            error: None,
        }
    }

    fn rrule(&self) -> Option<String> {
        match self.preset {
            RepeatPreset::Never => None,
            RepeatPreset::Custom => {
                let trimmed = self.raw_rrule.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_string())
            }
            other => {
                let freq = match other {
                    RepeatPreset::Daily => "DAILY",
                    RepeatPreset::Weekly => "WEEKLY",
                    RepeatPreset::Monthly => "MONTHLY",
                    RepeatPreset::Yearly => "YEARLY",
                    _ => unreachable!(),
                };
                Some(if self.interval > 1 {
                    format!("FREQ={freq};INTERVAL={}", self.interval)
                } else {
                    format!("FREQ={freq}")
                })
            }
        }
    }

    fn to_draft(&self) -> CalendarEventDraft {
        CalendarEventDraft {
            calendar_id: self.calendar,
            summary: self.summary.trim().to_string(),
            description: non_empty(&self.description),
            location: non_empty(&self.location),
            start_utc: utc_from_local(self.start),
            end_utc: utc_from_local(self.end),
            all_day: self.all_day,
            // 本地日程按绝对时刻保存：浮动时间不会因 DST 漂移，中国区完全等价。
            timezone: None,
            rrule: self.rrule(),
            exdates: self.exdates.clone(),
            reminders: self.reminder.into_iter().collect(),
        }
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn classify_rule(rule: &str) -> (RepeatPreset, u32) {
    let upper = rule.to_ascii_uppercase();
    let mut preset = RepeatPreset::Custom;
    let mut interval = 1;
    for part in upper.split(';') {
        if let Some(freq) = part.strip_prefix("FREQ=") {
            preset = match freq {
                "DAILY" => RepeatPreset::Daily,
                "WEEKLY" => RepeatPreset::Weekly,
                "MONTHLY" => RepeatPreset::Monthly,
                "YEARLY" => RepeatPreset::Yearly,
                _ => RepeatPreset::Custom,
            };
        } else if let Some(value) = part.strip_prefix("INTERVAL=") {
            interval = value.parse().unwrap_or(1);
        } else if !part.is_empty() && preset != RepeatPreset::Never {
            // 含 BYDAY / COUNT / UNTIL 等：交给「自定义」原文编辑，不擅自丢弃。
            preset = RepeatPreset::Custom;
        }
    }
    (preset, interval.max(1))
}

// ---------------------------------------------------------------------------
// 时间换算
// ---------------------------------------------------------------------------

pub fn local_from_utc(ts: i64) -> NaiveDateTime {
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|utc| utc.with_timezone(&Local).naive_local())
        .unwrap_or_else(|| Local.timestamp_opt(ts, 0).single().unwrap().naive_local())
}

pub fn utc_from_local(naive: NaiveDateTime) -> i64 {
    match Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => dt.timestamp(),
        // 夏令时「跳过」的时刻：取跳变后的第一个有效时刻。
        chrono::LocalResult::None => {
            let mut probe = naive;
            for _ in 0..180 {
                probe += ChrDuration::minutes(1);
                if let chrono::LocalResult::Single(dt) = Local.from_local_datetime(&probe) {
                    return dt.timestamp();
                }
            }
            naive.and_utc().timestamp()
        }
        // 夏令时「重复」的时刻：取较早一次。
        chrono::LocalResult::Ambiguous(earlier, _) => earlier.timestamp(),
    }
}

pub fn day_start_utc(date: NaiveDate) -> i64 {
    utc_from_local(date.and_hms_opt(0, 0, 0).expect("合法零点"))
}

fn parse_color(hex: &str) -> Color32 {
    let text = hex.trim_start_matches('#');
    if text.len() == 6
        && let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&text[0..2], 16),
            u8::from_str_radix(&text[2..4], 16),
            u8::from_str_radix(&text[4..6], 16),
        )
    {
        return Color32::from_rgb(r, g, b);
    }
    Color32::from_rgb(59, 130, 246)
}

fn time_label(occ: &OccurrenceView) -> String {
    if occ.all_day {
        return "全天".to_string();
    }
    let start = local_from_utc(occ.start_utc);
    let end = local_from_utc(occ.end_utc);
    if local_date(occ.start_utc) == local_date(occ.end_utc) {
        format!("{}-{}", start.format("%H:%M"), end.format("%H:%M"))
    } else {
        format!("{} 起", start.format("%m-%d %H:%M"))
    }
}

pub fn local_date(ts: i64) -> NaiveDate {
    local_from_utc(ts).date()
}

/// occurrence 是否覆盖某一天（全天与跨天事件按日期区间判断）。
fn occurs_on(occ: &OccurrenceView, date: NaiveDate) -> bool {
    let start_date = local_date(occ.start_utc);
    let end_date = {
        let raw = local_date(occ.end_utc);
        // 结束是独占边界：结束时刻正好在零点时，前一日才是最后一天。
        if occ.end_utc > occ.start_utc
            && local_from_utc(occ.end_utc).time() == chrono::NaiveTime::MIN
        {
            raw - ChrDuration::days(1)
        } else {
            raw
        }
    };
    date >= start_date && date <= end_date
}

// ---------------------------------------------------------------------------
// 主视图
// ---------------------------------------------------------------------------

pub fn show(state: &mut UiState, _commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    toolbar(state, ui);
    ui.separator();
    match state.calendar.view {
        CalendarViewKind::Month => month_view(state, ui),
        CalendarViewKind::Week => week_view(state, ui),
        CalendarViewKind::Agenda => agenda_view(state, ui),
    }
}

fn toolbar(state: &mut UiState, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        if ui.button("◀").on_hover_text("上一页").clicked() {
            shift_anchor(state, -1);
        }
        if ui.button("今天").clicked() {
            state.calendar.anchor = Local::now().date_naive();
            state.calendar.dirty = true;
        }
        if ui.button("▶").on_hover_text("下一页").clicked() {
            shift_anchor(state, 1);
        }
        ui.label(RichText::new(title_of(state)).strong());

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button("＋ 新建日程").clicked() {
                open_new_editor(state, Local::now().date_naive());
            }
            ui.separator();
            for (view, label) in [
                (CalendarViewKind::Agenda, "议程"),
                (CalendarViewKind::Week, "周"),
                (CalendarViewKind::Month, "月"),
            ] {
                if ui
                    .selectable_label(state.calendar.view == view, label)
                    .clicked()
                {
                    state.calendar.view = view;
                    state.calendar.dirty = true;
                }
            }
        });
    });
}

fn title_of(state: &UiState) -> String {
    let anchor = state.calendar.anchor;
    match state.calendar.view {
        CalendarViewKind::Month => anchor.format("%Y 年 %m 月").to_string(),
        CalendarViewKind::Week => {
            let (start, end) = state.calendar.range_days();
            format!(
                "{} - {}",
                start.format("%m 月 %d 日"),
                end.format("%m 月 %d 日")
            )
        }
        CalendarViewKind::Agenda => {
            let (start, end) = state.calendar.range_days();
            format!("{} - {}", start.format("%Y-%m-%d"), end.format("%m-%d"))
        }
    }
}

fn shift_anchor(state: &mut UiState, direction: i32) {
    let anchor = state.calendar.anchor;
    state.calendar.anchor = match state.calendar.view {
        CalendarViewKind::Month => add_months(anchor, direction),
        CalendarViewKind::Week => anchor + ChrDuration::days(7 * direction as i64),
        CalendarViewKind::Agenda => anchor + ChrDuration::days(30 * direction as i64),
    };
    state.calendar.dirty = true;
}

fn add_months(date: NaiveDate, delta: i32) -> NaiveDate {
    let mut year = date.year();
    let mut month = date.month() as i32 + delta;
    while month > 12 {
        month -= 12;
        year += 1;
    }
    while month < 1 {
        month += 12;
        year -= 1;
    }
    NaiveDate::from_ymd_opt(year, month as u32, 1).unwrap_or(date)
}

/// 网格布局常量。窗口缩放时按比例分摊，低于最小尺寸才出现滚动条。
const GRID_SPACING: f32 = 4.0;
const MONTH_HEADER_HEIGHT: f32 = 18.0;
const MIN_CELL_WIDTH: f32 = 62.0;
const MIN_CELL_HEIGHT: f32 = 54.0;
const CELL_PAD_X: f32 = 5.0;
const CELL_PAD_Y: f32 = 3.0;
const CHIP_HEIGHT: f32 = 16.0;
const TODAY_COLOR: Color32 = Color32::from_rgb(220, 80, 60);

/// 主题正文字号（egui 默认 12.5）。
///
/// 此前日期数字用 `RichText::small()`，而 `TextStyle::Small` 只有 9px，格子里
/// 几乎看不清。所有派生字号改为以正文为基准，换主题或缩放后仍然协调。
fn body_size(ui: &egui::Ui) -> f32 {
    ui.style()
        .text_styles
        .get(&egui::TextStyle::Body)
        .map(|font| font.size)
        .unwrap_or(12.5)
}

/// 日期数字字号：正文的 1.4 倍（约 17.5px），且不小于 16px。
fn day_number_size(ui: &egui::Ui) -> f32 {
    (body_size(ui) * 1.4).max(16.0)
}

fn month_view(state: &mut UiState, ui: &mut egui::Ui) {
    let (grid_start, _) = state.calendar.range_days();
    let month = state.calendar.anchor.month();
    let today = Local::now().date_naive();

    // 6 行 × 7 列铺满可用区域：单元格随窗口等比缩放，而不是固定像素。
    let available = ui.available_size();
    let cell_width = ((available.x - GRID_SPACING * 6.0) / 7.0).max(MIN_CELL_WIDTH);
    let cell_height =
        ((available.y - MONTH_HEADER_HEIGHT - GRID_SPACING * 6.0) / 6.0).max(MIN_CELL_HEIGHT);
    let total_width = cell_width * 7.0 + GRID_SPACING * 6.0;
    let total_height = MONTH_HEADER_HEIGHT + cell_height * 6.0 + GRID_SPACING * 6.0;

    if total_width > available.x + 0.5 || total_height > available.y + 0.5 {
        // 只有缩到最小尺寸仍放不下时才滚动，保证小窗口下内容可读。
        egui::ScrollArea::both()
            .id_salt("cal_month_scroll")
            .show(ui, |ui| {
                month_grid(state, ui, grid_start, month, today, cell_width, cell_height);
            });
    } else {
        month_grid(state, ui, grid_start, month, today, cell_width, cell_height);
    }
}

fn month_grid(
    state: &mut UiState,
    ui: &mut egui::Ui,
    grid_start: NaiveDate,
    month: u32,
    today: NaiveDate,
    cell_width: f32,
    cell_height: f32,
) {
    let spacing = GRID_SPACING;
    let total_width = cell_width * 7.0 + spacing * 6.0;
    let total_height = MONTH_HEADER_HEIGHT + spacing + cell_height * 6.0 + spacing * 5.0;
    // 一次性占位，之后按绝对矩形放置：不依赖父游标累加，单元格不会互相挤压。
    let (_, area) = ui.allocate_space(egui::vec2(total_width, total_height));
    let origin = area.min;
    // 星期表头。
    for (col, weekday) in ["一", "二", "三", "四", "五", "六", "日"]
        .iter()
        .enumerate()
    {
        let rect = egui::Rect::from_min_size(
            origin + egui::vec2(col as f32 * (cell_width + spacing), 0.0),
            egui::vec2(cell_width, MONTH_HEADER_HEIGHT),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
            ui.label(
                RichText::new(*weekday)
                    .size(body_size(ui) * 0.9)
                    .color(Color32::GRAY),
            );
        });
    }

    let grid_top = MONTH_HEADER_HEIGHT + spacing;
    let mut date = grid_start;
    for row in 0..6 {
        for col in 0..7 {
            let rect = egui::Rect::from_min_size(
                origin
                    + egui::vec2(
                        col as f32 * (cell_width + spacing),
                        grid_top + row as f32 * (cell_height + spacing),
                    ),
                egui::vec2(cell_width, cell_height),
            );
            day_cell(state, ui, rect, date, month, today);
            date += ChrDuration::days(1);
        }
    }
}

/// 按给定矩形绘制一天。矩形由 `month_grid` 绝对定位，因此窗口缩放时整体等比变化。
fn day_cell(
    state: &mut UiState,
    ui: &mut egui::Ui,
    rect: egui::Rect,
    date: NaiveDate,
    month: u32,
    today: NaiveDate,
) {
    let in_month = date.month() == month;
    let visuals = ui.visuals();
    let fill = if in_month {
        visuals.extreme_bg_color
    } else {
        visuals.faint_bg_color
    };
    let stroke = egui::Stroke::new(1.0_f32, visuals.widgets.noninteractive.bg_stroke.color);
    ui.painter()
        .rect(rect, 4.0, fill, stroke, egui::StrokeKind::Inside);

    // 整格都是「新建日程」热区，不必精准点中日期数字。
    // 先注册、日程条目后注册，后者在上层，因此点条目仍打开编辑而非新建。
    let response = ui.allocate_rect(rect, egui::Sense::click());
    if response.clicked() {
        open_new_editor(state, date);
    }

    let inner = rect.shrink2(egui::vec2(CELL_PAD_X, CELL_PAD_Y));
    // `sense` 必须是 hover：否则子 Ui 会注册一个覆盖整格的点击热区，
    // 把点击从格子上抢走（实测 hovered 恒为 false）。
    let builder = egui::UiBuilder::new()
        .max_rect(inner)
        .sense(egui::Sense::hover());
    ui.scope_builder(builder, |ui| {
        ui.set_clip_rect(inner);
        ui.spacing_mut().item_spacing.y = 1.0;

        let number_size = day_number_size(ui);
        let header_height = number_size + 4.0;

        // 日期数字用纯绘制：`ui.label` 会注册一个覆盖文字的悬停控件，
        // 实测会把整格的点击命中抢走（hovered 恒为 false）。
        let number_color = if !in_month {
            Color32::GRAY
        } else if date == today {
            TODAY_COLOR
        } else {
            ui.visuals().text_color()
        };
        let number_rect =
            egui::Rect::from_min_size(inner.min, egui::vec2(inner.width(), number_size));
        ui.painter().text(
            number_rect.left_top(),
            egui::Align2::LEFT_TOP,
            date.day().to_string(),
            egui::FontId::proportional(number_size),
            number_color,
        );
        ui.add_space(header_height);

        // 能放几条由单元格高度决定：窗口变高就多显示，变矮则自动收起。
        let capacity = (((inner.height() - header_height) / CHIP_HEIGHT).floor() as usize).max(1);
        let mut shown = 0usize;
        let mut total = 0usize;
        for index in 0..state.calendar.occurrences.len() {
            let occ = state.calendar.occurrences[index].clone();
            if !occurs_on(&occ, date) {
                continue;
            }
            total += 1;
            if shown >= capacity {
                continue;
            }
            shown += 1;
            occurrence_chip(state, ui, &occ, false);
        }
        if total > shown {
            ui.label(
                RichText::new(format!("+{} 项", total - shown))
                    .size(body_size(ui) * 0.9)
                    .color(Color32::GRAY),
            );
        }
    });
}

/// 事件条目。点击进入编辑。
fn occurrence_chip(state: &mut UiState, ui: &mut egui::Ui, occ: &OccurrenceView, with_time: bool) {
    let color = state
        .calendar
        .calendar_of(occ.calendar)
        .map(|calendar| parse_color(&calendar.color))
        .unwrap_or(Color32::from_rgb(59, 130, 246));
    let prefix = if occ.recurring { "⟳ " } else { "" };
    let text = if with_time {
        format!("{prefix}{} {}", time_label(occ), occ.summary)
    } else {
        format!("{prefix}{}", occ.summary)
    };
    let label = RichText::new(text).size(body_size(ui) * 0.95).color(color);
    if ui
        .add(
            egui::Label::new(label)
                .truncate()
                .sense(egui::Sense::click()),
        )
        .on_hover_text({
            let when = if occ.all_day {
                "全天".to_string()
            } else {
                format!(
                    "{} → {}",
                    local_from_utc(occ.start_utc).format("%Y-%m-%d %H:%M"),
                    local_from_utc(occ.end_utc).format("%Y-%m-%d %H:%M")
                )
            };
            let origin = match occ.source {
                CalendarSource::Local => "本地创建",
                CalendarSource::Ics => "ICS 导入",
                CalendarSource::Ai => "AI 创建",
            };
            format!("{}\n{when}\n{origin}", occ.summary)
        })
        .clicked()
    {
        state.calendar.pending_open = Some(occ.event);
    }
}

fn week_view(state: &mut UiState, ui: &mut egui::Ui) {
    let (start, _) = state.calendar.range_days();
    let today = Local::now().date_naive();

    // 7 列铺满宽度、列高铺满剩余高度。
    let available = ui.available_size();
    let column_width = ((available.x - GRID_SPACING * 6.0) / 7.0).max(MIN_CELL_WIDTH);
    let column_height = available.y.max(MIN_CELL_HEIGHT * 3.0);
    let total_width = column_width * 7.0 + GRID_SPACING * 6.0;

    if total_width > available.x + 0.5 || column_height > available.y + 0.5 {
        egui::ScrollArea::both()
            .id_salt("cal_week_scroll")
            .show(ui, |ui| {
                week_columns(state, ui, start, today, column_width, column_height);
            });
    } else {
        week_columns(state, ui, start, today, column_width, column_height);
    }
}

fn week_columns(
    state: &mut UiState,
    ui: &mut egui::Ui,
    start: NaiveDate,
    today: NaiveDate,
    width: f32,
    height: f32,
) {
    let spacing = GRID_SPACING;
    let total_width = width * 7.0 + spacing * 6.0;
    let (_, area) = ui.allocate_space(egui::vec2(total_width, height));
    let origin = area.min;

    let mut date = start;
    for col in 0..7 {
        let rect = egui::Rect::from_min_size(
            origin + egui::vec2(col as f32 * (width + spacing), 0.0),
            egui::vec2(width, height),
        );
        week_column(state, ui, rect, date, today);
        date += ChrDuration::days(1);
    }
}

fn week_column(
    state: &mut UiState,
    ui: &mut egui::Ui,
    rect: egui::Rect,
    date: NaiveDate,
    today: NaiveDate,
) {
    let visuals = ui.visuals();
    let stroke = egui::Stroke::new(1.0_f32, visuals.widgets.noninteractive.bg_stroke.color);
    ui.painter().rect(
        rect,
        4.0,
        visuals.extreme_bg_color,
        stroke,
        egui::StrokeKind::Inside,
    );

    // 整列都是新建热区；日期头只是提示，不再单独可点。
    let response = ui
        .interact(
            rect,
            ui.id().with(("week_column", date.num_days_from_ce())),
            egui::Sense::click(),
        )
        .on_hover_text("点击新建日程");
    if response.clicked() {
        open_new_editor(state, date);
    }

    let inner = rect.shrink2(egui::vec2(CELL_PAD_X + 1.0, CELL_PAD_Y + 1.0));
    ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
        ui.set_clip_rect(inner);
        ui.spacing_mut().item_spacing.y = 2.0;

        // 同样用纯绘制：列头若是 Label，会抢走整列的点击命中。
        let header_size = body_size(ui) * 1.1;
        let header_color = if date == today {
            TODAY_COLOR
        } else {
            ui.visuals().text_color()
        };
        ui.painter().text(
            inner.left_top(),
            egui::Align2::LEFT_TOP,
            format!("{} {}", weekday_label(date), date.format("%m-%d")),
            egui::FontId::proportional(header_size),
            header_color,
        );
        ui.add_space(header_size + 4.0);
        ui.separator();

        for index in 0..state.calendar.occurrences.len() {
            let occ = state.calendar.occurrences[index].clone();
            if occurs_on(&occ, date) {
                occurrence_chip(state, ui, &occ, true);
            }
        }
    });
}

fn weekday_label(date: NaiveDate) -> &'static str {
    match date.weekday() {
        chrono::Weekday::Mon => "周一",
        chrono::Weekday::Tue => "周二",
        chrono::Weekday::Wed => "周三",
        chrono::Weekday::Thu => "周四",
        chrono::Weekday::Fri => "周五",
        chrono::Weekday::Sat => "周六",
        chrono::Weekday::Sun => "周日",
    }
}

fn agenda_view(state: &mut UiState, ui: &mut egui::Ui) {
    let (start, end) = state.calendar.range_days();
    egui::ScrollArea::vertical()
        .id_salt("cal_agenda_scroll")
        .show(ui, |ui| {
            let mut date = start;
            let mut any = false;
            while date <= end {
                let mut items: Vec<OccurrenceView> = state
                    .calendar
                    .occurrences
                    .iter()
                    .filter(|occ| occurs_on(occ, date))
                    .cloned()
                    .collect();
                if !items.is_empty() {
                    any = true;
                    items.sort_by_key(|occ| (occ.all_day, occ.start_utc));
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(format!(
                            "{} {}",
                            date.format("%Y-%m-%d"),
                            weekday_label(date)
                        ))
                        .strong(),
                    );
                    for occ in items {
                        occurrence_chip(state, ui, &occ, true);
                    }
                    ui.separator();
                }
                date += ChrDuration::days(1);
            }
            if !any {
                ui.add_space(24.0);
                ui.label(
                    RichText::new("这段时间没有日程。点击左上角「＋ 新建日程」添加。")
                        .color(Color32::GRAY),
                );
            }
        });
}

fn open_new_editor(state: &mut UiState, date: NaiveDate) {
    let calendar = state
        .calendar
        .selected
        .or_else(|| {
            state
                .calendar
                .calendars
                .iter()
                .find(|calendar| calendar.is_default)
                .map(|calendar| calendar.id)
        })
        .or_else(|| state.calendar.calendars.first().map(|calendar| calendar.id));
    let Some(calendar) = calendar else {
        state.toast(ToastKind::Warn, "还没有可用日历");
        return;
    };
    // 默认时间：点今天时取当前小时（不早于 9 点），点其它日期固定 9 点。
    // 日期一律用被点格子的日期，用户不必再改。
    let now = Local::now();
    let hour = if date == now.date_naive() {
        now.hour().max(9)
    } else {
        9
    };
    state.calendar.editor = Some(CalendarEditor::new(calendar, date, hour));
}

// ---------------------------------------------------------------------------
// 编辑器窗口
// ---------------------------------------------------------------------------

pub fn show_editor(state: &mut UiState, commands: &mut Vec<Command>, ctx: &egui::Context) {
    let Some(mut editor) = state.calendar.editor.take() else {
        return;
    };
    let calendars = state.calendar.calendars.clone();

    let mut open = true;
    let mut cancel = false;
    let mut submit = false;
    let mut delete = false;

    egui::Window::new(if editor.event.is_some() {
        "编辑日程"
    } else {
        "新建日程"
    })
    .collapsible(false)
    .resizable(false)
    .default_width(460.0)
    .open(&mut open)
    .show(ctx, |ui| {
        egui::Grid::new("cal_editor_grid")
            .num_columns(2)
            .spacing([10.0, 8.0])
            .show(ui, |ui| {
                ui.label("标题");
                ui.add(
                    egui::TextEdit::singleline(&mut editor.summary)
                        .desired_width(320.0)
                        .hint_text("日程标题"),
                );
                ui.end_row();

                ui.label("日历");
                egui::ComboBox::from_id_salt("cal_editor_calendar")
                    .selected_text(
                        calendars
                            .iter()
                            .find(|calendar| calendar.id == editor.calendar)
                            .map(|calendar| calendar.name.clone())
                            .unwrap_or_else(|| "默认".to_string()),
                    )
                    .show_ui(ui, |ui| {
                        for calendar in &calendars {
                            ui.selectable_value(&mut editor.calendar, calendar.id, &calendar.name);
                        }
                    });
                ui.end_row();

                ui.label("时间");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut editor.all_day, "全天");
                    if editor.all_day {
                        date_widget(ui, &mut editor.start, "cal_start_date");
                        editor.end = editor.start + ChrDuration::days(1);
                        ui.label("（一天）");
                    } else {
                        datetime_widget(ui, &mut editor.start, "cal_start");
                        ui.label("→");
                        datetime_widget(ui, &mut editor.end, "cal_end");
                    }
                });
                ui.end_row();

                ui.label("重复");
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("cal_editor_repeat")
                        .selected_text(editor.preset.label())
                        .show_ui(ui, |ui| {
                            for preset in [
                                RepeatPreset::Never,
                                RepeatPreset::Daily,
                                RepeatPreset::Weekly,
                                RepeatPreset::Monthly,
                                RepeatPreset::Yearly,
                                RepeatPreset::Custom,
                            ] {
                                ui.selectable_value(&mut editor.preset, preset, preset.label());
                            }
                        });
                    if matches!(
                        editor.preset,
                        RepeatPreset::Daily
                            | RepeatPreset::Weekly
                            | RepeatPreset::Monthly
                            | RepeatPreset::Yearly
                    ) {
                        ui.label("每");
                        ui.add(egui::DragValue::new(&mut editor.interval).range(1..=52));
                        ui.label("个周期");
                    }
                });
                ui.end_row();

                if editor.preset == RepeatPreset::Custom {
                    ui.label("规则");
                    ui.add(
                        egui::TextEdit::singleline(&mut editor.raw_rrule)
                            .desired_width(320.0)
                            .hint_text("FREQ=WEEKLY;BYDAY=MO,WE"),
                    );
                    ui.end_row();
                }

                ui.label("提醒");
                ui.horizontal(|ui| {
                    let mut choice = editor.reminder;
                    egui::ComboBox::from_id_salt("cal_editor_reminder")
                        .selected_text(match choice {
                            None => "不提醒".to_string(),
                            Some(0) => "开始时".to_string(),
                            Some(minutes) => format!("提前 {minutes} 分钟"),
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut choice, None, "不提醒");
                            ui.selectable_value(&mut choice, Some(0), "开始时");
                            for minutes in [5u32, 10, 15, 30, 60] {
                                ui.selectable_value(
                                    &mut choice,
                                    Some(minutes),
                                    format!("提前 {minutes} 分钟"),
                                );
                            }
                        });
                    editor.reminder = choice;
                });
                ui.end_row();

                ui.label("地点");
                ui.add(egui::TextEdit::singleline(&mut editor.location).desired_width(320.0));
                ui.end_row();

                ui.label("备注");
                ui.add(
                    egui::TextEdit::multiline(&mut editor.description)
                        .desired_width(320.0)
                        .desired_rows(3),
                );
                ui.end_row();
            });

        if editor.source == CalendarSource::Ai {
            ui.label(
                RichText::new("该日程由 AI 创建")
                    .small()
                    .color(Color32::GRAY),
            );
        }

        if let Some(error) = editor.error.clone() {
            ui.label(RichText::new(error).color(Color32::from_rgb(210, 80, 80)));
        }

        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("保存").clicked() {
                submit = true;
            }
            if ui.button("取消").clicked() {
                cancel = true;
            }
            if editor.event.is_some() {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .button(RichText::new("删除").color(Color32::from_rgb(200, 80, 80)))
                        .clicked()
                    {
                        delete = true;
                    }
                });
            }
        });
    });

    if cancel {
        return;
    }

    if delete {
        if let Some(id) = editor.event {
            commands.push(Command::DeleteCalendarEvent { id });
        }
        return;
    }

    if submit {
        match validate_editor(&editor) {
            Ok(()) => {
                let draft = editor.to_draft();
                match editor.event {
                    Some(id) => commands.push(Command::UpdateCalendarEvent { id, draft }),
                    None => commands.push(Command::CreateCalendarEvent { draft }),
                }
                state.calendar.dirty = true;
                return;
            }
            Err(message) => {
                editor.error = Some(message);
                state.calendar.editor = Some(editor);
                return;
            }
        }
    }

    if open {
        state.calendar.editor = Some(editor);
    }
}

fn validate_editor(editor: &CalendarEditor) -> Result<(), String> {
    if editor.summary.trim().is_empty() {
        return Err("标题不能为空".to_string());
    }
    if editor.end < editor.start {
        return Err("结束时间不能早于开始时间".to_string());
    }
    if editor.preset == RepeatPreset::Custom && editor.raw_rrule.trim().is_empty() {
        return Err("自定义重复规则不能为空".to_string());
    }
    Ok(())
}

fn date_widget(ui: &mut egui::Ui, value: &mut NaiveDateTime, id: &str) {
    let mut year = value.year();
    let mut month = value.month();
    let mut day = value.day();
    ui.push_id(id, |ui| {
        ui.horizontal(|ui| {
            ui.add(
                egui::DragValue::new(&mut year)
                    .range(1970..=2100)
                    .suffix(" 年"),
            );
            ui.add(egui::DragValue::new(&mut month).range(1..=12).suffix(" 月"));
            ui.add(egui::DragValue::new(&mut day).range(1..=31).suffix(" 日"));
        });
    });
    *value = rebuild_datetime(*value, year, month, day);
}

fn datetime_widget(ui: &mut egui::Ui, value: &mut NaiveDateTime, id: &str) {
    let mut year = value.year();
    let mut month = value.month();
    let mut day = value.day();
    let mut hour = value.hour();
    let mut minute = value.minute();
    ui.push_id(id, |ui| {
        ui.horizontal(|ui| {
            ui.add(
                egui::DragValue::new(&mut year)
                    .range(1970..=2100)
                    .suffix(" 年"),
            );
            ui.add(egui::DragValue::new(&mut month).range(1..=12).suffix(" 月"));
            ui.add(egui::DragValue::new(&mut day).range(1..=31).suffix(" 日"));
            ui.add(egui::DragValue::new(&mut hour).range(0..=23).suffix(" 时"));
            ui.add(
                egui::DragValue::new(&mut minute)
                    .range(0..=59)
                    .suffix(" 分"),
            );
        });
    });
    let rebuilt = rebuild_datetime(*value, year, month, day);
    *value = rebuilt
        .date()
        .and_hms_opt(hour.clamp(0, 23), minute.clamp(0, 59), 0)
        .unwrap_or(rebuilt);
}

/// 把「年/月/日」拼回日期；该月没有这一天（如 2 月 31 日）时退到月末。
fn rebuild_datetime(previous: NaiveDateTime, year: i32, month: u32, day: u32) -> NaiveDateTime {
    let base = NaiveDate::from_ymd_opt(year, month, 1).unwrap_or_else(|| previous.date());
    let last_day = (28..=31)
        .rev()
        .find(|candidate| NaiveDate::from_ymd_opt(year, month, *candidate).is_some())
        .unwrap_or(28);
    base.with_day(day.clamp(1, last_day))
        .unwrap_or(base)
        .and_time(previous.time())
}

// ---------------------------------------------------------------------------
// 左侧日历列表
// ---------------------------------------------------------------------------

pub fn sidebar(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("我的日历").strong());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.small_button("＋").on_hover_text("新建日历").clicked() {
                state.calendar.new_calendar = Some(String::new());
            }
        });
    });
    ui.separator();

    if ui
        .selectable_label(state.calendar.selected.is_none(), "全部日历")
        .clicked()
    {
        state.calendar.selected = None;
        state.calendar.dirty = true;
    }

    let calendars = state.calendar.calendars.clone();
    for calendar in calendars {
        ui.horizontal(|ui| {
            let mut visible = calendar.visible;
            if ui
                .checkbox(&mut visible, "")
                .on_hover_text("显示 / 隐藏")
                .changed()
            {
                commands.push(Command::SetCalendarVisible {
                    id: calendar.id,
                    visible,
                });
                state.calendar.dirty = true;
            }
            let color = parse_color(&calendar.color);
            ui.label(RichText::new("●").color(color));
            let selected = state.calendar.selected == Some(calendar.id);
            if ui.selectable_label(selected, &calendar.name).clicked() {
                state.calendar.selected = Some(calendar.id);
                state.calendar.dirty = true;
            }
        });
    }
}

// ---------------------------------------------------------------------------
// 新建日历
// ---------------------------------------------------------------------------

pub fn new_calendar_dialog(state: &mut UiState, commands: &mut Vec<Command>, ctx: &egui::Context) {
    let Some(mut name) = state.calendar.new_calendar.take() else {
        return;
    };
    let mut open = true;
    let mut cancel = false;
    let mut submit = false;

    egui::Window::new("新建日历")
        .collapsible(false)
        .resizable(false)
        .default_width(280.0)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("名称");
                ui.add(
                    egui::TextEdit::singleline(&mut name)
                        .desired_width(180.0)
                        .hint_text("例如：工作"),
                );
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("创建").clicked() {
                    submit = true;
                }
                if ui.button("取消").clicked() {
                    cancel = true;
                }
            });
        });

    if cancel {
        return;
    }
    if submit && !name.trim().is_empty() {
        commands.push(Command::CreateCalendar {
            name: name.trim().to_string(),
            // 新日历的颜色由调色板轮转，避免全部同色。
            color: next_calendar_color(&state.calendar.calendars),
        });
        return;
    }
    if open {
        state.calendar.new_calendar = Some(name);
    }
}

fn next_calendar_color(calendars: &[Calendar]) -> String {
    const PALETTE: [&str; 6] = [
        "#3B82F6", "#EF4444", "#10B981", "#F59E0B", "#8B5CF6", "#EC4899",
    ];
    PALETTE[calendars.len() % PALETTE.len()].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_core::calendar::{CalendarEvent, CalendarEventStatus};

    fn sample_event(rrule: Option<&str>) -> CalendarEvent {
        let start = utc_from_local(
            NaiveDate::from_ymd_opt(2026, 11, 3)
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap(),
        );
        CalendarEvent {
            id: CalendarEventId(1),
            calendar_id: CalendarId(1),
            uid: "u1".into(),
            summary: "周会".into(),
            description: None,
            location: None,
            start_utc: start,
            end_utc: start + 3_600,
            all_day: false,
            timezone: None,
            rrule: rrule.map(|rule| rule.to_string()),
            recurrence_until: None,
            exdates: Vec::new(),
            reminders: vec![15],
            status: CalendarEventStatus::Confirmed,
            source: CalendarSource::Local,
            sequence: 0,
            created_at: start,
            updated_at: start,
            deleted_at: None,
        }
    }

    #[test]
    fn local_utc_round_trip() {
        let naive = NaiveDate::from_ymd_opt(2026, 11, 3)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let utc = utc_from_local(naive);
        assert_eq!(local_from_utc(utc), naive);
        assert_eq!(local_date(utc), naive.date());
    }

    #[test]
    fn all_day_event_covers_single_day() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 3).unwrap();
        let start = day_start_utc(day);
        let occ = OccurrenceView {
            event: CalendarEventId(1),
            calendar: CalendarId(1),
            summary: "全天".into(),
            start_utc: start,
            end_utc: day_start_utc(day + ChrDuration::days(1)),
            all_day: true,
            recurring: false,
            source: CalendarSource::Local,
        };
        // 结束是独占边界：次日零点不应算作覆盖。
        assert!(occurs_on(&occ, day));
        assert!(!occurs_on(&occ, day + ChrDuration::days(1)));
        assert!(!occurs_on(&occ, day - ChrDuration::days(1)));
    }

    #[test]
    fn multi_day_event_covers_each_day() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 3).unwrap();
        let occ = OccurrenceView {
            event: CalendarEventId(1),
            calendar: CalendarId(1),
            summary: "出差".into(),
            start_utc: day_start_utc(day),
            end_utc: day_start_utc(day + ChrDuration::days(3)),
            all_day: true,
            recurring: false,
            source: CalendarSource::Local,
        };
        for offset in 0..3 {
            assert!(occurs_on(&occ, day + ChrDuration::days(offset)));
        }
        assert!(!occurs_on(&occ, day + ChrDuration::days(3)));
    }

    #[test]
    fn simple_rules_map_to_presets() {
        let editor = CalendarEditor::from_event(&sample_event(Some("FREQ=DAILY")));
        assert_eq!(editor.preset, RepeatPreset::Daily);
        assert_eq!(editor.interval, 1);
        assert_eq!(editor.rrule().as_deref(), Some("FREQ=DAILY"));

        let editor = CalendarEditor::from_event(&sample_event(Some("FREQ=WEEKLY;INTERVAL=2")));
        assert_eq!(editor.preset, RepeatPreset::Weekly);
        assert_eq!(editor.interval, 2);
        assert_eq!(editor.rrule().as_deref(), Some("FREQ=WEEKLY;INTERVAL=2"));
    }

    #[test]
    fn complex_rules_are_preserved_as_custom() {
        // BYDAY 不在预设里：必须走「自定义」并保留原文，不能悄悄丢掉。
        let editor = CalendarEditor::from_event(&sample_event(Some("FREQ=WEEKLY;BYDAY=MO,WE")));
        assert_eq!(editor.preset, RepeatPreset::Custom);
        assert_eq!(editor.rrule().as_deref(), Some("FREQ=WEEKLY;BYDAY=MO,WE"));
    }

    #[test]
    fn editor_validates_before_submit() {
        let mut editor = CalendarEditor::new(
            CalendarId(1),
            NaiveDate::from_ymd_opt(2026, 11, 3).unwrap(),
            9,
        );
        assert!(validate_editor(&editor).is_err());
        editor.summary = "有效".into();
        assert!(validate_editor(&editor).is_ok());
        editor.end = editor.start - ChrDuration::hours(1);
        assert!(validate_editor(&editor).is_err());
    }

    fn state_with_events() -> UiState {
        let mut state = UiState::default();
        state.page = crate::state::Page::Calendar;
        state.calendar.calendars = vec![Calendar {
            id: CalendarId(1),
            name: "我的日历".into(),
            color: "#3B82F6".into(),
            visible: true,
            is_default: true,
            created_at: 0,
            updated_at: 0,
        }];
        // 基准日与示例日程同一天：周/议程视图才会包含它。
        state.calendar.anchor = NaiveDate::from_ymd_opt(2026, 11, 3).unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 11, 3).unwrap();
        state.calendar.occurrences = vec![
            OccurrenceView {
                event: CalendarEventId(1),
                calendar: CalendarId(1),
                summary: "普通日程".into(),
                start_utc: utc_from_local(day.and_hms_opt(9, 0, 0).unwrap()),
                end_utc: utc_from_local(day.and_hms_opt(10, 0, 0).unwrap()),
                all_day: false,
                recurring: false,
                source: CalendarSource::Local,
            },
            OccurrenceView {
                event: CalendarEventId(2),
                calendar: CalendarId(1),
                summary: "全天日程".into(),
                start_utc: day_start_utc(day),
                end_utc: day_start_utc(day + ChrDuration::days(1)),
                all_day: true,
                recurring: true,
                source: CalendarSource::Ai,
            },
        ];
        state
    }

    /// 一帧的绘制结果：文本用于断言「画了什么」，矩形用于断言「铺了多大」。
    /// 一段被绘制出来的文字：位置用于点击定位，字号用于验证可读性。
    #[derive(Debug)]
    struct TextRun {
        text: String,
        x: f32,
        y: f32,
        size: f32,
    }

    struct FrameOutput {
        texts: Vec<TextRun>,
        rects: Vec<egui::Rect>,
    }

    impl FrameOutput {
        fn has(&self, needle: &str) -> bool {
            self.texts.iter().any(|run| run.text.contains(needle))
        }

        fn has_exact(&self, label: &str) -> bool {
            self.texts.iter().any(|run| run.text == label)
        }

        fn left_of(&self, label: &str) -> Option<f32> {
            self.texts
                .iter()
                .find(|run| run.text == label)
                .map(|run| run.x)
        }

        fn run(&self, label: &str) -> Option<&TextRun> {
            self.texts.iter().find(|run| run.text == label)
        }
    }

    fn collect_shape(shape: &egui::Shape, out: &mut FrameOutput) {
        match shape {
            egui::Shape::Text(text) => {
                let size = text
                    .galley
                    .job
                    .sections
                    .first()
                    .map(|section| section.format.font_id.size)
                    .unwrap_or(0.0);
                out.texts.push(TextRun {
                    text: text.galley.text().to_string(),
                    x: text.pos.x,
                    y: text.pos.y,
                    size,
                });
            }
            egui::Shape::Rect(rect) => out.rects.push(rect.rect),
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_shape(shape, out);
                }
            }
            _ => {}
        }
    }

    fn run_frame_sized(
        state: &mut UiState,
        width: f32,
        height: f32,
        mut draw: impl FnMut(&mut UiState, &mut egui::Ui),
    ) -> FrameOutput {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, height),
            )),
            ..Default::default()
        };
        // egui 0.32 一帧可能布局多次，绘制闭包必须可重复调用。
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| draw(state, ui));
        });
        let mut out = FrameOutput {
            texts: Vec::new(),
            rects: Vec::new(),
        };
        for clipped in &output.shapes {
            collect_shape(&clipped.shape, &mut out);
        }
        out
    }

    fn run_frame(
        state: &mut UiState,
        draw: impl FnMut(&mut UiState, &mut egui::Ui),
    ) -> FrameOutput {
        run_frame_sized(state, 1280.0, 900.0, draw)
    }

    #[test]
    fn all_views_render_events_and_title() {
        for view in [
            CalendarViewKind::Month,
            CalendarViewKind::Week,
            CalendarViewKind::Agenda,
        ] {
            let mut state = state_with_events();
            state.calendar.view = view;
            let mut commands = Vec::new();
            let out = run_frame(&mut state, |state, ui| {
                show(state, &mut commands, ui);
            });
            assert!(
                out.has("普通日程"),
                "{view:?} 视图没有渲染出日程条目：{:?}",
                out.texts
            );
            assert!(out.has("全天"), "{view:?} 视图没有渲染出全天日程");
            assert!(
                out.has("今天") && out.has("新建日程"),
                "{view:?} 视图缺少工具栏"
            );
        }
    }

    #[test]
    fn month_view_shows_weekday_header_and_grid() {
        let mut state = state_with_events();
        let mut commands = Vec::new();
        let out = run_frame(&mut state, |state, ui| {
            show(state, &mut commands, ui);
        });
        for weekday in ["一", "二", "三", "四", "五", "六", "日"] {
            assert!(out.has_exact(weekday), "月视图缺少星期表头 {weekday}");
        }
        // 月网格应画出当月每一天的日期数字。
        for day in 1..=30 {
            let label = day.to_string();
            assert!(out.has_exact(&label), "月视图缺少 {day} 日单元格");
        }
    }

    /// 模拟一次真实点击：先布局一帧拿到坐标，再分别送 press / release。
    ///
    /// egui 的命中测试依赖上一帧的控件矩形，所以不能在同一帧里既布局又点击。
    fn click_text(state: &mut UiState, width: f32, height: f32, target: &str) {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, height));
        let make_input = |time: f64, events: Vec<egui::Event>| egui::RawInput {
            screen_rect: Some(screen),
            time: Some(time),
            events,
            ..Default::default()
        };

        let mut commands = Vec::new();
        let first = ctx.run(make_input(0.0, Vec::new()), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| show(state, &mut commands, ui));
        });
        let mut out = FrameOutput {
            texts: Vec::new(),
            rects: Vec::new(),
        };
        for clipped in &first.shapes {
            collect_shape(&clipped.shape, &mut out);
        }
        let run = out.run(target).expect("目标文字没有被绘制出来");
        let pos = egui::pos2(run.x + 2.0, run.y + run.size * 0.5);
        for (time, pressed) in [(0.1, true), (0.2, false)] {
            let mut commands = Vec::new();
            let events = vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::default(),
                },
            ];
            let _ = ctx.run(make_input(time, events), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| show(state, &mut commands, ui));
            });
        }
    }

    #[test]
    fn day_numbers_are_readable() {
        let mut state = state_with_events();
        let mut commands = Vec::new();
        let out = run_frame(&mut state, |state, ui| {
            show(state, &mut commands, ui);
        });
        // 旧实现用 `.small()` = 9px，正是「数字太小」的根因。
        let run = out.run("15").expect("月视图缺少 15 日");
        assert!(
            run.size >= 16.0,
            "日期数字字号只有 {:.1}px，仍然偏小",
            run.size
        );
        // 星期表头也不该回到 9px。
        let header = out.run("一").expect("缺少星期表头");
        assert!(header.size >= 11.0, "星期表头字号只有 {:.1}px", header.size);
    }

    #[test]
    fn clicking_day_cell_opens_editor_on_that_date() {
        let mut state = state_with_events();
        // 月视图锚定 2026-11，点击 15 号数字所在格子。
        click_text(&mut state, 1280.0, 900.0, "15");

        let editor = state
            .calendar
            .editor
            .as_ref()
            .expect("点击日期格子应打开新建日程");
        assert_eq!(
            editor.start.date(),
            NaiveDate::from_ymd_opt(2026, 11, 15).unwrap(),
            "新建日程的默认日期必须是被点格子的日期"
        );
        assert!(editor.summary.is_empty(), "新建时标题应为空");
        assert!(editor.event.is_none(), "新建时不应绑定已有事件");
    }

    /// 点日程条目应当是「打开编辑」，不能被格子抢成「新建」。
    #[test]
    fn clicking_event_chip_opens_it_instead_of_creating() {
        let mut state = state_with_events();
        click_text(&mut state, 1280.0, 900.0, "普通日程");

        assert_eq!(
            state.calendar.pending_open,
            Some(CalendarEventId(1)),
            "点击日程条目应请求打开该事件"
        );
        assert!(
            state.calendar.editor.is_none(),
            "点击已有日程不应打开新建编辑器"
        );
    }

    #[test]
    fn open_new_editor_defaults_to_nine_or_now() {
        let mut state = state_with_events();
        open_new_editor(&mut state, NaiveDate::from_ymd_opt(2026, 12, 24).unwrap());
        let editor = state.calendar.editor.as_ref().expect("应打开编辑器");
        assert_eq!(
            editor.start,
            NaiveDate::from_ymd_opt(2026, 12, 24)
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap()
        );
    }

    /// 月网格必须随窗口等比缩放：最后一列的表头要始终贴住右边缘。
    ///
    /// 固定像素宽度时，窗口放大 60% 而网格不变，右侧会留下大片空白；
    /// 这条断言把「占比稳定」钉死，改回固定宽度会立刻失败。
    #[test]
    fn month_grid_scales_with_window_width() {
        let mut ratios = Vec::new();
        for (width, height) in [(820.0_f32, 620.0_f32), (1280.0, 900.0), (1800.0, 1100.0)] {
            let mut state = state_with_events();
            let mut commands = Vec::new();
            let out = run_frame_sized(&mut state, width, height, |state, ui| {
                show(state, &mut commands, ui);
            });
            let x = out.left_of("日").expect("缺少最后一列表头");
            let ratio = x / width;
            assert!(
                ratio > 0.75,
                "窗口宽 {width} 时最后一列只到 {:.0}px（占比 {:.0}%），没有铺满",
                x,
                ratio * 100.0
            );
            ratios.push(ratio);
        }
        let spread = ratios.iter().cloned().fold(f32::MIN, f32::max)
            - ratios.iter().cloned().fold(f32::MAX, f32::min);
        assert!(
            spread < 0.05,
            "不同窗口宽度下最后一列占比不一致（{ratios:?}），说明没有等比缩放"
        );
    }

    /// 单元格高度同样必须随窗口高度变化（此前固定 96px）。
    #[test]
    fn month_cells_scale_with_window_height() {
        let tallest_cell = |height: f32| {
            let mut state = state_with_events();
            let mut commands = Vec::new();
            let out = run_frame_sized(&mut state, 1280.0, height, |state, ui| {
                show(state, &mut commands, ui);
            });
            // 排除面板背景（与窗口同尺寸），剩下的矩形里最高的是单元格。
            out.rects
                .iter()
                .filter(|rect| rect.height() < height - 40.0 && rect.height() > 30.0)
                .map(|rect| rect.height())
                .fold(0.0_f32, f32::max)
        };

        let small = tallest_cell(700.0);
        let large = tallest_cell(1100.0);
        assert!(small > 0.0, "没有找到单元格");
        assert!(
            large > small * 1.4,
            "窗口高度 700→1100 时单元格只从 {small:.0}px 变到 {large:.0}px，没有跟随缩放"
        );
    }

    /// 周视图的列高同样跟随窗口，而不是固定 320px。
    #[test]
    fn week_columns_fill_available_height() {
        let mut state = state_with_events();
        state.calendar.view = CalendarViewKind::Week;
        let mut commands = Vec::new();
        let out = run_frame_sized(&mut state, 1280.0, 700.0, |state, ui| {
            show(state, &mut commands, ui);
        });
        // 周视图里最高的单元格矩形应当接近可用高度（扣除工具栏与页边距）。
        let tallest = out
            .rects
            .iter()
            .map(|rect| rect.height())
            .fold(0.0_f32, f32::max);
        assert!(
            tallest > 500.0,
            "周视图列高只有 {tallest:.0}px，没有跟随窗口高度"
        );
    }

    #[test]
    fn editor_window_renders_without_panic() {
        let mut state = state_with_events();
        let event = sample_event(Some("FREQ=WEEKLY;INTERVAL=2"));
        state.calendar.editor = Some(CalendarEditor::from_event(&event));
        let mut commands = Vec::new();
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 900.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            show_editor(&mut state, &mut commands, ctx);
        });
        assert!(state.calendar.editor.is_some(), "未提交时应保留编辑器");
    }

    #[test]
    fn sidebar_renders_and_lists_calendars() {
        let mut state = state_with_events();
        let mut commands = Vec::new();
        let out = run_frame(&mut state, |state, ui| {
            sidebar(state, &mut commands, ui);
        });
        assert_eq!(state.calendar.calendars.len(), 1);
        assert!(out.has("我的日历"));
        assert!(out.has("全部日历"));
    }
}
