# AI 模块落地方案

> 定位：把 [AI模块设计.md](AI模块设计.md) 里标为「待建」的部分，翻译成**文件级、函数级、可验收**的改动。
> 设计文档讲「为什么这样设计」，本文只讲「在哪改、改成什么、怎么验」。
>
> 代码基线：workspace `0.3.0-dev`，schema v4，`cargo test --workspace` 513 passed（2026-11-05）。
> 本文所有行号对应该基线，改动后行号会漂移，以函数名为准。
>
> 阅读顺序：§1 看总量 → §2 看已经有什么（别重复造）→ §3 看文件地图 →
> §4 看数据结构 → §5 按 T 编号做事 → §6～§9 是迁移、测试、风险、验收。

---

## 1. 结论先行

| 阶段 | 目标 | 触及 crate | 新文件 | 改文件 | 可独立发布 | 主要风险 |
| --- | --- | --- | --- | --- | --- | --- |
| M0 | 修掉「档案解析失败静默清空」 | me-core / me-sync | 0 | 2 | 是 | 低（纯修 bug） |
| M1 | `motive` + `[结果]` 契约 | me-core / me-sync / me-store / me-app | 0 | 6 | 是 | 低 |
| M2 | 任务清单（含 schema v5） | me-core / me-sync / me-store / me-app | 1 | 8 | 是 | 中（迁移 + 预算） |
| M3 | 能力补齐（邮件/日历/天气/卡片） | me-core / me-sync / me-store / me-app | 0 | 11 | 是 | 高（权限结构迁移） |
| M4 | 音乐模块 | me-core / me-sync / me-app | 1 | 9 | 是 | 中（跨层回执 + 播放器惰性启动） |

数量口径：工具 **4 → 14**（新增 10 个），事件 **+3（另扩字段 2 个、改造 1 个）**，
命令 **+2**（`AiMusicResult` / `RestoreCalendarEvent`），权限开关 **4 → 9**，
schema **v4 → v5**（只加一列），页面不变（仍是 `Page::AiChat`，不新增页面）。

```
M0 ──► M1 ──► M2 ──► M3 ──► M4
修陷阱  契约层   清单层   能力层   音乐
(1天)   (2天)   (3天)   (4天)   (2天)
```

天数只是相对量级，用来判断「能不能一个 PR 做完」：M0+M1 可以一个 PR，M2 必须单独一个
（带迁移），M3 建议拆成「权限」「邮件+卡片」「日历」「天气」四个 PR，M4 单独一个。

---

## 2. 现状基线

### 2.1 已经具备，直接复用（不要重写）

| 能力 | 位置 | 说明 |
| --- | --- | --- |
| 工具循环 | `me-sync/src/ai.rs:1101`（`'rounds: for`）、`:1152`（逐 call 执行） | 4 轮 / 8 次预算，取消与总时长已接 |
| 执行入口 | `me-sync/src/ai_tools.rs:115`（`execute_ai_tool`） | 权限已校验、分档超时已接（`:42-45`） |
| 权限白名单 | `me-core/src/ai.rs:1127`（`enabled`）、`:1497`（`tool_specs`） | 未下发的工具，模型看不到 |
| 重放判定 | `me-core/src/ai.rs:583`（`is_replay_safe`） | 目前只排除 `calendar_create` |
| trace 落库 | `me-store/src/ai.rs:87`（`ToolTraceRow`） | `tool_trace` 是 TEXT 存 JSON，加字段不需要迁移 |
| 邮件卡片 | `me-app/src/views/ai.rs:663`（`show_cards`）+ `AiCard`（`:38`） | 已能打开同窗口，预览不标已读 |
| 活动条 | `me-app/src/views/ai.rs:645`（`show_activity`） | 显示工具 / 状态 / 耗时，缺动机 |
| 阅读窗 | `me-app/src/views/reader.rs:16`（`show_window`） | 卡片要复用的就是它 |
| 日历改删用例 | `me-sync/src/calendar.rs:131`（`update_event`）、`:149`（`delete_event`） | **已存在**，工具层只差壳 |
| 音频命令集 | `me-app/src/audio/mod.rs:38`（`AudioCommand`） | Play/Toggle/Next/Prev/Seek/SetOrder/Stop 全有 |
| 媒体库 | `me-app/src/state.rs:81`（`music.tracks`） | app 侧有完整曲目列表，搜索不用新接口 |

### 2.2 缺什么（这次要做的）

| 缺的件 | 现状 | 决定落在哪 |
| --- | --- | --- |
| `motive` 字段 | 无 | `TOOL_SPECS` + `AiToolTrace` |
| `[结果]` 解析与渲染 | 无 | `views/ai.rs::show_message` 前置分支 |
| 清单（plan） | 无 | `plan_update` 工具 + `ai_messages.plan` 列 |
| 权限粒度 | 4 个 bool | 扩到 9 个 bool（键名不变） |
| 邮件读正文 | 无 | `mail_read` + 去重/预算规则 |
| 日历读改删 | 无 | 3 个工具壳 + 删除确认 |
| 音乐控制 | 无 | 4 个工具 + app 侧执行器 |
| 卡片命中理由/原地弹 | 部分 | 事件带 reason + reader 渲染条件解耦 |

### 2.3 三个「看起来要做、其实不用做」

1. **`me-proto/src/ai.rs` 不用改**。工具 schema 是 `spec.input_schema` 原样透传
   （`:178` Anthropic / `:261` OpenAI），给 `motive` 加属性只改 `me-core` 的 `TOOL_SPECS`。
2. **`AudioCommand` 不用加变体**。播放指定曲目用现成的 `Play { index, path }`，
   app 侧从 `state.music.tracks[index]` 取 path 即可。
3. **日历用例不用重写**。`update_event` / `delete_event` 已具备，缺的只是工具壳与确认流程。

### 2.4 两个隐藏前提（不满足则对应任务直接失败）

这两条不是「已有能力」，也不是「缺口」，而是**会让任务做完却不生效**的前提，先记在这里：

| 前提 | 现状 | 影响的任务 |
| --- | --- | --- |
| 播放器是惰性启动的 | `app.rs:443` 的条件写死 `page == Page::Music && player.is_none()`，`state.music.tracks` 只在播放器起来后由 `AudioEvent::Playlist`（`:1562`）填充。**停在 AI 页时 player 为 `None`、tracks 为空** | T15：不补「收到音乐命令就启动播放器 + 加载目录」，所有音乐工具永远回「媒体库为空」 |
| 动机没有上事件通道 | `app.rs:1210` 用 `AiToolTrace { call_id, tool, status, summary, duration_ms }` 构造活动条条目，字段来自 `Event::AiToolCompleted`；该事件里没有 motive | T3：不扩事件，活动条只能在落库刷新后显示动机，等于流式期间没有 |

这两点的共同点：**代码能编译、任务看起来做完了，但功能不生效**。§8 的风险表里也各有一条。

---

## 3. 改动地图（文件级）

| crate | 文件 | 改什么 | 任务号 |
| --- | --- | --- | --- |
| me-core | `src/ai.rs` | 工具枚举 4→14（`as_str`/`parse`/`label`/`enabled` 四处穷尽 match）；**`is_replay_safe` 改成正向白名单**（现在是 `!matches!(self, CalendarCreate)`，加工具不会报错）；`TOOL_SPECS` +10；`AiToolPermissions` 4→9；`AiToolTrace` +2；`MailCardRef`；`SYSTEM_PROMPT` 改写 | T1,T2,T3,T10 |
| me-core | `src/ai_plan.rs`（新） | `Plan` / `PlanStep` / `Capability` / `PlanStatus` / `validate_plan` | T6 |
| me-core | `src/music.rs` | `MusicOp` / `MusicOutcome`（跨 `me-sync` ↔ `me-app` 的类型必须住在这里，两边都要引用） | T15 |
| me-core | `src/command.rs` | `Event` +3（`AiPlanUpdated` / `AiMusicCommand` / `AiToolCalendarDeletePending`）；`Command` +2（`AiMusicResult` / `RestoreCalendarEvent`）；`AiToolStarted` 加 `motive`、`AiToolCompleted` 加 `motive`/`motive_source`；`AiToolEmailResults` 改成带 reason 的 `cards` | T3,T7,T12,T13,T15 |
| me-core | `src/lib.rs` | `SCHEMA_VERSION` 4→5 | T8 |
| me-core | `src/port.rs` | `CalendarRepo` 加 `restore_event`（挨着 `:769` 的 `soft_delete_event`） | T12b |
| me-proto | — | **不动** | — |
| me-sync | `src/ai.rs` | `run_ai_stream` 四处钩子（空 tool_calls 续跑 / plan_update 拦截 / 步骤状态回写 / 取消与崩溃收尾）；`start_ai_turn` 把 `refs` 传进工具层；`ai.rs:238` 静默回落修复 | T1,T7,T8,T11 |
| me-sync | `src/ai_tools.rs` | `execute_ai_tool` 扩到 14 个分支；`ToolBudget` 加 `mail_read`/`calendar`/`music` 计数与 `refs` 快照；新增超时常量（plan 1s / mail 3s / calendar_list 2s / weather_locations 2s / music 3s；改删复用现有 `CALENDAR_TIMEOUT`）；新增 `tool_mail_read` / `tool_calendar_*` / `tool_weather_locations` | T2,T11,T12,T14 |
| me-sync | `src/ai_music.rs`（新） | `pending: Mutex<HashMap<String, oneshot::Sender<MusicOutcome>>>` + 3s 超时 + 取消清理 | T15 |
| me-sync | `src/lib.rs` | 分发 `Command::AiMusicResult` / `RestoreCalendarEvent` | T12b,T15 |
| me-sync | `src/calendar.rs` | 新增 `restore_event` 用例（发 `CalendarEventsChanged`） | T12b |
| me-store | `src/migrations.rs` | `migrate_v4_to_v5`：`ALTER TABLE ai_messages ADD COLUMN plan TEXT` | T8 |
| me-store | `src/ai.rs` | `AiMessage` + `plan`；`finish_assistant_message` 带 plan；`ToolTraceRow` + `motive`/`motive_source` | T2,T8 |
| me-store | `src/calendars.rs` | 新增 `restore_event`（`SET deleted_at = NULL`）——现在只有 `soft_delete_event`，没有恢复路径 | T12b |
| me-app | `src/views/ai.rs` | 清单面板、卡片改造（理由/上限/原地弹）、活动条显示动机、`[结果]` 分支、删除确认卡与撤销条、设置里权限分组 | T3,T4,T9,T12,T13 |
| me-app | `src/app.rs` | 三个新事件的处理；`AiToolTrace` 构造补 motive（`:1210`）；reader 渲染条件解耦（`:1504`）；音乐执行器跨帧状态机 + 播放器按需启动 | T3,T13,T15 |
| me-app | `src/state.rs` | `UiState` + `reader_open`、`pending_music`、`pending_delete` | T9,T12,T13,T15 |
| me-app | `src/views/music.rs` | 抽出纯函数 `find_tracks(query) -> Vec<usize>` 供 AI 执行器复用 | T15 |
| me-app | `src/views/richtext.rs` | **不动**（`[结果]` 在 `views/ai.rs` 里先切分再交给 markdown） | — |
| 测试 | `me-sync/tests/ai_flow_mock.rs` | 清单 / motive 降级 / 音乐超时 / 删除确认 四组 | 全部 |

---

## 4. 数据结构改动（字段级）

### 4.1 `AiToolName`：4 → 14

```rust
pub enum AiToolName {
    // 已有
    EmailSearch, WeatherGet, CalendarCreate, MemoryUpsert,
    // 新增
    PlanUpdate,
    MailRead,
    WeatherLocations,
    CalendarList, CalendarUpdate, CalendarDelete,
    MusicSearch, MusicPlay, MusicControl, MusicState,
}
```

必须同步的五处 match，但要分清**哪些编译器会兜住**：

| match | 形态 | 漏了会怎样 |
| --- | --- | --- |
| `as_str` / `parse` / `label` / `enabled` | 穷尽 match（逐个列出） | 编译报错，安全 |
| `is_replay_safe` | `!matches!(self, AiToolName::CalendarCreate)` | **编译通过，但 10 个新工具全被当成可重放** —— 「重新生成」会真的删日程、真的切歌 |

所以 `is_replay_safe` 要**反过来写**：枚举「只读工具白名单」，其余一律 `false`。
这样以后再加工具，默认就是不可重放（失败安全），而不是默认可重放。

| 工具 | 模块 | 读/写 | 重放安全 | 超时 | 计入次数 | 执行层 |
| --- | --- | --- | --- | --- | --- | --- |
| `plan_update` | 横切 | 只改内存计划 | 是 | 1s | **否** | me-sync |
| `email_search` | mail | 读 | 是 | 2s | 是 | me-sync |
| `mail_read` | mail | 读 | 是 | 3s | 是 | me-sync |
| `weather_get` | weather | 读 | 是 | 20s | 是 | me-sync |
| `weather_locations` | weather | 读 | 是 | 2s | 是 | me-sync |
| `calendar_list` | calendar | 读 | 是 | 2s | 是 | me-sync |
| `calendar_create` | calendar | 写 | **否** | 5s | 是 | me-sync |
| `calendar_update` | calendar | 写 | **否** | 5s | 是 | me-sync |
| `calendar_delete` | calendar | 写（待确认） | **否** | 5s | 是 | me-sync |
| `memory_upsert` | 横切 | 写 | 是 | 5s | 是 | me-sync |
| `music_search` | music | 读 | 是 | 2s | 是 | me-app |
| `music_play` | music | 写 | **否** | 3s | 是 | me-app |
| `music_control` | music | 写 | **否** | 3s | 是 | me-app |
| `music_state` | music | 读 | 是 | 3s | 是 | me-app |

### 4.2 `AiToolPermissions`：4 → 9（扁平扩列，键名不变）

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]                       // ← 关键：旧 JSON 缺新键时用 Default 补
pub struct AiToolPermissions {
    // 已有四键，名字一个字都不要改
    pub email_search: bool,
    pub weather: bool,
    pub calendar_create: bool,
    pub memory: bool,
    // 新增
    pub mail_read: bool,
    pub calendar_read: bool,
    pub calendar_update: bool,
    pub calendar_delete: bool,
    pub music: bool,
}

impl Default for AiToolPermissions {
    fn default() -> Self { /* 全 true，与现状一致 */ }
}
```

| 工具 | 读取哪个开关 |
| --- | --- |
| `email_search` | `email_search` |
| `mail_read` | `mail_read` |
| `weather_get` / `weather_locations` | `weather` |
| `calendar_list` | `calendar_read` |
| `calendar_create` | `calendar_create` |
| `calendar_update` | `calendar_update` |
| `calendar_delete` | `calendar_delete` |
| `memory_upsert` | `memory` |
| 四个 `music_*` | `music` |
| `plan_update` | **不受开关控制**（不触达用户数据） |

**为什么不改成嵌套 `AiModulePermissions`**：嵌套要手写 `Deserialize` 才能吃旧 JSON，
而旧键（`email_search` 等）名字本身已经够清楚；扁平扩列 + `#[serde(default)]` 零迁移成本，
UI 上按模块分组是渲染层的事，不该倒逼数据形状。

### 4.3 `AiToolTrace`：+2

```rust
pub struct AiToolTrace {
    pub call_id: String,
    pub tool: AiToolName,
    pub status: AiToolStatus,
    pub summary: String,
    pub duration_ms: u32,
    #[serde(default)]
    pub motive: Option<String>,          // 模型给的理由（截断 200）
    #[serde(default)]
    pub motive_source: MotiveSource,     // model | default
}
```

`ToolTraceRow`（`me-store/src/ai.rs:87`）加同名两列即可——它是 TEXT 存 JSON，
**不需要 schema 迁移**，只需要 `#[serde(default)]` 让旧行读得出来。

### 4.4 `Plan` / `PlanStep`：新增

```rust
pub struct Plan {
    pub id: String,                  // p_<yyyymmdd>_<n>，引擎生成
    pub goal: String,                // ≤120
    pub steps: Vec<PlanStep>,        // 1..=6
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct PlanStep {
    pub id: u8,
    pub capability: Capability,      // mail | weather | calendar | music
    pub title: String,               // ≤40
    pub status: PlanStatus,          // Pending/Running/Done/Failed/Skipped
    pub evidence: Option<String>,    // ≤120
}
```

校验函数放 `me-core`（纯函数、可单测）：

```rust
pub fn validate_plan(prev: Option<&Plan>, next: &Plan) -> Result<Plan, CoreError>
```

规则：步数 1–6；`capability` 合法；`id`/`title` 不可变；`status` 只能前进
（`Pending→Running→Done|Failed|Skipped`）；首次必须有 `goal`。

**运行期的 plan 放哪**：不要塞进 `ToolBudget`——Budget 是「本轮配额计数器」（`ai_tools.rs:28`），
plan 是「本轮产物」，混在一起会让「配额为 0」和「没有清单」这两种状态纠缠。
放在 `run_ai_stream` 的局部变量 `let mut plan: Option<Plan> = None;`，
`execute_ai_tool` 通过返回值（`AiToolResult` 里带 `plan_delta`）或一个专属 `&mut Option<Plan>`
参数交回。落库时从局部变量取。

### 4.5 事件与命令

```rust
// Event 新增
AiPlanUpdated { conversation, message, plan: Plan },
AiMusicCommand { conversation, call_id: String, op: MusicOp },
AiToolCalendarDeletePending { conversation, call_id, event: CalendarEventId, title: String, start_utc: i64 },

// Event 扩字段（不改结构，只加字段）
AiToolStarted { conversation, call_id, tool, motive: String },          // ← T3 依赖
AiToolCompleted { conversation, call_id, tool, status, summary, duration_ms,
                  motive: String, motive_source: MotiveSource },        // ← 活动条要用

// Command 新增
AiMusicResult { call_id: String, outcome: MusicOutcome },
RestoreCalendarEvent { id: CalendarEventId },                            // ← T12b 撤销删除

// Event 改造（破坏性，但只在进程内）
AiToolEmailResults {
    conversation,
    call_id,
    cards: Vec<MailCardRef>,   // { message: MessageId, reason: String }
    total: u32,
},
```

> **`AiToolStarted` 为什么也要带 motive**：活动条在工具执行期间就要显示「为什么调它」，
> 只在 `Completed` 上带会变成「做完才解释」。两处都带，UI 用 `Started` 建条目、`Completed` 更新状态。

三个跨层 / 新类型的落点（都在 `me-core`，否则 `me-sync` 与 `me-app` 无法共享）：

| 类型 | 放哪 | 为什么 |
| --- | --- | --- |
| `MailCardRef` | `me-core/src/ai.rs` | 事件载荷，和 `AiCard`（UI 侧）不是同一个东西 |
| `MusicOp` / `MusicOutcome` | `me-core/src/music.rs` | 已有 `MusicQueue` / `MusicOrder`，属同一领域 |
| `MotiveSource` | `me-core/src/ai.rs` | 跟 `AiToolTrace` 放一起 |

`reason` 由 `tool_email_search` 依据「命中了哪个字段」生成（主题命中 / 发件人命中 / 正文片段命中），
不额外调模型。

### 4.6 落库

```sql
-- schema v5，单事务
ALTER TABLE ai_messages ADD COLUMN plan TEXT;   -- JSON，NULL = 本轮没有清单
```

`AiMessage` 加 `pub plan: Option<Plan>`；`AiRepo::finish_assistant_message` 增加一个
`plan: Option<&Plan>` 参数（只有 `me-store` 一个实现，改签名成本很低）。

---

## 5. 分阶段任务

> 每个任务都写成「改哪里 / 怎么改 / 验收 / 坑」。按顺序做，前一个不稳不要开下一个。

### M0 — 修陷阱（T1）

**T1 档案解析失败不得静默清空**

- 改哪里：`me-sync/src/ai.rs:238-242`；`me-core/src/ai.rs:1106`、`:264`（`AiModel`）。
- 怎么改：
  1. `AiModel` 与 `AiToolPermissions` 都加 `#[serde(default)]`；
  2. 把 `serde_json::from_str(raw).ok()` 改成显式 `match`：解析失败时
     `tracing::error!` + 发一条 `Event::Error { scope: ErrorScope::Global, message }`
     （现有变体，`command.rs:853`），文案「AI 模型档案读取失败，原数据未改动」，
     **不要**走 `models.is_empty()` 的默认档案分支；
  3. 只有 `settings_key::AI_MODELS` 为 `None`（首次启动）才允许建默认档案。
- 验收：`me-store` 里手工塞一段非法 JSON 到 `meta.ai_models`，启动后：
  ① 不生成默认档案；② 界面出现「档案读取失败，原数据未改动」；③ 数据库原值未被覆盖。
- 坑：现在这条路径同时承担「v1 老配置迁移」（`legacy_config`），改的时候别把迁移分支一起删了。

### M1 — 契约层（T2～T5）

**T2 `motive` 字段与降级**

- 改哪里：`me-core/src/ai.rs` 的 `TOOL_SPECS`（每个工具的 `input_schema`）；
  `me-sync/src/ai_tools.rs:115`（`execute_ai_tool` 入口）；`me-store/src/ai.rs:87`。
- 怎么改：在每个 `input_schema.properties` 的第一位加 `motive`，`required` 首位加 `"motive"`；
  执行入口读它，空/缺失时填「（模型未说明动机）」并置 `motive_source = Default`；
  超长截断 200 字符；写进 `AiToolTrace`。
- 验收：MockAiProvider 返回一个不带 `motive` 的调用 → 工具**照常执行**，
  trace 里 `motive_source = default`，且 `MAX_TOOL_CALLS` 没有被多吃一次。
- 坑：`required` 里加 `motive` 之后，个别模型会把 motive 塞成参数值的一部分（例如把检索词
  写进 motive 而 `query` 留空）。工具层要单独校验「业务必填参数」，不能因为 motive 存在就放行。

**T3 活动条显示动机**

- 改哪里：`me-core/src/command.rs`（`AiToolStarted` / `AiToolCompleted` 加字段）、
  `me-app/src/app.rs:1210`（`AiToolTrace` 构造）、`me-app/src/views/ai.rs:645`（`show_activity`）。
- 怎么改：
  1. **先扩事件**：两个事件都加 `motive`，`Completed` 再加 `motive_source`；
  2. `app.rs:1210` 构造 trace 时带上两者（这一步不做，UI 侧编译都过不了）；
  3. `show_activity` 每条第二行显示 `动机：…`；`motive_source == default` 时用灰色并加「（模型未说明）」。
- 验收：对话进行中（未落库、未刷新）就能看到动机——这是最关键的一条，落库后才显示等于没做。
- 坑：`AiToolStarted` 与 `Completed` 的 motive 必须同源（都取模型给的原文），不要一处用截断值、
  一处用默认值，否则同一行前后会跳变。

**T4 `[结果]` 渲染与兜底**

- 改哪里：`me-app/src/views/ai.rs:544`（`show_message`），在 `markdown_to_blocks` 之前。
- 怎么改：写一个纯函数
  `split_result_block(content: &str) -> Option<(&str, ResultBlock)>`，
  按「最后一个独占一行的 `[结果]`」切分；尾部四行按 `状态/做了什么/关键信息/未完成` 解析；
  渲染时用一个 `Frame::group` + 四行 label（不新增 `Block` 变体，不动 `richtext.rs`）。
  模型漏写时，用活动区已有的 `state.ai.activity`（流式期间就有，来自 `AiToolCompleted`）生成降级块
  （「已执行 N 次工具调用」+ 各自 summary），不要等落库后的 `message.tool_trace`。
- 验收：单元测试覆盖四种输入——正常四行、缺字段、完全没写、**流式半截**（`[结` / `[结果\n状态：`）；
  四种都能渲染且不 panic，半截时按普通文本显示。
- 坑（三个）：
  1. `[结果]` 可能出现在代码块里（用户在聊 Markdown），切分前要先排除 fenced code 区间；
  2. 流式期间内容每 50ms 变一次，`[结果]` 会出现 `[结`、`[结果` 这类不完整前缀——只认「完整一行」
     `[结果]`，前缀状态当普通文本；
  3. `blocks_cache` 的 key 是 `(message.id, content.len())`（`views/ai.rs:580`），切分后要缓存
     **正文部分**的 blocks，否则尾部每帧都变、缓存永远命中不了，长回复会明显卡。

**T5 系统提示改写**

- 改哪里：`me-core/src/ai.rs:1138`（`SYSTEM_PROMPT`）。
- 怎么改：加三条（先复述工具结果、`[结果]` 格式、清单判定标准），并**保留原有安全条款**。
  第 6 条「不能修改/删除已有日程」在 M3 才改，M1 不要动（此时还没有那些工具，改了反而矛盾）。
- 验收：随便问一句「帮我删掉周五的会」，模型应回答「没有这个能力」，而不是声称已删。

### M2 — 清单层（T6～T9）

**T6 计划数据结构与校验**

- 改哪里：新建 `me-core/src/ai_plan.rs`（`lib.rs` 加 `pub mod ai_plan;` 与再导出）。
- 怎么改：按 §4.4 落 `Plan` / `PlanStep` / `Capability` / `PlanStatus` / `validate_plan`。
  全部 `Serialize + Deserialize`，字段名用 `snake_case`。
- 验收：单测覆盖 8 条规则（步数 0/1/6/7、非法 capability、改 title、status 回退、缺 goal、正常）。

**T7 `plan_update` 工具与循环钩子**

- 改哪里：`me-core`（枚举 + spec）、`me-sync/src/ai_tools.rs`、`me-sync/src/ai.rs:1101/1147/1152`。
- 怎么改：
  1. `execute_ai_tool` 加分支，执行 `validate_plan`，把结果通过专属 `&mut Option<Plan>` 参数交回
     `run_ai_stream` 的局部 `plan`（**不要放 `ToolBudget`**，见 §4.4），返回一句摘要；
  2. 循环里 `if call.name == PlanUpdate { 不计 executed_calls }`；
  3. `if turn.tool_calls.is_empty()` 时若 `plan` 还有 pending/running，追加一条 user 提醒并
     `continue`（不新开请求，直接改 `request.messages`）；
  4. 每次 plan 变化 `emit(Event::AiPlanUpdated)`。
- 验收：MockAiProvider 第一轮返回 `plan_update`，第二轮返回 `email_search`，第三轮空文本 →
  用户看到 3 步清单，且第二轮没有被「空 tool_calls」提前打断。
- 坑：续跑必须设上限——`plan` 有 pending 但模型连续两轮不调工具时，要强制收尾，
  否则会把 6 轮全烧在「催它干活」上。

**T8 schema v5 与落库**

- 改哪里：`me-store/src/migrations.rs`（新增 `migrate_v4_to_v5`，在 `migrate_v3_to_v4:585` 之后）、
  `me-core/src/lib.rs:71`、`me-store/src/ai.rs`。
- 怎么改：
  1. `ALTER TABLE ai_messages ADD COLUMN plan TEXT;`，`SCHEMA_VERSION = 5`，
     `AiMessage.plan`、`INSERT/SELECT` 同步；`finish_assistant_message` 增参；
  2. **取消与崩溃也要落 plan**（设计文档 §8.4 的要求，容易被漏）：取消时当前 `Running` 步改
     `Skipped`、其余保持 `Pending`；`fail_orphan_streaming_messages` 那条启动清理路径上，
     把 `streaming` 行的 plan 里所有非终态步骤改 `Skipped`，并保留已完成的 `evidence`；
  3. 这两条都要在 `finish_assistant_message` 之前算出最终 `Plan`，不能只写流式时的内存值。
- 验收：用 v4 的库启动 → 迁移成功 → 老消息 `plan = NULL` 且能正常渲染；
  中途点「停止」→ 该消息 plan 里当前步为 `Skipped`、前面已完成的步 `evidence` 还在；
  迁移后再回退程序版本 → 触发「版本高于程序」保护而不是崩。
- 坑：迁移函数必须只做「纯加列」，不要顺手改 CHECK / NOT NULL（`migrations.rs` 头注释的既定约束）；
  启动清理那条路径在 `me-store/src/ai.rs`（`fail_orphan_streaming_messages`），
  它只拿到 SQL，所以「把步骤改 Skipped」要么在 SQL 里改 JSON（不可行），要么在 Rust 侧读出来改再写回。

**T9 UI 清单条**

- 改哪里：`me-app/src/views/ai.rs`（新 `show_plan`，插在 `show_message` 上方）、`src/state.rs`。
- 怎么改：`AiPageState` 加 `plan: Option<Plan>`；事件 `AiPlanUpdated` 时覆盖；
  渲染 `①②③ + ○◐●✕ + 标题 + 回执`，完成后折叠成「已完成 4 步 · 1 步未完成」。
- 验收：跨模块需求（邮件 + 日历）能看到逐步打勾；中途某步失败时后续标 `skipped`。

### M3 — 能力补齐（T10～T14，含 T12b）

**T10 权限扩列 + 重放白名单**

- 改哪里：`me-core/src/ai.rs:1106/1127/583`、`me-app/src/views/ai.rs:1030-1033`。
- 怎么改：
  1. 按 §4.2 加 5 个字段与 `enabled` 分支（`enabled` 是穷尽 match，漏了编译不过）；
  2. **把 `is_replay_safe` 改成正向白名单**（`:583`）：只读工具 {`email_search`, `mail_read`,
     `weather_get`, `weather_locations`, `calendar_list`, `music_search`, `music_state`,
     `plan_update`, `memory_upsert`} → `true`，其余 `false`。理由见 §4.1 的表；
  3. 设置页按「邮件 / 天气 / 日历 / 音乐 / 记忆」分组，组内是动作级开关，
     组标题带「全开 / 全关」小按钮（作用于该组所有 bool）。
- 验收：① 关掉「日历 · 删除」后请求里没有 `calendar_delete`，硬调也被拒（执行期校验已有）；
  ② 单测断言 `is_replay_safe` 对 14 个工具逐个成立，且 `calendar_delete` / `music_play` 为 `false`。
- 坑：`AiModel` 的 `Debug` 是手写的（为了脱敏 key），加字段时记得在 `Debug` 里带上新开关，
  否则排障时看不到权限状态。

**T11 `mail_read`**

- 改哪里：`me-core`（枚举 + spec）、`me-sync/src/ai_tools.rs`（`ToolBudget` + 工具函数）、
  `me-sync/src/ai.rs`（把 refs 传下来）。
- 怎么改：
  1. **先把引用传进来**：`execute_ai_tool(conversation, call, budget)` 现在拿不到本次请求的
     `AiContextRef`。`start_ai_turn` 本来就有 `refs`，加一路参数到 `run_ai_stream`，
     再放进 `ToolBudget.context_refs: Vec<AiContextRef>`（惰性克隆，或只留 `Vec<MessageId>` 更省）；
  2. 入参 `message_id`；若该 id 已在 `context_refs` 里 → 直接返回「该邮件已在上下文中」，不附正文；
  3. 否则取本地正文，未缓存返回**可读提示**（不是 `is_error`）；截断到 `MAX_BODY_CHARS`；
     每条消息最多 3 次。
- 验收：引用同一封再问「总结一下」→ 工具回「已在上下文中」，`MAX_TOOL_CALLS` 只花 1 次；
  未缓存邮件 → 回复里出现「请先打开这封邮件」，且没有二次重试。
- 坑：只传 `Vec<MessageId>` 就够做去重，别把整个 `AiContextRef`（含正文快照）搬进 Budget——
  那是几千字符的克隆，每轮都复制一次不划算。

**T12 日历三工具 + 删除确认**

- 改哪里：`me-sync/src/ai_tools.rs`（三个壳）、`me-core/src/ai.rs`（枚举）、`me-core/src/command.rs`
  （待确认事件）、`me-app/src/views/ai.rs`、`me-app/src/state.rs`。
- 怎么改：
  - `calendar_list`：时间窗 + 关键词，上限 20 条，只回摘要；
  - `calendar_update`：`event_id` + 变更字段，调 `update_event`，回执「改了哪几项」；
  - `calendar_delete`：**不直接删**，返回 `pending_confirmation` + 事件 `AiToolCalendarDeletePending`；
  - UI：待删卡片 + 「确认删除 / 取消」→ 确认后发 `Command::DeleteCalendarEvent { id }`（已有），
    取消则把该 plan step 标 `skipped`。
- 验收：删一条 → 出现待删卡片 → 取消 → 事件仍在；再删 → 确认 → 事件消失且**出现 10 秒撤销条**
  （撤销依赖 T12b，没做 T12b 就把这条验收改成「事件消失，暂不提供撤销」）。
- 坑：`update_event` / `delete_event` 的 `Actor` 参数目前被忽略（`_actor`）。如果要做
  「AI 只能改自己创建的事件」，得先让 `Actor` 真正参与判断，别在工具层重复实现一套。

**T12b 删除撤销（与「增」对称）**

> 设计文档 §3.3 要求确认之后再给一步撤销。现在的代码只有 `soft_delete_event`
> （`calendars.rs:452`，写 `deleted_at`），**没有恢复路径**——不补这个任务，T12 的验收就自相矛盾。

- 改哪里：`me-core/src/port.rs`（`CalendarRepo` 端口，`soft_delete_event` 在 `:769`）、
  `me-store/src/calendars.rs`（实现）、`me-core/src/command.rs`、`me-sync/src/calendar.rs`、
  `me-sync/src/lib.rs`（分发）、`me-app/src/views/ai.rs`。
- 怎么改：
  1. `restore_event(id)`：`UPDATE calendar_events SET deleted_at = NULL, updated_at = ? WHERE id = ?`，
     返回 `CalendarEvent`（不存在或未删除时报错）；同步补端口方法（`CalendarRepo`）；
  2. `Command::RestoreCalendarEvent { id }` → 用例 `restore_event` 发 `CalendarEventsChanged`；
  3. UI：删除确认成功后显示「已删除「标题」· 撤销」（10 秒后淡出，与 AI 建日程的撤销条同形态）；
  4. 只允许撤销**刚刚删除**的那一条（UI 侧持有 event id + 时间戳，不做通用回收站）。
- 验收：删 → 撤销 → 事件回到原日历且内容一致；超过 10 秒撤销条消失；撤销已恢复的事件返回
  「事件不存在或未被删除」，UI 提示而不是 panic。
- 坑：`restore_event` 只动 `deleted_at` 与 `updated_at` 两个时间戳，别顺手改 `status` / `sequence` /
  `summary`——那会把「恢复」变成「修改」，与 `soft_delete_event` 的不动声色不对称。

**T13 卡片：原地弹 + 理由 + 上限**

- 改哪里：`me-app/src/app.rs:1504`、`src/views/ai.rs:663`、`me-sync/src/ai_tools.rs`（生成 reason）。
- 怎么改：
  1. `UiState` 加 `reader_open: bool`；`app.rs:1504` 的条件改成
     `if state.reader_open && state.selected_message.is_some()`。**不要**只放宽成
     `selected_message.is_some()` —— `switch_page`（`views/mod.rs:243`）不清 `selected_message`，
     那样阅读窗会跟着用户跳到每个页面。邮件列表点开与卡片点开都置 `reader_open = true`，
     关窗时同时置 `false` 并清 `selected_message`；
  2. `AiCard` 加 `reason: String`；`show_cards` 里直接展示，不再切页；
  3. 超过 5 张折叠为「还有 N 封，去列表看」。
- 验收：在 AI 页点卡片 → 阅读窗在 AI 页上弹出，返回后仍停在 AI 页；关掉重进不残留。
- 坑：`reader::show_window` 依赖 `state.messages` 里有该 id（`reader.rs:24-28`），
  AI 检索结果可能不在当前列表页——`show_cards` 点开前要先把摘要塞进 `state.messages`
  （`get_message_readonly` 已经存在，直接用）。

**T14 天气地点列表**

- 改哪里：`me-core`（枚举 + spec）、`me-sync/src/ai_tools.rs`。
- 怎么改：`weather_locations` 返回 `{ id, title, is_default }`，不触发网络。
- 验收：问「我关注了哪些城市」→ 结果与天气页侧栏一致。

### M4 — 音乐（T15）

**T15 四个音乐工具 + app 侧执行器**

- 改哪里：`me-core/src/music.rs`（`MusicOp` / `MusicOutcome`）、新建 `me-sync/src/ai_music.rs`、
  `me-sync/src/lib.rs`（命令分发）、`me-app/src/app.rs`（事件处理 + 跨帧状态机 + **播放器按需启动**）、
  `me-app/src/state.rs`、`me-app/src/views/music.rs`。
- 怎么改：
  0. **先解决播放器没起来的问题**（否则后面全白做）：`app.rs:443` 把启动条件写死成
     `page == Page::Music && player.is_none()`，所以停在 AI 页时 `player == None`、
     `state.music.tracks` 为空。要在收到 `AiMusicCommand` 且 `player.is_none()` 时执行
     `AudioPlayer::start()` + `LoadDirectory(dir)`，并**等 `AudioEvent::Playlist` 到齐**再执行工具，
     超时（3s）返回「媒体库还在扫描」；
  1. `me-sync`：`pending: Mutex<HashMap<String, oneshot::Sender<MusicOutcome>>>`；
     发 `Event::AiMusicCommand`，`tokio::time::timeout(3s)` 等结果；超时/取消都清 pending；
  2. `me-app`：收到事件后存进 `UiState.pending_music`（`call_id` + 目标状态 + 截止时间），
     每帧检查 `AudioState` 是否达到目标，达到或超时就发 `Command::AiMusicResult`；
  3. `views/music.rs` 抽出 `find_tracks(&[Track], &str) -> Vec<usize>`（按 `title` 匹配，
     大小写不敏感），`music_search` 与 UI 搜索框共用；
  4. 摘要文案在 `me-sync` 生成，app 只回结构化字段。
- 验收：五条路径都能给人话——① 没设音乐目录 ② 设了但还在扫描 ③ 设备不可用
  ④ 查询多命中 ⑤ 查询零命中；其中 ①② 的文案必须不同（「还没选音乐目录」vs「正在扫描媒体库」），
  否则用户会去反复重选目录。播放中点「停止」→ 不留 pending、不产生迟到的「成功」回执。
- 坑（两个）：
  1. 这是**唯一**一处工具执行不在 `me-sync` 的地方。UI 是 immediate 模式，没有后台任务，
     `pending_music` 必须按帧轮询，不能阻塞等 `oneshot`；
  2. 播放器启动是惰性的，别把启动逻辑复制一份到 AI 侧——抽成
     `App::ensure_player(&mut self)`，音乐页与 AI 执行器都调它，否则两处条件会越改越不一致。

---

### 收尾：每个阶段都要做（T16）

**T16 同步项目约定文档**

- `CLAUDE.md`：第 136 行的「AI 页」约定要加清单面板与删除确认；第 171 行的测试表要补新用例；
  新增一条不变量「工具结果只回摘要，音乐模块由 app 代执行但摘要仍在 me-sync 生成」。
- `doc/v03架构设计.md` §5.4：工具表从 4 行扩到 14 行，或改成「详见 AI模块设计.md」并保留指针。
- 每个阶段的 PR 描述里贴一次 §9 的 DoD 勾选，避免「代码合了、文档没动」。

---

## 6. 迁移与兼容

| 对象 | 变更 | 兼容策略 | 失败表现 |
| --- | --- | --- | --- |
| `ai_messages` | +`plan TEXT` | 纯加列，单事务 | 迁移失败不写 `schema_version`，启动中止 |
| `meta.ai_models` | 结构加 5 个 bool | `#[serde(default)]` + 键名不变 | 解析失败 → M0 的显式报错（修复前是静默清空） |
| `ai_messages.tool_trace` | 加 `motive` / `motive_source` | TEXT JSON + `#[serde(default)]` | 旧行照常读 |
| `AiToolEmailResults` | 字段改造 | 进程内事件，无持久化 | 无 |
| `AiToolStarted` / `AiToolCompleted` | 加 `motive` / `motive_source` | 进程内事件，无持久化；但 `app.rs:1210` 的构造必须同步改 | 漏改 = 编译失败（安全） |
| 新工具名 | 4 → 14 | 磁盘 trace 里出现新名字时，旧程序 `map_tool_name` 返回 `None` 丢弃该条 | 只影响回退版本的历史展示 |

**降级顺序**：v5 库 + v4 程序 → 已有「版本高于程序」保护，直接拒绝启动（`migrations.rs:425`）。
这条不用新增，但要写进发布说明。

---

## 7. 测试计划

| 文件 | 新增用例 | 断言要点 |
| --- | --- | --- |
| `me-core`（单元） | `validate_plan_*` 8 条 | 步数边界、非法 capability、id/title 不可变、status 不回退 |
| `me-core`（单元） | `replay_safe_covers_all_tools` | 遍历 14 个工具逐个断言；`calendar_delete` / `music_play` 必须为 `false` |
| `me-app`（单元） | `parse_result_block_*` | 正常四行 / 缺字段 / 代码块里的假 `[结果]` / 流式半截 `[结` |
| `me-sync/tests/ai_flow_mock.rs` | `plan_updates_drive_the_loop` | 首轮 plan → 次轮工具 → 空轮续跑，清单状态正确 |
| 同上 | `missing_motive_still_executes` | 工具成功、trace `motive_source=default`、次数只花 1 |
| 同上 | `tool_events_carry_motive` | `AiToolStarted` 与 `AiToolCompleted` 都带非空 motive（T3 的前提） |
| 同上 | `mail_read_skips_already_referenced` | 已引用 → 返回「已在上下文中」，不重复计费 |
| 同上 | `music_timeout_returns_readable_error` | 3s 超时 → 工具错误含工具名，整轮不失败 |
| 同上 | `regenerate_excludes_all_write_tools` | 重新生成只下发 replay-safe（含 `calendar_delete`、音乐两个写工具） |
| `me-store/tests/ai_store.rs` | `migrate_v4_to_v5_keeps_old_messages` | 迁移后老消息 `plan` 为 NULL 且可读 |
| `me-sync/tests/ai_flow_mock.rs` | `old_profile_json_keeps_disabled_tool_off` | **只有 4 个键的旧档案能读出来，且原本关掉的工具仍是关的**（R1 的核心回归；解析在 `ai.rs:238`，所以要起一次服务才测得到） |
| `me-store/tests/calendar_store.rs` | `restore_event_roundtrip` | 软删 → 恢复 → 字段一致；重复恢复报错 |
| `me-app`（单元） | `find_tracks_matches_title` | 大小写、部分匹配、空结果 |

手工验收（每阶段一次）：见 §9。

---

## 8. 风险与「别做」

| # | 风险 | 处理 |
| --- | --- | --- |
| R1 | 权限结构迁移清空用户档案 | T1 先修；T10 加 `#[serde(default)]`；发布前用真实 v4 库跑一遍 |
| R2 | 清单吃光预算 | `plan_update` 不计次数；空 `tool_calls` 续跑要设次数上限（T7 坑） |
| R3 | 音乐跨层回执泄漏 | 会话结束/落库时清 `pending`；迟到的 `AiMusicResult` 直接丢弃 |
| R4 | `[结果]` 切分误伤代码块 | 切分前排除 fenced code 区间（T4 坑） |
| R5 | 删除确认后模型自由发挥 | 确认走 UI 直发 `Command::DeleteCalendarEvent`，不重跑模型 |
| R6 | 卡片原地弹后阅读窗跨页残留 | 用独立的 `reader_open` 开关（不改 `page` 判断），关闭时同时清 `selected_message` |
| R7 | **音乐工具在 AI 页永远失败** | 播放器惰性启动（`app.rs:443`）；T15 的第 0 步必须先做，且抽成 `ensure_player` 供两处复用 |
| R8 | **动机只在落库后才出现** | 扩 `AiToolStarted`/`AiToolCompleted`；不给事件加字段，T3 就是个假完成 |
| R9 | 重放把写操作重做一遍 | `is_replay_safe` 改正向白名单（T10），并有逐工具的单元测试兜住 |

**不要做**（每一条都试过会更麻烦）：

1. 不要为清单建 `ai_plans` / `ai_plan_steps` 两张表——只加一列就够，等真有统计需求再拆。
2. 不要给 `AudioCommand` 加「播放第 N 首」以外的变体——`Play { index, path }` 已经够。
3. 不要在 `richtext.rs` 里加 `[结果]` 的 `Block` 变体——那会把「协议」混进「渲染器」。
4. 不要在工具层重复实现「只能改 AI 创建的事件」——先让 `Actor` 真正生效。
5. 不要把 `motive` 当权限校验——它不是安全边界，是给人看的说明。

---

## 9. 验收清单（每阶段 DoD）

**M0**
- [ ] 塞非法 `ai_models` JSON → 不生成默认档案、有错误提示、原值未被覆盖。
- [ ] `cargo test --workspace` 全绿，`clippy -D warnings` 无新增。

**M1**
- [ ] 工具执行**进行中**（未落库）活动条就能看到「动机」。
- [ ] 不带 `motive` 的调用照常成功且标为默认动机。
- [ ] 带工具的回复末尾有 `[结果]` 四段；漏写时有降级块；流式到一半不闪错。
- [ ] 问「删掉周五的会」→ 模型明确说没有该能力。

**M2**
- [ ] 跨模块需求（邮件 + 日历）跑完，清单逐步打勾。
- [ ] v4 库可迁移到 v5；老消息能读。
- [ ] 中途点「停止」→ 当前步 `Skipped`，已完成步的 `evidence` 还在。
- [ ] 6 轮 12 次下，三模块三步需求不超预算。

**M3**
- [ ] 四个模块的单模块任务全部走通。
- [ ] 关掉任一模块后，请求里无该模块工具，硬调被拒。
- [ ] 卡片在 AI 页原地弹出阅读窗，返回后仍在 AI 页。
- [ ] 删除需二次确认；确认后能撤销；取消后事件仍在。
- [ ] 「重新生成」不会下发 `calendar_delete` / `calendar_update`。

**M4**
- [ ] **从没进过音乐页**，直接在 AI 页说「放首歌」也能播（播放器按需启动）。
- [ ] 五种情况都有人话：没设目录 / 扫描中 / 设备不可用 / 多命中 / 零命中。
- [ ] 播放中「停止」不留 pending，不出现迟到回执。
- [ ] 重新生成不会切歌。

---

## 附录：与设计文档的对应关系

| 设计文档 | 本文 |
| --- | --- |
| §2.2 音乐执行层例外 | §5 T15、§8 R3 / R7 |
| §3.1 邮件（含 `mail_read` 三规则） | §5 T11 |
| §3.3 日历（含删除确认与撤销） | §5 T12、T12b |
| §3.4 音乐四工具 | §5 T15、§8 R7 |
| §3.5 记忆与实体上限 | 未在本轮（见设计文档 §12.2 L4，属独立议题） |
| §4 任务清单 | §4.4、§5 T6–T9 |
| §5 动机与结果 | §4.3、§4.5、§5 T2–T4 |
| §6 卡片 | §5 T13 |
| §7 权限模型 | §4.2、§5 T10 |
| §8.1 循环 / §8.4 取消与落库 | §5 T7、T8 |
| §10 现状映射 | §2 |
| §11 路线 | §1、§5 |

---

## 附录 E：落地状态（全量完成）

M0–M4 的代码与测试已全部落地：`cargo test --workspace` 546 passed / 0 failed，
`cargo fmt --check` 与 `cargo clippy --workspace --all-targets -- -D warnings` 均通过。

| 任务 | 状态 | 落点 |
| --- | --- | --- |
| T1 修陷阱 | 完成 | `AiToolPermissions` 容器级 `serde(default)`、档案解析失败不再静默回落 |
| T2 `motive` | 完成 | `TOOL_SPECS` 统一注入 + 执行期降级；`AiToolTrace.motive(_source)`；trace 落库两列 |
| T3 活动条动机 | 完成 | `AiToolStarted/Completed` 带同源动机；`show_activity` 第二行显示 |
| T4 `[结果]` | 完成 | `split_result_block`（排 fenced code、只认整行、流式半截按正文）+ 活动区兜底 |
| T5 系统提示 | 完成 | 规则 8–12（复述结果 / `[结果]` / 清单 / `mail_read` / 音乐） |
| T6 清单结构 | 完成 | `me-core/src/ai_plan.rs`：`Plan`/`PlanStep`/`validate_plan`（按 id 合并、状态只前进） |
| T7 `plan_update` | 完成 | 工具 + 循环钩子：不计预算、变更即发 `AiPlanUpdated`、空 `tool_calls` 时催办（上限 2 次） |
| T8 schema v5 | 完成 | `ai_messages.plan` 加列；取消/崩溃两条路径都把未完成步标 `Skipped`（Rust 侧读改写） |
| T9 清单 UI | 完成 | 底部清单条 `show_plan`：`○◐●✕—` + 回执；结束后折叠 |
| T10 权限 4→9 | 完成 | 扁平扩列（邮件 2 / 天气 / 日历 4 / 音乐 / 记忆）+ 正向重放白名单 + 设置页两行分组 |
| T11 `mail_read` | 完成 | 只读缓存正文、`context_refs` 命中即短路、未缓存给可读提示、每条消息最多 3 次 |
| T12 日历三工具 | 完成 | `calendar_list`/`calendar_update`/`calendar_delete`；删除只发确认 |
| T12b 撤销删除 | 完成 | `CalendarRepo::restore_event` + `Command::RestoreCalendarEvent` + 10 秒撤销条 |
| T13 卡片 | 完成 | `UiState::reader_open` 原地弹窗、`reason` 命中理由、最多 5 张 + 「还有 N 封」 |
| T14 天气地点 | 完成 | `weather_locations` 只读本地配置、不联网 |
| T15 音乐四工具 | 完成 | `MusicOp`/`MusicOutcome` + `Event::AiMusicCommand`/`Command::AiMusicResult` + `App::ensure_player` 按需启动 + 逐帧状态机；五条失败路径文案各不相同 |
| T16 文档同步 | 完成 | 本文件 + `CLAUDE.md`（工具白名单、两张执行例外、AI 页三块内容、测试表） |

### 与方案的偏差（都写进代码注释）

1. **`AiToolStarted` 不建活动条目**：实现上 `Completed` 一条成型（`Started` 在 UI 侧是空分支）。动机仍然两处同源，只是 UI 只用 `Completed` 的那份，避免同一行两帧跳变。
2. **设置页分组用两行紧凑排布**，没有做「全开/全关」小按钮：设置弹窗是固定高度的 `Modal`，五行分组会把下面的预算/删除挤出可视区（已有回归测试断言这些字段可见）。
3. **`calendar_list` 不做重复展开**：按「主行」返回（与日历页可视查询同源），因此重复日程只出现一条并标注「· 重复」。够模型拿到 `event_id`，不再引入一次 `expand_event`。
4. **删除确认卡不驱动 plan step**：取消删除时只丢卡片；把清单某一步改成 `skipped` 需要 `call_id → step` 的映射，而清单是按步不按调用组织的，硬映射会误伤。

### 仍需人工验证（自动化测试覆盖不到的）

- 真模型下跨模块需求（邮件 + 日历）跑完，清单逐步打勾；
- AI 页点卡片 → 阅读窗原地弹出，关掉后仍在 AI 页；
- 从没进过音乐页时说「放首歌」能播（惰性启动）；
- 弹窗尺寸在 1280×900 以外分辨率下的观感。
