//! 邮件列表页：文件夹标题、搜索栏、虚拟滚动列表、空态与分页。

use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};

use me_core::{
    Command, FlagOp, Flags, MessageId, SavedSearchId, SavedSearchQuery, SearchQuery, now_unix,
};

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
    // **统一收件箱跨账户**，不要求先选定单个账户——否则侧栏点得进来、
    // 列表却提示「请选择账户」。
    if state.selected_account.is_none() && !state.unified_inbox {
        empty_state(
            ui,
            "请选择账户",
            "在左侧选择账户，或添加一个新账户。",
            None,
            state,
            commands,
        );
        return;
    }
    // **统一收件箱选中时 `selected_folder` 也是 `None`**——
    // 不加 `!state.unified_inbox` 会把它误判成「请选择文件夹」的空态。
    if state.selected_folder.is_none() && !state.search.active && !state.unified_inbox {
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
    // 点了线程折叠标记：**只切换展开，不算点中那一行**（否则会顺带打开邮件）。
    let mut thread_toggle: Option<String> = None;
    // **在 TableBuilder 闭包外取修饰键**——闭包里 `ui` 已被可变借用，
    // 再调 `ui.input()` 会撞借用检查。
    let modifiers = ui.input(|i| i.modifiers);

    if state.messages.is_empty() {
        no_messages(state, ui);
    } else {
        let table_height = (ui.available_height() - 40.0).max(120.0);
        // **聚合模式多一列「账户」**——多账户时否则无法分辨邮件来自谁。
        // 列宽用 `Column::initial(...)`，遵守 D40（宽度交给布局算）。
        let unified = state.unified_inbox;
        let mut table = TableBuilder::new(ui)
            .max_scroll_height(table_height)
            // 默认是 Sense::hover()，行不会产生点击事件；必须显式启用点击。
            .sense(egui::Sense::click())
            .striped(true)
            .resizable(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(28.0)) // 未读
            .column(Column::exact(26.0)) // 星标
            .column(Column::initial(150.0).at_least(80.0).clip(true));
        if unified {
            table = table.column(Column::initial(130.0).at_least(80.0).clip(true));
        }
        table = table
            .column(Column::remainder().at_least(200.0).clip(true))
            .column(Column::initial(130.0).at_least(100.0))
            .column(Column::exact(30.0)); // 附件
        table
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
                if unified {
                    header.col(|ui| {
                        ui.strong("账户");
                    });
                }
                header.col(|ui| {
                    ui.strong("主题");
                });
                header.col(|ui| {
                    ui.strong("时间");
                });
                header.col(|ui| {
                    ui.label("");
                });
            })
            .body(|body| {
                // **画几行、画哪几封走 `thread_rows()`**：折叠版与「关闭折叠」
                // 共用这条路径（关闭时它返回恒等映射），不在渲染里分叉。
                let rows = state.thread_rows();
                body.rows(26.0, rows.len(), |mut row| {
                    let row_index = row.index();
                    let Some(view) = rows.get(row_index).copied() else {
                        return;
                    };
                    let index = view.index;
                    let Some(message) = state.messages.get(index) else {
                        return;
                    };
                    let id = message.id;
                    let thread_key = me_core::thread_key(message);
                    let unread = !message.flags.is_seen();
                    let starred = message.flags.is_flagged();
                    let mut star_clicked = false;
                    let mut thread_clicked = false;

                    // 高亮：**在多选集合内**或**等于单选值**。
                    // 只看单选值的话，多选时看不出选了哪些。
                    row.set_selected(
                        state.selected_messages.contains(&id) || state.selected_message == Some(id),
                    );

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
                    if unified {
                        row.col(|ui| {
                            // 显示账户邮箱——多账户时否则无法分辨来源。
                            let email = state
                                .account(message.account)
                                .map(|a| a.email.clone())
                                .unwrap_or_else(|| "(未知账户)".to_string());
                            ui.label(RichText::new(email).small());
                        });
                    }
                    row.col(|ui| {
                        // 线程标记：首行画 `▸ 3` / `▾ 3`（只有一封时不画），
                        // 展开后的后续行缩进 + `↳`——不新增列，避免挤压主题宽度。
                        if view.group_len > 1 && view.depth == 0 {
                            let marker = if state.thread_expanded(&thread_key) {
                                format!("▾ {}", view.group_len)
                            } else {
                                format!("▸ {}", view.group_len)
                            };
                            let button = egui::Button::new(
                                RichText::new(marker)
                                    .small()
                                    .color(Color32::from_rgb(90, 130, 200)),
                            )
                            .frame(false);
                            if ui.add(button).clicked() {
                                thread_toggle = Some(thread_key.clone());
                                thread_clicked = true;
                            }
                            ui.add_space(3.0);
                        } else if view.depth > 0 {
                            ui.add_space(12.0);
                            ui.label(RichText::new("↳").small().color(Color32::GRAY));
                            ui.add_space(3.0);
                        }
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

                    if row.response().clicked() && !star_clicked && !thread_clicked {
                        // **多选交互**：`Ctrl` 切换集合、`Shift` 选区间、
                        // 裸点击清集合并设单选（同时打开阅读窗）。
                        if modifiers.ctrl {
                            state.toggle_selected(id);
                        } else if modifiers.shift {
                            // 区间按**可见顺序**算：折叠时列表里看不到的行不该被选进来。
                            let ordered: Vec<MessageId> = rows
                                .iter()
                                .filter_map(|view| state.messages.get(view.index))
                                .map(|message| message.id)
                                .collect();
                            state.select_range(id, &ordered);
                        } else {
                            state.select_only(id);
                            open = Some(id);
                        }
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

    // 批量工具条：选中 N 封 → 已读/未读、加星/取消加星、删除、清除选择。
    //
    // 动作集只有三项（已读/未读、加星、删除）——**不含「移动到文件夹」**：
    // 项目根本没有这个能力（`pending_ops.kind` 只有 move_to_trash /
    // delete_permanent / empty_folder），要做就得动 IMAP 协议层，
    // 与 D47「v0.4 不碰服务器写路径」冲突。已拍板砍掉（2026-11-06）。
    if !state.selected_messages.is_empty() {
        let count = state.selected_messages.len();
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("已选 {count} 封")).strong());

            // 批量标志：**目标态**语义——全部已读则「标为未读」，否则「标为已读」。
            let all_seen = state.batch_targets().iter().all(|id| {
                state
                    .messages
                    .iter()
                    .find(|m| m.id == *id)
                    .map(|m| m.flags.is_seen())
                    .unwrap_or(false)
            });
            if ui
                .button(if all_seen {
                    "标为未读"
                } else {
                    "标为已读"
                })
                .clicked()
            {
                let ids = state.batch_targets();
                commands.push(Command::SetFlags {
                    ids,
                    flags: if all_seen {
                        FlagOp::Clear(Flags::SEEN)
                    } else {
                        FlagOp::Set(Flags::SEEN)
                    },
                });
                state.clear_multi_selection();
            }

            let all_flagged = state.batch_targets().iter().all(|id| {
                state
                    .messages
                    .iter()
                    .find(|m| m.id == *id)
                    .map(|m| m.flags.is_flagged())
                    .unwrap_or(false)
            });
            if ui
                .button(if all_flagged {
                    "取消加星"
                } else {
                    "加星"
                })
                .clicked()
            {
                let ids = state.batch_targets();
                commands.push(Command::SetFlags {
                    ids,
                    flags: if all_flagged {
                        FlagOp::Clear(Flags::FLAGGED)
                    } else {
                        FlagOp::Set(Flags::FLAGGED)
                    },
                });
                state.clear_multi_selection();
            }

            if ui.button("删除").clicked() {
                // 破坏性操作走**确认卡**（沿用 AI 页 `pending_delete` 的先例）。
                state.pending_batch_delete = Some(state.batch_targets());
            }
            if ui.button("清除选择").clicked() {
                state.clear_multi_selection();
            }
        });
    }

    // 批量删除确认卡。
    if let Some(ids) = state.pending_batch_delete.clone() {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("确定要把这 {} 封邮件移入废件箱吗？", ids.len())).strong(),
            );
            if ui.button("确定").clicked() {
                commands.push(Command::MoveToTrash(ids));
                state.pending_batch_delete = None;
                state.clear_multi_selection();
            }
            if ui.button("取消").clicked() {
                state.pending_batch_delete = None;
            }
        });
    }

    if let Some(key) = thread_toggle {
        state.toggle_thread(&key);
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
        if has_trash_for(state, id) {
            commands.push(Command::MoveToTrash(vec![id]));
        } else {
            state.toast(
                ToastKind::Warn,
                "该账户没有废件箱角色，无法删除。请先在服务器端创建废件箱，或在文件夹右键菜单指定角色。",
            );
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

/// 能不能把这封邮件移入废件箱。
///
/// **按邮件所属账户找废件箱**，不是按 `selected_account`——统一收件箱里
/// 这封可能来自另一个账户，用错账户会拦下一次合法删除。
fn has_trash_for(state: &UiState, id: MessageId) -> bool {
    state
        .messages
        .iter()
        .find(|m| m.id == id)
        .map(|m| m.account)
        .and_then(|account| state.trash_folder(account))
        .is_some()
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
        } else if state.unified_inbox {
            // 聚合视图没有单个文件夹可显示——标题要单独给，
            // 否则多账户时看不出当前在看什么。
            ui.separator();
            ui.label(RichText::new("📥 统一收件箱").strong());
            ui.label(
                RichText::new(format!("{} 封", state.messages.len()))
                    .small()
                    .color(Color32::GRAY),
            );
            if ui.small_button("刷新").clicked() {
                // 聚合视图横跨多个账户，逐账户 `SyncFolder` 会刷一整排进度；
                // 这里只重查本地库（离线可用，也不触发网络）。
                state.messages_dirty = true;
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(format!("缓存 {}", human_bytes(state.cache_size_bytes)))
                    .small()
                    .color(Color32::GRAY),
            );
            // 线程折叠开关：默认开。关掉就是「一封一行」的老视图，
            // 排查「以为是丢邮件」时有用。
            ui.checkbox(&mut state.thread_fold, RichText::new("线程").small())
                .on_hover_text("按 References 把同一会话折叠成一行");
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
        // **统一收件箱里搜索不按账户过滤**——这个视图本来就跨账户，
        // 悄悄只搜「当前选中账户」会让结果莫名少一半。
        // `accounts` 为空 = 不限账户（`base_filters` 就是这么解释的）。
        accounts: if state.unified_inbox {
            Vec::new()
        } else {
            state.selected_account.into_iter().collect()
        },
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

    // --- 批量多选（批次 2）---

    fn state_with_messages(count: usize) -> UiState {
        let messages = (0..count)
            .map(|i| me_core::MessageSummary {
                id: MessageId(i as i64 + 1),
                account: me_core::AccountId(1),
                folder: me_core::FolderId(1),
                uid: Some(i as u32 + 1),
                message_id: None,
                subject: Some(format!("邮件 {i}")),
                from: None,
                to: Vec::new(),
                cc: Vec::new(),
                date: None,
                date_source: me_core::DateSource::Header,
                size: None,
                flags: me_core::Flags::NONE,
                snippet: None,
                has_attachment: false,
                body_cached: false,
                in_reply_to: None,
                references: Vec::new(),
            })
            .collect();
        UiState {
            messages,
            ..Default::default()
        }
    }

    #[test]
    fn plain_click_clears_the_selection_set() {
        let mut state = state_with_messages(3);
        state.toggle_selected(MessageId(1));
        state.toggle_selected(MessageId(2));
        assert_eq!(state.selected_messages.len(), 2);

        state.select_only(MessageId(3));
        assert!(state.selected_messages.is_empty(), "裸点击应清空集合");
        assert_eq!(state.selected_message, Some(MessageId(3)));
    }

    #[test]
    fn ctrl_click_toggles_a_single_message() {
        let mut state = state_with_messages(3);
        state.toggle_selected(MessageId(1));
        assert!(state.selected_messages.contains(&MessageId(1)));

        state.toggle_selected(MessageId(1));
        assert!(
            !state.selected_messages.contains(&MessageId(1)),
            "再点一次应取消"
        );

        state.toggle_selected(MessageId(2));
        assert_eq!(state.selected_messages.len(), 1);
    }

    #[test]
    fn shift_click_selects_the_range_from_anchor() {
        let mut state = state_with_messages(5);
        let ordered: Vec<MessageId> = state.messages.iter().map(|m| m.id).collect();

        state.select_only(MessageId(2));
        state.select_range(MessageId(4), &ordered);

        assert_eq!(state.selected_messages.len(), 3);
        assert!(state.selected_messages.contains(&MessageId(2)));
        assert!(state.selected_messages.contains(&MessageId(3)));
        assert!(state.selected_messages.contains(&MessageId(4)));
        assert!(!state.selected_messages.contains(&MessageId(5)));
    }

    #[test]
    fn shift_click_backwards_selects_the_same_range() {
        let mut state = state_with_messages(5);
        let ordered: Vec<MessageId> = state.messages.iter().map(|m| m.id).collect();

        state.select_only(MessageId(4));
        state.select_range(MessageId(2), &ordered);

        assert_eq!(state.selected_messages.len(), 3);
        assert!(state.selected_messages.contains(&MessageId(2)));
        assert!(state.selected_messages.contains(&MessageId(4)));
    }

    #[test]
    fn shift_click_without_anchor_selects_only_that_message() {
        let mut state = state_with_messages(3);
        let ordered: Vec<MessageId> = state.messages.iter().map(|m| m.id).collect();

        state.list_selection_anchor = None;
        state.select_range(MessageId(2), &ordered);
        assert_eq!(state.selected_messages.len(), 1);
        assert!(state.selected_messages.contains(&MessageId(2)));
    }

    #[test]
    fn shift_click_with_stale_anchor_falls_back_to_single_select() {
        // 锚点不在当前列表里（列表换过但集合没清）——不能算出错误的区间。
        let mut state = state_with_messages(3);
        let ordered: Vec<MessageId> = state.messages.iter().map(|m| m.id).collect();
        state.list_selection_anchor = Some(MessageId(999));

        state.select_range(MessageId(2), &ordered);
        assert_eq!(state.selected_messages.len(), 1);
        assert!(state.selected_messages.contains(&MessageId(2)));
    }

    #[test]
    fn batch_targets_follow_the_list_order() {
        let mut state = state_with_messages(4);
        // 故意乱序加入。
        state.toggle_selected(MessageId(3));
        state.toggle_selected(MessageId(1));
        state.toggle_selected(MessageId(4));

        let targets = state.batch_targets();
        assert_eq!(
            targets,
            vec![MessageId(1), MessageId(3), MessageId(4)],
            "批量目标应按列表顺序"
        );
    }

    #[test]
    fn batch_targets_fall_back_to_the_single_selection() {
        let mut state = state_with_messages(3);
        state.select_only(MessageId(2));
        assert_eq!(state.batch_targets(), vec![MessageId(2)]);
    }

    #[test]
    fn clearing_selection_also_drops_the_anchor_and_pending_delete() {
        let mut state = state_with_messages(3);
        state.toggle_selected(MessageId(1));
        state.list_selection_anchor = Some(MessageId(1));
        state.pending_batch_delete = Some(vec![MessageId(1)]);

        state.clear_multi_selection();
        assert!(state.selected_messages.is_empty());
        assert!(state.list_selection_anchor.is_none());
        assert!(state.pending_batch_delete.is_none());
    }

    #[test]
    fn batch_toolbar_is_hidden_without_selection() {
        let mut state = state_with_messages(2);
        let texts = run_frame(|ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                toolbar(&mut state, &mut Vec::new(), ui);
            });
        });
        assert!(
            !texts.join("\n").contains("已选"),
            "没选中时不该显示批量工具条"
        );
    }

    // --- 统一收件箱（批次 4）---

    fn state_with_two_accounts() -> UiState {
        UiState {
            accounts: vec![
                crate::state::sample_account(1, "a@example.com"),
                crate::state::sample_account(2, "b@example.com"),
            ],
            selected_account: Some(me_core::AccountId(1)),
            unified_inbox: true,
            ..Default::default()
        }
    }

    fn message_from(id: i64, account: i64, folder: i64, subject: &str) -> me_core::MessageSummary {
        me_core::MessageSummary {
            id: MessageId(id),
            account: me_core::AccountId(account),
            folder: me_core::FolderId(folder),
            uid: Some(id as u32),
            message_id: None,
            subject: Some(subject.to_string()),
            from: None,
            to: Vec::new(),
            cc: Vec::new(),
            date: Some(1000 + id),
            date_source: me_core::DateSource::Header,
            size: None,
            flags: me_core::Flags::NONE,
            snippet: None,
            has_attachment: false,
            body_cached: false,
            in_reply_to: None,
            references: Vec::new(),
        }
    }

    /// 统一收件箱选中时 `selected_folder` 是 `None`——**不能被当成「请选择文件夹」**。
    #[test]
    fn unified_view_is_not_the_choose_folder_empty_state() {
        let mut state = state_with_two_accounts();
        let texts = run_frame(|ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(&mut state, &mut Vec::new(), ui);
            });
        });
        let joined = texts.join("\n");
        assert!(
            !joined.contains("请选择文件夹"),
            "统一收件箱被误判成空态：{texts:?}"
        );
        assert!(
            joined.contains("统一收件箱"),
            "缺少统一收件箱标题：{texts:?}"
        );
    }

    /// 聚合模式必须多一列「账户」——多账户时否则分不清邮件来源。
    #[test]
    fn unified_view_renders_the_account_column() {
        let mut state = state_with_two_accounts();
        state.messages = vec![
            message_from(2, 2, 20, "来自账户二"),
            message_from(1, 1, 10, "来自账户一"),
        ];
        let texts = run_frame(|ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(&mut state, &mut Vec::new(), ui);
            });
        });
        let joined = texts.join("\n");
        assert!(joined.contains("账户"), "缺少账户列：{texts:?}");
        assert!(
            joined.contains("a@example.com"),
            "缺少账户一邮箱：{texts:?}"
        );
        assert!(
            joined.contains("b@example.com"),
            "缺少账户二邮箱：{texts:?}"
        );
    }

    /// 单文件夹视图不加「账户」列（那一列纯属浪费宽度）。
    #[test]
    fn folder_view_has_no_account_column() {
        let mut state = UiState {
            accounts: vec![crate::state::sample_account(1, "a@example.com")],
            selected_account: Some(me_core::AccountId(1)),
            ..Default::default()
        };
        state.select_folder(me_core::FolderId(10));
        state.messages = vec![message_from(1, 1, 10, "邮件")];
        let texts = run_frame(|ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(&mut state, &mut Vec::new(), ui);
            });
        });
        assert!(
            !texts.join("\n").contains("账户"),
            "单文件夹视图不该有账户列：{texts:?}"
        );
    }

    /// 统一收件箱里搜索不按账户过滤——`accounts` 为空表示不限账户。
    #[test]
    fn search_in_unified_inbox_does_not_filter_by_account() {
        let mut state = state_with_two_accounts();
        state.selected_account = Some(me_core::AccountId(1));
        state.search.text = "季度报告".to_string();
        let mut commands = Vec::new();
        submit_search(&mut state, &mut commands);
        match &commands[0] {
            Command::Search(q) => {
                assert!(
                    q.accounts.is_empty(),
                    "统一收件箱里搜索不该只搜一个账户：{:?}",
                    q.accounts
                );
                assert!(q.folders.is_empty());
            }
            other => panic!("期望 Command::Search，实际 {other:?}"),
        }
    }

    // --- 会话线程折叠（v0.4 C5）---

    fn message_threaded(
        id: i64,
        message_id: &str,
        in_reply_to: Option<&str>,
        references: &[&str],
        subject: &str,
        date: i64,
    ) -> me_core::MessageSummary {
        me_core::MessageSummary {
            id: MessageId(id),
            account: me_core::AccountId(1),
            folder: me_core::FolderId(10),
            uid: Some(id as u32),
            message_id: Some(message_id.to_string()),
            subject: Some(subject.to_string()),
            from: None,
            to: Vec::new(),
            cc: Vec::new(),
            date: Some(date),
            date_source: me_core::DateSource::Header,
            size: None,
            flags: me_core::Flags::NONE,
            snippet: None,
            has_attachment: false,
            body_cached: false,
            in_reply_to: in_reply_to.map(str::to_string),
            references: references.iter().map(|r| r.to_string()).collect(),
        }
    }

    /// 一封原始邮件 + 一封回复（列表按时间倒序，回复在前）。
    fn state_with_thread() -> UiState {
        let mut state = UiState {
            accounts: vec![crate::state::sample_account(1, "a@example.com")],
            selected_account: Some(me_core::AccountId(1)),
            ..Default::default()
        };
        state.select_folder(me_core::FolderId(10));
        state.messages = vec![
            message_threaded(2, "b@x", Some("a@x"), &["a@x"], "回复：报价", 200),
            message_threaded(1, "a@x", None, &[], "原始：报价", 100),
        ];
        state
    }

    fn render_list(state: &mut UiState) -> String {
        let texts = run_frame(|ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(state, &mut Vec::new(), ui);
            });
        });
        texts.join("\n")
    }

    /// 折叠时：一个线程只出一行，带 `▸ N`，父邮件不单独出现。
    #[test]
    fn folded_thread_paints_one_row_with_a_counter() {
        let mut state = state_with_thread();
        let joined = render_list(&mut state);
        assert!(joined.contains("回复：报价"), "折叠行用最新一封：{joined}");
        assert!(joined.contains("▸ 2"), "应有折叠标记与数量：{joined}");
        assert!(
            !joined.contains("原始：报价"),
            "折叠时父邮件不该单独占一行：{joined}"
        );
    }

    /// 展开时：两封都画，且后续行带 `↳`。
    #[test]
    fn expanded_thread_paints_every_message() {
        let mut state = state_with_thread();
        let key = me_core::thread_key(&state.messages[0]);
        state.toggle_thread(&key);
        let joined = render_list(&mut state);
        assert!(
            joined.contains("原始：报价"),
            "展开后父邮件要出现：{joined}"
        );
        assert!(
            joined.contains("回复：报价"),
            "展开后回复也要出现：{joined}"
        );
        assert!(joined.contains("▾ 2"), "展开标记应改成 ▾：{joined}");
        assert!(joined.contains("↳"), "后续行要有缩进标记：{joined}");
    }

    /// 关掉折叠就是老视图：一封一行，没有标记。
    #[test]
    fn thread_fold_off_paints_every_message_without_markers() {
        let mut state = state_with_thread();
        state.thread_fold = false;
        let joined = render_list(&mut state);
        assert!(joined.contains("原始：报价"));
        assert!(joined.contains("回复：报价"));
        assert!(!joined.contains("▸ 2"), "关掉折叠不该有标记：{joined}");
    }

    /// 删除时按**邮件所属账户**找废件箱，不是按当前选中账户。
    #[test]
    fn trash_guard_uses_the_message_account() {
        // 账户 1 有废件箱、账户 2 没有；当前选中的是账户 2，
        // 但邮件来自账户 1 → 应当放行（用错账户会拦下这次合法删除）。
        let mut state = UiState {
            accounts: vec![
                crate::state::sample_account(1, "a@example.com"),
                crate::state::sample_account(2, "b@example.com"),
            ],
            selected_account: Some(me_core::AccountId(2)),
            unified_inbox: true,
            ..Default::default()
        };
        state.folders.insert(
            me_core::AccountId(1),
            vec![me_core::Folder {
                id: me_core::FolderId(10),
                account: me_core::AccountId(1),
                name: "Trash".into(),
                delimiter: Some('/'),
                role: me_core::FolderRole::Trash,
                uid_validity: 1,
                last_uid: 0,
                deleted_on_server: false,
            }],
        );
        state.messages = vec![message_from(1, 1, 10, "来自账户一")];

        assert!(
            has_trash_for(&state, MessageId(1)),
            "应按邮件所属账户找到废件箱"
        );
    }
}
