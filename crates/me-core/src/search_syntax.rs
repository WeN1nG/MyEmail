//! 搜索速记语法解析器（纯函数）。
//!
//! 放在 `me-core` 的理由有三个：
//!
//! 1. **无 IO、易测**——解析是纯字符串处理，不读数据库、不发网络请求；
//! 2. **AI 侧可复用**——AI 的 `email_search` 工具可以把模型给出的
//!    `from:zhang has:attachment` 走同一套解析（见 [v04候选功能.md](v04候选功能.md) D43）；
//! 3. **领域层是唯一汇点**——`me-app` 与 `me-sync` 都能用，不需要任何一方依赖另一方。
//!
//! # 支持的语法
//!
//! | 写法 | 含义 |
//! | --- | --- |
//! | `from:zhang` | 发件人包含 zhang |
//! | `to:alice` | 收件人包含 alice |
//! | `subject:报告` | 主题包含「报告」 |
//! | `has:attachment` | 仅含附件 |
//! | `has:none` | 不含附件 |
//! | `before:2026-11-01` | 早于该日期（含边界，按当天 00:00 之前） |
//! | `after:2026-10-01` | 晚于该日期（含边界，按当天 23:59:59 之后） |
//! | `is:unread` | 仅未读 |
//! | `is:flagged` | 仅星标 |
//! | `is:important` | 仅「重要」（星标，或近 7 天未读） |
//!
//! **不支持**：`OR`、括号、引号（O29 已拍板 v0.4 不做）。
//!
//! # 设计取舍
//!
//! **未知片段进 `unknown` 而不是报错**：搜索是探索性的——用户打错 `fromm:zhang`
//! 应该得到「搜索照常执行 + 一条提示」，而不是弹一个错误把输入清掉。
//!
//! **纯函数不读时钟**：`before:` 的上界要相对「今天」推算，但时钟由调用方传入
//! （`today` 参数），这样解析结果是确定的、可单测的。

use chrono::{DateTime, NaiveDate};

/// 解析结果。未识别的片段进 `text`，认识的进对应字段。
///
/// `text` 是**剩余的自由文本**（供 FTS 全文检索），
/// 不是原始输入——速记语法片段已被摘走。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedQuery {
    /// 剩余的自由文本（供 FTS 全文检索）。
    pub text: String,
    /// 发件人包含。
    pub from: Option<String>,
    /// 收件人包含。
    pub to: Option<String>,
    /// 主题包含。
    pub subject: Option<String>,
    /// 附件过滤。
    pub has_attachment: Option<bool>,
    /// 起始时间（含），unix 秒。
    pub since: Option<i64>,
    /// 结束时间（含），unix 秒。
    pub until: Option<i64>,
    /// 仅未读。
    pub unread_only: bool,
    /// 仅星标。
    pub flagged_only: bool,
    /// 仅「重要」：星标，或近 7 天未读。
    ///
    /// 这是 AI 那套「什么算重要」判据的用户侧版本：
    /// 用户手动搜 `is:important` 得到的，与 AI 回答「哪些邮件重要」是同一套口径。
    pub important_only: bool,
    /// 无法识别的片段（拼错的字段名、无法解析的日期等），UI 用来提示但不阻断搜索。
    pub unknown: Vec<String>,
}

impl ParsedQuery {
    /// 是否存在任何条件（含自由文本）。
    ///
    /// **这是「空查询」的唯一判据**——只看 `text` 会在速记语法下出错：
    /// `from:zhang` 的 `text` 为空，但显然不是空查询（见 §3.6 的坑）。
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
            && self.from.is_none()
            && self.to.is_none()
            && self.subject.is_none()
            && self.has_attachment.is_none()
            && self.since.is_none()
            && self.until.is_none()
            && !self.unread_only
            && !self.flagged_only
            && !self.important_only
    }

    /// 自由文本部分的字符数（用于「1～2 字仅匹配头部」的判定）。
    ///
    /// **只看正文部分，不看原始输入**——`from:ab` 的正文部分是空的，
    /// 它走的是 `from` 的 `LIKE`，与正文全文检索无关（见 §3.6 的坑）。
    pub fn text_len(&self) -> usize {
        self.text.trim().chars().count()
    }

    /// 条件数量（用于 UI 显示「N 个条件」）。
    pub fn condition_count(&self) -> usize {
        let mut n = 0;
        if self.from.is_some() {
            n += 1;
        }
        if self.to.is_some() {
            n += 1;
        }
        if self.subject.is_some() {
            n += 1;
        }
        if self.has_attachment.is_some() {
            n += 1;
        }
        if self.since.is_some() {
            n += 1;
        }
        if self.until.is_some() {
            n += 1;
        }
        if self.unread_only {
            n += 1;
        }
        if self.flagged_only {
            n += 1;
        }
        if self.important_only {
            n += 1;
        }
        n
    }
}

/// 解析搜索框输入。
///
/// **纯函数**：不 IO、不读时钟（`today` 由调用方传入，unix 秒）。
///
/// `today` 用于把 `before:` / `after:` 的日期边界算成 unix 秒：
/// `after:2026-10-01` 表示「晚于 2026-10-01 当天结束」，即从 10-02 00:00:00 起；
/// `before:2026-11-01` 表示「早于 2026-11-01 当天开始」，即到 10-31 23:59:59 止。
/// `today` 只在解析**相对日期**（如 `before:today`）时作为参照，
/// 绝对日期（`before:2026-11-01`）不依赖它。
///
/// 解析规则：
///
/// - 片段之间用空白分隔；`key:value` 形态才算速记语法；
/// - 值可以带引号（`subject:"季度 报告"`）——虽然 O29 说 v0.4 不做引号语法，
///   但**剥离**引号是零成本的，且避免 `"` 混进 LIKE 条件；
/// - 同一条件重复出现时，**最后一个生效**（后写覆盖先写，与直觉一致）；
/// - 无法识别的 key、无效日期 → 进 `unknown`，其余片段照常解析。
pub fn parse_search_syntax(input: &str, today: i64) -> ParsedQuery {
    let mut out = ParsedQuery::default();
    let mut rest: Vec<&str> = Vec::new();

    for token in split_tokens(input) {
        let Some((key, value)) = token.split_once(':') else {
            rest.push(token);
            continue;
        };
        // key 必须是纯标识符形态：不含空白、不含引号。
        // 否则像 `http://example.com` 这样的片段会被误判成条件。
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            rest.push(token);
            continue;
        }
        // 值以 `//` 开头 → 这是 URL（`http://…`），不是条件。
        // key 单独看是合法标识符（`http`），只能靠值来区分。
        if value.trim_start().starts_with("//") {
            rest.push(token);
            continue;
        }
        // 值可能带引号（`subject:"季度 报告"`）——剥掉引号再存，
        // 避免 `"` 混进 LIKE 条件。
        let value = strip_quotes(value.trim());
        if value.is_empty() {
            // 值为空 → 不构成条件：既不进字段，也不该报「未知」。
            // `from:` 就是「没有 from 条件」，用户可能只是打了一半。
            continue;
        }
        match key.to_ascii_lowercase().as_str() {
            "from" => out.from = Some(value.to_string()),
            "to" => out.to = Some(value.to_string()),
            "subject" => out.subject = Some(value.to_string()),
            "has" => match value.to_ascii_lowercase().as_str() {
                "attachment" | "attachments" => out.has_attachment = Some(true),
                "none" | "no" => out.has_attachment = Some(false),
                _ => out.unknown.push(format!("has:{value}")),
            },
            "is" => match value.to_ascii_lowercase().as_str() {
                "unread" => out.unread_only = true,
                "read" => out.unread_only = false,
                "flagged" | "starred" => out.flagged_only = true,
                "important" => out.important_only = true,
                _ => out.unknown.push(format!("is:{value}")),
            },
            "before" | "after" => {
                let Some(ts) = parse_date_bound(value, today, key.eq_ignore_ascii_case("after"))
                else {
                    out.unknown.push(format!("{key}:{value}"));
                    continue;
                };
                if key.eq_ignore_ascii_case("after") {
                    out.since = Some(ts);
                } else {
                    out.until = Some(ts);
                }
            }
            _ => out.unknown.push(token.to_string()),
        }
    }

    out.text = rest.join(" ");
    out
}

/// 拆分输入为片段：空白分隔，但**引号内**的空白不算分隔符。
///
/// 返回的片段已剥离引号。
fn split_tokens(input: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = input.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut in_quotes = false;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'"' {
            in_quotes = !in_quotes;
        } else if c.is_ascii_whitespace() && !in_quotes {
            if i > start {
                out.push(strip_quotes(&input[start..i]));
            }
            start = i + 1;
        }
        i += 1;
    }
    if start < input.len() {
        out.push(strip_quotes(&input[start..]));
    }
    out
}

/// 剥掉片段首尾的一对引号。
fn strip_quotes(token: &str) -> &str {
    let t = token.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

/// 把日期片段算成 unix 秒边界。
///
/// `after:2026-10-01` → 该日**结束**时刻（23:59:59），即「晚于 10-01 当天」；
/// `before:2026-11-01` → 该日**开始**时刻（00:00:00），即「早于 11-01 当天」。
///
/// 支持 `today` / `yesterday` 两个相对写法；绝对日期用 `YYYY-MM-DD`。
fn parse_date_bound(value: &str, today: i64, is_after: bool) -> Option<i64> {
    let lower = value.to_ascii_lowercase();
    let date = match lower.as_str() {
        "today" => today_date(today)?,
        "yesterday" => today_date(today)?.pred_opt()?,
        _ => NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?,
    };
    let time = if is_after {
        date.and_hms_opt(23, 59, 59)?
    } else {
        date.and_hms_opt(0, 0, 0)?
    };
    Some(time.and_utc().timestamp())
}

/// 从 unix 秒取「今天」的日期（UTC）。
fn today_date(today: i64) -> Option<NaiveDate> {
    DateTime::from_timestamp(today, 0).map(|dt| dt.date_naive())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 固定参照时刻：2026-11-06 12:00:00 UTC。
    const TODAY: i64 = 1_793_000_000;

    fn ts(y: i32, m: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, mi, s)
            .unwrap()
            .and_utc()
            .timestamp()
    }

    #[test]
    fn plain_text_is_kept_as_is() {
        let p = parse_search_syntax("季度报告", TODAY);
        assert_eq!(p.text, "季度报告");
        assert!(p.from.is_none());
        assert!(p.to.is_none());
        assert!(p.subject.is_none());
        assert!(p.unknown.is_empty());
    }

    #[test]
    fn empty_input_is_empty_query() {
        let p = parse_search_syntax("", TODAY);
        assert!(p.is_empty());
        assert_eq!(p.text, "");
    }

    #[test]
    fn whitespace_only_input_is_empty_query() {
        let p = parse_search_syntax("   \t  ", TODAY);
        assert!(p.is_empty());
    }

    #[test]
    fn extracts_from_clause() {
        let p = parse_search_syntax("from:zhang", TODAY);
        assert_eq!(p.from.as_deref(), Some("zhang"));
        assert_eq!(p.text, "");
        // 关键：速记语法下 text 为空，但**不是空查询**。
        assert!(!p.is_empty());
    }

    #[test]
    fn extracts_to_clause() {
        let p = parse_search_syntax("to:alice", TODAY);
        assert_eq!(p.to.as_deref(), Some("alice"));
        assert!(!p.is_empty());
    }

    #[test]
    fn extracts_subject_clause() {
        let p = parse_search_syntax("subject:预算", TODAY);
        assert_eq!(p.subject.as_deref(), Some("预算"));
        assert!(!p.is_empty());
    }

    #[test]
    fn has_attachment_sets_true() {
        let p = parse_search_syntax("has:attachment", TODAY);
        assert_eq!(p.has_attachment, Some(true));
        assert!(!p.is_empty());
    }

    #[test]
    fn has_none_sets_false() {
        let p = parse_search_syntax("has:none", TODAY);
        assert_eq!(p.has_attachment, Some(false));
    }

    #[test]
    fn is_unread_sets_unread_only() {
        let p = parse_search_syntax("is:unread", TODAY);
        assert!(p.unread_only);
        assert!(!p.is_empty());
    }

    #[test]
    fn is_read_clears_unread_only() {
        let p = parse_search_syntax("is:read", TODAY);
        assert!(!p.unread_only);
        assert!(p.is_empty());
    }

    #[test]
    fn is_flagged_sets_flagged_only() {
        let p = parse_search_syntax("is:flagged", TODAY);
        assert!(p.flagged_only);
        assert!(!p.is_empty());
    }

    #[test]
    fn is_starred_is_an_alias_for_flagged() {
        let p = parse_search_syntax("is:starred", TODAY);
        assert!(p.flagged_only);
    }

    #[test]
    fn is_important_sets_important_only() {
        let p = parse_search_syntax("is:important", TODAY);
        assert!(p.important_only);
        assert!(!p.is_empty());
    }

    #[test]
    fn is_unknown_value_goes_to_unknown() {
        let p = parse_search_syntax("is:whatever", TODAY);
        assert!(!p.flagged_only);
        assert!(!p.important_only);
        assert_eq!(p.unknown, vec!["is:whatever".to_string()]);
    }

    #[test]
    fn after_clause_sets_since_to_end_of_day() {
        let p = parse_search_syntax("after:2026-10-01", TODAY);
        assert_eq!(p.since, Some(ts(2026, 10, 1, 23, 59, 59)));
        assert_eq!(p.until, None);
    }

    #[test]
    fn before_clause_sets_until_to_start_of_day() {
        let p = parse_search_syntax("before:2026-11-01", TODAY);
        assert_eq!(p.until, Some(ts(2026, 11, 1, 0, 0, 0)));
        assert_eq!(p.since, None);
    }

    #[test]
    fn relative_dates_use_today_argument() {
        let today = ts(2026, 11, 6, 12, 0, 0);
        let p = parse_search_syntax("after:today", today);
        assert_eq!(p.since, Some(ts(2026, 11, 6, 23, 59, 59)));

        let p2 = parse_search_syntax("before:yesterday", today);
        assert_eq!(p2.until, Some(ts(2026, 11, 5, 0, 0, 0)));
    }

    #[test]
    fn mixed_conditions_all_extracted() {
        let p = parse_search_syntax("from:zhang has:attachment 报告", TODAY);
        assert_eq!(p.from.as_deref(), Some("zhang"));
        assert_eq!(p.has_attachment, Some(true));
        assert_eq!(p.text, "报告");
        assert_eq!(p.condition_count(), 2);
    }

    #[test]
    fn unknown_field_goes_to_unknown_without_blocking() {
        let p = parse_search_syntax("fromm:zhang report", TODAY);
        // 拼错的字段进 unknown，**其余片段照常解析**。
        assert_eq!(p.unknown, vec!["fromm:zhang".to_string()]);
        assert_eq!(p.text, "report");
        assert!(p.from.is_none());
    }

    #[test]
    fn invalid_date_goes_to_unknown() {
        let p = parse_search_syntax("before:not-a-date report", TODAY);
        assert_eq!(p.unknown, vec!["before:not-a-date".to_string()]);
        assert_eq!(p.until, None);
        assert_eq!(p.text, "report");
    }

    #[test]
    fn unknown_has_value_goes_to_unknown() {
        let p = parse_search_syntax("has:maybe", TODAY);
        assert_eq!(p.unknown, vec!["has:maybe".to_string()]);
        assert_eq!(p.has_attachment, None);
    }

    #[test]
    fn url_like_fragment_is_not_treated_as_clause() {
        // `http://example.com` 的 key 含 `/`，不是合法标识符 → 当普通文本。
        let p = parse_search_syntax("http://example.com", TODAY);
        assert!(p.unknown.is_empty());
        assert_eq!(p.text, "http://example.com");
    }

    #[test]
    fn quoted_value_keeps_inner_space() {
        let p = parse_search_syntax("subject:\"季度 报告\"", TODAY);
        assert_eq!(p.subject.as_deref(), Some("季度 报告"));
    }

    #[test]
    fn later_clause_overrides_earlier_one() {
        let p = parse_search_syntax("from:a from:b", TODAY);
        assert_eq!(p.from.as_deref(), Some("b"));
    }

    #[test]
    fn empty_value_is_not_a_condition() {
        let p = parse_search_syntax("from:", TODAY);
        // 值为空 → 不构成条件（不进 unknown，也不进 from）。
        assert!(p.from.is_none());
        assert!(p.unknown.is_empty());
        assert!(p.is_empty());
    }

    #[test]
    fn clause_keys_are_case_insensitive() {
        let p = parse_search_syntax("FROM:zhang HAS:attachment IS:unread", TODAY);
        assert_eq!(p.from.as_deref(), Some("zhang"));
        assert_eq!(p.has_attachment, Some(true));
        assert!(p.unread_only);
    }

    #[test]
    fn text_len_counts_only_body_part() {
        // `from:ab` 的正文部分是空的 → 长度 0，**不该触发**「1～2 字仅匹配头部」。
        let p = parse_search_syntax("from:ab", TODAY);
        assert_eq!(p.text_len(), 0);

        let p2 = parse_search_syntax("ab", TODAY);
        assert_eq!(p2.text_len(), 2);

        let p3 = parse_search_syntax("from:zhang 报告", TODAY);
        assert_eq!(p3.text_len(), 2);
    }

    #[test]
    fn is_empty_covers_every_condition_kind() {
        assert!(!parse_search_syntax("from:a", TODAY).is_empty());
        assert!(!parse_search_syntax("to:a", TODAY).is_empty());
        assert!(!parse_search_syntax("subject:a", TODAY).is_empty());
        assert!(!parse_search_syntax("has:attachment", TODAY).is_empty());
        assert!(!parse_search_syntax("has:none", TODAY).is_empty());
        assert!(!parse_search_syntax("after:2026-01-01", TODAY).is_empty());
        assert!(!parse_search_syntax("before:2026-01-01", TODAY).is_empty());
        assert!(!parse_search_syntax("is:unread", TODAY).is_empty());
        assert!(!parse_search_syntax("text", TODAY).is_empty());
    }

    #[test]
    fn unknown_only_input_is_still_a_search() {
        // 只拼错了字段名 → 文本为空但有 unknown。搜索照常执行（搜全文）。
        let p = parse_search_syntax("fromm:zhang", TODAY);
        assert!(p.is_empty());
        assert_eq!(p.unknown.len(), 1);
    }
}
