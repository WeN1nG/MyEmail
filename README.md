<div align="center">

# MyEmail

**用 Rust 从零实现的多账户原生邮件客户端**

*自研 HTML 排版引擎 · 自研 SSE 流式解码 · 分层架构 · 710 个测试全绿*

[![Rust](https://img.shields.io/badge/rust-1.88%2B-000?logo=rust)](https://www.rust-lang.org/)
![License](https://img.shields.io/badge/license-MIT-blue.svg)
![Tests](https://img.shields.io/badge/tests-548%20passed-2ea66f)
![Deps](https://img.shields.io/badge/WebView-0%20%C2%B7%20JS%20engine-0-8b5cf6)

</div>

---

## 这是什么

一个**不依赖浏览器内核**的桌面邮件客户端。

大多数邮件客户端用 WebView 渲染正文——这意味着要背上一个 Chromium、承担 JS 引擎的攻击面、还要处理 Electron 式的内存开销。本项目反其道而行:用 **egui 立即模式 GUI** 直接绘制,自己实现 HTML 解析、CSS 匹配与排版引擎。

代价是工作量大,收益是完全掌控渲染结果:邮件正文有多少行文字、每个表格单元格在哪、字间距怎么算,全部可预测、可测试、零攻击面。

在此之上还集成了**日历 / 天气 / AI 对话 / 音乐播放器**四个模块——它们共用同一套分层、`Command`/`Event` 通信契约与安全边界,不是拼装在一起的独立应用。

<div align="center">

| 指标 | 数值 |
|:--|:--|
| 代码规模 | 57,500 行(不含测试) / 66,600 行(含测试) |
| 测试 | **710 passed / 0 failed / 6 ignored** |
| 测试文件 | 15 个集成测试 + 46 个内联测试模块 |
| 直接依赖 | 50 个 crates(`async-imap` / `lettre` / `rusqlite` / `rodio` / `scraper`…) |
| 外部服务 | 0(全部本地化,可完全离线使用) |
| WebView / JS 引擎 | **0** |

</div>

---

## 技术要点

### 1. 自研 HTML 排版引擎

**难点在于**:邮件不是网页。`<table>` 布局是 1998 年的遗产,真实邮件里满地都是;而通用 CSS 布局引擎会为了「正确」把一个表格拆成几十个匿名盒,丢掉邮件作者精心调好的像素。

**解法**:两步走。

```
parse.rs   HTML5 DOM(scraper)→ 清洗 → 扁平块模型
   ↓         后台线程 spawn_blocking,并发 2
css.rs     CSS 子集解析 → 选择器匹配 → specificity 排序
   ↓
layout.rs  按内容宽度测量出每个块/行/单元格的绝对矩形 → 一次性绘制
```

关键设计是**测量与绘制分离**。布局引擎先算出所有元素的绝对 `Rect`,再交给 painter 按文档序绘制:

- 一个段落 = **一个** `LayoutJob`(而不是每个行内片段一个控件)——断行、CJK、字体度量、文本选区全部交给 egui 的原生能力,但保持段落整体性
- 真实支持 `rowspan` / `colspan` 表格网格、`display:inline-block` 列容器、`max-width` 约束、相邻边距折叠
- `line-height` / `letter-spacing` / 逐行 `text-align` 精确还原
- 选择器优先级按 `(id, class/attr/pseudo, type)` 三元组近似计算

**性能**(release):930 KB 复杂邮件 ≈ **35 ms**,1000×5 表格 ≈ **13 ms**。

**安全边界**:不执行脚本、不加载外部 CSS/字体、不使用 WebView。`position`/`float`/`flex`/`grid` 按块级文档流降级。硬上限(节点 ≤50k、深度 ≤128、文本 ≤2MB)超出即回退到简化视图,绝不 panic。

### 2. 自研 SSE 流式解码

大模型 API 的流式响应用 Server-Sent Events。`reqwest` 能收字节流,但**不解析 SSE**——引入一个 SSE 库意味着多一个依赖、多一份信任。

`sse.rs` 用 267 行解决了这个问题,核心是一个反直觉的细节:

> **`\n` 是 ASCII 字符,不可能出现在 UTF-8 多字节序列中间。**

所以只要**按字节缓冲、遇到 `\n` 才解码**,就不会把一个中文字符从中间切开——哪怕 TCP 恰好在那个位置断了包。这个性质无法靠事后补救,必须在缓冲策略上做对。

同时实现了字节级总上限(4 MiB)与单行上限(256 KiB),防止服务端异常导致内存无限增长。

### 3. 分层架构:靠编译器保证边界

```
me-app ──Command/Event──► me-sync ──► me-proto
  │                          │  └────► me-store
  └──只读快照───────────────► me-store
所有 crate ──► me-core(唯一汇点)
```

硬规则不是靠自觉,而是靠**编译期就会报错**的设计:

- `me-app` **不能** `use` `async-imap` / `lettre` / `rusqlite` 的任何符号——写错就编译不过
- `me-proto` 与 `me-store` 互不依赖,只通过 `me-core` 的类型通信
- `me-core` 是**唯一汇点**,不依赖 tokio / rusqlite / 任何协议库——领域规则可脱离运行时单测
- 端口 trait 返回 `impl Future + Send` 而非 `async fn`:多线程 tokio 要求 future 为 `Send`,而 `async fn` in trait 无法表达该约束

**`me-app` 不依赖 `me-proto`**——AI 传输层在 `me-sync` 的 `start_native()` 里注入。这是分层设计带来的直接收益:换模型服务商时,UI 层一行代码都不用动。

### 4. AI 工具的两段式安全边界

让模型调用工具(查邮件、创建日程、播放音乐)最大的风险是**模型被诱导执行有害操作**。本项目用三层拦截:

| 层 | 机制 | 效果 |
|:--|:--|:--|
| 请求期 | 14 个工具白名单 + 9 个权限开关 | 关掉的工具**根本不下发**给模型 |
| 执行期 | 二次校验本轮工具集 | 模型返回未下发的工具 → **直接拒绝** |
| 重放期 | 正向列举可重放工具 | 「重新生成」只下发 9 个只读工具 |

最后一条的设计值得单独说:**用正向白名单而不是反向排除**。写成「排除已知写工具」的话,以后新增一个带副作用的工具会**默认被判成可重放**——而重新生成时它可能被真的执行。正向列举是失败安全的:忘了登记的工具默认不可重放。

还有几条不变量:

- 每个工具调用**必须**携带 `motive`(模型说明调用理由),缺失则降级执行而非重试
- `calendar_delete` **不直接删**,只发确认卡,等用户在 UI 点「确认删除」;确认后 10 秒内还能撤销
- 音乐工具在 UI 侧执行(模型进程碰不到音频设备),3 秒超时,超时按「工具失败」回报模型而非让整轮崩掉
- 邮件正文与工具结果一律视为**不可信资料**,`---` 分隔符会被转义,防止伪造分隔符注入

### 5. 并发治理:抢连接不如先让路

同步引擎面对一个经典问题:后台补全正在下载 500 封邮件的正文,用户此刻点了「发送」。

常见做法是给任务排优先级队列。本项目选择**抢占式取消**——高优先任务先 `cancel_backfill(account)` 停掉该账户的补全,再抢连接:

```
Governor: 全局许可 → 账户锁(两段获取,避免环等待)
预算分离: 操作连接 全局2/单账户1,监听连接 全局4/单账户1
退避:    30s → 2min → 5min
```

两套预算分开计费是必需的:IDLE 监听连接是长连接,如果不与操作连接分开,一封长邮件的同步就能把发送请求饿死。

### 6. 数据可迁移:从 v1 活到 v6

数据库 schema 走了 6 个版本。v4/v5 是**纯加列**,靠一个反直觉的判据保证幂等:

> **判断「某列在不在」,而不是判断「版本号到没到」。**

新建库的 DDL 天然带最新列,而从 v2 升级来的库刚跑完 v3 迁移时列的情况又不同——两条路径到达同一版本时状态不一致,用版本号判断必然出错。所以 `migrate_v3_to_v4` / `migrate_v4_to_v5` 都用 `pragma_table_info` 查列,缺了才 `ALTER TABLE`。

v6 换了个判据:它是**纯索引调整**(不改列),幂等靠「先 DROP 再 CREATE」——SQLite 没有 `ALTER INDEX`,改索引列序只能重建。

配套一条铁律:**中间步骤的版本号必须写死**,写 `SCHEMA_VERSION` 会给旧库盖上「已是最新」的章,把后续迁移整段跳过——这个坑代码里留了注释(`migrations.rs:560`)。

每步独立事务 + 末尾 `foreign_key_check`,失败即回滚、不写版本号、启动中止,**不静默降级**。

### 7. 出网请求的安全默认值

凡是会主动发出去的 HTTP 请求,都按「假设对方会攻击你」设计:

| | 防护 |
|:--|:--|
| **远程图片** | 仅 http/https、端口白名单(80/443/8080/8443)、拒本机/私网/CGNAT、**逐跳 DNS 与重定向校验**、不发 Cookie/Referer、单张 ≤8 MB、20s 超时 |
| **天气 API** | 禁止重定向(防止被引到内网)、Key 只走请求头(`X-QW-Api-Key`,URL 里绝不出现 `key=`) |
| **OAuth 令牌** | 禁止重定向、20s 超时、响应体 ≤64 KB、令牌类型 `Debug` 脱敏 |
| **模型 API** | 禁止重定向(防 Authorization 泄露到别的主机)、错误响应体不进入 `detail` |

「逐跳校验」是重点:只检查初始 URL 是不够的,攻击者可以给一个公网地址,让它 302 到 `http://127.0.0.1/admin`。

### 8. 原生音频:独立线程 + 零阻塞回调

频谱分析的难点是**实时音频回调线程绝不能阻塞**——它跑在 cpal 的回调里,一旦卡住就是爆音。

`spectrum.rs` 的做法:`SpectrumSink` 是唯一跨线程交接点,回调里只做「整块替换」,拿不到锁就**丢弃**这一帧。FFT 全部在 UI 线程做,回调线程永不等待。

播放状态 250ms 回传一次 UI,频谱与播放状态**都不进 `Command`/`Event` 通道**,也不落库——音频是高频瞬时数据,走持久化通道是设计错误。

---

## 功能

<div align="center">

<table>
<tr><td width="50%" valign="top"><b>邮件</b></td><td width="50%" valign="top"><b>日历</b></td></tr>
<tr><td valign="top">

- 多账户 IMAP 收信 / SMTP 发信
- 全量摘要入库 + 正文文件缓存,断网仍可读
- FTS5 trigram 全文检索(≥3 字命中正文)
- 速记语法:`from:` `to:` `subject:` `has:` `is:unread` `before:` `after:`
- 保存的搜索:条件存本地,一键重跑
- 批量多选(Ctrl / Shift):已读、加星、删除
- 统一收件箱:各账户收件箱的并集,带来源账户列
- 会话线程:按 `References` 折叠,可展开成对话
- 联系人自动补全:发信时累积,只存本地
- IDLE 实时推送,自动降级轮询
- 草稿双向同步,冲突保留两份不静默覆盖
- 富文本 / 纯文本 / Markdown 三视图
- 远程图片自动加载(带完整 SSRF 防护)
- OAuth2 回环 + Microsoft 设备码

</td><td valign="top">

- 月 / 周 / 议程三视图
- 重复规则展开(纯函数,UI 不自己解析)
- 多个本地日历
- 最多 3 个提醒,应用内去重
- 与 AI 对话打通(模型可查/建日程)
- 删除需确认,10 秒内可撤销

</td></tr>
<tr><td valign="top"><b>AI 对话</b></td><td valign="top"><b>音乐 & 天气</b></td></tr>
<tr><td valign="top">

- Anthropic / OpenAI 兼容双协议
- 流式输出 / 停止 / 重新生成
- 多模型档案,每份可绑定不同对话
- 14 个工具 + 9 个权限开关
- 1–6 步任务清单,状态只前进
- 邮件命中卡片 + 原地阅读窗

</td><td valign="top">

- 目录递归扫描,48 柱实时频谱
- mp3 / flac / wav / ogg
- 标签与内嵌封面读取
- 播放顺序 / 随机,切页不中断
- 天气:城市搜索 / 24h / 7 天
- 按地点独立刷新与退避

</td></tr>
</table>

</div>

---

## 工程实践

**测试**:710 个测试全部通过。分层架构的直接收益是测试不需要真实服务器——`me-core` 的领域规则是纯函数,毫秒级跑完;`me-proto` 用本机 mock IMAP 测协议时序;`me-sync` 注入 mock provider 测端到端编排。

```bash
cargo test --workspace        # 710 passed / 6 ignored
```

**质量门禁**:`cargo fmt` / `cargo clippy -D warnings` / `cargo deny check`(许可证白名单 + 禁 OpenSSL / native-tls)全部通过,CI 只在 `windows-latest` 上跑这四道。

**发布安全**:打包脚本会做**信息泄露自检**——扫描 exe 与源码包里是否残留构建机绝对路径、本机标识、凭据形态、真实数据库内容,命中即阻塞发布。release 构建带 `--remap-path-prefix`,否则 exe 里会留下构建机的完整路径。

**性能基准**:HTML 解析在 release 下跑 930 KB 真实邮件与 1000×5 表格,作为 `#[ignore]` 测试保留,防止回归。

---

## 技术栈

| 领域 | 选型 |
|:--|:--|
| 语言 | Rust 2024 edition,MSRV 1.88 |
| GUI | `eframe` / `egui`(立即模式,无 WebView) |
| 异步 | `tokio`(multi-thread,4 worker) |
| 协议 | `async-imap` / `lettre` / `mail-parser`(full_encoding) |
| 存储 | `rusqlite`(bundled)+ `r2d2` 读池 / FTS5 trigram |
| 渲染 | `scraper`(html5ever) / `cssparser` / 自研布局引擎 |
| 图形 | `image` / `resvg`(SVG 栅格化) |
| 音频 | `rodio` / `symphonia` / `walkdir` |
| 网络 | `reqwest`(rustls,仅 `me-proto`) |
| TLS | `rustls`(**不用** OpenSSL / native-tls) |
| 凭证 | `keyring`(系统密钥库) |

---

## 已知边界

诚实地说明这个项目**不做**什么:

- HTML 渲染是**邮件排版子集**,不是浏览器。不执行 JS、不加载外部 CSS/字体;`position`/`float`/`flex`/`grid` 按块级文档流降级;边框按整框近似
- 无 `.ics` 导入导出,跨设备迁移靠数据库文件
- 会话线程只覆盖**新邮件**和**本地有原始报文的旧邮件**(正文补全时才落 `raw/`),没打开过的历史邮件单独成串;服务器重拉尚未做
- 无系统通知,日历提醒只在应用运行时可见
- 日历重复事件只能改整个系列,尚无「仅此次」编辑界面
- AI 会话列表固定拉取最近 100 条,暂无滚动分页
- AI / 天气密钥以明文存本地库(用户明确要求与其它参数一致保存),补偿手段是「不进日志、界面暗文、`Debug` 脱敏」
- 真实 Gmail/Outlook 授权、真机音频、Docker E2E 尚待人工验收

---

## 构建

```bash
git clone <repo>
cd MyEmail
cargo build --release          # 产物 target/release/myemail.exe
```

需要 Rust 1.88+。Windows 下直接运行即可,无需额外运行时。

**OAuth 客户端**(可选,构建期注入,密钥不进仓库):

```bash
MYEMAIL_GOOGLE_CLIENT_ID=xxx MYEMAIL_MICROSOFT_CLIENT_ID=xxx cargo build --release
```

数据目录:`%APPDATA%\MyEmail\`(其他平台走 `dirs::data_dir`)。

---

## 文档

| 文档 | 内容 |
|:--|:--|
| [doc/项目架构.md](doc/项目架构.md) | 分层、依赖方向、线程模型、通信契约 |
| [doc/数据结构.md](doc/数据结构.md) | DDL 唯一事实源、迁移链、索引与查询模式 |
| [doc/项目大纲.md](doc/项目大纲.md) | 目标、需求、选型理由、决策记录 |
| [doc/技术栈选用.md](doc/技术栈选用.md) | 逐项选型与**不引入**清单 |
| [doc/可行性分析.md](doc/可行性分析.md) | 风险登记与 Spike 门禁 |
| [doc/AI模块设计.md](doc/AI模块设计.md) | AI 工具契约与安全边界设计 |
| [CLAUDE.md](CLAUDE.md) | 面向 AI 助手的分层规则与行为不变量 |

---

<div align="center">

**MIT License** · 用 Rust 写成,一行 WebView 都没有

</div>
