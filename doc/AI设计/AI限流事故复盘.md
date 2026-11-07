# AI 对话：限流事故复盘与修复（2026-11-07）

> 起因：同一条提问「看看我的邮件 生成我的用户画像」连跑两次，**两次都以「请求过于频繁」失败**。
> 而两次失败之前，模型**都已经查到了 20 封邮件**——结果全丢了。

本文记录：现象 → 根因 → 修复 → 验证。

---

## 0. 一句话结论

不是「工具调用次数上限太低」，而是**两次真实的 HTTP 429**。

`MAX_TOOL_ROUNDS = 24` / `MAX_TOOL_CALLS = 24` 一次都没碰到（实测两轮分别只用了 10 次和 6 次工具调用）。
真正杀死对话的是：模型在 `plan_submit` 上反复写错参数 → 每次被拒都产生一次新的模型请求 →
请求密度顶到服务商限流线 → 后续请求全被 429 打掉 → 整轮失败。

**一次「差一点就对了」的计划参数错误，代价是整轮对话。**

---

## 1. 现象

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

## 5. 留下的经验

1. **「差一点点」的错误最贵。** 它既不成功也不明确失败，模型会反复重试，
   而每次重试都是一次真实请求。宁可替模型猜一个确定正确的答案。

2. **强制约束有代价，要补上信息缺口。** PLAN 阶段不给业务工具是**对的**（那才是「强制清单」），
   但必须同时告诉模型这些工具的参数长什么样，否则它只能猜。

3. **限流要当成瞬时故障。** 429 是「你太快了」，不是「你错了」。
   直接判死会把已经拿到的结果全丢掉——那是比失败更糟的失败。

4. **测试要照着真实事故写。** `repeated_plan_rejections_do_not_stall_the_turn`
   一写出来就抓到了一个真 bug：被熔断拦下的调用跳过了计数逻辑，
   闸门因此从未触发。真实数据比想象更刁钻。
