//! 轻量富文本渲染：把 HTML / Markdown 转成 egui 可绘制的块。
//!
//! 明确不是浏览器：不执行脚本、不加载远程图片、不解析 CSS。
//! 目标是把邮件正文的段落、标题、列表、引用、链接、粗体/斜体、代码块还原到可读状态。
//! 原始 HTML 仍随 `raw/*.eml.gz` 保留，v0.2 再做完整渲染。

use eframe::egui::{self, Color32, RichText};

// ---------------------------------------------------------------------------
// 块模型
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub link: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    Paragraph(Vec<Span>),
    ListItem {
        ordered: bool,
        index: Option<u32>,
        spans: Vec<Span>,
    },
    Quote(Vec<Span>),
    Code(String),
    Rule,
    /// 正文图片：`cid:` / `data:` / 远程 URL。渲染前由阅读页解析为纹理。
    Image {
        src: String,
        alt: String,
    },
    /// 简单表格：每行若干单元格，每个单元格是行内样式片段。
    Table {
        rows: Vec<TableRow>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRow {
    pub cells: Vec<Vec<Span>>,
    pub header: bool,
}

// ---------------------------------------------------------------------------
// HTML
// ---------------------------------------------------------------------------

pub fn html_to_blocks(html: &str) -> Vec<Block> {
    if html.trim().is_empty() {
        return Vec::new();
    }
    let cleaned = strip_tag_blocks(&strip_tag_blocks(html, "script"), "style");
    let mut parser = HtmlParser::new();
    parser.run(&cleaned);
    parser.finish()
}

struct HtmlParser {
    blocks: Vec<Block>,
    spans: Vec<Span>,
    bold: u32,
    italic: u32,
    code: u32,
    link: Option<String>,
    kind: Kind,
    ordered: bool,
    list_index: u32,
    pre: bool,
    pre_text: String,
    table: Option<TableBuild>,
}

/// 表格解析的中间状态。当前单元格的片段仍放在 `HtmlParser::spans` 中。
#[derive(Default)]
struct TableBuild {
    rows: Vec<TableRow>,
    current: Vec<Vec<Span>>,
    header: bool,
    cell_open: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Paragraph,
    Heading(u8),
    ListItem,
    Quote,
}

impl HtmlParser {
    fn new() -> Self {
        Self {
            blocks: Vec::new(),
            spans: Vec::new(),
            bold: 0,
            italic: 0,
            code: 0,
            link: None,
            kind: Kind::Paragraph,
            ordered: false,
            list_index: 0,
            pre: false,
            pre_text: String::new(),
            table: None,
        }
    }

    fn run(&mut self, html: &str) {
        let mut i = 0usize;
        let bytes = html.as_bytes();
        while i < html.len() {
            if bytes[i] == b'<' {
                let Some(rel_end) = html[i..].find('>') else {
                    // 未闭合的标签，余下当文本处理。
                    self.push_text(&html[i..]);
                    break;
                };
                let end = i + rel_end;
                let inner = &html[i + 1..end];
                if inner.starts_with("!--") {
                    if let Some(rel) = html[end..].find("-->") {
                        i = end + rel + 3;
                        continue;
                    }
                    break;
                }
                self.handle_tag(inner);
                i = end + 1;
            } else {
                let next = html[i..].find('<').map(|p| i + p).unwrap_or(html.len());
                self.push_text(&html[i..next]);
                i = next;
            }
        }
    }

    fn finish(mut self) -> Vec<Block> {
        if self.pre {
            self.pre = false;
            let text = std::mem::take(&mut self.pre_text);
            if !text.trim().is_empty() {
                self.blocks.push(Block::Code(text.trim_end().to_string()));
            }
        }
        if self.table.is_some() {
            self.finish_table();
        }
        self.flush();
        self.blocks
    }

    fn handle_tag(&mut self, inner: &str) {
        let trimmed = inner.trim();
        if trimmed.is_empty() || trimmed.starts_with('!') || trimmed.starts_with('?') {
            return;
        }
        let closing = trimmed.starts_with('/');
        let body = trimmed.trim_start_matches('/').trim();
        let name_end = body
            .find(|ch: char| ch.is_whitespace() || ch == '/')
            .unwrap_or(body.len());
        let name = body[..name_end].to_ascii_lowercase();
        let attrs = &body[name_end..];

        if self.pre {
            if closing && name == "pre" {
                self.pre = false;
                let text = std::mem::take(&mut self.pre_text);
                if !text.trim().is_empty() {
                    self.blocks.push(Block::Code(text.trim_end().to_string()));
                }
            }
            // pre 内部的其它标签一律忽略，保留文本原样。
            return;
        }

        if closing {
            match name.as_str() {
                "td" | "th" => {
                    self.commit_table_cell();
                    return;
                }
                "tr" => {
                    self.commit_table_row();
                    return;
                }
                "table" => {
                    self.finish_table();
                    return;
                }
                "thead" | "tbody" | "tfoot" => return,
                "p" | "div" | "ul" | "ol" | "li" | "blockquote" => {
                    self.flush();
                }
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => self.flush(),
                "pre" => self.flush(),
                "b" | "strong" => self.bold = self.bold.saturating_sub(1),
                "i" | "em" => self.italic = self.italic.saturating_sub(1),
                "code" => self.code = self.code.saturating_sub(1),
                "a" => self.link = None,
                _ => {}
            }
            return;
        }

        match name.as_str() {
            "br" => self.push_break(),
            "hr" => {
                self.flush();
                self.blocks.push(Block::Rule);
            }
            "p" | "div" => {
                self.flush();
                self.kind = Kind::Paragraph;
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush();
                let level = name[1..].parse::<u8>().unwrap_or(3);
                self.kind = Kind::Heading(level);
            }
            "blockquote" => {
                self.flush();
                self.kind = Kind::Quote;
            }
            "ul" => {
                self.flush();
                self.ordered = false;
                self.list_index = 0;
            }
            "ol" => {
                self.flush();
                self.ordered = true;
                self.list_index = 0;
            }
            "li" => {
                self.flush();
                self.kind = Kind::ListItem;
                self.list_index = self.list_index.saturating_add(1);
            }
            "table" => {
                // 嵌套表格忽略：只保留最外层结构。
                if self.table.is_none() {
                    self.flush();
                    self.table = Some(TableBuild::default());
                }
            }
            "thead" | "tbody" | "tfoot" => {}
            "tr" => {
                if self.table.is_some() {
                    self.commit_table_cell();
                    self.commit_table_row();
                } else {
                    self.flush();
                    self.kind = Kind::Paragraph;
                }
            }
            "td" | "th" => {
                if self.table.is_some() {
                    self.commit_table_cell();
                    self.spans.clear();
                    if let Some(table) = self.table.as_mut() {
                        if name == "th" {
                            table.header = true;
                        }
                        table.cell_open = true;
                    }
                } else {
                    self.push_text("  ");
                }
            }
            "pre" => {
                self.flush();
                self.pre = true;
                self.pre_text.clear();
            }
            "b" | "strong" => self.bold = self.bold.saturating_add(1),
            "i" | "em" => self.italic = self.italic.saturating_add(1),
            "code" => self.code = self.code.saturating_add(1),
            "a" => {
                self.link = attribute(attrs, "href").map(|href| decode_entities(&href));
            }
            "img" => {
                let alt = attribute(attrs, "alt")
                    .map(|value| {
                        collapse_whitespace(&decode_entities(&value))
                            .trim()
                            .to_string()
                    })
                    .unwrap_or_default();
                let src = attribute(attrs, "src")
                    .map(|value| decode_entities(&value))
                    .unwrap_or_default();
                if self.table.is_some() {
                    // 表格单元格内不单独出图，保留替代文本。
                    if !alt.is_empty() {
                        self.push_text(&format!("[图片: {alt}]"));
                    }
                    return;
                }
                self.flush();
                if !src.trim().is_empty() {
                    self.blocks.push(Block::Image { src, alt });
                } else if !alt.is_empty() {
                    self.push_text(&format!("[图片: {alt}]"));
                    self.flush();
                }
            }
            _ => {}
        }
    }

    fn push_break(&mut self) {
        if self.pre {
            self.pre_text.push('\n');
            return;
        }
        if let Some(last) = self.spans.last_mut() {
            last.text.push('\n');
        } else {
            self.spans.push(Span {
                text: "\n".to_string(),
                bold: false,
                italic: false,
                code: false,
                link: None,
            });
        }
    }

    fn push_text(&mut self, raw: &str) {
        if raw.is_empty() {
            return;
        }
        let decoded = decode_entities(raw);
        if self.pre {
            self.pre_text.push_str(&decoded);
            return;
        }
        let text = collapse_whitespace(&decoded);
        if text.is_empty() {
            return;
        }
        let bold = self.bold > 0;
        let italic = self.italic > 0;
        let code = self.code > 0;
        let link = self.link.clone();
        if let Some(last) = self.spans.last_mut()
            && last.bold == bold
            && last.italic == italic
            && last.code == code
            && last.link == link
        {
            last.text.push_str(&text);
            return;
        }
        self.spans.push(Span {
            text,
            bold,
            italic,
            code,
            link,
        });
    }

    fn flush(&mut self) {
        // 表格内：单元格内容由 td/th/tr 边界提交，不生成普通段落。
        if self.table.is_some() {
            return;
        }
        if self.spans.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        if spans.iter().all(|span| span.text.trim().is_empty()) {
            self.spans.clear();
            return;
        }
        let block = match self.kind {
            Kind::Heading(level) => Block::Heading { level, spans },
            Kind::ListItem => Block::ListItem {
                ordered: self.ordered,
                index: if self.ordered {
                    Some(self.list_index)
                } else {
                    None
                },
                spans,
            },
            Kind::Quote => Block::Quote(spans),
            Kind::Paragraph => Block::Paragraph(spans),
        };
        self.blocks.push(block);
        // 块结束后默认回到普通段落；列表/标题由后续标签重新设置。
        self.kind = Kind::Paragraph;
    }

    fn commit_table_cell(&mut self) {
        let open = self.table.as_ref().is_some_and(|table| table.cell_open);
        if !open {
            self.spans.clear();
            return;
        }
        let mut spans = std::mem::take(&mut self.spans);
        spans.retain(|span| !span.text.is_empty());
        if let Some(table) = self.table.as_mut() {
            table.current.push(spans);
            table.cell_open = false;
        }
    }

    fn commit_table_row(&mut self) {
        self.commit_table_cell();
        if let Some(table) = self.table.as_mut() {
            if !table.current.is_empty() {
                let cells = std::mem::take(&mut table.current);
                table.rows.push(TableRow {
                    cells,
                    header: table.header,
                });
            }
            table.header = false;
        }
    }

    fn finish_table(&mut self) {
        self.commit_table_row();
        if let Some(table) = self.table.take()
            && !table.rows.is_empty()
        {
            self.blocks.push(Block::Table { rows: table.rows });
        }
    }
}

fn strip_tag_blocks(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}");
    let mut out = String::with_capacity(html.len());
    let mut i = 0usize;
    while let Some(rel) = lower[i..].find(&open) {
        let start = i + rel;
        out.push_str(&html[i..start]);
        let Some(end_open) = lower[start..].find('>') else {
            return out;
        };
        let after_open = start + end_open + 1;
        if let Some(rel_close) = lower[after_open..].find(&close) {
            let close_start = after_open + rel_close;
            if let Some(end_close) = lower[close_start..].find('>') {
                i = close_start + end_close + 1;
                continue;
            }
        }
        i = html.len();
    }
    out.push_str(&html[i..]);
    out
}

fn attribute(attrs: &str, name: &str) -> Option<String> {
    let lower = attrs.to_ascii_lowercase();
    let needle = format!("{name}=");
    let pos = lower.find(&needle)?;
    let rest = &attrs[pos + needle.len()..];
    let rest = rest.trim_start();
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(stripped[..end].to_string())
    } else if let Some(stripped) = rest.strip_prefix('\'') {
        let end = stripped.find('\'')?;
        Some(stripped[..end].to_string())
    } else {
        let end = rest
            .find(|ch: char| ch.is_whitespace() || ch == '>')
            .unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
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

fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch != '&' {
            out.push(ch);
            continue;
        }
        let mut entity = String::new();
        let mut closed = false;
        while let Some(&(_, next)) = chars.peek() {
            if next == ';' {
                chars.next();
                closed = true;
                break;
            }
            if entity.len() > 12 || next.is_whitespace() || next == '&' {
                break;
            }
            entity.push(next);
            chars.next();
        }
        if !closed {
            out.push('&');
            out.push_str(&entity);
            continue;
        }
        match entity.as_str() {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" | "#39" => out.push('\''),
            "nbsp" => out.push(' '),
            _ => {
                if let Some(hex) = entity
                    .strip_prefix("#x")
                    .or_else(|| entity.strip_prefix("#X"))
                {
                    if let Ok(code) = u32::from_str_radix(hex, 16)
                        && let Some(ch) = char::from_u32(code)
                    {
                        out.push(ch);
                        continue;
                    }
                } else if let Some(dec) = entity.strip_prefix('#')
                    && let Ok(code) = dec.parse::<u32>()
                    && let Some(ch) = char::from_u32(code)
                {
                    out.push(ch);
                    continue;
                }
                out.push('&');
                out.push_str(&entity);
                out.push(';');
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Markdown（常用子集）
// ---------------------------------------------------------------------------

pub fn markdown_to_blocks(markdown: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut paragraph: Vec<String> = Vec::new();
    let mut code: Option<String> = None;

    let flush_paragraph = |blocks: &mut Vec<Block>, paragraph: &mut Vec<String>| {
        if paragraph.is_empty() {
            return;
        }
        let text = paragraph.join("\n");
        paragraph.clear();
        if !text.trim().is_empty() {
            blocks.push(Block::Paragraph(inline_markdown(&text)));
        }
    };

    for line in markdown.lines() {
        if code.is_some() {
            if line.trim_start().starts_with("```") {
                let value = code.take().unwrap_or_default();
                blocks.push(Block::Code(value.trim_end().to_string()));
            } else if let Some(buffer) = code.as_mut() {
                buffer.push_str(line);
                buffer.push('\n');
            }
            continue;
        }
        if line.trim_start().starts_with("```") {
            flush_paragraph(&mut blocks, &mut paragraph);
            code = Some(String::new());
            continue;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            flush_paragraph(&mut blocks, &mut paragraph);
            continue;
        }
        if trimmed == "---" || trimmed == "***" || trimmed == "___" {
            flush_paragraph(&mut blocks, &mut paragraph);
            blocks.push(Block::Rule);
            continue;
        }
        if let Some((level, rest)) = markdown_heading(trimmed) {
            flush_paragraph(&mut blocks, &mut paragraph);
            blocks.push(Block::Heading {
                level,
                spans: inline_markdown(rest),
            });
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("> ") {
            flush_paragraph(&mut blocks, &mut paragraph);
            blocks.push(Block::Quote(inline_markdown(rest)));
            continue;
        }
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            flush_paragraph(&mut blocks, &mut paragraph);
            blocks.push(Block::ListItem {
                ordered: false,
                index: None,
                spans: inline_markdown(rest),
            });
            continue;
        }
        if let Some((index, rest)) = markdown_ordered_item(trimmed) {
            flush_paragraph(&mut blocks, &mut paragraph);
            blocks.push(Block::ListItem {
                ordered: true,
                index: Some(index),
                spans: inline_markdown(rest),
            });
            continue;
        }

        paragraph.push(trimmed.to_string());
    }

    if let Some(value) = code {
        blocks.push(Block::Code(value.trim_end().to_string()));
    }
    flush_paragraph(&mut blocks, &mut paragraph);
    blocks
}

fn markdown_heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.chars().take_while(|ch| *ch == '#').count();
    if (1..=6).contains(&hashes) && line.chars().nth(hashes) == Some(' ') {
        Some((hashes as u8, line[hashes..].trim_start()))
    } else {
        None
    }
}

fn markdown_ordered_item(line: &str) -> Option<(u32, &str)> {
    let digits = line.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let rest = &line[digits..];
    let rest = rest
        .strip_prefix(". ")
        .or_else(|| rest.strip_prefix(") "))?;
    let index = line[..digits].parse().ok()?;
    Some((index, rest))
}

fn inline_markdown(text: &str) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;

    let push = |spans: &mut Vec<Span>,
                text: String,
                bold: bool,
                italic: bool,
                code: bool,
                link: Option<String>| {
        if text.is_empty() {
            return;
        }
        if let Some(last) = spans.last_mut()
            && last.bold == bold
            && last.italic == italic
            && last.code == code
            && last.link == link
        {
            last.text.push_str(&text);
            return;
        }
        spans.push(Span {
            text,
            bold,
            italic,
            code,
            link,
        });
    };

    while i < chars.len() {
        // 行内代码
        if chars[i] == '`'
            && let Some(end) = (i + 1..chars.len()).find(|&j| chars[j] == '`')
        {
            if !buf.is_empty() {
                push(
                    &mut spans,
                    std::mem::take(&mut buf),
                    false,
                    false,
                    false,
                    None,
                );
            }
            let code: String = chars[i + 1..end].iter().collect();
            push(&mut spans, code, false, false, true, None);
            i = end + 1;
            continue;
        }
        // 链接 [text](url)
        if chars[i] == '['
            && let Some((label, url, next)) = parse_md_link(&chars, i)
        {
            if !buf.is_empty() {
                push(
                    &mut spans,
                    std::mem::take(&mut buf),
                    false,
                    false,
                    false,
                    None,
                );
            }
            push(&mut spans, label, false, false, false, Some(url));
            i = next;
            continue;
        }
        // 粗体 **text** 或 __text__
        if i + 1 < chars.len() && (chars[i] == '*' || chars[i] == '_') && chars[i + 1] == chars[i] {
            let marker = chars[i];
            if let Some(end) = find_double(&chars, i + 2, marker) {
                if !buf.is_empty() {
                    push(
                        &mut spans,
                        std::mem::take(&mut buf),
                        false,
                        false,
                        false,
                        None,
                    );
                }
                let inner: String = chars[i + 2..end].iter().collect();
                push(&mut spans, inner, true, false, false, None);
                i = end + 2;
                continue;
            }
        }
        // 斜体 *text* 或 _text_
        if chars[i] == '*' || chars[i] == '_' {
            let marker = chars[i];
            if let Some(end) = (i + 1..chars.len()).find(|&j| chars[j] == marker)
                && end > i + 1
            {
                if !buf.is_empty() {
                    push(
                        &mut spans,
                        std::mem::take(&mut buf),
                        false,
                        false,
                        false,
                        None,
                    );
                }
                let inner: String = chars[i + 1..end].iter().collect();
                push(&mut spans, inner, false, true, false, None);
                i = end + 1;
                continue;
            }
        }
        buf.push(chars[i]);
        i += 1;
    }
    if !buf.is_empty() {
        push(&mut spans, buf, false, false, false, None);
    }
    spans
}

fn find_double(chars: &[char], start: usize, marker: char) -> Option<usize> {
    let mut i = start;
    while i + 1 < chars.len() {
        if chars[i] == marker && chars[i + 1] == marker {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn parse_md_link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let close = (start + 1..chars.len()).find(|&i| chars[i] == ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let url_end = (close + 2..chars.len()).find(|&i| chars[i] == ')')?;
    let label: String = chars[start + 1..close].iter().collect();
    let url: String = chars[close + 2..url_end].iter().collect();
    Some((label, url, url_end + 1))
}

// ---------------------------------------------------------------------------
// 渲染
// ---------------------------------------------------------------------------

/// 阅读页解析好的图片纹理，按 `src` 索引。
pub struct ImageTextures<'a> {
    pub textures: &'a std::collections::HashMap<String, egui::TextureHandle>,
    /// 正在下载的远程图片 src。
    pub pending: &'a std::collections::HashSet<String>,
    /// 下载失败的远程图片 src。
    pub failed: &'a std::collections::HashSet<String>,
    /// 用户在设置中关闭了远程图片自动加载。
    pub remote_disabled: bool,
}

pub fn show_with_images(ui: &mut egui::Ui, blocks: &[Block], images: Option<&ImageTextures<'_>>) {
    if blocks.is_empty() {
        ui.label(RichText::new("（正文为空）").color(Color32::GRAY).italics());
        return;
    }
    for (block_index, block) in blocks.iter().enumerate() {
        match block {
            Block::Heading { level, spans } => {
                let size = match level {
                    1 => 24.0,
                    2 => 21.0,
                    3 => 18.0,
                    4 => 16.0,
                    _ => 15.0,
                };
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| show_spans(ui, spans, Some(size), false));
                ui.add_space(2.0);
            }
            Block::Paragraph(spans) => {
                ui.horizontal_wrapped(|ui| show_spans(ui, spans, None, false));
                ui.add_space(6.0);
            }
            Block::ListItem {
                ordered,
                index,
                spans,
            } => {
                ui.horizontal_wrapped(|ui| {
                    let marker = match (ordered, index) {
                        (true, Some(index)) => format!("{index}. "),
                        _ => "• ".to_string(),
                    };
                    ui.label(RichText::new(marker).color(Color32::GRAY));
                    show_spans(ui, spans, None, false);
                });
            }
            Block::Quote(spans) => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("▍").color(Color32::from_rgb(90, 140, 190)));
                    ui.scope(|ui| {
                        ui.visuals_mut().override_text_color = Some(Color32::GRAY);
                        show_spans(ui, spans, None, false);
                    });
                });
                ui.add_space(4.0);
            }
            Block::Code(text) => {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(text).monospace().size(13.0)).wrap());
                });
                ui.add_space(4.0);
            }
            Block::Rule => {
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);
            }
            Block::Table { rows } => {
                let columns = rows.iter().map(|row| row.cells.len()).max().unwrap_or(0);
                if columns == 0 {
                    continue;
                }
                ui.add_space(4.0);
                egui::Grid::new(("richtext-table", block_index))
                    .num_columns(columns)
                    .striped(true)
                    .spacing([12.0, 4.0])
                    .show(ui, |ui| {
                        for row in rows {
                            for cell in &row.cells {
                                if cell.iter().all(|span| span.text.trim().is_empty()) {
                                    ui.label(" ");
                                } else {
                                    ui.horizontal_wrapped(|ui| {
                                        show_spans(ui, cell, None, row.header);
                                    });
                                }
                            }
                            ui.end_row();
                        }
                    });
                ui.add_space(4.0);
            }
            Block::Image { src, alt } => {
                let texture = images.and_then(|images| images.textures.get(src));
                if let Some(texture) = texture {
                    let available = ui.available_width().max(64.0);
                    let size = texture.size_vec2();
                    let scale = (available / size.x.max(1.0)).min(1.0);
                    let display = egui::vec2(size.x * scale, size.y * scale);
                    ui.add_space(4.0);
                    ui.add(egui::Image::new((texture.id(), display)));
                    if !alt.trim().is_empty() {
                        ui.label(RichText::new(alt).small().color(Color32::GRAY));
                    }
                    ui.add_space(4.0);
                } else if src.starts_with("http://")
                    || src.starts_with("https://")
                    || src.starts_with("//")
                {
                    let pending = images.is_some_and(|images| images.pending.contains(src));
                    let failed = images.is_some_and(|images| images.failed.contains(src));
                    let disabled = images.is_some_and(|images| images.remote_disabled);
                    if pending && !disabled {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new("正在加载图片…").color(Color32::GRAY));
                        });
                    } else {
                        let (prefix, color) = if disabled {
                            ("🖼 远程图片已关闭", Color32::GRAY)
                        } else if failed {
                            ("🖼 图片加载失败", Color32::from_rgb(200, 90, 90))
                        } else {
                            ("🖼 远程图片", Color32::GRAY)
                        };
                        let label = if alt.trim().is_empty() {
                            format!("{prefix}（点击在浏览器打开）")
                        } else {
                            format!("{prefix}：{alt}（点击在浏览器打开）")
                        };
                        if ui.link(RichText::new(label).color(color)).clicked() {
                            let url = if src.starts_with("//") {
                                format!("https:{src}")
                            } else {
                                src.clone()
                            };
                            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                        }
                    }
                } else {
                    let label = if alt.trim().is_empty() {
                        "🖼 图片不可用".to_string()
                    } else {
                        format!("🖼 图片不可用：{alt}")
                    };
                    ui.label(RichText::new(label).color(Color32::GRAY));
                }
            }
        }
    }
}

fn show_spans(ui: &mut egui::Ui, spans: &[Span], heading_size: Option<f32>, force_bold: bool) {
    for span in spans {
        if let Some(url) = &span.link {
            let text = if span.text.trim().is_empty() {
                url.clone()
            } else {
                span.text.clone()
            };
            let response = ui.link(RichText::new(text));
            if response.clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(url.clone()));
            }
            continue;
        }
        let mut rich = RichText::new(&span.text);
        if let Some(size) = heading_size {
            rich = rich.size(size).strong();
        } else {
            if span.bold || force_bold {
                rich = rich.strong();
            }
            if span.italic {
                rich = rich.italics();
            }
        }
        if span.code {
            rich = rich
                .monospace()
                .background_color(ui.visuals().code_bg_color);
        }
        ui.add(egui::Label::new(rich).wrap());
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(blocks: &[Block]) -> String {
        let mut out = String::new();
        for block in blocks {
            match block {
                Block::Heading { spans, .. } | Block::Paragraph(spans) | Block::Quote(spans) => {
                    for span in spans {
                        out.push_str(&span.text);
                    }
                    out.push('\n');
                }
                Block::ListItem { spans, .. } => {
                    for span in spans {
                        out.push_str(&span.text);
                    }
                    out.push('\n');
                }
                Block::Code(text) => {
                    out.push_str(text);
                    out.push('\n');
                }
                Block::Rule => {}
                Block::Table { rows } => {
                    for row in rows {
                        for cell in &row.cells {
                            for span in cell {
                                out.push_str(&span.text);
                            }
                            out.push('\t');
                        }
                        out.push('\n');
                    }
                }
                Block::Image { alt, .. } => {
                    if !alt.is_empty() {
                        out.push_str(alt);
                        out.push('\n');
                    }
                }
            }
        }
        out
    }

    #[test]
    fn html_headings_paragraphs_and_inline_styles() {
        let blocks = html_to_blocks(
            "<h1>标题</h1><p>普通 <b>粗体</b> 与 <i>斜体</i> 和 <code>code</code></p>",
        );
        assert!(matches!(&blocks[0], Block::Heading { level: 1, .. }));
        assert!(matches!(&blocks[1], Block::Paragraph(_)));
        let text = text_of(&blocks);
        assert!(text.contains("标题"));
        assert!(text.contains("粗体"));
        assert!(text.contains("斜体"));
        assert!(text.contains("code"));
    }

    #[test]
    fn html_lists_quotes_links_and_rules() {
        let blocks = html_to_blocks(
            "<ul><li>一</li><li>二</li></ul><blockquote>引用</blockquote><hr>\
             <p><a href=\"https://example.com\">链接</a></p>",
        );
        assert!(matches!(&blocks[0], Block::ListItem { ordered: false, .. }));
        assert!(matches!(&blocks[1], Block::ListItem { .. }));
        assert!(matches!(&blocks[2], Block::Quote(_)));
        assert!(matches!(&blocks[3], Block::Rule));
        match &blocks[4] {
            Block::Paragraph(spans) => {
                assert_eq!(
                    spans.first().and_then(|s| s.link.as_deref()),
                    Some("https://example.com")
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn html_strips_script_style_and_keeps_image_blocks() {
        let blocks = html_to_blocks(
            "<style>p{color:red}</style><script>alert(1)</script>\
             <p>正文</p><img alt=\"logo\">",
        );
        let text = text_of(&blocks);
        assert!(!text.contains("alert"));
        assert!(!text.contains("color:red"));
        assert!(text.contains("正文"));
        assert!(text.contains("[图片: logo]"));
    }

    #[test]
    fn html_br_preserves_line_break() {
        let blocks = html_to_blocks("<p>first<br>second</p>");
        let text = text_of(&blocks);
        assert!(
            text.contains(
                "first
second"
            ),
            "got {text:?}"
        );
    }

    #[test]
    fn html_entities_are_decoded() {
        let blocks = html_to_blocks("<p>A &amp; B &lt; C &#20013;&#x6587; &nbsp; D</p>");
        let text = text_of(&blocks);
        assert!(text.contains("A & B < C 中文"), "got {text:?}");
    }

    #[test]
    fn html_pre_is_preserved_as_code() {
        let blocks = html_to_blocks("<pre>fn main() {\n    println!(\"hi\");\n}</pre>");
        assert!(matches!(&blocks[0], Block::Code(text) if text.contains("println!(\"hi\")")));
    }

    #[test]
    fn html_table_becomes_grid() {
        let blocks = html_to_blocks(
            "<table><tr><th>名称</th><th>数量</th></tr><tr><td>A</td><td>1</td></tr></table>",
        );
        match &blocks[0] {
            Block::Table { rows } => {
                assert_eq!(rows.len(), 2);
                assert!(rows[0].header);
                assert_eq!(rows[1].cells.len(), 2);
                assert_eq!(rows[1].cells[0][0].text, "A");
                assert_eq!(rows[1].cells[1][0].text, "1");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn html_table_with_paragraphs_inside_cells() {
        let blocks =
            html_to_blocks("<table><tr><td><p>第一段</p><p>第二段</p></td><td>B</td></tr></table>");
        match &blocks[0] {
            Block::Table { rows } => {
                assert_eq!(rows.len(), 1);
                let text: String = rows[0].cells[0]
                    .iter()
                    .map(|span| span.text.as_str())
                    .collect();
                assert!(text.contains("第一段"), "got {text:?}");
                assert!(text.contains("第二段"), "got {text:?}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn markdown_headings_lists_code_and_inline() {
        let blocks = markdown_to_blocks(
            "# 标题\n\n**粗** 和 *斜* 和 `code`\n\n- 甲\n- 乙\n\n```\nlet x = 1;\n```\n\n> 引用\n\n[链接](https://example.com)\n",
        );
        assert!(matches!(&blocks[0], Block::Heading { level: 1, .. }));
        let text = text_of(&blocks);
        assert!(text.contains("粗"));
        assert!(text.contains("斜"));
        assert!(text.contains("code"));
        assert!(text.contains("甲"));
        assert!(text.contains("let x = 1;"));
        assert!(text.contains("引用"));
    }
}
