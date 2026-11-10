# 音乐页左侧曲目搜索（搜索决定播放列表）

> 事实基线：v0.4.0 / schema **v7**（本需求**不改 schema**）/ 相关代码为 `crates/me-app` + `crates/me-core`
>
> **落地状态：已实现**（`cargo test --workspace` 888 passed / 6 ignored，`fmt` + `clippy -D warnings` 通过）。
> 落地时与本文有两处偏差，文内已就地标注：`set_scope` 多一个 `playing` 参数（§3.2）、
> 「不读封面」是多做一步显式跳过而不是只靠 `limit_visual_bytes`（§3.6）。
> 版本归属见 [doc/versiondoc/版本跟踪.md](../versiondoc/版本跟踪.md) §10（未定版增量）。

---

## 0. 摘要

在音乐页左侧栏的曲目列表头部加一个搜索框，搜索类型可选 **歌曲名 / 歌手**。搜索生效后，**播放队列被裁剪为命中集合**——点击、上一首 / 下一首、自动续播都只在这批歌里发生；清空搜索即恢复全库。

四条要求对应的四个机制：

| 要求 | 机制 | 落点 |
| --- | --- | --- |
| 左侧曲目列表头部有搜索框 | 侧栏在当前「目录行 → 状态行」之后、列表之前插入搜索区 | `views/music.rs::sidebar` |
| 可选搜歌曲名 / 搜歌手 | `MatchField::{Title, Artist}`（AI 侧用 `Any`），界面与 AI 共用一份匹配函数 | `views/music.rs::match_tracks` |
| 搜索决定播放列表 | 给 `MusicQueue` 加**作用域（scope）**，由 `AudioCommand::SetScope` 下发 | `me-core/src/music.rs`、`audio/mod.rs` |
| 用户要知道「播放范围被限定了」 | footer 的「200 首」在搜索态变成可点击的「5 / 200 首」 | `views/music.rs::footer` |

五条必须先讲清的判断，否则后面的方案会被误读：

1. **这不是纯 UI 改动。** 播放队列住在音频线程（`PlayerCore.queue`），UI 里的 `MusicState.tracks` 只是扫描结果的副本。只在 `views/music.rs` 过滤渲染，得到的是「列表只剩 3 首、但下一首仍然播第 4 首」的假搜索。
2. **「歌手」今天没有数据。** `playlist::Track` 只有 `path` / `title`（文件名去扩展名）/ `duration_ms`；标签里的歌手只在**切歌那一刻**由 `tags::read_now_playing` 读一次。要按歌手搜，必须先把 artist 补齐到 `Track` 上——这是本需求里唯一有 IO 成本的部分（§3.6）。
3. **「裁剪队列」不能是「替换队列」。** 把 `MusicQueue.tracks` 换成搜索结果，索引语义就整体漂移：`MusicState.current`、专辑页、封面纹理、AI 的 `index` 参数全部指错歌。正确做法是保留全库 `tracks`，另加一层**作用域**：scope 决定「哪些」，order 决定「什么顺序」。
4. **视图序号 ≠ 曲库索引。** 搜索态下列表第 1 行可能是曲库第 137 首。编号必须按视图位置算，`Play` 必须传曲库索引；两者只能在一处转换。
5. **「搜索决定播放列表」是一个隐形状态，必须让它可见。** 用户搜完、点第 2 首、听了 5 首之后音乐停了——如果界面上没有任何东西说明「播放范围只有这 5 首」，这就是 bug 报告。所以 footer 要有范围指示（R8），列表顶部要有「正在播放但不在筛选内」那一行（R9）。

---

## 1. 需求拆解与验收口径

| 编号 | 行为 | 验收判据 |
| --- | --- | --- |
| R1 | 侧栏曲目列表**上方**有搜索框，固定不随列表滚动 | 滚动到底部时搜索框仍在原位 |
| R2 | 可切换搜索类型「歌曲名 / 歌手」，切换后用同一关键字重新过滤 | 同关键字切类型，命中集合与列表行数随之变化 |
| R3 | 输入即过滤（实时），大小写不敏感、去首尾空白、空白串 = 不过滤 | 输入「蓝」即刻只剩含「蓝」的行；全空格等于清空 |
| R4 | 点击结果行 → 播放该曲，且**播放顺序只含命中集合** | 点第 1 行后连按下一首，不会跑到命中集合之外 |
| R5 | 命中集合**首尾相通**：播完最后一首回到第一首（列表循环，手动切歌与自动续播一致） | 最后一首结束或按下一首 → 回到集合第一首并继续播；集合第一首按上一首 → 跳到最后一首 |
| R6 | 清空搜索 → 恢复全库，正在播放的那首**仍是同一文件** | 清空后列表回到 N 行，`current` 指向的 path 不变 |
| R7 | 无结果 → 明确空态，不清空正在播放的曲目 | 显示「没有匹配的曲目」+ 一键清除；专辑页照常显示当前曲 |
| R8 | 搜索态下界面必须说明「播放范围被限定」 | footer 显示可点击的「5 / 200 首」，点击即清除搜索；hover 说明范围 |
| R9 | 正在播放但不匹配当前搜索的曲目，仍要在列表面前可见 | 结果列表上方一行灰色只读「▶ 正在播放 · 不在筛选内」，不计入命中数、不可点 |

---

## 2. 现状盘点（均为代码实测）

### 2.1 两份「列表」

```text
me-app  UI 线程                         音频线程（me-audio）
  MusicState.tracks: Vec<Track>   ──命令──▶  PlayerCore.queue: MusicQueue
  （全库：path + 文件名 + 时长）            （只有 Vec<PathBuf> + 顺序 + 游标）
  MusicState.current: Option<usize> ◀─状态── AudioState.current
```

- [`MusicState`](../../crates/me-app/src/state.rs) 的 `tracks` 来自 `AudioEvent::Playlist`（[app.rs](../../crates/me-app/src/app.rs) 的 `apply_audio_event`），是扫描结果的原样副本。
- [`PlayerCore::load_directory`](../../crates/me-app/src/audio/mod.rs) 只把 `paths` 交进 `MusicQueue::replace_tracks`，**曲名/时长/标签都留在 UI 侧**。音频线程不认「歌曲」，只认路径与索引。
- 因此：任何「播放列表」的裁剪都必须发生在 `MusicQueue` 里，UI 侧只能通过新命令通知它。

### 2.2 现有搜索只匹配标题，且只服务 AI

[`find_tracks`](../../crates/me-app/src/views/music.rs) 的全文：

```rust
pub fn find_tracks(tracks: &[Track], query: &str) -> Vec<usize>   // 只比 track.title.to_lowercase().contains()
```

它是 AI 的 `music_search` / `music_play` 与「界面搜索框」的**共用匹配逻辑**——这个共用是刻意的不变量（两边各写一遍，规则迟早分叉）。今天只有 AI 在用；本需求落地后由同一函数服务界面，只是多一个「匹配哪个字段」的参数。

### 2.3 `Track` 没有 artist

```rust
pub struct Track { pub path: PathBuf, pub title: String, pub duration_ms: Option<u64> }
```

`title` = 文件名去扩展名；标签标题只在专辑页通过 `NowPlaying` 显示。`tags::read_now_playing` 会顺带解码封面（`COVER_MAX_EDGE = 640`、上限 8 MB），**不能**在扫描/补齐阶段逐首调用——那是几千次图片解码。

时长补齐走 `PlayerCore::probe_batch`：每 tick 取 `PROBE_BATCH = 8` 首，`TICK = 250ms` → 约 32 首/秒，2000 首 ≈ 62 秒。artist 补齐可以挂同一条流水线（§3.6）。

### 2.4 `MusicQueue` 的游标语义

```rust
pub struct MusicQueue { tracks: Vec<PathBuf>, order: MusicOrder, play_order: Vec<usize>, cursor: usize }
```

- `play_order` 是「播放序列」，`cursor` 是**在 `play_order` 里的位置**，`current_index()` 才返回曲库索引。
- `rebuild_play_order` 的 `MusicOrder::Sequential` 分支今天写的是 `self.cursor = current.unwrap_or(0).min(len - 1)`——**它默认 `play_order == 0..len` 才成立**（cursor 与曲库索引同值）。加了作用域这条假设立刻失效，必须改成 `position()` 查找（§3.2 的重点陷阱）。
- `next_index` / `prev_index` / `on_finished` 全部只走 `play_order`，所以只要把 `play_order` 裁成命中集合，**「搜索决定播放列表」自然成立**，不需要在 UI 侧额外拦「下一首」。

### 2.5 侧栏的高度与宽度

[`views/music.rs::sidebar`](../../crates/me-app/src/views/music.rs) 今天是：

```text
dir_row（目录路径 + 选择/重扫）
status_line（扫描中 / 跳过 N 个 / 截断提示）
separator
列表（list_height = available_height - 30，占满）
separator
footer（N 首 + 顺序/随机切换）
```

- `list_height` 由 `ui.available_height()` 现算，所以**搜索区必须画在 `list_height` 之前**；顺序写反会导致列表压掉 footer，进而 `min_rect` 变宽变高 → egui 持久化 → 下一帧再涨（[CLAUDE.md](../../CLAUDE.md) 记的侧栏正反馈缺陷，只在多帧下暴露）。
- 侧栏默认 220 px、最小 160 px；`NARROW_SIDEBAR = 210.0` 以下不显示时长列。
- 横向行里 **不许**用 `ui.available_width()` 手工减固定宽分配剩余空间（它返回父容器剩余宽，不含本行已放元素与 `item_spacing`）——用 `horizontal_wrapped` 自然流式排。

---

## 3. 方案

### 3.1 总体模型：作用域（scope）

```text
                 ┌─────────────── MusicQueue ───────────────┐
tracks (全库) →  │  scope: Option<Vec<曲库索引>>             │  ← SetScope 命令写入
                 │  order: Sequential | Shuffle             │  ← SetOrder 命令写入（既有）
                 │  play_order: Vec<曲库索引>  ← scope × order │
                 │  cursor: play_order 位置                  │
                 └───────────────────────────────────────────┘
                          │
                    next / prev / 自动续播只在这个 play_order 里走
```

三条不变量：

- **scope 管「哪些」，order 管「什么顺序」，两者正交。** `rebuild_play_order` 是唯一同时看两者的地方。
- **scope 为空集合 ≠ 全库。** `None` = 全库（未搜索）；`Some(vec![])` = 搜索无命中（什么都不能播）。二者不可混用。
- **`play_order` 可能比 scope 多一项**：队首那首「被钉住」的曲目（pinned）。它有两个来源——
  「正在播放但不匹配搜索」（§3.2 的 `rebuild_play_order`）与「AI 点名播了一首不在结果集里的歌」
  （§3.2 的 `pin`）。缺了它，`current_index()` 会在搜索瞬间变成 `None`，专辑页显示
  「未在播放」而声音还在放（违反 R7），或者 AI 播的歌根本播不出来。

### 3.2 me-core：`MusicQueue` 加作用域

[`crates/me-core/src/music.rs`](../../crates/me-core/src/music.rs)：

```rust
pub struct MusicQueue {
    tracks: Vec<PathBuf>,
    order: MusicOrder,
    /// `None` = 全库；`Some(..)` = 命中集合（曲库索引，升序去重）。
    scope: Option<Vec<usize>>,
    play_order: Vec<usize>,
    cursor: usize,
}

/// 设置播放作用域。**不打断正在播放的曲目**：
/// 当前曲目仍在作用域内 → 游标跟着它；不在 → 它被钉在序列首位（游标指向它），
/// 声音不变，下一首从命中集合开头续。详见 `rebuild_play_order`。
///
/// `playing` 是**真正在播（已加载）**的曲目，没播过传 `None`。
/// 【落地时的偏差】最初这里用 `current_index()` 顶替，实测不行：非空队列的游标
/// 永远有值（那是游标，不是播放状态），刚扫描完就搜索会把第 0 首钉进结果集。
/// 音频线程用 `loaded` 判定后传进来。
pub fn set_scope(&mut self, scope: Option<Vec<usize>>, playing: Option<usize>, rng: &mut impl Rng) {
    self.scope = scope.map(|list| {
        let mut list: Vec<usize> = list.into_iter().filter(|i| *i < self.tracks.len()).collect();
        list.sort_unstable();   // 排序 + 去重，后面才能用 binary_search
        list.dedup();
        list
    });
    // `playing` 原样传下去：**不要**在这里过滤掉「不在新作用域内」的那首——
    // `rebuild_play_order` 需要它把在播的那首钉回队首。过滤掉就等于静默停播。
    self.rebuild_play_order(playing, rng);
}

fn in_scope(&self, index: usize) -> bool {
    match &self.scope {
        Some(list) => list.binary_search(&index).is_ok(),
        None => true,
    }
}

pub fn scope(&self) -> Option<&[usize]>;
pub fn first_index(&self) -> Option<usize>;   // play_order.first()，给「没有当前曲目时按播放」用
```

`rebuild_play_order` 改成：

```rust
fn rebuild_play_order(&mut self, current: Option<usize>, rng: &mut impl Rng) {
    self.play_order = match &self.scope {
        Some(list) => list.clone(),
        None => (0..self.tracks.len()).collect(),
    };

    // 【关键】正在播放但不在作用域内的曲目，钉在序列首位。
    // 不这么做的话 `current_index()` 会变成 None：专辑页/进度条/封面全部显示
    // 「未在播放」，而声音其实还在响（违反 R7「清白搜索不清空正在播放」）。
    // 钉在首位还有个副作用正好是我们想要的：next 从命中集合第一首继续。
    if let Some(index) = current
        && index < self.tracks.len()
        && !self.play_order.contains(&index)
    {
        self.play_order.insert(0, index);
    }

    match self.order {
        MusicOrder::Sequential => {
            // ⚠ 不能再用 `current.min(len-1)`：cursor 是 play_order 位置，不是曲库索引。
            self.cursor = current
                .and_then(|i| self.play_order.iter().position(|&x| x == i))
                .unwrap_or(0);
        }
        MusicOrder::Shuffle => {
            self.play_order.shuffle(rng);
            if let Some(pos) = current.and_then(|i| self.play_order.iter().position(|&x| x == i)) {
                self.play_order.swap(0, pos);   // 当前曲目置首，next 从它之后继续
            }
            self.cursor = 0;
        }
    }
}
```

连带的行为（都要进测试，见 §7）：

| 搜索时的处境 | `current_index()` | `next_index()` | `prev_index()` |
| --- | --- | --- | --- |
| 正在播的那首在命中集合里 | 它自己 | 集合里的下一首 | 集合里的上一首（集合首位时 `None`） |
| 正在播的那首不在集合里 | 它自己（钉在首位） | 命中集合第一首 | `None` |
| 命中集合为空 | 它自己（序列只有它） | `None` | `None` |
| AI 点名播了一首不在集合里的 | 它自己（钉在首位） | 命中集合第一首 | `None` |

再补一个方法，专门给「作用域外的定点播放」用（§3.7 的 AI 路径）：

```rust
/// 把一首**不在作用域内**的曲目临时插到队首并选中。**作用域不变**：
/// 它播完 / 按下一首 → 回到命中集合首项，「搜索决定播放列表」的承诺不被悄悄推翻。
///
/// 细节：队首若已经是「上一首 pinned」（同样在 scope 外），就**替换**它而不是再插一条。
/// 否则 AI 连点三首会攒成 [C, B, A, ...结果集]，「上一首」会一路往回翻历史。
pub fn pin(&mut self, index: usize) -> bool {
    if index >= self.tracks.len() {
        return false;
    }
    if let Some(pos) = self.play_order.iter().position(|&i| i == index) {
        self.cursor = pos;                 // 本来就在序列里（大多是命中项），正常选中
        return true;
    }
    match self.play_order.first() {
        Some(&head) if !self.in_scope(head) => self.play_order[0] = index,
        _ => self.play_order.insert(0, index),
    }
    self.cursor = 0;
    true
}
```

`pin` 与 `rebuild_play_order` 里那段「在播但不匹配就插到首位」是**同一条不变量的两个入口**：前者是「用户/AI 明确点了一首」，后者是「作用域变了、在播的那首被落下了」。切换顺序（`set_order`）、重新搜索（`set_scope`）都会走 `rebuild_play_order`，所以 pinned 项在这两种操作后**自动保留**（因为 `current` 仍在 scope 外，会再被插回去）。这条性质要写进注释——将来有人“优化”掉它，AI 钉播就会在切一次随机模式后消失。

其余改动：

- `replace_tracks` 里加 `self.scope = None;`（新曲库的索引与旧作用域无对应关系）。
- `play_index` 的兜底：`queue.select(index)` 失败但 `index < self.tracks.len()` 时改用 `queue.pin(index)` —— 作用域外的定点播放（AI 的曲库序号、或重扫竞态）**不改作用域**，只临时钉到队首。这取代了「放宽 scope 到全库」的写法：后者会让界面写着「5 首」而队列已经是全库（§4 D6）。

### 3.3 协议：一条新命令 + 一条新事件

[`crates/me-app/src/audio/mod.rs`](../../crates/me-app/src/audio/mod.rs)：

```rust
pub enum AudioCommand {
    // ...既有...
    /// 把播放队列裁剪到这些曲库索引（`None` = 全库）。
    /// 只影响「下一首 / 上一首 / 自动续播走哪些歌」，**不改变正在播放的曲目**。
    SetScope { scope: Option<Vec<usize>> },
}

pub enum AudioEvent {
    /// 字段不变（`tracks` / `skipped` / `truncated`）。
    Playlist { tracks: Vec<Track>, skipped: usize, truncated: bool },
    /// 后台补齐的曲目信息（一次打开文件拿全：时长 + 标题 + 歌手）。
    /// **取代** 原来的 `Durations(Vec<(usize, u64)>)`——分开两条事件会让每个文件开两次。
    TrackInfo(Vec<TrackInfo>),
    // ...既有...
}

/// 定义在 `audio/playlist.rs`，与 `Track` 同住。
pub struct TrackInfo {
    pub index: usize,               // 曲库索引
    pub duration_ms: Option<u64>,
    pub title: Option<String>,      // 标签标题（可空）
    pub artist: Option<String>,     // 标签歌手（可空）
}
```

- **不引入 `epoch`（曲库版本号）。** 代价评估过：作用域只在**重扫**时才可能错位，而重扫后 `apply_audio_event` 一定会「先置 `matches = None`、再重算」从而强制重发 `SetScope`（§3.4 / §5），`pending` 又是 FIFO；再加上 `Play { index, path }` 本来就是双键兜底。最坏情况是**一帧**内队列与列表不同步，下一帧自愈。为这点收益给 `MusicState` / `AudioCommand` / `AudioEvent` 各加一个字段不划算。
- `AudioCommand::Toggle` 的 `None if !queue.is_empty()` 分支改用 `queue.first_index()`，否则作用域不含 0 时会播错歌。

### 3.4 UI 状态：命中集合是派生缓存

[`crates/me-app/src/state.rs`](../../crates/me-app/src/state.rs) 的 `MusicState` 增三个字段（都是**纯 UI 状态，不落库**）：

```rust
pub search_query: String,
pub search_field: crate::audio::playlist::MatchField,   // Title / Artist
/// 命中集合（曲库索引）；`None` = 未搜索（全库）。**派生缓存**，只在
/// 输入 / 类型 / 曲库 / 标签补全变化时重算，绝不每帧算。
pub matches: Option<Vec<usize>>,
```

三个方法（**唯一出口**，其余地方不许直接改 `matches` 或往 `pending` 推 `SetScope`）：

```rust
impl MusicState {
    pub fn is_searching(&self) -> bool { self.matches.is_some() }

    /// 重算命中集合；**只有结果真的变了**才把作用域同步给音频线程。
    /// 返回是否变化——随机模式下每 tick 无脑重发会让 play_order 反复重洗。
    pub fn apply_search(&mut self) -> bool {
        let next = if self.search_query.trim().is_empty() {
            None
        } else {
            Some(crate::views::music::match_tracks(
                &self.tracks,
                &self.search_query,
                self.search_field,
            ))
        };
        if self.matches == next {
            return false;
        }
        self.matches = next;
        self.pending.push(AudioCommand::SetScope {
            scope: self.matches.clone(),
        });
        true
    }

    pub fn clear_search(&mut self) { self.search_query.clear(); self.apply_search(); }
}
```

`apply_search` 的调用点只有这几处——正好覆盖 R3 / R6 / 重扫 / 标签补全：

| 触发 | 位置 |
| --- | --- |
| 输入变化、类型切换、清除 | `views/music.rs::search_bar` |
| 曲库被替换（重扫 / 换目录） | `app.rs::apply_audio_event` 的 `Playlist` 分支 |
| 标签补齐使 artist 从无到有 | `app.rs::apply_audio_event` 的 `TrackInfo` 分支 |

### 3.5 界面：搜索区布局

侧栏新顺序（**搜索区必须在 `list_height` 计算之前**——现有 `sidebar` 是 `allocate_space(width, list_height)` + `max_rect` 渲染列表，顺序写反会把 footer 顶掉，进而触发 egui 的面板尺寸正反馈）：

```text
dir_row            目录路径 + [选择目录] [重新扫描]
status_line        扫描中 / 跳过 N 个 / 截断
separator
search_bar         ← 新增：输入框（占满宽）+ 类型 chips
separator
┌ 列表区（list_height = available_height - 30）──────┐
│ pinned_row   ▶ 正在播放 · 不在筛选内  晴天        │ ← 只在「在播但不匹配」时出现
│ ScrollArea（show_rows，占满剩余）                  │
└────────────────────────────────────────────────────┘
separator
footer             5 / 200 首（可点击）+ 顺序/随机
```

```rust
fn search_bar(state: &mut UiState, ui: &mut egui::Ui) {
    let mut query = state.music.search_query.clone();
    let hint = match state.music.search_field {
        MatchField::Artist => "搜索歌手",
        _ => "搜索歌曲名",
    };
    let width = ui.available_width();          // 单个控件占满宽，本身不会漏 item_spacing
    let response = ui.add(
        egui::TextEdit::singleline(&mut query)
            .hint_text(hint)
            .desired_width(width),
    );
    if response.changed() {
        state.music.search_query = query.clone();
        state.music.apply_search();
    }
    if response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) && !query.is_empty() {
        state.music.clear_search();
    }
    // 焦点不用手动处理：egui 的 TextEdit 在点击别处时自动失焦（点列表行即失焦），
    // 不要再自己加 surrender_focus —— 那需要一个从 track_list 拿不到的 Response。

    // 第二行：类型 chips。用 horizontal_wrapped 自然换行，
    // **不要**手工按 available_width 减固定宽（会把 item_spacing 漏掉 → 每帧撑宽 8px 的正反馈）。
    // 命中数只在 footer 显示一处（§5）：窄侧栏 160px 下两处都写会挤。
    ui.horizontal_wrapped(|ui| {
        for field in [MatchField::Title, MatchField::Artist] {
            let label = if field == MatchField::Artist { "歌手" } else { "歌曲名" };
            let selected = state.music.search_field == field;
            if ui.selectable_label(selected, RichText::new(label).small()).clicked() && !selected {
                state.music.search_field = field;
                state.music.apply_search();
            }
        }
    });
}
```

**pinned 行**（R9）：它必须画在 `ScrollArea` **外面**（列表区顶部），否则会跟着列表滚走、看起来像结果里的第一项。

```rust
/// 「正在播放但不匹配当前搜索」那一行：只读、不计入命中数、不可点。
/// 返回是否真的画了这一行（调用方一般不用，但测试可以据此断言）。
fn pinned_row(state: &UiState, ui: &mut egui::Ui) -> bool {
    let Some(current) = state.music.current else { return false };
    if !state.music.is_searching()
        || state.music.matches.as_ref().is_some_and(|m| m.contains(&current))
    {
        return false;                       // 未搜索，或在播的那首本来就在结果里
    }
    let track = &state.music.tracks[current];
    ui.horizontal(|ui| {
        ui.label(RichText::new("▶").small().color(ACCENT));
        ui.label(RichText::new("正在播放 · 不在筛选内").small().color(Color32::GRAY));
        ui.add(egui::Label::new(RichText::new(track.label()).small().color(ACCENT)).truncate());
    })
    .response
    .on_hover_text("这首不属于当前搜索结果，但仍在播放；下一首从结果集第一首继续");
    true
}
```

列表渲染改成「视图位置 → 曲库索引」的单点转换：

```rust
fn track_list(state: &mut UiState, ui: &mut egui::Ui) {
    if state.music.tracks.is_empty() { /* 既有空态 */ return; }

    pinned_row(state, ui);                  // 占一行高度；ScrollArea 的可用高度自动少一行

    let view_len = state.music.matches.as_ref().map_or(state.music.tracks.len(), Vec::len);
    if view_len == 0 {
        // 曲库有歌、但搜索无命中：**不许**复用「这个目录里没有音频文件」的文案。
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.label(RichText::new("没有匹配的曲目").small().color(Color32::GRAY));
            if ui.small_button("清除搜索").clicked() { state.music.clear_search(); }
        });
        return;
    }

    let mut clicked = None;
    egui::ScrollArea::vertical()
        .id_salt("music_tracks")
        .auto_shrink([false, false])
        .show_rows(ui, ROW_HEIGHT, view_len, |ui, range| {
            for position in range {
                // 唯一一处「视图位置 → 曲库索引」。别处一律只谈曲库索引。
                let index = match &state.music.matches {
                    Some(list) => list[position],
                    None => position,
                };
                if track_row(state, ui, index, position) {
                    clicked = Some(index);
                }
            }
        });

    if let Some(index) = clicked {
        let path = state.music.tracks[index].path.clone();
        state.music.current = Some(index);            // 曲库索引，语义不变
        state.music.pending.push(AudioCommand::Play { index, path });
    }
}
```

**footer 的范围指示**（R8）：左侧那个「N 首」在搜索态变成「5 / 200 首」并**可点击**（点击＝清除搜索＝播放范围回到全库）。右侧的「顺序 / 随机」不动。

```rust
fn footer(state: &mut UiState, commands: &mut Vec<Command>, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        let count = match &state.music.matches {
            Some(matches) => format!("{} / {} 首", matches.len(), state.music.tracks.len()),
            None => format!("{} 首", state.music.tracks.len()),
        };
        let searching = state.music.is_searching();
        let text = RichText::new(count).small().color(if searching { ACCENT } else { Color32::GRAY });
        let label = if searching {
            // 可点击：这是「播放范围被限定」唯一显式的解释与退出路径。
            ui.selectable_label(false, text).on_hover_text("播放范围：仅搜索结果。点击恢复全部")
        } else {
            ui.label(text)
        };
        if searching && label.clicked() {
            state.music.clear_search();
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| { /* 既有顺序/随机 */ });
    });
}
```

> 160 px 窄侧栏下「5 / 200 首」+ 右侧两个 chip 会比较紧。若实测溢出，优先缩短文案（`5/200`），**不要**改回按 `available_width` 手工分配——现有 footer 用的是 `right_to_left` 布局，天然安全。

**`transport` 的按钮可用性要拆开**：现在是 `enabled = !tracks.is_empty() && device_error.is_none()` 一个布尔值管三个按钮。搜索无命中时队列里只剩「正在播的那首」，`next/prev` 会环绕到它自己（=从头重播），看起来像 bug；必须单独禁用，而播放/暂停不能被连坐（正在响的歌还要能暂停）。

```rust
let has_view = state.music.tracks.len() > 0
    && state.music.matches.as_ref().map_or(true, |m| !m.is_empty());
let play_enabled = ...;                              // 既有逻辑
let next_enabled = play_enabled && has_view;
let prev_enabled = play_enabled && has_view;
```

`track_row(state, ui, index, position)` 的四处变化：

- 行首编号用 `position + 1`（**视图序号**）。搜索态下不再打印曲库序号，否则会出现「137. 晴天」这种看着像坏了的行。
- 曲名用 `track.label()`（标签标题优先、回退文件名）——与匹配、与专辑页的 `tags::display_title` 同一套取值，见 D7。
- **歌手模式下，行尾的时长列换成歌手名**（右对齐、灰、截断）：搜「周杰伦」得到 5 行却看不到歌手，用户无从确认命中的是什么。行高保持 `ROW_HEIGHT`，不做两行式行——`show_rows` 依赖固定行高。
- 性能：**不要**为了渲染而每帧 `clone` 索引表（2000 项 ≈ 16 KB/帧）。`matches` 以 `&[usize]` 借用即可。

`transport` 的按钮可用性补一条：**搜索无命中时禁用「上一首 / 下一首」**（`view_len > 0`），播放/暂停不受影响。否则空作用域下按下一首会把当前这首从头重播，看起来像 bug。

### 3.6 匹配规则与歌手数据

**一份匹配函数，三处调用**（界面 + AI 搜索 + AI 播放），沿用既有不变量：

```rust
// audio/playlist.rs —— 与 `Track` 同住：它是数据层的概念，不是界面概念，
// 这样 state.rs 里的 `MusicState.search_field` 不必反向依赖 views。
pub enum MatchField { Any, Title, Artist }

// views/music.rs —— 匹配函数留在原处（沿用既有的共用不变量与内联测试）
use crate::audio::playlist::{MatchField, Track};

pub fn match_tracks(tracks: &[Track], query: &str, field: MatchField) -> Vec<usize> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() { return Vec::new(); }          // 空词不命中全部（既有语义）
    tracks.iter().enumerate()
        .filter(|(_, track)| {
            let title = track.label().to_lowercase();     // 标签标题优先，回退文件名
            let artist = track.artist.as_deref().unwrap_or("").to_lowercase();
            match field {
                MatchField::Title  => title.contains(&needle),
                MatchField::Artist => artist.contains(&needle),
                MatchField::Any    => title.contains(&needle) || artist.contains(&needle),
            }
        })
        .map(|(index, _)| index)
        .collect()
}

/// 旧名保留：AI 与既有测试零改动。`Any` = 标题或歌手命中。
pub fn find_tracks(tracks: &[Track], query: &str) -> Vec<usize> {
    match_tracks(tracks, query, MatchField::Any)
}
```

**artist 的三级来源**（`playlist::Track` 增 `artist: Option<String>` 与 `tag_title: Option<String>`）：

| 级别 | 来源 | 何时可用 | 成本 |
| --- | --- | --- | --- |
| 1 | 文件名 `艺术家 - 歌名` 前缀 | 扫描当场 | 0（纯字符串） |
| 2 | 文件标签（需新增 `tags::read_track_info`） | 后台补齐，≈60 秒/2000 首 | 每首一次文件头读取 |
| 3 | 空 | 都没有 | — |

文件名前缀的采纳规则（避免把 `01 - Intro` 认成歌手「01」）：

- 必须含 ` - `（两侧带空格）；取**第一个**分隔符左侧；
- 左侧 trim 后长度 ≥ 2、不以数字开头、不含路径分隔符；
- 命中则写入 `artist`。文件名推断只是**临时值**：标签 artist 一到就覆盖（`TrackInfo.artist.is_some()` 时直接写），标签为空才保留推断结果。不要为「这个值从哪来」再加一列状态。

`tags.rs` 新增（复用 `read_tags` 的解析，**但把封面预算压到 0**）：

```rust
pub struct TrackTags {
    pub title: Option<String>,
    pub artist: Option<String>,
}

/// 只读标题与歌手，**不读封面**。扫描补齐用，逐首调用。
/// 与 `read_now_playing`（切歌时用，允许读封面）是两条路径，不要合并。
pub fn read_track_info(path: &Path) -> TrackTags;
```

【落地实测】**`limit_visual_bytes: Limit::Maximum(0)` 在 symphonia 里并没有阻止读取**——64×64 的内嵌图照样被读出来。所以真正的做法是两道保险：

1. `read_tags_with(path, options, want_picture: bool)` 增一个开关，扫描路径传 `false`，`apply_revision` 里 `if want_picture && tags.picture.is_none()` 直接跳过 visual 解析；
2. `limit_visual_bytes` 保留 0（少读数据）。

单测 `tags::tests::scan_reader_takes_text_but_no_cover` 用同一个 fixture 同时断言「标题/歌手读到了」+「`picture` 为空」，把这条语义钉住；`limit` 单独靠不住，别只改常量。

`probe_batch` 改为一个批次同时产出时长与标签：

```rust
fn probe_batch(&mut self, events: &mut Vec<AudioEvent>) {
    if self.pending_probe.is_empty() { return; }
    let take = PROBE_BATCH.min(self.pending_probe.len());
    let batch: Vec<usize> = self.pending_probe.drain(..take).collect();
    let mut done = Vec::new();
    for index in batch {
        let Some(path) = self.queue.path_at(index).map(|p| p.to_path_buf()) else { continue };
        // 每首两次打开（rodio 解码器拿时长 + symphonia 拿标签），都只读文件头，
        // 第二次基本命中页缓存；PROBE_BATCH = 8 维持不变。
        let duration = self.backend.probe_duration(&path).map(|d| d.as_millis() as u64);
        let tags = tags::read_track_info(&path);
        done.push(TrackInfo {
            index,
            duration_ms: duration,
            title: tags.title,
            artist: tags.artist,
        });
    }
    if !done.is_empty() {
        events.push(AudioEvent::TrackInfo(done));
    }
}
```

### 3.7 与 AI 工具的交互

AI 面对的是**整个曲库**，而界面可能正处于搜索态。三条规则：

1. `music_search` / `music_play { query }` 一律用 `MatchField::Any`（标题或歌手都算），**不受界面搜索态影响**——AI 的 `query` 是曲库语义，不是「在当前筛选结果里再筛」。
2. **AI 的定点播放不改变作用域，只 `pin` 到队首**（§3.2）：用户搜「周杰伦」时让 AI「放一首晴天」，晴天被钉到 `play_order[0]` 正常播放，搜索结果集仍是队列的主体，它播完 / 按下一首 → 回到结果集首项。**不清空用户的搜索词**。
   - 为什么不走「播前 `clear_search()`」：那会销毁用户刚输入的上下文，还会引入一个跨函数的发送顺序陷阱（`clear_search` 只进 `pending`，而 `player.send(Play)` 是立即发，必须先 `flush_music_commands()` 再 send）。`pin` 把这个陷阱整条删掉。
   - 为什么不走「放宽 scope 到全库」：界面写着「5 首」而队列已经是全库，等于把「搜索决定播放列表」偷偷推翻。
3. AI 的 `music_control`（Next / Prev）**沿用当前作用域**，不特殊处理：音频线程的 `next_index()` 本来就走 `play_order`，用户看到什么就播什么。搜索无命中时序列只剩当前曲，`next` 会环绕到它自己（界面按钮此时是禁用的，AI 侧走不到正常路径）。

AI 的搜索摘要**不再带数字序号**。今天的实现是 `format!("{}. {}", index + 1, ..)`（曲库序号），而用户看到的列表编号是视图序号，搜索态下两者必然不同 → 模型说「第 3 首」用户会点到别的歌。改为只列曲名（`晴天、七里香、夜曲`），`index` 参数保留给程序化调用。

`pin` 只在 `play_index` 的兜底里触发（`select` 失败），AI 侧不需要任何额外调用：`begin_ai_music` 里 `player.send(AudioCommand::Play { index, path })` 保持原样。

---

## 4. 关键决策

| 编号 | 决策 | 理由 | 代价 / 被否方案 |
| --- | --- | --- | --- |
| D1 | 作用域加在 `MusicQueue`，而不是 UI 过滤渲染 | 队列在音频线程，「下一首」由它决定；UI 过滤只能改画面 | 改了 `me-core` 的公开类型（`MusicQueue` 加字段 + 3 个方法）。被否：把搜索集合作为 `Play` 的参数一次性传入——它只影响一次播放，改不了 `next` |
| D2 | 保留全库 `tracks`，`scope` 只做裁剪 | `current` / 封面 / AI `index` 全部仍是曲库索引，现有代码一行不用改语义 | `MusicQueue` 内部多一次 `position` 查找（O(n)，n ≤ 2000，纯内存）。被否：把 `tracks` 换成搜索结果——索引全体漂移 |
| D3 | 搜索**实时**生效，无「搜索」按钮 | 本地曲库是内存里的 2000 项过滤，比一次点击的往返还快 | 每敲一个字符重建一次 `play_order`（O(n)，随机模式会重洗）。缓解：只在命中集合变化时下发 |
| D4 | 搜索无命中 → 作用域是空集合，并禁用上一首/下一首 | `Some(vec![])` 与 `None` 语义必须分开，否则「搜了个没人名的词」会偷偷变成全库播放 | `transport` 的 `enabled` 要从「一个布尔管三个按钮」拆成 `play / next / prev` 三份（否则暂停会被连坐）。被否：空结果时回退全库——用户会以为搜索没生效 |
| D5 | 搜索词与类型**不持久化** | 与「播放状态不落库」一致；持久化会导致下次启动只剩 3 行的惊吓 | 每次进页面要重新搜。若将来要记住，写入 `meta` 而非新增表（本需求不做） |
| D6 | 作用域外的定点播放 → `MusicQueue::pin(index)` **临时钉到队首**，不改作用域 | 静默失败在 AI 场景里表现为「工具说播了、实际没播」；而「放宽 scope 到全库」会让界面写着「5 首」、队列却是全库 | `play_order` 首位可能是 scope 外的一项（§3.1 不变量），`pin` 要「替换上一首 pinned」而不是无限插入。被否：①清空搜索再播——销毁用户上下文 + 跨函数发送顺序陷阱；②放宽 scope——偷偷推翻承诺 |
| D7 | 列表显示用**标签标题优先**，回退文件名 | 专辑页已经这么显示（`tags::display_title`），列表却不一致——同一首歌两处两个名字 | 列表文案会变（`01. 晴天` → `晴天`）。若不想改动文案，只需不接 `tag_title`，其余设计不受影响 |
| D8 | 歌手模式下把行尾时长换成歌手名 | 搜索要能自证「为什么这行命中了」 | 歌手模式下看不到时长 |
| D9 | 正在播放但不匹配的曲目**钉在 `play_order` 首位**，并在列表顶部单列一行只读提示（R9） | 否则搜索瞬间 `current_index()` 变 `None`，专辑页显示「未在播放」而声音还在响（违反 R7）；只钉不显示又会让用户在 5 行结果里找不到正在响的那首 | 列表多一行、`play_order` 比 scope 多一项。被否：①让它掉出队列——专辑页错乱；②搜索时暂停播放——用户没要求停；③只在 P2 提示——本期就会收到“列表漏歌”的反馈 |
| D10 | footer 的「N 首」在搜索态变成**可点击的「5 / 200 首」** | 「搜索决定了播放范围」是个隐形状态：播完 5 首就停，界面必须给解释和退出路径 | footer 左半边的宽度在 160 px 下变紧（优先缩短文案为 `5/200`） |
| D11 | artist **后台全量补齐、不持久化** | `probe_batch` 本来就要逐首开文件拿时长，追加一次标签头读取边际成本低；不引入缓存失效逻辑 | 每次启动约 60 秒补全期（2000 首），期间搜歌手可能漏命中；`status_line` 提示补全进度留作 P2 |
| D12 | **不引入 `epoch`（曲库版本号）** | 重扫后 `apply_audio_event` 必强制重发 `SetScope`、`pending` 是 FIFO、`Play` 又是 `index + path` 双键，最坏只错一帧 | 三个类型少三个字段；代价是接受「一帧内队列与列表可能不同步」，无法用测试稳定复现（也就不为它写测试） |
| D13 | **首尾相通（列表循环）**：`next_index` 在末尾回到首项、`prev_index` 在首项回到末项 | 顺序与随机共用同一条环绕规则，不会造出第二套语义；也消掉「随机到最后一首后按下一首没反应」这个必然会被当成 bug 的现象 | **推翻初版「末尾停」**：单一作用域（只有 1 首 / 搜索结果只 1 首）时下一首就是它自己＝重播。不做单曲循环开关，全库与结果集一视同仁 |

---

## 5. 边界与错误处理

| 场景 | 行为 |
| --- | --- |
| 搜索中输入全空格 | 视为空查询 → 回到全库（`trim().is_empty()`） |
| 搜索态下重扫 / 换目录 | `Playlist` 分支**先**把 `matches` 置 `None`、**再**调 `apply_search()`：绕过「新旧命中集合恰好相等 → 不下发」的去抖，强制把新作用域送回音频线程（旧索引在新曲库里指别的歌）。`replace_tracks` 内部也把 scope 置 `None` 作为兜底 |
| 搜索态下换顺序（顺序 ⇄ 随机） | 既有 `set_order` → `rebuild_play_order` 会用当前 scope，无需额外处理 |
| 搜索态下正在播放的曲目不在集合内 | 队列里它被钉在首位（`play_order[0]`），专辑页 / 进度 / 封面**完全不受影响**；列表顶部单列一行只读提示（R9 / D9） |
| AI 在搜索态下点名播一首不匹配的歌 | `pin` 到队首，搜索词与作用域都不变；播完 / 下一首 → 回到结果集首项（§3.7） |
| AI 连着钉播多首不匹配的歌 | 只保留**一首** pinned：队首若已是 scope 外的项就替换它，不累积（否则「上一首」会一路翻历史） |
| 命中集合只有 1 首 | 下一首/上一首 → 重播这一首（列表循环的必然结果），符合 R5 / D13 |
| 从命中集合第 1 首按「上一首」 | 会回到「刚在播但被钉在队首」的那首——这是刻意的：它确实刚播过，且不属于当前集合 |
| artist 尚未补齐时搜歌手 | 只有文件名推断能命中（D11）；`status_line` 可加「正在补全曲目信息 N/M」（P2） |
| 音频设备打不开（`device_error`） | 搜索与列表照常可用（纯 UI），只有播放相关控件禁用 |
| 序号 ≥ 100 | `{:02}` 自然变宽，既有行为，不在本需求修 |
| 曲库里出现重复路径 | `set_scope` 去重；`play_order` 不含重复项 |

---

## 6. 落地步骤

每步都能独立编译 + 跑测试，不要一把梭。

| 步骤 | 改动 | 验证 |
| --- | --- | --- |
| 1 | `me-core/src/music.rs`：`scope` 字段、`set_scope` / `scope()` / `first_index()` / `in_scope` / **`pin`**、`rebuild_play_order` 改 `position()` + 在播不匹配插首位、`replace_tracks` 清 scope | `cargo test -p me-core music` |
| 2 | `audio/playlist.rs`：`Track` 加 `artist` / `tag_title` + `label()`、`MatchField`、`TrackInfo` 结构、文件名前缀推断；`audio/tags.rs`：`read_track_info`；`audio/mod.rs`：`AudioCommand::SetScope`、`AudioEvent::TrackInfo`、`probe_batch` 改造、`play_index` 兜底改 `pin` | `cargo test -p me-app audio::` |
| 3 | `state.rs`：`MusicState` 三个字段 + `apply_search` / `clear_search` / `is_searching`；`views/music.rs`：`search_bar`、`match_tracks`（+ `find_tracks` 改为薄封装）、`pinned_row`、`track_list` 视图化、`track_row` 加 `position`、footer 可点击范围指示、`transport` 拆按钮可用性 | `cargo test -p me-app views::music` |
| 4 | `app.rs::apply_audio_event`：`Playlist` / `TrackInfo` 分支接 `apply_search`（`Playlist` 先置 `None` 再重算）；`begin_ai_music`：搜索摘要去序号（**不再** `clear_search`、不再需要的 `flush_music_commands` 前置） | `cargo test -p me-app` |
| 5 | 回写 [CLAUDE.md](../../CLAUDE.md)（「crate 关键事实」的音乐条目 + 「测试布局」表 + me-core `music.rs` 的描述）与 [doc/versiondoc/版本跟踪.md](../versiondoc/版本跟踪.md)（按约定本表要持续维护） | 文档 diff 自查 |

质量门禁同其他改动：`cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`。

---

## 7. 测试计划（落地后按实际测试名回填）

| 位置 | 断言（测试名） |
| --- | --- |
| `me-core/src/music.rs` 内联 | `sequential_walks_forward_and_wraps_at_the_end` / `prev_at_start_wraps_to_the_last_track`（首尾相通）、`scope_limits_next_and_prev`（next/prev 只在集合内、集合末尾回到集合首项）、`select_outside_scope_is_rejected`、`clearing_scope_restores_the_full_library`、`replace_tracks_clears_scope`、`sequential_cursor_uses_position_not_index`（**回归**：`select(5)` + `scope([2,5,9])` → `cursor == 1`）、`playing_track_outside_scope_stays_current`、`empty_scope_keeps_current_and_blocks_navigation`、`scope_is_normalized_and_bounds_checked`、`shuffle_over_scope_is_a_permutation_of_the_scope`、`pin_plays_outside_scope_and_replaces_the_previous_pin`、`pinned_track_survives_order_and_scope_changes`、`pin_rejects_unknown_index` |
| `me-app/src/audio/mod.rs` 内联 | `scope_keeps_next_inside_the_search_results`（Next 跳过集合外、集合末尾回到集合首项）、`last_track_finishing_wraps_to_the_first`（自动续播也环绕）、`play_outside_scope_pins_without_widening_the_scope`（集合外索引能播、作用域不变、下一首回集合首项）、`probes_durations_in_batches`（`TrackInfo` 带时长、假 mp3 不编造歌手） |
| `me-app/src/audio/playlist.rs` 内联 | `artist_is_inferred_from_file_name_when_possible`（含 `01 - Intro` / 单字符 / 无分隔符 / 无空格四个否定用例）、`scan_fills_filename_artist_and_leaves_tag_fields_empty`、`label_prefers_the_tag_title` |
| `me-app/src/audio/tags.rs` 内联 | `scan_reader_takes_text_but_no_cover`（同一 fixture：标题/歌手读到了 + `picture` 为空） |
| `me-app/src/views/music.rs` 内联 | `search_filters_the_visible_list_and_the_footer`（未命中行不画、footer 显示 `1 / 2 首`）、`search_hint_follows_the_field`、`artist_field_matches_only_the_artist`（含 AI 侧 `Any` 的对照）、`empty_results_have_their_own_hint`、`pinned_row_reports_the_playing_track_outside_the_results`、`pinned_row_is_absent_when_the_playing_track_matches`、`clicking_a_filtered_row_plays_the_library_index`（点视图第 1 行 → `Play{index:1}`）、`stepping_is_disabled_only_when_nothing_matches`、`sidebar_content_does_not_drift_across_frames`（**连跑 3 帧**宽度不漂移，CLAUDE.md 的正反馈回归） |
| `me-app/src/app.rs` 内联 | `playlist_event_recomputes_and_resends_the_search_scope`（新旧命中集合「看起来相同」也必须重发）、`track_info_fills_artist_and_refreshes_artist_search`（补齐后按歌手搜才命中）、`duration_event_fills_matching_track_only`（越界索引不 panic） |

> 没为「AI 播歌不清空搜索」单独写测试：`begin_ai_music` 需要整套 `App` 与音频线程，成本远高于收益；
> 行为由 `pin` 的单测（作用域不变）+ `begin_ai_music` 里根本没有 `clear_search` 调用这两点共同保证。

---

## 8. 非目标

- 不做多目录 / 跨盘曲库，也不改 `ScanLimits`（深度 8、2000 文件）。
- 不做按专辑、格式、码率、时长搜索。
- 不做拼音 / 首字母 / 模糊容错（可作为 P2 挂在 `match_tracks` 上，不影响本期结构）。
- 不做搜索历史、不做正则、不做高亮命中片段。
- **不做循环开关**：列表循环是**唯一**行为（D13），不提供「单曲循环 / 不循环」的切换。将来若要加，做成全局开关，对全库与搜索结果一视同仁。
- **不做 artist 持久化缓存**（D11）：`meta` 里有位置（`MUSIC_DIR` / `MUSIC_ORDER` 的先例），但缓存失效逻辑不便宜，实测接受 60 秒补全期后再议。
- **不做 AI 工具的对外语义变更**：只改「AI 摘要不带序号」，`music_search` / `music_play` / `music_control` 的参数与含义不动。
- **不改 schema**：搜索词、命中集合、作用域全部是进程内状态。

---

## 9. 风险

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| 后台补 artist 约 60 秒/**每次启动**（`ensure_player` 每会话扫一次，不是每次进页面） | 刚启动就搜歌手，只有文件名推断能命中 | 文件名前缀先行（D11）；`status_line` 提示补全进度（P2）；标签到位后 `apply_search` 自动刷新 |
| 实时搜索 + 随机模式反复重洗 | 「下一首」顺序抖动 | 只在命中集合**变化**时下发 `SetScope`（§3.4） |
| 视图序号与曲库索引双轨 | 点击/编号错位 | 转换点只有 `track_list` 里那一行；AI 摘要不带序号 |
| 侧栏高度预算 | 列表压掉 footer，进而面板尺寸正反馈 | 搜索区与 pinned 行都画在 `list_height` 之前 / 之内；多帧宽度回归测试 |
| `rebuild_play_order` 的 Sequential 分支 | 改用 scope 后游标错位（点 A 播 B） | 改成 `position()` 查找，并用「scope 非连续」的用例钉住 |
| 「搜索决定播放范围」是隐形状态 | 播完 N 首就停，用户以为坏了 | footer 可点击的「n / N 首」（R8）+ 顶部 pinned 行（R9） |
| `pin` 连续调用累积 | 「上一首」一路翻历史，`play_order` 越来越长 | `pin` 里「队首已是 scope 外则替换」（§3.2），并写「连续 pin 两次只多一项」的测试 |
| 去掉 `epoch` 后重扫与搜索并发 | 最多一帧的队列/列表不一致 | 重扫路径强制重发 `SetScope`（§5）+ `Play` 的 `index/path` 双键兜底（D12） |
