# AI 工具调用链优化

> 范围：v0.4.0 **当前实现**（`crates/me-sync/src/ai.rs`、`ai_tools.rs`、`ai_music.rs`、`crates/me-core/src/ai.rs`、`ai_plan.rs`），不含规划中的功能。
> 依据：代码逐层通读 + 2026-11-09 两次真实对话的完整回放（`ai_messages` 的 `plan` / `tool_trace` / `thinking` / `error`，`logs/myemail.2026-11-09.log`）。
> 定位：本文只回答两件事——**现在哪里会坏**，**先改哪几处**。
>
> **状态（2026-11-09 收尾）**：11 项已修（P0-1/2/3、P1-1/3/4/5/6、P2-1/2/3），
> 1 项（P1-2 软失败第三态）评审后**明确不做**并写明理由。没有「待定」「择一」项。
> 实施记录见 §6.1（第一批）/ §6.2（第二批）/ §6.3（第三批）/ §6.4（收尾）。

---

## 0. 结论

链路本身是完整的：计划 → 执行 → 自检 → 重列 → 收尾，九道闸门一道不少，失败指纹、配额回溯、阶段化提示这些设计都比常见实现细。

问题集中在**层与层之间的事实口径不一致**，而不是某一处逻辑写错：

- 计划里的参数**从不参与执行**，执行的是模型另发的一份 `tool_calls`——清单是说明书，不是合同（P0-3）；
- 工具成功、清单全绿、用户什么都没看到——因为「模型没写正文」被兜底成「没拿到结果」（P0-1 / P0-2）；
- 临时故障、软失败、参数错误三种情况在 `AiToolStatus` 里只有两个格子，统计与清单因此会说谎（P1-2）；
- 出问题后只能靠 `thinking` 反推，`tool_trace` 里连调用参数都没有（P1-1）。

| 编号 | 级别 | 一句话 | 本次是否已修 |
|---|---|---|---|
| P0-1 | 高 | 收尾阶段收回工具，与「每步都要 plan_update」直接冲突，模型空转后交白卷（成因为推断，见 §6） | **第一批已修**（条件化 + 出口只开一次） |
| P0-2 | 高 | 兜底文案与同一条消息的 `[结果]` 自相矛盾，且写死邮件场景 | **第一批已修**（直出正文 + 按能力分支） |
| P0-3 | 高 | 计划参数不执行、`{{step.N}}` 引用从不替换、兜底绑定会把回执记到别的步 | **第一批已修**（写清契约 + evidence 记实际工具） |
| P1-1 | 中 | `AiToolTrace` 无 args；收尾被丢弃的调用不留痕；无按轮日志 | **第一批已修** |
| P1-2 | 已降 P2 | 软失败（0 命中 / 地点不明确 / 正文未缓存）与成功同色——**评审决议不做** | 否 |
| P1-3 | 中 | `weather_get` 1 次/消息封死「多城市对比」（只改限额，不改计费） | **第二批已修**（1 → 3 次，文案说明限额） |
| P1-4 | 中 | 工具结果每轮全量重发，且这条路径不受 `max_input_chars` 约束 | **第二批已修**（循环内压缩 + 预留 schema 预算） |
| P1-5 | 中 | 没有「无进展」判据，催促各占一轮，1 步任务最坏十几轮 | **第二批已修**（连续 3 轮无业务结果即收尾） |
| P1-6 | 中 | `criteria` 与普通条件同时给时，普通条件被**静默忽略** | **第一批已修**（计划期 + 执行期显式报错） |
| P2-1 | 低 | 超时文案写「超过 600 秒」，实际 `STREAM_BUDGET = 300` | **第一批已修**（文案跟常量走） |
| P2-2 | 低 | `repair_plan_step` 只对 `plan_submit` 生效，`plan_update` 不修参数名 | **收尾已修**（结论：不是缺陷；真正的问题是 `title` 必填，已改为可省略） |
| P2-3 | 低 | 音乐超时曾被归成「参数错误」（已改 `CoreError::timeout`，记录在案） | 是 |

---

## 1. 现状：一次工具调用要过九道闸

```
Plan ──plan_submit──▶ Execute ──工具成功且清单满了──▶ Final ──▶ 落库
                        │                              ▲
                        └──有失败步──▶ Reflect ──重列──▶ Replan ──┘
```

阶段切换本身就要一次模型往返。`Phase::Reflect` / `Phase::Final` **不下发任何工具**（`ai.rs:1498-1504`），`phase_allows` 在服务端二次拦截（`ai.rs:2656-2667`）。

| # | 闸门 | 位置 | 作用 | 风险 |
|---|---|---|---|---|
| 1 | 计划期参数校验 | `ai_plan::validate_plan_v2` → `check_args_against_schema` | 参数名 + schema `required` + `email_search` 至少一个条件 | 已补齐（2026-11-09）；仍是「键在不在」，不管值对不对 |
| 2 | 工具白名单 | 同上，`allowed_tools` | 按权限过滤 | 正常 |
| 3 | 阶段化下发 | `request.tools = plan_tools/execute_tools/replan_tools/[]` | 每阶段只给该给的 | 收尾期设计过严，见 P0-1 |
| 4 | 执行期权限 | `execute_ai_tool` → `permissions.enabled` | 二次校验 | 正常 |
| 5 | 配额 | `ToolBudget`（`ai_tools.rs:320-325`） | 每消息限额 | 失败也计费，见 P1-3 |
| 6 | 同参熔断 | `previous_failure` / `intercepted_result` | 不可重试的失败不再重发 | 正常，是防「8 连击」的关键 |
| 7 | 超时 | `with_timeout`（`ai_tools.rs:339`） | 各工具 2–20s | 超时归 `Transient`，可重试 |
| 8 | 错误分类 | `classify_tool_failure`（`ai_tools.rs:294-306`） | `Validation→InvalidArgs`、`Limit→Denied`、其余按 `is_retryable` | 语义太粗，见 P1-2 |
| 9 | 记账 | `bind_step` + `record_attempts` | 把回执写到清单某一步 | 兜底绑定会错绑，见 P0-3 |

收尾三件套：`fallback_answer`（`ai.rs:2813`）、`synthesized_result_block`（`ai.rs:2858`）、`finalize_plan`（`ai.rs:2934`）。**用户最终看到的东西，主要由这三处拼出来，而不是模型那句话。**

---

## 2. P0：会让用户「明明查到了却什么都没有」

### P0-1 收尾阶段收回工具，与规则 10 正面冲突

**现象**（2026-11-09 19:17 真实对话，输入 2496 / 输出 203 tokens）

用户问「天气怎么样」。`weather_get` 1ms 查成功（双流），清单 1/1 完成，然后模型交了一张白卷，最终答复是引擎兜底的「这次没有拿到可用的结果」。

**链路**

1. 工具成功 → 引擎 `record_attempts(Done)` **自动**记账 → `has_open_steps()` 变假；
2. `ai.rs:2359-2380`：无失败步 → 不进 Reflect，**直接切 Final**，并追加一句「计划已执行完，请给最终答复」；
3. 但系统提示规则 10 要求「每完成一步就调用一次 `plan_update`」，模型仍想补这一次记账；
4. Final 阶段 `request.tools = []`，模型发来的工具调用被**直接丢弃**（`ai.rs:1800-1819`，连 trace 都不留）；
5. 丢一轮、催一轮、空正文再催一轮（`ai.rs:1717-1732`）→ 放弃 → 兜底。

`thinking` 里留着证据：第 3 段「Let me update the plan step and then provide the final response」，第 4 段已经把答案要点列好了（13°C、体感 11、逐时、近日），正文却是空的。

**证据强度**：以上成因是**推断**，不是实测。`ai.rs:1795-1798` 的注释写明这类调用「有时填在 `tool_calls` 里，有时写成正文里的标签」，两种都会走到同一个丢弃分支；而当前没有任何按轮日志，无法区分「模型只输出了工具标签」与「模型什么都没输出」。这两种的修法不同（前者放行 `plan_update` 有效，后者只能靠催答），所以第一批先补日志确认成因，见 §6。

**根因**：引擎自动记账与提示词「手工记账」是同一件事的两套入口；收尾期又把 `plan_update` 一起收走，模型只能把这一轮花在「汇报完成」上。

**建议**（择一，推荐 A+B）

- A. 收尾阶段放行 `plan_update`：它是纯内存、无 IO、不计入 `MAX_TOOL_CALLS`，收回它没有任何收益。需同步 `phase_allows` 与 `phase_system_suffix`。
- B. 切 Final 的那句提示改写成「第 1 步已完成（回执：…）。**不要再更新清单**，直接回答用户」——把「已记账」这个事实明确告诉模型，而不是让它自己推断。

### P0-2 兜底文案与事实矛盾，且只认邮件场景

**现象**：同一条消息里

> 这次没有拿到可用的结果。

> [结果] 状态：已完成 … 做了什么：查询天气（双流 · 成都 · 四川省 天气）

后面还有「① 让我先同步最近的邮件；② …「这周未读的邮件」」——一次天气对话。

**根因**（`ai.rs:2813-2840`）

- 首句写死「这次没有拿到可用的结果」，而触发条件是「模型没写正文」（`ai.rs:2433`），不是「工具没成功」；
- 建议句写死邮件场景；
- 兜底只拿到 `trace.summary`（"双流 · 成都 · 四川省 天气"），**拿不到 `AiToolResult.content` 里的完整预报表**——数据在手，交不出去。清单里的 `evidence` 同样只存 `summary`（`ai.rs:2073`），所以连「用清单拼一份答复」这条路也走不通。

**建议**

- 兜底按「有没有成功回执」分支：有回执就说「工具已执行，模型没有把结果写出来」，并把最近一次成功的 `content` 截前 N 行给用户；
- 建议句按 `Capability` 生成（邮件 / 天气 / 日历 / 音乐各一套）；
- `synthesized_result_block` 的状态与首句必须同源，不能一处「已完成」一处「没拿到」。

### P0-3 计划不是执行的契约

三件事同源：**引擎只把计划当展示与校验对象，不当执行依据**。

1. **`plan.args` 从不执行**。执行用的是模型下一轮另发的 `tool_calls`（`ai.rs:1823`）。`step.args` 只出现在 `args_digest()`（界面显示）与计划期校验里。17:52 那次，计划写 `location_id=shuangliu`（库里根本没有这个 id），实际调用却带了别的地点参数并成功——事后无法从库里复现。
2. **`{{step.N.…}}` 引用从不替换**。`refs_in_args` 只用于「只许引用前面的步骤」和失败级联（`ai_plan.rs:2016`、`1141`），没有任何替换实现。模型若真按引用写，执行期会拿到字面量；好在提示词里从没宣传过这个语法，属于**死特性**。
3. **`bind_step` 兜底错绑**。按「同工具未终结的步 → 第一个未终结的步」降级（`ai_tools.rs:216-242`），于是步骤写 `weather_locations`、实际调 `weather_get` 也能绑上，`evidence` / `failure` 记到错的步骤头上。17:52 的清单就出现了「weather_locations 这一步失败，原因是 weather_get 的参数错误」。目前只加了 `tracing::warn!`。

**建议**

- 执行时把**实际参数**记进这一步（或至少记 `digest`），并与 `step.args` 比对：不一致就标记出来（UI 上显示「实际调用参数」），这是清单能「对账」的前提；
- `{{step.N}}` 三选一：实现替换 / 删掉校验与相关文档 / 明确标注「仅供依赖分析」。现状是校验了却不生效，最差的一种；
- 绑定时若 `declared != called`，除了告警，把这条记成 `step.failure = ToolMismatch`，让清单如实说「这一步实际用了另一个工具」。

---

## 3. P1：不会立刻炸，但会让问题查不出来、越修越乱

### P1-1 可观测性缺口（三个）

| 缺什么 | 后果 | 证据 |
|---|---|---|
| `AiToolTrace` 没有 `args`（`me-core/src/ai.rs:793`） | 事后无法知道「它到底传了什么」 | 两次复盘都只能猜天气调用带了哪个地点 |
| 收尾期被丢弃的调用不写 trace | 只能从 `thinking` 看出「它想调工具」，日志里一无所有 | `ai.rs:1800-1819` 只 `continue` |
| 没有按轮日志 | 日志只有一条「清单统计」，阶段/轮次/工具序列全不可见 | `logs/myemail.2026-11-09.log` 19:17 段仅 2 行 |

**建议**：`AiToolTrace` 增 `args`（截断 200 字符、去掉 motive/step_id）；丢弃的调用记一条 `status=error, summary="收尾阶段不能调用工具"`；每轮落一行 `INFO round=… phase=… tools=… ms=…`。

### P1-2 软失败与成功同色（已决议降为 P2：不做）

`0 命中`、`地点不明确`、`正文未缓存`、`曲库没有匹配` 都返回 `Ok`，于是 `AiToolStatus::Ok` + 步骤 `Done` + 统计「0 失败」。2026-11-09 修的只是「失败被标 done」；这一类是「成功但空」。

**建议**：`AiToolStatus` 增 `Partial`（或 `AiToolResult` 增 `outcome` 字段），只影响展示与统计，**不影响重试与清单终态**。清单里显示「完成（无结果）」而不是「完成」。

> **2026-11-09 决议：不做。** 三类软失败的 `evidence` 已经写清（「未找到（0 命中）」「地点不明确」「正文未缓存」），用户看清单就知道发生了什么；兜底改为「直出工具正文」之后，「完成但没结果」的矛盾感已被消掉大半。加第三态要动公共枚举 + 落库 + UI + 三处统计，收益只剩统计口径更准，不值。真需要时再提。

### P1-3 限额口径

- `WEATHER_LIMIT = 1`（`ai_tools.rs:323`）：**一条消息只能查一个地点**。而 `weather_get` 本身也只支持单地点 → 「成都和北京哪个热」在不改工具的前提下无解。这不只是限额，是功能缺口。
- `executed_calls += 1` 发生在执行**之前**（`ai.rs:1903-1910`），失败调用照样吃 `MAX_TOOL_CALLS`。参数错误反复重试时，24 次的预算会被「错得很稳定」的调用快速吃掉。
- 引擎自动重试能退回配额（`restore_quota`），但只覆盖 `Transient`；其余重试路径口径不一。

**建议**：只把 `weather_get` 提到 3 次/消息（或让工具支持一次多地点），预算耗尽时给用户一句明确的「这次只查到一个地点」。
**不要**改成「按逻辑调用计费」——失败调用也占预算是一道合理的防刷闸（参数写错就狂刷的场景会被放回来），这条建议原本写错了，2026-11-09 评审时撤回。

### P1-4 请求体预算只在「进对话」时生效，工具循环内部不受约束

`build_chat_request` 会按档案里的 `max_input_chars` 裁剪**历史消息**（`trim_history`，`me-core/src/ai.rs:1711`），这层是有效的。但工具循环里每一轮是把 `tool_result` **直接 push** 进 `request.messages`（`ai.rs:2185-2193`），这条路径不经过任何裁剪：

- 单条回执上限 `MAX_TOOL_RESULT_CHARS = 24 000`，24 次调用累加无上限；
- `MAX_CONTEXT_TOTAL_CHARS = 20 000` 只作用于**引用的邮件正文**（`me-core/src/ai.rs:1693`），与工具结果无关；
- 结果就是：模型侧返回 400「上下文过长」→ 整轮失败，而失败发生在所有工具都已成功之后。

**建议**：每一轮拼请求前按 `max_input_chars` 做一次预算：优先丢最早的 `tool_result`（保留最后 N 条），其次降级为摘要；裁剪发生时往上下文里塞一句「早期结果已省略」，别让模型以为它没查到。

### P1-5 没有「无进展」判据

现有催促各占一轮：`MAX_PLAN_ROUNDS=2`、`MAX_PLAN_SUBMIT_REJECTIONS=2`、`MAX_SELFCHECK_RETRIES=1`、`MAX_REPLANS=2`、`MAX_FINAL_TOOL_NUDGES=1`、`MAX_BLANK_RETRIES=1`（后两项在 §6.1 已合并为 `MAX_FINAL_RETRIES=2`）。1 步任务固定消耗 3 轮（Plan / Execute / Final），任何一次催促 +1 轮，每轮之间还有 400ms 强制间隔。19:17 那次 1 个 1ms 的工具调用**推算**约 4 轮（当前没有按轮日志，这个数字未实测）。

**建议**：加一条 `no_progress` 判据——「本轮既没有新的工具结果、正文也没变长」连续出现 2 次 → 立即收尾并走兜底，不再追问。这是唯一能同时压住所有催促路径的闸。

### P1-6 `criteria` 与普通条件同时出现时，普通条件被静默忽略

`tool_email_search` 看到 `criteria` 非空就直接走 `email_search_by_criteria`（`ai_tools.rs:734-739`），顶层的 `query` / `days` / `flagged_only` **一个都不用，也不报错**。模型若写「星标邮件 + criteria」，实际只跑了 criteria，用户拿到的结果比预期宽——而界面上看不出任何异常。

**建议**：两者互斥在**计划期**就说清（「要么写普通条件，要么写 criteria，不要混」），或执行期明确报错。静默丢参数是这个项目里最不该出现的形态（`reject_unknown_email_args` 的注释里已经吃过一次「模型不知道缺什么」的亏）。

---

## 4. P2：细节不一致

- **超时文案**：`STREAM_BUDGET = 300s`（`ai.rs:36`），但错误文案写「工具循环总时长超过 600 秒」（`ai.rs:1457`）。
- **修复器覆盖面**（2026-11-09 收尾核实，原判断**不准确**）：`repair_plan_step` 只在 `plan_submit` 链路生效，但 `plan_update` 的步骤契约里**根本没有 `tool` / `args`**——它只认 id/title/capability/status/evidence，换方案必须走重列。所以「传错参数名」这个情形不存在，也不算缺陷。
  真正的问题是同一个契约里的**自相矛盾**：工具描述写着「只能改状态与 evidence，不能改标题」，而 schema 把 `title` 列为 `required`、解析函数也硬要标题——模型照说明只传 `id/status/evidence` 会被判「第 N 步缺少标题」，还是一条不可重试的参数错误。已改为：`title` 可省略（省略则从上一版继承），写了就必须与原标题一致；首建清单仍然必填。
- **音乐超时口径**（2026-11-09 收尾核实，已成立）：`run_music_op` 的超时已从 `Validation` 改为 `CoreError::timeout`（`ai_music.rs`），与 `with_timeout` 口径一致；它**故意不套** `with_timeout`——那会叠成 3s + 3s，调用点（`ai_tools.rs` 音乐分支）有注释说明。收尾时补了一处同类漂移：错误文案原本写死「3 秒」，现改为 `MUSIC_TIMEOUT.as_secs()`（与 P2-1 的 600 秒同一个毛病）。

---

## 5. 优化方案（三批，按性价比）

> 本节是评审前的初版排序，保留以便对照。**范围与优先级以 §6 决议为准**：P0-1 改为条件化执行、兜底改为「先补一轮再直出」、P1-2 降级不做、P1-3 只改限额。

### 第一批：止血 + 看得见（0.5–1 天）

| 改动 | 位置 | 验收 |
|---|---|---|
| 收尾放行 `plan_update`，或改写「计划已执行完」的提示语 | `ai.rs:1800`、`2359-2380`、`phase_allows` | 同输入回归：总轮数 ≤3，不再出现空正文 |
| 兜底按回执作答（带上工具返回正文），建议句按能力分支 | `ai.rs:2813`、`2433` | 「天气怎么样」不再出现「没拿到可用的结果」；天气场景不出现「同步邮件」 |
| `AiToolTrace` 增 `args`；丢弃的收尾期调用记 trace | `me-core/src/ai.rs:793`、`ai.rs:1800` | 回放任何一条失败消息，能直接看到每次调用的参数 |
| 每轮一行 `INFO`（round/phase/tool/ms/retry） | `ai.rs` 主循环 | 日志可还原完整轮次序列 |
| 修 600→300 文案 | `ai.rs:1457` | — |

### 第二批：把「对账」做实（2–3 天）

- 执行 args 与 `step.args` 比对并记录差异（P0-3）；`{{step.N}}` 三选一（P0-3）；
- `AiToolStatus::Partial` + 清单/统计分别计数（P1-2）；
- `criteria` 与普通条件互斥显式化（P1-6）；
- 限额按逻辑调用计费；`weather_get` 提到 3 次/消息（P1-3）。

### 第三批：结构性改造（3–5 天）

> **已完成**：请求预算与 `no_progress` 闸在第二批落地（见 §6.2），回归语料在第三批落地（见 §6.3）。本节保留初版排序以便对照。

- 请求体预算：按 `max_input_chars` 裁剪旧 `tool_result`（P1-4）；
- `no_progress` 收尾闸（P1-5）；
- **回归语料**：天气（有/无地点/多候选）、邮件（0 命中/多条件/criteria 混写）、日历（创建/改期/删除确认）、音乐（超时/未扫描）各 2–3 条脚本化对话，断言四件事：**总轮数**、**工具序列**、**最终正文含关键字段**、**清单终态与 `[结果]` 一致**。现在是「跑得过测试」但「说不出跑了几轮」，这才是同类问题反复出现的原因。

---

## 6. 决议（2026-11-09 评审）

评审方式：逐条追问，五条主分支（观测顺序 / P0-1 修法 / 兜底口径 / 步骤引用 / 软失败状态）全部收口。以下为**已定**，实施时不再讨论；§5 的三批范围以本节为准。

| 分支 | 决议 | 影响的条目 |
|---|---|---|
| 顺序 | **先补观测，再改行为**。先上日志，用真实对话定位「空正文」成因，再动行为 | P0-1、P0-2 |
| 观测深度 | 按轮 `INFO` + `AiToolTrace.args`（`serde(default)`，不动表结构） | P1-1 |
| 判定依据 | `dropped_calls ≥ 1` → 工具调用被丢弃；`dropped_calls == 0 && text_chars == 0` → 模型真没输出 | P0-1 |
| 兜底口径 | **先多给一轮，再有限直出**：`final_tool_nudges` 与 `blank_retries` 合并为单一 `final_retries`（上限 2）；催两次仍空，就把成功调用的工具正文原样交给用户，前置「模型没能整理这次的结果，以下是查询原文」；多条按序拼接、总长截断 4000 字符 | P0-2 |
| P0-1 修法 | **条件化预承诺**，规则见下表 | P0-1 |
| 步骤引用 | **不实现替换**；在 `plan_submit` 说明里写明「args 仅供展示与校验，执行以实际调用为准」，保留前向引用校验 | P0-3 |
| 参数对账 | 声明 ≠ 实际时，把「实际调用了 X」写进该步 evidence 尾巴（不改结构、不加 UI 组件） | P0-3 |
| 软失败第三态 | **不做**，降为 P2（理由见该节决议） | P1-2 |
| `criteria` 混写 | 进第一批：计划期或执行期显式报错 | P1-6 |
| weather 限额 | 只做 1 → 3 次/消息；**不**按逻辑调用计费 | P1-3 |
| 请求预算 / 无进展闸 | 推迟到第二批 | P1-4、P1-5 |

**P0-1 条件化规则**

| 观测形态 | 判定 | 动作 |
|---|---|---|
| `dropped_calls ≥ 1 && text_chars == 0` | 收尾期想记账、工具被收回 | 收尾期**只**放行 `plan_update`（业务工具仍禁）；切 Final 的提示改为「第 N 步已完成（回执：…），不要更新清单，直接回答」 |
| `dropped_calls == 0 && text_chars == 0` | 模型确实没输出 | 不放行任何工具，只靠 `final_retries` 催答 + 兜底直出 |
| 两者都出现 | 混合 | 两条都上 |

**第一批范围（可直接开工）**

1. 按轮日志（`round/phase/tools/dropped_calls/text_chars/ms`）+ `AiToolTrace.args` + 收尾期被丢弃的调用记 trace；
2. `final_retries` 合并与上限 2；
3. 兜底直出工具正文（按能力分支的建议句）；
4. P0-1 按上表规则执行；
5. `plan_submit` 说明写清 args 契约；
6. 声明 ≠ 实际时写进 evidence；
7. `criteria` 与普通条件混写显式报错；
8. 超时文案 600 → 300。

**验收（第一批）**：跑 1 条 1 步天气 + 1 条 2 步邮件，日志须满足：无「连续两轮 `dropped_calls ≥ 1`」；`final_retries ≤ 2`；最终正文含工具返回的关键字段（温度 / 主题）；`[结果]` 状态与清单终态不矛盾。

**尚不具备**：脚本化回归（断言轮数与工具序列）要等第三批；第一批只能人工读日志核对——这一点决定了第三批不是「锦上添花」，而是唯一能防回归的手段。

### 6.1 第一批实施记录（2026-11-09）

八项全部落地，改的是行为，不只是文案。

| 项 | 落点 | 关键实现 |
|---|---|---|
| 1 按轮日志 | `ai.rs` 主循环 | 每轮一条 `INFO`：`round / phase / tools / dropped_calls / dropped_total / text_chars / final_retries / ms`。只记计数，不记正文与参数 |
| 1 `trace.args` | `me-core/src/ai.rs`、`me-store/src/ai.rs` | `AiToolTrace.args: Option<String>`（`serde(default)`，不动表结构），去 `motive`/`step_id`、截断 200 字符；`ToolTraceRow` 同步，老行读成 `None` |
| 1 丢弃留痕 | `ai.rs` 收尾分支 | 每次被丢弃的调用写一条 `status = Skipped` 的 trace（**不是 Error**：记成失败会让 `[结果]` 变成「部分完成」，反而制造新矛盾） |
| 2 合并预算 | `ai.rs` | `MAX_FINAL_RETRIES = 2`，`final_tool_nudges` 与 `blank_retries` 合并为 `final_retries` |
| 3 兜底直出 | `ai.rs::fallback_answer` | 成功调用的正文按工具去重后按序拼接、截断 4000 字符，前置「模型没能整理这次的结果，以下是查询原文」；没有可直出内容时才退回事实陈述，且建议句按本轮用过的能力分支 |
| 4 P0-1 | `ai.rs` | 观测到收尾期丢弃的是 `plan_update` → 下一轮 `request.tools` 只下发它，且**只开一次**（`final_bookkeeping_used`）；混着业务工具时仍整轮丢弃。切 Final 的提示改成「第 N 步已完成（回执：…），不要再更新清单」 |
| 5 args 契约 | `me-core/src/ai.rs` `plan_submit` schema | 明写「args 只用于展示与校验，执行以实际调用为准」，并把原来的 `{{step.2…}}` 示例改成「引擎不会替你替换」 |
| 6 参数对账 | `ai.rs` 记账处 | 声明 ≠ 实际时，evidence 尾巴追加「；实际调用的是 X」（截断 240），不改结构、不加 UI 组件 |
| 7 criteria 混写 | `me-core/src/ai_plan.rs`、`ai_tools.rs` | 新增 `conflicting_criteria_conditions()`；计划期与执行期各查一次（计划 args 不参与执行，只查计划期会漏） |
| 8 超时文案 | `ai.rs` | `STREAM_BUDGET.as_secs()` 拼进文案，不再写死 600 |

**回归测试（新增/改写 6 个）**

- `ai_plan::tests::plan_rejects_criteria_mixed_with_plain_conditions`：混写被拒且报错点名冲突项；只带 `limit` 放行。
- `ai_plan::tests::conflicting_criteria_conditions_names_only_plain_conditions`：冲突项集合与排序。
- `ai_flow_mock::blank_answer_falls_back_to_execution_summary`（改写）：三轮空白后直出工具正文，且留 `error` 痕迹。
- `ai_flow_mock::final_phase_tool_call_is_not_executed_but_asked_for_the_answer`（改写）：丢弃的调用留下 `Skipped` trace，追问明确指出「不要再更新清单」。
- `ai_flow_mock::final_phase_bookkeeping_is_allowed_at_most_once`（改写）：第一次 `Skipped`、第二次请求里只下发 `plan_update`、之后收回。
- `ai_flow_mock::weather_without_a_location_falls_back_to_the_default_one`（加强）：断言 `trace.args` 记下实际参数且剔除 `motive`。

**一个实施中发现的事实**（写进代码注释）：放行 `plan_update` 的收益是**消费掉记账念头**，不是让它改写清单——引擎已把步骤记成终态，且 evidence 不许被模型覆盖，所以它写的任何修正都会被 `validate_plan` 挡住。出口只开一次正是因此：开更多次也只是空转。

### 6.2 第二批实施记录（2026-11-09）

三项全部落地。门禁同上：`fmt --check` / `clippy -D warnings` / 全量测试全绿（36 个测试二进制）。

| 项 | 落点 | 关键实现 |
|---|---|---|
| P1-3 天气限额 | `ai_tools.rs` | `WEATHER_LIMIT` 1 → 3；文案从「本次消息已查询过天气」改成「最多查询 3 次，已经用完，要比较更多城市请分开发问」——**旧文案会让模型以为它已经拿到数据，转头去编一个温度** |
| P1-4 请求预算 | `me-core/src/ai.rs`、`ai.rs` | 新增 `compact_tool_results()`：超预算时把最早的 `tool_result` 压成 240 字符摘要，**最近 4 条原样**；消息本身不删（`assistant(tool_calls)` 后面必须紧跟对应 tool 消息）。主循环每轮发请求前调用，首次压缩时告诉模型一次。`StreamSetup` 增 `input_budget`，并预留 `SCHEMA_RESERVE_CHARS = 12_000` 给 system 与工具 schema |
| P1-5 无进展闸 | `ai.rs` | `MAX_NO_PROGRESS_ROUNDS = 3`：连续 3 轮没拿到**业务**工具结果就收尾，`error` 记「连续 3 轮没有拿到新的工具结果，已提前收尾」。PLAN / REPLAN 不计数（这两个阶段的进展就是交计划） |

**为什么不按「丢弃」实现 P1-4**：文里原建议是「优先丢最早的 `tool_result`」。实际做不到——OpenAI / Anthropic 协议要求每个 `tool_call` 都有对应的 tool 消息，删一条整份请求就不合法了。所以这里是**压短而非删除**，语义上等价（内容不再占预算），协议上安全（配对关系还在）。

**为什么无进展阈值是 3 而不是 2**：收尾期的两次补答（`MAX_FINAL_RETRIES = 2`）要先跑完。阈值设 2 会在第 2 次空白轮就把「再给模型一次机会」掐掉，等于把第一批的决定推翻了。

**已知取舍**（写进注释）：压完仍可能超预算。此时宁可让请求大一点，也不动最近 4 条——把模型手头正在用的数据抽走，它会转头编一个答案，比 400 更糟。

**新增测试 4 个**

- `ai::tests::compact_tool_results_keeps_the_recent_ones_intact`：超预算时只压最早两条、最近 4 条原样、消息条数不变。
- `ai::tests::compact_tool_results_does_nothing_when_it_should_not`：未超预算 / 条数在保护范围内时一个字不改。
- `ai_flow_mock::stalled_progress_stops_the_loop_early`：三轮只记账不干活 → 收尾，`error` 留痕，模型往返 ≤ 4 次。
- `ai_tools::tests::weather_limit_allows_multi_city_comparison`：`const { assert!(WEATHER_LIMIT >= 2) }`，改回 1 次直接编译失败。

**还没做的**：第三批的脚本化回归语料（断言总轮数 / 工具序列 / 正文关键字段 / 清单终态一致）。第二批的两条闸现在只能靠日志人工核对。

### 6.3 第三批实施记录（2026-11-09）

第三批的核心不是再改行为，而是**把「跑过了」变成「说得清怎么跑的」**。语料落在 `crates/me-sync/tests/ai_flow_mock.rs` 末尾（前缀 `corpus_`），共用两个 helper：

- `Expect { name, tools, max_rounds, contains }` —— 一条语料的期望；
- `run_case()` —— 跑完并断言四件事：**总轮数**、**工具序列**、**正文关键字段**、**清单终态与 `[结果]` 一致**。

第 4 条是重点，做法是：语料里的模型回答**故意不写 `[结果]`**，由引擎的 `synthesized_result_block` 合成，然后断言「清单全绿 ⟺ 正文含『状态：已完成』」。这样测的是引擎口径，不是测试自己写的文案。

| 语料 | 路径 | 工具序列 | 轮数上限 |
|---|---|---|---|
| `corpus_weather_falls_back_to_the_default_place` | 没说地点 → 用默认 | `plan_submit → weather_get` | 3 |
| `corpus_weather_uses_the_named_place` | 说了地点 → 用说的那个，不再问 | 同上 | 3 |
| `corpus_mail_empty_result_is_not_a_failure` | 0 命中 → 记完成，不是失败 | `plan_submit → email_search` | 3 |
| `corpus_mail_with_results_reports_the_count` | 多条件命中 | 同上 | 3 |
| `corpus_calendar_delete_only_asks_for_confirmation` | 删除只发确认卡，库里事件还在 | `plan_submit → calendar_delete` | 3 |
| `corpus_music_timeout_is_a_readable_tool_error` | 播放器无响应 → 可读错误，整轮不失败 | `plan_submit → music_state` | 5 |
| `corpus_criteria_mixed_with_plain_conditions_is_rejected_then_fixed` | 混写被拒 → 改对 → 成功 | `plan_submit ×2 → email_search` | 4 |

**工具序列用前缀匹配而非全等**：工具失败后引擎会走「自检 → 重列」，那里会多一次 `plan_submit`。那是补救路径，不是主路径跑偏——语料要钉的是「这次走了哪条主路」。`criteria` 那条反过来要求**恰好两次** `plan_submit`，因为它测的就是「第一次必须被拒」。

**写语料时发现的两个事实**（都已写进测试注释）：

1. **「多个关注地点且全都不是默认」不可构造**：`weather_locations` 的第一条恒为默认（`insert_weather_location_conn` 里 `is_default = count == 0`）。真正的多候选（让用户选）只发生在**联网按名字搜索**时，离线语料覆盖不到——所以第二条语料改成了「用户点名城市」这个可达路径，并与第一条成对（有地点 vs 没地点）。
2. **工具失败固定多一轮**：`plan → execute → reflect → final` 是四轮，不是三轮。音乐那条的轮数上限因此是 5，而不是照抄快速路径的 3。

**加一条新语料的步骤**（三步，不用改框架）：

1. 按路径准备 store（`seed_weather_place` / `seed_mail` / 直接 `insert_*`）；
2. 按「模型会怎么走」写 `Vec<Turn>`——计划、工具调用、答复；答复**不要写 `[结果]`**；
3. 调 `run_case`，填 `Expect`（工具序列写前缀、轮数按路径给足、正文写**数据里真正有的**字段）。

**语料覆盖不到的（如实记录）**

- 联网路径：天气按名字搜索、邮件同步、模型侧 400/429——需要网络或可注入的传输层，现在靠 mock provider 的价值是「不测就不糊弄」。
- 音乐**成功**路径：回执来自 UI 侧（`Event::AiMusicCommand` → `Command::AiMusicResult`），测试里没人回执，只能测超时。
- 取消、并发占位、跨轮续做：`ai_flow_mock.rs` 里已有各自的单点测试，但没有纳入这套四件事断言。

### 6.4 收尾（2026-11-09）

三批做完后剩两项，收尾时逐项给了结论，没有留悬空。

**P2-2 修复器覆盖面——原判断不准确，真正的问题是契约自相矛盾**

`plan_update` 的步骤契约里根本没有 `tool` / `args`（只认 id/title/capability/status/evidence，换方案必须走重列），所以「参数名不会被修」这个情形不存在。同一份契约里却有一处矛盾：描述写「只能改状态与 evidence，不能改标题」，schema 又把 `title` 列为 `required`，解析函数也硬要标题。模型照描述只传 `id/status/evidence` → 被判「第 N 步缺少标题」→ `InvalidArgs`，不可重试。

这不是纸上推演：[AI限流事故复盘.md](AI限流事故复盘.md) 的工具往返表里，2026-11-07 第一轮的第 7 次调用正是 `plan_update ✗ 第 1 步缺少 title`。当时归因成「模型不会写」，实际是契约自相矛盾——按说明写反而被拒。

修法（两处，语义不变）：

- `me-core/src/ai.rs`：`plan_update` 的 `required` 从 `["id","title","status"]` 改为 `["id","status"]`，`title` 描述改成「可省略：省略就沿用原标题；标题本身不可改，写了也必须与原标题一字不差」；工具描述补一句「title 可以不传」。
- `me-core/src/ai_plan.rs`：`validate_plan` 在标题为空且 `prev` 有同 id 步骤时**继承旧标题**，只有首建清单（`prev = None`）才报「缺少标题」。解析层（`plan_step_from_json`）不做猜测，留空交给合并层。

**P1-2 软失败第三态——按决议不做，已有替代**

评审时已降为 P2 并决定不做。收尾再确认一次理由仍然成立：兜底现在会直出工具正文（§6.1 第 3 项），「完成但用户什么都没看到」这个最刺眼的表现已经消失；evidence 里本来也写着「未找到 / 地点不明确 / 正文未缓存」。给 `AiToolStatus` 加第三态要同步公共枚举、落库、UI 与三处统计，换来的只是统计口径更准。

**新增测试 2 个**

- `ai_plan::tests::title_can_be_omitted_when_updating`：省略标题继承旧标题并通过；首建缺标题仍被拒。
- `ai_tools::tests::plan_update_step_without_title_inherits_it`：解析层留空（不猜），合并层继承标题与目标。

**至此的账**：P0-1 / P0-2 / P0-3、P1-1 / P1-3 / P1-4 / P1-5 / P1-6、P2-1 / P2-2 全部关闭；P1-2 明确不做并写明理由。没有「待定」「择一」项。

## 7. 不建议做

- **不要把业务工具放回收尾阶段**：那会绕过「重列不超过 2 次」的上限，也会重新引入 2026-11-07 的 XML 标签事故。放行 `plan_update` 就够——它没有副作用。
- **不要让自检每轮都跑**：全部成功时自检只会多花一轮，还可能把好结果自贬成 `partial`（`ai.rs:2355-2358` 的取舍是对的）。
- **不要用模型自述当事实来源**：`reconcile()` 用引擎记录压模型的自检结论，`synthesized_result_block` 只陈述回执——这两处是链路里最值得保留的设计，重构时别动。
- **不要靠放宽 `MAX_STEP_ATTEMPTS` 掩盖参数错误**：参数不变，结果不变；放宽只会把「快速失败」变成「慢慢地失败」。

---

## 8. 附录

### 8.1 两次真实事故

| | 17:52「天气怎么样」 | 19:17「天气怎么样」 |
|---|---|---|
| 修复前/后 | 修复前 | 修复后（默认地点兜底已生效） |
| 计划 | 1 步 `weather_locations`，`args={}` | 1 步 `weather_get`，`args={}` |
| 工具调用 | ① Plan 期调 `weather_locations` 被拦 ② 执行期 `weather_get` 缺地点报错 | 1 次 `weather_get`，1ms，成功 |
| 清单终态 | 1/1「完成」，`failure=invalid_args` | 1/1 完成，无 failure |
| 用户可见 | 「抱歉，我无法获取实时的天气数据」 | 兜底「这次没有拿到可用的结果」+「状态：已完成」 |
| 根因位置 | 工具缺省值 + 计划期不校验必填 | 收尾阶段规则冲突 + 兜底文案 |

### 8.2 关键常量

| 常量 | 值 | 位置 |
|---|---|---|
| `MAX_TOOL_ROUNDS` / `MAX_TOOL_CALLS` | 24 / 24 | `me-core/src/ai.rs:69,75` |
| `MAX_STEP_ATTEMPTS` / `MAX_REPLANS` | 3 / 2 | `ai_plan.rs:30,32` |
| `STREAM_BUDGET` / `MIN_REQUEST_INTERVAL` | 300s / 400ms | `ai.rs:36` |
| `MAX_TOOL_RESULT_CHARS` | 24 000 | `me-core/src/ai.rs:64` |
| `MAX_CONTEXT_TOTAL_CHARS` | 20 000（仅引用邮件） | `me-core/src/ai.rs:41` |
| 配额 | 读信 3 / 天气 1 / 日历 5 / 记忆 5 | `ai_tools.rs:320-325` |

### 8.3 术语

- **回执**：工具返回给模型的那段文本（`AiToolResult.content`），与 trace 里的 `summary` 不是一回事。
- **绑定**：把一次调用挂到清单某一步（`bind_step`），决定 `evidence` 记在哪一格。
- **自检**：Reflect 阶段模型输出一段 JSON 结论，由 `reconcile()` 用引擎事实裁剪。
- **兜底**：模型没交出正文时，引擎用回执拼出的答复（`fallback_answer` + `synthesized_result_block`）。
