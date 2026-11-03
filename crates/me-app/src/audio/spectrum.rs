//! 频谱：把音频回调里的样本搬到 UI 线程，再做 FFT 得到频带能量。
//!
//! 分工：
//! - [`SpectrumSink`] 是唯一的跨线程交接点。音频回调只做「整块替换」，用
//!   `try_lock` 拿不到锁就丢弃这一块 —— 回调线程永远不阻塞、稳态不分配内存；
//! - [`SpectrumAnalyzer`] 全在 UI 线程跑：加窗、FFT、对数分带、平滑。
//!
//! 不做在回调里算 FFT：那是实时线程，任何一次内存分配或锁等待都会变成爆音。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// 单次分析的窗口长度。48kHz 下约 21ms、频率分辨率 47Hz。
pub const FFT_SIZE: usize = 1024;
/// 频带数量：柱子的根数。
pub const BANDS: usize = 48;
/// 分析下限，低于它的能量基本是直流与窗泄漏。
const MIN_HZ: f32 = 30.0;
/// 分析上限；真正取值时还会被奈奎斯特频率压低。
const MAX_HZ: f32 = 16_000.0;
/// dB 显示的底：低于它一律算 0。
const FLOOR_DB: f32 = -60.0;
/// 上升/下降的时间常数（秒）。上升快、下降慢，观感才像"跟着音乐跳"。
/// 柱高上升的时间常数。太小会让柱子瞬间冲到顶，30fps 下看就是"闪"。
const ATTACK_SECS: f32 = 0.18;
/// 柱高下降的时间常数。比上升慢，视觉上才是"起伏"而不是"跳"。
const RELEASE_SECS: f32 = 0.42;
/// `dt` 上限：卡顿或最小化后恢复时，别让一次巨大的 dt 直接把系数算成 1。
const MAX_DT: f32 = 0.25;

// ---------------------------------------------------------------------------
// 跨线程交接
// ---------------------------------------------------------------------------

/// 音频线程 → UI 线程的样本交接点。
///
/// 刻意做成"最新一块覆盖旧块"的语义：频谱要的是当下的声音，积压的历史没有意义。
pub struct SpectrumSink {
    samples: Mutex<Vec<f32>>,
    rate: AtomicU32,
}

impl SpectrumSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            samples: Mutex::new(Vec::with_capacity(FFT_SIZE)),
            rate: AtomicU32::new(0),
        })
    }

    /// 音频线程调用：发布一整块单声道样本。
    ///
    /// 只在样本攒满时调用；拿不到锁就丢弃，绝不等待。
    pub fn publish(&self, mono: &[f32], sample_rate: u32) {
        if mono.is_empty() {
            return;
        }
        self.rate.store(sample_rate, Ordering::Relaxed);
        let Ok(mut slot) = self.samples.try_lock() else {
            return;
        };
        // 先 `clear` 再 `extend`：容量在第一次之后稳定，不会反复扩容。
        slot.clear();
        slot.extend_from_slice(mono);
    }

    /// UI 线程调用：取走最新一块样本。没有新样本（暂停、未播放）时返回 `None`。
    pub fn take(&self) -> Option<SpectrumFrame> {
        let Ok(mut slot) = self.samples.lock() else {
            return None;
        };
        if slot.is_empty() {
            return None;
        }
        let samples = std::mem::take(&mut *slot);
        // 立刻把容量还回去，避免下次发布时在音频线程里扩容。
        slot.reserve(FFT_SIZE);
        Some(SpectrumFrame {
            samples,
            sample_rate: self.rate.load(Ordering::Relaxed),
        })
    }
}

impl Default for SpectrumSink {
    fn default() -> Self {
        Self {
            samples: Mutex::new(Vec::with_capacity(FFT_SIZE)),
            rate: AtomicU32::new(0),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpectrumFrame {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

// ---------------------------------------------------------------------------
// FFT
// ---------------------------------------------------------------------------

/// 定长 radix-2 复数 FFT。
///
/// 只有一个 1024 点的固定变换，手写比引入 `rustfft` 更划算：没有新依赖、
/// 没有许可清单变动，而且每个中间步骤都能直接测。
pub struct Fft {
    n: usize,
    /// 位反转置换表。
    rev: Vec<usize>,
    /// `cos(2πk/n)` / `sin(2πk/n)`，`k < n/2`。
    cos: Vec<f32>,
    sin: Vec<f32>,
}

impl Fft {
    /// `n` 必须是 2 的幂。
    pub fn new(n: usize) -> Self {
        assert!(n.is_power_of_two(), "FFT 长度必须是 2 的幂");
        let bits = n.trailing_zeros();
        let mut rev = vec![0usize; n];
        for (i, slot) in rev.iter_mut().enumerate() {
            *slot = i.reverse_bits() >> (usize::BITS - bits);
        }
        let half = n / 2;
        let mut cos = Vec::with_capacity(half);
        let mut sin = Vec::with_capacity(half);
        for k in 0..half {
            let angle = std::f32::consts::TAU * k as f32 / n as f32;
            cos.push(angle.cos());
            sin.push(angle.sin());
        }
        Self { n, rev, cos, sin }
    }

    /// 原地变换 `re`/`im`，长度必须等于 `n`。
    pub fn transform(&self, re: &mut [f32], im: &mut [f32]) {
        let n = self.n;
        debug_assert_eq!(re.len(), n);
        debug_assert_eq!(im.len(), n);

        for i in 0..n {
            let j = self.rev[i];
            if i < j {
                re.swap(i, j);
                im.swap(i, j);
            }
        }

        let mut len = 2;
        while len <= n {
            let half = len / 2;
            let step = n / len;
            let mut start = 0;
            while start < n {
                for k in 0..half {
                    let twiddle = k * step;
                    let wr = self.cos[twiddle];
                    let wi = -self.sin[twiddle];
                    let a = start + k;
                    let b = a + half;
                    let tr = re[b] * wr - im[b] * wi;
                    let ti = re[b] * wi + im[b] * wr;
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
                start += len;
            }
            len <<= 1;
        }
    }
}

// ---------------------------------------------------------------------------
// 分析器
// ---------------------------------------------------------------------------

pub struct SpectrumAnalyzer {
    fft: Fft,
    window: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    /// 每个频带的 bin 区间 `[lo, hi)`。
    edges: Vec<(usize, usize)>,
    edges_rate: u32,
    levels: [f32; BANDS],
}

impl SpectrumAnalyzer {
    pub fn new() -> Self {
        let mut window = vec![0.0_f32; FFT_SIZE];
        for (i, slot) in window.iter_mut().enumerate() {
            // Hann 窗。
            let phase = std::f32::consts::TAU * i as f32 / FFT_SIZE as f32;
            *slot = 0.5 - 0.5 * phase.cos();
        }
        Self {
            fft: Fft::new(FFT_SIZE),
            window,
            re: vec![0.0; FFT_SIZE],
            im: vec![0.0; FFT_SIZE],
            edges: Vec::new(),
            edges_rate: 0,
            levels: [0.0; BANDS],
        }
    }

    /// 当前柱高，`0.0..=1.0`。
    pub fn levels(&self) -> &[f32; BANDS] {
        &self.levels
    }

    /// 是否还有可见的柱子（用于决定要不要继续重绘）。
    pub fn is_active(&self) -> bool {
        self.levels.iter().any(|level| *level > 0.004)
    }

    /// 喂一帧样本并推进平滑。
    ///
    /// `frame` 为 `None` 表示这段时间没有新样本（暂停 / 未播放），柱高按释放
    /// 时间常数衰减到 0，而不是瞬间清空。
    pub fn update(&mut self, frame: Option<&SpectrumFrame>, dt: f32) {
        let dt = dt.clamp(0.0, MAX_DT);
        let target = match frame {
            Some(frame) => self.analyze(frame),
            None => [0.0; BANDS],
        };
        for (level, goal) in self.levels.iter_mut().zip(target.iter()) {
            let tau = if *goal > *level {
                ATTACK_SECS
            } else {
                RELEASE_SECS
            };
            // 与帧率无关的指数趋近。
            let k = 1.0 - (-dt / tau).exp();
            *level += (*goal - *level) * k;
            if *level < 1e-4 {
                *level = 0.0;
            }
        }
    }

    /// 直接把柱高清零（停止播放、换曲时用）。
    pub fn reset(&mut self) {
        self.levels = [0.0; BANDS];
    }

    fn analyze(&mut self, frame: &SpectrumFrame) -> [f32; BANDS] {
        let mut out = [0.0_f32; BANDS];
        let rate = frame.sample_rate;
        if rate == 0 || frame.samples.len() < FFT_SIZE {
            return out;
        }
        self.rebuild_edges(rate);

        // 取最新的 FFT_SIZE 个样本，加窗。
        let start = frame.samples.len() - FFT_SIZE;
        for i in 0..FFT_SIZE {
            self.re[i] = frame.samples[start + i] * self.window[i];
            self.im[i] = 0.0;
        }
        self.fft.transform(&mut self.re, &mut self.im);

        // 单边幅度归一化：窗函数相干增益 0.5，单边再乘 2。
        let scale = 2.0 / (FFT_SIZE as f32 * 0.5);

        for (band, (lo, hi)) in self.edges.iter().enumerate() {
            let mut peak = 0.0_f32;
            for bin in *lo..*hi {
                let magnitude = self.re[bin].hypot(self.im[bin]) * scale;
                if magnitude > peak {
                    peak = magnitude;
                }
            }
            let db = 20.0 * (peak + 1e-9).log10();
            out[band] = ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0).powf(0.85);
        }
        out
    }

    /// 按采样率重算频带 bin 区间；采样率变化时才重建。
    fn rebuild_edges(&mut self, rate: u32) {
        if self.edges_rate == rate && !self.edges.is_empty() {
            return;
        }
        self.edges_rate = rate;
        self.edges.clear();
        let nyquist = rate as f32 / 2.0;
        let high = MAX_HZ.min(nyquist * 0.95).max(MIN_HZ * 2.0);
        let bins = FFT_SIZE / 2;
        let bin_hz = rate as f32 / FFT_SIZE as f32;
        // 相邻频带必须首尾相接：低频段只有几个 bin，向下取整会造成重叠。
        let mut previous = 0;
        for band in 0..BANDS {
            let f0 = MIN_HZ * (high / MIN_HZ).powf(band as f32 / BANDS as f32);
            let f1 = MIN_HZ * (high / MIN_HZ).powf((band + 1) as f32 / BANDS as f32);
            let lo = ((f0 / bin_hz).ceil() as usize).max(previous).min(bins - 1);
            // 每个带至少占用 1 个 bin，否则低频段会整片是 0。
            let hi = (((f1 / bin_hz).ceil() as usize).max(lo + 1)).min(bins);
            self.edges.push((lo, hi));
            previous = hi;
        }
    }
}

impl Default for SpectrumAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 44_100;

    fn sine(freq: f32, amplitude: f32, len: usize, rate: u32) -> Vec<f32> {
        (0..len)
            .map(|i| {
                let t = i as f32 / rate as f32;
                (std::f32::consts::TAU * freq * t).sin() * amplitude
            })
            .collect()
    }

    fn frame(samples: Vec<f32>, rate: u32) -> SpectrumFrame {
        SpectrumFrame {
            samples,
            sample_rate: rate,
        }
    }

    #[test]
    fn band_edges_cover_the_spectrum_monotonically() {
        let mut analyzer = SpectrumAnalyzer::new();
        analyzer.rebuild_edges(RATE);
        assert_eq!(analyzer.edges.len(), BANDS);
        // 44.1kHz 下 bin 宽度约 43Hz，30Hz 的起点落在 1 号 bin。
        assert!(analyzer.edges[0].0 <= 1, "最低频带应贴着 0 号 bin 开始");
        let mut previous = analyzer.edges[0].1;
        for (lo, hi) in analyzer.edges.iter().skip(1) {
            assert!(lo < hi, "频带必须至少包含一个 bin");
            assert_eq!(*lo, previous, "频带必须首尾相接，既不重叠也不留缝");
            assert!(*hi <= FFT_SIZE / 2, "不得越过奈奎斯特");
            previous = *hi;
        }
    }

    #[test]
    fn fft_puts_a_sine_into_its_bin() {
        let fft = Fft::new(256);
        let rate = 256.0_f32;
        // 频率正好是第 16 个 bin。
        let mut re: Vec<f32> = (0..256)
            .map(|i| (std::f32::consts::TAU * 16.0 * i as f32 / rate).sin())
            .collect();
        let mut im = vec![0.0_f32; 256];
        fft.transform(&mut re, &mut im);
        let magnitudes: Vec<f32> = (0..128).map(|i| re[i].hypot(im[i])).collect();
        let peak = magnitudes
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).expect("finite"))
            .map(|(index, _)| index)
            .expect("非空");
        assert_eq!(peak, 16, "正弦应落在对应 bin 上");
    }

    #[test]
    fn silence_stays_silent() {
        let mut analyzer = SpectrumAnalyzer::new();
        analyzer.update(Some(&frame(vec![0.0; FFT_SIZE], RATE)), 0.033);
        assert!(analyzer.levels().iter().all(|level| *level == 0.0));
        assert!(!analyzer.is_active());
    }

    #[test]
    fn sine_440_lights_the_band_that_contains_440hz() {
        let mut analyzer = SpectrumAnalyzer::new();
        analyzer.rebuild_edges(RATE);
        // 连续喂几帧让 attack 收敛。
        for _ in 0..30 {
            analyzer.update(Some(&frame(sine(440.0, 0.6, FFT_SIZE, RATE), RATE)), 0.033);
        }
        let levels = analyzer.levels();
        let peak = levels
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).expect("finite"))
            .map(|(index, _)| index)
            .expect("非空");
        let (lo, hi) = analyzer.edges[peak];
        let bin_hz = RATE as f32 / FFT_SIZE as f32;
        assert!(
            (lo as f32 * bin_hz) <= 440.0 && 440.0 < (hi as f32 * bin_hz),
            "峰值应在含 440Hz 的频带（实际第 {peak} 带，{}–{}Hz）",
            lo as f32 * bin_hz,
            hi as f32 * bin_hz
        );
    }

    #[test]
    fn louder_input_makes_taller_bars() {
        let mut quiet = SpectrumAnalyzer::new();
        let mut loud = SpectrumAnalyzer::new();
        for _ in 0..30 {
            quiet.update(
                Some(&frame(sine(1_000.0, 0.05, FFT_SIZE, RATE), RATE)),
                0.033,
            );
            loud.update(
                Some(&frame(sine(1_000.0, 0.8, FFT_SIZE, RATE), RATE)),
                0.033,
            );
        }
        let quiet_peak = quiet.levels().iter().cloned().fold(0.0_f32, f32::max);
        let loud_peak = loud.levels().iter().cloned().fold(0.0_f32, f32::max);
        assert!(
            loud_peak > quiet_peak + 0.2,
            "响的应明显更高：{loud_peak} vs {quiet_peak}"
        );
    }

    #[test]
    fn levels_decay_without_new_samples() {
        let mut analyzer = SpectrumAnalyzer::new();
        for _ in 0..30 {
            analyzer.update(
                Some(&frame(sine(1_000.0, 0.8, FFT_SIZE, RATE), RATE)),
                0.033,
            );
        }
        let before = analyzer.levels().iter().cloned().fold(0.0_f32, f32::max);
        assert!(before > 0.3, "先要有能量");

        // 1 秒没有样本：明显下降但还没归零——是渐隐，不是「啪」地清空。
        for _ in 0..30 {
            analyzer.update(None, 0.033);
        }
        let midway = analyzer.levels().iter().cloned().fold(0.0_f32, f32::max);
        assert!(
            midway < before * 0.5,
            "1 秒应至少减半：{before} -> {midway}"
        );
        assert!(midway > 0.0, "但不能瞬间清零");

        // 再过 3 秒才基本归零。
        for _ in 0..90 {
            analyzer.update(None, 0.033);
        }
        let after = analyzer.levels().iter().cloned().fold(0.0_f32, f32::max);
        assert!(after < 0.02, "4 秒后应几乎归零，实际 {after}");
    }

    #[test]
    fn smoothing_is_slow_enough_to_avoid_flicker() {
        // 视觉要求（回归护栏）：柱子既不能瞬间冲到顶，也不能掉得飞快，
        // 否则整块频谱看起来是在闪而不是在起伏。
        // 用编译期断言：有人把参数改回快值会直接编译不过。
        const { assert!(ATTACK_SECS >= 0.15, "上升太快会闪") };
        const { assert!(RELEASE_SECS >= 0.3, "下降太快会抖") };
        const { assert!(RELEASE_SECS > ATTACK_SECS, "下降应比上升更缓") };
    }

    #[test]
    fn short_or_unrated_frames_are_ignored() {
        let mut analyzer = SpectrumAnalyzer::new();
        analyzer.update(Some(&frame(vec![1.0; FFT_SIZE / 2], RATE)), 0.033);
        assert!(!analyzer.is_active());
        analyzer.update(Some(&frame(vec![1.0; FFT_SIZE], 0)), 0.033);
        assert!(!analyzer.is_active());
    }

    #[test]
    fn sink_hands_over_the_latest_block() {
        let sink = SpectrumSink::new();
        sink.publish(&[0.1; FFT_SIZE], RATE);
        let first = sink.take().expect("应有样本");
        assert_eq!(first.samples.len(), FFT_SIZE);
        assert_eq!(first.sample_rate, RATE);
        assert!(sink.take().is_none(), "取过一次后应为空");

        sink.publish(&[0.2; FFT_SIZE], 48_000);
        assert_eq!(sink.take().expect("应有样本").sample_rate, 48_000);
    }

    #[test]
    fn sink_ignores_empty_publishes() {
        let sink = SpectrumSink::new();
        sink.publish(&[], RATE);
        assert!(sink.take().is_none());
    }
}
