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
use std::sync::{Arc, Mutex};
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

/// 工具配额的一次快照。
///
/// 引擎自动重试**同一次逻辑调用**时不该重复计费：天气查询一次回复只允许一次，
/// 邮件正文一次回复最多读三封——重试如果是重新计一次，就会把「重试」变成
/// 「撞限额」，而限额错误会把真正的失败原因盖掉。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ToolQuota {
    weather: u32,
    calendar: u32,
    calendar_read: u32,
    mail_read: u32,
    memory: u32,
}

impl ToolBudget {
    /// 记下这次不可重试的失败。同一指纹只留第一条错误，后面重复的没有新信息。
    pub(crate) fn remember_failure(&mut self, call: &AiToolCall, error: &str) {
        self.failed
            .entry(attempt_fingerprint(call))
            .or_insert_with(|| truncate_chars(error, 120));
    }

    /// 当前的工具配额快照。
    pub(crate) fn quota_snapshot(&self) -> ToolQuota {
        ToolQuota {
            weather: self.weather,
            calendar: self.calendar,
            calendar_read: self.calendar_read,
            mail_read: self.mail_read,
            memory: self.memory,
        }
    }

    /// 把配额退回到某次快照。
    ///
    /// 只在**引擎自动重试**前调用：重试的是同一次逻辑调用，
    /// 配额已经付过一次了。少了这一步，「天气重试」第二次就会拿到
    /// 「本次消息已查询过天气」，把真正的网络错误盖掉。
    pub(crate) fn restore_quota(&mut self, snapshot: ToolQuota) {
        self.weather = snapshot.weather;
        self.calendar = snapshot.calendar;
        self.calendar_read = snapshot.calendar_read;
        self.mail_read = snapshot.mail_read;
        self.memory = snapshot.memory;
    }

    /// 本轮所有「不可重试失败」的调用指纹。
    ///
    /// 重列时用它剔除「换个说法但业务参数没变」的步骤——没有这道闸，
    /// 「重列」就是「重新抽卡」，用户看到的是 AI 认真地又失败了一次。
    pub(crate) fn failed_fingerprints(&self) -> Vec<String> {
        self.failed.keys().cloned().collect()
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
        failure: Some(me_core::ai::ToolFailure::InvalidArgs),
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
            cards: Vec::new(),
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

/// PLAN 阶段调了业务工具：不执行，告诉它现在该做什么。
///
/// 这是一个**防御层**：PLAN 阶段的请求里根本没下发这些工具，正常模型不会调。
/// 真调了说明它在凭历史记忆行动，这时候报「参数不对」毫无意义。
pub(crate) fn phase_rejected_result(
    call: &AiToolCall,
    motive: Option<String>,
    motive_source: MotiveSource,
    hint: &str,
    summary: &str,
) -> AiToolResult {
    AiToolResult {
        call_id: call.id.clone(),
        tool: call.name,
        content: hint.to_string(),
        is_error: true,
        failure: Some(me_core::ai::ToolFailure::Denied),
        trace: AiToolTrace {
            call_id: call.id.clone(),
            tool: call.name,
            status: AiToolStatus::Error,
            summary: summary.to_string(),
            duration_ms: 0,
            motive,
            motive_source,
            step_id: None,
            attempt: None,
            cards: Vec::new(),
        },
    }
}

/// 把一次工具调用绑定到清单里的某一步。
///
/// 降级顺序（**绑定失败不拒绝执行**——从 D1 学到的：报错换不来它改对）：
/// 1. 参数里的 `step_id`，且该步确实是这一步工具；
/// 2. `motive` 里写的「第 N 步」；
/// 3. 第一个「同一个工具、还没结束」的步；
/// 4. 第一个「还没结束」的工具步。
pub(crate) fn bind_step(plan: &Plan, call: &AiToolCall, motive: &Option<String>) -> Option<u8> {
    let usable = |id: u8| {
        plan.step(id)
            .filter(|step| step.is_tool_step() && !step.status.is_terminal())
            .filter(|step| step.tool == Some(call.name))
            .map(|step| step.id)
    };

    if let Some(id) = call_step_id(call).and_then(usable) {
        return Some(id);
    }
    if let Some(text) = motive.as_deref()
        && let Some(id) = step_id_from_text(text).and_then(usable)
    {
        return Some(id);
    }
    plan.steps
        .iter()
        .filter(|step| step.is_tool_step() && !step.status.is_terminal())
        .find(|step| step.tool == Some(call.name))
        .or_else(|| {
            plan.steps
                .iter()
                .find(|step| step.is_tool_step() && !step.status.is_terminal())
        })
        .map(|step| step.id)
}

/// 从「执行第 2 步」这类文本里取出 `2`。
fn step_id_from_text(text: &str) -> Option<u8> {
    let start = text.find('第')? + '第'.len_utf8();
    let rest = &text[start..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    if !rest[digits.len()..].starts_with('步') {
        return None;
    }
    digits.parse().ok()
}

/// 工具错误归类。
///
/// 引擎这里只有一句 `summary`（`CoreError` 已经在 `execute_ai_tool` 里被转成
/// 工具结果了），所以靠文案判定。分错类的代价可控：`InvalidArgs` 与 `Transient`
/// 都只是标在步上给人看，真正的刹车是靠指纹熔断（那一步与文案无关）。
pub(crate) fn classify_failure(
    tool: me_core::ai::AiToolName,
    failure: me_core::ai::ToolFailure,
    summary: &str,
) -> me_core::ai_plan::StepFailure {
    use me_core::ai::ToolFailure;
    use me_core::ai_plan::StepFailure;
    match failure {
        ToolFailure::InvalidArgs => StepFailure::InvalidArgs {
            detail: summary.to_string(),
        },
        ToolFailure::Transient => StepFailure::Transient {
            detail: summary.to_string(),
        },
        ToolFailure::Denied => StepFailure::Denied {
            tool: tool.label().to_string(),
        },
        ToolFailure::Write => StepFailure::Write {
            tool: tool.label().to_string(),
        },
    }
}

/// 把 `CoreError` 归到「改了参数有没有用」这一档。
///
/// 分档依据**不是**错误文案，而是错误的来源：
/// - 网络抖动 / 超时 → 原样重试有用；
/// - 参数校验失败 → 必须改参数；
/// - 工具没开 / 次数超限 → 什么办法都没用。
fn classify_tool_failure(
    tool: me_core::ai::AiToolName,
    err: &CoreError,
) -> me_core::ai::ToolFailure {
    use me_core::ai::ToolFailure;
    // 有副作用的工具优先：写操作失败时**绝不**自动重试，
    // 哪怕根因是网络抖动——重试一次可能就真的建了两个日程。
    if tool.has_side_effect() {
        return ToolFailure::Write;
    }
    match err {
        CoreError::Validation { .. } => ToolFailure::InvalidArgs,
        CoreError::Limit { .. } => ToolFailure::Denied,
        _ if err.is_retryable() => ToolFailure::Transient,
        _ => ToolFailure::InvalidArgs,
    }
}

/// 这次结果是不是「成功但空」。
///
/// 「未找到」不是成功也不是失败，是**计划需要修正**的信号：要按 `on_empty`
/// 处理，不能标成 done 让后面的步拿着空结果继续跑。
pub(crate) fn is_empty_result(summary: &str) -> bool {
    ["未找到", "没有找到", "0 命中", "没有匹配", "暂无"]
        .iter()
        .any(|hint| summary.contains(hint))
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
        // 超时是**临时故障**：原样再试一次通常就成了。
        // 不能用 `validation`——那会让 AI 层把它当成「参数不对」而不重试。
        Err(_) => Err(CoreError::timeout(format!(
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

/// `email_search` 认识的参数名。
///
/// 与 schema 的 `properties` 一一对应（`motive` / `step_id` 是引擎注入的，不算）。
const EMAIL_SEARCH_ARG_NAMES: &[&str] = &[
    "query",
    "from",
    "to",
    "subject",
    "after",
    "before",
    "days",
    "folder",
    "unread_only",
    "flagged_only",
    "has_attachment",
    "criteria",
    "limit",
];

/// 已经被移除的参数，以及它们的替代说法。
///
/// `important_only` 曾经是「什么算重要」的预置口径。它被移除是因为
/// 那等于系统替所有用户决定了「重要」等于哪几条——现在这件事由模型
/// 写成判据（`criteria`）来完成。
const RETIRED_EMAIL_ARGS: &[(&str, &str)] = &[(
    "important_only",
    "它曾预置了「什么算重要」的一套口径，现在这个判断由你自己做：\
用 criteria 把你理解的判据逐条写出来（每条一个 label 加一组条件），\
命中任意一条即算命中",
)];

/// 拒绝不认识的参数，并点名。
///
/// 只说「至少需要一个检索条件」等于让模型瞎猜——它不知道缺的是哪一类，
/// 只会换个说法再试一次（当初 8 连击的成因之一）。
fn reject_unknown_email_args(args: &Value) -> Result<(), CoreError> {
    let Some(map) = args.as_object() else {
        return Ok(());
    };
    for key in map.keys() {
        if key == "motive" || key == "step_id" {
            continue;
        }
        if let Some((_, hint)) = RETIRED_EMAIL_ARGS.iter().find(|(name, _)| name == key) {
            return Err(CoreError::validation(format!(
                "参数「{key}」已经没有了。{hint}"
            )));
        }
        if !EMAIL_SEARCH_ARG_NAMES.contains(&key.as_str()) {
            return Err(CoreError::validation(format!(
                "参数「{key}」不认得。可用的是：{}",
                EMAIL_SEARCH_ARG_NAMES.join(" / ")
            )));
        }
    }
    Ok(())
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
        // 检索类工具会把命中卡片写进这里，随后挂到 trace 上一起落库。
        // 用 per-call 的槽位而不是返回值：只有 email_search 产卡片，
        // 为它改所有工具的返回契约不值得。
        let cards_out: Arc<Mutex<Vec<me_core::MailCardRef>>> = Arc::new(Mutex::new(Vec::new()));
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
                        self.tool_email_search(conversation, call, &cards_out),
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
                failure: None,
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
                    cards: cards_out
                        .lock()
                        .map(|mut slot| std::mem::take(&mut *slot))
                        .unwrap_or_default(),
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
                    failure: Some(classify_tool_failure(call.name, &err)),
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
                        cards: Vec::new(),
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
        cards_out: &Arc<Mutex<Vec<me_core::MailCardRef>>>,
    ) -> Result<(String, String), CoreError> {
        if !call.parse_ok {
            return Err(CoreError::validation("参数不是合法 JSON"));
        }
        let args = &call.arguments;

        // 判据组：模型把定性需求翻译成的多条判据，取**并集**。
        //
        // 判据组与普通条件**互斥**：判据自己带条件，再叠一层普通条件
        // 会让「重要」变成「重要且最近」——那是另一个问题。
        let criteria = me_core::ai_plan::parse_criteria(args)?;
        if !criteria.is_empty() {
            return self
                .email_search_by_criteria(conversation, call, &criteria, cards_out)
                .await;
        }

        let query = self.build_email_query(args, 5).await?;
        // 卡片上的「命中理由」要在把条件 move 进 SearchQuery 之前留一份。
        let reason_text = arg_str(args, "query", 200);
        let reason_subject = arg_str(args, "subject", 200);
        let reason_from = arg_str(args, "from", 320);
        let outcome = self.search_mail(query, 20).await?;
        let messages = outcome.messages;
        let folder_names = self.folder_names().await;

        let cards: Vec<me_core::MailCardRef> = messages
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
            .collect();
        self.emit(Event::AiToolEmailResults {
            conversation,
            call_id: call.id.clone(),
            cards: cards.clone(),
            total: outcome.total,
        })
        .await;
        // 随 trace 落库：历史回看时这些卡片要重新显示出来。
        if let Ok(mut slot) = cards_out.lock() {
            *slot = cards;
        }

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

    /// 按**判据组**检索：每条判据各查一次，结果取并集。
    ///
    /// 为什么要单独一条路径：
    /// - 判据之间是「或」，而 `SearchQuery` 的多个条件是「且」——语义相反，
    ///   塞进同一个查询会得到完全不同的结果；
    /// - 每条结果要标注**命中了哪条判据**，用户据此判断模型的理解对不对；
    /// - 排序按判据在组里的先后：靠前的判据命中时排在前面。
    ///
    /// 这是「让模型自己理解需求」的落点：引擎只负责把模型给的判据
    /// 忠实执行并标出来，**不替模型决定什么算重要**。
    async fn email_search_by_criteria(
        &self,
        conversation: me_core::ai::AiConversationId,
        call: &AiToolCall,
        criteria: &[me_core::ai_plan::Criterion],
        cards_out: &Arc<Mutex<Vec<me_core::MailCardRef>>>,
    ) -> Result<(String, String), CoreError> {
        let limit = arg_u32(&call.arguments, "limit").unwrap_or(5).clamp(1, 20) as usize;
        let folder_names = self.folder_names().await;

        // 每条判据各查一次。按判据顺序累加，后面的判据不覆盖前面的。
        let mut ranked: Vec<(usize, MessageSummary)> = Vec::new();
        let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
        let mut per_criterion: Vec<(String, u32)> = Vec::new();

        for (index, criterion) in criteria.iter().enumerate() {
            let query = self
                .build_email_query(&criterion.query, limit as u32)
                .await?;
            let outcome = self.search_mail(query, 20).await?;
            per_criterion.push((criterion.label.clone(), outcome.total));
            for message in outcome.messages {
                if seen.insert(message.id.0) {
                    ranked.push((index, message));
                }
            }
        }

        // 排序：先按判据顺序（靠前的判据优先），同一条判据内保持时间倒序。
        // 稳定排序保证同一判据内的原始顺序不被打乱。
        ranked.sort_by_key(|(index, _)| *index);

        // 截到 limit，但**每条判据至少留一条**——
        // 否则第 3 条判据的结果会被前面的判据完全挤掉，用户看不到模型的另一种理解。
        let mut picked: Vec<(usize, MessageSummary)> = Vec::new();
        let mut per_label: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();
        for (index, message) in ranked.into_iter() {
            let used = per_label.entry(index).or_insert(0);
            if *used < 1 || picked.len() < limit {
                *used += 1;
                picked.push((index, message));
            }
            if picked.len() >= limit + criteria.len() {
                break;
            }
        }

        let cards: Vec<me_core::MailCardRef> = picked
            .iter()
            .map(|(index, message)| me_core::MailCardRef {
                message: message.id,
                reason: criteria[*index].label.clone(),
            })
            .collect();
        let total = cards.len() as u32;
        self.emit(Event::AiToolEmailResults {
            conversation,
            call_id: call.id.clone(),
            cards: cards.clone(),
            total,
        })
        .await;
        if let Ok(mut slot) = cards_out.lock() {
            *slot = cards;
        }

        if picked.is_empty() {
            let detail: Vec<String> = per_criterion
                .iter()
                .map(|(label, count)| format!("{label}（{count} 封）"))
                .collect();
            return Ok((
                format!(
                    "按 {} 条判据检索，本地缓存中 0 命中：{}。\
可尝试：① 放宽判据里的时间窗；② 去掉某条判据；③ 提示用户先同步最近的邮件。\
**不要用完全相同的参数再查一次**——同样的条件只会得到同样的结果。",
                    criteria.len(),
                    detail.join("、")
                ),
                "未找到（0 命中）".to_string(),
            ));
        }

        let mut text = format!(
            "按 {} 条判据检索，命中 {} 封（判据：{}）：\n",
            criteria.len(),
            total,
            per_criterion
                .iter()
                .map(|(label, count)| format!("{label} {count} 封"))
                .collect::<Vec<_>>()
                .join("、")
        );
        for (position, (criterion_index, message)) in picked.iter().enumerate() {
            text.push_str(&format!(
                "{}. [命中判据：{}] {}\n",
                position + 1,
                criteria[*criterion_index].label,
                format_message(
                    position + 1,
                    message,
                    folder_names.get(&message.folder).cloned()
                )
            ));
        }
        Ok((text, format!("命中 {total} 封")))
    }

    /// 把工具参数翻译成一次 `SearchQuery`。
    ///
    /// 普通条件与判据内部的条件走**同一套翻译**——判据只是「一组带名字的条件」，
    /// 底层没有第二种检索语义。
    async fn build_email_query(
        &self,
        args: &serde_json::Value,
        default_limit: u32,
    ) -> Result<SearchQuery, CoreError> {
        // 未知参数要点名。schema 声明了 `additionalProperties: false`，
        // 但模型仍可能写出一个不存在的参数（历史上就有 `important_only`）。
        // 只说「至少需要一个检索条件」等于让模型瞎猜——它不知道缺的是哪一类，
        // 只会换个说法再试一次。
        reject_unknown_email_args(args)?;
        let query_text = arg_str(args, "query", 200);
        let subject = arg_str(args, "subject", 200);
        let to = arg_str(args, "to", 320);
        let mut from = arg_str(args, "from", 320);
        let folder_arg = arg_str(args, "folder", 200);
        let unread_only = arg_bool(args, "unread_only").unwrap_or(false);
        let flagged_only = arg_bool(args, "flagged_only").unwrap_or(false);
        let has_attachment = arg_bool(args, "has_attachment");
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
            && has_attachment.is_none()
        {
            return Err(CoreError::validation(
                "至少需要一个检索条件：query / from / to / subject / after / before / \
days / folder / unread_only / flagged_only / has_attachment",
            ));
        }
        // `from` 允许写人名：先按实体注册表换成邮箱；换不到就按原文做子串匹配
        // （`from_addr` 里存的是 `Name <addr>`，姓名本身也能命中）。
        if let Some(raw) = from.clone() {
            from = Some(self.resolve_from_filter(raw).await);
        }
        let limit = arg_u32(args, "limit").unwrap_or(default_limit).clamp(1, 20);

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

        Ok(SearchQuery {
            text: query_text.unwrap_or_default(),
            accounts: Vec::new(),
            folders,
            since,
            until,
            has_attachment,
            from,
            to,
            subject,
            unread_only,
            flagged_only,
            limit,
            cursor: None,
        })
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

    /// 超时是**临时故障**：原样重试有用，所以它必须可重试。
    ///
    /// 之前超时走的是 `validation`，AI 层把它当成「参数不对」而不重试——
    /// 网络抖一下就要让模型重新想一遍参数，这是浪费。
    #[tokio::test]
    async fn timeout_is_retryable() {
        let err = with_timeout(Duration::from_millis(10), "天气查询", async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(("不该到这里".to_string(), String::new()))
        })
        .await
        .expect_err("必须超时");

        assert!(err.is_retryable(), "超时必须可重试：{err:?}");
        let failure = classify_tool_failure(me_core::ai::AiToolName::WeatherGet, &err);
        assert_eq!(
            failure,
            me_core::ai::ToolFailure::Transient,
            "超时归到临时故障"
        );
    }

    /// 参数错误不可重试：参数不变，结果就不会变。
    #[test]
    fn invalid_args_are_not_retryable() {
        let err = CoreError::validation("至少需要一个检索条件");
        let failure = classify_tool_failure(me_core::ai::AiToolName::EmailSearch, &err);
        assert_eq!(failure, me_core::ai::ToolFailure::InvalidArgs);
        assert!(!failure.is_auto_retryable());
    }

    /// 写操作失败**永不**自动重试：重试一次可能就真的做两遍。
    #[test]
    fn write_failures_are_never_retryable() {
        // 根因是网络抖动（本来可重试），但工具本身有副作用。
        let err = CoreError::network(true, "连接中断");
        let failure = classify_tool_failure(me_core::ai::AiToolName::CalendarCreate, &err);
        assert_eq!(failure, me_core::ai::ToolFailure::Write);
        assert!(!failure.is_auto_retryable());
    }

    /// 权限/限额类错误：改参数与重试都没用。
    #[test]
    fn denied_errors_are_not_retryable() {
        let err = CoreError::limit(me_core::LimitKind::ParseSize);
        let failure = classify_tool_failure(me_core::ai::AiToolName::MailRead, &err);
        assert_eq!(failure, me_core::ai::ToolFailure::Denied);
        assert!(!failure.is_auto_retryable());
    }

    /// 配额退回：重试是**同一次逻辑调用**，不能重复计费。
    #[test]
    fn restoring_the_quota_gives_the_slot_back() {
        let mut budget = ToolBudget {
            mail_read: 2,
            ..ToolBudget::default()
        };
        let snapshot = budget.quota_snapshot();
        budget.mail_read += 1;
        assert_eq!(budget.mail_read, 3);
        budget.restore_quota(snapshot);
        assert_eq!(
            budget.mail_read, 2,
            "重试前要退回配额，否则第二次就撞上「已读 3 次」"
        );
    }
}
