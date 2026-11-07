//! AI 工具（邮件/天气/日历/记忆/清单）的**薄壳**：参数转换、限额、超时、结果映射。
//!
//! 硬规则（`doc/v03架构设计.md` §2.7 门禁）：
//! - 不许出现 `*Repo` 的写方法、`*Provider` 的调用，也不许自写 SQL/FTS；
//! - 一律调用 `me-sync` 的用例函数（`create_event` / `refresh_weather` /
//!   `search_mail` / `upsert_ai_entity`）或 `me-store::search` 的共享查询。
//!
//! 工具结果只给模型**必要摘要**：不返回完整正文、附件、原始 JSON。

use std::collections::HashMap;
use std::future::Future;
use std::time::{Duration, Instant};

use me_core::ai::{
    AiEntityKind, AiEntitySource, AiToolCall, AiToolName, AiToolPermissions, AiToolResult,
    AiToolStatus, AiToolTrace, MAX_BODY_CHARS, MAX_TOOL_RESULT_CHARS, MotiveSource, NewAiEntity,
    tool_motive, truncate_chars,
};
use me_core::ai_plan::{
    Capability, OnEmpty, Plan, PlanStatus, PlanStep, StepKind, validate_plan, validate_plan_v2,
};
use me_core::calendar::{CalendarEventDraft, CalendarSeries};
use me_core::model::{FolderId, MessageId, MessageSummary, now_unix};
use me_core::music::{MusicAction, MusicOp};
use me_core::port::StorePort;
use me_core::{CoreError, Event, Flags, SearchQuery};
use serde_json::Value;

use crate::SyncService;
use crate::calendar::Actor;

/// 每条用户消息内的工具次数预算 + 本次请求的权限快照。
#[derive(Debug, Default)]
pub(crate) struct ToolBudget {
    pub weather: u32,
    pub calendar: u32,
    pub calendar_read: u32,
    pub mail_read: u32,
    pub memory: u32,
    /// 本轮**不可重试**失败的调用指纹 → 错误摘要。
    ///
    /// 只记 `!err.is_retryable()` 的失败：网络抖动、超时本来就该重试，
    /// 而参数/权限类错误，参数不变结果就不会变。`Validation` 重发一次是空转，
    /// 重发八次就是那次「有什么重要的邮件」的真实事故（8 次调用里 6 次同一报错）。
    pub failed: HashMap<String, String>,
    /// 执行期也要按它拦一次：模型可能返回一个本轮**没下发**的工具名，
    /// 而在协议层这类名字仍然合法（白名单固定）。
    pub permissions: AiToolPermissions,
    /// 本次请求里用户**已经引用**的邮件 id。
    ///
    /// `mail_read` 命中就短路：正文已经在上下文里了，再读一遍既费 token
    /// 又白吃一次工具预算。只存 id 而不是整份 `AiContextRef`——后者带正文快照，
    /// 每轮克隆几千字符没有任何收益。
    pub context_refs: Vec<MessageId>,
}

/// 「工具 + 业务参数」的指纹。
///
/// 剔除 `motive` 与 `step_id`：它们只说明「为什么调」，不改变「查什么」。
/// 模型换个说法重发同一份参数时 `motive` 会变，但语义完全一样——那正是要拦下的情况。
///
/// `serde_json::Value` 的对象在未启用 `preserve_order` 时是 `BTreeMap`，
/// 键天然有序，所以同一份参数永远得到同一个字符串。
fn attempt_fingerprint(call: &AiToolCall) -> String {
    // 指纹实现放在 `me-core`：计划期的「历史失败指纹」与执行期的「同参数熔断」
    // 必须是同一个函数，否则两处判断会对不上。
    me_core::ai::call_fingerprint(call.name, Some(&call.arguments))
}

impl ToolBudget {
    /// 记下这次不可重试的失败。同一指纹只留第一条错误，后面重复的没有新信息。
    pub(crate) fn remember_failure(&mut self, call: &AiToolCall, error: &str) {
        self.failed
            .entry(attempt_fingerprint(call))
            .or_insert_with(|| truncate_chars(error, 120));
    }

    /// 这次调用的参数是否已经失败过。返回上次的错误摘要。
    pub(crate) fn previous_failure(&self, call: &AiToolCall) -> Option<&str> {
        self.failed
            .get(&attempt_fingerprint(call))
            .map(String::as_str)
    }
}

/// 同参数已失败过的调用：**不执行**，直接告诉模型「换参数或放弃」。
///
/// 界面上仍然发一对 Started/Completed 事件：用户要能看到「这次被拦下了」，
/// 否则活动条会莫名其妙少一条，看起来像工具没被调用。
pub(crate) fn intercepted_result(
    call: &AiToolCall,
    motive: Option<String>,
    motive_source: MotiveSource,
    previous: &str,
) -> AiToolResult {
    AiToolResult {
        call_id: call.id.clone(),
        tool: call.name,
        content: format!(
            "这次没有执行：你刚用完全相同的参数调用过，返回的是同一个错误「{previous}」。\
必须改变参数（换关键词、放宽时间窗、去掉某个限定）或放弃这一步；\
原样重发不会得到不同结果。"
        ),
        is_error: true,
        trace: AiToolTrace {
            call_id: call.id.clone(),
            tool: call.name,
            status: AiToolStatus::Error,
            summary: "相同参数已失败过，未执行".to_string(),
            duration_ms: 0,
            motive,
            motive_source,
            step_id: call_step_id(call),
            attempt: None,
        },
    }
}

/// 模型在参数里声明的步骤号。非法值按「没写」处理——绑定失败不该拒绝执行。
fn call_step_id(call: &AiToolCall) -> Option<u8> {
    call.arguments
        .get("step_id")
        .and_then(Value::as_u64)
        .and_then(|value| u8::try_from(value).ok())
        .filter(|value| *value >= 1)
}

const CALENDAR_LIMIT: u32 = 5;
const CALENDAR_READ_LIMIT: u32 = 5;
const MEMORY_LIMIT: u32 = 5;
const WEATHER_LIMIT: u32 = 1;
/// 同一封邮件在一次回复里最多读几次正文。
const MAIL_READ_LIMIT: u32 = 3;

/// 单工具超时：本地查询 2s、联网天气 20s、本地写入 5s。
const EMAIL_SEARCH_TIMEOUT: Duration = Duration::from_secs(2);
const WEATHER_TIMEOUT: Duration = Duration::from_secs(20);
const CALENDAR_TIMEOUT: Duration = Duration::from_secs(5);
const MEMORY_TIMEOUT: Duration = Duration::from_secs(5);
const MAIL_READ_TIMEOUT: Duration = Duration::from_secs(3);

/// 给一次工具执行套超时。超时按「工具错误结果」回报给模型（不是整轮失败），
/// 模型可以换个说法或直接告诉用户稍后再试。
///
/// 用 `validation` 而不是 `ai(Transient)`：后者的 `user_message` 是固定的
/// 「服务暂时不可用」，会把「哪个工具超时、等了几秒」这条信息吃掉。
async fn with_timeout<F>(
    limit: Duration,
    label: &str,
    fut: F,
) -> Result<(String, String), CoreError>
where
    F: Future<Output = Result<(String, String), CoreError>>,
{
    match tokio::time::timeout(limit, fut).await {
        Ok(result) => result,
        Err(_) => Err(CoreError::validation(format!(
            "{label}超时（{} 秒），可稍后重试",
            limit.as_secs()
        ))),
    }
}

fn arg_str(args: &Value, key: &str, max: usize) -> Option<String> {
    let value = args.get(key)?.as_str()?.trim();
    if value.is_empty() {
        return None;
    }
    Some(truncate_chars(value, max))
}

fn arg_u32(args: &Value, key: &str) -> Option<u32> {
    args.get(key)?.as_u64().map(|value| value as u32)
}

fn arg_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key)?.as_bool()
}

fn arg_date(args: &Value, key: &str) -> Option<i64> {
    let raw = args.get(key)?.as_str()?;
    let date = chrono::NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d").ok()?;
    Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp())
}

/// 解析日程时间：带时区的 RFC 3339，或全天事件的 `YYYY-MM-DD`。
fn parse_when(raw: &str, all_day: bool) -> Option<i64> {
    let raw = raw.trim();
    if all_day && raw.len() == 10 {
        let date = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()?;
        return Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp());
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(parsed.timestamp());
    }
    // 允许 `2026-11-06T15:00:00+08:00` 之外的常见写法：带空格但有时区。
    if let Ok(parsed) = chrono::DateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S%:z") {
        return Some(parsed.timestamp());
    }
    None
}

impl<S, F, M> SyncService<S, F, M>
where
    S: StorePort + Clone,
    F: me_core::port::ImapSessionFactory + 'static,
    M: me_core::port::SmtpSender + 'static,
{
    /// 执行一次工具调用。任何错误都变成 `is_error` 的工具结果交给模型解释。
    /// 执行一次工具调用。
    ///
    /// `plan` 是**本轮产物**而不是配额（配额在 [`ToolBudget`] 里）：
    /// 只有 `plan_update` 会改它，其余工具只读。
    pub(crate) async fn execute_ai_tool(
        &self,
        conversation: me_core::ai::AiConversationId,
        call: &AiToolCall,
        budget: &mut ToolBudget,
        plan: &mut Option<Plan>,
    ) -> AiToolResult {
        let started = Instant::now();
        // 动机缺失**不拒绝执行**：协议里没有「重试同一个 tool_call」，让模型补一次
        // 意味着一枚新的 tool_use，白白吃掉 1/8 的调用预算，还会在 trace 里留下
        // 「一错一对」两条记录。它是给人看的说明，不是安全边界。
        let (motive, motive_source) = tool_motive(&call.arguments);
        let outcome = if !budget.permissions.enabled(call.name) {
            // 本轮没下发这个工具，模型却调用了它：不执行，让它自己向用户解释。
            Err(CoreError::validation(format!(
                "工具「{}」已关闭，无法使用",
                call.name.label()
            )))
        } else {
            match call.name {
                AiToolName::EmailSearch => {
                    with_timeout(
                        EMAIL_SEARCH_TIMEOUT,
                        "邮件检索",
                        self.tool_email_search(conversation, call),
                    )
                    .await
                }
                AiToolName::MailRead => {
                    if budget.mail_read >= MAIL_READ_LIMIT {
                        Err(CoreError::validation(
                            "本次回复已经读过 3 次正文，请直接用已有信息回答",
                        ))
                    } else {
                        budget.mail_read += 1;
                        let refs = budget.context_refs.clone();
                        with_timeout(
                            MAIL_READ_TIMEOUT,
                            "读取正文",
                            self.tool_mail_read(call, &refs),
                        )
                        .await
                    }
                }
                AiToolName::WeatherLocations => {
                    with_timeout(
                        EMAIL_SEARCH_TIMEOUT,
                        "读取地点列表",
                        self.tool_weather_locations(),
                    )
                    .await
                }
                AiToolName::CalendarList => {
                    if budget.calendar_read >= CALENDAR_READ_LIMIT {
                        Err(CoreError::validation(
                            "本次消息查看日程的次数已达上限（5 次）",
                        ))
                    } else {
                        budget.calendar_read += 1;
                        with_timeout(CALENDAR_TIMEOUT, "查看日程", self.tool_calendar_list(call))
                            .await
                    }
                }
                AiToolName::CalendarUpdate => {
                    if budget.calendar >= CALENDAR_LIMIT {
                        Err(CoreError::validation("本次消息修改的日程已达上限（5 条）"))
                    } else {
                        budget.calendar += 1;
                        with_timeout(
                            CALENDAR_TIMEOUT,
                            "修改日程",
                            self.tool_calendar_update(conversation, call),
                        )
                        .await
                    }
                }
                AiToolName::CalendarDelete => {
                    if budget.calendar >= CALENDAR_LIMIT {
                        Err(CoreError::validation("本次消息发起的删除已达上限（5 条）"))
                    } else {
                        budget.calendar += 1;
                        with_timeout(
                            CALENDAR_TIMEOUT,
                            "删除日程",
                            self.tool_calendar_delete(conversation, call),
                        )
                        .await
                    }
                }
                AiToolName::WeatherGet => {
                    if budget.weather >= WEATHER_LIMIT {
                        Err(CoreError::validation("本次消息已查询过天气"))
                    } else {
                        budget.weather += 1;
                        let memory = budget.permissions.memory;
                        with_timeout(
                            WEATHER_TIMEOUT,
                            "天气查询",
                            self.tool_weather_get(conversation, call, memory),
                        )
                        .await
                    }
                }
                AiToolName::CalendarCreate => {
                    if budget.calendar >= CALENDAR_LIMIT {
                        Err(CoreError::validation("本次消息创建的日程已达上限（5 条）"))
                    } else {
                        budget.calendar += 1;
                        let memory = budget.permissions.memory;
                        with_timeout(
                            CALENDAR_TIMEOUT,
                            "创建日程",
                            self.tool_calendar_create(conversation, call, memory),
                        )
                        .await
                    }
                }
                AiToolName::MemoryUpsert => {
                    if budget.memory >= MEMORY_LIMIT {
                        Err(CoreError::validation("本次消息写入的记忆已达上限（5 条）"))
                    } else {
                        budget.memory += 1;
                        with_timeout(
                            MEMORY_TIMEOUT,
                            "写入记忆",
                            self.tool_memory_upsert(conversation, call),
                        )
                        .await
                    }
                }
                // 音乐四个工具都由 UI 侧执行（播放器在那边），这里只负责转达与等待。
                // 超时在 `run_music_op` 内部处理，不再套 `with_timeout`（会叠成 3s+3s）。
                AiToolName::MusicSearch => {
                    let outcome = arg_str(&call.arguments, "query", 100)
                        .ok_or_else(|| CoreError::validation("缺少 query"));
                    match outcome {
                        Ok(query) => {
                            self.run_music_op(conversation, &call.id, MusicOp::Search { query })
                                .await
                        }
                        Err(err) => Err(err),
                    }
                }
                AiToolName::MusicPlay => {
                    let query = arg_str(&call.arguments, "query", 100);
                    // 序号从 1 开始（与界面列表一致），内部转成 0 基。
                    let index = arg_u32(&call.arguments, "index").map(|value| value as usize);
                    self.run_music_op(conversation, &call.id, MusicOp::Play { query, index })
                        .await
                }
                AiToolName::MusicState => {
                    self.run_music_op(conversation, &call.id, MusicOp::State)
                        .await
                }
                AiToolName::MusicControl => {
                    let raw = arg_str(&call.arguments, "action", 20)
                        .ok_or_else(|| CoreError::validation("缺少 action"));
                    match raw {
                        Ok(raw) => match MusicAction::parse(&raw) {
                            Some(action) => {
                                self.run_music_op(
                                    conversation,
                                    &call.id,
                                    MusicOp::Control { action },
                                )
                                .await
                            }
                            None => Err(CoreError::validation(format!(
                                "不支持的控制动作「{raw}」，可用：play/pause/next/prev/stop"
                            ))),
                        },
                        Err(err) => Err(err),
                    }
                }
                // 清单是纯内存操作，没有 IO，不套超时；也不占工具次数预算
                // （由调用方用 `is_bookkeeping()` 判定）。
                AiToolName::PlanUpdate => self.tool_plan_update(call, plan),
                // 建计划同样纯内存。校验放在这里（而不是引擎里）：模型收到的
                // `is_error` 就是它需要的「哪一步哪里不对」，改完重交即可。
                AiToolName::PlanSubmit => self.tool_plan_submit(call, &budget.permissions, plan),
                AiToolName::AnswerDirectly => Self::tool_answer_directly(call),
            }
        };

        let duration_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
        match outcome {
            Ok((content, summary)) => AiToolResult {
                call_id: call.id.clone(),
                tool: call.name,
                content: truncate_chars(&content, MAX_TOOL_RESULT_CHARS),
                is_error: false,
                trace: AiToolTrace {
                    call_id: call.id.clone(),
                    tool: call.name,
                    status: AiToolStatus::Ok,
                    summary,
                    duration_ms,
                    motive: motive.clone(),
                    motive_source,
                    step_id: call_step_id(call),
                    attempt: None,
                },
            },
            Err(err) => {
                // 记住**不可重试**的失败：同参数再调一次结果不会变。
                // 可重试的（网络、超时）不记，否则真的临时故障会被误伤成永久熔断。
                let message = err.user_message();
                if !err.is_retryable() {
                    budget.remember_failure(call, &message);
                }
                AiToolResult {
                    call_id: call.id.clone(),
                    tool: call.name,
                    content: format!("工具执行失败：{message}"),
                    is_error: true,
                    trace: AiToolTrace {
                        call_id: call.id.clone(),
                        tool: call.name,
                        status: AiToolStatus::Error,
                        summary: message,
                        duration_ms,
                        motive: motive.clone(),
                        motive_source,
                        step_id: call_step_id(call),
                        attempt: None,
                    },
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // email_search
    // -----------------------------------------------------------------------

    async fn tool_email_search(
        &self,
        conversation: me_core::ai::AiConversationId,
        call: &AiToolCall,
    ) -> Result<(String, String), CoreError> {
        if !call.parse_ok {
            return Err(CoreError::validation("参数不是合法 JSON"));
        }
        let args = &call.arguments;
        let query_text = arg_str(args, "query", 200);
        let subject = arg_str(args, "subject", 200);
        let to = arg_str(args, "to", 320);
        let mut from = arg_str(args, "from", 320);
        let folder_arg = arg_str(args, "folder", 200);
        let unread_only = arg_bool(args, "unread_only").unwrap_or(false);
        let flagged_only = arg_bool(args, "flagged_only").unwrap_or(false);
        // `days` 优先于 `after`：模型算日期常出错，让它直接说「最近 7 天」更可靠。
        // 两者同时给时**静默以 days 为准**，不报「参数冲突」——报冲突只会让模型
        // 换个说法再来一次，正是当初 8 连击的成因之一。
        let days = arg_u32(args, "days").map(|value| value.clamp(1, 365));
        let since = match days {
            Some(days) => Some(now_unix() - i64::from(days) * 86_400),
            None => arg_date(args, "after"),
        };
        let until = arg_date(args, "before");
        // 条件判定必须与 schema 里列出的条件**完全一致**。早期只认四个文本条件，
        // 模型写「近 7 天未读」（`unread_only` + `after`，schema 允许）必然拿到
        // 「至少需要一个检索条件」，它不知道缺的是哪一类，只能换说法反复重试。
        if query_text.is_none()
            && from.is_none()
            && subject.is_none()
            && to.is_none()
            && since.is_none()
            && until.is_none()
            && folder_arg.is_none()
            && !unread_only
            && !flagged_only
        {
            return Err(CoreError::validation(
                "至少需要一个检索条件：query / from / to / subject / after / before / \
days / folder / unread_only / flagged_only",
            ));
        }
        // `from` 允许写人名：先按实体注册表换成邮箱；换不到就按原文做子串匹配
        // （`from_addr` 里存的是 `Name <addr>`，姓名本身也能命中）。
        if let Some(raw) = from.clone() {
            from = Some(self.resolve_from_filter(raw).await);
        }
        let limit = arg_u32(args, "limit").unwrap_or(5).clamp(1, 20);

        let folders = if let Some(name) = folder_arg {
            let ids = self
                .db(move |s| {
                    let mut ids = Vec::new();
                    for account in s.list_accounts()? {
                        for folder in s.list_folders(account.id)? {
                            if folder.name.eq_ignore_ascii_case(&name) {
                                ids.push(folder.id);
                            }
                        }
                    }
                    Ok(ids)
                })
                .await?;
            if ids.is_empty() {
                return Err(CoreError::validation("没有找到这个文件夹"));
            }
            ids
        } else {
            Vec::new()
        };

        // 多个条件**取交集**：全文走 FTS 短语，头部字段与未读在 SQL 层 AND。
        // 之前「多于一个条件就报错」的做法会让模型多绕一轮，用户看到的是
        // 「AI 说它一次只能搜一个词」。
        // 卡片上的「命中理由」要在把条件 move 进 SearchQuery 之前留一份。
        let reason_text = query_text.clone();
        let reason_subject = subject.clone();
        let reason_from = from.clone();
        let query = SearchQuery {
            text: query_text.unwrap_or_default(),
            accounts: Vec::new(),
            folders,
            since,
            until,
            has_attachment: None,
            from,
            to,
            subject,
            unread_only,
            flagged_only,
            limit,
            cursor: None,
        };
        let outcome = self.search_mail(query, 20).await?;
        let messages = outcome.messages;
        let folder_names = self.folder_names().await;

        self.emit(Event::AiToolEmailResults {
            conversation,
            call_id: call.id.clone(),
            cards: messages
                .iter()
                .map(|message| me_core::MailCardRef {
                    message: message.id,
                    reason: card_reason(
                        message,
                        reason_subject.as_deref(),
                        reason_from.as_deref(),
                        reason_text.as_deref(),
                    ),
                })
                .collect(),
            total: outcome.total,
        })
        .await;

        if messages.is_empty() {
            return Ok((
                "本地缓存中 0 命中。可尝试：① 放宽时间窗（例如最近 30 天）；\
② 去掉 flagged_only / unread_only 这类限定；③ 换发件人或主题关键词；\
④ 一封都没有时提示用户先同步最近的邮件。\
**不要用完全相同的参数再查一次**——同样的条件只会得到同样的结果。"
                    .to_string(),
                "未找到（0 命中）".to_string(),
            ));
        }

        let mut text = format!(
            "本地缓存中检索到 {} 封（最多显示 {} 封）：\n",
            outcome.total,
            messages.len()
        );
        for (index, message) in messages.iter().enumerate() {
            text.push_str(&format!(
                "{}\n",
                format_message(
                    index + 1,
                    message,
                    folder_names.get(&message.folder).cloned()
                )
            ));
        }
        Ok((text, format!("检索到 {} 封", messages.len())))
    }

    // -----------------------------------------------------------------------
    // mail_read
    // -----------------------------------------------------------------------

    /// 读**已缓存**的正文。不下载、不标已读——标已读是「点开」的副作用，
    /// 让一次 AI 检索顺手把用户的未读清掉是不可接受的。
    async fn tool_mail_read(
        &self,
        call: &AiToolCall,
        context_refs: &[MessageId],
    ) -> Result<(String, String), CoreError> {
        if !call.parse_ok {
            return Err(CoreError::validation("参数不是合法 JSON"));
        }
        let raw = call
            .arguments
            .get("message_id")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| CoreError::validation("缺少 message_id"))?;
        let id = MessageId(raw as i64);

        // 用户提问时已经引用了它：正文就在上下文里，再读一遍纯属浪费。
        if context_refs.contains(&id) {
            return Ok((
                "这封邮件的正文已经在上下文中（用户在提问时引用了它），不需要再读一次。"
                    .to_string(),
                "正文已在上下文".to_string(),
            ));
        }

        let summary = self
            .db(move |s| s.get_message(id))
            .await?
            .ok_or_else(|| CoreError::validation("本地没有找到这封邮件，请先重新检索"))?;

        if !summary.body_cached {
            // 这里**不是** `is_error`：模型不需要重试，只需要换一句话告诉用户。
            return Ok((
                format!(
                    "「{}」的正文还没有缓存到本地。请让用户先在邮件列表里打开这封邮件\
（打开后会自动缓存正文），然后再让我读。不要反复重试，也不要根据主题猜正文。",
                    truncate_chars(summary.subject.as_deref().unwrap_or("(无主题)"), 60)
                ),
                "正文未缓存".to_string(),
            ));
        }

        let paths = self.inner.paths.clone();
        let body = tokio::task::spawn_blocking(move || me_store::cache::read_body(&paths, id))
            .await
            .map_err(|join| CoreError::storage(format!("正文读取失败：{join}")))?
            .map_err(|err| CoreError::storage(format!("正文读取失败：{err}")))?;
        let Some(body) = body else {
            return Ok((
                "这封邮件的正文文件不在缓存目录里，请让用户重新打开一次这封邮件。".to_string(),
                "正文缺失".to_string(),
            ));
        };

        let body = truncate_chars(body.trim(), MAX_BODY_CHARS);
        let sender = summary
            .from
            .as_ref()
            .map(|address| address.display())
            .unwrap_or_else(|| "(未知发件人)".to_string());
        let text = format!(
            "邮件：{}\n发件人：{}\n时间：{}\n\n{}",
            truncate_chars(summary.subject.as_deref().unwrap_or("(无主题)"), 120),
            truncate_chars(&sender, 120),
            fmt_local(summary.date),
            body
        );
        let label = truncate_chars(summary.subject.as_deref().unwrap_or("(无主题)"), 20);
        Ok((text, format!("读取「{label}」正文")))
    }

    // -----------------------------------------------------------------------
    // weather_locations
    // -----------------------------------------------------------------------

    /// 只读本地配置，不联网——用户问「我关注了哪些城市」时不该触发一次天气查询。
    async fn tool_weather_locations(&self) -> Result<(String, String), CoreError> {
        let saved = self.weather_locations().await?;
        if saved.is_empty() {
            return Ok((
                "用户还没有关注任何城市。可以提示他先在天气页添加一个城市。".to_string(),
                "无关注地点".to_string(),
            ));
        }
        let mut text = format!("已关注 {} 个地点：\n", saved.len());
        for (index, item) in saved.iter().enumerate() {
            text.push_str(&format!(
                "{}. {}（id={}{}）\n",
                index + 1,
                item.title(),
                item.id.0,
                if item.is_default { "，默认" } else { "" }
            ));
        }
        Ok((text, format!("{} 个地点", saved.len())))
    }

    // -----------------------------------------------------------------------
    // calendar_list / calendar_update / calendar_delete
    // -----------------------------------------------------------------------

    async fn tool_calendar_list(&self, call: &AiToolCall) -> Result<(String, String), CoreError> {
        if !call.parse_ok {
            return Err(CoreError::validation("参数不是合法 JSON"));
        }
        let args = &call.arguments;
        let from = match arg_str(args, "from", 40) {
            Some(raw) => parse_when(&raw, false)
                .ok_or_else(|| CoreError::validation("from 需要带时区的 ISO-8601 时间"))?,
            None => start_of_today(),
        };
        let to = match arg_str(args, "to", 40) {
            Some(raw) => parse_when(&raw, false)
                .ok_or_else(|| CoreError::validation("to 需要带时区的 ISO-8601 时间"))?,
            None => from + 7 * 86_400,
        };
        if to <= from {
            return Err(CoreError::validation("结束时间必须晚于起始时间"));
        }
        let limit = arg_u32(args, "limit").unwrap_or(10).clamp(1, 20);
        let keyword = arg_str(args, "keyword", 100).map(|value| value.to_lowercase());

        let series = self
            .db(move |store| {
                let visible: Vec<me_core::calendar::CalendarId> = store
                    .list_calendars()?
                    .into_iter()
                    .filter(|calendar| calendar.visible)
                    .map(|calendar| calendar.id)
                    .collect();
                if visible.is_empty() {
                    return Ok(Vec::new());
                }
                store.list_series_in_range(&visible, from, to, limit)
            })
            .await?;

        let hits: Vec<CalendarSeries> = series
            .into_iter()
            .filter(|item| match &keyword {
                Some(word) => item.summary.to_lowercase().contains(word),
                None => true,
            })
            .take(limit as usize)
            .collect();

        if hits.is_empty() {
            return Ok((
                format!(
                    "{} 到 {} 之间没有日程。",
                    fmt_local(Some(from)),
                    fmt_local(Some(to))
                ),
                "没有日程".to_string(),
            ));
        }
        let mut text = format!("共 {} 条日程：\n", hits.len());
        for (index, item) in hits.iter().enumerate() {
            text.push_str(&format!("{}\n", format_series(index + 1, item)));
        }
        Ok((text, format!("{} 条日程", hits.len())))
    }

    async fn tool_calendar_update(
        &self,
        conversation: me_core::ai::AiConversationId,
        call: &AiToolCall,
    ) -> Result<(String, String), CoreError> {
        if !call.parse_ok {
            return Err(CoreError::validation("参数不是合法 JSON"));
        }
        let args = &call.arguments;
        let raw = args
            .get("event_id")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| CoreError::validation("缺少 event_id（先用 calendar_list 查）"))?;
        let id = me_core::calendar::CalendarEventId(raw as i64);

        let existing = self
            .db(move |store| store.get_event(id))
            .await?
            .ok_or_else(|| CoreError::validation("没有找到这条日程（可能已被删除）"))?;
        if existing.deleted_at.is_some() {
            return Err(CoreError::validation("这条日程已经被删除了"));
        }

        let all_day = arg_bool(args, "all_day").unwrap_or(existing.all_day);
        let start_utc = match arg_str(args, "start", 40) {
            Some(raw) => parse_when(&raw, all_day)
                .ok_or_else(|| CoreError::validation("start 需要带时区的 ISO-8601 时间"))?,
            None => existing.start_utc,
        };
        let end_utc = match arg_str(args, "end", 40) {
            Some(raw) => parse_when(&raw, all_day)
                .ok_or_else(|| CoreError::validation("end 需要带时区的 ISO-8601 时间"))?,
            None => existing.end_utc.max(start_utc),
        };
        if end_utc < start_utc {
            return Err(CoreError::validation("结束时间不能早于开始时间"));
        }

        let mut changed: Vec<&'static str> = Vec::new();
        if start_utc != existing.start_utc || end_utc != existing.end_utc {
            changed.push("时间");
        }
        if all_day != existing.all_day {
            changed.push("全天标记");
        }
        let summary = arg_str(args, "title", 200);
        if let Some(title) = summary.as_deref()
            && title != existing.summary.trim()
        {
            changed.push("标题");
        }
        let location = arg_str(args, "location", 200);
        if location.is_some() {
            changed.push("地点");
        }
        let description = arg_str(args, "description", 2000);
        if description.is_some() {
            changed.push("备注");
        }
        if changed.is_empty() {
            return Ok((
                "参数里没有任何要修改的字段，日程保持原样。".to_string(),
                "没有变化".to_string(),
            ));
        }

        let draft = CalendarEventDraft {
            calendar_id: existing.calendar_id,
            summary: summary.unwrap_or_else(|| existing.summary.clone()),
            description: description.or_else(|| existing.description.clone()),
            location: location.or_else(|| existing.location.clone()),
            start_utc,
            end_utc,
            all_day,
            timezone: existing.timezone.clone(),
            rrule: existing.rrule.clone(),
            exdates: existing.exdates.clone(),
            reminders: existing.reminders.clone(),
        };
        let updated = self
            .update_event(
                id,
                draft,
                Actor::Ai {
                    conversation: conversation.0,
                },
            )
            .await?;
        Ok((
            format!(
                "已修改日程「{}」：{}。新时间 {} → {}。",
                truncate_chars(&updated.summary, 60),
                changed.join("、"),
                fmt_local(Some(updated.start_utc)),
                fmt_local(Some(updated.end_utc))
            ),
            format!("修改「{}」", truncate_chars(&updated.summary, 20)),
        ))
    }

    /// 删除**只发起请求**，真正的删除由用户在卡片上确认后走 `Command`。
    async fn tool_calendar_delete(
        &self,
        conversation: me_core::ai::AiConversationId,
        call: &AiToolCall,
    ) -> Result<(String, String), CoreError> {
        if !call.parse_ok {
            return Err(CoreError::validation("参数不是合法 JSON"));
        }
        let raw = call
            .arguments
            .get("event_id")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| CoreError::validation("缺少 event_id（先用 calendar_list 查）"))?;
        let id = me_core::calendar::CalendarEventId(raw as i64);

        let event = self
            .db(move |store| store.get_event(id))
            .await?
            .ok_or_else(|| CoreError::validation("没有找到这条日程（可能已被删除）"))?;
        if event.deleted_at.is_some() {
            return Ok((
                "这条日程已经被删除了，不需要再删一次。".to_string(),
                "已经不存在".to_string(),
            ));
        }

        self.emit(Event::AiToolCalendarDeletePending {
            conversation,
            call_id: call.id.clone(),
            event: id,
            title: event.summary.clone(),
            start_utc: event.start_utc,
        })
        .await;

        let title = truncate_chars(&event.summary, 60);
        Ok((
            format!(
                "已请用户确认删除「{title}」（{}）。在用户点「确认删除」之前这条日程仍然存在，\
不要声称已经删除；如果用户取消，就把这一步标成 skipped。",
                fmt_local(Some(event.start_utc))
            ),
            format!("待确认删除「{}」", truncate_chars(&event.summary, 20)),
        ))
    }

    // -----------------------------------------------------------------------
    // weather_get
    // -----------------------------------------------------------------------

    async fn tool_weather_get(
        &self,
        conversation: me_core::ai::AiConversationId,
        call: &AiToolCall,
        memory: bool,
    ) -> Result<(String, String), CoreError> {
        if !call.parse_ok {
            return Err(CoreError::validation("参数不是合法 JSON"));
        }
        let args = &call.arguments;
        let days = arg_u32(args, "days").unwrap_or(3).clamp(1, 7) as usize;
        let hours = arg_u32(args, "hours").unwrap_or(24).min(24) as usize;
        let wanted_name = arg_str(args, "location", 100);
        let wanted_id = arg_str(args, "location_id", 64);

        let saved = self.weather_locations().await?;
        let mut target = None;
        if let Some(raw_id) = wanted_id.as_deref() {
            if let Ok(id) = raw_id.parse::<i64>() {
                target = saved.iter().find(|item| item.id.0 == id).cloned();
            } else {
                target = saved
                    .iter()
                    .find(|item| item.provider_location_id == raw_id)
                    .cloned();
            }
        }
        if target.is_none()
            && let Some(name) = wanted_name.as_deref()
        {
            let matches: Vec<_> = saved
                .iter()
                .filter(|item| location_matches(item, name))
                .cloned()
                .collect();
            if matches.len() > 1 {
                return Ok((
                    format!(
                        "「{name}」匹配到多个已保存地点：{}。请让用户指定其中一个。",
                        matches
                            .iter()
                            .map(|item| format!("{}（id={}）", item.title(), item.id.0))
                            .collect::<Vec<_>>()
                            .join("、")
                    ),
                    "地点不明确".to_string(),
                ));
            }
            target = matches.into_iter().next();
        }

        // 城市名唯一命中候选时会顺手把它存进天气侧栏（否则没地方挂快照）。
        // 这算一次隐式写配置，所以要在结果里明说，让模型转述给用户。
        let mut auto_saved = false;
        let location = match target {
            Some(location) => location,
            None => {
                let Some(name) = wanted_name.clone() else {
                    return Err(CoreError::validation(
                        "请提供城市名（location）或已保存地点的 location_id",
                    ));
                };
                let candidates = self.weather_candidates(&name).await?;
                match candidates.len() {
                    0 => {
                        return Ok((
                            format!("没有找到「{name}」这个地点，请让用户换一个更完整的城市名。"),
                            "未找到地点".to_string(),
                        ));
                    }
                    1 => {
                        let candidate = candidates[0].clone();
                        self.add_weather_location(candidate.clone()).await;
                        auto_saved = true;
                        let saved = self.weather_locations().await?;
                        match saved.into_iter().find(|item| {
                            item.provider_location_id == candidate.provider_location_id
                        }) {
                            Some(location) => location,
                            None => {
                                return Err(CoreError::validation("地点保存失败，请稍后再试"));
                            }
                        }
                    }
                    _ => {
                        let list = candidates
                            .iter()
                            .take(10)
                            .map(|item| item.title())
                            .collect::<Vec<_>>()
                            .join("、");
                        return Ok((
                            format!(
                                "「{name}」有多个候选地点：{list}。\
不要自行挑选，请把候选列给用户，让用户确认后再查一次。"
                            ),
                            "地点不明确".to_string(),
                        ));
                    }
                }
            }
        };

        self.refresh_weather(location.id, false).await;
        let snapshot = self.weather_snapshot(location.id).await?;
        let Some(snapshot) = snapshot else {
            return Err(CoreError::validation("天气数据暂时不可用，请稍后再试"));
        };
        if memory {
            self.register_location_entity(conversation, &location).await;
        }
        let mut text = format_weather(&location.title(), &snapshot, days, hours);
        if auto_saved {
            text.push_str(&format!(
                "\n（已把「{}」加入天气地点，用户可在天气页删除）",
                location.title()
            ));
        }
        Ok((text, format!("{} 天气", location.title())))
    }

    // -----------------------------------------------------------------------
    // calendar_create
    // -----------------------------------------------------------------------

    async fn tool_calendar_create(
        &self,
        conversation: me_core::ai::AiConversationId,
        call: &AiToolCall,
        memory: bool,
    ) -> Result<(String, String), CoreError> {
        if !call.parse_ok {
            return Err(CoreError::validation("参数不是合法 JSON"));
        }
        let args = &call.arguments;
        let title =
            arg_str(args, "title", 200).ok_or_else(|| CoreError::validation("缺少日程标题"))?;
        let all_day = arg_bool(args, "all_day").unwrap_or(false);
        let start_raw =
            arg_str(args, "start", 64).ok_or_else(|| CoreError::validation("缺少开始时间"))?;
        let end_raw =
            arg_str(args, "end", 64).ok_or_else(|| CoreError::validation("缺少结束时间"))?;
        let start_utc = parse_when(&start_raw, all_day)
            .ok_or_else(|| CoreError::validation("开始时间格式不正确（需要带时区的 ISO-8601）"))?;
        let mut end_utc = parse_when(&end_raw, all_day)
            .ok_or_else(|| CoreError::validation("结束时间格式不正确（需要带时区的 ISO-8601）"))?;
        if all_day && end_utc <= start_utc {
            end_utc = start_utc + 86_400;
        }
        if end_utc <= start_utc {
            return Err(CoreError::validation("结束时间必须晚于开始时间"));
        }

        let calendar = match arg_u32(args, "calendar_id") {
            Some(id) => me_core::CalendarId(id as i64),
            None => self.db(|s| s.ensure_default_calendar()).await?.id,
        };

        // 去重：同标题 ±5 分钟视为同一条，直接返回既有事件。
        if let Some(existing) = self
            .find_duplicate_event(Some(calendar), &title, start_utc)
            .await?
        {
            return Ok((
                format!(
                    "已存在相同日程（id={}，开始于 {}），没有重复创建。",
                    existing.id.0,
                    me_core::format_local_datetime(existing.start_utc)
                ),
                "已存在，未重复创建".to_string(),
            ));
        }

        let reminders: Vec<u32> = args
            .get("reminders")
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_u64())
                    .map(|value| (value as u32).min(10_080))
                    .take(3)
                    .collect()
            })
            .unwrap_or_default();

        let draft = CalendarEventDraft {
            calendar_id: calendar,
            summary: title,
            description: arg_str(args, "description", 2000),
            location: arg_str(args, "location", 200),
            start_utc,
            end_utc,
            all_day,
            timezone: None,
            rrule: None,
            exdates: Vec::new(),
            reminders,
        };
        let event = self
            .create_event(
                draft,
                Actor::Ai {
                    conversation: conversation.0,
                },
            )
            .await?;

        self.emit(Event::AiToolCalendarCreated {
            conversation,
            call_id: call.id.clone(),
            event: event.id,
            start_utc: event.start_utc,
            undoable: true,
        })
        .await;

        if memory {
            self.register_event_entity(conversation, &event).await;
        }

        Ok((
            format!(
                "已创建日程「{}」，开始于 {}（id={}）。用户可以撤销。",
                event.summary,
                me_core::format_local_datetime(event.start_utc),
                event.id.0
            ),
            "已创建日程".to_string(),
        ))
    }

    // -----------------------------------------------------------------------
    // memory_upsert
    // -----------------------------------------------------------------------

    async fn tool_memory_upsert(
        &self,
        conversation: me_core::ai::AiConversationId,
        call: &AiToolCall,
    ) -> Result<(String, String), CoreError> {
        if !call.parse_ok {
            return Err(CoreError::validation("参数不是合法 JSON"));
        }
        let args = &call.arguments;
        let kind_raw =
            arg_str(args, "kind", 32).ok_or_else(|| CoreError::validation("缺少实体类型 kind"))?;
        let kind = AiEntityKind::parse(&kind_raw)
            .ok_or_else(|| CoreError::validation("实体类型不合法"))?;
        if kind == AiEntityKind::Event {
            return Err(CoreError::validation(
                "event 类型只由工具结果自动登记，不能手动写入",
            ));
        }
        let canonical_name = arg_str(args, "canonical_name", 120)
            .ok_or_else(|| CoreError::validation("缺少实体名称"))?;
        let aliases: Vec<String> = args
            .get("aliases")
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .map(|item| truncate_chars(item.trim(), 120))
                    .filter(|item| !item.is_empty())
                    .take(8)
                    .collect()
            })
            .unwrap_or_default();
        let email = arg_str(args, "email", 320);
        let attributes = args
            .get("attributes")
            .cloned()
            .filter(|value| value.is_object())
            .unwrap_or_else(|| serde_json::json!({}));
        if attributes.to_string().len() > 1024 {
            return Err(CoreError::validation("属性过大（上限 1 KB）"));
        }

        let saved = self
            .upsert_ai_entity(
                conversation,
                NewAiEntity {
                    kind,
                    canonical_name: canonical_name.clone(),
                    aliases: aliases.clone(),
                    email,
                    account: None,
                    calendar: None,
                    attributes,
                    source: AiEntitySource::AiInferred,
                    confidence: 0.9,
                },
            )
            .await?;
        self.emit(Event::AiEntitiesChanged).await;
        // 实体总数变了，设置页的「N / 200」也要跟着走；只发 AiEntitiesChanged
        // 时面板要等下一次配置变化才刷新。
        self.emit_ai_config().await;
        Ok((
            format!("已记住：{}（{}）", saved.canonical_name, saved.kind.label()),
            format!("已记住 {}", saved.canonical_name),
        ))
    }

    // -----------------------------------------------------------------------
    // 辅助：检索条件解析与工具结果自动登记
    // -----------------------------------------------------------------------

    /// `from` 允许写人名：命中实体且带邮箱时换成邮箱，否则原样返回。
    async fn resolve_from_filter(&self, raw: String) -> String {
        if raw.contains('@') {
            return raw;
        }
        let probe = raw.clone();
        let found = self
            .db(move |s| s.find_entities_by_alias(&probe, 5))
            .await
            .unwrap_or_default();
        found
            .into_iter()
            .find_map(|entity| entity.email)
            .unwrap_or(raw)
    }

    /// 文件夹 id → 名称。检索结果里带上文件夹，模型才能回答「在哪找到的」。
    async fn folder_names(&self) -> std::collections::HashMap<FolderId, String> {
        self.db(|s| {
            let mut map = std::collections::HashMap::new();
            for account in s.list_accounts()? {
                for folder in s.list_folders(account.id)? {
                    map.insert(folder.id, folder.name);
                }
            }
            Ok(map)
        })
        .await
        .unwrap_or_default()
    }

    /// 工具结果自动登记：只登记这次操作**真正用到**的对象，不读正文、不猜。
    ///
    /// 失败一律忽略——记忆写满（200 条）不该让工具本身失败；这也是为什么
    /// 返回类型是 `()` 而不是 `Result`。
    async fn register_location_entity(
        &self,
        conversation: me_core::ai::AiConversationId,
        location: &me_core::WeatherLocation,
    ) {
        let entity = NewAiEntity {
            kind: AiEntityKind::Location,
            canonical_name: location.title(),
            aliases: vec![location.name.clone()],
            email: None,
            account: None,
            calendar: None,
            attributes: serde_json::json!({
                "provider": location.provider,
                "provider_location_id": location.provider_location_id,
                "timezone": location.timezone,
            }),
            source: AiEntitySource::Tool,
            confidence: 1.0,
        };
        if self.upsert_ai_entity(conversation, entity).await.is_ok() {
            self.emit(Event::AiEntitiesChanged).await;
        }
    }

    async fn register_event_entity(
        &self,
        conversation: me_core::ai::AiConversationId,
        event: &me_core::CalendarEvent,
    ) {
        let entity = NewAiEntity {
            kind: AiEntityKind::Event,
            canonical_name: event.summary.clone(),
            aliases: Vec::new(),
            email: None,
            account: None,
            calendar: Some(event.calendar_id),
            attributes: serde_json::json!({
                "event_id": event.id.0,
                "start_utc": event.start_utc,
            }),
            source: AiEntitySource::Tool,
            confidence: 1.0,
        };
        if self.upsert_ai_entity(conversation, entity).await.is_ok() {
            self.emit(Event::AiEntitiesChanged).await;
        }
        if let Some(name) = event.location.as_deref().map(str::trim)
            && !name.is_empty()
        {
            let location = NewAiEntity {
                kind: AiEntityKind::Location,
                canonical_name: truncate_chars(name, 120),
                aliases: Vec::new(),
                email: None,
                account: None,
                calendar: None,
                attributes: serde_json::json!({"source_event": event.id.0}),
                source: AiEntitySource::Tool,
                confidence: 0.9,
            };
            if self.upsert_ai_entity(conversation, location).await.is_ok() {
                self.emit(Event::AiEntitiesChanged).await;
            }
        }
    }

    // -----------------------------------------------------------------------
    // plan_submit / answer_directly（PLAN 阶段）
    // -----------------------------------------------------------------------

    /// 收到一份**新**计划。校验失败时清单保持不变，模型会收到可行动的错误文案。
    ///
    /// 允许的工具集合来自本次请求的权限快照：模型不能把一个用户关掉的能力
    /// 写进计划——那会在执行期变成一条「工具已关闭」的错误，白烧一轮。
    fn tool_plan_submit(
        &self,
        call: &AiToolCall,
        permissions: &AiToolPermissions,
        plan: &mut Option<Plan>,
    ) -> Result<(String, String), CoreError> {
        if !call.parse_ok {
            return Err(CoreError::validation("参数不是合法 JSON"));
        }
        let args = &call.arguments;
        let goal = arg_str(args, "goal", 120)
            .ok_or_else(|| CoreError::validation("缺少 goal：一句话说清这次要做出什么成果"))?;
        let raw_steps = args
            .get("steps")
            .and_then(Value::as_array)
            .ok_or_else(|| CoreError::validation("缺少 steps（计划步骤数组）"))?;

        let mut steps = Vec::with_capacity(raw_steps.len());
        for item in raw_steps {
            steps.push(plan_step_v2_from_json(item)?);
        }

        let now = now_unix();
        let draft = Plan {
            goal,
            steps,
            created_at: now,
            updated_at: now,
            ..Plan::default()
        };
        let allowed: Vec<AiToolName> = me_core::ai::TOOL_SPECS
            .iter()
            .filter(|spec| !spec.name.is_bookkeeping() && permissions.enabled(spec.name))
            .map(|spec| spec.name)
            .collect();
        let next = validate_plan_v2(None, &draft, &allowed)?;
        let summary = next.summary_line();
        *plan = Some(next.clone());

        Ok((
            format!(
                "计划已收到（{} 步）。现在逐步执行：每完成一步就用 plan_update 把该步标成 \
done/failed/skipped 并写一句 evidence（拿到了什么）。{summary}",
                next.steps.len()
            ),
            summary,
        ))
    }

    /// 显式豁免：模型声明「这次不需要工具」。
    fn tool_answer_directly(call: &AiToolCall) -> Result<(String, String), CoreError> {
        if !call.parse_ok {
            return Err(CoreError::validation("参数不是合法 JSON"));
        }
        let reason = arg_str(&call.arguments, "reason", 200)
            .ok_or_else(|| CoreError::validation("缺少 reason：说明为什么这次不需要工具"))?;
        Ok((
            "已确认：本次不需要工具，请直接回答用户。".to_string(),
            format!("直接回答：{reason}"),
        ))
    }

    // -----------------------------------------------------------------------
    // plan_update
    // -----------------------------------------------------------------------

    /// 合并模型给的新清单。
    ///
    /// 不做 IO、不套超时：它只改内存里的 [`Plan`]，落库由 `run_ai_stream` 收尾时统一做。
    /// 校验失败时**清单保持不变**——宁可让模型重说一遍，也不要把一份坏清单写进库。
    fn tool_plan_update(
        &self,
        call: &AiToolCall,
        plan: &mut Option<Plan>,
    ) -> Result<(String, String), CoreError> {
        if !call.parse_ok {
            return Err(CoreError::validation("参数不是合法 JSON"));
        }
        let args = &call.arguments;
        let raw_steps = args
            .get("steps")
            .and_then(Value::as_array)
            .ok_or_else(|| CoreError::validation("缺少 steps（清单步骤数组）"))?;
        let mut steps = Vec::with_capacity(raw_steps.len());
        for item in raw_steps {
            steps.push(plan_step_from_json(item)?);
        }

        let now = me_core::now_unix();
        let draft = Plan {
            // id / created_at 由 `validate_plan` 从上一版继承，模型看不到也不需要填。
            id: String::new(),
            goal: arg_str(args, "goal", 240).unwrap_or_default(),
            steps,
            created_at: plan.as_ref().map(|item| item.created_at).unwrap_or(now),
            updated_at: now,
            ..Plan::default()
        };
        let next = validate_plan(plan.as_ref(), &draft)?;
        let summary = next.summary_line();
        *plan = Some(next.clone());

        // 回给模型的文字要含「还没做完什么」，它才能决定下一步调哪个工具；
        // 只回「已更新」会让模型以为清单做完就无需继续。
        Ok((format!("已更新清单。{summary}"), summary))
    }
}

/// 把 `plan_update` 的一个 step 对象转成领域类型。
///
/// 错误信息要**点名是哪一步的哪个字段**：模型收到的 `is_error` 工具结果里
/// 只有这句话，含糊的「参数错误」会让它原样重试一次。
fn plan_step_from_json(value: &Value) -> Result<PlanStep, CoreError> {
    let id = value
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| CoreError::validation("每一步都需要数字 id（1–6）"))?;
    let id = u8::try_from(id).map_err(|_| CoreError::validation("步骤编号必须在 1–6 之间"))?;

    let capability = value
        .get("capability")
        .and_then(Value::as_str)
        .and_then(Capability::parse)
        .ok_or_else(|| {
            CoreError::validation(format!(
                "第 {id} 步的 capability 只能是 mail / weather / calendar / music"
            ))
        })?;

    let title = value
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| CoreError::validation(format!("第 {id} 步缺少 title")))?;

    let status = value
        .get("status")
        .and_then(Value::as_str)
        .and_then(PlanStatus::parse)
        .ok_or_else(|| {
            CoreError::validation(format!(
                "第 {id} 步的 status 只能是 pending / running / done / failed / skipped"
            ))
        })?;

    let evidence = value
        .get("evidence")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);

    let mut step = PlanStep::new(id, title, capability, status);
    step.evidence = evidence;
    Ok(step)
}

/// 把 `plan_submit` 的一个 step 对象转成 v2 的领域类型。
///
/// 与 [`plan_step_from_json`] 分开写：那个只接受 id/status/evidence（`plan_update`
/// 的契约），这个要接受 kind/tool/args/expect/on_empty。混用一个函数会让
/// `plan_update` 不小心也能改工具与参数——那等于绕过了「重列才换方案」的约束。
fn plan_step_v2_from_json(value: &Value) -> Result<PlanStep, CoreError> {
    let id = value
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| CoreError::validation("每一步都需要数字 id（1–8）"))?;
    let id = u8::try_from(id).map_err(|_| CoreError::validation("步骤编号必须在 1–8 之间"))?;

    let title = value
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| CoreError::validation(format!("第 {id} 步缺少 title")))?;

    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .and_then(StepKind::parse)
        .ok_or_else(|| {
            CoreError::validation(format!(
                "第 {id} 步的 kind 只能是 tool / reason / ask（收到 {:?}）",
                value.get("kind")
            ))
        })?;

    let capability = value
        .get("capability")
        .and_then(Value::as_str)
        .and_then(Capability::parse)
        .ok_or_else(|| {
            CoreError::validation(format!(
                "第 {id} 步的 capability 只能是 mail / weather / calendar / music"
            ))
        })?;

    let tool = match value.get("tool").and_then(Value::as_str) {
        Some(name) if !name.trim().is_empty() => {
            let name = name.trim();
            Some(AiToolName::parse(name).ok_or_else(|| {
                CoreError::validation(format!(
                    "第 {id} 步的 tool「{name}」不是已知工具：可用的是 email_search / mail_read / \
weather_get / weather_locations / calendar_list / calendar_create / calendar_update / \
calendar_delete / memory_upsert / music_search / music_play / music_state / music_control"
                ))
            })?)
        }
        _ => None,
    };

    let args = match value.get("args") {
        Some(Value::Null) | None => None,
        Some(object @ Value::Object(_)) => Some(object.clone()),
        Some(_) => {
            return Err(CoreError::validation(format!(
                "第 {id} 步的 args 必须是对象，例如 {{\"unread_only\": true}}"
            )));
        }
    };

    let expect = value
        .get("expect")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);

    let on_empty = value
        .get("on_empty")
        .and_then(Value::as_str)
        .and_then(OnEmpty::parse)
        .unwrap_or_default();

    let mut step = PlanStep::new(id, title, capability, PlanStatus::Pending);
    step.kind = kind;
    step.tool = tool;
    step.args = args;
    step.expect = expect;
    step.on_empty = on_empty;
    Ok(step)
}

fn location_matches(location: &me_core::WeatherLocation, name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() {
        return false;
    }
    let admin2_matches = location
        .admin2
        .as_deref()
        .is_some_and(|admin| admin.eq_ignore_ascii_case(name));
    location.name.eq_ignore_ascii_case(name) || admin2_matches || location.title().contains(name)
}

/// 这封邮件**为什么**出现在结果里。
///
/// 只依据已经下发的筛选条件反推，不额外调模型——卡片上那句「主题命中」必须是
/// 真实原因，编一句解释比不解释更糟。
fn card_reason(
    message: &MessageSummary,
    subject: Option<&str>,
    from: Option<&str>,
    text: Option<&str>,
) -> String {
    let contains =
        |haystack: &str, needle: &str| haystack.to_lowercase().contains(&needle.to_lowercase());
    if let Some(word) = subject
        && message
            .subject
            .as_deref()
            .is_some_and(|value| contains(value, word))
    {
        return format!("主题命中「{word}」");
    }
    if let Some(word) = from
        && message
            .from
            .as_ref()
            .is_some_and(|address| contains(&address.display(), word))
    {
        return "发件人命中".to_string();
    }
    if let Some(word) = text {
        if message
            .snippet
            .as_deref()
            .is_some_and(|value| contains(value, word))
        {
            return format!("正文片段命中「{word}」");
        }
        return format!("匹配「{word}」");
    }
    "匹配筛选条件".to_string()
}

/// 本地时区的 `YYYY-MM-DD HH:MM`。工具结果给人看，必须按用户本地时间解释，
/// 直接甩 UTC 时间戳会让模型把「15:00」说成「07:00」。
fn fmt_local(ts: Option<i64>) -> String {
    ts.map(me_core::format_local_datetime)
        .unwrap_or_else(|| "未知".to_string())
}

/// 今天 00:00（本地）。`calendar_list` 不给时间窗时的默认起点。
fn start_of_today() -> i64 {
    let today = chrono::Local::now().date_naive();
    today
        .and_hms_opt(0, 0, 0)
        .and_then(|naive| naive.and_local_timezone(chrono::Local).single())
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

fn format_series(index: usize, item: &CalendarSeries) -> String {
    let when = if item.all_day {
        let day: String = fmt_local(Some(item.start_utc)).chars().take(10).collect();
        format!("{day}（全天）")
    } else {
        format!(
            "{} → {}",
            fmt_local(Some(item.start_utc)),
            fmt_local(Some(item.end_utc))
        )
    };
    let repeat = if item.rrule.is_some() {
        " · 重复"
    } else {
        ""
    };
    format!(
        "{index}. [event_id={}] {} · {when}{repeat}",
        item.id.0,
        truncate_chars(&item.summary, 60)
    )
}

fn format_message(index: usize, message: &MessageSummary, folder: Option<String>) -> String {
    let date = message
        .date
        .map(me_core::format_local_datetime)
        .unwrap_or_else(|| "未知日期".to_string());
    let snippet = message.snippet.as_deref().unwrap_or("").replace('\n', " ");
    let flag = if message.flags.contains(Flags::SEEN) {
        ""
    } else {
        "未读 "
    };
    let subject = message
        .subject
        .clone()
        .unwrap_or_else(|| "(无主题)".to_string());
    let mut tags = Vec::new();
    if let Some(folder) = folder {
        tags.push(folder);
    }
    if message.has_attachment {
        tags.push("有附件".to_string());
    }
    let tags = if tags.is_empty() {
        String::new()
    } else {
        format!(" · {}", tags.join(" · "))
    };
    format!(
        "{index}. [{date}] {flag}{subject} — {from}{tags}\n   {snippet}",
        from = message
            .from
            .as_ref()
            .map(|address| address.display())
            .unwrap_or_else(|| "(未知发件人)".to_string()),
        snippet = truncate_chars(&snippet, 300)
    )
}

fn format_weather(
    title: &str,
    snapshot: &me_core::WeatherSnapshot,
    days: usize,
    hours: usize,
) -> String {
    use me_core::weather::{format_humidity, format_temp, format_wind};
    let current = &snapshot.current;
    let mut text = format!(
        "地点：{title}\n更新时间：{}\n当前：{}，{}，体感 {}，{}，{}\n",
        me_core::format_local_datetime(snapshot.fetched_at),
        format_temp(current.temp_c),
        current.text,
        format_temp(current.feels_like_c),
        format_wind(current.wind_scale, current.wind_speed_kmh),
        format_humidity(current.humidity_percent),
    );
    if hours > 0 && !snapshot.hourly.is_empty() {
        text.push_str("逐时：");
        let items: Vec<String> = snapshot
            .hourly
            .iter()
            .take(hours)
            .map(|hour| {
                format!(
                    "{} {} {}",
                    me_core::format_local_datetime(hour.time),
                    format_temp(hour.temp_c),
                    hour.text
                )
            })
            .collect();
        text.push_str(&items.join("；"));
        text.push('\n');
    }
    if days > 0 && !snapshot.daily.is_empty() {
        text.push_str("逐日：");
        let items: Vec<String> = snapshot
            .daily
            .iter()
            .take(days)
            .map(|day| {
                format!(
                    "{} {}~{} {}",
                    day.date,
                    format_temp(day.temp_min_c),
                    format_temp(day.temp_max_c),
                    day.text
                )
            })
            .collect();
        text.push_str(&items.join("；"));
        text.push('\n');
    }
    text.push_str("（数据来源：和风天气）");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 超时必须变成一句**说得清**的工具错误：带上工具名与秒数，
    /// 而不是 `CoreError::ai(Transient)` 那句固定的「服务暂时不可用」。
    #[tokio::test]
    async fn timeout_becomes_a_readable_tool_error() {
        let result = with_timeout(Duration::from_millis(10), "邮件检索", async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(("不该到这里".to_string(), String::new()))
        })
        .await;

        let message = result.expect_err("必须超时").user_message();
        assert!(message.contains("邮件检索"), "要说明哪个工具：{message}");
        assert!(message.contains("超时"), "要说明是超时：{message}");
    }
}
