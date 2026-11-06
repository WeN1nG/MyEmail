//! HTML 渲染器 v2 的样式模型与 CSS 子集。
//!
//! 只支持邮件常见排版：块/行内、字体与颜色、背景、边框、margin/padding、
//! 宽度、对齐、行高、white-space、列表与表格属性。定位/浮动/flex/grid 一律按
//! 块级文档流降级；不加载外部 CSS/字体，不执行脚本。

use eframe::egui::Color32;
use scraper::{ElementRef, Selector};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Display {
    Block,
    #[default]
    Inline,
    InlineBlock,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerticalAlign {
    #[default]
    Top,
    Middle,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WhiteSpace {
    #[default]
    Normal,
    Pre,
    PreWrap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FontFamily {
    #[default]
    Proportional,
    Monospace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListStyle {
    #[default]
    Disc,
    Circle,
    Square,
    Decimal,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Length {
    Px(f32),
    Percent(f32),
    Auto,
}

impl Length {
    pub fn resolve(self, base: f32) -> Option<f32> {
        match self {
            Length::Px(value) => Some(value),
            Length::Percent(value) => Some(base * value / 100.0),
            Length::Auto => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Edges {
    pub top: Length,
    pub right: Length,
    pub bottom: Length,
    pub left: Length,
}

impl Default for Length {
    fn default() -> Self {
        Length::Px(0.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Border {
    pub width: f32,
    pub color: Color32,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TextStyle {
    pub color: Option<Color32>,
    pub background: Option<Color32>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub code: bool,
    pub font_size: Option<f32>,
    pub font_family: FontFamily,
    pub line_height: Option<f32>,
    pub text_align: TextAlign,
    pub white_space: WhiteSpace,
    pub letter_spacing: Option<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ComputedStyle {
    pub text: TextStyle,
    pub display: Display,
    pub margin: Edges,
    pub padding: Edges,
    pub width: Option<Length>,
    pub height: Option<Length>,
    pub max_width: Option<Length>,
    pub border: Option<Border>,
    pub border_collapse: bool,
    pub vertical_align: VerticalAlign,
    pub list_style: ListStyle,
    pub radius: f32,
}

impl Default for ComputedStyle {
    fn default() -> Self {
        Self {
            text: TextStyle::default(),
            display: Display::Inline,
            margin: Edges::default(),
            padding: Edges::default(),
            width: None,
            height: None,
            max_width: None,
            border: None,
            border_collapse: false,
            vertical_align: VerticalAlign::Top,
            list_style: ListStyle::Disc,
            radius: 0.0,
        }
    }
}

impl ComputedStyle {
    pub fn root() -> Self {
        Self {
            display: Display::Block,
            text: TextStyle {
                font_size: Some(14.0),
                line_height: Some(1.45),
                ..TextStyle::default()
            },
            ..Self::default()
        }
    }

    /// 子元素初始样式：只继承文本属性，块属性归零。
    pub fn inherit(parent: &Self) -> Self {
        let mut text = parent.text.clone();
        // CSS 的 background 不继承；否则容器的底色会重复画到每个子块上。
        // 行内元素自己的底色仍由 apply_declaration 写回 TextStyle.background。
        text.background = None;
        Self {
            text,
            display: Display::Inline,
            ..Self::default()
        }
    }
}

pub fn is_block_tag(tag: &str) -> bool {
    matches!(
        tag,
        "html"
            | "body"
            | "div"
            | "p"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "main"
            | "aside"
            | "nav"
            | "address"
            | "center"
            | "figure"
            | "figcaption"
            | "blockquote"
            | "pre"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "ul"
            | "ol"
            | "li"
            | "table"
            | "thead"
            | "tbody"
            | "tfoot"
            | "tr"
            | "td"
            | "th"
            | "hr"
            | "dl"
            | "dt"
            | "dd"
    )
}

// ---------------------------------------------------------------------------
// 样式表
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Declaration {
    name: String,
    value: String,
}

#[derive(Debug, Clone)]
struct Rule {
    selector: Selector,
    specificity: (u16, u16, u16),
    order: usize,
    declarations: Vec<Declaration>,
    /// 桶键（见 [`RuleKey`]）。`None` = 取不出必要条件，对每个元素都要试。
    key: Option<RuleKey>,
}

/// 规则的**桶键**：命中该选择器的**必要条件**。
///
/// 键只用来做**候选筛选**（跳过一定不匹配的规则），最终是否匹配仍由
/// `Selector::matches` 决定 —— 所以键取错最多是「多试几条」，不会漏匹配。
///
/// 大小写：键一律转小写存储与查找。这让筛选**偏宽**（保守方向）：
/// 例如规则 `.Foo` 与元素 `class="foo"` 会落进同一个桶，但 `matches()` 会否掉它。
/// 反过来若按大小写精确匹配，一旦浏览器按 quirks 模式放宽 class 匹配就会**漏掉**规则。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RuleKey {
    /// `#id`
    Id(String),
    /// `.class`
    Class(String),
    /// 标签名
    Tag(String),
}

#[derive(Debug, Clone, Default)]
pub struct Stylesheet {
    rules: Vec<Rule>,
    /// 规则下标，按 `(specificity, order)` 排好序 —— 即 CSS 层叠顺序。
    /// 排一次即可，不必每个元素重排。
    ordered: Vec<u32>,
    /// 取不出键的规则：对每个元素都要试。
    universal: Vec<u32>,
    /// 按键分桶，桶内是 `ordered` 里的下标（已按层叠顺序递增）。
    buckets: std::collections::HashMap<RuleKey, Vec<u32>>,
}

impl Stylesheet {
    pub fn parse(css: &str) -> Self {
        let mut sheet = Self::default();
        sheet.parse_block(&strip_comments(css));
        sheet.build_index();
        sheet
    }

    /// 排序 + 分桶。解析完调一次。
    fn build_index(&mut self) {
        let mut ordered: Vec<u32> = (0..self.rules.len() as u32).collect();
        ordered.sort_by_key(|&index| {
            let rule = &self.rules[index as usize];
            (rule.specificity, rule.order)
        });
        self.ordered = ordered;

        // 桶里存的是**层叠序号**（在 `ordered` 中的位置），不是规则下标。
        // 层叠序号递增 = 层叠顺序递增；规则下标则与层叠无关（源文件里靠前的
        // 规则可能特异度更低）。混用会让高特异度的规则被低特异度的覆盖。
        let mut universal = Vec::new();
        let mut buckets: std::collections::HashMap<RuleKey, Vec<u32>> =
            std::collections::HashMap::new();
        for (rank, &rule_index) in self.ordered.iter().enumerate() {
            let rule = &self.rules[rule_index as usize];
            match &rule.key {
                Some(key) => {
                    buckets.entry(key.clone()).or_default().push(rank as u32);
                }
                None => universal.push(rank as u32),
            }
        }
        self.universal = universal;
        self.buckets = buckets;
    }

    /// 把可能命中该元素的规则收集到 `scratch`（层叠下标）。
    ///
    /// 只收「键命中」的桶 + 无键规则；**最终判定仍走 `Selector::matches`**。
    /// 返回的候选需要按层叠顺序应用，故调用方要排序（桶内已递增，跨桶需归并）。
    fn collect_candidates(&self, element: &ElementRef<'_>, scratch: &mut Vec<u32>) {
        scratch.clear();
        scratch.extend_from_slice(&self.universal);

        if let Some(id) = element.value().id()
            && let Some(bucket) = self.buckets.get(&RuleKey::Id(id.to_lowercase()))
        {
            scratch.extend_from_slice(bucket);
        }
        for class in element.value().classes() {
            if let Some(bucket) = self.buckets.get(&RuleKey::Class(class.to_lowercase())) {
                scratch.extend_from_slice(bucket);
            }
        }
        if let Some(bucket) = self
            .buckets
            .get(&RuleKey::Tag(element.value().name().to_lowercase()))
        {
            scratch.extend_from_slice(bucket);
        }
    }

    fn parse_block(&mut self, css: &str) {
        let mut rest = css;
        loop {
            let open = rest.find('{');
            let semicolon = rest.find(';');
            let Some(open) = open else {
                break;
            };
            if let Some(semicolon) = semicolon
                && semicolon < open
            {
                // 顶层无块语句（@import 等）直接忽略。
                rest = &rest[semicolon + 1..];
                continue;
            }
            let prelude = rest[..open].trim();
            let Some(close) = matching_brace(&rest[open + 1..]) else {
                break;
            };
            let body = &rest[open + 1..open + 1 + close];
            rest = &rest[open + 1 + close + 1..];

            if prelude.starts_with('@') {
                let lower = prelude.to_ascii_lowercase();
                if lower.starts_with("@media")
                    && (lower.contains("screen") || lower.contains("all"))
                {
                    self.parse_block(body);
                }
                continue;
            }

            let declarations = parse_declarations(body);
            if declarations.is_empty() {
                continue;
            }
            for part in split_selector_list(prelude) {
                let Ok(selector) = Selector::parse(part) else {
                    continue;
                };
                let order = self.rules.len();
                self.rules.push(Rule {
                    selector,
                    specificity: selector_specificity(part),
                    order,
                    declarations: declarations.clone(),
                    key: rule_key(part),
                });
            }
        }
    }

    pub fn apply(&self, style: &mut ComputedStyle, element: &ElementRef<'_>) {
        // 先按桶收候选，再按层叠顺序应用。
        //
        // 候选来自「键命中」的桶 + 无键规则；**最终判定仍走 `Selector::matches`** ——
        // 桶只是「可能命中」的快速筛子，键取错只会多试几条，不会漏匹配。
        let mut candidates: Vec<u32> = Vec::new();
        self.collect_candidates(element, &mut candidates);
        if candidates.is_empty() {
            return;
        }
        // 候选存的是**层叠序号**，数值递增即层叠顺序递增；跨桶归并后排序即可。
        candidates.sort_unstable();
        candidates.dedup();
        for rank in candidates {
            let rule = &self.rules[self.ordered[rank as usize] as usize];
            if !rule.selector.matches(element) {
                continue;
            }
            for declaration in &rule.declarations {
                apply_declaration(style, &declaration.name, &declaration.value);
            }
        }
    }
}

pub fn apply_inline_style(style: &mut ComputedStyle, css: &str) {
    for declaration in parse_declarations(css) {
        apply_declaration(style, &declaration.name, &declaration.value);
    }
}

pub fn apply_html_attributes(style: &mut ComputedStyle, element: &ElementRef<'_>) {
    let tag = element.value().name();
    if let Some(color) = element.attr("bgcolor").and_then(parse_color) {
        style.text.background = Some(color);
    }
    if let Some(color) = element.attr("color").and_then(parse_color) {
        style.text.color = Some(color);
    }
    match tag {
        "center" => style.text.text_align = TextAlign::Center,
        "ul" => style.list_style = ListStyle::Disc,
        "ol" => style.list_style = ListStyle::Decimal,
        "pre" => {
            style.text.white_space = WhiteSpace::Pre;
            style.text.font_family = FontFamily::Monospace;
        }
        "b" | "strong" => style.text.bold = true,
        "i" | "em" => style.text.italic = true,
        "u" => style.text.underline = true,
        "s" | "strike" | "del" => style.text.strike = true,
        "code" | "kbd" | "samp" | "tt" => {
            style.text.code = true;
            style.text.font_family = FontFamily::Monospace;
        }
        "font" => {
            if let Some(size) = element.attr("size").and_then(parse_font_size) {
                style.text.font_size = Some(size);
            }
        }
        _ => {}
    }
    if let Some(align) = element.attr("align") {
        // `<table align="center">` 是表格整体居中，不是内容文本居中；
        // 不能让它继承到单元格文字上。
        if tag != "table" {
            style.text.text_align = match align.to_ascii_lowercase().as_str() {
                "center" => TextAlign::Center,
                "right" => TextAlign::Right,
                _ => TextAlign::Left,
            };
        }
    }
    if let Some(valign) = element.attr("valign") {
        style.vertical_align = match valign.to_ascii_lowercase().as_str() {
            "middle" => VerticalAlign::Middle,
            "bottom" => VerticalAlign::Bottom,
            _ => VerticalAlign::Top,
        };
    }
    if let Some(width) = element.attr("width").and_then(parse_attribute_length) {
        style.width = Some(width);
    }
    if let Some(height) = element.attr("height").and_then(parse_attribute_length) {
        style.height = Some(height);
    }
    if tag == "table" {
        if element
            .attr("align")
            .is_some_and(|value| value.eq_ignore_ascii_case("center"))
        {
            // HTML 的 `table align=center` 语义是表格整体居中。
            style.margin.left = Length::Auto;
            style.margin.right = Length::Auto;
        }
        if let Some(border) = element
            .attr("border")
            .and_then(|value| value.trim().parse::<f32>().ok())
            && border > 0.0
        {
            style.border = Some(Border {
                width: border,
                color: Color32::from_gray(180),
            });
        }
        if let Some(collapse) = element.attr("cellspacing") {
            style.border_collapse = collapse.trim() == "0";
        }
    }
}

// ---------------------------------------------------------------------------
// 声明
// ---------------------------------------------------------------------------

fn parse_declarations(css: &str) -> Vec<Declaration> {
    let mut declarations = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for ch in css.chars() {
        match ch {
            '"' | '\'' if quote.is_none() => {
                quote = Some(ch);
                current.push(ch);
            }
            c if Some(c) == quote => {
                quote = None;
                current.push(c);
            }
            '(' if quote.is_none() => {
                depth += 1;
                current.push(ch);
            }
            ')' if quote.is_none() => {
                depth -= 1;
                current.push(ch);
            }
            ';' if quote.is_none() && depth == 0 => {
                push_declaration(&mut declarations, &current);
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    push_declaration(&mut declarations, &current);
    declarations
}

fn push_declaration(declarations: &mut Vec<Declaration>, raw: &str) {
    let Some((name, value)) = raw.split_once(':') else {
        return;
    };
    let name = name.trim().to_ascii_lowercase();
    let mut value = value.trim().to_string();
    loop {
        let trimmed = value.trim_end();
        let Some(stripped) = trimmed
            .strip_suffix("!important")
            .or_else(|| trimmed.strip_suffix("! important"))
        else {
            break;
        };
        let next = stripped.trim_end().to_string();
        value = next;
    }
    if name.is_empty() || value.is_empty() {
        return;
    }
    let lower = value.to_ascii_lowercase();
    if lower.contains("expression(") || lower.contains("@import") {
        return;
    }
    // 外部资源一律不加载；但保留同一简写里的颜色等可用信息
    // （例如 `background: #fff url(tracker.gif)` 仍取出 #fff）。
    let value = strip_url_functions(&value);
    let value = value.trim().to_string();
    if value.is_empty() {
        return;
    }
    declarations.push(Declaration { name, value });
}

/// 移除 CSS 值里的 `url(...)` 函数，保留其余 token。
fn strip_url_functions(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    if !lower.contains("url(") {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.to_ascii_lowercase().find("url(") {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 4..];
        let end = tail
            .find(')')
            .map(|index| start + 4 + index + 1)
            .unwrap_or(rest.len());
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn apply_declaration(style: &mut ComputedStyle, name: &str, value: &str) {
    let font_size = style.text.font_size.unwrap_or(14.0);
    match name {
        "display" => {
            style.display = match value.to_ascii_lowercase().trim() {
                "none" => Display::None,
                "block" | "table" | "inline-table" | "table-row" | "table-cell"
                | "table-row-group" | "table-header-group" | "table-footer-group"
                | "table-caption" | "list-item" | "flex" | "grid" => Display::Block,
                "inline-block" => Display::InlineBlock,
                _ => Display::Inline,
            }
        }
        "visibility" => {
            if value.eq_ignore_ascii_case("hidden") || value.eq_ignore_ascii_case("collapse") {
                style.display = Display::None;
            }
        }
        "color" => {
            if let Some(color) = parse_color(value) {
                style.text.color = Some(color);
            }
        }
        "background" | "background-color" => {
            if let Some(color) = parse_background_color(value) {
                style.text.background = Some(color);
            }
        }
        "font-size" => {
            if let Some(length) = parse_length(value, font_size)
                && let Some(size) = length.resolve(font_size)
            {
                style.text.font_size = Some(size.clamp(6.0, 120.0));
            }
        }
        "font-weight" => {
            let lower = value.to_ascii_lowercase();
            style.text.bold = if lower == "normal" {
                false
            } else {
                lower == "bold"
                    || lower == "bolder"
                    || lower
                        .parse::<u32>()
                        .map(|weight| weight >= 600)
                        .unwrap_or(false)
            };
        }
        "font-style" => {
            style.text.italic =
                value.eq_ignore_ascii_case("italic") || value.eq_ignore_ascii_case("oblique");
        }
        "text-decoration" | "text-decoration-line" => {
            let lower = value.to_ascii_lowercase();
            style.text.underline = lower.contains("underline");
            style.text.strike = lower.contains("line-through");
        }
        "text-align" => {
            style.text.text_align = match value.to_ascii_lowercase().trim() {
                "center" => TextAlign::Center,
                "right" | "end" => TextAlign::Right,
                _ => TextAlign::Left,
            }
        }
        "line-height" => {
            let lower = value.to_ascii_lowercase();
            style.text.line_height = if lower == "normal" {
                Some(1.45)
            } else if let Some(length) = parse_length(value, font_size) {
                length.resolve(font_size).map(|px| px / font_size.max(1.0))
            } else {
                lower.parse::<f32>().ok()
            };
        }
        "white-space" => {
            style.text.white_space = match value.to_ascii_lowercase().trim() {
                "pre" => WhiteSpace::Pre,
                "pre-wrap" | "pre-line" => WhiteSpace::PreWrap,
                _ => WhiteSpace::Normal,
            }
        }
        "font-family" => {
            let lower = value.to_ascii_lowercase();
            if ["monospace", "courier", "consolas", "menlo", "monaco"]
                .iter()
                .any(|name| lower.contains(name))
            {
                style.text.font_family = FontFamily::Monospace;
            }
        }
        "letter-spacing" => {
            style.text.letter_spacing = parse_length(value, font_size).and_then(|length| {
                if matches!(length, Length::Px(_)) {
                    length.resolve(font_size)
                } else {
                    None
                }
            });
        }
        "margin" => {
            if let Some(edges) = parse_edges(value, font_size) {
                style.margin = edges;
            }
        }
        "margin-top" => set_edge(&mut style.margin.top, value, font_size),
        "margin-right" => set_edge(&mut style.margin.right, value, font_size),
        "margin-bottom" => set_edge(&mut style.margin.bottom, value, font_size),
        "margin-left" => set_edge(&mut style.margin.left, value, font_size),
        "padding" => {
            if let Some(edges) = parse_edges(value, font_size) {
                style.padding = edges;
            }
        }
        "padding-top" => set_edge(&mut style.padding.top, value, font_size),
        "padding-right" => set_edge(&mut style.padding.right, value, font_size),
        "padding-bottom" => set_edge(&mut style.padding.bottom, value, font_size),
        "padding-left" => set_edge(&mut style.padding.left, value, font_size),
        "width" => {
            if let Some(length) = parse_length(value, font_size) {
                style.width = Some(length);
            }
        }
        "height" => {
            if let Some(length) = parse_length(value, font_size) {
                style.height = Some(length);
            }
        }
        "max-width" => {
            if let Some(length) = parse_length(value, font_size) {
                style.max_width = Some(length);
            }
        }
        "border" | "border-top" | "border-right" | "border-bottom" | "border-left" => {
            style.border = parse_border(value);
        }
        "border-width" => {
            if let Some(width) =
                parse_length(value, font_size).and_then(|length| length.resolve(0.0))
                && let Some(border) = style.border.as_mut()
            {
                border.width = width;
            }
        }
        "border-color" => {
            if let Some(color) = parse_color(value) {
                if let Some(border) = style.border.as_mut() {
                    border.color = color;
                } else {
                    style.border = Some(Border { width: 1.0, color });
                }
            }
        }
        "border-style" => {
            if value.eq_ignore_ascii_case("none") || value.eq_ignore_ascii_case("hidden") {
                style.border = None;
            } else if style.border.is_none() {
                style.border = Some(Border {
                    width: 1.0,
                    color: Color32::from_gray(180),
                });
            }
        }
        "border-collapse" => {
            style.border_collapse = value.eq_ignore_ascii_case("collapse");
        }
        "border-radius" => {
            let first = value.split_whitespace().next().unwrap_or("");
            if let Some(length) = parse_length(first, font_size) {
                style.radius = match length {
                    // 百分比圆角按“贴合盒子短边”处理，绘制时再夹取。
                    Length::Percent(_) => f32::INFINITY,
                    other => other.resolve(font_size).unwrap_or(0.0).max(0.0),
                };
            }
        }
        "vertical-align" => {
            style.vertical_align = match value.to_ascii_lowercase().trim() {
                "middle" => VerticalAlign::Middle,
                "bottom" => VerticalAlign::Bottom,
                _ => VerticalAlign::Top,
            }
        }
        "list-style-type" | "list-style" => {
            let lower = value.to_ascii_lowercase();
            style.list_style = if lower.contains("decimal") {
                ListStyle::Decimal
            } else if lower.contains("circle") {
                ListStyle::Circle
            } else if lower.contains("square") {
                ListStyle::Square
            } else if lower.contains("none") {
                ListStyle::None
            } else {
                ListStyle::Disc
            };
        }
        _ => {}
    }
}

fn set_edge(edge: &mut Length, value: &str, font_size: f32) {
    if let Some(length) = parse_length(value, font_size) {
        *edge = length;
    }
}

fn parse_edges(value: &str, font_size: f32) -> Option<Edges> {
    let values: Vec<Length> = value
        .split_whitespace()
        .filter_map(|part| parse_length(part, font_size))
        .collect();
    match values.as_slice() {
        [all] => Some(Edges {
            top: *all,
            right: *all,
            bottom: *all,
            left: *all,
        }),
        [vertical, horizontal] => Some(Edges {
            top: *vertical,
            right: *horizontal,
            bottom: *vertical,
            left: *horizontal,
        }),
        [top, horizontal, bottom] => Some(Edges {
            top: *top,
            right: *horizontal,
            bottom: *bottom,
            left: *horizontal,
        }),
        [top, right, bottom, left] => Some(Edges {
            top: *top,
            right: *right,
            bottom: *bottom,
            left: *left,
        }),
        _ => None,
    }
}

fn parse_border(value: &str) -> Option<Border> {
    let lower = value.to_ascii_lowercase();
    if lower.contains("none") || lower.contains("hidden") {
        return None;
    }
    let mut width = 1.0;
    let mut color = Color32::from_gray(180);
    for part in value.split_whitespace() {
        if let Some(length) = parse_length(part, 14.0).and_then(|length| length.resolve(0.0)) {
            width = length.max(0.0);
        } else if let Some(parsed) = parse_color(part) {
            color = parsed;
        }
    }
    (width > 0.0).then_some(Border { width, color })
}

// ---------------------------------------------------------------------------
// 值解析
// ---------------------------------------------------------------------------

pub fn parse_length(value: &str, font_size: f32) -> Option<Length> {
    let value = value.trim().to_ascii_lowercase();
    if value == "auto" {
        return Some(Length::Auto);
    }
    let number = value
        .trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == '%')
        .parse::<f32>()
        .ok()?;
    if value.ends_with('%') {
        return Some(Length::Percent(number));
    }
    if value.ends_with("px") || number == 0.0 {
        return Some(Length::Px(number));
    }
    if value.ends_with("pt") {
        return Some(Length::Px(number * 4.0 / 3.0));
    }
    if value.ends_with("pc") {
        return Some(Length::Px(number * 16.0));
    }
    if value.ends_with("em") {
        return Some(Length::Px(number * font_size));
    }
    if value.ends_with("rem") {
        return Some(Length::Px(number * 16.0));
    }
    if value.ends_with("in") {
        return Some(Length::Px(number * 96.0));
    }
    if value.ends_with("cm") {
        return Some(Length::Px(number * 37.8));
    }
    if value.ends_with("mm") {
        return Some(Length::Px(number * 3.78));
    }
    None
}

pub fn parse_attribute_length(value: &str) -> Option<Length> {
    let value = value.trim();
    if let Some(percent) = value.strip_suffix('%') {
        return percent.parse::<f32>().ok().map(Length::Percent);
    }
    value.parse::<f32>().ok().map(Length::Px)
}

fn parse_font_size(value: &str) -> Option<f32> {
    match value.trim() {
        "1" => Some(10.0),
        "2" => Some(12.0),
        "3" => Some(14.0),
        "4" => Some(16.0),
        "5" => Some(19.0),
        "6" => Some(23.0),
        "7" => Some(28.0),
        _ => value.trim().parse::<f32>().ok(),
    }
}

/// 从 `background` 简写中取颜色：`#fff url(...) no-repeat`、`rgb(0, 0, 0) none` 等。
pub fn parse_background_color(value: &str) -> Option<Color32> {
    let value = value.trim();
    if let Some(color) = parse_color(value) {
        return Some(color);
    }
    for token in value.split_whitespace() {
        if let Some(color) = parse_color(token) {
            return Some(color);
        }
    }
    for prefix in ["rgba(", "rgb("] {
        if let Some(start) = value.to_ascii_lowercase().find(prefix)
            && let Some(end) = value[start..].find(')')
            && let Some(color) = parse_color(&value[start..start + end + 1])
        {
            return Some(color);
        }
    }
    None
}

pub fn parse_color(value: &str) -> Option<Color32> {
    let value = value.trim().trim_matches('"').trim_matches('\'');
    if value.is_empty() || value.eq_ignore_ascii_case("transparent") {
        return None;
    }
    if let Some(hex) = value.strip_prefix('#') {
        return cssparser::color::parse_hash_color(hex.as_bytes()).ok().map(
            |(red, green, blue, alpha)| {
                Color32::from_rgba_unmultiplied(red, green, blue, (alpha * 255.0).round() as u8)
            },
        );
    }
    let lower = value.to_ascii_lowercase();
    if let Some(rest) = lower
        .strip_prefix("rgba(")
        .or_else(|| lower.strip_prefix("rgb("))
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let parts: Vec<&str> = rest.split(',').map(str::trim).collect();
        if parts.len() >= 3 {
            let red = parts[0].parse::<u8>().ok()?;
            let green = parts[1].parse::<u8>().ok()?;
            let blue = parts[2].parse::<u8>().ok()?;
            let alpha = parts
                .get(3)
                .and_then(|value| value.parse::<f32>().ok())
                .map(|value| (value.clamp(0.0, 1.0) * 255.0).round() as u8)
                .unwrap_or(255);
            return Some(Color32::from_rgba_unmultiplied(red, green, blue, alpha));
        }
    }
    cssparser::color::parse_named_color(&lower)
        .ok()
        .map(|(red, green, blue)| Color32::from_rgb(red, green, blue))
}

// ---------------------------------------------------------------------------
// 选择器与规则扫描
// ---------------------------------------------------------------------------

fn strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start + 2..].find("*/") else {
            return out;
        };
        rest = &rest[start + 2 + end + 2..];
    }
    out.push_str(rest);
    out
}

fn matching_brace(input: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for (index, ch) in input.char_indices() {
        match ch {
            '"' | '\'' if quote.is_none() => quote = Some(ch),
            c if Some(c) == quote => quote = None,
            '{' if quote.is_none() => depth += 1,
            '}' if quote.is_none() => {
                if depth == 0 {
                    return Some(index);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}

fn split_selector_list(prelude: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for (index, ch) in prelude.char_indices() {
        match ch {
            '"' | '\'' if quote.is_none() => quote = Some(ch),
            c if Some(c) == quote => quote = None,
            '[' | '(' if quote.is_none() => depth += 1,
            ']' | ')' if quote.is_none() => depth -= 1,
            ',' if quote.is_none() && depth == 0 => {
                parts.push(prelude[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    let tail = prelude[start..].trim();
    if !tail.is_empty() {
        parts.push(tail);
    }
    parts
}

/// 从选择器文本取出**桶键**（命中该选择器的必要条件）。
///
/// 只看**最右一段**（最后一个组合符之后的部分）—— 组合符左边约束的是祖先，
/// 不是被匹配的元素本身。对 `div p.foo` 而言，元素必须同时满足 `p` 与 `.foo`，
/// 两者都是必要条件；取其中**更具体**的那个（id > class > 标签）做键，
/// 筛选面更窄、候选更少。
///
/// 返回 `None` 表示**取不出可靠的必要条件**（含伪类、属性选择器、通配等），
/// 这类规则进「通用桶」，每个元素都要试 —— 宁可多试，也不能漏。
///
/// 常见形态：
/// - `.foo` → `Class("foo")`
/// - `#bar` → `Id("bar")`
/// - `p` → `Tag("p")`
/// - `div p.foo` → `Class("foo")`
/// - `div > p` → `Tag("p")`
/// - `a:hover` → `None`（伪类不猜）
fn rule_key(selector: &str) -> Option<RuleKey> {
    let segment = rightmost_segment(selector.trim())?;
    compound_key(segment)
}

/// 取选择器**最右一段**（最后一个顶层组合符之后的部分）。
///
/// 组合符（后代 / 子 / 相邻 / 兄弟）左边的部分约束的是**祖先**，
/// 与被匹配元素自身的身份无关，不能拿来当桶键。
fn rightmost_segment(selector: &str) -> Option<&str> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut start = 0usize;
    for (index, ch) in selector.char_indices() {
        match ch {
            '"' | '\'' if quote.is_none() => quote = Some(ch),
            c if Some(c) == quote => quote = None,
            '[' | '(' if quote.is_none() => depth += 1,
            ']' | ')' if quote.is_none() => depth -= 1,
            // 顶层逗号 = 选择器组（`.a, .b`）。各段是「或」关系，
            // 任何一段的键都不能代表整条规则 —— 整体进通用桶。
            ',' if depth == 0 && quote.is_none() => return None,
            // 顶层组合符：切断，左边是祖先约束。
            ' ' | '\t' | '\n' | '\r' | '>' | '+' | '~' if depth == 0 && quote.is_none() => {
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    let segment = selector[start..].trim();
    if segment.is_empty() {
        None
    } else {
        Some(segment)
    }
}

/// 取一个**复合选择器**（compound selector）里的桶键。
///
/// 复合选择器是若干简单选择器的串联（如 `p.foo#bar`）。每个简单选择器都是
/// 命中该复合选择器的**必要条件**，取最具体的那个（id > class > 标签）。
///
/// 遇到伪类（`:`）、属性选择器（`[`）、通配（`*`）、转义（`\`）等一律返回
/// `None`：这些会让「必要条件」推断变得不可靠，宁可放进通用桶。
fn compound_key(segment: &str) -> Option<RuleKey> {
    let bytes = segment.as_bytes();
    let mut pos = 0usize;
    let mut id: Option<&str> = None;
    let mut class: Option<&str> = None;
    let mut tag: Option<&str> = None;
    let mut first = true;

    while pos < bytes.len() {
        let kind = match bytes[pos] {
            b'#' => b'#',
            b'.' => b'.',
            c if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' => b't',
            // 伪类 / 属性 / 通配 / 转义 / 命名空间 —— 不猜。
            _ => return None,
        };
        if kind == b't' && !first {
            // 标签名只能出现在复合选择器最前面。
            return None;
        }
        // 标签名的标识符**从当前位置开始**；`.`/`#` 之后才开始。
        let name_start = if kind == b't' { pos } else { pos + 1 };
        pos = name_start;
        while pos < bytes.len()
            && (bytes[pos].is_ascii_alphanumeric() || bytes[pos] == b'-' || bytes[pos] == b'_')
        {
            pos += 1;
        }
        let name = &segment[name_start..pos];
        if name.is_empty() {
            return None;
        }
        match kind {
            b'#' => {
                if id.is_some() {
                    return None;
                }
                id = Some(name);
            }
            b'.' => {
                if class.is_some() {
                    return None;
                }
                class = Some(name);
            }
            _ => tag = Some(name),
        }
        first = false;
    }

    if let Some(id) = id {
        Some(RuleKey::Id(id.to_lowercase()))
    } else if let Some(class) = class {
        Some(RuleKey::Class(class.to_lowercase()))
    } else {
        tag.map(|tag| RuleKey::Tag(tag.to_lowercase()))
    }
}

/// 近似 CSS specificity：(id, class/attr/pseudo, type)。
fn selector_specificity(selector: &str) -> (u16, u16, u16) {
    let mut ids = 0u16;
    let mut classes = 0u16;
    let mut types = 0u16;
    let mut token_start = true;
    let mut chars = selector.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '#' => {
                ids += 1;
                skip_ident(&mut chars);
                token_start = false;
            }
            '.' => {
                classes += 1;
                skip_ident(&mut chars);
                token_start = false;
            }
            '[' | ':' => {
                classes += 1;
                token_start = false;
            }
            '*' | '>' | '+' | '~' | ' ' | '\t' | '\n' => token_start = true,
            c if c.is_ascii_alphabetic() && token_start => {
                types += 1;
                skip_ident(&mut chars);
                token_start = false;
            }
            _ => token_start = false,
        }
    }
    (ids, classes, types)
}

fn skip_ident(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(next) = chars.peek() {
        if next.is_ascii_alphanumeric() || *next == '-' || *next == '_' {
            chars.next();
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lengths_and_colors() {
        assert_eq!(parse_length("12px", 14.0), Some(Length::Px(12.0)));
        assert_eq!(parse_length("1.5em", 14.0), Some(Length::Px(21.0)));
        assert_eq!(parse_length("50%", 14.0), Some(Length::Percent(50.0)));
        assert_eq!(parse_color("#ff0000"), Some(Color32::from_rgb(255, 0, 0)));
        assert_eq!(
            parse_color("rgb(0, 128, 255)"),
            Some(Color32::from_rgb(0, 128, 255))
        );
    }

    #[test]
    fn rejects_unsafe_declarations() {
        let declarations = parse_declarations(
            "color: red; background: url(http://evil/x.png); width: expression(1);",
        );
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].name, "color");
    }

    #[test]
    fn specificity_prefers_id_over_class() {
        let class = selector_specificity(".a");
        let id = selector_specificity("#a");
        assert!(id > class);
        assert!(selector_specificity("div p") > selector_specificity("p"));
    }

    // -- 桶键（rule_key）与分桶匹配 --------------------------------------

    /// 参考实现：**不做任何分桶**，逐条规则试匹配。
    ///
    /// 分桶版必须与它给出完全相同的计算样式 —— 这是分桶正确性的判据。
    fn apply_reference(sheet: &Stylesheet, style: &mut ComputedStyle, element: &ElementRef<'_>) {
        let mut matched: Vec<&Rule> = sheet
            .rules
            .iter()
            .filter(|rule| rule.selector.matches(element))
            .collect();
        matched.sort_by_key(|rule| (rule.specificity, rule.order));
        for rule in matched {
            for declaration in &rule.declarations {
                apply_declaration(style, &declaration.name, &declaration.value);
            }
        }
    }

    /// 桶键必须是「命中该选择器的必要条件」：凡匹配的元素，都必须带这个键。
    ///
    /// 这条不变量保证分桶**只会少试规则、不会漏规则**。
    #[test]
    fn rule_key_is_a_necessary_condition() {
        let cases = [
            (".foo", Some(RuleKey::Class("foo".into()))),
            ("#bar", Some(RuleKey::Id("bar".into()))),
            ("p", Some(RuleKey::Tag("p".into()))),
            ("div p.foo", Some(RuleKey::Class("foo".into()))),
            ("div > p", Some(RuleKey::Tag("p".into()))),
            ("DIV P", Some(RuleKey::Tag("p".into()))),
            // 有伪类/属性/通配 —— 取不出可靠必要条件，进通用桶。
            ("a:hover", None),
            ("[data-x]", None),
            ("*", None),
            (".a, .b", None),
        ];
        for (selector, expected) in cases {
            assert_eq!(
                rule_key(selector),
                expected,
                "选择器 {selector:?} 的桶键不对"
            );
        }
    }

    /// 收集文档里所有元素（递归遍历）。
    fn collect_elements(document: &scraper::Html) -> Vec<ElementRef<'_>> {
        fn walk<'a>(element: ElementRef<'a>, out: &mut Vec<ElementRef<'a>>) {
            out.push(element);
            for child in element.children() {
                if let Some(child_element) = ElementRef::wrap(child) {
                    walk(child_element, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(document.root_element(), &mut out);
        out
    }

    /// 分桶匹配与逐条扫描必须给出**完全相同**的计算样式。
    ///
    /// 覆盖各种选择器形态（标签 / 类 / id / 后代 / 子代 / 伪类 / 属性 / 逗号组），
    /// 以及大小写不同的类名与 id —— 后者是关键：桶键统一转小写，
    /// 若分桶改成精确匹配就会漏规则。
    #[test]
    fn bucketed_apply_matches_reference_scan() {
        let css = "\
            p { color: #111111; }\
            .foo { color: #222222; }\
            #bar { color: #333333; }\
            div p.foo { color: #444444; }\
            div > span { color: #555555; }\
            a:hover { color: #666666; }\
            [data-tag] { color: #777777; }\
            .Foo { color: #888888; }\
            #Bar { color: #999999; }\
            table td { font-size: 13px; }\
            * { margin: 0px; }\
        ";
        let sheet = Stylesheet::parse(css);
        let html = "\
            <div id='wrap'>\
              <p class='foo'>one</p>\
              <p class='Foo'>two</p>\
              <p>three</p>\
              <span class='foo'>four</span>\
              <a href='#' data-tag='x'>five</a>\
              <table><tr><td>six</td></tr></table>\
              <div id='bar'>seven</div>\
            </div>";
        let document = scraper::Html::parse_document(html);
        let mut compared = 0;
        for element in collect_elements(&document) {
            let mut reference = ComputedStyle::default();
            apply_reference(&sheet, &mut reference, &element);
            let mut bucketed = ComputedStyle::default();
            sheet.apply(&mut bucketed, &element);
            assert_eq!(
                reference,
                bucketed,
                "分桶结果与逐条扫描不一致：{:?}",
                element.value().name()
            );
            compared += 1;
        }
        assert!(
            compared >= 10,
            "比对的元素过少（{compared}），样例可能没覆盖到"
        );
    }

    /// 大规则表下分桶仍须与参考实现一致（桶多、命中分散）。
    #[test]
    fn bucketed_apply_matches_reference_on_many_rules() {
        let mut css = String::new();
        for index in 0..200 {
            css.push_str(&format!(".c{index} {{ color: #0000{index:02x}; }}\n"));
            css.push_str(&format!(
                "p.c{index} {{ font-size: {}px; }}\n",
                index % 20 + 10
            ));
            css.push_str(&format!("div span.c{index} {{ margin-top: {index}px; }}\n"));
        }
        let sheet = Stylesheet::parse(&css);
        let mut html = String::from("<div><span>");
        for index in 0..300 {
            html.push_str(&format!("<p class='c{index}'>x</p>"));
        }
        html.push_str("</span></div>");
        let document = scraper::Html::parse_document(&html);
        for element in collect_elements(&document) {
            let mut reference = ComputedStyle::default();
            apply_reference(&sheet, &mut reference, &element);
            let mut bucketed = ComputedStyle::default();
            sheet.apply(&mut bucketed, &element);
            assert_eq!(
                reference,
                bucketed,
                "规则较多时分桶结果与逐条扫描不一致：{:?}",
                element.value().name()
            );
        }
    }
}
