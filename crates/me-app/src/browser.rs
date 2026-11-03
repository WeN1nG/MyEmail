//! 系统浏览器打开端口。只接受 http / https。

use me_core::CoreError;
use me_core::port::BrowserOpener;

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemBrowser;

impl BrowserOpener for SystemBrowser {
    fn open(&self, url: &str) -> Result<(), CoreError> {
        let url = url.trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(CoreError::oauth(
                None::<String>,
                "只允许用系统浏览器打开 http/https 链接",
                false,
            ));
        }
        open_url(url)
    }
}

fn open_url(url: &str) -> Result<(), CoreError> {
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("cmd")
        .args(["/C", "start", ""])
        .arg(url)
        .spawn();

    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();

    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();

    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    let result: std::io::Result<std::process::Child> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no system browser integration",
    ));

    result.map(|_| ()).map_err(|err| {
        CoreError::oauth(None::<String>, format!("无法打开系统浏览器：{err}"), false)
    })
}
