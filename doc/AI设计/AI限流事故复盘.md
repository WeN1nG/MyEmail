# AI 对话：两次事故复盘与修复（2026-11-07）

> 起因（第一次）：同一条提问「看看我的邮件 生成我的用户画像」连跑两次，**两次都以「请求过于频繁」失败**。
> 而两次失败之前，模型**都已经查到了 20 封邮件**——结果全丢了。

> 起因（第二次）：限流修好后同一句提问又跑了一轮，这轮**没有 429**，状态是 `complete`，
> 但用户看到的是**一坨 XML 标签**——真正的用户画像一个字都没显示。

本文记录：现象 → 根因 → 修复 → 验证。两次事故的共同教训放在最后一节。

---

## 0. 一句话结论

不是「工具调用次数上限太低」，而是**两次真实的 HTTP 429**。

`MAX_TOOL_ROUNDS = 24` / `MAX_TOOL_CALLS = 24` 一次都没碰到（实测两轮分别只用了 10 次和 6 次工具调用）。
真正杀死对话的是：模型在 `plan_submit` 上反复写错参数 → 每次被拒都产生一次新的模型请求 →
请求密度顶到服务商限流线 → 后续请求全被 429 打掉 → 整轮失败。

**一次「差一点就对了」的计划参数错误，代价是整轮对话。**

第二次是**协议层不认「文本形态的工具调用」**：模型在收尾阶段把 `plan_update` 写进了正文，
协议层只解析结构化的 `tool_calls`，于是那段 XML 原样成了最终答复。
**一句「差一点就对了」的模型输出，代价是整条回复。**

---

## 1. 现象（第一次：429）

两次真实对话的助手消息都是 `failed`，错误均为「请求过于频繁」。

从 `%APPDATA%/MyEmail/myemail.db` 的 `ai_messages.tool_trace` 里能看到全部工具往返：

### 第一轮（10 次工具调用）

| # | 工具 | 结果 |
| --- | --- | --- |
| 1 | `plan_submit` | ✗ `criteria 第 1 项没有 label` |
| 2 | `plan_submit` | ✗ `criteria 第 1 项（recent_30days）有条件「description」不认得` |
| 3 | `plan_submit` | ✗ 同上（模型原样重交） |
| 4 | `plan_submit` | ✗ `第 2 步的 args 里有 mail_read 不认识的参数「count」` |
| 5 | `plan_submit` | ✗ `第 3 步是 推理 步，不能带 tool / args` |
| 6 | `email_search` | ✓ 命中 20 封 |
| 7 | `plan_update` | ✗ `第 1 步缺少 title` |
| 8–10 | `mail_read` ×3 | ✓ |

**结果：全部丢弃，对话以 429 失败。**

### 第二轮（6 次工具调用）

同样先连错 5 次 `plan_submit`（`date_from` / 缺 `steps` / 推理步带 tool / 缺 `expect` / 推理步带 tool），
再成功 `email_search` 命中 20 封，**同样以 429 失败**。

---

## 2. 根因

### 根因 A：PLAN 阶段模型看不到业务工具的参数表

PLAN 阶段按设计只下发 `plan_submit` / `answer_directly`（这是「强制清单」的实现方式：
模型物理上没有 `email_search` 可调，想查邮件就必须先说要查什么）。

但 `plan_submit` 的 `args` 字段要填的是**业务工具的参数**，而那些 schema 这一轮**根本没发给模型**。

于是模型只能猜：

| 模型写的 | 实际应该是 |
| --- | --- |
| `date_from` | `after` |
| `count`（给 `mail_read`） | 不存在（`mail_read` 只认 `message_id`） |
| `criteria` 项里的 `description` | 不存在（判据的条件只有 11 个固定键） |

### 根因 B：参数差一点点就整单拒绝

`validate_plan_v2` 是全有或全无：一步的某个字段不对，**整份计划作废**，
模型必须重交全部步骤。

### 根因 C：没有请求间隔，撞上 429 直接判死

- 工具循环里每一轮都是一次新的模型 HTTP 请求，连续重试会让请求密度瞬间顶上去。
- `map_status` 把 429 映射成 `RateLimited`，而 `AiErrorKind::RateLimited` 的 `retryable` 是 `false`
  → 撞上一次，整轮直接失败，前面已经拿到的 20 封邮件全部作废。

---

## 3. 修复

四处改动，目标是**让「差一点就对了」不再有代价**。

### 3.1 PLAN 阶段下发业务工具参数表（`me-core/src/ai.rs`）

`plan_phase_tool_specs()` 现在接收权限快照，并把各业务工具的参数名写进
`plan_submit` 的 `args` 字段说明：

```
- email_search：after / before / criteria / days / flagged_only / folder / from /
                limit / query / subject / to / unread_only
  criteria 是数组，每项的键只能是：after / before / days / flagged_only / folder /
                                  from / has_attachment / label / query / subject / to / unread_only
- mail_read：message_id
…
```

参数表**从 `TOOL_SPECS` 现算**，不手抄——手抄的清单必然和 schema 漂移。
用户关掉的能力不会出现在表里。

### 3.2 参数宽容修复（`me-core/src/ai_plan.rs`）

新增 `repair_plan_step()`：**能唯一确定意图的就替模型改对**，改完的说明回灌给模型，
让它知道下轮怎么写。只有改不动的才交给校验层报错。

| 症状 | 处理 |
| --- | --- |
| 参数名写错（`date_from`） | 改成正确名（`after`），带类型校验（`since: 7` 与 `since: "2026-01-01"` 含义不同，不换） |
| 工具步缺 `capability` | 按工具名反推（工具与能力模块一一对应，让模型再填一遍纯属多余） |
| 推理步却带了 `tool` | 提升为工具步（模型显然想调工具，kind 写错了） |
| 工具步缺 `expect` | 用标题兜底 |
| 判据缺 `label` | 按条件反推一个能看懂的名字（「已星标」「近 7 天未读」） |
| 判据含不认的条件键 | 删掉（前提是这条判据还有别的条件——删空了就宁可报错） |

**仍然报错**（不猜）：工具名不存在、`mail_read` 的 `count` 这类猜不出意图的参数、
引用了后面的步骤、步号越界。静默丢掉参数会让检索范围悄悄变掉，
用户拿到的是一份「没兑现的理解」——那比报错更糟。

### 3.3 PLAN 参数拒绝闸门（`me-sync/src/ai.rs`）

连续 2 次 `plan_submit` 被拒后，引擎**不再等**：自己合成一份最简清单，转入执行阶段。
清单不完美，总比整轮失败好。

计数放在工具循环**之后**统一统计——被「相同参数熔断」拦下的调用会提前 `continue`，
在循环内计数会漏掉它，而那恰恰是最该计入的一种拒绝。

### 3.4 请求间隔与 429 退避重试

| 位置 | 措施 |
| --- | --- |
| `me-sync/src/ai.rs` | 两次模型请求之间留 **400ms** 最小间隔。主动留间隔比撞上 429 再退避更省：撞上意味着那一轮已经白费了 |
| `me-proto/src/ai.rs` | 429 退避重试（2s → 5s → 12s，最多 3 次）。请求还没被服务端处理，重发是安全的；退避用完仍被限流才判失败 |

---

## 4. 验证

| 测试 | 断言 |
| --- | --- |
| `ai_plan::tests::wrong_argument_name_is_renamed_instead_of_rejected` | `date_from` → `after`，改完能过校验 |
| `ai_plan::tests::unguessable_argument_name_is_still_rejected` | `mail_read` 的 `count` 仍然报错（不静默丢） |
| `ai_plan::tests::tool_step_capability_is_inferred_from_the_tool_name` | capability 按工具名反推 |
| `ai_plan::tests::reason_step_carrying_a_tool_is_promoted_to_a_tool_step` | 推理步带 tool → 工具步 |
| `ai_plan::tests::criterion_without_label_gets_one_inferred_from_its_conditions` | 判据名按条件反推 |
| `ai_plan::tests::unknown_criterion_key_is_dropped_when_other_conditions_remain` | 不认的条件键被删，真条件留着 |
| `ai_plan::tests::missing_expect_is_filled_from_the_title` | expect 用标题兜底 |
| `ai::tests::plan_phase_carries_the_business_tool_parameter_table` | PLAN 阶段带参数表 |
| `ai::tests::parameter_table_only_lists_enabled_tools` | 关掉的工具不进表 |
| `ai::tests::argument_alias_only_applies_when_the_target_is_valid` | 别名映射守规矩 |
| `ai_rate_limit.rs::rate_limit_is_retried_and_the_turn_succeeds` | 撞 429 会重试并成功 |
| `ai_rate_limit.rs::persistent_rate_limit_eventually_fails` | 退避用完才失败（不会无限挂着） |
| `ai_flow_mock.rs::almost_right_plan_is_accepted_without_a_retry` | 事故里那份计划**一次就建清单成功** |
| `ai_flow_mock.rs::repeated_plan_rejections_do_not_stall_the_turn` | 连续被拒也不卡住，整轮能走完 |

全量：**848 passed / 0 failed**；`cargo fmt --check` 与
`cargo clippy --workspace --all-targets -- -D warnings` 全通过。

---

## 5. 第二次事故：工具调用退化成正文标签

### 5.1 现象

限流修好后，同一句提问「看看我的邮件 生成我的用户画像」又跑了一轮。
这轮**没有 429**（`ai_messages.status = complete`，`error` 为空），限流修复是生效的。

但用户看到的是**一坨 XML 标签**：

```text
从搜索结果看，你的邮件涉及较多校园招聘信息。让我读取几封关键邮件的细节来获取更完整的画像。

<tool_call>
<function=plan_update>
<parameter=goal>
检索近期邮件并基于内容生成用户画像
</parameter>
<parameter=steps>
[{"capability": "mail", "evidence": "获取到近30天共95封邮件…", "id": 1, "status": "done", …}]
</parameter>
</function>
</tool_call>
```

正文里**没有 `[结果]` 段落**，而模型其实已经把完整的用户画像写进了**思考区**
（`ai_messages.thinking` 的第 5 段，`followed_by_tool = false`）——差的就是「把它写进正文」这一步。

工具往返本身是正常的：`email_search` 命中 20 封，`mail_read` ×3 成功，清单两步都是 `done`。

### 5.2 根因

**根因 D：协议层只认结构化的 `tool_calls`，不认「文本形态的工具调用」。**

模型（`agnes-2.5-flash`）这一轮没有填 `delta.tool_calls`，而是把调用**写进了 `delta.content`**：

```text
<tool_call><function=plan_update><parameter=goal>…</parameter></function></tool_call>
```

`apply_openai` 只解析 `delta.content` 与 `delta.tool_calls` 两条通道，于是这段 XML
原样进了 `state.text` → 落库正文 → 用户界面。**模型会写 XML，但它不会自己把它变成调用。**

**根因 E：收尾阶段引擎「不给工具」，却没有告诉模型「这一轮不许调工具」。**

系统提示第 10 条要求「每完成一步就调用一次 `plan_update` 把该步标成 done/failed/skipped」，
而收尾阶段（`Phase::Final`）的 `request.tools` 是**空的**。模型忠实地执行了系统提示，
却无工具可调——于是退化成把调用写进正文。

**根因 F：推理步只带 `args`、不带 `tool` 时，宽容修复层不触发。**

`repair_plan_step` 的规则②只在 `step.tool.is_some()` 时把推理步改判成工具步。
模型写了一个 `kind = reason` 且带 `args` 的步（没有工具名），改判不了，
`validate_plan_v2` 照旧报「第 3 步是 推理 步，不能带 tool / args」——白费一轮模型往返。

### 5.3 修复

| 位置 | 措施 |
| --- | --- |
| `me-proto/src/ai.rs` | 新增**文本形态工具调用的扫描器**：把 `<tool_call>…</tool_call>` 从正文里剥掉，还原成一次真正的 `AiToolCall`；标签可以分片到达，标签里可以夹零宽字符，收尾没收完的标签直接丢掉 |
| `me-sync/src/ai.rs` | **阶段化系统提示**：收尾 / 自检 / 重列阶段各补一句「这一轮该做什么」，收尾阶段明说「不要调用任何工具」 |
| `me-sync/src/ai.rs` | **收尾阶段不再执行工具调用**：模型仍想调工具时不执行（执行了也只会得到一句「这个阶段不需要工具」），改问一次「直接给答复」；只追问一次，再犯就收场 |
| `me-sync/src/ai.rs` | **按阶段校验工具**：这一阶段没下发的工具一律不执行（`plan_submit` 在执行期属于此列——它一旦被执行就绕过了「重列不超过 2 次」的上限） |
| `me-sync/src/ai.rs` | **兜底 `[结果]` 段落**：模型调过工具却没按规范写 `[结果]` 时，引擎按清单与回执合成四行段落（只陈述事实，不替模型下结论） |
| `me-core/src/ai_plan.rs` | `repair_plan_step` 补规则：推理 / 提问步**只带 `args`** 时摘掉参数（没有工具名就没法改判成工具步，但参数挂在推理步上是校验必拒的） |

### 5.4 验证

| 测试 | 断言 |
| --- | --- |
| `me-proto::ai::tests::text_form_tool_call_is_parsed_not_shown` | 事故原文里的标签不进正文，变成一次真正的 `plan_update` 调用 |
| `me-proto::ai::tests::text_form_tool_call_survives_chunked_deltas` | 标签分片到达也认得出来 |
| `me-proto::ai::tests::text_form_tool_call_tolerates_zero_width_chars` | 标签里夹零宽字符也认得出来 |
| `me-proto::ai::tests::unfinished_text_tool_call_is_dropped` | 没收完的标签被丢掉，不落到用户眼前 |
| `me-proto::ai::tests::prose_mentioning_tool_call_survives` | 正文里提到「tool_call」这个词不被误伤 |
| `me-proto::ai::tests::structured_and_text_form_calls_coexist` | 结构化调用与文本形态调用同时出现，两条都认 |
| `me-core::ai_plan::tests::reason_step_carrying_only_args_loses_them` | 推理步只带参数 → 摘掉参数 → 计划一次就过 |
| `me-core::ai_plan::tests::ask_step_carrying_only_args_loses_them` | 提问步同理 |
| `me-sync::ai_flow_mock.rs::final_phase_tool_call_is_not_executed_but_asked_for_the_answer` | 收尾阶段的工具调用**不执行**，引擎追问一次，答复被收下来且带 `[结果]` |
| `me-sync::ai_flow_mock.rs::final_phase_tool_call_is_nagged_only_once` | 只追问一次（每次追问都是一次模型往返） |

全量：**858 passed / 0 failed**；`cargo fmt --check` 与
`cargo clippy --workspace --all-targets -- -D warnings` 全通过。

---

## 6. 留下的经验

1. **「差一点点」的错误最贵。** 它既不成功也不明确失败，模型会反复重试，
   而每次重试都是一次真实请求。宁可替模型猜一个确定正确的答案。

2. **强制约束有代价，要补上信息缺口。** PLAN 阶段不给业务工具是**对的**（那才是「强制清单」），
   但必须同时告诉模型这些工具的参数长什么样，否则它只能猜。

3. **限流要当成瞬时故障。** 429 是「你太快了」，不是「你错了」。
   直接判死会把已经拿到的结果全丢掉——那是比失败更糟的失败。

4. **测试要照着真实事故写。** `repeated_plan_rejections_do_not_stall_the_turn`
   一写出来就抓到了一个真 bug：被熔断拦下的调用跳过了计数逻辑，
   闸门因此从未触发。真实数据比想象更刁钻。

5. **「不给工具」必须配一句「不许调工具」。** 收尾阶段不下发工具是对的，
   但系统提示还在要求「每完成一步调一次 plan_update」——两边打架，
   模型只能把调用写进正文。**约束要成对**：物理上不给，嘴上也要说清。

6. **协议层要对「模型的退化形态」留一条路。** 模型不会保证自己永远填
   `tool_calls`；它把调用写成正文标签时，引擎要么认出来、要么明确拒绝，
   绝不能让那段 XML 原样变成用户看到的答复。

7. **回复格式要由引擎兜底。** 规范说「调用过工具时必须以 `[结果]` 段落收尾」，
   模型不总是照做。引擎按清单与回执合成一段——只陈述事实，不替模型下结论。
