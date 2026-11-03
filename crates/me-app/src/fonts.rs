//! CJK 字体装配与邮件正文专用字体族。
//!
//! 优先级：构建期内嵌的 `assets/fonts/cjk-subset.ttf`（若存在）作为主字体；
//! 系统字体作为缺字回退（覆盖内嵌子集之外的繁体/生僻字）。
//! egui 默认字体不含中文，必须补一份 CJK 字体，否则界面出现方框。
//!
//! 另外注册三组只给 HTML 邮件正文使用的字体族：
//! `myemail-email` / `myemail-email-bold` / `myemail-email-mono`。
//! 它们优先使用系统 UI 字体（Segoe UI、Consolas 等），让邮件声明的
//! `ui-sans-serif`、`font-weight:600`、`monospace` 更接近发件端设计；
//! 找不到时回退到 egui 默认字体，不影响启动。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui;

const FONT_KEY: &str = "cjk";
const FALLBACK_KEY: &str = "cjk-fallback";

const EMAIL_SANS_KEY: &str = "email-sans";
const EMAIL_SANS_BOLD_KEY: &str = "email-sans-bold";
const EMAIL_MONO_KEY: &str = "email-mono";
const EMAIL_CJK_BOLD_KEY: &str = "email-cjk-bold";

const EMAIL_FAMILY: &str = "myemail-email";
const EMAIL_BOLD_FAMILY: &str = "myemail-email-bold";
const EMAIL_MONO_FAMILY: &str = "myemail-email-mono";

static EMAIL_FAMILIES_READY: AtomicBool = AtomicBool::new(false);

/// 邮件正文使用的无衬线字体族；`bold` 选择真实粗体族。
///
/// 未调用 [`install`]（例如单元测试）时返回 egui 默认族：epaint 对未绑定的
/// `FontFamily::Name` 会直接 panic，不能凭猜。
pub fn email_family(bold: bool) -> egui::FontFamily {
    if !EMAIL_FAMILIES_READY.load(Ordering::Relaxed) {
        return egui::FontFamily::Proportional;
    }
    let name = if bold {
        EMAIL_BOLD_FAMILY
    } else {
        EMAIL_FAMILY
    };
    egui::FontFamily::Name(name.into())
}

/// 邮件正文使用的等宽字体族。
pub fn email_monospace() -> egui::FontFamily {
    if !EMAIL_FAMILIES_READY.load(Ordering::Relaxed) {
        return egui::FontFamily::Monospace;
    }
    egui::FontFamily::Name(EMAIL_MONO_FAMILY.into())
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mut installed = false;

    if let Some(bytes) = embedded_font() {
        fonts.font_data.insert(
            FONT_KEY.to_owned(),
            egui::FontData::from_static(bytes).into(),
        );
        installed = true;
        tracing::info!("using embedded CJK font subset");
        // 内嵌子集只覆盖 GB2312；系统字体补齐繁体与生僻字。
        if let Some((path, bytes)) = load_system_font() {
            fonts.font_data.insert(
                FALLBACK_KEY.to_owned(),
                egui::FontData::from_owned(bytes).into(),
            );
            tracing::info!(path = %path.display(), "loaded system CJK font as fallback");
        }
    } else if let Some((path, bytes)) = load_system_font() {
        fonts.font_data.insert(
            FONT_KEY.to_owned(),
            egui::FontData::from_owned(bytes).into(),
        );
        installed = true;
        tracing::info!(path = %path.display(), "loaded system CJK font");
    }

    if installed {
        let has_fallback = fonts.font_data.contains_key(FALLBACK_KEY);
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            let family = fonts.families.entry(family).or_default();
            family.push(FONT_KEY.to_owned());
            if has_fallback {
                family.push(FALLBACK_KEY.to_owned());
            }
        }
    } else {
        tracing::warn!(
            "no CJK font found; Chinese text may render as boxes. Place a font at assets/fonts/cjk-subset.ttf"
        );
    }

    install_email_families(&mut fonts);
    ctx.set_fonts(fonts);
    EMAIL_FAMILIES_READY.store(true, Ordering::Relaxed);
}

/// 注册邮件正文专用字体族。
///
/// 每个族都以 egui 默认族收尾，保证中文、emoji 与缺字回退。系统字体只在本机
/// 运行时读取，不随安装包分发。
fn install_email_families(fonts: &mut egui::FontDefinitions) {
    let mut loaded_any = false;

    if let Some(bytes) = load_first_font(&sans_candidates()) {
        fonts.font_data.insert(
            EMAIL_SANS_KEY.to_owned(),
            egui::FontData::from_owned(bytes).into(),
        );
        loaded_any = true;
    }
    if let Some(bytes) = load_first_font(&sans_bold_candidates()) {
        fonts.font_data.insert(
            EMAIL_SANS_BOLD_KEY.to_owned(),
            egui::FontData::from_owned(bytes).into(),
        );
        loaded_any = true;
    }
    if let Some(bytes) = load_first_font(&cjk_bold_candidates()) {
        fonts.font_data.insert(
            EMAIL_CJK_BOLD_KEY.to_owned(),
            egui::FontData::from_owned(bytes).into(),
        );
        loaded_any = true;
    }
    if let Some(bytes) = load_first_font(&mono_candidates()) {
        fonts.font_data.insert(
            EMAIL_MONO_KEY.to_owned(),
            egui::FontData::from_owned(bytes).into(),
        );
        loaded_any = true;
    }

    let proportional = fonts
        .families
        .get(&egui::FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let monospace = fonts
        .families
        .get(&egui::FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();

    let mut sans = Vec::new();
    if fonts.font_data.contains_key(EMAIL_SANS_KEY) {
        sans.push(EMAIL_SANS_KEY.to_owned());
    }
    sans.extend(proportional.iter().cloned());

    let mut bold = Vec::new();
    if fonts.font_data.contains_key(EMAIL_SANS_BOLD_KEY) {
        bold.push(EMAIL_SANS_BOLD_KEY.to_owned());
    }
    // CJK 粗体放在 CJK 常规之前，西文粗体负责拉丁字母，CJK 粗体负责中文。
    if fonts.font_data.contains_key(EMAIL_CJK_BOLD_KEY) {
        bold.push(EMAIL_CJK_BOLD_KEY.to_owned());
    }
    bold.extend(proportional.iter().cloned());

    let mut mono = Vec::new();
    if fonts.font_data.contains_key(EMAIL_MONO_KEY) {
        mono.push(EMAIL_MONO_KEY.to_owned());
    }
    mono.extend(monospace.iter().cloned());

    dedup(&mut sans);
    dedup(&mut bold);
    dedup(&mut mono);

    fonts
        .families
        .insert(egui::FontFamily::Name(EMAIL_FAMILY.into()), sans);
    fonts
        .families
        .insert(egui::FontFamily::Name(EMAIL_BOLD_FAMILY.into()), bold);
    fonts
        .families
        .insert(egui::FontFamily::Name(EMAIL_MONO_FAMILY.into()), mono);

    if loaded_any {
        tracing::info!("registered email font families from system fonts");
    }
}

fn dedup(list: &mut Vec<String>) {
    let mut seen = std::collections::BTreeSet::new();
    list.retain(|name| seen.insert(name.clone()));
}

fn sans_candidates() -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        fonts_in_system_dir(&["segoeui.ttf", "arial.ttf", "Deng.ttf"])
    }
    #[cfg(target_os = "macos")]
    {
        absolute(&[
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            "/System/Library/Fonts/HelveticaNeue.ttc",
            "/System/Library/Fonts/PingFang.ttc",
        ])
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        absolute(&[
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        ])
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    {
        Vec::new()
    }
}

fn sans_bold_candidates() -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        fonts_in_system_dir(&["segoeuib.ttf", "arialbd.ttf", "Dengb.ttf"])
    }
    #[cfg(target_os = "macos")]
    {
        absolute(&[
            "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
            "/System/Library/Fonts/HelveticaNeue.ttc",
        ])
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        absolute(&[
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
        ])
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    {
        Vec::new()
    }
}

fn cjk_bold_candidates() -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        fonts_in_system_dir(&["msyhbd.ttc", "Dengb.ttf", "simhei.ttf"])
    }
    #[cfg(target_os = "macos")]
    {
        absolute(&["/System/Library/Fonts/PingFang.ttc"])
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        absolute(&[
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc",
            "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        ])
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    {
        Vec::new()
    }
}

fn mono_candidates() -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        fonts_in_system_dir(&["consola.ttf", "cour.ttf"])
    }
    #[cfg(target_os = "macos")]
    {
        absolute(&[
            "/System/Library/Fonts/Menlo.ttc",
            "/System/Library/Fonts/Supplemental/Courier New.ttf",
        ])
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        absolute(&[
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
        ])
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    {
        Vec::new()
    }
}

fn fonts_in_system_dir(names: &[&str]) -> Vec<PathBuf> {
    let root = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("Fonts");
    names.iter().map(|name| root.join(name)).collect()
}

#[allow(dead_code)] // 仅在 macOS / Linux 的候选路径里使用。
fn absolute(paths: &[&str]) -> Vec<PathBuf> {
    paths.iter().map(Path::new).map(Path::to_path_buf).collect()
}

fn load_first_font(candidates: &[PathBuf]) -> Option<Vec<u8>> {
    for path in candidates {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if !bytes.is_empty() {
            tracing::debug!(path = %path.display(), "loaded email font");
            return Some(bytes);
        }
    }
    None
}

fn load_system_font() -> Option<(PathBuf, Vec<u8>)> {
    for path in system_font_candidates() {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if !bytes.is_empty() {
            return Some((path, bytes));
        }
    }
    None
}

#[cfg(has_cjk_asset)]
fn embedded_font() -> Option<&'static [u8]> {
    Some(include_bytes!("../../../assets/fonts/cjk-subset.ttf"))
}

#[cfg(not(has_cjk_asset))]
fn embedded_font() -> Option<&'static [u8]> {
    None
}

fn system_font_candidates() -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let root = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let fonts = root.join("Fonts");
        [
            "msyh.ttc",
            "msyhbd.ttc",
            "simhei.ttf",
            "simsun.ttc",
            "Deng.ttf",
        ]
        .iter()
        .map(|name| fonts.join(name))
        .collect()
    }
    #[cfg(target_os = "macos")]
    {
        [
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/STHeiti Medium.ttc",
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        [
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
            "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    {
        Vec::new()
    }
}
