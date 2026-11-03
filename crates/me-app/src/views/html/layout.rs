//! 邮件 HTML 的排版与绘制引擎。
//!
//! 排版分两步：先按内容宽度测量出每个块、行、表格单元格的绝对矩形
//! （[`LayoutDoc`]），再一次性绘制。文本使用 egui 的 `LayoutJob/Galley`
//! （断行、CJK、字体度量与选区都由 egui 负责），但**一个段落只生成一个
//! galley**，不再把每个行内片段拼成独立控件。背景、边框、图片、占位符由
//! painter 按文档顺序绘制。
//!
//! 这样做的目的：邮件常见的 `<table><tr><td>` + `display:inline-block`
//! 结构必须按块级文档流排布；嵌套 `Ui` 无法可靠表达 CSS 的宽度、行高、
//! 表格跨行跨列与对齐。

use std::ops::Range;
use std::sync::Arc;

use eframe::egui::text::{LayoutJob, LayoutSection, TextFormat};
use eframe::egui::{
    self, Align, Color32, CornerRadius, CursorIcon, FontId, Galley, Pos2, Rect, Sense, Stroke,
    StrokeKind, Vec2, pos2, vec2,
};

use crate::views::richtext::ImageTextures;

use super::css::{Edges, FontFamily as CssFontFamily, Length, TextAlign, TextStyle, VerticalAlign};
use super::{Block, BlockStyle, ImageRef, Inline, RenderedHtml, Table, TableCell};

/// 正文阅读宽度上限；窗口更宽时内容列居中，避免一行文字过长。
const MAX_CONTENT_WIDTH: f32 = 960.0;
const PAGE_PADDING_X: f32 = 12.0;
const PAGE_PADDING_Y: f32 = 10.0;
const IMAGE_GAP: f32 = 4.0;
const IMAGE_ROW_GAP: f32 = 2.0;
const LIST_MARKER_GAP: f32 = 8.0;

// ---------------------------------------------------------------------------
// 绘制条目
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct LinkHit {
    rect: Rect,
    url: String,
}

#[derive(Debug, Clone)]
struct ChipSpec {
    galley: Arc<Galley>,
    bg: Color32,
    fg: Color32,
    url: Option<String>,
    alt: String,
}

#[derive(Debug, Clone)]
struct ResolvedImage {
    size: Vec2,
    texture: Option<egui::TextureId>,
    link: Option<String>,
    alt: String,
    radius: f32,
    chip: Option<ChipSpec>,
}

#[derive(Debug)]
enum Item {
    Fill {
        rect: Rect,
        color: Color32,
        radius: f32,
    },
    Border {
        rect: Rect,
        stroke: Stroke,
        radius: f32,
    },
    Text {
        rect: Rect,
        galley: Arc<Galley>,
        links: Vec<LinkHit>,
    },
    Image {
        rect: Rect,
        texture: egui::TextureId,
        link: Option<String>,
        alt: String,
        radius: f32,
    },
    Chip {
        rect: Rect,
        text_pos: Pos2,
        text: Arc<Galley>,
        bg: Color32,
        fg: Color32,
        url: Option<String>,
        alt: String,
    },
}

#[derive(Debug, Default)]
struct Doc {
    items: Vec<Item>,
}

impl Doc {
    fn extend(&mut self, other: Doc) {
        self.items.extend(other.items);
    }
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

pub fn show(ui: &mut egui::Ui, rendered: &RenderedHtml, images: Option<&ImageTextures<'_>>) {
    if rendered.blocks.is_empty() {
        ui.label(
            egui::RichText::new("（正文为空）")
                .color(Color32::GRAY)
                .italics(),
        );
        return;
    }

    let available = ui.available_width();
    let available = if available.is_finite() {
        available.max(160.0)
    } else {
        720.0
    };
    let origin = ui.cursor().min;

    let surface = rendered
        .page_background
        .unwrap_or_else(|| ui.visuals().panel_fill);
    let dark = is_dark(surface);
    let fallback = if dark {
        Color32::from_gray(232)
    } else {
        Color32::from_gray(34)
    };
    let weak = if dark {
        Color32::from_gray(165)
    } else {
        Color32::from_gray(118)
    };
    let code_bg = if dark {
        Color32::from_rgb(56, 58, 62)
    } else {
        Color32::from_rgb(243, 244, 246)
    };

    let cx = LayoutCx {
        ui,
        images,
        fallback,
        weak,
        code_bg,
        dark,
    };
    let (doc, height) = cx.layout_document(rendered, origin, available);
    let doc_rect = Rect::from_min_size(origin, vec2(available, height.max(1.0)));

    paint(ui, doc, doc_rect, rendered.page_background);
    ui.advance_cursor_after_rect(doc_rect);

    if rendered.truncated {
        ui.label(
            egui::RichText::new("正文过长，已截断显示")
                .small()
                .color(Color32::from_rgb(190, 140, 40)),
        );
    }
}

fn paint(ui: &mut egui::Ui, doc: Doc, doc_rect: Rect, page_background: Option<Color32>) {
    let painter = ui.painter().clone();
    if let Some(background) = page_background {
        painter.rect_filled(doc_rect, 0.0, background);
    }

    for (index, item) in doc.items.into_iter().enumerate() {
        match item {
            Item::Fill {
                rect,
                color,
                radius,
            } => {
                let radius = corner_radius(radius, rect);
                painter.rect_filled(rect, radius, color);
            }
            Item::Border {
                rect,
                stroke,
                radius,
            } => {
                let radius = corner_radius(radius, rect);
                painter.rect_stroke(rect, radius, stroke, StrokeKind::Inside);
            }
            Item::Text {
                rect,
                galley,
                links,
            } => {
                // 交给 Label 绘制以保留 egui 的跨控件文本选择。
                ui.place(rect, egui::Label::new(galley).selectable(true));
                for (link_index, link) in links.into_iter().enumerate() {
                    let id = ui.id().with(("html-link", index, link_index));
                    let response = ui.interact(link.rect, id, Sense::click());
                    if response.hovered() {
                        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                    }
                    if response.clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(link.url.clone()));
                    }
                    response.on_hover_text(&link.url);
                }
            }
            Item::Image {
                rect,
                texture,
                link,
                alt,
                radius,
            } => {
                if radius > 0.0 {
                    let image =
                        egui::Image::new(egui::load::SizedTexture::new(texture, rect.size()))
                            .fit_to_exact_size(rect.size())
                            .corner_radius(corner_radius(radius, rect));
                    ui.place(rect, image);
                } else {
                    painter.image(
                        texture,
                        rect,
                        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                }
                let id = ui.id().with(("html-image", index));
                let response = ui.interact(rect, id, Sense::click());
                if let Some(url) = link {
                    if response.hovered() {
                        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                    }
                    if response.clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(url.clone()));
                    }
                    response.on_hover_text(if alt.is_empty() { url } else { alt });
                } else if !alt.is_empty() {
                    response.on_hover_text(&alt);
                }
            }
            Item::Chip {
                rect,
                text_pos,
                text,
                bg,
                fg,
                url,
                alt,
            } => {
                painter.rect_filled(rect, 5.0, bg);
                painter.galley(text_pos, text, fg);
                let id = ui.id().with(("html-image-chip", index));
                let response = ui.interact(rect, id, Sense::click());
                if let Some(url) = url {
                    if response.hovered() {
                        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                    }
                    if response.clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(url.clone()));
                    }
                    response.on_hover_text(url);
                } else if !alt.is_empty() {
                    response.on_hover_text(&alt);
                }
            }
        }
    }
}

fn corner_radius(radius: f32, rect: Rect) -> CornerRadius {
    let limit = rect.width().min(rect.height()).max(0.0) / 2.0;
    let radius = if radius.is_finite() {
        radius.clamp(0.0, limit)
    } else {
        limit
    };
    CornerRadius::from(radius)
}

fn is_dark(color: Color32) -> bool {
    let [red, green, blue, _] = color.to_array();
    let luminance = 0.2126 * f32::from(red) + 0.7152 * f32::from(green) + 0.0722 * f32::from(blue);
    luminance < 118.0
}

// ---------------------------------------------------------------------------
// 排版上下文
// ---------------------------------------------------------------------------

struct LayoutCx<'a, 'b> {
    ui: &'a egui::Ui,
    images: Option<&'a ImageTextures<'b>>,
    fallback: Color32,
    weak: Color32,
    code_bg: Color32,
    dark: bool,
}

struct BlockBox {
    rect_left: f32,
    rect_top: f32,
    rect_width: f32,
    pad_top: f32,
    pad_bottom: f32,
    content_x: f32,
    content_y: f32,
    content_w: f32,
}

struct LinkSpan {
    range: Range<usize>,
    url: String,
}

impl LayoutCx<'_, '_> {
    // -- 文本 ---------------------------------------------------------------

    fn font_id(&self, style: &TextStyle, base: f32) -> FontId {
        let size = style.font_size.unwrap_or(base).clamp(6.0, 120.0);
        let family = if style.code || style.font_family == CssFontFamily::Monospace {
            crate::fonts::email_monospace()
        } else if style.bold {
            crate::fonts::email_family(true)
        } else {
            crate::fonts::email_family(false)
        };
        FontId::new(size, family)
    }

    fn text_format(&self, style: &TextStyle, base: f32) -> TextFormat {
        let font_id = self.font_id(style, base);
        let color = style.color.unwrap_or(self.fallback);
        let size = font_id.size;
        let line_height = (size * style.line_height.unwrap_or(1.45))
            .round()
            .max(size * 1.05);
        let mut background = style.background.unwrap_or(Color32::TRANSPARENT);
        if style.code && background == Color32::TRANSPARENT {
            background = self.code_bg;
        }
        TextFormat {
            font_id,
            extra_letter_spacing: style.letter_spacing.unwrap_or(0.0).clamp(-2.0, 8.0),
            line_height: Some(line_height),
            color,
            background,
            expand_bg: 1.0,
            italics: style.italic,
            underline: if style.underline {
                Stroke::new(1.0_f32, color)
            } else {
                Stroke::NONE
            },
            strikethrough: if style.strike {
                Stroke::new(1.0_f32, color)
            } else {
                Stroke::NONE
            },
            valign: Align::Center,
        }
    }

    fn build_job(
        &self,
        inlines: &[Inline],
        base: f32,
        width: f32,
        align: TextAlign,
    ) -> (LayoutJob, Vec<LinkSpan>) {
        let mut job = LayoutJob::default();
        job.wrap.max_width = width.max(8.0);
        job.halign = egui_align(align);
        job.justify = false;
        job.break_on_newline = true;
        let mut links = Vec::new();

        for inline in inlines {
            match inline {
                Inline::Text { text, style, link } => {
                    if text.is_empty() {
                        continue;
                    }
                    let start = job.text.len();
                    job.text.push_str(text);
                    let end = job.text.len();
                    let mut format = self.text_format(style, base);
                    if link.is_some() {
                        format.underline = Stroke::new(1.0_f32, format.color);
                    }
                    job.sections.push(LayoutSection {
                        leading_space: 0.0,
                        byte_range: start..end,
                        format,
                    });
                    if let Some(url) = link {
                        links.push(LinkSpan {
                            range: start..end,
                            url: url.clone(),
                        });
                    }
                }
                Inline::Break => {
                    let start = job.text.len();
                    job.text.push('\n');
                    let format = job
                        .sections
                        .last()
                        .map(|section| section.format.clone())
                        .unwrap_or_else(|| self.text_format(&TextStyle::default(), base));
                    job.sections.push(LayoutSection {
                        leading_space: 0.0,
                        byte_range: start..start + 1,
                        format,
                    });
                }
                Inline::Image(_) => {}
            }
        }

        (job, links)
    }

    fn layout_galley(&self, job: LayoutJob, width: f32) -> Arc<Galley> {
        let galley = self.ui.fonts(|fonts| fonts.layout_job(job.clone()));
        if galley.size().x > width + 0.5 {
            // 超长 URL / 不可断词：允许任意位置断行，避免横向溢出。
            let mut retry = job;
            retry.wrap.break_anywhere = true;
            self.ui.fonts(|fonts| fonts.layout_job(retry))
        } else {
            galley
        }
    }

    fn plain_galley(&self, text: &str, style: &TextStyle, base: f32, width: f32) -> Arc<Galley> {
        let format = self.text_format(style, base);
        let mut job = LayoutJob::simple_format(text.to_string(), format);
        job.wrap.max_width = width.max(8.0);
        self.layout_galley(job, width)
    }

    fn layout_document(&self, rendered: &RenderedHtml, origin: Pos2, available: f32) -> (Doc, f32) {
        let content_width = (available - PAGE_PADDING_X * 2.0).clamp(120.0, MAX_CONTENT_WIDTH);
        let left = origin.x + (available - content_width) * 0.5;
        let top = origin.y + PAGE_PADDING_Y;

        let mut doc = Doc::default();
        let bottom = self.layout_blocks(&rendered.blocks, left, top, content_width, &mut doc);
        (doc, bottom - origin.y + PAGE_PADDING_Y)
    }

    // -- 块级 ---------------------------------------------------------------

    fn layout_blocks(&self, blocks: &[Block], x: f32, y: f32, width: f32, doc: &mut Doc) -> f32 {
        let mut cursor = y;
        let mut previous_bottom = 0.0f32;
        let mut first = true;

        for block in blocks {
            let (margin_top, margin_bottom) = block_margins(block, width);
            cursor += if first {
                margin_top
            } else {
                previous_bottom.max(margin_top)
            };
            cursor = self.layout_block(block, x, cursor, width, doc);
            previous_bottom = margin_bottom;
            first = false;
        }

        if first { y } else { cursor + previous_bottom }
    }

    fn layout_block(&self, block: &Block, x: f32, y: f32, width: f32, doc: &mut Doc) -> f32 {
        match block {
            Block::Paragraph { inlines, style } => {
                let fit = style
                    .fit_content
                    .then(|| self.inline_intrinsic(inlines, style, 14.0, width));
                let b = begin_box(style, x, y, width, fit);
                let mut child = Doc::default();
                let bottom = self.layout_text(
                    inlines,
                    style,
                    14.0,
                    b.content_x,
                    b.content_y,
                    b.content_w,
                    &mut child,
                );
                finish_box(doc, &b, style, child, bottom)
            }
            Block::Section { blocks, style } => {
                let fit = style
                    .fit_content
                    .then(|| self.intrinsic_width(blocks, width));
                let b = begin_box(style, x, y, width, fit);
                let mut child = Doc::default();
                let bottom =
                    self.layout_blocks(blocks, b.content_x, b.content_y, b.content_w, &mut child);
                finish_box(doc, &b, style, child, bottom)
            }
            Block::Heading {
                level,
                inlines,
                style,
            } => {
                let base = heading_base(*level);
                let fit = style
                    .fit_content
                    .then(|| self.inline_intrinsic(inlines, style, base, width));
                let b = begin_box(style, x, y, width, fit);
                let mut child = Doc::default();
                let bottom = self.layout_text(
                    inlines,
                    style,
                    base,
                    b.content_x,
                    b.content_y,
                    b.content_w,
                    &mut child,
                );
                finish_box(doc, &b, style, child, bottom)
            }
            Block::List { items, style, .. } => {
                let b = begin_box(style, x, y, width, None);
                let mut child = Doc::default();
                let mut cursor = b.content_y;
                let base_style = items
                    .iter()
                    .find_map(|item| first_text_style(&item.blocks))
                    .unwrap_or_default();
                for item in items {
                    let marker_w = if item.marker.is_empty() {
                        0.0
                    } else {
                        let mut marker_style = base_style.clone();
                        marker_style.color = Some(self.weak);
                        marker_style.bold = false;
                        marker_style.underline = false;
                        marker_style.code = false;
                        let galley =
                            self.plain_galley(&item.marker, &marker_style, 14.0, b.content_w);
                        let marker_width = galley.size().x;
                        let rect = Rect::from_min_size(
                            pos2(b.content_x, cursor),
                            vec2(marker_width, galley.size().y),
                        );
                        child.items.push(Item::Text {
                            rect,
                            galley,
                            links: Vec::new(),
                        });
                        marker_width + LIST_MARKER_GAP
                    };
                    let item_w = (b.content_w - marker_w).max(16.0);
                    cursor = self.layout_blocks(
                        &item.blocks,
                        b.content_x + marker_w,
                        cursor,
                        item_w,
                        &mut child,
                    );
                }
                finish_box(doc, &b, style, child, cursor)
            }
            Block::Quote { blocks, style } => {
                let mut quote_style = style.clone();
                let padding = resolve_edges(&quote_style.padding, width);
                if padding[3] < 8.0 {
                    quote_style.padding.left = Length::Px(10.0);
                }
                let b = begin_box(&quote_style, x, y, width, None);
                let mut child = Doc::default();
                let bottom =
                    self.layout_blocks(blocks, b.content_x, b.content_y, b.content_w, &mut child);
                let height = (bottom - b.content_y).max(0.0);
                let rect = Rect::from_min_size(
                    pos2(b.rect_left, b.rect_top),
                    vec2(b.rect_width, b.pad_top + height + b.pad_bottom),
                );
                if let Some(background) = quote_style.background {
                    doc.items.push(Item::Fill {
                        rect,
                        color: background,
                        radius: quote_style.radius,
                    });
                }
                let (bar_width, bar_color) = quote_style
                    .border
                    .map(|border| (border.width.max(2.0), border.color))
                    .unwrap_or((3.0, Color32::from_rgb(90, 140, 190)));
                child.items.insert(
                    0,
                    Item::Fill {
                        rect: Rect::from_min_size(
                            pos2(b.rect_left, b.rect_top),
                            vec2(bar_width, rect.height()),
                        ),
                        color: bar_color,
                        radius: 0.0,
                    },
                );
                doc.extend(child);
                rect.bottom()
            }
            Block::Pre {
                text,
                style,
                text_style,
            } => {
                let mut pre_style = style.clone();
                if pre_style.background.is_none() {
                    pre_style.background = Some(self.code_bg);
                }
                if pre_style.radius <= 0.0 {
                    pre_style.radius = 4.0;
                }
                let padding = resolve_edges(&pre_style.padding, width);
                if padding.iter().all(|value| *value < 0.5) {
                    pre_style.padding = Edges {
                        top: Length::Px(8.0),
                        right: Length::Px(8.0),
                        bottom: Length::Px(8.0),
                        left: Length::Px(8.0),
                    };
                }
                let b = begin_box(&pre_style, x, y, width, None);
                let mut child = Doc::default();
                let mut pre_text_style = text_style.clone();
                pre_text_style.font_family = CssFontFamily::Monospace;
                pre_text_style.font_size =
                    Some(pre_text_style.font_size.unwrap_or(13.0).clamp(10.0, 24.0));
                pre_text_style.color = Some(pre_text_style.color.unwrap_or(self.fallback));
                pre_text_style.line_height = Some(pre_text_style.line_height.unwrap_or(1.5));
                pre_text_style.white_space = super::css::WhiteSpace::Pre;
                let base = pre_text_style.font_size.unwrap_or(13.0);
                let galley = self.plain_galley(text, &pre_text_style, base, b.content_w);
                let mut content_bottom = b.content_y;
                let galley_height = galley.size().y;
                if galley_height > 0.0 {
                    let rect = Rect::from_min_size(
                        pos2(b.content_x, b.content_y),
                        vec2(b.content_w, galley_height),
                    );
                    child.items.push(Item::Text {
                        rect,
                        galley,
                        links: Vec::new(),
                    });
                    content_bottom = b.content_y + galley_height;
                }
                finish_box(doc, &b, &pre_style, child, content_bottom)
            }
            Block::Rule { style } => {
                let b = begin_box(style, x, y, width, None);
                let thickness = style
                    .border
                    .map(|border| border.width)
                    .unwrap_or(1.0)
                    .clamp(1.0, 8.0);
                let color = style
                    .border
                    .map(|border| border.color)
                    .unwrap_or(if self.dark {
                        Color32::from_gray(88)
                    } else {
                        Color32::from_gray(210)
                    });
                doc.items.push(Item::Fill {
                    rect: Rect::from_min_size(
                        pos2(b.rect_left, b.content_y),
                        vec2(b.rect_width, thickness),
                    ),
                    color,
                    radius: 0.0,
                });
                b.content_y + thickness + b.pad_bottom
            }
            Block::Table(table) => {
                // 未声明 width 的表格按内容收缩（按钮、社交图标这类小表格），
                // 声明了 width/width:100% 的布局表格则铺满容器。
                let fit = table
                    .style
                    .width
                    .is_none()
                    .then(|| self.table_intrinsic_width(table, width));
                let b = begin_box(&table.style, x, y, width, fit);
                let mut child = Doc::default();
                let bottom =
                    self.layout_table(table, b.content_x, b.content_y, b.content_w, &mut child);
                finish_box(doc, &b, &table.style, child, bottom)
            }
            Block::Image(image) => {
                let mut child = Doc::default();
                let bottom = self.layout_image_row(
                    &[image],
                    x,
                    y + IMAGE_ROW_GAP,
                    width,
                    image.align,
                    &mut child,
                );
                doc.extend(child);
                bottom + IMAGE_ROW_GAP
            }
        }
    }

    // -- 文本与图片 ---------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn layout_text(
        &self,
        inlines: &[Inline],
        style: &BlockStyle,
        base: f32,
        x: f32,
        y: f32,
        width: f32,
        doc: &mut Doc,
    ) -> f32 {
        if inlines.is_empty() {
            return y;
        }
        if !inlines
            .iter()
            .any(|inline| matches!(inline, Inline::Image(_)))
        {
            let (job, spans) = self.build_job(inlines, base, width, style.align);
            if is_blank(&job.text) {
                return y;
            }
            let galley = self.layout_galley(job, width);
            let height = galley.size().y;
            if height <= 0.0 {
                return y;
            }
            let rect = Rect::from_min_size(pos2(x, y), vec2(width, height));
            let links = link_hits(&galley, &spans, galley_pos(rect, style.align));
            doc.items.push(Item::Text {
                rect,
                galley,
                links,
            });
            return y + height;
        }
        self.layout_mixed(inlines, style, base, x, y, width, doc)
    }

    #[allow(clippy::too_many_arguments)]
    fn layout_mixed(
        &self,
        inlines: &[Inline],
        style: &BlockStyle,
        base: f32,
        x: f32,
        y: f32,
        width: f32,
        doc: &mut Doc,
    ) -> f32 {
        enum Segment {
            Text(Vec<Inline>),
            Images(Vec<ImageRef>),
            Break,
        }

        let mut segments: Vec<Segment> = Vec::new();
        let mut text: Vec<Inline> = Vec::new();
        let mut images: Vec<ImageRef> = Vec::new();
        for inline in inlines {
            match inline {
                Inline::Text { .. } => text.push(inline.clone()),
                Inline::Image(image) => images.push(image.clone()),
                Inline::Break => {
                    if !text.is_empty() {
                        segments.push(Segment::Text(std::mem::take(&mut text)));
                    }
                    if !images.is_empty() {
                        segments.push(Segment::Images(std::mem::take(&mut images)));
                    }
                    segments.push(Segment::Break);
                }
            }
        }
        if !text.is_empty() {
            segments.push(Segment::Text(text));
        }
        if !images.is_empty() {
            segments.push(Segment::Images(images));
        }

        let line_height = base * 1.45;
        let mut cursor = y;
        let mut after_break = false;
        let mut pending_empty_lines = 0u32;

        for segment in segments {
            match segment {
                Segment::Break => {
                    if after_break {
                        pending_empty_lines += 1;
                    }
                    after_break = true;
                }
                Segment::Text(runs) => {
                    if pending_empty_lines > 0 {
                        cursor += pending_empty_lines as f32 * line_height;
                        pending_empty_lines = 0;
                    }
                    let (job, spans) = self.build_job(&runs, base, width, style.align);
                    if !is_blank(&job.text) {
                        let galley = self.layout_galley(job, width);
                        let height = galley.size().y;
                        if height > 0.0 {
                            let rect = Rect::from_min_size(pos2(x, cursor), vec2(width, height));
                            let links = link_hits(&galley, &spans, galley_pos(rect, style.align));
                            doc.items.push(Item::Text {
                                rect,
                                galley,
                                links,
                            });
                            cursor += height;
                        }
                    }
                    after_break = false;
                }
                Segment::Images(images) => {
                    if pending_empty_lines > 0 {
                        cursor += pending_empty_lines as f32 * line_height;
                        pending_empty_lines = 0;
                    }
                    let refs: Vec<&ImageRef> = images.iter().collect();
                    cursor = self.layout_image_row(&refs, x, cursor, width, style.align, doc);
                    after_break = false;
                }
            }
        }

        cursor + pending_empty_lines as f32 * line_height
    }

    fn layout_image_row(
        &self,
        images: &[&ImageRef],
        x: f32,
        y: f32,
        width: f32,
        align: TextAlign,
        doc: &mut Doc,
    ) -> f32 {
        let mut rows: Vec<Vec<ResolvedImage>> = Vec::new();
        let mut row = Vec::new();
        let mut row_width = 0.0f32;

        for image in images {
            let Some(resolved) = self.resolve_image(image, width) else {
                continue;
            };
            let item_width = resolved.size.x;
            if !row.is_empty() && row_width + IMAGE_GAP + item_width > width {
                rows.push(std::mem::take(&mut row));
                row_width = 0.0;
            }
            if !row.is_empty() {
                row_width += IMAGE_GAP;
            }
            row_width += item_width;
            row.push(resolved);
        }
        if !row.is_empty() {
            rows.push(row);
        }

        let mut cursor = y;
        for row in rows {
            let row_height = row.iter().map(|item| item.size.y).fold(0.0f32, f32::max);
            let total_width = row.iter().map(|item| item.size.x).sum::<f32>()
                + IMAGE_GAP * (row.len().saturating_sub(1)) as f32;
            let offset = match align {
                TextAlign::Left => 0.0,
                TextAlign::Center => ((width - total_width) / 2.0).max(0.0),
                TextAlign::Right => (width - total_width).max(0.0),
            };
            let mut cursor_x = x + offset;
            for item in row {
                let top = cursor + (row_height - item.size.y) / 2.0;
                let rect = Rect::from_min_size(pos2(cursor_x, top), item.size);
                match (item.texture, item.chip) {
                    (Some(texture), _) => doc.items.push(Item::Image {
                        rect,
                        texture,
                        link: item.link,
                        alt: item.alt,
                        radius: item.radius,
                    }),
                    (None, Some(chip)) => {
                        let text_pos = rect.min + vec2(9.0, 6.0);
                        doc.items.push(Item::Chip {
                            rect,
                            text_pos,
                            text: chip.galley,
                            bg: chip.bg,
                            fg: chip.fg,
                            url: chip.url,
                            alt: chip.alt,
                        });
                    }
                    (None, None) => {}
                }
                cursor_x += item.size.x + IMAGE_GAP;
            }
            cursor += row_height + IMAGE_ROW_GAP;
        }
        cursor
    }

    fn resolve_image(&self, image: &ImageRef, max_width: f32) -> Option<ResolvedImage> {
        let src = image.src.trim();
        let natural = self
            .images
            .and_then(|images| images.textures.get(src))
            .map(|texture| texture.size_vec2());
        let texture = self
            .images
            .and_then(|images| images.textures.get(src))
            .map(|texture| texture.id());

        if let Some(natural) = natural {
            // 1×1 追踪像素不参与排版。
            if natural.x <= 2.0 && natural.y <= 2.0 && image.alt.is_empty() {
                return None;
            }
            let width = image
                .width
                .and_then(|length| length.resolve(max_width))
                .unwrap_or(natural.x)
                .max(4.0);
            let height = image
                .height
                .and_then(|length| length.resolve(max_width))
                .unwrap_or_else(|| {
                    if natural.x > 0.0 {
                        natural.y * (width / natural.x)
                    } else {
                        natural.y
                    }
                })
                .max(4.0);
            let scale = (max_width / width.max(1.0)).min(1.0);
            return Some(ResolvedImage {
                size: vec2((width * scale).max(4.0), (height * scale).max(4.0)),
                texture,
                link: image.link.clone(),
                alt: image.alt.clone(),
                radius: image.radius,
                chip: None,
            });
        }

        let is_remote =
            src.starts_with("http://") || src.starts_with("https://") || src.starts_with("//");
        let pending = self
            .images
            .is_some_and(|images| images.pending.contains(src));
        let failed = self
            .images
            .is_some_and(|images| images.failed.contains(src));
        let disabled = self.images.is_some_and(|images| images.remote_disabled);

        let (text, fg, url) = if is_remote {
            if disabled {
                ("远程图片已关闭".to_string(), self.weak, None)
            } else if failed {
                (
                    "图片加载失败 · 在浏览器中打开".to_string(),
                    Color32::from_rgb(190, 86, 86),
                    Some(remote_url(src)),
                )
            } else if pending {
                ("正在加载图片…".to_string(), self.weak, None)
            } else {
                (
                    "正在加载图片…".to_string(),
                    self.weak,
                    Some(remote_url(src)),
                )
            }
        } else if image.alt.trim().is_empty() {
            ("图片不可用".to_string(), self.weak, None)
        } else {
            (format!("图片不可用：{}", image.alt.trim()), self.weak, None)
        };

        let chip_bg = if self.dark {
            Color32::from_rgb(52, 54, 58)
        } else {
            Color32::from_rgb(241, 242, 244)
        };
        let font = FontId::new(12.0, crate::fonts::email_family(false));
        let wrap_width = (max_width - 18.0).max(16.0);
        let mut job = LayoutJob::simple(text.clone(), font, fg, wrap_width);
        job.break_on_newline = false;
        let galley = self.ui.fonts(|fonts| fonts.layout_job(job));
        // 占位块不得宽于所在列，否则会压到相邻单元格/列上。
        let size = vec2(
            (galley.size().x + 18.0).min(max_width).max(4.0),
            galley.size().y + 12.0,
        );

        Some(ResolvedImage {
            size,
            texture: None,
            link: None,
            alt: image.alt.clone(),
            radius: 0.0,
            chip: Some(ChipSpec {
                galley,
                bg: chip_bg,
                fg,
                url,
                alt: image.alt.clone(),
            }),
        })
    }

    // -- 表格 ---------------------------------------------------------------

    fn layout_table(&self, table: &Table, x: f32, y: f32, width: f32, doc: &mut Doc) -> f32 {
        let rows = table.rows.len();
        if rows == 0 {
            return y;
        }
        let columns = table.columns.clamp(1, 64);
        let spacing = if table.border_collapse {
            0.0
        } else {
            table.cell_spacing.max(0.0)
        };

        struct Place<'a> {
            row: usize,
            col: usize,
            rs: usize,
            cs: usize,
            cell: &'a TableCell,
        }

        let mut occupied = vec![vec![false; columns]; rows];
        let mut places: Vec<Place<'_>> = Vec::new();
        for (row_index, row) in table.rows.iter().enumerate() {
            let mut col = 0usize;
            for cell in &row.cells {
                while col < columns && occupied[row_index][col] {
                    col += 1;
                }
                if col >= columns {
                    break;
                }
                let cs = cell.colspan.clamp(1, columns - col);
                let rs = cell.rowspan.clamp(1, rows - row_index);
                for row in occupied.iter_mut().skip(row_index).take(rs) {
                    for cell in row.iter_mut().skip(col).take(cs) {
                        *cell = true;
                    }
                }
                places.push(Place {
                    row: row_index,
                    col,
                    rs,
                    cs,
                    cell,
                });
                col += cs;
            }
        }
        if places.is_empty() {
            return y;
        }

        let gaps = spacing * (columns.saturating_sub(1)) as f32;
        let target = width;
        let mut col_w = vec![0.0f32; columns];
        let mut fixed = vec![false; columns];
        let mut preferred = vec![0.0f32; columns];

        for place in &places {
            let intrinsic = self.intrinsic_width(&place.cell.blocks, width);
            if place.cs == 1 {
                if let Some(width) = place.cell.width.and_then(|length| length.resolve(target))
                    && width > col_w[place.col]
                {
                    col_w[place.col] = width;
                    fixed[place.col] = true;
                }
                preferred[place.col] = preferred[place.col].max(intrinsic);
            }
        }

        let fixed_total: f32 = col_w.iter().sum();
        let flex: Vec<usize> = (0..columns).filter(|col| !fixed[*col]).collect();
        let remaining = (target - gaps - fixed_total).max(0.0);
        let weights: f32 = flex.iter().map(|col| preferred[*col].max(4.0)).sum::<f32>();
        if !flex.is_empty() {
            if weights > 0.0 {
                for col in &flex {
                    col_w[*col] = remaining * preferred[*col].max(4.0) / weights;
                }
            } else {
                let each = remaining / flex.len() as f32;
                for col in &flex {
                    col_w[*col] = each;
                }
            }
        }

        // 跨列单元格至少要容纳自己的内容。
        for place in &places {
            if place.cs <= 1 {
                continue;
            }
            let needed = place
                .cell
                .width
                .and_then(|length| length.resolve(target))
                .unwrap_or_else(|| self.intrinsic_width(&place.cell.blocks, target))
                .min(target);
            let have = (place.col..place.col + place.cs)
                .map(|col| col_w[col])
                .sum::<f32>()
                + spacing * (place.cs - 1) as f32;
            if needed > have + 0.5 {
                let share = (needed - have) / place.cs as f32;
                for width in col_w.iter_mut().skip(place.col).take(place.cs) {
                    *width += share;
                }
            }
        }

        // 表格宽度由自身 width 决定：先填满，再按需缩放。
        let total = col_w.iter().sum::<f32>() + gaps;
        if total < target - 0.5 {
            let extra = target - total;
            let sum: f32 = col_w.iter().sum();
            if sum > 0.0 {
                for width in &mut col_w {
                    *width += extra * (*width / sum);
                }
            } else {
                let each = extra / columns as f32;
                for width in &mut col_w {
                    *width += each;
                }
            }
        } else if total > target + 0.5 {
            let scale = ((target - gaps).max(8.0) / col_w.iter().sum::<f32>().max(1.0)).min(1.0);
            for width in &mut col_w {
                *width = (*width * scale).max(4.0);
            }
        }

        let mut col_x = Vec::with_capacity(columns);
        let mut cursor = x;
        for (col, width) in col_w.iter().enumerate() {
            col_x.push(cursor);
            cursor += *width;
            if col + 1 < columns {
                cursor += spacing;
            }
        }

        let mut cell_docs: Vec<Option<Doc>> = (0..places.len()).map(|_| None).collect();
        let mut cell_heights = vec![0.0f32; places.len()];
        let mut content_heights = vec![0.0f32; places.len()];
        let mut cell_widths = vec![0.0f32; places.len()];
        let mut cell_pads = vec![[0.0f32; 4]; places.len()];

        for (index, place) in places.iter().enumerate() {
            let cell_width = (place.col..place.col + place.cs)
                .map(|col| col_w[col])
                .sum::<f32>()
                + spacing * (place.cs.saturating_sub(1)) as f32;
            let [pad_top, pad_right, pad_bottom, pad_left] =
                resolve_edges(&place.cell.padding, cell_width);
            let content_width = (cell_width - pad_left - pad_right).max(4.0);
            let content_x = col_x[place.col] + pad_left;
            let mut cell_doc = Doc::default();
            let content_bottom = self.layout_blocks(
                &place.cell.blocks,
                content_x,
                0.0,
                content_width,
                &mut cell_doc,
            );
            cell_docs[index] = Some(cell_doc);
            content_heights[index] = content_bottom.max(0.0);
            cell_heights[index] = pad_top + content_bottom.max(0.0) + pad_bottom;
            cell_widths[index] = cell_width;
            cell_pads[index] = [pad_top, pad_right, pad_bottom, pad_left];
        }

        let mut row_heights = vec![0.0f32; rows];
        for (index, place) in places.iter().enumerate() {
            if place.rs == 1 {
                row_heights[place.row] = row_heights[place.row].max(cell_heights[index]);
            }
        }
        for (index, place) in places.iter().enumerate() {
            if place.rs > 1 {
                let have = (place.row..place.row + place.rs)
                    .map(|row| row_heights[row])
                    .sum::<f32>()
                    + spacing * (place.rs - 1) as f32;
                if cell_heights[index] > have {
                    let share = (cell_heights[index] - have) / place.rs as f32;
                    for height in row_heights.iter_mut().skip(place.row).take(place.rs) {
                        *height += share;
                    }
                }
            }
        }

        let mut row_y = Vec::with_capacity(rows);
        let mut cursor = y;
        for (row, height) in row_heights.iter().enumerate() {
            row_y.push(cursor);
            cursor += *height;
            if row + 1 < rows {
                cursor += spacing;
            }
        }

        for (index, place) in places.iter().enumerate() {
            let cell_height = (place.row..place.row + place.rs)
                .map(|row| row_heights[row])
                .sum::<f32>()
                + spacing * (place.rs.saturating_sub(1)) as f32;
            let rect = Rect::from_min_size(
                pos2(col_x[place.col], row_y[place.row]),
                vec2(cell_widths[index], cell_height),
            );
            if let Some(background) = place.cell.background {
                doc.items.push(Item::Fill {
                    rect,
                    color: background,
                    radius: place.cell.radius,
                });
            }
            if let Some(border) = place.cell.border {
                doc.items.push(Item::Border {
                    rect,
                    stroke: Stroke::new(border.width, border.color),
                    radius: place.cell.radius,
                });
            }

            let [pad_top, _pad_right, pad_bottom, _pad_left] = cell_pads[index];
            let inner_height = (cell_height - pad_top - pad_bottom).max(0.0);
            let offset = match place.cell.valign {
                VerticalAlign::Top => 0.0,
                VerticalAlign::Middle => ((inner_height - content_heights[index]) / 2.0).max(0.0),
                VerticalAlign::Bottom => (inner_height - content_heights[index]).max(0.0),
            };
            if let Some(mut cell_doc) = cell_docs[index].take() {
                translate_doc(&mut cell_doc, 0.0, row_y[place.row] + pad_top + offset);
                doc.extend(cell_doc);
            }
        }

        row_y.last().copied().unwrap_or(y) + row_heights.last().copied().unwrap_or(0.0)
    }

    // -- 固有宽度 -----------------------------------------------------------

    fn inline_intrinsic(&self, inlines: &[Inline], style: &BlockStyle, base: f32, cap: f32) -> f32 {
        if inlines
            .iter()
            .any(|inline| matches!(inline, Inline::Image(_)))
        {
            return cap;
        }
        let (job, _) = self.build_job(inlines, base, cap, style.align);
        if is_blank(&job.text) {
            return 0.0;
        }
        self.ui
            .fonts(|fonts| fonts.layout_job(job))
            .size()
            .x
            .min(cap)
    }

    fn intrinsic_width(&self, blocks: &[Block], cap: f32) -> f32 {
        let mut widest = 0.0f32;
        for block in blocks {
            let width = match block {
                Block::Paragraph { inlines, style } => {
                    self.inline_intrinsic(inlines, style, 14.0, cap)
                }
                Block::Section { blocks, style } => {
                    let padding = resolve_edges(&style.padding, cap);
                    self.intrinsic_width(blocks, cap) + padding[1] + padding[3]
                }
                Block::Heading {
                    level,
                    inlines,
                    style,
                } => self.inline_intrinsic(inlines, style, heading_base(*level), cap),
                Block::List { items, style, .. } => {
                    let padding = resolve_edges(&style.padding, cap);
                    let inner = items
                        .iter()
                        .map(|item| self.intrinsic_width(&item.blocks, cap) + 16.0)
                        .fold(0.0f32, f32::max);
                    inner + padding[1] + padding[3]
                }
                Block::Quote { blocks, style } => {
                    let padding = resolve_edges(&style.padding, cap);
                    self.intrinsic_width(blocks, cap) + padding[1] + padding[3] + 10.0
                }
                Block::Pre {
                    text, text_style, ..
                } => {
                    let size = text_style.font_size.unwrap_or(13.0).clamp(10.0, 24.0);
                    let font = FontId::new(size, crate::fonts::email_monospace());
                    let color = text_style.color.unwrap_or(self.fallback);
                    let widest_line = text
                        .lines()
                        .map(|line| {
                            self.ui
                                .fonts(|fonts| {
                                    fonts.layout_no_wrap(line.to_string(), font.clone(), color)
                                })
                                .size()
                                .x
                        })
                        .fold(0.0f32, f32::max);
                    (widest_line + 24.0).min(cap)
                }
                Block::Rule { .. } => 0.0,
                // 嵌套表格按“占满可用宽度”处理：邮件里它们几乎都是布局表格。
                Block::Table(_) => cap,
                Block::Image(image) => self
                    .resolve_image(image, cap)
                    .map(|resolved| resolved.size.x)
                    .unwrap_or(0.0),
            };
            widest = widest.max(width.min(cap));
        }
        widest
    }

    /// 未显式设置宽度的表格的收缩宽度。
    fn table_intrinsic_width(&self, table: &Table, cap: f32) -> f32 {
        let spacing = if table.border_collapse {
            0.0
        } else {
            table.cell_spacing.max(0.0)
        };
        let mut widest = 0.0f32;
        for row in &table.rows {
            let mut row_width = 0.0f32;
            let mut columns = 0usize;
            for cell in &row.cells {
                let width = match cell.width.and_then(|length| length.resolve(cap)) {
                    Some(width) => width,
                    None => {
                        let [_, pad_right, _, pad_left] = resolve_edges(&cell.padding, cap);
                        self.intrinsic_width(&cell.blocks, cap) + pad_left + pad_right
                    }
                };
                row_width += width;
                columns += cell.colspan.max(1);
            }
            if columns > 1 {
                row_width += spacing * (columns - 1) as f32;
            }
            widest = widest.max(row_width);
        }
        widest.clamp(8.0, cap)
    }
}

// ---------------------------------------------------------------------------
// 盒模型与辅助
// ---------------------------------------------------------------------------

fn begin_box(style: &BlockStyle, x: f32, y: f32, width: f32, fit_content: Option<f32>) -> BlockBox {
    let [pad_top, pad_right, pad_bottom, pad_left] = resolve_edges(&style.padding, width);

    let mut rect_width = match fit_content {
        Some(content_width) => (content_width + pad_left + pad_right).min(width),
        None => style
            .width
            .and_then(|length| length.resolve(width))
            .unwrap_or(width),
    };
    if let Some(max_width) = style.max_width.and_then(|length| length.resolve(width)) {
        rect_width = rect_width.min(max_width);
    }
    rect_width = rect_width.clamp(8.0, width.max(8.0));

    let free = (width - rect_width).max(0.0);
    let auto_left = matches!(style.margin.left, Length::Auto);
    let auto_right = matches!(style.margin.right, Length::Auto);
    let (margin_left, margin_right) = if auto_left && auto_right {
        (free / 2.0, free / 2.0)
    } else if auto_left {
        (free, 0.0)
    } else if auto_right {
        (0.0, free)
    } else {
        (
            style.margin.left.resolve(width).unwrap_or(0.0).max(0.0),
            style.margin.right.resolve(width).unwrap_or(0.0).max(0.0),
        )
    };
    let available = (width - margin_left - margin_right).max(8.0);
    if rect_width > available {
        rect_width = available;
    }
    let fit_offset = if fit_content.is_some() && margin_left == 0.0 && margin_right == 0.0 {
        match style.align {
            TextAlign::Left => 0.0,
            TextAlign::Center => free / 2.0,
            TextAlign::Right => free,
        }
    } else {
        0.0
    };

    let rect_left = x + margin_left + fit_offset;
    let content_w = (rect_width - pad_left - pad_right).max(8.0);
    BlockBox {
        rect_left,
        rect_top: y,
        rect_width,
        pad_top,
        pad_bottom,
        content_x: rect_left + pad_left,
        content_y: y + pad_top,
        content_w,
    }
}

fn finish_box(
    doc: &mut Doc,
    block_box: &BlockBox,
    style: &BlockStyle,
    child: Doc,
    content_bottom: f32,
) -> f32 {
    let height = (content_bottom - block_box.content_y).max(0.0);
    let rect = Rect::from_min_size(
        pos2(block_box.rect_left, block_box.rect_top),
        vec2(
            block_box.rect_width,
            block_box.pad_top + height + block_box.pad_bottom,
        ),
    );
    if let Some(background) = style.background {
        doc.items.push(Item::Fill {
            rect,
            color: background,
            radius: style.radius,
        });
    }
    if let Some(border) = style.border {
        doc.items.push(Item::Border {
            rect,
            stroke: Stroke::new(border.width, border.color),
            radius: style.radius,
        });
    }
    doc.extend(child);
    rect.bottom()
}

fn block_style(block: &Block) -> Option<&BlockStyle> {
    match block {
        Block::Paragraph { style, .. }
        | Block::Heading { style, .. }
        | Block::List { style, .. }
        | Block::Quote { style, .. }
        | Block::Pre { style, .. }
        | Block::Section { style, .. }
        | Block::Rule { style } => Some(style),
        Block::Table(table) => Some(&table.style),
        Block::Image(_) => None,
    }
}

fn block_margins(block: &Block, width: f32) -> (f32, f32) {
    match block_style(block) {
        Some(style) => (
            style.margin.top.resolve(width).unwrap_or(0.0).max(0.0),
            style.margin.bottom.resolve(width).unwrap_or(0.0).max(0.0),
        ),
        None => (IMAGE_ROW_GAP, IMAGE_ROW_GAP),
    }
}

fn resolve_edges(edges: &Edges, base: f32) -> [f32; 4] {
    [
        edges.top.resolve(base).unwrap_or(0.0).max(0.0),
        edges.right.resolve(base).unwrap_or(0.0).max(0.0),
        edges.bottom.resolve(base).unwrap_or(0.0).max(0.0),
        edges.left.resolve(base).unwrap_or(0.0).max(0.0),
    ]
}

fn translate_doc(doc: &mut Doc, dx: f32, dy: f32) {
    let delta = vec2(dx, dy);
    for item in &mut doc.items {
        match item {
            Item::Fill { rect, .. } | Item::Border { rect, .. } => {
                *rect = rect.translate(delta);
            }
            Item::Text { rect, links, .. } => {
                *rect = rect.translate(delta);
                for link in links {
                    link.rect = link.rect.translate(delta);
                }
            }
            Item::Image { rect, .. } => {
                *rect = rect.translate(delta);
            }
            Item::Chip { rect, text_pos, .. } => {
                *rect = rect.translate(delta);
                *text_pos += delta;
            }
        }
    }
}

fn first_text_style(blocks: &[Block]) -> Option<TextStyle> {
    for block in blocks {
        match block {
            Block::Paragraph { inlines, .. } | Block::Heading { inlines, .. } => {
                for inline in inlines {
                    if let Inline::Text { style, .. } = inline {
                        return Some(style.clone());
                    }
                }
            }
            Block::List { items, .. } => {
                for item in items {
                    if let Some(style) = first_text_style(&item.blocks) {
                        return Some(style);
                    }
                }
            }
            Block::Quote { blocks, .. } => {
                if let Some(style) = first_text_style(blocks) {
                    return Some(style);
                }
            }
            Block::Section { blocks, .. } => {
                if let Some(style) = first_text_style(blocks) {
                    return Some(style);
                }
            }
            Block::Table(table) => {
                for row in &table.rows {
                    for cell in &row.cells {
                        if let Some(style) = first_text_style(&cell.blocks) {
                            return Some(style);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn heading_base(level: u8) -> f32 {
    match level {
        1 => 23.0,
        2 => 20.0,
        3 => 17.5,
        4 => 15.5,
        5 => 14.0,
        _ => 12.5,
    }
}

fn egui_align(align: TextAlign) -> Align {
    match align {
        TextAlign::Left => Align::LEFT,
        TextAlign::Center => Align::Center,
        TextAlign::Right => Align::RIGHT,
    }
}

fn galley_pos(rect: Rect, align: TextAlign) -> Pos2 {
    match align {
        TextAlign::Left => rect.left_top(),
        TextAlign::Center => rect.center_top(),
        TextAlign::Right => rect.right_top(),
    }
}

fn is_blank(text: &str) -> bool {
    text.chars().all(|ch| ch.is_whitespace() && ch != '\u{a0}')
}

fn remote_url(src: &str) -> String {
    if src.starts_with("//") {
        format!("https:{src}")
    } else {
        src.to_string()
    }
}

fn link_hits(galley: &Galley, spans: &[LinkSpan], origin: Pos2) -> Vec<LinkHit> {
    if spans.is_empty() {
        return Vec::new();
    }
    let char_index = |byte: usize| -> usize {
        galley.job.text[..byte.min(galley.job.text.len())]
            .chars()
            .filter(|ch| *ch != '\n')
            .count()
    };

    let mut hits = Vec::new();
    for span in spans {
        let start = char_index(span.range.start);
        let end = char_index(span.range.end);
        if start >= end {
            continue;
        }
        let mut index = 0usize;
        let mut current: Option<Rect> = None;
        for placed in &galley.rows {
            for glyph in &placed.row.glyphs {
                if index >= end {
                    break;
                }
                if index >= start {
                    let glyph_rect = glyph.logical_rect();
                    let rect = Rect::from_min_size(
                        origin + placed.pos.to_vec2() + glyph_rect.min.to_vec2(),
                        glyph_rect.size(),
                    );
                    current = Some(match current {
                        Some(existing) => existing.union(rect),
                        None => rect,
                    });
                }
                index += 1;
            }
            if let Some(rect) = current.take() {
                hits.push(LinkHit {
                    rect,
                    url: span.url.clone(),
                });
            }
            if index >= end {
                break;
            }
        }
        if let Some(rect) = current {
            hits.push(LinkHit {
                rect,
                url: span.url.clone(),
            });
        }
    }
    hits
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    #[derive(Debug, Clone)]
    pub struct TextRect {
        pub rect: Rect,
        pub text: String,
        pub halign: Align,
        /// 每一行文字在屏幕坐标中的 x 范围。
        pub rows: Vec<(f32, f32)>,
    }

    #[derive(Debug, Clone, Default)]
    pub struct Probe {
        pub texts: Vec<TextRect>,
        pub fills: Vec<Rect>,
        pub borders: Vec<Rect>,
        pub height: f32,
    }

    /// 只跑排版、不绘制，供几何回归测试使用。
    pub fn probe_layout(ui: &egui::Ui, rendered: &RenderedHtml, width: f32) -> Probe {
        let cx = LayoutCx {
            ui,
            images: None,
            fallback: Color32::from_gray(34),
            weak: Color32::from_gray(118),
            code_bg: Color32::from_rgb(243, 244, 246),
            dark: false,
        };
        let (doc, height) = cx.layout_document(rendered, pos2(0.0, 0.0), width);
        let mut probe = Probe {
            height,
            ..Probe::default()
        };
        for item in doc.items {
            match item {
                Item::Text { rect, galley, .. } => {
                    let origin_x = match galley.job.halign {
                        Align::Min => rect.left(),
                        Align::Center => rect.center().x,
                        Align::Max => rect.right(),
                    };
                    let mut rows = Vec::new();
                    for placed in &galley.rows {
                        let row = placed.rect();
                        rows.push((origin_x + row.left(), origin_x + row.right()));
                    }
                    probe.texts.push(TextRect {
                        rect,
                        text: galley.job.text.clone(),
                        halign: galley.job.halign,
                        rows,
                    });
                }
                Item::Chip { rect, text, .. } => probe.texts.push(TextRect {
                    rect,
                    text: text.job.text.clone(),
                    halign: text.job.halign,
                    rows: Vec::new(),
                }),
                Item::Fill { rect, .. } => probe.fills.push(rect),
                Item::Border { rect, .. } => probe.borders.push(rect),
                _ => {}
            }
        }
        probe
    }

    /// 在标准 egui 上下文里跑一次排版探针。
    pub fn run_probe(rendered: &RenderedHtml, width: f32) -> Probe {
        let mut probe = Probe::default();
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                probe = probe_layout(ui, rendered, width);
            });
        });
        probe
    }
}
