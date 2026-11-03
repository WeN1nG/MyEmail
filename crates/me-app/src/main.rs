//! 启动装配：单实例锁、日志、打开库、创建运行时与通道、启动 eframe。
//!
//! Windows 下按 GUI 子系统构建，双击启动不会额外弹出控制台窗口。
//! 因此启动失败不能依赖 stderr，统一用消息框告知用户。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod browser;
mod fonts;
mod images;
mod state;
mod views;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use eframe::egui;
use me_store::Store;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

fn main() {
    if let Err(err) = run() {
        tracing::error!(%err, "MyEmail 启动失败");
        // GUI 子系统下没有控制台，这是用户唯一能看到的失败提示。
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("MyEmail 无法启动")
            .set_description(format!("{err:#}"))
            .show();
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let root = data_root()?;
    std::fs::create_dir_all(&root).with_context(|| format!("无法创建 {}", root.display()))?;

    // 1. 单实例锁：多实例可能损坏 SQLite。
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(".instance.lock"))
        .context("无法创建单实例锁文件")?;
    if fs2::FileExt::try_lock_exclusive(&lock_file).is_err() {
        // 已有实例：通过回环地址请求它把窗口带到前台。
        if notify_existing_instance(&root) {
            // 已有窗口已被带到前台，启动器无需再提示。
            return Ok(());
        }
        anyhow::bail!("MyEmail 已在运行，但无法激活已有窗口。请检查任务管理器中是否有残留进程。");
    }

    // 2. 日志：按天滚动，保留 7 天。
    let _log_guard = init_logging(&root.join("logs"))?;

    // 3. 打开库（schema 初始化在 Store::open 内完成）。
    let store = Store::open(&root).context("无法打开本地数据库")?;
    let paths = store.paths();
    tracing::info!(root = %root.display(), "MyEmail starting");

    // 4. 通道：Command 无界，Event 有界 1024。
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::channel(1024);

    // 唤醒回调：由 UI 注入 egui Context；窗口创建前为空。
    let wake_slot: Arc<Mutex<Option<egui::Context>>> = Arc::new(Mutex::new(None));
    let wake = {
        let slot = wake_slot.clone();
        Arc::new(move || {
            if let Ok(slot) = slot.lock()
                && let Some(ctx) = slot.as_ref()
            {
                ctx.request_repaint();
            }
        })
    };
    // 5. 第二个实例的激活请求：回环监听 + 端口文件。
    let activation = Arc::new(AtomicBool::new(false));
    if let Err(err) = start_activation_listener(&root, activation.clone(), wake.clone()) {
        tracing::warn!(%err, "无法启动单实例激活监听；第二个实例将只提示已运行");
    }

    // 6. tokio 运行时（独立线程），不与 UI 线程共享。
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("myemail-worker")
        .enable_all()
        .build()
        .context("无法创建 tokio 运行时")?;

    let image_loader = images::RemoteImageLoader::new(runtime.handle().clone(), wake.clone());
    let html = views::html::HtmlRenderer::new(runtime.handle().clone(), wake.clone());
    let browser: std::sync::Arc<dyn me_core::port::BrowserOpener> =
        std::sync::Arc::new(browser::SystemBrowser);
    let events = me_sync::events::EventSender::new(event_tx, wake);

    let service = me_sync::start_native(store.writable(), paths.clone(), events, browser);
    runtime.spawn(service.run(command_rx));

    // 7. eframe 窗口。先渲染空态，不等待网络。
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("MyEmail")
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };

    let cache_dir = root.clone();
    let repo = store.read_only();
    let result = eframe::run_native(
        "MyEmail",
        native_options,
        Box::new(move |cc| {
            if let Ok(mut slot) = wake_slot.lock() {
                *slot = Some(cc.egui_ctx.clone());
            }
            let backend = app::BackendHandle::new(command_tx, event_rx);
            Ok(Box::new(app::MyEmailApp::new(
                cc,
                backend,
                repo,
                Some(cache_dir),
                image_loader,
                html,
                activation,
            )))
        }),
    );

    match &result {
        Ok(()) => tracing::info!("eframe event loop exited"),
        Err(err) => tracing::error!(%err, "eframe event loop failed"),
    }
    drop(runtime);
    result.map_err(|err| anyhow::anyhow!("UI 启动失败：{err}"))
}

fn data_root() -> anyhow::Result<PathBuf> {
    let base = dirs::data_dir()
        .or_else(dirs::config_dir)
        .context("无法定位用户数据目录")?;
    Ok(base.join("MyEmail"))
}

fn instance_port_file(root: &std::path::Path) -> PathBuf {
    root.join(".instance.port")
}

/// 监听第二个实例的激活请求。仅绑定回环地址，token 防止误触发。
fn start_activation_listener(
    root: &std::path::Path,
    activation: Arc<AtomicBool>,
    wake: Arc<dyn Fn() + Send + Sync>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let token = format!("{}-{nanos}", std::process::id());
    std::fs::write(instance_port_file(root), format!("{port}\n{token}\n"))?;

    std::thread::Builder::new()
        .name("myemail-activation".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = [0u8; 256];
                let read = stream.read(&mut buf).unwrap_or(0);
                let received = String::from_utf8_lossy(&buf[..read]);
                if received.trim() != token {
                    continue;
                }
                activation.store(true, Ordering::Relaxed);
                wake();
                let _ = stream.write_all(b"OK");
            }
        })?;
    Ok(())
}

/// 通知已有实例激活窗口；成功返回 `true`。
fn notify_existing_instance(root: &std::path::Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(instance_port_file(root)) else {
        return false;
    };
    let mut lines = contents.lines();
    let Some(port) = lines
        .next()
        .and_then(|line| line.trim().parse::<u16>().ok())
    else {
        return false;
    };
    let Some(token) = lines.next().map(str::trim).filter(|t| !t.is_empty()) else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(2),
    ) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    if stream.write_all(token.as_bytes()).is_err() {
        return false;
    }
    let mut response = [0u8; 8];
    matches!(stream.read(&mut response), Ok(read) if &response[..read] == b"OK")
}

fn init_logging(
    log_dir: &std::path::Path,
) -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    std::fs::create_dir_all(log_dir)?;
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("myemail")
        .filename_suffix("log")
        .max_log_files(7)
        .build(log_dir)
        .context("无法创建日志滚动文件")?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,me_sync=info,me_proto=info,me_store=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .init();
    Ok(guard)
}
