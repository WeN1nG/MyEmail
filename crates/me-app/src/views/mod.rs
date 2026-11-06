//! 视图框架：页面导航、共享控件、整体布局。

pub mod accounts;
pub mod ai;
pub mod calendar;
pub mod compose;
pub mod dialogs;
pub mod html;
pub mod mailbox;
pub mod music;
pub mod reader;
pub mod richtext;
pub mod settings;
pub mod weather;

use eframe::egui::{self, Align, Align2, Color32, Id, Layout, RichText};

use me_core::{
    AccountId, Command, ConnectionState, Folder, FolderRole, MessageSummary, RealtimeMode,
};

use crate::state::{Dialog, Page, ToastKind, UiState};

// 重新导出避免上层直接依赖 egui。

pub fn top_bar(state: &mut UiState, commands: &mut Vec<Command>, ctx: &egui::Context) {
    egui::TopBottomPanel::top("me_top_bar").show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.heading(RichText::new("MyEmail").strong());
            ui.separator();
            for page in [
                Page::Mail,
                Page::Calendar,
                Page::Weather,
                Page::AiChat,
                Page::Music,
                Page::Accounts,
                Page::Settings,
            ] {
                if ui
                    .selectable_label(state.page == page, page.label())
                    .clicked()
                {
                    switch_page(state, page);
                }
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("同步全部").clicked() {
                    for account in &state.accounts {
                        commands.push(Command::SyncAccount(account.id));
                    }
                }
                if let Some(account) = state.selected_account {
                    let connection = state
                        .connection
                        .get(&account)
                        .cloned()
                        .unwrap_or(ConnectionState::Disconnected);
                    status_chip(ui, &connection);
                    if let Some(progress) = state.sync_progress {
                        let total = progress
                            .total
                            .map(|t| format!("/{t}"))
                            .unwrap_or_else(|| "/?".to_string());
                        let folder_name = state
                            .folder(progress.folder)
                            .map(|f| f.name.clone())
                            .unwrap_or_else(|| "文件夹".to_string());
                        ui.label(
                            RichText::new(format!("同步中 {folder_name} {}{total}", progress.done))
                                .small()
                                .color(Color32::GRAY),
                        );
                        if ui.small_button("取消").clicked() {
                            commands.push(Command::CancelSync(account));
                        }
                    } else if let Some(progress) = state.index_progress
                        && progress.total > 0
                    {
                        let email = state
                            .account(progress.account)
                            .map(|a| a.email.clone())
                            .unwrap_or_default();
                        ui.label(
                            RichText::new(format!(
                                "补全正文 {email} {}/{}",
                                progress.indexed, progress.total
                            ))
                            .small()
                            .color(Color32::GRAY),
                        );
                    }
                    if let Some(account) = state.account(account) {
                        ui.label(RichText::new(&account.email).small());
                        realtime_chip(ui, state, account.id);
                    }
                }
            });
        });

        if state.is_offline() && !state.accounts.is_empty() {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("离线：正在使用本地缓存")
                        .small()
                        .color(Color32::from_rgb(180, 120, 0)),
                );
            });
        }

        if state.disk_low {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("磁盘空间不足：后台正文补全已暂停")
                        .small()
                        .color(Color32::from_rgb(200, 90, 60)),
                );
                if ui.small_button("知道了").clicked() {
                    state.disk_low = false;
                }
            });
        }

        if let Some(error) = state.last_error.clone() {
            let mut dismiss = false;
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(&error.message)
                        .small()
                        .color(Color32::LIGHT_RED),
                );
                if ui.small_button("详情").clicked()
                    && let Some(detail) = &error.detail
                {
                    ui.ctx().copy_text(format!(
                        "{:?}
{detail}",
                        error.scope
                    ));
                }
                if ui.small_button("关闭").clicked() {
                    dismiss = true;
                }
            });
            if dismiss {
                state.last_error = None;
            }
        }
    });
}

/// 实时同步状态（IDLE / 轮询）。只在开启时显示。
fn realtime_chip(ui: &mut egui::Ui, state: &UiState, account: AccountId) {
    let Some(realtime) = state.realtime.get(&account) else {
        return;
    };
    let (text, color) = match (realtime.mode, realtime.connected) {
        (RealtimeMode::Idle, true) => ("实时", Color32::from_rgb(60, 170, 90)),
        (RealtimeMode::Idle, false) => ("实时断开", Color32::from_rgb(200, 140, 40)),
        (RealtimeMode::Polling, _) => ("轮询", Color32::from_rgb(60, 140, 220)),
        (RealtimeMode::Off, _) => return,
    };
    let hover = realtime
        .last_event_at
        .map(|timestamp| {
            format!(
                "最近实时事件：{}",
                me_core::format_local_datetime(timestamp)
            )
        })
        .unwrap_or_else(|| "暂无实时事件".to_string());
    ui.label(RichText::new(format!("● {text}")).small().color(color))
        .on_hover_text(hover);
}

fn status_chip(ui: &mut egui::Ui, state: &ConnectionState) {
    let (text, color) = match state {
        ConnectionState::Disconnected => ("未连接", Color32::GRAY),
        ConnectionState::Connecting => ("连接中", Color32::from_rgb(200, 170, 40)),
        ConnectionState::Online => ("在线", Color32::from_rgb(60, 170, 90)),
        ConnectionState::Syncing => ("同步中", Color32::from_rgb(60, 140, 220)),
        ConnectionState::Backoff { .. } => ("稍后重试", Color32::from_rgb(200, 140, 40)),
        ConnectionState::Error { .. } => ("错误", Color32::from_rgb(210, 80, 80)),
    };
    ui.label(RichText::new(format!("● {text}")).small().color(color));
}

/// 左侧导航 + 内容（邮件页：账户与文件夹树；日历页：日历列表）。
pub fn folder_sidebar(state: &mut UiState, commands: &mut Vec<Command>, ctx: &egui::Context) {
    egui::SidePanel::left("me_folders")
        .resizable(true)
        .default_width(220.0)
        .min_width(160.0)
        .show(ctx, |ui| {
            ui.add_space(6.0);

            if state.page == Page::Calendar {
                calendar::sidebar(state, commands, ui);
                return;
            }
            if state.page == Page::Weather {
                // 天气页的侧栏放城市搜索与地点列表。
                weather::sidebar(state, commands, ui);
                return;
            }
            if state.page == Page::AiChat {
                // AI 页的侧栏放会话列表与服务商设置入口。
                ai::sidebar(state, commands, ui);
                return;
            }
            if state.page == Page::Music {
                // 音乐页的侧栏放播放列表（主区留给专辑页与频谱）。
                music::sidebar(state, commands, ui);
                return;
            }

            account_selector(state, ui);
            ui.separator();

            // 统一收件箱：**视图层的虚拟项**，不是数据库里的文件夹行。
            // 放在「还没选账户」的检查**之前**——它跨账户，
            // 正是「还没选定单个账户」时最该用的视图。
            // 一个账户都没有时不显示：聚合视图没有来源，点了只会是空的。
            if !state.accounts.is_empty() {
                unified_inbox_row(state, ui);
                ui.separator();
            }

            let Some(account) = state.selected_account else {
                ui.add_space(8.0);
                ui.label(RichText::new("还没有账户").color(Color32::GRAY));
                if ui.button("添加账户").clicked() {
                    state.page = Page::Accounts;
                    state.dialog = Dialog::AccountEditor(crate::state::AccountEditor::new());
                }
                return;
            };

            let folders = state.sorted_folders(account);
            egui::ScrollArea::vertical()
                .id_salt("folder_tree")
                .show(ui, |ui| {
                    for folder in &folders {
                        folder_row(state, commands, ui, folder);
                    }
                });
        });
}

/// 侧栏的「统一收件箱」行（视图层虚拟项）。
///
/// **不是数据库里的文件夹行**——`folders.role` 有 CHECK 约束且
/// `account_id NOT NULL`，插不进一行「属于所有账户的收件箱」。
/// 聚合范围是各账户 `role = 'inbox'` 的文件夹的并集。
fn unified_inbox_row(state: &mut UiState, ui: &mut egui::Ui) {
    let selected = state.unified_inbox;
    let unread = state.unified_unread();
    ui.horizontal(|ui| {
        let response = ui.selectable_label(selected, RichText::new("📥 统一收件箱"));
        if response.clicked() {
            // **走集中方法**——互斥关系只在这里维护。
            state.select_unified_inbox();
            state.search.active = false;
            state.messages.clear();
            state.selected_message = None;
            state.open_body = None;
            state.clear_multi_selection();
            state.messages_dirty = true;
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if unread > 0 {
                ui.label(
                    RichText::new(unread.to_string())
                        .small()
                        .strong()
                        .color(Color32::from_rgb(60, 140, 220)),
                );
            }
        });
    });
}

/// 切换页面时置脏，让下一次刷新拉取该页数据。
pub fn switch_page(state: &mut UiState, page: Page) {
    if state.page == page {
        return;
    }
    state.page = page;
    // 切页收起阅读窗：`selected_message` 仍然保留，方便回到邮件页接着看。
    state.reader_open = false;
    if page == Page::Calendar {
        state.calendar.dirty = true;
    }
    if page == Page::Weather {
        state.weather.dirty = true;
    }
    if page == Page::AiChat {
        state.ai_conversations_dirty = true;
        state.ai_messages_dirty = true;
        state.ai_entities_dirty = true;
    } else {
        // 设置是 AI 页的弹窗，离开页面就收掉，避免下次回来突然压在对话上。
        state.ai.show_settings = false;
    }
}

fn account_selector(state: &mut UiState, ui: &mut egui::Ui) {
    let current = state
        .selected_account
        .and_then(|id| state.account(id))
        .map(|a| a.email.clone())
        .unwrap_or_else(|| "选择账户".to_string());

    let mut changed: Option<Option<me_core::AccountId>> = None;
    egui::ComboBox::from_id_salt("account_selector")
        .selected_text(current)
        .width(ui.available_width() - 10.0)
        .show_ui(ui, |ui| {
            for account in &state.accounts {
                if ui
                    .selectable_label(Some(account.id) == state.selected_account, &account.email)
                    .clicked()
                {
                    changed = Some(Some(account.id));
                }
            }
            ui.separator();
            if ui.selectable_label(false, "＋ 添加账户…").clicked() {
                state.dialog = Dialog::AccountEditor(crate::state::AccountEditor::new());
            }
        });

    if let Some(new_account) = changed {
        state.selected_account = new_account;
        state.clear_folder_selection();
        state.messages.clear();
        state.selected_message = None;
        state.open_body = None;
        state.folders_dirty = true;
        state.messages_dirty = true;
    }
}

fn folder_row(
    state: &mut UiState,
    commands: &mut Vec<Command>,
    ui: &mut egui::Ui,
    folder: &Folder,
) {
    let selected = state.selected_folder == Some(folder.id);
    let unread = state.unread_of(folder.id);
    let label = format!("{} {}", role_icon(folder.role), folder.name);
    let mut text = RichText::new(label);
    if folder.deleted_on_server {
        text = text.color(Color32::GRAY).italics();
    }

    ui.horizontal(|ui| {
        let response = ui.selectable_label(selected, text);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if unread > 0 {
                ui.label(
                    RichText::new(unread.to_string())
                        .small()
                        .strong()
                        .color(Color32::from_rgb(60, 140, 220)),
                );
            }
        });

        if response.clicked() {
            state.select_folder(folder.id);
            state.search.active = false;
            state.messages.clear();
            state.selected_message = None;
            state.open_body = None;
            state.messages_dirty = true;
        }
        response.context_menu(|ui| {
            if ui.button("设为收件箱").clicked() {
                commands.push(Command::SetFolderRole {
                    folder: folder.id,
                    role: FolderRole::Inbox,
                });
                ui.close();
            }
            if ui.button("设为已发送").clicked() {
                commands.push(Command::SetFolderRole {
                    folder: folder.id,
                    role: FolderRole::Sent,
                });
                ui.close();
            }
            if ui.button("设为草稿箱").clicked() {
                commands.push(Command::SetFolderRole {
                    folder: folder.id,
                    role: FolderRole::Drafts,
                });
                ui.close();
            }
            if ui.button("设为废件箱").clicked() {
                commands.push(Command::SetFolderRole {
                    folder: folder.id,
                    role: FolderRole::Trash,
                });
                ui.close();
            }
            if ui.button("设为垃圾邮件").clicked() {
                commands.push(Command::SetFolderRole {
                    folder: folder.id,
                    role: FolderRole::Junk,
                });
                ui.close();
            }
            if folder.deleted_on_server {
                ui.separator();
                if ui.button("确认删除本地缓存").clicked() {
                    commands.push(Command::AcknowledgeFolderRemoval(folder.id));
                    ui.close();
                }
            }
        });
    });
}

pub fn role_icon(role: FolderRole) -> &'static str {
    match role {
        FolderRole::Inbox => "📥",
        FolderRole::Sent => "📤",
        FolderRole::Drafts => "📝",
        FolderRole::Trash => "🗑",
        FolderRole::Junk => "⚠",
        FolderRole::Archive => "🗄",
        FolderRole::Other => "📁",
    }
}

/// 右侧浮动提示。
pub fn toasts(state: &mut UiState, ctx: &egui::Context) {
    if state.toasts.is_empty() {
        return;
    }
    egui::Area::new(Id::new("me_toasts"))
        .anchor(Align2::RIGHT_BOTTOM, egui::vec2(-18.0, -18.0))
        .interactable(false)
        .show(ctx, |ui| {
            for toast in state.toasts.iter().rev().take(5) {
                let color = match toast.kind {
                    ToastKind::Info => Color32::from_rgb(70, 130, 200),
                    ToastKind::Success => Color32::from_rgb(50, 160, 90),
                    ToastKind::Warn => Color32::from_rgb(190, 140, 40),
                    ToastKind::Error => Color32::from_rgb(210, 80, 80),
                };
                egui::Frame::popup(ui.style())
                    .fill(ui.visuals().extreme_bg_color)
                    .stroke(egui::Stroke::new(1.0_f32, color))
                    .show(ui, |ui| {
                        ui.label(RichText::new(&toast.text).color(color));
                    });
                ui.add_space(4.0);
            }
        });
}

/// 日期展示。
pub fn message_date(message: &MessageSummary) -> String {
    message
        .date
        .map(me_core::format_local_datetime)
        .unwrap_or_else(|| "—".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 跑一帧顶部导航栏，收集绘制出来的文本。
    fn top_bar_texts(state: &mut UiState) -> Vec<String> {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 200.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| {
            top_bar(state, &mut Vec::new(), ctx);
        });
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_texts(&clipped.shape, &mut texts);
        }
        texts
    }

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

    /// 顶部导航栏必须列出全部页面，天气要在其中。
    ///
    /// 之前左侧栏也画了一套导航，两处信息重复；现在左侧栏已删除，
    /// 顶部栏是唯一的页面切换入口。
    #[test]
    fn top_bar_lists_every_page_including_weather() {
        let mut state = UiState::default();
        let texts = top_bar_texts(&mut state);
        for page in [
            Page::Mail,
            Page::Calendar,
            Page::Weather,
            Page::Music,
            Page::Accounts,
            Page::Settings,
        ] {
            assert!(
                texts.iter().any(|text| text == page.label()),
                "顶部导航栏应包含「{}」，实际 {texts:?}",
                page.label()
            );
        }
    }

    /// 页面切换入口只在顶部栏，页面上不应再出现重复的导航条目。
    #[test]
    fn page_switch_entries_are_not_duplicated() {
        let mut state = UiState {
            page: Page::Calendar,
            ..Default::default()
        };
        let texts = top_bar_texts(&mut state);
        // 「日历」在顶部栏出现一次即可；左栏不再重复画导航。
        let count = texts.iter().filter(|text| *text == "日历").count();
        assert_eq!(count, 1, "页面入口不应重复出现，实际 {texts:?}");
    }

    // --- 统一收件箱（批次 4）---

    /// 跑一帧左侧栏，收集绘制出来的文本。
    fn sidebar_texts(state: &mut UiState) -> Vec<String> {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 800.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| {
            folder_sidebar(state, &mut Vec::new(), ctx);
        });
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_texts(&clipped.shape, &mut texts);
        }
        texts
    }

    fn state_with_account() -> UiState {
        let account = crate::state::sample_account(1, "a@example.com");
        UiState {
            accounts: vec![account],
            selected_account: Some(me_core::AccountId(1)),
            ..Default::default()
        }
    }

    /// 侧栏必须有「统一收件箱」这一行——**光改状态不算做完，控件得真画出来**。
    #[test]
    fn sidebar_renders_the_unified_inbox_row() {
        let mut state = state_with_account();
        state.unified_unread_count = 3;
        let texts = sidebar_texts(&mut state);
        let joined = texts.join("\n");
        assert!(
            joined.contains("统一收件箱"),
            "侧栏缺少统一收件箱行：{texts:?}"
        );
        assert!(joined.contains('3'), "统一收件箱行应显示未读数：{texts:?}");
    }

    /// 一个账户都没有时不画统一收件箱——聚合视图没有来源。
    #[test]
    fn sidebar_hides_the_unified_inbox_without_accounts() {
        let mut state = UiState::default();
        let texts = sidebar_texts(&mut state);
        assert!(
            !texts.join("\n").contains("统一收件箱"),
            "没有账户时不该显示统一收件箱：{texts:?}"
        );
    }
}
