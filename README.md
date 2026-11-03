# MyEmail

多账户统一收发 + 本地全量缓存 + 离线全文检索的原生桌面邮件客户端（Rust + egui）。

当前实现对应 **v0.2（0.2.0）**：在 v0.1 基础上增加 OAuth2 登录、IDLE/轮询实时同步、草稿双向同步与 HTML 渲染器 v2；数据库 schema 为 v2。

> 发布前外部验收（真实 Gmail/Outlook 授权、Docker GreenMail E2E）尚未执行；详见「已知缺口」。

## 快速开始

```bash
# 构建
cargo build

# 运行（Windows 下生成 target/debug/myemail.exe）
cargo run -p me-app

# 全量测试
cargo test --workspace

# 真实 IMAP/SMTP 端到端测试（需 Docker Desktop 运行）
scripts/e2e.sh
```

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
| `me-core` | 领域模型、端口 trait、`Command`/`Event`、错误；不依赖 tokio/rusqlite |
| `me-proto` | IMAP/SMTP 适配、MIME 解析与构建、modified UTF-7 |
| `me-store` | SQLite 仓储、FTS5(trigram)、文件缓存、schema 初始化 |
| `me-sync` | 同步引擎、正文补全、连接预算、取消/退避、事件发送 |
| `me-app` | eframe/egui 界面、视图、输入、文件对话框 |

依赖方向与硬规则见 [doc/项目架构.md](doc/项目架构.md) §2。

## 测试

| 位置 | 内容 |
| --- | --- |
| `crates/me-core/src/*.rs` 内联 | 标志位、snippet、日期回退、附件上限、事件分类 |
| `crates/me-store/tests/store_schema.rs` | schema、外键、FTS 增删改、去重、分页、标志合并、缓存不变量 |
| `crates/me-store/tests/migration_v1_v2.rs` | v1→v2 迁移、备份、Drafts 清理、外键检查、失败回滚 |
| `crates/me-store/tests/draft_sync_store.rs` | Bcc、同步状态、服务器身份、墓碑、冲突副本 |
| `crates/me-proto/tests/proto_mock.rs` | mock IMAP：CAPABILITY/SELECT/LIST、标志分页、`BODY.PEEK[]`、MOVE/COPYUID、IDLE |
| `crates/me-proto/tests/oauth_mock.rs` | mock OAuth：授权码交换、刷新、设备码三态、`invalid_grant`、响应上限 |
| `crates/me-proto/tests/mime_fixtures.rs` | `.eml` 夹具：纯文本、multipart 附件、仅 HTML、中文主题、缺失 Date、畸形输入 |
| `crates/me-sync/tests/sync_mock.rs` | 真实 store + mock IMAP 的同步链路、去重与 IDLE 触发同步 |
| `crates/me-sync/tests/draft_sync_mock.rs` | 草稿推送、服务器导入、冲突保留两份、墓碑删除 |
| `crates/me-sync/tests/oauth_flow_mock.rs` | 回环授权回调、设备码 pending→token、账户令牌更新 |
| `crates/me-app/src/views/html/tests.rs` | HTML 清洗、CSS 子集、inline-block 结构、行高/对齐、表格跨度与行高、宽度约束、链接/图片白名单、超限回退 |
| `crates/me-app/src/*` 内联 | 地址解析、地址校验、HTML 表格解析、SVG 识别 |
| `crates/me-sync/tests/e2e_real_imap.rs` | 真实 GreenMail 容器：SMTP 投递 → IMAP 同步 → 正文补全（默认忽略） |

> 架构文档中的 `tests/` 是跨 crate 测试的规划位置；Cargo 集成测试必须位于各 crate 下，因此实际落在
> `crates/*/tests/`，文件名保持 `store_schema.rs` / `proto_mock.rs`。

## 已实现的行为要点

- 账户（v0.2）：密码/授权码；Google / Microsoft OAuth2 回环登录，Microsoft 另提供设备码；
  OAuth 账户可“重新授权”；刷新令牌存系统密钥库（或会话内存），访问令牌只在内存；授权回调只绑定 `127.0.0.1:0`。
- 同步：启动 + 手动刷新；`UIDVALIDITY` 变化重建；`UIDNEXT <= last_uid + 1` 判空；
  分页头部 `FETCH` + `BODYSTRUCTURE`；分页标志回拉；本地待同步标志优先。
- 实时同步（v0.2）：每账户 IDLE 只监听 INBOX，29 分钟重新 IDLE；事件去抖 2 秒后触发同步；
  服务器不支持 IDLE 或连续失败 3 次时自动降级轮询（默认 300 秒，±10% 抖动，可在设置页关闭或调整）。
  监听连接与操作连接独立计费（全局 4 / 单账户 1），手动同步优先于实时同步。
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
  解析在后台线程完成，UI 只绘制扁平块；超限或异常时自动回退到简化视图，旧解析结果按 generation 丢弃。
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

## 依赖与工具链

- Rust edition 2024，MSRV 1.88；`rust-toolchain.toml` 固定 `stable`（本机 1.99.1）。
- 依赖版本见根 `Cargo.toml` 的 `[workspace.dependencies]`，锁定在 `Cargo.lock`。
- v0.2 新增：OAuth 使用 `reqwest`（rustls，仅 `me-proto`）/ `rand` / `sha2`；HTML v2 使用 `scraper`（html5ever/selectors）/ `cssparser` / `url`。不引入 WebView 或 JS 引擎。
- `deny.toml` 提供 cargo-deny 的许可证/漏洞/重复版本策略。