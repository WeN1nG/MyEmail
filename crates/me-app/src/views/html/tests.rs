//! HTML 渲染器语料回归：清洗、CSS 子集、表格跨度、上限与回退。

use std::time::{Duration, Instant};

use eframe::egui::Color32;

use super::css::{ListStyle, TextAlign, WhiteSpace};
use super::layout::test_support::run_probe;
use super::parse::render_html;
use super::{Block, HtmlRenderError, Inline, Table};

fn text_of(blocks: &[Block]) -> String {
    let mut out = String::new();
    for block in blocks {
        match block {
            Block::Paragraph { inlines, .. } | Block::Heading { inlines, .. } => {
                append_inlines(inlines, &mut out);
                out.push('\n');
            }
            Block::List { items, .. } => {
                for item in items {
                    out.push_str(&item.marker);
                    out.push_str(&text_of(&item.blocks));
                }
            }
            Block::Quote { blocks, .. } => out.push_str(&text_of(blocks)),
            Block::Section { blocks, .. } => out.push_str(&text_of(blocks)),
            Block::Pre { text, .. } => {
                out.push_str(text);
                out.push('\n');
            }
            Block::Table(table) => {
                for row in &table.rows {
                    for cell in &row.cells {
                        out.push_str(&text_of(&cell.blocks));
                    }
                }
            }
            Block::Image(image) => out.push_str(&image.alt),
            Block::Rule { .. } => out.push('\n'),
        }
    }
    out
}

fn append_inlines(inlines: &[Inline], out: &mut String) {
    for inline in inlines {
        match inline {
            Inline::Text { text, .. } => out.push_str(text),
            Inline::Image(image) => out.push_str(&image.alt),
            Inline::Break => out.push('\n'),
        }
    }
}

fn first_text_style(blocks: &[Block]) -> Option<&super::TextStyle> {
    for block in blocks {
        match block {
            Block::Paragraph { inlines, .. } | Block::Heading { inlines, .. } => {
                for inline in inlines {
                    if let Inline::Text { style, .. } = inline {
                        return Some(style);
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

fn first_link(blocks: &[Block]) -> Option<String> {
    for block in blocks {
        if let Block::Paragraph { inlines, .. } = block {
            for inline in inlines {
                if let Inline::Text {
                    link: Some(link), ..
                } = inline
                {
                    return Some(link.clone());
                }
            }
        }
    }
    None
}

fn count_blocks(blocks: &[Block]) -> usize {
    let mut count = blocks.len();
    for block in blocks {
        match block {
            Block::Section { blocks, .. } | Block::Quote { blocks, .. } => {
                count += count_blocks(blocks);
            }
            Block::List { items, .. } => {
                for item in items {
                    count += count_blocks(&item.blocks);
                }
            }
            Block::Table(table) => {
                for row in &table.rows {
                    for cell in &row.cells {
                        count += count_blocks(&cell.blocks);
                    }
                }
            }
            _ => {}
        }
    }
    count
}

fn first_table(blocks: &[Block]) -> Option<&Table> {
    for block in blocks {
        match block {
            Block::Table(table) => return Some(table),
            Block::Section { blocks, .. } | Block::Quote { blocks, .. } => {
                if let Some(table) = first_table(blocks) {
                    return Some(table);
                }
            }
            Block::List { items, .. } => {
                for item in items {
                    if let Some(table) = first_table(&item.blocks) {
                        return Some(table);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

#[test]
fn removes_scripts_event_handlers_and_unsafe_links() {
    let html = r#"
        <script>alert(1)</script>
        <p onclick="steal()">Hello <b>world</b></p>
        <a href="javascript:alert(1)">bad</a>
        <a href="https://example.com/x">good</a>
        <iframe src="https://evil"></iframe>
    "#;
    let rendered = render_html(html).expect("render");
    let text = text_of(&rendered.blocks);
    assert!(text.contains("Hello world"));
    assert!(!text.contains("alert"));
    let link = first_link(&rendered.blocks);
    assert_eq!(link.as_deref(), Some("https://example.com/x"));
}

#[test]
fn applies_inline_style_over_stylesheet() {
    let html = r#"
        <style>
            .a { color: #ff0000; }
            .b { color: #00ff00; }
            #x { color: #0000ff; }
        </style>
        <p class="a b" id="x" style="font-weight: bold">Hi</p>
    "#;
    let rendered = render_html(html).expect("render");
    let style = first_text_style(&rendered.blocks).expect("style");
    assert_eq!(style.color, Some(Color32::from_rgb(0, 0, 255)));
    assert!(style.bold);
}

#[test]
fn media_screen_is_applied_and_print_ignored() {
    let html = r#"
        <style>
            @import url("https://evil/x.css");
            @media screen { .a { color: #ff0000; } }
            @media print { .a { color: #00ff00; } }
            @supports (display: grid) { .a { color: #123456; } }
        </style>
        <p class="a">X</p>
    "#;
    let rendered = render_html(html).expect("render");
    let style = first_text_style(&rendered.blocks).expect("style");
    assert_eq!(style.color, Some(Color32::from_rgb(255, 0, 0)));
}

#[test]
fn table_preserves_rowspan_and_colspan() {
    let html = r#"
        <table border="1" cellspacing="0">
            <tr><td colspan="2" rowspan="2">A</td><td>B</td></tr>
            <tr><td>C</td></tr>
        </table>
    "#;
    let rendered = render_html(html).expect("render");
    let table = first_table(&rendered.blocks).expect("table");
    assert_eq!(table.columns, 3);
    let first = &table.rows[0].cells[0];
    assert_eq!(first.colspan, 2);
    assert_eq!(first.rowspan, 2);
    assert_eq!(table.cell_spacing, 0.0);
    assert!(table.style.border.is_some());
}

#[test]
fn pre_preserves_whitespace() {
    let html = "<pre>  a\n    b</pre>";
    let rendered = render_html(html).expect("render");
    let pre = rendered
        .blocks
        .iter()
        .find_map(|block| match block {
            Block::Pre { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .expect("pre");
    assert!(pre.contains("  a\n    b"));
}

#[test]
fn lists_and_alignment_are_parsed() {
    let html =
        r#"<ol style="list-style-type: decimal"><li>one</li><li>two</li></ol><center>mid</center>"#;
    let rendered = render_html(html).expect("render");
    let list = rendered
        .blocks
        .iter()
        .find_map(|block| match block {
            Block::List { ordered, style, .. } => Some((*ordered, style)),
            _ => None,
        })
        .expect("list");
    assert!(list.0);
    let text = text_of(&rendered.blocks);
    assert!(text.contains("1. "));
    assert!(text.contains("mid"));
    let centered = rendered
        .blocks
        .iter()
        .find_map(|block| match block {
            Block::Paragraph { style, .. } => Some(style.align),
            _ => None,
        })
        .unwrap_or(TextAlign::Left);
    assert_eq!(centered, TextAlign::Center);
}

#[test]
fn image_sources_are_whitelisted() {
    let html = r#"
        <img src="cid:logo">
        <img src="data:image/png;base64,AAAA">
        <img src="https://example.com/a.png">
        <img src="file:///etc/passwd">
        <img src="javascript:alert(1)">
    "#;
    let rendered = render_html(html).expect("render");
    let mut images = Vec::new();
    fn collect(blocks: &[Block], out: &mut Vec<String>) {
        for block in blocks {
            match block {
                Block::Image(image) => out.push(image.src.clone()),
                Block::Paragraph { inlines, .. } => {
                    for inline in inlines {
                        if let Inline::Image(image) = inline {
                            out.push(image.src.clone());
                        }
                    }
                }
                Block::Section { blocks, .. } | Block::Quote { blocks, .. } => collect(blocks, out),
                Block::List { items, .. } => {
                    for item in items {
                        collect(&item.blocks, out);
                    }
                }
                Block::Table(table) => {
                    for row in &table.rows {
                        for cell in &row.cells {
                            collect(&cell.blocks, out);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    collect(&rendered.blocks, &mut images);
    assert_eq!(images.len(), 3);
    assert!(images.iter().any(|src| src == "cid:logo"));
    assert!(images.iter().any(|src| src.starts_with("data:image/png")));
    assert!(images.iter().any(|src| src.starts_with("https://")));
}

#[test]
fn deep_nesting_and_huge_text_are_rejected() {
    let mut deep = String::new();
    for _ in 0..300 {
        deep.push_str("<div>");
    }
    deep.push('x');
    for _ in 0..300 {
        deep.push_str("</div>");
    }
    assert!(matches!(render_html(&deep), Err(HtmlRenderError::TooLarge)));

    let huge = format!("<p>{}</p>", "x".repeat(2 * 1024 * 1024 + 10));
    assert!(matches!(render_html(&huge), Err(HtmlRenderError::TooLarge)));
}

#[test]
fn background_and_white_space_are_parsed() {
    let html = r#"<div style="background-color: #ffeeaa; white-space: pre-wrap">  a  b</div>"#;
    let rendered = render_html(html).expect("render");
    let style = first_text_style(&rendered.blocks).expect("style");
    assert_eq!(style.background, Some(Color32::from_rgb(255, 238, 170)));
    assert_eq!(style.white_space, WhiteSpace::PreWrap);
}

#[test]
fn list_style_type_none_removes_marker() {
    let html = r#"<ul style="list-style-type: none"><li>x</li></ul>"#;
    let rendered = render_html(html).expect("render");
    let list = rendered.blocks.iter().find_map(|block| match block {
        Block::List { items, style, .. } => Some((items, style)),
        _ => None,
    });
    let (items, _) = list.expect("list");
    assert!(items[0].marker.is_empty());
    let _ = ListStyle::None;
}

#[test]
fn inline_block_column_keeps_block_structure() {
    // 与 Ollama / MJML 邮件同构：主列是 display:inline-block，内部全是表格行。
    let html = r#"
      <div style="display:inline-block;width:100%">
        <table width="100%" cellpadding="0" cellspacing="0">
          <tr><td><div style="font-size:14px;line-height:25px"><span style="white-space:pre-wrap">First paragraph.</span></div></td></tr>
          <tr><td><pre style="background-color:#f4f4f5;padding:12px"><code>ollama run phonon</code></pre></td></tr>
          <tr><td><ul><li>item one</li><li>item two</li></ul></td></tr>
        </table>
      </div>
    "#;
    let rendered = render_html(html).expect("render");
    let table = first_table(&rendered.blocks).expect("outer table");
    assert_eq!(table.rows.len(), 3);

    assert!(matches!(
        table.rows[0].cells[0].blocks.as_slice(),
        [Block::Paragraph { .. }]
    ));
    assert!(matches!(
        table.rows[1].cells[0].blocks.as_slice(),
        [Block::Pre { .. }]
    ));
    assert!(matches!(
        table.rows[2].cells[0].blocks.as_slice(),
        [Block::List { .. }]
    ));

    // 排版后代码与列表文字必须是独立文本项，而不是混进第一段。
    let probe = run_probe(&rendered, 700.0);
    let paragraph = probe
        .texts
        .iter()
        .find(|text| text.text.contains("First paragraph"))
        .expect("paragraph text");
    assert!(!paragraph.text.contains("ollama run"));
    assert!(
        probe
            .texts
            .iter()
            .any(|text| text.text.contains("ollama run"))
    );
    assert!(
        probe
            .texts
            .iter()
            .any(|text| text.text.contains("item one"))
    );
}

#[test]
fn line_height_is_applied() {
    let html = r#"<p style="margin:0;font-size:14px;line-height:25px">alpha<br>beta</p>"#;
    let rendered = render_html(html).expect("render");
    let probe = run_probe(&rendered, 600.0);
    let text = probe
        .texts
        .iter()
        .find(|text| text.text.contains("alpha"))
        .expect("text");
    assert_eq!(text.rows.len(), 2, "两行文本应产生两行 galley");
    assert!(
        (text.rect.height() - 50.0).abs() < 2.0,
        "行高应为 2×25px，实际 {:.1}",
        text.rect.height()
    );
    assert!(probe.height >= text.rect.height());
}

#[test]
fn text_align_centers_each_line() {
    let html = r#"<p style="margin:0;text-align:center">centered line</p>"#;
    let rendered = render_html(html).expect("render");
    let probe = run_probe(&rendered, 600.0);
    let text = probe
        .texts
        .iter()
        .find(|text| text.text.contains("centered"))
        .expect("text");
    assert_eq!(text.halign, eframe::egui::Align::Center);
    let (left, right) = text.rows[0];
    let center = (left + right) / 2.0;
    assert!(
        (center - 300.0).abs() < 2.0,
        "文字应相对 600px 内容列居中，实际中心 {center:.1}"
    );
}

#[test]
fn max_width_container_constrains_nested_tables() {
    let html = r#"
      <div style="max-width:600px;margin:0 auto;background-color:#eeeeee">
        <table width="100%"><tr><td style="background-color:#dddddd">nested table text</td></tr></table>
      </div>
    "#;
    let rendered = render_html(html).expect("render");
    let probe = run_probe(&rendered, 1200.0);
    assert!(!probe.fills.is_empty());
    for rect in &probe.fills {
        assert!(
            rect.width() <= 600.5,
            "嵌套内容不应超过 600px 容器，实际宽度 {:.1}",
            rect.width()
        );
    }
}

#[test]
fn table_cellpadding_is_applied() {
    let html = r#"<table cellpadding="10" border="0"><tr><td>cell text</td></tr></table>"#;
    let rendered = render_html(html).expect("render");
    let probe = run_probe(&rendered, 600.0);
    let text = probe
        .texts
        .iter()
        .find(|text| text.text.contains("cell text"))
        .expect("cell text");
    assert!(
        text.rect.left() >= 21.5,
        "cellpadding 10px 应让文字从内容列左缘 + 10px 开始，实际 {:.1}",
        text.rect.left()
    );
}

#[test]
fn table_rowspan_spans_rows() {
    let html = r#"
      <table cellpadding="0" cellspacing="0" width="400">
        <tr>
          <td rowspan="2" style="background-color:#eeeeee">A</td>
          <td style="background-color:#dddddd">B</td>
        </tr>
        <tr><td style="background-color:#dddddd">C</td></tr>
      </table>
    "#;
    let rendered = render_html(html).expect("render");
    let probe = run_probe(&rendered, 800.0);

    let mut fills = probe.fills.clone();
    fills.sort_by(|a, b| {
        a.left()
            .partial_cmp(&b.left())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    assert_eq!(fills.len(), 3, "三个单元格背景");
    let left = fills[0];
    let right: Vec<_> = fills
        .iter()
        .skip(1)
        .filter(|rect| rect.left() >= left.right() - 0.5)
        .copied()
        .collect();
    assert_eq!(right.len(), 2);
    assert!(
        (left.height() - (right[0].height() + right[1].height())).abs() < 1.5,
        "跨行单元格高度应覆盖两行：{:.1} vs {:.1}",
        left.height(),
        right[0].height() + right[1].height()
    );
}

#[test]
fn missing_remote_image_uses_compact_placeholder() {
    let html = r#"<p style="margin:0"><img src="https://example.com/banner.png" alt="banner"></p>"#;
    let rendered = render_html(html).expect("render");
    let probe = run_probe(&rendered, 500.0);
    let chip = probe
        .texts
        .iter()
        .find(|text| text.text.contains("正在加载图片"))
        .expect("pending chip");
    assert!(chip.rect.width() <= 500.0);
    assert!(chip.rect.height() <= 40.0);
}

#[test]
fn nbsp_is_preserved() {
    let rendered = render_html("<p>a&nbsp;&nbsp;b</p>").expect("render");
    let text = text_of(&rendered.blocks);
    assert!(text.contains("a\u{a0}\u{a0}b"));
}

#[test]
fn table_align_center_does_not_center_cell_text() {
    let html = r#"<table align="center" width="300"><tr><td>text</td></tr></table>"#;
    let rendered = render_html(html).expect("render");
    let table = first_table(&rendered.blocks).expect("table");
    assert_eq!(table.rows[0].cells[0].align, TextAlign::Left);
}

#[test]
#[ignore = "性能基准：cargo test -p me-app large_html_parse --release -- --ignored --nocapture"]
fn large_html_parse_performance() {
    let paragraph = r#"<p style="color:#333;margin:4px 0;font-size:14px">这是一段用于性能基准的邮件正文，包含 <b>粗体</b>、<a href="https://example.com/x">链接</a> 与中文标点。</p>"#;
    let html = format!(
        r#"<html><body><div style="width:100%">{}</div></body></html>"#,
        paragraph.repeat(5_000)
    );
    assert!(html.len() > 900 * 1024, "payload should be ~1 MB");

    let started = Instant::now();
    let rendered = render_html(&html).expect("render large html");
    let elapsed = started.elapsed();
    println!(
        "HTML {} bytes -> {} blocks in {:?}",
        html.len(),
        count_blocks(&rendered.blocks),
        elapsed
    );
    assert!(count_blocks(&rendered.blocks) > 1_000);
    assert!(
        elapsed < Duration::from_secs(5),
        "large HTML parse too slow: {elapsed:?}"
    );
}

#[test]
#[ignore = "性能基准：cargo test -p me-app large_table_parse --release -- --ignored --nocapture"]
fn large_table_parse_performance() {
    let mut html = String::from("<html><body><table>");
    for row in 0..1_000 {
        html.push_str("<tr>");
        for col in 0..5 {
            html.push_str(&format!(
                "<td style=\"background-color:#f5f5f5\">r{row}c{col}</td>"
            ));
        }
        html.push_str("</tr>");
    }
    html.push_str("</table></body></html>");

    let started = Instant::now();
    let rendered = render_html(&html).expect("render large table");
    let elapsed = started.elapsed();
    let table = first_table(&rendered.blocks).expect("table");
    println!(
        "table {} bytes -> {}x{} in {:?}",
        html.len(),
        table.rows.len(),
        table.columns,
        elapsed
    );
    assert_eq!(table.rows.len(), 1_000);
    assert_eq!(table.columns, 5);
    assert!(
        elapsed < Duration::from_secs(5),
        "large table parse too slow: {elapsed:?}"
    );
}
