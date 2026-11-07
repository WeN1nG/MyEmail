# AI 对话页面重构方案（四区块 · 流式 · 可回看）

> 事实基线：v0.4.0 / schema v7 / `cargo test --workspace` 792 passed
> 本文与代码冲突时以本文为准，落地后由本文回写代码索引。

---

## 0. 摘要

目标形态是把「一条 AI 回复」拆成**四个可以独立出现、固定从上到下**的区块：

```text
┌──────────────────────────────┐
│  [思考过程]   （没有就不显示）│
│  [任务清单]   （没有就不显示）│
│  [工具调用]   （没有就不显示）│
│  [回答]       （总有）        │
└──────────────────────────────┘
```

四条硬要求与对应机制：

| 要求 | 本文机制 |
| --- | --- |
| 四部分由上到下排列，没有就不显示 | 统一渲染器 `TurnView`，四处只在**一个函数**里定顺序；每块 `Option`，空则不画 |
| 流式实时传输 | `AiStreamDelta` 增 `kind`（思考 / 回答）双通道；清单与工具已有独立事件 |
| 每轮所有内容持久化 | `ai_messages` 增 `thinking` 列（schema v8）；卡片随 `tool_trace` 落库 |
| 历史记录点开可显示 | 历史与实时**共用同一个渲染器**，历史消息自带四块数据，不依赖会话级临时态 |

先说三个必须先讲清的判断，否则后面的方案会被误读：

1. **这不是一次 UI 调整，是一次数据模型补全。** 四块里有两块（思考、卡片）在系统里**根本没有数据**，一块（清单）有数据但历史渲染路径不读它。只改 `views/ai.rs` 的绘制顺序，改完仍然会「历史里看不到清单、永远看不到思考」。

2. **「四区固定顺序」与「真实生成时序」是冲突的。** 模型实际是 `思考 → 工具 → 思考 → 工具 → 回答` 交错进行的，四个固定区块必须把这个时序**压平**。本文选择压平（因为需求明确要求四项由上到下），代价与补偿见 §2.2。

3. **思考内容不回灌上下文。** 只做展示，不进入下一轮请求。这不只是省 token——Anthropic 的 thinking block 带签名，跨轮回传有硬性约束，违反会直接 400。详见 §4.4。

4. **v1 的思考只支持 OpenAI 兼容端点。** 这不是「先做简单的」那种取舍：本应用的 PLAN 阶段
   恒下发工具，模型必然调用，于是必然触发 Anthropic 的签名回传要求，开了就 400。
   Anthropic 要等 P2 做完签名透传。详见 §4.2.2——这一条如果搞错，实现会以为「加个开关就行」。

---

## 1. 现状盘点

以下全部来自对当前代码的实测，不是推测。

### 1.1 四块内容今天的实际归属

| 区块 | 数据是否存在 | 存在哪里 | 历史消息里能看到吗 |
| --- | --- | --- | --- |
| 思考过程 | **不存在** | —— | 不存在 |
| 任务清单 | 存在 | `ai_messages.plan`（JSON） | **看不到**（`show_message` 不读 `message.plan`） |
| 工具调用 | 部分存在 | `ai_messages.tool_trace`（JSON） | 能，但顺序错、且缺卡片 |
| 回答 | 存在 | `ai_messages.content` | 能 |

**「思考过程」的缺失是全链路的**：

- [`AiDelta`](../../crates/me-core/src/ai.rs) 只有 `Text(String)` 与 `Usage { .. }` 两个变体；
- [`apply_anthropic`](../../crates/me-proto/src/ai.rs) 的 `content_block_delta` 分支只认 `text_delta` 与 `input_json_delta`，`thinking_delta` 落进 `_ => {}` 被静默丢弃；
- [`apply_openai`](../../crates/me-proto/src/ai.rs) 只读 `delta["content"]`，不读 `reasoning_content`；
- [`AiPageState`](../../crates/me-app/src/views/ai.rs) 里没有任何思考字段，流式期间只有一句硬编码的「正在思考…」。

也就是说：即使模型返回了思考内容，今天也会在协议层被丢掉，连日志里都没有。

### 1.2 顺序现状

[`show_message`](../../crates/me-app/src/views/ai.rs) 渲染一条助手消息的实际顺序：

```text
content（Markdown 叙述）
  → [结果] 段落（split_result_block 切出来的状态卡）
  → 工具活动（show_activity，折叠）
  → 错误 / 已停止
  → token 用量
  → 复制按钮
```

而任务清单**不在这个消息里**——它由 [`show`](../../crates/me-app/src/views/ai.rs) 在消息列表之后、流式正文之前单独渲染：

```text
所有消息 → plan（会话级，只有最后一份）→ 删除确认 → 流式正文 → 工具活动 → 卡片
```

这个布局有三个后果：

1. 清单与产生它的那条消息**在视觉上脱离了**，长对话里会被顶出视野；
2. 会话级只有一个 `state.ai.plan` 槽位，**历史消息的清单无处可放**；
3. 流式期间的工具活动（`state.ai.activity`）与历史消息的工具活动（`message.tool_trace`）是**两套数据源**、两段渲染代码。

### 1.3 流式现状

```text
AiStreamStarted      → 建 stream（buffer 空）
AiStreamDelta × N    → buffer.push_str（只有回答文本）
AiPlanUpdated × N    → 覆盖 state.ai.plan
AiToolStarted        → 🔇 app.rs 明确忽略（"工具开始不单独展示"）
AiToolCompleted      → activity.push
AiMessageCompleted   → 清 stream，ai_messages_dirty = true，重载全部消息
```

四个问题：

- **单通道**：无法区分「思考」与「回答」，两者会混进同一个 buffer；
- **工具执行期间无反馈**：`AiToolStarted` 被丢弃，用户看到的是「正在思考…」卡住，直到工具跑完；
- **视觉跳变**：流式用 `ui.label(&stream.buffer)` 纯文本（注释写明「每帧解析会让滚动卡顿」），完成后重载换成 Markdown 渲染，字号、段落、代码块全部变一次；
- **清单闪烁**：`AiPlanUpdated` 每次都覆盖，重列（replan）时清单整体替换，没有「上一版去哪了」的痕迹（虽有 `plan.replans`，但 UI 只有一行提示）。

节流参数：`DELTA_INTERVAL = 50ms`、`DELTA_BATCH_BYTES = 2048`（[ai.rs](../../crates/me-sync/src/ai.rs)）。

### 1.4 持久化现状

- [`begin_assistant_tx`](../../crates/me-store/src/ai.rs) 在流式**开始时**插入一行 `status='streaming'`、`content=''` 的占位；
- 整个流式过程**不写任何内容**；
- [`finish_assistant_tx`](../../crates/me-store/src/ai.rs) 在**结束时**一次性 `UPDATE` content / status / tool_trace / plan / tokens。

后果：中途崩溃或强退，[`fail_orphan_streaming_messages`](../../crates/me-store/src/ai.rs) 只能把这行标成失败，**内容全空**。用户白等一场，且看不到任何已生成的部分。

`ai_messages` 现有列：

```sql
id, conversation_id, role, content, context_refs, status,
error, input_tokens, output_tokens, tool_trace, plan, created_at
```

没有 `thinking`，没有地方放检索卡片。

### 1.5 历史回看现状

- **清单**：`AiPageState::sync_plan_from_messages` 只取 `messages.iter().rev().find_map(|m| m.plan.clone())`——**倒数第一份**，历史消息各自的清单永远不渲染；
- **卡片**：`state.ai.cards` 是纯 UI 内存态，切会话即 `clear()`，重载不恢复（存储层压根没有 `MailCardRef`）；
- **分页**：`refresh_ai_messages` 固定 `limit=200`、`before=None`，超过 200 条的历史**取不到**（`list_messages_conn` 支持游标，UI 没用）。

### 1.6 六个具体缺陷（本文要修的就是这些）

| # | 缺陷 | 位置 |
| --- | --- | --- |
| D1 | 思考内容全链路缺失，协议层静默丢弃 | `me-proto/src/ai.rs`、`me-core/src/ai.rs` |
| D2 | 历史消息的 `plan` 不渲染 | `views/ai.rs::show_message` |
| D3 | 渲染顺序与目标相反 | `views/ai.rs::show_message` |
| D4 | 历史与实时是两套渲染代码，样式与顺序会漂移 | `show_message` vs `show_stream`/`show_plan`/`show_activity` |
| D5 | 检索卡片不落库，历史回看丢失 | `me-store`、`views/ai.rs` |
| D6 | 流式期间不落库，崩溃即全丢 | `me-sync/src/ai.rs`、`me-store/src/ai.rs` |

---

## 2. 需求拆解

### 2.1 四条要求 → 机制

| 要求原文 | 落点 | 说明 |
| --- | --- | --- |
| 四部分组成，由上到下 | `render_turn()` 单一函数 | 顺序只在此定义，不许散落 |
| 没有就不显示 | 每块在 `TurnView` 里是 `Option` / 空集合 | 空字符串也算没有（见 §6.5） |
| 流式实时传输 | `AiStreamDelta.kind` 双通道 | 思考与回答分开累积 |
| 每轮内容持久化 | `thinking` 列 + `tool_trace.cards` | schema v8 |
| 历史点开可显示 | 历史/实时共用 `TurnView` | 历史自带四块数据 |

### 2.2 压平时序的代价与补偿

**问题**：真实生成是交错的。一次典型的「查重要邮件」可能是：

```text
思考A: 用户要重要邮件，先定判据
工具:  email_search(criteria=[...])
思考B: 只查到 2 封，可能时间窗太窄
工具:  email_search(criteria=[...], days=30)
回答:  找到 2 封，分别是…
```

如果严格四区，就必须变成：

```text
思考过程:  [第1段] …  [第2段] …
任务清单:  …
工具调用:  email_search ×2
回答:      找到 2 封…
```

**代价**：失去了「哪段思考导致了哪次调用」的直接对应。

**补偿**（三条，都要做）：

1. **思考分段编号**：思考区按轮次显示「第 1 段 / 第 2 段」并带分隔线，而不是拼成一坨；
2. **工具块标注 `step_id` + `attempt`**：渲染成「第 3 步 · 第 2 次尝试」，与清单里的步骤号可对上。
   注意这里**不是「已有字段」就够了**：`AiToolTrace` 确实有这两个字段，但那只对**已落库**的
   trace 成立。流式期间 `AiToolCompleted` 事件不带这两个字段，`app.rs` 构造 trace 时硬写
   `step_id: None, attempt: None`。所以必须给事件补字段，否则补偿措施只在历史里生效、
   实时态失效——而实时态恰恰是用户最需要看「第几步」的时候。见 §7.1。
3. **清单里保留 `evidence`**：每步回执已经写了「命中 N 封」，用户能从清单倒推时序。

**为什么不采用「时序流」**：需求明确写了「四项由上到下排列」。按时序渲染会出现「回答-工具-回答-工具」的交替，与要求直接冲突。若将来要改，`TurnView` 的渲染函数是唯一改动点，数据层不用动（思考已按段存储，天然带顺序）。

---

## 3. 数据模型

### 3.1 思考块

新增一列，存**分段**后的思考文本（JSON 数组），而不是一整段：

```rust
// crates/me-core/src/ai.rs
/// 一段思考。分段的原因是模型会在每次工具调用后重新思考，
/// 拼成一整段会让用户以为是一次性想完的。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThinkingSegment {
    /// 段号，从 1 开始。渲染成「第 1 段」。
    pub index: u8,
    /// 思考正文。已按 [`MAX_THINKING_CHARS`] 截断。
    pub text: String,
    /// 这一段思考之后是否紧跟着工具调用（决定要不要画分隔线）。
    #[serde(default)]
    pub followed_by_tool: bool,
}

/// 思考的段数上限与总长上限。
///
/// 为什么上限比 `MAX_MESSAGE_CHARS`（256 KiB）小这么多：
/// 思考是**纯展示**的，不进上下文（§4.4），所以它的大小不影响任何功能正确性，
/// 只影响渲染开销。32 KiB 足够覆盖绝大多数真实思考；再长的部分对用户
/// 没有增量价值，反而会把滚动拖慢。
///
/// 注意：这与「回答预算」无关 —— 改完之后思考与回答是两个独立的列，
/// 不存在互相挤占。设上限纯粹是为了渲染性能。
pub const MAX_THINKING_SEGMENTS: usize = 16;
pub const MAX_THINKING_CHARS: usize = 32 * 1024;
```

`AiMessage` 增字段：

```rust
/// 一条消息的思考块。**整体存成一个 JSON 对象**（而不是只存 segments 数组）。
///
/// 为什么必须带上 `truncated`：截断状态是展示契约的一部分
/// （UI 要显示「已截断」）。如果只存数组，重载后这个标记就丢了，
/// 用户会以为看到的是完整的思考。多包一层对象的成本是一个花括号，
/// 少包一层的成本是一个永远修不好的「看起来完整其实丢了」。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThinkingBlock {
    #[serde(default)]
    pub segments: Vec<ThinkingSegment>,
    /// 是否因超限被截断。
    #[serde(default)]
    pub truncated: bool,
}

pub struct AiMessage {
    // …既有字段…
    /// 仅 `assistant`：思考块。老数据为 `None`，读成「没有思考」。
    #[serde(default)]
    pub thinking: Option<ThinkingBlock>,
}
```

**为什么用 `Option` 而不是 `Default` 空值**：`None` = 这条消息没有思考（老数据、
或模型不支持），`Some(ThinkingBlock { segments: [], .. })` = 有思考但内容为空（异常）。
两者在 UI 上都表现为「不显示」，但在排障时能区分「没开」和「开了但没拿到」。

列里存的就是这个对象的 JSON：

```sql
-- 老数据：NULL
-- 新数据：{"segments":[{"index":1,"text":"…","followed_by_tool":true}],"truncated":false}
```

**为什么存 JSON 数组而不是纯文本**：如果只存字符串，就没法分段，只能靠分隔符猜；而模型输出的思考天然是按工具调用切开的。存成数组后，「第 N 段」是数据而非约定。

### 3.2 工具块带卡片

`MailCardRef` 现在只在事件里传，不落库。最小侵入的做法是塞进 `AiToolTrace`：

```rust
pub struct AiToolTrace {
    // …既有字段…
    /// 这次调用检索到的邮件（仅 `email_search` 有）。
    /// 老数据没有这个键，读成空 —— 历史里就只显示摘要，不显示卡片。
    #[serde(default)]
    pub cards: Vec<MailCardRef>,
}
```

这样 `tool_trace` 一个 JSON 列就带上了全部工具上下文，**不需要新表、不需要新查询**。`ToolTraceRow`（存储层 DTO）加同样的字段并 `#[serde(default)]`，老行自然读成空。

卡片本身只存 `message_id + reason`（现有 `MailCardRef` 结构），渲染时按 id 去查摘要——摘要可能已被删除，此时降级显示「邮件已删除」而不是报错。

**必须同步修改 `AiToolTrace` 的文档注释。** 它现在写的是：

```text
/// 工具调用摘要。**不存参数、不存结果原文**。
```

加 `cards` 之后这句就不再准确了。实际存下来的是「结果**引用**」而不是原文，但
`card_reason` 会产出 `主题命中「{用户查询词}」`——也就是把**用户的查询词**一并存了进去。
这不构成隐私问题（查询词本来就来自用户自己的输入、且只存在本地库），但注释必须改成
如实描述，否则下一个维护者会以为 `tool_trace` 里不可能有业务文本：

```text
/// 工具调用摘要。**不存参数原文、不存邮件正文**。
///
/// 例外是三类给用户看的短文本：`motive`（为什么调）、`summary`（一句话结果）、
/// `cards[].reason`（命中理由，可能含用户自己的查询词）。
/// 它们只进 `tool_trace` 与界面，**不进 tracing 日志**。
```

### 3.3 顺序不靠表结构，靠渲染器

**这里要明确一个反直觉的决定**：不给消息加 `blocks: Vec<Block>` 这样的通用有序块结构。

理由：

- 需求是**固定的四区**，不是任意块序列。引入通用块结构会让「顺序」变成数据的一部分，每次渲染都要判类型、排优先级，复杂度和收益不成正比；
- 四块的数据形状差异极大（文本 / 结构化计划 / 轨迹数组 / Markdown），统一成 `Block` enum 只会得到四个无共同行为的变体，等于把 `if` 挪进 enum；
- 已有字段（`content` / `plan` / `tool_trace`）都能直接复用，加一个 `thinking` 就齐了。

顺序的唯一来源是 §6.1 的 `render_turn()`。**这是硬约束：任何地方都不许自己拼四块的顺序。**

### 3.4 schema v8 迁移

```rust
// crates/me-store/src/migrations.rs

/// v7 → v8：给 `ai_messages` 补一列 `thinking`（思考块 JSON）。
///
/// 纯加列，不动既有约束 —— `ai_messages` 上有 CHECK（role/status），
/// 动它们就得重建表，风险远大于收益。与 v4→v5 加 `plan` 同一套理由。
fn migrate_v7_to_v8(conn: &mut Connection) -> Result<(), CoreError> {
    // 目标版本写死 8，不能写 SCHEMA_VERSION —— 否则 v7 库会被一步盖上最新版本，
    // 之后的 v8→v9 补列就被跳过了（v2→v3、v5→v6 都踩过这个坑）。
    const TARGET: u32 = 8;
    let tx = conn
        .transaction()
        .map_err(|e| storage(format!("开始 v7→v8 迁移事务失败: {e}")))?;

    // 按列存在性判断而不是无条件 ALTER：新建库走 `create_schema`，
    // 它的 DDL 已带这一列，重复 ALTER 会报「duplicate column」。
    let has_column: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('ai_messages') WHERE name = 'thinking')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| storage(format!("读取 ai_messages 列信息失败: {e}")))?;
    if !has_column {
        tx.execute_batch("ALTER TABLE ai_messages ADD COLUMN thinking TEXT;")
            .map_err(|e| storage(format!("执行 v7→v8 迁移失败: {e}")))?;
    }

    tx.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
        params![TARGET.to_string()],
    )
    .map_err(|e| storage(format!("写入 schema_version 失败: {e}")))?;

    check_foreign_keys(&tx)?;
    tx.commit()
        .map_err(|e| storage(format!("提交 v7→v8 迁移失败: {e}")))?;
    Ok(())
}
```

**不回填**：老行 `thinking` 保持 `NULL`，读出即 `None`，UI 不显示思考区。
不做「补一个空对象」的 UPDATE —— 那会让「老消息」和「新消息但没思考」变得无法区分。

DDL 同步：

```sql
-- SCHEMA_V3_DDL 中的 ai_messages
CREATE TABLE ai_messages (
    id              INTEGER PRIMARY KEY,
    conversation_id INTEGER NOT NULL REFERENCES ai_conversations(id) ON DELETE CASCADE,
    role            TEXT NOT NULL CHECK (role IN ('user','assistant')),
    content         TEXT NOT NULL DEFAULT '',
    context_refs    TEXT,
    status          TEXT NOT NULL DEFAULT 'complete'
                    CHECK (status IN ('streaming','complete','failed','cancelled')),
    error           TEXT,
    input_tokens    INTEGER,
    output_tokens   INTEGER,
    tool_trace      TEXT,
    plan            TEXT,
    -- v8 起：思考块 JSON（{"segments":[…],"truncated":false}）。老库由 migrate_v7_to_v8 补列。
    thinking        TEXT,
    created_at      INTEGER NOT NULL
);
```

`SCHEMA_VERSION` 7 → 8。

**兼容性**：老消息 `thinking` 为 NULL → 读成空数组 → 思考区不显示（符合「没有就不显示」）。无需回填。

---

## 4. 流式协议

### 4.1 `AiDelta` 增加思考通道

```rust
// crates/me-core/src/ai.rs
pub enum AiDelta {
    /// 回答正文。
    Text(String),
    /// 思考正文。与 `Text` 分开走，因为 UI 要把它放进另一个区块。
    Thinking(String),
    /// 服务商在流中或流尾给出的用量；缺省时保持 `None`。
    Usage { input: Option<u32>, output: Option<u32> },
}
```

### 4.2 两家厂商的差异

**先说结论：v1 只做 OpenAI 兼容端点的响应侧解析；Anthropic 整体推迟到 P2。**

#### 4.2.1 OpenAI 兼容（v1 实现）

字段名不统一，需要同时兼容：

| 来源 | 字段 |
| --- | --- |
| DeepSeek-R1 | `delta.reasoning_content` |
| vLLM / 部分网关 | `delta.reasoning` |
| 其他 | 无思考字段 |

```rust
let reasoning = delta["reasoning_content"]
    .as_str()
    .or_else(|| delta["reasoning"].as_str());
if let Some(text) = reasoning.filter(|t| !t.is_empty()) {
    state.thinking.push_str(text);
    sink(AiDelta::Thinking(text.to_string()));
}
```

没有统一的「开启思考」请求参数：DeepSeek-R1 是模型自带的，qwen 系用 `enable_thinking`。
**v1 不做请求侧开关，只做响应侧解析**——服务商给就展示，不给就没有。这样不需要为每个
端点猜参数，也不会因为传了不认识的字段被 400。

一个容易漏的兼容点：`delta.reasoning` 在部分网关上是**对象**（形如 `{"content": "…"}`）
而不是字符串，`.as_str()` 会拿到 `None`。如果 P1 之后收到「明明有思考却不显示」的反馈，
优先查这里，而不是查引擎。

#### 4.2.2 Anthropic（P2，且有前置条件）

**这里必须纠正一个容易想当然的判断：本应用不存在「Anthropic 无工具调用」这条路径。**

依据：`plan_phase_tool_specs()` 恒下发 `plan_submit` / `answer_directly`，其文档注释写明
「**不经 `AiToolPermissions` 过滤**——它们是引擎机制，不是用户可以关掉的能力」。两者都是工具，
且 PLAN 阶段会通过 nudge 逼迫模型调用其中之一。所以**每一次请求都带工具，模型必然产生
`tool_use` 块**。

而 Anthropic 的规则是：开启 extended thinking 后，含 `tool_use` 的 assistant 消息在
**下一轮请求里必须原样带回带 `signature` 的 thinking block**，否则直接 400。

结论：**只要开了 `thinking`，本应用的第一个请求就会 400。** 要支持 Anthropic 思考，
必须先完成 `signature` 透传（`AiWireMessage::Assistant` 增 `thinking_blocks` 字段，
把签名从协议层经引擎带到工具循环），这是 P2 的独立工作量，不是加个开关就行。

以下是 P2 落地时需要的东西，**v1 不实现**：

请求体（受三条服务端强制的硬约束，违反直接 400）：

```rust
// anthropic_body()
if config.thinking_budget > 0 {
    body["thinking"] = json!({
        "type": "enabled",
        "budget_tokens": config.thinking_budget,
    });
    // 开启 thinking 后 temperature 必须为 1 或不传；我们本来就不传，保持不传最稳。
}
```

- `budget_tokens >= 1024`；
- `max_tokens > budget_tokens`；
- `temperature` 为 1 或不传。

流式返回：

```text
content_block_start  { index, content_block: { type: "thinking" } }
content_block_delta  { index, delta: { type: "thinking_delta", thinking: "…" } }
content_block_delta  { index, delta: { type: "signature_delta", signature: "…" } }
content_block_stop   { index }
```

`apply_anthropic` 的解析本身很简单（与 OpenAI 分支同构）：

```rust
Some("thinking_delta") => {
    let text = value["delta"]["thinking"].as_str().unwrap_or("");
    if !text.is_empty() {
        state.thinking.push_str(text);
        sink(AiDelta::Thinking(text.to_string()));
    }
}
```

难的是 `signature_delta` 要存住并回传——P2 的范围。

### 4.3 事件增加 `kind`

```rust
// crates/me-core/src/command.rs
pub enum AiDeltaKind {
    /// 回答正文 → 回答区
    Answer,
    /// 思考 → 思考区
    Thinking,
}

AiStreamDelta {
    conversation: crate::ai::AiConversationId,
    message: crate::ai::AiMessageId,
    seq: u64,
    kind: AiDeltaKind,
    /// 思考的分段号，从 1 开始。`kind == Answer` 时恒为 0（无意义）。
    ///
    /// 为什么必须带：切段的时机在引擎（每次工具轮次结束），
    /// 而 UI 收到的是一条扁平的增量流。带不上段号，流式期间就画不出
    /// 「第 1 段 / 第 2 段」的分隔线，只能等落库重载才突然出现分隔——
    /// 那正是要消除的「跳变」。见 §5.2。
    segment: u8,
    delta: String,
},
```

**为什么加字段而不是加事件**：两个事件会让「先思考后回答」的顺序在通道层面丢失，并且要维护两套 seq。加 `kind` 保持单一序，UI 按 kind 分派到不同 buffer。加 `segment` 同理——它只是 `kind == Thinking` 时的附加属性，不值一个独立事件。

现有 `match` 都用了 `..`（`app.rs` 的 `AiStreamDelta { conversation, seq, delta, .. }`、
`ai_flow_mock.rs` 的 `AiStreamDelta { delta, .. }`），加字段不破坏它们。

`seq` 语义不变：**全局单调**，思考与回答共用一个序列，UI 靠它检测缺口。

### 4.4 思考不回灌上下文

**明确决定：`thinking` 只做展示，永不进入下一轮请求。**

三条理由，第一条是硬约束：

1. **Anthropic 的签名约束**。thinking block 带 `signature`，在**同一个 assistant turn 内**（含工具调用循环）必须原样回传，否则 400。跨 turn 的历史可以丢弃。我们的引擎在工具循环里会 `messages.push(Assistant { .. })`，若开了 thinking 就必须带上原始带签名的完整块——这需要 `AiWireMessage::Assistant` 增字段、并把 signature 从协议层一路透传到引擎。
2. **token 成本**。思考长度常是回答的数倍，回灌等于每轮多付几倍输入。
3. **没有收益**。思考是「过程」不是「结论」，模型不需要看自己上一次怎么想的；真正需要的是 `content`（结论）和 `tool_trace`（做了什么）。

`trim_history` **不改**：它只读 `message.content`，天然不带思考。

**因此 v1 的落地范围**：

| 场景 | 支持思考展示 | 说明 |
| --- | --- | --- |
| OpenAI 兼容端点（DeepSeek-R1 等） | ✅ v1 | 只解析 `reasoning_content`，不需要回传，无签名问题 |
| Anthropic（任意场景） | ❌ P2 | PLAN 阶段恒下发工具 → 模型必调 → 必须回传 signature。**不是「有工具才不支持」，是这个应用里根本不存在无工具的请求**，见 §4.2.2 |

原先把「Anthropic + 无工具调用」列为 ✅ 是错的：那条路径在本应用里不存在。
设置页的说明必须写成「v1 仅在 OpenAI 兼容端点下展示思考」，而不是「部分场景生效」——
后者会让用户以为换个模型就好，实际换了也拿不到。

---

## 5. 引擎侧

### 5.1 双通道累积

[`sink`](../../crates/me-sync/src/ai.rs) 现在只有一个 `DeltaThrottle`。改成两个（思考 / 回答），各自独立节流：

```rust
let answer_throttle = Arc::new(Mutex::new(DeltaThrottle::new()));
let thinking_throttle = Arc::new(Mutex::new(DeltaThrottle::new()));

AiDelta::Text(text)     => answer_throttle.push(..)   → AiStreamDelta { kind: Answer, .. }
AiDelta::Thinking(text) => thinking_throttle.push(..) → AiStreamDelta { kind: Thinking, .. }
```

**为什么分开节流**：如果共用一个 throttle，思考的 2KB 批量会把回答的首字延迟顶到下一批，用户会觉得「打字卡」。

`seq` 仍用**一个**计数器（放在 sink 外层 `AtomicU64`），保证两通道的增量有全序，UI 才能检测「有没有整段丢掉」。

### 5.2 分段与累积

引擎侧维护：

```rust
let mut thinking_segments: Vec<ThinkingSegment> = Vec::new();
/// 当前正在累积的那一段。
let mut current_thinking = String::new();
/// 当前段号，1 起。**每一条思考增量都带上它**（见 §4.3）。
let mut thinking_segment_no: u8 = 1;
```

**切段时机**：每次「模型返回了 tool_calls」时，把 `current_thinking` 收成一段并清空。

```rust
if !turn.tool_calls.is_empty() && !current_thinking.is_empty() {
    thinking_segments.push(ThinkingSegment {
        index: thinking_segment_no,
        text: std::mem::take(&mut current_thinking),
        followed_by_tool: true,
    });

    // **必须在切换段号前把 throttle 里攒着的残余刷出去**，
    // 否则残余会顶着旧段号、跟在新段号后面到达，UI 会把它们塞进错误的段里。
    //
    // 注意 `DeltaThrottle::take()` 现在返回 `(u64, String)` 而不是 `Option`——
    // 它是「无条件取走」。这里需要一个「有残余才取」的变体，所以要先给
    // `DeltaThrottle` 加一个方法（`pending` 字段已经在结构体里了）：
    //
    //     /// 有残余才返回；没有就不占 seq、不动时钟。
    //     fn flush(&mut self) -> Option<(u64, String)> {
    //         if self.pending.is_empty() { None } else { Some(self.take()) }
    //     }
    //
    if let Ok(mut guard) = thinking_throttle.lock()
        && let Some((seq, chunk)) = guard.flush()
    {
        events.send_progress(Event::AiStreamDelta {
            conversation,
            message,
            seq,
            kind: AiDeltaKind::Thinking,
            // 残余属于**刚关闭的那一段**，所以是自增前的段号。
            segment: thinking_segment_no,
            delta: chunk,
        });
    }

    // 刷完残余再自增，后续增量带新段号，UI 据此开新段。
    thinking_segment_no = thinking_segment_no.saturating_add(1);
}
```

**段号自增与刷残余的先后顺序不能反**：先自增再刷，残余就会被标成新段号，
而它其实属于旧段——UI 上表现为「上一段的尾巴跑到下一段开头」，且只在节流窗口
刚好跨过切段边界时偶发，极难复现。所以要有一个测试专门守它
（`throttle_residue_is_flushed_before_segment_switch`，见 §8.3）。

循环结束时若还有剩余，收成最后一段（`followed_by_tool = false`）。

**流式与落库的段号必须一致**：UI 按 `segment` 分组累积（`HashMap<u8, String>`
或定长 `Vec`），落库时按 `ThinkingSegment.index` 排序渲染。两处都用同一个段号来源，
否则会出现「流式显示 3 段，刷新后变 2 段」。

**截断策略**：超过 `MAX_THINKING_CHARS` 时丢弃后续并置 `truncated = true`，UI 显示
「思考过程过长，已截断」。段数超过 `MAX_THINKING_SEGMENTS` 时同样处理。

**注意截断会造成一次「向后跳」**：流式期间用户已经看到了完整思考，重载后却变短了。
这是无法避免的（总得有个上限），但必须让用户知道原因——所以 `truncated` 标记要落库
（§3.1 的 `ThinkingBlock`），并在标题旁显示「（已截断）」，而不是让内容无声地变短。

**不静默丢弃**——用户有权知道展示不全。

### 5.3 卡片随轨迹落库

`tool_email_search` 已经 emit 了 `AiToolEmailResults { cards, .. }`。现在要把它同时塞进 `AiToolTrace`：

```rust
traces.push(AiToolTrace {
    call_id,
    tool,
    status,
    summary,
    duration_ms,
    motive,
    motive_source,
    step_id,
    attempt,
    cards,           // ← 新增：本次检索的命中列表
});
```

`cards` 来自工具执行结果（当前 `tool_email_search` 内部已构造 `MailCardRef` 列表，直接复用，不重复计算）。

### 5.4 流式检查点落库（P1，可选）

现状是「结束才写」。要做到「每一轮内容都持久化」，最低要求是**结束时写全**（已有 + 加 thinking）。但崩溃仍会丢。

**建议做节流检查点**：

```rust
// 每 1 秒最多一次，且内容有变化才写
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(1);

fn maybe_checkpoint(&mut self, id: AiMessageId, content: &str, thinking: &[ThinkingSegment]) {
    if self.last_checkpoint.elapsed() < CHECKPOINT_INTERVAL { return; }
    // UPDATE ai_messages SET content = ?, thinking = ? WHERE id = ? AND status = 'streaming'
}
```

**权衡**：

| | 不检查点 | 检查点（1s 节流） |
| --- | --- | --- |
| 崩溃恢复 | 全丢 | 丢最后 1 秒 |
| 写放大 | 1 次/轮 | ~60 次/分钟（长回答） |
| SQLite 锁竞争 | 无 | 轻微，WAL 下可接受 |

**建议先做 P1 而不是 P0**：核心价值在「历史完整」，这在结束时写入就已满足；检查点是「抗崩溃」的额外保障，可以在核心功能稳定后再加，避免一开始就把写路径搞复杂。

注意 `status='streaming'` 的行在 `fail_orphan_streaming_messages` 里会被标成失败并 `skip_open("上次回复被中断…")`，检查点写入的内容**不会被清掉**——这正是我们想要的。

---

## 6. UI 重构

这是改动最集中的一块，也是唯一能同时解决 D2/D3/D4 的地方。

### 6.1 统一渲染器 `TurnView`

**核心改动**：把「一条 AI 回复」抽象成一个视图模型，历史与实时都构造它，再交给同一个渲染函数。

```rust
/// 一轮 AI 回复的四块。顺序由 [`render_turn`] 单独决定。
///
/// 文本用 `Cow` 而不是 `&str`：历史消息可以零拷贝借用，
/// 流式那条必须 owned —— 见下面「借用冲突」一节。
pub struct TurnView<'a> {
    pub thinking: Option<ThinkingView<'a>>,
    pub plan: Option<&'a me_core::ai_plan::Plan>,
    pub traces: &'a [AiToolTrace],
    pub answer: std::borrow::Cow<'a, str>,
    /// 正文是否还在流式增长（决定 Markdown 是否节流重解析）。
    pub streaming: bool,
    pub status: me_core::ai::AiMessageStatus,
    pub error: Option<String>,
    pub usage: Option<(u32, u32)>,
}

pub struct ThinkingView<'a> {
    pub segments: std::borrow::Cow<'a, [ThinkingSegment]>,
    pub truncated: bool,
    /// 是否正在产出（控制「思考中…」动画与默认展开）。
    /// 这里刻意不放「当前段号」——流式期间段边界还没定，见 §5.2。
    pub live: bool,
}
```

两个构造点：

```rust
/// 历史消息：全部借用，零拷贝。
fn turn_from_message(message: &AiMessage) -> TurnView<'_> { .. }

/// 流式中的那条：必须 owned。
///
/// `state.ai` 是整体借出的，而渲染又要改 `blocks_cache`，
/// 两者同源于 `UiState`，`&state.ai` 与 `&mut state.ai.blocks_cache` 会冲突。
/// 这里克隆一次（一轮的量级：正文 + ≤32 KiB 思考），每帧一次，代价可接受。
fn turn_from_stream(state: &AiPageState) -> Option<TurnView<'static>> { .. }
```

**借用冲突：为什么 `render_turn` 不能接 `&mut UiState`**

这是本节最容易写错的地方。第一版草稿给的是：

```rust
// ❌ 编译不过
fn render_turn(state: &mut UiState, turn: &TurnView<'_>)
```

`TurnView` 从 `state` 借出，同一个调用里又要 `&mut UiState`，典型的自引用借用冲突。
现有代码早就在绕这个：`show()` 里用 `std::mem::take(&mut state.ai.messages)` 把消息整批
移出，渲染完再放回。

正确的做法是**只把渲染真正需要改的那几个字段传进去**，而不是整个 `UiState`：

```rust
/// 渲染一轮所需的最小可变状态。**不含 `UiState`**。
struct TurnCtx<'a> {
    /// Markdown 解析缓存。键用「轮次键 + 正文长度」，长度没变就不重解析。
    blocks_cache: &'a mut Option<BlocksCache>,
    /// 用户点了卡片要打开哪封邮件；由调用方在渲染结束后处理，
    /// 避免渲染函数直接改 `selected_message` / `reader_open`。
    open_message: &'a mut Option<me_core::MessageId>,
    commands: &'a mut Vec<Command>,
}

fn render_turn(ui: &mut egui::Ui, ctx: &mut TurnCtx<'_>, turn: &TurnView<'_>) {
    // 顺序在这里，且只在这里。
    // 思考 → 清单 → 工具 → 回答
    if let Some(thinking) = &turn.thinking   { show_thinking(ui, thinking); }
    if let Some(plan) = turn.plan            { show_plan(ui, plan); }
    if !turn.traces.is_empty()               { show_tools(ui, ctx, turn.traces); }
    show_answer(ui, ctx, turn);
}
```

调用侧 —— **这里的语句顺序有讲究，不能随便调换**：

```rust
// 1) 先把渲染要改的状态「移出」state.ai，而不是「借用」它。
//    借用（&mut state.ai.blocks_cache）会和后面 turn_from_stream(&state.ai) 冲突。
let mut blocks_cache = state.ai.blocks_cache.take();
let mut open_message: Option<me_core::MessageId> = None;

{
    // 2) ctx 只持有移出来的值 + commands，完全碰不到 state.ai
    let mut ctx = TurnCtx {
        blocks_cache: &mut blocks_cache,
        open_message: &mut open_message,
        commands,
    };

    // 3) 流式那条：owned，借用在语句结束时就释放了
    let stream_turn = turn_from_stream(&state.ai);

    // 4) 历史消息：整批移出后逐条借用渲染（沿用现有 take / 放回 的写法）
    let messages = std::mem::take(&mut state.ai.messages);
    for message in &messages {
        if message.status == AiMessageStatus::Streaming {
            continue; // 占位行交给第 5 步，否则会出现两个气泡
        }
        let turn = turn_from_message(message);
        render_turn(ui, &mut ctx, &turn);
    }
    state.ai.messages = messages;

    // 5) 列表末尾补上流式那条
    if let Some(turn) = &stream_turn {
        render_turn(ui, &mut ctx, turn);
    }
}

// 6) 缓存放回。放在块外，保证 ctx 已释放。
state.ai.blocks_cache = blocks_cache;
```

三条容易踩的顺序约束，实现时照这个来：

- **先 `take` 再借用**：`&mut state.ai.blocks_cache` 与 `&state.ai` 不能同时存在；
- **`turn_from_stream` 必须返回 owned**：它一旦返回带生命周期的借用，第 4 步的
  `std::mem::take(&mut state.ai.messages)` 就又冲突了；
- **`blocks_cache` 放回要在 `ctx` 作用域之外**：否则 `&mut blocks_cache` 还活着。

**收益**：

- 历史消息现在能显示清单（D2 解决）；
- 顺序单点定义，不会漂移（D3 解决）；
- 历史与实时同一套渲染，样式必然一致（D4 解决）；
- 签名可编译，且不需要为了借用把整条消息 clone 一遍。

### 6.2 四块的渲染细节

**思考区**

```text
┌ 思考过程 · 3 段                         [展开/折叠]
├─────────────────────────────────────────────────
│ 第 1 段
│ 用户要「重要的邮件」，本地没有 importance 字段…
│ ──────────────────────────────────────────────
│ 第 2 段
│ 只查到 2 封，时间窗可能太窄…
└─────────────────────────────────────────────────
```

- 流式中：默认展开，标题显示「思考中…」+ 已产出字数；
- 完成后：默认折叠，标题显示「思考过程 · N 段」；
- **段序实时增长**：靠 `AiStreamDelta.segment`（§4.3）而不是等落库。第 1 段正在写时
  只有「第 1 段」，模型转去调工具、回来再想时自动多出「第 2 段」。这样流式与
  重载后的分段结构完全一致，不会出现「刷新一下突然多出分隔线」；
- `truncated` 时标题后缀「（已截断）」，末尾一行灰字说明；
- 思考正文用**等宽字体 + 稍小字号 + 灰底**，与回答的 Markdown 明显区分——它是「过程」不是「结论」；
- 不跑 Markdown 解析（思考里常有未闭合的代码块，硬解析会吞掉半屏）。

**清单区**

沿用现有 `show_plan`，但**改为消息内渲染**：

- 历史消息的清单默认折叠（已结束，不占地方）；
- 流式中的清单默认展开（`plan.open_steps() > 0`）；
- `replans` 已有记录，保留现有「第 N 次计划」+「上一轮的问题」提示；
- 删除会话级的 `state.ai.plan` 渲染位与 `plans` HashMap 中「仅为显示」的那部分，改由消息承载。

> 注意：`plans` 缓存仍要保留，但**只用于流式中尚未落库的那份**。落库后一律从消息读。

**工具区**

现在是 `CollapsingHeader::new("工具活动（N）")` + 每行一句摘要。要改成带卡片的结构：

```text
┌ 工具调用 · 2 次                              [展开/折叠]
├─────────────────────────────────────────────────
│ ✓ email_search · 检索到 3 封 · 42ms · 第 2 步
│   动机：按两条判据查
│   ┌ 结算通知  财务部 <fin@x.com>  11-06
│   │ 命中：已星标
│   ├ 面试安排  HR <hr@y.com>  11-05
│   │ 命中：近 7 天未读
│   └ …另有 1 封
│ ✗ mail_read · 正文未缓存 · 12ms · 第 3 步 · 第 2/3 次
│   动机：读第一封
└─────────────────────────────────────────────────
```

- 状态图标区分成功 / 失败 / 取消（复用 `AiToolStatus` 的 `Ok` / `Error` / `Cancelled` / `Skipped`）；
- `attempt > 1` 时显示「第 N/M 次」，与清单角标一致；
- 卡片最多显示 `MAX_CARDS = 5`，超出显示「另有 N 封」；
- 卡片点击 → 写 `ctx.open_message`，由调用方转成 `Command::OpenMessage`（沿用现有 `show_cards` 的逻辑）。

**流式期间显示「进行中」的工具 —— 但不要给 `AiToolStatus` 加 `Running`**

第一版草稿写的是「插入一条 `status = Running` 的 trace」，这是错的：

- `AiToolStatus` 只有 `Ok / Error / Cancelled / Skipped` 四个变体；
- 更关键的是它是**持久化类型**——`ToolTraceRow::into_trace` 按字符串 `parse`，
  解析失败会**整条 trace 被丢弃**。加 `Running` 等于引入一个「落库但不该出现」的状态，
  打破「落库的 trace 永远是终态」这条隐含不变量（`fail_orphan_streaming_messages`
  等收尾逻辑也默认这一点）。

正确做法：把「正在进行」留在 UI 层，不碰持久化枚举。

```rust
/// 流式期间正在跑的那一次工具调用。只活在内存里，永不落库。
pub struct LiveTool {
    pub call_id: String,
    pub tool: AiToolName,
    pub motive: Option<String>,
    pub started: Instant,
}

pub struct AiStreamView {
    // …既有字段…
    /// `Some` = 有一次调用正在执行。收到 `AiToolCompleted` 时置回 `None`，
    /// 同时那条 trace 由 app 侧推进 `activity`（现有逻辑）。
    pub live_tool: Option<LiveTool>,
}
```

渲染顺序：`activity` 里的已完成 trace → 最后补一条 `live_tool`（转圈 + 「正在调用 email_search…」）。
`AiToolStarted` 不再被 `app.rs` 丢弃，而是写进 `live_tool`。

这样工具执行的那几秒用户能看到「正在调用 X」而不是干等，而 `tool_trace` 的持久化语义一点没变。

**回答区**

- 正文 Markdown 渲染，复用现有 `markdown_to_blocks` + `with_images`；
- `[结果]` 段落保留在回答区**内部**末尾（它是对本次回答的总结，不是独立块）；
- 流式中也要跑 Markdown（见 §6.4）；
- 底部：错误 / 已停止 / token 用量 / 复制按钮。

### 6.3 历史与实时同构

关键不变量：**历史消息和流式中的那条，走过的是同一个 `render_turn`。**

这意味着流式结束时的「重载」不再产生视觉跳变——因为渲染路径没变，只是数据源从内存 buffer 换成了数据库。

实现上有个细节：流式中的那条**还没有** `AiMessage`（只有 id 和 buffer）。两种做法：

- (a) 构造一个临时 `AiMessage` 塞进 `messages` 列表 → 与数据库里的 streaming 占位行重复；
- (b) 流式那条**不进** `messages`，在消息列表末尾单独 `render_turn(turn_from_stream(..))`。

选 **(b)**。渲染后视觉上仍是一条消息（同样的 `Frame::group`、同样的宽度），只是数据来源不同。

#### 但 (b) 单独不够：数据库那条占位行会被渲染成第二个气泡

这是第一版草稿漏掉的一半，而且**今天的代码就已经有这个问题**：

```text
begin_assistant_message()  →  INSERT ... status='streaming', content=''
emit(AiStreamStarted)
  →  app.rs: ai_messages_dirty = true
  →  refresh_ai_messages()  →  list_messages 把这行一起取回来
  →  show_message() 对 content='' 的助手消息渲染「…」
show_stream()  →  再渲染一个气泡
```

也就是说流式期间现在会出现**两个气泡**：一个显示「…」（来自数据库占位行），
一个显示流式正文。`list_messages_conn` 没有按 `status` 过滤，`views/ai.rs` 与 `app.rs`
里也搜不到任何 `Streaming` 相关的判断——这个问题是实实在在存在的。

修法是**渲染时跳过未完成的占位行**，而不是不查它（它还得留在列表里保证顺序）：

```rust
// show() 里遍历消息时（**语句顺序以 §6.1 的完整版为准**，这里只示意判断逻辑）
for message in &messages {
    // 流式占位行由 turn_from_stream 负责渲染，这里跳过，否则会出现两个气泡。
    // 只跳过 Streaming；Failed / Cancelled 是终态，必须照常显示。
    if message.status == AiMessageStatus::Streaming {
        continue;
    }
    let turn = turn_from_message(message);
    render_turn(ui, &mut ctx, &turn);
}
// 列表末尾补上流式那条
if let Some(turn) = turn_from_stream(&state.ai) {
    render_turn(ui, &mut ctx, &turn);
}
```

> 注意：上面这段是为了突出「跳过」这个判断，**借用顺序上并不完整**。
> 实际实现请按 §6.1 的六步来——那里的 `take` / `owned` / 作用域顺序才是能编译的版本。

**注意边界**：只跳过 `Streaming`。`Failed` / `Cancelled` 是终态，用户要能看到「这条失败了」，
不能一起过滤掉。

另一个副作用：跳过之后，`messages` 里那条占位行的 `id` 正好可以当作
`turn_from_stream` 的「轮次键」，用于 Markdown 缓存。`AiStreamStarted` 事件已经带了
`message: AiMessageId`，但 `app.rs` 现在用 `..` 把它忽略了——需要接住并存进 `AiStreamView`。

### 6.4 流式 Markdown 与跳变消除

现状注释说「每帧解析会让滚动卡顿」，这个判断是对的，但不能因此退回纯文本——那会导致完成后跳变。

**方案：时间节流 + 内容指纹缓存**

```rust
/// 流式 Markdown 的重解析节流。
const STREAM_MD_INTERVAL: Duration = Duration::from_millis(120);
```

- 每 120ms 最多重解析一次（人眼几乎察觉不到，但 CPU 降到 1/6）；
- 缓存键用 `(消息 id, 正文长度)` —— 长度没变就不解析（复用现有 `blocks_cache` 的思路）；
- 解析在**后台线程**做，UI 线程只取结果（避免长回答卡住滚动）。

如果 P0 阶段不想引入后台线程，退一步：120ms 节流 + 只解析「最后 4KB」用于显示，滚动到上方时用完整缓存。但推荐直接上后台线程，`Markdown` 解析是可测的纯函数，线程化风险低。

### 6.5 空区块不显示的判定

「没有」的判定要严格，否则会出现只剩标题的空壳：

| 区块 | 视为「没有」的条件 |
| --- | --- |
| 思考 | `segments` 为空，或所有 segment 的 `text.trim()` 都为空 |
| 清单 | `plan` 为 `None`，或 `plan.steps` 为空 |
| 工具 | `traces` 为空（不是「全部失败」——失败也要显示） |
| 回答 | `answer.trim()` 为空且不在流式中 |

回答区在流式中即使为空也保留（显示「…」占位），因为它是必然存在的那一块。

### 6.6 布局与滚动

```text
┌─ ScrollArea (stick_to_bottom = true) ─────────────┐
│  [用户消息]                                       │
│  ┌─ AI 回复 ────────────────────────────────┐    │
│  │ 思考过程（折叠/展开）                     │    │
│  │ 任务清单（折叠/展开）                     │    │
│  │ 工具调用（折叠/展开，含卡片）             │    │
│  │ 回答（Markdown）                          │    │
│  └───────────────────────────────────────────┘    │
│  [用户消息]                                       │
│  ┌─ AI 回复（流式中）──────────────────────┐     │
│  │ …                                        │     │
│  └───────────────────────────────────────────┘    │
└───────────────────────────────────────────────────┘
```

- `stick_to_bottom` 在用户主动上滚后要**暂停**（egui 的 `stick_to_bottom` 已内置此行为，但要注意折叠区块的高度变化会触发跳动）；
- 折叠/展开用 `CollapsingHeader` + 稳定 `id_salt`（现有代码已用 `("ai_plan_done", plan.id)`，沿用）。

---

## 7. 变更清单

### 7.1 类型与事件

| 文件 | 变更 |
| --- | --- |
| `me-core/src/ai.rs` | 新增 `ThinkingBlock`、`ThinkingSegment`、`MAX_THINKING_CHARS`、`MAX_THINKING_SEGMENTS`；`AiMessage` 加 `thinking: Option<ThinkingBlock>`；`AiToolTrace` 加 `cards: Vec<MailCardRef>`；`AiDelta` 加 `Thinking(String)`；**trait `AiRepo::finish_assistant_message` 加 `thinking` 参数**（定义在 `ai.rs:2220`，不在 `port.rs`） |
| `me-core/src/command.rs` | 新增 `AiDeltaKind`；`AiStreamDelta` 加 `kind` + `segment`；**`AiToolCompleted` 加 `step_id: Option<u8>` / `attempt: Option<u8>`**（否则流式态显示不出「第 N 步」，见 §2.2） |
| `me-store/src/ai.rs` | `ToolTraceRow` 加 `cards`；`MESSAGE_COLUMNS` 加 `thinking`；**`map_message` 读新列**（漏了它会导致列在 SELECT 里但结构体拿不到值）；`finish_assistant_tx` 加 `thinking` 参数；新增 `checkpoint_assistant_message`（P1） |
| `me-store/src/migrations.rs` | `SCHEMA_VERSION` 7→8；`migrate_v7_to_v8`；DDL 加列 |
| `me-proto/src/ai.rs` | `apply_openai` 认 `reasoning_content` / `reasoning`（v1）；`apply_anthropic` 认 `thinking_delta`（P2） |
| `me-sync/src/ai.rs` | 双 throttle + **切段时刷残余**；思考分段与段号下发；trace 带 cards；`AiToolCompleted` 填 `step_id` / `attempt`；P1 检查点 |
| `me-app/src/views/ai.rs` | `TurnView` / `TurnCtx` / `render_turn`；新增 `show_thinking` / `show_tools`；`show_plan` 改为消息内渲染；**删除会话级的 `show_plan(ui, &state.ai.plan)` 调用、`show_cards`、以及 `show_activity` 的独立调用位**；渲染时跳过 `status == Streaming` 的占位行 |
| `me-app/src/app.rs` | `AiStreamDelta` 按 `kind` + `segment` 分派；`AiToolStarted` 写入 `live_tool`（不是伪造成 trace）；`AiToolCompleted` 带 `step_id` / `attempt`；`AiStreamStarted` 接住被 `..` 忽略的 `message` id |
| `me-app/src/views/ai.rs`（状态） | `AiStreamView` 加 `thinking: BTreeMap<u8, String>`、`live_tool: Option<LiveTool>`、`message: AiMessageId`；**`AiPageState.cards` 与 `.activity` 删除**（数据改从 trace / stream 读） |

**`Event` 变体总数不变**：`kind`、`segment`、`step_id`、`attempt` 都是给既有变体加字段，
不新增变体（§4.3 已说明为什么不拆成两个事件）。

#### 加字段会打断结构体字面量（真实工作量）

`#[serde(default)]` 只解决**反序列化**，解决不了 Rust 的结构体字面量。实测受影响处：

| 结构体 | 字面量构造点 | 说明 |
| --- | --- | --- |
| `AiMessage` | 11 处 | 含 `views/ai.rs` 的测试 helper `user_message()` |
| `AiToolTrace` | 9 处 | 含 `app.rs`、`me-sync` 的事件构造、测试 |

这些都要手工补 `thinking: None` / `cards: Vec::new()`。不是难题，但会占掉相当一部分
工时，**排期时要算进去**，否则会在「改完类型以为快好了」的时候撞上一堆编译错误。

如果嫌补字段烦，可以给 `AiToolTrace` 加 `..Default::default()` 式构造——但那要求实现
`Default`，而 `call_id` / `tool` 这类字段没有合理默认值，反而更糟。**直接补**是更好的选择。

### 7.2 设置项

**v1 不加任何设置项。**

理由：v1 只做 OpenAI 兼容端点的响应侧解析（§4.2.1），这条路径**不需要请求参数**——
DeepSeek-R1 这类模型的思考是自带的，给就展示、不给就没有。加一个「开启思考」的开关
反而会误导用户以为能控制它。

设置页只需要在模型表单里补一行**说明文字**（不是控件）：

```text
思考过程：当前仅在 OpenAI 兼容端点下展示（若模型提供 reasoning_content）。
Anthropic 需等待签名回传支持，暂不展示。
```

P2 支持 Anthropic 时再加字段：

```rust
/// Anthropic extended thinking 的预算（tokens）。0 = 不开启。
/// 只在 `kind == Anthropic` 时生效。
#[serde(default)]
pub thinking_budget: u32,
```

同时**必须**同步到 `AiProviderConfig`（`AiProviderConfig` 目前只有 `kind` / `base_url` /
`model` / `max_output_tokens` 四个字段，`AiModel::to_config()` 要把它带过去，
否则 `anthropic_body` 拿不到预算值）。保存时校验
`1024 <= budget < max_output_tokens`。

---

## 8. 测试计划

### 8.1 协议层（`me-proto`）

| 测试 | 期 | 断言 |
| --- | --- | --- |
| `openai_reasoning_content_reaches_sink` | P0 | `delta.reasoning_content` → `AiDelta::Thinking` |
| `openai_reasoning_fallback_field` | P0 | `delta.reasoning` 也能认 |
| `openai_reasoning_object_form_is_ignored_safely` | P0 | `delta.reasoning` 是对象时不 panic、不产生思考（见 §4.2.1） |
| `openai_without_reasoning_emits_no_thinking` | P0 | 只有 `content` 时不产生空 Thinking |
| `openai_text_and_thinking_do_not_mix` | P0 | 交错到达时两条通道各自正确 |
| `anthropic_thinking_delta_reaches_sink` | P2 | `thinking_delta` → `AiDelta::Thinking` |
| `anthropic_thinking_blocks_survive_tool_loop` | P2 | 带 signature 的 thinking block 原样回传（这是 P2 的真正难点） |

Anthropic 的两条**不在 P0 范围内**，见 §4.2.2——不是「顺手也测一下」的问题，
是整条路径在 P0 都不成立。

### 8.2 存储层（`me-store`）

| 测试 | 断言 |
| --- | --- |
| `thinking_round_trips` | 存 3 段 → 读回 3 段，顺序与内容一致 |
| `thinking_truncated_flag_round_trips` | `truncated = true` 存取一致（**如果没有这条，截断标记丢了也没人发现**） |
| `old_message_without_thinking_reads_none` | 手工插 NULL → 读出 `None`，不报错 |
| `empty_thinking_block_is_distinguishable_from_none` | `None`（没思考）与 `Some(空)`（有但空）能区分 |
| `tool_trace_cards_round_trip` | 带 2 张卡片的 trace 存读一致 |
| `old_tool_trace_without_cards_reads_empty` | 老 JSON 无 cards 键 → 空数组 |
| `migrate_v7_to_v8_adds_column` | v7 库升级后列存在，且 `SCHEMA_VERSION = 8` |
| `migrate_v7_to_v8_is_idempotent` | 重复迁移不报 duplicate column |
| `persisted_traces_are_always_terminal` | 落库/读出的 trace 状态不含「进行中」——守住 A3 那条不变量 |

### 8.3 引擎（`me-sync`）

| 测试 | 断言 |
| --- | --- |
| `thinking_is_segmented_per_tool_round` | 2 次工具调用 → 3 段（含末尾无工具的收尾段） |
| `thinking_truncates_at_limit` | 超长思考被截断且 `ThinkingBlock.truncated == true` |
| `segment_number_is_monotonic_and_never_goes_back` | 段号 1→2→3，不出现回退或跳号 |
| `throttle_residue_is_flushed_before_segment_switch` | 切段前 throttle 里的残余带着**旧**段号发出，不会跑到新段里去（§5.2） |
| `cards_ride_along_with_trace` | `email_search` 的 trace 带上了卡片 |
| `tool_completed_carries_step_and_attempt` | `AiToolCompleted` 事件带上 `step_id` / `attempt`（否则流式态显示不出「第 N 步」） |
| `stream_delta_carries_kind_and_segment` | 思考与回答增量分别标 `Thinking` / `Answer`；回答的 `segment` 恒为 0 |
| `seq_is_globally_monotonic_across_channels` | 两通道共享 seq，无回退 |

### 8.4 UI（`me-app`）

| 测试 | 断言 |
| --- | --- |
| `render_turn_orders_four_blocks` | 收集 shape 文本（复用现有 `collect_texts`），索引满足 思考 < 清单 < 工具 < 回答 |
| `empty_thinking_renders_nothing` | 无思考时输出里不含「思考过程」 |
| `blank_segments_do_not_render_header` | 全空白思考不画标题（不留空壳） |
| `historical_message_shows_its_own_plan` | 构造两条带不同 plan 的消息，各自显示自己的 |
| `streaming_and_historical_use_same_renderer` | 两者输出结构一致（除流式标记） |
| `thinking_collapsed_after_completion` | 完成后默认折叠 |
| `streaming_placeholder_produces_single_bubble` | `messages` 里放一条 `status = Streaming` 的空助手消息 + 一个 stream，渲染结果只有**一个**气泡（守住 B1） |
| `failed_message_still_renders` | `status = Failed` 的消息照常显示，不被上一条的过滤逻辑误伤 |
| `live_tool_shows_while_running` | `live_tool = Some` 时出现「正在调用 …」，完成后消失 |

### 8.5 回归

现有 792 条必须全绿。重点检查：

- `plan_survives_switching_away_and_back`（`plans` 缓存的语义变了）；
- `the_original_incident_now_produces_a_visible_answer`（回答仍然非空）；
- 所有 `AiStreamDelta` 的构造点（加字段后要编译通过）。

---

## 9. 分期落地

| 期 | 内容 | 可独立验收 |
| --- | --- | --- |
| **P0** | `ThinkingBlock` / `ThinkingSegment` + `thinking` 列 + v8 迁移；`AiDelta::Thinking`；**OpenAI 兼容的响应侧解析**；`AiStreamDelta` 加 `kind` + `segment`；`AiToolCompleted` 加 `step_id` / `attempt`；四区块 `TurnView` / `TurnCtx` / `render_turn`；历史清单入消息；顺序修正；**跳过流式占位行** | 是：思考能显示、历史能回看、顺序正确、流式只有一个气泡 |
| **P1** | 卡片随 trace 落库；`live_tool` 实时显示；流式 Markdown 节流；流式检查点落库 | 是：历史能看到当时检索的邮件、工具执行期间有反馈 |
| **P2** | **Anthropic 思考支持**（前置：`signature` 透传，需 `AiWireMessage::Assistant` 扩展 —— 不做这个，Anthropic 一开 thinking 就 400，见 §4.2.2）；`thinking_budget` 设置项；历史分页加载（`before` 游标） | 是：Anthropic 端点也能看到思考；长对话可翻看早期内容 |

P0 是「能不能用」，P1 是「好不好用」，P2 是「完整性」。

**P2 的第一项不是可选项而是前置条件**：它和「历史分页」不是同一性质的工作——分页是增强，
Anthropic 签名透传是**解锁一个端点类别**。如果用户主要用 Claude，实际要等到 P2 才有思考展示，
这个预期要在文档和设置页里说清楚，不能等到用户问「为什么我的 Claude 没有思考」才解释。

---

## 10. 风险与未决问题

### 10.1 已识别风险

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| 误以为 Anthropic 也能用 | 用户开了发现没反应，或直接 400 | v1 只在设置页说明「仅 OpenAI 兼容端点」；P2 前不要给 Anthropic 加 `thinking` 参数 |
| Anthropic `thinking_budget` 违反约束（P2） | 请求 400 | 保存时校验 `1024 <= budget < max_output_tokens` |
| 思考过长拖慢渲染 | 滚动卡顿 | 截断到 32 KiB + 折叠 + 不跑 Markdown |
| 截断导致「流式时很长、重载后变短」 | 观感倒退，用户以为丢数据 | `truncated` 落库 + 标题显示「（已截断）」+ 末尾灰字说明原因 |
| 卡片引用的邮件被删 | 历史显示空白 | 查不到时降级为「邮件已删除」，不报错 |
| 检查点写放大 | SQLite 锁竞争 | 1s 节流 + 仅内容变化时写；P1 再做 |
| 折叠区块高度变化导致滚动跳动 | 体验 | `stick_to_bottom` + 展开时不自动滚到底（用户点开某块时不应被拉走） |
| 过滤占位行时误伤终态消息 | 失败/取消的消息看不见 | 只跳 `status == Streaming`，并有 `failed_message_still_renders` 测试守住 |

### 10.2 未决问题

1. **「过程叙述」归哪一块？** 模型在工具调用前常说「我先查一下」。现在这段文本混在 `content` 里，会出现在回答区顶部，读起来像「回答的第一句是废话」。彻底解决需要引擎区分「有工具调用的 turn 的文本」与「最后一个 turn 的文本」，分别存 `narration` / `answer`。**v1 不做**（会增加一个字段与一套回灌逻辑），但要在文档里留痕：如果用户体验反馈强烈，再补。

2. **思考内容是否要参与搜索？** `ai_messages` **目前不在 FTS 索引内**（迁移里没有为它建 FTS 表或触发器），所以现在无论 `content` 还是将来的 `thinking` 都搜不到。如果以后给 AI 对话加搜索，建议**只索引 `content`，不索引 `thinking`**——思考里大量重复的推理措辞会把命中率稀释掉。

3. **历史分页的交互**：`list_messages_conn` 支持 `before` 游标，但 UI 没有「加载更早」的入口。这属于 P2，需要先定交互（滚动到顶自动加载 vs 显式按钮）。

4. **同一条消息里会不会出现「分段」和「整段」混排？** v1 不会——只有 OpenAI 兼容端点
   产出思考，而它们都是连续的增量流，引擎统一按工具轮次切段后必然是分段的。
   真正会混排的是 P2 支持 Anthropic 之后（它的 thinking block 边界由协议决定），
   但那时落库的仍是统一的 `Vec<ThinkingSegment>`，渲染层不需要特判——这正是
   存结构化而不是存纯文本换来的好处。

---

## 11. 与既有设计的关系

- **不改变** `PLAN → EXECUTE → REFLECT → REPLAN → FINAL` 五阶段引擎（见 `doc/AI设计/AItodolist功能实现.md`）；
- **不改变** `Plan v2` 的结构与校验；
- **不改变** 工具白名单与 `criteria` 判据机制；
- **不改变** 上下文裁剪策略（`trim_history` 仍只读 `content`）。

这次重构的边界很清晰：**只补「一块缺失的数据（思考）」+「一处错位的渲染（四区顺序）」+「一个漏掉的持久化（卡片）」**，不碰决策逻辑。

---

## 12. 变更记录

| 日期 | 内容 |
| --- | --- |
| 2026-11-07 | 初版。基于当前代码实测盘点 D1–D6 六个缺陷，提出四区块 `TurnView` 渲染器、思考双通道流式、schema v8 |
| 2026-11-07 | 实施完成（6 次提交，792 → 821），补 §13 实施状态、七处偏差与未做项 |
| 2026-11-07 | 审查修正（9 处）。A1 删除「Anthropic 无工具调用 ✅」这一不存在的路径，Anthropic 整体改列 P2；A2 `render_turn` 改为接 `TurnCtx` 而非 `&mut UiState`（原签名自引用借用冲突，编译不过）；A3 `status = Running` 改为 UI 侧 `LiveTool`（不污染持久化枚举）；B1 补上「数据库流式占位行会被渲染成第二个气泡」这一半；B2 `AiToolCompleted` 补 `step_id` / `attempt`；B3 `thinking` 列改存 `ThinkingBlock` 对象以带上 `truncated`；B4 `AiStreamDelta` 补 `segment` 让流式期间能分段；B5 变更清单补上删除 `show_cards` / `state.ai.cards` / `state.ai.activity`；C1–C8 修正注释理由、补齐迁移实现、补 trait 签名与 `map_message`、补字面量成本、同步 `AiToolTrace` 注释、把 `../crates` 改为 `../../crates` |
| 2026-11-07 | 限流事故修复（见 [《AI限流事故复盘》](./AI限流事故复盘.md)）。同一条提问连跑两次都因 HTTP 429 失败，而模型两次都已查到 20 封邮件。根因是 PLAN 阶段模型看不到业务工具参数表 → `plan_submit` 连续 5 次被拒 → 请求密度顶到限流线。修四处：PLAN 阶段下发参数表、参数宽容修复、PLAN 拒绝闸门、请求间隔 + 429 退避重试 |

---

## 13. 实施状态（2026-11-07）

**已全部落地**：P0 五批 + 流式检查点，共 6 次提交。
测试 **792 → 821 passed / 0 failed / 6 ignored**；`cargo fmt --check` 与
`cargo clippy --workspace --all-targets -- -D warnings` 全通过。

| 提交 | 内容 |
| --- | --- |
| `feb3a19` | 批次 1：`ThinkingBlock` / `AiMessage.thinking` / `AiToolTrace.cards` / `AiDelta::Thinking` / `AiDeltaKind` / schema v8 与 `migrate_v7_to_v8` / `finish_assistant_message` 加参数 |
| `49dddb9` | 批次 2：`apply_openai` 认 `reasoning_content` / `reasoning`（v1 不动 Anthropic，见 A1） |
| `ac5a14d` | 批次 3：`DeltaThrottle` 剥 seq + `flush()`；双通道共享 `AtomicU64`；`ThinkingAccumulator` 分段/截断；卡片随 trace 落库；`AiToolCompleted` 填真实步号 |
| `b4d3fa9` | 批次 4+5：`TurnView` / `TurnCtx` / `render_turn` 四区统一渲染；`LiveTool`；流式 `thinking` 分桶；删除四处会话级渲染；修 `card_messages` |
| `565c88b` | 检查点：`checkpoint_assistant_message` + 每轮写一次，抗中途崩溃 |

### 13.1 与方案的偏差（都是实现期实测后调整的）

| # | 方案 | 实际 | 理由 |
| --- | --- | --- | --- |
| 1 | `TurnView` 由「两个构造点」分别组装 | 统一持 `&AiMessage`，流式那条构造一份临时 `AiMessage` | `TurnView` 四块全是 assistant 语义，装不下 user 的 `context_refs`；保留独立 `show_user_message`，历史零拷贝借用，流式每帧只 clone 一次（§开发准备 E2/E3） |
| 2 | 检查点 1s 节流 | **每轮模型返回后写一次** | 轮数上限就是 6，最多 6 次写比定时器写放大更小，却正好覆盖「长工具循环 + 中途断电」 |
| 3 | 删除 `state.ai.activity` / `cards` | 两个都**保留** | 流式期间 `AiToolCompleted` 先于落库到达，没有内存位就显示不出刚调过什么；`cards` / `card_messages` 是 reader 打开邮件的摘要缓存（§开发准备 §2.3 ④、E11） |
| 4 | `LiveTool` 在工具区显示 | 工具区有 live_tool 时**默认展开** | 默认折叠时「执行中…」藏在里面，等于没显示；测试 `live_tool_shows_while_running` 抓到了这一点 |
| 5 | 未提 | 折叠区 `id_salt` 按 `message_id` 隔离 | egui 是 immediate mode，共用 salt 会让所有历史消息共享同一个折叠状态 |
| 6 | 「沿用现有 `show_cards` 逻辑」 | 该逻辑是**坏的**：`card_messages` 从未被写入 | `reader.rs:32` 的回退分支永远命中不了 → 点卡片打开不在当前列表的邮件时阅读窗立刻关。顺手修了，并在 `refresh_ai_messages` 里补历史卡片摘要 |
| 7 | 未提 | `AiCard` 删掉 `reason` / `from` / `date` | 命中理由现在随 `MailCardRef` 落在 trace 上，两边各存一份必然漂移 |

### 13.2 明确未做（方案里就列为可选 / P2）

- **流式 Markdown 节流**（P1 后半）：流式期间仍是纯文本直出，结束后解析一次并缓存。这是体验优化，不是硬要求；纯文本反而更稳。
- **Anthropic 思考**（P2）：A1 已证明 PLAN 阶段恒下发两个工具 → 必然产生 `tool_use` → 同轮必须原样回传带 `signature` 的 thinking block，否则 400。v1 请求侧不开 thinking，服务端也不会返回 `thinking_delta`，所以只做响应侧 OpenAI 兼容解析。要解锁需先把 signature 从协议层透传到 `AiWireMessage::Assistant`。
- **历史分页**（P2）：`list_messages_conn` 支持 `before` 游标，UI 仍是固定 200 条。需要先定交互（滚动到顶自动加载 vs 显式按钮）。
- **过程叙述归块**（§10.2 未决 1）：工具调用前的「我先查一下」仍混在 `content` 里，出现在回答区顶部。彻底解决要引擎区分 `narration` / `answer` 两个字段。

### 13.3 四条硬要求的落点

| 要求 | 落点 |
| --- | --- |
| 四部分由上到下、没有就不显示 | `render_turn` 单点定顺序；每块 `Option` / 空数组则不画。`missing_blocks_render_nothing` 守着 |
| 流式实时传输 | `AiStreamDelta.kind` 双通道 + `segment` 分桶；`live_tool` 让工具执行期间有反馈；检查点让内容边生成边落库 |
| 每轮全部内容持久化 | `ai_messages.thinking` + `tool_trace.cards` + `plan`（已有）；schema v8 迁移；检查点抗崩溃 |
| 点击历史记录可显示 | 历史与实时共用 `render_turn`；历史消息自带四块数据；卡片摘要在 `refresh_ai_messages` 里补回；`historical_message_shows_its_own_plan` 守着 |
