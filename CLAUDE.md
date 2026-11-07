# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目概览

MyEmail 是 Rust + egui/eframe 的原生桌面邮件客户端：多账户 IMAP 收信、SMTP 发信、SQLite 全量摘要 + 文件正文缓存、FTS5 离线全文检索。**v0.1、v0.2、v0.3 已闭环**；当前为 **v0.4（0.4.0）**：schema **v7**，在 v0.3 的「邮件 + 日历 / 天气 / AI / 音乐」之上，邮件侧新增**搜索语法与保存的搜索、批量多选、联系人自动补全、统一收件箱、会话线程**五项能力（详见 [doc/0.4.0发布说明.md](doc/0.4.0发布说明.md)）。
自动化测试 **710 passed / 6 ignored**（`cargo test --workspace` 实测）；`cargo fmt --check` / `cargo clippy -D warnings` 通过。**已验收**真实天气 API Key、真实 AI Key（两种协议）、真机音频；**尚未验收**真实 Gmail/Outlook 的 OAuth 与 IDLE、Docker GreenMail E2E、多账户真实环境的批量删除联调——见 [doc/0.4.0发布说明.md](doc/0.4.0发布说明.md) §验证状态。

**仍未交付的项**（写代码前先看，别当成已实现）：`.ics` 导入导出与邮件 `text/calendar` 邀请导入、系统通知（只有应用内 toast）、日历「仅此次」编辑界面、四个模块的 `feature_*` 开关接线（`FEATURE_CALENDAR` / `FEATURE_AI` 只有常量、无读取方）、AI 会话滚动分页（当前固定拉 100 条）、阅读页的线程侧栏、线程头的服务器重拉（没打开过的历史邮件无法成串）、联系人手动编辑 / 导入、批量「移动到文件夹」。

文档的事实源关系（与代码冲突时以文档为准，改代码也要同步文档）：

- [doc/数据结构.md](doc/数据结构.md) §4.2/§4.3/§4.4：DDL 与字段语义的**唯一事实源**，[crates/me-store/src/migrations.rs](crates/me-store/src/migrations.rs) 的 `SCHEMA_V2_DDL` / `SCHEMA_V3_DDL` 必须与之一致。
- [doc/项目架构.md](doc/项目架构.md)：分层、依赖方向、线程模型、通信契约。
- [doc/versiondoc/v01架构设计.md](doc/versiondoc/v01架构设计.md)：v0.1 的文件级职责与关键流程。
- [README.md](README.md)：**GitHub 主页展示版**——面向访客讲技术要点与创新点（自研 HTML 排版引擎 / SSE 解码 / 分层架构 / AI 安全边界），不是使用说明。使用说明与已知缺口的完整清单在 `doc/versiondoc/v03收尾.md` §7。
- `doc/versiondoc/` 下是**历史版本快照**（v01/v02/v03 架构设计、收尾清单、发布说明），改当前事实不要改它们；新内容写进 `doc/` 根目录。

## 常用命令

```bash
# 构建 / 运行（bin 名为 myemail，产物 target/debug/myemail.exe）
cargo build
cargo run -p me-app

# 全量测试（自动跳过 #[ignore] 的 Docker E2E）
cargo test --workspace

# 单个测试文件 / 单个测试
cargo test -p me-store --test store_schema
cargo test -p me-store --test migration_v1_v2   # v1→v2 迁移、备份、回滚
cargo test -p me-store --test draft_sync_store  # Bcc / 同步状态 / 墓碑 / 冲突副本
cargo test -p me-store --test calendar_store    # 日历 CRUD、重复范围查询、提醒去重
cargo test -p me-store --test ai_store          # AI 仓储：分页、JSON 往返、实体上限
cargo test -p me-proto --test mime_fixtures
cargo test -p me-proto --test oauth_mock   # OAuth 令牌/设备码 mock
cargo test -p me-proto --test proto_mock   # mock IMAP 时序 + IDLE
cargo test -p me-sync --test sync_mock
cargo test -p me-sync --test draft_sync_mock
cargo test -p me-sync --test oauth_flow_mock
cargo test -p me-sync --test calendar_flow
cargo test -p me-sync --test weather_flow
cargo test -p me-sync --test ai_flow_mock
cargo test -p me-app views::html            # HTML 渲染器语料回归
cargo test -p me-core attachment_limits  # me-core / me-app 的测试是模块内联测试，按测试名过滤

# 真实 IMAP/SMTP 端到端（需 Docker Desktop 运行；启动 GreenMail: IMAP 3143 / SMTP 3025）
scripts/e2e.sh
# 已手动起容器时：
cargo test -p me-sync --test e2e_real_imap -- --ignored --nocapture

# 质量门禁（doc/v02架构设计.md §7 约定）
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check

# release 构建（lto=thin，codegen-units=1；**带 --remap-path-prefix**，否则 exe 里会留下构建机绝对路径）
bash scripts/build_release.sh

# 打发布包（exe + 同源源码 zip + 发布说明 → dist/<版本>/；会自动拒绝比源码旧的 exe）
python scripts/pack_dist.py --version 0.4.0

# 发布前泄露自检（本机标识 / 凭据形态 / 绝对路径 / 本机数据库真实内容），阻塞任何命中
python scripts/check_leak.py --dist dist/0.4.0

# OAuth 客户端：仅构建期注入，密钥不进仓库
# 未注入时，可在高级设置写 meta oauth_client_google / oauth_client_microsoft 覆盖 client_id/tenant；
# client_secret 无法在运行时覆盖。
MYEMAIL_GOOGLE_CLIENT_ID=... MYEMAIL_GOOGLE_CLIENT_SECRET=... \
MYEMAIL_MICROSOFT_CLIENT_ID=... MYEMAIL_MICROSOFT_CLIENT_SECRET=... \
cargo build --release -p me-app

# 重新生成内嵌 CJK 字体子集（需要 fontTools 与 C:\Windows\Fonts\NotoSansSC-VF.ttf）
python scripts/build-cjk-subset.py

# 音频后端能力验证（不参与产品逻辑，只回答设备/解码/暂停/seek/结束检测五个问题）
cargo run -p me-app --example audio_spike -- checktemp/audio/*
```

注意：Cargo 集成测试必须放在各 crate 的 `tests/` 下；根目录 `tests/` 只放 `e2e/docker-compose.yml`，不要往那里加 Rust 测试。`me-app` **没有** `tests/*.rs` 集成测试（只有 `tests/fixtures/tagged.mp3` 夹具），它的测试全部内联在各模块。

## 分层与依赖硬规则

```text
me-app ──Command/Event──► me-sync ──► me-proto
  │                          │  └────► me-store
  └──只读快照───────────────► me-store
所有 crate ──► me-core（唯一汇点，不依赖 tokio/rusqlite/协议库）
```

实际依赖（`Cargo.toml` 实测）：`me-core` 无内部依赖；`me-proto` → `me-core`；`me-store` → `me-core`；`me-sync` → `me-core` + `me-proto` + `me-store`；`me-app` → `me-core` + `me-store` + `me-sync`（**不依赖 `me-proto`**）。

1. 依赖只能沿箭头方向，不得成环。`me-core` 是唯一汇点。
2. `me-app` **不得 `use`** `async-imap`、`lettre`、`rusqlite` 的任何符号，也不持有写连接。它只拿 `me-store::ReadOnlyStore`（`ReadOnlyRepo`），且仅限键集分页与未读计数，单次 `LIMIT ≤ 200`，禁止全表扫描、FTS 检索与写操作。写操作一律发 `Command`。
3. `me-proto` 与 `me-store` 互不依赖，只通过 `me-core` 的类型通信。
4. 上游错误必须在 `me-proto` / `me-store` 内翻译为 `CoreError`；`async-imap` 的 error 类型、`rusqlite` 类型不得越过本层。

## 运行时与通信契约

- **主线程（eframe）**：只做事件循环、布局绘制、UI 状态更新和只读短查询；禁止网络/磁盘 IO、`await`、写库。
- **tokio 运行时**（独立线程，`worker_threads(4)`）：网络、同步、SMTP 投递、正文补全。DB 访问经 `SyncService::db()` 包 `spawn_blocking`。
- **DB**：全局唯一写连接（`Mutex<Connection>`）+ 4 条只读连接池（r2d2）；每次取连接执行 `foreign_keys=ON / journal_mode=WAL / busy_timeout=5000 / synchronous=NORMAL`。`Store::open()` 的顺序固定：建目录 → 开连接 → PRAGMA → `PRAGMA quick_check` → **schema 初始化** → WAL > 64 MB 时 `wal_checkpoint(TRUNCATE)` → 建只读池。
- **通道**：`Command` 无界；`Event` 有界 1024。判据是 `Event::is_progress()`——只有 `SyncProgress` / `IndexProgress` / `AiStreamDelta` 三个可丢；其余 60 个关键事件满了等待（背压）。UI 每帧最多抽 256 条事件（[crates/me-app/src/app.rs](crates/me-app/src/app.rs) 的 `MAX_EVENTS_PER_FRAME`）。
- **事件不是真相**：真相在数据库，事件只是「已完成 / 状态已变化」的快照。UI 用脏标记懒加载（`folders_dirty` / `messages_dirty` / `drafts_dirty` / `ai_conversations_dirty` / `ai_messages_dirty` / `ai_entities_dirty`，另有 `calendar.dirty` / `weather.dirty`），不要在事件处理里立即查询。
- **并发治理**（[crates/me-sync/src/tasks.rs](crates/me-sync/src/tasks.rs)）：`TaskRegistry` 按账户发放 `CancellationToken`，重复同步取消旧任务；`Governor` 用「全局许可 → 账户锁」两段获取避免环等待。**两套独立预算**：操作连接全局 2 / 单账户 1，监听连接全局 4 / 单账户 1。代码里**没有优先级队列**——所谓「发信 > 手动同步 > 后台补全」是**抢占式取消**：高优先任务先 `cancel_backfill(account)` 把该账户的后台补全停掉再抢连接。退避 30s → 2min → 5min（`BACKOFF_SECS = [30, 120, 300]`）。

## crate 关键事实

- **me-core**（[port.rs](crates/me-core/src/port.rs)、[command.rs](crates/me-core/src/command.rs)、[model.rs](crates/me-core/src/model.rs)、[ai.rs](crates/me-core/src/ai.rs)、[ai_plan.rs](crates/me-core/src/ai_plan.rs)、[calendar.rs](crates/me-core/src/calendar.rs)、[weather.rs](crates/me-core/src/weather.rs)、[music.rs](crates/me-core/src/music.rs)、[search_syntax.rs](crates/me-core/src/search_syntax.rs)、[error.rs](crates/me-core/src/error.rs)）
  - `SCHEMA_VERSION = 7`（[lib.rs](crates/me-core/src/lib.rs)）。v3 表结构冻结，v4/v5 是**纯加列**，v6 是**纯索引调整**（不动列/约束），v7 新增 `contacts` / `saved_searches` 两表并给 `messages` 补 `in_reply_to` / `"references"`。
  - [ai.rs](crates/me-core/src/ai.rs)：AI 领域类型与**纯函数**——`build_chat_request`（拼系统提示/历史/引用/实体，含字符预算与整轮裁剪）、`escape_context`（把 `---` 换成破折号，堵死伪造分隔符）、`select_entities`、`TOOL_SPECS`（**14 个工具**）/ `tool_specs(permissions)`、`AiToolName::is_replay_safe`、`CancelFlag`（`Arc<AtomicBool>`，不把 tokio 带进领域层）、`AiProvider` / `AiRepo` / `AiMemoryRepo` 端口。`AiCredential` 的 `Debug` 脱敏且**不实现 `Serialize`**。
  - 模型档案（v0.3）：`AiModel` 是**唯一**的调用参数来源（`id` / `name` / `kind` / `base_url` / `model` / `max_input_chars` / `max_output_tokens` / `tools` / `api_key`）。`tools` 是 `AiToolPermissions` 的 **9 个** bool 开关（`email_search` / `mail_read` / `weather` / `calendar_read` / `calendar_create` / `calendar_update` / `calendar_delete` / `music` / `memory`），`#[serde(default)]` 全开，**加字段不迁移**。`validate_model` 复用 `validate_config` 的端点规则并校验 key 形态（空可接受），名称判重走 `normalize_model_name`（trim + 小写），`next_model_id` 生成 `m1`/`m2`…，`resolve_model` 按「会话绑定 → 当前模型 → 列表第一个」三级回退。`AiConversation` 8 个字段，`model_id` 记录绑定，`provider`/`model` 保留为创建时快照。
  - **`api_key` 是普通字段**：与其他参数一起序列化进 `meta.ai_models`（明文），不走 keyring，没有「保存 Key」这类单独按钮。`AiModel` 因此**手写 `Debug`**，`api_key` 只输出 `<redacted>` / `<empty>`——这条不能因为「反正已经明文存了」而删掉：事件载荷、日志、测试失败输出都会经过 `Debug`。
  - 网络端口 `ImapSession` / `ImapSessionFactory` / `SmtpSender` 返回 **`impl Future<Output=..> + Send`** 而不是 `async fn`（多线程 tokio 要求 future 为 `Send`，而 `async fn` in trait 无法表达该约束；这是 RPITIT，不引入 `async-trait`）。`AiProvider` / `WeatherProvider` 则用 boxed future（`AiFuture` / `WeatherFuture`）以便 `dyn` 兼容。
  - 仓储端口（`*Repo`）是**同步** trait：`me-store` 明确禁止依赖 tokio。`StorePort` 是写端口的组合超 trait；`ReadOnlyRepo` 是 UI 唯一可见的读句柄。
  - v0.2 端口已冻结：`ImapSession::search_all_uids` / `wait_for_changes`（`IdleOutcome`）、`BrowserOpener`、`DraftRepo` 的草稿同步方法、`DraftSyncItem`。
  - 领域常量：附件单文件/单封总量 25 MB、单页 200（`MAX_PAGE_SIZE`）、解析上限 50 MB；附件超限用 `check_attachment_limits`。
  - [ai_plan.rs](crates/me-core/src/ai_plan.rs)：任务清单校验是纯函数 `validate_plan(prev, next)`，**按 id 合并**而非整份覆盖。约束：步数 1–6（`MAX_PLAN_STEPS`）、步骤 id 必须在 1–6 且不重复、标题必填且 ≤40 字符（创建后不可改）、`capability` 只能是 `mail`/`weather`/`calendar`/`music`（创建后不可改）、状态只能前进（`Pending`→`Running`→终态，终态不可改）、首次必须给 `goal`（≤120 字符）、`evidence` ≤120 字符。`PlanStatus` = `Pending`/`Running`/`Done`/`Failed`/`Skipped`。
  - [calendar.rs](crates/me-core/src/calendar.rs)：重复规则只支持 `FREQ=DAILY|WEEKLY|MONTHLY|YEARLY` + `INTERVAL`/`COUNT`/`UNTIL`/`BYDAY`(仅 WEEKLY)/`WKST`；`BYMONTHDAY`/`BYSETPOS`/`BYYEARDAY`/`BYWEEKNO`/`BYMONTH`/`BYHOUR`/`BYMINUTE`/`BYSECOND` 与带序号的 `BYDAY` 一律 `Unsupported`。**不支持的规则不丢弃原文**，展开时降级为「只显示首个实例 + warning」。展开纯函数 `expand_event(series, exceptions, from, to, limits)` 不返回 `Result`；配套 `recurrence_until`（算冗余上界）与 `due_reminders`。提醒上限：单事件最多 3 个（`MAX_REMINDERS`）、提前量最多 7 天（`MAX_REMINDER_MINUTES`）。展开保护：`max_scan=10_000` / `max_instances=2_000`，病态规则硬上限 200_000。
  - [weather.rs](crates/me-core/src/weather.rs)：`location_title(name, admin2, admin1)` 按 `name → adm2 → adm1` 取值、trim + 去空 + 去重后用 ` · ` 连接。和风天气的最小行政单位是**区/县/县级市**（搜「成都」会返回成都本级 + 锦江区/都江堰市一串下辖区县，`adm2` 全是「成都」），所以标题必须是 `具体地点 · 上级城市 · 省`，同名层级只显示一次。地点上限 8（`MAX_WEATHER_LOCATIONS`）、候选上限 20、快照 TTL 30 分钟、最小刷新间隔 60 秒、逐小时 48 点、逐日 7 天。`StoredWeatherSnapshot::VERSION = 2`，版本不符按「没有缓存」处理（v1 曾把 `iconDay` 当 `icon`）。
  - [music.rs](crates/me-core/src/music.rs)：播放队列与顺序的**纯计算**，不读文件、不依赖音频库、不碰 egui；随机顺序由调用方注入 RNG。
  - [search_syntax.rs](crates/me-core/src/search_syntax.rs)：搜索速记语法的**纯函数**解析器 `parse_search_syntax(input, today)` → `ParsedQuery`。支持 `from:` `to:` `subject:` `has:attachment` `has:none` `before:` `after:` `is:unread`；**不支持** `OR` / 括号 / 引号语法（O29 已拍板 v0.4 不做，但**会剥离**值两侧的引号）。**纯函数不读时钟**——`today` 由调用方传入（相对日期 `today`/`yesterday` 靠它推算），这样解析结果确定可单测。**未知片段进 `unknown` 而不报错**（探索性场景：拼错 `fromm:` 应得到「搜索照常执行 + 一条提示」，不是弹错误清掉输入）。`ParsedQuery::is_empty()` 是「空查询」的唯一判据（**不能只看 `text`**——速记语法下 `from:zhang` 的 `text` 为空但显然不是空查询）；`text_len()` 只数**正文部分**（`from:ab` 的正文部分是空的，走 `from` 的 `LIKE` 与全文检索无关）。AI 的 `email_search` 工具可复用同一解析器（D43）。
  - [thread.rs](crates/me-core/src/thread.rs)：会话线程分组的**纯函数**（v0.4 的 C5）。`thread_root` 取 `References` 第一个 → `In-Reply-To` → 自身 `Message-ID`；`thread_key` 是「账户 + 根」（**跨账户不合并**，同账户内同根才同串），三者都没有时退化成 `local:<id>`，保证缺 `Message-ID` 的邮件不挤成假线程。`thread_rows(messages, expanded)` 把一页邮件折叠成渲染行：组顺序取「组内最新一封」的位置（有新回复的线程浮上来），展开时组内按**时间正序**铺开（对话顺序），`group_len == 1` 不出标记。**根不在当前页也能成串**——分页只加载一部分时这是唯一可行的做法。
- **me-proto**（[imap.rs](crates/me-proto/src/imap.rs)、[oauth.rs](crates/me-proto/src/oauth.rs)、[mime.rs](crates/me-proto/src/mime.rs)、[smtp.rs](crates/me-proto/src/smtp.rs)、[utf7.rs](crates/me-proto/src/utf7.rs)、[ai.rs](crates/me-proto/src/ai.rs)、[sse.rs](crates/me-proto/src/sse.rs)、[qweather.rs](crates/me-proto/src/qweather.rs)、[bodystructure.rs](crates/me-proto/src/bodystructure.rs)）
  - 唯一允许使用 `async-imap` / `lettre` / OAuth / 天气 HTTP 的 crate；对外只暴露 `me-core::port` 契约与纯编解码函数。
  - [mime.rs](crates/me-proto/src/mime.rs) 启用 `mail-parser` 的 `full_encoding`：按 MIME 声明的字符集解码头与正文（UTF-8、GBK/GB18030、Big5、Shift_JIS 等），禁止再用 `from_utf8_lossy` 处理非 UTF-8 邮件。
  - 回复头（v0.4 C5）：`ParsedHeader` 带 `in_reply_to` / `references`（**裸 Message-ID**，不含尖括号）。解析时用 `header_raw` 取原文再自己扫 `<...>`——`References` 是**空格分隔**的 id 列表，`parse_comma_separared` 只按逗号切，用它会把整条链当成一个字符串。没有尖括号的脏数据才退化成按空白/逗号切；去重保序，上限 `MAX_REFERENCES = 32`（保留**开头**，根不会被截掉）。
  - 读正文/部件必须用 `BODY.PEEK[]`，禁止 `BODY[]`（后者会把邮件标为已读）。
  - 登录后按 CAPABILITY 发 `ID`（QQ/163 需要）；文件夹名用 modified UTF-7 编解码。
  - [oauth.rs](crates/me-proto/src/oauth.rs)：`ProviderConfig`（Google/Microsoft 端点，字段可被 mock 覆盖）、PKCE S256、授权码/刷新/设备码、userinfo。令牌端点禁止重定向、**20s 超时**、响应体 **≤64 KB**；`invalid_grant` 映射 `reauth_required`；所有令牌类型 `Debug` 脱敏。
  - 凭证按 `AuthCredential` 选择 `LOGIN` 或 `AUTHENTICATE XOAUTH2`；能力探测含 `IDLE` / `AUTH=XOAUTH2` / `MOVE` / `UIDPLUS` / `SPECIAL-USE`。
  - `wait_for_changes` 通过 async-imap `idle()` 实现：**超时必须 `DONE` 后放回 Session**；取消或错误留下半状态连接（`session` 置 `None`），由调用方丢弃，不 logout。
  - [ai.rs](crates/me-proto/src/ai.rs) 一个实现同时服务 Anthropic Messages 与 OpenAI 兼容两种协议——请求体是**纯函数**（`anthropic_body` / `openai_body`，相邻同角色消息合并、`tool_result` 并入 user），流式用 [sse.rs](crates/me-proto/src/sse.rs) 的按行增量解析。连接 15s / 单轮 120s / 响应 ≤4 MiB / 单行 ≤256 KiB；错误 `detail` 只含状态码与类别，不含响应体。**白名单外的工具名按协议错误处理**（见 §5.4 偏差说明）。
  - [sse.rs](crates/me-proto/src/sse.rs)：**按字节缓冲、按行切分**——`\n` 是 ASCII，不可能出现在 UTF-8 多字节序列中间，所以凑成完整行再解码，中文字符被 TCP 切在中间也不会切坏。响应体总上限 4 MiB、单行 256 KiB。`finish()` 会把残留缓冲当普通行冲刷出来（服务商没发最后一个空行时也能交出事件）。
  - [qweather.rs](crates/me-proto/src/qweather.rs)：**唯一**允许访问天气外部服务的 crate。端点 `/v7/weather/now`、`/v7/weather/24h`、`/v7/weather/7d`、`/geo/v2/city/lookup`（旧 Host 回退 `/v2/city/lookup`，路径按 Host 记忆探测结果）。**Host 是每次调用的参数，不是 provider 的构造参数**；`normalize_host` 去协议前缀、转小写、拒绝用户名/密码与查询串、协议一律 https、按字面量拒绝本机与内网（**不做 DNS 预解析**）。API Key 只走 `X-QW-Api-Key` 请求头（官方明确要求不要同时用 `key` 查询参数）。单请求 10s 超时、响应 ≤256 KiB、禁止重定向。
  - [bodystructure.rs](crates/me-proto/src/bodystructure.rs) 是**私有模块**（`mod bodystructure;`，不转出公开符号）：BODYSTRUCTURE 的递归解析与附件判定（`disposition == attachment`，或带 `name`/`filename` 且 `content-type` 不是 `text/plain`/`text/html`），畸形输入只返回 `None` 不 panic。
- **me-store**（[db.rs](crates/me-store/src/db.rs)、[migrations.rs](crates/me-store/src/migrations.rs)、[cache.rs](crates/me-store/src/cache.rs)、[search.rs](crates/me-store/src/search.rs)、[accounts.rs](crates/me-store/src/accounts.rs)、[folders.rs](crates/me-store/src/folders.rs)、[messages.rs](crates/me-store/src/messages.rs)、[attachments.rs](crates/me-store/src/attachments.rs)、[drafts.rs](crates/me-store/src/drafts.rs)、[pending.rs](crates/me-store/src/pending.rs)、[calendars.rs](crates/me-store/src/calendars.rs)、[weather.rs](crates/me-store/src/weather.rs)、[ai.rs](crates/me-store/src/ai.rs)）
  - `Store::open()` 依次做目录 / PRAGMA / `quick_check` / schema 初始化 / WAL checkpoint；之后 `writable()` 交给 `me-sync`，`read_only()` 交给 `me-app`。
  - 只做「存取 + 约束」，不判断业务语义；事务内不做网络 IO。**两处约束刻意放在存储层**（防竞态）：AI 实体上限 200、助手消息 256 KiB 截断；另有天气地点上限 8、默认地点唯一（部分唯一索引 + 事务内先清零再置位）。
  - FTS5 使用 `trigram` 分词：查询词 ≥3 字符才能命中正文，1–2 字符仅 `LIKE` 头部三列（`subject`/`from_addr`/`to_addrs`）。
  - `schema_version = 7`，迁移链 **v1→v2→v3→v4→v5→v6→v7**，每步单独事务、末尾 `foreign_key_check`；迁移失败不写版本号并中止启动。v3 引入日历/天气/AI 共 11 张表，v4 给 `ai_conversations` 补 `model_id`，v5 给 `ai_messages` 补 `plan`（任务清单 JSON），v7 新增 `contacts` / `saved_searches` 两表并给 `messages` 补 `in_reply_to` / `references` 两列。v4/v5 都用 `pragma_table_info` 判列**幂等**执行——新建库的 DDL 已带该列，只有真正的老库需要 ALTER。中间步骤的版本号**写死**（v2→v3 写 `3`、v3→v4 写 `4`、v4→v5 写 `5`、v5→v6 写 `6`、v6→v7 写 `7`），不能写 `SCHEMA_VERSION`，否则会给旧库盖上「已是最新」的章而跳过后续迁移。
  - **v5 → v6 是纯索引调整**（[migrations.rs](crates/me-store/src/migrations.rs) 的 `migrate_v5_to_v6`）：`idx_messages_folder_date` 补 `id DESC`、`idx_messages_body_pending` 改成 `(account_id, folder_id, date DESC, id DESC)`、新增 `idx_messages_unread(folder_id, (flags & 1))` 与 `idx_drafts_updated`、两条 AI 索引补 `id DESC`。SQLite 没有 `ALTER INDEX`，改列序只能 **DROP + CREATE**（对同名索引幂等，且列序必然与 DDL 一致）。**索引列序必须与 `ORDER BY` 前缀严格对齐**——只补 `id` 不够，首列不对仍要临时排序（详见 [数据结构.md](doc/数据结构.md) §6.1/§6.3）。
  - 全库 **22 张表**（21 张普通表 + `messages_fts` 虚表；`contacts` / `saved_searches` 是 v7 新增的两张）。
  - **线程两列**（v0.4 C5）：`messages.in_reply_to` / `messages."references"`（`references` 是 SQLite 保留字，**DDL 与所有 SQL 里都要加引号**，`pragma_table_info` 比对时用裸名）。`"references"` 存 JSON 数组。**`NULL` 与 `"[]"` 语义不同**：`NULL` = 还没回填过（回填队列的判据就是 `in_reply_to IS NULL AND "references" IS NULL`），`"[]"` = 回填过、确实没有引用链。入库时空链写 `NULL`，回填写 `"[]"`——两者写反的话，没有回复头的邮件会永远留在队列里、每次启动重解析一遍。回填队列用 `id > after` **游标推进**（`thread_backlog`），不取「前 N 条」：读不到 `raw/` 的行会一直留在队列里，取前 N 条会让同一批被反复选中、整轮卡死。
  - Drafts 文件夹不再写入 `messages`；草稿只走 `drafts` 表（`bcc_addrs` 仅本地保存，不写入 RFC822）。草稿同步状态机 6 态：`local_only` / `synced` / `local_dirty` / `conflict` / `deleted_local` / `deleted_server`；墓碑 = `deleted_at` 时间戳 + `deleted_local`；冲突副本用 `conflict_of` 列指向原草稿、标题加 `[冲突副本] ` 前缀、`raw_path` 置空串。
  - 日历范围查询把「一次性事件」与「未结束的重复事件」拆成**两条 SQL**：重复系列用 `start_utc <= ? AND (recurrence_until IS NULL OR recurrence_until >= ?)` 且**降序**取最近一批，避免历史重复事件挤掉当月视图。提醒去重主键是 `calendar_reminder_fires (event_id, occurrence_at, minutes_before)`，写入用 `INSERT OR IGNORE`。
- **me-sync**（[lib.rs](crates/me-sync/src/lib.rs)、[auth.rs](crates/me-sync/src/auth.rs)、[watcher.rs](crates/me-sync/src/watcher.rs)、[engine.rs](crates/me-sync/src/engine.rs)、[indexer.rs](crates/me-sync/src/indexer.rs)、[events.rs](crates/me-sync/src/events.rs)、[tasks.rs](crates/me-sync/src/tasks.rs)、[drafts.rs](crates/me-sync/src/drafts.rs)、[oauth_flow.rs](crates/me-sync/src/oauth_flow.rs)、[calendar.rs](crates/me-sync/src/calendar.rs)、[weather.rs](crates/me-sync/src/weather.rs)、[ai.rs](crates/me-sync/src/ai.rs)、[ai_tools.rs](crates/me-sync/src/ai_tools.rs)、[ai_music.rs](crates/me-sync/src/ai_music.rs)）
  - `SyncService<S, F, M>` 泛型化以便测试注入 mock；生产组合是 `NativeService = SyncService<WritableStore, ProtoFactory, ProtoSmtp>`，由 `start_native()` 构造——**AI 传输层在这里注入**（`HttpAiProvider::new()`），所以 `me-app` 不需要直接依赖 `me-proto`。
  - `Command` 共 **71 个变体**，`dispatch` 全覆盖（其中 6 个日历命令用 or-pattern 合并成一个 arm）。`Event` 共 **63 个变体**（定义在 `me-core/src/command.rs`，`me-sync/src/events.rs` 只放发送器）。
  - `auth.rs::TokenManager` 统一解析凭证：内存缓存（OAuth 提前 60 秒视为过期）→ keyring（密码）；OAuth2 用 keyring 中的刷新令牌 JSON 单飞刷新（per-account `AsyncMutex`），访问令牌只在内存；缺少客户端配置是配置错误，`invalid_grant` 才标记 `needs_reauth`。刷新令牌来源优先级：**会话内存 → keyring**。
  - `watcher.rs`：每账户 IDLE（仅 INBOX，**29 分钟**一轮）+ 轮询兜底；IDLE 去抖 **2s**；连续失败 **3 次**降级轮询；轮询间隔 clamp 到 [60, 3600] 秒、默认 300 秒、**±10%** 抖动；监听连接走独立 `Governor`；手动同步进行中不启动实时同步。
  - `drafts.rs`：Drafts 全量 UID 扫描、本地/服务器双基线哈希、推送/导入/冲突副本/墓碑删除；`draft_gate` 串行化草稿同步；草稿失败不影响整账户邮件同步退避。
  - `oauth_flow.rs`：回环回调（只绑 `127.0.0.1:0`，state 是 32 字节随机数、**常量时间比较**、5 分钟超时、socket 读取单次 2s）与设备码轮询（`SlowDown` 时间隔 +5s）；成功后创建账户或更新同一账户的令牌；浏览器通过注入的 `BrowserOpener` 打开。
  - `run()` 消费 Command 循环；每个命令 `dispatch` 到独立 handler，DB 访问统一走 `self.db(...)`。
  - `threads.rs`（v0.4 C5）是线程头的**离线回填**：`thread_backlog` → 解压 `raw/*.eml.gz` → `parse_headers` → 写回两列。**不联网**，因此不占 `Governor` 的连接预算；文件 IO 与解析都在 `spawn_blocking` 里。启动时 `startup()` 自动跑一次，另有 `Command::BackfillThreads` 手动入口；`threads_running: AtomicBool` 做全局单飞。写完发 `Event::ThreadsBackfilled { updated }`（`updated == 0` 不发），UI 收到后置 `messages_dirty`。
  - 事件经 `EventSender` 发出：进度事件可丢，关键事件背压；`ProgressThrottle` 每 ≥200 ms **或**每 50 封发一次；AI delta 另有 `DeltaThrottle`（≥50 ms 或 ≥2 KB）。
  - 后台补全队列为 `body_path IS NULL AND uid IS NOT NULL`，按 `(folder_id, date DESC, id DESC)` 排序（取批 200），排除 Drafts 文件夹；**`folder_id` 必须留在最前**——indexer 是**游程式**消费（`jobs[index].folder` 之后只吃「连续同 folder」的一段，每段一次 `SELECT` 再批量 `FETCH`），文件夹一旦交错，同一文件夹会被反复 SELECT、IMAP 往返成倍增加；文件夹**内部**按时间倒序，保证最近的邮件先被索引。`date` 为 NULL（缺 Date 头）在 DESC 下排末位。进度天然持久（`body_path` 非空即完成）。
  - `calendar.rs` 是日历写操作的**唯一实现**（`dispatch` 与 `ai_tools` 都只是薄壳）——算 `recurrence_until`、校验、来源归属、事务内落库、发事件只在这里做一次。AI 侧不复用 `Command`：Command 是无界通道 fire-and-forget，拿不到返回值，而工具调用必须同步返回事件 id。
  - `weather.rs` 是天气地点与快照的**唯一写入方**。刷新**以地点为单位**：每个地点各自单飞、60s 最小间隔（**手动刷新也遵守**）、30s→2min→5min 退避（`force` 可跳过退避但不能跳过最小间隔）、连续失败 3 次后停止自动重试；快照未过期直接复用不发请求；**失败时保留旧快照**（只发 `WeatherFailed { has_cache }`）。Key 与 Host 一起明文存 `meta`（不走 keyring）。
  - `ai.rs` 是 AI 用例层：全局并发 2 条流、**同一会话同一时刻只允许一条**、最多 **6 轮模型 / 12 次工具调用**（`MAX_TOOL_ROUNDS` / `MAX_TOOL_CALLS`）、整轮预算 **300 秒**（`STREAM_BUDGET`）。三条不变量：①流式期间只写内存与事件，**终止时一次性落库**；②终止事件不携带全文，UI 收到后从数据库重载（`AiStreamDelta` 可丢）；③工具调用与最终回复**不是同一事务**。
  - `ai_tools.rs` 是工具的**薄壳**：只做参数转换、限额、超时、结果映射，不许出现 `*Repo` 写方法、`*Provider` 调用，也不许自写 SQL/FTS。限额：`mail_read` 3 次/回复、`weather_get` 1 次/回复、`calendar_*` 5 次、`memory_upsert` 5 条；超时：邮件检索 2s / 读信 3s / 日历 5s / 记忆 5s / 天气 20s。`plan_update` 是纯内存、不套超时、**不占 12 次工具预算**。
  - `ai_music.rs` 是音乐工具的桥：发 `Event::AiMusicCommand` → `me-app` 用 `AudioPlayer` 执行 → `Command::AiMusicResult` 回执，**超时 3 秒**；超时按「工具错误结果」回报给模型，不让整轮失败。超时后必须清掉等待项，否则用户点「停止」后迟到的回执会唤醒下一次调用。
- **me-app**（[main.rs](crates/me-app/src/main.rs)、[app.rs](crates/me-app/src/app.rs)、[state.rs](crates/me-app/src/state.rs)、[views/](crates/me-app/src/views/)、[views/html/](crates/me-app/src/views/html/)、[audio/](crates/me-app/src/audio/)、[browser.rs](crates/me-app/src/browser.rs)、[images.rs](crates/me-app/src/images.rs)、[fonts.rs](crates/me-app/src/fonts.rs)）
  - 这是 bin crate（`myemail`），`src/lib.rs` 为空，测试内联在各模块。
  - `main.rs` 是启动装配：数据目录 → 单实例文件锁 → 日志 → `Store::open` → 通道 → 回环 IPC 激活监听 → tokio 运行时 → `service.run` → eframe；**窗口先渲染空态，不等待网络**。第二个实例通过 `.instance.port` 里的端口 + token 请求已有实例聚焦窗口。
  - `build.rs` 检测 `assets/fonts/cjk-subset.ttf` 并设置 `cfg(has_cjk_asset)`；缺字体时运行时回退系统 CJK 字体。
  - [browser.rs](crates/me-app/src/browser.rs) 实现 `SystemBrowser`（只接受 http/https）；OAuth 授权页与设备码页交给系统浏览器。
  - 远程图片加载（[images.rs](crates/me-app/src/images.rs)）：仅 http/https、端口 80/443/8080/8443、拒绝本机/私网/CGNAT、逐跳 DNS 与重定向校验（最多 3 跳）、不发 Cookie/Referer、单张 ≤8 MB、20s 总超时（连接 8s）、并发 4、会话内缓存 ≤64 MB；设置页可整体关闭。
  - [views/html/](crates/me-app/src/views/html/) 是 v2 渲染器：`scraper` 解析 HTML5 DOM，`css.rs` 处理 CSS 子集与选择器优先级，`parse.rs` 清洗并生成扁平 `RenderedHtml`（保留 `inline-block` 列容器、`Section` 盒样式与表格单元格 padding），`layout.rs` 是排版引擎（按内容宽度测量块/行/单元格矩形，`LayoutJob` 整段断行、`line-height`/字距/逐行对齐、真实 `rowspan`/`colspan` 网格、图片占位），`mod.rs` 负责后台并发解析（`spawn_blocking`，并发 2）与结果模型。解析任务按 generation 丢弃旧结果；**节点 ≤50k、深度 ≤128、文本 ≤2MB**，超限/异常回退到 v0.1 的 `richtext` 子集。
  - AI 传输（v0.3）：[ai.rs](crates/me-proto/src/ai.rs) 一个实现同时服务 Anthropic Messages 与 OpenAI 兼容两种协议——请求体是**纯函数**（`anthropic_body` / `openai_body`，相邻同角色消息合并、`tool_result` 并入 user），流式用 [sse.rs](crates/me-proto/src/sse.rs) 的按行增量解析（一条事件被 TCP 切成两半、甚至切在 UTF-8 中间都要能拼回来）。禁重定向、连接 15s / 单轮 120s、响应 ≤4 MiB、单行 ≤256 KiB；错误 `detail` 只含状态码与类别，不含响应体。**白名单外的工具名按协议错误处理**（见 §5.4 偏差说明）。
  - AI 页的三块「非对话」内容：**任务清单**（`plan_update` 维护，底部清单条，全部结束折叠成一行）、**删除确认卡**（`pending_delete`，确认/取消，取消只丢卡片）、**邮件卡片**（最多 5 张 + 「还有 N 封」，卡片带命中理由；点「打开邮件」在当前页原地弹阅读窗——`UiState::reader_open` 控制，切页时清除，**不能**用 `selected_message.is_some()` 代替：那样阅读窗会跟着用户跳到每个页面）。
  - AI 页（[views/ai.rs](crates/me-app/src/views/ai.rs)）：**主区永远只有对话**，设置是 `egui::Modal` 弹窗（`settings_modal`，点遮罩/Esc/「关闭」收掉，离开 AI 页自动关闭），由 `app.rs` 在顶层用 `ctx` 渲染。表单分两形态：新建只列「模型名称 / 协议 / URL 端点 / 模型实际名称 / APIKey」五项（工具与预算走默认值），编辑已有档案才展开全部参数与「测试连接 / 设为当前 / 删除」。**切换会话必须走 `UiState::select_ai_conversation`**：它把「清缓冲」和「置 `ai_messages_dirty`」绑在一起，只调 `AiPageState::select` 会让主区一直空白（切页回来时 `switch_page` 顺手置脏，才显得「回来就正常」）。
  - 音频与日历（v0.3）：[audio/](crates/me-app/src/audio/) 是音乐播放器——`walkdir` 扫描（深度 ≤8、≤2000 文件、不跟随符号链接；只认 mp3/flac/wav/ogg）、rodio 解码、独立音频线程（状态 **250ms** 回传）、`symphonia` 读标签与内嵌封面（随 rodio 一起编译，不新增 crate）、`SpectrumSink` 在音频回调里采样并整块交接给 UI 做 FFT（**48 柱**，回调线程永不阻塞）。频谱**不经 Command/Event**，播放状态也不落库。
  - [views/calendar.rs](crates/me-app/src/views/calendar.rs) 与 [views/music.rs](crates/me-app/src/views/music.rs) 渲染日历与音乐页；两者都用「先 `allocate_space` 占整块、再用绝对 `Rect` 定位」而不是嵌套 panel（CentralPanel 内嵌的 `TopBottomPanel` 拿不到高度，`allocate_ui_with_layout` 会收缩父游标）。
  - **统一收件箱（v0.4 的 C1）是视图层的虚拟项**，不是数据库里的文件夹行：`folders.role` 有 CHECK 约束且 `account_id NOT NULL`，插不进一行「属于所有账户的收件箱」。状态是 `UiState.unified_inbox: bool`，与 `selected_folder` **互斥**——互斥只在 `select_unified_inbox()` / `select_folder()` / `clear_folder_selection()` 三个方法里维护，**禁止别处直接写这两个字段**（否则会出现两者同时成立的矛盾态，列表取数二义）。聚合必须在 store 层一次查完（`ReadOnlyRepo::list_unified_inbox_readonly` / `unified_unread_count_readonly`）：`folder_id IN (SELECT id FROM folders WHERE role='inbox')`（**「收件箱」是文件夹角色不是账户**，一个账户可以有多个 inbox）且**排除 `uid IS NULL`**（本地已移动、服务器身份待确认的行不该出现在聚合视图）；UI 端逐账户拉再本地合并会破坏键集分页。聚合模式多一列「账户」，且**不要求先选定单个账户**；`selected_folder.is_none()` 的空态判断必须带 `!unified_inbox`（否则统一收件箱被误判成「请选择文件夹」）。聚合视图里搜索**不按账户过滤**（`SearchQuery.accounts` 为空 = 不限），删除时按**邮件所属账户**找废件箱。
  - 天气页（[views/weather.rs](crates/me-app/src/views/weather.rs)）：**刷新以地点为单位**——`Command::RefreshWeather { id, force }` 带地点 id，`me-sync` 按地点分别计算单飞 / 60s 最小间隔 / 退避，每个地点各有一份快照。界面侧「当前地点」= 选中优先、否则默认地点；「刷新中」与失败提示也按地点分别记。侧栏地点行一行放齐：左「删除」· 中「名称」· 右「设置默认」，**按钮任何侧栏宽度下都要可见**。
  - **侧栏横向行的宽度必须交给布局算**：`available_width()` 在 horizontal 布局里返回的是**父容器**的剩余宽度，不含本行已放元素的占位。手工按它减固定宽分配会漏掉元素间的 `item_spacing`（8px）→ 每帧溢出 → 面板 `min_rect` 变宽 → egui 持久化 → 下一帧再宽 8px → **正反馈**，面板一路涨到窗口边缘（`SidePanel` 宽度上限是 `f32::INFINITY`，拦不住）。行内剩余要用 `max_rect.right() - cursor.min.x`。这类缺陷只在**多帧**下暴露，必须补「真实容器 + 连跑多帧 + 断言尺寸不漂移」的回归测试。
  - **列表按会话线程折叠**（v0.4 C5）：`UiState::thread_fold`（默认开）+ `expanded_threads: HashSet<String>`（键是 `me_core::thread_key`，允许留过期键）。渲染行统一走 `UiState::thread_rows()`——**关闭折叠时它返回恒等映射**，折叠版与关闭版共用同一条渲染路径（分成两套分支迟早画出不一样的东西）。折叠只改「画哪几行」，`messages` 永远是完整一页：多选、批量操作、分页仍按 `messages` 下标工作；`Shift` 连选的区间则按**可见顺序**算（折叠时看不见的行不该被选进来）。折叠标记画在主题列（`▸ N` / `▾ N`、后续行 `↳`），不新增列；点标记用 `thread_clicked` 拦住行点击，否则会顺带打开邮件。
  - 天气候选的行政层级（[me-core/src/weather.rs](crates/me-core/src/weather.rs) 的 `location_title`）：搜「成都」会返回成都本级 + 锦江区/龙泉驿/都江堰市一串下辖区县（`adm2` 全是「成都」），所以标题必须是 `具体地点 · 上级城市 · 省`，同名层级只显示一次；请求带 `number=20` 以免截断。
- **AI 工具**（`TOOL_SPECS`，14 个）：邮件 2（`email_search` / `mail_read`）/ 天气 2（`weather_get` / `weather_locations`）/ 日历 4（`calendar_list` / `calendar_create` / `calendar_update` / `calendar_delete`）/ 记忆 1（`memory_upsert`）/ 音乐 4（`music_search` / `music_play` / `music_state` / `music_control`）/ 清单 1（`plan_update`）。请求期按档案 9 个开关过滤（扁平 bool，`#[serde(default)]` 全开，**加字段不迁移**），**执行期再校验一次**（模型返回本轮未下发的工具 → 直接拒绝）。
  - **可重放白名单是正向列举**（`AiToolName::is_replay_safe`）：只读的 `email_search` / `mail_read` / `weather_get` / `weather_locations` / `calendar_list` / `memory_upsert` / `music_search` / `music_state` / `plan_update` 为 `true`（9 个），其余 5 个 `calendar_create` / `calendar_update` / `calendar_delete` / `music_play` / `music_control` 一律 `false`。「重新生成」那一轮只下发 `true` 的——写成「排除已知写工具」的话，以后新增有副作用的工具会默认可重放。
  - **两个工具的执行不在 `me-sync`**：
    - `calendar_delete` 只发 `Event::AiToolCalendarDeletePending`，真正的删除由用户在 AI 页卡片上点「确认删除」→ `Command::DeleteCalendarEvent`；确认后 10 秒内可 `Command::RestoreCalendarEvent` 撤销（只清 `deleted_at`，不动内容）。返回给模型的文本必须写明「在用户点确认之前这条日程仍然存在，不要声称已经删除」。
    - 四个音乐工具发 `Event::AiMusicCommand`，由 `me-app` 用 `AudioPlayer` 执行，再以 `Command::AiMusicResult` 回执（3s 超时）。播放器用 `App::ensure_player` 惰性启动，**不依赖用户先进过音乐页**。
  - 工具结果只回摘要，`tool_trace` 不存参数与结果原文。每个工具必须带 `motive`（缺失降级执行，不重试）。

## 必须保持的行为不变量

- **标志位本地优先**：本地变更先写 `messages.flags` + `pending_flags`（目标态，UPSERT 合并），再异步回写服务器；服务器标志回拉时，有 `pending_flags` 的行以本地目标态为准。
- **删除路径**：移入废件箱用 MOVE，否则 COPY + `\Deleted`；永久删除/清空废件箱要求 `UIDPLUS`，否则禁用并提示。待同步操作写 `pending_ops`，`message_id` 用 `ON DELETE SET NULL`，不能级联丢掉队列。
- **草稿**：本地草稿是唯一事实源；发送前固定 `Message-ID`；SMTP 成功但归档失败要发 `SendCompleted { archived: false }` 并支持 `RetryArchive`。**Bcc 存在 `drafts.bcc_addrs`（本地明文库），绝不写入 RFC 5322 原文**；重新打开草稿时从该列恢复。
- **凭证**：`AuthCredential` 的 `Debug` 必须保持脱敏（密码与 access token 输出 `<redacted>`）。密码/授权码只进系统密钥库（`keyring`），keyring 不可用时仅进程内存；OAuth 刷新令牌只进 keyring、访问令牌只在内存（`me-sync/src/auth.rs` 的 `TokenManager`）。日志不得记录正文、凭证、snippet 或完整邮箱（用 `mask_email`）。
  - **例外**：AI 的 `AiModel.api_key` 明文存 `meta.ai_models`（用户明确要求「key 与其他参数一样的保存方式」，见 v03 文档偏差 5）；天气 API Key 也与 Host 一起明文存 `meta.weather_api_key`（用户要求「跟 Host 一起保留」，见偏差 7——keyring 在部分 Windows 环境写入静默失败，导致退出应用后失效）。两者仍需过 `Debug` 脱敏、仍不得进 `tracing`；天气配置弹窗只用暗文（密码框）显示，不回显明文。
- **TLS**：统一 rustls；`deny.toml` 禁止 OpenSSL / native-tls；默认隐式 TLS / STARTTLS，明文 TLS 需 `plaintext_tls_ack` 显式确认。
- **缓存布局**：`%APPDATA%\MyEmail\`（其他平台 `dirs::data_dir()`）下 `myemail.db`、`bodies/`、`raw/*.eml.gz`、`attach/`、`drafts/`、`logs/`（按天滚动保留 7 天）。磁盘剩余 <500 MB（`MIN_FREE_BYTES`）暂停后台补全并提示；这个检查在 `me-sync/src/indexer.rs` 与 `me-sync/src/ai.rs`（`me-store::disk_space_ok()` 目前只有测试调用）。`attach/` 可通过设置重定位：`meta.attachment_dir` 保存绝对路径（空=默认），迁移会复制文件、重写 `attachments.local_path` 并删除旧目录；新目录须为空或带 `cache::ATTACHMENT_MARKER`。
- **事件载荷**是快照；不要让 UI 依赖「事件一定送达」来维护关键状态。

## 测试布局

| 位置 | 内容 |
| --- | --- |
| `crates/me-core/src/*.rs` 内联 | 标志位、snippet、日期回退、附件上限、事件分类、AI 请求构建、任务清单校验、重复规则展开 |
| `crates/me-store/tests/store_schema.rs` | schema、外键、FTS 增删改、去重、分页、标志合并、缓存不变量 |
| `crates/me-store/tests/migration_v1_v2.rs` | 冻结 v1 DDL → 迁移：备份、纯加列、Drafts 清理、级联/`foreign_key_check`、失败回滚 |
| `crates/me-store/tests/draft_sync_store.rs` | Bcc、同步状态、候选/墓碑、服务器身份唯一、冲突副本 |
| `crates/me-store/tests/calendar_store.rs` | 日历/事件 CRUD、重复范围查询（一次性与未结束重复两条路）、提醒候选、例外读取 |
| `crates/me-store/tests/ai_store.rs` | AI 仓储：会话/消息分页与正序、JSON 往返、孤儿 `streaming` 修复、超长截断、实体去重/别名/关联/200 上限 |
| `crates/me-store/src/weather.rs` 内联 | 地点上限 8、默认地点唯一、快照往返与版本失效 |
| `crates/me-proto/tests/proto_mock.rs` | mock IMAP 时序 + IDLE |
| `crates/me-proto/tests/oauth_mock.rs` | OAuth 授权码/刷新/设备码/错误分类/响应上限 |
| `crates/me-proto/tests/mime_fixtures.rs` | `.eml` 夹具（纯文本/multipart/仅 HTML/中文主题/缺 Date/畸形） |
| `crates/me-sync/tests/sync_mock.rs` | 真实 store + mock IMAP 的同步链路、去重、IDLE 触发同步 |
| `crates/me-sync/tests/draft_sync_mock.rs` | 草稿推送/导入/冲突/墓碑删除 |
| `crates/me-sync/tests/oauth_flow_mock.rs` | 回环授权回调、设备码 pending→token、重新授权 |
| `crates/me-sync/tests/weather_flow.rs` | 天气用例：城市搜索、多地点各自取数、按地点刷新与退避、失败保留缓存、配置保存/测试连接/密钥跨重启 |
| `crates/me-sync/tests/calendar_flow.rs` | 日历用例：事件增删改、提醒去重、重复展开后的范围查询 |
| `crates/me-sync/tests/ai_flow_mock.rs` | AI 编排：脚本化 `MockAiProvider` 的流式、工具循环与结果回灌、取消、401 分类、隐私确认拦截、工具开关收紧请求、重新生成只下发可重放工具（5 个有副作用的工具逐个断言）、已关闭工具在执行期被拒、清单建立/推进/落库/催促、取消时步骤标 `Skipped`、`calendar_delete` 只发确认不删库 |
| `crates/me-app/src/views/html/tests.rs` | HTML 清洗、CSS 子集、inline-block 结构、行高/对齐、表格跨度、宽度约束、URL 白名单、超限 |
| `crates/me-app/src/views/ai.rs` 内联 | 会话切换清态、流式增量覆盖、配置快照不覆盖正在编辑的草稿、模型 id 不冲突、会话回退到当前模型；离屏跑帧断言「设置是弹窗（主区仍有对话）」与「新建表单只有五项、编辑态仍有全参数」 |
| `crates/me-app/src/views/calendar.rs` 内联 | 日历视图与重复展开 |
| `crates/me-app/src/views/weather.rs` 内联 | 侧栏布局、逐地点刷新状态 |
| `crates/me-app/src/views/music.rs` 内联 | 曲目搜索匹配（AI 与界面共用 `find_tracks`） |
| `crates/me-app/src/audio/*` 内联 | 播放状态机、频谱、标签与封面、目录扫描 |
| `crates/me-app/src/*` 内联 | 地址解析/校验、HTML 表格、远程图片 URL 校验 |
| `crates/me-core/src/ai_plan.rs` 内联 | 清单校验：步数 1–6、非法 capability、标题/能力不可变、状态不可回退、首次必须给 goal、按 id 合并 |

## 其他

- 工具链由 [rust-toolchain.toml](rust-toolchain.toml) 固定为 `stable`（不写死版本，避免镜像缺包）；edition 2024，MSRV 1.88。
- 新增依赖先核对 [deny.toml](deny.toml) 的许可证白名单，并跑 `cargo deny check`。CI 工作流见 [.github/workflows/ci.yml](.github/workflows/ci.yml)（**只在 windows-latest 上跑** fmt / clippy / test / deny）。
- HTML v2 依赖 `scraper`（html5ever/selectors）、`cssparser`、`url`；不引入 WebView 或 JS 引擎。邮件正文字体族（`myemail-email` / `-bold` / `-mono`）在运行时从系统字体装配，找不到时回退内置字体。OAuth/HTML 的令牌与正文都不得进入日志。
- `target/`、`dist/`、`checktemp/`、`.review/` 是本机构建产物、发布包与审查报告，不是源码。
- 本机运行脚本用 `python`（`python3` 是 Windows 商店占位符）。
- HTML 性能基准（release）：`cargo test -p me-app large_html_parse --release -- --ignored --nocapture`（930 KB ≈ 35 ms）与 `large_table_parse`（1000×5 表格 ≈ 13 ms）。
