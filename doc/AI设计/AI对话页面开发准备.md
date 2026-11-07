# AI 对话页面重构 · 开发准备

> 对象：[AI对话页面优化.md](AI对话页面优化.md)（1,247 行，P0/P1/P2 三期）
> 本文件只做一件事：**在动手改代码前，把方案里的每条假设与当前代码对一遍**，
> 把会让人中途卡住的地方提前标出来。

---

## 0. 结论

**可以开工，但必须先改掉 11 处实现手法。** 方案的设计判断是对的——四区块 `TurnView`、
思考分段存储、`LiveTool` 不污染持久化枚举、`TurnCtx` 绕开借用冲突，这些我都逐条核对过，
落在代码上成立。

但方案里有 **1 处事实误判**（`show_cards` 的「打开邮件」被当作可用逻辑沿用，
实际支撑它的 `card_messages` 从未被写入）、**7 处遗漏**（user 消息的渲染落点、
`blocks_cache` 的真实类型、双通道 seq 撞号、`thinking` 列的位置、`on_stream_started`
的清理、`trimmed_turns` 的既有行为、assistant 分支里另外三样要搬的东西）、
**3 处引用需要精确化**（字面量数量、`AiToolTrace` 注释原文、`AiModel` 的字段数）。
这些不改，实现会在同一个地方反复卡：先是编译不过，改通了之后是行为不对。

一句话概括风险分布：**数据模型层（P0 前半）风险低，照抄即可；UI 层（P0 后半）风险高，
方案的伪代码不能直接落地，要按 §3 的手法改。**

---

## 1. 基线快照

```bash
git status --short          # 干净（上一提交 c4a259c 已收尾）
git log --oneline -1        # c4a259c docs: 修正 doc 目录重组后失效的相对链接与路径引用

cargo test --workspace      # 792 passed / 0 failed / 6 ignored
```

| 项 | 值 |
| --- | --- |
| schema 版本 | `SCHEMA_VERSION = 7`（[lib.rs:88](../../crates/me-core/src/lib.rs)） |
| 事件变体总数 | 69（本次只加字段，不变） |
| 相关文件规模 | `views/ai.rs` 2,201 行、`me-sync/src/ai.rs` 约 2,400 行 |

**验收方式**：每个 P0 子项完成后跑一次 `cargo test --workspace`，
数字必须 ≥ 792 + 本轮新增数，failed 恒为 0；`cargo fmt --check` 与
`cargo clippy --workspace --all-targets -- -D warnings` 全通过。

---

## 2. 事实核对：方案假设 vs 代码实测

### 2.1 通过（可以照抄，不用再想）

| # | 方案断言 | 实测 | 位置 |
| --- | --- | --- | --- |
| 1 | `AiDelta` 只有 `Text` / `Usage` | ✓ 完全一致 | [ai.rs:1191](../../crates/me-core/src/ai.rs) |
| 2 | `AiToolTrace` 有 `step_id` / `attempt` | ✓ 且都有 `#[serde(default)]` | [ai.rs:792](../../crates/me-core/src/ai.rs) |
| 3 | `AiMessage` 12 个字段，无 `thinking` | ✓ | [ai.rs:874](../../crates/me-core/src/ai.rs) |
| 4 | `AiMessageStatus` 四变体 | ✓ Streaming / Complete / Failed / Cancelled | [ai.rs:527](../../crates/me-core/src/ai.rs) |
| 5 | `AiToolStatus` 四变体、无 `Running` | ✓ Ok / Error / Cancelled / Skipped | [ai.rs:708](../../crates/me-core/src/ai.rs) |
| 6 | `AiStreamDelta` 无 `kind` | ✓ 只有 `conversation/message/seq/delta` | [command.rs:860](../../crates/me-core/src/command.rs) |
| 7 | `AiStreamDelta` 的解构都用 `..` | ✓ 3 处全部兼容加字段 | [app.rs:1547](../../crates/me-app/src/app.rs)、测试 2 处 |
| 8 | `AiToolCompleted` 不带 `step_id`/`attempt` | ✓，且 `app.rs` 硬写 `None` | [app.rs:1618](../../crates/me-app/src/app.rs) |
| 9 | `AiStreamStarted` 的 `message` 被 `..` 忽略 | ✓ 只用了 `conversation` / `trimmed_turns` | [app.rs:1531](../../crates/me-app/src/app.rs) |
| 10 | `list_messages_conn` 不按 `status` 过滤 | ✓ SQL 只有 `conversation_id` + `id <` | [ai.rs:251](../../crates/me-store/src/ai.rs) |
| 11 | `show_message` 不读 `message.plan` | ✓ 全文无 `message.plan` | [views/ai.rs:625](../../crates/me-app/src/views/ai.rs) |
| 12 | 渲染顺序为 正文→结果→工具→错误→tokens→复制 | ✓ 逐行核对一致 | 同上 |
| 13 | `show()` 布局：消息→plan→删除确认→流式→活动→卡片 | ✓ | [views/ai.rs:527](../../crates/me-app/src/views/ai.rs) |
| 14 | 流式用纯文本 `ui.label(&stream.buffer)` | ✓ 注释写明「每帧解析会让滚动卡顿」 | [views/ai.rs:728](../../crates/me-app/src/views/ai.rs) |
| 15 | `DeltaThrottle::take()` 返回 `(u64, String)` 不是 `Option` | ✓ | [ai.rs:194](../../crates/me-sync/src/ai.rs) |
| 16 | `DELTA_INTERVAL = 50ms`、`DELTA_BATCH_BYTES = 2048` | ✓ | [ai.rs:92](../../crates/me-sync/src/ai.rs) |
| 17 | `migrate_v4_to_v5` 的写法可照抄 | ✓ 方案给的 `migrate_v7_to_v8` 与它逐行同构 | [migrations.rs:723](../../crates/me-store/src/migrations.rs) |
| 18 | `finish_assistant_message` 在 `ai.rs:2220`（不在 port.rs） | ✓ | [ai.rs:2220](../../crates/me-core/src/ai.rs) |
| 19 | `AiProviderConfig` 只有 4 字段 | ✓ kind / base_url / model / max_output_tokens | [ai.rs:164](../../crates/me-core/src/ai.rs) |
| 20 | **A1 成立**：PLAN 阶段恒下发两个工具、不过权限过滤 | ✓ 注释原文「**不经 `AiToolPermissions` 过滤**」 | [ai.rs:2097](../../crates/me-core/src/ai.rs) |
| 21 | `apply_openai` 只读 `delta["content"]` | ✓ 不读 `reasoning_content` | [proto/ai.rs:526](../../crates/me-proto/src/ai.rs) |
| 22 | `apply_anthropic` 的 `thinking_delta` 落进 `_ => {}` | ✓ 只认 `text_delta` / `input_json_delta` | [proto/ai.rs:460](../../crates/me-proto/src/ai.rs) |
| 23 | `MAX_CARDS = 5` | ✓ | [views/ai.rs:1076](../../crates/me-app/src/views/ai.rs) |
| 24 | Markdown 缓存键是 `(message_id, 长度)` | ✓ 命中判断就是这两项 | [views/ai.rs:666](../../crates/me-app/src/views/ai.rs) |
| 25 | `fail_orphan_streaming_messages` 只改 plan 与 status，不清 content | ✓ 只 UPDATE 这两列 | [ai.rs:787](../../crates/me-store/src/ai.rs) |

第 25 条顺带说明：P1 做检查点时，写入的内容在孤儿收尾后**会保留**——正是想要的效果，
不需要额外处理。

### 2.2 偏差（必须按 §3 修正）

| # | 方案怎么说 | 实际是什么 | 后果 |
| --- | --- | --- | --- |
| E1 | `AiMessage` 字面量 11 处、`AiToolTrace` 9 处 | 实测 **10 处 / 8 处** | 数字差 1，不影响方案，但统计口径要改（见 §3.1） |
| E2 | `turn_from_message` 处理「一条 AI 回复」 | `messages` 里**混着 user 消息**，且 user 有 `context_refs` 要渲染 | `TurnView` 装不下，硬塞会丢「引用：xxx」 |
| E3 | `TurnCtx.blocks_cache: &mut Option<BlocksCache>` | 实际类型是 `Option<(AiMessageId, usize, Vec<Block>)>` | 直接抄编译不过 |
| E4 | 「seq 仍用一个计数器（放 sink 外层 AtomicU64）」 | `seq` 在 `DeltaThrottle` **内部**自增 | 拆双通道后两套 seq 从 0 起，UI 缺口检测误判 |
| E5 | 「`AiToolTrace` 的注释要改」 | 注释实际还写了「唯一的例外是 `motive`」，比方案引用的多半句 | 照方案替换会丢掉 `motive` 的说明 |
| E6 | 未提 | `on_stream_started` 会 `activity.clear()` + `cards.clear()` | 删 `cards` 时这里会编译错；`activity` 保留与否要定 |
| E7 | 未提 | `AiStreamStarted` 还带 `trimmed_turns`，app 用它弹 toast | 接 `message` 时别把 `trimmed_turns` 的处理弄丢 |
| E8 | 未提 | `show_message` 的 assistant 分支里，`[结果]` / tokens / 复制按钮与 `show_activity` **在同一次调用里串行** | 拆函数时容易漏搬，漏了就是功能丢失 |
| E9 | 未提 | `map_message` 用**硬编码列索引** `row.get(0..11)` | `thinking` 列插错位置会让所有字段错位 |
| E10 | 未提 | `AiModel` 除 4 个 config 字段外还有 `max_input_chars` | P2 加 `thinking_budget` 时别以为只有 4 个字段要搬 |
| E11 | 「卡片点击沿用现有 `show_cards` 的逻辑」 | `card_messages` **从未被写入**，`show_cards` 的「打开邮件」对不在当前列表的邮件是坏的 | 这是**现存 bug**，不是重构引入的，但卡片落库后会更常撞到。见 §3.11 |

### 2.3 方案没写、但实现时必须决定的

**① `AiToolStarted` 已经有 `motive` 了。**

方案说「`AiToolStarted` 被丢弃，改成写进 `live_tool`」，读起来像是要从头补动机。
实际事件定义里**早就有** `motive` + `motive_source`（[command.rs:746](../../crates/me-core/src/command.rs)），
app.rs 只是没接。所以 `LiveTool` 直接搬过来即可，**不需要给事件加字段**。

这算是个好消息：实时态显示动机的成本比方案预估的低。

**② `AiToolStatus` 的 `Skipped` 在工具区怎么显示。**

方案的工具区示例只画了 ✓ 和 ✗。`Skipped`（依赖前序步骤失败）需要一个第三种图标
（建议 `⊘`）与文案「已跳过」，否则会显示成成功。

**③ 流式期间的清单来源。**

`turn_from_stream` 的 `plan` 要取 `state.ai.plan`（来自 `AiPlanUpdated` 未落库的那份）。
但 `state.ai.plan` 是「当前会话的显示位」，切会话会被 `sync_plan_from_messages` 覆盖——
**流式中切换会话再切回来**，这份清单可能已经不是刚才那条消息的了。

`plans: HashMap<conversation, Plan>` 缓存正是为这个场景存在，取的时候优先用它：

```rust
let plan = state.plans.get(&conversation).cloned().or_else(|| state.plan.clone());
```

**④ 保留 `state.ai.activity` 还是删掉。**

方案 §7.1 写「删除 `state.ai.activity`」，但 `AiToolCompleted` 在流式期间**先于落库**到达，
那时消息还没写进库，没有 trace 可读。所以流式态的已完成 trace 仍然需要一个内存位。

**结论：`activity` 保留**，只删 `cards`（卡片改从 trace 读，且 `card_messages` 一并处理）。

---

## 3. 实现手法调整（照这个来，不要照方案里的伪代码）

### 3.1 🔧 字面量统计口径

方案给的 11 / 9 是用 `grep "AiMessage {"` 数的，会把 `NewAiMessage {` 一起数进去。
正确口径要加词边界：

```bash
grep -rnE "(^|[^A-Za-z_])AiMessage \{" crates/ --include=*.rs | grep -v "struct AiMessage"
# → 10 处
grep -rnE "(^|[^A-Za-z_])AiToolTrace \{" crates/ --include=*.rs | grep -v "struct AiToolTrace"
# → 8 处
```

分布（补字段时的清单）：

| 结构体 | 文件 | 处数 |
| --- | --- | --- |
| `AiMessage` | `me-core/src/ai.rs` | 5（含测试） |
| | `me-app/src/views/ai.rs` | 2（含 `user_message()` helper） |
| | `me-store/src/ai.rs` | 2 |
| | `me-sync/tests/ai_flow_mock.rs` | 1 |
| `AiToolTrace` | `me-sync/src/ai_tools.rs` | 4 |
| | `me-store/tests/ai_store.rs` | 2 |
| | `me-app/src/app.rs` | 1 |
| | `me-store/src/ai.rs` | 1（`ToolTraceRow` 是另一个类型，不算在内） |

**合计 18 处要手工补字段。** 这是排期里实打实的一块，不是「改完类型就好了」。

### 3.2 🔧 `TurnView` 装不下 user 消息 —— 必须保留双分支

方案的 `TurnView` 四个字段全是 assistant 的语义（思考 / 清单 / 工具 / 回答）。
但 `show_message` 现在**两个角色都处理**，user 分支还要渲染 `context_refs`：

```rust
// 现状（views/ai.rs:634-653）
if is_user {
    // 右对齐气泡 + message.content + context_refs 逐条「引用：主题（mode）」
}
```

`TurnView` 没有 `context_refs` 字段，硬塞进去会让 user 消息的引用快照消失。

**改法：`render_turn` 只负责 assistant，user 走独立函数，在循环里分流。**

```rust
// show() 里
for message in &messages {
    if message.status == AiMessageStatus::Streaming {
        continue;                       // 占位行交给末尾的流式那条
    }
    if message.role == AiRole::User {
        show_user_message(ui, message); // 原样搬过来，不动
        continue;
    }
    let turn = turn_from_message(message);
    render_turn(ui, &mut ctx, &turn);
}
```

这样 `TurnView` 的四个字段语义保持干净，不会被 user 消息的 `None` 污染。
**方案 §6.1 的「两个构造点」要改成三个：`turn_from_message` / `turn_from_stream` / `show_user_message`。**

### 3.3 🔧 `blocks_cache` 的真实类型

```rust
// 现状（views/ai.rs:198-199）
/// 已解析的 Markdown 缓存：`(message, 长度, blocks)`。
blocks_cache: Option<(AiMessageId, usize, Vec<Block>)>,
```

方案写的 `Option<BlocksCache>` 不存在。两条路，选**前者**：

```rust
// 路 A（推荐）：加一个类型别名，字段类型不变，零迁移
type BlocksCache = (AiMessageId, usize, Vec<Block>);

// 路 B：抽成结构体，要给现有 3 处使用点改名
```

`TurnCtx` 相应写成：

```rust
struct TurnCtx<'a> {
    blocks_cache: &'a mut Option<BlocksCache>,
    open_message: &'a mut Option<me_core::MessageId>,
    commands: &'a mut Vec<Command>,
}
```

注意：`blocks_cache` 与 `plans` 都是**私有字段**（无 `pub`），`TurnCtx` 放在同一模块内，
访问不受影响。如果将来把 `render_turn` 挪到子模块，需要先加 `pub(crate)`。

### 3.4 🔧 双通道 seq 会撞号 —— 要先把 seq 从 throttle 里剥出来

现状（[ai.rs:166-199](../../crates/me-sync/src/ai.rs)）：

```rust
struct DeltaThrottle {
    pending: String,
    total: String,
    seq: u64,        // ← 每个实例自己的计数器
    last: Instant,
}
fn take(&mut self) -> (u64, String) {
    self.seq = self.seq.saturating_add(1);   // ← 自增在这里
    ...
}
```

直接建两个 `DeltaThrottle`，两个都从 `seq = 0` 开始，就会发出
`(1,思考) (1,回答) (2,思考) (2,回答)…` 两套重复序号。UI 现在用 `stream.seq = seq` 记录最新值，
重复序号会让「有没有整段丢掉」的判断失效。

**必须先改 `DeltaThrottle`：**

```rust
struct DeltaThrottle {
    pending: String,
    total: String,
    last: Instant,
    // seq 移除
}

impl DeltaThrottle {
    /// 返回待发送的文本（不再带 seq）。
    fn push(&mut self, text: &str) -> Option<String> { .. }

    /// 有残余才取走；没有就不动时钟。切段时用。
    fn flush(&mut self) -> Option<String> {
        if self.pending.is_empty() { None } else { Some(std::mem::take(&mut self.pending)) }
    }
}
```

seq 提到 sink 外层，由两条通道共享：

```rust
let seq = Arc::new(AtomicU64::new(0));
// 每发一条：
let n = seq.fetch_add(1, Ordering::Relaxed) + 1;
```

**这是 P0 的前置改动**，不是「顺手优化」——不改就实现不出正确的双通道。

### 3.5 🔧 `AiToolTrace` 的注释要保留 `motive` 那半句

方案引用的原文是「不存参数、不存结果原文」，但实际注释还有下面这段
（[ai.rs:787-790](../../crates/me-core/src/ai.rs)）：

```text
/// 唯一的例外是 `motive`：它是「为什么调这个工具」的一句话说明，给用户看的，
/// 不是原始参数。它只进 `tool_trace` 与界面，**不进 tracing 日志**（否则用户问题会落盘）。
```

加 `cards` 之后要改成「两类例外」，别把 `motive` 那句删掉：

```rust
/// 工具调用摘要。**不存参数原文、不存邮件正文**。
///
/// 例外是三类给用户看的短文本：`motive`（为什么调）、`summary`（一句话结果）、
/// `cards[].reason`（命中理由，可能含用户自己的查询词）。
/// 它们只进 `tool_trace` 与界面，**不进 tracing 日志**。
```

### 3.6 🔧 assistant 分支里还有三样东西要搬

`show_message` 的 assistant 分支（[views/ai.rs:654-714](../../crates/me-app/src/views/ai.rs)）
不是只有「正文 + 结果块 + 工具活动」，还夹着：

| 内容 | 行号 | 搬迁目标 |
| --- | --- | --- |
| `[结果]` 段落 + 兜底块 | 662-688 | `show_answer` 内部末尾 |
| 错误 / 「已停止」 | 692-698 | `show_answer` 内部末尾 |
| token 用量 | 699-705 | `show_answer` 内部末尾 |
| 复制按钮 | 706-713 | `show_answer` 内部末尾 |

方案 §6.2「回答区」提了这几点，但没说明它们**原本和工具区在同一个闭包里串行**。
拆的时候按上表逐项对照，漏一项就是功能丢失（尤其 `show_result_fallback`，
只在「模型漏写 `[结果]` 且状态 Complete」时触发，很容易漏）。

### 3.7 🔧 `thinking` 列必须加在 `created_at` 之后

`map_message`（[ai.rs:63-86](../../crates/me-store/src/ai.rs)）用**硬编码索引**：

```rust
role: AiRole::parse(&row.get::<_, String>(2)?),
status: ... row.get(5)?,
tool_trace: parse_list(row.get(9)?),
plan: parse_plan(row.get(10)?),
created_at: row.get(11)?,
```

`MESSAGE_COLUMNS` 是位置敏感的字符串列表。**新列一律追加在 `created_at` 之后**：

```rust
const MESSAGE_COLUMNS: &str = "id, conversation_id, role, content, context_refs, status, error, \
     input_tokens, output_tokens, tool_trace, plan, created_at, thinking";
```

`map_message` 只需新增一行 `thinking: parse_thinking(row.get(12)?)`，既有索引一个都不用动。
DDL 里也放最后（方案 §3.4 已这样写，是对的）。

**反例**：如果把 `thinking` 插在 `plan` 和 `created_at` 之间，`created_at` 的索引要从 11 变成 12，
而 `created_at: row.get(11)?` 读出来是个 JSON 字符串，运行期才炸，且报错信息完全不指向真实原因。

### 3.8 🔧 `on_stream_started` 的两行清理要分别处理

```rust
// 现状（views/ai.rs:331-340）
pub fn on_stream_started(&mut self, conversation: AiConversationId) {
    self.stream = Some(AiStreamView { .. });
    self.activity.clear();
    self.cards.clear();     // ← cards 要删，这行也要删
}
```

`activity` **保留**（§2.3 ④ 的理由），`cards` 删除。`card_messages` 单独处理，见 §3.11 ——
**它不能跟着删**，而且现在是坏的。

### 3.9 🔧 `AiStreamView` 要接住 `message` id

`AiStreamStarted` 带了 `message`，app.rs 用 `..` 丢掉了。方案 §6.3 说它是「轮次键」，
用于 Markdown 缓存——但缓存键现在就是 `(AiMessageId, usize)`，所以**必须接住**：

```rust
Event::AiStreamStarted { conversation, message, trimmed_turns } => {
    if self.state.ai.selected == Some(conversation) {
        self.state.ai.on_stream_started(conversation, message);  // 加一个参数
        if trimmed_turns > 0 { /* 原有 toast，不能丢 */ }
    }
}
```

`on_stream_started` 的签名要加 `message: AiMessageId`，`AiStreamView` 加同名字段。

### 3.10 🔧 `thinking` 的 JSON 解析要容错

`parse_plan` 的先例（[ai.rs:88-100](../../crates/me-store/src/ai.rs)）是「坏 JSON 按没有处理」。
`parse_thinking` 照这个来，但**比它多一种情况**：列存在但不是对象（比如手工插了个字符串）。
两种都是「当作没有思考」，不能 `unwrap`。

```rust
fn parse_thinking(raw: Option<String>) -> Option<ThinkingBlock> {
    let raw = raw?;
    serde_json::from_str::<ThinkingBlock>(&raw).ok()   // 失败 = None，不报错
}
```

注意这里**不要**像 `parse_plan` 那样记日志：思考是展示项，坏了就当没有，
记日志反而会在每次打开会话时刷屏。

### 3.11 🔧 `card_messages` 从未被写入 —— 现存 bug，必须一起修

`card_messages` 在代码里只有三处出现，**没有一处写它**：

```text
views/ai.rs:162    pub card_messages: Vec<MessageSummary>,   ← 声明
views/ai.rs:297    self.card_messages.clear();                ← 切会话清空
views/reader.rs:32 state.ai.card_messages.iter().find(..)     ← 读
```

它的用途是给阅读窗兜底（[reader.rs:26-32](../../crates/me-app/src/views/reader.rs)）：

```rust
// 先找当前列表，再找 AI 卡片带出来的摘要：AI 页检索到的邮件可能不在
// 邮件列表里，但用户在 AI 页点「打开邮件」时照样要能读到。
let message = state.messages.iter().find(|m| m.id == id)
    .or_else(|| state.ai.card_messages.iter().find(|m| m.id == id))  // ← 永远命中不了
    .cloned();
```

`or_else` 分支永远拿不到东西，于是 `let Some(message) = message else { .. }` 立刻关闭阅读窗。

**触发条件**：AI 检索到的邮件不在当前 `state.messages` 里——比如用户在收件箱视图，
AI 检索到已归档或其他文件夹的邮件，点「打开邮件」不会有任何反应
（`Command::OpenMessage` 照样发出去加载正文，但窗口已经关了）。

**为什么这次必须修**：方案 P1 让卡片随 `tool_trace` 落库，历史回看时卡片会重新出现。
而历史回看时 `state.messages` 往往是当前文件夹的内容，与历史卡片对不上——
**卡片落库之后，这个 bug 的命中率会明显上升**。

修法很小，`AiToolEmailResults` 的处理里 `MessageSummary` 本来就在手边
（[app.rs:1636-1649](../../crates/me-app/src/app.rs) 已经 `get_message_readonly` 拿到它，
只是用完就丢了）：

```rust
// app.rs · Event::AiToolEmailResults
let mut built = Vec::new();
let mut summaries = Vec::new();
for card in cards {
    if let Ok(Some(message)) = self.repo.get_message_readonly(card.message) {
        summaries.push(message.clone());   // ← 新增：留给 reader 兜底
        built.push(views::ai::AiCard { /* 字段都从 message 来 */ });
    }
}
self.state.ai.cards = built;
// 事件每次带全量列表，所以是替换而不是追加。
self.state.ai.card_messages = summaries;
```

零额外 IO（`message` 已经读出来了），只是在丢掉之前多存一份。

> 注意这**不是**本次重构的范围扩大——它是「点卡片打开邮件」这个已有功能的修复。
> 但不修的话，P1 交付的「历史卡片」会是个点了没反应的死控件。

---

## 4. 实现顺序

方案分了 P0/P1/P2，但 P0 内部还有依赖顺序。按下面五批走，每批一次提交 + 全量门禁：

### 批次 1 · 数据模型（低风险，照抄方案）

1. `ThinkingSegment` / `ThinkingBlock` / 两个常量 → `me-core/src/ai.rs`
2. `AiMessage` 加 `thinking` → 补 **10 处**字面量
3. `AiToolTrace` 加 `cards` → 补 **8 处**字面量 + 改注释（§3.5）
4. `AiDelta` 加 `Thinking(String)`
5. schema v8：`MESSAGE_COLUMNS` 尾部加列（§3.7）、`map_message` 加一行、DDL、`migrate_v7_to_v8`
6. `finish_assistant_message` / `finish_assistant_tx` 加 `thinking` 参数

**验收**：`cargo test -p me-store` 绿；新增 4 条存储层测试（§6.2）；
编译通过即说明字面量补全了。

> 这一批改完，`thinking` 能存能读，但界面上还看不到——这是故意的，先把数据打通。

### 批次 2 · 协议层（只做 OpenAI 侧）

1. `TurnState` 加 `thinking: String`
2. `apply_openai` 认 `reasoning_content` / `reasoning`
3. `AiDelta::Thinking` 在 sink 里先当作「忽略」处理（避免批次 2 就改引擎）

**验收**：新增 4 条协议层测试（§6.1）。**不碰 Anthropic**——A1 已证明那条路现在走不通。

### 批次 3 · 引擎双通道（有技术债要先还）

1. **先改 `DeltaThrottle`**：剥掉 `seq`、加 `flush()`（§3.4）
2. sink 里两个 throttle + 共享 `AtomicU64`
3. 切段逻辑 + 刷残余顺序（方案 §5.2 的代码可直接用，但 `flush()` 签名按 §3.4 改）
4. `AiToolCompleted` 加 `step_id` / `attempt`，引擎填值
5. trace 带 `cards`
6. 落库时传 `thinking`

**验收**：新增 8 条引擎测试（§6.3），重点 `throttle_residue_is_flushed_before_segment_switch`。

### 批次 4 · UI 渲染器（风险最高，单独一批）

1. `BlocksCache` 类型别名 + `TurnCtx`（§3.3）
2. `turn_from_message` / `turn_from_stream` / `show_user_message`（§3.2）
3. `render_turn` + `show_thinking` + `show_tools`
4. `show_plan` 改为消息内渲染，删会话级调用位
5. 跳过 `Streaming` 占位行
6. `show()` 的六步借用顺序（方案 §6.1，顺序不能动）
7. 顺手修 `card_messages`（§3.11）——4 行代码，别留到 P1 才发现卡片点不开

**验收**：新增 8 条 UI 测试（§6.4）。重点 `streaming_placeholder_produces_single_bubble`
与 `failed_message_still_renders` 这对——它们守的是同一个过滤判断的两个方向。

### 批次 5 · 流式实时（P1 前半）

1. `AiStreamView` 加 `thinking: BTreeMap<u8, String>` / `live_tool` / `message`
2. `on_delta` 按 `kind` 分派，按 `segment` 分桶
3. `AiToolStarted` 写 `live_tool`（动机已有，直接搬）
4. `turn_from_stream` 读这三样

**验收**：`live_tool_shows_while_running`。

P1 剩下的（流式 Markdown 节流、检查点落库）与 P2 按方案走，本文件不展开。

---

## 5. 开工前必须先跑一遍的三条命令

```bash
# 1) 确认基线
cargo test --workspace 2>&1 | grep -E "^test result" | awk '{p+=$4;f+=$6} END{print p" passed "f" failed"}'
# 期望：792 passed 0 failed

# 2) 确认 schema 版本没被别人动过
grep -n "SCHEMA_VERSION" crates/me-core/src/lib.rs | head -1
# 期望：= 7

# 3) 确认要改的字面量数量（补字段的清单）
grep -rnE "(^|[^A-Za-z_])AiMessage \{" crates/ --include=*.rs | grep -v "struct AiMessage" | wc -l
grep -rnE "(^|[^A-Za-z_])AiToolTrace \{" crates/ --include=*.rs | grep -v "struct AiToolTrace" | wc -l
# 期望：10 / 8
```

---

## 6. 测试计划（按批次对应）

方案 §8 已列全，这里只标**必须有的守卫**——它们守的是「改错了不会有人发现」的地方：

### 6.1 协议层（批次 2）

| 测试 | 守护什么 |
| --- | --- |
| `openai_reasoning_object_form_is_ignored_safely` | `delta.reasoning` 是对象时 `.as_str()` 拿 `None`，**不能 panic** |
| `openai_text_and_thinking_do_not_mix` | 两条通道交错到达时各自正确，不会串味 |

### 6.2 存储层（批次 1）

| 测试 | 守护什么 |
| --- | --- |
| `thinking_truncated_flag_round_trips` | 少了它，截断标记丢了没人发现 |
| `empty_thinking_block_is_distinguishable_from_none` | `None` 与 `Some(空)` 的区分——方案说这是排障依据 |
| `old_message_without_thinking_reads_none` | 老数据 NULL 不能报错 |

### 6.3 引擎（批次 3）

| 测试 | 守护什么 |
| --- | --- |
| `throttle_residue_is_flushed_before_segment_switch` | 只在节流窗口跨边界时偶发，**不写测试永远发现不了** |
| `seq_is_globally_monotonic_across_channels` | §3.4 的撞号问题 |
| `segment_number_is_monotonic_and_never_goes_back` | 段号回退会让 UI 分桶错乱 |

### 6.4 UI（批次 4）

| 测试 | 守护什么 |
| --- | --- |
| `streaming_placeholder_produces_single_bubble` | B1 的两个气泡（**今天就有这个 bug**） |
| `failed_message_still_renders` | 上一条的过滤逻辑不能误伤终态消息 |
| `historical_message_shows_its_own_plan` | D2：两条消息各自的清单，不能只显示最后一份 |
| `render_turn_orders_four_blocks` | D3：顺序靠 shape 索引断言，不靠肉眼 |
| `card_message_outside_current_list_can_open` | P1：点历史卡片打开一封不在当前 `state.messages` 的邮件，阅读窗要真的打开（守 §3.11 的修复） |

`collect_texts`（[views/ai.rs:1936](../../crates/me-app/src/views/ai.rs)）已存在，直接复用。
注意它接的是 `&egui::Shape`，与现有 `run_frame` helper 配套使用。

---

## 7. 风险

| 风险 | 判断 | 缓解 |
| --- | --- | --- |
| UI 批次（批次 4）返工 | **高**。方案的伪代码在借用与 user 消息上不完整 | 先只改 `show()` 的调度，把 `render_turn` 做成空壳走通编译，再逐块填 |
| 18 处字面量补漏 | 中。编译器会全部报出来，不会漏 | 按批次 1 一次性做完，不要分批补 |
| 双通道 seq | 中。撞号是静默的，测试抓不到类型错误 | §3.4 先改 throttle，再动 sink |
| 思考截断造成「越看越短」 | 低但观感差 | `truncated` 落库 + 标题标注（方案 §5.2 已覆盖） |
| Anthropic 用户预期落空 | 中。用户可能以为换个模型就有 | 设置页写「仅 OpenAI 兼容端点」，见方案 §7.2 |
| 流式 Markdown 节流引卡顿 | 低。P1 才做，可回退纯文本 | 保留一个常量开关，出问题先关 |
| 卡片落库后「点开」失效更明显 | 中。`card_messages` 从未写入（§3.11），P1 之前不修的话命中率会上升 | 批次 4 顺手修掉，加一条测试守住 |

---

## 8. 与方案需要确认的地方

开工前需要你拍一下，这四处我按方案（或按我的修正）做，但都是「改了之后不好回头」的决定：

**① user 消息走独立分支（§3.2）。**
方案原本想用一个 `TurnView` 统一，我判断装不下 `context_refs`。
如果你希望「user 气泡也进统一渲染器」，那 `TurnView` 要加 `role` 和 `context_refs`，
复杂度会明显上升。建议按 §3.2 分两路。

**② `activity` 保留、只删 `cards`（§2.3 ④、§3.8）。**
方案写的是两个都删。但流式期间 `AiToolCompleted` 先于落库到达，
没有内存位就显示不出「刚才调了什么」。建议保留 `activity`。

**③ `thinking` 列追加在 `created_at` 之后（§3.7）。**
方案没规定位置。追加在尾部是最小改动，代价是列序与语义序不一致
（`thinking` 排在 `created_at` 后面，看起来有点怪）。可接受。

**④ 顺带修 `card_messages`（§3.11）。**
这是现存 bug，不在方案范围内。修它只多 4 行代码，不修则 P1 的历史卡片是死控件。
建议一起修；如果你希望严格按方案范围走，那就先在文档里登记为已知问题。

---

## 9. 变更记录

| 日期 | 内容 |
| --- | --- |
| 2026-11-07 | 初版。逐条核对方案 25 条假设（全部通过）、11 处偏差（E1–E11）、4 处方案未覆盖的决策点；给出五批实现顺序与 11 条守门测试。发现一个现存 bug：`card_messages` 从未写入，导致「点卡片打开不在当前列表的邮件」无反应 |
