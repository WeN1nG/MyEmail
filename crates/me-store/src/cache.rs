//! 磁盘缓存：路径规则、读写、gzip、清理与占用统计。
//!
//! 命名规则见 `doc/数据结构.md` §2.1 / §4.8：文件名一律使用本地
//! `messages.id` / `drafts.id`，绝不使用 RFC 5322 `Message-ID`。

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use me_core::{CoreError, DraftId, MessageId};

use crate::storage;

/// 附件根目录归属标记：存在该文件的目录可安全整目录迁移/清空。
pub const ATTACHMENT_MARKER: &str = ".myemail-attachments";

/// 缓存根目录与各子目录的路径解析器。
///
/// `new` 会确保 `bodies/ raw/ attach/ drafts/ logs/` 存在。
#[derive(Debug, Clone)]
pub struct CachePaths {
    root: PathBuf,
    bodies: PathBuf,
    raw: PathBuf,
    attach: PathBuf,
    drafts: PathBuf,
    logs: PathBuf,
}

impl CachePaths {
    pub fn new(root: PathBuf) -> std::io::Result<CachePaths> {
        let bodies = root.join("bodies");
        let raw = root.join("raw");
        let attach = root.join("attach");
        let drafts = root.join("drafts");
        let logs = root.join("logs");
        for dir in [&bodies, &raw, &attach, &drafts, &logs] {
            fs::create_dir_all(dir)?;
        }
        Ok(CachePaths {
            root,
            bodies,
            raw,
            attach,
            drafts,
            logs,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn db_file(&self) -> PathBuf {
        self.root.join("myemail.db")
    }

    pub fn bodies_dir(&self) -> &Path {
        &self.bodies
    }

    pub fn raw_dir(&self) -> &Path {
        &self.raw
    }

    pub fn attach_dir(&self) -> &Path {
        &self.attach
    }

    pub fn drafts_dir(&self) -> &Path {
        &self.drafts
    }

    pub fn logs_dir(&self) -> &Path {
        &self.logs
    }

    pub fn body_path(&self, id: MessageId) -> PathBuf {
        self.bodies.join(format!("{}.txt", id.0))
    }

    pub fn raw_path(&self, id: MessageId) -> PathBuf {
        self.raw.join(format!("{}.eml.gz", id.0))
    }

    pub fn attachment_dir(&self, id: MessageId) -> PathBuf {
        self.attach.join(id.0.to_string())
    }

    pub fn attachment_path(
        &self,
        id: MessageId,
        part_index: u32,
        filename: Option<&str>,
    ) -> PathBuf {
        attachment_path_in(&self.attach, id, part_index, filename)
    }

    pub fn draft_dir(&self, id: DraftId) -> PathBuf {
        self.drafts.join(id.0.to_string())
    }

    pub fn draft_raw_path(&self, id: DraftId) -> PathBuf {
        self.draft_dir(id).join("message.eml")
    }

    pub fn draft_attachment_dir(&self, id: DraftId) -> PathBuf {
        self.draft_dir(id).join("attachments")
    }
}

// ---------------------------------------------------------------------------
// safe_name
// ---------------------------------------------------------------------------

/// 仅保留 ASCII 字母数字、`.`、`-`、`_` 与 CJK 字符；其余替换为 `_`。
/// 去除开头 `.`、截断到 100 字符；为空则用 `attachment`。
pub fn safe_name(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| if is_safe_char(c) { c } else { '_' })
        .collect();
    let stripped = replaced.trim_start_matches('.');
    let truncated: String = stripped.chars().take(100).collect();
    if truncated.is_empty() {
        "attachment".to_string()
    } else {
        truncated
    }
}

fn is_safe_char(c: char) -> bool {
    if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
        return true;
    }
    is_cjk(c)
}

fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x3400..=0x4DBF      // CJK 扩展 A
            | 0x4E00..=0x9FFF // CJK 基本区
            | 0xF900..=0xFAFF // CJK 兼容表意文字
            | 0x3040..=0x30FF // 平假名 / 片假名
            | 0xAC00..=0xD7AF // 韩文音节
            | 0x20000..=0x2FA1F // CJK 扩展 B~F 与兼容补充
    )
}

// ---------------------------------------------------------------------------
// 正文
// ---------------------------------------------------------------------------

pub fn write_body(paths: &CachePaths, id: MessageId, text: &str) -> Result<PathBuf, CoreError> {
    let path = paths.body_path(id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| storage(format!("创建正文目录失败: {e}")))?;
    }
    fs::write(&path, text.as_bytes()).map_err(|e| storage(format!("写入正文失败: {e}")))?;
    Ok(path)
}

pub fn read_body(paths: &CachePaths, id: MessageId) -> Result<Option<String>, CoreError> {
    let path = paths.body_path(id);
    match fs::read(&path) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|e| storage(format!("正文不是合法 UTF-8: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(storage(format!("读取正文失败: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// 原始邮件（gzip）
// ---------------------------------------------------------------------------

pub fn write_raw_gz(paths: &CachePaths, id: MessageId, raw: &[u8]) -> Result<PathBuf, CoreError> {
    let path = paths.raw_path(id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| storage(format!("创建原始邮件目录失败: {e}")))?;
    }
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(raw)
        .map_err(|e| storage(format!("gzip 压缩失败: {e}")))?;
    let compressed = encoder
        .finish()
        .map_err(|e| storage(format!("gzip 压缩失败: {e}")))?;
    fs::write(&path, compressed).map_err(|e| storage(format!("写入原始邮件失败: {e}")))?;
    Ok(path)
}

pub fn read_raw_gz(paths: &CachePaths, id: MessageId) -> Result<Option<Vec<u8>>, CoreError> {
    let path = paths.raw_path(id);
    let compressed = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(storage(format!("读取原始邮件失败: {e}"))),
    };
    let mut decoder = GzDecoder::new(compressed.as_slice());
    let mut raw = Vec::new();
    decoder
        .read_to_end(&mut raw)
        .map_err(|e| storage(format!("gzip 解压失败: {e}")))?;
    Ok(Some(raw))
}

// ---------------------------------------------------------------------------
// 附件
// ---------------------------------------------------------------------------

/// 任意附件根目录下的附件文件路径（支持用户在设置中重定位的目录）。
pub fn attachment_path_in(
    dir: &Path,
    id: MessageId,
    part_index: u32,
    filename: Option<&str>,
) -> PathBuf {
    let name = safe_name(filename.unwrap_or("attachment"));
    dir.join(id.0.to_string())
        .join(format!("{part_index}_{name}"))
}

pub fn write_attachment(
    paths: &CachePaths,
    id: MessageId,
    part_index: u32,
    filename: Option<&str>,
    bytes: &[u8],
) -> Result<PathBuf, CoreError> {
    write_attachment_in(paths.attach_dir(), id, part_index, filename, bytes)
}

/// 写入附件到指定根目录（默认 `attach/` 或用户重定位后的目录）。
pub fn write_attachment_in(
    dir: &Path,
    id: MessageId,
    part_index: u32,
    filename: Option<&str>,
    bytes: &[u8],
) -> Result<PathBuf, CoreError> {
    let path = attachment_path_in(dir, id, part_index, filename);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| storage(format!("创建附件目录失败: {e}")))?;
    }
    fs::write(&path, bytes).map_err(|e| storage(format!("写入附件失败: {e}")))?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// 草稿原文
// ---------------------------------------------------------------------------

pub fn write_draft_raw(paths: &CachePaths, id: DraftId, raw: &[u8]) -> Result<PathBuf, CoreError> {
    let path = paths.draft_raw_path(id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| storage(format!("创建草稿目录失败: {e}")))?;
    }
    fs::write(&path, raw).map_err(|e| storage(format!("写入草稿原文失败: {e}")))?;
    Ok(path)
}

pub fn read_draft_raw(paths: &CachePaths, id: DraftId) -> Result<Option<Vec<u8>>, CoreError> {
    match fs::read(paths.draft_raw_path(id)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(storage(format!("读取草稿原文失败: {e}"))),
    }
}

/// 把服务器草稿的已解码附件写入托管目录；用 part_index 避免同名覆盖。
pub fn write_draft_attachment_bytes(
    paths: &CachePaths,
    id: DraftId,
    part_index: u32,
    filename: Option<&str>,
    data: &[u8],
) -> Result<PathBuf, CoreError> {
    let dir = paths.draft_attachment_dir(id);
    fs::create_dir_all(&dir).map_err(|e| storage(format!("创建草稿附件目录失败: {e}")))?;
    let safe = filename
        .filter(|name| !name.trim().is_empty())
        .map(safe_name)
        .unwrap_or_else(|| "attachment".to_string());
    let path = dir.join(format!("{part_index}_{safe}"));
    fs::write(&path, data).map_err(|e| storage(format!("写入草稿附件失败: {e}")))?;
    Ok(path)
}

/// 清空草稿托管附件目录（服务器内容覆盖本地前调用）。
pub fn clear_draft_attachments(paths: &CachePaths, id: DraftId) -> u64 {
    remove_dir_counting(&paths.draft_attachment_dir(id))
}

// ---------------------------------------------------------------------------
// 占用与清理
// ---------------------------------------------------------------------------

/// 递归统计目录（或单个文件）占用；不存在或不可读按 0 计。
pub fn dir_size(path: &Path) -> u64 {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return 0,
    };
    if metadata.file_type().is_symlink() {
        return 0;
    }
    if metadata.is_file() {
        return metadata.len();
    }
    let mut total = 0u64;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            total = total.saturating_add(dir_size(&entry.path()));
        }
    }
    total
}

fn remove_file_counting(path: &Path) -> u64 {
    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    match fs::remove_file(path) {
        Ok(()) => size,
        Err(_) => 0,
    }
}

fn remove_dir_counting(path: &Path) -> u64 {
    let size = dir_size(path);
    match fs::remove_dir_all(path) {
        Ok(()) => size,
        Err(_) => 0,
    }
}

/// 递归复制目录树（跨卷安全），目标目录可已存在，同名文件覆盖。
///
/// 不跟随符号链接；返回复制的字节数。源目录不存在时视为空目录。
pub fn copy_dir_tree(from: &Path, to: &Path) -> Result<u64, CoreError> {
    let entries = match fs::read_dir(from) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(storage(format!("读取附件目录失败: {e}"))),
    };
    fs::create_dir_all(to).map_err(|e| storage(format!("创建附件目录失败: {e}")))?;
    let mut copied = 0u64;
    for entry in entries {
        let entry = entry.map_err(|e| storage(format!("读取附件目录失败: {e}")))?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|e| storage(format!("读取附件类型失败: {e}")))?;
        if file_type.is_dir() {
            copied = copied.saturating_add(copy_dir_tree(&source, &target)?);
        } else if file_type.is_file() {
            fs::copy(&source, &target).map_err(|e| {
                storage(format!(
                    "复制附件失败 {} -> {}: {e}",
                    source.display(),
                    target.display()
                ))
            })?;
            copied = copied.saturating_add(fs::metadata(&target).map(|m| m.len()).unwrap_or(0));
        }
    }
    Ok(copied)
}

/// 目录是否为空（不存在视为空）。
pub fn dir_is_empty(path: &Path) -> bool {
    match fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_none(),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

/// 删除一批邮件的正文 / 原始邮件 / 附件目录，返回释放字节数（尽力而为）。
pub fn remove_message_files(paths: &CachePaths, ids: &[MessageId]) -> u64 {
    remove_message_files_in(paths, paths.attach_dir(), ids)
}

/// 同 [`remove_message_files`]，附件目录使用调用方给定的根目录。
pub fn remove_message_files_in(paths: &CachePaths, attach_dir: &Path, ids: &[MessageId]) -> u64 {
    let mut freed = 0u64;
    for &id in ids {
        freed = freed.saturating_add(remove_file_counting(&paths.body_path(id)));
        freed = freed.saturating_add(remove_file_counting(&paths.raw_path(id)));
        freed = freed.saturating_add(remove_dir_counting(&attach_dir.join(id.0.to_string())));
    }
    freed
}

/// 删除草稿目录，返回释放字节数。
pub fn remove_draft_dir(paths: &CachePaths, id: DraftId) -> u64 {
    remove_dir_counting(&paths.draft_dir(id))
}

/// 清理 `drafts/` 下没有对应草稿行的目录，返回释放字节数。
///
/// 仅处理名称可解析为 `DraftId` 的目录；其他名称交由用户或后续版本处理。
pub fn cleanup_orphan_draft_dirs(paths: &CachePaths, valid: &[DraftId]) -> u64 {
    let valid: HashSet<i64> = valid.iter().map(|id| id.0).collect();
    let mut freed = 0u64;
    let entries = match fs::read_dir(paths.drafts_dir()) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }
        let id = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i64>().ok());
        match id {
            Some(id) if !valid.contains(&id) => {
                freed = freed.saturating_add(remove_dir_counting(&path));
            }
            _ => {}
        }
    }
    freed
}

/// 缓存根目录总占用（含数据库与 WAL，递归统计）。
pub fn total_size(paths: &CachePaths) -> u64 {
    dir_size(paths.root())
}
