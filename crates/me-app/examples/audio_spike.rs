//! 音频后端 spike：验证 rodio 0.23 在本机的能力边界。
//!
//! 只回答五个问题，不参与产品逻辑：
//! 1. 能否打开默认输出（`default-features = false` + `playback` 是否够用）
//! 2. MP3 / FLAC / WAV / OGG 能否解码
//! 3. `get_pos` 在暂停期间是否冻结
//! 4. `try_seek` 是否成功且位置立即更新
//! 5. 曲目结束能否被可靠检测
//!
//! 用法：`cargo run -p me-app --example audio_spike -- checktemp/audio/*`

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rodio::{ChannelCount, Decoder, Player, SampleRate, Source};

/// 在源末尾打标记：`next()` 返回 `None` 即「这首放完」。
///
/// 不依赖 `Player::empty()`——暂停时队列同样非空，语义分不清「放完」与「暂停」。
struct FinishFlag<S> {
    inner: S,
    done: Arc<AtomicBool>,
}

impl<S: Source> Iterator for FinishFlag<S> {
    type Item = rodio::Sample;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.inner.next();
        if item.is_none() {
            self.done.store(true, Ordering::Relaxed);
        }
        item
    }
}

impl<S: Source> Source for FinishFlag<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }

    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), rodio::source::SeekError> {
        self.inner.try_seek(pos)
    }
}

fn main() {
    let files: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if files.is_empty() {
        eprintln!("用法: audio_spike <文件>...");
        return;
    }

    // 1. 打开默认输出。
    let sink = match rodio::DeviceSinkBuilder::open_default_sink() {
        Ok(sink) => sink,
        Err(err) => {
            println!("DEVICE_ERROR {err}");
            return;
        }
    };
    println!("DEVICE_OK sample_rate={:?}", sink.config().sample_rate());
    let player = Player::connect_new(sink.mixer());

    for file in &files {
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        // 2. 解码。
        let decoder = match File::open(file)
            .map_err(|e| e.to_string())
            .and_then(|f| Decoder::try_from(f).map_err(|e| e.to_string()))
        {
            Ok(decoder) => decoder,
            Err(err) => {
                println!("DECODE_FAIL {name} {err}");
                continue;
            }
        };
        let duration = decoder.total_duration();
        let seekable = decoder.total_duration().is_some();

        let done = Arc::new(AtomicBool::new(false));
        player.append(FinishFlag {
            inner: decoder,
            done: done.clone(),
        });
        player.play();
        std::thread::sleep(Duration::from_millis(400));

        let pos_playing = player.get_pos();

        // 3. 暂停期间位置是否冻结。
        player.pause();
        let paused_first = player.get_pos();
        let empty_while_paused = player.empty();
        std::thread::sleep(Duration::from_millis(1_500));
        let paused_second = player.get_pos();

        // 4. seek。
        player.play();
        let seek_result = player.try_seek(Duration::from_millis(1_500));
        std::thread::sleep(Duration::from_millis(200));
        let pos_after_seek = player.get_pos();

        // 5. 等自然结束（最多 6 秒，避免测试文件过长时挂住）。
        let started = Instant::now();
        while !done.load(Ordering::Relaxed) && started.elapsed() < Duration::from_secs(6) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let finished = done.load(Ordering::Relaxed);
        let waited = started.elapsed();

        println!(
            "TRACK {name} duration={:?} seekable={seekable} pos_playing={:?} \
             paused_first={:?} paused_second={:?} pos_frozen={} empty_while_paused={} \
             seek={:?} pos_after_seek={:?} finished={finished} wait={:?}",
            duration,
            pos_playing,
            paused_first,
            paused_second,
            paused_first == paused_second,
            empty_while_paused,
            seek_result.map(|_| "ok").map_err(|e| format!("{e}")),
            pos_after_seek,
            waited,
        );

        player.clear();
    }

    // 显式释放：确认 Drop 不会卡住。
    let dropped = Instant::now();
    drop(player);
    drop(sink);
    println!("DROP_OK {:?}", dropped.elapsed());
}
