//! 播放队列与顺序：纯计算，不读文件、不依赖音频库、不碰 egui。
//!
//! 随机顺序由调用方注入 RNG，测试可用固定种子复现；扫描与解码在 `me-app`。

use std::path::{Path, PathBuf};

use rand::Rng;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};

/// 播放顺序。持久化到 `meta.music_order`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MusicOrder {
    #[default]
    Sequential,
    Shuffle,
}

impl MusicOrder {
    pub fn as_str(self) -> &'static str {
        match self {
            MusicOrder::Sequential => "sequential",
            MusicOrder::Shuffle => "shuffle",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "sequential" => Some(MusicOrder::Sequential),
            "shuffle" => Some(MusicOrder::Shuffle),
            _ => None,
        }
    }
}

/// 播放队列。
///
/// `play_order` 是「播放序列」：顺序模式下就是 `0..len`，随机模式下是一份排列。
/// `cursor` 是**在 `play_order` 中的位置**，不是曲目索引——两者在同一时刻通常不同，
/// 混用会导致「点 A 播 B」。对外只暴露 `current_index()` 这类语义化方法。
#[derive(Debug, Clone, Default)]
pub struct MusicQueue {
    tracks: Vec<PathBuf>,
    order: MusicOrder,
    play_order: Vec<usize>,
    cursor: usize,
}

impl MusicQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn order(&self) -> MusicOrder {
        self.order
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn tracks(&self) -> &[PathBuf] {
        &self.tracks
    }

    pub fn path_at(&self, index: usize) -> Option<&Path> {
        self.tracks.get(index).map(PathBuf::as_path)
    }

    /// 当前选中曲目的原始索引。
    pub fn current_index(&self) -> Option<usize> {
        self.play_order.get(self.cursor).copied()
    }

    pub fn current_path(&self) -> Option<&Path> {
        self.current_index().and_then(|i| self.path_at(i))
    }

    /// 重建队列（扫描完成 / 切换目录）。游标回到开头，顺序保持不变。
    pub fn replace_tracks(&mut self, tracks: Vec<PathBuf>, rng: &mut impl Rng) {
        self.tracks = tracks;
        self.cursor = 0;
        self.rebuild_play_order(None, rng);
    }

    pub fn clear(&mut self) {
        self.tracks.clear();
        self.play_order.clear();
        self.cursor = 0;
    }

    /// 选中某个曲目索引；不存在时返回 `false` 且不改动游标。
    pub fn select(&mut self, index: usize) -> bool {
        if index >= self.tracks.len() {
            return false;
        }
        if let Some(pos) = self.play_order.iter().position(|&i| i == index) {
            self.cursor = pos;
            true
        } else {
            false
        }
    }

    /// 顺序播放的下一个曲目索引；到末尾返回 `None`（不循环）。
    pub fn next_index(&self) -> Option<usize> {
        self.play_order.get(self.cursor + 1).copied()
    }

    /// 上一个曲目索引；已在开头返回 `None`。
    pub fn prev_index(&self) -> Option<usize> {
        if self.cursor == 0 {
            return None;
        }
        self.play_order.get(self.cursor - 1).copied()
    }

    /// 切换顺序：保留当前曲目与位置语义。
    ///
    /// 进入随机时把当前曲目放到序列首位，`next` 从它之后继续；
    /// 回到顺序时恢复原始路径次序，游标指向当前曲目。
    pub fn set_order(&mut self, order: MusicOrder, rng: &mut impl Rng) {
        let current = self.current_index();
        self.order = order;
        self.rebuild_play_order(current, rng);
    }

    fn rebuild_play_order(&mut self, current: Option<usize>, rng: &mut impl Rng) {
        let len = self.tracks.len();
        self.play_order = (0..len).collect();
        match self.order {
            MusicOrder::Sequential => {
                self.cursor = current.unwrap_or(0).min(len.saturating_sub(1));
            }
            MusicOrder::Shuffle => {
                self.play_order.shuffle(rng);
                if let Some(current) = current {
                    if let Some(pos) = self.play_order.iter().position(|&i| i == current) {
                        self.play_order.swap(0, pos);
                    }
                    self.cursor = 0;
                } else {
                    self.cursor = 0;
                }
            }
        }
    }
}

/// 播放控制动作。与 [`MusicOp::Control`] 一起跨层传递，不含任何 UI 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MusicAction {
    Play,
    Pause,
    Next,
    Prev,
    Stop,
}

impl MusicAction {
    pub fn as_str(self) -> &'static str {
        match self {
            MusicAction::Play => "play",
            MusicAction::Pause => "pause",
            MusicAction::Next => "next",
            MusicAction::Prev => "prev",
            MusicAction::Stop => "stop",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "play" => Some(MusicAction::Play),
            "pause" => Some(MusicAction::Pause),
            "next" => Some(MusicAction::Next),
            "prev" => Some(MusicAction::Prev),
            "stop" => Some(MusicAction::Stop),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MusicAction::Play => "播放",
            MusicAction::Pause => "暂停",
            MusicAction::Next => "下一首",
            MusicAction::Prev => "上一首",
            MusicAction::Stop => "停止",
        }
    }
}

/// AI 音乐工具要执行的操作。
///
/// **执行方是 `me-app`**，不是 `me-sync`：播放器（`AudioPlayer`）住在 UI 线程侧，
/// 走的是本地 `AudioCommand` 通道而不是 Command/Event。所以这里是「请求」，
/// 真正做事的在 UI，结果通过 [`MusicOutcome`] 回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum MusicOp {
    /// 在曲库里按标题搜索（大小写不敏感）。
    Search { query: String },
    /// 播放：按查询词找第一首，或按索引直接播。
    Play {
        query: Option<String>,
        index: Option<usize>,
    },
    /// 当前播放状态（不产生副作用）。
    State,
    /// 播放控制。
    Control { action: MusicAction },
}

impl MusicOp {
    /// 给日志/活动条用的一句话，不含参数细节。
    pub fn label(&self) -> &'static str {
        match self {
            MusicOp::Search { .. } => "搜索曲库",
            MusicOp::Play { .. } => "播放",
            MusicOp::State => "查看播放状态",
            MusicOp::Control { action } => action.label(),
        }
    }
}

/// 音乐操作的执行结果。
///
/// `summary` 是**给模型看**的一句话（也是给用户的活动条摘要），
/// `ok` 决定工具结果是不是 `is_error`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MusicOutcome {
    pub ok: bool,
    pub summary: String,
}

impl MusicOutcome {
    pub fn ok(summary: impl Into<String>) -> Self {
        Self {
            ok: true,
            summary: summary.into(),
        }
    }

    pub fn fail(summary: impl Into<String>) -> Self {
        Self {
            ok: false,
            summary: summary.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    fn paths(n: usize) -> Vec<PathBuf> {
        (0..n)
            .map(|i| PathBuf::from(format!("/m/{i}.mp3")))
            .collect()
    }

    fn rng() -> StdRng {
        StdRng::seed_from_u64(42)
    }

    fn queue(n: usize) -> MusicQueue {
        let mut q = MusicQueue::new();
        q.replace_tracks(paths(n), &mut rng());
        q
    }

    #[test]
    fn sequential_walks_forward_and_stops_at_end() {
        let mut q = queue(3);
        assert_eq!(q.current_index(), Some(0));
        assert_eq!(q.next_index(), Some(1));

        assert!(q.select(1));
        assert_eq!(q.prev_index(), Some(0));
        assert_eq!(q.next_index(), Some(2));

        assert!(q.select(2));
        assert_eq!(q.next_index(), None, "末尾不循环");
    }

    #[test]
    fn prev_at_start_is_none() {
        let q = queue(3);
        assert_eq!(q.prev_index(), None);
    }

    #[test]
    fn empty_and_single_track_are_safe() {
        let q = MusicQueue::new();
        assert_eq!(q.next_index(), None);
        assert_eq!(q.prev_index(), None);
        assert_eq!(q.current_index(), None);

        let q = queue(1);
        assert_eq!(q.current_index(), Some(0));
        assert_eq!(q.next_index(), None);
        assert_eq!(q.prev_index(), None);
    }

    #[test]
    fn shuffle_is_a_permutation_and_keeps_current_first() {
        let mut q = queue(6);
        assert!(q.select(3));
        q.set_order(MusicOrder::Shuffle, &mut rng());

        assert_eq!(q.order(), MusicOrder::Shuffle);
        assert_eq!(q.current_index(), Some(3), "切换顺序必须保留当前曲目");

        let mut seen = q.play_order.clone();
        seen.sort_unstable();
        assert_eq!(seen, (0..6).collect::<Vec<_>>(), "必须是完整排列");
        assert_eq!(q.play_order[0], 3, "当前曲目置于首位");
    }

    #[test]
    fn shuffle_next_follows_play_order() {
        let mut q = queue(5);
        q.set_order(MusicOrder::Shuffle, &mut rng());
        let first = q.current_index().expect("有当前曲目");
        let next = q.next_index().expect("应还有下一首");
        assert_ne!(next, first);
        assert!(q.select(next), "下一首必须在序列中");
    }

    #[test]
    fn back_to_sequential_restores_path_order() {
        let mut q = queue(5);
        q.select(4);
        q.set_order(MusicOrder::Shuffle, &mut rng());
        q.set_order(MusicOrder::Sequential, &mut rng());

        assert_eq!(q.current_index(), Some(4), "切回顺序仍停在当前曲目");
        assert_eq!(q.next_index(), None, "4 是最后一首");
        assert_eq!(q.prev_index(), Some(3));
    }

    #[test]
    fn select_unknown_index_is_rejected() {
        let mut q = queue(2);
        assert!(!q.select(9));
        assert_eq!(q.current_index(), Some(0), "失败时游标不变");
    }

    #[test]
    fn replace_tracks_resets_cursor_and_keeps_order() {
        let mut q = queue(3);
        q.select(2);
        q.set_order(MusicOrder::Shuffle, &mut rng());
        q.replace_tracks(paths(4), &mut rng());

        assert_eq!(q.len(), 4);
        assert_eq!(q.order(), MusicOrder::Shuffle, "重扫不改变用户选的顺序");
        assert!(q.current_index().is_some());
        let mut seen = q.play_order.clone();
        seen.sort_unstable();
        assert_eq!(seen, (0..4).collect::<Vec<_>>());
    }

    /// 音乐操作要能过 Command 的 serde 边界（UI 回执走的是同一条通道）。
    #[test]
    fn music_op_survives_serde() {
        let op = MusicOp::Play {
            query: Some("晴天".to_string()),
            index: Some(3),
        };
        let text = serde_json::to_string(&op).expect("序列化");
        let back: MusicOp = serde_json::from_str(&text).expect("反序列化");
        assert_eq!(back, op);
        assert_eq!(MusicAction::parse("pause"), Some(MusicAction::Pause));
        assert_eq!(MusicAction::parse("PAUSE"), None, "不做模糊匹配");
    }

    #[test]
    fn order_round_trips_through_str() {
        for order in [MusicOrder::Sequential, MusicOrder::Shuffle] {
            assert_eq!(MusicOrder::parse(order.as_str()), Some(order));
        }
        assert_eq!(MusicOrder::parse("shuffle "), None, "不做模糊匹配");
        assert_eq!(MusicOrder::parse(""), None);
        assert_eq!(MusicOrder::default(), MusicOrder::Sequential);
    }
}
