//! 知识库页：列表、详情、导入（两级确认）、模型下载引导与检索。
//!
//! 这一页的**所有写操作都发 `Command`**，读走 `ReadOnlyRepo`——与项目既有分层一致：
//! 界面不碰 `rusqlite`，也不碰向量库。检索也走命令（要 KNN，只能在写连接上跑）。
//!
//! 三条刻意的设计（别当成疏漏改掉）：
//!
//! 1. **pdf/pptx 在「选完文件」这一步就拦住**：没有能吃文档的模型时，
//!    对话框里直接说「请更换多模态模型」，不让用户等导入跑到一半才失败。
//! 2. **每次导入都弹预估框，且不提供「不再提示」**：它是唯一的成本闸门（决策 O-18）。
//! 3. **列表默认只显示本地文件**：邮件几万条会把列表淹掉（要走来源筛选切过去）。

use eframe::egui::{self, Color32, RichText};

use me_core::knowledge::{
    KbFilter, KbKind, KbOrigin, KbSearchHit, KbStatus, KnowledgeChunk, KnowledgeDoc,
    KnowledgeDocId, KnowledgeSettings, KnowledgeStats, RetrievalMode,
};
use me_core::{Command, truncate_chars};

use crate::state::{ToastKind, UiState};

/// 导入前的探测结果。界面靠它算「几批 / 多少 token / 大概多少钱」。
#[derive(Debug, Clone)]
pub struct ProbeView {
    pub path: String,
    pub kind: KbKind,
    pub bytes: u64,
    pub pages: u32,
    pub batches: u32,
    pub tokens_low: u32,
    pub tokens_high: u32,
    pub cost_usd: Option<(f32, f32)>,
    pub blocked: Option<String>,
}

#[derive(Debug, Default)]
pub struct KnowledgeState {
    pub docs: Vec<KnowledgeDoc>,
    pub dirty: bool,
    /// 来源筛选；`None` = 全部。**默认 `file`**：邮件会把列表淹掉。
    pub origin: Option<KbOrigin>,
    pub status: Option<KbStatus>,
    pub filter_text: String,
    pub search: String,
    pub hits: Vec<KbSearchHit>,
    pub search_mode: RetrievalMode,
    pub search_error: Option<String>,
    pub searching: bool,
    /// 正在看详情的文档。
    pub selected: Option<KnowledgeDocId>,
    pub doc: Option<KnowledgeDoc>,
    pub chunks: Vec<KnowledgeChunk>,
    pub chunk_page: u32,
    pub entities: Vec<String>,
    pub stats: KnowledgeStats,
    pub settings: KnowledgeSettings,
    /// 模型下载进度（`Some` = 正在下载）。
    pub download: Option<(u64, Option<u64>)>,
    /// 正在导入的文件名（只用于界面回显）。
    pub importing: Vec<String>,
    /// 待确认：一次性总确认（费用 + 隐私）。
    pub ack_pending: Option<ProbeView>,
    /// 待确认：本次导入的预估（**不可永久关闭**）。
    pub estimate_pending: Option<ProbeView>,
    /// AI 发起的删除请求（确认卡）。
    pub pending_delete: Option<(KnowledgeDocId, String, u32)>,
    /// 正在看的那一份文档的摄入进度（阶段, 已完成, 总数）。
    pub progress: Option<(me_core::knowledge::KbStage, u32, u32)>,
}

impl KnowledgeState {
    /// 拼出当前筛选条件。
    pub fn filter(&self) -> KbFilter {
        KbFilter {
            origin: self.origin,
            status: self.status,
            query: if self.filter_text.trim().is_empty() {
                None
            } else {
                Some(self.filter_text.trim().to_string())
            },
        }
    }

    pub fn reset_detail(&mut self) {
        self.selected = None;
        self.doc = None;
        self.chunks.clear();
        self.chunk_page = 0;
        self.entities.clear();
    }
}

pub fn show(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.heading("知识库");
        ui.add_space(8.0);
        let stats = &state.knowledge.stats;
        let mut line = format!("语块 {} · 本地文档 {}", stats.chunks, stats.file_docs);
        if stats.mail_capacity > 0 {
            line.push_str(&format!(
                " · 邮件已索引 {}/{} 封",
                stats.mail_ready, stats.mail_capacity
            ));
        } else {
            line.push_str(" · 邮件自动入库已关闭");
        }
        ui.label(RichText::new(line).small().color(Color32::GRAY));

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("导入文件…").clicked() {
                pick_files(state, commands);
            }
            if ui.button("刷新").clicked() {
                state.knowledge.dirty = true;
            }
        });
    });

    show_embed_banner(state, commands, ui);

    if state.knowledge.settings.mail_capacity > 0 && !state.knowledge.stats.per_account.is_empty() {
        let parts: Vec<String> = state
            .knowledge
            .stats
            .per_account
            .iter()
            .map(|(email, count)| format!("{email} {count} 封"))
            .collect();
        ui.label(
            RichText::new(format!("其中 {}", parts.join("、")))
                .small()
                .color(Color32::GRAY),
        );
    }

    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("来源").small().color(Color32::GRAY));
        for (label, value) in [
            ("全部", None),
            ("本地文件", Some(KbOrigin::File)),
            ("邮件", Some(KbOrigin::Mail)),
            ("AI 写入", Some(KbOrigin::Ai)),
        ] {
            let selected = state.knowledge.origin == value;
            if ui.selectable_label(selected, label).clicked() {
                state.knowledge.origin = value;
                state.knowledge.dirty = true;
            }
        }
        ui.separator();
        ui.label(RichText::new("状态").small().color(Color32::GRAY));
        let mut changed_status = false;
        egui::ComboBox::from_id_salt("kb_status_filter")
            .selected_text(
                state
                    .knowledge
                    .status
                    .map(KbStatus::label)
                    .unwrap_or("全部"),
            )
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(state.knowledge.status.is_none(), "全部")
                    .clicked()
                {
                    state.knowledge.status = None;
                    changed_status = true;
                }
                for status in [
                    KbStatus::Pending,
                    KbStatus::Chunked,
                    KbStatus::Embedded,
                    KbStatus::Skipped,
                    KbStatus::Failed,
                ] {
                    if ui
                        .selectable_label(state.knowledge.status == Some(status), status.label())
                        .clicked()
                    {
                        state.knowledge.status = Some(status);
                        changed_status = true;
                    }
                }
            });
        if changed_status {
            state.knowledge.dirty = true;
        }
    });

    ui.horizontal(|ui| {
        let response = ui.add(
            egui::TextEdit::singleline(&mut state.knowledge.filter_text)
                .hint_text("按标题筛选")
                .desired_width(180.0),
        );
        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            state.knowledge.dirty = true;
        }
        ui.separator();
        let search = ui.add(
            egui::TextEdit::singleline(&mut state.knowledge.search)
                .hint_text("检索知识库（关键词 + 语义）")
                .desired_width(260.0),
        );
        let go = ui.button("检索").clicked()
            || (search.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
        if go && !state.knowledge.search.trim().is_empty() {
            state.knowledge.searching = true;
            commands.push(Command::SearchKnowledge {
                query: state.knowledge.search.trim().to_string(),
                limit: 20,
            });
        }
        if !state.knowledge.hits.is_empty() && ui.button("清除结果").clicked() {
            state.knowledge.hits.clear();
            state.knowledge.search_error = None;
        }
        ui.label(
            RichText::new(state.knowledge.search_mode.label())
                .small()
                .color(Color32::GRAY),
        );
    });

    if let Some(error) = state.knowledge.search_error.clone() {
        ui.label(
            RichText::new(error)
                .small()
                .color(Color32::from_rgb(0xD0, 0x4A, 0x3C)),
        );
    }

    if state.knowledge.searching {
        ui.label(RichText::new("检索中…").small().color(Color32::GRAY));
    }

    ui.separator();

    if state.knowledge.selected.is_some() {
        show_detail(state, commands, ui);
    } else if !state.knowledge.hits.is_empty() {
        show_hits(state, commands, ui);
    } else {
        show_list(state, commands, ui);
    }
}

/// 语义检索没启用时的引导卡 / 提示条。
fn show_embed_banner(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    let settings = state.knowledge.settings.clone();
    if settings.semantic_ready() {
        ui.label(
            RichText::new(format!(
                "语义检索已启用 · {}",
                settings
                    .embed_model
                    .as_deref()
                    .unwrap_or(me_core::EMBED_MODEL_ID)
            ))
            .small()
            .color(Color32::from_rgb(0x2E, 0x7D, 0x32)),
        );
        return;
    }
    if state.knowledge.settings.embed_declined {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("语义检索未启用（仅关键词检索）")
                    .small()
                    .color(Color32::from_rgb(0xE0, 0x8A, 0x1E)),
            );
            if ui.small_button("启用").clicked() {
                commands.push(Command::SetKbEmbedDeclined { declined: false });
                state.knowledge.settings.embed_declined = false;
                state.knowledge.settings.embed_model = None;
            }
        });
        return;
    }
    if let Some((done, total)) = state.knowledge.download {
        let fraction = total
            .filter(|total| *total > 0)
            .map(|total| done as f32 / total as f32)
            .unwrap_or(0.0);
        ui.add(
            egui::ProgressBar::new(fraction)
                .text(format!(
                    "下载模型 {:.1} / {:.1} MB",
                    done as f64 / 1024.0 / 1024.0,
                    total.unwrap_or(0) as f64 / 1024.0 / 1024.0
                ))
                .desired_width(360.0),
        );
        if ui.button("取消下载").clicked() {
            commands.push(Command::CancelEmbedModelDownload);
        }
        return;
    }

    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(RichText::new("语义检索需要外置嵌入模型").strong());
        ui.label(
            RichText::new(
                "bge-small-zh-v1.5 · 约 96 MB · 512 维中文嵌入\n\
                 下载后只在本机计算，文档内容不上传（PDF/PPT 分析另需你确认）\n\
                 来源：ModelScope",
            )
            .small()
            .color(Color32::GRAY),
        );
        ui.horizontal(|ui| {
            if ui.button("下载并启用").clicked() {
                commands.push(Command::DownloadEmbedModel);
            }
            if ui.button("暂不启用，仅用关键词检索").clicked() {
                // 落进 meta：下次启动不再弹引导卡，顶部保留一条「启用」提示。
                commands.push(Command::SetKbEmbedDeclined { declined: true });
                state.knowledge.settings.embed_declined = true;
            }
        });
    });
}

fn show_list(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    let docs = &state.knowledge.docs;
    if docs.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.label(RichText::new("还没有文档。点右上角「导入文件…」开始。").color(Color32::GRAY));
        });
        return;
    }
    let mut select: Option<KnowledgeDocId> = None;
    let mut delete: Option<KnowledgeDocId> = None;
    egui::ScrollArea::vertical()
        .id_salt("kb_doc_list")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for doc in docs {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(truncate_chars(&doc.title, 60)).strong());
                        ui.label(
                            RichText::new(format!(
                                "{} · {} · {}",
                                doc.kind.label(),
                                doc.origin.label(),
                                status_label(doc.status)
                            ))
                            .small()
                            .color(status_color(doc.status)),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("删除").clicked() {
                                delete = Some(doc.id);
                            }
                            if ui.small_button("查看").clicked() {
                                select = Some(doc.id);
                            }
                        });
                    });
                    let mut meta = format!(
                        "{} 块 · {} 字 · {:.1} KB",
                        doc.chunk_count,
                        doc.char_count,
                        doc.size_bytes as f64 / 1024.0
                    );
                    if let Some(reason) = doc.skip_reason {
                        meta.push_str(&format!(" · 未索引：{}", reason.label()));
                    }
                    if let Some(error) = doc.error.as_deref() {
                        meta.push_str(&format!(" · {}", truncate_chars(error, 80)));
                    }
                    ui.label(RichText::new(meta).small().color(Color32::GRAY));
                });
            }
        });
    if let Some(id) = select {
        state.knowledge.selected = Some(id);
        state.knowledge.chunk_page = 0;
        state.knowledge.dirty = true;
    }
    if let Some(id) = delete {
        commands.push(Command::DeleteKnowledgeDoc { doc: id });
    }
}

fn show_hits(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    let hits = state.knowledge.hits.clone();
    let mut select = None;
    egui::ScrollArea::vertical()
        .id_salt("kb_hits")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for hit in &hits {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    let locator = hit
                        .locator
                        .as_deref()
                        .map(|value| format!(" · {value}"))
                        .unwrap_or_default();
                    if ui
                        .link(
                            RichText::new(format!("{}（第 {} 块）{locator}", hit.title, hit.ord))
                                .strong(),
                        )
                        .clicked()
                    {
                        select = Some(hit.doc);
                    }
                    ui.label(
                        RichText::new(truncate_chars(&hit.text, 220))
                            .small()
                            .color(Color32::GRAY),
                    );
                });
            }
        });
    if let Some(id) = select {
        state.knowledge.selected = Some(id);
        state.knowledge.chunk_page = 0;
        state.knowledge.dirty = true;
    }
    let _ = commands;
}

fn show_detail(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    let Some(doc) = state.knowledge.doc.clone() else {
        ui.label(RichText::new("读取中…").color(Color32::GRAY));
        return;
    };
    ui.horizontal(|ui| {
        if ui.button("← 返回列表").clicked() {
            state.knowledge.reset_detail();
            state.knowledge.dirty = true;
        }
        ui.heading(truncate_chars(&doc.title, 60));
        ui.label(
            RichText::new(format!("{} · {}", doc.kind.label(), doc.origin.label()))
                .small()
                .color(Color32::GRAY),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("删除").clicked() {
                state.knowledge.pending_delete = Some((doc.id, doc.title.clone(), doc.chunk_count));
            }
            if ui.button("重建索引").clicked() {
                commands.push(Command::RebuildKnowledgeDoc { doc: doc.id });
            }
        });
    });
    let mut meta = format!(
        "状态：{} · {} 块 · {} 字",
        status_label(doc.status),
        doc.chunk_count,
        doc.char_count
    );
    if let Some(model) = doc.embed_model.as_deref() {
        meta.push_str(&format!(" · 向量：{model}"));
    }
    if let Some(reason) = doc.skip_reason {
        meta.push_str(&format!(" · 未索引：{}", reason.label()));
    }
    ui.label(RichText::new(meta).small().color(Color32::GRAY));
    if let Some(error) = doc.error.as_deref() {
        ui.label(
            RichText::new(error)
                .small()
                .color(Color32::from_rgb(0xD0, 0x4A, 0x3C)),
        );
    }
    if !state.knowledge.entities.is_empty() {
        ui.label(
            RichText::new(format!("涉及：{}", state.knowledge.entities.join("、")))
                .small()
                .color(Color32::GRAY),
        );
    }

    ui.separator();
    let total = doc.chunk_count.max(1);
    let page = state.knowledge.chunk_page;
    let pages = total.div_ceil(20);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(page > 0, egui::Button::new("上一页"))
            .clicked()
        {
            state.knowledge.chunk_page = page.saturating_sub(1);
            state.knowledge.dirty = true;
        }
        ui.label(
            RichText::new(format!("第 {}/{} 页", page + 1, pages))
                .small()
                .color(Color32::GRAY),
        );
        if ui
            .add_enabled(page + 1 < pages, egui::Button::new("下一页"))
            .clicked()
        {
            state.knowledge.chunk_page = page + 1;
            state.knowledge.dirty = true;
        }
    });

    let chunks = state.knowledge.chunks.clone();
    egui::ScrollArea::vertical()
        .id_salt("kb_chunks")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for chunk in &chunks {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label(
                        RichText::new(format!(
                            "#{}{}",
                            chunk.ord,
                            chunk
                                .locator
                                .as_deref()
                                .map(|value| format!(" · {value}"))
                                .unwrap_or_default()
                        ))
                        .small()
                        .color(Color32::GRAY),
                    );
                    ui.label(&chunk.text);
                });
            }
            if chunks.is_empty() {
                ui.label(RichText::new("这一页没有语块。").color(Color32::GRAY));
            }
        });
}

fn status_label(status: KbStatus) -> &'static str {
    status.label()
}

/// 状态色：`skipped` 是**橙**不是红——它是正常结果，不是错误。
fn status_color(status: KbStatus) -> Color32 {
    match status {
        KbStatus::Pending => Color32::GRAY,
        KbStatus::Extracted | KbStatus::Chunked => Color32::from_rgb(0x3B, 0x82, 0xF6),
        KbStatus::Embedded => Color32::from_rgb(0x2E, 0x7D, 0x32),
        KbStatus::Skipped => Color32::from_rgb(0xE0, 0x8A, 0x1E),
        KbStatus::Failed => Color32::from_rgb(0xD0, 0x4A, 0x3C),
    }
}

/// 选文件 → 按类型分流：pdf/pptx 先探测（要页数/批数），其余直接导入。
fn pick_files(state: &mut UiState, commands: &mut Vec<Command>) {
    let Some(paths) = rfd::FileDialog::new()
        .add_filter(
            "文档",
            &["md", "markdown", "txt", "docx", "pdf", "pptx", "doc", "ppt"],
        )
        .pick_files()
    else {
        return;
    };
    for path in paths {
        let name = path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_default();
        let ext = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let text = path.to_string_lossy().to_string();
        if matches!(ext.as_str(), "pdf" | "pptx") {
            state.knowledge.importing.push(name);
            commands.push(Command::ProbeKnowledgeFile { path: text });
        } else {
            state.knowledge.importing.push(name);
            commands.push(Command::ImportKnowledgeFile { path: text });
        }
    }
}

/// 两个弹窗：一次性总确认（费用 + 隐私）、每次导入的预估。
///
/// **预估框不提供「不再提示」**：它是每次花钱前唯一的闸门（决策 O-18）。
pub fn modals(state: &mut UiState, commands: &mut Vec<Command>, ctx: &egui::Context) {
    if let Some(probe) = state.knowledge.ack_pending.clone() {
        let mut close = false;
        let mut accept = false;
        egui::Modal::new(egui::Id::new("kb_llm_ack")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading("PDF / PPT 需要大语言模型分析");
            ui.add_space(6.0);
            ui.label(
                "这类文档无法在本地可靠地识别内容。导入后，整份文件会发送给\
                 「当前模型」进行分析并切成语块。",
            );
            ui.add_space(4.0);
            for line in [
                "· 文档内容会离开本机，发送给模型服务商",
                "· 按页计费，索引完成后不再产生费用",
                "· 参考价：约 $0.006/页（一本 300 页文档约 $1.8）",
            ] {
                ui.label(RichText::new(line).small().color(Color32::GRAY));
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("我已了解，继续").clicked() {
                    accept = true;
                }
                if ui.button("改用其它格式").clicked() {
                    close = true;
                }
            });
        });
        if accept {
            commands.push(Command::SetKbLlmAck { accepted: true });
            state.knowledge.settings.llm_ack = true;
            state.knowledge.ack_pending = None;
            state.knowledge.estimate_pending = Some(probe);
        } else if close {
            // 不写标记：用户下次还会看到这个框。我们无权替他转格式。
            state.knowledge.ack_pending = None;
            drop_importing(&mut state.knowledge, &probe.path);
        }
    }

    if let Some(probe) = state.knowledge.estimate_pending.clone() {
        let mut start = false;
        let mut cancel = false;
        egui::Modal::new(egui::Id::new("kb_estimate")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.heading("本次导入的预估");
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!("文件大小 {}", human_bytes(probe.bytes)))
                    .small()
                    .color(Color32::GRAY),
            );
            if probe.kind == KbKind::Pdf {
                ui.label(format!(
                    "将发送 {} 页，分 {} 批",
                    probe.pages, probe.batches
                ));
            } else {
                ui.label(format!(
                    "共 {} 字，分 {} 批",
                    probe.tokens_low, probe.batches
                ));
            }
            ui.label(
                RichText::new(format!(
                    "预计消耗约 {}k ~ {}k token{}",
                    probe.tokens_low / 1000,
                    probe.tokens_high / 1000,
                    match probe.cost_usd {
                        Some((low, high)) => format!(" · 约 ${low:.2} ~ ${high:.2}"),
                        // 内置价表里没有这个模型：宁可少说，不要瞎说。
                        None => " · 以服务商账单为准".to_string(),
                    }
                ))
                .small()
                .color(Color32::GRAY),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("开始导入").clicked() {
                    start = true;
                }
                if ui.button("取消").clicked() {
                    cancel = true;
                }
            });
        });
        if start {
            commands.push(Command::ImportKnowledgeFile {
                path: probe.path.clone(),
            });
            state.knowledge.estimate_pending = None;
        } else if cancel {
            state.knowledge.estimate_pending = None;
            drop_importing(&mut state.knowledge, &probe.path);
        }
    }

    if let Some((doc, title, chunks)) = state.knowledge.pending_delete.clone() {
        let mut confirm = false;
        let mut cancel = false;
        egui::Modal::new(egui::Id::new("kb_delete_confirm")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.heading("删除知识库文档");
            ui.label(format!(
                "「{title}」及其 {chunks} 个语块、向量索引都会被删除。\n\
                 原文文件不受影响；邮件正文仍在邮箱里。"
            ));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("确认删除").clicked() {
                    confirm = true;
                }
                if ui.button("取消").clicked() {
                    cancel = true;
                }
            });
        });
        if confirm {
            commands.push(Command::ConfirmKnowledgeDelete { doc });
            state.knowledge.pending_delete = None;
            if state.knowledge.selected == Some(doc) {
                state.knowledge.reset_detail();
            }
        } else if cancel {
            state.knowledge.pending_delete = None;
        }
    }
}

fn drop_importing(state: &mut KnowledgeState, path: &str) {
    if let Some(name) = std::path::Path::new(path)
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        && let Some(index) = state.importing.iter().position(|item| *item == name)
    {
        state.importing.remove(index);
    }
}

/// 供 `app.rs` 在收到探测结果时调用：把事件转成弹窗状态。
pub fn on_probed(state: &mut UiState, probe: ProbeView) {
    drop_importing(&mut state.knowledge, &probe.path);
    if let Some(reason) = probe.blocked.clone() {
        state.toast(ToastKind::Warn, reason);
        return;
    }
    if probe.kind.needs_llm() && !state.knowledge.settings.llm_ack {
        state.knowledge.ack_pending = Some(probe);
    } else {
        state.knowledge.estimate_pending = Some(probe);
    }
}

/// 文件大小的人类可读文本。
fn human_bytes(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const KB: u64 = 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}
