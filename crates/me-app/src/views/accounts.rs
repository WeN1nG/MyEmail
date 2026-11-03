//! 账户页：列表、编辑、TLS 高级选项、连接测试、删除确认、草稿提示。

use eframe::egui::{self, Color32, RichText};

use me_core::{
    AccountId, AuthMethod, Command, DraftConflictResolution, DraftDeletionResolution, DraftStatus,
    DraftSyncState, OAuthFlowMode, OAuthProvider, TlsMode,
};

use crate::state::{
    AccountEditor, ConfirmAction, ConfirmDialog, Dialog, OAuthLoginState, ToastKind, UiState,
};

pub fn show(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    ui.heading("账户");
    ui.add_space(4.0);
    let oauth_in_progress = matches!(state.dialog, Dialog::OAuthLogin(_));
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!oauth_in_progress, egui::Button::new("＋ 添加账户"))
            .clicked()
        {
            state.dialog = Dialog::AccountEditor(AccountEditor::new());
        }
        for provider in OAuthProvider::ALL {
            if ui
                .add_enabled(
                    !oauth_in_progress,
                    egui::Button::new(format!("使用 {} 登录", provider.label())),
                )
                .clicked()
            {
                start_oauth(
                    state,
                    commands,
                    provider,
                    OAuthFlowMode::Loopback,
                    None,
                    String::new(),
                );
            }
            if provider.supports_device_code()
                && ui
                    .add_enabled(!oauth_in_progress, egui::Button::new("设备码"))
                    .clicked()
            {
                start_oauth(
                    state,
                    commands,
                    provider,
                    OAuthFlowMode::DeviceCode,
                    None,
                    String::new(),
                );
            }
        }
    });
    ui.label(
        RichText::new(
            "密码/授权码用于 QQ、163 等；Gmail / Outlook 使用 OAuth2（回环重定向，Microsoft 另提供设备码）。",
        )
        .small()
        .color(Color32::GRAY),
    );
    ui.add_space(6.0);

    if state.accounts.is_empty() {
        ui.add_space(30.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("还没有账户").heading());
            ui.label(RichText::new("添加账户后即可同步邮件。").color(Color32::GRAY));
        });
        return;
    }

    let mut edit: Option<AccountId> = None;
    let mut delete: Option<AccountId> = None;
    let mut test: Option<AccountId> = None;
    let mut reauth: Option<(AccountId, OAuthProvider, String)> = None;

    egui::ScrollArea::vertical()
        .id_salt("accounts_list")
        .show(ui, |ui| {
            for account in state.accounts.clone() {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&account.email).strong().size(15.0));
                                if account.imap.tls == TlsMode::None
                                    || account.smtp.tls == TlsMode::None
                                {
                                    ui.label(
                                        RichText::new("⚠ 明文 TLS")
                                            .small()
                                            .strong()
                                            .color(Color32::from_rgb(210, 90, 60)),
                                    );
                                }
                                if let Some(provider) = account.oauth_provider {
                                    ui.label(
                                        RichText::new(format!("OAuth2 · {}", provider.label()))
                                            .small()
                                            .color(Color32::from_rgb(60, 140, 220)),
                                    );
                                }
                                if account.needs_reauth {
                                    ui.label(
                                        RichText::new("⚠ 需要重新授权")
                                            .small()
                                            .strong()
                                            .color(Color32::from_rgb(210, 90, 60)),
                                    );
                                }
                                let status = state
                                    .connection
                                    .get(&account.id)
                                    .map(|s| s.label())
                                    .unwrap_or("未连接");
                                ui.label(RichText::new(status).small().color(Color32::GRAY));
                            });
                            if let Some(name) = &account.display_name
                                && !name.trim().is_empty()
                            {
                                ui.label(RichText::new(name).small());
                            }
                            ui.label(
                                RichText::new(format!(
                                    "IMAP {}:{} ({:?})　SMTP {}:{} ({:?})",
                                    account.imap.host,
                                    account.imap.port,
                                    account.imap.tls,
                                    account.smtp.host,
                                    account.smtp.port,
                                    account.smtp.tls
                                ))
                                .small()
                                .color(Color32::GRAY),
                            );
                        });

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("删除").clicked() {
                                delete = Some(account.id);
                            }
                            if ui.button("编辑").clicked() {
                                edit = Some(account.id);
                            }
                            if ui.button("测试连接").clicked() {
                                test = Some(account.id);
                            }
                            if account.auth_method == AuthMethod::OAuth2
                                && let Some(provider) = account.oauth_provider
                                && ui.button("重新授权").clicked()
                            {
                                reauth = Some((account.id, provider, account.email.clone()));
                            }
                        });
                    });
                });
            }
        });

    if let Some(id) = edit
        && let Some(account) = state.account(id)
    {
        state.dialog = Dialog::AccountEditor(AccountEditor::from_account(account));
    }
    if let Some((id, provider, email)) = reauth {
        start_oauth(
            state,
            commands,
            provider,
            OAuthFlowMode::Loopback,
            Some(id),
            email,
        );
    }
    if let Some(id) = test {
        commands.push(Command::TestConnection(id));
        state.toast(ToastKind::Info, "正在测试 IMAP 与 SMTP 连接…");
    }
    if let Some(id) = delete {
        let draft_count = state
            .drafts
            .iter()
            .filter(|d| d.account == id && d.status == DraftStatus::Draft)
            .count() as u32;
        if draft_count > 0 {
            state.dialog = Dialog::Confirm(ConfirmDialog {
                title: "删除账户".into(),
                body: format!(
                    "该账户还有 {draft_count} 封草稿。默认拒绝删除。\n\n如确认删除账户并连同草稿一并删除，请点击“连同草稿删除”。草稿不可恢复。"
                ),
                action: ConfirmAction::DeleteAccount {
                    id,
                    delete_drafts: true,
                },
                danger: true,
            });
        } else {
            state.dialog = Dialog::Confirm(ConfirmDialog {
                title: "删除账户".into(),
                body: "将删除该账户的本地缓存与设置（不含服务器邮件）。".into(),
                action: ConfirmAction::DeleteAccount {
                    id,
                    delete_drafts: false,
                },
                danger: true,
            });
        }
    }

    ui.add_space(12.0);
    drafts_section(state, commands, ui);
}

fn drafts_section(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    if state.drafts.is_empty() {
        return;
    }
    ui.separator();
    ui.heading("草稿");
    ui.label(
        RichText::new(
            "本地草稿是唯一事实来源；自动镜像开启时会同步到服务器草稿箱，冲突始终保留两份。",
        )
        .small()
        .color(Color32::GRAY),
    );
    egui::ScrollArea::vertical()
        .id_salt("drafts_list")
        .max_height(300.0)
        .show(ui, |ui| {
            for draft in state.drafts.clone() {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(if draft.status == DraftStatus::Sent {
                            "✓"
                        } else {
                            "📝"
                        });
                        let subject = draft
                            .subject
                            .as_deref()
                            .filter(|s| !s.trim().is_empty())
                            .unwrap_or("(无主题)");
                        ui.label(subject);
                        ui.label(
                            RichText::new(format!(
                                "{} · {} · {}",
                                draft.status.as_str(),
                                draft.sync_state.label(),
                                me_core::format_local_datetime(draft.updated_at)
                            ))
                            .small()
                            .color(Color32::GRAY),
                        );
                        if let Some(err) = &draft.last_error {
                            ui.label(
                                RichText::new(err)
                                    .small()
                                    .color(Color32::from_rgb(210, 90, 60)),
                            );
                        }
                    });

                    if draft.status == DraftStatus::Draft {
                        ui.horizontal_wrapped(|ui| {
                            if ui.small_button("编辑").clicked() {
                                commands.push(Command::LoadDraft(draft.id));
                            }
                            if ui.small_button("保存到服务器").clicked() {
                                commands.push(Command::SaveDraftToServer(draft.id));
                            }
                            if ui.small_button("重试同步").clicked() {
                                commands.push(Command::RetryDraftSync { draft: draft.id });
                            }
                            if ui.small_button("删除").clicked() {
                                state.dialog = Dialog::Confirm(ConfirmDialog {
                                    title: "删除草稿".into(),
                                    body: "草稿将从本地删除；如已同步到服务器，也会尝试删除服务器副本。"
                                        .into(),
                                    action: ConfirmAction::DeleteDraft { id: draft.id },
                                    danger: true,
                                });
                            }
                        });
                    } else if draft.status == DraftStatus::Sent
                        && draft.last_error.is_some()
                        && ui.small_button("重试归档").clicked()
                    {
                        commands.push(Command::RetryArchive(draft.id));
                    }

                    if draft.sync_state == DraftSyncState::Conflict {
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new("本地与服务器都修改过；请选择保留方式。")
                                .small()
                                .color(Color32::from_rgb(210, 90, 60)),
                        );
                        ui.horizontal_wrapped(|ui| {
                            if ui.small_button("保留本地").clicked() {
                                commands.push(Command::ResolveDraftConflict {
                                    draft: draft.id,
                                    resolution: DraftConflictResolution::KeepLocal,
                                });
                            }
                            if ui.small_button("使用服务器").clicked() {
                                commands.push(Command::ResolveDraftConflict {
                                    draft: draft.id,
                                    resolution: DraftConflictResolution::UseServer,
                                });
                            }
                            if ui.small_button("保留两份").clicked() {
                                commands.push(Command::ResolveDraftConflict {
                                    draft: draft.id,
                                    resolution: DraftConflictResolution::KeepBoth,
                                });
                            }
                        });
                    }
                    if draft.sync_state == DraftSyncState::DeletedServer {
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new("服务器副本已删除；本地副本仍保留。")
                                .small()
                                .color(Color32::from_rgb(210, 90, 60)),
                        );
                        ui.horizontal_wrapped(|ui| {
                            if ui.small_button("保留本地").clicked() {
                                commands.push(Command::ResolveDraftDeletion {
                                    draft: draft.id,
                                    resolution: DraftDeletionResolution::KeepLocal,
                                });
                            }
                            if ui.small_button("删除本地").clicked() {
                                commands.push(Command::ResolveDraftDeletion {
                                    draft: draft.id,
                                    resolution: DraftDeletionResolution::DeleteLocal,
                                });
                            }
                        });
                    }
                });
            }
        });
}

fn start_oauth(
    state: &mut UiState,
    commands: &mut Vec<Command>,
    provider: OAuthProvider,
    mode: OAuthFlowMode,
    account: Option<AccountId>,
    email_hint: String,
) {
    state.dialog = Dialog::OAuthLogin(OAuthLoginState::new(
        provider,
        mode,
        account,
        email_hint.clone(),
    ));
    commands.push(Command::StartOAuth {
        provider,
        mode,
        account,
        email_hint: Some(email_hint).filter(|value| !value.is_empty()),
    });
}

pub fn show_editor(state: &mut UiState, commands: &mut Vec<Command>, ctx: &egui::Context) {
    let Dialog::AccountEditor(editor) = &mut state.dialog else {
        return;
    };
    let mut close = false;
    let mut save = false;
    let mut test = false;
    let is_edit = editor.editing.is_some();

    let mut open = true;
    egui::Window::new(if is_edit { "编辑账户" } else { "添加账户" })
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(520.0)
        .show(ctx, |ui| {
            egui::Grid::new("account_form")
                .num_columns(2)
                .spacing([10.0, 8.0])
                .show(ui, |ui| {
                    ui.label("邮箱地址");
                    ui.add(
                        egui::TextEdit::singleline(&mut editor.email)
                            .hint_text("you@example.com")
                            .desired_width(320.0),
                    );
                    ui.end_row();

                    ui.label("显示名");
                    ui.add(
                        egui::TextEdit::singleline(&mut editor.display_name)
                            .desired_width(320.0),
                    );
                    ui.end_row();

                    ui.label("IMAP 服务器");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut editor.imap_host)
                                .hint_text("imap.example.com")
                                .desired_width(220.0),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut editor.imap_port)
                                .desired_width(60.0),
                        );
                        tls_combo(ui, "imap_tls", &mut editor.imap_tls);
                    });
                    ui.end_row();

                    ui.label("SMTP 服务器");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut editor.smtp_host)
                                .hint_text("smtp.example.com")
                                .desired_width(220.0),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut editor.smtp_port)
                                .desired_width(60.0),
                        );
                        tls_combo(ui, "smtp_tls", &mut editor.smtp_tls);
                    });
                    ui.end_row();

                    if editor.auth_method == AuthMethod::OAuth2 {
                        ui.label("认证方式");
                        ui.label("OAuth2（请在账户页点击“重新授权”更新令牌）");
                        ui.end_row();
                    } else {
                        ui.label("登录用户");
                        ui.add(
                            egui::TextEdit::singleline(&mut editor.auth_user)
                                .hint_text("留空则使用邮箱地址")
                                .desired_width(320.0),
                        );
                        ui.end_row();

                        ui.label("密码 / 授权码");
                        ui.add(
                            egui::TextEdit::singleline(&mut editor.auth_secret)
                                .password(true)
                                .hint_text(if is_edit {
                                    "留空表示保留现有凭证"
                                } else {
                                    "QQ/163 请填授权码"
                                })
                                .desired_width(320.0),
                        );
                        ui.end_row();

                        ui.label("认证方式");
                        egui::ComboBox::from_id_salt("auth_method")
                            .selected_text(match editor.auth_method {
                                AuthMethod::Password => "密码",
                                AuthMethod::AppPassword => "授权码",
                                AuthMethod::OAuth2 => "OAuth2",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut editor.auth_method,
                                    AuthMethod::Password,
                                    "密码",
                                );
                                ui.selectable_value(
                                    &mut editor.auth_method,
                                    AuthMethod::AppPassword,
                                    "授权码",
                                );
                            });
                        ui.end_row();
                    }
                });

            ui.add_space(4.0);
            ui.collapsing("高级：TLS 选项", |ui| {
                editor.advanced_open = true;
                ui.label(
                    RichText::new(
                        "默认只开放隐式 TLS 与 STARTTLS。明文传输会让凭据与邮件以未加密形式通过网络。",
                    )
                    .small()
                    .color(Color32::GRAY),
                );
                if editor.uses_plaintext_tls() {
                    ui.checkbox(
                        &mut editor.plaintext_ack,
                        "我了解凭据与邮件将以明文传输，并自行承担风险",
                    );
                    ui.label(
                        RichText::new("该账户会在账户列表中常驻“明文 TLS”风险标记。")
                            .small()
                            .color(Color32::from_rgb(210, 90, 60)),
                    );
                }
            });

            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "凭证存入系统密钥库；不可用时仅本次会话记忆，不落盘明文。Gmail / Outlook 请在账户页使用 OAuth2 登录。",
                )
                .small()
                .color(Color32::GRAY),
            );

            if let Some(error) = &editor.error {
                ui.add_space(4.0);
                ui.label(RichText::new(error).color(Color32::from_rgb(210, 80, 80)));
            }
            if let Some(result) = &editor.test_result {
                ui.add_space(4.0);
                ui.label(RichText::new(result).color(Color32::GRAY));
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let save_button = ui.add_enabled(!editor.busy, egui::Button::new("保存"));
                if save_button.clicked() {
                    match editor.validate() {
                        Ok(()) => save = true,
                        Err(err) => editor.error = Some(err),
                    }
                }
                if ui.add_enabled(editor.can_test && !editor.busy, egui::Button::new("测试连接")).clicked() {
                    test = true;
                }
                if ui.button("取消").clicked() {
                    close = true;
                }
                if editor.busy {
                    ui.spinner();
                }
            });
        });

    if close || !open {
        state.dialog.close();
        return;
    }

    if save {
        let draft = editor.to_draft();
        editor.busy = true;
        editor.error = None;
        match editor.editing {
            Some(id) => commands.push(Command::UpdateAccount { id, draft }),
            None => commands.push(Command::AddAccount(draft)),
        }
    }
    if test && let Some(id) = editor.editing {
        editor.busy = true;
        editor.test_result = None;
        commands.push(Command::TestConnection(id));
    }
}

fn tls_combo(ui: &mut egui::Ui, id: &str, value: &mut TlsMode) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(match value {
            TlsMode::Implicit => "隐式 TLS",
            TlsMode::StartTls => "STARTTLS",
            TlsMode::None => "明文",
        })
        .show_ui(ui, |ui| {
            ui.selectable_value(value, TlsMode::Implicit, "隐式 TLS");
            ui.selectable_value(value, TlsMode::StartTls, "STARTTLS");
            ui.selectable_value(value, TlsMode::None, "明文（不推荐）");
        });
}
