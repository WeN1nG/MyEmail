//! 日历领域模型与重复规则。
//!
//! 约束：本模块不做 IO、不依赖 tokio / rusqlite / 任何协议库。
//! 所有时间以 **UTC Unix 秒** 持久化；IANA 时区只参与「本地民用时间 → UTC」的换算。
//!
//! 三个对外函数：
//! - [`recurrence_until`]：把 RRULE 的 `UNTIL` / `COUNT` 折算成冗余上界列，
//!   供 `calendar_events.recurrence_until` 存储（见 `doc/v03架构设计.md` §4.5）；
//! - [`expand_event`]：在给定时间范围内展开一个系列，UI 与提醒循环共用；
//! - [`due_reminders`]：从已展开的 occurrence 推导到期提醒。
//!
//! 重复规则只支持 §5.2.2 的子集；不支持的规则**不丢弃原文**，展开时降级为
//! 「只显示首个实例 + warning」。

use std::fmt;

use chrono::offset::MappedLocalTime;
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::error::CoreError;

// ---------------------------------------------------------------------------
// 标识
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CalendarId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CalendarEventId(pub i64);

impl fmt::Display for CalendarId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "calendar#{}", self.0)
    }
}

impl fmt::Display for CalendarEventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "event#{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// 日历与事件
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Calendar {
    pub id: CalendarId,
    pub name: String,
    /// `#RRGGBB`。
    pub color: String,
    pub visible: bool,
    pub is_default: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 事件来源。决定是否可由 `UndoAiCalendarCreate` 撤销。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CalendarSource {
    Local,
    Ics,
    Ai,
}

impl CalendarSource {
    pub fn as_str(self) -> &'static str {
        match self {
            CalendarSource::Local => "local",
            CalendarSource::Ics => "ics",
            CalendarSource::Ai => "ai",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "local" => Some(CalendarSource::Local),
            "ics" => Some(CalendarSource::Ics),
            "ai" => Some(CalendarSource::Ai),
            _ => None,
        }
    }

    /// AI 创建的事件才允许撤销。
    pub fn is_undoable(self) -> bool {
        matches!(self, CalendarSource::Ai)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CalendarEventStatus {
    Confirmed,
    Tentative,
    Cancelled,
}

impl CalendarEventStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CalendarEventStatus::Confirmed => "confirmed",
            CalendarEventStatus::Tentative => "tentative",
            CalendarEventStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "confirmed" => Some(CalendarEventStatus::Confirmed),
            "tentative" => Some(CalendarEventStatus::Tentative),
            "cancelled" => Some(CalendarEventStatus::Cancelled),
            _ => None,
        }
    }
}

/// 完整事件行（详情用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: CalendarEventId,
    pub calendar_id: CalendarId,
    /// ICS `UID`；同一日历内未删除事件唯一。
    pub uid: String,
    pub summary: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_utc: i64,
    /// 独占边界；全天事件为结束日次日当地 00:00。
    pub end_utc: i64,
    pub all_day: bool,
    /// IANA 名称；`None` = 浮动时间（按 UTC 迭代，不参与 DST 跳变）。
    pub timezone: Option<String>,
    pub rrule: Option<String>,
    /// 冗余上界，由 [`recurrence_until`] 计算；`None` = 无限重复或无 rrule。
    pub recurrence_until: Option<i64>,
    pub exdates: Vec<i64>,
    pub reminders: Vec<u32>,
    pub status: CalendarEventStatus,
    pub source: CalendarSource,
    pub sequence: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
}

/// 列表查询用的精简行：不含描述、地点、时间戳与墓碑。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarSeries {
    pub id: CalendarEventId,
    pub calendar_id: CalendarId,
    pub summary: String,
    pub start_utc: i64,
    pub end_utc: i64,
    pub all_day: bool,
    pub timezone: Option<String>,
    pub rrule: Option<String>,
    pub recurrence_until: Option<i64>,
    pub exdates: Vec<i64>,
    pub reminders: Vec<u32>,
    pub source: CalendarSource,
}

impl CalendarEvent {
    pub fn as_series(&self) -> CalendarSeries {
        CalendarSeries {
            id: self.id,
            calendar_id: self.calendar_id,
            summary: self.summary.clone(),
            start_utc: self.start_utc,
            end_utc: self.end_utc,
            all_day: self.all_day,
            timezone: self.timezone.clone(),
            rrule: self.rrule.clone(),
            recurrence_until: self.recurrence_until,
            exdates: self.exdates.clone(),
            reminders: self.reminders.clone(),
            source: self.source,
        }
    }
}

/// 新建/编辑事件的输入。`recurrence_until` 不在此处：由用例层用
/// [`recurrence_until`] 计算后与事件同事务写入。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarEventDraft {
    pub calendar_id: CalendarId,
    pub summary: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_utc: i64,
    pub end_utc: i64,
    pub all_day: bool,
    pub timezone: Option<String>,
    pub rrule: Option<String>,
    pub exdates: Vec<i64>,
    pub reminders: Vec<u32>,
}

/// 被覆盖或被取消的单次 occurrence。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarException {
    /// 原 occurrence 的 UTC 起点，用于与展开结果匹配。
    pub recurrence_at: i64,
    pub cancelled: bool,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_utc: Option<i64>,
    pub end_utc: Option<i64>,
    pub all_day: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecurrenceEditScope {
    AllEvents,
    ThisEvent,
    ThisAndFollowing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CalendarViewKind {
    Month,
    Week,
    Agenda,
}

// ---------------------------------------------------------------------------
// 重复规则
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

impl Freq {
    fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_uppercase().as_str() {
            "DAILY" => Some(Freq::Daily),
            "WEEKLY" => Some(Freq::Weekly),
            "MONTHLY" => Some(Freq::Monthly),
            "YEARLY" => Some(Freq::Yearly),
            _ => None,
        }
    }
}

/// 重复规则的失败原因。`Unsupported` 表示「规则合法但超出 v0.3 子集」，
/// 调用方应保留原文并降级展示；`Invalid` 表示解析失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecurrenceError {
    Invalid { detail: String },
    Unsupported { detail: String },
}

impl RecurrenceError {
    pub fn invalid(detail: impl Into<String>) -> Self {
        RecurrenceError::Invalid {
            detail: detail.into(),
        }
    }

    pub fn unsupported(detail: impl Into<String>) -> Self {
        RecurrenceError::Unsupported {
            detail: detail.into(),
        }
    }

    /// 是否属于「保留原文、降级显示」而非「拒绝」。
    pub fn is_degradable(&self) -> bool {
        matches!(self, RecurrenceError::Unsupported { .. })
    }
}

impl fmt::Display for RecurrenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecurrenceError::Invalid { detail } => write!(f, "重复规则无效：{detail}"),
            RecurrenceError::Unsupported { detail } => {
                write!(f, "重复规则超出支持范围：{detail}")
            }
        }
    }
}

impl std::error::Error for RecurrenceError {}

/// 解析后的 RRULE。只保留 v0.3 子集需要的字段。
#[derive(Debug, Clone)]
pub(crate) struct ParsedRule {
    pub(crate) freq: Freq,
    pub(crate) interval: u32,
    pub(crate) count: Option<u32>,
    pub(crate) until_utc: Option<i64>,
    /// 仅在 `WEEKLY` 下生效；空表示沿用 `dtstart` 的星期。
    pub(crate) by_day: Vec<chrono::Weekday>,
    pub(crate) wkst: chrono::Weekday,
}

/// 解析 RRULE 文本。接受带或不带 `RRULE:` 前缀的形式。
///
/// 不支持的字段（`BYMONTHDAY` / `BYSETPOS` / `BYYEARDAY` / `BYWEEKNO` /
/// 带序号的 `BYDAY`）返回 [`RecurrenceError::Unsupported`]，由调用方决定降级。
pub(crate) fn parse_rrule(text: &str) -> Result<ParsedRule, RecurrenceError> {
    let body = text
        .trim()
        .strip_prefix("RRULE:")
        .unwrap_or(text.trim())
        .trim();

    if body.is_empty() {
        return Err(RecurrenceError::invalid("规则为空"));
    }

    let mut freq = None;
    let mut interval = 1u32;
    let mut count = None;
    let mut until_utc = None;
    let mut by_day = Vec::new();
    let mut wkst = chrono::Weekday::Mon;

    for part in body.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some((key, value)) = part.split_once('=') else {
            return Err(RecurrenceError::invalid(format!("缺少 `=`：{part}")));
        };
        let key = key.trim().to_ascii_uppercase();
        let value = value.trim();

        match key.as_str() {
            "FREQ" => {
                freq = Some(
                    Freq::parse(value)
                        .ok_or_else(|| RecurrenceError::unsupported(format!("FREQ={value}")))?,
                );
            }
            "INTERVAL" => {
                let parsed: u32 = value
                    .parse()
                    .map_err(|_| RecurrenceError::invalid(format!("INTERVAL={value}")))?;
                if parsed == 0 {
                    return Err(RecurrenceError::invalid("INTERVAL 不能为 0"));
                }
                interval = parsed;
            }
            "COUNT" => {
                let parsed: u32 = value
                    .parse()
                    .map_err(|_| RecurrenceError::invalid(format!("COUNT={value}")))?;
                if parsed == 0 {
                    return Err(RecurrenceError::invalid("COUNT 不能为 0"));
                }
                count = Some(parsed);
            }
            "UNTIL" => {
                until_utc = Some(parse_until(value)?);
            }
            "BYDAY" => {
                for token in value.split(',') {
                    let token = token.trim();
                    if token.is_empty() {
                        continue;
                    }
                    by_day.push(parse_weekday(token)?);
                }
            }
            "WKST" => {
                wkst = parse_weekday(value)?;
            }
            // 已知但不支持的扩展规则。
            "BYMONTHDAY" | "BYSETPOS" | "BYYEARDAY" | "BYWEEKNO" | "BYMONTH" | "BYHOUR"
            | "BYMINUTE" | "BYSECOND" => {
                return Err(RecurrenceError::unsupported(format!("{key}={value}")));
            }
            // 未知属性按规范忽略，但保守起见视为不支持，避免静默改变语义。
            _ => return Err(RecurrenceError::unsupported(key)),
        }
    }

    let freq = freq.ok_or_else(|| RecurrenceError::invalid("缺少 FREQ"))?;

    if freq != Freq::Weekly && !by_day.is_empty() {
        // `BYDAY` 只在 WEEKLY 下有意义；其他频率交给调用方降级。
        return Err(RecurrenceError::unsupported("BYDAY 仅支持 WEEKLY"));
    }

    Ok(ParsedRule {
        freq,
        interval,
        count,
        until_utc,
        by_day,
        wkst,
    })
}

fn parse_weekday(token: &str) -> Result<chrono::Weekday, RecurrenceError> {
    // 带序号的 BYDAY（如 `2MO`）不在子集内。
    if token.len() != 2 {
        return Err(RecurrenceError::unsupported(format!("BYDAY={token}")));
    }
    match token.to_ascii_uppercase().as_str() {
        "MO" => Ok(chrono::Weekday::Mon),
        "TU" => Ok(chrono::Weekday::Tue),
        "WE" => Ok(chrono::Weekday::Wed),
        "TH" => Ok(chrono::Weekday::Thu),
        "FR" => Ok(chrono::Weekday::Fri),
        "SA" => Ok(chrono::Weekday::Sat),
        "SU" => Ok(chrono::Weekday::Sun),
        _ => Err(RecurrenceError::invalid(format!("未知星期：{token}"))),
    }
}

/// `UNTIL` 支持 `YYYYMMDD`（当天结束）与 `YYYYMMDDTHHMMSSZ`。
fn parse_until(value: &str) -> Result<i64, RecurrenceError> {
    if let Ok(dt) = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ") {
        return Ok(dt.and_utc().timestamp());
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y%m%d") {
        // DATE 形式表示「该日期结束」，取 23:59:59 作为保守上界。
        let end = date
            .and_hms_opt(23, 59, 59)
            .ok_or_else(|| RecurrenceError::invalid(format!("UNTIL={value}")))?;
        return Ok(end.and_utc().timestamp());
    }
    Err(RecurrenceError::invalid(format!("UNTIL={value}")))
}

// ---------------------------------------------------------------------------
// 展开
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecurrenceLimits {
    /// 最多迭代多少个候选（含范围外），防止无限规则拖死 UI。
    pub max_scan: u32,
    /// 最多返回多少个 occurrence。
    pub max_instances: u32,
}

impl Default for RecurrenceLimits {
    fn default() -> Self {
        Self {
            max_scan: 10_000,
            max_instances: 2_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventOccurrence {
    pub start_utc: i64,
    pub end_utc: i64,
    pub all_day: bool,
    /// 该次被例外覆盖（标题/时间可能来自 exception）。
    pub overridden: bool,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecurrenceWarning {
    /// 规则超出子集：只显示首个实例。
    Unsupported { detail: String },
    /// 触发上限，结果被截断。
    Truncated { scanned: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpandedEvent {
    pub occurrences: Vec<EventOccurrence>,
    pub warnings: Vec<RecurrenceWarning>,
    pub truncated: bool,
}

impl ExpandedEvent {
    fn single(series: &CalendarSeries) -> Self {
        Self {
            occurrences: vec![EventOccurrence {
                start_utc: series.start_utc,
                end_utc: series.end_utc,
                all_day: series.all_day,
                overridden: false,
                summary: None,
            }],
            warnings: Vec::new(),
            truncated: false,
        }
    }
}

/// 解析事件时区。`None`（浮动时间）按 UTC 迭代：不产生 DST 跳变，
/// 显示时由 UI 按本机时区换算。
fn series_tz(timezone: Option<&str>) -> Tz {
    timezone
        .and_then(|name| name.parse::<Tz>().ok())
        .unwrap_or(chrono_tz::UTC)
}

/// 本地民用时间 → UTC。DST 规则见 `doc/v03架构设计.md` §5.2.2：
/// - 春季不存在的时刻顺延到跳变后第一个有效时刻；
/// - 秋季重复的时刻取第一次出现（较早的 UTC）。
fn local_to_utc(naive: NaiveDateTime, tz: Tz) -> i64 {
    match tz.from_local_datetime(&naive) {
        MappedLocalTime::Single(dt) => dt.timestamp(),
        MappedLocalTime::Ambiguous(earlier, _later) => earlier.timestamp(),
        MappedLocalTime::None => {
            // 逐分钟向前探测，最多 3 小时（覆盖所有现实 DST 跳变量）。
            let mut probe = naive;
            for _ in 0..(3 * 60) {
                probe += Duration::minutes(1);
                match tz.from_local_datetime(&probe) {
                    MappedLocalTime::Single(dt) => return dt.timestamp(),
                    MappedLocalTime::Ambiguous(earlier, _) => return earlier.timestamp(),
                    MappedLocalTime::None => continue,
                }
            }
            // 极端时区数据缺失：退回把本地时间当作 UTC，保证不 panic。
            naive.and_utc().timestamp()
        }
    }
}

fn utc_to_local(ts: i64, tz: Tz) -> DateTime<Tz> {
    Utc.timestamp_opt(ts, 0)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap())
        .with_timezone(&tz)
}

/// 按 `wkst` 对齐到所在周的起始日。
fn week_start(date: NaiveDate, wkst: chrono::Weekday) -> NaiveDate {
    let offset = (date.weekday().num_days_from_monday() as i64
        - wkst.num_days_from_monday() as i64)
        .rem_euclid(7);
    date - Duration::days(offset)
}

/// 重复规则的候选迭代器。
///
/// 以「周期索引 + 本地民用时间」推进，而不是移动日期游标：`MONTHLY` 遇到
/// 不存在的日期（1/31 → 2 月）时只需跳过该周期，游标不会漂移成 2/28。
struct RecurrenceIter {
    rule: ParsedRule,
    tz: Tz,
    /// 首次 occurrence 的 UTC 起点（系列自身）。
    dtstart_utc: i64,
    /// `dtstart` 的本地日期，所有候选由它派生。
    start_date: NaiveDate,
    /// 本地时间部分（所有候选共用）。
    time_of_day: chrono::NaiveTime,
    /// 第几个周期（从 0 开始）。
    period: u32,
    /// WEEKLY + BYDAY 的周内槽位。
    slots: Vec<chrono::Weekday>,
    slot_index: usize,
    /// WEEKLY 的周起点（按 `WKST` 对齐）。
    week_anchor: NaiveDate,
    /// 已产出数量（用于 COUNT）。
    emitted: u32,
    /// 已产出候选数（用于 `max_scan` 保护）。
    scanned: u32,
    finished: bool,
}

/// 病态规则的硬上限，防止任何情况下死循环。
const HARD_SCAN_LIMIT: u32 = 200_000;

impl RecurrenceIter {
    fn new(rule: ParsedRule, dtstart_utc: i64, tz: Tz) -> Self {
        let start_local = utc_to_local(dtstart_utc, tz).naive_local();
        let mut slots = rule.by_day.clone();
        slots.sort_by_key(|d| d.num_days_from_monday());
        slots.dedup();
        if rule.freq == Freq::Weekly && slots.is_empty() {
            slots.push(start_local.date().weekday());
        }
        Self {
            tz,
            dtstart_utc,
            start_date: start_local.date(),
            time_of_day: start_local.time(),
            period: 0,
            slots,
            slot_index: 0,
            week_anchor: week_start(start_local.date(), rule.wkst),
            emitted: 0,
            scanned: 0,
            finished: false,
            rule,
        }
    }

    /// 生成下一个候选的本地日期时间；`None` 表示规则耗尽。
    fn next_naive(&mut self) -> Option<NaiveDateTime> {
        loop {
            if self.finished || self.scanned >= HARD_SCAN_LIMIT {
                self.finished = true;
                return None;
            }

            let Some(candidate) = self.next_period_date() else {
                self.finished = true;
                return None;
            };

            let Some(date) = candidate else {
                // 该周期没有对应日期（如 MONTHLY 的 2 月 31 日）：跳过。
                continue;
            };

            self.scanned = self.scanned.saturating_add(1);
            let naive = NaiveDateTime::new(date, self.time_of_day);
            let start_utc = local_to_utc(naive, self.tz);

            // WEEKLY 首周可能包含早于 dtstart 的槽位。
            if start_utc < self.dtstart_utc {
                continue;
            }

            self.emitted = self.emitted.saturating_add(1);

            if let Some(until) = self.rule.until_utc
                && start_utc > until
            {
                self.finished = true;
                return None;
            }
            if let Some(count) = self.rule.count
                && self.emitted > count
            {
                self.finished = true;
                return None;
            }

            return Some(naive);
        }
    }

    /// 取当前周期的日期；`Some(None)` 表示该周期无效需跳过。
    fn next_period_date(&mut self) -> Option<Option<NaiveDate>> {
        match self.rule.freq {
            Freq::Daily => {
                let date = self.start_date
                    + Duration::days(self.period as i64 * self.rule.interval as i64);
                self.period = self.period.saturating_add(1);
                Some(Some(date))
            }
            Freq::Weekly => {
                if self.slot_index >= self.slots.len() {
                    self.slot_index = 0;
                    self.period = self.period.saturating_add(1);
                }
                let weekday = self.slots[self.slot_index];
                self.slot_index += 1;
                let week = self.week_anchor
                    + Duration::days(self.period as i64 * self.rule.interval as i64 * 7);
                let offset = (weekday.num_days_from_monday() as i64
                    - week.weekday().num_days_from_monday() as i64)
                    .rem_euclid(7);
                Some(Some(week + Duration::days(offset)))
            }
            Freq::Monthly => {
                let index = self.start_date.year() as i64 * 12
                    + (self.start_date.month() as i64 - 1)
                    + self.period as i64 * self.rule.interval as i64;
                self.period = self.period.saturating_add(1);
                let year = i32::try_from(index.div_euclid(12)).ok()?;
                let month = (index.rem_euclid(12) + 1) as u32;
                Some(NaiveDate::from_ymd_opt(year, month, self.start_date.day()))
            }
            Freq::Yearly => {
                let year = self.start_date.year()
                    + (self.period as i64 * self.rule.interval as i64) as i32;
                self.period = self.period.saturating_add(1);
                Some(NaiveDate::from_ymd_opt(
                    year,
                    self.start_date.month(),
                    self.start_date.day(),
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// recurrence_until
// ---------------------------------------------------------------------------

/// 计算 `calendar_events.recurrence_until` 冗余上界。
///
/// - 无 `rrule` → `None`；
/// - 有 `UNTIL` → 直接取该值（保守上界）；
/// - 有 `COUNT` → 迭代到第 N 次的 UTC 起点；
/// - 既无 `UNTIL` 也无 `COUNT`（无限重复）→ `None`。
///
/// 迭代受 `max_scan` 保护。若 `COUNT` 过大导致扫描超限，返回 `None`
/// （按无限重复处理）：宁可多扫，也不要用偏小的上界把系列提前判死、
/// 导致提醒永久漏发。
pub fn recurrence_until(
    rrule: Option<&str>,
    dtstart_utc: i64,
    timezone: Option<&str>,
) -> Result<Option<i64>, RecurrenceError> {
    let Some(text) = rrule else {
        return Ok(None);
    };
    let rule = parse_rrule(text)?;

    if let Some(until) = rule.until_utc {
        return Ok(Some(until));
    }
    let Some(count) = rule.count else {
        return Ok(None);
    };

    let tz = series_tz(timezone);
    let mut iter = RecurrenceIter::new(rule, dtstart_utc, tz);
    let mut last = dtstart_utc;
    let mut produced = 0u32;
    let limit = RecurrenceLimits::default().max_scan;

    while produced < count {
        if produced >= limit {
            // 规则过大：按无限重复处理，避免写入偏小的上界。
            return Ok(None);
        }
        let Some(naive) = iter.next_naive() else {
            break;
        };
        last = local_to_utc(naive, tz);
        produced += 1;
    }

    if produced < count {
        // 规则提前耗尽（如 MONTHLY 遇到不存在的日期），最后一个是实际末次。
        return Ok(if produced == 0 { None } else { Some(last) });
    }
    Ok(Some(last))
}

// ---------------------------------------------------------------------------
// expand_event
// ---------------------------------------------------------------------------

/// 在 `[from_utc, to_utc]` 内展开一个系列。
///
/// 不返回 `Result`：不支持的规则降级为「首个实例 + warning」，与
/// `doc/v03架构设计.md` §5.2.2 的降级口径一致。
pub fn expand_event(
    series: &CalendarSeries,
    exceptions: &[CalendarException],
    from_utc: i64,
    to_utc: i64,
    limits: RecurrenceLimits,
) -> ExpandedEvent {
    let Some(rrule) = series.rrule.as_deref() else {
        // 非重复事件：只在范围相交时返回。
        if series.end_utc >= from_utc && series.start_utc <= to_utc {
            return ExpandedEvent::single(series);
        }
        return ExpandedEvent {
            occurrences: Vec::new(),
            warnings: Vec::new(),
            truncated: false,
        };
    };

    let rule = match parse_rrule(rrule) {
        Ok(rule) => rule,
        Err(err) => {
            // 降级：保留主行，只显示首个实例。
            let mut expanded = ExpandedEvent {
                occurrences: Vec::new(),
                warnings: vec![RecurrenceWarning::Unsupported {
                    detail: err.to_string(),
                }],
                truncated: false,
            };
            if series.end_utc >= from_utc && series.start_utc <= to_utc {
                expanded.occurrences = ExpandedEvent::single(series).occurrences;
            }
            return expanded;
        }
    };

    let tz = series_tz(series.timezone.as_deref());
    let duration = series.end_utc - series.start_utc;
    let mut iter = RecurrenceIter::new(rule, series.start_utc, tz);

    let mut occurrences = Vec::new();
    let mut warnings = Vec::new();
    let mut truncated = false;

    while let Some(naive) = iter.next_naive() {
        if iter.scanned >= limits.max_scan {
            truncated = true;
            warnings.push(RecurrenceWarning::Truncated {
                scanned: iter.scanned,
            });
            break;
        }

        let start_utc = local_to_utc(naive, tz);
        if start_utc > to_utc {
            // 已越过范围上界；后续只会更晚。
            break;
        }
        if series.exdates.contains(&start_utc) {
            continue;
        }

        let exception = exceptions.iter().find(|e| e.recurrence_at == start_utc);
        if let Some(exception) = exception
            && exception.cancelled
        {
            continue;
        }

        let (occ_start, occ_end, all_day, overridden, summary) = match exception {
            Some(exception) => (
                exception.start_utc.unwrap_or(start_utc),
                exception.end_utc.unwrap_or(start_utc + duration),
                exception.all_day.unwrap_or(series.all_day),
                true,
                exception.summary.clone(),
            ),
            None => (start_utc, start_utc + duration, series.all_day, false, None),
        };

        // 覆盖后的时间可能移出范围，仍需按最终时间判断。
        if occ_end >= from_utc {
            occurrences.push(EventOccurrence {
                start_utc: occ_start,
                end_utc: occ_end,
                all_day,
                overridden,
                summary,
            });
            if occurrences.len() >= limits.max_instances as usize {
                truncated = true;
                warnings.push(RecurrenceWarning::Truncated {
                    scanned: iter.scanned,
                });
                break;
            }
        }
    }

    ExpandedEvent {
        occurrences,
        warnings,
        truncated,
    }
}

// ---------------------------------------------------------------------------
// 提醒
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DueReminder {
    pub occurrence_at: i64,
    pub minutes_before: u32,
    /// 应当触发通知的时刻（UTC）。
    pub due_at: i64,
}

/// 从已展开的 occurrence 推导 `[window_start, window_end]` 内到期的提醒。
///
/// 只负责算术；去重由 `calendar_reminder_fires` 主键保证。
pub fn due_reminders(
    occurrences: &[EventOccurrence],
    reminders: &[u32],
    window_start: i64,
    window_end: i64,
) -> Vec<DueReminder> {
    let mut due = Vec::new();
    for occurrence in occurrences {
        for &minutes in reminders {
            let due_at = occurrence.start_utc.saturating_sub(minutes as i64 * 60);
            if due_at >= window_start && due_at <= window_end {
                due.push(DueReminder {
                    occurrence_at: occurrence.start_utc,
                    minutes_before: minutes,
                    due_at,
                });
            }
        }
    }
    due
}

/// 事件时区是否可用。用于写入前校验。
pub fn is_valid_timezone(name: &str) -> bool {
    name.parse::<Tz>().is_ok()
}

// ---------------------------------------------------------------------------
// 写入校验
// ---------------------------------------------------------------------------

/// 标题上限（字符数）。
pub const MAX_SUMMARY_CHARS: usize = 200;
/// 单事件最多提醒数。
pub const MAX_REMINDERS: usize = 3;
/// 提醒提前量上限：7 天。
pub const MAX_REMINDER_MINUTES: u32 = 7 * 24 * 60;

/// 写入前的纯校验。**用户口径**的错误消息，可直接展示。
///
/// 放在 `me-core` 而不是用例层：界面可以先用它做即时提示，用例再用它兜底，
/// 两条路径的判断因此不会分叉。
pub fn validate_draft(draft: &CalendarEventDraft) -> Result<(), CoreError> {
    if draft.summary.trim().is_empty() {
        return Err(CoreError::validation("日程标题不能为空"));
    }
    if draft.summary.chars().count() > MAX_SUMMARY_CHARS {
        return Err(CoreError::validation(format!(
            "日程标题过长（上限 {MAX_SUMMARY_CHARS} 字）"
        )));
    }
    if draft.end_utc < draft.start_utc {
        return Err(CoreError::validation("结束时间不能早于开始时间"));
    }
    if draft.reminders.len() > MAX_REMINDERS {
        return Err(CoreError::validation(format!(
            "最多 {MAX_REMINDERS} 个提醒"
        )));
    }
    if draft.reminders.iter().any(|m| *m > MAX_REMINDER_MINUTES) {
        return Err(CoreError::validation("提醒提前量不能超过 7 天"));
    }
    if let Some(tz) = draft.timezone.as_deref()
        && !is_valid_timezone(tz)
    {
        return Err(CoreError::validation(format!("未知时区：{tz}")));
    }
    if let Some(rule) = draft.rrule.as_deref() {
        if rule.trim().is_empty() {
            return Err(CoreError::validation("重复规则不能为空"));
        }
        // 语法不合法（而非「超出子集」）时拒绝写入，避免存下无法展开的规则。
        if let Err(err) = parse_rrule(rule)
            && !err.is_degradable()
        {
            return Err(CoreError::validation(format!("{err}")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEW_YORK: &str = "America/New_York";

    /// 把「纽约本地时间」转成 UTC 秒，用于构造系列起点与查询范围。
    fn ny(text: &str) -> i64 {
        let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap();
        Tz::America__New_York
            .from_local_datetime(&naive)
            .earliest()
            .expect("本地时间必须存在")
            .timestamp()
    }

    /// 把 UTC 秒渲染成纽约本地时间，便于断言。
    fn ny_str(ts: i64) -> String {
        Utc.timestamp_opt(ts, 0)
            .single()
            .unwrap()
            .with_timezone(&Tz::America__New_York)
            .format("%Y-%m-%d %H:%M")
            .to_string()
    }

    fn series(rrule: &str, start_local: &str) -> CalendarSeries {
        let start_utc = ny(start_local);
        CalendarSeries {
            id: CalendarEventId(1),
            calendar_id: CalendarId(1),
            summary: "测试".into(),
            start_utc,
            end_utc: start_utc + 3600,
            all_day: false,
            timezone: Some(NEW_YORK.into()),
            rrule: Some(rrule.into()),
            recurrence_until: None,
            exdates: Vec::new(),
            reminders: Vec::new(),
            source: CalendarSource::Local,
        }
    }

    fn expand(s: &CalendarSeries, from_local: &str, to_local: &str) -> ExpandedEvent {
        expand_event(
            s,
            &[],
            ny(from_local),
            ny(to_local),
            RecurrenceLimits::default(),
        )
    }

    fn starts(out: &ExpandedEvent) -> Vec<String> {
        out.occurrences
            .iter()
            .map(|o| ny_str(o.start_utc))
            .collect()
    }

    #[test]
    fn daily_interval_and_count() {
        let s = series("FREQ=DAILY;INTERVAL=2;COUNT=3", "2026-01-01 09:00");
        let out = expand(&s, "2026-01-01 00:00", "2026-01-31 00:00");
        assert_eq!(
            starts(&out),
            vec!["2026-01-01 09:00", "2026-01-03 09:00", "2026-01-05 09:00"]
        );
    }

    #[test]
    fn weekly_by_day() {
        // 2026-01-01 是周四。
        let s = series("FREQ=WEEKLY;BYDAY=MO,WE", "2026-01-01 10:00");
        let out = expand(&s, "2026-01-01 00:00", "2026-01-15 00:00");
        assert_eq!(
            starts(&out),
            vec![
                "2026-01-05 10:00", // 周一
                "2026-01-07 10:00", // 周三
                "2026-01-12 10:00",
                "2026-01-14 10:00",
            ]
        );
    }

    #[test]
    fn weekly_interval_skips_weeks() {
        let s = series("FREQ=WEEKLY;INTERVAL=2;BYDAY=TH", "2026-01-01 10:00");
        let out = expand(&s, "2026-01-01 00:00", "2026-02-01 00:00");
        assert_eq!(
            starts(&out),
            vec!["2026-01-01 10:00", "2026-01-15 10:00", "2026-01-29 10:00",]
        );
    }

    #[test]
    fn monthly_skips_missing_day() {
        // 1/31 起每月重复：2 月没有 31 日，应跳过该周期而不是结束系列。
        let s = series("FREQ=MONTHLY;COUNT=3", "2026-01-31 08:00");
        let out = expand(&s, "2026-01-01 00:00", "2026-12-31 00:00");
        assert_eq!(
            starts(&out),
            vec!["2026-01-31 08:00", "2026-03-31 08:00", "2026-05-31 08:00"]
        );
    }

    #[test]
    fn yearly_leap_day_skips_non_leap() {
        let s = series("FREQ=YEARLY;COUNT=2", "2024-02-29 12:00");
        let out = expand(&s, "2024-01-01 00:00", "2032-12-31 00:00");
        assert_eq!(starts(&out), vec!["2024-02-29 12:00", "2028-02-29 12:00"]);
    }

    #[test]
    fn until_is_inclusive_boundary() {
        // 系列每日 09:00 EST = 14:00 UTC；UNTIL 恰好等于第三次。
        let s = series("FREQ=DAILY;UNTIL=20260103T140000Z", "2026-01-01 09:00");
        let out = expand(&s, "2026-01-01 00:00", "2026-01-31 00:00");
        assert_eq!(out.occurrences.len(), 3);
    }

    #[test]
    fn exdate_removes_occurrence() {
        let mut s = series("FREQ=DAILY;COUNT=3", "2026-01-01 09:00");
        s.exdates.push(ny("2026-01-02 09:00"));
        let out = expand(&s, "2026-01-01 00:00", "2026-01-31 00:00");
        assert_eq!(starts(&out), vec!["2026-01-01 09:00", "2026-01-03 09:00"]);
    }

    #[test]
    fn exception_replaces_time() {
        let s = series("FREQ=DAILY;COUNT=3", "2026-01-01 09:00");
        let exceptions = vec![CalendarException {
            recurrence_at: ny("2026-01-02 09:00"),
            cancelled: false,
            summary: Some("改期".into()),
            description: None,
            location: None,
            start_utc: Some(ny("2026-01-02 15:00")),
            end_utc: Some(ny("2026-01-02 16:00")),
            all_day: None,
        }];
        let out = expand_event(
            &s,
            &exceptions,
            ny("2026-01-01 00:00"),
            ny("2026-01-31 00:00"),
            RecurrenceLimits::default(),
        );
        assert_eq!(out.occurrences.len(), 3);
        assert_eq!(ny_str(out.occurrences[1].start_utc), "2026-01-02 15:00");
        assert!(out.occurrences[1].overridden);
    }

    #[test]
    fn cancelled_exception_is_dropped() {
        let s = series("FREQ=DAILY;COUNT=3", "2026-01-01 09:00");
        let exceptions = vec![CalendarException {
            recurrence_at: ny("2026-01-02 09:00"),
            cancelled: true,
            summary: None,
            description: None,
            location: None,
            start_utc: None,
            end_utc: None,
            all_day: None,
        }];
        let out = expand_event(
            &s,
            &exceptions,
            ny("2026-01-01 00:00"),
            ny("2026-01-31 00:00"),
            RecurrenceLimits::default(),
        );
        assert_eq!(out.occurrences.len(), 2);
    }

    #[test]
    fn dst_spring_forward_shifts_missing_hour() {
        // 2026-03-08 02:30 在纽约不存在（02:00 → 03:00）。
        let s = series("FREQ=DAILY;COUNT=2", "2026-03-07 02:30");
        let out = expand(&s, "2026-03-06 00:00", "2026-03-10 00:00");
        assert_eq!(out.occurrences.len(), 2);
        assert_eq!(ny_str(out.occurrences[1].start_utc), "2026-03-08 03:00");
    }

    #[test]
    fn dst_fall_back_picks_first_occurrence() {
        // 2025-11-02 01:30 出现两次；应取较早的一次（EDT, UTC-4）。
        let s = series("FREQ=DAILY;COUNT=1", "2025-11-02 01:30");
        let out = expand(&s, "2025-11-02 00:00", "2025-11-02 23:00");
        assert_eq!(out.occurrences.len(), 1);
        let local = Utc
            .timestamp_opt(out.occurrences[0].start_utc, 0)
            .single()
            .unwrap()
            .with_timezone(&Tz::America__New_York);
        assert_eq!(local.format("%H:%M").to_string(), "01:30");
        assert_eq!(local.format("%z").to_string(), "-0400");
    }

    #[test]
    fn unsupported_rule_degrades_to_single() {
        let s = series("FREQ=MONTHLY;BYMONTHDAY=15", "2026-01-15 09:00");
        let out = expand(&s, "2026-01-01 00:00", "2026-12-31 00:00");
        assert_eq!(out.occurrences.len(), 1);
        assert!(matches!(
            out.warnings.first(),
            Some(RecurrenceWarning::Unsupported { .. })
        ));
    }

    #[test]
    fn invalid_rule_is_rejected() {
        assert!(parse_rrule("FREQ=HOURLY").is_err());
        assert!(parse_rrule("INTERVAL=2").is_err());
        assert!(parse_rrule("FREQ=DAILY;INTERVAL=0").is_err());
        assert!(parse_rrule("").is_err());
        assert!(parse_rrule("FREQ=WEEKLY;BYDAY=2MO").is_err());
    }

    #[test]
    fn recurrence_until_for_until() {
        let until = recurrence_until(
            Some("FREQ=DAILY;UNTIL=20260110T090000Z"),
            ny("2026-01-01 09:00"),
            Some(NEW_YORK),
        )
        .unwrap()
        .unwrap();
        assert_eq!(until, ny("2026-01-10 04:00"));
    }

    #[test]
    fn recurrence_until_for_count() {
        let until = recurrence_until(
            Some("FREQ=DAILY;COUNT=5"),
            ny("2026-01-01 09:00"),
            Some(NEW_YORK),
        )
        .unwrap()
        .unwrap();
        assert_eq!(ny_str(until), "2026-01-05 09:00");
    }

    #[test]
    fn recurrence_until_none_for_infinite_and_absent() {
        assert_eq!(recurrence_until(None, 0, None).unwrap(), None);
        assert_eq!(
            recurrence_until(Some("FREQ=WEEKLY"), ny("2026-01-01 09:00"), None).unwrap(),
            None
        );
    }

    #[test]
    fn recurrence_until_none_when_count_exceeds_scan_limit() {
        // COUNT 极大：返回 None（按无限处理），不写入偏小上界。
        let until = recurrence_until(
            Some("FREQ=DAILY;COUNT=1000000"),
            ny("2020-01-01 09:00"),
            None,
        )
        .unwrap();
        assert_eq!(until, None);
    }

    #[test]
    fn due_reminders_window() {
        let occurrences = vec![EventOccurrence {
            start_utc: ny("2026-01-01 10:00"),
            end_utc: ny("2026-01-01 11:00"),
            all_day: false,
            overridden: false,
            summary: None,
        }];
        let due = due_reminders(
            &occurrences,
            &[15, 60],
            ny("2026-01-01 09:00"),
            ny("2026-01-01 10:00"),
        );
        assert_eq!(due.len(), 2);
        assert_eq!(due[0].due_at, ny("2026-01-01 09:45"));
        assert_eq!(due[1].due_at, ny("2026-01-01 09:00"));
    }

    #[test]
    fn all_day_series_keeps_local_midnight() {
        let start = ny("2026-06-01 00:00");
        let s = CalendarSeries {
            id: CalendarEventId(1),
            calendar_id: CalendarId(1),
            summary: "全天".into(),
            start_utc: start,
            end_utc: ny("2026-06-02 00:00"),
            all_day: true,
            timezone: Some(NEW_YORK.into()),
            rrule: Some("FREQ=DAILY;COUNT=2".into()),
            recurrence_until: None,
            exdates: Vec::new(),
            reminders: Vec::new(),
            source: CalendarSource::Local,
        };
        let out = expand(&s, "2026-06-01 00:00", "2026-06-10 00:00");
        assert_eq!(out.occurrences.len(), 2);
        assert!(out.occurrences.iter().all(|o| o.all_day));
    }

    #[test]
    fn non_recurring_outside_range_is_empty() {
        let mut s = series("FREQ=DAILY", "2026-01-01 09:00");
        s.rrule = None;
        let out = expand(&s, "2026-02-01 00:00", "2026-02-28 00:00");
        assert!(out.occurrences.is_empty());
        assert!(out.warnings.is_empty());
    }
}
