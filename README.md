# MyEmail

多账户统一收发 + 本地全量缓存 + 离线全文检索的原生桌面邮件客户端（Rust + egui）。

当前实现对应 **v0.3（0.3.0）**：在 v0.2 邮件主链路上加入**日历 / 天气 / AI 对话 / 音乐播放器**四个页面，
并把它们纳入同一套分层、`Command`/`Event` 通道与安全边界；数据库 schema 为 **v5**。

> v0.3 **未交付**：`.ics` 导入导出与邮件邀请导入、系统通知（只有应用内提示）、日历「仅此次」编辑、
> 四个模块的功能开关。真实 Gmail/Outlook 授权、真实天气/AI 密钥、真机音频、Docker E2E 仍需人工验收；
> 全部缺口见「[已知缺口](#已知缺口相对架构文档)」，逐项状态见 [doc/versiondoc/v03收尾.md](doc/versiondoc/v03收尾.md)。
>
> 发布包在 `dist/0.3.0/`（`dist/v03/`、`dist/v04/` 是过程快照，后者才是完整 0.3.0）。

## 快速开始

```bash
# 构建
cargo build

# 运行（Windows 下生成 target/debug/myemail.exe）
cargo run -p me-app

# 全量测试（548 passed / 5 ignored）
cargo test --workspace

# 真实 IMAP/SMTP 端到端测试（需 Docker Desktop 运行）
scripts/e2e.sh

# 打发布包：exe + 同源源码 zip + 发布说明 → dist/<版本>/
python scripts/pack_dist.py --version 0.3.0

# 发布前信息泄露自检（本机标识 / 凭据形态 / 真实数据）
python scripts/check_leak.py --dist dist/0.3.0
```

四个新页面在顶部页签：`邮件 / 日历 / 天气 / AI / 音乐 / 账户 / 设置`（`Ctrl+1..7` 直达）。
天气与 AI 都需要自备密钥：天气在页面右上角「配置密钥」，AI 在 AI 页「设置 → 模型」。

### OAuth 客户端配置（构建期）

Google / Microsoft 的 `client_id` 与可选 `client_secret` 通过**编译期环境变量**注入，不写入仓库：

```bash
MYEMAIL_GOOGLE_CLIENT_ID=xxx MYEMAIL_GOOGLE_CLIENT_SECRET=xxx \
MYEMAIL_MICROSOFT_CLIENT_ID=xxx MYEMAIL_MICROSOFT_CLIENT_SECRET=xxx \
cargo build --release -p me-app
```

高级设置可覆盖 `client_id` 与 Microsoft `tenant`（写入 `meta.oauth_client_google` / `meta.oauth_client_microsoft`）；`client_secret` 只能构建期注入，用户自带客户端需自行构建。

应用内入口：账户页「使用 Google / Microsoft 登录」（回环重定向）；Microsoft 另提供设备码登录；OAuth 账户可随时「重新授权」。授权回调只绑定 `127.0.0.1`，`state` 单次校验，5 分钟超时。

首次启动会提示“本地缓存为明文保存”，点击「我已知晓」后不再弹出。数据库与缓存位于：

```
%APPDATA%\MyEmail\            Windows（macOS/Linux 走 dirs::data_dir）
├─ myemail.db                 SQLite 主库（WAL）
├─ bodies/                    正文纯文本
├─ raw/                       原始邮件 .eml.gz
├─ attach/                    附件缓存
├─ drafts/                    草稿原文与托管附件
└─ logs/                      按天滚动，保留 7 天
```

`attach/` 可在「设置 → 本地缓存 → 附件下载目录」重定位到任意绝对路径（默认即上述路径）。更改会迁移已下载附件并更新数据库中的本地路径；「另存为…」也以该目录为起始位置。

## 工程结构

| crate | 职责 |
| --- | --- |
| `me-core` | 领域模型、端口 trait、`Command`/`Event`、错误；日历重复规则展开、天气图标码映射、AI 上下文裁剪与工具规格、任务清单校验；不依赖 tokio/rusqlite/reqwest |
| `me-proto` | IMAP/SMTP 适配、MIME 解析与构建、modified UTF-7；和风天气 HTTPS、AI 协议（Anthropic / OpenAI 兼容）与 SSE 解析、BODYSTRUCTURE 附件判定 |
| `me-store` | SQLite 仓储（邮件 / 日历 / 天气 / AI）、FTS5(trigram)、文件缓存、schema 与 v1→v5 迁移 |
| `me-sync` | 同步引擎、正文补全、连接预算、取消/退避、事件发送；日历/天气/AI 用例与工具执行 |
| `me-app` | eframe/egui 界面、视图、输入、文件对话框；HTML 渲染器 v2、音频线程与频谱 |

依赖方向与硬规则见 [doc/项目架构.md](doc/项目架构.md) §2。`me-app` 不依赖 `me-proto`——AI 传输层由 `me-sync` 的 `start_native()` 注入。

## 测试

| 位置 | 内容 |
| --- | --- |
| `crates/me-core/src/*.rs` 内联 | 标志位、snippet、日期回退、附件上限、事件分类、AI 请求构建、清单校验、重复规则展开 |
| `crates/me-store/tests/store_schema.rs` | schema、外键、FTS 增删改、去重、分页、标志合并、缓存不变量 |
| `crates/me-store/tests/migration_v1_v2.rs` | v1→v2 迁移、备份、Drafts 清理、外键检查、失败回滚 |
| `crates/me-store/tests/draft_sync_store.rs` | Bcc、同步状态、服务器身份、墓碑、冲突副本 |
| `crates/me-store/tests/calendar_store.rs` | 日历/事件 CRUD、重复范围查询、提醒候选、例外 |
| `crates/me-store/tests/ai_store.rs` | AI 仓储：会话/消息/实体、`tool_trace`、孤儿流式行修复、实体 200 上限 |
| `crates/me-store/src/weather.rs` 内联 | 地点 CRUD 与 8 个上限、默认地点、快照往返与版本失效 |
| `crates/me-proto/tests/proto_mock.rs` | mock IMAP：CAPABILITY/SELECT/LIST、标志分页、`BODY.PEEK[]`、MOVE/COPYUID、IDLE |
| `crates/me-proto/tests/oauth_mock.rs` | mock OAuth：授权码交换、刷新、设备码三态、`invalid_grant`、响应上限 |
| `crates/me-proto/tests/mime_fixtures.rs` | `.eml` 夹具：纯文本、multipart 附件、仅 HTML、中文主题、缺失 Date、畸形输入 |
| `crates/me-sync/tests/sync_mock.rs` | 真实 store + mock IMAP 的同步链路、去重与 IDLE 触发同步 |
| `crates/me-sync/tests/draft_sync_mock.rs` | 草稿推送、服务器导入、冲突保留两份、墓碑删除 |
| `crates/me-sync/tests/oauth_flow_mock.rs` | 回环授权回调、设备码 pending→token、账户令牌更新 |
| `crates/me-sync/tests/calendar_flow.rs` | 事件增删改、提醒去重、重复展开后的范围查询 |
| `crates/me-sync/tests/weather_flow.rs` | 城市搜索、按地点刷新与退避、缓存跨重启、配置弹窗的保存/测试/密钥持久化 |
| `crates/me-sync/tests/ai_flow_mock.rs` | 脚本化 `MockAiProvider`：流式、工具循环、取消、401、隐私确认、开关过滤、清单 |
| `crates/me-app/src/**` 内联 | HTML 渲染器语料回归、地址解析、远程图片 URL 校验、音频播放/频谱/标签、视图交互 |

> 架构文档中的 `tests/` 是跨 crate 测试的规划位置；Cargo 集成测试必须位于各 crate 下，因此实际落在
> `crates/*/tests/`，文件名保持 `store_schema.rs` / `proto_mock.rs`。`me-app` 没有集成测试目录，
> 它的测试全部内联在各模块。

## 已实现的行为要点

- 账户（v0.2）：密码/授权码；Google / Microsoft OAuth2 回环登录，Microsoft 另提供设备码；
  OAuth 账户可“重新授权”；刷新令牌存系统密钥库（或会话内存），访问令牌只在内存；授权回调只绑定 `127.0.0.1:0`。
- 同步：启动 + 手动刷新；`UIDVALIDITY` 变化重建；`UIDNEXT <= last_uid + 1` 判空；
  分页头部 `FETCH` + `BODYSTRUCTURE`；分页标志回拉；本地待同步标志优先。
- 实时同步（v0.2）：每账户 IDLE 只监听 INBOX，29 分钟重新 IDLE；事件去抖 2 秒后触发同步；
  服务器不支持 IDLE 或连续失败 3 次时自动降级轮询（默认 300 秒，±10% 抖动，可在设置页关闭或调整）。
  监听连接与操作连接独立计费（全局 4 / 单账户 1 与 全局 2 / 单账户 1），手动同步优先于实时同步。
- 正文：打开即本地已读并入队；`BODY.PEEK[]` 下载；`text/plain` 优先，仅有 HTML 时 `html2text` 生成索引文本；
  MIME 头与正文按邮件声明的字符集解码（UTF-8、GBK/GB18030、Big5、Shift_JIS 等）；
  正文与 `raw/*.eml.gz` 落盘，FTS 与 `body_path` 同事务。
  旧版本遗留的 U+FFFD 乱码会在启动时从原始邮件重新解析并修复摘要、正文缓存与 FTS。
- 阅读：支持 **富文本 / 纯文本 / Markdown** 三种视图。富文本使用应用内 HTML 渲染器 v2：
  HTML5 DOM 解析 + CSS 子集（类型/类/id/后代/子/属性选择器、内联样式、背景色、字体、行高、字距、
  边距、边框、圆角、文本对齐、`white-space`）。
  排版由独立布局引擎完成：先按内容宽度算出每个块、行、表格单元格的绝对矩形，再统一绘制；
  支持嵌套表格与 `display:inline-block` 列容器、`max-width` 约束、相邻边距折叠、真正的
  `rowspan`/`colspan` 网格、列表、引用、代码块、链接与图片。
  段落整体断行（一个段落一个 `LayoutJob`），`line-height`/`letter-spacing`、逐行 `text-align`
  与文本选区都按邮件设计还原；正文使用系统 UI 字体与真实粗体（找不到时回退内置字体）。
  解析在后台线程完成（并发 2），UI 只绘制扁平块；超限或异常时自动回退到简化视图，旧解析结果按 generation 丢弃。
  不执行脚本、不加载外部 CSS/字体、不使用 WebView；`position`/`float`/`flex`/`grid` 按块级文档流降级。
  并渲染正文图片：
  - `cid:` 内嵌图片（MIME inline 部件，已解码，单张 ≤ 4 MB、总量 ≤ 8 MB）；
  - `data:image/...;base64,...` 内联图片；
  - SVG 图片（logo / 签名图）经 resvg 光栅化，最大边长 2048 px；
  - `http(s)` 远程图片自动异步加载：仅 80/443/8080/8443，拒绝本机/私网/CGNAT 地址（含逐跳 DNS 检查与重定向校验），
    不发送 Cookie/Referer，单张 ≤ 8 MB、20 秒超时；加载中显示进度，失败时保留可点击的浏览器打开链接。
    可在“设置 → 隐私”整体关闭自动加载；关闭后只渲染内嵌图片。
    注意：自动加载远程图片会向图片服务器暴露你的 IP，这是为满足内联显示所做的取舍。
  Markdown 为常用子集（标题、列表、引用、代码块、粗斜体、链接）。不执行脚本、不解析 CSS。
- 撰写与草稿：本地草稿保存后可重新打开编辑（附件、收件人、抄送、密送、主题、正文）；密送只保存在
  本地 `drafts.bcc_addrs`，不写入 RFC 5322 草稿原文，重新编辑时从该列恢复。发送成功后归档到 Sent，归档失败可“重试归档”。
- 草稿双向同步（v0.2）：Drafts 文件夹全量 UID 扫描；本地/服务器各自维护基线哈希；新增、修改、删除双向传播；
  双方都修改时保留两份（冲突副本），不静默覆盖；服务器删除时保留本地副本等待用户决定；
  `UIDVALIDITY` 变化重建服务器身份；无 `UIDPLUS` 时明确降级并提示。草稿不进入 `messages` 与正文补全队列。
  账户页草稿列表提供「保留本地 / 使用服务器 / 保留两份」冲突处理，以及「保留本地 / 删除本地」服务器删除处置。
- 删除：移入废件箱（MOVE，或 COPY + `\Deleted`）；永久删除/清空废件箱要求 `UIDPLUS`。
- 搜索：≥3 字走 FTS5 trigram；1～2 字仅头部 `LIKE`；显示正文索引覆盖率。
- 缓存清理：保留摘要/标志/草稿，清空正文/原始邮件/附件与 FTS 正文列。
- TLS：统一 rustls；默认仅隐式 TLS / STARTTLS；明文需高级选项 + 风险确认。
- 凭证：优先系统密钥库；不可用时弹出二选一：仅本次会话记忆，或取消添加；不落盘明文。
- 磁盘保护：缓存目录剩余空间 < 500 MB 时暂停后台正文补全，并在顶部常驻提示。
- 单实例：第二个实例通过回环 IPC 请求已有窗口激活（`Focus`），不会重复打开数据库。
- 字体：内嵌 Noto Sans SC 子集（GB2312 + 界面字符，约 2.2 MB，OFL 1.1），无系统 CJK 字体也能正常显示；
  若系统存在 CJK 字体，则作为内嵌子集之外的缺字回退（繁体/生僻字）。

### v0.3：日历 / 天气 / AI / 音乐

- **日历**：多个本地日历；月 / 周 / 议程三视图；全天与定时事件；重复事件常用子集
  （`DAILY` / `WEEKLY` / `MONTHLY` / `YEARLY` + `COUNT` / `UNTIL`），时区存 UTC + IANA，
  展开由 `me-core` 纯函数完成（不支持的规则降级为「只显示首个实例 + 提示」，不丢弃原文）；
  最多 3 个提醒、提前量最多 7 天，应用运行时每 30 秒扫描一次并用
  `calendar_reminder_fires` 主键（事件 × 发生时刻 × 提前分钟数）去重（应用关闭时不提醒）。
  范围查询把「一次性事件」与「未结束的重复事件」拆成两条，避免大量历史重复事件挤掉当月视图。
- **天气**：和风天气（QWeather）。城市搜索（标题显示到县级市，同名层级只显示一次）、最多 8 个地点、当前天气 +
  24 小时折线 + 7 天预报、SQLite 快照缓存（离线可看）。刷新**按地点**为单位：每个地点各自
  单飞、60 秒最小间隔（手动刷新也遵守）、30s→2m→5m 退避、连续失败 3 次后停止自动重试；
  新增地点立即刷新它自己；**刷新失败保留旧快照**，界面显示缓存而不是空白。
  图标全部矢量自绘，未知天气画「?」而不是云；快照带版本号，版本不符按无缓存处理。
  配置弹窗只填 **API Host**（裸域名即可，程序补 `https://` 与端点路径）与 **API Key**（暗文），
  可先「测试连接」（发一次真实请求，不落库）。
- **AI 对话**：Anthropic Messages 与 OpenAI 兼容协议，流式输出、停止生成、重新生成、token 用量展示。
  支持多份模型档案（每份可绑定到不同对话）。工具白名单固定 14 个、权限为 9 个扁平开关
  （邮件 2 / 天气 / 日历 4 / 音乐 / 记忆），关掉的工具既不下发也会在执行期被拒。
  每次工具调用必须给出 `motive`，回复末尾按 `[结果]` 四段拆块；多步需求用任务清单
  （1–6 步，状态只前进，不计工具预算）。一轮最多 6 轮模型 / 12 次工具调用 / 300 秒。
  邮件检索结果以卡片展示命中理由，点开在当前页原地弹阅读窗；`calendar_delete` 只发确认、
  确认后 10 秒内可撤销；音乐四件套由界面执行并回执（3 秒超时）。正文只有用户主动「引用到 AI 对话」才进请求。
- **音乐**：选目录递归扫描（深度 ≤8、≤2000 文件、不跟随符号链接），MP3 / FLAC / WAV / OGG。
  左侧播放列表，主区上为专辑页（封面 + 标签 + 进度 + 上一首/播放暂停/下一首）、下为 48 柱实时频谱；
  正序 / 随机；切页不中断。音频在独立线程解码输出（状态 250ms 回传），不复制、不改写源文件；
  播放状态与频谱都不进 `Command`/`Event` 通道、也不落库。

## 已知缺口（相对架构文档）

1. HTML 渲染器 v2 仍是**邮件排版子集**，不是浏览器：不执行 JS、不加载外部 CSS/字体、不使用 WebView；
   `position`/`float`/`flex`/`grid` 按块级文档流降级；行内 SVG 不渲染（图片形式的 SVG 仍经 resvg 光栅化）；
   边框按整框近似（`border-top` 等单边样式会画成四边），复杂阴影、渐变与背景图不还原；
   非常规 CSS（变形、多列、定位）不做像素级还原。原始 HTML 保留在 `raw/*.eml.gz`。
2. 密送地址仅保存在本地 `drafts.bcc_addrs`，不写入 RFC 5322 原文；从服务器导入的草稿不含 Bcc，需重新填写。
3. 服务器无 `UIDPLUS` 时无法精确删除旧草稿版本，会保留 `\Deleted` 副本并提示（已在 UI 标注）。
4. OAuth2 登录入口已接入（回环重定向 + Microsoft 设备码 + 重新授权）；仓库不内置真实 `client_id`，
   需构建期注入或高级设置覆盖。真实 Gmail/Outlook 的 G0 验收尚未执行（缺少真实客户端与账号）。
5. 实时同步已实现后端、设置页开关与账户级覆盖；真实 Gmail/Outlook/QQ/163 的 IDLE 行为仍需人工验收。
6. Docker 端到端测试已提供（`scripts/e2e.sh` + GreenMail），但需要本机 Docker 守护进程，未纳入 CI。
7. 多实例激活只做窗口聚焦；不提供“在新标签页打开”等高级行为。
8. **v0.3 未交付 `.ics` 导入导出，也不支持从邮件的 `text/calendar` 附件一键导入**（架构文档 §1.2 曾列为目标项）。
   日历数据只在本机，跨设备迁移需等 v0.4。
9. **没有系统通知**：日历提醒只在应用运行时以应用内提示出现，窗口在后台时看不见（未接入 `Notifier`）。
10. **重复事件只能「改整个系列」**：`calendar_event_exceptions` 表与仓储已具备，但没有新建例外的界面。
11. **四个模块的功能开关未接线**：`meta.feature_calendar` / `meta.feature_ai` 只有常量、没有读取方，
    天气与音乐连键都没有；写进去不会改变行为。
12. **天气与 AI 的密钥以明文存在 `myemail.db` 的 `meta` 表里**（用户明确要求与其它参数一起保存）。
    读得到数据库就读得到 Key，也不再享受系统密钥库（DPAPI）的加密保护；补偿只有
    「不进日志/事件、界面暗文、`Debug` 脱敏」三条。升级到 v0.3 后旧 keyring 里的 Key 不再被读取，需重新粘贴一次。
13. **AI 会话列表固定拉取最近 100 条**，没有滚动分页；消息分页接口已在仓储层就绪。
14. 发布前的 6 项人工验收（真实 Gmail/Outlook 授权与 IDLE、真实和风 Key、真实 AI Key、
    真机音频、Docker E2E、非 1280×900 分辨率观感）尚未执行，逐项见 [doc/versiondoc/v03收尾.md](doc/versiondoc/v03收尾.md) §7。

## 依赖与工具链

- Rust edition 2024，MSRV 1.88；`rust-toolchain.toml` 固定 `stable`（本机 1.99.1）。
- 依赖版本见根 `Cargo.toml` 的 `[workspace.dependencies]`，锁定在 `Cargo.lock`。
- v0.2 新增：OAuth 使用 `reqwest`（rustls，仅 `me-proto`）/ `rand` / `sha2`；HTML v2 使用 `scraper`（html5ever/selectors）/ `cssparser` / `url`。不引入 WebView 或 JS 引擎。
- v0.3 新增：日历用 `chrono-tz`（IANA 时区）；音频用 `rodio` + `symphonia`（mp3/flac/wav/ogg）+ `walkdir`（扫描）；
  天气与 AI 复用 `reqwest`（rustls + gzip + stream，SSE 自行解析，不引入额外 HTTP/SSE 库）。
  仍不引入 WebView、JS 引擎与系统通知库。
- `deny.toml` 提供 cargo-deny 的许可证/漏洞/重复版本策略（禁 OpenSSL / native-tls）。
