# AI Todolist 开发准备（开工交底）

> 对应方案：[AItodolist功能实现.md](AItodolist功能实现.md)
> 本文只回答「现在能不能开工、按什么顺序开工、开工前必须定什么」，不重复设计论证。
> 记录时间：2026-11-07 13:50，基线提交 `7455027`（master，工作区干净）

---

## 0. 结论

**可以开工。** 三条前置检查全部通过：

| 检查 | 结果 |
| --- | --- |
| 工作区状态 | 干净，只有未跟踪的 `doc/AItodolist功能实现.md` |
| 测试基线 | **710 passed / 0 failed / 6 ignored** |
| 格式与静态检查 | `cargo fmt --check` ✅、`clippy -D warnings` ✅ |

核对代码时发现 **7 处需要调整实现手法**（见 §2 标 🔧 的条目）。它们都不动摇设计，
但会改变实现手法，必须在动手前改掉认知，否则第一版代码就会写歪。

---

## 1. 基线快照

```bash
# 工作区
git status --short          # 仅 ?? doc/AItodolist功能实现.md
git branch --show-current   # master
git log --oneline -1        # 7455027 docs: 新增《版本跟踪》…

# 测试（全量，含 6 个 #[ignore] 的 Docker E2E）
cargo test --workspace      # 710 passed / 0 failed / 6 ignored（16s，增量编译）

# 门禁
cargo fmt --check                                  # 通过
cargo clippy --workspace --all-targets -- -D warnings   # 通过
```

`cargo deny` 未跑：本轮没有新增依赖，`deny.toml` 与 `Cargo.toml` 都不动，
P0–P2 全程不需要 `cargo deny`（除非 P3 引入新 crate）。

**验收方式**：每完成一个 P0 子项就跑一次 `cargo test --workspace`，
数字必须 ≥ 710 + 本轮新增数，且 failed 恒为 0。

---

## 2. 事实核对：方案的 15 条假设 vs 代码实测

| # | 方案里的假设 | 实测 | 结论 |
| --- | --- | --- | --- |
| F1 | `Event` 共 63 个变体 | **65 个**（P0 时）→ **69 个**（P2 加了四个清单事件后） | 🔧 方案已改；`CLAUDE.md` 的 63 已改成 69 |
| F2 | 协议层有第二份工具名白名单需要同步 | `me-proto/src/ai.rs:392` 用的是 `AiToolName::parse`，没有硬编码列表 | ✅ 新增变体只要登记 `parse`/`as_str` 即可，协议层自动跟随 |
| F3 | 熔断可以放在工具函数里 | `execute_ai_tool`（`ai_tools.rs:134`）统一把 `Err` 转成 `is_error` 的 trace | 🔧 **熔断必须插在调用方 `run_ai_stream` 的循环里**，插在 `execute_ai_tool` 入口会绕过 `attempts` 记账 |
| F4 | `is_progress()` 只有 3 个可丢事件 | `command.rs:868`：`SyncProgress` / `IndexProgress` / `AiStreamDelta` | ✅ 新增的 `AiPlanStepAttempt` 要加进去 |
| F5 | `ToolBudget` 可以加字段 | `ai_tools.rs:31`，`#[derive(Debug, Default)]` | ✅ 加 `failed_fingerprints: HashMap<..>` 不破坏 `..Default::default()` |
| F6 | 阶段白名单需要在循环内替换 `tools` | `AiChatRequest.tools` 是 `pub`（`ai.rs:1205`），`request` 在循环外构建、循环内 `clone()` | ✅ `request.tools = plan_phase_tool_specs()` 可行 |
| F7 | `plan` 落库有长度限制 | `finish_assistant_message`（`me-store/src/ai.rs:449`）只对 `content` 限 256 KiB，`plan` 不限 | 🔧 需要自己守：`history` 最多 2 份、`failure.detail` 截 80 字 |
| F8 | 切页会丢清单 | `app.rs:1582` 仅在 `selected == Some(conversation)` 时写 `state.ai.plan`；`views/ai.rs:261` 在**切会话**时清 | ✅ 切页不丢、切会话才清。§10.3 的改造点是「切会话时保留正在流式的那条」 |
| F9 | 测试框架要扩展 | `MockAiProvider`（`ai_flow_mock.rs:52`）已支持脚本化 `Turn`，且记录每轮 `tool_counts` / `tool_names` | ✅ **不用改框架**，直接用 `tool_names` 断言「PLAN 阶段只下发 2 个工具」 |
| F10 | 加 `PlanStep` 字段影响面小 | 全仓 `PlanStep { .. }` 构造点 **7 处** | 🔧 见 §4，加字段会让 7 处编译失败，需先收敛构造器 |
| F11 | 清单解析可以留在 `me-sync` | `tool_plan_update`（`ai_tools.rs:1207`）内部逐字段取值 | 🔧 v2 建议把 JSON→`PlanStep` 的解析搬进 `ai_plan.rs`，否则 `validate_plan_v2` 没法单测 |
| F12 | 搜索的未读条件写法 | `me-store/src/search.rs:167`：`(m.flags & 1) = 0` | ✅ 星标照写 `(m.flags & 4) != 0`（`FLAG_FLAGGED = 1 << 2`） |
| F13 | 写操作重试风险 | `calendar_create` 用「同日历同标题 ±5 分钟」去重 | ✅ 去重窗口存在，但仍按不变量 5「写操作永不自动重试」执行，不依赖去重兜底 |
| F14 | `days` 与 `after`/`before` 的关系没定 | 方案只写了「互斥，优先 `days`」 | 🔧 实现要写成**静默覆盖**（`days` 在时忽略 `after`），不能报「参数冲突」——那又会造出一个「校验失败→反复重试」的死路 |
| F15 | per-tool 限额与 attempts 是两套计数 | `mail_read` 限 3 次/回复（`ai_tools.rs:53`），每次执行都 +1 | 🔧 **重试会吃 per-tool 限额**：同一封未缓存的邮件重试 3 次正好打满，第 4 次会得到「已读 3 次」这个**新错误**，掩盖真正的失败原因。解法见 §6 末尾 |

---

## 3. 契约冻结（开工后不再改名）

名字一旦写进代码、测试、事件、JSON，再改就是全仓搜索。以下先定死。

### 3.1 工具

| 工具 | 参数 | 阶段 | 计预算 |
| --- | --- | --- | --- |
| `plan_submit` | `motive` / `goal` / `steps[]` | PLAN | 否（bookkeeping） |
| `answer_directly` | `motive` / `reason` | PLAN | 否（bookkeeping） |
| `plan_update` | `motive` / `steps[]` | EXECUTE | 否（bookkeeping，只能改状态与 evidence） |
| `email_search` | 原有 + `flagged_only: bool` + `days: 1..=365` | EXECUTE | 是 |

`steps[]` 字段：`id` / `title` / `kind` / `capability` / `tool` / `args` / `expect` / `on_empty`（`plan_submit`）；
`plan_update` 只接受 `id` / `status` / `evidence`。

### 3.2 枚举值（JSON 里的小写下划线形式）

| 枚举 | 取值 |
| --- | --- |
| `StepKind` | `tool` / `reason` / `ask` |
| `OnEmpty` | `narrow` / `ask` / `accept` |
| `StepFailure` | `invalid_args` / `repeated_call` / `transient` / `denied` / `expect_miss` / `upstream` / `budget` |
| `Diagnosis` | `wrong_plan` / `wrong_args` / `missing_data` / `tool_broken` / `user_ambiguous` |
| `Verdict` | `done` / `partial` / `failed` |

`PlanStatus` 沿用现有 5 个，**不新增**（`Ask` 是 `kind`，不是状态）。

### 3.3 Plan v2 字段顺序（JSON 稳定，便于 diff 与测试）

```text
Plan:     version, id, goal, steps, replans, history, created_at, updated_at
PlanStep: id, title, kind, capability, tool, args, expect, status,
          attempts, evidence, failure, on_empty
ReplanRecord: round, diagnosis, reason, changes, failed_steps, at
```

全部带 `#[serde(default)]`，`version` 缺省 = 1。**不加 `deny_unknown_fields`**——
否则以后加字段会让老库的 plan 解析失败，被 `parse_plan` 静默降级成「无清单」。

### 3.4 事件

| 事件 | 载荷字段 | 可丢 |
| --- | --- | --- |
| `AiPlanUpdated`（扩展） | `conversation` / `message` / `plan` | 否 |
| `AiPlanStepAttempt` | `conversation` / `message` / `step_id` / `attempt` / `max` / `tool` / `args_digest` / `error` | 是 |
| `AiPlanSelfCheck` | `conversation` / `message` / `verdict` / `failed_step` / `diagnosis` / `reason` | 否 |
| `AiPlanReplanned` | `conversation` / `message` / `round` / `diagnosis` / `changes` | 否 |
| `AiPlanFinished` | `conversation` / `message` / `verdict` / `done` / `total` / `replans` | 否 |

命名用 `AiPlan*` 前缀（现有是 `AiPlanUpdated`，保持一致），不要另起 `AiTodo*`。

### 3.5 常量（唯一一份，代码里按此拼写）

```rust
// me-core/src/ai_plan.rs
pub const PLAN_VERSION_V2: u8 = 2;
pub const MAX_PLAN_STEPS: usize = 8;        // 6 → 8
pub const MAX_STEP_ATTEMPTS: u8 = 3;
pub const MAX_REPLANS: u8 = 2;
pub const MAX_PLAN_HISTORY: usize = 2;
pub const MAX_STEP_ARGS_BYTES: usize = 1024;
pub const MAX_STEP_FAILURE_CHARS: usize = 80;

// me-core/src/ai.rs
pub const MAX_TOOL_ROUNDS: u32 = 24;        // 6 → 24
pub const MAX_TOOL_CALLS: u32 = 24;         // 12 → 24

// me-sync/src/ai.rs
const STREAM_BUDGET: Duration = Duration::from_secs(600);   // 300 → 600
const MAX_BLANK_RETRIES: u32 = 1;
const PHASE_PLAN_BUDGET: Duration = Duration::from_secs(60);
const PHASE_EXECUTE_BUDGET: Duration = Duration::from_secs(300);
const PHASE_REFLECT_BUDGET: Duration = Duration::from_secs(45);
const PHASE_FINAL_BUDGET: Duration = Duration::from_secs(45);
const RETRY_BACKOFF: [Duration; 2] = [Duration::from_millis(300), Duration::from_millis(900)];
```

---

## 4. 编译触点：改一处会连带失败的地方

这一节是给「动手时不想被 20 个编译错误淹没」用的。按改动类型归类。

### 4.1 新增两个 `AiToolName` 变体（`PlanSubmit` / `AnswerDirectly`）

| 位置 | 改什么 |
| --- | --- |
| `me-core/src/ai.rs:582` | `as_str()` 加两个分支 |
| `me-core/src/ai.rs:600` | `parse()` 加两个分支 |
| `me-core/src/ai.rs:621` | `label()` 加两个分支（UI 不显示，但要有） |
| `me-core/src/ai.rs:645` | `is_replay_safe()`：两个都 `true` |
| `me-core/src/ai.rs:665` | `is_bookkeeping()`：三个都 `true` |
| `me-core/src/ai.rs:1301` | `enabled()`：两个都 `true`（穷尽 match 会强制你写） |
| `me-core/src/ai.rs:1601` | `TOOL_SPECS` 加两条规格 |
| `me-sync/src/ai_tools.rs:153` | `execute_ai_tool` 的 `match call.name` 加两个分支（`plan_submit` 走纯内存，`answer_directly` 返回固定回执） |

`me-proto` **不用改**（F2：它只调 `parse`）。

### 4.2 给 `PlanStep` 加 7 个字段

现有构造点 7 处，加字段后全部报「missing field」：

| 位置 | 处理 |
| --- | --- |
| `me-core/src/ai_plan.rs:249`（`validate_plan` 内部） | 改走 `PlanStep::new(...)` |
| `me-core/src/ai_plan.rs:350`（测试 helper `step()`） | 内部改走 `PlanStep::new(...)` |
| `me-store/src/ai.rs:1023`、`:1030` | 测试里的样例数据，改构造器 |
| `me-store/tests/ai_store.rs:670`、`:677` | 同上 |
| `me-sync/src/ai_tools.rs:1289`（`tool_plan_update` 解析） | v2 重写为 `ai_plan::parse_step(value)` |

**先决动作**：在 `ai_plan.rs` 里加构造器
```rust
impl PlanStep {
    /// 新步的默认形态：kind=tool、attempts=0、on_empty=narrow，其余为空。
    pub fn new(id: u8, title: &str, capability: Capability, status: PlanStatus) -> Self { .. }
}
```
把 7 处构造点先收敛到它，**再加字段**。这样「加字段」只改一个函数体，7 处调用点一个字都不用动。
顺序反了的话，第一次 `cargo check` 会同时报 7 个 missing field，很难判断是哪个改漏了。

### 4.3 扩展 `AiToolTrace`

`me-core/src/ai.rs:756` 加 `step_id: Option<u8>` / `attempt: Option<u8>`，
并检查 `me-store/src/ai.rs` 的 `ToolTraceRow::from_trace` 往返（`tool_trace` 列是 JSON，
老数据缺字段必须能读）。

### 4.4 扩展 `SearchQuery`

`me-core/src/model.rs:1036` 加 `flagged_only: bool`。构造点：
`me-store/src/search.rs`（SQL）、`me-app` 的搜索提交处、`me-sync/src/ai_tools.rs:406`、
`me-core/src/search_syntax.rs` 的 `to_query`（如果有）。全部补 `false`。

---

## 5. 测试基建评估

### 5.1 已够用的部分

`MockAiProvider` 已经能做 golden case 需要的一切：

- 脚本化多轮（`VecDeque<Turn>`，`Turn::text` / `Turn::with_tool` / `Turn::error`）；
- 记录每轮 `tool_names` → 直接断言「PLAN 阶段只有 `plan_submit` 与 `answer_directly`」；
- 记录 `message_counts` → 断言工具结果确实回灌；
- `start_service(&store, provider, true)` + `wait_for(&mut rx, pred)` + `latest_conversation(&store)`。

**结论：golden case 不需要扩测试框架，只需要写剧本。**

### 5.2 要新增的两个小工具

```rust
// ai_flow_mock.rs 顶部，与 Turn 并列
fn tool_call(id: &str, name: AiToolName, args: serde_json::Value) -> AiToolCall {
    AiToolCall { id: id.to_string(), name, arguments: args, parse_ok: true }
}

/// 从 `ai_messages` 里读回落库的 plan（断言最终态用）。
fn stored_plan(store: &Store, conversation: AiConversationId) -> Option<me_core::ai_plan::Plan>;
```

第二个是必需的：现在测试只断言事件里的 `plan`，但方案的不变量是「落库的是最终态」，
不查库就验不到 `finalize_plan` 的行为。

### 5.3 测试数据

**不需要导出真实邮件。** golden case 的剧本是「模型行为序列」，参数用占位值
（`{"folder": "INBOX"}` 代表那次必然被拒的形态）。真实库里 `tool_trace` 也只存
摘要不存参数，本来也导不出来。

---

## 6. 开发顺序（P0，五步，每步一次提交）

每一步都独立可验收、可回滚。顺序按「依赖方向 + 风险从低到高」排。

### 步骤 1：商店层加星标检索（最低风险，无行为变化）

- `me-core/src/model.rs:1036` `SearchQuery` 加 `flagged_only`
- `me-store/src/search.rs` 加 `(m.flags & 4) != 0`
- 所有构造点补 `false`
- 测试：`crates/me-store/tests/store_schema.rs` 加 `search_flagged_only`（插 2 封带 FLAGGED、1 封不带，断言只回 2 封）

验收：`cargo test -p me-store` 全绿；UI 搜索行为不变。

### 步骤 2：`email_search` 契约对齐（修 D1）

- `me-core/src/ai.rs:1608` schema 加 `flagged_only` / `days`
- `me-sync/src/ai_tools.rs:353` 校验改为「至少一个有效条件」，文案列出全部九个条件
- `days` → `since` 的换算（今天由 `me-sync` 传入，保持 `search_syntax` 那套纯函数约定）
- 测试：`ai_flow_mock.rs` 加两条 —— 只传 `days` 能成功；只传 `folder` 且无任何条件时报错文案含 `flagged_only`

验收：**这是 P0 的关键一步**，D1 消掉后 6/8 次失败从源头消失。

### 步骤 3：指纹熔断（修 D4）

- `ToolBudget` 加 `failed_fingerprints: HashMap<String, String>`
- 熔断逻辑写在 `run_ai_stream` 的 `for call in &turn.tool_calls` 循环里（F3），
  在 `execute_ai_tool` **之前**判断
- `InvalidArgs` 第 2 次同指纹不执行、第 3 次判 failed
- 测试：`same_args_error_short_circuits`（同一份参数连发 3 次，断言真实执行 1 次、第 2 次 summary 含「相同参数」）

验收：8 连击场景压到 1 次执行（golden case Case B）。

### 步骤 4：空正文守卫（修 D5）

- `is_blank_answer` + 一次重问 + 引擎兜底文本
- 兜底文本从 `plan` 的 evidence 拼（此时 plan 可能为 None，要有「无清单」分支）
- 状态保持 `Complete` + `error`（`status` 列有 CHECK 约束，`partial` 要迁移）
- 测试：`blank_answer_gets_fallback_summary`

验收：落库 `content` 非空、`status != Complete`、`error` 非空。

### 步骤 5：提示词（只加第 13 条 + 删豁免，**不动工具名**）

- 新增第 13 条「重要」判据
- 第 10 条删掉「一句话能答完不要建清单」，但仍以 `plan_update` 为主
- **不要**在这一步写 `plan_submit`（工具还不存在，见方案 §14）

验收：`SYSTEM_PROMPT` 里不再出现「不要建清单」；跑一次真机对话确认模型仍能正常建/不建清单。

### P0 收尾时必须一起处理的 F15

重试与 per-tool 限额会打架，且只在「同一封邮件读不到正文」这种场景暴露——正是那次失败里
「第 4 步只读到 2/3 封」的形态。两个可选做法：

| 做法 | 改动 | 取舍 |
| --- | --- | --- |
| A. 失败读缓存（推荐） | `ToolBudget` 加 `failed_reads: HashSet<MessageId>`，同一封读失败后直接返回「未缓存」，**不消耗** `mail_read` 限额，但仍消耗 attempts | 精确：限额只统计「真的读了一次」 |
| B. 等比放宽限额 | `MAIL_READ_LIMIT` 3 → 5 | 粗糙：会掩盖「模型乱读 5 封」的滥用 |

选 A。注意 A 与 `context_refs` 短路是两个不同的集合：前者是「这次读失败过」，
后者是「用户已经把正文放进上下文了」，不要合并。

同理检查 `calendar_read` 与 `weather`：前者有 5 次限额（重试一次日程查询不冤枉），
后者是 1 次/回复（天气重试必然撞限额）——天气的网络错误应当**由引擎重试**而不是让模型
再发一次，否则第二次一定拿到「本次消息已查询过天气」。这一条要在 `Transient` 重试路径里
直接复用上一次的结果，不进模型轮次。

---

## 7. 第一条红灯测试（可直接粘贴）

放在 `crates/me-sync/tests/ai_flow_mock.rs`，紧邻 `tool_events_carry_motive`：

```rust
/// D1 的回归：只给时间窗（不给关键词/发件人/主题/收件人）也必须能检索。
/// 修复前：报「至少需要一个检索条件」；修复后：正常返回（哪怕 0 命中）。
#[tokio::test]
async fn email_search_accepts_time_window_without_keyword() {
    let (_dir, store) = open();
    let call = tool_call(
        "c1",
        AiToolName::EmailSearch,
        json!({
            "motive": "用户问最近有什么邮件，按时间窗检索",
            "days": 7,
            "limit": 5
        }),
    );
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(call),
        Turn::text("近 7 天没有新的邮件。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider, true);

    commands.send(Command::CreateAiConversation { model: None }).expect("send");
    let _ = wait_for(&mut rx, |e| matches!(e, Event::AiConversationsChanged)).await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "最近有什么邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");

    let completed = wait_for(&mut rx, |e| matches!(e, Event::AiToolCompleted { .. })).await;
    let Event::AiToolCompleted { status, summary, .. } = completed else {
        panic!("期望工具完成事件");
    };
    assert_eq!(status, me_core::ai::AiToolStatus::Ok, "时间窗是合法检索条件");
    assert!(
        !summary.contains("至少需要一个检索条件"),
        "不应再出现 D1 的报错文案，实际：{summary}"
    );
}
```

这条测试在改动前会红（`status == Error`），改完 `days` 后变绿。

---

## 8. 三个待拍板项（已给默认值，可先按默认开工）

| # | 问题 | 默认取值 | 影响 |
| --- | --- | --- | --- |
| Q1 | 纯闲聊是否也强制走一次 `answer_directly`（多 1 次模型往返，约 2–4 s） | **是** | 否则「强制」留有缺口：模型可以既不建计划也不声明豁免 |
| Q2 | 计划步数上限 6 → 8 | **8** | UI 清单条最多 8 行（展开态约 240 px）；预算够 |
| Q3 | 保留 `plan_update` 作为第三个 bookkeeping 工具，还是合并进 `plan_submit(mode=...)` | **保留三个** | 合并会让 schema 出现 5 种 mode 分支，校验函数难写；代价是 `enabled()`/`is_bookkeeping()` 各多一个分支 |

另有两条已按方案定死、无需再议：P0 不动工具名（§6 步骤 5）；写操作永不自动重试。

---

## 9. 开工检查表

- [x] 工作区干净，基线提交 `7455027`
- [x] `cargo test --workspace` = 710 / 0 / 6
- [x] `cargo fmt --check`、`clippy -D warnings` 通过
- [x] 方案里的 15 条假设已逐条核对，7 处需要调整实现手法（F1 / F3 / F7 / F10 / F11 / F14 / F15）
- [x] 契约（工具名、字段名、枚举值、事件名、常量）已冻结
- [x] 编译触点已列全（新增枚举变体 8 处、`PlanStep` 构造 7 处）
- [x] 测试基建确认够用（`MockAiProvider` 无需扩展）
- [x] 第一条红灯测试已写好，可直接落盘
- [x] Q1–Q3 按默认值执行（纯闲聊走 answer_directly / 8 步 / 保留 plan_update）
- [x] P0 五步已全部落地（见 §11）

---

## 10. 变更记录

| 日期 | 内容 |
| --- | --- |
| 2026-11-07 | 初版。基线核对 + 13 条假设实测 + 契约冻结 + P0 五步顺序 + 首条红灯测试 |

---

## 11. P0 执行记录（2026-11-07，已完成）

| 步骤 | 提交 | 结果 |
| --- | --- | --- |
| 1 星标检索 | `87fee7d` | `SearchQuery.flagged_only` + `(m.flags & 4) != 0`；`SavedSearchQuery` 同步加字段带 serde default |
| 2 契约对齐 | `5d73f5d` | 九条件判定、报错列全、`flagged_only` / `days`、空结果文案可操作 |
| 3 指纹熔断 | `6d7fdaf` | `ToolBudget.failed` + 剔除 `motive`/`step_id` 的指纹；拦截在计数之前 |
| 4 空正文守卫 | `2cc524a` | 一次催促 + 引擎兜底摘要 |
| 5 提示词 | `44e7fd5` | 删豁免、加第 13 条「重要」判据 |

**测试 710 → 718（+8），0 failed / 6 ignored；fmt 与 clippy 全程通过。**

### 11.1 实施中撞到的两个真问题（准备阶段没预见）

1. **判空阈值不能用字符数。** 第一版 `count() < 8` 把 `记住了。`、`已删除。` 判成空白，
   触发强制重问 → 两个既有测试脚本耗尽 → 失败。改成「一个可见字符都没有」后恢复。
   结论：引擎只判断**有没有回答**，不判断**好不好**。
2. **`status` 有 CHECK 约束**（`streaming/complete/failed/cancelled`），设计稿里的
   `Partial` 落不了地，除非做表重建迁移。P0 改为 `complete` + `error`，
   语义由兜底正文首句承担。**P2 若要 `partial`，必须和 v8 迁移一起做。**

### 11.2 F15 的最终解法（与准备文档的建议不同）

§2 的 F15 建议用 `ToolBudget.failed_reads: HashSet<MessageId>`——「失败读短路且不消耗限额」。
**实现时没采用**，改成了 `ToolBudget::quota_snapshot()` / `restore_quota()`：
重试前把配额整体退回快照。

为什么换：那个方案只解决 `mail_read` 一个工具，而「重试会重复计配额」是**所有**带
per-tool 限额的工具的通病（天气 1 次/回复、日程 5 次、记忆 5 次……）。
配额退回是通用解法：重试是**同一次逻辑调用**，配额已经付过一次了。

```
执行前 snapshot = budget.quota_snapshot()
重试前  budget.restore_quota(snapshot)   ← 第二次不会撞「本次消息已查询过天气」
```

P2 已实现并有测试（`restoring_the_quota_gives_the_slot_back`）。

### 11.3 P1–P3 的执行顺序（已全部完成）

P1 的第一件事不是写 `plan_submit`，而是**先把 `PlanStep` 的构造点收敛到 `PlanStep::new`**
（7 处，见 §4.2），否则加 7 个字段会一次报 7 个 missing field。
实际顺序：

1. **P1**：收敛构造器 → Plan v2 结构 → 工具规格 → 首轮白名单 → 清单条 UI；
2. **P2**：`SelfCheck` + `reconcile` → `validate_replan` → 错误分级与引擎重试 →
   0 命中放宽 → 阶段预算 → 四个新事件；
3. **P3**：`is:important` / `important_only` → 跨轮延续 → 清单统计。

每一步一次提交 + 全量门禁。最终 **测试 710 → 777（+67）**，0 failed / 6 ignored。

### 11.4 准备阶段没预见、但撞出来的四个真问题

1. **放宽条件必须由引擎做，不能回灌给模型。** 第一版把放宽后的参数回灌给模型，
   让它下一轮重试——但模型常常换个毫不相干的关键词。改成引擎自己在同一轮内改参数重试。
2. **0 命中不是失败。** 第一版把「放宽到底仍无结果」判成失败，结果触发 REFLECT 白跑一轮，
   用户还看到红色叉号——而「最近 30 天没有这样的邮件」本来就是正确答案。
3. **放宽不能把时间窗整段丢掉。** 丢掉等于把「最近 7 天的未读」变成「所有未读」——
   那是换了一个问题，不是放宽了同一个问题。改成扩到 30 天。
4. **续做提示必须在第一次请求里发出。** 第一版在「模型没调工具」的分支里补提示，
   但模型第一轮直接提交计划是**正常路径**——那时再补就晚了，它已经规划完了。
