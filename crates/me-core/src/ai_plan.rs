//! 任务清单（plan）：模型把一次需求拆成步骤，引擎逐步推进。
//!
//! 为什么是**独立模块**而不是塞进 `ai.rs`：这是一套自洽的状态机（步骤状态只能前进、
//! 标题创建后不可变），校验逻辑是纯函数，可以脱离运行时单测。`ai.rs` 已经很长了。
//!
//! 落库形态就是这里的 [`Plan`]（JSON，写在 `ai_messages.plan`），
//! 模型的入参是它的**子集**：`id` / `created_at` / `updated_at` 由引擎填。

use serde::{Deserialize, Serialize};

use crate::CoreError;
use crate::ai::truncate_chars;

/// 一条清单最多几步。超过这个数就不是「清单」而是「项目计划」了。
pub const MAX_PLAN_STEPS: usize = 6;
pub const MAX_PLAN_GOAL_CHARS: usize = 120;
pub const MAX_STEP_TITLE_CHARS: usize = 40;
pub const MAX_STEP_EVIDENCE_CHARS: usize = 120;

/// 步骤归属的能力模块。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Mail,
    Weather,
    Calendar,
    Music,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::Mail => "mail",
            Capability::Weather => "weather",
            Capability::Calendar => "calendar",
            Capability::Music => "music",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "mail" => Some(Capability::Mail),
            "weather" => Some(Capability::Weather),
            "calendar" => Some(Capability::Calendar),
            "music" => Some(Capability::Music),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Capability::Mail => "邮件",
            Capability::Weather => "天气",
            Capability::Calendar => "日历",
            Capability::Music => "音乐",
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    /// 步骤序号，从 1 开始；创建后不可变（模型靠它回填状态）。
    pub id: u8,
    pub capability: Capability,
    pub title: String,
    pub status: PlanStatus,
    /// 完成时的一句回执（拿到了什么、命中几条）。**不是**工具返回值原文。
    #[serde(default)]
    pub evidence: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// `p_<created_at>`：只在日志与排障里用，不是数据库主键（清单是消息的一部分）。
    #[serde(default)]
    pub id: String,
    pub goal: String,
    pub steps: Vec<PlanStep>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Plan {
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

    /// `(已结束, 总数)`，给「已完成 4 步 · 1 步未完成」这类摘要用。
    pub fn progress(&self) -> (usize, usize) {
        let done = self.steps.iter().filter(|s| s.status.is_terminal()).count();
        (done, self.steps.len())
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

        let mut out = PlanStep {
            id: step.id,
            capability: step.capability,
            title: title.clone(),
            status: step.status,
            evidence,
        };

        if let Some(old) = prev.and_then(|plan| plan.steps.iter().find(|s| s.id == step.id)) {
            if old.title != out.title {
                // 允许改标题会带来一个隐蔽的坏结果：模型边做边改目标，
                // 事后看清单完全看不出它其实换了要求。
                return Err(CoreError::validation(format!(
                    "第 {} 步的标题不能修改（原为「{}」）",
                    step.id, old.title
                )));
            }
            if old.capability != out.capability {
                return Err(CoreError::validation(format!(
                    "第 {} 步的所属能力不能修改（原为 {}）",
                    step.id,
                    old.capability.label()
                )));
            }
            if old.status.is_terminal() && out.status != old.status {
                return Err(CoreError::validation(format!(
                    "第 {} 步已经是「{}」，不能再改成「{}」",
                    step.id,
                    old.status.label(),
                    out.status.label()
                )));
            }
            if out.status.rank() < old.status.rank() {
                return Err(CoreError::validation(format!(
                    "第 {} 步的状态不能回退（{} → {}）",
                    step.id,
                    old.status.label(),
                    out.status.label()
                )));
            }
            if out.evidence.is_none() {
                out.evidence = old.evidence.clone();
            }
        }

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
        id,
        goal,
        steps: merged,
        created_at: prev.map(|plan| plan.created_at).unwrap_or(next.created_at),
        updated_at: next
            .updated_at
            .max(prev.map(|plan| plan.updated_at).unwrap_or(0)),
    })
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

    fn step(id: u8, title: &str, status: PlanStatus) -> PlanStep {
        PlanStep {
            id,
            capability: Capability::Mail,
            title: title.to_string(),
            status,
            evidence: None,
        }
    }

    fn draft(goal: &str, steps: Vec<PlanStep>) -> Plan {
        Plan {
            id: String::new(),
            goal: goal.to_string(),
            steps,
            created_at: 1_760_000_000,
            updated_at: 1_760_000_000,
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

        let six: Vec<PlanStep> = (1..=6)
            .map(|id| step(id, &format!("第 {id} 步"), PlanStatus::Pending))
            .collect();
        assert!(
            validate_plan(None, &draft("目标", six)).is_ok(),
            "6 步应通过"
        );
    }

    #[test]
    fn seven_steps_are_rejected() {
        // 编号上限就是 6，第 7 步在 id 校验处就会被拦下。
        let mut steps: Vec<PlanStep> = (1..=6)
            .map(|id| step(id, &format!("第 {id} 步"), PlanStatus::Pending))
            .collect();
        steps.push(step(7, "第 7 步", PlanStatus::Pending));
        let err = validate_plan(None, &draft("目标", steps)).expect_err("7 步要拒绝");
        assert!(err.user_message().contains("1–6"), "{}", err.user_message());
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
}
