//! 阅读页：正文、已读切换、星标、附件下载/另存/打开、移入废件箱。

use std::path::PathBuf;

use eframe::egui::{self, Color32, RichText};

use base64::Engine as _;
use me_core::{Command, Flags, MessageBody, MessageSummary};

use crate::state::{
    ComposeState, ConfirmAction, ConfirmDialog, Dialog, HtmlJobView, ReaderMode, RenderedBlocks,
    ToastKind, UiState,
};
use crate::views::html::{self, HtmlRenderJob, HtmlRenderer};

pub fn show_window(
    state: &mut UiState,
    commands: &mut Vec<Command>,
    images: &crate::images::RemoteImageLoader,
    html_renderer: &HtmlRenderer,
    ctx: &egui::Context,
) {
    let Some(id) = state.selected_message else {
        return;
    };
    let Some(message) = state.messages.iter().find(|m| m.id == id).cloned() else {
        state.selected_message = None;
        return;
    };

    let mut open = true;
    let mut close = false;

    // 留出至少 80px 的余量，保证窗口永远不会和视口等高——等高时 egui 会把窗口位置
    // 约束回视口内，表现为“拖不动、松手弹回顶部”。
    let screen = ctx.screen_rect().size();
    egui::Window::new("阅读邮件")
        .id(egui::Id::new("me_reader_window"))
        .open(&mut open)
        .default_width(760.0)
        .default_height(620.0)
        .max_width((screen.x - 80.0).max(320.0))
        .max_height((screen.y - 80.0).max(240.0))
        .resizable(true)
        .collapsible(true)
        .show(ctx, |ui| {
            reader_content(
                state,
                commands,
                images,
                html_renderer,
                ui,
                &message,
                &mut close,
            );
        });

    if !open || close {
        state.selected_message = None;
        state.open_body = None;
        state.body_loading = false;
    }
}

/// 阅读窗口内部：头部（自适应）→ 正文（占满剩余高度）→ 附件（贴底）。
///
/// 附件必须用底部面板**预留固定高度**，不能像旧实现那样按 `header + 正文滚动区 +
/// 附件` 顺序堆叠：正文滚动区用 `auto_shrink=false` 会吃掉当前窗口的全部剩余高度，
/// 附件再追加到它下面，于是窗口内容的最小高度每帧都比窗口本身高一点；egui 的
/// `Resize` 每帧把窗口放大到 `last_content_size`，形成正反馈，直到窗口与视口等高
/// ——此时 egui 会把窗口位置约束在视口内，表现为“拖不动、松手弹回顶部”。
///
/// 面板高度按附件行数估算并封顶；估算偏小时面板内的 `ScrollArea` 会自己滚动，
/// 不会溢出窗口，因此布局高度恒定。
fn reader_content(
    state: &mut UiState,
    commands: &mut Vec<Command>,
    images: &crate::images::RemoteImageLoader,
    html_renderer: &HtmlRenderer,
    ui: &mut egui::Ui,
    message: &MessageSummary,
    close: &mut bool,
) {
    header(state, commands, ui, message, close);
    ui.separator();

    let attachment_count = state
        .open_body
        .as_ref()
        .map_or(0, |body| body.attachments.len());

    // 两个闭包都要改同一份 UI 状态，而闭包无法各自独占借用它，所以把状态作为
    // 参数传进闭包，而不是让闭包捕获。
    let mut shared = (state, commands);
    reader_layout(
        ui,
        attachment_count,
        &mut shared,
        |(state, commands), ui| attachments(state, commands, ui, message),
        |(state, _), ui| body_content(ui, state, images, html_renderer, message),
    );
}

/// 附件区最大高度；附件再多也只在自己的滚动区里滚动，不挤占正文。
const ATTACHMENTS_MAX_HEIGHT: f32 = 180.0;

fn reader_layout<C>(
    ui: &mut egui::Ui,
    attachment_count: usize,
    shared: &mut C,
    attachments_ui: impl FnOnce(&mut C, &mut egui::Ui),
    body_ui: impl FnOnce(&mut C, &mut egui::Ui),
) {
    if attachment_count > 0 {
        let height = attachments_panel_height(ui, attachment_count);
        egui::TopBottomPanel::bottom("reader_attachments_panel")
            .frame(egui::Frame::NONE)
            .exact_height(height)
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("reader_attachments_scroll")
                    .auto_shrink([false, true])
                    .show(ui, |ui| attachments_ui(shared, ui));
            });
    }

    // 正文滚动区放在面板之后，占满剩余高度；之后不再追加任何控件。
    let available = ui.available_size();
    body_scroll_area(available).show(ui, |ui| body_ui(shared, ui));
}

/// 估算附件区高度：“附件”标题一行 + 每个附件一行，封顶 `ATTACHMENTS_MAX_HEIGHT`。
fn attachments_panel_height(ui: &egui::Ui, count: usize) -> f32 {
    let text_height = ui.text_style_height(&egui::TextStyle::Body);
    let row_height = text_height.max(ui.spacing().interact_size.y);
    let spacing = ui.spacing().item_spacing.y;
    (text_height + spacing + count as f32 * (row_height + spacing)).min(ATTACHMENTS_MAX_HEIGHT)
}

fn header(
    state: &mut UiState,
    commands: &mut Vec<Command>,
    ui: &mut egui::Ui,
    message: &MessageSummary,
    close: &mut bool,
) {
    // 主题单独一行并自动换行。若与按钮放在同一个 horizontal 中，
    // 标题会先占满宽度，右侧按钮拿到 0 宽而叠在标题上。
    ui.add(
        egui::Label::new(
            RichText::new(message.display_subject())
                .heading()
                .size(20.0),
        )
        .wrap(),
    );
    ui.add_space(6.0);

    // 操作按钮单独一行，空间不足时自动换行。
    ui.horizontal_wrapped(|ui| {
        if ui.button("回复").clicked() {
            *close = true;
            start_reply(state, message);
        }

        let starred = message.flags.is_flagged();
        if ui
            .button(if starred { "★ 取消星标" } else { "☆ 星标" })
            .clicked()
        {
            commands.push(Command::SetFlags {
                ids: vec![message.id],
                flags: me_core::FlagOp::toggle(Flags::FLAGGED, !starred),
            });
        }

        if message.flags.is_seen() {
            if ui.button("标记为未读").clicked() {
                commands.push(Command::SetFlags {
                    ids: vec![message.id],
                    flags: me_core::FlagOp::Clear(Flags::SEEN),
                });
            }
        } else if ui.button("标记为已读").clicked() {
            commands.push(Command::SetFlags {
                ids: vec![message.id],
                flags: me_core::FlagOp::Set(Flags::SEEN),
            });
        }

        if ui.button("移入废件箱").clicked() {
            let has_trash = state
                .selected_account
                .and_then(|a| state.trash_folder(a))
                .is_some();
            if !has_trash {
                state.toast(
                    ToastKind::Warn,
                    "该账户没有废件箱角色，无法删除。请先在服务器端创建废件箱，或在文件夹右键菜单指定角色。",
                );
            } else {
                state.dialog = Dialog::Confirm(ConfirmDialog {
                    title: "移入废件箱".into(),
                    body: format!(
                        "确定把“{}”移入废件箱？v0.1 不提供一键恢复，移回需在服务器端或其他客户端操作。",
                        message.display_subject()
                    ),
                    action: ConfirmAction::MoveToTrash { ids: vec![message.id] },
                    danger: false,
                });
            }
        }

        if ui.button("关闭").clicked() {
            *close = true;
        }
    });

    ui.add_space(4.0);
    egui::Grid::new("reader_headers")
        .num_columns(2)
        .spacing([12.0, 4.0])
        .show(ui, |ui| {
            ui.label(RichText::new("发件人").color(Color32::GRAY));
            ui.label(message.display_from());
            ui.end_row();

            ui.label(RichText::new("收件人").color(Color32::GRAY));
            ui.label(join_addresses(&message.to));
            ui.end_row();

            if !message.cc.is_empty() {
                ui.label(RichText::new("抄送").color(Color32::GRAY));
                ui.label(join_addresses(&message.cc));
                ui.end_row();
            }

            ui.label(RichText::new("时间").color(Color32::GRAY));
            ui.label(
                message
                    .date
                    .map(me_core::format_local_datetime)
                    .unwrap_or_else(|| "—".to_string()),
            );
            ui.end_row();

            if let Some(size) = message.size {
                ui.label(RichText::new("大小").color(Color32::GRAY));
                ui.label(crate::app::human_bytes(size as u64));
                ui.end_row();
            }
        });
}

/// 正文滚动区：双轴滚动，外框尺寸锁定在当前窗格的可用空间。
///
/// 只开垂直滚动时，egui 会按内容宽度放大 ScrollArea，进而被窗口的
/// `Resize` 采纳为 `last_content_size`；宽 HTML 会把阅读窗口一路撑到
/// 应用/屏幕边界。横向也开启滚动后，超宽内容改为在正文区内滚动。
fn body_scroll_area(available: egui::Vec2) -> egui::ScrollArea {
    egui::ScrollArea::both()
        .id_salt("reader_body")
        .auto_shrink([false, false])
        .max_width(available.x.max(120.0))
        .max_height(available.y.max(120.0))
}

fn body_content(
    ui: &mut egui::Ui,
    state: &mut UiState,
    images: &crate::images::RemoteImageLoader,
    html_renderer: &HtmlRenderer,
    message: &MessageSummary,
) {
    if state.body_loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new("正在获取正文…").color(Color32::GRAY));
        });
        return;
    }
    if state.open_body.is_none() {
        if !message.body_cached {
            ui.label(RichText::new("正文尚未缓存，打开后将从服务器下载。").color(Color32::GRAY));
        }
        return;
    }

    let has_html = state
        .open_body
        .as_ref()
        .and_then(|body| body.html.as_deref())
        .is_some_and(|html| !html.trim().is_empty());

    // 视图切换：HTML 富文本 / 纯文本 / Markdown。
    ui.horizontal(|ui| {
        let rich = ui
            .add_enabled(
                has_html,
                egui::Button::new("富文本")
                    .selected(state.reader_mode == ReaderMode::Rich && has_html),
            )
            .on_disabled_hover_text("该邮件没有 text/html 正文");
        if rich.clicked() {
            state.reader_mode = ReaderMode::Rich;
            state.reader_blocks = None;
        }
        if ui
            .add(egui::Button::new("纯文本").selected(
                state.reader_mode == ReaderMode::Plain
                    || (state.reader_mode == ReaderMode::Rich && !has_html),
            ))
            .clicked()
        {
            state.reader_mode = ReaderMode::Plain;
            state.reader_blocks = None;
        }
        if ui
            .add(egui::Button::new("Markdown").selected(state.reader_mode == ReaderMode::Markdown))
            .clicked()
        {
            state.reader_mode = ReaderMode::Markdown;
            state.reader_blocks = None;
        }
        if !has_html && state.reader_mode == ReaderMode::Rich {
            ui.label(
                RichText::new("（无 HTML 正文，已回退为纯文本）")
                    .small()
                    .color(Color32::GRAY),
            );
        }
    });
    ui.add_space(4.0);

    let mode = state.reader_mode;
    let use_rich = mode == ReaderMode::Markdown || (mode == ReaderMode::Rich && has_html);

    // 富文本 v2：后台解析；旧 generation 由 app 回收时丢弃。
    if state.html_renderer == me_core::HtmlRendererMode::Enhanced && use_rich {
        let has_result = state
            .html_rendered
            .as_ref()
            .is_some_and(|result| result.message == message.id);
        let pending = state.html_job.as_ref().map(|job| job.message);
        if !has_result
            && pending != Some(message.id)
            && let Some(source) = state
                .open_body
                .as_ref()
                .and_then(|body| body.html.as_deref())
                .filter(|html| !html.trim().is_empty())
                .map(str::to_string)
        {
            let generation = state.next_html_generation;
            state.next_html_generation = state.next_html_generation.wrapping_add(1);
            state.html_job = Some(HtmlJobView {
                message: message.id,
                generation,
                started: std::time::Instant::now(),
            });
            html_renderer.submit(HtmlRenderJob {
                message: message.id,
                generation,
                html: source,
            });
        }
    }

    // 只在消息或视图变化时解析一次，避免每帧重新解析正文。
    let needs_parse = !matches!(
        &state.reader_blocks,
        Some(cache) if cache.message == message.id && cache.mode == mode
    );
    if needs_parse {
        let blocks = {
            let Some(body) = state.open_body.as_ref() else {
                return;
            };
            match mode {
                ReaderMode::Rich => body
                    .html
                    .as_deref()
                    .map(crate::views::richtext::html_to_blocks)
                    .unwrap_or_default(),
                ReaderMode::Markdown => crate::views::richtext::markdown_to_blocks(&body.text),
                ReaderMode::Plain => Vec::new(),
            }
        };
        state.reader_blocks = Some(RenderedBlocks {
            message: message.id,
            mode,
            blocks,
        });
    }

    let image_sources: Vec<String> = state
        .html_rendered
        .as_ref()
        .filter(|result| result.message == message.id)
        .and_then(|result| result.result.as_ref().ok())
        .map(html::collect_image_sources)
        .unwrap_or_else(|| {
            state
                .reader_blocks
                .as_ref()
                .map(|cache| {
                    cache
                        .blocks
                        .iter()
                        .filter_map(|block| match block {
                            crate::views::richtext::Block::Image { src, .. } => Some(src.clone()),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default()
        });
    ensure_images(state, message.id, ui.ctx(), images, image_sources);

    let Some(body) = state.open_body.as_ref() else {
        return;
    };
    let blocks = state
        .reader_blocks
        .as_ref()
        .map(|cache| cache.blocks.as_slice())
        .unwrap_or(&[]);

    if use_rich {
        if let Some(job) = state
            .html_job
            .as_ref()
            .filter(|job| job.message == message.id)
            && job.started.elapsed() < html::UI_FALLBACK_AFTER
        {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    RichText::new("正在排版富文本…")
                        .small()
                        .color(Color32::GRAY),
                );
            });
        }
        let textures = crate::views::richtext::ImageTextures {
            textures: &state.reader_textures,
            pending: &state.remote_pending,
            failed: &state.remote_failed,
            remote_disabled: !state.remote_images,
        };
        let rendered = state.html_rendered.as_ref().and_then(|result| {
            if result.message == message.id {
                result.result.as_ref().ok()
            } else {
                None
            }
        });
        if let Some(rendered) = rendered {
            html::show(ui, rendered, Some(&textures));
        } else {
            crate::views::richtext::show_with_images(ui, blocks, Some(&textures));
            if blocks.is_empty() {
                ui.label(RichText::new("（正文为空）").color(Color32::GRAY).italics());
            }
        }
    } else if body.text.trim().is_empty() {
        ui.label(RichText::new("（正文为空）").color(Color32::GRAY).italics());
    } else {
        ui.add(
            egui::Label::new(
                RichText::new(&body.text)
                    .family(egui::FontFamily::Proportional)
                    .size(14.0),
            )
            .wrap(),
        );
    }
}

fn attachments(
    state: &mut UiState,
    commands: &mut Vec<Command>,
    ui: &mut egui::Ui,
    message: &MessageSummary,
) {
    let Some(body) = state.open_body.as_ref() else {
        return;
    };
    if body.attachments.is_empty() {
        return;
    }

    ui.label(RichText::new("附件").strong());
    let mut action: Option<(u32, AttachmentAction)> = None;
    for attachment in &body.attachments {
        ui.horizontal(|ui| {
            ui.label("📎");
            ui.label(attachment.display_name());
            if let Some(size) = attachment.size {
                ui.label(
                    RichText::new(crate::app::human_bytes(size as u64))
                        .small()
                        .color(Color32::GRAY),
                );
            }
            if let Some(path) = &attachment.local_path {
                if ui.small_button("打开").clicked() {
                    action = Some((attachment.part_index, AttachmentAction::Open(path.clone())));
                }
                if ui.small_button("另存为…").clicked() {
                    action = Some((
                        attachment.part_index,
                        AttachmentAction::SaveAs(path.clone()),
                    ));
                }
                ui.label(RichText::new("已下载").small().color(Color32::GRAY));
            } else if ui.small_button("下载").clicked() {
                action = Some((attachment.part_index, AttachmentAction::Download));
            }
        });
    }

    if let Some((part_index, action)) = action {
        match action {
            AttachmentAction::Download => {
                commands.push(Command::DownloadAttachment {
                    message: message.id,
                    part_index,
                });
            }
            AttachmentAction::Open(path) => {
                state.dialog = Dialog::Confirm(ConfirmDialog {
                    title: "打开附件".into(),
                    body: "附件可能包含恶意内容。请确认来源可信后再打开。".into(),
                    action: ConfirmAction::OpenAttachment { path },
                    danger: true,
                });
            }
            AttachmentAction::SaveAs(path) => {
                let mut dialog = rfd::FileDialog::new().set_file_name(
                    path.file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_else(|| "attachment".into()),
                );
                if let Some(dir) = state.attachment_dir.as_deref() {
                    dialog = dialog.set_directory(dir);
                }
                if let Some(target) = dialog.save_file() {
                    match std::fs::copy(&path, &target) {
                        Ok(_) => state
                            .toast(ToastKind::Success, format!("已保存到 {}", target.display())),
                        Err(err) => state.toast(ToastKind::Error, format!("保存失败：{err}")),
                    }
                }
            }
        }
    }
}

enum AttachmentAction {
    Download,
    Open(PathBuf),
    SaveAs(PathBuf),
}

fn join_addresses(addrs: &[me_core::Address]) -> String {
    if addrs.is_empty() {
        "—".to_string()
    } else {
        addrs
            .iter()
            .map(me_core::Address::display)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn start_reply(state: &mut UiState, message: &MessageSummary) {
    let Some(account) = state.selected_account_struct() else {
        return;
    };
    let mut compose = ComposeState::new(account.id, &account.email);
    if let Some(from) = &message.from {
        compose.to = from.display();
    }
    let subject = message.display_subject();
    compose.subject = if subject.to_ascii_lowercase().starts_with("re:") {
        subject.to_string()
    } else {
        format!("Re: {subject}")
    };
    if let Some(body) = &state.open_body {
        let quoted = body
            .text
            .lines()
            .map(|line| format!("> {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        compose.body = format!("\n\n{quoted}\n");
    }
    state.dialog = Dialog::Compose(compose);
}

// ---------------------------------------------------------------------------
// 图片解析
// ---------------------------------------------------------------------------

/// 把正文里的 `cid:` / `data:` 图片解码成 egui 纹理并缓存。
fn ensure_images(
    state: &mut UiState,
    message: me_core::MessageId,
    ctx: &egui::Context,
    loader: &crate::images::RemoteImageLoader,
    sources: Vec<String>,
) {
    if state.reader_texture_message != Some(message) {
        state.reader_textures.clear();
        state.remote_pending.clear();
        state.remote_failed.clear();
        state.reader_texture_message = Some(message);
    }
    if sources.is_empty() {
        return;
    }
    let Some(body) = state.open_body.as_ref() else {
        return;
    };
    for src in sources {
        // 关闭远程图片后不再请求，也不显示已缓存的远程纹理。
        if !state.remote_images && is_remote_src(&src) {
            state.reader_textures.remove(&src);
            state.remote_pending.remove(&src);
            continue;
        }
        if state.reader_textures.contains_key(&src) {
            continue;
        }
        if let Some(bytes) = resolve_image_bytes(body, &src) {
            if let Some(texture) = decode_texture(ctx, &src, &bytes) {
                state.reader_textures.insert(src, texture);
            }
            continue;
        }

        // 远程图片：异步抓取，返回后唤醒重绘，下一帧解码为纹理。
        if is_remote_src(&src) {
            let url = normalize_remote_url(&src);
            match loader.lookup(&url) {
                Some(crate::images::RemoteImage::Ready(bytes)) => {
                    if let Some(texture) = decode_texture(ctx, &src, &bytes) {
                        state.reader_textures.insert(src.clone(), texture);
                    }
                    state.remote_pending.remove(&src);
                    state.remote_failed.remove(&src);
                }
                Some(crate::images::RemoteImage::Loading) => {
                    state.remote_pending.insert(src.clone());
                    state.remote_failed.remove(&src);
                }
                Some(crate::images::RemoteImage::Failed) => {
                    state.remote_pending.remove(&src);
                    state.remote_failed.insert(src.clone());
                }
                None => {
                    loader.request(&url);
                    state.remote_pending.insert(src.clone());
                    state.remote_failed.remove(&src);
                }
            }
        }
    }
}

fn is_remote_src(src: &str) -> bool {
    let src = src.trim();
    src.starts_with("http://") || src.starts_with("https://") || src.starts_with("//")
}

fn normalize_remote_url(src: &str) -> String {
    let src = src.trim();
    if src.starts_with("//") {
        format!("https:{src}")
    } else {
        src.to_string()
    }
}

fn resolve_image_bytes(body: &MessageBody, src: &str) -> Option<Vec<u8>> {
    let src = src.trim();
    if let Some(rest) = src.strip_prefix("cid:") {
        let wanted = normalize_cid(rest);
        return body
            .inline_images
            .iter()
            .find(|image| {
                normalize_cid(image.content_id.as_deref().unwrap_or_default())
                    .eq_ignore_ascii_case(&wanted)
            })
            .map(|image| image.bytes.clone());
    }
    if let Some(rest) = src.strip_prefix("data:") {
        let (meta, payload) = rest.split_once(',')?;
        if !meta.to_ascii_lowercase().contains(";base64") {
            return None;
        }
        return base64::engine::general_purpose::STANDARD
            .decode(payload.trim())
            .ok();
    }
    None
}

fn normalize_cid(value: &str) -> String {
    percent_decode(value.trim())
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim()
        .to_string()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
        {
            out.push(high * 16 + low);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_texture(ctx: &egui::Context, key: &str, bytes: &[u8]) -> Option<egui::TextureHandle> {
    let color = decode_raster(bytes).or_else(|| decode_svg(bytes))?;
    Some(ctx.load_texture(
        format!("reader-image:{key}"),
        color,
        egui::TextureOptions::LINEAR,
    ))
}

fn decode_raster(bytes: &[u8]) -> Option<egui::ColorImage> {
    let image = image::load_from_memory(bytes).ok()?;
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 {
        return None;
    }
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [width as usize, height as usize],
        rgba.as_raw(),
    ))
}

/// 邮件里的 SVG（logo、签名图）用 resvg 光栅化；像素按 egui 的预乘格式传入。
fn decode_svg(bytes: &[u8]) -> Option<egui::ColorImage> {
    if !looks_like_svg(bytes) {
        return None;
    }
    let tree = resvg::usvg::Tree::from_data(bytes, &resvg::usvg::Options::default()).ok()?;
    let size = tree.size().to_int_size();
    let width = size.width().max(1);
    let height = size.height().max(1);

    // 限制最大边长，避免恶意 SVG 申请超大纹理。
    const MAX_DIM: f32 = 2048.0;
    let scale = (MAX_DIM / width as f32)
        .min(MAX_DIM / height as f32)
        .min(1.0);
    let target_width = ((width as f32 * scale).round() as u32).max(1);
    let target_height = ((height as f32 * scale).round() as u32).max(1);

    let mut pixmap = resvg::tiny_skia::Pixmap::new(target_width, target_height)?;
    let transform = resvg::tiny_skia::Transform::from_scale(
        target_width as f32 / width as f32,
        target_height as f32 / height as f32,
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Some(egui::ColorImage::from_rgba_premultiplied(
        [target_width as usize, target_height as usize],
        pixmap.data(),
    ))
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(2048)];
    let Ok(text) = std::str::from_utf8(head) else {
        return false;
    };
    let lower = text.trim_start().to_ascii_lowercase();
    lower.starts_with("<svg") || (lower.starts_with("<?xml") && lower.contains("<svg"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_core::{Attachment, InlineImage, MessageId};

    fn body_with_image() -> MessageBody {
        MessageBody {
            message: MessageId(1),
            text: String::new(),
            html: None,
            inline_images: vec![InlineImage {
                content_id: Some("logo@example.com".into()),
                filename: None,
                mime_type: Some("image/png".into()),
                bytes: vec![0x89, b'P', b'N', b'G'],
            }],
            attachments: Vec::<Attachment>::new(),
            raw_cached: true,
        }
    }

    #[test]
    fn rasterizes_simple_svg() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2">
            <rect width="2" height="2" fill="#ff0000"/>
        </svg>"##;
        let image = decode_svg(svg).expect("SVG 应能光栅化");
        assert_eq!(image.size, [2, 2]);
        assert!(
            image
                .pixels
                .iter()
                .any(|pixel| pixel.r() > 200 && pixel.g() < 50),
            "红色矩形未渲染：{:?}",
            image.pixels
        );
    }

    #[test]
    fn detects_svg_payloads() {
        assert!(looks_like_svg(
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>"
        ));
        assert!(looks_like_svg(
            b"<?xml version=\"1.0\"?><svg width=\"1\" height=\"1\"></svg>"
        ));
        assert!(!looks_like_svg(b"\x89PNG\r\n\x1a\n"));
        assert!(!looks_like_svg(b"<html><body>no</body></html>"));
    }

    #[test]
    fn resolves_cid_with_percent_encoding_and_case() {
        let body = body_with_image();
        assert_eq!(
            resolve_image_bytes(&body, "cid:LOGO%40example.com").as_deref(),
            Some(&[0x89, b'P', b'N', b'G'][..])
        );
        assert!(resolve_image_bytes(&body, "cid:missing@example.com").is_none());
    }

    #[test]
    fn resolves_data_uri_base64() {
        let body = body_with_image();
        let bytes =
            resolve_image_bytes(&body, "data:image/png;base64,iVBORw0KGgo=").expect("data uri");
        assert_eq!(&bytes[..4], &[0x89, b'P', b'N', b'G']);
    }

    #[test]
    fn ignores_remote_and_unknown_sources() {
        let body = body_with_image();
        assert!(resolve_image_bytes(&body, "https://tracker.example/p.png").is_none());
        assert!(resolve_image_bytes(&body, "file:///C:/secret.png").is_none());
    }

    #[test]
    fn body_scroll_area_does_not_expand_for_wide_content() {
        // 回归：宽 HTML 不应把阅读窗口撑满整个应用；超宽内容只在正文区内滚动。
        let ctx = egui::Context::default();
        let probe = egui::Id::new("reader_wide_content_probe");
        let mut size = egui::Vec2::ZERO;
        for _ in 0..8 {
            let _ = ctx.run(Default::default(), |ctx| {
                egui::Window::new("阅读邮件")
                    .id(probe)
                    .default_width(760.0)
                    .default_height(620.0)
                    .resizable(true)
                    .show(ctx, |ui| {
                        body_scroll_area(ui.available_size()).show(ui, |ui| {
                            ui.allocate_space(egui::vec2(4000.0, 2000.0));
                        });
                    });
            });
            if let Some(rect) = ctx.memory(|m| m.area_rect(probe)) {
                size = rect.size();
            }
        }
        assert!(
            size.x < 900.0,
            "宽内容不应撑开阅读窗口，实际宽度 {:.1}",
            size.x
        );
    }

    #[test]
    fn attachments_do_not_grow_reader_window() {
        // 回归：正文滚动区占满剩余高度后，附件若再顺序追加到下面，会让窗口内容
        // 最小高度每帧大于窗口高度，egui 的 Resize 逐帧放大窗口，最终与视口等高
        // 而无法拖动。附件必须走底部面板预留空间。
        let ctx = egui::Context::default();
        let probe = egui::Id::new("reader_attachments_growth_probe");
        let mut size = egui::Vec2::ZERO;
        for _ in 0..8 {
            let _ = ctx.run(Default::default(), |ctx| {
                egui::Window::new("阅读邮件")
                    .id(probe)
                    .default_size([760.0, 620.0])
                    .resizable(true)
                    .show(ctx, |ui| {
                        ui.label("主题");
                        ui.allocate_space(egui::vec2(0.0, 120.0));
                        ui.separator();
                        reader_layout(
                            ui,
                            20,
                            &mut (),
                            |_, ui| {
                                for index in 0..20 {
                                    ui.label(format!("📎 attachment-{index}.bin"));
                                }
                            },
                            |_, ui| {
                                ui.allocate_space(egui::vec2(400.0, 4000.0));
                            },
                        );
                    });
            });
            if let Some(rect) = ctx.memory(|m| m.area_rect(probe)) {
                size = rect.size();
            }
        }
        assert!(
            size.y <= 700.0,
            "附件区不应把阅读窗口逐帧撑高，实际高度 {:.1}",
            size.y
        );
    }
}
