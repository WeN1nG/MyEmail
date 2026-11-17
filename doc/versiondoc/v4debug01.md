# v0.4 调试修正 01：每日简报幻觉与发件人检索失效

> **性质**：一次性调试记录，**不是长期事实源**。落地后把结论回填 [版本跟踪.md](版本跟踪.md)，
> 本文只保留"当时怎么发现的、为什么这么改"的推理链。
> **版本口径**：`0.4.x` 增量｜schema `8`（D4 落地后为 `9`）
> **现场**：2026-11-17 上午，每日简报会话（本机 `ai_conversations` #11，模型 `agnes-2.5-flash`）
> **触发问句**：「给我 wangtao 的这两个邮件」→「我要简报中……这两封邮件」→ AI 答"本地缓存里没有"
> **修正口径**：`甲 + 甲-a + A`（2026-11-17 逐条确认，见 §9 决策记录）——模型在简报里显式写 `[id=…]` 锚点，
> 引擎据此对账；归并行也要逐个带 id；UI 侧只摘锚点、不渲染行内卡片。
>
> 状态标记：`☑ 已修` / `☐ 待修` / `⊘ 不做`

---

## 0. 结论先行

| 问题 | 结论 |
| --- | --- |
| 简报表里的两封「Wang Tao」实习邮件是否真实存在 | **不存在，是模型幻觉**。证据与可复现 SQL 见 §1.2 |
| 「给不出来」是检索工具的设计失误吗 | **是，但不是一处**：`from` 只查地址列（A）、索引缺显示名（B）、回执不给 `message_id`（C）、0 命中不给候选（D），以及**姓名写法差异**（`wangtao` 匹配不到 `Wang Tao`，D9）。**C 才是"索取却给不出来"的直接原因**，D9 是"按人搜根本搜不到"的那一半 |

两个问题在链路上是**同一根因的两端**：检索回执既不携带可指认的 id（C）、也搜不到发件人姓名（A/B），
模型只能凭主题"补全剧情"；而收尾阶段没有任何一处核对"它说的话在不在回执里"，于是补出来的东西直接落库并展示。

**修正的总思路**：给每封邮件一个**可机检的锚点**（`[id=143]`），让"这句话说的是哪封信"变成可验证的事实，
而不是靠主题字符串去猜。锚点由模型在简报里写出、由引擎对本轮回执校验、由渲染层摘掉——三处各管一段。

---

## 1. 现场与证据

### 1.1 复现路径（本机库实测）

```text
会话 #11「每日简报 · 11月17日」
  10:22:40  assistant  id=18  complete   ← 简报正文（含两封 Wang Tao）
  11:13:48  user       id=19  给我wangtao的这两个邮件
  11:13:48  assistant  id=20  failed    请求过于频繁，请稍后手动重试
  11:25:27  user       id=21  我要简报中的 {…这两封邮件}
  11:25:27  assistant  id=22  complete  ← 抱歉，找不到
```

`id=18` 的 `tool_trace` 里，`email_search` 只有**一次**成功调用：
`{unread_only:true, days:7, limit:5}` → 卡片 `message` = `150 / 149 / 147 / 145 / 143`。

> `id=20` 的 `failed` 是**模型限流**（"请求过于频繁"），与检索无关；但同一轮里 7 次 `email_search`
> 已有 2 次因参数为空报错——这正是"手上没有 id、只能换个说法重试"的现场。

### 1.2 事实核对

| 核对项 | 实测结果 |
| --- | --- |
| 库规模 | 2 个账户、**155 封**邮件（INBOX 127 = 账户1 的 120 + 账户2 的 7；Sent 17；Junk 11） |
| 发件人里有没有 Wang Tao | `from_name` / `from_addr` 双列搜索 `wang` / `tao` / `wangtao` 全部 **0 行**；唯一含 `wang` 的是 `notice.weicewang.com`（中国移动笔试通知，与实习无关） |
| 那 5 张卡片实际是什么 | 150 = 淘米网络 2027 校招（`services@51job.com`）、149 / 147 = ChatGPT 产品推送、145 = Ollama 更新、143 = Microsoft 新登录提醒 |
| 简报那几行是怎么错位的 | 回执是 5 封，简报只写了 **4 行**：微软（真）+ 「另有 2 封通知类」（Ollama、ChatGPT，真）+ **两条 Wang Tao（假）**。即**淘米那封校招被"改写"成两封不存在的实习邮件**，而"另有 2 封"覆盖的实际是 3 封（Ollama + ChatGPT ×2） |
| 那个 Google Forms 链接 | `...1FAIpQLSdZ6eDAJH9UwMk3Qg8VhF9bBqXzY6KjLmNpQrStUvWxYz` 尾部是字母表顺序，纯编造；而且 `format_message` 从不输出链接字段，模型手上**从来没有过任何 URL** |
| 模型后续自查 | `from:"Wang Tao"` / `from:"wangtao"` / `from:"tao.wang"` / `query:"Wang Tao"` / `query:"Tao Wang"` 全 0 命中；`query:"Internship"` 只命中 60 / 108（10 月的旧面试题），也不是它说的那两封 |

> 可复现的核对 SQL（换成自己的库即可，行数为本机口径）：
>
> ```sql
> -- 发件人是否真的存在（两列都要查，见 §3 D2）
> SELECT COUNT(*) FROM messages WHERE from_addr LIKE '%wangtao%';   -- 0
> SELECT COUNT(*) FROM messages WHERE from_name LIKE '%wangtao%';   -- 0
> -- 那 5 张卡片是谁
> SELECT id, from_name, from_addr, subject FROM messages WHERE id IN (150,149,147,145,143);
> -- 显示名根本没进地址列（这行是本轮的关键证据）
> SELECT COUNT(*) FROM messages WHERE from_addr LIKE '%<%';        -- 0
> ```

### 1.3 为什么"编得出来"

不是模型单纯跑偏，是三个条件同时成立：

1. **回执的信息量低于提示词的要求。** `DAILY_BRIEFING_PROMPT` 要求"每封一行「发件人 · 主题 · 时间」+ 一句要点"，
   但 150 那封的 `snippet` 是 `[logo] [退订][1] [投诉][2]` 这类 HTML 残留（`make_snippet` 从纯文本正文首段切出，
   见 `me-sync/src/indexer.rs:383`），
   模型手上只有一个"淘米网络 2027 校招启动"的标题，要写满"实习申请 / 面试感谢信"的细节就只能补。
2. **回执里没有可指认的实体。** 见 §3 D1：`format_message` 不输出 `message_id`，模型无从"指认"，
   也就无从在下一轮把同一封信捞回来——它只能重新描述一遍（描述就会漂移）。
3. **没有任何落地校验。** 提示词里写了"绝对不要编造"，但收尾只有两重兜底：
   正文为空时拼回执（`ai.rs:2905-2925`）、缺 `[结果]` 时补一段（`ai.rs:2935-2938`），
   **都不检查"正文里说的邮件在不在本轮回执里"**。Reflect 阶段的判据是"这一步做完没有"，不是"你说的是不是真的"。

---

## 2. 检索侧的真实缺陷（回答第二个问题）

「无法根据发件人查询」不是一个 bug，是**四层缺陷 + 一层写法差异**（后者见下方补注，对应 D9）。按"是否直接导致索取失败"排序：

| # | 缺陷 | 证据 | 影响 |
| --- | --- | --- | --- |
| **C** | 检索回执**不给 `message_id`**，而 `mail_read` 的唯一入参就是 `message_id` | `format_message`（`ai_tools.rs:2642-2678`）输出 `序号 · [日期] 主题 — 发件人`，无 id；`mail_read` 描述却写"用检索结果里的 message_id 指定"（`ai.rs:2123`） | **`mail_read` 事实上不可用**（除非用户在提问里引用了邮件）。模型只能反复搜：两轮追问实测 **13 次** `email_search`（9 次拿到结果、2 次参数为空报错、2 次因收尾被跳过） |
| **A** | `from` 只 `LIKE from_addr`，**不查 `from_name`** | `search.rs:155-165` 的 `base_filters`；实测 `from_addr` 含 `<` 的行数为 **0**，显示名存在 `from_name` 列 | `from:"<中文姓名>"` 恒 0 命中（本机有 17 封的 `from_name` 含同一中文显示名，一封都查不到） |
| **B** | 全文索引里**没有 `from_name` 列**，且两字中文词搜不到正文 | `messages_fts(subject, body, from_addr, to_addrs)`；trigram 要求 ≥3 字符，`<3` 时走 LIKE 且只查这三列（`search.rs:246-268`） | `query:"王涛"` 这类两字人名恒 0；正文含"实习"13 封、含"面试"34 封，但两字查询都捞不到 |
| **D** | 0 命中时**不给候选**，只说"换关键词" | `ai_tools.rs:1005-1013`、`ai_tools.rs:1201-1230` | 模型不知道库里有哪些发件人，只能瞎试不同拼写；用户也看不出"到底是没这封信还是名字不对" |

> 这张表起初漏了一层，2026-11-17 11:59 由用户指出并补上：**A/B 是「字段缺失」，D9 是「写法差异」**。
> `from:"wangtao"` 即使在 A 修好之后照样 0 命中——`LIKE '%wangtao%'` 匹配不到 `Wang Tao`，
> 而「去掉空格写」正是模型和用户最自然的输入。两层必须一起补，只补一层等于没修。

**A 的认知来源值得单独记一笔**：`ai_tools.rs:1329-1330` 的注释写着
"`from_addr` 里存的是 `Name <addr>`，姓名本身也能命中"——**这与存储事实相反**。
`me-store` 把显示名和地址拆成两列，`Address::display()` 才是 `Name <addr>` 的拼装方。
代码是照着这句错注释写的，所以修的时候必须连注释一起改，否则下一个改的人还会踩。

**为什么引擎的"放宽"救不了它**：0 命中时引擎会自动放宽条件重试（`ai.rs:2329`，
本机那一轮就是 7 天 → 30 天 → 90 天）。但"发件人不存在"不是"条件太窄"，
放宽只会让模型在更大的范围里更确定地确认"没有"，然后把"没有"解释成"还没同步"。

---

## 3. 修正项清单

> 优先级：`P0` 本轮必修（不修则同类幻觉会复发）｜`P1` 体验补齐｜`P2` 可延后（`D4` 是唯一需要迁库的一项，单独提交）

| # | 项 | 修法 | 文件:符号 | 迁库 | 优先级 | 状态 |
| --- | --- | --- | --- | --- | --- | --- |
| D1 | 回执携带 `message_id` | `format_message` 每封**第一行行尾**加 `[id=143]`（与 `[结果]` 同款方括号；`format_series` 的 `event_id={}` 是同类先例）；`mail_read` 描述补一句"id 形如 `[id=143]`"。这条同时是 D6 的锚点来源，**不能单独摘出去先上** | `me-sync/src/ai_tools.rs:2642-2678`、`me-core/src/ai.rs:2122-2124` | 否 | P0 | ☑ |
| D2 | `from` 同时匹配地址与显示名 | `base_filters` 的 from 条件改为 `(m.from_addr LIKE ? ESCAPE '\' OR m.from_name LIKE ? ESCAPE '\')`；`card_reason` 的判定用 `Address::display()`（已含显示名），**只需确认口径、不必改**；**删掉 `ai_tools.rs:1329-1330` 的错误注释**并改写为真实存储说明。UI 搜索的 `from:` 语法走同一个 `search()`（`me-core/src/search_syntax.rs:178`，不做邮箱格式校验），这条会一起生效，回归要带 UI 搜索 | `me-store/src/search.rs:155-165`、`me-sync/src/ai_tools.rs:1329-1333` | 否 | P0 | ☑ |
| D3 | 姓名→邮箱的解析数据源太窄 | `resolve_from_filter` 现在只查 `ai_entities`（本机 5 条），而 `contacts` 有 17 条真实往来。改为：含 `@` 直接放行 → 查 `contacts`（`list_contacts`）→ 查 `ai_entities` → 都没有则原样交给 D2 的子串匹配 | `me-sync/src/ai_tools.rs:2116-2129`、`me-core/src/port.rs:687-730` | 否 | P1 | ☐ |
| D4 | 全文索引补 `from_name`；两字中文词能搜正文 | ① schema 9：重建 `messages_fts` 五列（+ `from_name`），按 `rowid = messages.id` 从旧表 **LEFT JOIN** `messages` 搬数据（`COALESCE(m.from_name,'')`，孤儿行不丢；**不需要重新索引正文**）；② `<3` 字符时先走原 LIKE，若 0 命中再用 `EXISTS(SELECT 1 FROM messages_fts f WHERE f.rowid=m.id AND f.body LIKE ?)` 做一次正文回退 | `me-store/src/migrations.rs`、`me-store/src/search.rs:120-180`、`me-core/src/lib.rs:SCHEMA_VERSION`、`doc/数据结构.md` §4.2 | **是** | P2 | ☐ |
| D5 | 0 命中给候选发件人 | 两处空回执补一段"本地缓存里相近的发件人：`显示名 <地址>`（各 N 封）"，取自按 `from_name/from_addr LIKE` 聚合的 top 5；把模型从"换拼写重试"推向"跟用户确认身份"。与 D9③ 是同一段代码，实现时合并。**实现陷阱**：summary 必须保留「未找到」/「0 命中」字样——`is_empty_result`（`ai_tools.rs:395`）靠它判定"成功但空"，改掉会让这一步被当成正常完成，引擎不再放宽条件也不再标跳过；候选只能写进正文 | `me-sync/src/ai_tools.rs:1005-1013`、`me-sync/src/ai_tools.rs:1201-1230` | 否 | P1 | ☑ |
| D6 | 收尾做引用对账（防幻觉落地） | 新增"兜底之三"，插在现有两重兜底之后、落库之前。口径：**命中则信、未命中只标不删**。① 白名单 = 本轮邮件回执里 cards 的 id；② **全篇**扫 `[id=N]`：N 在白名单 → 一个字不改，N 不在 → **就地换成 `（未核实）`**（行尾追加会破坏 Markdown 表格）；③ 「另有 N 封」的 N 必须等于该行列出的 id 个数；④ **仅「未读邮件」段**给完全没写 id 的邮件行补 `（未核实）`；⑤ 整段一条都对不上（自定义提示词没要求 id 时必然如此）→ **不逐行标**，只在段尾加一句「本段未能与本轮检索回执对齐，内容未核实」；⑥ `cited / unknown / unmarked / mismatched` 打 `tracing::info!`。**实现细节**：⒜「邮件行」= `-`/`*`/`•`/`·`/`>`/`|`/`1.` 开头（漏了 `>` 就会放过 `> 另有 3 封通知类` 那行，见 §11）；⒝ 解析 `[id=N]` 要容忍 `[id = 143]` 这类空格变体。**触发面已于 2026-11-17 修订**：不再看「是不是简报」，改看「本轮有没有邮件回执」（§11） | `me-sync/src/ai.rs`（收尾兜底区）；纯文本逻辑在 `me-core/src/ai.rs:align_email_citations` | 否 | P0 | ☑ |
| D7 | 简报提示词收口 | 在 `DAILY_BRIEFING_PROMPT` 补四条硬约束：① 每条邮件带 `[id=…]`，编号**照抄**回执、不许自己从 1 编号；② 发件人照抄回执原文（显示名与地址），不得翻译/改写/换成一个回执里没有的人；③ 回执没有链接字段，就不要写"链接"或任何 URL；④ `另有 N 封通知类` 的 **N 必须等于该行列出的 id 个数**（不是"回执条数减已列条数"——只约束数会得到"数对、身份错"） | `me-core/src/ai.rs:1597-1619` | 否 | P0 | ☑ |
| D8 | UI：`[id=…]` 不裸露给用户 | **摘掉，不渲染卡片**：`strip_mail_ids` 在流式分支与收尾分支各过一遍（流式那条是 `views/ai.rs:1054-1056` 的纯文本绘制）；**只改展示，落库正文保留原文**——下一轮用户说"把那封打开"时，历史里的 id 是模型直接 `mail_read` 的唯一锚点。**暴露面已核实**：工具轨迹面板只画 `summary` + 卡片（`views/ai.rs:937-971`），不渲染回执正文，所以摘 `show_answer` 一处就够 | `me-app/src/views/ai.rs`（`show_answer`，含 `split_result_block` 前置切分先例） | 否 | P1 | ☑ |
| D9 | **发件人检索能力补强**（2026-11-17 11:59 用户指出的遗漏项） | D2 只解决"字段没查"，这条解决"写法不同"。① **归一化匹配**：比较前 `replace(lower(col),' ','')` 再去掉 `.`/`-`，使 `wangtao` ↔ `Wang Tao`、`tao.wang` ↔ `Tao Wang` 互相命中（纯 SQL 内置函数，**不迁库**）；② **多词 AND**：`from` 按空白/逗号/顿号拆词，每个词都得出现在 `from_name` 或 `from_addr` 里，解决姓前名后；③ **0 命中给候选**：把 D5 并进来，用归一化 token 在 distinct 发件人里找 top 5。附带约束：`resolve_from_filter` 只在实体表**精确命中**时才把姓名换成邮箱，否则会把用户原话改写成错地址，把①②两条路一起堵死 | `me-store/src/search.rs:124-180`（条件构造）、`me-sync/src/ai_tools.rs:2116-2129`（少改写、多匹配）、`me-sync/src/ai_tools.rs:1005-1013`（空回执候选） | 否 | P0 | ☑ |

### 3.1 为什么锚点写进正文、又由 UI 摘掉

这是本次讨论的核心取舍，也是 D1/D6/D8 三条串起来的原因：

- **id 是唯一的地面真值。** 模型手上有 id 就可以自由改写主题做摘要（「淘米网络 2027 校招启动」缩成「淘米网络校招」是想要的能力），而 id 仍钉死它引用的是 150 这一封。改成"引擎拿发件人+主题去匹配"就必须二选一：禁止改写（摘要变差）或容忍改写（真幻觉混过去）。
- **但不能让用户看到 `[id=150]`。** 所以渲染层统一摘掉：任何对话、任何时刻都不露锚点，不用为简报和闲聊写两套规则。
- **落库必须是原文。** 摘掉之后，"把那封打开"就永远只能靠重新检索；留着 id，模型下一轮可以直接 `mail_read`——这恰好补上了本次事故里"追问拿不到"的那条路。

顺带说清 D1 的另一个收益：`mail_read` 是整个 AI 邮件能力里唯一的"读正文"入口，而它现在**没有任何可达路径**——
搜索回执不给 id，UI 引用只覆盖用户主动贴的那一封。所以"帮我看看这封邮件说了什么"这类请求，
模型永远只能回答"标题 + 片段"。D1 修的就是这个，它是九条修正项里改动最小、收益最大的一条。

### 3.2 为什么「没写 id 的行」只在简报里补标（**触发面已修订，见 §11**）

自然语言回复里"发件人 · 主题"没有稳定结构，硬做对账必然误伤（模型合理改写标题就会被判为编造）。
简报是**产品自己发起的、格式固定的四段**，才有可机检的形状；其余对话不标不改。

但**「查什么」与「什么时候查」是两件事**，这里一度被混在一起：最初连触发条件也绑在
「是不是简报」上，结果用户把简报提示词粘进输入框就绕过了整道校验（§11）。现在的分工是——

| 检查项 | 范围 | 为什么 |
| --- | --- | --- |
| 假锚点（`[id=999]` 不在回执里） | **全篇**，只要本轮有邮件回执 | id 是硬事实，不存在误伤 |
| 「另有 N 封」数错 | **全篇** | N 与回执可直接比对 |
| 邮件行没写 id | **仅「未读邮件」段** | 只有这种固定结构才能可靠认出「这是一封邮件」 |

**自定义提示词是唯一漏口**：用户把简报提示词改成不要求 id 的写法时，模型不会写锚点，
此时"未读邮件"段会整段对不上——所以第⑤条才是"整段零命中只在段尾加一句"，而不是逐行标，
否则用户会看到满屏「未核实」却不知道为什么。

---

## 4. 明确不做

| 不做 | 理由 |
| --- | --- |
| 给模型下发"整库发件人名单" | 200 个发件人 × 邮箱会把每轮输入预算吃掉一大块，且泄露面变大；D9③ 的"按相似度取 top 5"够用 |
| 在 `email_search` 里做 NLP 姓名归一（"Wang Tao" ↔ "王涛"） | 归一是模型该做的事（`resolve` + 子串两条路已覆盖），引擎做拼音/繁简转换属越界，且难维护 |
| 用 FTS 的 `unicode61` 换掉 `trigram` | trigram 是中文子串检索的现状基础，换分词器等于重写检索层；D4 的"0 命中再回退"是成本最低的补法 |
| 把幻觉判定做成模型自评 | 已试过形态相同的 Reflect 阶段——它判"做完没有"尚可，判"说得对不对"不可靠；对账必须落到**可机检的 id 集合**上 |
| 把 `[id=…]` 渲染成行内邮件卡片 | 卡片已经按 `call_id` 持久化在 `cards_by_call` 里（`views/ai.rs:180-182`），历史回看由 `show_trace_cards`（`views/ai.rs:995`）重画；正文里再插一张是重复入口，还要把 `show_answer` 重构成分段布局 |
| 对账失败就删行 | 删除会把"模型合理改写"和"模型编造"一起杀掉，用户还看不出少了什么；改标「（未核实）」，既不撒谎也不隐藏 |

---

## 5. 验收与回归

### 5.1 新增测试

| 用例 | 所在 | 断言 |
| --- | --- | --- |
| `email_search_result_carries_message_ids` | `me-sync/tests/ai_flow_mock.rs` | 回执文本里每封都出现 `[id=`，且 id 属于本次 cards |
| `search_from_matches_display_name` | `me-store/tests/store_schema.rs` | `from_name="张三"` / `from_addr="a@b.com"` 的邮件，`SearchQuery.from="张三"` 命中；`from="a@b.com"` 仍然命中（回归） |
| `search_from_normalized_ignores_spaces_and_punctuation` | `me-store/tests/store_schema.rs` | `from="wangtao"` 命中 `Wang Tao`；`from="tao.wang"` 命中 `Tao Wang`；`from="WANG TAO"`（大小写）也命中（D9①） |
| `search_from_multiple_tokens_is_and` | `me-store/tests/store_schema.rs` | `from="Wang Tao"` 命中姓名为 `Wang Tao` 的邮件，**不**命中只姓 `Wang` 的另一人；`from="Tao Wang"` 同样命中（D9②） |
| `search_two_char_cjk_hits_body` | `me-store/tests/store_schema.rs` | `query="实习"` 能命中正文含该词的邮件（D4 落地后） |
| `migration_v8_v9_keeps_fts_rows_and_adds_from_name` | `me-store/tests/migration_v8_v9.rs` | 迁移后**非孤儿** FTS 行数不变（用 LEFT JOIN 搬数据，孤儿行也要留下）、`from_name` 可命中、`body` 内容未丢失 |
| `briefing_tolerates_spaced_ids` | `me-core/src/ai.rs`（纯函数单测） | 正文写 `[id = 143]`（带空格）仍被识别为有效锚点，不误标「未核实」 |
| `briefing_marks_uncited_mail_lines` | `me-sync/tests/ai_flow_mock.rs` | 正文里一封"回执里没有的邮件" → 该行标 `（未核实）`、**行还在**、假 id 被删；同段另一封真实邮件不受影响 |
| `briefing_warns_once_when_no_ids_at_all` | `me-sync/tests/ai_flow_mock.rs` | 整段没有一个 id（自定义提示词场景）→ 不逐行标，只在段尾出现一句"未能与本轮检索回执对齐" |
| `briefing_alignment_is_noop_without_mail_search` | `me-core/src/ai.rs`（纯函数单测） | 本轮没有 `email_search` 回执 → 正文原样返回，不产生任何标记 |
| `answer_hides_mail_ids` → 实际落成 `strip_mail_ids_removes_anchor_but_keeps_verify_mark` | `me-core/src/ai.rs`（纯函数单测；UI 只是调用它） | 摘掉 `[id=143]`，保留 `（未核实）` 与其余文字；`[2026-11-16]` 这类方括号不动 |
| `empty_result_lists_candidate_senders` | `me-sync/tests/ai_flow_mock.rs` | 0 命中回执含候选发件人，且不含隐私字段（正文/附件名） |

### 5.2 回归口径

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

### 5.3 手工复核（修完必做一次）

用本机真实库重放现场：打开应用 → 今日简报 → 追问"给我简报里那两封 Wang Tao 的邮件"。
期望：AI 在第 1 次 `email_search` 后直接回答"库里没有这个发件人，相近的是 X / Y"，**不再编造**，
且 `tool_trace` 里的 `email_search` 调用次数 ≤ 2。

同时看三件落地效果：

1. 简报的「未读邮件」段每条都带 `[id=…]`（看 `ai_messages.content` 原文），且 `cited` 计数等于段内 id 总数、
   `unknown = 0`（归并行带多个 id，别按"行数"对）；
2. **界面上一个 `[id=` 都看不到**（流式期间和收尾后都要看一遍），`（未核实）` 该出现时出现；
3. 追问"把那封打开"能直接读到正文——这是 D1 留下的 id 在起作用，不是重新检索碰运气。

---

## 6. 文档同步

| 文件 | 改什么 |
| --- | --- |
| [版本跟踪.md](版本跟踪.md) | **已写**：新增 §17，含缺陷定位 / 新增能力 / 行为变更 / 新增测试 / 未落地四张表 |
| [../数据结构.md](../数据结构.md) §4.2 | D4 落地后更新 `messages_fts` DDL（五列）——本轮未做 |
| `CLAUDE.md` / `AGENTS.md` | **已写**：补 `SearchRepo::senders`、发件人三层匹配、`align_briefing_citations` / `strip_mail_ids`（两份同步，仅第 3 行 Codex/Claude Code 不同） |
| 本文 | 每项落地后把状态列改为 `☑`，并补一行实测口径（本机复现结果） |

---

## 7. 风险与回退

| 项 | 风险 | 回退 |
| --- | --- | --- |
| D1 | 模型可能把 `[id=143]` 当正文复述给用户 | 一线证据：本机 15 条助手消息里模型从未复述过 `event_id=`；即便复述，D8 也会摘掉。**回退必须三处同时退**（D1 的 `format!` + D7 的提示词要求 + D6 的校验）——只退 D1 会让 D6 每段都走"零命中"分支，简报被加一句莫名的"未能对齐" |
| D2 | `from_name` 上的 `LIKE '%x%'` 是前缀通配，走全表扫 | 与改动前的 `from_addr` 同量级（155 封 0.1ms 级）；超过 5 万封时再补 `from_name` 上的普通索引 + 前缀匹配 |
| D3 | `list_contacts` 是**前缀**匹配，与 `from` 的子串语义不一致 | 保持"前缀查不到就原样返回"，由 D2 的子串匹配兜底；不做两套语义混用 |
| D4 | 重建 FTS 表若中途失败会丢索引 | 沿用 `migrate_v7_to_v8` 的事务模板（每个 `migrate_vX_to_vY` 自带 `conn.transaction()` + `check_foreign_keys`），失败即回滚，旧表仍在；另需**新增** `myemail.db.bak-v8` 备份——现有 `backup_v1` 只在 v1 路径上调用，v8→v9 得自己加 |
| D4 | 两字回退的 `body LIKE` 可能拖慢查询 | 只在"前三列 0 命中"时触发，且限制扫描集合（同 `days`/`folder` 过滤后的行）；实测 >200ms 就退回不做 |
| D6 | 标记错位（把好行标成未核实） | ① 「没写 id」的补标只在「未读邮件」段做（假锚点与归并数错才全篇查）；② 找不到「未读邮件」段就不补标；③ 本轮没查过邮件就整段不动；④ 整段零命中时退化为"段尾一句话"，不逐行标。四条都是 fail-open，宁可少标不多标 |
| D6 | 提示词与自定义提示词打架 | 用户自定义简报提示词里通常不含 id 要求 → 走"段尾一句话"分支；`cited` 计数会掉到 0，日志里能直接看出来 |
| D8 | 摘掉标记后正文出现"双空格"或悬空标点 | `strip_mail_ids` 只删锚点本身，不动两侧空白；单测里带此断言，视觉问题在复核时一眼可见 |
| D9 | 归一化把匹配放得太宽（`from:"a"` 命中一片） | 单 token 长度 < 2 时只走原样子串、不进归一化分支；多词时每个 token 同样要求 ≥ 2 字符 |
| D9 | `replace()` 包住列名后无法走索引 | 现在本来就是 `LIKE '%x%'` 全表扫，量级不变（155 封 0.1ms 级）；真到 5 万封以上再考虑落一列归一化的 `from_key` 配普通索引，和 D4 一起做 |

---

## 8. 执行顺序

```text
D1 → D2 → D9（检索这一层必须连着做：D2 补字段、D9 补写法，拆开任一条都还是搜不到）
  → D7（提示词收口）
  → D8（必须紧跟着 D1：锚点一进正文，界面上就会看到 [id=…]）
  → D6（依赖 D1 的锚点：没有 id 就没有可机检的集合；也依赖 D8 决定"标了给谁看"）
  → D5（不单独排，随 D9③ 一起上）
  → D3（数据源收窄，注意别破坏 D9 的"少改写"约束）
  → D4（唯一迁库项，单独提交：schema 9 + 迁移测试 + 数据结构.md）
  → 全量回归 → 手工复核 §5.3 → 回填 版本跟踪.md
```

> 三条硬依赖：① **D2 与 D9 同批**——只补字段不补写法，用户写 `wangtao` 依旧 0 命中；
> ② D1 必须在 D6 之前——D6 对账靠 id 集合，没有锚点就只能靠主题字符串相似度，误判率不可接受；
> ③ D8 必须紧跟 D1——锚点进正文后如果不摘，用户在流式和收尾都会看到 `[id=150]`，属于**修一个坑挖一个坑**。

---

## 9. 决策记录（2026-11-17 逐条确认）

| 分支 | 结论 | 为什么这么定 |
| --- | --- | --- |
| 锚点由谁写 | **甲：模型在简报里显式写 `[id=…]`**，引擎对"没写 id 的邮件行"只标不删 | id 是唯一地面真值，允许模型自由改写主题做摘要；靠"发件人+主题"匹配则必须在"禁止改写"和"容忍改写"之间二选一 |
| 归并行怎么办 | **甲-a：归并行也逐个带 id，N = 该行 id 个数** | 这次事故里"另有 2 封（Ollama、ChatGPT）"实为 3 封，两封假邮件正好占了正列名额——归并行不能当例外区 |
| 标记怎么显示 | **A：渲染层统一摘掉，不渲染行内卡片** | 卡片已按 `call_id` 持久化并在 `show_trace_cards` 重画，正文再插一张是重复入口；摘掉只需一次字符过滤 |
| 对账失败怎么处置 | **标 `（未核实）`，不删行**；整段零命中只加一句段尾提示 | 删除会连带杀掉合理改写，用户还看不出少了什么；标记既不撒谎也不隐藏 |
| 落库正文要不要动 | **不动**，只改展示 | 历史里的 id 是下一轮 `mail_read` 的唯一锚点，抹掉就把"把那封打开"这条能力一起抹了 |
| 发件人检索补到哪一层 | **D9：归一化 + 多词 AND + 0 命中给候选**（2026-11-17 11:59 用户补的第 5 个分支） | 只修 A/B（字段缺失）仍搜不到 `wangtao`；比对前去掉空格与标点、拆词取 AND，是唯一不迁库又能覆盖真实写法的做法 |

**两个待定分支已在实施时定掉**：

- **B3 `from` 参数语义**：**保持单参数**，内部按「地址 OR 显示名 OR 归一化形态」匹配（D2 + D9 已实现）。
  不新增 `from_name` 参数——"发件人"对人只有一个概念，加参数只会让模型在两者之间犹豫。
- **B4 D4 是否本轮做**：**推迟**。D2 + D9 落地后"按姓名搜"已经可用；D4 只增量覆盖
  "`query` 全文命中发件人名"和"两字中文搜正文"，不值一次 schema 迁移的风险，留给下一轮单独提交。

---

## 10. 实施记录（2026-11-17 12:10）

**已落地 7 项**：D1 / D2 / D5 / D6 / D7 / D8 / D9；**未做 2 项**：D3（P1，姓名→邮箱的数据源）、
D4（P2，需迁库）。D5 按计划并入 D9③，不再单列。

| 改动 | 文件:符号 | 要点 |
| --- | --- | --- |
| 回执带锚点 | `me-sync/src/ai_tools.rs:format_message` | 第一行行尾 `[id=143]`；`mail_read` 终于有可达路径 |
| 发件人三层匹配 | `me-store/src/search.rs:base_filters` | `from_addr OR from_name` + 归一化 `replace(lower(...))` + 多 token AND；单字符 token 不进归一化分支 |
| 拆分与归一化规则 | `me-core/src/model.rs:split_sender_tokens` / `normalize_sender_key` | 带 `@` 整串一个 token（否则 `…@example.com` 会被拆出 `com` 命中全库）；不带 `@` 时点与连字符也拆，`tao.wang` 才能命中 `Tao Wang`；上限 4 个 token |
| 0 命中给候选 | `me-sync/src/ai_tools.rs:empty_result_hint` / `similar_senders` / `pick_similar_senders` | 新增 `SearchRepo::senders`（`me-core/src/port.rs`）+ `me-store/src/search.rs:senders`；summary 仍写「未找到（0 命中）」，`is_empty_result` 不受影响 |
| 引用对账 | `me-core/src/ai.rs:align_email_citations` | 纯函数，两遍扫描：全篇查假锚点与「另有 N 封」数错，仅「未读邮件」段补标没写 id 的行 |
| 提示词收口 | `me-core/src/ai.rs:DAILY_BRIEFING_PROMPT` | 四条约束（照抄 id / 照抄发件人 / 不编链接 / 归并行 N = id 个数） |
| 渲染摘锚点 | `me-app/src/views/ai.rs:show_answer` | 流式与收尾共用 `me_core::ai::strip_mail_ids`；**落库正文不动**，"复制"也走摘过的文本 |
| 对账接线 | `me-sync/src/ai.rs` 收尾兜底区 | 判据是**本轮有没有邮件回执**（`trace.tool.is_email() && !cards.is_empty()`）；`cited/unknown/unmarked/mismatched` 打 `tracing::info!` |

**实施时新定的三件事**（原文档没写死）：

1. **token 上限 4**、单 token ≥ 2 字符才归一化——否则 `from:"a"` 会捞出半个邮箱库。
2. **不做引号精确通道**：`from:` 变宽是收益不是回归；等真有人需要"精确"再加，不预先造语法。
3. **执行顺序改为 D2 + D9 → D1 → D7 → D8 → D6**：先把"按人搜得到"修好，再修"搜到之后读得到"。

**门禁**：`cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings` 通过；
`cargo test --workspace` **976 passed / 7 ignored**（本轮新增 20 个用例；其中 6 个是 §11 补的）。

**上线后要盯的两件事**：

- `邮件引用对账` 这行日志的 `cited / unknown / unmarked / mismatched`——`unknown > 0` 说明模型仍在编 id；
  `cited = 0` 且简报有邮件行，说明提示词没被遵守（自定义提示词时属预期）。
- 按姓名检索的**误命中**：多 token AND 会放宽范围（`from:"Wang"` 仍命中所有姓王的），
  这是有意的取舍；真出现"搜一个人出来一堆"再考虑加"必须全字匹配"的开关。


---

## 11. 第二次验收：对账一次都没跑（2026-11-17 14:00 发现）

### 11.1 现场

新 exe（13:56 构建）跑了两轮对话，`[id=…]` 锚点按预期出现，但 **`tracing` 日志里一条「简报引用对账」都没有**。
查下来不是"跑了没改"，是**函数体一次都没进**：

| 轮次 | 消息 | 为什么没进 |
| --- | --- | --- |
| 自动简报 10:22 | msg 18 | `daily_briefing=true`，但跑的是**旧二进制**（新 exe 13:56 才出），代码不在进程里 |
| 粘贴简报 13:59 | msg 26 | 上一句 msg 25 是一条 **user 消息**（提示词被粘进输入框）→ `insert_user=true` → 标志 `false` |

两轮之间没有第三次机会：`meta.ai_daily_briefing = 2026-11-17` 已写，重启也不会再自动发。

### 11.2 两个设计错误

**错误一：判据选错了对象。** 原条件是 `daily_briefing: !insert_user && !replay_only`——
判的是**"这条消息由谁发起"**，而该判的是**"这一轮有没有邮件回执"**。
用户手粘提示词、点重新生成、复制一份简报，全都合法绕开。

**错误二：`>` 行不在对账范围内。** 原 `is_bullet_line` 只认 `-`/`*`/`•`/`·`/`1.`，
而第二份简报把归并句写成了 blockquote：

```text
> 另有 **3 封通知类**（均为 Junk 文件夹中的推广/产品更新邮件）。
```

**这行是那一轮唯一的编造**：`email_search(unread_only=true, days=7, limit=5)` 回执 4 张卡片
（149/147/145/143），正文已列 4 封，"另有 3 封"正确值是 **0**。
即便 `daily_briefing` 为真，它也照样会被跳过。

### 11.3 修正

| 项 | 改动 | 位置 |
| --- | --- | --- |
| 触发面 | `daily_briefing` **不再参与判定**，改为 `traces.iter().any(\|t\| t.tool.is_email() && !t.cards.is_empty())`——本轮只要调过邮件工具并拿到回执，落库前一定对账 | `me-sync/src/ai.rs` 收尾兜底区 |
| 检查范围 | 拆成两层：假锚点与「另有 N 封」数错**全篇**查；「没写 id 的邮件行」只在一「未读邮件」段补标 | `me-core/src/ai.rs:align_email_citations` |
| 邮件行定义 | 补 `>` 与 `\|`（引用块、表格行）；重命名 `is_bullet_line` → `is_mail_line` | 同上 |
| 归并句 | 新增 `merged_mail_count`：`另有 N 封` 的 N 必须等于该行列出的 id 个数，不符则计数 `mismatched` 并标 `（未核实）` | 同上 |
| 标记方式 | 假锚点**就地替换**成 `（未核实）`，不再"删锚点 + 行尾追加"——行尾追加会破坏 Markdown 表格 | 同上 |
| 函数/类型名 | `align_briefing_citations` → `align_email_citations`、`BriefingCitationAudit` → `MailCitationAudit`（名字里带 briefing 会继续误导后来人） | 同上 |
| 日志 | 消息改成「邮件引用对账」，字段加 `mismatched` 与 `briefing`（后者仅作标记，供区分自动简报与手粘） | `me-sync/src/ai.rs` |

### 11.4 新增用例

| 用例 | 钉住什么 |
| --- | --- |
| `align_cleans_unknown_ids_outside_briefing` | 没有「未读邮件」段的普通回答，假锚点照样被摘 |
| `align_does_not_mark_bare_lines_outside_briefing` | 普通句子不因"没写 id"被标——证明范围放宽没有连带误伤 |
| `align_marks_merged_count_mismatch` | 复刻 msg 26：4 封全列 + `> 另有 3 封` → `mismatched=1`，行留、标记补 |
| `align_accepts_merged_line_with_matching_ids` | 归并句写对时一个字都不改 |
| `align_keeps_table_row_shape` | 表格行里的假锚点就地替换，行仍是 `\| … \|` |

原有 6 个用例全部保留并继续通过（其中 `align_leaves_content_alone_without_unread_section`
被**删掉并替换**——它断言的正是"非简报不动"，正是这次要推翻的行为）。

### 11.5 仍然查不到的两处（诚实记账）

1. **完全没有 id 的编造**：msg 24 编了两封「笔试通知」+ 一个假链接，全文没有 `[id=…]`。
   id 对账对此无解——要么改模型让它必须写 id，要么按"发件人 + 主题"做模糊匹配（会误伤合理改写）。
2. **`mail_read` 独用的轮次**：`mail_read` 不产出 cards，白名单为空 → 直接原样返回。
   安全（不会误标），但"读了正文再复述"这条路没有校验。

> 这两条都写在 §4「明确不做」与 §10 的后续项里，等真出事故再动。
