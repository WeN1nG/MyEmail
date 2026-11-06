//! 音乐页：左侧栏放播放列表，主区上为专辑页（封面 + 信息 + 控制）、下为频谱。
//!
//! 界面**不读音频文件、不自己计时、不做频谱分析**：播放动作转成 [`AudioCommand`]
//! 交给 app 转交音频线程；位置只来自音频线程回传；柱高只来自
//! [`crate::audio::spectrum::SpectrumAnalyzer`] 的计算结果（文档 §2.4 / §5.5.3）。

use eframe::egui::{self, Align, Align2, Color32, CornerRadius, FontId, Layout, Rect, RichText};
use me_core::{Command, MusicOrder};

use crate::audio::AudioCommand;
use crate::state::UiState;

/// 按标题匹配曲目，返回**曲库里的索引**（不是命中序号）。
///
/// AI 的 `music_search` / `music_play` 与界面搜索框共用这一份：两边各写一遍，
/// 匹配规则迟早会分叉，用户看到「搜索有、AI 说没有」。
pub fn find_tracks(tracks: &[crate::audio::playlist::Track], query: &str) -> Vec<usize> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    tracks
        .iter()
        .enumerate()
        .filter(|(_, track)| track.title.to_lowercase().contains(&needle))
        .map(|(index, _)| index)
        .collect()
}

const ROW_HEIGHT: f32 = 24.0;
const DURATION_WIDTH: f32 = 56.0;
/// 侧栏宽度小于它就不再挤时长列，只显示曲名。
const NARROW_SIDEBAR: f32 = 210.0;
const ACCENT: Color32 = Color32::from_rgb(59, 130, 246);
/// 柱子的最小可见高度：静音时也留一条底线，否则整块区域像坏了。
const BAR_MIN_HEIGHT: f32 = 2.0;
const BAR_GAP: f32 = 2.0;

/// 主区上下两块的高度占比：专辑页 : 频谱 = 7 : 3。
const ALBUM_RATIO: f32 = 0.7;
/// 主区上下两块之间的间隙。
const PANEL_GAP: f32 = 8.0;
/// 频谱区的最小高度（柱子不至于被压扁）。
const SPECTRUM_MIN_HEIGHT: f32 = 110.0;
/// 封面正方形的最大边长：专辑页变高时封面不再跟着长。
const COVER_MAX_SIDE: f32 = 420.0;

/// 专辑页右列：曲名区高度上限。
const TITLE_HEIGHT: f32 = 76.0;
/// 专辑页右列：进度条那一行的高度。
const PROGRESS_HEIGHT: f32 = 30.0;
/// 专辑页右列：传输控件一行的高度。
const CONTROLS_HEIGHT: f32 = 34.0;
/// 传输控件下方状态小字的高度。
const STATUS_HEIGHT: f32 = 16.0;
/// 进度条与控件之间的间距。
const BAND_GAP: f32 = 10.0;
/// 传输按钮之间的间距。
const BUTTON_GAP: f32 = 8.0;
/// 曲目行左侧给"正在播放"色条留的宽度。
const GUTTER_WIDTH: f32 = 6.0;

// ---------------------------------------------------------------------------
// 侧栏：目录 + 播放列表
// ---------------------------------------------------------------------------

/// 音乐页的左侧栏内容（由 `views::folder_sidebar` 在音乐页调用）。
pub fn sidebar(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    dir_row(state, commands, ui);
    status_line(state, ui);
    ui.separator();

    // 列表占满剩余高度，底部「共 N 首 · 顺序」固定贴底。
    let footer_height = 30.0;
    let list_height = (ui.available_height() - footer_height).max(80.0);
    let width = ui.available_width();
    let (_, area) = ui.allocate_space(egui::vec2(width, list_height));
    ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| {
        track_list(state, ui);
    });

    ui.separator();
    footer(state, commands, ui);
}

fn dir_row(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    let dir_label = state
        .music
        .dir
        .as_ref()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| "未选择目录".to_string());
    ui.add(
        egui::Label::new(RichText::new(dir_label).small().color(Color32::GRAY))
            .truncate()
            .sense(egui::Sense::hover()),
    )
    .on_hover_text("当前音乐目录");

    ui.horizontal(|ui| {
        if ui.small_button("选择目录").clicked() {
            pick_folder(state, commands);
        }
        let can_rescan = state.music.dir.is_some() && state.music.device_error.is_none();
        if ui
            .add_enabled(can_rescan, egui::Button::new("重新扫描").small())
            .clicked()
        {
            state.music.scanning = true;
            state.music.pending.push(AudioCommand::Rescan);
        }
    });
}

fn status_line(state: &UiState, ui: &mut egui::Ui) {
    if let Some(error) = state.music.device_error.clone() {
        ui.label(
            RichText::new(format!("{error}；连接音频设备后重新进入本页可重试"))
                .small()
                .color(Color32::from_rgb(210, 80, 80)),
        );
    } else if state.music.scanning {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new("正在扫描目录…").small().color(Color32::GRAY));
        });
    } else if state.music.truncated {
        ui.label(
            RichText::new(format!(
                "文件较多，只加载了前 {} 首",
                state.music.tracks.len()
            ))
            .small()
            .color(Color32::GRAY),
        );
    } else if state.music.skipped > 0 {
        ui.label(
            RichText::new(format!("已跳过 {} 个非音频文件", state.music.skipped))
                .small()
                .color(Color32::GRAY),
        );
    }
}

fn footer(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{} 首", state.music.tracks.len()))
                .small()
                .color(Color32::GRAY),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            for (order, label) in [
                (MusicOrder::Shuffle, "随机"),
                (MusicOrder::Sequential, "顺序"),
            ] {
                if ui
                    .selectable_label(state.music.order == order, RichText::new(label).small())
                    .clicked()
                {
                    set_order(state, commands, order);
                }
            }
        });
    });
}

fn pick_folder(state: &mut UiState, commands: &mut Vec<Command>) {
    let mut dialog = rfd::FileDialog::new();
    if let Some(dir) = &state.music.dir {
        dialog = dialog.set_directory(dir);
    }
    let Some(dir) = dialog.pick_folder() else {
        return;
    };

    // 换目录立即清空旧列表：旧索引在新队列里会指向别的歌。
    state.music.dir = Some(dir.clone());
    state.music.tracks.clear();
    state.music.current = None;
    state.music.position_ms = 0;
    state.music.duration_ms = None;
    state.music.error = None;
    state.music.scanning = true;
    state
        .music
        .pending
        .push(AudioCommand::LoadDirectory(dir.clone()));
    commands.push(Command::SetMusicSettings {
        dir: Some(dir),
        order: None,
    });
}

fn set_order(state: &mut UiState, commands: &mut Vec<Command>, order: MusicOrder) {
    if state.music.order == order {
        return;
    }
    state.music.order = order;
    state.music.pending.push(AudioCommand::SetOrder(order));
    commands.push(Command::SetMusicSettings {
        dir: None,
        order: Some(order),
    });
}

fn track_list(state: &mut UiState, ui: &mut egui::Ui) {
    if state.music.tracks.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            let hint = if state.music.dir.is_some() {
                "这个目录里没有找到音频文件"
            } else {
                "选择一个目录开始播放"
            };
            ui.label(RichText::new(hint).small().color(Color32::GRAY));
        });
        return;
    }

    let mut clicked = None;
    egui::ScrollArea::vertical()
        .id_salt("music_tracks")
        .auto_shrink([false, false])
        .show_rows(ui, ROW_HEIGHT, state.music.tracks.len(), |ui, range| {
            for index in range {
                if track_row(state, ui, index) {
                    clicked = Some(index);
                }
            }
        });

    if let Some(index) = clicked {
        let path = state.music.tracks[index].path.clone();
        // 乐观更新选中态，真实状态以音频线程回传为准。
        state.music.current = Some(index);
        state.music.pending.push(AudioCommand::Play { index, path });
    }
}

fn track_row(state: &UiState, ui: &mut egui::Ui, index: usize) -> bool {
    let track = &state.music.tracks[index];
    let is_current = state.music.current == Some(index);
    let mut text = RichText::new(format!("{:02}. {}", index + 1, track.title));
    if is_current {
        text = text.color(ACCENT).strong();
    }

    let show_duration = ui.available_width() >= NARROW_SIDEBAR;
    let base = ui.available_width() - GUTTER_WIDTH;
    let width = if show_duration {
        (base - DURATION_WIDTH).max(80.0)
    } else {
        base.max(80.0)
    };

    let mut clicked = false;
    ui.horizontal(|ui| {
        // 左侧留一条 gutter 画"正在播放"色条：用图形而不是字符，
        // 免得字体缺字变成方框。播放中满格、暂停时短一截。
        let (gutter, _) =
            ui.allocate_exact_size(egui::vec2(GUTTER_WIDTH, ROW_HEIGHT), egui::Sense::hover());
        let response = ui.add_sized(
            [width, ROW_HEIGHT],
            egui::Label::new(text)
                .truncate()
                .sense(egui::Sense::click()),
        );
        if response.clicked() {
            clicked = true;
        }
        if is_current {
            let height = if state.music.playing { 16.0 } else { 9.0 };
            let bar = Rect::from_min_max(
                egui::pos2(gutter.left() + 1.0, gutter.center().y - height / 2.0),
                egui::pos2(gutter.left() + 3.5, gutter.center().y + height / 2.0),
            );
            ui.painter().rect_filled(bar, CornerRadius::same(1), ACCENT);
        }
        if show_duration {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let duration = track
                    .duration_ms
                    .map(format_time)
                    .unwrap_or_else(|| "—".to_string());
                ui.label(RichText::new(duration).small().color(Color32::GRAY));
            });
        }
    });
    clicked
}

// ---------------------------------------------------------------------------
// 主区：上专辑页、下频谱
// ---------------------------------------------------------------------------

pub fn show(state: &mut UiState, _commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    let available = ui.available_size();
    let (_, area) = ui.allocate_space(available);

    // 上部占 7、下部占 3：拖动窗口时两块按同比例一起缩放。
    let (album_height, _spectrum_height) = split_album_spectrum(area.height());
    let album_area = Rect::from_min_size(area.min, egui::vec2(area.width(), album_height));
    let spectrum_area = Rect::from_min_max(
        egui::pos2(area.left(), album_area.bottom() + PANEL_GAP),
        area.max,
    );

    ui.scope_builder(egui::UiBuilder::new().max_rect(album_area), |ui| {
        album_card(state, ui, album_area);
    });
    ui.scope_builder(egui::UiBuilder::new().max_rect(spectrum_area), |ui| {
        spectrum_panel(state, ui, spectrum_area);
    });
}

/// 主区上下两块的高度分配：`(专辑页, 频谱)`，不含中间间隙。
///
/// 目标是 7 : 3；两块各有最小高度，窗口太小时按"先保频谱、再保专辑页"退化，
/// 免得频谱被压成一条缝。
fn split_album_spectrum(height: f32) -> (f32, f32) {
    let usable = (height - PANEL_GAP).max(0.0);
    let target_album = usable * ALBUM_RATIO;
    let target_spectrum = usable - target_album;
    // 频谱先保底：不够就从专辑页扣，专辑页拿剩下的（空间够时正好是 7:3）。
    let spectrum = target_spectrum.max(SPECTRUM_MIN_HEIGHT.min(usable));
    let album = (usable - spectrum).max(0.0);
    (album, spectrum)
}

/// 专辑页内部的三块区域。
struct AlbumLayout {
    cover: Rect,
    /// 右侧文字区与控件区；窗口窄到放不下时为 `None`。
    info: Option<AlbumInfo>,
}

/// 专辑页右列从上到下的三块：标题、进度条、控件。
struct AlbumInfo {
    /// 曲名与副标题。
    title: Rect,
    /// 进度条与时间。
    progress: Rect,
    /// 上一首 / 播放暂停 / 下一首，以及下方一行状态小字。
    controls: Rect,
}

/// 纯几何：同样的输入永远得到同样的分区，方便直接验证不重叠。
fn album_layout(inner: Rect) -> AlbumLayout {
    let side = inner
        .height()
        .min(inner.width() * 0.42)
        .clamp(60.0, COVER_MAX_SIDE);
    let cover = Rect::from_min_size(
        egui::pos2(inner.left(), inner.center().y - side / 2.0),
        egui::vec2(side, side),
    );

    let info_left = cover.right() + 22.0;
    if inner.right() - info_left < 120.0 {
        return AlbumLayout { cover, info: None };
    }

    let column = Rect::from_min_max(
        egui::pos2(info_left, inner.top()),
        egui::pos2(inner.right(), inner.bottom()),
    );

    // 标题区贴在右列顶部。
    let title = Rect::from_min_max(
        column.left_top(),
        egui::pos2(
            column.right(),
            column.top() + TITLE_HEIGHT.min(column.height() * 0.45),
        ),
    );

    // 进度条与控件当成一组，垂直居中在标题区与列底之间。
    // 这样三个按钮落在进度条的正下方（而不是掉到页面最底部），
    // 且整组位于专辑页中部。
    let band_height = PROGRESS_HEIGHT + BAND_GAP + CONTROLS_HEIGHT + STATUS_HEIGHT;
    let rest_top = title.bottom();
    let available = (column.bottom() - rest_top).max(0.0);
    let band_top = rest_top + (available - band_height).max(0.0) / 2.0;
    // 空间不够时先压进度条，保证按钮完整可用。
    let progress_height = PROGRESS_HEIGHT
        .min((column.bottom() - band_top - BAND_GAP - CONTROLS_HEIGHT - STATUS_HEIGHT).max(0.0));
    let progress = Rect::from_min_max(
        egui::pos2(column.left(), band_top),
        egui::pos2(column.right(), band_top + progress_height),
    );
    let controls = Rect::from_min_max(
        egui::pos2(column.left(), progress.bottom() + BAND_GAP),
        egui::pos2(
            column.right(),
            (progress.bottom() + BAND_GAP + CONTROLS_HEIGHT + STATUS_HEIGHT).min(column.bottom()),
        ),
    );
    AlbumLayout {
        cover,
        info: Some(AlbumInfo {
            title,
            progress,
            controls,
        }),
    }
}

fn album_card(state: &mut UiState, ui: &mut egui::Ui, area: Rect) {
    let visuals = ui.visuals().clone();
    ui.painter()
        .rect_filled(area, CornerRadius::same(12), visuals.faint_bg_color);

    let inner = area.shrink(18.0);
    if inner.width() < 40.0 || inner.height() < 40.0 {
        return;
    }

    if state.music.tracks.is_empty() {
        empty_hint(state, ui, inner);
        return;
    }

    // 封面占左侧一块正方形；窄窗口时随高度收缩，不会挤压右侧文字。
    let AlbumLayout {
        cover: cover_rect,
        info,
    } = album_layout(inner);
    cover(state, ui, cover_rect);

    // 实在太窄就只留封面，避免文字被压成一列竖杠。
    let Some(AlbumInfo {
        title: title_rect,
        progress: progress_rect,
        controls: controls_rect,
    }) = info
    else {
        return;
    };

    ui.scope_builder(egui::UiBuilder::new().max_rect(title_rect), |ui| {
        ui.vertical(|ui| {
            let title = state
                .music
                .display_title()
                .unwrap_or_else(|| "未在播放".to_string());
            ui.add(
                egui::Label::new(RichText::new(title).size(26.0).strong())
                    .truncate()
                    .sense(egui::Sense::hover()),
            );

            let subtitle = state.music.subtitle();
            let subtitle = if subtitle.is_empty() {
                state
                    .music
                    .current_track()
                    .map(|track| track.path.to_string_lossy().to_string())
                    .unwrap_or_default()
            } else {
                subtitle
            };
            ui.add(
                egui::Label::new(RichText::new(subtitle).size(14.0).color(Color32::GRAY))
                    .truncate()
                    .sense(egui::Sense::hover()),
            );

            if let Some(error) = state.music.error.clone() {
                ui.label(
                    RichText::new(error)
                        .small()
                        .color(Color32::from_rgb(210, 80, 80)),
                );
            }
        });
    });

    ui.scope_builder(egui::UiBuilder::new().max_rect(progress_rect), |ui| {
        progress(state, ui);
    });

    ui.scope_builder(egui::UiBuilder::new().max_rect(controls_rect), |ui| {
        transport(state, ui, controls_rect);
    });
}

fn empty_hint(state: &mut UiState, ui: &mut egui::Ui, area: Rect) {
    ui.painter().text(
        area.center() - egui::vec2(0.0, 12.0),
        Align2::CENTER_CENTER,
        if state.music.dir.is_some() {
            "这个目录里没有找到音频文件"
        } else {
            "选择一个目录开始播放"
        },
        FontId::proportional(18.0),
        Color32::GRAY,
    );
    ui.painter().text(
        area.center() + egui::vec2(0.0, 14.0),
        Align2::CENTER_CENTER,
        "支持 mp3 / flac / wav / ogg",
        FontId::proportional(12.0),
        Color32::GRAY,
    );
}

/// 封面：有图就贴图，没图就画渐变占位。
fn cover(state: &mut UiState, ui: &mut egui::Ui, rect: Rect) {
    if let Some(texture) = cover_texture(state, ui.ctx()) {
        ui.painter().image(
            texture.id(),
            rect,
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    } else {
        let seed = state
            .music
            .display_title()
            .or_else(|| {
                state
                    .music
                    .now_playing
                    .as_ref()
                    .and_then(|now| now.title.clone())
            })
            .unwrap_or_else(|| "MyEmail".to_string());
        let (top, bottom) = placeholder_colors(&seed);
        paint_gradient(ui.painter(), rect, top, bottom);
        let initial = seed
            .chars()
            .next()
            .map(|ch| ch.to_uppercase().to_string())
            .unwrap_or_else(|| "♪".to_string());
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            initial,
            FontId::proportional(rect.height() * 0.36),
            Color32::from_white_alpha(230),
        );
    }
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(8),
        egui::Stroke::new(1.0_f32, ui.visuals().weak_text_color()),
        egui::StrokeKind::Inside,
    );
}

/// 按当前曲目路径惰性建纹理；换歌时旧纹理直接丢弃。
fn cover_texture(state: &mut UiState, ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let now = state.music.now_playing.as_ref()?;
    let cover = now.cover.as_ref()?;
    let fresh = !matches!(
        state.music.cover_texture.as_ref(),
        Some((key, _)) if *key == now.path
    );
    if fresh {
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [cover.width as usize, cover.height as usize],
            &cover.rgba,
        );
        let handle = ctx.load_texture(
            format!("music-cover:{}", now.path.display()),
            image,
            egui::TextureOptions::LINEAR,
        );
        state.music.cover_texture = Some((now.path.clone(), handle));
    }
    state
        .music
        .cover_texture
        .as_ref()
        .map(|(_, handle)| handle.clone())
}

fn paint_gradient(painter: &egui::Painter, rect: Rect, top: Color32, bottom: Color32) {
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(egui::Shape::mesh(mesh));
}

/// 由曲名派生的两个渐变色：同一首歌每次进来颜色都一样。
fn placeholder_colors(seed: &str) -> (Color32, Color32) {
    let mut hash: u32 = 2_166_136_261;
    for byte in seed.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(16_777_619);
    }
    let hue = (hash % 360) as f32;
    (hsv(hue, 0.55, 0.62), hsv((hue + 45.0) % 360.0, 0.62, 0.32))
}

fn hsv(hue: f32, saturation: f32, value: f32) -> Color32 {
    let c = value * saturation;
    let x = c * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
    let m = value - c;
    let (r, g, b) = match hue as u32 / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    Color32::from_rgb(
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

fn progress(state: &mut UiState, ui: &mut egui::Ui) {
    let duration_ms = state.music.duration_ms.unwrap_or(0);
    let seekable =
        duration_ms > 0 && state.music.device_error.is_none() && !state.music.tracks.is_empty();
    let mut seconds = state.music.display_position_ms() as f64 / 1000.0;
    let max = duration_ms as f64 / 1000.0;

    ui.horizontal(|ui| {
        let time_width = 96.0;
        ui.spacing_mut().slider_width = (ui.available_width() - time_width).max(80.0);
        let response = ui.add_enabled(
            seekable,
            egui::Slider::new(&mut seconds, 0.0..=max.max(0.001)).show_value(false),
        );
        // 拖动期间用本地值，松手才发 Seek：否则 250ms 的位置回传会把滑块拽回去。
        if response.dragged() {
            state.music.scrubbing = Some((seconds * 1000.0).round() as u64);
        }
        if response.drag_stopped() {
            let target = (seconds * 1000.0).round().max(0.0) as u64;
            state.music.pending.push(AudioCommand::Seek(target));
            state.music.scrubbing = None;
        }

        let position = format_time(state.music.display_position_ms());
        let total = if duration_ms > 0 {
            format_time(duration_ms)
        } else {
            "--:--".to_string()
        };
        ui.label(
            RichText::new(format!("{position} / {total}"))
                .small()
                .color(Color32::GRAY),
        );
    });
}

/// 三个按钮在一行里的排布：返回左侧留白与各按钮宽度。
///
/// 抽成纯函数是为了能直接断言「按钮组水平居中在进度条中间」。
fn transport_row(width: f32) -> (f32, [f32; 3]) {
    let widths = [46.0_f32, 64.0, 46.0];
    let gaps = BUTTON_GAP * (widths.len() as f32 - 1.0);
    let total: f32 = widths.iter().sum::<f32>() + gaps;
    let offset = ((width - total) / 2.0).max(0.0);
    (offset, widths)
}

/// 传输控件：按钮组水平居中于 `rect`（也就是进度条正下方），状态小字另起一行。
fn transport(state: &mut UiState, ui: &mut egui::Ui, rect: Rect) {
    let enabled = !state.music.tracks.is_empty() && state.music.device_error.is_none();
    let row_height = CONTROLS_HEIGHT.min(rect.height());
    let (offset, widths) = transport_row(rect.width());

    ui.spacing_mut().item_spacing.x = BUTTON_GAP;
    ui.horizontal(|ui| {
        ui.add_space(offset);
        if icon_button(ui, Icon::Prev, egui::vec2(widths[0], row_height), enabled).clicked() {
            state.music.pending.push(AudioCommand::Prev);
        }
        let icon = if state.music.playing {
            Icon::Pause
        } else {
            Icon::Play
        };
        if icon_button(ui, icon, egui::vec2(widths[1], row_height), enabled).clicked() {
            state.music.pending.push(AudioCommand::Toggle);
        }
        if icon_button(ui, Icon::Next, egui::vec2(widths[2], row_height), enabled).clicked() {
            state.music.pending.push(AudioCommand::Next);
        }
    });

    let status = if state.music.playing {
        Some(("播放中", ACCENT))
    } else if state.music.current.is_some() {
        Some(("已暂停", Color32::GRAY))
    } else {
        None
    };
    if let Some((text, color)) = status {
        ui.with_layout(
            egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
            |ui| {
                ui.label(RichText::new(text).small().color(color));
            },
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Icon {
    Prev,
    Play,
    Pause,
    Next,
}

/// 用图形而不是字符画按钮：不依赖字体是否收录 ⏮/⏸ 这类符号。
fn icon_button(ui: &mut egui::Ui, icon: Icon, size: egui::Vec2, enabled: bool) -> egui::Response {
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    let visuals = ui.visuals();
    let fill = if !enabled {
        Color32::TRANSPARENT
    } else if response.is_pointer_button_down_on() {
        visuals.widgets.active.weak_bg_fill
    } else if response.hovered() {
        visuals.widgets.hovered.weak_bg_fill
    } else {
        Color32::TRANSPARENT
    };
    if fill != Color32::TRANSPARENT {
        ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    }
    let color = if enabled {
        visuals.strong_text_color()
    } else {
        visuals.weak_text_color()
    };
    ui.painter().extend(icon_shapes(icon, rect, color));
    response
}

fn icon_shapes(icon: Icon, rect: Rect, color: Color32) -> Vec<egui::Shape> {
    let center = rect.center();
    let half = (rect.width().min(rect.height()) * 0.18).clamp(5.0, 9.0);
    let triangle = |dir: f32| {
        egui::Shape::convex_polygon(
            vec![
                center + egui::vec2(dir * half * 1.2, 0.0),
                center + egui::vec2(dir * -half * 0.6, -half),
                center + egui::vec2(dir * -half * 0.6, half),
            ],
            color,
            egui::Stroke::NONE,
        )
    };
    let bar_shape = |x: f32| {
        egui::Shape::rect_filled(
            Rect::from_min_max(
                egui::pos2(x - half * 0.16, center.y - half),
                egui::pos2(x + half * 0.16, center.y + half),
            ),
            CornerRadius::ZERO,
            color,
        )
    };
    match icon {
        Icon::Play => vec![triangle(1.0)],
        Icon::Pause => vec![
            bar_shape(center.x - half * 0.45),
            bar_shape(center.x + half * 0.45),
        ],
        Icon::Prev => vec![
            triangle(-1.0),
            egui::Shape::rect_filled(
                Rect::from_min_max(
                    egui::pos2(center.x - half * 1.15, center.y - half),
                    egui::pos2(center.x - half * 0.95, center.y + half),
                ),
                CornerRadius::ZERO,
                color,
            ),
        ],
        Icon::Next => vec![
            triangle(1.0),
            egui::Shape::rect_filled(
                Rect::from_min_max(
                    egui::pos2(center.x + half * 0.95, center.y - half),
                    egui::pos2(center.x + half * 1.15, center.y + half),
                ),
                CornerRadius::ZERO,
                color,
            ),
        ],
    }
}
fn spectrum_panel(state: &UiState, ui: &mut egui::Ui, area: Rect) {
    let visuals = ui.visuals().clone();
    ui.painter()
        .rect_filled(area, CornerRadius::same(12), visuals.extreme_bg_color);

    let inner = area.shrink2(egui::vec2(16.0, 12.0));
    if inner.width() < 40.0 || inner.height() < 20.0 {
        return;
    }

    let levels = state.music.levels();
    let rects = bar_rects(inner, levels);
    let painter = ui.painter();
    for (index, rect) in rects.iter().enumerate() {
        let level = levels.get(index).copied().unwrap_or(0.0);
        painter.rect_filled(
            *rect,
            CornerRadius {
                nw: 2,
                ne: 2,
                sw: 0,
                se: 0,
            },
            bar_color(level),
        );
    }

    if !state.music.playing {
        let hint = if state.music.tracks.is_empty() {
            "没有正在播放的音乐"
        } else {
            "已暂停"
        };
        ui.painter().text(
            inner.center(),
            Align2::CENTER_CENTER,
            hint,
            FontId::proportional(12.0),
            Color32::from_white_alpha(70),
        );
    }
}

/// 每根柱子的矩形：底部对齐、等宽、等间隔，高度与柱高成正比。
///
/// 抽成纯函数是为了能直接测：柱数、对齐、单调性都不该靠截图判断。
fn bar_rects(area: Rect, levels: &[f32]) -> Vec<Rect> {
    let count = levels.len().max(1);
    if area.width() <= 0.0 || area.height() <= 0.0 {
        return Vec::new();
    }
    let slot = (area.width() - BAR_GAP * (count as f32 - 1.0)) / count as f32;
    let bar_width = slot.max(1.0);
    let usable = (area.height() - BAR_MIN_HEIGHT).max(0.0);
    levels
        .iter()
        .enumerate()
        .map(|(index, level)| {
            let height = BAR_MIN_HEIGHT + usable * level.clamp(0.0, 1.0);
            let left = area.left() + index as f32 * (bar_width + BAR_GAP);
            Rect::from_min_max(
                egui::pos2(left, area.bottom() - height),
                egui::pos2(left + bar_width, area.bottom()),
            )
        })
        .collect()
}

/// 低柱偏暗、高柱更亮，靠亮度制造层次。
fn bar_color(level: f32) -> Color32 {
    let level = level.clamp(0.0, 1.0);
    let mix = |low: u8, high: u8| (low as f32 + (high - low) as f32 * level) as u8;
    Color32::from_rgb(mix(40, 96), mix(90, 200), mix(140, 255))
}

/// `m:ss`，超过一小时用 `h:mm:ss`。
pub fn format_time(ms: u64) -> String {
    let total = ms / 1000;
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::playlist::Track;
    use crate::audio::spectrum::BANDS;
    use crate::state::Page;
    use std::path::PathBuf;

    /// AI 工具与界面搜索共用的一份匹配：大小写不敏感、空词返回空。
    #[test]
    fn find_tracks_matches_case_insensitively() {
        let tracks = vec![
            Track {
                path: PathBuf::from("a.mp3"),
                title: "Blue Moon".to_string(),
                duration_ms: None,
            },
            Track {
                path: PathBuf::from("b.mp3"),
                title: "blue 的夜".to_string(),
                duration_ms: None,
            },
            Track {
                path: PathBuf::from("c.mp3"),
                title: "晴天".to_string(),
                duration_ms: None,
            },
        ];
        assert_eq!(find_tracks(&tracks, "BLUE"), vec![0, 1]);
        assert_eq!(find_tracks(&tracks, "  晴天 "), vec![2]);
        assert!(find_tracks(&tracks, "  ").is_empty(), "空查询不该命中全部");
        assert!(find_tracks(&tracks, "不存在").is_empty());
    }

    #[test]
    fn formats_short_and_long_durations() {
        assert_eq!(format_time(0), "0:00");
        assert_eq!(format_time(9_000), "0:09");
        assert_eq!(format_time(65_000), "1:05");
        assert_eq!(format_time(3_600_000), "1:00:00");
        assert_eq!(format_time(3_725_000), "1:02:05");
    }

    fn state_with_tracks() -> UiState {
        let mut state = UiState {
            page: Page::Music,
            ..Default::default()
        };
        state.music.dir = Some(PathBuf::from("C:/music"));
        state.music.tracks = vec![
            Track {
                path: PathBuf::from("C:/music/one.mp3"),
                title: "第一首".into(),
                duration_ms: Some(65_000),
            },
            Track {
                path: PathBuf::from("C:/music/two.flac"),
                title: "第二首".into(),
                duration_ms: Some(3_725_000),
            },
        ];
        state.music.current = Some(0);
        state.music.duration_ms = Some(65_000);
        state
    }

    #[derive(Default)]
    struct FrameOutput {
        texts: Vec<(String, f32, f32)>,
        /// 文本连同它的外接矩形，用来验证没画到界外。
        text_rects: Vec<(String, Rect)>,
        rects: Vec<Rect>,
    }

    impl FrameOutput {
        fn has(&self, needle: &str) -> bool {
            self.texts.iter().any(|(text, _, _)| text.contains(needle))
        }

        fn find(&self, needle: &str) -> Option<(f32, f32)> {
            self.texts
                .iter()
                .find(|(text, _, _)| text.contains(needle))
                .map(|(_, x, y)| (*x, *y))
        }

        fn text_rect(&self, needle: &str) -> Option<Rect> {
            self.text_rects
                .iter()
                .find(|(text, _)| text.contains(needle))
                .map(|(_, rect)| *rect)
        }
    }

    fn collect(shape: &egui::Shape, out: &mut FrameOutput) {
        match shape {
            egui::Shape::Text(text) => {
                out.texts
                    .push((text.galley.text().to_string(), text.pos.x, text.pos.y));
                out.text_rects.push((
                    text.galley.text().to_string(),
                    Rect::from_min_size(text.pos, text.galley.size()),
                ));
            }
            egui::Shape::Rect(rect) => out.rects.push(rect.rect),
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect(shape, out);
                }
            }
            _ => {}
        }
    }

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

    #[test]
    fn empty_state_shows_hints_not_panic() {
        let mut state = UiState {
            page: Page::Music,
            ..Default::default()
        };
        let mut commands = Vec::new();
        let out = run_frame(&mut state, 1000.0, 700.0, |state, ui| {
            show(state, &mut commands, ui);
            sidebar(state, &mut commands, ui);
        });
        assert!(out.has("选择一个目录开始播放"));
        assert!(out.has("未选择目录"));
        assert!(out.has("没有正在播放的音乐"));
    }

    #[test]
    fn sidebar_lists_titles_and_durations() {
        let mut state = state_with_tracks();
        let mut commands = Vec::new();
        let out = run_frame(&mut state, 240.0, 700.0, |state, ui| {
            sidebar(state, &mut commands, ui);
        });
        assert!(out.has("第一首"), "曲目名必须渲染");
        assert!(out.has("第二首"));
        assert!(out.has("1:05"), "短时长 m:ss");
        assert!(out.has("1:02:05"), "长时长 h:mm:ss");
        assert!(out.has("顺序") && out.has("随机"), "顺序切换在侧栏底部");
        assert!(out.has("2 首"));
    }

    #[test]
    fn album_card_shows_title_and_controls() {
        let mut state = state_with_tracks();
        state.music.now_playing = Some(crate::audio::tags::NowPlaying {
            path: PathBuf::from("C:/music/one.mp3"),
            title: Some("标签标题".into()),
            artist: Some("艺术家".into()),
            album: Some("专辑".into()),
            cover: None,
        });
        let mut commands = Vec::new();
        let out = run_frame(&mut state, 1000.0, 700.0, |state, ui| {
            show(state, &mut commands, ui);
        });
        assert!(out.has("标签标题"), "优先显示标签标题");
        assert!(out.has("艺术家 · 专辑"));
        assert!(out.has("已暂停"), "未播放时应显示暂停态");
        assert!(
            out.rects.iter().filter(|rect| rect.width() > 40.0).count() >= 3,
            "三个传输按钮各自占一块矩形"
        );
        assert!(out.has("0:00 / 1:05"), "进度与总时长");
    }

    #[test]
    fn album_area_sits_above_spectrum() {
        let mut state = state_with_tracks();
        // 用一个已知的柱高，确保频谱区确实画了东西。
        let frame = crate::audio::spectrum::SpectrumFrame {
            samples: (0..crate::audio::spectrum::FFT_SIZE)
                .map(|i| (i as f32 / 44_100.0 * 440.0 * std::f32::consts::TAU).sin() * 0.8)
                .collect(),
            sample_rate: 44_100,
        };
        for _ in 0..20 {
            state.music.analyzer.update(Some(&frame), 0.033);
        }

        let mut commands = Vec::new();
        let out = run_frame(&mut state, 1000.0, 700.0, |state, ui| {
            show(state, &mut commands, ui);
        });

        let title_y = out.find("第一首").expect("标题应渲染").1;
        // 柱子在标题下方：取最低的一批矩形，其顶边应明显低于标题。
        let bars: Vec<&Rect> = out.rects.iter().filter(|r| r.height() > 4.0).collect();
        let lowest_top = bars.iter().map(|rect| rect.top()).fold(f32::MIN, f32::max);
        assert!(
            lowest_top > title_y,
            "频谱必须在专辑页下方（柱顶 {lowest_top}，标题 {title_y}）"
        );
    }

    #[test]
    fn main_area_splits_seven_to_three() {
        // 常见窗口高度下：上半（专辑页）占七成，下半（频谱）占三成。
        for height in [400.0_f32, 520.0, 700.0, 900.0, 1100.0] {
            let (album, spectrum) = split_album_spectrum(height);
            let usable = height - PANEL_GAP;
            assert!(
                (album / usable - 0.7).abs() < 0.01,
                "{height} 下应是 7:3，实际 {album}/{usable}"
            );
            assert!(
                (album + spectrum - usable).abs() < 0.01,
                "两块之和应等于可用高度"
            );
        }
    }

    #[test]
    fn main_area_keeps_spectrum_readable_when_short() {
        // 窗口太矮时不能让频谱被压成一条缝。
        for height in [120.0_f32, 200.0, 260.0, 320.0] {
            let (album, spectrum) = split_album_spectrum(height);
            assert!(
                spectrum >= SPECTRUM_MIN_HEIGHT.min(height - PANEL_GAP) - 0.01,
                "{height} 下频谱区应保底，实际 {spectrum}"
            );
            assert!(album >= 0.0, "专辑页高度不能为负");
            assert!(
                (album + spectrum + PANEL_GAP - height).abs() < 0.01,
                "总高必须守恒"
            );
        }
    }

    #[test]
    fn icons_are_painted_not_typed() {
        let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(46.0, 34.0));
        let color = Color32::WHITE;
        // 播放是一个三角，暂停是两根竖条，上/下一首是三角 + 竖条。
        assert_eq!(icon_shapes(Icon::Play, rect, color).len(), 1);
        assert_eq!(icon_shapes(Icon::Pause, rect, color).len(), 2);
        assert_eq!(icon_shapes(Icon::Prev, rect, color).len(), 2);
        assert_eq!(icon_shapes(Icon::Next, rect, color).len(), 2);
        // 所有图元都落在按钮矩形内，不会画到邻居身上。
        for icon in [Icon::Play, Icon::Pause, Icon::Prev, Icon::Next] {
            for shape in icon_shapes(icon, rect, color) {
                let bounds = shape.visual_bounding_rect();
                assert!(
                    rect.contains_rect(bounds),
                    "{icon:?} 的图元越界：{bounds:?} 不在 {rect:?} 内"
                );
            }
        }
    }

    #[test]
    fn album_texts_stay_inside_the_window() {
        let mut state = state_with_tracks();
        state.music.now_playing = Some(crate::audio::tags::NowPlaying {
            path: PathBuf::from("C:/music/one.mp3"),
            title: Some("一个特别特别长的曲目名称用来验证截断不会撑破布局".into()),
            artist: Some("艺术家".into()),
            album: Some("专辑名称也很长很长".into()),
            cover: None,
        });
        let mut commands = Vec::new();
        let out = run_frame(&mut state, 1200.0, 800.0, |state, ui| {
            show(state, &mut commands, ui);
        });

        for needle in ["特别特别长", "艺术家", "/ 1:05"] {
            let rect = out
                .text_rect(needle)
                .unwrap_or_else(|| panic!("缺少文本 {needle}"));
            assert!(rect.right() <= 1200.0, "{needle} 超出窗口右边界：{rect:?}");
            assert!(rect.top() < 400.0, "{needle} 应在专辑页（上半部分）");
        }
        let title = out.text_rect("特别特别长").expect("标题");
        assert!(title.width() > 80.0, "标题被截得太狠：{:?}", title.width());
    }

    #[test]
    fn album_layout_never_overlaps_at_any_window_size() {
        // 覆盖最小窗口 900×600 到 1920×1080 之间的常见尺寸。
        for height in (300..=1000).step_by(50) {
            for width in (280..=1600).step_by(40) {
                let inner = Rect::from_min_size(
                    egui::pos2(200.0, 100.0),
                    egui::vec2(width as f32, height as f32 * 0.45),
                );
                let layout = album_layout(inner);
                assert!(
                    inner.contains_rect(layout.cover),
                    "封面越界：{:?} 不在 {:?}",
                    layout.cover,
                    inner
                );
                assert!(
                    (layout.cover.width() - layout.cover.height()).abs() < 0.01,
                    "封面必须是正方形"
                );
                let Some(info) = layout.info else {
                    continue;
                };
                assert!(inner.contains_rect(info.title), "标题区越界");
                assert!(inner.contains_rect(info.progress), "进度条区越界");
                assert!(inner.contains_rect(info.controls), "控件区越界");
                assert!(
                    layout.cover.right() < info.title.left(),
                    "封面与右列不得重叠"
                );
                assert!(
                    info.title.bottom() <= info.progress.top() + 0.01,
                    "标题区不得压到进度条"
                );
                assert!(
                    (info.progress.bottom() + BAND_GAP - info.controls.top()).abs() < 0.01,
                    "控件区必须紧贴在进度条正下方"
                );
                assert!(
                    info.controls.bottom() <= inner.bottom() + 0.01,
                    "控件区不得越出专辑页"
                );
                assert!(
                    (info.progress.width() - info.controls.width()).abs() < 0.01,
                    "进度条与控件区同宽，按钮才能居中在进度条中间"
                );
                assert!(info.title.height() > 0.0, "标题区必须有高度");
                assert!(info.title.width() >= 120.0, "右列太窄时应干脆不显示");
            }
        }
    }

    #[test]
    fn transport_row_is_centered_under_the_progress_bar() {
        for width in [140.0_f32, 200.0, 320.0, 460.0, 720.0] {
            let (offset, widths) = transport_row(width);
            let total: f32 = widths.iter().sum::<f32>() + BUTTON_GAP * 2.0;
            if total > width {
                assert_eq!(offset, 0.0, "放不下时从左排，不允许负偏移");
                continue;
            }
            let center = offset + total / 2.0;
            assert!(
                (center - width / 2.0).abs() < 0.01,
                "按钮组必须居中：中心 {center}，列中心 {}",
                width / 2.0
            );
        }
    }

    #[test]
    fn album_layout_hides_info_when_too_narrow() {
        let inner = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(200.0, 300.0));
        assert!(
            album_layout(inner).info.is_none(),
            "窄到挤不下文字时只画封面"
        );
    }

    #[test]
    fn bars_are_bottom_aligned_and_monotonic() {
        let area = Rect::from_min_size(egui::pos2(0.0, 100.0), egui::vec2(480.0, 200.0));
        let mut levels = [0.0_f32; BANDS];
        for (index, level) in levels.iter_mut().enumerate() {
            *level = index as f32 / BANDS as f32;
        }
        let rects = bar_rects(area, &levels);
        assert_eq!(rects.len(), BANDS);
        assert!(rects.iter().all(|rect| rect.bottom() == area.bottom()));
        assert!(rects.iter().all(|rect| rect.height() >= BAR_MIN_HEIGHT));
        // 柱高单调不减，宽度一致，互不重叠。
        for pair in rects.windows(2) {
            assert!(pair[1].height() >= pair[0].height());
            assert!((pair[1].width() - pair[0].width()).abs() < 0.01);
            assert!(pair[1].left() >= pair[0].right());
        }
    }

    #[test]
    fn silent_levels_keep_a_baseline() {
        let area = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(480.0, 200.0));
        let rects = bar_rects(area, &[0.0; BANDS]);
        assert!(rects.iter().all(|rect| rect.height() == BAR_MIN_HEIGHT));
    }

    #[test]
    fn bar_color_brightens_with_level() {
        let quiet = bar_color(0.0);
        let loud = bar_color(1.0);
        assert!(loud.b() > quiet.b() && loud.g() > quiet.g());
    }

    #[test]
    fn placeholder_colors_are_stable_per_title() {
        assert_eq!(placeholder_colors("夜曲"), placeholder_colors("夜曲"));
        assert_ne!(placeholder_colors("夜曲"), placeholder_colors("晴天"));
    }

    #[test]
    fn clicking_track_queues_play_command() {
        let mut state = state_with_tracks();
        let mut commands = Vec::new();

        // 先布局拿到坐标，再分别送 press / release（egui 命中依赖上一帧的矩形）。
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(240.0, 700.0));
        let make_input = |time: f64, events: Vec<egui::Event>| egui::RawInput {
            screen_rect: Some(screen),
            time: Some(time),
            events,
            ..Default::default()
        };
        let first = ctx.run(make_input(0.0, Vec::new()), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| sidebar(&mut state, &mut commands, ui));
        });
        let mut out = FrameOutput::default();
        for clipped in &first.shapes {
            collect(&clipped.shape, &mut out);
        }
        let (x, y) = out.find("第二首").expect("应渲染第二首");
        let pos = egui::pos2(x + 2.0, y + 6.0);

        for (time, pressed) in [(0.1, true), (0.2, false)] {
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
                egui::CentralPanel::default()
                    .show(ctx, |ui| sidebar(&mut state, &mut commands, ui));
            });
        }

        let plays: Vec<usize> = state
            .music
            .pending
            .iter()
            .filter_map(|command| match command {
                AudioCommand::Play { index, .. } => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(plays, vec![1], "点击第二行应请求播放索引 1");
    }

    #[test]
    fn scrubbing_overrides_reported_position() {
        let mut state = state_with_tracks();
        state.music.position_ms = 10_000;
        assert_eq!(state.music.display_position_ms(), 10_000);
        state.music.scrubbing = Some(42_000);
        assert_eq!(
            state.music.display_position_ms(),
            42_000,
            "拖动期间必须用本地值，不能被回传覆盖"
        );
    }

    #[test]
    fn device_error_disables_controls_but_still_renders() {
        let mut state = state_with_tracks();
        state.music.device_error = Some("没有可用的音频设备".into());
        let mut commands = Vec::new();
        let out = run_frame(&mut state, 1000.0, 700.0, |state, ui| {
            show(state, &mut commands, ui);
            sidebar(state, &mut commands, ui);
        });
        assert!(out.has("没有可用的音频设备"));
        assert!(out.has("第一首"), "设备错误也要能看到列表");
    }
}
