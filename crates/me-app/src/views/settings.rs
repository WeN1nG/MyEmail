//! 设置页：主题、后台补全、缓存占用与清理、重建索引、日志目录、明文缓存告知。

use std::path::Path;

use eframe::egui::{self, Color32, RichText};

use me_core::{Command, ThemeMode};

use crate::state::{ConfirmAction, ConfirmDialog, Dialog, ToastKind, UiState};

/// 设置页滚动区：铺满中央面板。
///
/// 默认 `auto_shrink` 会收窄到内容宽度，滚动条停在内容右侧而不是页面最右侧。
fn page_scroll_area() -> egui::ScrollArea {
    egui::ScrollArea::vertical()
        .id_salt("settings_page")
        .auto_shrink([false, false])
}

pub fn show(
    state: &mut UiState,
    commands: &mut Vec<Command>,
    cache_dir: Option<&Path>,
    ui: &mut egui::Ui,
) {
    ui.heading("设置");
    ui.add_space(6.0);

    page_scroll_area().show(ui, |ui| {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(RichText::new("外观").strong());
            ui.horizontal(|ui| {
                let mut theme = state.theme;
                ui.radio_value(&mut theme, ThemeMode::System, "跟随系统");
                ui.radio_value(&mut theme, ThemeMode::Light, "浅色");
                ui.radio_value(&mut theme, ThemeMode::Dark, "深色");
                if theme != state.theme {
                    state.theme = theme;
                    commands.push(Command::SetTheme(theme));
                }
            });
        });

        ui.add_space(8.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(RichText::new("同步与索引").strong());
            let mut backfill = state.body_backfill;
            if ui
                .checkbox(&mut backfill, "后台补全正文并写入全文索引")
                .changed()
            {
                state.body_backfill = backfill;
                commands.push(Command::SetBodyBackfill { enabled: backfill });
            }
            ui.label(
                RichText::new(
                    "关闭后，只有打开过的邮件正文可被搜索。同步触发为启动、手动刷新与实时同步。",
                )
                .small()
                .color(Color32::GRAY),
            );
            ui.add_space(4.0);
            let mut realtime = state.realtime_sync;
            if ui
                .checkbox(&mut realtime, "实时同步（IDLE，不可用时自动降级轮询）")
                .changed()
            {
                state.realtime_sync = realtime;
                commands.push(Command::SetRealtimeSync { enabled: realtime });
            }
            ui.horizontal(|ui| {
                ui.label("默认轮询间隔");
                ui.add(
                    egui::DragValue::new(&mut state.poll_interval_secs)
                        .range(me_core::MIN_POLL_SECS..=me_core::MAX_POLL_SECS)
                        .suffix(" 秒"),
                );
                if ui.button("应用").clicked() {
                    commands.push(Command::SetPollInterval {
                        secs: state.poll_interval_secs,
                    });
                }
            });
            ui.label(
                RichText::new(
                    "只监听收件箱（INBOX）；IDLE 连续失败 3 次后自动改用轮询。轮询会与手动同步共用连接。",
                )
                .small()
                .color(Color32::GRAY),
            );
            if !state.accounts.is_empty() {
                ui.add_space(6.0);
                ui.label(RichText::new("账户级实时覆盖").small().strong());
                for account in state.accounts.clone() {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(&account.email)
                                .small()
                                .color(Color32::GRAY),
                        );
                        let mut idle = account.idle_enabled;
                        ui.checkbox(&mut idle, "IDLE");
                        let mut custom = account.poll_interval_secs.is_some();
                        ui.checkbox(&mut custom, "自定义轮询");
                        let mut interval =
                            account.poll_interval_secs.unwrap_or(state.poll_interval_secs);
                        if custom {
                            ui.add(
                                egui::DragValue::new(&mut interval)
                                    .range(me_core::MIN_POLL_SECS..=me_core::MAX_POLL_SECS)
                                    .suffix(" 秒"),
                            );
                        }
                        if ui.small_button("应用").clicked() {
                            commands.push(Command::SetAccountRealtime {
                                account: account.id,
                                enabled: idle,
                                poll_interval_secs: custom.then_some(interval),
                            });
                        }
                    });
                }
            }
            ui.add_space(4.0);
            if ui.button("重建全文索引").clicked() {
                state.toast(ToastKind::Info, "正在重建索引…");
                commands.push(Command::RebuildIndex);
            }
            ui.label(
                RichText::new("用于兜底修复：清空 FTS 后按已缓存正文重新导入。")
                    .small()
                    .color(Color32::GRAY),
            );
        });

        ui.add_space(8.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(RichText::new("草稿").strong());
            let mut auto_sync = state.draft_server_sync;
            if ui
                .checkbox(&mut auto_sync, "自动把本地草稿同步到服务器草稿箱")
                .changed()
            {
                state.draft_server_sync = auto_sync;
                commands.push(Command::SetDraftServerSync { enabled: auto_sync });
            }
            ui.label(
                RichText::new(
                    "关闭后仍会拉取服务器草稿；本地修改只在你手动点击“保存到服务器”时上传。冲突始终保留两份。",
                )
                .small()
                .color(Color32::GRAY),
            );
            if state.draft_bulk_upload_pending {
                ui.add_space(4.0);
                ui.label(
                    RichText::new("检测到从未上传过的旧草稿（v0.1 迁移）")
                        .color(Color32::from_rgb(190, 140, 40)),
                );
                ui.horizontal(|ui| {
                    if ui.button("上传这些旧草稿").clicked() {
                        state.draft_bulk_upload_pending = false;
                        commands.push(Command::ConfirmDraftBulkUpload { accept: true });
                    }
                    if ui.button("保持仅本地").clicked() {
                        state.draft_bulk_upload_pending = false;
                        commands.push(Command::ConfirmDraftBulkUpload { accept: false });
                    }
                });
            }
        });

        ui.add_space(8.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(RichText::new("阅读").strong());
            let mut mode = state.html_renderer;
            ui.radio_value(
                &mut mode,
                me_core::HtmlRendererMode::Enhanced,
                "增强 HTML 渲染（表格跨度 / 内联样式 / 背景色）",
            );
            ui.radio_value(
                &mut mode,
                me_core::HtmlRendererMode::Legacy,
                "简化渲染（回退到 v0.1 子集）",
            );
            if mode != state.html_renderer {
                state.html_renderer = mode;
                state.html_rendered = None;
                state.html_job = None;
                commands.push(Command::SetHtmlRenderer { mode });
            }
            ui.label(
                RichText::new(
                    "增强模式在后台解析 HTML，不执行脚本、不加载外部 CSS/字体；超限或异常时自动回退到简化视图。远程图片策略不变。",
                )
                .small()
                .color(Color32::GRAY),
            );
        });

        ui.add_space(8.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(RichText::new("高级：OAuth 客户端").strong());
            ui.label(
                RichText::new(
                    "留空表示使用构建期默认 client_id；client_secret 只能构建期注入，不在此处保存。",
                )
                .small()
                .color(Color32::GRAY),
            );
            ui.horizontal(|ui| {
                ui.label("Google client_id");
                ui.add(
                    egui::TextEdit::singleline(&mut state.oauth_google_client_id)
                        .desired_width(280.0)
                        .hint_text("构建期默认 / 自定义 public client"),
                );
                if ui.button("应用").clicked() {
                    commands.push(Command::SetOAuthClient {
                        provider: me_core::OAuthProvider::Google,
                        config: me_core::OAuthClientConfig {
                            client_id: state.oauth_google_client_id.trim().to_string(),
                            tenant: None,
                        },
                    });
                }
            });
            ui.horizontal(|ui| {
                ui.label("Microsoft client_id");
                ui.add(
                    egui::TextEdit::singleline(&mut state.oauth_microsoft_client_id)
                        .desired_width(220.0)
                        .hint_text("构建期默认 / 自定义 public client"),
                );
                ui.label("tenant");
                ui.add(
                    egui::TextEdit::singleline(&mut state.oauth_microsoft_tenant)
                        .desired_width(120.0)
                        .hint_text("common"),
                );
                if ui.button("应用").clicked() {
                    commands.push(Command::SetOAuthClient {
                        provider: me_core::OAuthProvider::Microsoft,
                        config: me_core::OAuthClientConfig {
                            client_id: state.oauth_microsoft_client_id.trim().to_string(),
                            tenant: Some(state.oauth_microsoft_tenant.trim().to_string())
                                .filter(|tenant| !tenant.is_empty()),
                        },
                    });
                }
            });
        });

        ui.add_space(8.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(RichText::new("隐私").strong());
            let mut remote = state.remote_images;
            if ui
                .checkbox(&mut remote, "自动加载邮件正文中的远程图片")
                .changed()
            {
                state.remote_images = remote;
                commands.push(Command::SetRemoteImages { enabled: remote });
            }
            ui.label(
                RichText::new(
                    "开启后，图片服务器会知道你在何时阅读了邮件，并可能看到你的 IP。\
                     关闭后只渲染内嵌（cid / data）图片，远程图片显示为可点击的浏览器链接。",
                )
                .small()
                .color(Color32::GRAY),
            );
        });

        ui.add_space(8.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(RichText::new("本地缓存").strong());
            ui.horizontal(|ui| {
                if ui.button("刷新占用").clicked()
                    && let Some(dir) = cache_dir {
                        let mut total = dir_size(dir);
                        // 附件目录可能被重定位到缓存根目录之外，需单独计入。
                        if let Some(attach) = state.attachment_dir.as_deref()
                            && !attach.starts_with(dir)
                        {
                            total = total.saturating_add(dir_size(attach));
                        }
                        state.cache_size_bytes = total;
                    }
                ui.label(format!("当前占用：{}", crate::app::human_bytes(state.cache_size_bytes)));
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("附件下载目录");
                let effective = state
                    .attachment_dir
                    .clone()
                    .or_else(|| cache_dir.map(|dir| dir.join("attach")));
                let display = effective
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "（加载中）".into());
                ui.label(RichText::new(display).small().color(Color32::GRAY));
                if ui.button("更改…").clicked() {
                    let mut dialog = rfd::FileDialog::new();
                    if let Some(dir) = effective.as_ref() {
                        dialog = dialog.set_directory(dir);
                    }
                    if let Some(picked) = dialog.pick_folder() {
                        state.toast(ToastKind::Info, "正在迁移附件目录…");
                        commands.push(Command::SetAttachmentDir {
                            path: Some(picked),
                        });
                    }
                }
                if ui.button("恢复默认").clicked() {
                    state.toast(ToastKind::Info, "正在恢复默认附件目录…");
                    commands.push(Command::SetAttachmentDir { path: None });
                }
            });
            ui.label(
                RichText::new(
                    "更改后会把已下载的附件迁移到新目录，并同步更新本地路径；新目录需为空或此前由 MyEmail 使用。",
                )
                .small()
                .color(Color32::GRAY),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("清理全部账户缓存").clicked() {
                    state.dialog = Dialog::Confirm(ConfirmDialog {
                        title: "清理缓存".into(),
                        body: "将删除正文、原始邮件与附件文件，保留摘要、标志与文件夹。清理后正文需重新下载。".into(),
                        action: ConfirmAction::ClearCacheAll,
                        danger: false,
                    });
                }
                if let Some(account) = state.selected_account
                    && ui.button("清理当前账户缓存").clicked() {
                        state.dialog = Dialog::Confirm(ConfirmDialog {
                            title: "清理当前账户缓存".into(),
                            body: "将删除当前账户的正文、原始邮件与附件文件，保留摘要与标志。".into(),
                            action: ConfirmAction::ClearCacheAccount(account),
                            danger: false,
                        });
                    }
            });
            ui.label(
                RichText::new("清理不删除邮件摘要、标志与草稿；FTS 头部列保留，正文列清空。")
                    .small()
                    .color(Color32::GRAY),
            );
        });

        ui.add_space(8.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(RichText::new("日志与隐私").strong());
            ui.horizontal(|ui| {
                if ui.button("打开日志目录").clicked()
                    && let Some(dir) = cache_dir {
                        let logs = dir.join("logs");
                        let _ = std::fs::create_dir_all(&logs);
                        open_path(&logs);
                    }
                ui.label(
                    RichText::new("日志按天滚动，保留 7 天；不记录正文与凭证，邮箱地址脱敏。")
                        .small()
                        .color(Color32::GRAY),
                );
            });
            ui.add_space(6.0);
            ui.label(
                RichText::new("本地缓存为明文保存")
                    .strong()
                    .color(Color32::from_rgb(190, 140, 40)),
            );
            ui.label(
                RichText::new(
                    "SQLite 数据库、正文、原始邮件与附件均为未加密文件。任何能访问本机账户目录的程序或用户都可以读取它们。MyEmail 不承诺合规存储。",
                )
                .small()
                .color(Color32::GRAY),
            );
        });
    });
}

pub fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            total = total.saturating_add(dir_size(&entry.path()));
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

/// 用系统默认程序打开目录/文件。
pub fn open_path(path: &Path) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(path)
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    {
        let _ = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_scroll_area_reaches_right_edge() {
        // 回归：滚动条必须贴在页面最右侧，不能随内容宽度内缩。
        let ctx = egui::Context::default();
        let mut edges = (0.0_f32, 0.0_f32);
        for _ in 0..6 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let available_right = ui.available_rect_before_wrap().right();
                    let scope = ui.scope(|ui| {
                        page_scroll_area().show(ui, |ui| {
                            for i in 0..20 {
                                egui::Frame::group(ui.style()).show(ui, |ui| {
                                    ui.label("外观");
                                    ui.label(format!("设置项 {i}"));
                                });
                            }
                        });
                    });
                    edges = (available_right, scope.response.rect.right());
                });
            });
        }
        assert!(
            (edges.0 - edges.1).abs() < 1.0,
            "滚动区右边缘 {:.1} 应贴齐可用区域 {:.1}",
            edges.1,
            edges.0
        );
    }
}
