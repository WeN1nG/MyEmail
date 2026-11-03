//! 邮件列表页：文件夹标题、搜索栏、虚拟滚动列表、空态与分页。

use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};

use me_core::{Command, MessageId, SearchQuery};

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
                .hint_text("搜索主题、发件人、收件人或正文（≥3 字可搜正文）")
                .desired_width(ui.available_width() - 150.0),
        );
        let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if ui.button("搜索").clicked() || enter {
            submit_search(state, commands);
        }
        if ui.button("条件").clicked() {
            state.search.show_filters = !state.search.show_filters;
        }
        if state.search.active && ui.small_button("清除").clicked() {
            state.search.clear();
            state.messages_dirty = true;
        }
    });

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
            ui.label(
                RichText::new("过滤范围为当前账户；文件夹过滤使用左侧选中的文件夹。")
                    .small()
                    .color(Color32::GRAY),
            );
        });
    }
}

fn submit_search(state: &mut UiState, commands: &mut Vec<Command>) {
    if state.search.is_empty_query() {
        state.search.active = false;
        state.messages_dirty = true;
        return;
    }
    let since = parse_date(&state.search.since_text, false);
    let until = parse_date(&state.search.until_text, true);
    if let (Some(since), Some(until)) = (since, until)
        && until <= since
    {
        state.toast(ToastKind::Warn, "结束日期必须晚于起始日期");
        return;
    }
    let query = SearchQuery {
        text: state.search.text.trim().to_string(),
        accounts: state.selected_account.into_iter().collect(),
        folders: state.selected_folder.into_iter().collect(),
        since,
        until,
        has_attachment: state.search.has_attachment,
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
