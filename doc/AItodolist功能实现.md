# AI Todolist（强制任务清单）功能实现方案

> 目标版本：v0.5（暂定 AIT-1 ~ AIT-4 四期）
> 事实基线：v0.4.0 / schema v7 / `cargo test --workspace` 710 passed
> 本文与代码冲突时以本文为准，落地后由本文回写代码索引。

---

## 0. 摘要

现状的「任务清单」（`plan_update`）是**模型自愿使用的展示性清单**：只有 `title` 与 `status`，
不描述执行方式，不约束工具调用，不设重试，没有执行后的自我评估，也没有重列机制。
它的触发条件甚至被写反了——系统提示与工具描述都明确要求「一句话能答完的事情不要建清单」。

本文把它改造成**引擎强制的 Plan → Execute → Reflect → Replan → Answer 闭环**：

| 用户要求 | 本文机制 |
| --- | --- |
| 拿到需求后**强制**列 todolist | 首轮工具白名单只下发 `plan_submit`（物理约束，不是提示词建议） |
| 清单包含任务、是否调工具、调什么参数、期望结果 | `Plan v2`：`kind` / `tool` / `args` / `expect` 四个新字段 |
| 每步 3 次重试 | `MAX_STEP_ATTEMPTS = 3`，按错误类别分级，同参数同错误熔断 |
| 结果回灌模型判断是否完成 | `REFLECT` 阶段 + 结构化 `SelfCheck`，**事实优先**（模型只能降级不能虚报） |
| 未完成则诊断并重列，上限 2 次 | `REPLAN` 阶段，`MAX_REPLANS = 2`，必须带 `diagnosis`，禁止重复失败组合 |
| 最后输出结果 | `FINAL` 阶段 + 空正文守卫 + 引擎兜底摘要 |
| 清单动作可见、过程实时更新 | 引擎持有清单所有权，每步状态变化即发事件；清单条 v2 显示工具/参数/尝试次数 |

三个必须先说清的判断：

1. **提示词治不了这个病。** `SYSTEM_PROMPT` 第 10 条（[ai.rs:1342](../crates/me-core/src/ai.rs)）
   早就写了「需要 2 步以上先建清单」，那一轮依然没建——因为同一条规则的后半句
   「一句话能答完的事情不要建清单」给了模型一个合法出口。要求「强制」时，
   只能靠**下发哪些工具**来约束，不能靠**说了什么话**来约束。
2. **清单的所有权必须从模型转移到引擎。** 现状是模型调 `plan_update` 才更新，
   所以「实时更新」的上限就是模型愿意调几次。v2 里步骤状态由引擎在执行工具后直接改写，
   模型只在「跳步 / 改判 / 放弃」时介入。
3. **重试必须分类，否则就是刷调用。** 那次 8 连击里 6 次是同一个参数校验错误
   （`Validation`，`is_retryable() == false`，见 [error.rs:246](../crates/me-core/src/error.rs)）。
   对不可重试错误原样重发，不叫重试，叫空转。

---

## 1. 问题复盘：那一轮到底发生了什么

### 1.1 真实数据

来源：`%APPDATA%\MyEmail\myemail.db` 的 `ai_messages`（只读查询），conversation 2。

| 消息 | role | 内容 / 轨迹 |
| --- | --- | --- |
| id=29 | user | `有什么重要的邮件` |
| id=30 | assistant | `content` = 13 个换行符（**空回复**）；`plan` = `NULL`；`status` = `complete`；`input_tokens` 6573 |

`tool_trace`（8 条，全部 `email_search`，`motive_source` 全是 `model`）：

| # | status | summary | motive |
| --- | --- | --- | --- |
| 1 | error | 至少需要一个检索条件 | 查看重要邮件 |
| 2 | error | 至少需要一个检索条件 | 查看重要邮件，未读优先 |
| 3 | error | 至少需要一个检索条件 | 查看近期未读重要邮件 |
| 4 | ok | 未找到 | 查看近期重要邮件 |
| 5 | error | 至少需要一个检索条件 | 查看近期重要邮件 |
| 6 | error | 至少需要一个检索条件 | 查看重要未读邮件 |
| 7 | ok | 检索到 4 封 | 查看重要邮件，未读优先 |
| 8 | error | 至少需要一个检索条件 | 查看招聘相关重要邮件 |

结果：用户得到一个空白的回答，界面上只有一串「检索邮件」活动条。

### 1.2 六个缺陷（按严重度排序）

**D1 · 契约缺口：`email_search` 的 schema 允许一种实现必然拒绝的调用。**

schema（[ai.rs:1608](../crates/me-core/src/ai.rs)）只把 `motive` 设为必填，
`after` / `before` / `folder` / `unread_only` / `limit` 全都可以单独出现；
实现（[ai_tools.rs:366](../crates/me-sync/src/ai_tools.rs)）却要求
`query | from | subject | to` 至少有一个：

```rust
if query_text.is_none() && from.is_none() && subject.is_none() && to.is_none() {
    return Err(CoreError::validation("至少需要一个检索条件"));
}
```

模型很自然地写「查近 7 天未读」——`unread_only: true` + `after`，完全合法，
但必然报错。而且报错文案不告诉它缺的是哪一类条件，于是它换个说法再来一次。
**8 次调用里有 6 次死在这个错上。**

**D2 · 「重要」在系统里没有语义。**
`SearchQuery`（[model.rs:1036](../crates/me-core/src/model.rs)）有 `since` / `until` /
`unread_only`，**没有星标（`FLAG_FLAGGED`）条件**；搜索语法（`search_syntax.rs`）
只有 `is:unread`，没有 `is:flagged`。用户说的「重要」在可用工具里根本无法表达，
模型只能瞎猜关键词，于是猜到了「招聘」这种窄口径。

> **补充（本次改造中修正的认知）**：这一条的真实病灶不是「少了一个 `is:flagged`」，
> 而是**「重要」这类词根本不该由系统来定义**。补上星标是必要的（它是本地唯一
> 的显式标注），但把它和「近 7 天未读」绑成一个 `important_only` 预置条件，
> 就等于替所有用户决定了「什么算重要」——那不是修复，是把一个例子的答案
> 抄进系统。正确的做法见 §12：给模型**通用机制**（判据），
> 让它自己把定性词翻译成条件，而客观维度（星标、未读、时间…）才是一等过滤条件。

**D3 · 清单的触发条件被写反。**
`plan_update` 的描述（[ai.rs:1811](../crates/me-core/src/ai.rs)）：
「只有需要 **2 步以上**或跨模块的需求才建清单；一句话能答完的事情**不要**建」。
`SYSTEM_PROMPT` 第 10 条同义复述。模型判断「有什么重要的邮件」是一句话 → 不建清单 →
引擎的 `nudge` 机制（[ai.rs:1185](../crates/me-sync/src/ai.rs)）因为 `plan.is_none()` 而完全不生效。
于是整轮没有任何「你在做什么、做到哪一步、还剩几步」的结构。

**D4 · 没有重试语义，也没有熔断。**
唯一的刹车是 `MAX_TOOL_CALLS = 12`（[ai.rs:72](../crates/me-core/src/ai.rs)）。
模型在 12 次预算里用 8 次重复同一个错误调用，引擎全盘接受。
系统提示第 8 条写着「同一个失败调用最多重试一次」，这是**提示**，不是**机制**。

**D5 · 空回复被当成成功。**
`turn.tool_calls.is_empty()` 时直接 `break`（[ai.rs:1182](../crates/me-sync/src/ai.rs)），
随后 `finish_assistant_message` 原样落库。`content` 是 13 个 `\n`，`status` 是 `complete`，
用户界面上是一块空白。**没有任何一处校验「模型是否真的回答了用户」。**

**D6 · `Plan` 不足以表达一份可执行计划。**
`PlanStep`（[ai_plan.rs:121](../crates/me-core/src/ai_plan.rs)）只有
`id / capability / title / status / evidence`。它回答不了用户最关心的四个问题：
这步**要不要调工具**、调**哪个**、传**什么参数**、**期待什么结果**。
所以即使模型建了清单，那也只是一个目录，引擎无法据此校验、重试或诊断。

### 1.3 一句话结论

那次失败不是「模型笨」，是**系统没有给它一份可执行的契约**：
能表达「重要」的工具不存在，能约束执行的清单不存在，能拦住空转的熔断不存在，
能发现空回答的守卫不存在。补上这四样，同样的问题就会走向另一条路径（见 §12）。

---

## 2. 需求转译

把用户原话逐条转成可实现、可验收的机制。

| # | 用户原话 | 机制 | 落点 | 验收判据 |
| --- | --- | --- | --- | --- |
| R1 | 拿到需求后强制列出 todolist | 阶段机 + 首轮工具白名单 | `me-sync/src/ai.rs` 新增 `Phase` | 任务型请求在第一次业务工具调用前必有 `plan`；无 `plan` 的业务调用被拒 |
| R2 | 清单包含任务清单 | `Plan.steps[].title` | `me-core/src/ai_plan.rs` | 1–8 步，标题动词开头 ≤40 字 |
| R3 | 每一步要不要调用工具 | `PlanStep.kind` ∈ `tool / reason / ask` | 同上 | `reason` 步不允许出现工具调用 |
| R4 | 调用工具时传入什么参数 | `PlanStep.args`（JSON 模板，可含 `$ref`） | 同上 | 执行前用真实 schema 校验；非法参数在**计划期**就被拦下 |
| R5 | 期望的结果是什么 | `PlanStep.expect`（可验收的一句话） | 同上 | `reflect` 阶段逐条比对；不符可回退重做 |
| R6 | 每一步有 3 次重试机会 | `MAX_STEP_ATTEMPTS = 3` + 错误分类 + 指纹熔断 | `me-sync/src/ai_tools.rs` | 单步最多 3 次；参数错误时同参数最多执行 1 次，第 3 次直接判该步失败 |
| R7 | 执行完把结果再给模型判断是否完成 | `REFLECT` 阶段 + `SelfCheck` JSON | `me-sync/src/ai.rs` | 每轮必有 `verdict`；与事实冲突时一律降级为 `partial`，不得虚报完成 |
| R8 | 未完成要诊断哪一步出问题 / 思路全错 | `SelfCheck.diagnosis` 枚举 + `failed_step` | `me-core/src/ai_plan.rs` | 枚举五选一，缺 `diagnosis` 拒收 |
| R9 | 重新列清单，上限 2 次 | `MAX_REPLANS = 2` + `ReplanRecord` | 同上 | 第 3 次 replan 请求返回「已达上限」，直接 FINAL |
| R10 | 最后输出结果 | `FINAL` 阶段 + 空正文守卫 | `me-sync/src/ai.rs` | 正文非空；空则兜底摘要并写 `error` |
| R11 | 列清单的动作要输出给用户 | `Event::AiPlanUpdated` 在执行前发出 | `me-core/src/command.rs` | 首次工具调用前 UI 已渲染清单条 |
| R12 | 过程中实时更新完成情况 | 引擎持有清单所有权，状态变化即发事件 | 同上 | 每步 ≤200 ms 内反映到界面 |

R1 与 R11 有一处容易做错：现在 `plan` 只在**落库**时写 `ai_messages.plan`，
流式期间靠 `Event::AiPlanUpdated` 送内存。用户切页再回来（`AiPageState::select` 会
`self.plan = None`，[views/ai.rs:261](../crates/me-app/src/views/ai.rs)）清单就消失了。
v2 要保持「流式不落库」的不变量，但**清单快照不能随切会话清空**——它是本轮的过程状态，
不是会话内容。详见 §10.3。

---

## 3. 总体设计

### 3.1 状态机

```text
                    ┌──────────────────────────────────────────┐
                    │                                          │
  user turn ──► PLAN ──► EXECUTE ──► REFLECT ──► FINAL ──► 落库 │
                 │         ▲  │         │                        │
                 │         │  │         │ verdict=failed         │
                 │         │  │         │ 且 replans < 2         │
                 │         │  └─────────┴────────► REPLAN ───────┘
                 │         │                        （重建清单后回 EXECUTE）
                 │         │
                 │         └── 单步内部：attempt 1 → 2 → 3（≤3）
                 │
                 └── answer_directly（显式豁免，记 bypass 原因）
```

五个阶段，每个阶段有独立的**入口条件、出口条件、预算**：

| 阶段 | 入口 | 出口 | 子预算 |
| --- | --- | --- | --- |
| `PLAN` | 用户消息 | 拿到合法 `Plan`，或 `answer_directly` | 60 s / 2 轮 |
| `EXECUTE` | 有 `Plan` 且存在未终结步 | 所有步终结，或工具预算/时间耗尽 | 300 s / 12 轮（全局轮次共享，见 §11） |
| `REFLECT` | `EXECUTE` 结束 | 拿到 `SelfCheck` | 45 s / 1 轮（+1 次重问） |
| `REPLAN` | `verdict = failed` 且 `replans < 2` | 新 `Plan`（旧计划进 history） | 计入下一轮 EXECUTE |
| `FINAL` | `verdict ∈ {done, partial}` 或预算耗尽 | 非空正文 | 45 s / 2 轮（含 1 次空正文重问） |

**为什么不是「一轮里顺手把计划也做了」**：模型一边规划一边执行时，计划会退化成
「我刚才做了什么」的记录，而不是「我打算做什么」的契约，`expect` 也就无从校验。
PLAN 独占一轮，代价是 1 次模型往返，换到的是一份可以拿来对账的清单。

### 3.2 分层落位（不破依赖方向）

```text
me-core/src/ai_plan.rs    Plan v2 结构 + 全部纯函数判定（状态机、指纹、replan 校验）
me-core/src/ai.rs         plan_submit / answer_directly 规格、step_id 注入、SYSTEM_PROMPT
me-core/src/command.rs    新增 4 个清单事件
me-core/src/model.rs      SearchQuery 加 flagged_only
me-sync/src/ai.rs         五阶段驱动器、重试执行器、空正文守卫、事件发送
me-sync/src/ai_tools.rs   工具错误分类（ToolFailure）、email_search 契约对齐
me-store/src/search.rs    flagged_only 的 SQL 条件
me-app/src/views/ai.rs    清单条 v2
```

`me-core` 只新增纯函数，不引入 tokio / 时间读取（`now` 由调用方传入，沿用
`search_syntax` 的既有约定）；`me-sync` 负责编排；`me-app` 只渲染事件。

### 3.3 不变量（沿用 + 新增）

沿用：

1. 流式期间只写内存与事件，终止时一次性落库（文本 / plan / traces / 用量）。
2. 终止事件不携带全文，UI 收到后从数据库重载。
3. 工具调用与最终回复不是同一事务。

新增（v2 专属，破一条就会出事故）：

4. **一份消息最多一份活跃清单**；replan 后旧清单进 `history`，不并行。
5. **写操作永不自动重试**（`calendar_create` 重试 3 次会造 3 条日程）。
6. **事实优先**：`SelfCheck` 只能把 `done` 降级，不能把 `failed` 升为 `done`。
7. **计划是不可信输入**：`tool` 必须过白名单，`args` 必须过 schema，
   `args` 里的 `$ref` 只能引用结构化字段（id / count / date），不能引用正文文本。

---

## 4. 数据结构：Plan v2

### 4.1 Rust 定义

```rust
// crates/me-core/src/ai_plan.rs

pub const PLAN_VERSION_V1: u8 = 1;
pub const PLAN_VERSION_V2: u8 = 2;

pub const MAX_PLAN_STEPS: usize = 8;          // 6 → 8：覆盖"整理重要邮件"这类真实需求
pub const MAX_STEP_ATTEMPTS: u8 = 3;          // R6
pub const MAX_REPLANS: u8 = 2;                // R9
pub const MAX_STEP_ARGS_BYTES: usize = 1024;  // 单步参数模板上限
pub const MAX_PLAN_HISTORY: usize = 2;        // history 最多留 2 份，防止 JSON 膨胀

/// 这一步「做什么」。决定引擎在执行期允许什么样的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// 调一个工具。`tool` 必填。
    Tool,
    /// 纯推理：归纳、排序、写理由。**不允许**出现工具调用。
    Reason,
    /// 需要用户补充信息才能继续。执行到这一步即暂停整轮，等用户回答。
    Ask,
}

/// 参数模板。字符串里可以出现 `$ref`：
///   {{step.1.messages.0.message_id}}
/// 只允许取结构化字段，见 `bind_args`。
pub type StepArgs = serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub id: u8,
    pub title: String,
    pub kind: StepKind,
    pub capability: Capability,
    /// `kind == Tool` 时必填；其余必须为 None（校验期强制）。
    #[serde(default)]
    pub tool: Option<AiToolName>,
    /// 参数模板；`None` 表示"执行时由模型按上下文填"。
    #[serde(default)]
    pub args: Option<StepArgs>,
    /// 期望结果：一句可验收的话。reflect 阶段拿它和 evidence 比对。
    #[serde(default)]
    pub expect: Option<String>,
    pub status: PlanStatus,
    /// 已尝试次数，0..=MAX_STEP_ATTEMPTS。**只增不减**。
    #[serde(default)]
    pub attempts: u8,
    #[serde(default)]
    pub evidence: Option<String>,
    /// 最近一次失败的结构化原因（给 reflect 与 UI 用）。
    #[serde(default)]
    pub failure: Option<StepFailure>,
    /// 0 命中时的策略，见 §6.4。
    #[serde(default)]
    pub on_empty: OnEmpty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepFailure {
    /// 参数不合法（不可重试，必须改参数）
    InvalidArgs { detail: String },
    /// 同参数同错误已出现 2 次，熔断
    RepeatedCall { fingerprint: String },
    /// 工具/网络临时错误，重试用尽
    Transient { detail: String },
    /// 权限未开启或被执行期拒绝
    Denied { tool: String },
    /// 结果不满足 expect（reflect 判定，不是工具报错）
    ExpectMiss { expect: String, got: String },
    /// 依赖的前序步骤失败
    Upstream,
    /// 时间或次数预算耗尽
    Budget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnEmpty {
    /// 缩小条件再来一次（用掉一次 attempt）
    #[default]
    Narrow,
    /// 停下来问用户
    Ask,
    /// 接受空结果，继续下一步
    Accept,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplanRecord {
    pub round: u8,
    /// 模型给的诊断：五选一，见 SelfCheck
    pub diagnosis: Diagnosis,
    /// 一句话说清「这一轮错在哪」
    pub reason: String,
    /// 一句话说清「这次改了什么」
    pub changes: String,
    /// 上一份清单里失败/跳过的步骤编号
    pub failed_steps: Vec<u8>,
    pub at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// 缺省 = 1，老数据按 v1 渲染。
    #[serde(default = "plan_version_default")]
    pub version: u8,
    #[serde(default)]
    pub id: String,
    pub goal: String,
    pub steps: Vec<PlanStep>,
    #[serde(default)]
    pub replans: Vec<ReplanRecord>,
    /// 被本份清单取代的历史清单（最多 MAX_PLAN_HISTORY 份）。
    #[serde(default)]
    pub history: Vec<Plan>,
    pub created_at: i64,
    pub updated_at: i64,
}

fn plan_version_default() -> u8 { PLAN_VERSION_V1 }
```

两点实现提示：

- `ai_plan.rs` 需要 `use crate::ai::AiToolName`，而 `ai.rs` 已经/将要 `use crate::ai_plan::Plan`。
  同一 crate 内的模块互相引用是合法的，**不要**为了「解耦」把 `AiToolName` 复制一份到
  `ai_plan`——两份枚举迟早不同步，而 `match` 不会报错。
- `StepFailure` 里的 `detail` 一律截断到 80 字再存。它会进 `plan` JSON、进事件、进 UI，
  存工具原始错误会把整段响应体带进数据库（`MAX_TOOL_RESULT_CHARS` 是给回灌用的，不是给落库用的）。

### 4.2 JSON 示例（「有什么重要的邮件」应该长这样）

```json
{
  "version": 2,
  "id": "p_1762500000",
  "goal": "找出当前邮箱里值得优先处理的邮件，并说明为什么",
  "steps": [
    {
      "id": 1,
      "title": "按判据检索候选邮件",
      "kind": "tool",
      "capability": "mail",
      "tool": "email_search",
      "args": {
        "criteria": [
          {"label": "已星标", "flagged_only": true},
          {"label": "近 7 天未读", "unread_only": true, "days": 7},
          {"label": "主题含某个关键词", "subject": "某个关键词"}
        ],
        "limit": 10
      },
      "expect": "按三条判据检索，每封标注命中了哪一条，0 封也要说明",
      "status": "running",
      "attempts": 1,
      "on_empty": "narrow"
    },
    {
      "id": 2,
      "title": "读前 3 封的正文要点",
      "kind": "tool",
      "capability": "mail",
      "tool": "mail_read",
      "args": { "message_id": "{{step.1.messages.0.message_id}}" },
      "expect": "每封拿到 1–2 句要点与是否需要行动",
      "status": "pending",
      "attempts": 0
    },
    {
      "id": 3,
      "title": "归纳并标注优先级",
      "kind": "reason",
      "capability": "mail",
      "expect": "按优先级给出最多 5 封，每封一句理由并写明命中哪条判据",
      "status": "pending",
      "attempts": 0
    }
  ],
  "replans": [],
  "history": [],
  "created_at": 1762500000,
  "updated_at": 1762500006
}
```

这份清单本身就是对用户需求的**显式理解**：判据写在第 1 步的 `args.criteria` 里，
每条带一个能让人看懂的名字，「最多 5 封、每封一句理由」写在最后一步的 `expect` 里。
用户看到它就能在第一步发现「我不认为未读等于重要」，而不是等到最后收到一份跑偏的答案。

**判据的顺序即优先级**，引擎按这个顺序合并与排序——这是模型对需求理解的直接体现。

### 4.3 兼容性：不需要 schema v8

`ai_messages.plan` 是 TEXT 列（[migrations.rs:307](../crates/me-store/src/migrations.rs)，
v4→v5 加入），存的是 `Plan` 的 JSON。v2 只增加字段且全部带 `#[serde(default)]`：

- 老数据缺 `version` → 默认 1，UI 走 v1 分支（只画 title/status）。
- 老数据 `steps[].tool` 为 `None`、`kind` 缺省 → 默认值取 `Tool`？
  **不能**。取 `Tool` 会让老清单被引擎当成可执行计划。正确做法是给 `kind`
  一个显式的 `#[serde(default)]` 指向新枚举值 `Legacy`，或干脆在解析时按
  `version == 1` 强制视为「只读展示」，不参与任何执行判定。本文取后者：
  `Plan::is_executable()` 要求 `version >= 2`。
- `parse_plan`（[ai.rs:89](../crates/me-store/src/ai.rs)）解析失败按「无清单」处理，
  这个兜底保留。

结论：**零迁移**。这是把 plan 做成 JSON 列而不是独立表带来的直接收益。

---

## 5. PLAN 阶段：怎么做到「强制」

### 5.1 为什么不能只写提示词

三条证据：

1. `SYSTEM_PROMPT` 第 10 条已经要求 2 步以上建清单，那一轮没建。
2. 同一条规则的后半句**主动豁免**了「一句话能答完的事」，模型据此认为不需要清单。
3. `plan_update` 的 schema 里 `steps` 是唯一必填项，`goal` 甚至可以不传（后续调用），
   所以「计划」在协议层是一个**可选动作**。

协议层的可选动作，不可能靠提示词变成必然动作。

### 5.2 机制：首轮工具白名单

PLAN 阶段发出的请求里，`tools` 只有两个：

```rust
// me-core/src/ai.rs
/// PLAN 阶段只下发「建计划」与「显式豁免」两个工具。
/// **不经 `AiToolPermissions` 过滤**：它们是引擎机制，不是用户可关的能力。
pub fn plan_phase_tool_specs() -> Vec<AiToolSpec> {
    TOOL_SPECS
        .iter()
        .filter(|s| matches!(s.name, AiToolName::PlanSubmit | AiToolName::AnswerDirectly))
        .cloned()
        .collect()
}
```

这样模型在物理上没有 `email_search` 可调——它想查邮件，就必须先说要查什么。
这是整个方案里最关键的一处设计：**用「你能做什么」代替「你应该做什么」**。

两处必须同步的细节：

1. `AiToolPermissions::enabled()`（[ai.rs:1301](../crates/me-core/src/ai.rs)）是**穷尽 match**，
   新增两个 `AiToolName` 变体会直接编译失败——这是好事，它强制实现者决定
   「这个工具归不归用户开关管」。两个都返回 `true`（与 `PlanUpdate => true` 同理由：
   清单是引擎机制，关掉它模型就没法把多步需求说清楚）。
2. `is_bookkeeping()` 要把三个都纳入（`PlanSubmit` / `PlanUpdate` / `AnswerDirectly`），
   否则 PLAN 的那一次调用会吃掉 `MAX_TOOL_CALLS` 的额度，8 步计划还没开始就少一次。

`plan_submit` 的 schema：

```json
{
  "type": "object",
  "properties": {
    "motive": {"type": "string", "minLength": 4, "maxLength": 200},
    "goal": {"type": "string", "minLength": 4, "maxLength": 120,
             "description": "一句话复述用户要的成果，必须可验收"},
    "steps": {
      "type": "array", "minItems": 1, "maxItems": 8,
      "items": {
        "type": "object",
        "properties": {
          "id": {"type": "integer", "minimum": 1, "maximum": 8},
          "title": {"type": "string", "maxLength": 40},
          "kind": {"type": "string", "enum": ["tool", "reason", "ask"]},
          "capability": {"type": "string", "enum": ["mail","weather","calendar","music"]},
          "tool": {"type": "string"},
          "args": {"type": "object"},
          "expect": {"type": "string", "maxLength": 120},
          "on_empty": {"type": "string", "enum": ["narrow","ask","accept"]}
        },
        "required": ["id", "title", "kind", "capability", "expect"],
        "additionalProperties": false
      }
    }
  },
  "required": ["motive", "goal", "steps"],
  "additionalProperties": false
}
```

`answer_directly` 只有两个参数：`reason`（为什么不需要工具）与 `motive`。
它是**显式的豁免通道**：把「跳过清单」从隐式行为变成一条可审计的记录
（进 `tool_trace`，UI 的 bookkeeping 工具不显示）。

### 5.3 三条兜底，保证「永不卡死」

强制清单最大的风险是：模型不配合，整轮空转。三道兜底：

| 情形 | 处理 |
| --- | --- |
| 模型首轮返回纯文本，无工具调用 | 先判定文本是否像任务（含动作动词 + 宾语，或提到邮件/日程/天气/音乐）。**像任务** → 追加一条 reminder「必须调用 plan_submit 或 answer_directly」，再给 1 轮；**不像任务**（寒暄、纯知识问答）→ 直接把这段文本作为 FINAL 输出，`plan = None`，状态 `Complete`，不合成计划 |
| 第 2 轮仍不提交 | **引擎合成单步计划**：`steps = [{id:1, title:"直接回答", kind:"reason"}]`，进 EXECUTE（无事可做）→ REFLECT（`verdict` 由引擎填 `partial`）→ FINAL。记 `error = "模型未提交计划，已按单步处理"` || 模型提交的计划非法（步数超限、`kind=tool` 但没 `tool`） | 返回 `is_error` + **具体修法**（不只是「参数不合法」），最多重问 2 次；仍非法 → 取合法前缀（逐条剔除非法项），剩 0 条则按上一条兜底 |

第二道兜底是必须的：`LLM 不配合` 是常态，不是异常。任何「等模型学会」的设计都会在生产里变成死循环。

### 5.4 PLAN 阶段的校验（纯函数）

`validate_plan_v2(prev, next, allowed_tools) -> Result<Plan, PlanError>`：

1. `goal` 非空且 ≤120 字；
2. 步数 1–8，`id` 在 1–8 内且不重复；
3. `title` 非空、≤40 字、不以「处理/看情况/等等」开头；
4. `kind == Tool` ⇔ `tool.is_some()`；
5. `tool` 必须在 `allowed_tools` 里（权限过滤后的集合）；
6. `kind == Reason` 时 `tool`/`args` 必须为空（避免"推理步偷偷调工具"）；
7. `kind == Ask` 时必须是最后一步（问完就停，后面的步没有意义）；
8. `args` 序列化后 ≤1 KB；含 `$ref` 时引用的步号必须 < 自身步号（**不允许前向/自引用**）；
9. `args` 里出现的字段必须在该工具 schema 的 `properties` 里（提前拦掉 D1 那类错误）；
10. `expect` 非空、≤120 字。

第 9 条直接治 D1：模型若写 `{"folder":"INBOX"}` 而 schema 要求至少一个检索条件，
在**计划期**就返回「第 2 步的 args 缺少检索条件，可用的有 query/from/subject/to/after/before/folder/unread_only/flagged_only」，
而不是等执行期报一句「至少需要一个检索条件」然后被原样重发 6 次。

---

## 6. EXECUTE 阶段

### 6.1 绑定：工具调用怎么对上步骤

业务工具新增一个固定参数 `step_id`（与 `motive` 同级注入，见 `with_motive` 的写法）：

```json
{ "motive": "执行第 2 步：检索星标邮件", "step_id": 2, "flagged_only": true, "limit": 10 }
```

执行前校验：`step_id` 存在、该步 `kind == Tool`、`tool` 匹配、`attempts < 3`。

**绑定失败时不拒绝**——这是从 D1 学到的：校验报错如果不能让模型立刻改对，就会变成空转。
降级顺序：

1. `step_id` 缺失 → 从 `motive` 里正则提取「第 N 步」；
2. 仍无 → 绑定到「第一个 `kind == Tool` 且状态为 pending/running 的步」；
3. 仍无（所有工具步都终结了）→ 视为**计划外调用**：允许执行一次，但在 `evidence`
   里记 `unplanned`，并在 reflect 阶段作为「计划不完整」的证据。

绑定结果记进 `AiToolTrace.step_id` 与 `AiToolTrace.attempt`（新增两个字段，带 `#[serde(default)]`）。

### 6.2 两种执行模式

| 模式 | 条件 | 谁执行 | 收益 |
| --- | --- | --- | --- |
| `Bound`（默认） | 模型发起调用，带 `step_id` | 模型驱动，引擎校验 | 保留模型对上下文的适应能力 |
| `Prefetch`（可选，P3） | `kind == Tool` 且 `args` 完整、无 `$ref`、工具只读 | 引擎直接执行，不进模型轮次 | 省轮次；适合「查天气」「列日程」这类参数确定的步 |

`Prefetch` 的准入函数必须是**正向白名单**（沿用 `is_replay_safe` 的教训）：

```rust
fn can_prefetch(step: &PlanStep) -> bool {
    matches!(step.tool, Some(AiToolName::EmailSearch | AiToolName::WeatherGet
        | AiToolName::CalendarList | AiToolName::MusicSearch | AiToolName::MusicState))
        && step.args.as_ref().is_some_and(|a| !has_ref(a) && !a.is_null())
}
```

写工具永不 prefetch：它们的参数依赖用户当前消息的语义，且副作用不可撤销。

### 6.3 重试：按错误分类，不是按次数数数

错误分五类，决定「重试 / 改参 / 放弃」：

| 类别 | 来源 | 处理 | 计入 attempts |
| --- | --- | --- | --- |
| `Transient` | `CoreError::is_retryable() == true`、工具超时（2s/3s/5s/20s）、音乐 3s 超时 | 引擎**自动**重试同参数，退避 300 ms → 900 ms | 是 |
| `InvalidArgs` | `CoreError::Validation`（含 D1 的「至少需要一个检索条件」） | **不重试**。回灌「哪个字段不对 + 一个可用示例」，要求改参数 | 是 |
| `Empty` | 检索 0 命中（工具 status = ok，但结果空） | 按 `on_empty` 处理，见 §6.4 | 只有 `Narrow` 计 |
| `Denied` | 权限未开 / 本轮未下发该工具 / 执行期二次校验失败 | 不重试，步标 failed，提示用户开权限 | 是 |
| `Write` | 任何有副作用的工具失败 | **永不自动重试**（不变量 5） | 是 |

**指纹熔断**（治 D4 的核心）：

```rust
/// 规范化：去掉 motive / step_id，键排序，压缩空白。
fn attempt_fingerprint(tool: AiToolName, args: &serde_json::Value) -> String { ... }
```

规则（严格区分「临时错误」与「参数错误」）：

- `Transient`：同 fingerprint 允许引擎自动重试，最多 3 次（这是真重试，退避 300 ms → 900 ms）。
- `InvalidArgs`：同 fingerprint **最多执行 1 次**。
  - 第 2 次出现同一指纹 → **不执行**，直接回灌「你刚用完全相同的参数调用过，返回同样的错误：{err}。必须改参数」，
    `attempts` 仍然 +1（消耗掉一次机会）；
  - 第 3 次仍同一指纹 → 该步判 `StepFailure::RepeatedCall`，标 failed，不再消耗任何预算。
- 参数一旦变化（fingerprint 不同），计数从该新指纹重新开始——模型换个说法探索是允许的，
  「换个说法但业务参数没变」（只改 `motive`）不算变化，因为指纹已剔除 `motive` / `step_id`。

也就是说，那次 8 连击会被压成「执行 1 次 + 拦截 2 次」，最多 3 轮模型往返、1 次真实查询。
三次机会仍然给足——只是第 2、3 次不再允许拿同一份参数去撞同一堵墙。
熔断状态放在 `ToolBudget` 里
（`failed_fingerprints: HashMap<String, String>`），生命周期 = 一次用户消息，replan 不清空。

### 6.4 0 命中的处理

「未找到」不是成功，也不是失败。它是**计划需要修正**的信号。

- `on_empty = narrow`（默认）：用掉一次 attempt，把条件放宽一档（去掉最严格的条件：
  先丢 `flagged_only`，再丢 `subject`，再扩时间窗到 30 天），由引擎改写 args 重试；
- `on_empty = ask`：整轮暂停，输出「没找到，需要你补充：…」，该步保持 running；
- `on_empty = accept`：写 evidence「0 命中」，继续下一步。

同理，`email_search` 的空结果文案要改成可操作的（现状只建议「先同步账户」）：

> 0 命中。可尝试：① 放宽为近 30 天；② 去掉星标条件；③ 换发件人姓名。

### 6.5 单步内部状态推进

```rust
impl Plan {
    /// 开始一步。返回 false 表示该步已终结或依赖未满足。
    pub fn begin_step(&mut self, id: u8, now: i64) -> bool;
    /// 记一次尝试结果。attempts 只增不减；达到上限即转终态。
    pub fn record_attempt(&mut self, id: u8, outcome: AttemptOutcome, now: i64);
    /// reflect 判定结果不达 expect：可把 done 打回 running（消耗 1 次 attempt）。
    pub fn reopen_step(&mut self, id: u8, miss: StepFailure) -> bool;
}
```

`reopen_step` 是 R5 落地的关键：`expect` 不是装饰，它给了 reflect 一个把步退回重做的依据。
但**只能退一次**（`attempts` 已 +1，第 3 次失败即终态），避免反复自我否定。

### 6.6 EXECUTE 阶段的工具范围（防止绕过重列上限）

进入 EXECUTE 后，下发给模型的是**全部已授权能力工具 + `plan_update`（不含 `plan_submit`）**：

| 工具 | EXECUTE 阶段可用 | 理由 |
| --- | --- | --- |
| 能力工具（邮件/天气/日历/音乐） | 是 | 干活的就是它们 |
| `plan_update` | 是 | 模型回填 evidence、标记 `skipped`；**不能新增步骤** |
| `plan_submit` | **否** | 能新建计划就等于绕过了「重列 ≤2 次」的上限 |
| `answer_directly` | 否 | 已经开始执行了，不需要豁免通道 |

想改结构（插步、拆步、换方案）只能走 `REFLECT → REPLAN`，而 REPLAN 计入 2 次上限。
这条约束是「重列上限」真正生效的前提——否则模型每次失败都悄悄补一步，用户永远看不到
「这是第 2 次计划」，上限也就没有意义。

### 6.7 依赖与跳过

计划里的依赖只有一种显式形式：`args` 里的 `$ref`（`{{step.3.messages.0.message_id}}`）。

- 被引用的步失败或跳过 → 引用它的步自动标 `Skipped`，`failure = Upstream`，
  不再尝试执行（避免拿一个空的 message_id 去读正文，白吃一次错误）；
- 没有 `$ref` 关系的步**允许乱序执行**（模型可以先去抓星标邮件，再补判据），
  但 `Ask` 步除外：它一旦被触发，整轮立即暂停；
- 计划里没有任何依赖、却排在失败步之后的步，继续执行——这符合「失败不静默、
  能独立完成的继续」的既有原则（`AI模块设计.md` §4.3 规则 3）。

---

## 7. REFLECT 阶段

### 7.1 回灌什么

不把工具返回原文塞回去（正文可能上万字，且是不可信输入）。回灌一份**结构化事实表**：

```text
【本轮执行事实】
目标：找出当前邮箱里值得优先处理的邮件，并说明为什么
1. [完成] 按判据检索候选邮件 —— email_search(判据:已星标 / 近 7 天未读 / 主题含某个关键词) → 命中 7 封
2. [失败] 读前 3 封的正文要点 —— mail_read(message_id=41) → 正文未缓存（2/3 次）
   期望：每封拿到 1–2 句要点与是否需要行动
3. [跳过] 归纳并标注优先级 —— 依赖第 2 步

用户原始需求：有什么重要的邮件
请判断：这次任务完成了吗？
```

**判据名要出现在事实表里**：自检时模型得看到自己当初是怎么理解需求的，
才能判断「结果是否回答了用户的需求」而不是「步骤是否都打了勾」。

每步一行、不超过 80 字、不含正文原文。这样即使 8 步全跑完也只有 600 余字，
不会把上下文顶爆，也不会把邮件内容当成指令送进模型（保持 §9 的注入边界）。

### 7.2 SelfCheck 契约

```json
{
  "verdict": "done | partial | failed",
  "failed_step": 4,
  "diagnosis": "wrong_plan | missing_data | wrong_args | tool_broken | user_ambiguous",
  "reason": "第 4 步读不到正文，第 5 步的归纳只有主题没有内容",
  "replan_hint": "改为只用主题+发件人+是否未读做优先级判断，不读正文"
}
```

| 字段 | 约束 |
| --- | --- |
| `verdict` | 三选一 |
| `failed_step` | `verdict != done` 时必填，必须指向一个真实存在且非 done 的步 |
| `diagnosis` | 五选一，决定 replan 的方向 |
| `reason` | ≥8 字，不能是「失败了」这类空话 |
| `replan_hint` | `verdict == failed` 时必填，≥8 字 |

`diagnosis` 五类的含义与对应动作：

| diagnosis | 含义 | replan 期望的改法 |
| --- | --- | --- |
| `wrong_plan` | 思路全错，步骤本身不对 | 重写步骤（允许整份换掉） |
| `wrong_args` | 步骤对，参数不对 | 只改 args |
| `missing_data` | 本地没有这份数据（正文未缓存 / 未同步） | 加「先让用户打开邮件 / 先同步」的步 |
| `tool_broken` | 工具本身不可用（权限、服务） | 换路径或直接告知用户 |
| `user_ambiguous` | 用户需求本身含糊 | 转 `Ask` 步，停下来问 |

### 7.3 事实优先（不变量 6）

模型的自我评估是**不可信**的，尤其倾向于说「完成了」。引擎做一致性裁剪：

```rust
fn reconcile(selfcheck: SelfCheck, plan: &Plan) -> Verdict {
    let all_done = plan.steps.iter().all(|s| s.status == PlanStatus::Done);
    let any_failed = plan.steps.iter().any(|s| matches!(s.status, PlanStatus::Failed | PlanStatus::Skipped));
    match (selfcheck.verdict, all_done, any_failed) {
        // 全做完却说失败：不当成虚报，也不当成事实。降为 partial，原话保留。
        (Verdict::Failed, true, false) => Verdict::Partial,
        // 有失败步却说完成：一律降级。
        (Verdict::Done, _, true) => Verdict::Partial,
        (v, _, _) => v,
    }
}
```

为什么「全做完却说失败」不直接判 `Done`：模型有时是自我苛责（常见），有时是真的发现了
evidence 与目标不符（少见但重要）。两者引擎分不出来，所以取**信息量最大**的处理——
判 `Partial` 并保留模型原话，让用户在清单上同时看到「5 步都打了勾」和「模型认为不够好」。
强行判 `Done` 会把有价值的不确定性抹掉；强行判 `Failed` 又会触发无意义的重列。

`failed_step` 指向一个 done 步时同样降级为 `partial` 并保留原话。

---

## 8. REPLAN 阶段

### 8.1 触发与上限

```text
verdict == failed  &&  replans.len() < 2  →  REPLAN
verdict == failed  &&  replans.len() == 2 →  FINAL（说明「两次重列后仍未完成」）
```

第 3 次请求直接拒绝，回灌「已达重列上限（2 次），请直接给出当前能给出的最好结果」，
并把阶段切到 FINAL——**不接受「再试一次」**，否则上限形同虚设。

### 8.2 重列的硬约束

`validate_replan(prev, next, failed_fingerprints) -> (Plan, Vec<DropReason>)`：

1. 必须带 `diagnosis`、`reason`（≥8 字）、`changes`（≥4 字），缺一拒收；
2. 新计划的 `(tool, args)` 指纹**不得与历史失败指纹重复**——重复的步**直接剔除**
   （不是整份拒绝，避免又一轮无效往返）；剔除后为空 → 判 FAILED 收尾；
3. `attempts` 不重置（同一组合的历史失败仍算数）；
4. 旧计划进 `history`（最多留 2 份），失败步编号记进 `ReplanRecord.failed_steps`；
5. 新计划的 `goal` 允许改写（思路全错时目标本身可能就错了），但必须在 `changes` 里说明。

第 2 条是防「重列 = 重新抽卡」的关键。没有它，模型会在两次重列里把同样的调用再发一遍，
用户看到的是「AI 认真地又失败了一次」。

### 8.3 重列的预算

重列**不重置**工具预算（不变量：全局共享 `ToolAllowance`）。
`REPLAN` 只是「换一份计划」，不是「再给一整套预算」。具体见 §11。

---

## 9. FINAL 阶段与空正文守卫

### 9.1 收尾要求

- 必须按既有 `[结果]` 段落格式（`SYSTEM_PROMPT` 第 9 条）——**四行不变**；
- 计划的完成情况由**清单条**承担（它就在正文上方，且是实时的），不塞进 `[结果]`；
- 正文叙述里要自然交代「哪些做到了、哪些没做到、下一步建议」；
- 正文里的「做了什么」只能写真正执行过的动作（`tool_trace` 有据可查的那部分）；
- 未完成项必须逐条写原因（取 `PlanStep.failure` 的人话版本）。

> 为什么不给 `[结果]` 加「计划：4/5」这类行：`split_result_block`
> （[views/ai.rs:872](../crates/me-app/src/views/ai.rs)）是按 `状态：/做了什么：/关键信息：/未完成：`
> 四个前缀逐行匹配的，`ResultBlock` 只有四个字段，未知行会被静默丢弃——写进去也不会显示。
> 要加就得同时改解析器、渲染、三个单测，收益不抵成本。清单条已经把这件事件讲清楚了。

### 9.2 空正文守卫（治 D5）

```rust
/// 一个可见字符都没有才算「没说话」——那次事故的正文就是 13 个换行。
fn is_blank_answer(text: &str) -> bool {
    text.chars().all(char::is_whitespace)
}
```

**不要**把阈值提高到「少于 N 个字符」去判「回答得够不够好」。第一版写的是
`count() < 8`，实现时立刻被两个既有测试打回来：`记住了。`（4 字）、`已删除。`
这类短回执是合法回答，被误判成空白后会强制重问一轮，测试脚本耗尽而失败。
引擎只该判断**有没有回答**，不该判断**回答得好不好**。

流程：

1. `EXECUTE/REFLECT` 结束后，若本轮**调用过工具**且 `is_blank_answer(content)`：
   追加一条 reminder「你必须用自然语言向用户交代结果：找到了什么、没找到什么」，
   再给 1 轮（`MAX_BLANK_RETRIES = 1`）；
2. 仍为空 → 引擎**合成兜底文本**（不调模型）：

```text
这次没有拿到可用的结果。

计划：完成 2/5 步
· 定「重要」的判据：已完成 —— 判据：星标优先，其次近 7 天未读
· 检索星标邮件：已完成 —— 命中 3 封
· 检索近 7 天未读：失败 —— 参数不合法（已尝试 3 次）
· 读前 3 封的正文要点：跳过 —— 依赖上一步
· 归纳并标注优先级：跳过 —— 依赖上一步

你可以：① 让我先同步最近的邮件；② 换一个更具体的说法（例如「这周未读的邮件」）。
```

状态保持 `Complete`，`error = "模型未输出正文，已用执行回执兜底"`。

> **为什么不是 `Partial`**：`ai_messages.status` 有 CHECK 约束
> `IN ('streaming','complete','failed','cancelled')`（[migrations.rs:299](../crates/me-store/src/migrations.rs)），
> 加第五个值需要一次表重建迁移。P0 不做迁移，所以「没拿到结果」这层语义由
> **正文首句**（`这次没有拿到可用的结果。`）与 `error` 字段承担。
> 真要引入 `partial` 状态时，和 v8 的其他改动一起做。

界面因此不会再出现一块空白，出错也能被下一轮排查到。

### 9.3 落库

沿用 `finish_assistant_message`（文本 / plan / traces / 用量一次性写）。
`plan` 写**最终态**：`finalize_plan` 已有「取消只标 running → skipped」的逻辑
（[ai.rs:1438](../crates/me-sync/src/ai.rs)），v2 追加「预算耗尽 → 未终结步标 skipped 并写 `StepFailure::Budget`」。

---

## 10. 清单的实时呈现

### 10.1 事件

| 事件 | 时机 | 载荷 | 可丢 |
| --- | --- | --- | --- |
| `AiPlanUpdated`（扩展） | 引擎每次改 plan（建计划、每步状态变化、replan） | `Plan`（v2） | 否 |
| `AiPlanStepAttempt` | 每步开始 / 每次重试 | `step_id, attempt, max, tool, args_digest, error` | **是** |
| `AiPlanSelfCheck` | reflect 完成 | `verdict, failed_step, diagnosis, reason` | 否 |
| `AiPlanReplanned` | replan 完成 | `round, diagnosis, changes, failed_step_titles` | 否 |
| `AiPlanFinished` | final | `verdict, done, total, replans` | 否 |

`AiPlanStepAttempt` 允许丢：它只影响「第 2/3 次」这个角标的完整性，最终态由
`AiPlanUpdated` + 落库保证。其余四个是关键事件，走背压。

**必须同步的事**：`Event::is_progress()` 的分类函数、`Event` 变体计数（**实测 65**，P2 落地后 69）、
`CLAUDE.md` 的「63 个变体」（该数字已过期，**已改成 69**）与「事件分类」表述。

### 10.2 清单条 v2

```text
┌──────────────────────────────────────────────────────────┐
│ 任务清单 · 找出值得优先处理的邮件          2/3   第 1 次计划 │
├──────────────────────────────────────────────────────────┤
│ ◐ 1. 按判据检索候选邮件                                    │
│      email_search {判据:已星标 / 近 7 天未读 / 主题含某个关键词} │
│      期望：按三条判据检索，每封标注命中了哪一条             │
│ ○ 2. 读前 3 封的正文要点                                  │
│      mail_read {{{step.1.messages.0.message_id}}}         │
│ ○ 3. 归纳并标注优先级                                     │
├──────────────────────────────────────────────────────────┤
│ ⚠ 第 1 步：0 命中，条件已放宽为 … 后重试                 │
└──────────────────────────────────────────────────────────┘
```

**判据名要出现在参数摘要里**（`判据:已星标 / 近 7 天未读`）——
这是用户第一眼看到「模型怎么理解我的需求」的地方，也是他当场纠正的入口。

规则：

- **未完成时默认展开**，全部终结后折叠成一行（沿用现有 `show_plan` 的分支，
  [views/ai.rs:704](../crates/me-app/src/views/ai.rs)）；
- 每步两行：标题行（状态图标 + 序号 + 标题 + 尝试角标）、细节行（kind/工具/参数摘要/期望）；
- 参数摘要用**脱敏 + 截断**：`flagged_only:true, limit:10`，超过 60 字截断，
  `message_id` 这类引用显示为 `→ 第 3 步结果`；
- 重列后清单条上方加一条分隔行：`第 2 次计划 · 上一轮的问题：mail_read 读不到未缓存正文`；
- 状态图标沿用 `plan_marker`，新增 `Ask` 态的 `?`（等待用户回答）。

### 10.3 切页不丢清单

现状：`AiPageState::select` 清 `self.plan`（[views/ai.rs:261](../crates/me-app/src/views/ai.rs)），
而流式期间 plan 不落库 → 切走再回来，清单消失、界面只剩半截文本。

v2 的处理：把「正在流式的那一条消息的清单」提到 `UiState` 层（`streaming_plan: Option<(AiMessageId, Plan)>`），
只在「该消息结束」或「用户切到别的会话」时清空，不在「切页」时清空。
这不需要改落库策略，也不破坏「流式不落库」的不变量——它只是一个 UI 状态的作用域调整。

---

## 11. 预算：重算一遍账

### 11.1 现值 → 新值

| 项 | 现值 | 新值 | 理由 |
| --- | --- | --- | --- |
| `MAX_PLAN_STEPS` | 6 | 8 | 「整理重要邮件」真实需要 5 步；6 步对跨模块需求偏紧 |
| `MAX_TOOL_ROUNDS` | 6 | 24 | PLAN 2 + EXECUTE 12 + REFLECT 2 + REPLAN 2 + 后续 EXECUTE 4 + FINAL 2；**全局不重置** |
| `MAX_TOOL_CALLS` | 12 | 24 | 8 步 × 3 次最坏 24；bookkeeping 不计 |
| `STREAM_BUDGET` | 300 s | 600 s | 阶段子预算之和的上界，见下 |
| `MAX_STEP_ATTEMPTS` | —（无） | 3 | R6 |
| `MAX_REPLANS` | —（无） | 2 | R9 |
| `MAX_PLAN_NUDGES` | 2 | 2（保留，仅用于「计划开着却停手」） | 与新机制不重叠 |
| `MAX_BLANK_RETRIES` | —（无） | 1 | §9.2 |
| 单工具超时 | 2/3/5/5/20 s | 不变 | 重试次数变了，单次超时不必变 |
| 全局并发 | 2 / 单会话 1 | 不变 | 变的是单轮内部的步数，不是并发流数 |

### 11.2 阶段子预算

| 阶段 | 时间 | 模型轮次 | 说明 |
| --- | --- | --- | --- |
| `PLAN` | 60 s | 2 | 含 1 次「不提交计划」的重问 |
| `EXECUTE`（第 1 份计划） | 300 s | 12 | |
| `REFLECT` | 45 s | 2 | 含 1 次格式不合规的重问 |
| `REPLAN` + `EXECUTE`（第 2 份） | 100 s | 4 | 复用剩余工具预算 |
| `REPLAN` + `EXECUTE`（第 3 份） | 50 s | 2 | 复用剩余工具预算 |
| `FINAL` | 45 s | 2 | 含 1 次空正文重问 |
| **合计（上界）** | **600 s** | **24** | 时间与轮次都全局封顶，不随重列重置 |

总上界 600 s。注意这是**上界**，正常路径（3 步、无重试）仍是一次 20–40 s 的对话。

### 11.3 最坏情况与刹车

最坏：8 步 × 3 次 = 24 次工具，两次 replan 各 8 步 × 3 次 = 48 次，合计 72 次。

**不能让它真的跑 72 次。** 三道刹车：

1. `MAX_TOOL_CALLS = 24` 是**全局**的，不随阶段/重列重置；
2. **只有「真正执行」才计数**：被 PLAN 白名单拒绝的、被指纹熔断拦下的、被权限拒掉的调用
   都不消耗次数（它们不产生 IO，只产生一条错误回执）。这一条很关键——否则 6 次同参数
   报错会吃掉 1/4 预算，模型还没来得及改参数就撞墙，正好复现 D4；
3. `ToolAllowance` 在 replan 前预检：剩余次数 < 计划步数 → 直接 FINAL（`partial`），
   并写明「预算不足，已完成 X/Y 步」。

这样最坏情况是 24 次真实执行 + 600 s 封顶，用户最多等 10 分钟，且界面全程显示进度、可随时取消。

### 11.4 一个必须算清的账

| 场景 | 真实执行次数 | 说明 |
| --- | --- | --- |
| 3 步无重试 | 3 | 正常路径，20–40 s |
| 5 步、其中 1 步失败 3 次 | 7 | 4 + 3 |
| 8 步全成功 | 8 | |
| 8 步、3 步各失败 3 次 | 17 | 5 + 9 + 熔断拦截不算 |
| 上述后再 replan 一次（新计划 5 步） | 22 | 剩 7 次，够 5 步，剩余 2 次给重试 |
| 再 replan 第二次 | 触发刹车 2 | 剩余 2 < 5 步 → 直接 FINAL |

### 11.5 预算耗尽的语义

**预算耗尽不是失败，是 `partial`。** 已完成的部分必须如实呈现（清单上的回执就是证据），
未完成的部分给出原因（`StepFailure::Budget`）与下一步建议。把预算耗尽记成 `failed`
会让用户以为「什么都没做成」，而实际上可能已经拿到了 80% 的答案。

---

## 12. 让模型自己理解需求（而不是替它理解）

这一节回答「同一个问题，改完之后会怎样」。

### 12.0 先说清楚：为什么不能把「重要」的定义写进系统

事故的原始需求是「有什么重要的邮件」。一个省事的做法是在提示词里直接写死：

```text
13. 用户说「重要的邮件」时，按以下判据执行：
    ① 已星标；② 未读且 7 天内；③ 主题命中事项词（面试 / offer / 合同 / 发票 …）。
```

**这个做法是错的**，而且错得很隐蔽：

- 它把**一个例子**的答案抄进了系统。用户的原意是「让模型去理解我的需求」，
  而「什么算重要」只是这个能力的一个样本。写死之后，模型不需要理解需求，
  只需要套模板——换个需求（「紧急的」「该跟进的」「这周要回的」）就全错。
- 它替**所有用户**做了决定。什么算重要，因人、因事、因时而异；
  系统无权用一个固定口径回答所有人。
- 它在系统里留下了**三份拷贝**：提示词第 13 条、`email_search` 的 `important_only`
  参数、`is:important` 速记语法 + SQL 里的 7 天窗口。三份一旦漂移，
  「AI 说的」和「用户搜的」就对不上。

所以这一节的落点不是「把『重要』定义对」，而是**给模型一套通用的翻译机制**：
模型自己把定性词翻译成可执行的条件，引擎负责让这个翻译**可见、可执行、可校验**。

### 12.1 机制：判据（criteria）

「重要」「紧急」「待办」「值得看的」在邮件表里**没有对应字段**——它们是判断。
而本地确实存在一批**客观维度**：已星标 / 已读 / 时间范围 / 发件人 / 收件人 /
主题与正文关键词 / 附件 / 文件夹。

翻译的动作就是：**把定性词映射到客观维度的组合上，并给每条组合起一个能让人看懂的名字。**

这个映射由模型做，不由引擎做。引擎只提供三个支撑：

| 支撑 | 做法 |
| --- | --- |
| **可表达** | `email_search` 增加 `criteria` 参数：一组 `{label, ...条件}`，取**并集**，顺序即优先级 |
| **可执行** | 引擎逐条判据检索、合并去重、按判据顺序排序，每条结果标注**命中了哪条判据** |
| **可见** | 判据名出现在清单条（`判据:已星标 / 近 7 天未读`）与邮件卡片上，用户据此判断模型有没有理解错 |

```json
"criteria": [
  {"label": "已星标", "flagged_only": true},
  {"label": "近 7 天未读", "unread_only": true, "days": 7},
  {"label": "主题含某个关键词", "subject": "某个关键词"}
]
```

**判据组与普通条件互斥**：判据自己带条件，再叠一层普通条件会让「重要」变成
「重要且最近」——那是另一个问题。

### 12.2 提示词侧

第 13 条不再定义「重要」，而是给出**翻译方法**：

```text
13. 用户的定性需求由你自己翻译成判据，引擎不会替你决定。
    用户说「重要的邮件」「紧急的事」「该处理的」时，这些词在本地数据里没有
    对应字段——你要先想清楚「在这个邮箱里，什么东西算满足这个词」，再把它
    写成一条条可执行的判据：
    ① 只能从本地真实存在的维度取条件：已星标 / 未读 / 时间范围 / 发件人 /
       收件人 / 主题或正文关键词 / 附件 / 文件夹；
    ② 用 email_search 的 criteria 把判据逐条列出，每条给它一个能让人看懂的
       名字，结果会标注命中了哪一条，用户据此判断你有没有理解错；
    ③ 判据之间是「或」的关系；
    ④ 不要凭空扩大用户没提过的维度；
    ⑤ 判据要写进清单对应步骤，让用户第一眼就能看到你的理解；
    ⑥ 一条都没命中时明确说「按这些判据没找到」，不要用无关邮件凑数。
```

这一段的意义是**让判据变成可见的**：它会出现在清单对应步骤的参数摘要里
（`判据:已星标 / 近 7 天未读`），用户可以当场纠正。

**守卫测试**：提示词里不许再出现任何具体需求的答案。

```rust
#[test]
fn system_prompt_does_not_hardcode_a_particular_requirement() {
    for needle in ["important_only", "面试", "offer", "合同", "发票"] {
        assert!(!SYSTEM_PROMPT.contains(needle));
    }
    for needle in ["criteria", "判据", "已星标", "未读"] {
        assert!(SYSTEM_PROMPT.contains(needle));
    }
}
```

### 12.3 移除预置口径

`important_only` 与 `is:important` 全部删除：

| 位置 | 处置 |
| --- | --- |
| `email_search` 的 `important_only` 参数 | 删除；工具层改为**点名**拒绝并说明替代做法（`criteria`） |
| `is:important` 速记语法 | 删除；它现在进 `unknown`，搜索照常执行并提示该片段没被认出来 |
| `SearchQuery.important_only` 与 SQL 里的 7 天窗口 | 删除；星标仍可单独筛（`is:flagged`），但「重要」不再是筛选项 |

**留下的分界线**：

- **客观维度**（星标、未读、时间、发件人、附件、文件夹）→ 一等过滤条件，用户与 AI 都能用；
- **主观判断**（重要、紧急、值得看）→ **不是**过滤条件，由回答问题的一方翻译成判据。

### 12.4 引擎侧

| 现象 | 机制 | 改后表现 |
| --- | --- | --- |
| 没建清单 | 首轮只下发 `plan_submit` / `answer_directly` | 第一次业务调用前必有 4–5 步清单 |
| 6 次同参数报错 | `InvalidArgs` 不重试 + 指纹熔断 + 计划期参数校验 | 同参数只真正执行 1 次，第 2 次被拦下并给具体修法，第 3 次判该步失败 |
| 2 次「未找到」后继续乱试 | `on_empty` 策略 | 第 3 步 0 命中 → 自动放宽一档（或问用户） |
| 空回复 | 空正文守卫 + 兜底摘要 | 界面必有内容；`error` 非空可追溯 |
| **需求被误解** | **判据可见 + 每条结果标注命中判据** | **用户第一眼就能看出「你把重要理解成了什么」，当场纠正** |

**放宽规则也分两种**，因为判据组与普通条件的语义不同：

- 普通条件（一组 AND）：先丢星标 → 再丢主题 → 拉长时间窗到 30 天；
- 判据组（多条 OR）：**去掉最宽的那一条判据**。判据是模型对需求的翻译，
  去掉一条等于放弃一种理解；保留剩下的比放宽剩下的更忠实于模型原本的意图。

### 12.5 预期轨迹（同一个问题）

```text
① PLAN   提交 4 步清单（第 1 步的 args 里带 criteria）
② EXEC   第 1 步 email_search(criteria=[已星标 / 近 7 天未读 / 主题含某个关键词])
         → 引擎逐条查、合并去重、按判据顺序排序，每封标注命中判据
③ REFLECT facts_block 里带着判据与每条判据的命中数
④ FINAL   回答里说明「我按这几条判据找的」，逐条给理由
```

**与旧轨迹的区别**：判据由模型决定，引擎不参与。换个需求，模型给出的是**另一组**
判据，引擎照样跑得通——这才是「让模型理解需求」而不是「背下一个需求的答案」。

---

## 13. 安全

计划是**模型生成的不可信输入**，比工具参数更危险——它决定「接下来会做什么」。

| 面 | 措施 |
| --- | --- |
| 工具白名单 | `step.tool` 必须在本轮下发的集合里；执行期再校验一次（已有机制，扩展到此） |
| 参数 | 计划期过 schema；执行期再过一次；`args` ≤1 KB |
| `$ref` | 只能引用前序步骤的**结构化字段**（`message_id` / `count` / `date`），不允许引用文本；解析器不接受任意 JSONPath，只接受 `{{step.N.path}}` 形式的白名单路径 |
| 写操作 | 计划里可以声明，但**执行期参数必须由模型带最新上下文重新给**；`calendar_delete` 仍走确认卡，计划不能替代用户确认 |
| 重试 | 写操作永不自动重试（不变量 5）；`memory_upsert` 因去重幂等可重试 |
| 注入 | 回灌给 reflect 的是**结构化事实表**，不是工具返回原文；正文永远走 `escape_context` 的既有边界 |
| 日志 | `args_digest` 只存前 120 字且不含自由文本字段；`motive` / `reason` 不进文件日志 |
| 展示 | 清单条用 `egui::Label`（纯文本），不解析 HTML/Markdown |
| 崩溃 | `streaming` 行修复时把 plan 标 skipped（已有逻辑，v2 补 `StepFailure::Budget`） |

一条容易被忽略的：**`Ask` 步必须真的停下来**。模型可能把 `Ask` 写成最后一步然后
自己编一个答案继续跑。引擎在遇到 `kind == Ask` 时直接切 FINAL 并输出该步的提问，
不再给模型任何工具——否则「问用户」会退化成「自问自答」。

---

## 14. 落地分期

每期都能独立验收，前一期不依赖后一期。

### P0 · 修现象（不改状态机，约 1 天）

| 改动 | 文件 |
| --- | --- |
| `email_search` 加 `flagged_only` / `days`，校验对齐，文案列出全部条件 | `me-core/src/ai.rs`（schema）、`me-sync/src/ai_tools.rs` |
| `SearchQuery.flagged_only` + SQL 条件 | `me-core/src/model.rs`、`me-store/src/search.rs` |
| 指纹熔断（`ToolBudget.failed_fingerprints`） | `me-sync/src/ai_tools.rs` |
| 空正文守卫 + 兜底摘要 | `me-sync/src/ai.rs` |
| 提示词：新增第 13 条（**定性需求的翻译方法**，不写死任何具体口径）；第 10 条**只删掉**「一句话能答完不要建清单」的豁免，仍以 `plan_update` 为主 | `me-core/src/ai.rs` |

验收：golden case（§15.1）回放，`email_search` 同参数实际执行 ≤1 次，正文非空，
提示词不再出现「一句话不要建清单」。

**P0 不要改第 10 条的工具名。** 这一期还没有 `plan_submit`，如果提示词改成「必须调用
`plan_submit`」，模型会去调一个不存在的工具——这正是 v0.3 那次「代码开了、提示没开」
的反向版本：**提示开了、代码没开**。第 10 条改成 `plan_submit` 是 P1 的动作，
必须与工具规格同一次提交。

P0 的意义：**不动架构就能把用户看到的现象修掉**，给 P1 留出安全的重构窗口。

### P1 · Plan v2 与强制清单（约 2–3 天）

| 改动 | 文件 |
| --- | --- |
| `Plan v2` 结构与纯函数（`validate_plan_v2` / `begin_step` / `record_attempt` / `is_executable`） | `me-core/src/ai_plan.rs` |
| `plan_submit` / `answer_directly` 规格、`step_id` 注入 | `me-core/src/ai.rs` |
| 五阶段驱动器的 `PLAN` 段 + 首轮白名单 + 三条兜底 | `me-sync/src/ai.rs` |
| `plan` 事件（`AiPlanUpdated` 扩展）与 `AiToolTrace.step_id/attempt` | `me-core/src/command.rs`、`me-core/src/ai.rs` |
| 清单条 v2 | `me-app/src/views/ai.rs` |
| 流式清单作用域上移（§10.3） | `me-app/src/state.rs` / `views/ai.rs` |

验收：任务型请求必出清单；`answer_directly` 有记录；模型不配合时走兜底不卡死。

### P2 · 重试、反思、重列（约 3–4 天）

| 改动 | 文件 |
| --- | --- |
| `ToolFailure` 五类分类 + 重试执行器 + 退避 | `me-sync/src/ai_tools.rs` |
| `EXECUTE` / `REFLECT` / `REPLAN` / `FINAL` 四段 | `me-sync/src/ai.rs` |
| `SelfCheck` 校验与 `reconcile`（事实优先） | `me-core/src/ai_plan.rs` |
| `validate_replan` + 失败指纹剔除 + history | 同上 |
| 预算重算（§11）与阶段子预算 | `me-sync/src/ai.rs`、`me-core/src/ai.rs` |
| 4 个新事件 + `is_progress()` 分类 | `me-core/src/command.rs` |
| 清单条：重试角标、重列分隔行、Ask 态 | `me-app/src/views/ai.rs` |

验收：golden case 的「重试—反思—重列」路径全部可回放；第 3 次重列被拒。

### P3 · 可选增强

- `Prefetch` 执行模式（§6.2）；
- 跨轮清单：`plan_id` 与消息解耦，支持「继续上一条没做完的清单」；
- 搜索语法 `is:flagged`（复用 `parse_search_syntax`，让用户手动搜索受益）；
- 清单统计（每步耗时、失败率），用于调 `MAX_STEP_ATTEMPTS` 这类常量。

> **注意**：原本这一条里还有 `is:important`。它已经被**移除**——
> 「重要」是判断，不是可筛的字段，详见 §12.3。

---

## 15. 测试计划

### 15.1 golden case：把那次失败做成回归用例

在 `crates/me-sync/tests/ai_flow_mock.rs` 里用脚本化 `MockAiProvider` 重放 id=29/30 的真实序列。
拆成两个 case，分别覆盖「模型不配合」与「模型配合但踩契约缺口」。

**Case A · 模型完全不配合**（复现 id=30 的形态）：

```rust
turns = [
    // PLAN 阶段只下发 plan_submit / answer_directly，模型却"想"调 email_search
    tool_call("email_search", {"motive": "查看重要邮件"}),
    // 第 2 轮仍不提交计划
    tool_call("email_search", {"motive": "查看重要邮件，未读优先"}),
    // 第 3 轮模型放弃，输出空文本
    text("\n\n\n\n\n\n\n\n\n\n\n\n\n"),
];
```

| 断言 | 对应缺陷 |
| --- | --- |
| `email_search` 的实际执行次数 == 0（PLAN 白名单拦下） | D3 + §5.2 |
| 第 3 轮后 `plan.is_some()`（引擎兜底单步计划） | §5.3 兜底 |
| 落库 `content` 的可见字符 ≥ 8，`status == Partial` | D5 |
| `error` 字段非空且含「未提交计划」 | D5 可排查 |

**Case B · 模型配合，但参数踩了 D1 那个坑**：

```rust
turns = [
    // 提交计划，第 2 步只有 folder（旧校验必拒）
    plan_submit(steps: [.., step(2, tool: email_search, args: {"folder": "INBOX"}), ..]),
    // 引擎在计划期就驳回，模型改参数重交
    plan_submit(steps: [.., step(2, tool: email_search, args: {"unread_only": true, "days": 7}), ..]),
    // EXECUTE：同参数连续 3 次
    tool_call("email_search", {"unread_only": true, "days": 7, "motive": "执行第 2 步"}),
    tool_call("email_search", {"unread_only": true, "days": 7, "motive": "再试一次"}),
    tool_call("email_search", {"unread_only": true, "days": 7, "motive": "还是不行"}),
    text("[结果] ..."),
];
```

| 断言 | 对应缺陷 |
| --- | --- |
| 第 1 次 `plan_submit` 被驳回，且文案列出可用条件 | D1 |
| `email_search` 真实执行次数 == 1 | D4（8 次 → 1 次） |
| 第 2 次 `email_search` 的 summary 含「相同参数」 | D4 熔断 |
| 第 3 次之后该步 `status == Failed`、`failure == RepeatedCall` | R6 |
| 最终 `verdict == Partial`，`content` 非空 | D5 + R7 |

### 15.2 纯函数单测（`ai_plan.rs` 内联）

| 测试 | 断言 |
| --- | --- |
| `plan_v2_requires_tool_for_tool_kind` | `kind=tool` 无 `tool` → 拒 |
| `reason_step_rejects_args` | `kind=reason` 带 `args` → 拒 |
| `ask_must_be_last` | `Ask` 不在末尾 → 拒 |
| `args_field_must_exist_in_schema` | `{"foo":1}` → 拒，文案含可用字段 |
| `ref_cannot_forward` | `{{step.5.x}}` 出现在第 3 步 → 拒 |
| `attempts_cap_at_three` | 第 4 次 `record_attempt` → 步转 failed |
| `replan_cap_at_two` | 第 3 次 → `Err(ReplanLimit)` |
| `replan_drops_duplicate_fingerprint` | 新计划含已失败组合 → 被剔除 |
| `selfcheck_cannot_upgrade_failed` | `verdict=done` + 有 failed 步 → `partial` |
| `selfcheck_cannot_downgrade_all_done` | 全 done + `verdict=failed` → `partial`（保留原话） |
| `legacy_plan_is_not_executable` | `version=1` → `is_executable() == false` |

### 15.3 引擎集成（`ai_flow_mock.rs`）

- `transient_error_retries_three_times`：工具返回可重试错误 → 3 次后 failed；
- `validation_error_does_not_retry`：`InvalidArgs` → **只调 1 次**，且回灌含修法；
- `write_tool_never_retries`：`calendar_create` 失败 → 只调 1 次；
- `empty_result_narrows_once`：0 命中 + `on_empty=narrow` → 第 2 次参数被放宽；
- `replan_twice_then_final`：连续 failed → 2 次 replan → 第 3 次被拒 → FINAL `partial`；
- `budget_exhausted_is_partial`：`MAX_TOOL_CALLS` 打满 → `partial` + 未完成步 `StepFailure::Budget`；
- `ask_step_stops_round`：`kind=Ask` → 不给后续工具，直接 FINAL；
- `plan_event_order`：`AiPlanUpdated` 在第一个业务工具调用**之前**发出。

### 15.4 UI（`views/ai.rs` 内联，离屏跑帧）

- 清单条 v2 在 4 步 2 完成时高度受控，连跑 3 帧尺寸不漂移（沿用侧栏那个
  `min_rect` 正反馈坑的回归写法）；
- 重试角标 `2/3` 正确显示、完成后角标消失；
- 重列后出现「第 2 次计划」分隔行且旧步骤仍可见（history 展开）；
- `plan_marker` 覆盖 `Ask` 态。

---

## 16. 风险与取舍

| 风险 | 影响 | 处理 |
| --- | --- | --- |
| 强制清单让简单问题变慢 | 多 1 次模型往返（约 2–4 s） | `answer_directly` 轻量通道；单步清单折叠成一行 |
| 首轮只给两个工具，模型可能"不会用" | PLAN 阶段失败率升高 | 三条兜底（§5.3），最坏退化成单步计划，不会卡死 |
| 600 s 太长 | 用户等不了 | 阶段子预算 + 全程进度可见 + 随时可取消；正常路径 20–40 s |
| 重试用在写操作上造成重复副作用 | 数据污染 | 硬规则：写操作永不自动重试；写工具不进 `Prefetch` |
| 计划 JSON 膨胀（8 步 × 3 次） | 消息行变大 | `attempts` 只存计数，`failure` 只存一句摘要，`args` ≤1 KB；history 最多 2 份 |
| 提示词与代码再次冲突 | 模型按旧规则拒绝新能力（历史坑：日程改删） | 改代码的同时改 `SYSTEM_PROMPT` 第 10 条与 `plan_update` 描述；两者进同一次提交 |
| `Event` 变体增加 | 文档/分类函数不同步 | 同步 `is_progress()`、`CLAUDE.md` 的 63 → 69、`版本跟踪.md` |
| 事实优先被误用 | 用户看不到模型的担忧 | `reconcile` 只改 `verdict`，模型原话保留在 `reason` 与 UI |

### 明确不做

- 不做「多智能体」分工（planner / executor / critic 三个独立模型）——一次需求 3 倍 token，
  且现有单模型 + 强约束已经能覆盖；
- 不做计划的**自动**生成（不经模型、由规则拼）——那会在需求含糊时编出一份像模像样的错计划；
- 不做跨会话的清单持久化查询（「上周那个计划做到哪了」）——先有需求再做表。

---

## 17. 常量与索引速查

### 17.1 新增常量

| 常量 | 值 | 位置 |
| --- | --- | --- |
| `PLAN_VERSION_V2` | 2 | `me-core/src/ai_plan.rs` |
| `MAX_PLAN_STEPS` | 8（原 6） | 同上 |
| `MAX_STEP_ATTEMPTS` | 3 | 同上 |
| `MAX_REPLANS` | 2 | 同上 |
| `MAX_PLAN_HISTORY` | 2 | 同上 |
| `MAX_STEP_ARGS_BYTES` | 1024 | 同上 |
| `MAX_BLANK_RETRIES` | 1 | `me-sync/src/ai.rs` |
| `PHASE_PLAN_BUDGET` | 60 s | 同上 |
| `PHASE_EXECUTE_BUDGET` | 300 s | 同上 |
| `PHASE_REFLECT_BUDGET` | 45 s | 同上 |
| `PHASE_FINAL_BUDGET` | 45 s | 同上 |
| `RETRY_BACKOFF` | `[300ms, 900ms]` | 同上 |
| `MAX_TOOL_ROUNDS` | 24（原 6） | `me-core/src/ai.rs` |
| `MAX_TOOL_CALLS` | 24（原 12） | 同上 |
| `STREAM_BUDGET` | 600 s（原 300） | `me-sync/src/ai.rs` |

### 17.2 代码索引（现状 → 目标）

| 符号 | 现状 | 目标 |
| --- | --- | --- |
| `ai_plan::PlanStep` | `ai_plan.rs:121`，5 字段 | +`kind/tool/args/expect/attempts/failure/on_empty` |
| `ai_plan::validate_plan` | `ai_plan.rs:210` | 拆成 `validate_plan_v2` + `validate_replan` |
| `ai::TOOL_SPECS` | `ai.rs:1601`，14 个 | 16 个（`plan_submit` / `answer_directly`） |
| `ai::SYSTEM_PROMPT` | `ai.rs:1322`，第 10 条豁免清单 | P0 新增第 13 条；P1 改写第 10 条（见 §14） |
| `ai::with_motive` | `ai.rs:1854` | 同时注入 `step_id` |
| `sync::run_ai_stream` | `ai.rs:1136` 单循环 | 五阶段驱动器 |
| `sync::MAX_PLAN_NUDGES` | `ai.rs:39`，= 2 | 保留，语义收窄为「计划开着却停手」 |
| `ai_tools::tool_email_search` | `ai_tools.rs:353` | +`flagged_only` / `days`，校验对齐 |
| `model::SearchQuery` | `model.rs:1036` | +`flagged_only: bool` |
| `command::Event::AiPlanUpdated` | `command.rs:769` | 载荷扩展；新增 4 个清单事件 |
| `views::show_plan` | `views/ai.rs:704` | 清单条 v2 |
| `store::parse_plan` | `store/ai.rs:89` | 兼容 v1（`is_executable() == false`） |

### 17.3 验收清单（人工 + 自动）

- [ ] `cargo test --workspace` 全绿，新增测试 ≥ 30 条；
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` 通过；
- [ ] `cargo fmt --check` 通过；
- [ ] golden case 回放：同参数 `email_search` 执行 ≤1 次、正文非空、有兜底计划；
- [ ] 真机：问「有什么重要的邮件」→ 清单先出现，逐步打勾，最后给出 ≤5 封带理由；
- [ ] 真机：断网后问同一问题 → 3 次重试后 failed → reflect → replan 1 次 → FINAL 说明原因；
- [ ] 真机：中途点「停止」→ 当前步 `Skipped`，清单不消失（切页回来仍在）；
- [ ] 真机：关掉「邮件」工具开关 → 计划里不出现邮件步，模型直说需要开启。

---

## 18. P0 实施状态（2026-11-07 已落地）

五个提交，每步一次门禁（`cargo test --workspace` + `fmt --check` + `clippy -D warnings`）：

| 提交 | 内容 | 新增测试 |
| --- | --- | --- |
| `87fee7d` | `SearchQuery.flagged_only` + SQL `(m.flags & 4) != 0`；`SavedSearchQuery` 同步加字段（serde default） | `search_flagged_only_returns_only_starred` |
| `5d73f5d` | `email_search` 契约对齐：九条件判定、报错列全、新增 `flagged_only` / `days`、空结果文案可操作 | `email_search_accepts_time_window_without_keyword`、`email_search_without_any_condition_lists_them_all` |
| `6d7fdaf` | 同参数熔断：`ToolBudget.failed` + 指纹（剔除 `motive` / `step_id`），拦截在 `executed_calls` 之前 | `same_args_after_validation_error_is_not_retried`、`changed_args_are_not_blocked_by_the_circuit_breaker` |
| `2cc524a` | 空正文守卫：一次催促 + 引擎兜底摘要（有清单列清单，无清单列工具回执） | `blank_answer_falls_back_to_execution_summary`、`normal_answer_is_not_replaced_by_fallback` |
| `44e7fd5` | 提示词：删「不要建清单」豁免、新增第 13 条「重要」判据 | `system_prompt_forces_plan_and_defines_importance` |

测试总数 **710 → 718**（+8），0 failed，6 ignored。`fmt` / `clippy` 全程通过。

### 18.1 与设计稿的三处偏差

| 设计稿 | 实现 | 原因 |
| --- | --- | --- |
| 空正文兜底后状态记 `Partial` | 保持 `Complete` + `error` | `ai_messages.status` 有 CHECK 约束，加第五个值要表重建迁移；P0 不迁移 |
| `is_blank_answer` 用「可见字符 < 8」 | 改成「一个可见字符都没有」 | 阈值 8 把 `记住了。` / `已删除。` 误判成空白，两个既有测试被强制重问后失败 |
| 拦截后第 3 次才判该步失败 | 第 2 次起一律不执行 | P0 还没有 `attempts` 记账（Plan v2 在 P1）

### 18.2 P0 验收：那次事故的三种走法（都在 `ai_flow_mock.rs` 里）

| 场景 | P0 之后的行为 |
| --- | --- |
| 模型写「近 7 天未读」 | 直接执行，不再报「至少需要一个检索条件」 |
| 模型连发同参数 8 次 | 执行 1 次，第 2 次起摘要固定为「相同参数已失败过，未执行」 |
| 模型查完一个字不说 | 催一次 → 仍空白则落库兜底摘要，`error` 记「模型未输出正文」 |
| 模型问「重要的邮件」 | 必须先建清单（提示词已无豁免），并由模型**自己**把「重要」翻译成判据（见 §12；此处不预置任何口径） |

注意第 3 行只说「不会空白」，**不保证**「回答得好」：模型仍然可能给出一份平庸的清单式回答。
真正的强制清单（首轮工具白名单）在 P1，那时才谈得上「用户能在第一步发现理解错了」。

### 18.3 P0 明确没做的事

> **这一节记录的是 P0 当时的边界。P1–P3 已经把它们全部补上**，
> 现状见 §19。

- 没有 `plan_submit` / `answer_directly`（P1）
- 没有 `Plan v2`、没有 `attempts`、没有 reflect / replan（P1 / P2）
- 没有新增事件（仍是 `AiPlanUpdated` 一个），所以 `Event` 变体数还是 **65**
- `search_syntax` 没有 `is:flagged`（P3，UI 搜索框仍无星标筛选入口）

---

## 19. P1–P3 实施状态（2026-11-07 全部落地）

P0 之后又做了 7 个提交，把 P1 / P2 / P3 全部实现。**测试 710 → 777（+67），0 failed，6 ignored**，
`fmt --check` 与 `clippy -D warnings` 全程通过。

### 19.1 提交清单

| 提交 | 内容 |
| --- | --- |
| `f4984e2` | 清单条 v2：每步两行（工具 + 参数摘要 + 期望 + 回执）、尝试角标、重列分隔行、Ask 态图标；清单缓存提到 `AiPageState`，切页不丢 |
| `c735255` | REFLECT 自检与 REPLAN 重列闭环：`SelfCheck` 解析、`reconcile` 事实优先、`validate_replan` 剔除历史失败指纹 |
| `c8579c0` | 引擎级重试（Transient 退避 300ms → 900ms）、0 命中自动放宽、阶段预算、四个新事件 |
| `85fc70c` | 「重要」成为一等检索条件（**后被本次改造推翻并移除**，见 §19.4） |
| `714ae01` | 跨轮清单延续 + 清单统计（失败率、最慢一步、累计耗时） |

### 19.2 五阶段闭环的最终形态

```
PLAN ──► EXECUTE ──► REFLECT ──► REPLAN ──┐
  │         │           │         │       │
  │         │           │         └───────┘
  │         │           └──► FINAL ◄──────┘
  │         └──► FINAL
  └──► FINAL（模型不配合 / 超时 / 豁免）
```

| 阶段 | 引擎做什么 | 模型做什么 |
| --- | --- | --- |
| PLAN | 只下发 `plan_submit` / `answer_directly`；模型不配合则合成单步清单 | 提交 1–8 步清单，每步写 tool / args / expect |
| EXECUTE | 绑定调用到步骤、记 attempts、临时故障自动重试、0 命中自动放宽 | 带 `step_id` 调用工具 |
| REFLECT | 把每步状态/回执/失败拼成事实表回灌 | 输出一段 JSON 自检（被引擎按事实裁剪） |
| REPLAN | 必须带诊断/理由/改法；剔除与历史失败指纹重复的步 | 提交新计划 |
| FINAL | 空正文守卫 + 兜底摘要 | 按 `[结果]` 格式给出最终答复 |

### 19.3 与设计稿的偏差（实施中撞出来的）

| 设计稿 | 实现 | 原因 |
| --- | --- | --- |
| 空正文兜底后状态记 `Partial` | 保持 `Complete` + `error` | `ai_messages.status` 有 CHECK 约束，加第五个值要表重建迁移 |
| `is_blank_answer` 用「可见字符 < 8」 | 「一个可见字符都没有」 | 阈值 8 把 `记住了。` / `已删除。` 误判成空白 |
| 放宽条件由**模型**改参数重试 | 由**引擎**改参数重试 | 回灌给模型后它常常换个毫不相干的关键词；引擎的放宽阶梯是确定的 |
| 放宽时把时间窗整段丢掉 | 扩到 30 天，不丢 | 丢掉时间窗等于把「最近 7 天的未读」变成「所有未读」——换了一个问题 |
| 0 命中算失败 | 0 命中算**完成**（回执写明放宽到底仍无结果） | 0 命中是合法业务结果，算失败会让 REFLECT 白跑一轮 |
| `Event` 变体 63 → 69 | 实测 65 → 69 | CLAUDE.md 的 63 已过期 |

### 19.4 「什么算重要」不再是系统的一个口径

> **这一节记录的是 2026-11-07 的最终形态。它推翻了本文件早期版本（§12.1 / §19.1 的
> `85fc70c` 提交）里「把『重要』定义成一个检索条件」的做法。**

早期实现把「重要 = 星标 **或** 近 7 天未读」固化在三个地方：

- AI 侧：`email_search` 的 `important_only` 参数 + 提示词第 13 条写死的三条判据；
- 用户侧：速记语法 `is:important`；
- SQL 侧：`((m.flags & 4) != 0 OR ((m.flags & 1) = 0 AND m.date >= now - 604800))`。

**问题**：这套口径是用户那一个例子的答案，不是产品该有的东西。
写死之后，模型不需要理解需求，只需要套模板；换个定性需求就全错；
而且同一个口径复制三份，一旦漂移，「AI 说的」与「用户搜的」就对不上。

**现状**：定性需求由模型自己翻译成**判据**，引擎只提供通用机制。

| 面 | 现状 |
| --- | --- |
| 客观维度 | 星标 / 未读 / 时间 / 发件人 / 附件 / 文件夹——一等过滤条件，用户与 AI 都能用 |
| 主观判断 | **不是**过滤条件。模型用 `email_search` 的 `criteria` 参数把它翻译成一条条带名字的判据 |
| 引擎 | 逐条判据检索 → 合并去重 → 按判据顺序排序 → 每条结果标注命中了哪条判据 |
| 用户 | 判据名出现在清单条（`判据:已星标 / 近 7 天未读`）与邮件卡片上，可当场纠正 |

`important_only` 与 `is:important` 均已删除；工具层会**点名**拒绝并说明替代做法。
分界线是：**客观维度可筛，主观判断需翻译**。

### 19.5 「每步 3 次机会」的实际语义

三次机会按**物理尝试**计，不按模型调用计：

| 消耗机会的动作 | 不消耗的 |
| --- | --- |
| 模型调一次工具 | 引擎的临时故障重试（同参数，结果不会变） |
| 引擎的 0 命中放宽重试（改了参数） | 被指纹熔断拦下的重复调用（没有 IO） |
| | 计划期被驳回的调用 |

被 PLAN 白名单拒绝的、被熔断拦下的、计划期驳回的，都不产生 IO，不该消耗机会。

---

## 20. 变更记录

| 日期 | 内容 |
| --- | --- |
| 2026-11-07 | 初版。基于 `ai_messages` id=29/30 的真实失败复盘，提出 Plan v2 + 五阶段闭环 + 四期落地 |
| 2026-11-07 | P0 落地（5 个提交），补 §18 实施状态与三处偏差；修正 `is_blank_answer` 与 status 的表述 |
| 2026-11-07 | P1–P3 全部落地（7 个提交，测试 710 → 777），补 §19 实施状态、阶段闭环图与六处偏差 |
| 2026-11-07 | **通用化改造**：把「什么算重要」从「系统预置的口径」改成「模型自己产出判据的通用机制」。移除 `important_only` / `is:important` 与提示词第 13 条里写死的三条判据；新增 `email_search` 的 `criteria` 参数（判据组，取并集、按判据优先级排序、结果标注命中判据）；放宽逻辑区分判据组与普通条件。测试 777 → 791（+14），0 failed。详见 §12 与 §19.4 |
