//! 任务清单（plan）：模型把一次需求拆成步骤，引擎逐步推进。
//!
//! 为什么是**独立模块**而不是塞进 `ai.rs`：这是一套自洽的状态机（步骤状态只能前进、
//! 标题创建后不可变），校验逻辑是纯函数，可以脱离运行时单测。`ai.rs` 已经很长了。
//!
//! 落库形态就是这里的 [`Plan`]（JSON，写在 `ai_messages.plan`），
//! 模型的入参是它的**子集**：`id` / `created_at` / `updated_at` 由引擎填。

use serde::{Deserialize, Serialize};

use crate::CoreError;
use crate::ai::AiToolName;
use crate::ai::truncate_chars;

/// 清单版本。v1 只有 title/status，**不可执行**（只用于展示老数据）。
pub const PLAN_VERSION_V1: u8 = 1;
pub const PLAN_VERSION_V2: u8 = 2;

/// 一条清单最多几步。超过这个数就不是「清单」而是「项目计划」了。
///
/// 6 → 8：「整理重要邮件」这类真实需求就要 5 步，6 步对跨模块的需求偏紧。
pub const MAX_PLAN_STEPS: usize = 8;
pub const MAX_PLAN_GOAL_CHARS: usize = 120;
pub const MAX_STEP_TITLE_CHARS: usize = 40;
pub const MAX_STEP_EVIDENCE_CHARS: usize = 120;
pub const MAX_STEP_EXPECT_CHARS: usize = 120;
/// 单步参数模板的字节上限。
pub const MAX_STEP_ARGS_BYTES: usize = 1024;
/// 单步最多尝试几次（含第一次）。
pub const MAX_STEP_ATTEMPTS: u8 = 3;
/// 一轮对话最多重列几次。
pub const MAX_REPLANS: u8 = 2;
/// `Plan.history` 最多留几份旧计划——防止 plan JSON 无限膨胀。
pub const MAX_PLAN_HISTORY: usize = 2;
/// `StepFailure` 里 `detail` 的截断长度。
///
/// 这个上限是**落库**用的，不是回灌用的：`MAX_TOOL_RESULT_CHARS` 管的是给模型看的
/// 工具结果，把它存进 plan 会把整段响应体带进数据库。
pub const MAX_STEP_FAILURE_CHARS: usize = 80;

/// 步骤归属的能力模块。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Mail,
    Weather,
    Calendar,
    Music,
    /// 不属于任何特定能力：推理归纳（`kind=reason`）、向用户提问（`kind=ask`）。
    ///
    /// 这两类步骤不调工具，硬让模型填一个领域只会让它瞎猜——
    /// 2026-11-07 的事故里，模型为了一步 `reason` 连续改了 4 次
    /// `capability`（试过省略、试过空串、试过改成 tool 步），白白烧掉
    /// 4 次工具往返。推理步骤本来就没有「属于哪个工具模块」的概念。
    General,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::Mail => "mail",
            Capability::Weather => "weather",
            Capability::Calendar => "calendar",
            Capability::Music => "music",
            Capability::General => "general",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "mail" => Some(Capability::Mail),
            "weather" => Some(Capability::Weather),
            "calendar" => Some(Capability::Calendar),
            "music" => Some(Capability::Music),
            "general" => Some(Capability::General),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Capability::Mail => "邮件",
            Capability::Weather => "天气",
            Capability::Calendar => "日历",
            Capability::Music => "音乐",
            Capability::General => "通用",
        }
    }
}

/// 步骤状态。只能前进，不能回退。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

impl PlanStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanStatus::Pending => "pending",
            PlanStatus::Running => "running",
            PlanStatus::Done => "done",
            PlanStatus::Failed => "failed",
            PlanStatus::Skipped => "skipped",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(PlanStatus::Pending),
            "running" => Some(PlanStatus::Running),
            "done" => Some(PlanStatus::Done),
            "failed" => Some(PlanStatus::Failed),
            "skipped" => Some(PlanStatus::Skipped),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PlanStatus::Pending => "待执行",
            PlanStatus::Running => "进行中",
            PlanStatus::Done => "已完成",
            PlanStatus::Failed => "失败",
            PlanStatus::Skipped => "跳过",
        }
    }

    /// 已结束的状态：再改就是回退。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            PlanStatus::Done | PlanStatus::Failed | PlanStatus::Skipped
        )
    }

    /// 前进程度。终态并列，因此「Done → Failed」要靠 [`Self::is_terminal`] 拦。
    fn rank(self) -> u8 {
        match self {
            PlanStatus::Pending => 0,
            PlanStatus::Running => 1,
            PlanStatus::Done | PlanStatus::Failed | PlanStatus::Skipped => 2,
        }
    }
}

/// 这一步「做什么」。决定引擎在执行期允许什么样的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// 调一个工具。`tool` 必填。
    #[default]
    Tool,
    /// 纯推理：归纳、排序、写理由。**不允许**出现工具调用。
    Reason,
    /// 需要用户补充信息才能继续：执行到这一步就整轮暂停，等用户回答。
    Ask,
}

impl StepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StepKind::Tool => "tool",
            StepKind::Reason => "reason",
            StepKind::Ask => "ask",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "tool" => Some(StepKind::Tool),
            "reason" => Some(StepKind::Reason),
            "ask" => Some(StepKind::Ask),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StepKind::Tool => "工具",
            StepKind::Reason => "推理",
            StepKind::Ask => "待确认",
        }
    }

    /// 是否是「要调工具」的步骤。只有工具步需要 `capability`。
    pub fn is_tool(self) -> bool {
        matches!(self, StepKind::Tool)
    }
}

/// 0 命中（或者条件太窄）时的策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnEmpty {
    /// 缩小条件再来一次（用掉一次 attempt）
    #[default]
    Narrow,
    /// 停下来问用户
    Ask,
    /// 接受空结果，继续下一步
    Accept,
}

impl OnEmpty {
    pub fn as_str(self) -> &'static str {
        match self {
            OnEmpty::Narrow => "narrow",
            OnEmpty::Ask => "ask",
            OnEmpty::Accept => "accept",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "narrow" | "retry" => Some(OnEmpty::Narrow),
            "ask" => Some(OnEmpty::Ask),
            "accept" | "continue" => Some(OnEmpty::Accept),
            _ => None,
        }
    }
}

/// 一条**判据**：把用户口语里的定性词翻译成的、可执行的条件组。
///
/// 为什么要有这个类型：用户说的是「重要的邮件」「该处理的」「紧急的」，
/// 这些词在邮件表里**没有对应字段**。模型必须自己决定「重要」等于
/// 「已星标」还是「近 7 天未读」还是「主题含合同」——这个决定不能由
/// 引擎代劳（代劳就等于把某一种口径写死，别的需求就用不上了）。
///
/// 一条判据 = 一个**人能读懂的名字** + 一组**机器能执行的条件**。
/// 名字是关键：它出现在清单条与每条结果的「命中理由」里，
/// 用户据此判断模型到底把「重要」理解成了什么。
///
/// 判据是 `email_search` 参数的一部分（`args.criteria`），**不另设计划字段**——
/// 同一个东西写两遍，模型一定会写歪，而清单条显示的那份会与真正执行的那份对不上。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Criterion {
    /// 这条判据的名字（≤ 24 字），给人看。
    #[serde(default)]
    pub label: String,
    /// 这条判据对应的检索条件（`email_search` 的参数子集）。
    #[serde(default)]
    pub query: serde_json::Value,
}

impl Criterion {
    pub fn new(label: &str, query: serde_json::Value) -> Self {
        Self {
            label: label.to_string(),
            query,
        }
    }

    /// 判据有没有至少一个可执行条件。
    ///
    /// 空条件的判据等于「什么都算」，会让并集退化成全量——
    /// 那样模型就等于没做判断，比不做判据更糟。
    pub fn has_condition(&self) -> bool {
        self.query.as_object().is_some_and(|map| !map.is_empty())
    }
}

/// 判据名字的长度上限。
pub const MAX_CRITERION_LABEL_CHARS: usize = 24;
/// 一组判据最多几条。
///
/// 6 是为了让「重要」这类复合需求能拆成 3–4 条，同时挡住
/// 「把检索条件列成 20 条」——那说明模型在瞎试，不是在判断。
pub const MAX_STEP_CRITERIA: usize = 6;

/// 判据里允许出现的条件名。
///
/// 与 `EMAIL_SEARCH_CONDITIONS` 同源，但**不含 `limit`**：
/// 判据是「什么样的邮件算命中」，条数上限不属于判据。
pub const CRITERION_CONDITIONS: &[&str] = &[
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
];

/// 从 `email_search` 的参数里取出判据组。
///
/// 判据藏在 `args.criteria` 里（数组），每项形如
/// `{"label": "已星标", "flagged_only": true}`：
/// `label` 是名字，其余键是这一条判据的条件。
///
/// 解析失败返回 `Err`（原因写进人话），不静默忽略——
/// 静默忽略会让模型以为判据生效了，用户看到的是一份**没有兑现的理解**。
pub fn parse_criteria(args: &serde_json::Value) -> Result<Vec<Criterion>, CoreError> {
    let Some(raw) = args.get("criteria") else {
        return Ok(Vec::new());
    };
    let Some(items) = raw.as_array() else {
        return Err(CoreError::validation(
            "criteria 必须是数组：每项是一个判据对象（{\"label\": \"…\", 条件…}）",
        ));
    };
    if items.is_empty() {
        return Ok(Vec::new());
    }
    if items.len() > MAX_STEP_CRITERIA {
        return Err(CoreError::validation(format!(
            "criteria 有 {} 条，上限 {MAX_STEP_CRITERIA} 条：\
判据多说明你在瞎试，不是在做判断",
            items.len()
        )));
    }
    let mut out = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let Some(map) = item.as_object() else {
            return Err(CoreError::validation(format!(
                "criteria 第 {} 项不是对象：每项应形如 {{\"label\": \"已星标\", \"flagged_only\": true}}",
                index + 1
            )));
        };
        let label = map
            .get("label")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim();
        if label.is_empty() {
            return Err(CoreError::validation(format!(
                "criteria 第 {} 项没有 label：\
给它一个短名字（如「已星标」「近 7 天未读」），用户要靠它判断你有没有理解错",
                index + 1
            )));
        }
        if label.chars().count() > MAX_CRITERION_LABEL_CHARS {
            return Err(CoreError::validation(format!(
                "criteria 第 {} 项的 label 太长（{} 字，上限 {MAX_CRITERION_LABEL_CHARS} 字）",
                index + 1,
                label.chars().count()
            )));
        }
        // 名字之外的部分才是条件。
        let mut query = serde_json::Map::new();
        for (key, value) in map {
            if key == "label" {
                continue;
            }
            if !CRITERION_CONDITIONS.contains(&key.as_str()) {
                return Err(CoreError::validation(format!(
                    "criteria 第 {} 项（{label}）有条件「{key}」不认得：可用的是 {}",
                    index + 1,
                    CRITERION_CONDITIONS.join(" / ")
                )));
            }
            query.insert(key.clone(), value.clone());
        }
        if query.is_empty() {
            return Err(CoreError::validation(format!(
                "criteria 第 {} 项（{label}）没有检索条件：\
判据必须写成可执行的条件（如 {{\"flagged_only\": true}}），\
空条件的判据等于「什么都算」",
                index + 1
            )));
        }
        out.push(Criterion::new(label, serde_json::Value::Object(query)));
    }
    Ok(out)
}

/// 一步为什么没做成。**结构化**存，UI 与 reflect 都要用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum StepFailure {
    /// 参数不合法（不可重试，必须改参数）
    InvalidArgs { detail: String },
    /// 同参数同错误已出现多次，熔断
    RepeatedCall { fingerprint: String },
    /// 工具/网络临时错误，重试用尽
    Transient { detail: String },
    /// 权限未开启或被执行期拒绝
    Denied { tool: String },
    /// 有副作用的工具失败：永不自动重试。
    Write { tool: String },
    /// 结果不满足 expect（reflect 判定，不是工具报错）
    ExpectMiss { expect: String, got: String },
    /// 依赖的前序步骤失败
    Upstream,
    /// 时间或次数预算耗尽
    Budget,
}

impl StepFailure {
    /// 一句话人话版本，直接给用户看。
    pub fn human(&self) -> String {
        match self {
            StepFailure::InvalidArgs { detail } => format!("参数不合法：{detail}"),
            StepFailure::RepeatedCall { .. } => "同样的参数连续失败，已停止重试".to_string(),
            StepFailure::Transient { detail } => format!("工具/网络错误：{detail}"),
            StepFailure::Denied { tool } => format!("没有开启「{tool}」权限"),
            StepFailure::Write { tool } => format!("「{tool}」执行失败（有副作用，已停止重试）"),
            StepFailure::ExpectMiss { expect, got } => {
                format!("结果不符合预期（期望：{expect}；实际：{got}）")
            }
            StepFailure::Upstream => "依赖的前一步没有完成".to_string(),
            StepFailure::Budget => "预算不足，这一步没有执行".to_string(),
        }
    }

    /// 落库前截断：`detail` 里可能带着整段工具错误。
    pub fn sanitized(mut self) -> Self {
        let cut = |text: String| truncate_chars(&text, MAX_STEP_FAILURE_CHARS);
        match &mut self {
            StepFailure::InvalidArgs { detail } | StepFailure::Transient { detail } => {
                *detail = cut(detail.clone());
            }
            StepFailure::RepeatedCall { fingerprint } => {
                *fingerprint = cut(fingerprint.clone());
            }
            StepFailure::Denied { tool } | StepFailure::Write { tool } => {
                *tool = cut(tool.clone());
            }
            StepFailure::ExpectMiss { expect, got } => {
                *expect = cut(expect.clone());
                *got = cut(got.clone());
            }
            StepFailure::Upstream | StepFailure::Budget => {}
        }
        self
    }
}

/// 重列的原因（模型自己给，引擎只校验取值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Diagnosis {
    /// 思路全错，步骤本身不对
    WrongPlan,
    /// 步骤对，参数不对
    WrongArgs,
    /// 本地没有这份数据
    MissingData,
    /// 工具本身不可用
    ToolBroken,
    /// 用户需求本身含糊
    UserAmbiguous,
}

impl Diagnosis {
    pub fn as_str(self) -> &'static str {
        match self {
            Diagnosis::WrongPlan => "wrong_plan",
            Diagnosis::WrongArgs => "wrong_args",
            Diagnosis::MissingData => "missing_data",
            Diagnosis::ToolBroken => "tool_broken",
            Diagnosis::UserAmbiguous => "user_ambiguous",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "wrong_plan" => Some(Diagnosis::WrongPlan),
            "wrong_args" => Some(Diagnosis::WrongArgs),
            "missing_data" => Some(Diagnosis::MissingData),
            "tool_broken" => Some(Diagnosis::ToolBroken),
            "user_ambiguous" => Some(Diagnosis::UserAmbiguous),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Diagnosis::WrongPlan => "思路不对",
            Diagnosis::WrongArgs => "参数不对",
            Diagnosis::MissingData => "缺少数据",
            Diagnosis::ToolBroken => "工具不可用",
            Diagnosis::UserAmbiguous => "需求含糊",
        }
    }
}

/// 这一轮到底算不算做完。模型的自我评估**不可信**，引擎会做一致性裁剪。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Done,
    Partial,
    Failed,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Done => "done",
            Verdict::Partial => "partial",
            Verdict::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "done" => Some(Verdict::Done),
            "partial" => Some(Verdict::Partial),
            "failed" => Some(Verdict::Failed),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Verdict::Done => "已完成",
            Verdict::Partial => "部分完成",
            Verdict::Failed => "未完成",
        }
    }
}

/// 一次重列的记录。**必须**有诊断与改法，否则拒收——
/// 「再试一次」不构成一次重列，那只是重新抽卡。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplanRecord {
    pub round: u8,
    pub diagnosis: Diagnosis,
    pub reason: String,
    pub changes: String,
    #[serde(default)]
    pub failed_steps: Vec<u8>,
    pub at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanStep {
    /// 步骤序号，从 1 开始；创建后不可变（模型靠它回填状态）。
    pub id: u8,
    pub title: String,
    #[serde(default)]
    pub kind: StepKind,
    pub capability: Capability,
    /// `kind == Tool` 时必填；其余必须为 `None`（校验期强制）。
    #[serde(default)]
    pub tool: Option<AiToolName>,
    /// 参数模板；`None` 表示「执行时由模型按上下文填」。
    ///
    /// 字符串里可以出现 `{{step.3.messages.0.message_id}}` 这样的引用，
    /// 只允许引用**前面的**步骤（见 [`refs_in_args`]）。
    #[serde(default)]
    pub args: Option<serde_json::Value>,
    /// 期望结果：一句可验收的话。reflect 阶段拿它和 evidence 比对。
    #[serde(default)]
    pub expect: Option<String>,
    pub status: PlanStatus,
    /// 已尝试次数，0..=MAX_STEP_ATTEMPTS。**只增不减**。
    #[serde(default)]
    pub attempts: u8,
    /// 完成时的一句回执（拿到了什么、命中几条）。**不是**工具返回值原文。
    #[serde(default)]
    pub evidence: Option<String>,
    /// 最近一次失败的结构化原因（给 reflect 与 UI 用）。
    #[serde(default)]
    pub failure: Option<StepFailure>,
    /// 0 命中时的策略。
    #[serde(default)]
    pub on_empty: OnEmpty,
    /// 这一步累计花掉的毫秒数（所有尝试之和）。
    ///
    /// 用来调 `MAX_STEP_ATTEMPTS` 这类常量：哪一步最慢、哪一步最常失败，
    /// 光看成功率看不出来，得看时间。
    #[serde(default)]
    pub duration_ms: u64,
}

impl PlanStep {
    /// 新步的默认形态：`kind = tool`、`attempts = 0`、`on_empty = narrow`，其余为空。
    ///
    /// 之所以要有这个构造器：`PlanStep` 有 7 处构造点，加字段时若直接改结构体字面量，
    /// 第一次 `cargo check` 会同时报 7 个 missing field，很难看出漏了哪个。
    /// 收敛到这里之后，加字段只改这一个函数体。
    pub fn new(id: u8, title: &str, capability: Capability, status: PlanStatus) -> Self {
        Self {
            id,
            title: title.to_string(),
            kind: StepKind::Tool,
            capability,
            tool: None,
            args: None,
            expect: None,
            status,
            attempts: 0,
            evidence: None,
            failure: None,
            on_empty: OnEmpty::Narrow,
            duration_ms: 0,
        }
    }

    pub fn with_evidence(mut self, evidence: &str) -> Self {
        self.evidence = Some(evidence.to_string());
        self
    }

    pub fn with_tool(mut self, tool: AiToolName) -> Self {
        self.kind = StepKind::Tool;
        self.tool = Some(tool);
        self
    }

    pub fn with_args(mut self, args: serde_json::Value) -> Self {
        self.args = Some(args);
        self
    }

    pub fn with_expect(mut self, expect: &str) -> Self {
        self.expect = Some(expect.to_string());
        self
    }

    /// 这一步能不能自动执行（只有工具步可以）。
    pub fn is_tool_step(&self) -> bool {
        self.kind == StepKind::Tool && self.tool.is_some()
    }

    /// 参数摘要，给清单条的细节行用。
    ///
    /// `{{step.3.messages.0.message_id}}` 这类引用显示成 `→ 第 3 步结果`：
    /// 原始模板对用户没有意义，反而会把一条清单撑满。
    ///
    /// `criteria` 单独渲染成 `判据:已星标 / 近 7 天未读`——
    /// 那是模型对用户需求的理解，用户最需要看到的就是它。
    pub fn args_digest(&self) -> Option<String> {
        let args = self.args.as_ref()?;
        let mut parts: Vec<String> = Vec::new();
        match args {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    if key == "motive" || key == "step_id" {
                        continue;
                    }
                    if key == "criteria" {
                        if let Some(digest) = criteria_digest(value) {
                            parts.push(digest);
                        }
                        continue;
                    }
                    parts.push(format!("{key}:{}", digest_value(value)));
                }
            }
            other => parts.push(digest_value(other)),
        }
        if parts.is_empty() {
            return None;
        }
        Some(truncate_chars(&parts.join(", "), 60))
    }
}

/// 判据组的摘要：`判据:已星标 / 近 7 天未读`。
///
/// 解析失败时不报错——摘要只是给人看的，参数校验另有其人。
fn criteria_digest(value: &serde_json::Value) -> Option<String> {
    let items = value.as_array()?;
    let names: Vec<String> = items
        .iter()
        .filter_map(|item| item.get("label").and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(str::to_string)
        .collect();
    if names.is_empty() {
        return None;
    }
    Some(format!("判据:{}", names.join(" / ")))
}

fn digest_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => match ref_step_id(text) {
            Some(id) => format!("→ 第 {id} 步结果"),
            None => truncate_chars(text, 24),
        },
        serde_json::Value::Null => "null".to_string(),
        other => truncate_chars(&other.to_string(), 24),
    }
}

/// 从 `{{step.3.messages.0.message_id}}` 里取出 `3`。
pub fn ref_step_id(text: &str) -> Option<u8> {
    let rest = text.trim().strip_prefix("{{")?.strip_suffix("}}")?;
    let rest = rest.trim().strip_prefix("step.")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// 递归取出参数里所有引用到的步号。
pub fn refs_in_args(args: &serde_json::Value) -> Vec<u8> {
    let mut out = Vec::new();
    fn walk(value: &serde_json::Value, out: &mut Vec<u8>) {
        match value {
            serde_json::Value::String(text) => {
                if let Some(id) = ref_step_id(text)
                    && !out.contains(&id)
                {
                    out.push(id);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    walk(item, out);
                }
            }
            serde_json::Value::Object(map) => {
                for item in map.values() {
                    walk(item, out);
                }
            }
            _ => {}
        }
    }
    walk(args, &mut out);
    out
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// 缺省 = 1：老数据按 v1 渲染，且**不参与执行判定**。
    #[serde(default = "plan_version_default")]
    pub version: u8,
    /// `p_<created_at>`：只在日志与排障里用，不是数据库主键（清单是消息的一部分）。
    #[serde(default)]
    pub id: String,
    pub goal: String,
    pub steps: Vec<PlanStep>,
    #[serde(default)]
    pub replans: Vec<ReplanRecord>,
    /// 被本份清单取代的历史清单（最多 [`MAX_PLAN_HISTORY`] 份）。
    #[serde(default)]
    pub history: Vec<Plan>,
    pub created_at: i64,
    pub updated_at: i64,
}

fn plan_version_default() -> u8 {
    PLAN_VERSION_V1
}

impl Default for Plan {
    /// 只给测试与「构造后逐字段填」的场景用：版本取 v1（最保守，不会被当成可执行计划）。
    fn default() -> Self {
        Self {
            version: PLAN_VERSION_V1,
            id: String::new(),
            goal: String::new(),
            steps: Vec::new(),
            replans: Vec::new(),
            history: Vec::new(),
            created_at: 0,
            updated_at: 0,
        }
    }
}

impl Plan {
    /// 新建一份 v2 清单（引擎侧构造，含 `plan_submit` 的合法输入）。
    pub fn v2(goal: &str, steps: Vec<PlanStep>, now: i64) -> Self {
        Self {
            version: PLAN_VERSION_V2,
            id: format!("p_{now}"),
            goal: goal.to_string(),
            steps,
            replans: Vec::new(),
            history: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// 能不能被引擎当作「可执行计划」。
    ///
    /// 老数据（v1）只有 title/status，没有工具与参数——当成可执行计划会让引擎
    /// 去执行一堆 `tool = None` 的步。这里用版本号一刀切，比逐字段猜安全得多。
    pub fn is_executable(&self) -> bool {
        self.version >= PLAN_VERSION_V2 && !self.steps.is_empty()
    }

    /// 还没结束的步数（`pending` + `running`）。
    pub fn open_steps(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| !step.status.is_terminal())
            .count()
    }

    pub fn has_open_steps(&self) -> bool {
        self.open_steps() > 0
    }

    /// 这份清单是不是「上一轮没做完、可以接着做」的。
    ///
    /// 判据是**还有没终结的步**：全做完的清单不需要续，用户再问就是新需求。
    /// 取消掉的清单也不算——那是用户明确放弃了。
    pub fn is_resumable(&self) -> bool {
        self.steps
            .iter()
            .any(|step| matches!(step.status, PlanStatus::Pending | PlanStatus::Running))
    }

    /// 还没做完的步（给「继续上一条」的提示用）。
    pub fn unfinished(&self) -> Vec<&PlanStep> {
        self.steps
            .iter()
            .filter(|step| matches!(step.status, PlanStatus::Pending | PlanStatus::Running))
            .collect()
    }

    /// 把一次尝试的耗时记到某一步上。
    pub fn add_duration(&mut self, id: u8, millis: u64) {
        if let Some(step) = self.steps.iter_mut().find(|step| step.id == id) {
            step.duration_ms = step.duration_ms.saturating_add(millis);
        }
    }

    /// 清单的统计摘要：给日志与「调参」用。
    ///
    /// 三件事：总耗时、最慢的一步、失败率。
    /// 失败率特别有用——它直接回答「`MAX_STEP_ATTEMPTS` 是不是给少了」。
    pub fn stats(&self) -> PlanStats {
        let mut total_ms = 0u64;
        let mut slowest: Option<(u8, &str, u64)> = None;
        let mut failed = 0usize;
        let mut retried = 0usize;
        for step in &self.steps {
            total_ms = total_ms.saturating_add(step.duration_ms);
            if step.status == PlanStatus::Failed {
                failed += 1;
            }
            if step.attempts > 1 {
                retried += 1;
            }
            let candidate = (step.id, step.title.as_str(), step.duration_ms);
            if slowest.is_none_or(|(_, _, current)| candidate.2 > current) {
                slowest = Some(candidate);
            }
        }
        let total = self.steps.len();
        PlanStats {
            total_steps: total,
            done_steps: self.done_steps(),
            failed_steps: failed,
            retried_steps: retried,
            total_ms,
            slowest: slowest.map(|(id, title, ms)| (id, title.to_string(), ms)),
            failure_rate: if total == 0 {
                0.0
            } else {
                failed as f64 / total as f64
            },
        }
    }

    /// 一句话摘要：还剩哪几步没做。
    pub fn unfinished_summary(&self) -> String {
        self.unfinished()
            .iter()
            .map(|step| format!("第 {} 步「{}」", step.id, step.title))
            .collect::<Vec<_>>()
            .join("、")
    }

    /// `(已结束, 总数)`，给「已完成 4 步 · 1 步未完成」这类摘要用。
    pub fn progress(&self) -> (usize, usize) {
        let done = self.steps.iter().filter(|s| s.status.is_terminal()).count();
        (done, self.steps.len())
    }

    /// 已真正完成的步数（不含失败/跳过）。给 FINAL 的 verdict 用。
    pub fn done_steps(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| s.status == PlanStatus::Done)
            .count()
    }

    /// 有没有**真正失败**的步。
    ///
    /// 与 [`Self::any_terminal_failure`] 的区别很重要：`skipped` 常常是
    /// 「这一步不需要做」（用户没给日期、条件不适用），为它触发自检与重列
    /// 只会白花两轮。
    pub fn any_failed(&self) -> bool {
        self.steps
            .iter()
            .any(|step| step.status == PlanStatus::Failed)
    }

    pub fn any_terminal_failure(&self) -> bool {
        self.steps
            .iter()
            .any(|s| matches!(s.status, PlanStatus::Failed | PlanStatus::Skipped))
    }

    /// 这一步还剩几次机会。
    ///
    /// 引擎自动重试也要占这个预算，所以重试次数不能超过它。
    pub fn remaining_attempts(&self, id: u8) -> u8 {
        self.step(id)
            .map(|step| MAX_STEP_ATTEMPTS.saturating_sub(step.attempts))
            .unwrap_or(0)
    }

    /// 按 `on_empty` 处理一次「0 命中」，返回下一步该怎么走。
    ///
    /// 0 命中不是成功也不是失败，是**计划需要修正**的信号：
    /// - [`OnEmpty::Narrow`]：条件放宽一档再来（默认）；
    /// - [`OnEmpty::Ask`]：停下来问用户；
    /// - [`OnEmpty::Accept`]：接受空结果，继续下一步。
    pub fn apply_empty(&mut self, id: u8, evidence: &str, tries: u8) -> EmptyAction {
        let Some(step) = self.steps.iter_mut().find(|s| s.id == id) else {
            return EmptyAction::Accept;
        };
        step.attempts = step.attempts.saturating_add(tries.max(1));
        step.evidence = Some(truncate_chars(evidence, MAX_STEP_EVIDENCE_CHARS));
        if step.status == PlanStatus::Pending {
            step.status = PlanStatus::Running;
        }
        match step.on_empty {
            OnEmpty::Accept => {
                // 接受空结果：这一步就算做完了（回执写明「0 命中」）。
                step.status = PlanStatus::Done;
                EmptyAction::Accept
            }
            OnEmpty::Ask => EmptyAction::Ask,
            OnEmpty::Narrow => {
                // 走到这里说明**放宽已经用尽**（引擎试过每一档，或机会用完）。
                //
                // 这一步仍然算**完成**：0 命中是一个合法的业务结果
                // （确实没有这样的邮件），不是「工具坏了」。
                // 把它算成失败会让 REFLECT 白跑一轮，还会让用户看到红色叉号——
                // 而实际上「最近 30 天没有这样的邮件」就是正确答案。
                step.status = PlanStatus::Done;
                step.evidence = Some(truncate_chars(
                    &format!("{evidence}（条件已放宽到底，仍无结果）"),
                    MAX_STEP_EVIDENCE_CHARS,
                ));
                EmptyAction::Accept
            }
        }
    }

    /// 把一步的参数换掉（引擎放宽条件时用）。
    ///
    /// 放宽后的参数会写回清单：用户看到的是**真实执行过**的条件，
    /// 而不是模型最初写的那份。
    ///
    /// 同时把「条件被放宽过」写进回执：步骤标题是模型写的、引擎不能改，
    /// 标题与实际条件脱节时，回执是唯一能让用户对得上账的地方。
    pub fn set_step_args(&mut self, id: u8, args: serde_json::Value, note: &str) -> bool {
        match self.steps.iter_mut().find(|step| step.id == id) {
            Some(step) => {
                // 清单里存的是**业务参数**：`motive` 是给模型看的说明，
                // 留在清单里会出现在参数摘要上，干扰用户对账。
                let mut map = args.as_object().cloned().unwrap_or_default();
                map.remove("motive");
                map.remove("step_id");
                step.args = Some(serde_json::Value::Object(map));
                step.evidence = Some(truncate_chars(note, MAX_STEP_EVIDENCE_CHARS));
                true
            }
            None => false,
        }
    }

    /// 第一个还没结束的工具步——绑定兜底用（模型没给 `step_id` 时）。
    pub fn next_tool_step(&self) -> Option<u8> {
        self.steps
            .iter()
            .find(|step| step.is_tool_step() && !step.status.is_terminal())
            .map(|step| step.id)
    }

    pub fn step(&self, id: u8) -> Option<&PlanStep> {
        self.steps.iter().find(|step| step.id == id)
    }

    pub fn step_mut(&mut self, id: u8) -> Option<&mut PlanStep> {
        self.steps.iter_mut().find(|step| step.id == id)
    }

    /// 把**进行中**的步标成跳过。取消时用它：没开始的步保持 `pending`，
    /// 因为用户可能接着说「继续」，那些步还没被放弃。
    pub fn skip_running(&mut self, reason: &str) -> bool {
        self.skip_where(reason, |status| status == PlanStatus::Running)
    }

    /// 把所有未结束的步标成跳过。崩溃恢复时用它：这一轮已经不可能再继续了。
    pub fn skip_open(&mut self, reason: &str) -> bool {
        self.skip_where(reason, |status| !status.is_terminal())
    }

    fn skip_where(&mut self, reason: &str, pred: impl Fn(PlanStatus) -> bool) -> bool {
        let mut changed = false;
        for step in self.steps.iter_mut().filter(|s| pred(s.status)) {
            step.status = PlanStatus::Skipped;
            if step.evidence.is_none() {
                step.evidence = Some(truncate_chars(reason, MAX_STEP_EVIDENCE_CHARS));
            }
            changed = true;
        }
        changed
    }

    /// 预算耗尽：未终结的步标 `skipped` 并写明原因。
    ///
    /// 预算耗尽**不是失败**——已完成的部分必须如实留着，只是没做完。
    pub fn mark_budget_exhausted(&mut self) -> bool {
        let mut changed = false;
        for step in self.steps.iter_mut().filter(|s| !s.status.is_terminal()) {
            step.status = PlanStatus::Skipped;
            step.failure = Some(StepFailure::Budget);
            if step.evidence.is_none() {
                step.evidence = Some("预算不足，未执行".to_string());
            }
            changed = true;
        }
        changed
    }

    /// 开始一步。返回 `false` 表示该步不存在、已终结，或还有 `attempts` 余额用尽。
    pub fn begin_step(&mut self, id: u8) -> bool {
        let Some(step) = self.steps.iter_mut().find(|s| s.id == id) else {
            return false;
        };
        if step.status.is_terminal() || step.attempts >= MAX_STEP_ATTEMPTS {
            return false;
        }
        step.status = PlanStatus::Running;
        true
    }

    /// 记一次尝试结果。`attempts` 只增不减；失败到上限即转终态。
    ///
    /// 返回该步的最新状态：`Done` / `Failed`（用尽） / `Running`（还可以再来一次）。
    /// 把「还能不能再试」的判断留在引擎侧，模型不需要理解次数。
    pub fn record_attempt(&mut self, id: u8, outcome: AttemptOutcome) -> PlanStatus {
        self.record_attempts(id, 1, outcome)
    }

    /// 记 `tries` 次尝试（引擎自动重试时一次模型调用对应多次物理尝试）。
    ///
    /// 物理尝试与模型调用**共用**同一个次数预算：一步总共 3 次机会，
    /// 引擎重试用掉的次数也要算，否则「3 次机会」会被引擎的重试悄悄放大。
    pub fn record_attempts(&mut self, id: u8, tries: u8, outcome: AttemptOutcome) -> PlanStatus {
        let Some(step) = self.steps.iter_mut().find(|s| s.id == id) else {
            return PlanStatus::Failed;
        };
        step.attempts = step.attempts.saturating_add(tries.max(1));
        match outcome {
            AttemptOutcome::Done { evidence } => {
                step.status = PlanStatus::Done;
                step.evidence = Some(truncate_chars(&evidence, MAX_STEP_EVIDENCE_CHARS));
                step.failure = None;
            }
            AttemptOutcome::Empty { evidence } => {
                // 0 命中不是失败，但也不算完成：由调用方按 `on_empty` 决定下一步。
                step.evidence = Some(truncate_chars(&evidence, MAX_STEP_EVIDENCE_CHARS));
                if step.status == PlanStatus::Pending {
                    step.status = PlanStatus::Running;
                }
            }
            AttemptOutcome::Failed(failure) => {
                step.failure = Some(failure.sanitized());
                if step.attempts >= MAX_STEP_ATTEMPTS {
                    step.status = PlanStatus::Failed;
                } else {
                    step.status = PlanStatus::Running;
                }
            }
        }
        step.status
    }

    /// reflect 判定「结果不达 expect」：把 done 打回 running，再给一次机会。
    ///
    /// **只能退一次**：`attempts` 已经 +1，第 3 次失败即终态，避免模型反复自我否定。
    pub fn reopen_step(&mut self, id: u8, miss: StepFailure) -> bool {
        let Some(step) = self.steps.iter_mut().find(|s| s.id == id) else {
            return false;
        };
        if step.status != PlanStatus::Done || step.attempts >= MAX_STEP_ATTEMPTS {
            return false;
        }
        step.attempts = step.attempts.saturating_add(1);
        step.status = PlanStatus::Running;
        step.evidence = None;
        step.failure = Some(miss.sanitized());
        true
    }

    /// 直接把一步判失败（权限、依赖、熔断等引擎侧判定）。
    pub fn fail_step(&mut self, id: u8, failure: StepFailure) {
        if let Some(step) = self.steps.iter_mut().find(|s| s.id == id) {
            step.status = PlanStatus::Failed;
            step.failure = Some(failure.sanitized());
        }
    }

    pub fn skip_step(&mut self, id: u8, failure: StepFailure, reason: &str) {
        if let Some(step) = self.steps.iter_mut().find(|s| s.id == id) {
            step.status = PlanStatus::Skipped;
            step.failure = Some(failure.sanitized());
            if step.evidence.is_none() {
                step.evidence = Some(truncate_chars(reason, MAX_STEP_EVIDENCE_CHARS));
            }
        }
    }

    /// 依赖被引用步的为失败/跳过的步级联跳过（`$ref` 是唯一的显式依赖）。
    ///
    /// 必须做：拿一个空的 `message_id` 去读正文只会白吃一次错误，
    /// 而且会让用户看到「明明前面失败了，后面还在跑」。
    pub fn skip_dependents(&mut self) -> bool {
        let mut changed = false;
        loop {
            let dead: Vec<u8> = self
                .steps
                .iter()
                .filter(|s| matches!(s.status, PlanStatus::Failed | PlanStatus::Skipped))
                .map(|s| s.id)
                .collect();
            let mut round = Vec::new();
            for step in &self.steps {
                if step.status.is_terminal() {
                    continue;
                }
                let Some(args) = step.args.as_ref() else {
                    continue;
                };
                if refs_in_args(args).iter().any(|id| dead.contains(id)) {
                    round.push(step.id);
                }
            }
            if round.is_empty() {
                break;
            }
            for id in round {
                self.skip_step(id, StepFailure::Upstream, "依赖的前一步没有完成");
                changed = true;
            }
        }
        changed
    }

    /// 收尾时把 `running` 的步收干净：有回执（含「0 命中」）算完成，
    /// 一次都没执行过的如实标跳过。
    ///
    /// 留一个 `running` 在界面上就是永远转圈的圆圈——用户会以为还在跑。
    pub fn settle_running(&mut self) -> bool {
        let mut changed = false;
        for step in self.steps.iter_mut() {
            if step.status != PlanStatus::Running {
                continue;
            }
            if step.kind == StepKind::Reason || step.evidence.is_some() {
                step.status = PlanStatus::Done;
                if step.evidence.is_none() {
                    step.evidence = Some("已直接回答".to_string());
                }
            } else {
                step.status = PlanStatus::Skipped;
                step.evidence = Some("本轮没有执行".to_string());
            }
            changed = true;
        }
        changed
    }

    /// 把当前清单推进 `history`（重列时调用），最多留 [`MAX_PLAN_HISTORY`] 份。
    pub fn push_history(&mut self, mut previous: Plan) {
        previous.history.clear();
        self.history.insert(0, previous);
        self.history.truncate(MAX_PLAN_HISTORY);
    }

    /// 一句话摘要，给工具返回给模型的文字用。
    pub fn summary_line(&self) -> String {
        let (done, total) = self.progress();
        let open: Vec<String> = self
            .steps
            .iter()
            .filter(|step| !step.status.is_terminal())
            .map(|step| format!("{}（{}，{}）", step.title, step.status.label(), step.id))
            .collect();
        if open.is_empty() {
            format!("清单已完成（{done}/{total}）：{}", self.goal)
        } else {
            format!("清单进度 {done}/{total}，未完成：{}", open.join("；"))
        }
    }

    /// REFLECT 阶段的事实表：每步一行、不含正文原文。
    ///
    /// 8 步全跑完也只有 600 余字：既不会把上下文顶爆，也不会把邮件内容
    /// 当成指令送进模型（保持注入边界）。
    pub fn facts_block(&self) -> String {
        let mut out = String::new();
        for step in &self.steps {
            let mut line = format!("{}. [{}] {}", step.id, step.status.label(), step.title);
            if let Some(tool) = step.tool {
                line.push_str(&format!(" —— {}", tool.as_str()));
                if let Some(digest) = step.args_digest() {
                    line.push_str(&format!("({digest})"));
                }
            } else {
                line.push_str(&format!(" —— {}", step.kind.label()));
            }
            match (&step.evidence, &step.failure) {
                (Some(evidence), _) => line.push_str(&format!(" → {evidence}")),
                (None, Some(failure)) => line.push_str(&format!(" → {}", failure.human())),
                (None, None) => {}
            }
            if step.status == PlanStatus::Failed {
                line.push_str(&format!("（{} 次尝试）", step.attempts));
            } else if step.attempts > 1 {
                line.push_str(&format!("（第 {} 次尝试）", step.attempts));
            }
            if let Some(expect) = &step.expect
                && step.status != PlanStatus::Done
            {
                line.push_str(&format!("\n   期望：{expect}"));
            }
            out.push_str(&line);
            out.push('\n');
        }
        truncate_chars(&out, 1200)
    }
}

/// 模型的自检结果（REFLECT 阶段）。
///
/// 它是**不可信**的：模型倾向说「完成了」。所以 `verdict` 只是一份证词，
/// 最终结论由 [`reconcile`] 拿事实裁剪。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfCheck {
    pub verdict: Verdict,
    pub failed_step: Option<u8>,
    pub diagnosis: Option<Diagnosis>,
    pub reason: String,
    pub replan_hint: String,
}

impl SelfCheck {
    /// 从模型输出里抠出 JSON。
    ///
    /// 不能要求「整段就是 JSON」：模型常包一层 ```json 代码块或加一句前言。
    /// 抠不出来就当没自检成功，由调用方决定重问还是按 `partial` 收场。
    pub fn parse(text: &str) -> Option<Self> {
        let start = text.find('{')?;
        let end = text.rfind('}')?;
        if end <= start {
            return None;
        }
        let value: serde_json::Value = serde_json::from_str(&text[start..=end]).ok()?;
        let verdict = value
            .get("verdict")
            .and_then(serde_json::Value::as_str)
            .and_then(Verdict::parse)?;
        let failed_step = value
            .get("failed_step")
            .and_then(serde_json::Value::as_u64)
            .and_then(|id| u8::try_from(id).ok());
        let diagnosis = value
            .get("diagnosis")
            .and_then(serde_json::Value::as_str)
            .and_then(Diagnosis::parse);
        let reason = value
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let replan_hint = value
            .get("replan_hint")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        Some(Self {
            verdict,
            failed_step,
            diagnosis,
            reason: truncate_chars(&reason, MAX_STEP_EVIDENCE_CHARS),
            replan_hint: truncate_chars(&replan_hint, MAX_STEP_EVIDENCE_CHARS),
        })
    }

    /// 自检是否给出了「可以据此重列」的信息。
    pub fn can_replan(&self) -> bool {
        self.verdict == Verdict::Failed
            && self.reason.chars().count() >= 8
            && self.replan_hint.chars().count() >= 4
    }
}

/// 事实优先：模型的结论不能盖过清单上的事实。
///
/// 三种裁剪：
/// - 全部完成却说失败 → `partial`（保留模型原话，界面上「5 步都打了勾」与
///   「模型认为不够好」同时可见）；
/// - 有失败/跳过的步却说完成 → `partial`（虚报是最常见的）；
/// - `failed_step` 指向一个已完成的步 → `partial`。
///
/// 为什么「全做完却说失败」不直接判 `done`：模型有时是自我苛责（常见），
/// 有时是真的发现 evidence 与目标不符（少见但重要）。引擎分不出来，
/// 取信息量最大的处理——判 partial，而不是抹掉这层不确定性。
pub fn reconcile(check: &SelfCheck, plan: &Plan) -> Verdict {
    let all_done = plan
        .steps
        .iter()
        .all(|step| step.status == PlanStatus::Done);
    let any_failure = plan.any_terminal_failure();
    let points_to_done = check
        .failed_step
        .and_then(|id| plan.step(id))
        .is_some_and(|step| step.status == PlanStatus::Done);

    match check.verdict {
        Verdict::Failed if all_done => Verdict::Partial,
        Verdict::Failed if points_to_done => Verdict::Partial,
        Verdict::Done if any_failure => Verdict::Partial,
        other => other,
    }
}

/// 「0 命中」之后引擎该怎么走。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyAction {
    /// 放宽条件再来一次。
    Narrow,
    /// 停下来问用户要更多信息。
    Ask,
    /// 接受空结果，继续下一步。
    Accept,
    /// 放宽的机会用尽，如实判失败。
    GiveUp,
}

/// 把一步的检索条件**放宽一档**。
///
/// 0 命中时不该原样重试（结果不会变），也不该让模型瞎猜换个什么词。
///
/// **两种放宽方式**，取决于这一步用的是普通条件还是判据组：
///
/// - 普通条件（一组 AND 条件）：按「先去掉最严格的限定」逐档放宽——
///   先丢 `flagged_only`（用户自己标的，本轮可能还没标），
///   再丢 `subject`（比全文更窄），最后拉长时间窗到 30 天。
/// - 判据组（多条 OR 条件）：**先去掉最宽的那一条判据**。
///   判据是模型对需求的翻译，去掉一条等于放弃一种理解；
///   保留剩下的比放宽剩下的更忠实于模型原本的意图。
///
/// 时间窗**只拉长不删掉**：删掉等于把「最近 7 天的未读」变成「所有未读」——
/// 那是换了一个问题，不是放宽了同一个问题。
///
/// 返回 `None` 表示没有可放宽的条件了（该步到此为止）。
///
/// `motive` 与 `step_id` 原样保留：它们不参与检索，但重试时要靠 `step_id`
/// 绑回清单里的那一步，丢了就绑不上了。
pub fn relax_args(args: &serde_json::Value) -> Option<serde_json::Value> {
    // 判据组：去掉最后一条（最宽的那条）。
    if let Some(items) = args.get("criteria").and_then(serde_json::Value::as_array)
        && items.len() > 1
    {
        let mut map = args.as_object()?.clone();
        map.insert(
            "criteria".to_string(),
            serde_json::Value::Array(items[..items.len() - 1].to_vec()),
        );
        return Some(serde_json::Value::Object(map));
    }
    let mut map = args.as_object()?.clone();
    // 1) 先丢星标
    if map.remove("flagged_only").is_some() {
        return Some(serde_json::Value::Object(map));
    }
    // 2) 再丢主题关键词
    if map.remove("subject").is_some() {
        return Some(serde_json::Value::Object(map));
    }
    // 3) 最后拉长时间窗：扩到 30 天，**不整段丢掉**。
    //
    // 丢掉时间窗等于把「最近 7 天的未读」变成「所有未读」——那是换了一个问题，
    // 不是放宽了同一个问题。用户要的是最近的消息，不是全部消息。
    if let Some(days) = map.get("days").and_then(serde_json::Value::as_u64) {
        if days < 30 {
            map.insert("days".to_string(), serde_json::Value::from(30));
            return Some(serde_json::Value::Object(map));
        }
        return None;
    }
    if map.get("after").is_some() {
        map.remove("after");
        map.insert("days".to_string(), serde_json::Value::from(30));
        return Some(serde_json::Value::Object(map));
    }
    // 没有可放宽的了。
    None
}

/// 清单的统计摘要。
///
/// 这些数字是给**调参**用的，不是给用户看的：它们回答
/// 「`MAX_STEP_ATTEMPTS` 是不是给少了」「哪一步最慢」这类问题。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanStats {
    pub total_steps: usize,
    pub done_steps: usize,
    pub failed_steps: usize,
    /// 试过不止一次的步数。
    pub retried_steps: usize,
    /// 全部步骤累计耗时（毫秒）。
    pub total_ms: u64,
    /// 最慢的一步：`(步号, 标题, 毫秒)`。
    pub slowest: Option<(u8, String, u64)>,
    /// 失败步占比（0.0 ~ 1.0）。
    pub failure_rate: f64,
}

impl PlanStats {
    /// 一行摘要，给日志看。
    pub fn summary(&self) -> String {
        let slowest = match &self.slowest {
            Some((id, title, ms)) => format!("最慢第 {id} 步「{title}」{ms}ms"),
            None => "无耗时数据".to_string(),
        };
        format!(
            "{}/{} 步完成，{} 步失败，{} 步重试过，失败率 {:.0}%，累计 {}ms，{}",
            self.done_steps,
            self.total_steps,
            self.failed_steps,
            self.retried_steps,
            self.failure_rate * 100.0,
            self.total_ms,
            slowest
        )
    }
}

/// 一次执行的归类结果。引擎用它推进步骤状态，不用自己数 attempts。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// 拿到了可用结果。
    Done { evidence: String },
    /// 工具成功但 0 命中。
    Empty { evidence: String },
    /// 这次没成，但还可以再来（`attempts` 用尽后自动转 `Failed`）。
    Failed(StepFailure),
}

/// 合并 + 校验一次 `plan_update`。
///
/// `prev = None` 表示这是本轮第一次建清单。返回**归一化后**的清单，
/// 调用方直接替换内存/落库值；任何违规都返回错误，清单保持不变。
///
/// 语义是「按 id 合并」而不是「整份覆盖」：模型可以只传改动过的步骤，
/// 没提到的步骤沿用上一步的状态。全量传入时行为与覆盖一致。
pub fn validate_plan(prev: Option<&Plan>, next: &Plan) -> Result<Plan, CoreError> {
    let goal = normalized_goal(prev, next)?;

    if next.steps.is_empty() {
        return Err(CoreError::validation("清单至少要有一步"));
    }

    let mut seen: Vec<u8> = Vec::new();
    let mut merged: Vec<PlanStep> = Vec::new();
    for step in &next.steps {
        if step.id == 0 || step.id as usize > MAX_PLAN_STEPS {
            return Err(CoreError::validation(format!(
                "步骤编号必须在 1–{MAX_PLAN_STEPS} 之间（收到 {}）",
                step.id
            )));
        }
        if seen.contains(&step.id) {
            return Err(CoreError::validation(format!(
                "步骤编号 {} 重复了",
                step.id
            )));
        }
        seen.push(step.id);

        let title = step.title.trim();
        if title.is_empty() {
            return Err(CoreError::validation(format!(
                "第 {} 步缺少标题（title）",
                step.id
            )));
        }
        let title = truncate_chars(title, MAX_STEP_TITLE_CHARS);
        let evidence = step
            .evidence
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| truncate_chars(text, MAX_STEP_EVIDENCE_CHARS));

        let mut out = match prev
            .and_then(|plan| plan.steps.iter().find(|s| s.id == step.id))
            .cloned()
        {
            // 合并时以**旧步**为底：`plan_update` 只传 id/status/evidence，
            // 用它整份替换会把 tool/args/expect 清空——那等于把计划改成空壳。
            Some(old) => {
                if old.title != title {
                    // 允许改标题会带来一个隐蔽的坏结果：模型边做边改目标，
                    // 事后看清单完全看不出它其实换了要求。
                    return Err(CoreError::validation(format!(
                        "第 {} 步的标题不能修改（原为「{}」）",
                        step.id, old.title
                    )));
                }
                // `General` 意味着「模型没写 / 写不出来」，不是「要改成通用」。
                // 拿它跟旧步的具体领域一比就会误判成「改能力」，把正常的
                // 状态回填挡在门外——所以这里放行，合并时沿用旧步的值。
                if step.capability != Capability::General && old.capability != step.capability {
                    return Err(CoreError::validation(format!(
                        "第 {} 步的所属能力不能修改（原为 {}）",
                        step.id,
                        old.capability.label()
                    )));
                }
                if old.status.is_terminal() && step.status != old.status {
                    return Err(CoreError::validation(format!(
                        "第 {} 步已经是「{}」，不能再改成「{}」",
                        step.id,
                        old.status.label(),
                        step.status.label()
                    )));
                }
                if step.status.rank() < old.status.rank() {
                    return Err(CoreError::validation(format!(
                        "第 {} 步的状态不能回退（{} → {}）",
                        step.id,
                        old.status.label(),
                        step.status.label()
                    )));
                }
                let mut merged = old;
                merged.title = title;
                merged.status = step.status;
                // **引擎记过事实的步骤，evidence 不能被模型复述覆盖。**
                //
                // 2026-11-07 的事故：email_search 实际返回 20 封，引擎把
                // 「检索到 20 封」写进了这一步的 evidence；模型随后调
                // `plan_update` 写「已检索近7天邮件样本，拿到 50 封邮件摘要
                // 列表」，把真实回执盖掉了。清单是给用户对账的，对不上账的
                // 回执比没有回执更糟。
                //
                // 判定用「attempts > 0 且已有 evidence」：attempts 只由引擎的
                // `record_attempts` 递增，模型改不了。模型在自己跑工具之前
                // 写的复述照常采纳——那一步引擎还没碰过。
                if evidence.is_some() && !(merged.attempts > 0 && merged.evidence.is_some()) {
                    merged.evidence = evidence;
                }
                merged
            }
            None => {
                let mut fresh = PlanStep::new(step.id, &title, step.capability, step.status);
                fresh.kind = step.kind;
                fresh.tool = step.tool;
                fresh.args = step.args.clone();
                fresh.expect = step.expect.clone();
                fresh.on_empty = step.on_empty;
                fresh.evidence = evidence;
                fresh
            }
        };
        out.id = step.id;

        merged.push(out);
    }

    // 没被提到的旧步骤原样保留：模型经常只回传「刚动过的那一步」。
    if let Some(prev) = prev {
        for old in &prev.steps {
            if !seen.contains(&old.id) {
                merged.push(old.clone());
            }
        }
    }
    merged.sort_by_key(|step| step.id);

    if merged.len() > MAX_PLAN_STEPS {
        return Err(CoreError::validation(format!(
            "清单最多 {MAX_PLAN_STEPS} 步（当前 {}）",
            merged.len()
        )));
    }

    let id = match prev {
        Some(prev) if !prev.id.is_empty() => prev.id.clone(),
        _ if !next.id.trim().is_empty() => next.id.trim().to_string(),
        _ => format!("p_{}", next.created_at),
    };

    Ok(Plan {
        version: prev.map(|plan| plan.version).unwrap_or(next.version),
        id,
        goal,
        steps: merged,
        replans: prev.map(|plan| plan.replans.clone()).unwrap_or_default(),
        history: prev.map(|plan| plan.history.clone()).unwrap_or_default(),
        created_at: prev.map(|plan| plan.created_at).unwrap_or(next.created_at),
        updated_at: next
            .updated_at
            .max(prev.map(|plan| plan.updated_at).unwrap_or(0)),
    })
}

// ---------------------------------------------------------------------------
// v2：plan_submit 的全量校验
// ---------------------------------------------------------------------------

/// 计划期的一步「能被猜出来的错误」。
///
/// 每修好一处，就少一次模型往返。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRepair {
    /// 修了什么（回灌给模型 + 界面上给用户看）。
    pub note: String,
}

/// 工具步的 `capability` 缺失时，从工具名反推。
///
/// 工具与能力模块是一一对应的，让模型再填一遍纯属多余——而且它会填错。
/// 2026-11-07 的事故里，模型为了一步 `reason` 的 capability 连改 4 次。
fn infer_capability(tool: AiToolName) -> Capability {
    match tool {
        AiToolName::EmailSearch | AiToolName::MailRead => Capability::Mail,
        AiToolName::WeatherGet | AiToolName::WeatherLocations => Capability::Weather,
        AiToolName::CalendarList
        | AiToolName::CalendarCreate
        | AiToolName::CalendarUpdate
        | AiToolName::CalendarDelete => Capability::Calendar,
        AiToolName::MusicSearch
        | AiToolName::MusicPlay
        | AiToolName::MusicState
        | AiToolName::MusicControl => Capability::Music,
        // 记账类工具不进计划；落到这里说明模型写错了工具名。
        AiToolName::MemoryUpsert
        | AiToolName::PlanUpdate
        | AiToolName::PlanSubmit
        | AiToolName::AnswerDirectly => Capability::General,
    }
}

/// 把模型写错的参数名改成正确的。
///
/// 返回被改掉的名字（`date_from` → `after`）。改不动的（拼错且猜不出）
/// 原样留下，交给 [`validate_plan_v2`] 报错——宁可报错也不要静默丢掉参数，
/// 静默丢掉会让检索范围悄悄变掉，用户拿到的是一份「没兑现的理解」。
fn repair_argument_names(tool: AiToolName, args: &mut serde_json::Value) -> Vec<String> {
    let Some(map) = args.as_object_mut() else {
        return Vec::new();
    };
    let mut renamed = Vec::new();
    for (key, value) in map.iter() {
        if key == "motive" || key == "step_id" {
            continue;
        }
        if let Some(target) = crate::ai::tool_argument_alias(tool, key, value) {
            renamed.push((key.clone(), target.to_string(), value.clone()));
        }
    }
    let mut notes = Vec::new();
    for (from, to, value) in &renamed {
        map.remove(from);
        map.insert(to.clone(), value.clone());
        notes.push(format!("{from} → {to}"));
    }
    notes
}

/// 判据项缺 `label` 时，按它自己的条件反推一个名字。
///
/// 判据名是**给人看的**（「已星标」「近 7 天未读」），模型偶尔会漏写。
/// 引擎能从条件反推出一个够用的名字，但不能替模型决定「什么算重要」——
/// 条件本身仍然是模型写的。
fn infer_criterion_label(conditions: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if conditions
        .get("flagged_only")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        parts.push("已星标".to_string());
    }
    if conditions
        .get("unread_only")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        parts.push("未读".to_string());
    }
    if let Some(days) = conditions.get("days").and_then(serde_json::Value::as_u64) {
        parts.push(format!("近 {days} 天"));
    }
    if let Some(subject) = conditions
        .get("subject")
        .and_then(serde_json::Value::as_str)
    {
        parts.push(format!("主题含「{subject}」"));
    }
    if let Some(query) = conditions.get("query").and_then(serde_json::Value::as_str) {
        parts.push(format!("含「{query}」"));
    }
    if let Some(from) = conditions.get("from").and_then(serde_json::Value::as_str) {
        parts.push(format!("来自 {from}"));
    }
    if let Some(folder) = conditions.get("folder").and_then(serde_json::Value::as_str) {
        parts.push(folder.to_string());
    }
    if parts.is_empty() {
        return "自定义条件".to_string();
    }
    parts.join("、")
}

/// 修好判据组：补名字、删掉不认的条件键。
///
/// 删条件键是有代价的（判据范围会变），所以只在**该判据还有别的条件**时才删；
/// 删完就空了的话宁可留着让校验报错——那说明模型写的判据压根没法执行。
fn repair_criteria(args: &mut serde_json::Value) -> Vec<String> {
    let mut notes = Vec::new();
    let Some(items) = args
        .get_mut("criteria")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return notes;
    };
    for (index, item) in items.iter_mut().enumerate() {
        let Some(map) = item.as_object_mut() else {
            continue;
        };
        // 删掉不认的条件键。
        let unknown: Vec<String> = map
            .keys()
            .filter(|key| key.as_str() != "label" && !CRITERION_CONDITIONS.contains(&key.as_str()))
            .cloned()
            .collect();
        let has_known = map
            .keys()
            .any(|key| CRITERION_CONDITIONS.contains(&key.as_str()));
        if !unknown.is_empty() && has_known {
            for key in unknown {
                map.remove(&key);
                notes.push(format!(
                    "判据 {} 删掉了不认的条件「{key}」（可用：{}）",
                    index + 1,
                    CRITERION_CONDITIONS.join(" / ")
                ));
            }
        }
        // 补名字。
        let missing = map
            .get("label")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .is_none();
        if missing {
            let label = infer_criterion_label(map);
            map.insert(
                "label".to_string(),
                serde_json::Value::String(label.clone()),
            );
            notes.push(format!("判据 {} 补上了名字「{label}」", index + 1));
        }
    }
    notes
}

/// 把一步里「明显写错了但猜得出意图」的地方改对。
///
/// 2026-11-07 的事故：模型连续 5 次 `plan_submit` 被拒，每次都是差一点点——
/// `date_from`（应为 `after`）、`count`（`mail_read` 没有这个参数）、
/// 判据少写 `label`、推理步却带了 `tool`。每一次被拒都是一次模型往返，
/// 5 次往返把请求密度顶到限流线上，整轮以 429 收场。
///
/// 这里的原则：**能唯一确定意图的就替模型改对**，改完的说明回灌给它，
/// 让它知道下轮怎么写；只有改不动的才交给 [`validate_plan_v2`] 报错。
pub fn repair_plan_step(step: &mut PlanStep) -> Vec<StepRepair> {
    let mut repairs = Vec::new();

    // 1) 工具步的 capability：从工具名反推。
    if let Some(tool) = step.tool
        && step.capability == Capability::General
    {
        let inferred = infer_capability(tool);
        if inferred != Capability::General {
            step.capability = inferred;
            repairs.push(StepRepair {
                note: format!(
                    "第 {} 步的 capability 补成了「{}」",
                    step.id,
                    inferred.label()
                ),
            });
        }
    }

    // 2) 推理/提问步却带了 tool：多半是模型想调工具，把 kind 改成 tool。
    //
    // 这一条值得单独修：事故里模型为了一步 reason 反复改 kind，
    // 改了 4 次，每次都撞「不能带 tool / args」。
    if matches!(step.kind, StepKind::Reason | StepKind::Ask) && step.tool.is_some() {
        step.kind = StepKind::Tool;
        repairs.push(StepRepair {
            note: format!("第 {} 步写了 tool，已按工具步处理", step.id),
        });
    }

    // 3) 工具步的 expect 缺失：用标题兜底。
    //
    // expect 是「拿到什么才算成功」，缺了它模型在自检时无从判断。
    // 兜底成标题的复述虽然不如模型自己写的好，但比整份计划被拒划算。
    if step.kind == StepKind::Tool
        && step
            .expect
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .is_none()
    {
        let fallback = format!("完成「{}」", step.title.trim());
        step.expect = Some(fallback.clone());
        repairs.push(StepRepair {
            note: format!("第 {} 步的 expect 补成了「{fallback}」", step.id),
        });
    }

    // 4) 参数名修正 + 判据修复。
    if let Some(tool) = step.tool
        && let Some(args) = step.args.as_mut()
    {
        for renamed in repair_argument_names(tool, args) {
            repairs.push(StepRepair {
                note: format!("第 {} 步的参数名 {renamed}", step.id),
            });
        }
        for note in repair_criteria(args) {
            repairs.push(StepRepair { note });
        }
    }

    repairs
}

/// 计划期就把「参数填得不对」拦下来。
///
/// D1 那类错误的代价是极高的：schema 允许、实现拒绝、报错又不说明缺什么，
/// 模型只能换一种说法再撞一次。计划期校验把「改参数」提前到「还没开始执行」。
pub fn validate_plan_v2(
    prev: Option<&Plan>,
    next: &Plan,
    allowed_tools: &[AiToolName],
) -> Result<Plan, CoreError> {
    let goal = normalized_goal(prev, next)?;

    if next.steps.is_empty() {
        return Err(CoreError::validation(
            "计划至少要有一步：把用户要的成果拆成 1–8 步",
        ));
    }
    if next.steps.len() > MAX_PLAN_STEPS {
        return Err(CoreError::validation(format!(
            "计划最多 {MAX_PLAN_STEPS} 步（收到 {}）：合并同类动作，别把每一步工具调用都拆成一步",
            next.steps.len()
        )));
    }

    let mut seen: Vec<u8> = Vec::new();
    let mut steps: Vec<PlanStep> = Vec::with_capacity(next.steps.len());
    for step in &next.steps {
        if step.id == 0 || step.id as usize > MAX_PLAN_STEPS {
            return Err(CoreError::validation(format!(
                "第 {} 步的编号超出范围：id 必须在 1–{MAX_PLAN_STEPS} 之间且从 1 开始",
                step.id
            )));
        }
        if seen.contains(&step.id) {
            return Err(CoreError::validation(format!(
                "第 {} 步的编号重复了：每步要有唯一 id",
                step.id
            )));
        }
        seen.push(step.id);

        let title = step.title.trim();
        if title.is_empty() {
            return Err(CoreError::validation(format!(
                "第 {} 步缺少 title：写清这一步要做什么（不超过 40 字）",
                step.id
            )));
        }
        if title.chars().count() > MAX_STEP_TITLE_CHARS {
            return Err(CoreError::validation(format!(
                "第 {} 步的 title 太长（{} 字，上限 {MAX_STEP_TITLE_CHARS}）",
                step.id,
                title.chars().count()
            )));
        }
        for vague in ["处理", "看情况", "等等", "具体问题具体分析"] {
            if title.starts_with(vague) {
                return Err(CoreError::validation(format!(
                    "第 {} 步的 title「{title}」太笼统：写清**做什么、对什么做**，\
例如「检索星标邮件」而不是「处理邮件」",
                    step.id
                )));
            }
        }

        match step.kind {
            StepKind::Tool => {
                let Some(tool) = step.tool else {
                    return Err(CoreError::validation(format!(
                        "第 {} 步是工具步，但没写 tool：填上要调的工具名，\
或者把 kind 改成 reason",
                        step.id
                    )));
                };
                if !allowed_tools.contains(&tool) {
                    return Err(CoreError::validation(format!(
                        "第 {} 步要用的工具 {} 不在可用列表里：可用的有 {}",
                        step.id,
                        tool.as_str(),
                        allowed_tools
                            .iter()
                            .map(|item| item.as_str())
                            .collect::<Vec<_>>()
                            .join(" / ")
                    )));
                }
            }
            StepKind::Reason | StepKind::Ask => {
                if step.tool.is_some() || step.args.is_some() {
                    return Err(CoreError::validation(format!(
                        "第 {} 步是 {} 步，不能带 tool / args：\
要调工具就把 kind 改成 tool",
                        step.id,
                        step.kind.label()
                    )));
                }
            }
        }

        if step.kind == StepKind::Ask && step.id != next.steps.last().map(|s| s.id).unwrap_or(0) {
            return Err(CoreError::validation(format!(
                "第 {} 步是「等用户确认」，只能放在最后一步：问完就停，后面的步骤没有意义",
                step.id
            )));
        }

        if let Some(args) = step.args.as_ref() {
            let bytes = serde_json::to_string(args)
                .map(|text| text.len())
                .unwrap_or(usize::MAX);
            if bytes > MAX_STEP_ARGS_BYTES {
                return Err(CoreError::validation(format!(
                    "第 {} 步的 args 太大（{bytes} 字节，上限 {MAX_STEP_ARGS_BYTES}）",
                    step.id
                )));
            }
            for reference in refs_in_args(args) {
                if reference >= step.id {
                    return Err(CoreError::validation(format!(
                        "第 {} 步的 args 引用了第 {reference} 步的结果：只能引用**前面的**步骤",
                        step.id
                    )));
                }
            }
            if let Some(tool) = step.tool {
                check_args_against_schema(step.id, tool, args)?;
                validate_criteria(step.id, tool, args)?;
            }
        }

        let expect = step.expect.as_deref().map(str::trim).unwrap_or("");
        if expect.is_empty() {
            return Err(CoreError::validation(format!(
                "第 {} 步缺少 expect：用一句话写清「这一步拿到什么才算成功」，\
例如「列出星标邮件，0 封也要说明」",
                step.id
            )));
        }
        if expect.chars().count() > MAX_STEP_EXPECT_CHARS {
            return Err(CoreError::validation(format!(
                "第 {} 步的 expect 太长（上限 {MAX_STEP_EXPECT_CHARS} 字）",
                step.id
            )));
        }

        let mut out = PlanStep::new(step.id, title, step.capability, PlanStatus::Pending);
        out.kind = step.kind;
        out.tool = step.tool;
        out.args = step.args.clone();
        out.expect = Some(expect.to_string());
        out.on_empty = step.on_empty;
        steps.push(out);
    }

    steps.sort_by_key(|step| step.id);
    let now = next.updated_at.max(next.created_at);
    let mut plan = Plan::v2(&goal, steps, now);
    if let Some(prev) = prev
        && !prev.id.is_empty()
    {
        plan.id = prev.id.clone();
        plan.created_at = prev.created_at;
        plan.replans = prev.replans.clone();
        plan.history = prev.history.clone();
    }
    Ok(plan)
}

/// 校验一组判据。
///
/// 判据是模型对定性需求的翻译结果，所以校验的重点不是「格式对不对」，
/// 而是「它有没有真的在做判断」：
/// - 每条都要有名字：没有名字的判据用户看不见，模型就等于没说出自己的理解；
/// - 每条都要有可执行条件：空条件的判据会让并集退化成全量，
///   那比不判断更糟；
/// - 条件里的参数名必须在允许集合里：拼错了要到执行期才炸，
///   那时模型已经花掉一次机会。
fn validate_criteria(
    step_id: u8,
    tool: AiToolName,
    args: &serde_json::Value,
) -> Result<(), CoreError> {
    let criteria = parse_criteria(args)?;
    if criteria.is_empty() {
        return Ok(());
    }
    if tool != AiToolName::EmailSearch {
        return Err(CoreError::validation(format!(
            "第 {step_id} 步的 criteria 只对 email_search 有意义（它是给检索用的）"
        )));
    }
    Ok(())
}

/// args 里的每个键都必须在该工具的参数表里。
///
/// 这一条专门治「参数名拼错 / 用了该工具没有的参数」：执行期报错是一句笼统的
/// 「参数不合法」，模型看不出哪里错；计划期可以直接点名。
fn check_args_against_schema(
    step_id: u8,
    tool: AiToolName,
    args: &serde_json::Value,
) -> Result<(), CoreError> {
    let Some(names) = crate::ai::tool_argument_names(tool) else {
        return Ok(());
    };
    let serde_json::Value::Object(map) = args else {
        return Err(CoreError::validation(format!(
            "第 {step_id} 步的 args 必须是对象（例如 {{\"unread_only\": true}}）"
        )));
    };
    for key in map.keys() {
        // `motive` / `step_id` 是引擎约定字段，不在业务 schema 里。
        if key == "motive" || key == "step_id" {
            continue;
        }
        if !names.contains(key) {
            return Err(CoreError::validation(format!(
                "第 {step_id} 步的 args 里有 {} 不认识的参数「{key}」：可用的是 {}",
                tool.as_str(),
                names.join(" / ")
            )));
        }
    }
    // 判据组本身就算「给了条件」——每条判据内部都是一组可执行条件。
    if tool == AiToolName::EmailSearch
        && !map.is_empty()
        && !map.contains_key("criteria")
        && !EMAIL_SEARCH_CONDITIONS
            .iter()
            .any(|key| map.contains_key(*key))
    {
        return Err(CoreError::validation(format!(
            "第 {step_id} 步的 args 缺少检索条件：至少给一个 {}（只给 limit 不算条件）",
            EMAIL_SEARCH_CONDITIONS.join(" / ")
        )));
    }
    Ok(())
}

/// `email_search` 里算「条件」的参数。**只给 limit 不算条件**。
///
/// `criteria` 是判据组：它内部就是一组条件，所以它也算「给了条件」。
/// 这里**没有** `important_only`——「什么算重要」由模型自己写成判据，
/// 引擎不替它决定任何一种口径。
pub const EMAIL_SEARCH_CONDITIONS: &[&str] = &[
    "query",
    "from",
    "to",
    "subject",
    "after",
    "before",
    "folder",
    "unread_only",
    "flagged_only",
    "days",
    "criteria",
];

/// 一次重列的输入（来自 SelfCheck，不是来自计划 JSON）。
pub struct ReplanRequest<'a> {
    pub diagnosis: Diagnosis,
    pub reason: &'a str,
    pub changes: &'a str,
    pub at: i64,
}

#[derive(Debug)]
pub struct ReplanOutcome {
    /// `None` = 剔除失败指纹后一步不剩，应当直接收尾。
    pub plan: Option<Plan>,
    /// 被剔除的步编号（UI 与日志要能看到「这次重列砍掉了什么」）。
    pub dropped: Vec<u8>,
}

/// 重列的硬约束。
///
/// 最关键的是第 2 条：新计划里与**历史失败指纹**重复的步直接剔除。
/// 没有它，「重列」就会变成「重新抽卡」——模型把同样的调用再发一遍，
/// 用户看到的是「AI 认真地又失败了一次」。
pub fn validate_replan(
    prev: &Plan,
    next: &Plan,
    request: &ReplanRequest<'_>,
    failed_fingerprints: &[String],
) -> Result<ReplanOutcome, CoreError> {
    let reason = request.reason.trim();
    if reason.chars().count() < 8 {
        return Err(CoreError::validation(
            "重列必须写清「这一轮错在哪」（reason，至少 8 个字）：\
只说「失败了」等于没说",
        ));
    }
    let changes = request.changes.trim();
    if changes.chars().count() < 4 {
        return Err(CoreError::validation(
            "重列必须写清「这次改了什么」（changes，至少 4 个字）",
        ));
    }
    if prev.replans.len() >= MAX_REPLANS as usize {
        return Err(CoreError::validation(format!(
            "已达重列上限（{MAX_REPLANS} 次）：请直接给出当前能给出的最好结果"
        )));
    }

    let mut drafted = next.clone();
    drafted.version = PLAN_VERSION_V2;
    let mut merged = validate_plan_v2(Some(prev), &drafted, &allowed_tool_list(prev))?;

    let mut dropped = Vec::new();
    merged.steps.retain(|step| {
        let Some(fingerprint) = step_fingerprint(step) else {
            return true;
        };
        if failed_fingerprints.contains(&fingerprint) {
            dropped.push(step.id);
            false
        } else {
            true
        }
    });

    if merged.steps.is_empty() {
        return Ok(ReplanOutcome {
            plan: None,
            dropped,
        });
    }

    let failed_steps = prev
        .steps
        .iter()
        .filter(|s| matches!(s.status, PlanStatus::Failed | PlanStatus::Skipped))
        .map(|s| s.id)
        .collect();
    merged.replans = prev.replans.clone();
    merged.replans.push(ReplanRecord {
        round: prev.replans.len() as u8 + 1,
        diagnosis: request.diagnosis,
        reason: truncate_chars(reason, MAX_STEP_EVIDENCE_CHARS),
        changes: truncate_chars(changes, MAX_STEP_EVIDENCE_CHARS),
        failed_steps,
        at: request.at,
    });
    merged.push_history(prev.clone());
    Ok(ReplanOutcome {
        plan: Some(merged),
        dropped,
    })
}

/// 重列时允许的工具集合：沿用上一份计划里出现过的工具 + 全量能力工具。
///
/// 单看「上一份计划的工具」会漏掉「换个工具再试」这种正确改法，
/// 所以这里放宽到全量；真正的约束是 `failed_fingerprints`。
fn allowed_tool_list(prev: &Plan) -> Vec<AiToolName> {
    let mut tools: Vec<AiToolName> = prev.steps.iter().filter_map(|s| s.tool).collect();
    for spec in crate::ai::TOOL_SPECS.iter() {
        if !spec.name.is_bookkeeping() && !tools.contains(&spec.name) {
            tools.push(spec.name);
        }
    }
    tools
}

/// 一步的调用指纹；纯推理步返回 `None`。
pub fn step_fingerprint(step: &PlanStep) -> Option<String> {
    let tool = step.tool?;
    Some(crate::ai::call_fingerprint(tool, step.args.as_ref()))
}

fn normalized_goal(prev: Option<&Plan>, next: &Plan) -> Result<String, CoreError> {
    let goal = next.goal.trim();
    if goal.is_empty() {
        return match prev {
            // 后续调用可以不重复说目标。
            Some(prev) if !prev.goal.is_empty() => Ok(prev.goal.clone()),
            _ => Err(CoreError::validation(
                "清单缺少目标（goal）：第一次调用 plan_update 必须说清这次要完成什么",
            )),
        };
    }
    Ok(truncate_chars(goal, MAX_PLAN_GOAL_CHARS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn step(id: u8, title: &str, status: PlanStatus) -> PlanStep {
        PlanStep::new(id, title, Capability::Mail, status)
    }

    fn draft(goal: &str, steps: Vec<PlanStep>) -> Plan {
        Plan {
            id: String::new(),
            goal: goal.to_string(),
            steps,
            created_at: 1_760_000_000,
            updated_at: 1_760_000_000,
            ..Plan::default()
        }
    }

    #[test]
    fn creates_plan_and_fills_id() {
        let next = draft("找报价单", vec![step(1, "检索邮件", PlanStatus::Pending)]);
        let plan = validate_plan(None, &next).expect("应当通过");
        assert_eq!(plan.id, "p_1760000000");
        assert_eq!(plan.goal, "找报价单");
        assert_eq!(plan.created_at, 1_760_000_000);
    }

    #[test]
    fn first_plan_without_goal_is_rejected() {
        let next = draft("   ", vec![step(1, "检索邮件", PlanStatus::Pending)]);
        let err = validate_plan(None, &next).expect_err("缺目标要拒绝");
        assert!(
            err.user_message().contains("goal"),
            "{}",
            err.user_message()
        );
    }

    #[test]
    fn step_count_boundaries() {
        let empty = draft("目标", Vec::new());
        assert!(validate_plan(None, &empty).is_err(), "0 步应拒绝");

        let one = draft("目标", vec![step(1, "一", PlanStatus::Pending)]);
        assert!(validate_plan(None, &one).is_ok(), "1 步应通过");

        let eight: Vec<PlanStep> = (1..=8)
            .map(|id| step(id, &format!("第 {id} 步"), PlanStatus::Pending))
            .collect();
        assert!(
            validate_plan(None, &draft("目标", eight)).is_ok(),
            "8 步应通过"
        );
    }

    #[test]
    fn ninth_step_is_rejected() {
        // 编号上限就是 8，第 9 步在 id 校验处就会被拦下。
        let mut steps: Vec<PlanStep> = (1..=8)
            .map(|id| step(id, &format!("第 {id} 步"), PlanStatus::Pending))
            .collect();
        steps.push(step(9, "第 9 步", PlanStatus::Pending));
        let err = validate_plan(None, &draft("目标", steps)).expect_err("9 步要拒绝");
        assert!(err.user_message().contains("1–8"), "{}", err.user_message());
    }

    #[test]
    fn title_is_immutable_after_creation() {
        let first = validate_plan(
            None,
            &draft("目标", vec![step(1, "检索邮件", PlanStatus::Running)]),
        )
        .expect("首建");
        let mut changed = first.clone();
        changed.steps[0].title = "改个标题".to_string();
        let err = validate_plan(Some(&first), &changed).expect_err("改标题要拒绝");
        assert!(
            err.user_message().contains("标题不能修改"),
            "{}",
            err.user_message()
        );
    }

    #[test]
    fn status_cannot_go_backwards() {
        let first = validate_plan(
            None,
            &draft("目标", vec![step(1, "检索邮件", PlanStatus::Running)]),
        )
        .expect("首建");
        let mut back = first.clone();
        back.steps[0].status = PlanStatus::Pending;
        let err = validate_plan(Some(&first), &back).expect_err("不允许回退");
        assert!(
            err.user_message().contains("不能回退"),
            "{}",
            err.user_message()
        );

        // 终态之间的挪动由「已结束」那条规则先拦住，文案也要说得清。
        let done = validate_plan(
            None,
            &draft("目标", vec![step(1, "检索邮件", PlanStatus::Done)]),
        )
        .expect("首建");
        let mut reuse = done.clone();
        reuse.steps[0].status = PlanStatus::Running;
        let err = validate_plan(Some(&done), &reuse).expect_err("终态不可改");
        assert!(
            err.user_message().contains("已经是"),
            "{}",
            err.user_message()
        );
    }

    #[test]
    fn terminal_step_cannot_switch_to_another_terminal() {
        let first = validate_plan(
            None,
            &draft("目标", vec![step(1, "检索邮件", PlanStatus::Done)]),
        )
        .expect("首建");
        let mut changed = first.clone();
        changed.steps[0].status = PlanStatus::Failed;
        let err = validate_plan(Some(&first), &changed).expect_err("终态不可改");
        assert!(
            err.user_message().contains("已经是"),
            "{}",
            err.user_message()
        );
    }

    #[test]
    fn partial_update_keeps_untouched_steps() {
        let first = validate_plan(
            None,
            &draft(
                "目标",
                vec![
                    step(1, "检索邮件", PlanStatus::Running),
                    step(2, "查日程", PlanStatus::Pending),
                ],
            ),
        )
        .expect("首建");

        // 只回传第 1 步（模型常见的增量写法）。
        let partial = draft("", vec![step(1, "检索邮件", PlanStatus::Done)]);
        let merged = validate_plan(Some(&first), &partial).expect("增量应通过");
        assert_eq!(merged.steps.len(), 2, "没提到的步骤要保留");
        assert_eq!(merged.steps[0].status, PlanStatus::Done);
        assert_eq!(merged.steps[1].status, PlanStatus::Pending);
        assert_eq!(merged.goal, "目标", "没传 goal 时沿用原目标");
    }

    #[test]
    fn duplicate_step_ids_are_rejected() {
        let steps = vec![
            step(1, "第一步", PlanStatus::Pending),
            step(1, "又是第一步", PlanStatus::Pending),
        ];
        let err = validate_plan(None, &draft("目标", steps)).expect_err("重复编号要拒绝");
        assert!(
            err.user_message().contains("重复"),
            "{}",
            err.user_message()
        );
    }

    #[test]
    fn open_steps_skip_marks_status_and_evidence() {
        let mut plan = validate_plan(
            None,
            &draft(
                "目标",
                vec![
                    step(1, "已完成", PlanStatus::Done),
                    step(2, "进行中", PlanStatus::Running),
                    step(3, "还没开始", PlanStatus::Pending),
                ],
            ),
        )
        .expect("首建");

        assert!(plan.skip_running("用户停止生成"));
        assert_eq!(plan.steps[1].status, PlanStatus::Skipped);
        assert_eq!(plan.steps[2].status, PlanStatus::Pending, "未开始的步不动");
        assert!(plan.steps[0].evidence.is_none(), "已完成的步不受影响");

        assert!(plan.skip_open("上次回复被中断"));
        assert_eq!(plan.steps[2].status, PlanStatus::Skipped);
        assert_eq!(plan.open_steps(), 0);
    }

    #[test]
    fn summary_line_lists_open_steps() {
        let plan = validate_plan(
            None,
            &draft(
                "找报价单",
                vec![
                    step(1, "检索邮件", PlanStatus::Done),
                    step(2, "查日程", PlanStatus::Pending),
                ],
            ),
        )
        .expect("首建");
        let line = plan.summary_line();
        assert!(line.contains("1/2"), "{line}");
        assert!(line.contains("查日程"), "{line}");
    }

    // -----------------------------------------------------------------------
    // v2
    // -----------------------------------------------------------------------

    fn tool_step(id: u8, title: &str, tool: AiToolName, args: serde_json::Value) -> PlanStep {
        let mut step = PlanStep::new(id, title, Capability::Mail, PlanStatus::Pending);
        step.tool = Some(tool);
        step.args = Some(args);
        step.expect = Some("拿到结果".to_string());
        step
    }

    fn reason_step(id: u8, title: &str) -> PlanStep {
        let mut step = PlanStep::new(id, title, Capability::Mail, PlanStatus::Pending);
        step.kind = StepKind::Reason;
        step.expect = Some("给出结论".to_string());
        step
    }

    fn v2_draft(goal: &str, steps: Vec<PlanStep>) -> Plan {
        Plan {
            goal: goal.to_string(),
            steps,
            version: PLAN_VERSION_V2,
            created_at: 1_760_000_000,
            updated_at: 1_760_000_000,
            ..Plan::default()
        }
    }

    fn allowed() -> Vec<AiToolName> {
        vec![AiToolName::EmailSearch, AiToolName::MailRead]
    }

    // -----------------------------------------------------------------------
    // 宽容修复：2026-11-07 事故的直接复现
    // -----------------------------------------------------------------------

    /// 事故里模型把 `after` 写成 `date_from`，整份计划被拒。
    ///
    /// 它接着又改了 4 次参数名，每一次都是一次模型往返；5 次往返把请求
    /// 密度顶到限流线上，整轮以「请求过于频繁」失败——而模型其实已经
    /// 查到了 20 封邮件。
    #[test]
    fn wrong_argument_name_is_renamed_instead_of_rejected() {
        let mut step = tool_step(
            1,
            "检索近 30 天邮件",
            AiToolName::EmailSearch,
            json!({ "date_from": "2026-10-01" }),
        );
        let repairs = repair_plan_step(&mut step);
        assert_eq!(
            step.args.as_ref().and_then(|args| args.get("after")),
            Some(&json!("2026-10-01")),
            "date_from 要被改成 after"
        );
        assert!(
            repairs.iter().any(|r| r.note.contains("date_from → after")),
            "改名要有说明：{repairs:?}"
        );
        // 改完之后这份计划能过校验——这是关键：不再被拒。
        validate_plan_v2(None, &v2_draft("目标", vec![step]), &allowed())
            .expect("改过之后应该能过校验");
    }

    /// `count` 在 `mail_read` 上没有对应参数（它只认 `message_id`），
    /// 这种猜不出的错仍然要报错——静默丢掉会让检索悄悄变了味。
    #[test]
    fn unguessable_argument_name_is_still_rejected() {
        let step = tool_step(1, "读正文", AiToolName::MailRead, json!({ "count": 3 }));
        let err = validate_plan_v2(None, &v2_draft("目标", vec![step]), &allowed())
            .expect_err("猜不出的参数名仍然要报错");
        assert!(
            err.user_message().contains("count"),
            "{}",
            err.user_message()
        );
    }

    /// 事故里模型为了一步 reason 连改 4 次 capability。
    ///
    /// 现在工具步的 capability 由引擎按工具名反推，模型不必猜。
    #[test]
    fn tool_step_capability_is_inferred_from_the_tool_name() {
        let mut step = tool_step(
            1,
            "检索邮件",
            AiToolName::EmailSearch,
            json!({ "days": 30 }),
        );
        step.capability = Capability::General;
        let repairs = repair_plan_step(&mut step);
        assert_eq!(step.capability, Capability::Mail);
        assert!(repairs.iter().any(|r| r.note.contains("capability")));
    }

    /// 事故里模型给 reason 步写了 tool，撞上「不能带 tool / args」。
    ///
    /// 它显然是想调工具，kind 写错了——改成 tool 步比拒收整份计划划算。
    #[test]
    fn reason_step_carrying_a_tool_is_promoted_to_a_tool_step() {
        let mut step = reason_step(2, "归纳结论");
        step.tool = Some(AiToolName::EmailSearch);
        let repairs = repair_plan_step(&mut step);
        assert_eq!(step.kind, StepKind::Tool);
        assert!(repairs.iter().any(|r| r.note.contains("tool")));
        // 改完之后这份计划能过校验。
        validate_plan_v2(None, &v2_draft("目标", vec![step]), &allowed())
            .expect("改过之后应该能过校验");
    }

    /// 事故里判据项漏了 `label`，被整份计划拒掉。
    ///
    /// 判据名是给人看的，引擎能从条件反推一个够用的名字。
    #[test]
    fn criterion_without_label_gets_one_inferred_from_its_conditions() {
        let mut step = tool_step(
            1,
            "检索邮件",
            AiToolName::EmailSearch,
            json!({ "criteria": [{ "flagged_only": true, "days": 7 }] }),
        );
        let repairs = repair_plan_step(&mut step);
        let label = step
            .args
            .as_ref()
            .and_then(|args| args.get("criteria"))
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("label"))
            .and_then(Value::as_str)
            .expect("判据要有名字");
        assert!(label.contains("已星标"), "反推的名字要能看懂：{label}");
        assert!(repairs.iter().any(|r| r.note.contains("名字")));
        validate_plan_v2(None, &v2_draft("目标", vec![step]), &allowed())
            .expect("改过之后应该能过校验");
    }

    /// 判据里写了不认的条件键：删掉它（前提是这条判据还有别的条件）。
    ///
    /// 删条件是有代价的（判据范围会变），所以只有在这条判据还剩别的
    /// 条件时才删——删完就空的话，宁可留着报错。
    #[test]
    fn unknown_criterion_key_is_dropped_when_other_conditions_remain() {
        let mut step = tool_step(
            1,
            "检索邮件",
            AiToolName::EmailSearch,
            json!({ "criteria": [{ "label": "近 30 天", "days": 30, "description": "随便写" }] }),
        );
        repair_plan_step(&mut step);
        let criterion = &step
            .args
            .as_ref()
            .and_then(|args| args.get("criteria"))
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .expect("判据还在");
        assert!(
            criterion.get("description").is_none(),
            "不认的条件键要删掉：{criterion}"
        );
        assert_eq!(criterion.get("days"), Some(&json!(30)), "真条件要留着");
    }

    /// 工具步漏了 expect：用标题兜底，而不是整份计划被拒。
    #[test]
    fn missing_expect_is_filled_from_the_title() {
        let mut step = tool_step(
            1,
            "检索近 30 天邮件",
            AiToolName::EmailSearch,
            json!({ "days": 30 }),
        );
        step.expect = None;
        let repairs = repair_plan_step(&mut step);
        assert_eq!(step.expect.as_deref(), Some("完成「检索近 30 天邮件」"));
        assert!(repairs.iter().any(|r| r.note.contains("expect")));
        validate_plan_v2(None, &v2_draft("目标", vec![step]), &allowed())
            .expect("补过之后应该能过校验");
    }

    #[test]
    fn v2_tool_step_must_name_its_tool() {
        let mut step = PlanStep::new(1, "检索邮件", Capability::Mail, PlanStatus::Pending);
        step.expect = Some("列出结果".to_string());
        let err = validate_plan_v2(None, &v2_draft("目标", vec![step]), &allowed())
            .expect_err("工具步没写 tool 要拒绝");
        assert!(
            err.user_message().contains("没写 tool"),
            "{}",
            err.user_message()
        );
    }

    #[test]
    fn v2_rejects_unknown_arg_key() {
        let step = tool_step(
            1,
            "检索星标邮件",
            AiToolName::EmailSearch,
            json!({
                "flagged_only": true,
                "folderx": "INBOX"
            }),
        );
        let err = validate_plan_v2(None, &v2_draft("目标", vec![step]), &allowed())
            .expect_err("拼错的参数名要在计划期拦下");
        let text = err.user_message();
        assert!(text.contains("folderx"), "{text}");
        assert!(text.contains("可用的是"), "{text}");
    }

    #[test]
    fn v2_rejects_search_args_without_any_condition() {
        let step = tool_step(
            1,
            "检索邮件",
            AiToolName::EmailSearch,
            json!({ "limit": 10 }),
        );
        let err = validate_plan_v2(None, &v2_draft("目标", vec![step]), &allowed())
            .expect_err("只给 limit 不算检索条件");
        let text = err.user_message();
        assert!(text.contains("缺少检索条件"), "{text}");
        assert!(text.contains("flagged_only"), "文案要列出可用条件：{text}");
    }

    #[test]
    fn v2_rejects_forward_reference() {
        let mut second = tool_step(
            2,
            "读第二封",
            AiToolName::MailRead,
            json!({
                "message_id": "{{step.2.messages.0.message_id}}"
            }),
        );
        second.expect = Some("拿到正文".to_string());
        let err = validate_plan_v2(None, &v2_draft("目标", vec![second]), &allowed())
            .expect_err("自引用要拒绝");
        assert!(
            err.user_message().contains("只能引用"),
            "{}",
            err.user_message()
        );
    }

    #[test]
    fn v2_reason_step_cannot_carry_tool() {
        let mut step = reason_step(1, "归纳");
        step.tool = Some(AiToolName::EmailSearch);
        let err = validate_plan_v2(None, &v2_draft("目标", vec![step]), &allowed())
            .expect_err("推理步不能带工具");
        assert!(
            err.user_message().contains("不能带"),
            "{}",
            err.user_message()
        );
    }

    #[test]
    fn v2_ask_step_must_be_last() {
        let mut ask = reason_step(1, "问用户要哪个邮箱");
        ask.kind = StepKind::Ask;
        let after = reason_step(2, "归纳");
        let err = validate_plan_v2(None, &v2_draft("目标", vec![ask, after]), &allowed())
            .expect_err("Ask 只能在最后一步");
        assert!(
            err.user_message().contains("最后一步"),
            "{}",
            err.user_message()
        );
    }

    #[test]
    fn v2_rejects_generic_title() {
        let step = reason_step(1, "处理邮件");
        let err = validate_plan_v2(None, &v2_draft("目标", vec![step]), &allowed())
            .expect_err("笼统标题要拒绝");
        assert!(
            err.user_message().contains("太笼统"),
            "{}",
            err.user_message()
        );
    }

    #[test]
    fn v2_steps_start_pending_and_plan_is_executable() {
        let plan = validate_plan_v2(
            None,
            &v2_draft(
                "找重要邮件",
                vec![
                    reason_step(1, "定重要判据"),
                    tool_step(
                        2,
                        "检索星标",
                        AiToolName::EmailSearch,
                        json!({ "flagged_only": true }),
                    ),
                ],
            ),
            &allowed(),
        )
        .expect("合法计划");
        assert_eq!(plan.version, PLAN_VERSION_V2);
        assert!(plan.is_executable());
        assert_eq!(plan.steps[0].status, PlanStatus::Pending);
        assert_eq!(plan.steps[1].attempts, 0);
        assert_eq!(plan.steps[1].kind, StepKind::Tool);
    }

    #[test]
    fn v1_plan_is_not_executable() {
        let plan = validate_plan(
            None,
            &draft("目标", vec![step(1, "检索", PlanStatus::Pending)]),
        )
        .expect("v1 合并仍然可用");
        assert!(!plan.is_executable(), "v1 计划不该被当成可执行计划");
    }

    #[test]
    fn record_attempt_fails_after_three_tries() {
        let mut plan = validate_plan_v2(
            None,
            &v2_draft(
                "目标",
                vec![tool_step(
                    1,
                    "检索",
                    AiToolName::EmailSearch,
                    json!({ "days": 7 }),
                )],
            ),
            &allowed(),
        )
        .expect("合法");

        assert_eq!(
            plan.record_attempt(
                1,
                AttemptOutcome::Failed(StepFailure::InvalidArgs {
                    detail: "缺少条件".to_string()
                })
            ),
            PlanStatus::Running
        );
        assert_eq!(plan.steps[0].attempts, 1);

        assert_eq!(
            plan.record_attempt(
                1,
                AttemptOutcome::Failed(StepFailure::InvalidArgs {
                    detail: "缺少条件".to_string()
                })
            ),
            PlanStatus::Running
        );
        assert_eq!(
            plan.record_attempt(
                1,
                AttemptOutcome::Failed(StepFailure::Transient {
                    detail: "超时".to_string()
                })
            ),
            PlanStatus::Failed,
            "第三次失败即终态"
        );
        assert_eq!(plan.steps[0].attempts, MAX_STEP_ATTEMPTS);
        assert!(plan.steps[0].failure.is_some());
    }

    #[test]
    fn record_attempt_keeps_evidence_short() {
        let mut plan = validate_plan_v2(
            None,
            &v2_draft(
                "目标",
                vec![tool_step(
                    1,
                    "检索",
                    AiToolName::EmailSearch,
                    json!({ "days": 7 }),
                )],
            ),
            &allowed(),
        )
        .expect("合法");
        plan.record_attempt(
            1,
            AttemptOutcome::Done {
                evidence: "命".repeat(500),
            },
        );
        let evidence = plan.steps[0].evidence.clone().unwrap();
        assert!(
            // `truncate_chars` 会在末尾补一个省略号，所以上限是 N+1。
            evidence.chars().count() <= MAX_STEP_EVIDENCE_CHARS + 1,
            "回执要截断，否则整段工具结果会进数据库"
        );
        assert_eq!(plan.steps[0].status, PlanStatus::Done);
    }

    #[test]
    fn reopen_only_once() {
        let mut plan = validate_plan_v2(
            None,
            &v2_draft(
                "目标",
                vec![tool_step(
                    1,
                    "检索",
                    AiToolName::EmailSearch,
                    json!({ "days": 7 }),
                )],
            ),
            &allowed(),
        )
        .expect("合法");
        plan.record_attempt(
            1,
            AttemptOutcome::Done {
                evidence: "命中 2 封".to_string(),
            },
        );
        assert!(plan.reopen_step(
            1,
            StepFailure::ExpectMiss {
                expect: "至少 3 封".to_string(),
                got: "2 封".to_string(),
            }
        ));
        assert_eq!(plan.steps[0].status, PlanStatus::Running);
        assert_eq!(plan.steps[0].attempts, 2);

        plan.record_attempt(
            1,
            AttemptOutcome::Done {
                evidence: "还是 2 封".to_string(),
            },
        );
        assert!(
            !plan.reopen_step(
                1,
                StepFailure::ExpectMiss {
                    expect: "至少 3 封".to_string(),
                    got: "2 封".to_string(),
                }
            ),
            "第三次不能再退：避免反复自我否定"
        );
    }

    #[test]
    fn skip_dependents_cascades_through_refs() {
        let mut plan = validate_plan_v2(
            None,
            &v2_draft(
                "目标",
                vec![
                    tool_step(
                        1,
                        "检索星标",
                        AiToolName::EmailSearch,
                        json!({ "flagged_only": true }),
                    ),
                    tool_step(
                        2,
                        "读第一封",
                        AiToolName::MailRead,
                        json!({
                            "message_id": "{{step.1.messages.0.message_id}}"
                        }),
                    ),
                    reason_step(3, "归纳"),
                ],
            ),
            &allowed(),
        )
        .expect("合法");
        plan.fail_step(
            1,
            StepFailure::Transient {
                detail: "超时".to_string(),
            },
        );
        assert!(plan.skip_dependents());
        assert_eq!(plan.steps[1].status, PlanStatus::Skipped);
        assert_eq!(plan.steps[1].failure, Some(StepFailure::Upstream));
        assert_eq!(
            plan.steps[2].status,
            PlanStatus::Pending,
            "无依赖的步不受影响"
        );
    }

    #[test]
    fn validate_replan_drops_steps_that_already_failed() {
        let mut prev = validate_plan_v2(
            None,
            &v2_draft(
                "找重要邮件",
                vec![
                    tool_step(
                        1,
                        "检索星标",
                        AiToolName::EmailSearch,
                        json!({ "flagged_only": true }),
                    ),
                    reason_step(2, "归纳"),
                ],
            ),
            &allowed(),
        )
        .expect("合法");
        prev.fail_step(
            1,
            StepFailure::Transient {
                detail: "检索超时".to_string(),
            },
        );
        let failed = vec![step_fingerprint(&prev.steps[0]).expect("指纹")];

        // 新计划里第一步与历史失败指纹完全一致 —— 必须被剔除，而不是再跑一遍。
        let next = v2_draft(
            "找重要邮件",
            vec![
                tool_step(
                    1,
                    "检索星标",
                    AiToolName::EmailSearch,
                    json!({ "flagged_only": true }),
                ),
                tool_step(
                    2,
                    "检索近 7 天未读",
                    AiToolName::EmailSearch,
                    json!({
                        "unread_only": true,
                        "days": 7
                    }),
                ),
                reason_step(3, "归纳"),
            ],
        );
        let request = ReplanRequest {
            diagnosis: Diagnosis::WrongArgs,
            reason: "第一步参数不对，星标查不到",
            changes: "改成查近 7 天未读",
            at: 1_760_000_100,
        };
        let outcome = validate_replan(&prev, &next, &request, &failed).expect("重列");
        assert_eq!(outcome.dropped, vec![1], "同指纹的步要剔除");
        let plan = outcome.plan.expect("还有两步，不该整份丢掉");
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.replans.len(), 1);
        assert_eq!(plan.replans[0].round, 1);
        assert_eq!(plan.replans[0].diagnosis, Diagnosis::WrongArgs);
        assert_eq!(plan.history.len(), 1, "旧计划要进 history");
    }

    #[test]
    fn replan_with_everything_dropped_yields_no_plan() {
        let mut prev = validate_plan_v2(
            None,
            &v2_draft(
                "目标",
                vec![tool_step(
                    1,
                    "检索",
                    AiToolName::EmailSearch,
                    json!({ "days": 7 }),
                )],
            ),
            &allowed(),
        )
        .expect("合法");
        prev.fail_step(
            1,
            StepFailure::Transient {
                detail: "超时".to_string(),
            },
        );
        let failed = vec![step_fingerprint(&prev.steps[0]).expect("指纹")];
        let next = v2_draft(
            "目标",
            vec![tool_step(
                1,
                "检索",
                AiToolName::EmailSearch,
                json!({ "days": 7 }),
            )],
        );
        let request = ReplanRequest {
            diagnosis: Diagnosis::ToolBroken,
            reason: "同一个调用已经失败过三次",
            changes: "原样重试",
            at: 1,
        };
        let outcome = validate_replan(&prev, &next, &request, &failed).expect("重列");
        assert!(outcome.plan.is_none(), "一步不剩时应当直接收尾");
    }

    #[test]
    fn replan_requires_a_real_reason() {
        let prev = validate_plan_v2(
            None,
            &v2_draft("目标", vec![reason_step(1, "归纳")]),
            &allowed(),
        )
        .expect("合法");
        let next = v2_draft("目标", vec![reason_step(1, "归纳")]);
        let request = ReplanRequest {
            diagnosis: Diagnosis::WrongPlan,
            reason: "失败",
            changes: "重来",
            at: 1,
        };
        let err = validate_replan(&prev, &next, &request, &[]).expect_err("理由太短要拒绝");
        assert!(
            err.user_message().contains("错在哪"),
            "{}",
            err.user_message()
        );
    }

    #[test]
    fn args_digest_turns_refs_into_step_numbers() {
        let step = tool_step(
            2,
            "读第一封",
            AiToolName::MailRead,
            json!({
                "message_id": "{{step.1.messages.0.message_id}}"
            }),
        );
        let digest = step.args_digest().expect("有摘要");
        assert!(digest.contains("第 1 步"), "{digest}");
        assert!(
            !digest.contains("{{"),
            "原始模板不该出现在清单条上：{digest}"
        );
    }

    /// 判据名要出现在清单条上——那是用户判断模型有没有理解错的唯一依据。
    #[test]
    fn args_digest_shows_criteria_labels() {
        let step = tool_step(
            1,
            "查邮件",
            AiToolName::EmailSearch,
            json!({
                "criteria": [
                    {"label": "已星标", "flagged_only": true},
                    {"label": "近 7 天未读", "unread_only": true, "days": 7}
                ]
            }),
        );
        let digest = step.args_digest().expect("有摘要");
        assert!(digest.contains("判据"), "{digest}");
        assert!(digest.contains("已星标"), "{digest}");
        assert!(digest.contains("近 7 天未读"), "{digest}");
        // 判据的原始条件不该挤进摘要——用户看的是名字，不是 JSON。
        assert!(!digest.contains("flagged_only"), "{digest}");
    }

    #[test]
    fn plan_v2_roundtrip_keeps_new_fields() {
        let plan = validate_plan_v2(
            None,
            &v2_draft(
                "目标",
                vec![tool_step(
                    1,
                    "检索",
                    AiToolName::EmailSearch,
                    json!({ "days": 7 }),
                )],
            ),
            &allowed(),
        )
        .expect("合法");
        let json = serde_json::to_string(&plan).expect("序列化");
        let back: Plan = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back, plan);
        assert_eq!(back.version, PLAN_VERSION_V2);
        assert_eq!(back.steps[0].tool, Some(AiToolName::EmailSearch));
    }

    fn check(verdict: Verdict, failed_step: Option<u8>) -> SelfCheck {
        SelfCheck {
            verdict,
            failed_step,
            diagnosis: Some(Diagnosis::WrongArgs),
            reason: "第一步的参数一直不对，查不到东西".to_string(),
            replan_hint: "改成按时间窗查".to_string(),
        }
    }

    #[test]
    fn selfcheck_parses_json_wrapped_in_prose() {
        let text = "我核对了一下：\n```json\n{\"verdict\":\"failed\",\"failed_step\":2,\
\"diagnosis\":\"missing_data\",\"reason\":\"正文没有缓存，读不出来\",\
\"replan_hint\":\"先让用户打开这封邮件\"}\n```";
        let check = SelfCheck::parse(text).expect("要能从散文里抠出 JSON");
        assert_eq!(check.verdict, Verdict::Failed);
        assert_eq!(check.failed_step, Some(2));
        assert_eq!(check.diagnosis, Some(Diagnosis::MissingData));
        assert!(check.can_replan());
    }

    #[test]
    fn selfcheck_rejects_garbage() {
        assert!(SelfCheck::parse("我觉得还行").is_none());
        assert!(
            SelfCheck::parse("{\"verdict\":\"unsure\"}").is_none(),
            "枚举外的值不认"
        );
    }

    #[test]
    fn selfcheck_needs_a_real_reason_to_replan() {
        let mut check = check(Verdict::Failed, Some(1));
        check.reason = "失败".to_string();
        assert!(!check.can_replan(), "理由是空话时不能据此重列");
    }

    #[test]
    fn reconcile_downgrades_false_claims_in_both_directions() {
        let mut plan = validate_plan_v2(
            None,
            &v2_draft(
                "目标",
                vec![
                    tool_step(1, "检索", AiToolName::EmailSearch, json!({ "days": 7 })),
                    reason_step(2, "归纳"),
                ],
            ),
            &allowed(),
        )
        .expect("合法");

        // 全部完成却说失败：不抹掉模型的原话，降为 partial。
        plan.steps[0].status = PlanStatus::Done;
        plan.steps[1].status = PlanStatus::Done;
        assert_eq!(
            reconcile(&check(Verdict::Failed, Some(1)), &plan),
            Verdict::Partial
        );

        // 有失败步却说完成：一律降级。
        plan.steps[0].status = PlanStatus::Failed;
        assert_eq!(
            reconcile(&check(Verdict::Done, None), &plan),
            Verdict::Partial
        );

        // 事实与自检一致时不动。
        assert_eq!(
            reconcile(&check(Verdict::Failed, Some(1)), &plan),
            Verdict::Failed
        );
        plan.steps[0].status = PlanStatus::Done;
        assert_eq!(reconcile(&check(Verdict::Done, None), &plan), Verdict::Done);
    }

    #[test]
    fn settle_running_closes_spinners_honestly() {
        let mut plan = validate_plan_v2(
            None,
            &v2_draft(
                "目标",
                vec![
                    tool_step(1, "检索", AiToolName::EmailSearch, json!({ "days": 7 })),
                    tool_step(
                        2,
                        "读正文",
                        AiToolName::MailRead,
                        json!({ "message_id": 1 }),
                    ),
                ],
            ),
            &allowed(),
        )
        .expect("合法");
        plan.steps[0].status = PlanStatus::Running;
        plan.steps[0].evidence = Some("0 命中".to_string());
        plan.steps[1].status = PlanStatus::Running;

        assert!(plan.settle_running());
        assert_eq!(plan.steps[0].status, PlanStatus::Done, "有回执就是执行过了");
        assert_eq!(
            plan.steps[1].status,
            PlanStatus::Skipped,
            "一次没执行要如实标跳过"
        );
        assert_eq!(plan.open_steps(), 0, "不能留永远转圈的步");
    }

    // ---- 放宽阶梯 ----

    #[test]
    fn relax_drops_the_strictest_condition_first() {
        // 星标最先被去掉：它是最容易「用户还没标」而空掉的条件。
        let first = relax_args(&json!({
            "flagged_only": true,
            "subject": "预算",
            "days": 7,
            "unread_only": true
        }))
        .expect("有可放宽的");
        assert!(first.get("flagged_only").is_none(), "{first}");
        assert_eq!(first.get("subject").unwrap(), "预算", "{first}");
        assert_eq!(first.get("days").unwrap(), 7, "{first}");

        // 然后是主题关键词。
        let second = relax_args(&first).expect("还有可放宽的");
        assert!(second.get("subject").is_none(), "{second}");
        assert_eq!(second.get("days").unwrap(), 7, "{second}");

        // 最后拉长时间窗到 30 天，**不整段丢掉**。
        let third = relax_args(&second).expect("还有可放宽的");
        assert_eq!(third.get("days").unwrap(), 30, "{third}");
        assert!(
            third.get("unread_only").is_some(),
            "其它条件要保留：{third}"
        );
    }

    #[test]
    fn relax_stops_when_nothing_is_left_to_relax() {
        // 时间窗已经 30 天：再放宽就等于「所有邮件」，那是换了一个问题。
        assert!(relax_args(&json!({ "days": 30, "unread_only": true })).is_none());
        // 只有布尔条件、没有可放宽的限定。
        assert!(relax_args(&json!({ "unread_only": true })).is_none());
    }

    #[test]
    fn relax_keeps_motive_and_step_id() {
        // 这两个不参与检索，但重试时要靠 `step_id` 绑回清单里的那一步。
        let relaxed = relax_args(&json!({
            "motive": "查邮件",
            "step_id": 2,
            "flagged_only": true
        }))
        .expect("可放宽");
        assert!(relaxed.get("flagged_only").is_none(), "{relaxed}");
        assert_eq!(relaxed.get("motive").unwrap(), "查邮件", "{relaxed}");
        assert_eq!(relaxed.get("step_id").unwrap(), 2, "{relaxed}");
    }

    // ---- 判据 ----

    /// 判据组解析：名字 + 条件，逐条分开。
    #[test]
    fn criteria_parse_splits_label_from_conditions() {
        let criteria = parse_criteria(&json!({
            "criteria": [
                {"label": "已星标", "flagged_only": true},
                {"label": "近 7 天未读", "unread_only": true, "days": 7}
            ]
        }))
        .expect("能解析");
        assert_eq!(criteria.len(), 2);
        assert_eq!(criteria[0].label, "已星标");
        assert_eq!(criteria[0].query.get("flagged_only").unwrap(), true);
        assert_eq!(criteria[1].label, "近 7 天未读");
        assert_eq!(criteria[1].query.get("days").unwrap(), 7);
    }

    /// 没有 `criteria` 时就是空组——普通条件走老路。
    #[test]
    fn criteria_absent_is_an_empty_group() {
        assert!(
            parse_criteria(&json!({ "query": "报告" }))
                .unwrap()
                .is_empty()
        );
        assert!(parse_criteria(&json!({})).unwrap().is_empty());
    }

    /// 没有名字的判据要拒：用户看不见它，模型就等于没说出自己的理解。
    #[test]
    fn criteria_without_label_is_rejected() {
        let err = parse_criteria(&json!({
            "criteria": [{"flagged_only": true}]
        }))
        .expect_err("要拒");
        assert!(err.to_string().contains("label"), "{err}");
    }

    /// 空条件的判据要拒：它会让并集退化成全量，比不判断更糟。
    #[test]
    fn criteria_without_condition_is_rejected() {
        let err = parse_criteria(&json!({
            "criteria": [{"label": "随便什么都算"}]
        }))
        .expect_err("要拒");
        assert!(err.to_string().contains("没有检索条件"), "{err}");
    }

    /// 条件名拼错要拒，并说清可用的是哪些。
    #[test]
    fn criteria_with_unknown_condition_is_rejected() {
        let err = parse_criteria(&json!({
            "criteria": [{"label": "星标", "starred_only": true}]
        }))
        .expect_err("要拒");
        assert!(err.to_string().contains("starred_only"), "{err}");
        assert!(err.to_string().contains("flagged_only"), "{err}");
    }

    /// 判据太多要拒：那说明模型在瞎试，不是在判断。
    #[test]
    fn too_many_criteria_is_rejected() {
        let items: Vec<serde_json::Value> = (0..(MAX_STEP_CRITERIA + 1))
            .map(|index| json!({"label": format!("判据{index}"), "flagged_only": true}))
            .collect();
        let err = parse_criteria(&json!({ "criteria": items })).expect_err("要拒");
        assert!(err.to_string().contains("上限"), "{err}");
    }

    /// 判据组的放宽：**去掉最宽的那一条**。
    ///
    /// 与普通条件的放宽是两种不同的逻辑：判据是模型对需求的翻译，
    /// 去掉一条等于放弃一种理解；保留剩下的比放宽剩下的更忠实。
    #[test]
    fn relax_drops_the_widest_criterion_first() {
        let first = relax_args(&json!({
            "criteria": [
                {"label": "已星标", "flagged_only": true},
                {"label": "近 7 天未读", "unread_only": true, "days": 7}
            ]
        }))
        .expect("有可放宽的");
        let items = first
            .get("criteria")
            .and_then(|v| v.as_array())
            .expect("还有判据");
        assert_eq!(items.len(), 1, "去掉了一条：{first}");
        assert_eq!(items[0]["label"], "已星标", "{first}");

        // 只剩一条时没有可放宽的了——不能把最后一条也丢掉，
        // 那就变成「什么都算」了。
        assert!(relax_args(&first).is_none());
    }

    /// 判据组的放宽不能顺手把普通条件也改掉——两组条件互不相干。
    #[test]
    fn relax_keeps_criteria_and_other_conditions_apart() {
        let relaxed = relax_args(&json!({
            "criteria": [
                {"label": "A", "flagged_only": true},
                {"label": "B", "unread_only": true}
            ],
            "limit": 5
        }))
        .expect("可放宽");
        assert!(relaxed.get("criteria").is_some(), "{relaxed}");
        assert_eq!(relaxed.get("limit").unwrap(), 5, "{relaxed}");
    }

    // ---- on_empty ----

    #[test]
    fn empty_result_is_not_a_failure() {
        let mut plan = validate_plan_v2(
            None,
            &v2_draft(
                "目标",
                vec![tool_step(
                    1,
                    "检索",
                    AiToolName::EmailSearch,
                    json!({ "days": 7 }),
                )],
            ),
            &allowed(),
        )
        .expect("合法");
        // 放宽已经用尽：仍然算完成，只是回执写明「没有结果」。
        let action = plan.apply_empty(1, "未找到", 3);
        assert_eq!(action, EmptyAction::Accept);
        assert_eq!(plan.steps[0].status, PlanStatus::Done);
        assert!(
            plan.steps[0]
                .evidence
                .as_deref()
                .is_some_and(|text| text.contains("放宽")),
            "{:?}",
            plan.steps[0].evidence
        );
        assert!(
            plan.steps[0].failure.is_none(),
            "0 命中不是失败：{:?}",
            plan.steps[0].failure
        );
    }

    #[test]
    fn on_empty_ask_stops_for_the_user() {
        let mut plan = validate_plan_v2(
            None,
            &v2_draft(
                "目标",
                vec![tool_step(
                    1,
                    "检索",
                    AiToolName::EmailSearch,
                    json!({ "days": 7 }),
                )],
            ),
            &allowed(),
        )
        .expect("合法");
        plan.steps[0].on_empty = OnEmpty::Ask;
        let action = plan.apply_empty(1, "未找到", 1);
        assert_eq!(action, EmptyAction::Ask);
        assert_eq!(
            plan.steps[0].status,
            PlanStatus::Running,
            "等用户回答时这一步还没结束"
        );
    }

    #[test]
    fn on_empty_accept_finishes_the_step() {
        let mut plan = validate_plan_v2(
            None,
            &v2_draft(
                "目标",
                vec![tool_step(
                    1,
                    "检索",
                    AiToolName::EmailSearch,
                    json!({ "days": 7 }),
                )],
            ),
            &allowed(),
        )
        .expect("合法");
        plan.steps[0].on_empty = OnEmpty::Accept;
        let action = plan.apply_empty(1, "未找到", 1);
        assert_eq!(action, EmptyAction::Accept);
        assert_eq!(plan.steps[0].status, PlanStatus::Done);
        assert_eq!(plan.steps[0].attempts, 1);
    }

    #[test]
    fn attempts_count_every_physical_try() {
        let mut plan = validate_plan_v2(
            None,
            &v2_draft(
                "目标",
                vec![tool_step(
                    1,
                    "检索",
                    AiToolName::EmailSearch,
                    json!({ "days": 7 }),
                )],
            ),
            &allowed(),
        )
        .expect("合法");
        // 一次模型调用 + 一次放宽重试 = 两次物理尝试。
        plan.record_attempts(
            1,
            2,
            AttemptOutcome::Done {
                evidence: "命中 1 封".to_string(),
            },
        );
        assert_eq!(plan.steps[0].attempts, 2);
        assert_eq!(plan.steps[0].status, PlanStatus::Done);
    }

    // ---- 统计与跨轮延续 ----

    #[test]
    fn stats_report_failures_and_slowest_step() {
        let mut plan = validate_plan_v2(
            None,
            &v2_draft(
                "目标",
                vec![
                    tool_step(1, "检索", AiToolName::EmailSearch, json!({ "days": 7 })),
                    tool_step(
                        2,
                        "读正文",
                        AiToolName::MailRead,
                        json!({ "message_id": 1 }),
                    ),
                    tool_step(3, "归纳", AiToolName::EmailSearch, json!({ "days": 3 })),
                ],
            ),
            &allowed(),
        )
        .expect("合法");
        plan.add_duration(1, 100);
        plan.add_duration(2, 900);
        plan.add_duration(3, 50);
        plan.steps[0].status = PlanStatus::Done;
        plan.steps[1].status = PlanStatus::Failed;
        plan.steps[2].attempts = 2;

        let stats = plan.stats();
        assert_eq!(stats.total_steps, 3);
        assert_eq!(stats.done_steps, 1);
        assert_eq!(stats.failed_steps, 1);
        assert_eq!(stats.retried_steps, 1, "试过两次的步要算进去");
        assert_eq!(stats.total_ms, 1050);
        assert_eq!(stats.slowest.as_ref().unwrap().0, 2, "最慢的是第 2 步");
        assert!((stats.failure_rate - 1.0 / 3.0).abs() < 1e-9);
        assert!(
            stats.summary().contains("失败率 33%"),
            "{}",
            stats.summary()
        );
    }

    #[test]
    fn resumable_detects_an_unfinished_plan() {
        let mut plan = validate_plan_v2(
            None,
            &v2_draft(
                "目标",
                vec![tool_step(
                    1,
                    "检索",
                    AiToolName::EmailSearch,
                    json!({ "days": 7 }),
                )],
            ),
            &allowed(),
        )
        .expect("合法");
        assert!(plan.is_resumable(), "还没开始的步可以续做");
        assert_eq!(plan.unfinished_summary(), "第 1 步「检索」");

        plan.steps[0].status = PlanStatus::Done;
        assert!(!plan.is_resumable(), "全做完的不用续");
        assert!(plan.unfinished_summary().is_empty());

        // 跳过的步也不算「没做完」：那不是用户的未竟之事。
        plan.steps[0].status = PlanStatus::Skipped;
        assert!(!plan.is_resumable());
    }

    #[test]
    fn legacy_plan_without_v2_fields_still_parses() {
        // 老库里的 plan 只有 title/status：解析不能失败，否则会被降级成「无清单」。
        let raw = r#"{"id":"p_1","goal":"旧目标","steps":[
            {"id":1,"capability":"mail","title":"旧步骤","status":"done"}
        ],"created_at":1,"updated_at":2}"#;
        let plan: Plan = serde_json::from_str(raw).expect("老数据要能读");
        assert_eq!(plan.version, PLAN_VERSION_V1);
        assert_eq!(plan.steps[0].kind, StepKind::Tool);
        assert_eq!(plan.steps[0].attempts, 0);
        assert!(!plan.is_executable());
    }

    /// 引擎记过事实后，模型复述的 evidence 不能覆盖它。
    ///
    /// 2026-11-07 的事故：email_search 实际返回 20 封，引擎把「检索到 20 封」
    /// 写进步骤；模型随后调 `plan_update` 写「拿到 50 封邮件摘要列表」，
    /// 把真实回执盖掉。清单是给用户对账的，对不上账的回执比没有更糟。
    #[test]
    fn engine_recorded_evidence_survives_a_model_restatement() {
        let mut first = step(1, "检索邮件", PlanStatus::Pending);
        first.tool = Some(crate::ai::AiToolName::EmailSearch);
        let plan = validate_plan(None, &draft("找邮件", vec![first])).expect("应当通过");

        // 引擎跑完工具：attempts 递增，evidence 是工具的真实回执。
        let mut engine_plan = plan.clone();
        engine_plan.record_attempts(
            1,
            1,
            AttemptOutcome::Done {
                evidence: "检索到 20 封".to_string(),
            },
        );
        assert_eq!(
            engine_plan.steps[0].evidence.as_deref(),
            Some("检索到 20 封")
        );

        // 模型随后回填：说「拿到 50 封」。
        let mut model_step = step(1, "检索邮件", PlanStatus::Done);
        model_step.tool = Some(crate::ai::AiToolName::EmailSearch);
        model_step.evidence = Some("已检索近7天邮件样本，拿到50封邮件摘要列表".to_string());
        let merged = validate_plan(Some(&engine_plan), &draft("找邮件", vec![model_step]))
            .expect("应当通过");

        assert_eq!(
            merged.steps[0].evidence.as_deref(),
            Some("检索到 20 封"),
            "引擎记下的真实回执不能被模型的复述覆盖"
        );
    }

    /// 但模型在引擎跑工具**之前**写的复述要照常采纳——
    /// 那一步引擎还没碰过，模型说的是它自己的计划。
    #[test]
    fn model_evidence_before_any_tool_run_is_kept() {
        let mut first = step(1, "检索邮件", PlanStatus::Pending);
        first.tool = Some(crate::ai::AiToolName::EmailSearch);
        let plan = validate_plan(None, &draft("找邮件", vec![first])).expect("应当通过");
        assert_eq!(plan.steps[0].attempts, 0);

        let mut model_step = step(1, "检索邮件", PlanStatus::Running);
        model_step.tool = Some(crate::ai::AiToolName::EmailSearch);
        model_step.evidence = Some("准备开始检索".to_string());
        let merged =
            validate_plan(Some(&plan), &draft("找邮件", vec![model_step])).expect("应当通过");

        assert_eq!(merged.steps[0].evidence.as_deref(), Some("准备开始检索"));
    }
}
