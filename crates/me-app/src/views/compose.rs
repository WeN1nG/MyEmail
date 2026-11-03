//! 撰写页：纯文本正文、收件人/抄送/密送、附件、保存草稿、发送、重发确认。

use std::path::PathBuf;

use eframe::egui::{self, Color32, RichText};

use me_core::{
    Address, Command, DraftSyncState, MAX_ATTACHMENT_BYTES, OutgoingMessage,
    check_attachment_limits,
};

use crate::state::{ComposeState, ComposeStatus, Dialog, ToastKind, UiState, parse_address_list};

#[derive(Default)]
struct ComposeActions {
    close: bool,
    discard: bool,
    save: bool,
    send: bool,
    save_to_server: bool,
    attach_files: bool,
    attachment_notice: Option<String>,
}

pub fn show_window(state: &mut UiState, commands: &mut Vec<Command>, ctx: &egui::Context) {
    let account_id = match &state.dialog {
        Dialog::Compose(compose) => compose.account,
        _ => return,
    };
    let Some(account) = state.account(account_id).cloned() else {
        state.dialog.close();
        return;
    };
    let from = account.from_address();
    let from_email = account.email.clone();
    let draft_sync = match &state.dialog {
        Dialog::Compose(compose) => compose.draft_id.and_then(|id| {
            state
                .drafts
                .iter()
                .find(|draft| draft.id == id)
                .map(|draft| (draft.sync_state, draft.last_error.clone()))
        }),
        _ => None,
    };

    let mut actions = ComposeActions::default();

    {
        let Dialog::Compose(compose) = &mut state.dialog else {
            return;
        };

        // 退出窗口：有未保存草稿时先落库，DraftSaved 后再关闭。
        if ctx.input(|i| i.viewport().close_requested())
            && compose.is_dirty()
            && !compose.is_empty()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            compose.exit_after_save = true;
            actions.save = true;
        }

        let title = if compose.draft_id.is_some() {
            "编辑草稿"
        } else {
            "写邮件"
        };
        let mut open = true;

        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(680.0)
            .default_height(560.0)
            // 与阅读窗口一致：始终给视口留出余量，窗口与视口等高时会被位置约束，
            // 拖动后弹回原处。
            .max_width((ctx.screen_rect().width() - 80.0).max(320.0))
            .max_height((ctx.screen_rect().height() - 80.0).max(240.0))
            .show(ctx, |ui| {
                if let Some((sync_state, error)) = &draft_sync {
                    let (text, color) = match sync_state {
                        DraftSyncState::Conflict => (
                            "本地与服务器都修改过；请在账户页草稿列表选择保留方式。",
                            Color32::from_rgb(210, 90, 60),
                        ),
                        DraftSyncState::DeletedServer => (
                            "服务器副本已删除；请在账户页草稿列表选择保留或删除本地。",
                            Color32::from_rgb(210, 90, 60),
                        ),
                        DraftSyncState::LocalDirty => {
                            ("本地修改待同步到服务器。", Color32::from_rgb(190, 140, 40))
                        }
                        DraftSyncState::LocalOnly => ("仅本地草稿，尚未上传。", Color32::GRAY),
                        DraftSyncState::Synced => ("已同步到服务器草稿箱。", Color32::GRAY),
                        DraftSyncState::DeletedLocal => {
                            ("本地已删除，等待服务器确认。", Color32::GRAY)
                        }
                    };
                    ui.label(RichText::new(text).small().color(color));
                    if let Some(error) = error {
                        ui.label(
                            RichText::new(error)
                                .small()
                                .color(Color32::from_rgb(210, 90, 60)),
                        );
                    }
                    ui.add_space(4.0);
                }
                render_form(compose, ui, &from_email, &mut actions);
            });

        if !open {
            if compose.is_dirty() && !compose.is_empty() {
                // 窗口 X 也走未保存确认；下一帧重新打开确认界面。
                compose.confirm_close = true;
            } else {
                actions.close = true;
            }
        }

        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            actions.attachment_notice = add_attachments(compose, dropped);
        }

        if actions.attach_files
            && let Some(files) = rfd::FileDialog::new().pick_files()
        {
            actions.attachment_notice = add_attachments(compose, files);
        }
    }

    if let Some(notice) = actions.attachment_notice.take() {
        state.toast(ToastKind::Warn, notice);
    }
    if actions.discard {
        state.dialog.close();
        return;
    }

    let Dialog::Compose(compose) = &mut state.dialog else {
        return;
    };

    if actions.save {
        match build_outgoing(compose, from.clone(), true) {
            Ok(outgoing) => {
                compose.status = ComposeStatus::Saving;
                compose.error = None;
                commands.push(Command::SaveDraft(outgoing));
            }
            Err(err) => compose.error = Some(err),
        }
    }

    if actions.save_to_server
        && let Some(id) = compose.draft_id
    {
        compose.error = None;
        commands.push(Command::SaveDraftToServer(id));
    }

    if actions.send {
        match build_outgoing(compose, from, false) {
            Ok(outgoing) => {
                compose.status = ComposeStatus::Sending;
                compose.error = None;
                commands.push(Command::Send(outgoing));
            }
            Err(err) => compose.error = Some(err),
        }
    }

    if actions.close {
        state.dialog.close();
    }
}

fn render_form(
    compose: &mut ComposeState,
    ui: &mut egui::Ui,
    from_email: &str,
    actions: &mut ComposeActions,
) {
    if compose.confirm_close {
        ui.label(
            RichText::new("这封邮件有未保存的修改。")
                .strong()
                .color(Color32::from_rgb(190, 140, 40)),
        );
        ui.horizontal(|ui| {
            if ui.button("保存草稿并关闭").clicked() {
                actions.save = true;
                actions.close = true;
            }
            if ui.button("放弃修改").clicked() {
                actions.discard = true;
            }
            if ui.button("继续编辑").clicked() {
                compose.confirm_close = false;
            }
        });
        ui.separator();
    }

    if compose.confirm_resend {
        ui.label(
            RichText::new("该邮件可能已经发送过。确定要重新发送吗？将复用原 Message-ID。")
                .strong()
                .color(Color32::from_rgb(210, 90, 60)),
        );
        ui.horizontal(|ui| {
            if ui.button("确定重发").clicked() {
                actions.send = true;
                compose.confirm_resend = false;
            }
            if ui.button("取消").clicked() {
                compose.confirm_resend = false;
            }
        });
        ui.separator();
    }

    // 底部操作栏固定在窗口底部：`exact_height` 只按一行按钮的高度预留，
    // 不随正文或附件行数变化。
    let actions_height = ui.spacing().interact_size.y + 6.0 + ui.spacing().item_spacing.y;
    egui::TopBottomPanel::bottom("compose_actions_panel")
        .frame(egui::Frame::NONE)
        .exact_height(actions_height)
        .show_inside(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let busy = compose.status != ComposeStatus::Editing;
                if ui.add_enabled(!busy, egui::Button::new("发送")).clicked() {
                    if compose.may_have_been_sent {
                        compose.confirm_resend = true;
                    } else {
                        actions.send = true;
                    }
                }
                if ui
                    .add_enabled(!busy, egui::Button::new("保存草稿"))
                    .clicked()
                {
                    actions.save = true;
                }
                if ui
                    .add_enabled(
                        !busy && compose.draft_id.is_some(),
                        egui::Button::new("保存到服务器草稿"),
                    )
                    .clicked()
                {
                    actions.save_to_server = true;
                }
                if ui.button("关闭").clicked() {
                    if compose.is_dirty() && !compose.is_empty() {
                        compose.confirm_close = true;
                    } else {
                        actions.close = true;
                    }
                }
                if busy {
                    ui.spinner();
                    ui.label(
                        RichText::new(if compose.status == ComposeStatus::Sending {
                            "正在发送…"
                        } else {
                            "正在保存…"
                        })
                        .small()
                        .color(Color32::GRAY),
                    );
                }
            });
        });

    // 表单、正文、附件与状态提示共用一个滚动区。滚动区是窗口内容的最后一个
    // 元素并占满剩余高度，窗口内容高度恒定；旧实现把附件等追加在正文滚动区
    // 之后，窗口内容每帧都比窗口本身高一点，egui 的 `Resize` 会逐帧放大窗口，
    // 直到与视口等高而无法拖动。
    let available = ui.available_size();
    egui::ScrollArea::vertical()
        .id_salt("compose_form_scroll")
        .auto_shrink([false, false])
        .max_height(available.y.max(120.0))
        .show(ui, |ui| {
            egui::Grid::new("compose_form")
                .num_columns(2)
                .spacing([8.0, 6.0])
                .show(ui, |ui| {
                    ui.label("发件人");
                    ui.label(RichText::new(from_email).color(Color32::GRAY));
                    ui.end_row();

                    ui.label("收件人");
                    ui.add(
                        egui::TextEdit::singleline(&mut compose.to)
                            .hint_text("a@example.com, Name <b@example.com>")
                            .desired_width(ui.available_width() - 20.0),
                    );
                    ui.end_row();

                    ui.label("抄送");
                    ui.add(
                        egui::TextEdit::singleline(&mut compose.cc)
                            .desired_width(ui.available_width() - 20.0),
                    );
                    ui.end_row();

                    ui.label("密送");
                    ui.add(
                        egui::TextEdit::singleline(&mut compose.bcc)
                            .desired_width(ui.available_width() - 20.0),
                    );
                    ui.end_row();

                    ui.label("主题");
                    ui.add(
                        egui::TextEdit::singleline(&mut compose.subject)
                            .desired_width(ui.available_width() - 20.0),
                    );
                    ui.end_row();
                });

            ui.separator();
            ui.add(
                egui::TextEdit::multiline(&mut compose.body)
                    .hint_text("纯文本正文（UTF-8）")
                    .desired_width(f32::INFINITY)
                    .desired_rows(12),
            );

            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("📎 添加附件").clicked() {
                    actions.attach_files = true;
                }
                ui.label(
                    RichText::new(format!(
                        "单文件 ≤ {}，单封合计 ≤ {}",
                        crate::app::human_bytes(MAX_ATTACHMENT_BYTES),
                        crate::app::human_bytes(MAX_ATTACHMENT_BYTES)
                    ))
                    .small()
                    .color(Color32::GRAY),
                );
            });

            let mut remove: Option<usize> = None;
            for (index, path) in compose.attachments.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label("📎");
                    ui.label(
                        path.file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| path.display().to_string()),
                    );
                    if let Ok(meta) = std::fs::metadata(path) {
                        ui.label(
                            RichText::new(crate::app::human_bytes(meta.len()))
                                .small()
                                .color(Color32::GRAY),
                        );
                    }
                    if ui.small_button("移除").clicked() {
                        remove = Some(index);
                    }
                });
            }
            if let Some(index) = remove {
                compose.attachments.remove(index);
            }

            if let Some(error) = &compose.error {
                ui.add_space(4.0);
                ui.label(RichText::new(error).color(Color32::from_rgb(210, 80, 80)));
            }
            if let Some(failure) = &compose.last_failure {
                ui.label(
                    RichText::new(format!("上次发送失败：{failure}"))
                        .small()
                        .color(Color32::from_rgb(190, 140, 40)),
                );
            }
        });
}

fn add_attachments(compose: &mut ComposeState, files: Vec<PathBuf>) -> Option<String> {
    let mut notice = None;
    let mut sizes: Vec<u64> = compose
        .attachments
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len()))
        .collect();
    for path in files {
        let size = match std::fs::metadata(&path) {
            Ok(meta) => meta.len(),
            Err(err) => {
                notice = Some(format!("无法读取 {}：{err}", path.display()));
                continue;
            }
        };
        sizes.push(size);
        if check_attachment_limits(&sizes).is_err() {
            notice = Some("附件超过 25 MB 限制（单文件与单封合计均为 25 MB）".into());
            sizes.pop();
            continue;
        }
        if !compose.attachments.contains(&path) {
            compose.attachments.push(path);
        }
    }
    notice
}

fn build_outgoing(
    compose: &ComposeState,
    from: Address,
    allow_empty_to: bool,
) -> Result<OutgoingMessage, String> {
    let to = parse_address_list(&compose.to);
    let cc = parse_address_list(&compose.cc);
    let bcc = parse_address_list(&compose.bcc);
    if to.is_empty() && !allow_empty_to {
        return Err("至少需要一位收件人".into());
    }
    if to
        .iter()
        .chain(cc.iter())
        .chain(bcc.iter())
        .any(|a| !crate::state::is_valid_addr(&a.addr))
    {
        return Err("存在格式不正确的邮箱地址".into());
    }

    let mut sizes = Vec::new();
    for path in &compose.attachments {
        let meta = std::fs::metadata(path)
            .map_err(|err| format!("无法读取附件 {}：{err}", path.display()))?;
        sizes.push(meta.len());
    }
    check_attachment_limits(&sizes).map_err(|err| err.user_message())?;

    Ok(OutgoingMessage {
        account: compose.account,
        draft_id: compose.draft_id,
        message_id: compose.message_id.clone(),
        from,
        to,
        cc,
        bcc,
        subject: compose.subject.clone(),
        body_text: compose.body.clone(),
        attachments: compose.attachments.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ComposeState;

    #[test]
    fn compose_window_does_not_grow_with_attachments() {
        let ctx = egui::Context::default();
        let probe = egui::Id::new("compose_attachments_growth_probe");
        let mut compose = ComposeState::new(me_core::AccountId(1), "me@example.com");
        for index in 0..10 {
            compose
                .attachments
                .push(PathBuf::from(format!("C:/nonexistent/file-{index}.bin")));
        }
        let mut size = egui::Vec2::ZERO;
        for _ in 0..8 {
            let _ = ctx.run(Default::default(), |ctx| {
                egui::Window::new("写邮件")
                    .id(probe)
                    .default_size([680.0, 560.0])
                    .resizable(true)
                    .show(ctx, |ui| {
                        let mut actions = ComposeActions::default();
                        render_form(&mut compose, ui, "me@example.com", &mut actions);
                    });
            });
            if let Some(rect) = ctx.memory(|m| m.area_rect(probe)) {
                size = rect.size();
            }
        }
        assert!(
            size.y <= 620.0,
            "附件不应把写信窗口逐帧撑高，实际高度 {:.1}",
            size.y
        );
    }
}
