//! 天气页：城市搜索、地点管理、当前天气、24 小时折线图与 7 天预报。
//!
//! 布局沿用音乐页的做法：侧栏放搜索与地点列表，主区自上而下三块
//! （当前天气 / 24 小时折线图 / 7 天预报），全部用绝对矩形定位——
//! CentralPanel 里嵌 `TopBottomPanel` 拿不到高度，`allocate_ui_with_layout`
//! 又会收缩父游标。
//!
//! 数据全部来自本地缓存（`weather_locations` + `weather_snapshots`），
//! 界面只发 `Command`，不直接联网。

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Local, NaiveDate};
use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, RichText, Sense, Stroke, Vec2};

use me_core::weather::{
    CurrentWeather, DailyForecast, HourlyForecast, SeriesPoint, WeatherKind, WeatherLocation,
    WeatherLocationCandidate, WeatherLocationId, WeatherSnapshot, format_humidity, format_precip,
    format_temp, format_wind, hourly_series, hourly_temp_range,
};

use crate::state::UiState;

/// 主色。
const ACCENT: Color32 = Color32::from_rgb(59, 130, 246);
/// 地点行高。
const ROW_HEIGHT: f32 = 26.0;
/// 地点行右侧「设置默认」按钮预留的宽度。
///
/// 侧栏默认宽 220px，可用宽度只剩约 204px——放不下两个按钮加名字。
/// 所以按钮用小字号，行内固定占位，名字按剩余宽度截断。
const ACTION_BTN_WIDTH: f32 = 58.0;
/// 折线描边宽度。
const CHART_STROKE: f32 = 2.0;
/// 折线填充透明度。
const CHART_FILL_ALPHA: u8 = 46;
/// 折线下方填充的颜色。
///
/// 手工拼 Mesh 填充时，顶点颜色必须是预乘 alpha 的 sRGBA。
/// `Color32` 用 gamma 空间存色，所以这里按 `r * a / 255` 预乘。
const CHART_FILL_COLOR: Color32 = Color32::from_rgba_premultiplied(11, 23, 44, CHART_FILL_ALPHA);
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
    /// 当前显示的快照属于哪个地点。
    ///
    /// 切换地点后快照会换，但两者之间有间隙；记下归属才能判断
    /// 「手上这份快照是不是当前地点的」，否则会用上一个城市的缓存
    /// 去决定当前城市要不要刷新。
    pub snapshot_location: Option<WeatherLocationId>,
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
    /// 用户刚点过「保存」，等待后端确认。
    ///
    /// 启动时读回配置也会发 `WeatherConfigChanged`，用它区分两者，
    /// 免得每次启动都弹「配置已保存」。
    pub save_requested: bool,
    /// 正在刷新的地点集合。
    ///
    /// 按地点分别记：刷新 A 时切到 B，B 不该显示「刷新中」。
    pub refreshing: HashSet<WeatherLocationId>,
    /// 上一次的失败提示。
    pub error: Option<String>,
    /// 失败提示属于哪个地点；`None` 表示与地点无关的全局失败。
    pub error_location: Option<WeatherLocationId>,
    /// 数据需要重查。
    pub dirty: bool,
    /// 每个地点上次自动刷新尝试的时间（UTC 秒）；避免每帧重复触发。
    pub auto_refresh_at: HashMap<WeatherLocationId, i64>,
    /// 刚添加、等待选中的地点（按 `provider_location_id` 记）。
    ///
    /// 新地点入库后才有数据库 id，所以按服务商地点 id 认领：
    /// 列表重查时找到它，就把它设为当前显示的城市。
    pub pending_select: Option<String>,
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
            snapshot_location: None,
            query: String::new(),
            candidates: Vec::new(),
            search_request_id: 0,
            key_present: false,
            api_host: String::new(),
            host_input: HostInput::default(),
            test_result: None,
            testing: false,
            save_requested: false,
            refreshing: HashSet::new(),
            error: None,
            error_location: None,
            dirty: true,
            auto_refresh_at: HashMap::new(),
            pending_select: None,
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
    ///
    /// 选中的地点被删除后要能退回默认地点，而不是让整页空白。
    pub fn active_location(&self) -> Option<&WeatherLocation> {
        if let Some(id) = self.selected
            && let Some(location) = self.locations.iter().find(|item| item.id == id)
        {
            return Some(location);
        }
        self.locations
            .iter()
            .find(|item| item.is_default)
            .or_else(|| self.locations.first())
    }

    /// 当前应显示的地点 ID。
    pub fn active_location_id(&self) -> Option<WeatherLocationId> {
        self.active_location().map(|item| item.id)
    }

    /// 某地点是否正在刷新。
    pub fn is_refreshing(&self, id: WeatherLocationId) -> bool {
        self.refreshing.contains(&id)
    }

    /// 某地点的失败提示（`None` 表示该地点没有失败提示）。
    ///
    /// 全局失败（与地点无关）不属于任何地点，只有当前地点的失败才显示，
    /// 否则会出现「A 城失败、切到 B 城却还在显示 A 城的错误」。
    pub fn error_for(&self, id: Option<WeatherLocationId>) -> Option<&str> {
        let error = self.error.as_deref()?;
        match self.error_location {
            // 全局失败：任何地点都显示。
            None => Some(error),
            Some(owner) if Some(owner) == id => Some(error),
            Some(_) => None,
        }
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

    // 只显示**属于当前地点**的快照：切换城市后那一帧里，
    // 手上可能还拿着上一个城市的快照，画出来就成了「A 城的天气配 B 城的名字」。
    let active = state.weather.active_location_id();
    let snapshot = match (
        state.weather.snapshot.clone(),
        state.weather.snapshot_location,
    ) {
        (Some(snapshot), Some(owner)) if Some(owner) == active => Some(snapshot),
        _ => None,
    };
    match snapshot {
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
    let active = state.weather.active_location_id();
    // 失败提示只属于当前地点：别的城市失败不该在这里显示。
    let error = state.weather.error_for(active).map(str::to_string);
    let refreshing = active.is_some_and(|id| state.weather.is_refreshing(id));

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
                // 刷新的是**当前显示的地点**，不是默认地点——
                // 否则切到别的城市后点刷新，动的还是第一个城市。
                let enabled = active.is_some() && !refreshing;
                if ui
                    .add_enabled(enabled, egui::Button::new(RichText::new(label).size(13.0)))
                    .clicked()
                    && let Some(id) = active
                {
                    commands.push(me_core::Command::RefreshWeather { id, force: true });
                    state.weather.refreshing.insert(id);
                    state.weather.error = None;
                    state.weather.error_location = None;
                }
            });
        });
    });
}

/// 一张可点击的参数卡片。
///
/// 卡片上只显示「参数名 / 数值」，点击后弹窗再补充参数的具体含义。
struct MetricCard {
    /// 参数名（卡片上行）。
    label: &'static str,
    /// 数值（卡片下行）。
    value: String,
    /// 参数含义（弹窗里展示）。
    meaning: &'static str,
}

/// 当前天气卡片。
///
/// 布局自上而下两段：
/// - 顶部主行：天气图标 + 温度 + 天气描述，三者作为一个整体水平居中；
/// - 下方参数卡片网格：每张卡片显示「参数名 / 数值」，
///   点击后弹窗补上这个参数到底是什么意思。
fn current_card(ui: &mut egui::Ui, area: Rect, current: &CurrentWeather) {
    let visuals = ui.visuals().clone();
    ui.painter()
        .rect_filled(area, CornerRadius::same(12), visuals.faint_bg_color);
    let inner = area.shrink(18.0);

    // 顶部主行：图标 + 温度 + 描述，整体居中。
    let icon_size = (inner.height() * 0.26).clamp(28.0, 52.0);
    let temp_font = egui::FontId::proportional(38.0);
    let cond_font = egui::FontId::proportional(15.0);
    let painter = ui.painter();
    let temp_galley = painter.layout_no_wrap(
        format!("{:.0}°", current.temp_c),
        temp_font,
        visuals.strong_text_color(),
    );
    let cond_galley = painter.layout_no_wrap(
        condition_text(current),
        cond_font,
        visuals.weak_text_color(),
    );
    let gap = 14.0;
    let main_h = icon_size
        .max(temp_galley.size().y)
        .max(cond_galley.size().y)
        + 8.0;
    let total_w = icon_size + gap + temp_galley.size().x + gap + cond_galley.size().x;
    let mut x = inner.center().x - total_w / 2.0;

    let icon_top = inner.top() + (main_h - icon_size) / 2.0;
    paint_icon(
        ui,
        Rect::from_min_size(egui::pos2(x, icon_top), egui::vec2(icon_size, icon_size)),
        current.kind,
        icon_size,
    );
    x += icon_size + gap;
    let temp_top = inner.top() + (main_h - temp_galley.size().y) / 2.0;
    painter.galley(
        egui::pos2(x, temp_top),
        temp_galley.clone(),
        visuals.strong_text_color(),
    );
    x += temp_galley.size().x + gap;
    let cond_top = inner.top() + (main_h - cond_galley.size().y) / 2.0;
    painter.galley(
        egui::pos2(x, cond_top),
        cond_galley.clone(),
        visuals.weak_text_color(),
    );

    // 下方参数卡片。
    let cards_area = Rect::from_min_max(
        egui::pos2(inner.left(), inner.top() + main_h + 8.0),
        egui::pos2(inner.right(), inner.bottom()),
    );
    if cards_area.height() < 20.0 {
        return;
    }
    let cards = metric_cards(current);
    let rects = metric_grid_layout(cards_area, cards.len());
    for (index, (card, rect)) in cards.iter().zip(rects.iter()).enumerate() {
        metric_tile(ui, *rect, card, index);
    }
}

/// 当前天气的参数卡片清单。
fn metric_cards(current: &CurrentWeather) -> Vec<MetricCard> {
    vec![
        MetricCard {
            label: "体感温度",
            value: format_temp(current.feels_like_c),
            meaning: "结合风速、湿度与日照算出的主观感受温度，通常与实测气温接近，\
                      风吹时偏低、闷热时偏高。",
        },
        MetricCard {
            label: "相对湿度",
            value: format_humidity(current.humidity_percent),
            meaning: "空气中水汽占同温度下饱和水汽的百分比。数值越高越闷，\
                      汗液越不容易蒸发，体感会偏热。",
        },
        MetricCard {
            label: "风力",
            value: format_wind(current.wind_scale, current.wind_speed_kmh),
            meaning: "风速是空气流动的速度（km/h），风级是按风速划分的等级（蒲福风级）。\
                      风力越大，体感温度通常越低。",
        },
        MetricCard {
            label: "降水量",
            value: format_precip(current.precip_mm),
            meaning: "最近一个观测时段内落下的雨雪总量，单位毫米。\
                      0.1 mm 就已经能打湿地面。",
        },
        MetricCard {
            label: "气压",
            value: format!("{:.0} hPa", current.pressure_hpa),
            meaning: "作用在单位面积上的大气压力，单位百帕（hPa）。\
                      标准大气压是 1013.25 hPa，气压骤降常预示变天。",
        },
        MetricCard {
            label: "能见度",
            value: format!("{:.0} km", current.visibility_km),
            meaning: "肉眼能清楚辨认物体的最大距离，单位公里。\
              雾、霾、沙尘都会明显压低能见度。",
        },
        MetricCard {
            label: "云量",
            value: format!("{}%", current.cloud_percent),
            meaning: "天空被云遮蔽的比例，0% 是晴空，100% 是阴天。",
        },
    ]
}

/// 参数卡片的网格布局。
///
/// 固定四列：七张卡片排成「4 + 3」，第二行居中，整体始终居中。
/// 列宽按可用宽度等分，窄屏下也不会溢出卡片区。
fn metric_grid_layout(area: Rect, count: usize) -> Vec<Rect> {
    const GAP: f32 = 8.0;
    let mut out = Vec::with_capacity(count);
    if count == 0 || area.height() <= 0.0 || area.width() <= 0.0 {
        return out;
    }
    let cols = count.clamp(1, 4);
    let rows = count.div_ceil(cols);
    let col_w = (area.width() - GAP * (cols as f32 - 1.0)) / cols as f32;
    let row_h = ((area.height() - GAP * (rows as f32 - 1.0)) / rows as f32).max(24.0);

    for row in 0..rows {
        let in_row = (count - row * cols).min(cols);
        let row_w = col_w * in_row as f32 + GAP * (in_row as f32 - 1.0);
        let row_left = area.center().x - row_w / 2.0;
        let top = area.top() + row as f32 * (row_h + GAP);
        for col in 0..in_row {
            let x = row_left + col as f32 * (col_w + GAP);
            // 末列与末行的边界可能因浮点误差越出区域，收回区域内。
            let right = if col + 1 == in_row {
                (x + col_w).min(area.right())
            } else {
                x + col_w
            };
            let bottom = if row + 1 == rows {
                (top + row_h).min(area.bottom())
            } else {
                top + row_h
            };
            out.push(Rect::from_min_max(
                Pos2::new(x, top),
                Pos2::new(right, bottom),
            ));
        }
    }
    out
}

/// 画一张参数卡片，并挂上点击后弹出的说明。
fn metric_tile(ui: &mut egui::Ui, rect: Rect, card: &MetricCard, index: usize) {
    let visuals = ui.visuals().clone();
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(8), visuals.extreme_bg_color);
    painter.text(
        Pos2::new(rect.center().x, rect.top() + 4.0),
        egui::Align2::CENTER_TOP,
        card.label,
        egui::FontId::proportional(11.0),
        visuals.weak_text_color(),
    );
    let value_font = egui::FontId::proportional(13.0);
    let value_galley =
        painter.layout_no_wrap(card.value.clone(), value_font, visuals.strong_text_color());
    let value_size = value_galley.size();
    painter.galley(
        Pos2::new(
            rect.center().x - value_size.x / 2.0,
            rect.bottom() - 4.0 - value_size.y,
        ),
        value_galley,
        visuals.strong_text_color(),
    );

    let response = ui.interact(
        rect,
        ui.make_persistent_id(("weather_metric", index)),
        Sense::click(),
    );
    if response.hovered() {
        painter.rect_stroke(
            rect,
            CornerRadius::same(8),
            Stroke::new(1.0_f32, ACCENT),
            egui::StrokeKind::Middle,
        );
    }
    egui::Popup::from_toggle_button_response(&response)
        .width(280.0)
        .show(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(card.label).size(14.0).strong());
                ui.add(
                    egui::Label::new(RichText::new(card.meaning).size(12.0).color(Color32::GRAY))
                        .wrap(),
                );
                ui.add_space(6.0);
                ui.label(RichText::new(&card.value).size(22.0).strong().color(ACCENT));
            });
        });
}

/// 24 小时折线图。
///
/// 布局自上而下三段，彼此留出固定间隔，互不重叠：
/// - 标题带：高度按字体真实行高算，不再硬编码；
/// - 绘图区：左侧留出 Y 轴刻度宽度，底部留出 X 轴刻度高度；
/// - 坐标轴刻度：X 轴是相对小时（0..24），Y 轴是温度值。
///
/// X 轴用相对小时而不是时刻，是为了让读者一眼看出「未来 24 小时」的跨度；
/// 起始时刻放在标题右侧，不占用坐标轴空间。
fn hourly_card(ui: &mut egui::Ui, area: Rect, snapshot: &WeatherSnapshot, timezone: &str) {
    let visuals = ui.visuals().clone();
    ui.painter()
        .rect_filled(area, CornerRadius::same(12), visuals.faint_bg_color);
    let inner = area.shrink(18.0);

    // 标题带：用真实行高量出来，避免和下面的折线图叠在一起。
    let title_font = egui::FontId::proportional(15.0);
    let title_height = ui.fonts(|f| f.row_height(&title_font)) + 6.0;
    ui.painter().text(
        Pos2::new(inner.left(), inner.top()),
        egui::Align2::LEFT_TOP,
        "24 小时预报",
        title_font,
        visuals.strong_text_color(),
    );
    // 起始时刻贴在标题右侧，读者知道 0 点对应几点。
    if let Some(first) = snapshot.hourly.first() {
        let start = format_hour(timezone, first.time);
        if !start.is_empty() {
            ui.painter().text(
                Pos2::new(inner.right(), inner.top()),
                egui::Align2::RIGHT_TOP,
                format!("自 {start} 起"),
                egui::FontId::proportional(12.0),
                visuals.weak_text_color(),
            );
        }
    }

    // 绘图区：左边让出 Y 轴刻度，下边让出 X 轴刻度。
    const Y_AXIS_WIDTH: f32 = 40.0;
    const X_AXIS_HEIGHT: f32 = 18.0;
    let plot_area = Rect::from_min_max(
        egui::pos2(inner.left(), inner.top() + title_height),
        egui::pos2(inner.right(), inner.bottom() - X_AXIS_HEIGHT),
    );
    let chart_area = Rect::from_min_max(
        egui::pos2(plot_area.left() + Y_AXIS_WIDTH, plot_area.top()),
        egui::pos2(plot_area.right(), plot_area.bottom()),
    );
    if chart_area.height() < 30.0 || snapshot.hourly.len() < 2 {
        return;
    }

    let points = hourly_series(&snapshot.hourly, 24);
    let positions = chart_positions(chart_area, &points);
    if positions.len() < 2 {
        return;
    }
    let painter = ui.painter();
    let weak = visuals.weak_text_color();

    // 温度量程：Y 轴刻度与折线共用同一套范围，读数才对得上。
    let (min_temp, max_temp) = chart_range(&snapshot.hourly).unwrap_or((0.0, 0.0));
    let ticks = y_axis_ticks(min_temp, max_temp, 4);

    // 网格线 + Y 轴刻度值。
    for tick in &ticks {
        let y = chart_area.bottom() - chart_area.height() * tick.ratio;
        painter.rect_filled(
            Rect::from_min_max(
                Pos2::new(chart_area.left(), y),
                Pos2::new(chart_area.right(), y),
            ),
            CornerRadius::ZERO,
            Color32::from_gray(60),
        );
        painter.text(
            Pos2::new(chart_area.left() - 6.0, y),
            egui::Align2::RIGHT_CENTER,
            format!("{:.0}°", tick.value),
            egui::FontId::proportional(11.0),
            weak,
        );
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
    //
    // 不能用 `Shape::convex_polygon`：epaint 的 `fill_closed_path` 是扇形三角化，
    // 只对凸多边形成立。「折线 + 基线」是非凸图形，扇形会把三角形甩到图形外面，
    // 渲染出楔形伪影。这里手工按「相邻两点 + 基线」拼凸四边形再拆三角形。
    fill_under_curve(painter, &positions, chart_area.bottom(), CHART_FILL_COLOR);

    // 折线本身。
    for pair in positions.windows(2) {
        painter.line_segment([pair[0], pair[1]], Stroke::new(CHART_STROKE, ACCENT));
    }
    // 端点圆点。
    for position in &positions {
        painter.circle_filled(*position, 2.5, ACCENT);
    }

    // X 轴：相对小时 0..24，对应 24 个采样点。
    for tick in x_axis_ticks(points.len()) {
        let x = chart_area.left() + chart_area.width() * tick.ratio;
        painter.text(
            Pos2::new(x, chart_area.bottom() + 4.0),
            egui::Align2::CENTER_CENTER,
            tick.label,
            egui::FontId::proportional(11.0),
            weak,
        );
    }
}

/// 折线下方的填充区域。
///
/// 手工三角化：每一段「上沿两点 + 底部两点」构成一个凸四边形，
/// 拆成两个三角形后整条带子严丝合缝，不会像扇形三角化那样溢出。
fn fill_under_curve(painter: &egui::Painter, positions: &[Pos2], bottom: f32, color: Color32) {
    let mut mesh = egui::epaint::Mesh::default();
    for pair in positions.windows(2) {
        let (left, right) = (pair[0], pair[1]);
        let base_left = Pos2::new(left.x, bottom);
        let base_right = Pos2::new(right.x, bottom);
        let index = mesh.vertices.len() as u32;
        mesh.colored_vertex(left, color);
        mesh.colored_vertex(right, color);
        mesh.colored_vertex(base_right, color);
        mesh.colored_vertex(base_left, color);
        // 拆成两个三角形：左上-右上-右下、左上-右下-左下。
        mesh.add_triangle(index, index + 1, index + 2);
        mesh.add_triangle(index, index + 2, index + 3);
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// Y 轴刻度：温度值与它在图内的相对高度。
#[derive(Debug)]
pub struct AxisTick {
    /// 温度值。
    pub value: f64,
    /// 0 = 图底，1 = 图顶。
    pub ratio: f32,
}

/// 生成 Y 轴刻度。
///
/// 温差很小时也要给出上下两个刻度，否则曲线会贴在边框上看不出量程。
pub fn y_axis_ticks(min: f64, max: f64, count: usize) -> Vec<AxisTick> {
    let count = count.max(2);
    let span = max - min;
    let (min, max) = if span.abs() < f64::EPSILON {
        (min - 1.0, max + 1.0)
    } else {
        (min, max)
    };
    let span = max - min;
    (0..count)
        .map(|index| {
            let ratio = index as f32 / (count - 1) as f32;
            AxisTick {
                value: min + span * ratio as f64,
                ratio,
            }
        })
        .collect()
}

/// X 轴刻度：相对小时数。
#[derive(Debug)]
pub struct HourTick {
    /// 0 = 最左，1 = 最右。
    pub ratio: f32,
    /// 刻度文本（相对小时数）。
    pub label: String,
}

/// 生成 X 轴刻度：0、6、12、18、24。
///
/// 采样点多于 5 个时按 6 小时一格；点数少就按 4 或 1 小时一格，
/// 保证标签不会挤成一团。
pub fn x_axis_ticks(count: usize) -> Vec<HourTick> {
    if count == 0 {
        return Vec::new();
    }
    // 最后一个点对应第 24 小时，所以整条轴的跨度是 24 小时。
    const SPAN_HOURS: f32 = 24.0;
    let step = if count > 12 {
        6
    } else if count > 6 {
        4
    } else {
        1
    };
    let mut ticks = Vec::new();
    let mut hour = 0;
    while hour <= SPAN_HOURS as i32 {
        ticks.push(HourTick {
            ratio: hour as f32 / SPAN_HOURS,
            label: hour.to_string(),
        });
        hour += step;
    }
    ticks
}

/// 7 天预报：横向排布的卡片。
///
/// 每张卡片的底图就是当天的天气（矢量图标铺满卡片作背景），
/// 卡片本身显示日期、天气与最高/最低温；点击弹出当天的详细信息。
fn daily_card(ui: &mut egui::Ui, area: Rect, snapshot: &WeatherSnapshot, timezone: &str) {
    let visuals = ui.visuals().clone();
    ui.painter()
        .rect_filled(area, CornerRadius::same(12), visuals.faint_bg_color);
    let inner = area.shrink(18.0);

    let title_font = egui::FontId::proportional(15.0);
    let title_height = ui.fonts(|f| f.row_height(&title_font)) + 6.0;
    ui.painter().text(
        Pos2::new(inner.left(), inner.top()),
        egui::Align2::LEFT_TOP,
        "7 天预报",
        title_font,
        visuals.strong_text_color(),
    );

    let cards_area = Rect::from_min_max(
        egui::pos2(inner.left(), inner.top() + title_height),
        egui::pos2(inner.right(), inner.bottom()),
    );
    let labels = day_labels(&snapshot.daily, timezone);
    let rects = daily_card_layout(cards_area, snapshot.daily.len());

    for (index, (day, rect)) in snapshot.daily.iter().zip(rects.iter()).enumerate() {
        let label = labels.get(index).map(String::as_str).unwrap_or("");
        daily_tile(ui, *rect, day, label, index);
    }
}

/// 一天预报的背景图标种类。
///
/// 优先用**原始图标码**判定，而不是 `kind`：
/// `kind` 只有「晴/多云/阴」这种粗分类，白天晴天（`100`）与
/// 夜间晴天（`150`）都会落成 `Clear`，背景图看不出昼夜差别；
/// 原始码能区分，背景图才真正反映天气。
///
/// 原始码缺失时（老缓存里没有这个字段）退回 `kind`。
fn day_icon_kind(day: &DailyForecast) -> WeatherKind {
    let from_code = WeatherKind::from_icon_code(&day.icon_day);
    if from_code != WeatherKind::Unknown {
        return from_code;
    }
    let from_night = WeatherKind::from_icon_code(&day.icon_night);
    if from_night != WeatherKind::Unknown {
        return from_night;
    }
    day.kind
}

/// 7 天卡片的排布：能横排就横排，放不下就折成两行。
///
/// 每张卡片有最小宽度，窗口不够宽时自动换行，避免挤成一团。
fn daily_card_layout(area: Rect, count: usize) -> Vec<Rect> {
    const GAP: f32 = 8.0;
    const MIN_CARD_W: f32 = 84.0;
    let mut out = Vec::with_capacity(count);
    if count == 0 || area.height() <= 0.0 || area.width() <= 0.0 {
        return out;
    }
    let per_row = ((area.width() + GAP) / (MIN_CARD_W + GAP)).floor().max(1.0) as usize;
    let per_row = per_row.min(count);
    let rows = count.div_ceil(per_row);
    let card_w = (area.width() - GAP * (per_row as f32 - 1.0)) / per_row as f32;
    let card_h = ((area.height() - GAP * (rows as f32 - 1.0)) / rows as f32).max(56.0);

    for row in 0..rows {
        let in_row = (count - row * per_row).min(per_row);
        let row_w = card_w * in_row as f32 + GAP * (in_row as f32 - 1.0);
        let row_left = area.center().x - row_w / 2.0;
        let top = area.top() + row as f32 * (card_h + GAP);
        for col in 0..in_row {
            let x = row_left + col as f32 * (card_w + GAP);
            // 末列与末行的边界可能因浮点误差越出区域，收回区域内。
            let right = if col + 1 == in_row {
                (x + card_w).min(area.right())
            } else {
                x + card_w
            };
            let bottom = if row + 1 == rows {
                (top + card_h).min(area.bottom())
            } else {
                top + card_h
            };
            out.push(Rect::from_min_max(
                Pos2::new(x, top),
                Pos2::new(right, bottom),
            ));
        }
    }
    out
}

/// 画一天的预报卡片，并挂上点击后弹出的详情。
fn daily_tile(ui: &mut egui::Ui, rect: Rect, day: &DailyForecast, label: &str, index: usize) {
    let visuals = ui.visuals().clone();
    let painter = ui.painter();

    // 卡片底：天气色淡淡铺一层，让每张卡片一眼能分出天气。
    let tint = kind_color(day.kind);
    painter.rect_filled(rect, CornerRadius::same(10), visuals.extreme_bg_color);
    painter.rect_filled(
        rect,
        CornerRadius::same(10),
        Color32::from_rgba_unmultiplied(tint.r(), tint.g(), tint.b(), 26),
    );

    // 背景图：把当天天气图标放大铺满卡片，当作卡片的底图。
    //
    // 尺寸取卡片宽高的较小值再打折，保证图标既不越出卡片，
    // 也不会盖住上下两行的文字。
    //
    // 图标种类取自**原始图标码**：`kind` 只有「晴/多云/阴…」这种粗分类，
    // 而原始码能区分白天与夜间（`100` 晴 / `150` 夜间晴），
    // 背景图才能真正反映当天的天气。
    let bg_size = (rect.width().min(rect.height()) * 0.72).max(24.0);
    let bg_rect = Rect::from_min_size(
        Pos2::new(
            rect.center().x - bg_size / 2.0,
            rect.center().y - bg_size / 2.0,
        ),
        egui::vec2(bg_size, bg_size),
    );
    let bg_kind = day_icon_kind(day);
    paint_icon_translucent(ui, bg_rect, bg_kind, bg_size, 0.5);

    // 文字：日期在顶部，温度在底部。
    painter.text(
        Pos2::new(rect.center().x, rect.top() + 6.0),
        egui::Align2::CENTER_TOP,
        label,
        egui::FontId::proportional(12.0),
        visuals.strong_text_color(),
    );
    painter.text(
        Pos2::new(rect.center().x, rect.top() + 22.0),
        egui::Align2::CENTER_TOP,
        &day.text,
        egui::FontId::proportional(12.0),
        visuals.weak_text_color(),
    );
    let temp_font = egui::FontId::proportional(13.0);
    let temp_text = format!("{:.0}° / {:.0}°", day.temp_max_c, day.temp_min_c);
    let temp_galley = painter.layout_no_wrap(temp_text, temp_font, visuals.strong_text_color());
    let temp_size = temp_galley.size();
    painter.galley(
        Pos2::new(
            rect.center().x - temp_size.x / 2.0,
            rect.bottom() - 6.0 - temp_size.y,
        ),
        temp_galley,
        visuals.strong_text_color(),
    );

    let response = ui.interact(
        rect,
        ui.make_persistent_id(("weather_daily", index)),
        Sense::click(),
    );
    if response.hovered() {
        painter.rect_stroke(
            rect,
            CornerRadius::same(10),
            Stroke::new(1.0_f32, ACCENT),
            egui::StrokeKind::Middle,
        );
    }
    egui::Popup::from_toggle_button_response(&response)
        .width(280.0)
        .show(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(label).size(14.0).strong());
                // 白天 / 夜间各一行：左边是图标，右边是天气描述。
                // 和风 v7 的逐日预报本来就分昼夜给出两套图标与文字，
                // 这里如实呈现，卡片背景图也就有了依据。
                day_weather_row(ui, "白天", &day.icon_day, &day.text, day.kind);
                if !day.text_night.is_empty() || !day.icon_night.is_empty() {
                    day_weather_row(ui, "夜间", &day.icon_night, &day.text_night, day.kind);
                }
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!(
                        "最高 {:.0}°　最低 {:.0}°",
                        day.temp_max_c, day.temp_min_c
                    ))
                    .size(15.0)
                    .strong(),
                );
                ui.label(
                    RichText::new(format!("降水 {}", format_precip(day.precip_mm)))
                        .size(12.0)
                        .color(Color32::GRAY),
                );
                if let (Some(sunrise), Some(sunset)) = (&day.sunrise, &day.sunset) {
                    ui.label(
                        RichText::new(format!("日出 {sunrise}　日落 {sunset}"))
                            .size(12.0)
                            .color(Color32::GRAY),
                    );
                }
            });
        });
}

/// 详情弹窗里的「白天 / 夜间」天气行：图标 + 描述。
fn day_weather_row(
    ui: &mut egui::Ui,
    period: &str,
    icon_code: &str,
    text: &str,
    fallback: WeatherKind,
) {
    let kind = match WeatherKind::from_icon_code(icon_code) {
        WeatherKind::Unknown => fallback,
        kind => kind,
    };
    ui.horizontal(|ui| {
        let icon_size = 26.0_f32;
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(icon_size, icon_size), egui::Sense::hover());
        paint_icon(ui, rect, kind, icon_size);
        ui.label(RichText::new(period).size(12.0).color(Color32::GRAY));
        let label = if text.is_empty() { kind.label() } else { text };
        ui.label(RichText::new(label).size(13.0));
    });
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
        // 刷新当前显示的地点（不是默认地点）。
        let active = state.weather.active_location_id();
        let refreshing = active.is_some_and(|id| state.weather.is_refreshing(id));
        if ui
            .add_enabled(
                active.is_some() && !refreshing,
                egui::Button::new(if refreshing { "刷新中…" } else { "刷新" }),
            )
            .clicked()
            && let Some(id) = active
        {
            commands.push(me_core::Command::RefreshWeather { id, force: true });
            state.weather.refreshing.insert(id);
            state.weather.error = None;
            state.weather.error_location = None;
        }
    });

    // 候选列表。
    if !state.weather.candidates.is_empty() {
        ui.add_space(4.0);
        ui.label(RichText::new("搜索结果").size(12.0).color(Color32::GRAY));
        for candidate in state.weather.candidates.clone() {
            // 和风天气的最小单位是区/县/县级市：搜「成都」会返回成都本级
            // 加上锦江区、龙泉驿、都江堰市等一串下辖区县。
            // 所以标题必须带**具体地点名**（`龙泉驿 · 成都 · 四川省`），
            // 只显示「成都 · 四川省」的话十几个候选长得一模一样。
            let title = candidate.title();
            if ui
                .add_sized(
                    [ui.available_width(), ROW_HEIGHT],
                    egui::Label::new(RichText::new(title)).truncate(),
                )
                .clicked()
            {
                commands.push(me_core::Command::AddWeatherLocation {
                    candidate: candidate.clone(),
                });
                state.weather.candidates.clear();
                // 新地点入库后才有 id，先按服务商地点 id 认领它：
                // 列表重查时把它设为当前显示的城市。
                state.weather.pending_select = Some(candidate.provider_location_id.clone());
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
    let active = state.weather.active_location_id();
    let locations = state.weather.locations.clone();
    for location in locations {
        let is_active = active == Some(location.id);
        let mut text = RichText::new(location.name.clone());
        if is_active {
            text = text.color(ACCENT).strong();
        }
        // 一行放齐：左「删除」、中「名称」、右「设置默认」。
        //
        // 宽度分配必须交给 egui 的布局自己做，**不能手工算**：
        // 手工按 `available_width()` 减去固定宽来分配名字，会漏掉元素之间的
        // `item_spacing`（默认 8px）——整行因此比可用宽度多出 8px，
        // 侧栏的 `min_rect` 随之变宽，`PanelState` 又把这个更宽的值存下来，
        // 下一帧以它为基准再宽 8px……形成正反馈，侧栏会一路涨到窗口右边缘
        // （`SidePanel` 的宽度上限是 `f32::INFINITY`，拦不住）。
        //
        // 做法：名称用 `Layout::right_to_left` 之外的普通 `Label` + `truncate`，
        // 让 egui 自己算剩余空间；右侧按钮用固定宽度，但**放在名称之后**，
        // 由布局自动扣掉 spacing。
        ui.horizontal(|ui| {
            // 左侧：删除按钮。
            if ui.small_button("删除").clicked() {
                commands.push(me_core::Command::RemoveWeatherLocation { id: location.id });
                // 删掉的正是当前显示的城市时，回到默认地点。
                if state.weather.selected == Some(location.id) {
                    state.weather.selected = None;
                }
            }
            // 中间：当前地点标记 + 名称（点击切换到该城市）。
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
            // 名称：吃掉除右侧按钮外的全部剩余宽度。
            //
            // 关键：`available_width()` 在 horizontal 布局里返回的是**父容器的剩余宽度**，
            // 不含本行已放元素（删除按钮 + 标记）的占位——用它直接分配会算出超宽的名字。
            // 行内真实剩余是 `max_rect.right() - cursor.min.x`，还要再扣掉右侧按钮
            // 与它前面的 `item_spacing`。
            let row_remaining = ui.max_rect().right() - ui.cursor().min.x;
            let name_width =
                (row_remaining - ACTION_BTN_WIDTH - ui.spacing().item_spacing.x).max(0.0);
            let response =
                ui.add_sized([name_width, ROW_HEIGHT], egui::Label::new(text).truncate());
            let clicked = response.clicked();
            // 行内只显示具体地点名（侧栏窄），悬停给出完整层级
            // （如「都江堰 · 成都 · 四川省」），免得同名区县分不清。
            response.on_hover_text(location.title());
            if clicked {
                // 切到别的城市：换显示目标，并按新目标重新判断要不要刷新。
                // 旧的失败提示属于上一个城市，一并清掉。
                state.weather.selected = Some(location.id);
                state.weather.dirty = true;
                state.weather.error = None;
                state.weather.error_location = None;
            }
            // 右侧：「设置默认」。已经是默认的那行显示「默认」而不是按钮，
            // 但宽度相同，各行才对得齐。
            if location.is_default {
                ui.add_sized(
                    [ACTION_BTN_WIDTH, ROW_HEIGHT],
                    egui::Label::new(RichText::new("默认").size(12.0).color(Color32::GRAY)),
                );
            } else if ui
                .add_sized(
                    [ACTION_BTN_WIDTH, ROW_HEIGHT],
                    egui::Button::new(RichText::new("设置默认").size(12.0)),
                )
                .clicked()
            {
                commands.push(me_core::Command::SetDefaultWeatherLocation { id: location.id });
            }
        });
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

/// 把一个不透明颜色调成半透明（按比例缩放 alpha）。
///
/// `Color32` 是预乘 alpha 的，缩放 alpha 时 RGB 也要一起缩，
/// 否则半透明的填充会比原色偏亮。
fn fade(color: Color32, alpha: f32) -> Color32 {
    let alpha = alpha.clamp(0.0, 1.0);
    Color32::from_rgba_unmultiplied(
        color.r(),
        color.g(),
        color.b(),
        (color.a() as f32 * alpha).round() as u8,
    )
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
    paint_icon_with_alpha(ui, rect, kind, size, 1.0);
}

/// 画一个半透明的天气图标。
///
/// 卡片背景图用这个版本：图标铺满整张卡片，压到很淡当底纹用。
pub fn paint_icon_translucent(ui: &egui::Ui, rect: Rect, kind: WeatherKind, size: f32, alpha: f32) {
    paint_icon_with_alpha(ui, rect, kind, size, alpha);
}

/// 画天气图标的真正实现。
///
/// `alpha` 会同时作用到填充色与描边色上，半透明版本不会留下
/// 「填充淡了但描边还是实心」的违和感。
fn paint_icon_with_alpha(ui: &egui::Ui, rect: Rect, kind: WeatherKind, size: f32, alpha: f32) {
    let painter = ui.painter();
    let color = fade(kind_color(kind), alpha);

    // 未知天气没有可画的图形，画一个「?」占位，
    // **不能**画成云——否则和「阴」混成一片，7 天卡片就全都一个样了。
    if kind == WeatherKind::Unknown {
        paint_unknown_icon(painter, rect, size, color);
        return;
    }

    // 晴天只有太阳；多云是「太阳 + 云」；其余是云加各自的特征。
    // 三者必须画得不一样，否则卡片背景分不出晴、多云与阴。
    let sun = matches!(kind, WeatherKind::Clear | WeatherKind::PartlyCloudy);
    let cloud = kind != WeatherKind::Clear;

    if sun {
        // 晴天太阳居中；多云时太阳偏左上，被云压住一部分。
        let center = if kind == WeatherKind::Clear {
            Pos2::new(rect.center().x, rect.center().y - size * 0.06)
        } else {
            Pos2::new(rect.left() + size * 0.32, rect.bottom() - size * 0.66)
        };
        painter.circle_filled(center, size * 0.17, color);
        for index in 0..8 {
            let angle = index as f32 * std::f32::consts::TAU / 8.0;
            let start = center + Vec2::new(angle.cos(), angle.sin()) * size * 0.24;
            let end = center + Vec2::new(angle.cos(), angle.sin()) * size * 0.33;
            painter.line_segment([start, end], Stroke::new(1.4_f32, color));
        }
    }

    // 云：几段折线拼成的轮廓，避免用字符。
    let base = rect.bottom() - size * 0.22;
    let cloud_shape = vec![
        Pos2::new(rect.left() + size * 0.20, base),
        Pos2::new(rect.left() + size * 0.28, base - size * 0.16),
        Pos2::new(rect.left() + size * 0.46, base - size * 0.26),
        Pos2::new(rect.left() + size * 0.66, base - size * 0.18),
        Pos2::new(rect.left() + size * 0.80, base - size * 0.04),
        Pos2::new(rect.left() + size * 0.80, base),
    ];
    if cloud {
        painter.add(egui::Shape::convex_polygon(
            cloud_shape,
            if sun {
                fade(Color32::from_rgb(205, 215, 230), alpha)
            } else {
                color
            },
            Stroke::NONE,
        ));
    }

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
        WeatherKind::Sleet => {
            // 雨夹雪：左边两滴雨，右边一个雪点。
            for index in 0..2 {
                let x = rect.left() + size * (0.32 + 0.14 * index as f32);
                painter.line_segment(
                    [
                        Pos2::new(x, base),
                        Pos2::new(x - size * 0.05, base + size * 0.15),
                    ],
                    Stroke::new(1.5_f32, color),
                );
            }
            painter.circle_filled(
                Pos2::new(rect.left() + size * 0.62, base + size * 0.10),
                size * 0.045,
                color,
            );
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
        WeatherKind::Hail => {
            // 冰雹：云下一颗大点加两颗小点，和雪点的排布刻意不同。
            painter.circle_filled(
                Pos2::new(rect.left() + size * 0.46, base + size * 0.11),
                size * 0.07,
                color,
            );
            painter.circle_filled(
                Pos2::new(rect.left() + size * 0.34, base + size * 0.06),
                size * 0.035,
                color,
            );
            painter.circle_filled(
                Pos2::new(rect.left() + size * 0.62, base + size * 0.08),
                size * 0.035,
                color,
            );
        }
        WeatherKind::Fog | WeatherKind::Haze | WeatherKind::Dust => {
            // 雾/霾/浮尘：三道横线；浮尘额外加两个颗粒点以示区别。
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
            if kind == WeatherKind::Dust {
                painter.circle_filled(
                    Pos2::new(rect.left() + size * 0.36, base + size * 0.02),
                    size * 0.03,
                    color,
                );
                painter.circle_filled(
                    Pos2::new(rect.left() + size * 0.64, base + size * 0.02),
                    size * 0.03,
                    color,
                );
            }
        }
        _ => {}
    }
}

/// 未知天气的占位图标：一个圆圈加一个问号。
///
/// 画问号而不是画云，是为了让「图标码没解析出来」这件事**看得见**——
/// 一旦所有卡片都退回成云朵，就再也分不出哪几天是真的阴天了。
fn paint_unknown_icon(painter: &egui::Painter, rect: Rect, size: f32, color: Color32) {
    let center = rect.center();
    painter.circle_stroke(center, size * 0.28, Stroke::new(1.6_f32, color));
    // 问号的钩：一段折线。
    let hook = [
        Pos2::new(center.x - size * 0.07, center.y - size * 0.09),
        Pos2::new(center.x - size * 0.02, center.y - size * 0.14),
        Pos2::new(center.x + size * 0.06, center.y - size * 0.12),
        Pos2::new(center.x + size * 0.07, center.y - size * 0.05),
        Pos2::new(center.x + size * 0.01, center.y + size * 0.01),
    ];
    for pair in hook.windows(2) {
        painter.line_segment([pair[0], pair[1]], Stroke::new(1.6_f32, color));
    }
    // 问号的点。
    painter.circle_filled(
        Pos2::new(center.x + size * 0.01, center.y + size * 0.11),
        size * 0.035,
        color,
    );
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

    /// 造一个地点。
    fn location(
        id: i64,
        provider_location_id: &str,
        name: &str,
        is_default: bool,
    ) -> WeatherLocation {
        WeatherLocation {
            id: WeatherLocationId(id),
            provider: "qweather".to_string(),
            provider_location_id: provider_location_id.to_string(),
            name: name.to_string(),
            admin1: Some("北京市".to_string()),
            admin2: None,
            country: Some("中国".to_string()),
            latitude: 39.9,
            longitude: 116.4,
            timezone: "Asia/Shanghai".to_string(),
            is_default,
            sort_order: id,
            created_at: 0,
        }
    }

    fn state_with_snapshot() -> UiState {
        let mut state = UiState {
            page: crate::state::Page::Weather,
            ..Default::default()
        };
        state.weather.key_present = true;
        state.weather.api_host = "https://devapi.qweather.com".to_string();
        state.weather.locations = vec![location(1, "101010100", "北京", true)];
        // 快照归属：北京。界面只显示属于当前地点的快照。
        state.weather.snapshot_location = Some(WeatherLocationId(1));
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
                icon_day: "100d".to_string(),
                icon_night: "150n".to_string(),
                text_night: "晴".to_string(),
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

    /// 回归：切到**非默认地点**时，当前地点必须是它，而不是默认地点。
    ///
    /// 早先 `active_location()` 在 `selected` 命中不到时直接返回 `None`，
    /// 加上刷新流程写死默认地点，第二个城市就读不出数据了。
    #[test]
    fn selecting_a_second_location_switches_the_active_one() {
        let mut state = state_with_snapshot();
        state
            .weather
            .locations
            .push(location(2, "101020100", "上海", false));
        state.weather.selected = Some(WeatherLocationId(2));
        assert_eq!(
            state.weather.active_location_id(),
            Some(WeatherLocationId(2)),
            "选中的地点应成为当前地点"
        );
        assert_eq!(
            state
                .weather
                .active_location()
                .map(|item| item.name.clone()),
            Some("上海".to_string())
        );
    }

    /// 选中的地点被删除后，要退回默认地点，而不是让整页空白。
    #[test]
    fn a_deleted_selection_falls_back_to_the_default_location() {
        let mut state = state_with_snapshot();
        state.weather.selected = Some(WeatherLocationId(99));
        assert_eq!(
            state.weather.active_location_id(),
            Some(WeatherLocationId(1)),
            "选中的地点不存在时应退回默认地点"
        );
    }

    /// 回归：每个地点的失败提示各自独立。
    ///
    /// A 城失败不应该在切到 B 城后还显示着 A 城的错误。
    #[test]
    fn error_belongs_to_its_own_location() {
        let mut state = state_with_snapshot();
        state
            .weather
            .locations
            .push(location(2, "101020100", "上海", false));
        state.weather.error = Some("上海刷新失败".to_string());
        state.weather.error_location = Some(WeatherLocationId(2));

        assert_eq!(
            state.weather.error_for(Some(WeatherLocationId(2))),
            Some("上海刷新失败"),
            "失败提示应显示在自己的地点上"
        );
        assert_eq!(
            state.weather.error_for(Some(WeatherLocationId(1))),
            None,
            "别的地点不该显示这条提示"
        );
    }

    /// 全局失败（与地点无关，如没配 Key）对所有地点都显示。
    #[test]
    fn global_errors_show_for_every_location() {
        let mut state = state_with_snapshot();
        state.weather.error = Some("请先配置 API Key".to_string());
        state.weather.error_location = None;
        assert_eq!(
            state.weather.error_for(Some(WeatherLocationId(1))),
            Some("请先配置 API Key")
        );
        assert_eq!(
            state.weather.error_for(Some(WeatherLocationId(2))),
            Some("请先配置 API Key")
        );
    }

    /// 回归：刷新状态按地点分别记。
    ///
    /// 刷新北京时切到上海，上海不该显示「刷新中」。
    #[test]
    fn refreshing_state_is_tracked_per_location() {
        let mut state = state_with_snapshot();
        state
            .weather
            .locations
            .push(location(2, "101020100", "上海", false));
        state.weather.refreshing.insert(WeatherLocationId(1));

        assert!(state.weather.is_refreshing(WeatherLocationId(1)));
        assert!(
            !state.weather.is_refreshing(WeatherLocationId(2)),
            "别的地点不该显示「刷新中」"
        );
    }

    /// 侧栏要多地点管理入口：左侧「删除」、右侧「设置默认」。
    #[test]
    fn sidebar_offers_default_and_remove_actions() {
        let mut state = state_with_snapshot();
        state
            .weather
            .locations
            .push(location(2, "101020100", "上海", false));
        let mut commands = Vec::new();
        let out = run_frame(&mut state, 1200.0, 800.0, |state, ui| {
            sidebar(state, &mut commands, ui);
        });
        assert!(out.has("上海"), "侧栏应列出第二个地点");
        assert!(out.has("设置默认"), "非默认地点应提供「设置默认」");
        assert!(out.has("删除"), "应提供删除入口");
    }

    /// 回归：地点行的操作按钮必须**一直可见**。
    ///
    /// 侧栏默认宽 220px，扣掉内边距只剩约 204px——早先按「宽度 < 210 就
    /// 隐藏次要信息」的做法，导致默认宽度下按钮根本不显示，用户点了没反应。
    #[test]
    fn location_actions_are_visible_at_the_default_sidebar_width() {
        let mut state = state_with_snapshot();
        state
            .weather
            .locations
            .push(location(2, "101020100", "上海", false));
        let mut commands = Vec::new();
        // 220px = 侧栏的 default_width，也就是用户最常见的宽度。
        let out = run_frame(&mut state, 220.0, 800.0, |state, ui| {
            sidebar(state, &mut commands, ui);
        });
        assert!(
            out.has("设置默认"),
            "默认侧栏宽度下也必须能看到「设置默认」"
        );
        assert!(out.has("删除"), "默认侧栏宽度下也必须能看到「删除」");
    }

    /// 回归：侧栏宽度不能自己一路涨到窗口右边缘。
    ///
    /// 曾经的缺陷：地点行按 `available_width()` 手工分配名字宽度，
    /// 漏掉了元素之间的 `item_spacing`（8px），整行因此比可用宽度多出 8px。
    /// 侧栏的 `min_rect` 随之变宽 → `PanelState` 把这个更宽的值存下来 →
    /// 下一帧以它为基准又多 8px → **正反馈**，侧栏一路涨到窗口右边缘，
    /// 天气内容全被挤没了。
    ///
    /// 必须在**真实 `SidePanel`** 里跑多帧才能测到：宽度漂移依赖
    /// `PanelState` 的跨帧持久化，只画一帧是测不出来的。
    #[test]
    fn the_sidebar_width_does_not_drift_across_frames() {
        let mut state = state_with_snapshot();
        state
            .weather
            .locations
            .push(location(2, "101020100", "上海", false));
        let ctx = egui::Context::default();
        let mut widths = Vec::new();
        for _ in 0..12 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let output = ctx.run(input, |ctx| {
                egui::SidePanel::left("me_folders")
                    .resizable(true)
                    .default_width(220.0)
                    .min_width(160.0)
                    .show(ctx, |ui| {
                        sidebar(&mut state, &mut Vec::new(), ui);
                    });
            });
            // 侧栏分隔线的 x 坐标就是面板的右边界。
            fn find_edge(shape: &egui::Shape, out: &mut Vec<f32>) {
                match shape {
                    egui::Shape::LineSegment { points, .. } => {
                        // 分隔线是竖直的长线，横向坐标即面板右边界。
                        if (points[0].y - points[1].y).abs() > 100.0 {
                            out.push(points[0].x);
                        }
                    }
                    egui::Shape::Vec(shapes) => {
                        for s in shapes {
                            find_edge(s, out);
                        }
                    }
                    _ => {}
                }
            }
            let mut edges = Vec::new();
            for clipped in &output.shapes {
                find_edge(&clipped.shape, &mut edges);
            }
            widths.push(edges.first().copied().unwrap_or(f32::NAN));
        }
        assert!(
            widths.iter().all(|w| w.is_finite()),
            "每帧都应能测到侧栏边界，实际 {widths:?}"
        );
        let first = widths[0];
        let last = widths[widths.len() - 1];
        assert!(
            (last - first).abs() < 0.5,
            "侧栏宽度不能自己漂移：首帧 {first:.1} → 末帧 {last:.1}，逐帧 {widths:?}"
        );
    }

    /// 回归：侧栏被用户拖窄之后也不能自己涨回来。
    ///
    /// 用户报告「可以拉回左边，但松开鼠标后又不断向右移动」——说明拖拽本身
    /// 没问题，问题是**每一帧**内容都比可用宽度宽一点，于是宽度被重新撑大。
    /// 这里用真实 `SidePanel` 跑多帧，宽度稳定即说明内容没有溢出。
    #[test]
    fn a_narrow_sidebar_stays_narrow_across_frames() {
        let mut state = state_with_snapshot();
        state
            .weather
            .locations
            .push(location(2, "101270111", "都江堰", false));
        let ctx = egui::Context::default();
        let mut widths = Vec::new();
        for _ in 0..12 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let output = ctx.run(input, |ctx| {
                egui::SidePanel::left("me_folders")
                    .resizable(true)
                    // 模拟用户把它拖到最窄：只有 min_width 那一点点。
                    .default_width(160.0)
                    .min_width(160.0)
                    .show(ctx, |ui| {
                        sidebar(&mut state, &mut Vec::new(), ui);
                    });
            });
            fn find_edge(shape: &egui::Shape, out: &mut Vec<f32>) {
                match shape {
                    egui::Shape::LineSegment { points, .. } => {
                        if (points[0].y - points[1].y).abs() > 100.0 {
                            out.push(points[0].x);
                        }
                    }
                    egui::Shape::Vec(shapes) => {
                        for s in shapes {
                            find_edge(s, out);
                        }
                    }
                    _ => {}
                }
            }
            let mut edges = Vec::new();
            for clipped in &output.shapes {
                find_edge(&clipped.shape, &mut edges);
            }
            widths.push(edges.first().copied().unwrap_or(f32::NAN));
        }
        let first = widths[0];
        let last = widths[widths.len() - 1];
        assert!(
            (last - first).abs() < 0.5,
            "窄侧栏也不能自己变宽：首帧 {first:.1} → 末帧 {last:.1}，逐帧 {widths:?}"
        );
    }

    /// 回归：切到第二个城市后，不能拿**第一个城市**的快照当第二个城市显示。
    ///
    /// 侧栏在主区之前绘制，点击城市的那一帧里手上还拿着旧城市的快照；
    /// 不校验归属就会出现「北京的天气配上海的名字」。
    #[test]
    fn a_snapshot_from_another_location_is_not_shown() {
        let mut state = state_with_snapshot();
        state
            .weather
            .locations
            .push(location(2, "101020100", "上海", false));
        // 快照仍属于北京，但当前地点已经是上海。
        state.weather.selected = Some(WeatherLocationId(2));
        assert_eq!(state.weather.snapshot_location, Some(WeatherLocationId(1)));

        let mut commands = Vec::new();
        let out = run_frame(&mut state, 1200.0, 800.0, |state, ui| {
            show(state, &mut commands, ui);
        });
        assert!(out.has("暂无天气数据"), "别的城市的快照不该显示出来");
        assert!(!out.has("24 小时预报"), "不该画出旧城市的预报");
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
            icon_day: "100d".into(),
            icon_night: "150n".into(),
            text_night: "晴".into(),
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

    /// X 轴必须是 0..24 的相对小时，不能再出现 00:00 这种时刻。
    #[test]
    fn x_axis_uses_relative_hours_from_0_to_24() {
        let ticks = x_axis_ticks(24);
        assert!(!ticks.is_empty());
        assert_eq!(ticks.first().unwrap().label, "0");
        assert_eq!(ticks.last().unwrap().label, "24");
        // 刻度必须单调递增，且都落在 0..=1 的相对位置里。
        for pair in ticks.windows(2) {
            assert!(pair[1].ratio > pair[0].ratio, "刻度位置必须递增：{pair:?}");
        }
        for tick in &ticks {
            assert!((0.0..=1.0).contains(&tick.ratio), "刻度越界：{tick:?}");
        }
        // 24 小时跨度时每隔 6 小时一个刻度。
        let labels: Vec<&str> = ticks.iter().map(|tick| tick.label.as_str()).collect();
        assert_eq!(labels, vec!["0", "6", "12", "18", "24"]);
    }

    /// 采样点少时也要覆盖 0 和 24 两端。
    #[test]
    fn x_axis_always_spans_the_full_day() {
        for count in [2usize, 5, 12, 24] {
            let ticks = x_axis_ticks(count);
            assert_eq!(ticks.first().unwrap().label, "0", "count={count}");
            assert_eq!(ticks.last().unwrap().label, "24", "count={count}");
        }
        assert!(x_axis_ticks(0).is_empty());
    }

    /// Y 轴刻度要覆盖实际的温度范围，冷热温差也要看得出来。
    #[test]
    fn y_axis_ticks_cover_the_temperature_range() {
        let ticks = y_axis_ticks(10.0, 20.0, 4);
        assert_eq!(ticks.len(), 4);
        assert_eq!(ticks.first().unwrap().value, 10.0);
        assert_eq!(ticks.last().unwrap().value, 20.0);
        // 底部是最低温、顶部是最高温。
        assert_eq!(ticks.first().unwrap().ratio, 0.0);
        assert_eq!(ticks.last().unwrap().ratio, 1.0);
    }

    /// 温度全一样的序列也要给出上下两个刻度，否则曲线贴着边框看不出量程。
    #[test]
    fn y_axis_ticks_keep_headroom_for_flat_series() {
        let ticks = y_axis_ticks(20.0, 20.0, 4);
        assert!(ticks.first().unwrap().value < 20.0);
        assert!(ticks.last().unwrap().value > 20.0);
    }

    /// 参数卡片：七张排成四列加三列，第二行要居中。
    #[test]
    fn metric_grid_is_four_columns_with_a_centered_last_row() {
        let area = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 120.0));
        let rects = metric_grid_layout(area, 7);
        assert_eq!(rects.len(), 7);
        // 第一行四张，第二行三张。
        let first_row = &rects[..4];
        let second_row = &rects[4..];
        assert_eq!(first_row.len(), 4);
        assert_eq!(second_row.len(), 3);
        // 第二行整体要落在第一行范围内并居中。
        let top_left = first_row.iter().map(|r| r.left()).fold(f32::MAX, f32::min);
        let top_right = first_row.iter().map(|r| r.right()).fold(f32::MIN, f32::max);
        let row_left = second_row.iter().map(|r| r.left()).fold(f32::MAX, f32::min);
        let row_right = second_row
            .iter()
            .map(|r| r.right())
            .fold(f32::MIN, f32::max);
        assert!(row_left > top_left, "第二行应比第一行窄");
        assert!(row_right < top_right, "第二行应比第一行窄");
        let top_center = (top_left + top_right) / 2.0;
        let row_center = (row_left + row_right) / 2.0;
        assert!((top_center - row_center).abs() < 0.5, "第二行应居中");
        // 所有卡片都要落在给定区域内。
        for rect in &rects {
            assert!(area.contains_rect(*rect), "卡片越界：{rect:?}");
        }
    }

    /// 7 天卡片：宽度够时横排成一行，不够时折行且不越界。
    #[test]
    fn daily_cards_fit_the_available_area() {
        let wide = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(900.0, 160.0));
        let rects = daily_card_layout(wide, 7);
        assert_eq!(rects.len(), 7);
        for rect in &rects {
            assert!(wide.contains_rect(*rect), "卡片越界：{rect:?}");
        }
        // 一行放下时所有卡片顶部相同。
        let top = rects[0].top();
        assert!(rects.iter().all(|r| (r.top() - top).abs() < 0.01));

        // 窄区域要折行，且依然不越界。
        let narrow = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(200.0, 200.0));
        let rects = daily_card_layout(narrow, 7);
        assert_eq!(rects.len(), 7);
        for rect in &rects {
            assert!(narrow.contains_rect(*rect), "卡片越界：{rect:?}");
        }
        assert!(
            rects.iter().any(|r| r.top() > narrow.top() + 1.0),
            "窄区域应该折行"
        );
    }

    /// 回归：老的「凸多边形填充」用扇形三角化，在起伏曲线上会多算出
    /// 一大块互相重叠的三角形——半透明填充叠在一起就成了深色楔形伪影。
    ///
    /// 这个测试直接比对两种算法覆盖的面积：
    /// - 手工逐段拼四边形：面积必须与曲线上方的真实区域完全一致；
    /// - 凸多边形扇形三角化：面积必然偏大（这就是伪影的来源）。
    #[test]
    fn curve_fill_stays_inside_the_chart_area() {
        let area = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(200.0, 100.0));
        // 一条先升后降的曲线：中间有明显的高点，两头低。
        let temps: [f64; 24] = [
            12.0, 12.5, 13.0, 14.0, 16.0, 18.0, 20.0, 21.0, 22.0, 22.5, 22.0, 21.0, 20.0, 19.0,
            18.0, 17.0, 16.0, 15.0, 14.0, 13.5, 13.0, 12.5, 12.0, 12.0,
        ];
        let min = temps.iter().copied().fold(f64::MAX, f64::min);
        let max = temps.iter().copied().fold(f64::MIN, f64::max);
        let points: Vec<SeriesPoint> = temps
            .iter()
            .enumerate()
            .map(|(index, temp)| SeriesPoint {
                x: index as f32 / (temps.len() - 1) as f32,
                y: ((temp - min) / (max - min)) as f32,
                temp_c: *temp,
                time: index as i64 * 3600,
                kind: WeatherKind::Cloudy,
                pop_percent: 0,
            })
            .collect();
        let positions = chart_positions(area, &points);

        // 曲线上方（也就是图表底部以上）的真实面积：逐段梯形求和。
        // 梯形的高是「曲线到图表底部」的距离。
        let mut expected = 0.0_f64;
        for pair in positions.windows(2) {
            let left = (area.bottom() - pair[0].y) as f64;
            let right = (area.bottom() - pair[1].y) as f64;
            expected += (pair[1].x - pair[0].x) as f64 * (left + right) / 2.0;
        }

        // 新做法：逐段拼四边形再拆三角形。
        let mut mesh = egui::epaint::Mesh::default();
        for pair in positions.windows(2) {
            let (left, right) = (pair[0], pair[1]);
            let index = mesh.vertices.len() as u32;
            mesh.colored_vertex(left, CHART_FILL_COLOR);
            mesh.colored_vertex(right, CHART_FILL_COLOR);
            mesh.colored_vertex(Pos2::new(right.x, area.bottom()), CHART_FILL_COLOR);
            mesh.colored_vertex(Pos2::new(left.x, area.bottom()), CHART_FILL_COLOR);
            mesh.add_triangle(index, index + 1, index + 2);
            mesh.add_triangle(index, index + 2, index + 3);
        }
        let strip_area = mesh_area(&mesh);
        assert!(
            (strip_area - expected).abs() < 1.0,
            "逐段四边形的面积应与真实区域一致：{strip_area} vs {expected}"
        );
        // 每个顶点都必须落在图表区域内。
        for vertex in &mesh.vertices {
            assert!(
                area.expand(0.5).contains(vertex.pos),
                "填充顶点越出图表区域：{:?}",
                vertex.pos
            );
        }

        // 老做法：整条曲线加基线拼成一个多边形，用凸多边形填充。
        let mut legacy = positions.clone();
        legacy.push(Pos2::new(positions[positions.len() - 1].x, area.bottom()));
        legacy.push(Pos2::new(positions[0].x, area.bottom()));
        let legacy_shape = egui::Shape::convex_polygon(legacy, Color32::WHITE, egui::Stroke::NONE);
        let mut tessellator = egui::epaint::Tessellator::new(
            1.0,
            egui::epaint::TessellationOptions::default(),
            [1024, 1024],
            Vec::new(),
        );
        let mut legacy_mesh = egui::epaint::Mesh::default();
        tessellator.tessellate_shape(legacy_shape, &mut legacy_mesh);
        let legacy_area = mesh_area(&legacy_mesh);
        assert!(
            legacy_area > expected * 1.05,
            "扇形三角化的面积应明显偏大（伪影来源）：{legacy_area} vs {expected}"
        );
    }

    /// 网格里所有三角形的面积之和。
    ///
    /// 羽化会在边缘生成一圈透明顶点，其中可能出现退化三角形；
    /// 这里的面积只统计填充主体（不透明的三角形）。
    fn mesh_area(mesh: &egui::epaint::Mesh) -> f64 {
        mesh.triangles()
            .filter_map(|triangle| {
                let points: Vec<Pos2> = triangle
                    .iter()
                    .map(|index| mesh.vertices[*index as usize].pos)
                    .collect();
                // 透明顶点属于羽化边缘，不计入填充面积。
                let opaque = triangle
                    .iter()
                    .all(|index| mesh.vertices[*index as usize].color.a() > 0);
                if !opaque {
                    return None;
                }
                let (a, b, c) = (points[0], points[1], points[2]);
                let area =
                    ((b.x - a.x) * (c.y - a.y) - (c.x - a.x) * (b.y - a.y)).abs() as f64 / 2.0;
                if area.is_finite() { Some(area) } else { None }
            })
            .sum()
    }

    /// 卡片的背景图标必须按「原始图标码」判定，才能区分白天与夜间。
    ///
    /// 回归：早先只按 `kind` 判定，而 `kind` 只有「晴/多云/阴」这种粗分类，
    /// 白天晴天（`100`）与夜间晴天（`150`）都是 `Clear`，
    /// 背景图自然分不出来。
    #[test]
    fn day_icon_kind_prefers_the_raw_icon_code() {
        let mut day = DailyForecast {
            date: "2026-11-04".into(),
            sunrise: Some("06:30".into()),
            sunset: Some("17:12".into()),
            temp_max_c: 22.0,
            temp_min_c: 11.0,
            precip_mm: 0.0,
            kind: WeatherKind::Clear,
            text: "晴".into(),
            icon_day: "100d".into(),
            icon_night: "150n".into(),
            text_night: "晴".into(),
        };
        // 白天码优先。
        assert_eq!(day_icon_kind(&day), WeatherKind::Clear);

        // 白天是雨、夜间是晴时，背景图应按白天画成雨。
        day.icon_day = "300d".into();
        assert_eq!(day_icon_kind(&day), WeatherKind::Rain);

        // 白天码缺失时退回夜间码。
        day.icon_day = String::new();
        day.icon_night = "302n".into();
        assert_eq!(day_icon_kind(&day), WeatherKind::Thunderstorm);

        // 两个原始码都没有时（老缓存）退回 `kind`。
        day.icon_night = String::new();
        day.kind = WeatherKind::Snow;
        assert_eq!(day_icon_kind(&day), WeatherKind::Snow);
    }

    /// 折线填充必须逐段拼四边形，不能用凸多边形填充整个曲线区域。
    #[test]
    fn fill_under_curve_builds_a_valid_triangle_strip() {
        let area = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(200.0, 100.0));
        let points = vec![
            SeriesPoint {
                x: 0.0,
                y: 0.2,
                temp_c: 12.0,
                time: 0,
                kind: WeatherKind::Cloudy,
                pop_percent: 0,
            },
            SeriesPoint {
                x: 0.5,
                y: 0.9,
                temp_c: 19.0,
                time: 3600,
                kind: WeatherKind::Cloudy,
                pop_percent: 0,
            },
            SeriesPoint {
                x: 1.0,
                y: 0.1,
                temp_c: 11.0,
                time: 7200,
                kind: WeatherKind::Cloudy,
                pop_percent: 0,
            },
        ];
        let positions = chart_positions(area, &points);
        let mut mesh = egui::epaint::Mesh::default();
        for pair in positions.windows(2) {
            let (left, right) = (pair[0], pair[1]);
            let index = mesh.vertices.len() as u32;
            mesh.colored_vertex(left, CHART_FILL_COLOR);
            mesh.colored_vertex(right, CHART_FILL_COLOR);
            mesh.colored_vertex(Pos2::new(right.x, area.bottom()), CHART_FILL_COLOR);
            mesh.colored_vertex(Pos2::new(left.x, area.bottom()), CHART_FILL_COLOR);
            mesh.add_triangle(index, index + 1, index + 2);
            mesh.add_triangle(index, index + 2, index + 3);
        }
        assert!(mesh.is_valid(), "网格索引必须合法");
        // 两段折线 → 4 个三角形。
        assert_eq!(mesh.triangles().count(), 4);
        assert_eq!(mesh.vertices.len(), 8);
    }
}
