//! 文件内容获取：只负责「把字节搬到能交给模型/本地拆分的地方」。
//!
//! **本模块不做任何「这段文字是什么」的判断**（决策 O-19）。它只做三件事：
//!
//! 1. 文本类（md / txt / docx）——把文件读成纯字符串；
//! 2. PPTX——机械抽取 `<a:t>` 节点的字符串（不猜标题层级、不做版面重建）；
//! 3. PDF——**无损切页**（`lopdf` 只为这一件事引入），不提取文本。
//!
//! PDF 与 PPTX 的有效性、语块边界、语块长度、`locator` 全由模型定（决策 O-18）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use me_core::CoreError;
use me_core::knowledge::{
    KbKind, OTHER_MAX_BYTES, PDF_MAX_LOCAL_BYTES, PdfBatch, plan_pdf_batches, split_text_batches,
};

/// 老格式转换的硬超时。Word 会弹「文件已损坏，是否修复」把子进程永久挂住。
const CONVERT_TIMEOUT: Duration = Duration::from_secs(8 * 60);

fn parse_err(message: impl std::fmt::Display) -> CoreError {
    CoreError::parse(message.to_string())
}

// ---------------------------------------------------------------------------
// 文本类
// ---------------------------------------------------------------------------

/// 读纯文本文件：先按 UTF-8，失败再按 GBK（国内老 txt 很常见）。
pub fn read_text_file(path: &Path) -> Result<String, CoreError> {
    let bytes = std::fs::read(path)
        .map_err(|e| CoreError::storage(format!("读取 {} 失败: {e}", path.display())))?;
    match String::from_utf8(bytes.clone()) {
        Ok(text) => Ok(strip_bom(&text)),
        Err(_) => {
            let (decoded, _, had_errors) = encoding_rs::GBK.decode(&bytes);
            if had_errors {
                return Err(parse_err(format!(
                    "{} 既不是 UTF-8 也不是 GBK，无法作为纯文本读取",
                    path.display()
                )));
            }
            Ok(strip_bom(decoded.as_ref()))
        }
    }
}

fn strip_bom(text: &str) -> String {
    text.trim_start_matches('\u{feff}').to_string()
}

/// `docx`：解 `word/document.xml`，按 `<w:p>` 分段、取 `<w:t>` 的文本。
///
/// 段落是最好的本地切分单元：docx 的结构干净，本地切又快又稳。
/// 这是**结构搬运**，不是内容理解——没有判断哪段是正文、也没有猜标题层级。
pub fn docx_text(bytes: &[u8]) -> Result<String, CoreError> {
    let xml = read_zip_entry(bytes, "word/document.xml")?;
    let mut reader = quick_xml::Reader::from_reader(xml.as_slice());
    reader.config_mut().trim_text(false);

    let mut out = String::new();
    let mut text = String::new();
    let mut in_text = false;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(start)) => {
                match start.local_name().as_ref() {
                    "t" => {
                        in_text = true;
                        text.clear();
                    }
                    // 段落内的换行符
                    "br" | "cr" => out.push('\n'),
                    _ => {}
                }
            }
            Ok(quick_xml::events::Event::Empty(empty)) => {
                if matches!(empty.local_name().as_ref(), "br" | "cr") {
                    out.push('\n');
                }
            }
            Ok(quick_xml::events::Event::Text(event)) => {
                if in_text {
                    text.push_str(&text_of(event));
                }
            }
            Ok(quick_xml::events::Event::End(end)) => match end.local_name().as_ref() {
                "t" => {
                    in_text = false;
                    out.push_str(&text);
                }
                "p" => out.push('\n'),
                _ => {}
            },
            Ok(quick_xml::events::Event::Eof) => break,
            Ok(_) => {}
            Err(err) => return Err(parse_err(format!("解析 docx 失败: {err}"))),
        }
        buf.clear();
    }
    Ok(normalize_blank_lines(&out))
}

/// `pptx`：按幻灯片顺序抽 `<a:t>` 的文本，幻灯片之间加一行位置标记。
///
/// 位置标记（`[幻灯片 N]`）是**唯一**额外加的东西：它给模型一个可靠的位置锚点，
/// 让返回的 `locator` 能对上页。除此之外不猜标题、不排序、不去重。
pub fn pptx_text(bytes: &[u8]) -> Result<String, CoreError> {
    let slides = slide_entries(bytes)?;
    let mut out = String::new();
    for (index, name) in slides.iter().enumerate() {
        let xml = read_zip_entry(bytes, name)?;
        out.push_str(&format!("[幻灯片 {}]\n", index + 1));
        out.push_str(&slide_text(&xml)?);
        out.push('\n');
    }
    Ok(normalize_blank_lines(&out))
}

/// 列出 `ppt/slides/slideN.xml`，按 N 数字顺序（不能按字典序：slide10 < slide2）。
fn slide_entries(bytes: &[u8]) -> Result<Vec<String>, CoreError> {
    let reader = std::io::Cursor::new(bytes);
    let archive = zip::ZipArchive::new(reader)
        .map_err(|e| parse_err(format!("打开 pptx 失败（不是有效的 zip）: {e}")))?;
    let mut slides: Vec<(u32, String)> = Vec::new();
    for entry in archive.file_names() {
        let Ok(name) = entry else { continue };
        let Some(rest) = name.strip_prefix("ppt/slides/slide") else {
            continue;
        };
        let Some(number) = rest.strip_suffix(".xml") else {
            continue;
        };
        if let Ok(number) = number.parse::<u32>() {
            slides.push((number, name.to_string()));
        }
    }
    if slides.is_empty() {
        return Err(parse_err("pptx 里没有找到任何幻灯片"));
    }
    slides.sort_by_key(|(number, _)| *number);
    Ok(slides.into_iter().map(|(_, name)| name).collect())
}

fn slide_text(xml: &[u8]) -> Result<String, CoreError> {
    let mut reader = quick_xml::Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut out = String::new();
    let mut in_text = false;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(start)) => {
                if start.local_name().as_ref() == "t" {
                    in_text = true;
                }
            }
            Ok(quick_xml::events::Event::Text(event)) => {
                if in_text {
                    out.push_str(&text_of(event));
                }
            }
            Ok(quick_xml::events::Event::End(end)) => {
                if end.local_name().as_ref() == "t" {
                    in_text = false;
                    // `<a:t>` 是段落内的 run，run 之间给一个空格比直接粘连更可读。
                    out.push(' ');
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Ok(_) => {}
            Err(err) => return Err(parse_err(format!("解析幻灯片失败: {err}"))),
        }
        buf.clear();
    }
    Ok(out.trim().to_string())
}

fn read_zip_entry(bytes: &[u8], name: &str) -> Result<Vec<u8>, CoreError> {
    use std::io::Read;
    let reader = std::io::Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| parse_err(format!("打开 zip 容器失败: {e}")))?;
    let mut file = archive
        .by_name(name)
        .map_err(|e| parse_err(format!("压缩包里没有 {name}: {e}")))?;
    let mut out = Vec::new();
    file.read_to_end(&mut out)
        .map_err(|e| parse_err(format!("读取 {name} 失败: {e}")))?;
    Ok(out)
}

/// 取出文本节点并反转义（`&amp;` → `&`）。quick-xml 0.42 不再有 `decode()`。
fn text_of(event: quick_xml::events::BytesText<'_>) -> String {
    let raw = event.into_inner();
    match quick_xml::escape::unescape(&raw) {
        Ok(value) => value.into_owned(),
        Err(_) => raw.into_owned(),
    }
}

/// 多个空行压成一个，避免「一页一个空行」把切分阈值虚耗掉。
fn normalize_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.trim_end().to_string()
}

// ---------------------------------------------------------------------------
// PDF：只切页，不提文本
// ---------------------------------------------------------------------------

pub fn pdf_page_count(bytes: &[u8]) -> Result<u32, CoreError> {
    let doc = lopdf::Document::load_mem(bytes)
        .map_err(|e| parse_err(format!("PDF 已加密或损坏，无法读取: {e}")))?;
    Ok(doc.get_pages().len() as u32)
}

/// 从整份 PDF 里切出指定页（1 基），返回**仍然是 PDF** 的字节。
///
/// 做法是「重新 load → 删掉不要的页 → save」：`lopdf` 没有 `extract_pages`。
///
/// **不要调 `prune_objects`**：它可能删掉被保留页共享的资源（字体、图片 XObject），
/// 结果是子文档渲染成空白页。临时文件偏大但内容安全。
pub fn extract_pdf_pages(bytes: &[u8], batch: PdfBatch) -> Result<Vec<u8>, CoreError> {
    let mut doc = lopdf::Document::load_mem(bytes)
        .map_err(|e| parse_err(format!("PDF 已加密或损坏，无法读取: {e}")))?;
    let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
    let keep: Vec<u32> = (batch.first_page..=batch.last_page())
        .filter(|page| pages.contains(page))
        .collect();
    if keep.is_empty() {
        return Err(parse_err("切页失败：这一批没有任何页"));
    }
    let drop: Vec<u32> = pages
        .into_iter()
        .filter(|page| !keep.contains(page))
        .collect();
    doc.delete_pages(&drop);
    let mut out = Vec::new();
    doc.save_to(&mut out)
        .map_err(|e| parse_err(format!("保存切页结果失败: {e}")))?;
    Ok(out)
}

/// 切页后自检：重新加载子文档，页数必须等于预期。
///
/// 不相等说明 `delete_pages` 破坏了结构——**不要硬着头皮发半成品**，
/// 上层会退化成「整份发」或直接失败。
pub fn verify_pdf_pages(bytes: &[u8], expected: u32) -> Result<(), CoreError> {
    let count = pdf_page_count(bytes)?;
    if count != expected {
        return Err(parse_err(format!(
            "切页自检失败：预期 {expected} 页，实得 {count} 页"
        )));
    }
    Ok(())
}

/// 一份 PDF 应当怎么切（纯计算，便于单测）。
pub fn pdf_batches(total_pages: u32, file_bytes: u64) -> Vec<PdfBatch> {
    plan_pdf_batches(total_pages, file_bytes)
}

// ---------------------------------------------------------------------------
// 前置校验
// ---------------------------------------------------------------------------

/// 按路径判断类型与大小上限。**老格式（doc/ppt）单独返回**，由调用方先转换。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    Direct(KbKind),
    LegacyDoc,
    LegacyPpt,
}

pub fn classify(path: &Path) -> Result<ImportKind, CoreError> {
    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let kind = match ext.as_str() {
        "md" | "markdown" => ImportKind::Direct(KbKind::Md),
        "txt" => ImportKind::Direct(KbKind::Txt),
        "docx" => ImportKind::Direct(KbKind::Docx),
        "pdf" => ImportKind::Direct(KbKind::Pdf),
        "pptx" => ImportKind::Direct(KbKind::Pptx),
        "doc" => ImportKind::LegacyDoc,
        "ppt" => ImportKind::LegacyPpt,
        _ => {
            return Err(CoreError::validation(format!(
                "不支持的文件类型：.{ext}（支持 md / txt / docx / pdf / pptx / doc / ppt）"
            )));
        }
    };
    Ok(kind)
}

/// 大小校验。PDF 的 512 MB 是**本地切分上限**，其它类型 50 MB。
pub fn check_size(kind: KbKind, bytes: u64) -> Result<(), CoreError> {
    let limit = if kind == KbKind::Pdf {
        PDF_MAX_LOCAL_BYTES
    } else {
        OTHER_MAX_BYTES
    };
    if bytes > limit {
        return Err(CoreError::validation(format!(
            "文件过大（{:.1} MB），上限 {} MB。请先拆分后再导入",
            bytes as f64 / 1024.0 / 1024.0,
            limit / 1024 / 1024
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 老格式转换（决策 O-12 / O-17）
// ---------------------------------------------------------------------------

/// 转换后端。探测结果缓存进 `meta.kb_converter`，避免每次导入都跑一遍探测。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Converter {
    Soffice,
    Office,
    None,
}

impl Converter {
    pub fn as_str(self) -> &'static str {
        match self {
            Converter::Soffice => "soffice",
            Converter::Office => "office",
            Converter::None => "none",
        }
    }

    pub fn parse(value: &str) -> Converter {
        match value {
            "soffice" => Converter::Soffice,
            "office" => Converter::Office,
            _ => Converter::None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Converter::Soffice => "LibreOffice",
            Converter::Office => "Office COM",
            Converter::None => "未检测到转换器",
        }
    }
}

/// 探测可用的转换器：`soffice` 优先（跨平台、质量最好），其次 Office COM。
pub fn detect_converter() -> Converter {
    if which_soffice().is_some() {
        return Converter::Soffice;
    }
    if cfg!(windows) && office_available() {
        return Converter::Office;
    }
    Converter::None
}

fn which_soffice() -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["soffice.exe", "soffice.com"]
    } else {
        &["soffice"]
    };
    // PATH 里的优先；Windows 再补两个最常见的安装位置。
    for name in names {
        if let Some(found) = search_path(name) {
            return Some(found);
        }
    }
    // Windows 上再试两个最常见的安装位置。
    //
    // **路径从环境变量拼，不写死**：写死 `C:\Program Files\...` 有两个问题——
    // 一是非英文 Windows 上这个目录是本地化的（「程序文件」），写死就找不到；
    // 二是安装被重定向到别的盘时同样找不到。用 `ProgramFiles` /
    // `ProgramFiles(x86)` 跟着系统走，两个问题一起解决。
    //
    // 顺带的好处：这两个字面量不会进二进制（发布前的泄露自检会拦绝对路径）。
    if cfg!(windows) {
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            let Some(root) = std::env::var_os(var) else {
                continue;
            };
            let candidate = PathBuf::from(root)
                .join("LibreOffice")
                .join("program")
                .join("soffice.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn search_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Windows 上「装了 Office」的近似判断：Word.Application 能创建出来才算。
///
/// 真正的判断只能靠试；这里只看注册表里有没有 ProgID 的 CLSID，
/// 避免为了探测就拉起一个 Word 进程。
fn office_available() -> bool {
    if !cfg!(windows) {
        return false;
    }
    // `reg query HKCR\Word.Application\CLSID` 成功即认为装了 Word。
    std::process::Command::new("reg")
        .args(["query", r"HKCR\Word.Application\CLSID"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// 把 `.doc` / `.ppt` 转成新格式，返回临时文件路径（调用方负责删）。
pub async fn convert_legacy(
    path: &Path,
    target: KbKind,
    tmp_dir: &Path,
) -> Result<PathBuf, CoreError> {
    let converter = detect_converter();
    if converter == Converter::None {
        return Err(CoreError::validation(
            "导入 .doc / .ppt 需要 LibreOffice 或 Microsoft Office：\
             装好其中之一后重试（设置页有「重新检测」）",
        ));
    }
    std::fs::create_dir_all(tmp_dir)
        .map_err(|e| CoreError::storage(format!("创建临时目录失败: {e}")))?;
    let format = match target {
        KbKind::Docx => "docx",
        KbKind::Pptx => "pptx",
        _ => return Err(CoreError::validation("只有 doc/ppt 需要转换")),
    };
    let stem = format!("{}-{}", std::process::id(), me_core::now_unix());
    match converter {
        Converter::Soffice => convert_with_soffice(path, tmp_dir, format, &stem).await,
        Converter::Office => convert_with_office(path, tmp_dir, format, &stem).await,
        Converter::None => unreachable!("上面已经拦掉"),
    }
}

async fn convert_with_soffice(
    path: &Path,
    tmp_dir: &Path,
    format: &str,
    stem: &str,
) -> Result<PathBuf, CoreError> {
    let soffice = which_soffice().ok_or_else(|| CoreError::validation("找不到 soffice"))?;
    let mut command = tokio::process::Command::new(soffice);
    // **逐参数传，绝不拼命令行字符串**：路径里的空格与引号会被 shell 解释，
    // 等于给自己开一个命令注入的口子。
    command
        .arg("--headless")
        .arg("--norestore")
        .arg("--convert-to")
        .arg(format)
        .arg("--outdir")
        .arg(tmp_dir)
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(CONVERT_TIMEOUT, command.status())
        .await
        .map_err(|_| CoreError::validation("转换超时（LibreOffice 8 分钟未完成）"))?
        .map_err(|e| CoreError::storage(format!("启动 LibreOffice 失败: {e}")))?;
    if !status.success() {
        return Err(CoreError::validation(format!(
            "LibreOffice 转换失败（退出码 {:?}）",
            status.code()
        )));
    }
    // soffice 用原文件名 + 新扩展名输出。
    let name = path
        .file_stem()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| stem.to_string());
    let produced = tmp_dir.join(format!("{name}.{format}"));
    if !produced.exists() {
        return Err(CoreError::validation(
            "LibreOffice 报告成功但没有产出文件（可能弹了对话框）",
        ));
    }
    Ok(produced)
}

/// Office COM：内嵌 PowerShell 脚本。Windows 一定带 PowerShell，用户不必额外装东西。
const CONVERT_PS1: &str = r#"param($Src, $Dst, $Kind)
if ($Kind -eq 'pptx') {
  $app = New-Object -ComObject PowerPoint.Application
  $doc = $app.Presentations.Open($Src, $true, $false, $false)
  $doc.SaveAs($Dst, 24)
  $doc.Close()
  $app.Quit()
} else {
  $app = New-Object -ComObject Word.Application
  $app.Visible = $false
  $doc = $app.Documents.Open($Src, $false, $true)
  $doc.SaveAs2($Dst, 16)
  $doc.Close($false)
  $app.Quit()
}
"#;

async fn convert_with_office(
    path: &Path,
    tmp_dir: &Path,
    format: &str,
    stem: &str,
) -> Result<PathBuf, CoreError> {
    let script = tmp_dir.join(format!("convert-{stem}.ps1"));
    std::fs::write(&script, CONVERT_PS1)
        .map_err(|e| CoreError::storage(format!("写入转换脚本失败: {e}")))?;
    let target = tmp_dir.join(format!("{stem}.{format}"));
    let mut command = tokio::process::Command::new("powershell");
    command
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(&script)
        .arg("-Src")
        .arg(path)
        .arg("-Dst")
        .arg(&target)
        .arg("-Kind")
        .arg(format)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let result = tokio::time::timeout(CONVERT_TIMEOUT, command.status()).await;
    // 脚本无论成功失败都要删掉（三条路径都删，别靠人工记得）。
    let _ = std::fs::remove_file(&script);
    let status = result
        .map_err(|_| CoreError::validation("转换超时（Office 8 分钟未完成，可能弹了对话框）"))?
        .map_err(|e| CoreError::storage(format!("启动 PowerShell 失败: {e}")))?;
    if !status.success() || !target.exists() {
        return Err(CoreError::validation(
            "Office 转换失败（Word/PowerPoint 可能弹了对话框或文件已损坏）",
        ));
    }
    Ok(target)
}

/// 启动时清空 `kb/tmp/`：上次异常退出会留下残留脚本与中间文件。
pub fn clean_tmp(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// 把 PPTX 文本切成若干批（每批 ≤ 4000 字，切点落在段落边界）。
pub fn text_batches(text: &str) -> Vec<String> {
    split_text_batches(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_rejects_unknown_extension() {
        assert!(classify(Path::new("/tmp/a.exe")).is_err());
        assert_eq!(
            classify(Path::new("/tmp/A.MD")).unwrap(),
            ImportKind::Direct(KbKind::Md)
        );
        assert_eq!(
            classify(Path::new("/tmp/a.doc")).unwrap(),
            ImportKind::LegacyDoc
        );
        assert_eq!(
            classify(Path::new("/tmp/a.PPT")).unwrap(),
            ImportKind::LegacyPpt
        );
    }

    #[test]
    fn size_limit_is_512mb_for_pdf_and_50mb_for_others() {
        assert!(check_size(KbKind::Pdf, 100 << 20).is_ok());
        assert!(check_size(KbKind::Pdf, 600 << 20).is_err());
        assert!(check_size(KbKind::Docx, 60 << 20).is_err());
    }

    #[test]
    fn gbk_text_is_readable() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("gbk.txt");
        let (encoded, _, _) = encoding_rs::GBK.encode("这是一段 GBK 编码的中文文本。");
        std::fs::write(&path, encoded.as_ref()).expect("write");
        let text = read_text_file(&path).expect("read");
        assert!(text.contains("GBK 编码"));
    }

    #[test]
    fn blank_lines_are_collapsed() {
        assert_eq!(normalize_blank_lines("a\n\n\n\nb\n"), "a\n\nb");
    }

    #[test]
    fn detect_converter_does_not_panic() {
        // 只保证不崩、能返回一个确定值；具体是哪个取决于本机装了什么。
        let found = detect_converter();
        assert!(matches!(
            found,
            Converter::Soffice | Converter::Office | Converter::None
        ));
    }
}
