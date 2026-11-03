//! 读取当前曲目的标签与封面。
//!
//! 用 `symphonia` 而不引第三方标签库：它本来就随 rodio 一起编译进来（版本与
//! 编码特性完全对齐），所以这里不增加任何依赖、不联网，只在**切歌那一刻**
//! 读当前这一个文件。曲库扫描仍然只看文件名与扩展名。
//!
//! 封面来源三级回退：内嵌图 → 同目录 `cover/folder/front…` → 交给界面画占位。

use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{
    Metadata, MetadataOptions, MetadataRevision, StandardTagKey, StandardVisualKey, Value,
};
use symphonia::core::probe::Hint;

/// 封面最长边。界面最大也就 400px 左右，再大纯属浪费显存。
const COVER_MAX_EDGE: u32 = 640;
/// 内嵌图片的体积上限：防止个别文件里塞几十 MB 的图。
const MAX_COVER_BYTES: usize = 8 * 1024 * 1024;
/// 同目录封面文件名（不含扩展名），按优先级排列。
const FOLDER_COVER_STEMS: [&str; 6] = ["cover", "folder", "front", "albumart", "album", "artwork"];
const FOLDER_COVER_EXTS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

#[derive(Clone, PartialEq, Eq)]
pub struct CoverImage {
    pub width: u32,
    pub height: u32,
    /// 未预乘的 RGBA8。
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for CoverImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoverImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.rgba.len())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NowPlaying {
    /// 这条信息属于哪个文件：换歌时用它判断旧封面纹理是否过期。
    pub path: PathBuf,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub cover: Option<CoverImage>,
}

/// 读一个文件的标签与封面；任何失败都只是"这一项没有"，从不返回错误。
pub fn read_now_playing(path: &Path) -> NowPlaying {
    let tags = read_tags(path);
    let cover = tags
        .picture
        .as_deref()
        .and_then(decode_cover)
        .or_else(|| folder_cover(path));
    NowPlaying {
        path: path.to_path_buf(),
        title: tags.title,
        artist: tags.artist,
        album: tags.album,
        cover,
    }
}

#[derive(Default)]
struct Tags {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    picture: Option<Vec<u8>>,
}

fn read_tags(path: &Path) -> Tags {
    let mut tags = Tags::default();
    let Ok(file) = File::open(path) else {
        return tags;
    };
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(OsStr::to_str) {
        hint.with_extension(ext);
    }
    let options = MetadataOptions {
        limit_visual_bytes: symphonia::core::meta::Limit::Maximum(MAX_COVER_BYTES),
        ..Default::default()
    };
    let Ok(mut probed) =
        symphonia::default::get_probe().format(&hint, stream, &FormatOptions::default(), &options)
    else {
        return tags;
    };

    // MP3 的 ID3v2 在探测阶段就被读出来了，FLAC/OGG 的标签挂在容器里，
    // 两处都看一遍，先到先得。
    if let Some(metadata) = probed.metadata.get() {
        apply_metadata(&metadata, &mut tags);
    }
    let metadata = probed.format.metadata();
    apply_metadata(&metadata, &mut tags);
    tags
}

fn apply_metadata(metadata: &Metadata<'_>, tags: &mut Tags) {
    let Some(revision) = metadata.current() else {
        return;
    };
    apply_revision(revision, tags);
}

fn apply_revision(revision: &MetadataRevision, tags: &mut Tags) {
    for tag in revision.tags() {
        // 只关心字符串标签；数值/二进制类型（时长、封面指针等）与展示无关。
        let Value::String(value) = &tag.value else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let slot = match tag.std_key {
            Some(StandardTagKey::TrackTitle) => &mut tags.title,
            Some(StandardTagKey::Artist | StandardTagKey::AlbumArtist) => &mut tags.artist,
            Some(StandardTagKey::Album) => &mut tags.album,
            _ => continue,
        };
        if slot.is_none() {
            *slot = Some(value.to_string());
        }
    }

    if tags.picture.is_none() {
        // 优先正封面；没有标注用途就退而取第一张。
        let pick = revision
            .visuals()
            .iter()
            .find(|visual| visual.usage == Some(StandardVisualKey::FrontCover))
            .or_else(|| revision.visuals().first());
        if let Some(visual) = pick
            && !visual.data.is_empty()
            && visual.data.len() <= MAX_COVER_BYTES
        {
            tags.picture = Some(visual.data.to_vec());
        }
    }
}

fn decode_cover(bytes: &[u8]) -> Option<CoverImage> {
    let image = image::load_from_memory(bytes).ok()?;
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 {
        return None;
    }
    let image = if width.max(height) > COVER_MAX_EDGE {
        image.thumbnail(COVER_MAX_EDGE, COVER_MAX_EDGE)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    Some(CoverImage {
        width,
        height,
        rgba: rgba.into_raw(),
    })
}

/// 曲目所在目录里的封面图（很多播放器抓轨后会单独存一张）。
fn folder_cover(track: &Path) -> Option<CoverImage> {
    let dir = track.parent()?;
    for stem in FOLDER_COVER_STEMS {
        for ext in FOLDER_COVER_EXTS {
            let candidate = dir.join(format!("{stem}.{ext}"));
            let Ok(metadata) = candidate.metadata() else {
                continue;
            };
            if !metadata.is_file() || metadata.len() as usize > MAX_COVER_BYTES {
                continue;
            }
            let Ok(bytes) = std::fs::read(&candidate) else {
                continue;
            };
            if let Some(cover) = decode_cover(&bytes) {
                return Some(cover);
            }
        }
    }
    None
}

/// 界面展示用的曲名：标签优先，回退到文件名。
pub fn display_title(track_title: &str, now: Option<&NowPlaying>) -> String {
    now.and_then(|now| now.title.clone())
        .unwrap_or_else(|| track_title.to_string())
}

/// 艺术家 + 专辑，缺哪个就少哪段。
pub fn subtitle(now: Option<&NowPlaying>) -> String {
    let Some(now) = now else {
        return String::new();
    };
    match (&now.artist, &now.album) {
        (Some(artist), Some(album)) => format!("{artist} · {album}"),
        (Some(artist), None) => artist.clone(),
        (None, Some(album)) => album.clone(),
        (None, None) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn write_png(path: &Path, width: u32, height: u32) {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([200, 40, 40, 255]));
        image.save(path).expect("写测试图片");
    }

    #[test]
    fn garbage_file_yields_empty_tags_without_panic() {
        let dir = temp_dir();
        let path = dir.path().join("broken.mp3");
        std::fs::write(&path, b"not really an mp3").expect("write");
        let now = read_now_playing(&path);
        assert_eq!(now.title, None);
        assert_eq!(now.artist, None);
        assert_eq!(now.cover, None);
    }

    #[test]
    fn folder_cover_is_used_when_nothing_is_embedded() {
        let dir = temp_dir();
        let track = dir.path().join("track.mp3");
        std::fs::write(&track, b"x").expect("write");
        write_png(&dir.path().join("cover.png"), 32, 24);

        let now = read_now_playing(&track);
        let cover = now.cover.expect("应找到同目录封面");
        assert_eq!((cover.width, cover.height), (32, 24));
        assert_eq!(cover.rgba.len(), (cover.width * cover.height * 4) as usize);
    }

    #[test]
    fn oversized_cover_is_downscaled() {
        let dir = temp_dir();
        let track = dir.path().join("track.mp3");
        std::fs::write(&track, b"x").expect("write");
        write_png(&dir.path().join("folder.png"), 1600, 800);

        let cover = read_now_playing(&track).cover.expect("应有封面");
        assert!(
            cover.width.max(cover.height) <= COVER_MAX_EDGE,
            "长边应被压到 {COVER_MAX_EDGE} 以内，实际 {}x{}",
            cover.width,
            cover.height
        );
        assert!(cover.width > cover.height, "宽高比应保持");
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let now = read_now_playing(Path::new("C:/definitely/not/here.mp3"));
        assert!(now.cover.is_none());
        assert!(now.title.is_none());
    }

    #[test]
    fn title_falls_back_to_file_name() {
        let now = NowPlaying::default();
        assert_eq!(display_title("文件名", Some(&now)), "文件名");
        assert_eq!(display_title("文件名", None), "文件名");

        let tagged = NowPlaying {
            title: Some("标签标题".into()),
            ..Default::default()
        };
        assert_eq!(display_title("文件名", Some(&tagged)), "标签标题");
    }

    #[test]
    fn subtitle_skips_missing_parts() {
        let cases = [
            (Some("A"), Some("B"), "A · B"),
            (Some("A"), None, "A"),
            (None, Some("B"), "B"),
            (None, None, ""),
        ];
        for (artist, album, want) in cases {
            let now = NowPlaying {
                artist: artist.map(str::to_string),
                album: album.map(str::to_string),
                ..Default::default()
            };
            assert_eq!(subtitle(Some(&now)), want);
        }
        assert_eq!(subtitle(None), "");
    }

    #[test]
    fn reads_tags_and_embedded_cover_from_a_real_mp3() {
        // 一个 0.4 秒的 440Hz 单声道 MP3，带 ID3v2 标签与 64×64 内嵌封面。
        const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/tagged.mp3");
        let dir = temp_dir();
        let path = dir.path().join("tagged.mp3");
        std::fs::write(&path, FIXTURE).expect("write");

        let now = read_now_playing(&path);
        assert_eq!(now.title.as_deref(), Some("测试曲目"));
        assert_eq!(now.artist.as_deref(), Some("测试艺术家"));
        assert_eq!(now.album.as_deref(), Some("测试专辑"));
        let cover = now.cover.expect("应解析出内嵌封面");
        assert_eq!((cover.width, cover.height), (64, 64));
    }
}
