//! AI 对话页：会话列表、聊天区、工具活动、引用 chip 与服务商设置。
//!
//! 渲染约束（`doc/v03架构设计.md` §5.4.7）：
//! - 助手输出按 Markdown 子集渲染；**不执行 HTML、不加载图片**；
//! - 流式期间用纯文本直出（每帧解析 Markdown 会卡），完成后解析一次并缓存；
//! - 工具只展示摘要与邮件卡片，不展示完整正文。

use std::time::Instant;

use eframe::egui::{self, Color32, RichText};
use me_core::ai::{
    AiContextMode, AiContextRequest, AiConversation, AiConversationId, AiEntity, AiMessage,
    AiMessageId, AiMessageStatus, AiModel, AiProviderKind, AiToolPermissions, AiToolTrace,
    DEFAULT_MAX_INPUT_CHARS, DEFAULT_MAX_OUTPUT_TOKENS, estimate_tokens, next_model_id,
    truncate_chars,
};
use me_core::{Command, MessageId};

use crate::state::{Page, ToastKind, UiState};
use crate::views::richtext::{Block, markdown_to_blocks, show_with_images};

/// 模型发起的删除请求，等用户在卡片上点「确认删除」。
///
/// 只保留最近一条：第二张确认卡会覆盖第一张，避免堆一屏「删不删？」。
#[derive(Debug, Clone)]
pub struct PendingDelete {
    pub event: me_core::CalendarEventId,
    pub title: String,
    pub start_utc: i64,
}

/// 刚删除、仍可撤销的日程（10 秒窗口）。
#[derive(Debug, Clone)]
pub struct DeletedEvent {
    pub event: me_core::CalendarEventId,
    pub title: String,
    pub at: Instant,
}

/// 撤销窗口：与 AI 建日程的撤销条一致，10 秒后自动消失。
const UNDO_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

/// 输入框上方的引用 chip：id 用于移除，subject 用于显示。
#[derive(Debug, Clone)]
pub struct PendingContext {
    pub subject: String,
    pub request: AiContextRequest,
}

#[derive(Debug, Clone)]
pub struct AiStreamView {
    pub conversation: AiConversationId,
    pub seq: u64,
    pub buffer: String,
    pub started: Instant,
}

#[derive(Debug, Clone)]
pub struct AiCard {
    pub message: MessageId,
    /// 为什么这封邮件出现在结果里（主题命中 / 发件人命中 / 正文片段命中）。
    pub reason: String,
    pub subject: String,
    pub from: String,
    pub date: Option<i64>,
    pub snippet: String,
}

/// 设置区里正在编辑的模型档案草稿。
///
/// 数字用字符串存：`TextEdit` 直接绑数字类型时，清空输入框会立刻变成 0，
/// 用户没法「全删再重打」。保存时才解析，非法值由 `me-core` 统一拒绝。
#[derive(Debug, Clone)]
pub struct AiModelDraft {
    /// 空字符串 = 尚未落库的新档案（保存前由界面分配 id）。
    pub id: String,
    pub name: String,
    pub kind: AiProviderKind,
    pub base_url: String,
    pub model: String,
    pub max_input: String,
    pub max_output: String,
    pub tools: AiToolPermissions,
    /// API Key。与其他参数一样随「保存」一起写入，不单独保存。
    pub api_key: String,
}

impl AiModelDraft {
    /// 新建档案的初始草稿。
    ///
    /// `id` 在界面上就定下来（而不是等后端回填）：这样保存后仍能停在
    /// 刚保存的档案上，用户接着改参数不用再点一次列表。
    pub fn new(id: String, kind: AiProviderKind) -> Self {
        Self {
            id,
            name: String::new(),
            kind,
            base_url: kind.default_base_url().to_string(),
            model: kind
                .preset_models()
                .first()
                .copied()
                .unwrap_or_default()
                .to_string(),
            max_input: DEFAULT_MAX_INPUT_CHARS.to_string(),
            max_output: DEFAULT_MAX_OUTPUT_TOKENS.to_string(),
            tools: AiToolPermissions::default(),
            api_key: String::new(),
        }
    }

    pub fn from_model(model: &AiModel) -> Self {
        Self {
            id: model.id.clone(),
            name: model.name.clone(),
            kind: model.kind,
            base_url: model.base_url.clone(),
            model: model.model.clone(),
            max_input: model.max_input_chars.to_string(),
            max_output: model.max_output_tokens.to_string(),
            tools: model.tools,
            api_key: model.api_key.clone(),
        }
    }

    pub fn to_model(&self) -> AiModel {
        AiModel {
            id: self.id.trim().to_string(),
            name: self.name.trim().to_string(),
            kind: self.kind,
            base_url: self.base_url.trim().to_string(),
            model: self.model.trim().to_string(),
            max_input_chars: self
                .max_input
                .trim()
                .parse()
                .unwrap_or(DEFAULT_MAX_INPUT_CHARS),
            max_output_tokens: self
                .max_output
                .trim()
                .parse()
                .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
            tools: self.tools,
            api_key: self.api_key.trim().to_string(),
        }
    }
}

#[derive(Default)]
pub struct AiPageState {
    pub conversations: Vec<AiConversation>,
    pub selected: Option<AiConversationId>,
    pub messages: Vec<AiMessage>,
    pub input: String,
    pub pending: Vec<PendingContext>,
    pub stream: Option<AiStreamView>,
    pub activity: Vec<AiToolTrace>,
    pub cards: Vec<AiCard>,
    /// 卡片对应的完整摘要。AI 页的检索结果可能不在当前邮件列表里，
    /// 阅读窗要能在 AI 页直接打开它（`reader::show_window` 只认摘要）。
    pub card_messages: Vec<me_core::MessageSummary>,
    pub entities: Vec<AiEntity>,
    /// 当前这轮的任务清单（来自 `AiPlanUpdated` 或最后一条消息的落库值）。
    pub plan: Option<me_core::ai_plan::Plan>,

    /// 全部模型档案，顺序即列表顺序。
    pub models: Vec<AiModel>,
    /// 当前模型（新对话默认用它）。
    pub active_model: Option<String>,
    /// 正在编辑的档案；`None` = 设置页显示模型列表。
    pub draft: Option<AiModelDraft>,
    pub privacy_ack_anthropic: bool,
    pub privacy_ack_openai: bool,
    pub entity_count: u32,
    pub test_result: Option<(bool, String)>,
    /// 刚由 AI 创建、仍可撤销的日程。
    pub last_calendar_event: Option<me_core::CalendarEventId>,
    /// 模型发起的删除请求，等用户确认。
    pub pending_delete: Option<PendingDelete>,
    /// 刚删除、10 秒内可撤销的日程。
    pub last_deleted: Option<DeletedEvent>,
    /// 设置弹窗是否打开。
    pub show_settings: bool,
    /// 正在改写的会话标题；`Some` = 标题处于编辑态。
    pub renaming: Option<String>,
    /// 「清空全部对话」的二次确认态。
    pub confirm_clear_conversations: bool,
    /// 表单校验失败的原因（保存前在本地拦一次，不依赖后端回包）。
    pub form_error: Option<String>,

    /// 已解析的 Markdown 缓存：`(message, 长度, blocks)`。
    blocks_cache: Option<(AiMessageId, usize, Vec<Block>)>,
    pub loading: bool,
}

impl AiPageState {
    pub fn selected_conversation(&self) -> Option<&AiConversation> {
        let id = self.selected?;
        self.conversations.iter().find(|item| item.id == id)
    }

    pub fn privacy_acked(&self, provider: AiProviderKind) -> bool {
        match provider {
            AiProviderKind::Anthropic => self.privacy_ack_anthropic,
            AiProviderKind::OpenAiCompatible => self.privacy_ack_openai,
        }
    }

    pub fn model_by_id(&self, id: &str) -> Option<&AiModel> {
        self.models.iter().find(|model| model.id == id)
    }

    /// 某个会话实际会用哪套参数：会话绑定 → 当前模型 → 第一个。
    pub fn model_for_conversation(&self, conversation: &AiConversation) -> Option<&AiModel> {
        conversation
            .model_id
            .as_deref()
            .and_then(|id| self.model_by_id(id))
            .or_else(|| {
                self.active_model
                    .as_deref()
                    .and_then(|id| self.model_by_id(id))
            })
            .or_else(|| self.models.first())
    }

    pub fn is_streaming(&self) -> bool {
        self.stream.is_some()
    }

    /// 开始编辑一个新档案。
    pub fn start_new_model(&mut self) {
        let id = next_model_id(&self.models);
        self.draft = Some(AiModelDraft::new(id, AiProviderKind::Anthropic));
        self.test_result = None;
        self.form_error = None;
    }

    /// 编辑已有档案。
    pub fn edit_model(&mut self, id: &str) {
        if let Some(model) = self.model_by_id(id) {
            self.draft = Some(AiModelDraft::from_model(model));
            self.test_result = None;
            self.form_error = None;
        }
    }

    /// 切换会话：清掉上一个会话的临时 UI（工具活动、卡片、流缓冲）。
    ///
    /// 只清本地状态，**不负责重查消息**。真实切换请走 `UiState::select_ai_conversation`，
    /// 它会一起置 `ai_messages_dirty`；单独调用这里的结果是主区空白。
    pub fn select(&mut self, id: Option<AiConversationId>) {
        if self.selected == id {
            return;
        }
        self.selected = id;
        self.messages.clear();
        self.activity.clear();
        self.cards.clear();
        self.card_messages.clear();
        self.plan = None;
        self.pending_delete = None;
        self.last_deleted = None;
        self.stream = None;
        self.blocks_cache = None;
        self.loading = true;
    }

    /// 收到设置快照。**不动草稿**：用户可能正在输入，重填会把字吃掉。
    /// 只处理「草稿指向的档案已被删除」这一种必须打断的情况。
    pub fn apply_config(
        &mut self,
        models: Vec<AiModel>,
        active: Option<String>,
        acks: (bool, bool),
        entity_count: u32,
    ) {
        let draft_orphaned = self.draft.as_ref().is_some_and(|draft| {
            !draft.id.is_empty() && !models.iter().any(|model| model.id == draft.id)
        });
        self.models = models;
        self.active_model = active;
        self.privacy_ack_anthropic = acks.0;
        self.privacy_ack_openai = acks.1;
        self.entity_count = entity_count;
        if draft_orphaned {
            self.draft = None;
        }
    }

    /// 流开始。助手消息 id 由服务端落库，UI 不需要持有——终止事件后统一重载。
    pub fn on_stream_started(&mut self, conversation: AiConversationId) {
        self.stream = Some(AiStreamView {
            conversation,
            seq: 0,
            buffer: String::new(),
            started: Instant::now(),
        });
        self.activity.clear();
        self.cards.clear();
    }

    pub fn on_delta(&mut self, conversation: AiConversationId, seq: u64, delta: &str) {
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        if stream.conversation != conversation {
            return;
        }
        stream.buffer.push_str(delta);
        stream.seq = seq;
    }

    pub fn on_finished(&mut self) {
        self.stream = None;
        self.blocks_cache = None;
        self.loading = true;
    }
}

/// 左侧栏：会话列表 + 设置入口。
///
/// **设置内容只在主区渲染**。侧栏宽度放不下「名称/端点/Key/工具」这一整套，
/// 两处同时渲染还会让同一个输入框出现两个焦点、两份滚动位置。
pub fn sidebar(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    ui.add_space(4.0);
    ui.label(RichText::new("AI 对话").strong());

    ui.horizontal(|ui| {
        if ui.button("＋ 新对话").clicked() {
            commands.push(Command::CreateAiConversation { model: None });
        }
        if ui.button("设置").clicked() {
            state.ai.show_settings = !state.ai.show_settings;
        }
    });

    // 当前模型：新建对话默认用它，放在这里省得每次都要进设置确认。
    let active = state
        .ai
        .active_model
        .as_deref()
        .and_then(|id| state.ai.model_by_id(id))
        .map(|model| model.name.clone())
        .unwrap_or_else(|| "未配置".to_string());
    ui.label(
        RichText::new(format!("当前模型：{active}"))
            .small()
            .color(Color32::GRAY),
    );

    ui.separator();
    egui::ScrollArea::vertical()
        .id_salt("ai_conversations")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let mut pick: Option<AiConversationId> = None;
            for conversation in &state.ai.conversations {
                let selected = state.ai.selected == Some(conversation.id);
                let text = RichText::new(truncate_chars(&conversation.title, 24))
                    .size(13.0)
                    .color(if selected {
                        Color32::WHITE
                    } else {
                        ui.visuals().text_color()
                    });
                let button = egui::Button::new(text)
                    .fill(if selected {
                        ui.visuals().selection.bg_fill
                    } else {
                        Color32::TRANSPARENT
                    })
                    .min_size(egui::vec2(ui.available_width().max(80.0), 24.0));
                if ui.add(button).clicked() {
                    pick = Some(conversation.id);
                }
            }
            if let Some(id) = pick {
                // 走状态层入口：它负责清缓冲 + 置脏重查（只调 `ai.select` 会留一页空白）。
                state.select_ai_conversation(Some(id));
            }
        });
}

/// 主区：聊天 + 输入。
///
/// 设置不占用主区——它是独立弹窗（`settings_modal`），否则点开设置后对话就被顶掉了，
/// 关掉设置还得再来一次「切页回 AI」才能看到记录。
pub fn show(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    let Some(conversation_id) = state.ai.selected else {
        ui.vertical_centered(|ui| {
            ui.add_space(80.0);
            ui.label(
                RichText::new("选择左侧的对话，或新建一个")
                    .size(14.0)
                    .color(Color32::GRAY),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new("AI 只能检索本地已缓存的邮件，不会自动读取邮箱或发送邮件。")
                    .small()
                    .color(Color32::GRAY),
            );
        });
        return;
    };

    let title = state
        .ai
        .selected_conversation()
        .map(|item| item.title.clone())
        .unwrap_or_else(|| "对话".to_string());
    // 标题编辑态单独取出：否则下面的模型下拉要连着借 `state.ai` 会冲突。
    let mut renaming = state.ai.renaming.take();
    ui.horizontal(|ui| {
        if let Some(buffer) = renaming.as_mut() {
            let response = ui.add(
                egui::TextEdit::singleline(buffer)
                    .desired_width(220.0)
                    .hint_text("对话标题"),
            );
            let submitted = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let save = ui.small_button("保存").clicked() || submitted;
            let cancel =
                ui.small_button("取消").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape));
            if save {
                let title = buffer.trim().to_string();
                renaming = None;
                if !title.is_empty() {
                    commands.push(Command::RenameAiConversation {
                        id: conversation_id,
                        title,
                    });
                }
            } else if cancel {
                renaming = None;
            }
        } else {
            ui.label(RichText::new(truncate_chars(&title, 32)).strong());
            if ui
                .small_button("✎")
                .on_hover_text("重命名这个对话")
                .clicked()
            {
                renaming = Some(title.clone());
            }
        }
        // 模型下拉与标题编辑互不影响：改标题时也能看到当前用哪个模型。
        show_model_picker(state, commands, ui, conversation_id);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("删除").clicked() {
                commands.push(Command::DeleteAiConversation {
                    id: conversation_id,
                });
                state.select_ai_conversation(None);
            }
            if state.ai.is_streaming() {
                if ui.button("停止").clicked() {
                    commands.push(Command::CancelAiStream {
                        conversation: conversation_id,
                    });
                }
            } else if ui.button("重新生成").clicked() {
                commands.push(Command::RegenerateAiMessage {
                    conversation: conversation_id,
                });
            }
        });
    });
    state.ai.renaming = renaming;
    ui.separator();

    let input_height = 96.0;
    let available = ui.available_height();
    egui::ScrollArea::vertical()
        .id_salt("ai_messages")
        .auto_shrink([false, false])
        .max_height((available - input_height - 24.0).max(120.0))
        .stick_to_bottom(true)
        .show(ui, |ui| {
            // 刚切会话时消息还在查库，先给个占位，别让主区看起来像空的。
            if state.ai.messages.is_empty() && state.ai.loading && !state.ai.is_streaming() {
                ui.add_space(24.0);
                ui.label(RichText::new("正在加载对话…").small().color(Color32::GRAY));
            }
            // 先把消息取出再渲染：`show_message` 需要 `&mut state`（Markdown 缓存），
            // 直接迭代会把 `state.ai.messages` 一直借用着。取出的 Vec 随后放回。
            let messages = std::mem::take(&mut state.ai.messages);
            for message in &messages {
                show_message(state, commands, ui, message);
            }
            state.ai.messages = messages;
            // 清单放在消息与流式正文之间：它描述「这一轮正在做什么」，
            // 贴在底部才不会被长对话顶出视野。
            if let Some(plan) = &state.ai.plan {
                show_plan(ui, plan);
            }
            show_delete_confirm(state, commands, ui);
            show_stream(ui, &state.ai);
            if !state.ai.activity.is_empty() {
                show_activity(ui, &state.ai.activity);
            }
            if !state.ai.cards.is_empty() {
                show_cards(state, commands, ui);
            }
        });

    ui.separator();
    show_input(state, commands, ui, conversation_id);
}

/// 「设置」弹窗。
///
/// 用 `egui::Modal` 而不是 `Window`：遮罩会吃掉下层输入，点遮罩或按 Esc 即关闭，
/// 不会出现「弹窗开着还能点对话里的按钮」这种半模态状态。
pub fn settings_modal(state: &mut UiState, commands: &mut Vec<Command>, ctx: &egui::Context) {
    if !state.ai.show_settings {
        return;
    }
    let mut close = false;
    let modal = egui::Modal::new(egui::Id::new("ai_settings_modal")).show(ctx, |ui| {
        ui.set_width(620.0);
        ui.horizontal(|ui| {
            ui.heading("AI 设置");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("关闭").clicked() {
                    close = true;
                }
            });
        });
        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .id_salt("ai_settings_scroll")
            .max_height(520.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                settings_section(state, commands, ui);
            });
    });
    if close || modal.should_close() {
        state.ai.show_settings = false;
    }
}

/// 对话栏的模型切换。
///
/// 只改**这个会话**绑定的档案，不动「当前模型」——用户临时换个模型问一句，
/// 不该让之后新建的对话也跟着变。
fn show_model_picker(
    state: &mut UiState,
    commands: &mut Vec<Command>,
    ui: &mut egui::Ui,
    conversation: AiConversationId,
) {
    let current = state
        .ai
        .selected_conversation()
        .and_then(|conv| state.ai.model_for_conversation(conv))
        .map(|model| (model.id.clone(), model.name.clone(), model.kind));
    let label = match &current {
        Some((_, name, kind)) => format!("{} · {}", kind.label(), name),
        None => "未配置模型".to_string(),
    };
    egui::ComboBox::from_id_salt("ai_model_picker")
        .selected_text(RichText::new(truncate_chars(&label, 26)).small())
        .width(170.0)
        .show_ui(ui, |ui| {
            if state.ai.models.is_empty() {
                ui.label("还没有模型，去「设置」里添加");
                return;
            }
            for model in &state.ai.models {
                let selected = current.as_ref().is_some_and(|(id, _, _)| id == &model.id);
                let text = format!("{} · {}", model.kind.label(), model.name);
                if ui.selectable_label(selected, text).clicked() && !selected {
                    commands.push(Command::SetConversationAiModel {
                        conversation,
                        model: model.id.clone(),
                    });
                }
            }
        });
}

fn show_message(
    state: &mut UiState,
    commands: &mut Vec<Command>,
    ui: &mut egui::Ui,
    message: &AiMessage,
) {
    let is_user = message.role == me_core::ai::AiRole::User;
    let width = (ui.available_width() * 0.82).max(200.0);
    ui.add_space(6.0);
    if is_user {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            egui::Frame::group(ui.style())
                .fill(ui.visuals().faint_bg_color)
                .show(ui, |ui| {
                    ui.set_max_width(width);
                    ui.label(&message.content);
                    for reference in &message.context_refs {
                        ui.label(
                            RichText::new(format!(
                                "引用：{}（{}）",
                                truncate_chars(&reference.subject, 30),
                                reference.mode.as_str()
                            ))
                            .small()
                            .color(Color32::GRAY),
                        );
                    }
                });
        });
    } else {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_max_width(width);
            if message.content.is_empty() {
                ui.label(RichText::new("…").color(Color32::GRAY));
            } else {
                // 先切出 `[结果]` 段落，再按 Markdown 渲染叙述部分。
                // 缓存键用**叙述长度**：结果段落还在流式增长时，正文没变就不必重排。
                let (body, result) = match split_result_block(&message.content) {
                    Some((body, block)) => (body, Some(block)),
                    None => (message.content.as_str(), None),
                };
                let cache_hit = matches!(
                    &state.ai.blocks_cache,
                    Some((id, len, _)) if *id == message.id && *len == body.len()
                );
                if !cache_hit {
                    let blocks = markdown_to_blocks(body);
                    state.ai.blocks_cache = Some((message.id, body.len(), blocks));
                }
                if let Some((_, _, blocks)) = &state.ai.blocks_cache {
                    show_with_images(ui, blocks, None);
                }
                match result {
                    Some(block) => show_result_block(ui, &block),
                    // 模型漏写 `[结果]` 时用本轮工具回执兜底：只给事实，不假装有结论。
                    // 不重发请求——用户已经看到正文了，重发要再付一次完整调用。
                    None if message.status == AiMessageStatus::Complete
                        && !message.tool_trace.is_empty() =>
                    {
                        show_result_fallback(ui, &message.tool_trace);
                    }
                    None => {}
                }
            }
            if !message.tool_trace.is_empty() {
                show_activity(ui, &message.tool_trace);
            }
            if message.status == AiMessageStatus::Failed {
                if let Some(error) = &message.error {
                    ui.label(RichText::new(error).small().color(Color32::RED));
                }
            } else if message.status == AiMessageStatus::Cancelled {
                ui.label(RichText::new("已停止").small().color(Color32::GRAY));
            }
            if let (Some(input), Some(output)) = (message.input_tokens, message.output_tokens) {
                ui.label(
                    RichText::new(format!("{input} 输入 / {output} 输出 tokens"))
                        .small()
                        .color(Color32::GRAY),
                );
            }
            if !is_user {
                ui.horizontal(|ui| {
                    if ui.small_button("复制").clicked() {
                        ui.ctx().copy_text(message.content.clone());
                    }
                    let _ = commands;
                });
            }
        });
    }
}

fn show_stream(ui: &mut egui::Ui, state: &AiPageState) {
    let Some(stream) = &state.stream else {
        return;
    };
    ui.add_space(6.0);
    egui::Frame::group(ui.style()).show(ui, |ui| {
        if stream.buffer.is_empty() {
            ui.label(RichText::new("正在思考…").color(Color32::GRAY));
        } else {
            // 流式期间不跑 Markdown：每帧解析会让滚动卡顿，而且光标位置会跳。
            ui.label(&stream.buffer);
        }
        ui.label(
            RichText::new(format!(
                "生成中… {}s · 约 {} tokens",
                stream.started.elapsed().as_secs(),
                estimate_tokens(&stream.buffer)
            ))
            .small()
            .color(Color32::GRAY),
        );
    });
}

/// 任务清单条。全部结束之后收成一行，避免常驻占地方。
fn show_plan(ui: &mut egui::Ui, plan: &me_core::ai_plan::Plan) {
    ui.add_space(6.0);
    let (done, total) = plan.progress();
    if plan.open_steps() == 0 {
        egui::CollapsingHeader::new(format!(
            "任务清单 · {}（{done}/{total} 已结束）",
            truncate_chars(&plan.goal, 36)
        ))
        .id_salt(("ai_plan_done", plan.id.clone()))
        .default_open(false)
        .show(ui, |ui| plan_steps(ui, plan));
        return;
    }

    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .show(ui, |ui| {
            ui.set_max_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("任务清单").small().strong());
                ui.label(
                    RichText::new(truncate_chars(&plan.goal, 40))
                        .small()
                        .color(Color32::GRAY),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{done}/{total}"))
                            .small()
                            .color(Color32::GRAY),
                    );
                });
            });
            plan_steps(ui, plan);
        });
}

fn plan_steps(ui: &mut egui::Ui, plan: &me_core::ai_plan::Plan) {
    for step in &plan.steps {
        let (mark, color) = plan_marker(step.status);
        ui.horizontal(|ui| {
            ui.label(RichText::new(mark).small().color(color));
            ui.label(
                RichText::new(format!("{}. {}", step.id, truncate_chars(&step.title, 40))).small(),
            );
            ui.label(
                RichText::new(step.capability.label())
                    .small()
                    .color(Color32::GRAY),
            );
        });
        if let Some(evidence) = &step.evidence {
            ui.label(
                RichText::new(format!("     {}", truncate_chars(evidence, 60)))
                    .small()
                    .color(Color32::GRAY),
            );
        }
    }
}

fn plan_marker(status: me_core::ai_plan::PlanStatus) -> (&'static str, Color32) {
    use me_core::ai_plan::PlanStatus;
    match status {
        PlanStatus::Pending => ("○", Color32::GRAY),
        PlanStatus::Running => ("◐", Color32::from_rgb(0x3B, 0x82, 0xF6)),
        PlanStatus::Done => ("●", Color32::from_rgb(0x22, 0x8B, 0x4A)),
        PlanStatus::Failed => ("✕", Color32::from_rgb(0xC0, 0x39, 0x2B)),
        PlanStatus::Skipped => ("—", Color32::GRAY),
    }
}

/// 删除确认卡。模型只能「发起」删除，真正删不删由这里决定。
fn show_delete_confirm(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    let Some(pending) = state.ai.pending_delete.clone() else {
        return;
    };
    let mut confirm = false;
    let mut cancel = false;
    ui.add_space(6.0);
    egui::Frame::group(ui.style())
        .fill(ui.visuals().extreme_bg_color)
        .show(ui, |ui| {
            ui.set_max_width(ui.available_width());
            ui.label(RichText::new("删除这条日程？").small().strong());
            ui.label(
                RichText::new(format!(
                    "「{}」· {}",
                    truncate_chars(&pending.title, 60),
                    me_core::format_local_datetime(pending.start_utc)
                ))
                .small()
                .color(Color32::GRAY),
            );
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
        commands.push(Command::DeleteCalendarEvent { id: pending.event });
        state.ai.last_deleted = Some(DeletedEvent {
            event: pending.event,
            title: pending.title.clone(),
            at: Instant::now(),
        });
        state.ai.pending_delete = None;
        state.toast(
            ToastKind::Info,
            format!("已删除「{}」", truncate_chars(&pending.title, 20)),
        );
    } else if cancel {
        state.ai.pending_delete = None;
        state.toast(ToastKind::Info, "已取消删除".to_string());
    }
}

fn show_activity(ui: &mut egui::Ui, traces: &[AiToolTrace]) {
    egui::CollapsingHeader::new(format!("工具活动（{}）", traces.len()))
        .id_salt("ai_tool_activity")
        .show(ui, |ui| {
            for trace in traces {
                ui.label(
                    RichText::new(format!(
                        "{} · {} · {} ms",
                        trace.tool.label(),
                        trace.summary,
                        trace.duration_ms
                    ))
                    .small(),
                );
                // 动机是「为什么调这个工具」。模型没写时显示占位并标灰——
                // 宁可承认没说明，也不要编一句像模像样的理由。
                let (text, weak) = match trace.motive.as_deref() {
                    Some(motive) => (format!("动机：{motive}"), false),
                    None => (format!("动机：{}", me_core::ai::DEFAULT_MOTIVE), true),
                };
                let mut label = RichText::new(text).small();
                if weak || trace.motive_source == me_core::ai::MotiveSource::Default {
                    label = label.color(Color32::GRAY);
                }
                ui.label(label);
            }
        });
}

/// `[结果]` 段落：状态 / 做了什么 / 关键信息 / 未完成。
#[derive(Debug, PartialEq, Eq)]
struct ResultBlock {
    status: String,
    done: String,
    key: String,
    todo: String,
}

/// 把回复切成「叙述」与「`[结果]` 段落」。
///
/// 三条判定规则（都有对应单测）：
/// 1. 只认**独占一行**的 `[结果]`，两侧空白允许；
/// 2. 跳过 fenced code 区间——用户在聊 Markdown 时，示例里的 `[结果]` 不是真段落；
/// 3. 四行字段一个都解析不出来时返回 `None`，整段当正文，绝不吞掉内容。
///
/// 流式期间可能出现 `[结`、`[结果` 这类半截，它们不满足「整行等于」，自然被当正文。
fn split_result_block(content: &str) -> Option<(&str, ResultBlock)> {
    let mut in_code = false;
    let mut marker: Option<(usize, usize)> = None;
    let mut offset = 0usize;
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.trim_start().starts_with("```") {
            in_code = !in_code;
        } else if !in_code && trimmed.trim() == "[结果]" {
            marker = Some((offset, offset + trimmed.len()));
            break;
        }
        offset += line.len();
    }
    let (start, end) = marker?;

    let body = content[..start].trim_end();
    let tail = content[end..].trim_start_matches(['\n', '\r']);
    let (mut status, mut done, mut key, mut todo) = (None, None, None, None);
    for line in tail.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("状态：") {
            status = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("做了什么：") {
            done = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("关键信息：") {
            key = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("未完成：") {
            todo = Some(value.trim().to_string());
        }
    }
    if status.is_none() && done.is_none() && key.is_none() && todo.is_none() {
        return None;
    }
    let dash = "—".to_string();
    // 空值也补「—」：流式到 `状态：` 就断掉时，不能渲染成一个空白标签。
    let fill = |value: Option<String>| {
        value
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| dash.clone())
    };
    Some((
        body,
        ResultBlock {
            status: fill(status),
            done: fill(done),
            key: fill(key),
            todo: fill(todo),
        },
    ))
}

fn show_result_block(ui: &mut egui::Ui, block: &ResultBlock) {
    ui.add_space(4.0);
    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .show(ui, |ui| {
            ui.label(RichText::new("结果").small().strong());
            for (name, value) in [
                ("状态", &block.status),
                ("做了什么", &block.done),
                ("关键信息", &block.key),
                ("未完成", &block.todo),
            ] {
                ui.label(RichText::new(format!("{name}：{value}")).small());
            }
        });
}

/// 模型漏写 `[结果]` 时的降级块：只陈述工具回执，不替模型下结论。
fn show_result_fallback(ui: &mut egui::Ui, traces: &[AiToolTrace]) {
    let key = traces
        .iter()
        .map(|trace| format!("{}：{}", trace.tool.label(), trace.summary))
        .collect::<Vec<_>>()
        .join("；");
    let block = ResultBlock {
        status: "未总结".to_string(),
        done: format!("执行了 {} 次工具调用", traces.len()),
        key: truncate_chars(&key, 200),
        todo: "模型没有给出结论，可让它再总结一次".to_string(),
    };
    show_result_block(ui, &block);
}

/// 一次最多铺几张卡片；多出来的折叠成一句「还有 N 封」。
const MAX_CARDS: usize = 5;

fn show_cards(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    let cards = std::mem::take(&mut state.ai.cards);
    egui::CollapsingHeader::new(format!("检索到的邮件（{}）", cards.len()))
        .id_salt("ai_email_cards")
        .default_open(true)
        .show(ui, |ui| {
            let mut open: Option<MessageId> = None;
            for card in cards.iter().take(MAX_CARDS) {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label(RichText::new(truncate_chars(&card.subject, 50)).strong());
                    let date = card
                        .date
                        .map(me_core::format_local_datetime)
                        .unwrap_or_default();
                    ui.label(
                        RichText::new(format!("{} · {date}", card.from))
                            .small()
                            .color(Color32::GRAY),
                    );
                    ui.label(RichText::new(truncate_chars(&card.snippet, 120)).small());
                    // 命中理由：让用户知道这封为什么被挑出来，而不是只有模型知道。
                    ui.label(
                        RichText::new(truncate_chars(&card.reason, 60))
                            .small()
                            .color(Color32::from_rgb(0x3B, 0x82, 0xF6)),
                    );
                    if ui.small_button("打开邮件").clicked() {
                        open = Some(card.message);
                    }
                });
            }
            if cards.len() > MAX_CARDS {
                let more = cards.len() - MAX_CARDS;
                ui.label(
                    RichText::new(format!("还有 {more} 封，去邮件列表看"))
                        .small()
                        .color(Color32::GRAY),
                );
            }
            if let Some(id) = open {
                // **不切页**：阅读窗在 AI 页上原地弹出，关掉之后仍然停在 AI 页。
                state.selected_message = Some(id);
                state.reader_open = true;
                state.open_body = None;
                state.body_loading = true;
                commands.push(Command::OpenMessage(id));
            }
        });
    state.ai.cards = cards;
}

fn show_input(
    state: &mut UiState,
    commands: &mut Vec<Command>,
    ui: &mut egui::Ui,
    conversation: AiConversationId,
) {
    if let Some(event) = state.ai.last_calendar_event {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("AI 创建了一条日程")
                    .small()
                    .color(Color32::GRAY),
            );
            if ui.small_button("撤销").clicked() {
                commands.push(Command::UndoAiCalendarCreate { event });
                state.ai.last_calendar_event = None;
            }
        });
    }
    // 删除后的 10 秒撤销窗口。到期就丢掉，不做「回收站」。
    if let Some(deleted) = state.ai.last_deleted.clone() {
        if deleted.at.elapsed() < UNDO_WINDOW {
            let mut undo = false;
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("已删除「{}」", truncate_chars(&deleted.title, 20)))
                        .small()
                        .color(Color32::GRAY),
                );
                if ui.small_button("撤销").clicked() {
                    undo = true;
                }
            });
            if undo {
                commands.push(Command::RestoreCalendarEvent { id: deleted.event });
                state.ai.last_deleted = None;
            }
            // 撤销条自己会过期，界面得在窗口内持续重绘。
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(500));
        } else {
            state.ai.last_deleted = None;
        }
    }
    if !state.ai.pending.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("引用：").small().color(Color32::GRAY));
            let mut remove: Option<usize> = None;
            for (index, chip) in state.ai.pending.iter().enumerate() {
                if ui
                    .small_button(format!("{} ✕", truncate_chars(&chip.subject, 20)))
                    .on_hover_text("点击移除这封引用")
                    .clicked()
                {
                    remove = Some(index);
                }
            }
            if let Some(index) = remove {
                state.ai.pending.remove(index);
            }
        });
    }

    let streaming = state.ai.is_streaming();
    ui.add_space(4.0);
    ui.add_enabled_ui(!streaming, |ui| {
        let response = ui.add(
            egui::TextEdit::multiline(&mut state.ai.input)
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .hint_text("输入问题，Ctrl+Enter 发送"),
        );
        let send_shortcut =
            ui.input(|input| input.key_pressed(egui::Key::Enter) && input.modifiers.command);
        if response.has_focus() && send_shortcut {
            send(state, commands, conversation);
        }
    });

    ui.horizontal(|ui| {
        let can_send = !streaming && !state.ai.input.trim().is_empty();
        if ui
            .add_enabled(can_send, egui::Button::new("发送"))
            .clicked()
        {
            send(state, commands, conversation);
        }
        if streaming {
            ui.label(
                RichText::new("生成中…可以点上方「停止」")
                    .small()
                    .color(Color32::GRAY),
            );
        } else {
            // 用这个会话实际会走的模型判断隐私确认，而不是「当前模型」：
            // 用户可能在对话里切到了另一个协议的模型。
            let provider = state
                .ai
                .selected_conversation()
                .and_then(|conv| state.ai.model_for_conversation(conv))
                .map(|model| model.kind)
                .unwrap_or(AiProviderKind::Anthropic);
            if !state.ai.privacy_acked(provider) {
                ui.label(
                    RichText::new("首次使用需在设置中确认隐私说明")
                        .small()
                        .color(Color32::from_rgb(200, 140, 0)),
                );
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new("AI 生成内容可能不准确，请自行核实")
                    .small()
                    .color(Color32::GRAY),
            );
        });
    });
}

fn send(state: &mut UiState, commands: &mut Vec<Command>, conversation: AiConversationId) {
    let content = state.ai.input.trim().to_string();
    if content.is_empty() {
        return;
    }
    let context: Vec<AiContextRequest> = state
        .ai
        .pending
        .iter()
        .map(|chip| chip.request.clone())
        .collect();
    state.ai.input.clear();
    state.ai.pending.clear();
    state.ai.activity.clear();
    state.ai.cards.clear();
    commands.push(Command::SendAiMessage {
        conversation,
        content,
        context,
    });
}

/// 服务商设置区块。设置页与 AI 侧栏共用同一份 UI。
/// 设置页：模型档案列表 + 编辑表单 + 隐私确认 + 记忆。
pub fn settings_section(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("模型").strong());
        if ui.button("＋ 新建模型").clicked() {
            state.ai.start_new_model();
        }
    });
    ui.label(
        RichText::new("每个模型是一套独立的调用参数；对话里可以随时切。名称不能重复。")
            .small()
            .color(Color32::GRAY),
    );

    if state.ai.models.is_empty() && state.ai.draft.is_none() {
        ui.add_space(4.0);
        ui.label(
            RichText::new("还没有模型。点「＋ 新建模型」填名称、端点和 Key。")
                .small()
                .color(Color32::GRAY),
        );
    }

    // 列表用 take 换出：点击回调里要 `&mut state`，直接迭代会一直借用 models。
    let models = std::mem::take(&mut state.ai.models);
    let active = state.ai.active_model.clone();
    let editing_id = state.ai.draft.as_ref().map(|draft| draft.id.clone());
    let mut pick: Option<String> = None;
    for model in &models {
        let editing = editing_id.as_deref() == Some(model.id.as_str());
        ui.horizontal(|ui| {
            if ui
                .selectable_label(editing, format!("{} · {}", model.kind.label(), model.name))
                .clicked()
            {
                pick = Some(model.id.clone());
            }
            if active.as_deref() == Some(model.id.as_str()) {
                ui.label(RichText::new("当前").small().color(Color32::GREEN));
            }
            if !model.has_key() {
                ui.label(RichText::new("未配置 Key").small().color(Color32::GRAY));
            }
        });
    }
    state.ai.models = models;
    if let Some(id) = pick {
        state.ai.edit_model(&id);
    }

    if state.ai.draft.is_some() {
        ui.separator();
        show_model_form(state, commands, ui);
    }

    ui.separator();
    // 隐私确认按正在编辑（或当前）模型的协议判断：同协议换模型不必再确认。
    let provider = state
        .ai
        .draft
        .as_ref()
        .map(|draft| draft.kind)
        .or_else(|| {
            state
                .ai
                .active_model
                .as_deref()
                .and_then(|id| state.ai.model_by_id(id))
                .map(|model| model.kind)
        })
        .unwrap_or(AiProviderKind::Anthropic);
    show_privacy(state, commands, ui, provider);
    ui.separator();
    show_memory(state, commands, ui);
    ui.separator();
    show_conversation_maintenance(state, commands, ui);
}

/// 设置里的「对话历史」区：清空全部对话（带二次确认）。
///
/// 单条删除在对话标题栏；这里是唯一会一次删掉全部记录的地方，所以要两道手：
/// 先点按钮进入确认态，再点「确认清空」才真的发命令。
fn show_conversation_maintenance(
    state: &mut UiState,
    commands: &mut Vec<Command>,
    ui: &mut egui::Ui,
) {
    ui.label(RichText::new("对话历史").strong());
    ui.label(
        RichText::new("清空会删除全部对话与消息（含工具活动摘要），不可恢复。")
            .small()
            .color(Color32::GRAY),
    );
    if state.ai.confirm_clear_conversations {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("确定清空全部对话？")
                    .small()
                    .color(Color32::from_rgb(220, 80, 80)),
            );
            if ui.button("确认清空").clicked() {
                commands.push(Command::ClearAiConversations);
                state.ai.confirm_clear_conversations = false;
                state.select_ai_conversation(None);
            }
            if ui.button("取消").clicked() {
                state.ai.confirm_clear_conversations = false;
            }
        });
    } else if ui.button("清空全部对话").clicked() {
        state.ai.confirm_clear_conversations = true;
    }
}

/// 模型表单。草稿先取出再渲染，避免同时借用 `state.ai.draft` 与 `state.ai.models`。
///
/// **「新建模型」只有五项**：名称、协议、端点、模型实际名称、Key。
/// 工具开关与长度预算留默认值，保存后再从列表点进「编辑模型」调——新建时列一堆
/// 可选项只会把人挡在门外。已落库的档案仍显示完整参数。
fn show_model_form(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    let Some(mut draft) = state.ai.draft.take() else {
        return;
    };
    let is_new = !state.ai.models.iter().any(|model| model.id == draft.id);

    ui.label(
        RichText::new(if is_new {
            "新建模型"
        } else {
            "编辑模型"
        })
        .strong()
        .size(14.0),
    );
    ui.add_space(2.0);

    // 两列网格：第一列标签自动同宽，输入框左边线自然对齐。
    egui::Grid::new("ai_model_fields")
        .num_columns(2)
        .spacing([10.0, 8.0])
        .show(ui, |ui| {
            ui.label("模型名称");
            ui.add(
                egui::TextEdit::singleline(&mut draft.name)
                    .desired_width(360.0)
                    .hint_text("必填，无法重名"),
            );
            ui.end_row();

            ui.label("协议");
            let mut kind = draft.kind;
            egui::ComboBox::from_id_salt("ai_model_kind")
                .selected_text(draft.kind.label())
                .width(200.0)
                .show_ui(ui, |ui| {
                    for candidate in [AiProviderKind::Anthropic, AiProviderKind::OpenAiCompatible] {
                        ui.selectable_value(&mut kind, candidate, candidate.label());
                    }
                });
            if kind != draft.kind {
                switch_kind(&mut draft, kind);
            }
            ui.end_row();

            let default_url = draft.kind.default_base_url();
            ui.label("URL 端点");
            ui.add(
                egui::TextEdit::singleline(&mut draft.base_url)
                    .desired_width(360.0)
                    .hint_text(default_url),
            );
            ui.end_row();

            ui.label("模型实际名称");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut draft.model)
                        .desired_width(360.0)
                        .hint_text("发给服务商的 model id"),
                );
                // 预置列表只在「编辑模型」里露出：新建时按约定只留五个字段。
                if !is_new {
                    let presets: Vec<&'static str> = draft.kind.preset_models().to_vec();
                    egui::ComboBox::from_id_salt("ai_preset_model")
                        .selected_text("预置")
                        .width(110.0)
                        .show_ui(ui, |ui| {
                            for preset in &presets {
                                if ui.selectable_label(false, *preset).clicked() {
                                    draft.model = (*preset).to_string();
                                }
                            }
                        });
                }
            });
            ui.end_row();

            ui.label("APIKey");
            ui.add(
                egui::TextEdit::singleline(&mut draft.api_key)
                    .password(true)
                    .desired_width(360.0)
                    .hint_text("粘贴 API Key，与其它参数一起保存"),
            );
            ui.end_row();
        });

    if is_new {
        show_new_model_actions(state, commands, draft.clone(), ui);
        return;
    }

    // ---- 工具开关 ----
    ui.separator();
    ui.label(RichText::new("工具开关").strong());
    // 按能力模块分组只是**渲染层**的事：数据结构保持扁平 9 个 bool，
    // 老档案因此零迁移（见 `doc/AI模块落地方案.md` §4.2）。
    // 分两行而不是五行：设置弹窗高度有限，多出来的行会把下面的预算/删除挤出可视区。
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("邮件").small().color(Color32::GRAY));
        ui.checkbox(&mut draft.tools.email_search, "检索");
        ui.checkbox(&mut draft.tools.mail_read, "读正文");
        ui.separator();
        ui.label(RichText::new("天气").small().color(Color32::GRAY));
        ui.checkbox(&mut draft.tools.weather, "天气与地点");
    });
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("日历").small().color(Color32::GRAY));
        ui.checkbox(&mut draft.tools.calendar_read, "查看");
        ui.checkbox(&mut draft.tools.calendar_create, "创建");
        ui.checkbox(&mut draft.tools.calendar_update, "修改");
        ui.checkbox(&mut draft.tools.calendar_delete, "删除");
        ui.separator();
        ui.checkbox(&mut draft.tools.music, "音乐");
        ui.checkbox(&mut draft.tools.memory, "记忆");
    });
    ui.label(
        RichText::new("关掉的工具不会出现在请求里；删除日程即使是开着，也要你再确认一次。")
            .small()
            .color(Color32::GRAY),
    );

    // ---- 预算 ----
    ui.separator();
    ui.horizontal(|ui| {
        ui.label("最长输入");
        ui.add(
            egui::TextEdit::singleline(&mut draft.max_input)
                .desired_width(84.0)
                .hint_text("48000"),
        );
        ui.label(
            RichText::new("字符（历史+本次，超出从最早一轮丢）")
                .small()
                .color(Color32::GRAY),
        );
    });
    ui.horizontal(|ui| {
        ui.label("最长输出");
        ui.add(
            egui::TextEdit::singleline(&mut draft.max_output)
                .desired_width(84.0)
                .hint_text("4096"),
        );
        ui.label(
            RichText::new("tokens（1～8192）")
                .small()
                .color(Color32::GRAY),
        );
    });

    // ---- 操作 ----
    ui.separator();
    let mut deleted = false;
    ui.horizontal(|ui| {
        if ui.button("保存").clicked() {
            save_draft(state, commands, &draft);
        }
        if ui.button("测试连接").clicked() {
            state.ai.test_result = None;
            commands.push(Command::TestAiModel {
                model: draft.id.clone(),
            });
        }
        let is_active = state.ai.active_model.as_deref() == Some(draft.id.as_str());
        if !is_active && ui.button("设为当前").clicked() {
            commands.push(Command::SetActiveAiModel {
                id: draft.id.clone(),
            });
        }
        if ui.button("删除").clicked() {
            commands.push(Command::DeleteAiModel {
                id: draft.id.clone(),
            });
            deleted = true;
        }
        match &state.ai.test_result {
            Some((true, detail)) => {
                ui.label(
                    RichText::new(format!("连接正常 {detail}"))
                        .small()
                        .color(Color32::GREEN),
                );
            }
            Some((false, detail)) => {
                ui.label(RichText::new(detail.clone()).small().color(Color32::RED));
            }
            None => {}
        }
    });
    show_form_error(state, ui);

    if !deleted {
        state.ai.draft = Some(draft);
    }
}

/// 「新建模型」只有五项，所以按钮也只留保存 / 取消。
///
/// 成功保存后收起表单：列表里立刻能看到新档案，需要调工具或预算再点进去。
fn show_new_model_actions(
    state: &mut UiState,
    commands: &mut Vec<Command>,
    draft: AiModelDraft,
    ui: &mut egui::Ui,
) {
    ui.separator();
    let mut saved = false;
    let mut cancelled = false;
    ui.horizontal(|ui| {
        if ui.button("保存").clicked() {
            save_draft(state, commands, &draft);
            saved = state.ai.form_error.is_none();
        }
        if ui.button("取消").clicked() {
            cancelled = true;
        }
    });
    show_form_error(state, ui);

    if cancelled {
        state.ai.form_error = None;
        return;
    }
    if !saved {
        state.ai.draft = Some(draft);
    }
}

/// 保存前先在本地校验一次（名称必填/不重名、端点合法、Key 形态），
/// 免得乐观更新列表后又要等后端回包才发现没存上。
fn save_draft(state: &mut UiState, commands: &mut Vec<Command>, draft: &AiModelDraft) {
    let model = draft.to_model();
    let others: Vec<AiModel> = state
        .ai
        .models
        .iter()
        .filter(|item| item.id != model.id)
        .cloned()
        .collect();
    if let Err(err) = me_core::ai::validate_model(&model, &others) {
        state.ai.form_error = Some(err.user_message());
        return;
    }
    state.ai.form_error = None;
    commands.push(Command::SaveAiModel {
        model: model.clone(),
    });
    // 本地先更新列表：事件要过一趟消息循环，慢一帧列表就是空的。
    match state.ai.models.iter_mut().find(|item| item.id == model.id) {
        Some(slot) => *slot = model,
        None => state.ai.models.push(model),
    }
}

fn show_form_error(state: &UiState, ui: &mut egui::Ui) {
    let Some(error) = &state.ai.form_error else {
        return;
    };
    ui.label(RichText::new(error.clone()).small().color(Color32::RED));
}

/// 换协议时，端点和模型名还停在旧协议默认值上的才跟着改；用户手填过的不动。
fn switch_kind(draft: &mut AiModelDraft, kind: AiProviderKind) {
    let before = draft.kind;
    draft.kind = kind;
    let old_default = before.default_base_url();
    if draft.base_url.trim().is_empty() || draft.base_url.trim() == old_default {
        draft.base_url = kind.default_base_url().to_string();
    }
    let old_model = draft.model.trim().to_string();
    let still_preset = before
        .preset_models()
        .iter()
        .any(|preset| *preset == old_model);
    if old_model.is_empty() || still_preset {
        draft.model = kind
            .preset_models()
            .first()
            .copied()
            .unwrap_or_default()
            .to_string();
    }
}

/// 隐私确认：按协议记，换同协议的模型不必重新确认。
fn show_privacy(
    state: &mut UiState,
    commands: &mut Vec<Command>,
    ui: &mut egui::Ui,
    provider: AiProviderKind,
) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("隐私确认").strong());
        if state.ai.privacy_acked(provider) {
            ui.label(
                RichText::new(format!("{} 已确认", provider.label()))
                    .small()
                    .color(Color32::GREEN),
            );
        } else if ui.button("我已了解，继续使用").clicked() {
            commands.push(Command::SetAiPrivacyAck {
                provider,
                accepted: true,
            });
        }
    });
    ui.label(
        RichText::new(
            "AI 可能调用邮件检索，匹配邮件的主题/发件人/摘要片段会发送给服务商；\
不会发送附件或完整正文。可在模型里单独关闭某个工具。",
        )
        .small()
        .color(Color32::GRAY),
    );
}

/// 记忆管理：实体注册表的查看、删除与清空。
fn show_memory(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("记忆（{} 条）", state.ai.entity_count)).strong());
        if ui.small_button("刷新").clicked() {
            state.ai_entities_dirty = true;
        }
        if state.ai.entity_count > 0 && ui.small_button("清空").clicked() {
            commands.push(Command::ClearAiEntities);
        }
    });
    for entity in state.ai.entities.iter().take(20) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "{}（{}）{}",
                    truncate_chars(&entity.canonical_name, 20),
                    entity.kind.label(),
                    entity
                        .email
                        .as_deref()
                        .map(|email| format!(" · {email}"))
                        .unwrap_or_default()
                ))
                .small(),
            );
            if ui.small_button("删除").clicked() {
                commands.push(Command::DeleteAiEntity { entity: entity.id });
            }
        });
    }
    if state.ai.entity_count as usize > state.ai.entities.len().min(20) {
        ui.label(
            RichText::new("仅显示最近 20 条")
                .small()
                .color(Color32::GRAY),
        );
    }
}

/// 阅读页「引用到 AI 对话」的落点：把邮件加入 chip 并切到 AI 页。
pub fn attach_context(state: &mut UiState, message: MessageId, subject: &str) {
    if state
        .ai
        .pending
        .iter()
        .any(|chip| chip.request.message == message)
    {
        state.page = Page::AiChat;
        return;
    }
    state.ai.pending.push(PendingContext {
        subject: subject.to_string(),
        request: AiContextRequest {
            message,
            mode: AiContextMode::FullBody,
            selection: None,
        },
    });
    state.page = Page::AiChat;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation(id: i64) -> AiConversation {
        AiConversation {
            id: AiConversationId(id),
            title: format!("对话{id}"),
            model_id: None,
            provider: AiProviderKind::Anthropic,
            model: "m".to_string(),
            created_at: 0,
            updated_at: 0,
            message_count: 0,
        }
    }

    fn model(id: &str, name: &str, kind: AiProviderKind) -> AiModel {
        AiModel::preset(kind, id.to_string(), name.to_string())
    }

    #[test]
    fn selecting_another_conversation_clears_transient_state() {
        let mut state = AiPageState {
            conversations: vec![conversation(1), conversation(2)],
            ..AiPageState::default()
        };
        state.select(Some(AiConversationId(1)));
        state.on_stream_started(AiConversationId(1));
        state.on_delta(AiConversationId(1), 1, "部分");
        state.cards.push(AiCard {
            message: MessageId(1),
            reason: "主题命中".to_string(),
            subject: "主题".to_string(),
            from: "a@example.com".to_string(),
            date: None,
            snippet: String::new(),
        });
        assert!(state.is_streaming());

        state.select(Some(AiConversationId(2)));
        assert!(!state.is_streaming(), "切会话必须丢掉上一条流");
        assert!(state.cards.is_empty(), "邮件卡片属于上一轮对话");
        assert!(state.messages.is_empty());
    }

    #[test]
    fn deltas_from_another_conversation_are_ignored() {
        let mut state = AiPageState::default();
        state.select(Some(AiConversationId(1)));
        state.on_stream_started(AiConversationId(1));
        state.on_delta(AiConversationId(1), 1, "你好");
        state.on_delta(AiConversationId(2), 2, "不该出现");
        let stream = state.stream.expect("流仍在");
        assert_eq!(stream.buffer, "你好");
        assert_eq!(stream.seq, 1);
    }

    #[test]
    fn config_snapshot_does_not_clobber_an_open_draft() {
        let mut state = AiPageState::default();
        state.apply_config(
            vec![model("m1", "本地", AiProviderKind::OpenAiCompatible)],
            Some("m1".to_string()),
            (true, false),
            7,
        );
        state.edit_model("m1");
        // 用户正在改名称，此时后端推来一份新快照（例如别处改了 Key）。
        state.draft.as_mut().unwrap().name = "改到一半的名称".to_string();
        state.apply_config(
            vec![model("m1", "本地", AiProviderKind::OpenAiCompatible)],
            Some("m1".to_string()),
            (true, false),
            9,
        );
        let draft = state.draft.as_ref().expect("草稿不能被关掉");
        assert_eq!(draft.name, "改到一半的名称");
        assert_eq!(state.entity_count, 9);
        assert!(state.privacy_acked(AiProviderKind::Anthropic));
        assert!(!state.privacy_acked(AiProviderKind::OpenAiCompatible));
    }

    #[test]
    fn deleting_the_edited_model_closes_the_draft() {
        let mut state = AiPageState::default();
        state.apply_config(
            vec![
                model("m1", "A", AiProviderKind::Anthropic),
                model("m2", "B", AiProviderKind::Anthropic),
            ],
            Some("m1".to_string()),
            (false, false),
            0,
        );
        state.edit_model("m2");
        state.apply_config(
            vec![model("m1", "A", AiProviderKind::Anthropic)],
            Some("m1".to_string()),
            (false, false),
            0,
        );
        assert!(state.draft.is_none(), "档案被删后必须退出编辑态");
    }

    #[test]
    fn new_model_ids_do_not_collide() {
        let mut state = AiPageState::default();
        state.apply_config(
            vec![model("m1", "A", AiProviderKind::Anthropic)],
            Some("m1".to_string()),
            (false, false),
            0,
        );
        state.start_new_model();
        assert_eq!(state.draft.as_ref().unwrap().id, "m2");
        // 未落库的草稿不占位：再点一次「新建」仍得到同一个 id 是正常的。
        state.start_new_model();
        assert_eq!(state.draft.as_ref().unwrap().id, "m2");
        state.draft.as_mut().unwrap().id = "m3".to_string();
        state.start_new_model();
        assert_eq!(state.draft.as_ref().unwrap().id, "m2");
    }

    #[test]
    fn conversation_falls_back_to_active_model() {
        let state = AiPageState {
            models: vec![model("m1", "A", AiProviderKind::Anthropic)],
            active_model: Some("m1".to_string()),
            ..AiPageState::default()
        };
        let mut conv = conversation(1);
        conv.model_id = Some("gone".to_string());
        let picked = state.model_for_conversation(&conv).expect("必须回退");
        assert_eq!(picked.id, "m1");
    }

    #[test]
    fn draft_round_trips_through_to_model() {
        let draft = AiModelDraft {
            id: "m4".to_string(),
            name: " 车间模型 ".to_string(),
            kind: AiProviderKind::OpenAiCompatible,
            base_url: " http://127.0.0.1:11434/v1 ".to_string(),
            model: " qwen ".to_string(),
            max_input: "60000".to_string(),
            max_output: "2048".to_string(),
            tools: AiToolPermissions {
                weather: false,
                ..AiToolPermissions::default()
            },
            api_key: String::new(),
        };
        let model = draft.to_model();
        assert_eq!(model.id, "m4");
        assert_eq!(model.name, "车间模型");
        assert_eq!(model.base_url, "http://127.0.0.1:11434/v1");
        assert_eq!(model.max_input_chars, 60_000);
        assert_eq!(model.max_output_tokens, 2_048);
        assert!(!model.tools.weather);
        assert_eq!(model.api_key, "", "key 随保存写入，草稿里没填就是空");
    }

    // ---- 渲染回归：设置是弹窗、新建表单只列五项 ----

    fn collect_texts(shape: &egui::Shape, out: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(text) => out.push(text.galley.text().to_string()),
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_texts(shape, out);
                }
            }
            _ => {}
        }
    }

    /// 跑一帧，收集所有绘制出来的文本。
    fn run_frame(mut body: impl FnMut(&egui::Context)) -> Vec<String> {
        let ctx = egui::Context::default();
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 900.0),
            )),
            ..Default::default()
        };
        // 第一帧建布局，第二帧才是稳定输出（弹窗要等字体与屏幕尺寸就绪）。
        let _ = ctx.run(input(), &mut body);
        let output = ctx.run(input(), body);
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_texts(&clipped.shape, &mut texts);
        }
        texts
    }

    fn user_message(content: &str) -> AiMessage {
        AiMessage {
            id: AiMessageId(1),
            conversation: AiConversationId(1),
            role: me_core::ai::AiRole::User,
            content: content.to_string(),
            context_refs: Vec::new(),
            status: AiMessageStatus::Complete,
            error: None,
            input_tokens: None,
            output_tokens: None,
            tool_trace: Vec::new(),
            plan: None,
            created_at: 0,
        }
    }

    /// 点「设置」不能把主区的对话顶掉：它只是浮在上面的弹窗。
    #[test]
    fn opening_settings_keeps_the_conversation_in_the_main_area() {
        let mut state = UiState::default();
        state.ai.conversations = vec![conversation(1)];
        state.select_ai_conversation(Some(AiConversationId(1)));
        state.ai.loading = false;
        state.ai.messages.push(user_message("上周的报销邮件"));

        state.ai.show_settings = true;
        let mut commands = Vec::new();
        let texts = run_frame(|ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(&mut state, &mut commands, ui);
            });
        });
        assert!(
            texts.iter().any(|text| text == "上周的报销邮件"),
            "设置打开时主区必须还是对话：{texts:?}"
        );
        assert!(
            !texts.iter().any(|text| text == "AI 设置"),
            "设置内容不该再占主区：{texts:?}"
        );
    }

    /// 新建模型只给五项：名称、协议、端点、模型实际名称、Key。
    #[test]
    fn new_model_form_shows_only_the_five_fields() {
        let mut state = UiState::default();
        state.ai.start_new_model();
        state.ai.show_settings = true;
        let mut commands = Vec::new();
        let texts = run_frame(|ctx| settings_modal(&mut state, &mut commands, ctx));

        for label in ["模型名称", "协议", "URL 端点", "模型实际名称", "APIKey"] {
            assert!(
                texts.iter().any(|text| text == label),
                "缺少字段「{label}」：{texts:?}"
            );
        }
        for extra in [
            "工具开关",
            "最长输入",
            "最长输出",
            "测试连接",
            "设为当前",
            "删除",
        ] {
            assert!(
                !texts.iter().any(|text| text == extra),
                "新建表单不该出现「{extra}」：{texts:?}"
            );
        }
    }

    /// 已经落库的档案仍要能看到全部参数（工具开关、预算、测试、删除）。
    #[test]
    fn editing_an_existing_model_still_shows_every_parameter() {
        let mut state = UiState::default();
        state.ai.apply_config(
            vec![model("m1", "甲", AiProviderKind::Anthropic)],
            Some("m1".to_string()),
            (true, true),
            0,
        );
        state.ai.edit_model("m1");
        state.ai.show_settings = true;
        let mut commands = Vec::new();
        let texts = run_frame(|ctx| settings_modal(&mut state, &mut commands, ctx));

        // 「设为当前」只在档案不是当前模型时出现，这里是当前模型，故不在断言里。
        for label in ["工具开关", "最长输入", "最长输出", "测试连接", "删除"] {
            assert!(
                texts.iter().any(|text| text == label),
                "编辑态缺少「{label}」：{texts:?}"
            );
        }
    }

    // ---- `[结果]` 段落切分 ----

    #[test]
    fn result_block_is_split_off_the_narrative() {
        let content = "已找到 2 封，最新一封是 11-04。\n\n[结果]\n状态：已完成\n做了什么：检索了本周邮件\n关键信息：2 封命中\n未完成：无";
        let (body, block) = split_result_block(content).expect("应当切出结果段");
        assert_eq!(body, "已找到 2 封，最新一封是 11-04。");
        assert_eq!(block.status, "已完成");
        assert_eq!(block.done, "检索了本周邮件");
        assert_eq!(block.key, "2 封命中");
        assert_eq!(block.todo, "无");
    }

    #[test]
    fn partial_result_marker_stays_in_the_body() {
        // 流式到一半：不能把正文切掉，也不能 panic。
        for content in [
            "已找到 2 封\n[结",
            "已找到 2 封\n[结果",
            "已找到 2 封\n[结果]\n状态：",
        ] {
            match split_result_block(content) {
                Some((body, block)) => {
                    assert!(content.contains(body) || body.is_empty());
                    assert!(!block.status.is_empty(), "缺字段要填空值而不是丢掉整段");
                }
                None => assert!(!content.contains("状态：")),
            }
        }
    }

    #[test]
    fn result_marker_inside_code_fence_is_ignored() {
        let content = "示例：\n```text\n[结果]\n状态：已完成\n```\n就这样。";
        assert!(
            split_result_block(content).is_none(),
            "代码块里的 [结果] 不是真段落"
        );
    }

    #[test]
    fn result_block_without_any_known_field_is_not_treated_as_result() {
        let content = "先说结论。\n[结果]\n这里只是随便写写";
        assert!(split_result_block(content).is_none(), "认不出字段就当正文");
    }

    #[test]
    fn result_block_fills_missing_fields_with_dash() {
        let content = "正文\n[结果]\n状态：部分完成";
        let (_, block) = split_result_block(content).expect("有状态就够");
        assert_eq!(block.done, "—");
        assert_eq!(block.key, "—");
        assert_eq!(block.todo, "—");
    }
}
