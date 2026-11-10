//! 音乐播放器的音频线程与状态机。
//!
//! 分层：
//! - [`PlayerCore`] 是纯状态机，靠 [`AudioBackend`] trait 操作后端，可在测试里用 mock 驱动；
//! - [`AudioPlayer`] 是线程门面，UI 只与它交互：发命令、收事件，绝不直接读音频文件。
//!
//! 播放状态不进 Command/Event 通道，也不落库（文档 §2.4 / §5.5.3）。

pub mod backend;
pub mod playlist;
pub mod spectrum;
pub mod tags;

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use me_core::{MusicOrder, MusicQueue};
use rand::SeedableRng;
use rand::rngs::StdRng;

use backend::{AudioBackend, AudioError, BackendFactory, RodioBackend};
use playlist::{ScanLimits, Track, TrackInfo};
use spectrum::{SpectrumFrame, SpectrumSink};
use std::sync::Arc;

/// 每次补多少条时长，避免一次探测 2000 首把命令响应拖住。
const PROBE_BATCH: usize = 8;
/// 连续多少首加载失败后停止（文档 §5.5.2）。
const MAX_CONSECUTIVE_FAILURES: u32 = 3;
/// 播放中的状态回传间隔。
const TICK: Duration = Duration::from_millis(250);
/// `Prev` 在本曲播放超过这个时长时先回到开头。
const PREV_RESTART_THRESHOLD: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub enum AudioCommand {
    LoadDirectory(PathBuf),
    Rescan,
    Play {
        index: usize,
        path: PathBuf,
    },
    Toggle,
    Next,
    Prev,
    /// 毫秒。
    Seek(u64),
    SetOrder(MusicOrder),
    /// 把播放队列裁剪到这些曲库索引（`None` = 全库）。
    ///
    /// 只影响「下一首 / 上一首 / 自动续播走哪些歌」，**不改变正在播放的曲目**。
    /// 这是曲目搜索的落点：界面只在命中集合变化时下发一次。
    SetScope(Option<Vec<usize>>),
    Stop,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioState {
    pub current: Option<usize>,
    pub playing: bool,
    pub position_ms: u64,
    pub duration_ms: Option<u64>,
    pub order: MusicOrder,
    pub error: Option<String>,
    pub scanning: bool,
}

#[derive(Debug, Clone)]
pub enum AudioEvent {
    Playlist {
        tracks: Vec<Track>,
        skipped: usize,
        truncated: bool,
    },
    /// 批量补全的曲目信息（时长 + 标签标题 + 歌手）。
    ///
    /// 一次打开文件拿三样：分开两条事件会让每首文件开两次，扫描期白白翻倍 IO。
    TrackInfo(Vec<TrackInfo>),
    Error(String),
    /// 轻量状态快照：UI 每帧只取最新一条。
    State(AudioState),
    /// 设备打不开：音频线程进入降级态，只等待退出。
    DeviceError(String),
    /// 切歌时读出的标签与封面（只在换曲时发一次）。
    NowPlaying(tags::NowPlaying),
}

// ---------------------------------------------------------------------------
// 状态机
// ---------------------------------------------------------------------------

pub struct PlayerCore {
    backend: Box<dyn AudioBackend>,
    queue: MusicQueue,
    rng: StdRng,
    /// 后端是否已加载了当前曲目。用于区分「暂停」与「尚未开始」。
    loaded: bool,
    playing: bool,
    position: Duration,
    duration: Option<Duration>,
    error: Option<String>,
    fail_streak: u32,
    scanning: bool,
    pending_probe: Vec<usize>,
    active_dir: Option<PathBuf>,
}

impl PlayerCore {
    pub fn new(backend: Box<dyn AudioBackend>, rng: StdRng) -> Self {
        Self {
            backend,
            queue: MusicQueue::new(),
            rng,
            loaded: false,
            playing: false,
            position: Duration::ZERO,
            duration: None,
            error: None,
            fail_streak: 0,
            scanning: false,
            pending_probe: Vec::new(),
            active_dir: None,
        }
    }

    pub fn state(&self) -> AudioState {
        AudioState {
            current: self.queue.current_index(),
            playing: self.playing,
            position_ms: self.position.as_millis() as u64,
            duration_ms: self.duration.map(|d| d.as_millis() as u64),
            order: self.queue.order(),
            error: self.error.clone(),
            scanning: self.scanning,
        }
    }

    pub fn set_scanning(&mut self, scanning: bool) {
        self.scanning = scanning;
    }

    pub fn handle(&mut self, command: AudioCommand) -> Vec<AudioEvent> {
        let mut events = Vec::new();
        match command {
            AudioCommand::LoadDirectory(dir) => self.load_directory(dir, &mut events),
            AudioCommand::Rescan => {
                if let Some(dir) = self.active_dir.clone() {
                    self.load_directory(dir, &mut events);
                }
            }
            AudioCommand::Play { index, path } => {
                // 索引与路径双写：重扫竞态下以路径为准，避免「点 A 播 B」。
                let resolved = if self.queue.path_at(index) == Some(path.as_path()) {
                    Some(index)
                } else {
                    self.queue.tracks().iter().position(|p| p == &path)
                };
                if let Some(index) = resolved {
                    self.play_index(index, &mut events);
                } else {
                    self.error = Some("该曲目已不在当前列表中，请重新扫描".to_string());
                    events.push(AudioEvent::Error(self.error.clone().unwrap_or_default()));
                }
            }
            AudioCommand::Toggle => self.toggle(&mut events),
            AudioCommand::Next => match self.queue.next_index() {
                Some(next) => self.play_index(next, &mut events),
                None => self.finish_playback(),
            },
            AudioCommand::Prev => self.prev(&mut events),
            AudioCommand::Seek(ms) => self.seek(Duration::from_millis(ms), &mut events),
            AudioCommand::SetOrder(order) => self.queue.set_order(order, &mut self.rng),
            AudioCommand::SetScope(scope) => {
                // 「正在播放」由 `loaded` 判定：非空队列的游标永远有值，
                // 那是游标不是播放状态，拿它顶替会把第 0 首钉进搜索结果。
                let playing = if self.loaded {
                    self.queue.current_index()
                } else {
                    None
                };
                self.queue.set_scope(scope, playing, &mut self.rng);
            }
            AudioCommand::Stop => self.stop(),
        }
        events
    }

    /// 每个 tick 调用：刷新位置、处理曲目结束、分批补时长。
    pub fn tick(&mut self) -> Vec<AudioEvent> {
        let mut events = Vec::new();
        if self.playing {
            // 暂停时不再刷新，位置自然冻结在最后一次播放值。
            self.position = self.backend.position();
            if self.backend.is_finished() {
                self.on_finished(&mut events);
            }
        }
        self.probe_batch(&mut events);
        events
    }

    fn load_directory(&mut self, dir: PathBuf, events: &mut Vec<AudioEvent>) {
        self.stop();
        self.queue.clear();
        self.pending_probe.clear();
        self.error = None;

        let result = playlist::scan_dir_with(&dir, ScanLimits::default());
        let paths: Vec<PathBuf> = result.tracks.iter().map(|t| t.path.clone()).collect();
        let count = paths.len();
        self.queue.replace_tracks(paths, &mut self.rng);
        self.pending_probe = (0..count).collect();
        self.active_dir = Some(dir);
        self.scanning = false;

        events.push(AudioEvent::Playlist {
            tracks: result.tracks,
            skipped: result.skipped,
            truncated: result.truncated,
        });
    }

    fn toggle(&mut self, events: &mut Vec<AudioEvent>) {
        if self.playing {
            self.backend.pause();
            self.playing = false;
            return;
        }
        if self.loaded {
            self.backend.resume();
            self.playing = true;
            return;
        }
        let target = self
            .queue
            .current_index()
            .or_else(|| self.queue.first_index());
        match target {
            Some(index) => self.play_index(index, events),
            None => {
                self.error = Some("列表为空，请先选择音乐目录".to_string());
                events.push(AudioEvent::Error(self.error.clone().unwrap_or_default()));
            }
        }
    }

    fn prev(&mut self, events: &mut Vec<AudioEvent>) {
        if self.loaded && self.position >= PREV_RESTART_THRESHOLD {
            if self.backend.seek(Duration::ZERO).is_ok() {
                self.position = Duration::ZERO;
            }
            return;
        }
        if let Some(prev) = self.queue.prev_index() {
            self.play_index(prev, events);
        }
    }

    fn seek(&mut self, target: Duration, events: &mut Vec<AudioEvent>) {
        if !self.loaded {
            return;
        }
        let Some(duration) = self.duration else {
            self.error = Some("该曲目无法跳转".to_string());
            events.push(AudioEvent::Error(self.error.clone().unwrap_or_default()));
            return;
        };
        let clamped = target.min(duration);
        match self.backend.seek(clamped) {
            Ok(()) => {
                self.position = clamped;
                self.error = None;
            }
            Err(err) => {
                self.error = Some(err.to_string());
                events.push(AudioEvent::Error(err.to_string()));
            }
        }
    }

    fn stop(&mut self) {
        self.backend.stop();
        self.loaded = false;
        self.playing = false;
        self.position = Duration::ZERO;
    }

    fn finish_playback(&mut self) {
        self.playing = false;
        self.position = self.duration.unwrap_or_default();
        // 曲源已经放到末尾：`loaded` 必须落回 false，否则下一次 Toggle 会去
        // `resume()` 一个已结束的 source，表现为「按播放没反应」。
        self.loaded = false;
    }

    fn on_finished(&mut self, events: &mut Vec<AudioEvent>) {
        match self.queue.next_index() {
            Some(next) => self.play_index(next, events),
            None => self.finish_playback(),
        }
    }

    /// 播放指定曲目；失败自动顺延，连续失败达到上限才停。
    fn play_index(&mut self, index: usize, events: &mut Vec<AudioEvent>) {
        let mut attempt = Some(index);
        loop {
            let Some(current) = attempt else {
                self.playing = false;
                self.loaded = false;
                return;
            };
            if !self.queue.select(current) {
                // 作用域外的定点播放（AI 报的曲库序号、重扫竞态）：临时钉到队首播。
                // **不改作用域** —— 界面写着「5 首」时，队列不能偷偷变成全库。
                if !self.queue.pin(current) {
                    self.playing = false;
                    self.loaded = false;
                    return;
                }
            }
            let Some(path) = self.queue.current_path().map(|p| p.to_path_buf()) else {
                self.playing = false;
                self.loaded = false;
                return;
            };

            match self.backend.load(&path) {
                Ok(duration) => {
                    self.duration = duration;
                    self.position = Duration::ZERO;
                    self.loaded = true;
                    self.playing = true;
                    self.error = None;
                    self.fail_streak = 0;
                    // 只在换曲时读一次标签：扫描阶段读几千个文件太贵。
                    events.push(AudioEvent::NowPlaying(tags::read_now_playing(&path)));
                    return;
                }
                Err(err) => {
                    self.fail_streak = self.fail_streak.saturating_add(1);
                    self.error = Some(err.to_string());
                    events.push(AudioEvent::Error(err.to_string()));
                    if self.fail_streak >= MAX_CONSECUTIVE_FAILURES {
                        self.loaded = false;
                        self.playing = false;
                        self.position = Duration::ZERO;
                        return;
                    }
                    attempt = self.queue.next_index();
                }
            }
        }
    }

    fn probe_batch(&mut self, events: &mut Vec<AudioEvent>) {
        if self.pending_probe.is_empty() {
            return;
        }
        let take = PROBE_BATCH.min(self.pending_probe.len());
        let batch: Vec<usize> = self.pending_probe.drain(..take).collect();
        let mut done = Vec::new();
        for index in batch {
            let Some(path) = self.queue.path_at(index).map(|p| p.to_path_buf()) else {
                continue;
            };
            // 每首两次打开（rodio 解码器拿时长 + symphonia 拿标签），都只读文件头，
            // 第二次基本命中页缓存；`PROBE_BATCH` 维持 8，避免一次探测拖住命令响应。
            let duration_ms = self
                .backend
                .probe_duration(&path)
                .map(|duration| duration.as_millis() as u64);
            let tags = tags::read_track_info(&path);
            if duration_ms.is_none() && tags.title.is_none() && tags.artist.is_none() {
                continue;
            }
            done.push(TrackInfo {
                index,
                duration_ms,
                tag_title: tags.title,
                artist: tags.artist,
            });
        }
        if !done.is_empty() {
            events.push(AudioEvent::TrackInfo(done));
        }
    }
}

// ---------------------------------------------------------------------------
// 线程门面
// ---------------------------------------------------------------------------

pub struct AudioPlayer {
    cmd_tx: Sender<AudioCommand>,
    evt_rx: Receiver<AudioEvent>,
    handle: Option<JoinHandle<()>>,
    /// 频谱样本交接点：由门面创建，后端往里灌，UI 自己取（不走命令/事件通道）。
    spectrum: Arc<SpectrumSink>,
}

impl AudioPlayer {
    pub fn start() -> Self {
        Self::start_with(default_factory)
    }

    pub fn start_with(factory: BackendFactory) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (evt_tx, evt_rx) = mpsc::channel();
        let spectrum = SpectrumSink::new();
        let thread_spectrum = spectrum.clone();
        let handle = thread::Builder::new()
            .name("me-audio".to_string())
            .spawn(move || run_audio_thread(factory, cmd_rx, evt_tx, thread_spectrum))
            .ok();
        Self {
            cmd_tx,
            evt_rx,
            handle,
            spectrum,
        }
    }

    /// 取走自上次调用以来最新的一块频谱样本。
    pub fn spectrum(&self) -> Option<SpectrumFrame> {
        self.spectrum.take()
    }

    pub fn send(&self, command: AudioCommand) {
        // 线程退出（设备错误或应用关闭）后发送失败，直接忽略即可。
        let _ = self.cmd_tx.send(command);
    }

    pub fn try_recv(&self) -> Option<AudioEvent> {
        self.evt_rx.try_recv().ok()
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(AudioCommand::Stop);
        let Some(handle) = self.handle.take() else {
            return;
        };
        // rodio 释放 WASAPI 可能阻塞；最多等 500ms，超时就 detach，避免关窗卡死。
        let deadline = Instant::now() + Duration::from_millis(500);
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
    }
}

fn default_factory(spectrum: &Arc<SpectrumSink>) -> Result<Box<dyn AudioBackend>, AudioError> {
    Ok(Box::new(RodioBackend::open(spectrum)?))
}

fn run_audio_thread(
    factory: BackendFactory,
    cmd_rx: Receiver<AudioCommand>,
    evt_tx: Sender<AudioEvent>,
    spectrum: Arc<SpectrumSink>,
) {
    let backend = match factory(&spectrum) {
        Ok(backend) => backend,
        Err(err) => {
            let _ = evt_tx.send(AudioEvent::DeviceError(err.to_string()));
            // 降级：不再尝试播放，但仍要响应退出，避免线程泄漏。
            while let Ok(command) = cmd_rx.recv() {
                if matches!(command, AudioCommand::Stop) {
                    break;
                }
            }
            return;
        }
    };

    let mut core = PlayerCore::new(backend, StdRng::from_entropy());
    loop {
        match cmd_rx.recv_timeout(TICK) {
            Ok(AudioCommand::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(command) => {
                if matches!(
                    command,
                    AudioCommand::LoadDirectory(_) | AudioCommand::Rescan
                ) {
                    core.set_scanning(true);
                    let _ = evt_tx.send(AudioEvent::State(core.state()));
                }
                for event in core.handle(command) {
                    let _ = evt_tx.send(event);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        for event in core.tick() {
            let _ = evt_tx.send(event);
        }
        let _ = evt_tx.send(AudioEvent::State(core.state()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::fs;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct MockState {
        position: Duration,
        duration: Option<Duration>,
        finished: bool,
        loaded: Vec<PathBuf>,
        fail: HashSet<PathBuf>,
        seek_calls: Vec<Duration>,
        paused: bool,
    }

    /// 可编程后端；状态放在 `Arc<Mutex>` 里，测试能直接读写。
    struct MockBackend {
        state: Arc<Mutex<MockState>>,
    }

    impl AudioBackend for MockBackend {
        fn load(&mut self, path: &Path) -> Result<Option<Duration>, AudioError> {
            let mut state = self.state.lock().expect("lock");
            if state.fail.contains(path) {
                return Err(AudioError::Decode("mock failure".into()));
            }
            state.loaded.push(path.to_path_buf());
            state.position = Duration::ZERO;
            state.finished = false;
            state.paused = false;
            Ok(state.duration)
        }

        fn pause(&mut self) {
            self.state.lock().expect("lock").paused = true;
        }

        fn resume(&mut self) {
            self.state.lock().expect("lock").paused = false;
        }

        fn stop(&mut self) {
            let mut state = self.state.lock().expect("lock");
            state.finished = false;
            state.position = Duration::ZERO;
        }

        fn seek(&mut self, pos: Duration) -> Result<(), AudioError> {
            let mut state = self.state.lock().expect("lock");
            state.seek_calls.push(pos);
            state.position = pos;
            Ok(())
        }

        fn position(&self) -> Duration {
            self.state.lock().expect("lock").position
        }

        fn is_finished(&self) -> bool {
            self.state.lock().expect("lock").finished
        }

        fn probe_duration(&mut self, _path: &Path) -> Option<Duration> {
            self.state.lock().expect("lock").duration
        }
    }

    fn mock() -> (MockBackend, Arc<Mutex<MockState>>) {
        let state = Arc::new(Mutex::new(MockState::default()));
        (
            MockBackend {
                state: state.clone(),
            },
            state,
        )
    }

    fn core_with(mock_state: MockBackend) -> PlayerCore {
        PlayerCore::new(Box::new(mock_state), StdRng::seed_from_u64(7))
    }

    fn audio_dir(names: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in names {
            fs::write(dir.path().join(name), b"x").expect("write");
        }
        dir
    }

    fn playlist_of(events: &[AudioEvent]) -> Vec<Track> {
        events
            .iter()
            .find_map(|e| match e {
                AudioEvent::Playlist { tracks, .. } => Some(tracks.clone()),
                _ => None,
            })
            .expect("应回传播放列表")
    }

    #[test]
    fn load_directory_lists_tracks_without_autoplay() {
        let dir = audio_dir(&["b.mp3", "a.mp3", "note.txt"]);
        let (backend, _state) = mock();
        let mut core = core_with(backend);
        let events = core.handle(AudioCommand::LoadDirectory(dir.path().to_path_buf()));

        let tracks = playlist_of(&events);
        let titles: Vec<&str> = tracks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, vec!["a", "b"], "按路径排序、跳过非音频");
        assert!(!core.state().playing, "扫描完成不自动播放");
        assert_eq!(core.state().current, Some(0));
    }

    #[test]
    fn play_then_toggle_pauses_and_resumes_without_reloading() {
        let dir = audio_dir(&["a.mp3", "b.mp3"]);
        let (backend, state) = mock();
        let mut core = core_with(backend);
        core.handle(AudioCommand::LoadDirectory(dir.path().to_path_buf()));

        core.handle(AudioCommand::Play {
            index: 1,
            path: dir.path().join("b.mp3"),
        });
        assert!(core.state().playing);
        assert_eq!(core.state().current, Some(1));

        core.handle(AudioCommand::Toggle);
        assert!(!core.state().playing);
        assert!(state.lock().expect("lock").paused, "暂停应调用后端 pause");

        core.handle(AudioCommand::Toggle);
        assert!(core.state().playing);
        assert_eq!(
            state.lock().expect("lock").loaded.len(),
            1,
            "恢复不应重新加载曲目"
        );
    }

    #[test]
    fn finished_track_advances_to_next() {
        let dir = audio_dir(&["a.mp3", "b.mp3"]);
        let (backend, state) = mock();
        let mut core = core_with(backend);
        core.handle(AudioCommand::LoadDirectory(dir.path().to_path_buf()));
        core.handle(AudioCommand::Play {
            index: 0,
            path: dir.path().join("a.mp3"),
        });

        state.lock().expect("lock").finished = true;
        core.tick();
        assert_eq!(core.state().current, Some(1), "自动播下一首");
        assert!(core.state().playing);
    }

    /// 首尾相通：最后一首放完回到第一首（列表循环），不是停在那里。
    #[test]
    fn last_track_finishing_wraps_to_the_first() {
        let dir = audio_dir(&["a.mp3", "b.mp3"]);
        let (backend, state) = mock();
        let mut core = core_with(backend);
        core.handle(AudioCommand::LoadDirectory(dir.path().to_path_buf()));
        core.handle(AudioCommand::Play {
            index: 1,
            path: dir.path().join("b.mp3"),
        });

        state.lock().expect("lock").finished = true;
        core.tick();
        assert_eq!(core.state().current, Some(0), "末尾之后回到第一首");
        assert!(core.state().playing, "循环播放下不该停");
    }

    #[test]
    fn seek_is_clamped_to_duration() {
        let dir = audio_dir(&["a.mp3"]);
        let (backend, state) = mock();
        state.lock().expect("lock").duration = Some(Duration::from_secs(10));
        let mut core = core_with(backend);
        core.handle(AudioCommand::LoadDirectory(dir.path().to_path_buf()));
        core.handle(AudioCommand::Play {
            index: 0,
            path: dir.path().join("a.mp3"),
        });

        core.handle(AudioCommand::Seek(999_000));
        assert_eq!(core.state().position_ms, 10_000, "越界取时长上限");
        assert_eq!(
            state.lock().expect("lock").seek_calls.last(),
            Some(&Duration::from_secs(10))
        );
    }

    #[test]
    fn three_consecutive_failures_stop_playback() {
        let dir = audio_dir(&["a.mp3", "b.mp3", "c.mp3"]);
        let (backend, state) = mock();
        let mut core = core_with(backend);
        core.handle(AudioCommand::LoadDirectory(dir.path().to_path_buf()));

        {
            let mut guard = state.lock().expect("lock");
            guard.fail = ["a.mp3", "b.mp3", "c.mp3"]
                .iter()
                .map(|name| dir.path().join(name))
                .collect();
        }

        let events = core.handle(AudioCommand::Play {
            index: 0,
            path: dir.path().join("a.mp3"),
        });
        assert!(!core.state().playing, "连续失败达到上限必须停");
        assert!(
            events.iter().any(|e| matches!(e, AudioEvent::Error(_))),
            "至少上报一次错误"
        );
    }

    #[test]
    fn prev_restarts_current_track_when_past_threshold() {
        let dir = audio_dir(&["a.mp3", "b.mp3"]);
        let (backend, state) = mock();
        state.lock().expect("lock").duration = Some(Duration::from_secs(30));
        let mut core = core_with(backend);
        core.handle(AudioCommand::LoadDirectory(dir.path().to_path_buf()));
        core.handle(AudioCommand::Play {
            index: 1,
            path: dir.path().join("b.mp3"),
        });
        state.lock().expect("lock").position = Duration::from_secs(10);
        core.tick();

        core.handle(AudioCommand::Prev);
        assert_eq!(core.state().current, Some(1), "超过 3 秒先回本曲开头");
        assert_eq!(core.state().position_ms, 0);
    }

    #[test]
    fn prev_at_beginning_of_track_moves_to_previous() {
        let dir = audio_dir(&["a.mp3", "b.mp3"]);
        let (backend, state) = mock();
        state.lock().expect("lock").duration = Some(Duration::from_secs(30));
        let mut core = core_with(backend);
        core.handle(AudioCommand::LoadDirectory(dir.path().to_path_buf()));
        core.handle(AudioCommand::Play {
            index: 1,
            path: dir.path().join("b.mp3"),
        });
        state.lock().expect("lock").position = Duration::from_secs(1);
        core.tick();

        core.handle(AudioCommand::Prev);
        assert_eq!(core.state().current, Some(0));
    }

    #[test]
    fn set_order_keeps_current_track() {
        let dir = audio_dir(&["a.mp3", "b.mp3", "c.mp3"]);
        let (backend, _state) = mock();
        let mut core = core_with(backend);
        core.handle(AudioCommand::LoadDirectory(dir.path().to_path_buf()));
        core.handle(AudioCommand::Play {
            index: 2,
            path: dir.path().join("c.mp3"),
        });

        core.handle(AudioCommand::SetOrder(MusicOrder::Shuffle));
        assert_eq!(core.state().order, MusicOrder::Shuffle);
        assert_eq!(core.state().current, Some(2), "切顺序不换曲目");
    }

    #[test]
    fn probes_durations_in_batches() {
        let dir = audio_dir(&["a.mp3", "b.mp3", "c.mp3"]);
        let (backend, state) = mock();
        state.lock().expect("lock").duration = Some(Duration::from_secs(90));
        let mut core = core_with(backend);
        core.handle(AudioCommand::LoadDirectory(dir.path().to_path_buf()));

        let events = core.tick();
        let infos: Vec<TrackInfo> = events
            .iter()
            .filter_map(|e| match e {
                AudioEvent::TrackInfo(list) => Some(list.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(infos.len(), 3);
        assert!(infos.iter().all(|info| info.duration_ms == Some(90_000)));
        assert!(
            infos
                .iter()
                .all(|info| info.artist.is_none() && info.tag_title.is_none()),
            "假 mp3 没有标签，不该编造出歌手"
        );
    }

    #[test]
    fn scope_keeps_next_inside_the_search_results() {
        let dir = audio_dir(&["a.mp3", "b.mp3", "c.mp3", "d.mp3"]);
        let (backend, _state) = mock();
        let mut core = core_with(backend);
        core.handle(AudioCommand::LoadDirectory(dir.path().to_path_buf()));

        core.handle(AudioCommand::Play {
            index: 1,
            path: dir.path().join("b.mp3"),
        });
        core.handle(AudioCommand::SetScope(Some(vec![1, 3])));

        assert_eq!(core.state().current, Some(1), "正在播的那首留在原地");
        core.handle(AudioCommand::Next);
        assert_eq!(core.state().current, Some(3), "跳过集合外的 c");
        core.handle(AudioCommand::Next);
        assert_eq!(
            core.state().current,
            Some(1),
            "集合末尾回到集合首项，绝不跑到集合外"
        );
        assert!(core.state().playing);
    }

    /// 作用域外的定点播放（AI 报曲库序号）要能播，且**不能**把作用域放宽。
    #[test]
    fn play_outside_scope_pins_without_widening_the_scope() {
        let dir = audio_dir(&["a.mp3", "b.mp3", "c.mp3"]);
        let (backend, state) = mock();
        let mut core = core_with(backend);
        core.handle(AudioCommand::LoadDirectory(dir.path().to_path_buf()));
        core.handle(AudioCommand::SetScope(Some(vec![0, 1])));

        core.handle(AudioCommand::Play {
            index: 2,
            path: dir.path().join("c.mp3"),
        });
        assert!(core.state().playing, "点名的那首必须真的播起来");
        assert_eq!(core.state().current, Some(2));
        assert_eq!(
            state.lock().expect("lock").loaded.last(),
            Some(&dir.path().join("c.mp3"))
        );

        core.handle(AudioCommand::SetScope(Some(vec![0, 1])));
        assert_eq!(core.state().current, Some(2), "钉住的那首仍是当前曲目");
        core.handle(AudioCommand::Next);
        assert_eq!(core.state().current, Some(0), "下一首回到命中集合开头");
    }

    #[test]
    fn play_with_stale_index_falls_back_to_path() {
        let dir = audio_dir(&["a.mp3", "b.mp3"]);
        let (backend, _state) = mock();
        let mut core = core_with(backend);
        core.handle(AudioCommand::LoadDirectory(dir.path().to_path_buf()));

        // 索引 0 实际是 a，但传的是 b 的路径：应以路径为准播 b。
        core.handle(AudioCommand::Play {
            index: 0,
            path: dir.path().join("b.mp3"),
        });
        assert_eq!(core.state().current, Some(1));
    }

    /// 生成一个 16-bit 单声道 WAV，避免测试依赖外部音频文件。
    fn write_test_wav(path: &Path, seconds: f32) {
        let rate = 22_050u32;
        let count = (rate as f32 * seconds) as u32;
        let data_len = count * 2;
        let mut data = Vec::with_capacity(44 + data_len as usize);
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&(36 + data_len).to_le_bytes());
        data.extend_from_slice(b"WAVEfmt ");
        data.extend_from_slice(&16u32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&rate.to_le_bytes());
        data.extend_from_slice(&(rate * 2).to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&16u16.to_le_bytes());
        data.extend_from_slice(b"data");
        data.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..count {
            let t = i as f32 / rate as f32;
            let value = (t * 440.0 * std::f32::consts::TAU).sin() * 0.02;
            data.extend_from_slice(&((value * i16::MAX as f32) as i16).to_le_bytes());
        }
        fs::write(path, data).expect("写测试音频");
    }

    /// 真实设备 + 真实解码的冒烟测试。CI 无音频设备，默认忽略。
    ///
    /// 手动运行：`cargo test -p me-app --bin myemail -- --ignored real_backend`
    #[test]
    #[ignore = "需要真实音频输出设备"]
    fn real_backend_plays_seeks_and_finishes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tone.wav");
        write_test_wav(&path, 1.0);

        let spectrum = SpectrumSink::new();
        let backend = match RodioBackend::open(&spectrum) {
            Ok(backend) => backend,
            Err(err) => {
                eprintln!("跳过：无法打开音频设备（{err}）");
                return;
            }
        };
        let mut core = PlayerCore::new(Box::new(backend), StdRng::seed_from_u64(1));
        core.handle(AudioCommand::LoadDirectory(dir.path().to_path_buf()));
        // 扫描完成后时长由 tick 补齐。
        core.tick();
        core.handle(AudioCommand::Play {
            index: 0,
            path: path.clone(),
        });
        assert!(core.state().playing, "应进入播放状态");
        assert!(
            core.state().duration_ms.unwrap_or(0) >= 900,
            "真实后端应报出约 1 秒的时长，实际 {:?}",
            core.state().duration_ms
        );

        std::thread::sleep(Duration::from_millis(300));
        core.tick();
        let played = core.state().position_ms;
        assert!(played > 0, "播放位置应前进，实际 {played}ms");

        // 频谱采样点必须真的收到样本：这是"柱状图会不会动"的唯一硬证据。
        let frame = spectrum.take().expect("播放中应有频谱样本");
        assert_eq!(frame.samples.len(), spectrum::FFT_SIZE);
        assert_eq!(frame.sample_rate, 22_050);
        assert!(
            frame.samples.iter().any(|sample| sample.abs() > 0.001),
            "样本不应全为静音"
        );

        // seek 到开头之后的位置，验证后端真的接受。
        core.handle(AudioCommand::Seek(600));
        assert_eq!(core.state().position_ms, 600);

        let deadline = Instant::now() + Duration::from_secs(6);
        while core.state().playing && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(80));
            core.tick();
        }
        assert!(!core.state().playing, "1 秒音频应自然结束");
    }

    #[test]
    fn playing_a_tagged_file_reports_now_playing() {
        let dir = audio_dir(&["song.mp3"]);
        // 样本里带标题/艺术家/专辑与 64×64 内嵌封面。
        fs::write(
            dir.path().join("song.mp3"),
            include_bytes!("../../tests/fixtures/tagged.mp3"),
        )
        .expect("write fixture");
        let (backend, _state) = mock();
        let mut core = core_with(backend);
        core.handle(AudioCommand::LoadDirectory(dir.path().to_path_buf()));

        let events = core.handle(AudioCommand::Play {
            index: 0,
            path: dir.path().join("song.mp3"),
        });
        let info = events
            .iter()
            .find_map(|event| match event {
                AudioEvent::NowPlaying(info) => Some(info.clone()),
                _ => None,
            })
            .expect("切歌应发出 NowPlaying");
        assert_eq!(info.path, dir.path().join("song.mp3"));
        assert_eq!(info.title.as_deref(), Some("测试曲目"));
        let cover = info.cover.expect("应带内嵌封面");
        assert_eq!((cover.width, cover.height), (64, 64));
    }

    #[test]
    fn toggle_with_empty_playlist_reports_error() {
        let (backend, _state) = mock();
        let mut core = core_with(backend);
        let events = core.handle(AudioCommand::Toggle);
        assert!(!core.state().playing);
        assert!(events.iter().any(|e| matches!(e, AudioEvent::Error(_))));
    }
}
