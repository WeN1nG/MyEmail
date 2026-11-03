//! 天气页：城市搜索、地点管理、当前天气、24 小时折线图与 7 天预报。
//!
//! 布局沿用音乐页的做法：侧栏放搜索与地点列表，主区自上而下三块
//! （当前天气 / 24 小时折线图 / 7 天预报），全部用绝对矩形定位——
//! CentralPanel 里嵌 `TopBottomPanel` 拿不到高度，`allocate_ui_with_layout`
//! 又会收缩父游标。
//!
//! 数据全部来自本地缓存（`weather_locations` + `weather_snapshots`），
//! 界面只发 `Command`，不直接联网。

use chrono::{DateTime, Local, NaiveDate};
use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, RichText, Sense, Stroke, Vec2};

use me_core::weather::{
    CurrentWeather, DailyForecast, HourlyForecast, SeriesPoint, WeatherKind, WeatherLocation,
    WeatherLocationCandidate, WeatherLocationId, WeatherSnapshot, hourly_series, hourly_temp_range,
};

use crate::state::UiState;

/// 主色。
const ACCENT: Color32 = Color32::from_rgb(59, 130, 246);
/// 地点行高。
const ROW_HEIGHT: f32 = 26.0;
/// 侧栏宽度低于此值时隐藏次要信息。
const NARROW_SIDEBAR: f32 = 210.0;
/// 折线描边宽度。
const CHART_STROKE: f32 = 2.0;
/// 折线填充透明度。
const CHART_FILL_ALPHA: u8 = 46;
/// 降水概率柱的颜色。
const POP_COLOR: Color32 = Color32::from_rgb(90, 150, 230);

/// 天气页状态（数据由 app 按脏标记重查）。
pub struct WeatherState {
    /// 已保存的地点。
    pub locations: Vec<WeatherLocation>,
    /// 当前选中的地点；`None` 表示跟随默认地点。
    pub selected: Option<WeatherLocationId>,
    /// 当前显示的快照。
    pub snapshot: Option<WeatherSnapshot>,
    /// 城市搜索输入。
    pub query: String,
    /// 搜索结果候选。
    pub candidates: Vec<WeatherLocationCandidate>,
    /// 当前搜索的请求号；只接受与它相同的结果。
    pub search_request_id: u64,
    /// 是否已配置 API Key。
    pub key_present: bool,
    /// 当前生效的天气 Host。
    pub api_host: String,
    /// 配置弹窗里 Host 输入框的草稿。
    ///
    /// 与生效值分离：用户在弹窗里敲到一半不应该影响正在使用的配置，
    /// 只有点「保存」才提交。
    pub host_input: HostInput,
    /// 连接测试的结果（`None` 表示还没测过）。
    pub test_result: Option<(bool, String)>,
    /// 连接测试进行中。
    pub testing: bool,
    /// 是否正在刷新。
    pub refreshing: bool,
    /// 上一次的失败提示。
    pub error: Option<String>,
    /// 数据需要重查。
    pub dirty: bool,
    /// 上次自动刷新尝试的时间（UTC 秒）；避免每帧重复触发。
    pub auto_refresh_at: i64,
    /// API Key 输入草稿（保存后立即清空，不留在状态里）。
    pub key_input: String,
}

/// 配置弹窗的 Host 输入草稿。
#[derive(Clone, Default)]
pub struct HostInput {
    pub api_host: String,
}

impl Default for WeatherState {
    fn default() -> Self {
        Self {
            locations: Vec::new(),
            selected: None,
            snapshot: None,
            query: String::new(),
            candidates: Vec::new(),
            search_request_id: 0,
            key_present: false,
            api_host: String::new(),
            host_input: HostInput::default(),
            test_result: None,
            testing: false,
            refreshing: false,
            error: None,
            dirty: true,
            auto_refresh_at: 0,
            key_input: String::new(),
        }
    }
}

impl WeatherState {
    /// 打开配置弹窗：把当前生效的 Host 灌进草稿。
    ///
    /// Host 为空时用官方公共域名兜底，这样用户只填 Key 也能先用起来。
    pub fn open_config(&mut self) {
        self.host_input.api_host = if self.api_host.trim().is_empty() {
            me_core::DEFAULT_API_HOST.to_string()
        } else {
            self.api_host.clone()
        };
        self.key_input.clear();
        // 重新打开时清掉上一次的测试结果。
        self.test_result = None;
    }

    /// 当前应显示的地点：优先选中的，否则默认地点。
    pub fn active_location(&self) -> Option<&WeatherLocation> {
        if let Some(id) = self.selected {
            return self.locations.iter().find(|item| item.id == id);
        }
        self.locations
            .iter()
            .find(|item| item.is_default)
            .or_else(|| self.locations.first())
    }
}

// ---------------------------------------------------------------------------
// 主区
// ---------------------------------------------------------------------------

/// 画右上角的「配置密钥」按钮，返回是否被点击。
///
/// 用 `Shape` 直接绘制而不是 `Button` 控件：这样按钮能精确贴在
/// 主区右上角，不受 egui 布局的间距与对齐影响。
fn config_button(ui: &mut egui::Ui, rect: Rect) -> bool {
    let response = ui.interact(rect, egui::Id::new("me_weather_config_btn"), Sense::click());
    let visuals = ui.visuals();
    let fill = if response.is_pointer_button_down_on() {
        visuals.widgets.active.weak_bg_fill
    } else if response.hovered() {
        visuals.widgets.hovered.weak_bg_fill
    } else if visuals.dark_mode {
        Color32::from_rgb(45, 52, 66)
    } else {
        Color32::from_rgb(232, 236, 243)
    };
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(4), fill);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "配置密钥",
        egui::FontId::proportional(13.0),
        visuals.strong_text_color(),
    );
    response.clicked()
}

/// 天气页主区。
pub fn show(state: &mut UiState, commands: &mut Vec<me_core::Command>, ui: &mut egui::Ui) {
    let available = ui.available_size();

    // 右上角常驻「配置密钥」按钮：无论配没配置都能改配置。
    // 先占一条整宽的横带，再用绝对矩形把按钮贴到横带右端。
    const CONFIG_BTN: egui::Vec2 = egui::vec2(92.0, 26.0);
    let (header_rect, _) =
        ui.allocate_exact_size(egui::vec2(available.x, CONFIG_BTN.y), Sense::hover());
    let button_rect = Rect::from_min_size(
        egui::pos2(header_rect.right() - CONFIG_BTN.x, header_rect.top()),
        CONFIG_BTN,
    );
    let clicked = config_button(ui, button_rect);
    if clicked {
        state.weather.open_config();
        state.dialog = crate::state::Dialog::WeatherConfig;
    }

    // 内容区 = 标题带下方剩余空间。必须扣掉标题带自身的高度，
    // 否则区域会超出面板底边，底部那行（数据来源 / 刷新）会被裁掉。
    let content_height = (available.y - CONFIG_BTN.y - 8.0).max(0.0);
    let area = Rect::from_min_max(
        egui::pos2(header_rect.left(), header_rect.bottom() + 8.0),
        egui::pos2(
            header_rect.right(),
            header_rect.bottom() + 8.0 + content_height,
        ),
    );

    if !state.weather.key_present {
        setup_guide(state, commands, ui, area);
        return;
    }

    // 三块：当前天气 / 24 小时 / 7 天。
    let gap = 10.0;
    let total = area.height();
    let current_height = (total * 0.32).clamp(140.0, 240.0);
    let hourly_height = (total * 0.34).clamp(150.0, 280.0);
    let daily_height = (total - current_height - hourly_height - gap * 2.0).max(110.0);

    let current_area = Rect::from_min_size(area.min, egui::vec2(area.width(), current_height));
    let hourly_area = Rect::from_min_max(
        egui::pos2(area.left(), current_area.bottom() + gap),
        egui::pos2(area.right(), current_area.bottom() + gap + hourly_height),
    );
    let daily_area = Rect::from_min_max(
        egui::pos2(area.left(), hourly_area.bottom() + gap),
        egui::pos2(area.right(), hourly_area.bottom() + gap + daily_height),
    );

    match state.weather.snapshot.clone() {
        Some(snapshot) => {
            let timezone = state
                .weather
                .active_location()
                .map(|location| location.timezone.clone())
                .unwrap_or_default();
            current_card(ui, current_area, &snapshot.current);
            hourly_card(ui, hourly_area, &snapshot, &timezone);
            daily_card(ui, daily_area, &snapshot, &timezone);
        }
        None => {
            ui.scope_builder(egui::UiBuilder::new().max_rect(current_area), |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(20.0);
                    ui.label(RichText::new("暂无天气数据").size(18.0));
                    ui.label(
                        RichText::new("点击下方「刷新」获取最新天气")
                            .size(13.0)
                            .color(Color32::GRAY),
                    );
                });
            });
        }
    }

    footer(state, commands, ui, area);
}

/// 未配置 Key 时的引导。
///
/// 引导里的「配置密钥」按钮与右上角那个打开**同一个弹窗**，
/// 用户不需要知道配置放在设置页还是别处。
fn setup_guide(
    state: &mut UiState,
    commands: &mut Vec<me_core::Command>,
    ui: &mut egui::Ui,
    area: Rect,
) {
    let _ = commands;
    ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.label(RichText::new("还没有配置和风天气").size(20.0));
            ui.add_space(6.0);
            ui.label(
                RichText::new("天气数据来自和风天气（QWeather），需要先申请一个 API Key。")
                    .size(13.0)
                    .color(Color32::GRAY),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new("1. 登录 console.qweather.com 创建项目，在「凭据」里生成 API KEY")
                    .size(13.0)
                    .color(Color32::GRAY),
            );
            ui.label(
                RichText::new("2. 在控制台「设置」页复制你的专属 API Host")
                    .size(13.0)
                    .color(Color32::GRAY),
            );
            ui.add_space(12.0);
            if ui.button(RichText::new("配置密钥").size(14.0)).clicked() {
                state.weather.open_config();
                state.dialog = crate::state::Dialog::WeatherConfig;
            }
        });
    });
}

/// 底部一行：数据来源、缓存时间、刷新按钮。
fn footer(
    state: &mut UiState,
    commands: &mut Vec<me_core::Command>,
    ui: &mut egui::Ui,
    area: Rect,
) {
    let height = 26.0;
    let rect = Rect::from_min_max(
        egui::pos2(area.left(), area.bottom() - height),
        egui::pos2(area.right(), area.bottom()),
    );
    let snapshot = state.weather.snapshot.clone();
    let error = state.weather.error.clone();
    let refreshing = state.weather.refreshing;

    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
        ui.horizontal(|ui| {
            if let Some(snapshot) = &snapshot {
                let fetched = format_clock(snapshot.fetched_at);
                ui.label(
                    RichText::new(format!("数据来源：和风天气 QWeather · 更新于 {fetched}"))
                        .size(12.0)
                        .color(Color32::GRAY),
                );
            } else {
                ui.label(
                    RichText::new("数据来源：和风天气 QWeather")
                        .size(12.0)
                        .color(Color32::GRAY),
                );
            }
            if let Some(error) = &error {
                ui.label(
                    RichText::new(error)
                        .size(12.0)
                        .color(Color32::from_rgb(210, 90, 90)),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if refreshing { "刷新中…" } else { "刷新" };
                if ui
                    .add_enabled(
                        !refreshing,
                        egui::Button::new(RichText::new(label).size(13.0)),
                    )
                    .clicked()
                {
                    commands.push(me_core::Command::RefreshWeather { force: true });
                    state.weather.refreshing = true;
                    state.weather.error = None;
                }
            });
        });
    });
}

/// 当前天气卡片。
fn current_card(ui: &mut egui::Ui, area: Rect, current: &CurrentWeather) {
    let visuals = ui.visuals().clone();
    ui.painter()
        .rect_filled(area, CornerRadius::same(12), visuals.faint_bg_color);
    let inner = area.shrink(18.0);

    let icon_size = inner.height().min(96.0);
    let icon_rect = Rect::from_min_size(
        egui::pos2(inner.left(), inner.center().y - icon_size / 2.0),
        egui::vec2(icon_size, icon_size),
    );
    paint_icon(ui, icon_rect, current.kind, icon_size);

    let text_left = icon_rect.right() + 22.0;
    if inner.right() - text_left < 120.0 {
        return;
    }
    let column = Rect::from_min_max(
        egui::pos2(text_left, inner.top()),
        egui::pos2(inner.right(), inner.bottom()),
    );
    ui.scope_builder(egui::UiBuilder::new().max_rect(column), |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{:.0}°", current.temp_c))
                    .size(46.0)
                    .strong(),
            );
            ui.label(
                RichText::new(condition_text(current))
                    .size(18.0)
                    .color(Color32::GRAY),
            );
        });
        ui.label(
            RichText::new(format!("体感 {:.0}°", current.feels_like_c))
                .size(13.0)
                .color(Color32::GRAY),
        );
        ui.label(
            RichText::new(format!(
                "湿度 {}% · {} · 降水 {:.1} mm",
                current.humidity_percent,
                me_core::weather::format_wind(current.wind_scale, current.wind_speed_kmh),
                current.precip_mm
            ))
            .size(13.0)
            .color(Color32::GRAY),
        );
        ui.label(
            RichText::new(format!(
                "气压 {:.0} hPa · 能见度 {:.0} km · 云量 {}%",
                current.pressure_hpa, current.visibility_km, current.cloud_percent
            ))
            .size(13.0)
            .color(Color32::GRAY),
        );
    });
}

/// 24 小时折线图。
fn hourly_card(ui: &mut egui::Ui, area: Rect, snapshot: &WeatherSnapshot, timezone: &str) {
    let visuals = ui.visuals().clone();
    ui.painter()
        .rect_filled(area, CornerRadius::same(12), visuals.faint_bg_color);
    let inner = area.shrink(18.0);

    ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
        ui.label(RichText::new("24 小时预报").size(15.0).strong());
    });
    let title_bottom = inner.top() + 22.0;
    let chart_area = Rect::from_min_max(
        egui::pos2(inner.left(), title_bottom),
        egui::pos2(inner.right(), inner.bottom() - 20.0),
    );
    if chart_area.height() < 40.0 || snapshot.hourly.len() < 2 {
        return;
    }

    let points = hourly_series(&snapshot.hourly, 24);
    let positions = chart_positions(chart_area, &points);
    if positions.len() < 2 {
        return;
    }
    let painter = ui.painter();

    // 网格线。
    for line in grid_lines(chart_area, 3) {
        painter.rect_filled(line, CornerRadius::ZERO, Color32::from_gray(60));
    }

    // 降水概率：小竖条画在图表底部。
    let pop_height = chart_area.height() * 0.22;
    for (index, hour) in snapshot.hourly.iter().take(positions.len()).enumerate() {
        if hour.pop_percent == 0 {
            continue;
        }
        let x = positions[index].x;
        let height = pop_height * (hour.pop_percent as f32 / 100.0);
        painter.rect_filled(
            Rect::from_min_max(
                Pos2::new(x - 1.5, chart_area.bottom() - height),
                Pos2::new(x + 1.5, chart_area.bottom()),
            ),
            CornerRadius::same(1),
            POP_COLOR,
        );
    }

    // 折线下方填充。
    let mut fill = positions.clone();
    fill.push(Pos2::new(
        positions[positions.len() - 1].x,
        chart_area.bottom(),
    ));
    fill.push(Pos2::new(positions[0].x, chart_area.bottom()));
    painter.add(egui::Shape::convex_polygon(
        fill,
        ACCENT.gamma_multiply(CHART_FILL_ALPHA as f32 / 255.0),
        Stroke::NONE,
    ));

    // 折线本身。
    for pair in positions.windows(2) {
        painter.line_segment([pair[0], pair[1]], Stroke::new(CHART_STROKE, ACCENT));
    }
    // 端点圆点。
    for position in &positions {
        painter.circle_filled(*position, 2.5, ACCENT);
    }

    // 最高/最低温标注：用真实数值，读者才知道折线的量程。
    if let Some((min, max)) = chart_range(&snapshot.hourly) {
        painter.text(
            Pos2::new(chart_area.left(), chart_area.top() - 2.0),
            egui::Align2::LEFT_BOTTOM,
            format!("最高 {:.0}°", max),
            egui::FontId::proportional(11.0),
            ui.visuals().weak_text_color(),
        );
        painter.text(
            Pos2::new(chart_area.right(), chart_area.top() - 2.0),
            egui::Align2::RIGHT_BOTTOM,
            format!("最低 {:.0}°", min),
            egui::FontId::proportional(11.0),
            ui.visuals().weak_text_color(),
        );
    }

    // 时间轴：首、中、尾三个标签，避免挤成一团。
    let labels = hour_labels(&snapshot.hourly, timezone);
    if !labels.is_empty() {
        let indices = [0usize, labels.len() / 2, labels.len().saturating_sub(1)];
        for index in indices {
            let Some(label) = labels.get(index) else {
                continue;
            };
            let x = chart_area.left() + chart_area.width() * index as f32 / labels.len() as f32;
            painter.text(
                Pos2::new(x, chart_area.bottom() + 4.0),
                egui::Align2::LEFT_CENTER,
                label,
                egui::FontId::proportional(11.0),
                ui.visuals().weak_text_color(),
            );
        }
    }
}

/// 7 天预报列表。
fn daily_card(ui: &mut egui::Ui, area: Rect, snapshot: &WeatherSnapshot, timezone: &str) {
    let visuals = ui.visuals().clone();
    ui.painter()
        .rect_filled(area, CornerRadius::same(12), visuals.faint_bg_color);
    let inner = area.shrink(18.0);

    ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
        ui.label(RichText::new("7 天预报").size(15.0).strong());
    });
    let title_bottom = inner.top() + 22.0;
    let rows_area = Rect::from_min_max(
        egui::pos2(inner.left(), title_bottom),
        egui::pos2(inner.right(), inner.bottom()),
    );
    let row_height = ((rows_area.height()) / 7.0).clamp(18.0, 30.0);
    let labels = day_labels(&snapshot.daily, timezone);

    for (index, day) in snapshot.daily.iter().enumerate() {
        let top = rows_area.top() + index as f32 * row_height;
        if top + row_height > rows_area.bottom() + 1.0 {
            break;
        }
        let row = Rect::from_min_max(
            Pos2::new(rows_area.left(), top),
            Pos2::new(rows_area.right(), top + row_height),
        );
        daily_row(
            ui,
            row,
            day,
            labels.get(index).map(String::as_str).unwrap_or(""),
        );
    }
}

/// 7 天预报的一行。
fn daily_row(ui: &mut egui::Ui, row: Rect, day: &DailyForecast, label: &str) {
    let painter = ui.painter();
    let baseline = row.center().y;
    let icon_size = row.height().min(22.0);
    let icon_rect = Rect::from_min_size(
        Pos2::new(row.left() + 4.0, baseline - icon_size / 2.0),
        egui::vec2(icon_size, icon_size),
    );
    paint_icon(ui, icon_rect, day.kind, icon_size);

    let text_left = icon_rect.right() + 10.0;
    let text = format!(
        "{label}  {}  {:.0}° / {:.0}°",
        day.text, day.temp_max_c, day.temp_min_c
    );
    painter.text(
        Pos2::new(text_left, baseline),
        egui::Align2::LEFT_CENTER,
        text,
        egui::FontId::proportional(13.0),
        ui.visuals().strong_text_color(),
    );

    let mut right = format!("降水 {:.1} mm", day.precip_mm);
    if let (Some(sunrise), Some(sunset)) = (&day.sunrise, &day.sunset) {
        right.push_str(&format!(" · 日出 {sunrise} / 日落 {sunset}"));
    }
    painter.text(
        Pos2::new(row.right() - 4.0, baseline),
        egui::Align2::RIGHT_CENTER,
        right,
        egui::FontId::proportional(12.0),
        ui.visuals().weak_text_color(),
    );
}

// ---------------------------------------------------------------------------
// 侧栏
// ---------------------------------------------------------------------------

/// 侧栏：搜索框 + 候选列表 + 地点列表。
pub fn sidebar(state: &mut UiState, commands: &mut Vec<me_core::Command>, ui: &mut egui::Ui) {
    ui.label(RichText::new("天气").size(16.0).strong());
    ui.add_space(4.0);

    // 搜索框。
    let mut query = state.weather.query.clone();
    let response = ui.add(
        egui::TextEdit::singleline(&mut query)
            .hint_text("搜索城市，如「北京」")
            .desired_width(ui.available_width()),
    );
    if response.changed() {
        state.weather.query = query;
    }
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                !state.weather.query.trim().is_empty(),
                egui::Button::new("搜索"),
            )
            .clicked()
        {
            state.weather.search_request_id += 1;
            state.weather.candidates.clear();
            commands.push(me_core::Command::SearchWeatherLocations {
                request_id: state.weather.search_request_id,
                query: state.weather.query.trim().to_string(),
            });
        }
        if ui.button("刷新").clicked() {
            commands.push(me_core::Command::RefreshWeather { force: true });
            state.weather.refreshing = true;
        }
    });

    // 候选列表。
    if !state.weather.candidates.is_empty() {
        ui.add_space(4.0);
        ui.label(RichText::new("搜索结果").size(12.0).color(Color32::GRAY));
        for candidate in state.weather.candidates.clone() {
            if ui
                .add_sized(
                    [ui.available_width(), ROW_HEIGHT],
                    egui::Label::new(RichText::new(candidate.title())).truncate(),
                )
                .clicked()
            {
                commands.push(me_core::Command::AddWeatherLocation { candidate });
                state.weather.candidates.clear();
            }
        }
        ui.separator();
    }

    // 地点列表。
    ui.label(RichText::new("我的地点").size(12.0).color(Color32::GRAY));
    if state.weather.locations.is_empty() {
        ui.label(
            RichText::new("还没有添加地点，搜索一个城市吧")
                .size(12.0)
                .color(Color32::GRAY),
        );
        return;
    }
    let narrow = ui.available_width() < NARROW_SIDEBAR;
    let active = state.weather.active_location().map(|item| item.id);
    let locations = state.weather.locations.clone();
    for location in locations {
        let is_active = active == Some(location.id);
        let mut text = RichText::new(location.name.clone());
        if is_active {
            text = text.color(ACCENT).strong();
        }
        ui.horizontal(|ui| {
            let (marker, _) =
                ui.allocate_exact_size(egui::vec2(5.0, ROW_HEIGHT), egui::Sense::hover());
            if is_active {
                ui.painter().rect_filled(
                    Rect::from_min_max(
                        Pos2::new(marker.left(), marker.center().y - 7.0),
                        Pos2::new(marker.left() + 2.5, marker.center().y + 7.0),
                    ),
                    CornerRadius::same(1),
                    ACCENT,
                );
            }
            if ui
                .add_sized(
                    [ui.available_width(), ROW_HEIGHT],
                    egui::Label::new(text).truncate(),
                )
                .clicked()
            {
                state.weather.selected = Some(location.id);
                state.weather.dirty = true;
            }
        });
        if !narrow {
            ui.label(
                RichText::new(if location.is_default {
                    "默认地点（自动刷新）"
                } else {
                    "点击设为默认"
                })
                .size(11.0)
                .color(Color32::GRAY),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 纯函数（便于测试）
// ---------------------------------------------------------------------------

/// 折线图采样点 → 屏幕坐标。
pub fn chart_positions(area: Rect, points: &[SeriesPoint]) -> Vec<Pos2> {
    points
        .iter()
        .map(|point| {
            Pos2::new(
                area.left() + point.x * area.width(),
                area.bottom() - point.y * area.height(),
            )
        })
        .collect()
}

/// 逐时预报在地点时区下的小时标签。
pub fn hour_labels(hourly: &[HourlyForecast], timezone: &str) -> Vec<String> {
    hourly
        .iter()
        .map(|hour| format_hour(timezone, hour.time))
        .collect()
}

/// 逐日预报的日期标签（`MM-DD`）。
pub fn day_labels(daily: &[DailyForecast], timezone: &str) -> Vec<String> {
    daily
        .iter()
        .map(|day| {
            let raw = day.date.clone();
            let Some(date) = parse_date(&raw) else {
                return raw;
            };
            match timezone.parse::<chrono_tz::Tz>() {
                Ok(tz) => date
                    .and_hms_opt(12, 0, 0)
                    .and_then(|moment| moment.and_local_timezone(tz).single())
                    .map(|value| value.format("%m-%d").to_string())
                    .unwrap_or(raw),
                Err(_) => raw,
            }
        })
        .collect()
}

/// UTC 秒 → 地点时区的 `HH:MM`。
pub fn format_hour(timezone: &str, unix: i64) -> String {
    let Some(moment) = DateTime::from_timestamp(unix, 0) else {
        return String::new();
    };
    match timezone.parse::<chrono_tz::Tz>() {
        Ok(tz) => moment.with_timezone(&tz).format("%H:%M").to_string(),
        Err(_) => moment.with_timezone(&Local).format("%H:%M").to_string(),
    }
}

/// UTC 秒 → `HH:MM`（本地时区）。
fn format_clock(unix: i64) -> String {
    DateTime::from_timestamp(unix, 0)
        .map(|value| value.with_timezone(&Local).format("%H:%M").to_string())
        .unwrap_or_default()
}

/// `YYYY-MM-DD` → `NaiveDate`。
fn parse_date(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()
}

/// 天气类型的中文说明（优先用服务商给的文字）。
pub fn condition_text(current: &CurrentWeather) -> String {
    if current.text.trim().is_empty() {
        current.kind.label().to_string()
    } else {
        current.text.clone()
    }
}

/// 折线图的数值范围；温差过小时留出余量，避免贴边。
pub fn chart_range(hourly: &[HourlyForecast]) -> Option<(f64, f64)> {
    hourly_temp_range(hourly).map(|(min, max)| {
        if (max - min).abs() < 1.0 {
            (min - 1.0, max + 1.0)
        } else {
            (min, max)
        }
    })
}

/// 水平网格线。
pub fn grid_lines(area: Rect, count: usize) -> Vec<Rect> {
    (0..count)
        .map(|index| {
            let ratio = (index + 1) as f32 / (count + 1) as f32;
            let y = area.bottom() - area.height() * ratio;
            Rect::from_min_max(Pos2::new(area.left(), y), Pos2::new(area.right(), y))
        })
        .collect()
}

/// 天气种类对应的颜色（用于折线与图标）。
pub fn kind_color(kind: WeatherKind) -> Color32 {
    match kind {
        WeatherKind::Clear => Color32::from_rgb(250, 200, 60),
        WeatherKind::PartlyCloudy => Color32::from_rgb(150, 180, 230),
        WeatherKind::Cloudy => Color32::from_rgb(160, 170, 185),
        WeatherKind::Fog | WeatherKind::Haze | WeatherKind::Dust => {
            Color32::from_rgb(170, 170, 160)
        }
        WeatherKind::Rain | WeatherKind::Drizzle => Color32::from_rgb(90, 150, 230),
        WeatherKind::Sleet => Color32::from_rgb(140, 170, 210),
        WeatherKind::Snow => Color32::from_rgb(200, 220, 245),
        WeatherKind::Thunderstorm => Color32::from_rgb(150, 120, 240),
        WeatherKind::Hail => Color32::from_rgb(170, 200, 240),
        WeatherKind::Unknown => Color32::GRAY,
    }
}

/// 画一个天气图标（用图形而不是字符：字体子集不一定收录符号）。
pub fn paint_icon(ui: &egui::Ui, rect: Rect, kind: WeatherKind, size: f32) {
    let painter = ui.painter();
    let color = kind_color(kind);
    let sun = matches!(kind, WeatherKind::Clear | WeatherKind::PartlyCloudy);

    if sun {
        let center = Pos2::new(rect.left() + size * 0.32, rect.bottom() - size * 0.66);
        painter.circle_filled(center, size * 0.15, color);
        for index in 0..8 {
            let angle = index as f32 * std::f32::consts::TAU / 8.0;
            let start = center + Vec2::new(angle.cos(), angle.sin()) * size * 0.21;
            let end = center + Vec2::new(angle.cos(), angle.sin()) * size * 0.30;
            painter.line_segment([start, end], Stroke::new(1.4_f32, color));
        }
    }

    // 云：几段折线拼成的轮廓，避免用字符。
    let base = rect.bottom() - size * 0.22;
    let cloud = vec![
        Pos2::new(rect.left() + size * 0.20, base),
        Pos2::new(rect.left() + size * 0.28, base - size * 0.16),
        Pos2::new(rect.left() + size * 0.46, base - size * 0.26),
        Pos2::new(rect.left() + size * 0.66, base - size * 0.18),
        Pos2::new(rect.left() + size * 0.80, base - size * 0.04),
        Pos2::new(rect.left() + size * 0.80, base),
    ];
    painter.add(egui::Shape::convex_polygon(
        cloud,
        if sun {
            Color32::from_rgb(205, 215, 230)
        } else {
            color
        },
        Stroke::NONE,
    ));

    match kind {
        WeatherKind::Rain | WeatherKind::Drizzle => {
            for index in 0..3 {
                let x = rect.left() + size * (0.34 + 0.16 * index as f32);
                painter.line_segment(
                    [
                        Pos2::new(x, base),
                        Pos2::new(x - size * 0.05, base + size * 0.16),
                    ],
                    Stroke::new(1.5_f32, color),
                );
            }
        }
        WeatherKind::Snow => {
            for index in 0..3 {
                let x = rect.left() + size * (0.34 + 0.16 * index as f32);
                painter.circle_filled(Pos2::new(x, base + size * 0.10), size * 0.045, color);
            }
        }
        WeatherKind::Thunderstorm => {
            // 闪电：折线要分段画，`line_segment` 只接受两个点。
            let points = [
                Pos2::new(rect.left() + size * 0.52, base),
                Pos2::new(rect.left() + size * 0.44, base + size * 0.12),
                Pos2::new(rect.left() + size * 0.52, base + size * 0.12),
                Pos2::new(rect.left() + size * 0.46, base + size * 0.24),
            ];
            for pair in points.windows(2) {
                painter.line_segment([pair[0], pair[1]], Stroke::new(1.8_f32, color));
            }
        }
        WeatherKind::Fog | WeatherKind::Haze | WeatherKind::Dust => {
            for index in 0..3 {
                let y = base + size * (0.04 + 0.07 * index as f32);
                painter.line_segment(
                    [
                        Pos2::new(rect.left() + size * 0.24, y),
                        Pos2::new(rect.left() + size * 0.76, y),
                    ],
                    Stroke::new(1.2_f32, color),
                );
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_core::weather::WeatherKind;

    fn hour(time: i64, temp: f64) -> HourlyForecast {
        HourlyForecast {
            time,
            temp_c: temp,
            pop_percent: 20,
            precip_mm: 0.0,
            kind: WeatherKind::Cloudy,
            text: "阴".into(),
        }
    }

    /// 跑一帧，收集绘制出来的文本与线段。
    fn run_frame(
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
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| draw(state, ui));
        });
        let mut out = FrameOutput::default();
        for clipped in &output.shapes {
            collect(&clipped.shape, &mut out);
        }
        out
    }

    #[derive(Default)]
    struct FrameOutput {
        texts: Vec<String>,
        lines: usize,
    }

    impl FrameOutput {
        fn has(&self, needle: &str) -> bool {
            self.texts.iter().any(|text| text.contains(needle))
        }
    }

    fn collect(shape: &egui::Shape, out: &mut FrameOutput) {
        match shape {
            egui::Shape::Text(text) => out.texts.push(text.galley.text().to_string()),
            egui::Shape::LineSegment { .. } => out.lines += 1,
            egui::Shape::Path(path) => out.lines += path.points.len().saturating_sub(1),
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect(shape, out);
                }
            }
            _ => {}
        }
    }

    fn state_with_snapshot() -> UiState {
        let mut state = UiState {
            page: crate::state::Page::Weather,
            ..Default::default()
        };
        state.weather.key_present = true;
        state.weather.api_host = "https://devapi.qweather.com".to_string();
        state.weather.locations = vec![WeatherLocation {
            id: WeatherLocationId(1),
            provider: "qweather".to_string(),
            provider_location_id: "101010100".to_string(),
            name: "北京".to_string(),
            admin1: Some("北京市".to_string()),
            admin2: None,
            country: Some("中国".to_string()),
            latitude: 39.9,
            longitude: 116.4,
            timezone: "Asia/Shanghai".to_string(),
            is_default: true,
            sort_order: 0,
            created_at: 0,
        }];
        state.weather.snapshot = Some(WeatherSnapshot {
            provider: "qweather".to_string(),
            fetched_at: 1_700_000_000,
            expires_at: 1_700_001_800,
            current: CurrentWeather {
                observed_at: 1_700_000_000,
                temp_c: 18.5,
                feels_like_c: 17.0,
                humidity_percent: 55,
                wind_scale: 2,
                wind_dir: "北风".to_string(),
                wind_speed_kmh: 8.0,
                precip_mm: 0.0,
                pressure_hpa: 1015.0,
                visibility_km: 20.0,
                cloud_percent: 30,
                kind: WeatherKind::PartlyCloudy,
                text: "多云".to_string(),
            },
            hourly: (0..24)
                .map(|i| HourlyForecast {
                    time: 1_700_000_000 + i * 3600,
                    temp_c: 18.0 + i as f64 * 0.5,
                    pop_percent: 20,
                    precip_mm: 0.0,
                    kind: WeatherKind::PartlyCloudy,
                    text: "多云".to_string(),
                })
                .collect(),
            daily: vec![DailyForecast {
                date: "2026-11-04".to_string(),
                sunrise: Some("06:30".to_string()),
                sunset: Some("17:12".to_string()),
                temp_max_c: 22.0,
                temp_min_c: 11.0,
                precip_mm: 0.0,
                kind: WeatherKind::Clear,
                text: "晴".to_string(),
            }],
        });
        state
    }

    #[test]
    fn without_key_the_page_shows_setup_guide() {
        let mut state = UiState {
            page: crate::state::Page::Weather,
            ..Default::default()
        };
        let mut commands = Vec::new();
        let out = run_frame(&mut state, 1000.0, 700.0, |state, ui| {
            show(state, &mut commands, ui);
        });
        assert!(out.has("还没有配置和风天气"), "应显示配置引导");
        assert!(out.has("API Key"), "应说明需要 Key");
    }

    /// 右上角必须常驻「配置密钥」按钮：配置过也要能改。
    #[test]
    fn config_button_is_always_in_the_top_right() {
        for configured in [false, true] {
            let mut state = state_with_snapshot();
            state.weather.key_present = configured;
            let mut commands = Vec::new();
            let out = run_frame(&mut state, 1200.0, 800.0, |state, ui| {
                show(state, &mut commands, ui);
            });
            assert!(
                out.has("配置密钥"),
                "configured={configured} 时右上角都应有「配置密钥」按钮"
            );
        }
    }

    /// 未配置时引导页也要有「配置密钥」按钮，且与右上角是同一个入口。
    #[test]
    fn setup_guide_offers_the_same_config_entry() {
        let mut state = UiState {
            page: crate::state::Page::Weather,
            ..Default::default()
        };
        let mut commands = Vec::new();
        let out = run_frame(&mut state, 1000.0, 700.0, |state, ui| {
            show(state, &mut commands, ui);
        });
        assert!(out.has("配置密钥"), "未配置时引导页应提供「配置密钥」按钮");
        assert!(
            !out.has("去设置"),
            "配置入口已迁到弹窗，不该再引导用户去设置页"
        );
    }

    /// 打开配置弹窗时要把生效的 Host 灌进草稿；没配过则用官方默认域名兜底。
    #[test]
    fn opening_config_fills_the_host_draft() {
        let mut state = UiState::default();
        state.weather.open_config();
        assert_eq!(state.weather.host_input.api_host, me_core::DEFAULT_API_HOST);

        state.weather.api_host = "https://abc.qweatherapi.com".to_string();
        state.weather.open_config();
        assert_eq!(
            state.weather.host_input.api_host,
            "https://abc.qweatherapi.com"
        );
    }

    #[test]
    fn page_renders_conditions_chart_and_forecast() {
        let mut state = state_with_snapshot();
        let mut commands = Vec::new();
        let out = run_frame(&mut state, 1200.0, 800.0, |state, ui| {
            show(state, &mut commands, ui);
        });
        assert!(out.has("24 小时预报"), "应有逐时区块");
        assert!(out.has("7 天预报"), "应有逐日区块");
        assert!(out.has("多云"), "应显示天气描述");
        assert!(out.has("和风天气"), "应标注数据来源");
        assert!(out.has("更新于"), "应显示缓存时间");
        // 折线：24 个采样点之间有 23 段线。
        assert!(out.lines >= 23, "应画出折线，实际线段 {}", out.lines);
    }

    #[test]
    fn sidebar_lists_locations_and_offers_search() {
        let mut state = state_with_snapshot();
        let mut commands = Vec::new();
        let out = run_frame(&mut state, 1200.0, 800.0, |state, ui| {
            sidebar(state, &mut commands, ui);
        });
        assert!(out.has("北京"), "侧栏应列出地点");
        assert!(out.has("搜索城市"), "侧栏应有搜索框");
    }

    #[test]
    fn chart_positions_map_to_area() {
        let area = Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(200.0, 100.0));
        let points = vec![
            SeriesPoint {
                x: 0.0,
                y: 0.0,
                temp_c: 10.0,
                time: 0,
                kind: WeatherKind::Cloudy,
                pop_percent: 0,
            },
            SeriesPoint {
                x: 1.0,
                y: 1.0,
                temp_c: 20.0,
                time: 3600,
                kind: WeatherKind::Cloudy,
                pop_percent: 0,
            },
            SeriesPoint {
                x: 0.5,
                y: 0.5,
                temp_c: 15.0,
                time: 1800,
                kind: WeatherKind::Cloudy,
                pop_percent: 0,
            },
        ];
        let positions = chart_positions(area, &points);
        assert_eq!(positions.len(), 3);
        assert_eq!(positions[0], Pos2::new(10.0, 120.0));
        assert_eq!(positions[1], Pos2::new(210.0, 20.0));
        assert_eq!(positions[2], Pos2::new(110.0, 70.0));
        assert!(
            positions.iter().all(|p| {
                p.x >= area.left()
                    && p.x <= area.right()
                    && p.y >= area.top()
                    && p.y <= area.bottom()
            }),
            "采样点必须落在图表区域内：{positions:?}"
        );
    }

    #[test]
    fn hour_labels_use_the_location_timezone() {
        let hours = vec![hour(1_700_000_000, 20.0), hour(1_700_003_600, 21.0)];
        let labels = hour_labels(&hours, "Asia/Shanghai");
        assert_eq!(labels.len(), 2);
        // 相邻两小时相差 1 小时。
        let first: i64 = labels[0].replace(':', "").parse().unwrap();
        let second: i64 = labels[1].replace(':', "").parse().unwrap();
        assert!(
            (second - first).abs() <= 100,
            "相邻标签应相差一小时：{labels:?}"
        );
        assert!(labels[0].contains(':'));
    }

    #[test]
    fn day_labels_are_month_day() {
        let days = vec![DailyForecast {
            date: "2026-11-04".into(),
            sunrise: Some("06:30".into()),
            sunset: Some("17:12".into()),
            temp_max_c: 22.0,
            temp_min_c: 11.0,
            precip_mm: 0.0,
            kind: WeatherKind::Clear,
            text: "晴".into(),
        }];
        let labels = day_labels(&days, "Asia/Shanghai");
        assert_eq!(labels.len(), 1);
        assert!(labels[0].ends_with("04"), "实际 {}", labels[0]);
    }

    #[test]
    fn chart_range_keeps_padding_for_flat_series() {
        let flat = vec![hour(0, 20.0), hour(3600, 20.0)];
        let (min, max) = chart_range(&flat).unwrap();
        assert!(max - min >= 1.9, "温差过小也要留出余量：{min}..{max}");
        let varied = vec![hour(0, 10.0), hour(3600, 20.0)];
        let (min, max) = chart_range(&varied).unwrap();
        assert_eq!(min, 10.0);
        assert_eq!(max, 20.0);
    }

    #[test]
    fn condition_falls_back_to_kind_label() {
        let mut current = CurrentWeather {
            observed_at: 0,
            temp_c: 20.0,
            feels_like_c: 20.0,
            humidity_percent: 50,
            wind_scale: 1,
            wind_dir: String::new(),
            wind_speed_kmh: 1.0,
            precip_mm: 0.0,
            pressure_hpa: 1010.0,
            visibility_km: 10.0,
            cloud_percent: 10,
            kind: WeatherKind::Snow,
            text: "  ".into(),
        };
        assert_eq!(condition_text(&current), "雪");
        current.text = "大雪".into();
        assert_eq!(condition_text(&current), "大雪");
    }

    #[test]
    fn grid_lines_are_inside_the_area() {
        let area = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(200.0, 100.0));
        let lines = grid_lines(area, 3);
        assert_eq!(lines.len(), 3);
        for line in lines {
            assert!(area.contains_rect(line), "网格线越界：{line:?}");
        }
    }
}
