//! HTML5 DOM 解析、清洗与扁平块布局（在阻塞线程中执行）。

use crate::views::html::css::{
    ComputedStyle, Display, Edges, FontFamily, Length, ListStyle, Stylesheet, TextAlign, TextStyle,
    WhiteSpace, apply_html_attributes, apply_inline_style, is_block_tag,
};
use crate::views::html::{
    Block, BlockStyle, HtmlRenderError, ImageRef, Inline, ListItem, RenderedHtml, Table, TableCell,
    TableRow,
};
use scraper::{ElementRef, Html, Node};

const MAX_NODES: usize = 50_000;
const MAX_DEPTH: usize = 128;
const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;

const SKIP_TAGS: &[&str] = &[
    "script", "style", "head", "title", "meta", "link", "base", "iframe", "object", "embed",
    "form", "input", "button", "select", "textarea", "svg", "math", "template", "noscript",
    "canvas", "video", "audio",
];

struct ParseCtx<'a> {
    stylesheet: &'a Stylesheet,
    nodes: usize,
    text_bytes: usize,
    truncated: bool,
}

impl ParseCtx<'_> {
    fn bump_node(&mut self) -> Result<(), HtmlRenderError> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(HtmlRenderError::TooLarge);
        }
        Ok(())
    }

    fn add_text_bytes(&mut self, len: usize) -> Result<(), HtmlRenderError> {
        self.text_bytes = self.text_bytes.saturating_add(len);
        if self.text_bytes > MAX_TEXT_BYTES {
            self.truncated = true;
            return Err(HtmlRenderError::TooLarge);
        }
        Ok(())
    }
}

pub fn render_html(html: &str) -> Result<RenderedHtml, HtmlRenderError> {
    if html.trim().is_empty() {
        return Ok(RenderedHtml::empty());
    }
    let document = Html::parse_document(html);
    let stylesheet = Stylesheet::parse(&collect_styles(&document));
    let mut ctx = ParseCtx {
        stylesheet: &stylesheet,
        nodes: 0,
        text_bytes: 0,
        truncated: false,
    };
    let root = document.root_element();
    let root_style = ComputedStyle::root();
    let html_style = compute_style(&root, &root_style, &mut ctx)?;

    // `<body>` 的背景是页面底色；它的其余样式继续通过继承进入各块。
    let body = scraper::Selector::parse("body")
        .ok()
        .and_then(|selector| document.select(&selector).next());
    let (page_style, blocks) = if let Some(body) = body {
        let body_style = compute_style(&body, &html_style, &mut ctx)?;
        let blocks = layout_flow(&body, &body_style, &mut ctx, 0)?;
        (body_style, blocks)
    } else {
        let blocks = layout_element(&root, &root_style, &mut ctx, 0)?;
        (html_style, blocks)
    };
    Ok(RenderedHtml {
        blocks,
        truncated: ctx.truncated,
        page_background: page_style.text.background,
    })
}

fn collect_styles(document: &Html) -> String {
    let Ok(selector) = scraper::Selector::parse("style") else {
        return String::new();
    };
    document
        .select(&selector)
        .map(|element| element.text().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn compute_style(
    element: &ElementRef<'_>,
    parent: &ComputedStyle,
    ctx: &mut ParseCtx<'_>,
) -> Result<ComputedStyle, HtmlRenderError> {
    ctx.bump_node()?;
    let tag = element.value().name().to_ascii_lowercase();
    let mut style = ComputedStyle::inherit(parent);
    style.display = if is_block_tag(&tag) {
        Display::Block
    } else {
        Display::Inline
    };

    // HTML 标签默认样式（UA 样式表，低于作者 CSS）。
    let parent_size = parent.text.font_size.unwrap_or(14.0);
    let em = |factor: f32| (parent_size * factor).max(0.0);
    match tag.as_str() {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let (scale, margin) = match tag.as_str() {
                "h1" => (1.7, 0.67),
                "h2" => (1.45, 0.83),
                "h3" => (1.25, 1.0),
                "h4" => (1.1, 1.33),
                "h5" => (1.0, 1.67),
                _ => (0.9, 2.33),
            };
            style.text.bold = true;
            style.text.font_size = Some((parent_size * scale).clamp(10.0, 48.0));
            style.margin.top = Length::Px(em(margin));
            style.margin.bottom = Length::Px(em(margin));
        }
        "p" => {
            style.margin.top = Length::Px(em(1.0));
            style.margin.bottom = Length::Px(em(1.0));
        }
        "ul" | "ol" => {
            style.list_style = if tag == "ol" {
                ListStyle::Decimal
            } else {
                ListStyle::Disc
            };
            style.margin.top = Length::Px(em(0.75));
            style.margin.bottom = Length::Px(em(0.75));
            style.padding.left = Length::Px(em(2.5));
        }
        "li" => {
            style.display = Display::Block;
            style.margin.top = Length::Px(em(0.15));
            style.margin.bottom = Length::Px(em(0.15));
        }
        "dl" => {
            style.margin.top = Length::Px(em(1.0));
            style.margin.bottom = Length::Px(em(1.0));
        }
        "dd" => {
            style.margin.left = Length::Px(em(2.0));
        }
        "th" => {
            style.text.bold = true;
            style.text.text_align = TextAlign::Center;
            style.padding = Edges {
                top: Length::Px(1.0),
                right: Length::Px(1.0),
                bottom: Length::Px(1.0),
                left: Length::Px(1.0),
            };
        }
        "td" => {
            style.padding = Edges {
                top: Length::Px(1.0),
                right: Length::Px(1.0),
                bottom: Length::Px(1.0),
                left: Length::Px(1.0),
            };
        }
        "table" => {
            style.display = Display::Block;
            style.border_collapse = false;
        }
        "b" | "strong" => style.text.bold = true,
        "i" | "em" | "cite" | "var" => style.text.italic = true,
        "u" | "ins" => style.text.underline = true,
        "s" | "strike" | "del" => style.text.strike = true,
        "code" | "kbd" | "samp" | "tt" => {
            style.text.code = true;
            style.text.font_family = FontFamily::Monospace;
        }
        "small" => style.text.font_size = Some(parent_size * 0.85),
        "big" => style.text.font_size = Some(parent_size * 1.2),
        "pre" => {
            style.text.white_space = WhiteSpace::Pre;
            style.text.font_family = FontFamily::Monospace;
            style.text.font_size = Some(parent_size * 0.9);
            style.margin.top = Length::Px(em(1.0));
            style.margin.bottom = Length::Px(em(1.0));
        }
        "hr" => {
            style.display = Display::Block;
            style.margin.top = Length::Px(em(0.7));
            style.margin.bottom = Length::Px(em(0.7));
        }
        "blockquote" => {
            style.margin.top = Length::Px(em(1.0));
            style.margin.bottom = Length::Px(em(1.0));
            style.margin.left = Length::Px(em(1.6));
            style.margin.right = Length::Px(em(1.6));
        }
        "center" => style.text.text_align = TextAlign::Center,
        "a" => {
            if style.text.color.is_none() {
                style.text.color = Some(eframe::egui::Color32::from_rgb(26, 95, 180));
            }
            style.text.underline = true;
        }
        "body" | "html" => {
            style.margin = Edges::default();
            style.padding = Edges::default();
        }
        _ => {}
    }

    apply_html_attributes(&mut style, element);
    ctx.stylesheet.apply(&mut style, element);
    if let Some(inline) = element.attr("style") {
        apply_inline_style(&mut style, inline);
    }
    if let Some(size) = style.text.font_size {
        style.text.font_size = Some(size.clamp(6.0, 120.0));
    }
    if tag == "table" {
        style.display = Display::Block;
    }
    Ok(style)
}

fn layout_element(
    element: &ElementRef<'_>,
    parent: &ComputedStyle,
    ctx: &mut ParseCtx<'_>,
    depth: usize,
) -> Result<Vec<Block>, HtmlRenderError> {
    let style = compute_style(element, parent, ctx)?;
    if style.display == Display::None {
        return Ok(Vec::new());
    }
    layout_block(element, &style, ctx, depth)
}

fn layout_block(
    element: &ElementRef<'_>,
    style: &ComputedStyle,
    ctx: &mut ParseCtx<'_>,
    depth: usize,
) -> Result<Vec<Block>, HtmlRenderError> {
    if depth > MAX_DEPTH {
        return Err(HtmlRenderError::TooLarge);
    }
    let tag = element.value().name().to_ascii_lowercase();
    if SKIP_TAGS.contains(&tag.as_str()) {
        return Ok(Vec::new());
    }
    match tag.as_str() {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = tag[1..].parse::<u8>().unwrap_or(3).clamp(1, 6);
            let mut inlines = inline_children(element, style, None, ctx, depth + 1)?;
            trim_inlines(&mut inlines);
            Ok(vec![Block::Heading {
                level,
                inlines,
                style: block_style(style),
            }])
        }
        "ul" | "ol" => Ok(vec![layout_list(element, style, ctx, depth, tag == "ol")?]),
        "blockquote" => {
            let blocks = layout_flow(element, style, ctx, depth + 1)?;
            Ok(vec![Block::Quote {
                blocks,
                style: block_style(style),
            }])
        }
        "pre" => {
            let mut text = String::new();
            collect_pre_text(element, &mut text, ctx)?;
            Ok(vec![Block::Pre {
                text,
                style: block_style(style),
                text_style: style.text.clone(),
            }])
        }
        "hr" => Ok(vec![Block::Rule {
            style: block_style(style),
        }]),
        "img" => Ok(image_ref(element, style, None)
            .map_or_else(Vec::new, |image| vec![Block::Image(image)])),
        "table" => Ok(vec![Block::Table(parse_table(element, style, ctx, depth)?)]),
        _ => {
            let blocks = layout_flow(element, style, ctx, depth + 1)?;
            if blocks.is_empty() {
                return Ok(Vec::new());
            }
            // 容器自身的盒样式（MJML 的 `div.wfix { max-width:600px }` 等）必须保留，
            // 否则内部的 `width:100%` 表格会按窗口宽度铺开。
            if has_box_style(style) {
                Ok(vec![Block::Section {
                    blocks,
                    style: block_style(style),
                }])
            } else {
                Ok(blocks)
            }
        }
    }
}

fn has_box_style(style: &ComputedStyle) -> bool {
    style.text.background.is_some()
        || style.border.is_some()
        || style.width.is_some()
        || style.max_width.is_some()
        || style.radius > 0.0
        || !edges_are_zero(&style.padding)
        || !edges_are_zero(&style.margin)
}

fn edges_are_zero(edges: &Edges) -> bool {
    [edges.top, edges.right, edges.bottom, edges.left]
        .iter()
        .all(|edge| edge.resolve(0.0).unwrap_or(0.0).abs() < 0.01)
}

fn layout_flow(
    element: &ElementRef<'_>,
    style: &ComputedStyle,
    ctx: &mut ParseCtx<'_>,
    depth: usize,
) -> Result<Vec<Block>, HtmlRenderError> {
    if depth > MAX_DEPTH {
        return Err(HtmlRenderError::TooLarge);
    }
    let mut blocks = Vec::new();
    let mut inlines = Vec::new();
    flow_children_into(element, style, None, ctx, depth, &mut blocks, &mut inlines)?;
    flush_inlines(&mut inlines, &mut blocks, style);
    Ok(blocks)
}

/// 把元素的子节点写入当前行内流；遇到块级后代时先冲刷行内片段。
///
/// 行内元素（如 MJML 的 `<mj-raw>`、`<span>`）不能把内部的块级元素
/// （`ul` / `table` / `pre`）拍平成文本，否则列表、代码块会粘进段落。
#[allow(clippy::too_many_arguments)]
fn flow_children_into(
    element: &ElementRef<'_>,
    style: &ComputedStyle,
    link: Option<&str>,
    ctx: &mut ParseCtx<'_>,
    depth: usize,
    blocks: &mut Vec<Block>,
    inlines: &mut Vec<Inline>,
) -> Result<(), HtmlRenderError> {
    if depth > MAX_DEPTH {
        return Err(HtmlRenderError::TooLarge);
    }
    for child in element.children() {
        match child.value() {
            Node::Text(text) => {
                add_text(inlines, text, &style.text, link, ctx)?;
            }
            Node::Element(_) => {
                let Some(child_element) = ElementRef::wrap(child) else {
                    continue;
                };
                let tag = child_element.value().name().to_ascii_lowercase();
                if tag == "br" {
                    inlines.push(Inline::Break);
                    continue;
                }
                if SKIP_TAGS.contains(&tag.as_str()) {
                    continue;
                }
                let child_style = compute_style(&child_element, style, ctx)?;
                if child_style.display == Display::None {
                    continue;
                }
                if matches!(child_style.display, Display::Block | Display::InlineBlock) {
                    flush_inlines(inlines, blocks, style);
                    blocks.extend(layout_block(&child_element, &child_style, ctx, depth + 1)?);
                } else if tag == "img" {
                    if let Some(image) = image_ref(&child_element, &child_style, link) {
                        inlines.push(Inline::Image(image));
                    }
                } else {
                    let child_link = if tag == "a" {
                        child_element.attr("href").and_then(filter_link)
                    } else {
                        link.map(str::to_string)
                    };
                    flow_children_into(
                        &child_element,
                        &child_style,
                        child_link.as_deref(),
                        ctx,
                        depth + 1,
                        blocks,
                        inlines,
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn flush_inlines(inlines: &mut Vec<Inline>, blocks: &mut Vec<Block>, style: &ComputedStyle) {
    if inlines.is_empty() {
        return;
    }
    trim_inlines(inlines);
    if inlines.is_empty() {
        return;
    }
    blocks.push(Block::Paragraph {
        inlines: std::mem::take(inlines),
        // 容器的盒样式（背景/边框/边距）由外层 Section 或表格单元格负责，
        // 这里只保留文字对齐，避免同一块底色被重复绘制。
        style: BlockStyle {
            align: style.text.text_align,
            ..BlockStyle::default()
        },
    });
}

/// 标题等纯行内内容使用：遇到块级后代仍然按行内文本处理（标题内不应有块）。
fn inline_children(
    element: &ElementRef<'_>,
    style: &ComputedStyle,
    link: Option<&str>,
    ctx: &mut ParseCtx<'_>,
    depth: usize,
) -> Result<Vec<Inline>, HtmlRenderError> {
    if depth > MAX_DEPTH {
        return Err(HtmlRenderError::TooLarge);
    }
    let own_link = if element.value().name() == "a" {
        element.attr("href").and_then(filter_link)
    } else {
        link.map(str::to_string)
    };
    let mut inlines = Vec::new();
    for child in element.children() {
        match child.value() {
            Node::Text(text) => {
                add_text(&mut inlines, text, &style.text, own_link.as_deref(), ctx)?
            }
            Node::Element(_) => {
                let Some(child_element) = ElementRef::wrap(child) else {
                    continue;
                };
                let tag = child_element.value().name().to_ascii_lowercase();
                if tag == "br" {
                    inlines.push(Inline::Break);
                    continue;
                }
                if SKIP_TAGS.contains(&tag.as_str()) {
                    continue;
                }
                let child_style = compute_style(&child_element, style, ctx)?;
                if child_style.display == Display::None {
                    continue;
                }
                if tag == "img" {
                    if let Some(image) =
                        image_ref(&child_element, &child_style, own_link.as_deref())
                    {
                        inlines.push(Inline::Image(image));
                    }
                    continue;
                }
                let child_link = if tag == "a" {
                    child_element.attr("href").and_then(filter_link)
                } else {
                    own_link.clone()
                };
                inlines.extend(inline_children(
                    &child_element,
                    &child_style,
                    child_link.as_deref(),
                    ctx,
                    depth + 1,
                )?);
            }
            _ => {}
        }
    }
    Ok(inlines)
}

fn add_text(
    inlines: &mut Vec<Inline>,
    raw: &str,
    style: &TextStyle,
    link: Option<&str>,
    ctx: &mut ParseCtx<'_>,
) -> Result<(), HtmlRenderError> {
    ctx.add_text_bytes(raw.len())?;
    let text = if style.white_space == WhiteSpace::Normal {
        collapse_whitespace(raw)
    } else {
        raw.replace("\r\n", "\n").replace('\r', "\n")
    };
    if text.is_empty() {
        return Ok(());
    }
    if let Some(Inline::Text {
        text: last,
        style: last_style,
        link: last_link,
    }) = inlines.last_mut()
        && last_style == style
        && last_link.as_deref() == link
    {
        last.push_str(&text);
        return Ok(());
    }
    inlines.push(Inline::Text {
        text,
        style: style.clone(),
        link: link.map(str::to_string),
    });
    Ok(())
}

fn collapse_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_space = false;
    for ch in input.chars() {
        if ch == '\u{a0}' {
            // 不换行空格有排版意义，不能折叠成普通空格。
            out.push(ch);
            last_space = false;
        } else if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out
}

fn trim_inlines(inlines: &mut Vec<Inline>) {
    while let Some(Inline::Text { text, .. }) = inlines.first_mut() {
        let trimmed = trim_ws_start(text).to_string();
        if trimmed.is_empty() {
            inlines.remove(0);
        } else {
            *text = trimmed;
            break;
        }
    }
    while let Some(Inline::Text { text, .. }) = inlines.last_mut() {
        let trimmed = trim_ws_end(text).to_string();
        if trimmed.is_empty() {
            inlines.pop();
        } else {
            *text = trimmed;
            break;
        }
    }
    inlines.retain(|inline| !matches!(inline, Inline::Text { text, .. } if text.is_empty()));
}

/// 与 `str::trim_*` 不同，`&nbsp;` 是排版内容，不能被去掉。
fn trim_ws_start(value: &str) -> &str {
    value.trim_start_matches(|ch: char| ch.is_whitespace() && ch != '\u{a0}')
}

fn trim_ws_end(value: &str) -> &str {
    value.trim_end_matches(|ch: char| ch.is_whitespace() && ch != '\u{a0}')
}

fn block_style(style: &ComputedStyle) -> BlockStyle {
    BlockStyle {
        margin: style.margin,
        padding: style.padding,
        background: style.text.background,
        border: style.border,
        width: style.width,
        max_width: style.max_width,
        align: style.text.text_align,
        radius: style.radius,
        fit_content: matches!(style.display, Display::InlineBlock) && style.width.is_none(),
    }
}

fn layout_list(
    element: &ElementRef<'_>,
    style: &ComputedStyle,
    ctx: &mut ParseCtx<'_>,
    depth: usize,
    ordered: bool,
) -> Result<Block, HtmlRenderError> {
    let mut items = Vec::new();
    let mut index = 1u32;
    for child in element.child_elements() {
        if child.value().name() != "li" {
            continue;
        }
        let child_style = compute_style(&child, style, ctx)?;
        let marker = match style.list_style {
            ListStyle::None => String::new(),
            ListStyle::Decimal => format!("{index}. "),
            ListStyle::Circle => "◦ ".to_string(),
            ListStyle::Square => "▪ ".to_string(),
            ListStyle::Disc => "• ".to_string(),
        };
        let mut blocks = layout_flow(&child, &child_style, ctx, depth + 1)?;
        if has_box_style(&child_style) {
            blocks = vec![Block::Section {
                blocks,
                style: block_style(&child_style),
            }];
        }
        items.push(ListItem { marker, blocks });
        index += 1;
    }
    Ok(Block::List {
        ordered,
        items,
        style: block_style(style),
    })
}

fn collect_pre_text(
    element: &ElementRef<'_>,
    out: &mut String,
    ctx: &mut ParseCtx<'_>,
) -> Result<(), HtmlRenderError> {
    for child in element.children() {
        match child.value() {
            Node::Text(text) => {
                ctx.add_text_bytes(text.len())?;
                out.push_str(text);
            }
            Node::Element(_) => {
                let Some(element) = ElementRef::wrap(child) else {
                    continue;
                };
                if element.value().name() == "br" {
                    out.push('\n');
                } else if !SKIP_TAGS.contains(&element.value().name()) {
                    collect_pre_text(&element, out, ctx)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn image_ref(
    element: &ElementRef<'_>,
    style: &ComputedStyle,
    link: Option<&str>,
) -> Option<ImageRef> {
    let src = filter_image_src(element.attr("src").unwrap_or(""))?;
    Some(ImageRef {
        src,
        alt: element.attr("alt").unwrap_or("").trim().to_string(),
        width: style.width,
        height: style.height,
        align: style.text.text_align,
        link: link.map(str::to_string),
        radius: style.radius,
    })
}

fn filter_link(href: &str) -> Option<String> {
    let href = href.trim();
    if href.starts_with("mailto:") {
        return Some(href.to_string());
    }
    if href.starts_with("http://") || href.starts_with("https://") {
        return url::Url::parse(href).ok().map(|_| href.to_string());
    }
    None
}

fn filter_image_src(src: &str) -> Option<String> {
    let src = src.trim();
    if src.starts_with("cid:") || src.starts_with("data:image/") {
        return Some(src.to_string());
    }
    if src.starts_with("//") {
        return Some(src.to_string());
    }
    if (src.starts_with("http://") || src.starts_with("https://")) && url::Url::parse(src).is_ok() {
        return Some(src.to_string());
    }
    None
}

fn parse_table(
    element: &ElementRef<'_>,
    style: &ComputedStyle,
    ctx: &mut ParseCtx<'_>,
    depth: usize,
) -> Result<Table, HtmlRenderError> {
    let cell_spacing = element
        .attr("cellspacing")
        .and_then(|value| value.trim().parse::<f32>().ok())
        .unwrap_or(if style.border_collapse { 0.0 } else { 2.0 });
    let cellpadding = element
        .attr("cellpadding")
        .and_then(|value| value.trim().parse::<f32>().ok())
        .unwrap_or(0.0);
    let mut table = Table {
        rows: Vec::new(),
        columns: 0,
        border_collapse: style.border_collapse,
        cell_spacing,
        style: block_style(style),
    };

    for row_element in table_rows(element) {
        let row_style = compute_style(&row_element, style, ctx)?;
        let mut cells = Vec::new();
        let mut columns = 0usize;
        for cell_element in row_element.child_elements() {
            let tag = cell_element.value().name();
            if tag != "td" && tag != "th" {
                continue;
            }
            let cell_style = compute_style(&cell_element, &row_style, ctx)?;
            // `cellpadding` 是表格给单元格的默认内边距；作者 CSS 更大时取较大值。
            let mut padding = cell_style.padding;
            if cellpadding > 0.0 {
                for edge in [
                    &mut padding.top,
                    &mut padding.right,
                    &mut padding.bottom,
                    &mut padding.left,
                ] {
                    let current = edge.resolve(cellpadding).unwrap_or(0.0);
                    if current < cellpadding {
                        *edge = Length::Px(cellpadding);
                    }
                }
            }
            let colspan = parse_span(cell_element.attr("colspan"));
            let rowspan = parse_span(cell_element.attr("rowspan"));
            let blocks = layout_flow(&cell_element, &cell_style, ctx, depth + 1)?;
            let background = cell_style
                .text
                .background
                .or(row_style.text.background)
                .or(style.text.background);
            let border = cell_style.border.or(row_style.border).or(style.border);
            cells.push(TableCell {
                blocks,
                colspan,
                rowspan,
                background,
                border,
                align: cell_style.text.text_align,
                valign: cell_style.vertical_align,
                width: cell_style.width,
                padding,
                radius: cell_style.radius,
            });
            columns += colspan;
        }
        table.columns = table.columns.max(columns);
        table.rows.push(TableRow { cells });
    }
    Ok(table)
}

fn table_rows<'a>(element: &ElementRef<'a>) -> Vec<ElementRef<'a>> {
    let mut rows = Vec::new();
    for child in element.child_elements() {
        match child.value().name() {
            "tr" => rows.push(child),
            "thead" | "tbody" | "tfoot" => {
                rows.extend(
                    child
                        .child_elements()
                        .filter(|row| row.value().name() == "tr"),
                );
            }
            _ => {}
        }
    }
    rows
}

fn parse_span(value: Option<&str>) -> usize {
    value
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 20)
}
