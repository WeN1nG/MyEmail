//! 目录扫描：只找文件、生成显示名，不读音频内容。
//!
//! 时长探测属于解码能力，放在 `audio/backend.rs`；这样本模块是纯文件系统遍历，
//! 可以用临时目录覆盖大小写扩展名、隐藏项、深度与数量上限。

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

/// 支持的扩展名（比较时统一转小写）。
pub const AUDIO_EXTENSIONS: [&str; 4] = ["mp3", "flac", "wav", "ogg"];

#[derive(Debug, Clone, Copy)]
pub struct ScanLimits {
    pub max_depth: usize,
    pub max_files: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_depth: 8,
            max_files: 2000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Track {
    pub path: PathBuf,
    /// 显示名：文件名去掉扩展名。
    pub title: String,
    /// 标签里的标题；补齐后优先显示它（空 = 还没读到或文件没有标签）。
    pub tag_title: Option<String>,
    /// 歌手：先是文件名推断，标签补齐后以标签为准。
    pub artist: Option<String>,
    /// 时长（毫秒）；`None` = 尚未探测或探测失败。
    pub duration_ms: Option<u64>,
}

impl Track {
    /// 列表/摘要用的曲名：标签标题优先，回退文件名。
    ///
    /// 与专辑页的 `tags::display_title` 是同一套取值顺序，否则同一首歌两处两个名字。
    pub fn label(&self) -> &str {
        self.tag_title.as_deref().unwrap_or(&self.title)
    }
}

/// 搜索匹配哪个字段。`Any` 只给 AI 用（曲库语义）；界面只有歌曲名 / 歌手两个选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchField {
    Any,
    #[default]
    Title,
    Artist,
}

/// 后台补齐的单曲信息（一次打开文件拿全：时长 + 标签标题 + 歌手）。
#[derive(Debug, Clone, Default)]
pub struct TrackInfo {
    /// 曲库索引。
    pub index: usize,
    pub duration_ms: Option<u64>,
    pub tag_title: Option<String>,
    pub artist: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    pub tracks: Vec<Track>,
    /// 扩展名不符而被跳过的文件数（用于「已跳过 N 个文件」提示）。
    pub skipped: usize,
    /// 是否因达到 `max_files` 而截断。
    pub truncated: bool,
}

pub fn scan_dir_with(root: &Path, limits: ScanLimits) -> ScanResult {
    let mut result = ScanResult::default();
    if !root.is_dir() {
        return result;
    }

    let mut found: Vec<(String, PathBuf)> = Vec::new();
    let walker = WalkDir::new(root)
        .max_depth(limits.max_depth)
        .follow_links(false)
        .into_iter()
        // 只在与根目录的相对路径上判断隐藏：根目录本身可能就在 `.music` 这类
        // 隐藏目录里（临时目录更是默认 `.tmp*`），不能因此整棵树都跳过。
        .filter_entry(|entry| {
            entry
                .path()
                .strip_prefix(root)
                .map(|relative| !is_hidden(relative))
                .unwrap_or(true)
        });

    for entry in walker {
        let Ok(entry) = entry else {
            result.skipped += 1;
            continue;
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !has_audio_extension(path) {
            result.skipped += 1;
            continue;
        }
        let relative = path.strip_prefix(root).unwrap_or(path);
        // 用小写相对路径排序：大小写不敏感、跨平台一致、结果确定。
        found.push((
            relative.to_string_lossy().to_lowercase(),
            path.to_path_buf(),
        ));
        if found.len() >= limits.max_files {
            result.truncated = true;
            break;
        }
    }

    found.sort_by(|a, b| a.0.cmp(&b.0));
    result.tracks = found
        .into_iter()
        .map(|(_, path)| {
            let title = display_title(&path);
            let artist = infer_artist(&title);
            Track {
                path,
                title,
                tag_title: None,
                artist,
                duration_ms: None,
            }
        })
        .collect();
    result
}

/// 从「艺术家 - 歌名」式文件名里推断歌手。
///
/// 扫描阶段不读标签（几千次文件 IO + 解析），先吃文件名这条免费线索；
/// 真正的标签由 `audio::tags::read_track_info` 后台补齐并**覆盖**它。
///
/// 排除 `01 - Intro` 这类：左侧是纯编号时不是歌手。
fn infer_artist(title: &str) -> Option<String> {
    let (left, _) = title.split_once(" - ")?;
    let artist = left.trim();
    if artist.chars().count() < 2 {
        return None;
    }
    if artist.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(artist.to_string())
}

pub fn has_audio_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            let lower = ext.to_ascii_lowercase();
            AUDIO_EXTENSIONS.contains(&lower.as_str())
        })
        .unwrap_or(false)
}

/// 文件名的可读形式；非 UTF-8 路径用有损转换，不 panic。
fn display_title(path: &Path) -> String {
    path.file_stem()
        .or_else(|| path.file_name())
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// 隐藏项：相对路径任一段以 `.` 开头。
fn is_hidden(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .map(|name| name.starts_with('.'))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, b"x").expect("write");
    }

    #[test]
    fn collects_supported_extensions_case_insensitively() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        touch(&root.join("a.mp3"));
        touch(&root.join("B.FLAC"));
        touch(&root.join("sub/c.wav"));
        touch(&root.join("sub/deep/d.OGG"));
        touch(&root.join("notes.txt"));
        touch(&root.join(".hidden.mp3"));
        touch(&root.join(".git/ignored.mp3"));

        let result = scan_dir_with(root, ScanLimits::default());
        let names: Vec<&str> = result.tracks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(
            names,
            vec!["a", "B", "c", "d"],
            "按相对路径升序、大小写归一"
        );
        assert_eq!(result.skipped, 1, "只统计扩展名不符的文件");
        assert!(!result.truncated);
    }

    #[test]
    fn missing_directory_is_empty_not_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = scan_dir_with(&dir.path().join("does-not-exist"), ScanLimits::default());
        assert!(result.tracks.is_empty());
        assert!(!result.truncated);
    }

    #[test]
    fn depth_limit_skips_deep_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        touch(&root.join("top.mp3"));
        touch(&root.join("l1/l2/l3/deep.mp3"));

        let result = scan_dir_with(
            root,
            ScanLimits {
                max_depth: 2,
                max_files: 100,
            },
        );
        assert_eq!(result.tracks.len(), 1, "超过深度的文件不计入");
        assert_eq!(result.tracks[0].title, "top");
    }

    #[test]
    fn file_limit_truncates_and_flags() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for i in 0..5 {
            touch(&root.join(format!("t{i}.mp3")));
        }
        let result = scan_dir_with(
            root,
            ScanLimits {
                max_depth: 8,
                max_files: 3,
            },
        );
        assert_eq!(result.tracks.len(), 3);
        assert!(result.truncated, "达到上限必须标记，界面要提示");
    }

    #[test]
    fn durations_start_unknown() {
        let dir = tempfile::tempdir().expect("tempdir");
        touch(&dir.path().join("a.mp3"));
        let result = scan_dir_with(dir.path(), ScanLimits::default());
        assert!(result.tracks[0].duration_ms.is_none(), "时长由解码层补");
    }

    #[test]
    fn extension_check_is_strict() {
        assert!(has_audio_extension(Path::new("a.MP3")));
        assert!(has_audio_extension(Path::new("a.flac")));
        assert!(!has_audio_extension(Path::new("a.mp3.txt")));
        assert!(!has_audio_extension(Path::new("a")));
        assert!(!has_audio_extension(Path::new("a.m4a")), "不在 v0.3 范围");
    }

    #[test]
    fn artist_is_inferred_from_file_name_when_possible() {
        assert_eq!(infer_artist("周杰伦 - 晴天"), Some("周杰伦".to_string()));
        assert_eq!(infer_artist("AB - C"), Some("AB".to_string()), "两字符也认");
        assert_eq!(infer_artist("A - B"), None, "单字符太短，多半不是歌手");
        assert_eq!(infer_artist("01 - Intro"), None, "编号不是歌手");
        assert_eq!(infer_artist("晴天"), None, "没有分隔符就没有线索");
        assert_eq!(infer_artist(" - 无名"), None, "左侧为空");
        assert_eq!(infer_artist("周杰伦-晴天"), None, "要求两侧带空格");
    }

    #[test]
    fn scan_fills_filename_artist_and_leaves_tag_fields_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("周杰伦 - 晴天.mp3"), b"x").expect("write");
        let result = scan_dir_with(dir.path(), ScanLimits::default());
        let track = &result.tracks[0];
        assert_eq!(track.title, "周杰伦 - 晴天");
        assert_eq!(track.artist.as_deref(), Some("周杰伦"));
        assert_eq!(track.tag_title, None, "标签留给后台补齐");
        assert_eq!(track.label(), "周杰伦 - 晴天", "没有标签标题时回退文件名");
    }

    #[test]
    fn label_prefers_the_tag_title() {
        let track = Track {
            path: PathBuf::from("01. intro.mp3"),
            title: "01. intro".to_string(),
            tag_title: Some("前奏".to_string()),
            artist: None,
            duration_ms: None,
        };
        assert_eq!(track.label(), "前奏");
    }
}
