//! HTML 渲染器 v2（WP-12/13）。
//!
//! 解析在后台 `spawn_blocking` 完成，UI 只接收 `Send` 的扁平块模型；
//! DOM 不跨线程，旧 generation 的结果直接丢弃。
//!
//! 绘制由 [`layout`] 的排版引擎完成：先测量并算出每个块/行/单元格的矩形，
//! 再一次性绘制，避免嵌套 `Ui` 在复杂邮件结构下互相挤压。

pub mod css;
pub mod layout;
pub mod parse;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use eframe::egui::{self, Color32};
use me_core::MessageId;
use tokio::sync::Semaphore;

use crate::views::richtext::ImageTextures;

pub use css::{Border, Edges, Length, TextAlign, TextStyle, VerticalAlign};
pub use parse::render_html;

const MAX_CONCURRENT_PARSES: usize = 2;

// ---------------------------------------------------------------------------
// 扁平结果模型
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct RenderedHtml {
    pub blocks: Vec<Block>,
    /// 解析触发上限时置位；UI 可提示已截断。
    pub truncated: bool,
    /// `<body>` / 页面的背景色；`None` 表示沿用应用主题底色。
    pub page_background: Option<Color32>,
}

impl RenderedHtml {
    pub fn empty() -> Self {
        Self {
            blocks: Vec::new(),
            truncated: false,
            page_background: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BlockStyle {
    pub margin: Edges,
    pub padding: Edges,
    pub background: Option<Color32>,
    pub border: Option<Border>,
    pub width: Option<Length>,
    pub max_width: Option<Length>,
    pub align: TextAlign,
    /// 圆角半径（px）；`f32::INFINITY` 表示按短边贴合（`border-radius: 50%`）。
    pub radius: f32,
    /// `display:inline-block` 且未显式设置宽度：按内容宽度收缩。
    pub fit_content: bool,
}

impl Default for BlockStyle {
    fn default() -> Self {
        Self {
            margin: Edges::default(),
            padding: Edges::default(),
            background: None,
            border: None,
            width: None,
            max_width: None,
            align: TextAlign::Left,
            radius: 0.0,
            fit_content: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    Text {
        text: String,
        style: TextStyle,
        link: Option<String>,
    },
    Image(ImageRef),
    Break,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImageRef {
    pub src: String,
    pub alt: String,
    pub width: Option<Length>,
    pub height: Option<Length>,
    pub align: TextAlign,
    /// `<a href><img></a>` 的链接。
    pub link: Option<String>,
    /// 图片圆角（px）；`f32::INFINITY` 表示按短边贴合。
    pub radius: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Paragraph {
        inlines: Vec<Inline>,
        style: BlockStyle,
    },
    /// 带盒样式（背景/边框/宽度/内边距等）的块级容器，如 MJML 的 `div.wfix`。
    Section {
        blocks: Vec<Block>,
        style: BlockStyle,
    },
    Heading {
        level: u8,
        inlines: Vec<Inline>,
        style: BlockStyle,
    },
    List {
        ordered: bool,
        items: Vec<ListItem>,
        style: BlockStyle,
    },
    Quote {
        blocks: Vec<Block>,
        style: BlockStyle,
    },
    Pre {
        text: String,
        style: BlockStyle,
        text_style: TextStyle,
    },
    Rule {
        style: BlockStyle,
    },
    Table(Table),
    Image(ImageRef),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListItem {
    pub marker: String,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Table {
    pub rows: Vec<TableRow>,
    pub columns: usize,
    pub border_collapse: bool,
    pub cell_spacing: f32,
    pub style: BlockStyle,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TableRow {
    pub cells: Vec<TableCell>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableCell {
    pub blocks: Vec<Block>,
    pub colspan: usize,
    pub rowspan: usize,
    pub background: Option<Color32>,
    pub border: Option<Border>,
    pub align: TextAlign,
    pub valign: VerticalAlign,
    pub width: Option<Length>,
    pub padding: Edges,
    pub radius: f32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HtmlRenderError {
    TooLarge,
    ParseFailed,
}

impl std::fmt::Display for HtmlRenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HtmlRenderError::TooLarge => f.write_str("邮件 HTML 超出渲染上限"),
            HtmlRenderError::ParseFailed => f.write_str("邮件 HTML 解析失败"),
        }
    }
}

impl std::error::Error for HtmlRenderError {}

#[derive(Debug, Clone)]
pub struct HtmlRenderJob {
    pub message: MessageId,
    pub generation: u64,
    pub html: String,
}

#[derive(Debug, Clone)]
pub struct HtmlRenderResult {
    pub message: MessageId,
    pub generation: u64,
    pub result: Result<RenderedHtml, HtmlRenderError>,
}

/// 后台 HTML 解析器：并发 2，结果经无界通道回 UI，旧 generation 由 UI 丢弃。
pub struct HtmlRenderer {
    tx: Sender<HtmlRenderResult>,
    rx: Receiver<HtmlRenderResult>,
    runtime: tokio::runtime::Handle,
    permits: Arc<Semaphore>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl HtmlRenderer {
    pub fn new(runtime: tokio::runtime::Handle, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            tx,
            rx,
            runtime,
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_PARSES)),
            wake,
        }
    }

    pub fn submit(&self, job: HtmlRenderJob) {
        let tx = self.tx.clone();
        let permits = self.permits.clone();
        let wake = self.wake.clone();
        self.runtime.spawn(async move {
            let Ok(_permit) = permits.acquire_owned().await else {
                return;
            };
            let parsed = tokio::task::spawn_blocking(move || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| render_html(&job.html)))
                    .unwrap_or(Err(HtmlRenderError::ParseFailed))
            })
            .await
            .unwrap_or(Err(HtmlRenderError::ParseFailed));
            let _ = tx.send(HtmlRenderResult {
                message: job.message,
                generation: job.generation,
                result: parsed,
            });
            wake();
        });
    }

    pub fn try_recv(&self) -> Option<HtmlRenderResult> {
        self.rx.try_recv().ok()
    }
}

impl std::fmt::Debug for HtmlRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HtmlRenderer").finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// 绘制入口
// ---------------------------------------------------------------------------

pub fn show(ui: &mut egui::Ui, rendered: &RenderedHtml, images: Option<&ImageTextures<'_>>) {
    layout::show(ui, rendered, images);
}

pub fn collect_image_sources(rendered: &RenderedHtml) -> Vec<String> {
    let mut sources = Vec::new();
    collect_block_images(&rendered.blocks, &mut sources);
    sources.sort();
    sources.dedup();
    sources
}

fn collect_block_images(blocks: &[Block], sources: &mut Vec<String>) {
    for block in blocks {
        match block {
            Block::Paragraph { inlines, .. } | Block::Heading { inlines, .. } => {
                for inline in inlines {
                    if let Inline::Image(image) = inline {
                        sources.push(image.src.clone());
                    }
                }
            }
            Block::List { items, .. } => {
                for item in items {
                    collect_block_images(&item.blocks, sources);
                }
            }
            Block::Quote { blocks, .. } => collect_block_images(blocks, sources),
            Block::Section { blocks, .. } => collect_block_images(blocks, sources),
            Block::Table(table) => {
                for row in &table.rows {
                    for cell in &row.cells {
                        collect_block_images(&cell.blocks, sources);
                    }
                }
            }
            Block::Image(image) => sources.push(image.src.clone()),
            _ => {}
        }
    }
}

/// 解析超时后 UI 的回退判断；不取消后台任务，只按 generation 丢弃迟到结果。
pub const UI_FALLBACK_AFTER: Duration = Duration::from_millis(1_000);
