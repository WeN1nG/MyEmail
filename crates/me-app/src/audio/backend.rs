//! 音频输出后端：唯一允许依赖 rodio 的地方。
//!
//! 设备与解码器都必须在音频线程内创建（cpal 的输出流不是 `Send`），因此这里提供
//! 工厂函数而不是可跨线程传递的实例。`AudioBackend` trait 让状态机可以用 mock 测试。

use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rodio::{
    ChannelCount, Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, SampleRate, Source,
};

use super::spectrum::{FFT_SIZE, SpectrumSink};

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("没有可用的音频设备：{0}")]
    Device(String),
    #[error("无法打开文件：{0}")]
    Open(String),
    #[error("无法解码该音频：{0}")]
    Decode(String),
    #[error("跳转失败：{0}")]
    Seek(String),
}

pub trait AudioBackend {
    /// 加载并开始播放，返回时长（未知则 `None`）。
    fn load(&mut self, path: &Path) -> Result<Option<Duration>, AudioError>;
    fn pause(&mut self);
    fn resume(&mut self);
    fn stop(&mut self);
    fn seek(&mut self, pos: Duration) -> Result<(), AudioError>;
    fn position(&self) -> Duration;
    /// 当前曲目是否已经自然播放结束。
    fn is_finished(&self) -> bool;
    /// 只读文件头取时长，不改变当前播放。
    fn probe_duration(&mut self, path: &Path) -> Option<Duration>;
}

/// 后端工厂：在音频线程内部调用，因此可以返回 `!Send` 的实现。
///
/// 频谱交接点由门面创建后传进来：后端负责往里灌样本，UI 负责取。
pub type BackendFactory = fn(&Arc<SpectrumSink>) -> Result<Box<dyn AudioBackend>, AudioError>;

pub struct RodioBackend {
    /// 必须保活：`MixerDeviceSink` 一旦 drop，播放立即结束。
    _sink: MixerDeviceSink,
    player: Player,
    finished: Arc<AtomicBool>,
    spectrum: Arc<SpectrumSink>,
}

impl RodioBackend {
    pub fn open(spectrum: &Arc<SpectrumSink>) -> Result<Self, AudioError> {
        let mut sink = DeviceSinkBuilder::open_default_sink()
            .map_err(|err| AudioError::Device(err.to_string()))?;
        // 默认会在 drop 时打印一行提示，产品里不需要。
        sink.log_on_drop(false);
        let player = Player::connect_new(sink.mixer());
        Ok(Self {
            _sink: sink,
            player,
            finished: Arc::new(AtomicBool::new(false)),
            spectrum: spectrum.clone(),
        })
    }
}

impl AudioBackend for RodioBackend {
    fn load(&mut self, path: &Path) -> Result<Option<Duration>, AudioError> {
        let decoder = open_decoder(path)?;
        let duration = decoder.total_duration();
        // `clear()` 会等队列清空并暂停；`stop()` 是永久停止，不能用于切歌。
        self.player.clear();
        self.finished.store(false, Ordering::Relaxed);
        self.player.append(SpectrumTap {
            inner: FinishFlag {
                inner: decoder,
                done: self.finished.clone(),
            },
            sink: self.spectrum.clone(),
            pending: Vec::with_capacity(FFT_SIZE),
            sum: 0.0,
            channel: 0,
        });
        self.player.play();
        Ok(duration)
    }

    fn pause(&mut self) {
        self.player.pause();
    }

    fn resume(&mut self) {
        self.player.play();
    }

    fn stop(&mut self) {
        self.player.clear();
    }

    fn seek(&mut self, pos: Duration) -> Result<(), AudioError> {
        self.player
            .try_seek(pos)
            .map_err(|err| AudioError::Seek(err.to_string()))
    }

    fn position(&self) -> Duration {
        self.player.get_pos()
    }

    fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }

    fn probe_duration(&mut self, path: &Path) -> Option<Duration> {
        open_decoder(path).ok().and_then(|d| d.total_duration())
    }
}

fn open_decoder(path: &Path) -> Result<Decoder<std::io::BufReader<File>>, AudioError> {
    let file = File::open(path).map_err(|err| AudioError::Open(err.to_string()))?;
    Decoder::try_from(file).map_err(|err| AudioError::Decode(err.to_string()))
}

/// 在源末尾打标记。
///
/// 不依赖 `Player::empty()`：它只说明队列为空，分不清「放完」与「被清空」；
/// 而暂停时音频回调不拉取样本，这个标记也不会误置位，正好是需要的语义。
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

/// 在样本流里"顺手"抄一份给频谱分析。
///
/// 这是实时回调线程：只往预先分配好的本地缓冲里追加，攒满一块才 `try_lock`
/// 交给 UI，拿不到锁就丢掉这块。不做任何可能阻塞或分配内存的事。
struct SpectrumTap<S> {
    inner: S,
    sink: Arc<SpectrumSink>,
    /// 下混后的单声道样本，容量固定为 `FFT_SIZE`。
    pending: Vec<f32>,
    /// 当前帧内已累加的各声道样本和。
    sum: f32,
    /// 当前帧内已到达的声道数。
    channel: usize,
}

impl<S: Source> SpectrumTap<S> {
    fn flush_if_full(&mut self) {
        if self.pending.len() >= FFT_SIZE {
            let rate = self.inner.sample_rate().get();
            self.sink.publish(&self.pending, rate);
            self.pending.clear();
        }
    }
}

impl<S: Source> Iterator for SpectrumTap<S> {
    type Item = rodio::Sample;

    fn next(&mut self) -> Option<Self::Item> {
        let sample = self.inner.next()?;
        let channels = self.inner.channels().get().max(1) as usize;
        self.sum += sample;
        self.channel += 1;
        if self.channel >= channels {
            self.pending.push(self.sum / channels as f32);
            self.sum = 0.0;
            self.channel = 0;
            // 攒满一整块（1024 个单声道样本）才交接一次，避免每样本一把锁。
            self.flush_if_full();
        }
        Some(sample)
    }
}

impl<S: Source> Source for SpectrumTap<S> {
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
        // 跳转前的样本属于旧位置，必须丢掉，否则频谱会滞后一拍。
        self.pending.clear();
        self.sum = 0.0;
        self.channel = 0;
        self.inner.try_seek(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages_are_user_readable() {
        let err = AudioError::Decode("bad header".into());
        assert!(err.to_string().contains("无法解码"));
        let err = AudioError::Device("no device".into());
        assert!(err.to_string().contains("没有可用的音频设备"));
    }

    #[test]
    fn missing_file_is_open_error_not_panic() {
        let path = Path::new("C:/definitely/not/here.mp3");
        match open_decoder(path) {
            Err(AudioError::Open(_)) => {}
            Err(other) => panic!("缺失文件应返回 Open 错误，实际 {other}"),
            Ok(_) => panic!("缺失文件不应解码成功"),
        }
    }

    /// 可预测的样本源：每个样本的值就是它的序号，便于断言下混结果。
    struct FakeSource {
        samples: Vec<f32>,
        pos: usize,
        channels: u16,
        rate: u32,
    }

    impl FakeSource {
        fn new(len: usize, channels: u16, rate: u32) -> Self {
            Self {
                samples: (0..len).map(|i| i as f32).collect(),
                pos: 0,
                channels,
                rate,
            }
        }
    }

    impl Iterator for FakeSource {
        type Item = rodio::Sample;

        fn next(&mut self) -> Option<Self::Item> {
            let sample = self.samples.get(self.pos).copied()?;
            self.pos += 1;
            Some(sample)
        }
    }

    impl Source for FakeSource {
        fn current_span_len(&self) -> Option<usize> {
            Some(self.samples.len() - self.pos)
        }

        fn channels(&self) -> ChannelCount {
            std::num::NonZero::new(self.channels).expect("声道数非零")
        }

        fn sample_rate(&self) -> SampleRate {
            std::num::NonZero::new(self.rate).expect("采样率非零")
        }

        fn total_duration(&self) -> Option<Duration> {
            Some(Duration::from_secs(1))
        }

        fn try_seek(&mut self, _pos: Duration) -> Result<(), rodio::source::SeekError> {
            self.pos = 0;
            Ok(())
        }
    }

    fn tap<S: Source>(inner: S, sink: &Arc<SpectrumSink>) -> SpectrumTap<S> {
        SpectrumTap {
            inner,
            sink: sink.clone(),
            pending: Vec::with_capacity(FFT_SIZE),
            sum: 0.0,
            channel: 0,
        }
    }

    #[test]
    fn tap_publishes_a_full_block_only() {
        let sink = SpectrumSink::new();
        // 单声道：攒够 1024 个才交接，少一个都不发。
        let mut source = tap(FakeSource::new(FFT_SIZE - 1, 1, 44_100), &sink);
        for _ in 0..FFT_SIZE - 1 {
            source.next();
        }
        assert!(sink.take().is_none(), "不满一块不应发布");

        let mut source = tap(FakeSource::new(FFT_SIZE, 1, 44_100), &sink);
        for _ in 0..FFT_SIZE {
            source.next();
        }
        let frame = sink.take().expect("满一块应发布");
        assert_eq!(frame.samples.len(), FFT_SIZE);
        assert_eq!(frame.sample_rate, 44_100);
        assert_eq!(frame.samples[0], 0.0);
        assert_eq!(frame.samples[FFT_SIZE - 1], (FFT_SIZE - 1) as f32);
    }

    #[test]
    fn tap_downmixes_stereo_to_mono() {
        let sink = SpectrumSink::new();
        // 样本序列 0,2,4,…：相邻两个声道平均后应得到 0.5,2.5,4.5…
        let mut source = tap(FakeSource::new(FFT_SIZE * 2, 2, 48_000), &sink);
        for _ in 0..FFT_SIZE * 2 {
            source.next();
        }
        let frame = sink.take().expect("应发布");
        assert_eq!(frame.samples.len(), FFT_SIZE);
        assert!((frame.samples[0] - 0.5).abs() < f32::EPSILON);
        assert!((frame.samples[1] - 2.5).abs() < f32::EPSILON);
        assert_eq!(frame.sample_rate, 48_000);
    }

    #[test]
    fn seek_drops_the_partial_block() {
        let sink = SpectrumSink::new();
        let mut source = tap(FakeSource::new(FFT_SIZE * 2, 1, 44_100), &sink);
        for _ in 0..100 {
            source.next();
        }
        source.try_seek(Duration::ZERO).expect("假源支持跳转");
        // 跳转后再攒 100 个：不足一块，之前的 100 个必须已被丢弃。
        for _ in 0..100 {
            source.next();
        }
        assert!(sink.take().is_none(), "跳转后的半块数据不得混入新一块");
    }

    #[test]
    fn tap_forwards_source_metadata() {
        let sink = SpectrumSink::new();
        let source = tap(FakeSource::new(8, 2, 22_050), &sink);
        assert_eq!(source.channels().get(), 2);
        assert_eq!(source.sample_rate().get(), 22_050);
        assert_eq!(source.total_duration(), Some(Duration::from_secs(1)));
        assert_eq!(source.current_span_len(), Some(8));
    }
}
