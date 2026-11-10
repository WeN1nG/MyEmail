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
///
/// **作用域（`scope`）**是曲目搜索落地的地方：它决定「哪些」曲目参与播放，
/// `order` 决定「什么顺序」，两者正交，`rebuild_play_order` 是唯一同时看两者的地方。
/// `scope = None` 表示全库；`Some(vec![])` 表示命中为空（什么都不能播）——二者不可混用。
///
/// **`play_order` 可能比 `scope` 多一项**：队首那首「被钉住」（pinned）的曲目。
/// 来源有两个：①作用域变化时正在播放、但不匹配的那首；②`pin()` 显式钉入的。
/// 缺了它，`current_index()` 会在搜索瞬间变成 `None`，上层会以为「没在播」。
#[derive(Debug, Clone, Default)]
pub struct MusicQueue {
    tracks: Vec<PathBuf>,
    order: MusicOrder,
    scope: Option<Vec<usize>>,
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

    /// 当前作用域（曲库索引，升序去重）；`None` = 全库。
    pub fn scope(&self) -> Option<&[usize]> {
        self.scope.as_deref()
    }

    pub fn path_at(&self, index: usize) -> Option<&Path> {
        self.tracks.get(index).map(PathBuf::as_path)
    }

    /// 曲目是否在当前作用域内（`scope = None` 时恒为真）。
    pub fn in_scope(&self, index: usize) -> bool {
        match &self.scope {
            Some(list) => list.binary_search(&index).is_ok(),
            None => true,
        }
    }

    /// 序列首项；「没有当前曲目时按播放」用它，而不是硬编码 0——
    /// 作用域可能不含 0。
    pub fn first_index(&self) -> Option<usize> {
        self.play_order.first().copied()
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
        // 新曲库的索引与旧作用域没有对应关系，必须一起丢掉。
        self.scope = None;
        self.cursor = 0;
        self.rebuild_play_order(None, rng);
    }

    pub fn clear(&mut self) {
        self.tracks.clear();
        self.scope = None;
        self.play_order.clear();
        self.cursor = 0;
    }

    /// 设置播放作用域（曲目搜索的落点）。
    ///
    /// `playing` 是**真正在播（已加载）**的曲目，没播过传 `None`。
    /// **不能**用 `current_index()` 顶替：非空队列的游标永远有值，那是「游标」不是
    /// 「正在播放」；拿它当在播曲目会让「刚扫描完就搜索」把第 0 首钉进结果集。
    ///
    /// **不打断正在播放的曲目**：它仍在作用域内 → 游标跟着它；不在 → 钉在序列首位
    /// （游标指向它），声音不变，下一首从命中集合开头续。详见 [`Self::rebuild_play_order`]。
    pub fn set_scope(
        &mut self,
        scope: Option<Vec<usize>>,
        playing: Option<usize>,
        rng: &mut impl Rng,
    ) {
        self.scope = scope.map(|list| {
            let mut list: Vec<usize> = list
                .into_iter()
                .filter(|index| *index < self.tracks.len())
                .collect();
            list.sort_unstable(); // 排序 + 去重，`in_scope` 才能用二分
            list.dedup();
            list
        });
        self.rebuild_play_order(playing, rng);
    }

    /// 把一首**不在作用域内**的曲目临时插到队首并选中（「点名播放」）。
    ///
    /// 作用域不变：它播完 / 按下一首 → 回到命中集合首项。用于 AI 指定播放、
    /// 以及重扫竞态下的定点播放——静默失败会比「多播一首」难查得多。
    ///
    /// 队首若已经是「上一首 pinned」（同样在作用域外），就**替换**它而不是再插一条，
    /// 否则连续点名会攒成 `[C, B, A, ...结果集]`，「上一首」会一路往回翻历史。
    pub fn pin(&mut self, index: usize) -> bool {
        if index >= self.tracks.len() {
            return false;
        }
        if let Some(pos) = self.play_order.iter().position(|&i| i == index) {
            self.cursor = pos;
            return true;
        }
        match self.play_order.first() {
            Some(&head) if !self.in_scope(head) => self.play_order[0] = index,
            _ => self.play_order.insert(0, index),
        }
        self.cursor = 0;
        true
    }

    /// 选中某个曲目索引；不在当前播放序列里时返回 `false` 且不改动游标。
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

    /// 下一首。**首尾相通**：走到序列末尾就回到第一首（列表循环）。
    ///
    /// 空序列返回 `None`（搜索无命中时才可能为空）。
    /// 顺序播放与随机播放共用这一条——两者的 `play_order` 不同，环绕规则一致。
    pub fn next_index(&self) -> Option<usize> {
        if self.play_order.is_empty() {
            return None;
        }
        if self.cursor + 1 < self.play_order.len() {
            self.play_order.get(self.cursor + 1).copied()
        } else {
            self.play_order.first().copied()
        }
    }

    /// 上一首。**首尾相通**：在序列首项按上一首回到最后一首。
    pub fn prev_index(&self) -> Option<usize> {
        if self.play_order.is_empty() {
            return None;
        }
        if self.cursor > 0 {
            self.play_order.get(self.cursor - 1).copied()
        } else {
            self.play_order.last().copied()
        }
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

    /// 重建播放序列（`scope` × `order` 的唯一汇合点）。
    ///
    /// 两个要点：
    /// - 顺序模式下 `cursor` 必须按 `position()` 找，**不能**再假设它等于曲库索引
    ///   （老写法 `current.min(len-1)` 只在 `play_order == 0..len` 时成立）。
    /// - 正在播放但不在作用域内的曲目钉在队首。否则 `current_index()` 会变成
    ///   `None`，上层（专辑页、进度、封面）全部以为「没在播」，而声音还在响。
    fn rebuild_play_order(&mut self, current: Option<usize>, rng: &mut impl Rng) {
        self.play_order = match &self.scope {
            Some(list) => list.clone(),
            None => (0..self.tracks.len()).collect(),
        };

        if let Some(index) = current
            && index < self.tracks.len()
            && !self.play_order.contains(&index)
        {
            self.play_order.insert(0, index);
        }

        match self.order {
            MusicOrder::Sequential => {
                self.cursor = current
                    .and_then(|i| self.play_order.iter().position(|&x| x == i))
                    .unwrap_or(0);
            }
            MusicOrder::Shuffle => {
                self.play_order.shuffle(rng);
                if let Some(pos) =
                    current.and_then(|i| self.play_order.iter().position(|&x| x == i))
                {
                    // 当前曲目置首，`next` 从它之后继续。
                    self.play_order.swap(0, pos);
                }
                self.cursor = 0;
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
    fn sequential_walks_forward_and_wraps_at_the_end() {
        let mut q = queue(3);
        assert_eq!(q.current_index(), Some(0));
        assert_eq!(q.next_index(), Some(1));

        assert!(q.select(1));
        assert_eq!(q.prev_index(), Some(0));
        assert_eq!(q.next_index(), Some(2));

        assert!(q.select(2));
        assert_eq!(q.next_index(), Some(0), "末尾回到开头（首尾相通）");
        assert_eq!(q.prev_index(), Some(1));
    }

    #[test]
    fn prev_at_start_wraps_to_the_last_track() {
        let q = queue(3);
        assert_eq!(q.prev_index(), Some(2));
    }

    #[test]
    fn empty_and_single_track_are_safe() {
        let q = MusicQueue::new();
        assert_eq!(q.next_index(), None);
        assert_eq!(q.prev_index(), None);
        assert_eq!(q.current_index(), None);

        // 单曲队列：上一首/下一首都是它自己（列表循环的必然结果），不会 panic。
        let q = queue(1);
        assert_eq!(q.current_index(), Some(0));
        assert_eq!(q.next_index(), Some(0));
        assert_eq!(q.prev_index(), Some(0));
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
        assert_eq!(q.next_index(), Some(0), "4 是最后一首，下一首回到开头");
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

    // -----------------------------------------------------------------------
    // 作用域（曲目搜索）
    // -----------------------------------------------------------------------

    #[test]
    fn scope_limits_next_and_prev() {
        let mut q = queue(10);
        q.set_scope(Some(vec![2, 5, 9]), None, &mut rng());

        assert_eq!(q.current_index(), Some(2), "首个命中项成为当前曲目");
        assert_eq!(q.next_index(), Some(5), "跳过集合外曲目");
        assert_eq!(q.prev_index(), Some(9), "集合首位再上一首回到末尾");

        assert!(q.select(5));
        assert_eq!(q.prev_index(), Some(2));
        assert_eq!(q.next_index(), Some(9));

        assert!(q.select(9));
        assert_eq!(
            q.next_index(),
            Some(2),
            "集合末尾回到集合首项（只在集合内环绕）"
        );
    }

    #[test]
    fn select_outside_scope_is_rejected() {
        let mut q = queue(10);
        q.set_scope(Some(vec![2, 5, 9]), None, &mut rng());
        assert!(!q.select(4), "集合外索引不能播");
        assert_eq!(q.current_index(), Some(2), "失败时游标不变");
    }

    #[test]
    fn clearing_scope_restores_the_full_library() {
        let mut q = queue(10);
        assert!(q.select(6));
        q.set_scope(Some(vec![2, 5]), Some(6), &mut rng());
        assert_eq!(q.current_index(), Some(6), "在播曲目被钉在队首");

        q.set_scope(None, Some(6), &mut rng());
        assert_eq!(q.scope(), None);
        assert_eq!(q.current_index(), Some(6), "恢复全库仍停在当前曲目");
        assert_eq!(q.prev_index(), Some(5));
        assert!(q.select(4), "全库下集合外也能播");
    }

    #[test]
    fn replace_tracks_clears_scope() {
        let mut q = queue(5);
        q.set_scope(Some(vec![1, 3]), None, &mut rng());
        q.replace_tracks(paths(4), &mut rng());

        assert_eq!(q.scope(), None, "新曲库与旧作用域无对应关系");
        assert_eq!(q.len(), 4);
    }

    /// 回归：顺序模式下 `cursor` 是 `play_order` 位置，不是曲库索引。
    ///
    /// 老写法 `cursor = current.min(len-1)` 只对 `play_order == 0..len` 成立，
    /// 加了作用域就会「点 A 播 B」。
    #[test]
    fn sequential_cursor_uses_position_not_index() {
        let mut q = queue(10);
        assert!(q.select(5));
        q.set_scope(Some(vec![2, 5, 9]), Some(5), &mut rng());

        assert_eq!(q.current_index(), Some(5));
        assert_eq!(q.cursor, 1, "5 在序列里排第 2");
        assert_eq!(q.next_index(), Some(9));
        assert_eq!(q.prev_index(), Some(2));
    }

    #[test]
    fn playing_track_outside_scope_stays_current() {
        let mut q = queue(10);
        assert!(q.select(7));
        q.set_scope(Some(vec![2, 5]), Some(7), &mut rng());

        assert_eq!(q.current_index(), Some(7), "在播曲目不能被搜索挤掉");
        assert_eq!(q.next_index(), Some(2), "下一首从命中集合开头续");
        assert_eq!(q.prev_index(), Some(5), "序列首位再上一首回到末尾");
    }

    #[test]
    /// 空集合（搜索无命中）：序列里只剩「正在播的那首」，环绕到它自己。
    ///
    /// 界面上这时 next/prev 是禁用的（`transport_enabled` 看 `view_len`），
    /// 所以「重播当前曲」这条路径只有内部/竞态才会走到，不算用户可见行为。
    fn empty_scope_keeps_current_and_wraps_onto_itself() {
        let mut q = queue(10);
        assert!(q.select(3));
        q.set_scope(Some(Vec::new()), Some(3), &mut rng());

        assert_eq!(q.scope(), Some(&[][..]), "空集合 ≠ 全库");
        assert_eq!(q.current_index(), Some(3));
        assert_eq!(q.play_order, vec![3], "序列只剩被钉住的那首");
        assert_eq!(q.next_index(), Some(3));
        assert_eq!(q.prev_index(), Some(3));
    }

    /// 真正的空队列：没有任何可播项，导航必须返回 `None` 而不是 panic。
    #[test]
    fn empty_queue_has_no_neighbours() {
        let q = MusicQueue::new();
        assert_eq!(q.next_index(), None);
        assert_eq!(q.prev_index(), None);
    }

    #[test]
    fn scope_is_normalized_and_bounds_checked() {
        let mut q = queue(5);
        q.set_scope(Some(vec![3, 1, 99, 1, 3]), None, &mut rng());
        assert_eq!(q.scope(), Some(&[1, 3][..]), "排序、去重、越界丢弃");
        assert_eq!(q.current_index(), Some(1));
    }

    #[test]
    fn shuffle_over_scope_is_a_permutation_of_the_scope() {
        let mut q = queue(10);
        q.set_scope(Some(vec![1, 3, 5, 7]), None, &mut rng());
        q.set_order(MusicOrder::Shuffle, &mut rng());

        let mut seen = q.play_order.clone();
        seen.sort_unstable();
        assert_eq!(seen, vec![1, 3, 5, 7], "随机只在作用域内打乱");
        assert!(q.play_order.contains(&q.current_index().unwrap()));
    }

    #[test]
    fn pin_plays_outside_scope_and_replaces_the_previous_pin() {
        let mut q = queue(5);
        q.set_scope(Some(vec![1, 2]), None, &mut rng());

        assert!(q.pin(4), "集合外索引可以点名播");
        assert_eq!(q.current_index(), Some(4));
        assert_eq!(q.next_index(), Some(1), "播完回到命中集合开头");
        assert_eq!(q.play_order.len(), 3, "[4] + 命中集合");

        assert!(q.pin(0));
        assert_eq!(q.current_index(), Some(0));
        assert_eq!(
            q.play_order.len(),
            3,
            "连续点名只替换队首那一项，不累积历史"
        );
        assert_eq!(q.next_index(), Some(1));
    }

    #[test]
    fn pinned_track_survives_order_and_scope_changes() {
        let mut q = queue(6);
        q.set_scope(Some(vec![1, 2]), None, &mut rng());
        assert!(q.pin(5));

        q.set_order(MusicOrder::Shuffle, &mut rng());
        assert_eq!(q.current_index(), Some(5), "切随机不丢点名播的那首");

        q.set_scope(Some(vec![1, 2, 3]), Some(5), &mut rng());
        assert_eq!(q.current_index(), Some(5), "重新搜索仍保留它");
        assert_eq!(q.play_order[0], 5, "仍在队首");
        assert_eq!(q.play_order.len(), 4);
    }

    #[test]
    fn pin_rejects_unknown_index() {
        let mut q = queue(2);
        assert!(!q.pin(9));
        assert_eq!(q.current_index(), Some(0));
    }
}
