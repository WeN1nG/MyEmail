//! 通用确认框、首次启动的明文缓存告知、keyring 降级提示。

use eframe::egui::{self, Align, Color32, Layout, RichText};

use me_core::Command;

use crate::state::{ConfirmAction, Dialog, ToastKind, UiState};

use super::settings::open_path;

pub fn show(state: &mut UiState, commands: &mut Vec<Command>, ctx: &egui::Context) {
    if state.show_plaintext_notice && !state.plaintext_notice_ack {
        plaintext_notice(state, commands, ctx);
    }
    credential_fallback(state, commands, ctx);
    oauth_login(state, commands, ctx);
    weather_config(state, commands, ctx);
    confirm_dialog(state, commands, ctx);
}

fn credential_fallback(state: &mut UiState, commands: &mut Vec<Command>, ctx: &egui::Context) {
    let account = match &state.dialog {
        Dialog::CredentialFallback { account } => *account,
        _ => return,
    };
    let email = state
        .account(account)
        .map(|account| account.email.clone())
        .unwrap_or_else(|| format!("账户 {}", account.0));

    let mut keep = false;
    let mut cancel = false;
    egui::Window::new("系统密钥库不可用")
        .id(egui::Id::new("me_credential_fallback"))
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            ui.label(format!("无法把 {email} 的凭证/刷新令牌写入系统密钥库。"));
            ui.add_space(6.0);
            ui.label(
                "凭证目前只保存在本次会话内存中：退出应用后需要重新输入。\n\
                 不会以明文写入磁盘。",
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("仅本次会话记忆").clicked() {
                    keep = true;
                }
                if ui.button("取消添加").clicked() {
                    cancel = true;
                }
            });
        });

    if keep {
        state.dialog.close();
        state.toast(
            ToastKind::Info,
            "凭证仅保存在本次会话；退出后需要重新输入或重新授权。",
        );
    }
    if cancel {
        state.dialog.close();
        commands.push(Command::DeleteAccount {
            id: account,
            delete_drafts: false,
        });
        state.toast(ToastKind::Info, "已取消添加账户");
    }
}

/// 和风天气配置弹窗：**只有 API Host 与 API Key 两个输入框**。
///
/// 用户不需要知道「城市搜索 Host」——官方已把 GeoAPI 收进同一个专属域名，
/// 路径差异由程序自动探测（见 `me_proto::qweather`）。
///
/// **Host 可以只填裸域名**（控制台复制到的就是这样）：程序负责补
/// `https://`、拼端点路径，并把最终请求地址显示出来。
///
/// 从天气页的「配置密钥」按钮打开（主页面未配置时也用同一个入口）。
/// Key 只提交给后端写密钥库，**不留在界面状态里**——保存后立刻清空。
fn weather_config(state: &mut UiState, commands: &mut Vec<Command>, ctx: &egui::Context) {
    if !matches!(state.dialog, Dialog::WeatherConfig) {
        return;
    }

    let mut save = false;
    let mut cancel = false;
    let mut test = false;
    egui::Window::new("配置和风天气")
        .id(egui::Id::new("me_weather_config"))
        .collapsible(false)
        .resizable(false)
        .default_width(460.0)
        .show(ctx, |ui| {
            ui.label(RichText::new("天气数据来自和风天气（QWeather）。").size(13.0));
            ui.label(
                RichText::new(
                    "1. 登录 console.qweather.com 创建项目，在「凭据」里生成 API KEY\n\
                     2. 在控制台「设置」页复制你的专属 API Host（只有域名，https:// 可省略）\n\
                     3. 把两者填入下面，可先「测试连接」确认可用",
                )
                .size(12.0)
                .color(Color32::GRAY),
            );
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.label("API Host");
                ui.add(
                    egui::TextEdit::singleline(&mut state.weather.host_input.api_host)
                        .desired_width(280.0)
                        .hint_text("abcxyz.qweatherapi.com"),
                );
            });
            ui.label(
                RichText::new(me_core::HOST_HINT)
                    .size(11.0)
                    .color(Color32::GRAY),
            );

            // 实时预览：Host 规范化后程序实际会请求哪个地址。
            // 协议与路径都由程序决定，用户只管填域名。
            let host_input = state.weather.host_input.api_host.trim();
            if !host_input.is_empty() {
                match me_core::weather::normalize_host(host_input) {
                    Ok(host) => {
                        ui.label(
                            RichText::new(format!(
                                "将请求：{host}{}（城市搜索：{host}{}）",
                                me_core::PATH_NOW,
                                me_core::PATH_CITY_LOOKUP
                            ))
                            .size(11.0)
                            .color(Color32::GRAY),
                        );
                    }
                    Err(err) => {
                        ui.label(
                            RichText::new(err.user_message())
                                .size(11.0)
                                .color(Color32::from_rgb(200, 80, 80)),
                        );
                    }
                }
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("API Key");
                ui.add(
                    egui::TextEdit::singleline(&mut state.weather.key_input)
                        .password(true)
                        .desired_width(280.0),
                );
            });
            ui.label(
                RichText::new(
                    "Key 只保存在系统密钥库，不写入数据库、不写日志；密钥库不可用时仅本次会话有效。",
                )
                .size(11.0)
                .color(Color32::GRAY),
            );

            // 上一次测试连接的结果。
            if let Some((ok, message)) = &state.weather.test_result {
                ui.add_space(6.0);
                let color = if *ok {
                    Color32::from_rgb(70, 160, 90)
                } else {
                    Color32::from_rgb(200, 80, 80)
                };
                ui.label(RichText::new(message.clone()).size(12.0).color(color));
            }

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("保存").clicked() {
                    save = true;
                }
                if ui
                    .add_enabled(
                        !state.weather.testing,
                        egui::Button::new(if state.weather.testing {
                            "测试中…"
                        } else {
                            "测试连接"
                        }),
                    )
                    .clicked()
                {
                    test = true;
                }
                if ui.button("取消").clicked() {
                    cancel = true;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!(
                            "当前状态：{}",
                            if state.weather.key_present {
                                "已配置 Key"
                            } else {
                                "尚未配置 Key"
                            }
                        ))
                        .small()
                        .color(Color32::GRAY),
                    );
                });
            });
        });

    if cancel {
        state.dialog.close();
        state.weather.key_input.clear();
        return;
    }

    // 测试连接：用当前输入的 Host + Key 发一次请求，**不落库**。
    if test {
        let host = state.weather.host_input.api_host.trim().to_string();
        let key = state.weather.key_input.trim().to_string();
        state.weather.testing = true;
        state.weather.test_result = None;
        commands.push(Command::TestWeatherConnection { host, key });
        return;
    }

    if save {
        let host = state.weather.host_input.api_host.trim().to_string();
        let key = state.weather.key_input.trim().to_string();
        // Host 为空就退回官方公共域名，别让用户因为只填了 Key 而完全不能用。
        let host = if host.is_empty() {
            me_core::DEFAULT_API_HOST.to_string()
        } else {
            host
        };
        // 先本地校验：Host 不合法就别提交，弹窗保持打开并显示原因。
        if let Err(err) = me_core::weather::normalize_host(&host) {
            state.weather.test_result = Some((false, err.user_message()));
            return;
        }
        commands.push(Command::SetWeatherConfig {
            host: host.clone(),
            // Key 留空表示「不改动已保存的 Key」。
            key: if key.is_empty() { None } else { Some(key) },
        });
        // Key 不留在界面状态里。
        state.weather.key_input.clear();
        // Host 的最终生效值由后端 `WeatherConfigChanged` 事件回填，
        // 这里不抢先改，免得显示的值和实际生效的不一致。
        state.weather.save_requested = true;
        state.dialog.close();
    }
}

fn oauth_login(state: &mut UiState, commands: &mut Vec<Command>, ctx: &egui::Context) {
    let (
        flow_id,
        provider,
        mode,
        verification_uri,
        user_code,
        expires_at,
        message,
        busy,
        reauth_account,
        email_hint,
    ) = match &state.dialog {
        Dialog::OAuthLogin(login) => (
            login.flow_id,
            login.provider,
            login.mode,
            login.verification_uri.clone(),
            login.user_code.clone(),
            login.expires_at,
            login.message.clone(),
            login.busy,
            login.reauth_account,
            login.email_hint.clone(),
        ),
        _ => return,
    };

    let mut open_url = None;
    let mut cancel = false;
    egui::Window::new(format!("{} 授权", provider.label()))
        .id(egui::Id::new("me_oauth_login"))
        .collapsible(false)
        .resizable(false)
        .default_width(520.0)
        .show(ctx, |ui| {
            let is_reauth = reauth_account.is_some();
            ui.label(if is_reauth {
                "重新授权后，令牌会更新到同一账户。"
            } else {
                "授权完成后将自动创建账户并开始首次同步。"
            });
            if !email_hint.is_empty() {
                ui.label(
                    RichText::new(format!("账户邮箱：{email_hint}"))
                        .small()
                        .color(Color32::GRAY),
                );
            }
            ui.add_space(6.0);
            match mode {
                me_core::OAuthFlowMode::Loopback => {
                    ui.label("已在系统浏览器打开授权页面；完成登录后会自动返回。");
                }
                me_core::OAuthFlowMode::DeviceCode => {
                    ui.label("在浏览器打开下面的地址，并输入设备码。");
                }
            }
            if !verification_uri.is_empty() {
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("授权地址").small().color(Color32::GRAY));
                    if ui.link(&verification_uri).clicked() {
                        open_url = Some(verification_uri.clone());
                    }
                });
            }
            if let Some(code) = &user_code {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("设备码");
                    ui.label(
                        RichText::new(code)
                            .monospace()
                            .size(20.0)
                            .strong()
                            .color(Color32::from_rgb(40, 110, 190)),
                    );
                });
            }
            if expires_at > 0 {
                let remaining = expires_at.saturating_sub(me_core::now_unix());
                ui.label(
                    RichText::new(format!("剩余 {} 秒", remaining.max(0)))
                        .small()
                        .color(Color32::GRAY),
                );
            }
            if !message.trim().is_empty() {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(&message)
                        .small()
                        .color(Color32::from_rgb(190, 120, 40)),
                );
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if !verification_uri.is_empty() && ui.button("在浏览器打开").clicked() {
                    open_url = Some(verification_uri.clone());
                }
                if ui.button("取消").clicked() {
                    cancel = true;
                }
                if busy {
                    ui.spinner();
                }
            });
        });

    if let Some(url) = open_url {
        ctx.open_url(egui::OpenUrl::new_tab(url));
    }
    if cancel {
        commands.push(Command::CancelOAuth {
            flow_id: flow_id.unwrap_or(0),
        });
        state.dialog.close();
        state.toast(ToastKind::Info, "已取消授权");
    }
}

fn plaintext_notice(state: &mut UiState, commands: &mut Vec<Command>, ctx: &egui::Context) {
    egui::Window::new("关于本地缓存")
        .id(egui::Id::new("me_plaintext_notice"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.heading("本地缓存为明文保存");
            ui.add_space(6.0);
            ui.label(
                "MyEmail 把邮件摘要、正文、原始邮件与附件以未加密形式保存在本机。\n\
                 任何能访问本机账户目录的程序或用户都可以读取这些文件。\n\
                 本应用不承诺加密存储或合规存储。",
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new("可以在“设置 → 本地缓存”中清理缓存。")
                    .small()
                    .color(Color32::GRAY),
            );
            ui.add_space(10.0);
            if ui.button("我已知晓").clicked() {
                state.plaintext_notice_ack = true;
                state.show_plaintext_notice = false;
                // 落库到 meta，下次启动不再提示。
                commands.push(Command::AcknowledgePlaintextNotice);
            }
        });
}

fn confirm_dialog(state: &mut UiState, commands: &mut Vec<Command>, ctx: &egui::Context) {
    let Dialog::Confirm(confirm) = &state.dialog else {
        return;
    };
    let mut apply = false;
    let mut cancel = false;

    egui::Window::new(&confirm.title)
        .id(egui::Id::new("me_confirm"))
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            ui.label(&confirm.body);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let text = if confirm.danger {
                    "确认（不可恢复）"
                } else {
                    "确认"
                };
                let button = if confirm.danger {
                    egui::Button::new(RichText::new(text).color(Color32::from_rgb(210, 80, 80)))
                } else {
                    egui::Button::new(text)
                };
                if ui.add(button).clicked() {
                    apply = true;
                }
                if ui.button("取消").clicked() {
                    cancel = true;
                }
            });
        });

    if cancel {
        state.dialog.close();
        return;
    }
    if !apply {
        return;
    }

    let action = match &state.dialog {
        Dialog::Confirm(confirm) => confirm.action.clone(),
        _ => return,
    };

    match action {
        ConfirmAction::DeleteAccount { id, delete_drafts } => {
            commands.push(Command::DeleteAccount { id, delete_drafts });
        }
        ConfirmAction::DeleteDraft { id } => {
            commands.push(Command::DeleteDraft { id });
        }
        ConfirmAction::EmptyTrash { folder } => {
            commands.push(Command::EmptyTrash(folder));
        }
        ConfirmAction::MoveToTrash { ids } => {
            commands.push(Command::MoveToTrash(ids));
        }
        ConfirmAction::OpenAttachment { path } => {
            open_path(&path);
        }
        ConfirmAction::ClearCacheAll => {
            commands.push(Command::ClearCache(None));
            state.toast(ToastKind::Info, "正在清理缓存…");
        }
        ConfirmAction::ClearCacheAccount(account) => {
            commands.push(Command::ClearCache(Some(account)));
            state.toast(ToastKind::Info, "正在清理当前账户缓存…");
        }
    }
    state.dialog.close();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 跑一帧，收集绘制出来的文本。
    fn dialog_texts(state: &mut UiState) -> Vec<String> {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            ..Default::default()
        };
        // egui 的 Window 首帧可能还没排好版（读者窗口的测试也循环多帧），
        // 所以跑几帧把文本收集起来。
        let mut texts = Vec::new();
        for _ in 0..8 {
            let output = ctx.run(input.clone(), |ctx| {
                egui::CentralPanel::default().show(ctx, |_| {
                    show(state, &mut Vec::new(), ctx);
                });
            });
            for clipped in &output.shapes {
                collect_texts(&clipped.shape, &mut texts);
            }
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

    #[test]
    fn weather_config_dialog_is_hidden_until_opened() {
        let mut state = UiState::default();
        let texts = dialog_texts(&mut state);
        assert!(
            !texts.iter().any(|text| text.contains("配置和风天气")),
            "没打开时不该显示天气配置弹窗，实际 {texts:?}"
        );
    }

    #[test]
    fn weather_config_dialog_shows_hosts_key_and_hint() {
        let mut state = UiState {
            dialog: Dialog::WeatherConfig,
            ..Default::default()
        };
        state.weather.open_config();
        let texts = dialog_texts(&mut state);
        assert!(
            texts.iter().any(|text| text.contains("配置和风天气")),
            "应弹出配置弹窗，实际 {texts:?}"
        );
        assert!(
            texts.iter().any(|text| text.contains("API Host")),
            "应有 API Host 输入"
        );
        assert!(
            texts.iter().any(|text| text.contains("API Key")),
            "应有 API Key 输入"
        );
        assert!(
            texts.iter().any(|text| text.contains("qweatherapi.com")),
            "应提示用户在哪里取专属 Host"
        );
        assert!(
            texts.iter().any(|text| text.contains("保存")),
            "应有保存按钮"
        );
        assert!(
            texts.iter().any(|text| text.contains("测试连接")),
            "应有测试连接按钮"
        );
        // 城市搜索 Host 由程序自动处理，不该让用户填。
        assert!(
            !texts.iter().any(|text| text.contains("城市搜索 Host")),
            "不应要求用户填城市搜索 Host，实际 {texts:?}"
        );
    }

    /// 用户只填裸域名时，弹窗要能看到程序实际会请求的地址。
    #[test]
    fn weather_config_previews_the_request_url_for_a_bare_host() {
        let mut state = UiState {
            dialog: Dialog::WeatherConfig,
            ..Default::default()
        };
        state.weather.open_config();
        // 模拟用户把控制台里的裸域名填进 Host 输入框。
        state.weather.host_input.api_host = "abcxyz.qweatherapi.com".into();
        let texts = dialog_texts(&mut state);
        assert!(
            texts
                .iter()
                .any(|text| text.contains("https://abcxyz.qweatherapi.com/v7/weather/now")),
            "应预览最终请求地址，实际 {texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|text| text.contains("/geo/v2/city/lookup")),
            "应预览城市搜索地址，实际 {texts:?}"
        );
    }

    /// 内网 Host 要在弹窗里就给出原因，而不是提交后才报错。
    #[test]
    fn weather_config_warns_about_a_private_host() {
        let mut state = UiState {
            dialog: Dialog::WeatherConfig,
            ..Default::default()
        };
        state.weather.open_config();
        state.weather.host_input.api_host = "192.168.1.10".into();
        let texts = dialog_texts(&mut state);
        assert!(
            texts.iter().any(|text| text.contains("内网")),
            "应提示不能指向内网，实际 {texts:?}"
        );
    }
}
