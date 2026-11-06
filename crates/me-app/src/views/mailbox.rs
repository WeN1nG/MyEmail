//! 邮件列表页：文件夹标题、搜索栏、虚拟滚动列表、空态与分页。

use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};

use me_core::{Command, MessageId, SavedSearchId, SavedSearchQuery, SearchQuery, now_unix};

use crate::app::human_bytes;
use crate::state::{ComposeState, Dialog, Page, ToastKind, UiState};

pub fn show(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    toolbar(state, commands, ui);
    ui.separator();

    if state.accounts.is_empty() {
        empty_state(
            ui,
            "还没有账户",
            "添加一个 IMAP/SMTP 账户后即可收信。",
            Some(("添加账户", true)),
            state,
            commands,
        );
        return;
    }
    let Some(_account) = state.selected_account else {
        empty_state(
            ui,
            "请选择账户",
            "在左侧选择账户，或添加一个新账户。",
            None,
            state,
            commands,
        );
        return;
    };
    if state.selected_folder.is_none() && !state.search.active {
        empty_state(
            ui,
            "请选择文件夹",
            "在左侧文件夹树中选择一个文件夹。",
            None,
            state,
            commands,
        );
        return;
    }

    if state.search.active {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "搜索“{}”：{} 封",
                    state.search.text,
                    state.messages.len()
                ))
                .strong(),
            );
            ui.label(
                RichText::new(state.search.coverage_label())
                    .small()
                    .color(Color32::GRAY),
            );
            if state.search.is_short_query() {
                ui.label(
                    RichText::new("查询词 1～2 字时仅匹配头部，正文不参与")
                        .small()
                        .color(Color32::from_rgb(190, 140, 40)),
                );
            }
            if ui.small_button("退出搜索").clicked() {
                state.search.clear();
                state.messages_dirty = true;
            }
        });
    }

    let mut open: Option<MessageId> = None;
    let mut toggle_star: Option<MessageId> = None;
    let mut toggle_read: Option<MessageId> = None;
    let mut move_trash: Option<MessageId> = None;

    if state.messages.is_empty() {
        no_messages(state, ui);
    } else {
        let table_height = (ui.available_height() - 40.0).max(120.0);
        TableBuilder::new(ui)
            .max_scroll_height(table_height)
            // 默认是 Sense::hover()，行不会产生点击事件；必须显式启用点击。
            .sense(egui::Sense::click())
            .striped(true)
            .resizable(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(28.0)) // 未读
            .column(Column::exact(26.0)) // 星标
            .column(Column::initial(150.0).at_least(80.0).clip(true))
            .column(Column::remainder().at_least(200.0).clip(true))
            .column(Column::initial(130.0).at_least(100.0))
            .column(Column::exact(30.0)) // 附件
            .header(22.0, |mut header| {
                header.col(|ui| {
                    ui.label("");
                });
                header.col(|ui| {
                    ui.label("");
                });
                header.col(|ui| {
                    ui.strong("发件人");
                });
                header.col(|ui| {
                    ui.strong("主题");
                });
                header.col(|ui| {
                    ui.strong("时间");
                });
                header.col(|ui| {
                    ui.strong("");
                });
            })
            .body(|body| {
                body.rows(26.0, state.messages.len(), |mut row| {
                    let index = row.index();
                    let Some(message) = state.messages.get(index) else {
                        return;
                    };
                    let id = message.id;
                    let unread = !message.flags.is_seen();
                    let starred = message.flags.is_flagged();
                    let mut star_clicked = false;

                    row.set_selected(state.selected_message == Some(id));

                    row.col(|ui| {
                        if unread {
                            ui.label(
                                RichText::new("●")
                                    .color(Color32::from_rgb(60, 140, 220))
                                    .small(),
                            );
                        }
                    });
                    row.col(|ui| {
                        let button = egui::Button::new(
                            RichText::new(if starred { "★" } else { "☆" }).color(if starred {
                                Color32::from_rgb(220, 170, 40)
                            } else {
                                Color32::GRAY
                            }),
                        )
                        .frame(false)
                        .small();
                        if ui.add(button).clicked() {
                            toggle_star = Some(id);
                            star_clicked = true;
                        }
                    });
                    row.col(|ui| {
                        let from = message
                            .from
                            .as_ref()
                            .map(|a| {
                                a.name
                                    .clone()
                                    .filter(|n| !n.trim().is_empty())
                                    .unwrap_or_else(|| a.addr.clone())
                            })
                            .unwrap_or_else(|| "(未知)".to_string());
                        let text = if unread {
                            RichText::new(from).strong()
                        } else {
                            RichText::new(from)
                        };
                        ui.label(text);
                    });
                    row.col(|ui| {
                        let subject = message.display_subject();
                        let mut text = if unread {
                            RichText::new(subject).strong()
                        } else {
                            RichText::new(subject)
                        };
                        if unread {
                            text = text.color(ui.visuals().strong_text_color());
                        }
                        ui.label(text);
                        let preview = state
                            .messages
                            .get(index)
                            .map(|m| m.snippet.as_deref().filter(|s| !s.is_empty()).unwrap_or(""))
                            .unwrap_or("");
                        if !preview.is_empty() {
                            ui.label(
                                RichText::new(format!("— {preview}"))
                                    .small()
                                    .color(Color32::GRAY),
                            );
                        }
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(crate::views::message_date(message)).small());
                    });
                    row.col(|ui| {
                        if message.has_attachment {
                            ui.label(RichText::new("📎").small());
                        }
                    });

                    if row.response().clicked() && !star_clicked {
                        open = Some(id);
                    }
                    row.response().context_menu(|ui| {
                        if ui.button("标记为已读/未读").clicked() {
                            toggle_read = Some(id);
                            ui.close();
                        }
                        if ui.button("切换星标").clicked() {
                            toggle_star = Some(id);
                            ui.close();
                        }
                        if ui.button("移入废件箱").clicked() {
                            move_trash = Some(id);
                            ui.close();
                        }
                    });
                });
            });
    }

    if !state.search.active && state.messages_has_more && ui.button("加载更多").clicked() {
        state.load_more_requested = true;
    }

    if let Some(id) = toggle_star
        && let Some(message) = state.messages.iter().find(|m| m.id == id)
    {
        commands.push(Command::SetFlags {
            ids: vec![id],
            flags: crate::state::toggle_star_op(message),
        });
    }
    if let Some(id) = toggle_read
        && let Some(message) = state.messages.iter().find(|m| m.id == id)
    {
        commands.push(Command::SetFlags {
            ids: vec![id],
            flags: crate::state::toggle_seen_op(message),
        });
    }
    if let Some(id) = move_trash {
        if state
            .selected_account
            .and_then(|account| state.trash_folder(account))
            .is_none()
        {
            state.toast(
                ToastKind::Warn,
                "该账户没有废件箱角色，无法删除。请先在服务器端创建废件箱，或在文件夹右键菜单指定角色。",
            );
        } else {
            commands.push(Command::MoveToTrash(vec![id]));
        }
    }
    if let Some(id) = open {
        state.selected_message = Some(id);
        state.reader_open = true;
        state.open_body = None;
        state.body_loading = true;
        commands.push(Command::OpenMessage(id));
    }
}

fn toolbar(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        if let Some(account) = state.selected_account
            && ui.button("✏ 写邮件").clicked()
            && let Some(account) = state.account(account)
        {
            state.dialog = Dialog::Compose(ComposeState::new(account.id, &account.email));
        }
        if let Some(folder) = state.selected_folder_struct() {
            ui.separator();
            ui.label(RichText::new(&folder.name).strong());
            if folder.deleted_on_server {
                ui.label(
                    RichText::new("（服务器已删除）")
                        .small()
                        .color(Color32::GRAY)
                        .italics(),
                );
            }
            ui.label(
                RichText::new(format!("{} 封", state.messages.len()))
                    .small()
                    .color(Color32::GRAY),
            );
            if ui.small_button("刷新").clicked()
                && let Some(account) = state.selected_account
            {
                commands.push(Command::SyncFolder {
                    account,
                    folder: folder.id,
                });
            }
            if folder.role == me_core::FolderRole::Trash {
                let can_expunge = true; // 服务器能力在同步侧判断；命令会在不支持时报告错误。
                if can_expunge && ui.small_button("清空废件箱").clicked() {
                    state.dialog = Dialog::Confirm(crate::state::ConfirmDialog {
                        title: "清空废件箱".into(),
                        body: "将永久删除废件箱中的全部邮件，且无法恢复。".into(),
                        action: crate::state::ConfirmAction::EmptyTrash { folder: folder.id },
                        danger: true,
                    });
                }
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(format!("缓存 {}", human_bytes(state.cache_size_bytes)))
                    .small()
                    .color(Color32::GRAY),
            );
        });
    });

    ui.horizontal(|ui| {
        let response = ui.add(
            egui::TextEdit::singleline(&mut state.search.text)
                .hint_text("搜索正文，或用 from: / to: / subject: / has: / is: 速记语法")
                .desired_width(ui.available_width() - 150.0),
        );
        let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        // **输入变化就重新解析**——chips 必须反映输入框当前内容，
        // 否则用户改了 `from:` 但没回车，界面显示的还是旧条件。
        if response.changed() {
            state.search.sync_parsed(now_unix());
        }
        if ui.button("搜索").clicked() || enter {
            submit_search(state, commands);
        }
        if ui.button("条件").clicked() {
            state.search.show_filters = !state.search.show_filters;
        }
        // 「保存此搜索」——**只在有条件时可用**（空搜索存下来没意义）。
        let can_save = !state.search.is_empty_query();
        if ui
            .add_enabled(can_save, egui::Button::new("保存此搜索"))
            .clicked()
        {
            state.search.show_save_search = true;
            state.search.save_search_name.clear();
        }
        if state.search.active && ui.small_button("清除").clicked() {
            state.search.clear();
            state.messages_dirty = true;
        }
    });

    // 已保存的搜索：点击即执行（D42：只存条件，不做自动动作）。
    if !state.search.saved_searches.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("已保存：").small().color(Color32::GRAY));
            let mut to_delete: Option<SavedSearchId> = None;
            for saved in &state.search.saved_searches {
                let name = saved.name.clone();
                if ui.small_button(&name).clicked() {
                    commands.push(Command::RunSavedSearch { id: saved.id });
                }
                if ui.small_button("×").clicked() {
                    to_delete = Some(saved.id);
                }
            }
            if let Some(id) = to_delete {
                commands.push(Command::DeleteSearch { id });
            }
        });
    }

    // 「保存此搜索」弹窗由 `app.rs` 在顶层用 `ctx` 渲染
    // （egui::Window 需要 &Context，而这里只有 &mut Ui）。

    // 条件标签：让用户看得见「我搜的到底是什么」，并能单独删掉某个条件。
    if !state.search.conditions.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("条件：").small().color(Color32::GRAY));
            let mut to_remove: Option<usize> = None;
            for (i, chip) in state.search.conditions.iter().enumerate() {
                let label = chip.label.clone();
                if ui.small_button(format!("{label} ×")).clicked() {
                    to_remove = Some(i);
                }
            }
            if let Some(i) = to_remove {
                state.search.remove_chip(i);
                state.search.sync_parsed(now_unix());
            }
        });
    }

    // 速记语法里拼错的片段：提示但不阻断搜索（探索性场景）。
    if !state.search.parsed.unknown.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(format!(
                    "无法识别：{}",
                    state.search.parsed.unknown.join("、")
                ))
                .small()
                .color(Color32::YELLOW),
            );
        });
    }

    if state.search.show_filters {
        ui.horizontal(|ui| {
            ui.label("时间：");
            ui.add(
                egui::TextEdit::singleline(&mut state.search.since_text)
                    .hint_text("起始 YYYY-MM-DD")
                    .desired_width(120.0),
            );
            ui.label("至");
            ui.add(
                egui::TextEdit::singleline(&mut state.search.until_text)
                    .hint_text("结束 YYYY-MM-DD")
                    .desired_width(120.0),
            );
            ui.separator();
            ui.label("附件：");
            let label = match state.search.has_attachment {
                None => "不限",
                Some(true) => "仅含附件",
                Some(false) => "不含附件",
            };
            egui::ComboBox::from_id_salt("has_attachment_filter")
                .selected_text(label)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut state.search.has_attachment, None, "不限");
                    ui.selectable_value(&mut state.search.has_attachment, Some(true), "仅含附件");
                    ui.selectable_value(&mut state.search.has_attachment, Some(false), "不含附件");
                });
        });
        ui.horizontal(|ui| {
            ui.label("发件人：");
            ui.add(
                egui::TextEdit::singleline(&mut state.search.from)
                    .hint_text("包含")
                    .desired_width(110.0),
            );
            ui.label("收件人：");
            ui.add(
                egui::TextEdit::singleline(&mut state.search.to)
                    .hint_text("包含")
                    .desired_width(110.0),
            );
            ui.label("主题：");
            ui.add(
                egui::TextEdit::singleline(&mut state.search.subject)
                    .hint_text("包含")
                    .desired_width(110.0),
            );
            ui.checkbox(&mut state.search.unread_only, "仅未读");
        });
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("过滤范围为当前账户；文件夹过滤使用左侧选中的文件夹。")
                    .small()
                    .color(Color32::GRAY),
            );
        });
    }
}

/// 「保存此搜索」弹窗：给当前条件起个名字存起来。
///
/// **只存条件、不做自动动作**（D42）——保存的是「怎么搜」，不是「什么时候自动搜」。
///
/// 由 `app.rs` 在顶层用 `ctx` 渲染（与 `accounts::show_editor` 同一模式）——
/// egui 的 `Window` 需要 `&Context`，而 `mailbox::show` 只有 `&mut Ui`。
pub fn show_save_search_dialog(
    state: &mut UiState,
    commands: &mut Vec<Command>,
    ctx: &egui::Context,
) {
    if !state.search.show_save_search {
        return;
    }
    let mut open = true;
    let mut submit = false;
    egui::Window::new("保存此搜索")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(320.0)
        .show(ctx, |ui| {
            ui.label("给这组条件起个名字：");
            ui.add(
                egui::TextEdit::singleline(&mut state.search.save_search_name)
                    .hint_text("例如「张老板的报价」")
                    .desired_width(260.0),
            );
            ui.add_space(6.0);
            // 条件摘要——让用户确认存的是「刚才搜的那个」。
            let conditions = state.search.conditions.len();
            ui.label(
                RichText::new(format!(
                    "将保存 {conditions} 个条件（只存条件，不做自动动作）"
                ))
                .small()
                .color(Color32::GRAY),
            );
            ui.horizontal(|ui| {
                if ui.button("保存").clicked() {
                    submit = true;
                }
                if ui.button("取消").clicked() {
                    state.search.show_save_search = false;
                    state.search.save_search_name.clear();
                }
            });
        });

    if !open {
        state.search.show_save_search = false;
        state.search.save_search_name.clear();
    }
    if submit {
        let name = state.search.save_search_name.trim().to_string();
        if !name.is_empty() {
            // **只存条件**：从当前解析结果 + 面板条件提取，
            // 不含 cursor / limit 这类分页态。
            let query = SavedSearchQuery::from_query(&current_query(state));
            commands.push(Command::SaveSearch { name, query });
        }
        state.search.show_save_search = false;
        state.search.save_search_name.clear();
    }
}

/// 当前界面上的查询条件（不含分页态）。
fn current_query(state: &UiState) -> SearchQuery {
    let parsed = &state.search.parsed;
    let non_empty = |s: &str| {
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    };
    SearchQuery {
        text: parsed.text.trim().to_string(),
        accounts: state.selected_account.into_iter().collect(),
        folders: state.selected_folder.into_iter().collect(),
        since: parsed.since,
        until: parsed.until,
        has_attachment: state.search.has_attachment.or(parsed.has_attachment),
        from: non_empty(&state.search.from).or_else(|| parsed.from.clone()),
        to: non_empty(&state.search.to).or_else(|| parsed.to.clone()),
        subject: non_empty(&state.search.subject).or_else(|| parsed.subject.clone()),
        unread_only: state.search.unread_only || parsed.unread_only,
        limit: 200,
        cursor: None,
    }
}

fn submit_search(state: &mut UiState, commands: &mut Vec<Command>) {
    // **先解析速记语法**——搜索框里的 `from:zhang` 这类片段要拆成结构化条件，
    // 剩余的自由文本才进 FTS。
    state.search.sync_parsed(now_unix());
    let parsed = state.search.parsed.clone();

    if state.search.is_empty_query() {
        state.search.active = false;
        state.messages_dirty = true;
        return;
    }
    // 面板里的时间优先于速记语法里的 `before:` / `after:`——
    // 面板是显式操作，速记语法是输入框里的文本。
    let since = parse_date(&state.search.since_text, false).or(parsed.since);
    let until = parse_date(&state.search.until_text, true).or(parsed.until);
    if let (Some(since), Some(until)) = (since, until)
        && until <= since
    {
        state.toast(ToastKind::Warn, "结束日期必须晚于起始日期");
        return;
    }
    // 面板条件优先（同上）。
    let non_empty = |s: &str| {
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    };
    let query = SearchQuery {
        // **用解析后的正文**，不是原始输入——原始输入里的 `from:zhang`
        // 不该被当成正文去 FTS 匹配。
        text: parsed.text.trim().to_string(),
        accounts: state.selected_account.into_iter().collect(),
        folders: state.selected_folder.into_iter().collect(),
        since,
        until,
        has_attachment: state.search.has_attachment.or(parsed.has_attachment),
        from: non_empty(&state.search.from).or(parsed.from),
        to: non_empty(&state.search.to).or(parsed.to),
        subject: non_empty(&state.search.subject).or(parsed.subject),
        unread_only: state.search.unread_only || parsed.unread_only,
        limit: 200,
        cursor: None,
    };
    commands.push(Command::Search(query));
}

fn parse_date(text: &str, end_of_day: bool) -> Option<i64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let date = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
    let time = if end_of_day {
        date.and_hms_opt(23, 59, 59)?
    } else {
        date.and_hms_opt(0, 0, 0)?
    };
    Some(time.and_utc().timestamp())
}

fn no_messages(state: &UiState, ui: &mut egui::Ui) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        if state.sync_progress.is_some() {
            ui.spinner();
            ui.label(RichText::new("正在同步…").color(Color32::GRAY));
        } else if state.search.active {
            ui.label(RichText::new("没有匹配的邮件").color(Color32::GRAY));
        } else if state.is_offline() {
            ui.label(RichText::new("离线，且本地没有缓存邮件").color(Color32::GRAY));
        } else {
            ui.label(RichText::new("此文件夹没有邮件").color(Color32::GRAY));
        }
    });
}

fn empty_state(
    ui: &mut egui::Ui,
    title: &str,
    body: &str,
    action: Option<(&str, bool)>,
    state: &mut UiState,
    commands: &mut Vec<Command>,
) {
    ui.add_space(60.0);
    ui.vertical_centered(|ui| {
        ui.heading(title);
        ui.add_space(6.0);
        ui.label(RichText::new(body).color(Color32::GRAY));
        ui.add_space(12.0);
        if let Some((label, _)) = action
            && ui.button(label).clicked()
        {
            state.page = Page::Accounts;
            state.dialog = Dialog::AccountEditor(crate::state::AccountEditor::new());
        }
        if state.accounts.is_empty() {
            ui.add_space(16.0);
            ui.label(
                RichText::new("本地缓存为明文保存，不承诺加密。")
                    .small()
                    .color(Color32::GRAY),
            );
        }
    });
    let _ = commands;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ChipKind;

    // --- 速记语法进入查询 ---

    #[test]
    fn syntax_only_query_is_not_treated_as_empty() {
        // 核心回归：`from:zhang` 的 text 为空，旧实现会判成空查询 →
        // 点搜索直接清空搜索、什么都不搜。
        let mut state = UiState::default();
        state.search.text = "from:zhang".to_string();
        let mut commands = Vec::new();
        submit_search(&mut state, &mut commands);

        assert_eq!(commands.len(), 1, "速记语法必须能触发搜索");
        match &commands[0] {
            Command::Search(q) => {
                assert_eq!(q.from.as_deref(), Some("zhang"));
                assert!(q.text.is_empty(), "速记语法片段不该进正文全文检索");
            }
            other => panic!("期望 Command::Search，实际 {other:?}"),
        }
    }

    #[test]
    fn syntax_text_is_split_from_conditions() {
        let mut state = UiState::default();
        state.search.text = "from:zhang 季度报告".to_string();
        let mut commands = Vec::new();
        submit_search(&mut state, &mut commands);

        match &commands[0] {
            Command::Search(q) => {
                assert_eq!(q.from.as_deref(), Some("zhang"));
                assert_eq!(q.text, "季度报告");
            }
            other => panic!("期望 Command::Search，实际 {other:?}"),
        }
    }

    #[test]
    fn panel_conditions_are_filled_into_query() {
        let mut state = UiState::default();
        state.search.from = "alice".to_string();
        state.search.to = "bob".to_string();
        state.search.subject = "预算".to_string();
        state.search.unread_only = true;
        let mut commands = Vec::new();
        submit_search(&mut state, &mut commands);

        match &commands[0] {
            Command::Search(q) => {
                assert_eq!(q.from.as_deref(), Some("alice"));
                assert_eq!(q.to.as_deref(), Some("bob"));
                assert_eq!(q.subject.as_deref(), Some("预算"));
                assert!(q.unread_only);
            }
            other => panic!("期望 Command::Search，实际 {other:?}"),
        }
    }

    #[test]
    fn panel_condition_overrides_syntax_of_same_kind() {
        // 面板是显式操作，速记语法是输入框里的文本——面板优先。
        let mut state = UiState::default();
        state.search.text = "from:syntax".to_string();
        state.search.from = "panel".to_string();
        let mut commands = Vec::new();
        submit_search(&mut state, &mut commands);

        match &commands[0] {
            Command::Search(q) => assert_eq!(q.from.as_deref(), Some("panel")),
            other => panic!("期望 Command::Search，实际 {other:?}"),
        }
    }

    #[test]
    fn empty_query_clears_search_instead_of_searching() {
        // `messages_dirty` 默认是 true（首帧要加载），这里显式置 false 才能断言它被置回。
        let mut state = UiState {
            messages_dirty: false,
            ..Default::default()
        };
        let mut commands = Vec::new();
        submit_search(&mut state, &mut commands);

        assert!(commands.is_empty(), "空查询不该发 Command");
        assert!(!state.search.active);
        assert!(state.messages_dirty, "清空搜索后应标记列表待刷新");
    }

    #[test]
    fn unknown_syntax_still_searches_without_blocking() {
        // 拼错的字段名不该阻断搜索（探索性场景）。
        let mut state = UiState::default();
        state.search.text = "fromm:zhang 报告".to_string();
        let mut commands = Vec::new();
        submit_search(&mut state, &mut commands);

        assert_eq!(commands.len(), 1, "未知片段不该阻断搜索");
        match &commands[0] {
            Command::Search(q) => assert_eq!(q.text, "报告"),
            other => panic!("期望 Command::Search，实际 {other:?}"),
        }
    }

    #[test]
    fn short_query_hint_uses_body_part_only() {
        // `from:ab` 走 from 的 LIKE，与正文全文检索无关 → 不该提示「1~2 字」。
        let mut state = UiState::default();
        state.search.text = "from:ab".to_string();
        state.search.sync_parsed(now_unix());
        assert!(!state.search.is_short_query());

        state.search.text = "ab".to_string();
        state.search.sync_parsed(now_unix());
        assert!(state.search.is_short_query());
    }

    // --- 条件标签（chips）---

    #[test]
    fn chips_reflect_current_input_not_the_parsed_snapshot() {
        // 用户改了输入框但没回车，chips 必须跟着变——
        // 否则界面显示的还是旧条件，与用户以为的不一致。
        let mut state = UiState::default();
        state.search.text = "from:alice".to_string();
        state.search.sync_parsed(now_unix());
        assert_eq!(state.search.conditions.len(), 1);
        assert_eq!(state.search.conditions[0].kind, ChipKind::From);

        state.search.text = "from:bob to:carol".to_string();
        state.search.sync_parsed(now_unix());
        assert_eq!(state.search.conditions.len(), 2);
    }

    #[test]
    fn chips_include_panel_conditions() {
        let mut state = UiState::default();
        state.search.from = "alice".to_string();
        state.search.unread_only = true;
        state.search.sync_parsed(now_unix());

        let kinds: Vec<ChipKind> = state.search.conditions.iter().map(|c| c.kind).collect();
        assert!(kinds.contains(&ChipKind::From));
        assert!(kinds.contains(&ChipKind::Unread));
    }

    #[test]
    fn removing_a_chip_clears_the_matching_panel_field() {
        let mut state = UiState::default();
        state.search.from = "alice".to_string();
        state.search.subject = "预算".to_string();
        state.search.unread_only = true;
        state.search.sync_parsed(now_unix());

        let from_idx = state
            .search
            .conditions
            .iter()
            .position(|c| c.kind == ChipKind::From)
            .expect("应有发件人标签");
        state.search.remove_chip(from_idx);
        assert!(state.search.from.is_empty(), "删标签要同时清掉面板输入");
        assert_eq!(state.search.subject, "预算", "其它条件不受影响");
        assert!(state.search.unread_only, "其它条件不受影响");
    }

    #[test]
    fn removing_unread_chip_turns_the_flag_off() {
        let mut state = UiState::default();
        state.search.unread_only = true;
        state.search.sync_parsed(now_unix());
        let idx = state
            .search
            .conditions
            .iter()
            .position(|c| c.kind == ChipKind::Unread)
            .expect("应有未读标签");
        state.search.remove_chip(idx);
        assert!(!state.search.unread_only);
    }

    #[test]
    fn removing_out_of_range_chip_is_a_no_op() {
        let mut state = UiState::default();
        state.search.from = "alice".to_string();
        state.search.sync_parsed(now_unix());
        state.search.remove_chip(99);
        assert_eq!(state.search.from, "alice", "越界删除不该改状态");
    }

    #[test]
    fn clear_resets_all_conditions() {
        let mut state = UiState::default();
        state.search.text = "from:alice".to_string();
        state.search.from = "bob".to_string();
        state.search.unread_only = true;
        state.search.sync_parsed(now_unix());
        state.search.clear();

        assert!(state.search.text.is_empty());
        assert!(state.search.from.is_empty());
        assert!(!state.search.unread_only);
        assert!(state.search.conditions.is_empty());
        assert!(state.search.parsed.is_empty());
        assert!(state.search.is_empty_query());
    }

    #[test]
    fn is_empty_query_is_true_when_nothing_is_set() {
        let state = UiState::default();
        assert!(state.search.is_empty_query());
    }

    // --- 离屏渲染 ---

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
        let _ = ctx.run(input(), &mut body);
        let output = ctx.run(input(), body);
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_texts(&clipped.shape, &mut texts);
        }
        texts
    }

    fn collect_texts(shape: &egui::Shape, out: &mut Vec<String>) {
        if let egui::Shape::Text(text) = shape {
            out.push(text.galley.text().to_string());
        }
        if let egui::Shape::Vec(shapes) = shape {
            for s in shapes {
                collect_texts(s, out);
            }
        }
    }

    #[test]
    fn conditions_panel_renders_the_four_new_controls() {
        // 条件面板必须真的画出来——后端支持九个条件，UI 只暴露三个是缺口。
        let mut state = UiState::default();
        state.search.show_filters = true;
        let texts = run_frame(|ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                toolbar(&mut state, &mut Vec::new(), ui);
            });
        });
        let joined = texts.join("\n");
        assert!(joined.contains("发件人："), "缺少发件人条件");
        assert!(joined.contains("收件人："), "缺少收件人条件");
        assert!(joined.contains("主题："), "缺少主题条件");
        assert!(joined.contains("仅未读"), "缺少仅未读条件");
    }

    #[test]
    fn chips_are_rendered_with_deletable_labels() {
        // 条件标签要真的出现在界面上，且带删除标记。
        let mut state = UiState::default();
        state.search.text = "from:zhang".to_string();
        state.search.sync_parsed(now_unix());
        let texts = run_frame(|ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                toolbar(&mut state, &mut Vec::new(), ui);
            });
        });
        let joined = texts.join("\n");
        assert!(
            joined.contains("发件人含 zhang"),
            "条件标签未渲染，实际文本：{joined}"
        );
        assert!(joined.contains('×'), "条件标签应带删除标记");
    }

    #[test]
    fn unknown_syntax_fragment_is_shown_but_not_blocking() {
        // 拼错的片段要有提示，同时搜索照常可用。
        let mut state = UiState::default();
        state.search.text = "fromm:zhang".to_string();
        state.search.sync_parsed(now_unix());
        let texts = run_frame(|ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                toolbar(&mut state, &mut Vec::new(), ui);
            });
        });
        let joined = texts.join("\n");
        assert!(
            joined.contains("无法识别"),
            "未知片段应给出提示，实际文本：{joined}"
        );
    }

    #[test]
    fn search_hint_mentions_the_syntax() {
        // 搜索框的提示文案要告诉用户支持哪些语法。
        let mut state = UiState::default();
        let texts = run_frame(|ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                toolbar(&mut state, &mut Vec::new(), ui);
            });
        });
        let joined = texts.join("\n");
        assert!(joined.contains("from:"), "提示未提及速记语法：{joined}");
    }

    // --- 保存的搜索 ---

    #[test]
    fn saved_search_button_is_disabled_without_conditions() {
        // 空搜索存下来没意义——按钮该是禁用的。
        let mut state = UiState::default();
        let texts = run_frame(|ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                toolbar(&mut state, &mut Vec::new(), ui);
            });
        });
        assert!(texts.join("\n").contains("保存此搜索"));
    }

    #[test]
    fn saved_search_list_is_rendered_when_present() {
        let mut state = UiState::default();
        state.search.saved_searches = vec![me_core::SavedSearch {
            id: me_core::SavedSearchId(1),
            name: "张老板的报价".to_string(),
            query: me_core::SavedSearchQuery::default(),
            created_at: 0,
            last_used_at: 0,
        }];
        let texts = run_frame(|ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                toolbar(&mut state, &mut Vec::new(), ui);
            });
        });
        let joined = texts.join("\n");
        assert!(
            joined.contains("张老板的报价"),
            "已保存的搜索未渲染：{joined}"
        );
        assert!(joined.contains("已保存"), "缺少「已保存」分组标签");
    }

    #[test]
    fn current_query_excludes_pagination_state() {
        // 保存时**只存条件**——cursor / limit 不该进去。
        let mut state = UiState::default();
        state.search.text = "from:zhang".to_string();
        state.search.sync_parsed(now_unix());

        let q = current_query(&state);
        assert_eq!(q.from.as_deref(), Some("zhang"));
        assert_eq!(q.cursor, None);

        // 持久化子集里**没有** cursor / limit 字段——
        // 它们的类型是 `Option<(i64, i64)>` 与 `u32`，
        // 存进 JSON 就会带上分页态。这里用「字段不存在」来断言。
        let saved = me_core::SavedSearchQuery::from_query(&q);
        // 反向验证：从持久化子集还原时，分页态是「干净」的。
        let restored = saved.to_query();
        assert_eq!(restored.cursor, None);
        assert_eq!(restored.limit, me_core::MAX_PAGE_SIZE);
    }

    #[test]
    fn current_query_merges_panel_and_syntax_conditions() {
        let mut state = UiState::default();
        state.search.text = "to:carol".to_string();
        state.search.from = "alice".to_string();
        state.search.unread_only = true;
        state.search.sync_parsed(now_unix());

        let q = current_query(&state);
        assert_eq!(q.from.as_deref(), Some("alice"));
        assert_eq!(q.to.as_deref(), Some("carol"));
        assert!(q.unread_only);
    }
}
