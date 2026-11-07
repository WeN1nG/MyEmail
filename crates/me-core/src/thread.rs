//! 会话线程分组的**纯函数**（v0.4 的 C5）。
//!
//! 这里只做「把一页邮件摘要按线程折叠成行」这件事：不读库、不碰 egui、
//! 不关心展开状态存在哪。这样分组规则可以毫秒级单测，UI 只负责画。
//!
//! ## 根怎么定
//!
//! 一条邮件的**线程根**按优先级取：
//!
//! 1. `References` 的第一个 id（RFC 5322 里就是根）；
//! 2. `In-Reply-To`（没有 References 时，父 id 就是这一段的根）；
//! 3. 邮件自身的 `Message-ID`（新线程的起点）。
//!
//! 这样父邮件的键（自身 id）与子邮件的键（References 的第一个）天然相等，
//! 即使**根不在当前页**也能把父子凑到一起——分页只加载了一部分邮件时，
//! 这是唯一可行的做法。
//!
//! ## 跨账户不合并
//!
//! 键里带 `account`：同一封邮件的多个副本（自己发给自己、Bcc）、
//! 或不同账户里恰好相同的 Message-ID 都不该被并成一串（D44 / O31）。

use std::collections::{HashMap, HashSet};

use crate::model::MessageSummary;

/// 线程分组行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadRow {
    /// 指回输入的 `messages` 下标。
    pub index: usize,
    /// `0` = 线程首行（或单封邮件），`1` = 展开后的后续邮件。
    pub depth: u8,
    /// 该线程的邮件数（单封为 1）。
    pub group_len: usize,
}

/// 线程根 id。`None` = 这封邮件既没有回复头也没有 `Message-ID`。
pub fn thread_root(message: &MessageSummary) -> Option<&str> {
    message
        .references
        .first()
        .map(String::as_str)
        .or(message.in_reply_to.as_deref())
        .or(message.message_id.as_deref())
}

/// 线程分组键：账户 + 根。
///
/// 三者都没有时退化成「本封邮件自己的本地 id」——保证它单独成串，
/// 而不是和所有同样缺 id 的邮件挤成一条假线程。
pub fn thread_key(message: &MessageSummary) -> String {
    match thread_root(message) {
        Some(root) => format!("{}|{root}", message.account.0),
        None => format!("{}|local:{}", message.account.0, message.id.0),
    }
}

/// 把一页邮件折叠成待渲染的行。
///
/// - `messages` 必须按**时间倒序**（列表的既有顺序）——线程内部要反过来；
/// - 线程位置取「组内最新一封」的位置，符合「有新回复的线程浮上来」；
/// - `expanded` 里放已展开的 `thread_key`，未展开的线程只出一行。
pub fn thread_rows(messages: &[MessageSummary], expanded: &HashSet<String>) -> Vec<ThreadRow> {
    // 归组，同时记住首次出现顺序（即线程的最新活动时间倒序）。
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        let key = thread_key(message);
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(index);
    }

    let mut rows: Vec<ThreadRow> = Vec::with_capacity(messages.len());
    for key in order {
        let Some(indexes) = groups.get(&key) else {
            continue;
        };
        let group_len = indexes.len();
        if group_len == 1 {
            rows.push(ThreadRow {
                index: indexes[0],
                depth: 0,
                group_len: 1,
            });
            continue;
        }
        if !expanded.contains(&key) {
            // 折叠：只出最新一封（输入是倒序，第一条就是最新的）。
            rows.push(ThreadRow {
                index: indexes[0],
                depth: 0,
                group_len,
            });
            continue;
        }
        // 展开：按时间正序（对话顺序）铺开。
        for (position, index) in indexes.iter().rev().enumerate() {
            rows.push(ThreadRow {
                index: *index,
                depth: if position == 0 { 0 } else { 1 },
                group_len,
            });
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AccountId, Address, DateSource, Flags, FolderId, MessageId};

    fn message(
        id: i64,
        account: i64,
        message_id: Option<&str>,
        in_reply_to: Option<&str>,
        references: &[&str],
        date: i64,
    ) -> MessageSummary {
        MessageSummary {
            id: MessageId(id),
            account: AccountId(account),
            folder: FolderId(1),
            uid: Some(id as u32),
            message_id: message_id.map(str::to_string),
            subject: Some(format!("邮件 {id}")),
            from: Some(Address {
                name: None,
                addr: "a@example.com".into(),
            }),
            to: Vec::new(),
            cc: Vec::new(),
            date: Some(date),
            date_source: DateSource::Header,
            size: None,
            flags: Flags::NONE,
            snippet: None,
            has_attachment: false,
            body_cached: false,
            in_reply_to: in_reply_to.map(str::to_string),
            references: references.iter().map(|r| r.to_string()).collect(),
        }
    }

    #[test]
    fn standalone_message_is_its_own_thread() {
        let messages = vec![message(1, 1, Some("a@x"), None, &[], 100)];
        let rows = thread_rows(&messages, &HashSet::new());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].group_len, 1);
        assert_eq!(rows[0].depth, 0);
    }

    #[test]
    fn child_reply_collapses_with_parent() {
        // 输入按时间倒序：子（200）在前，父（100）在后。
        let messages = vec![
            message(2, 1, Some("b@x"), Some("a@x"), &["a@x"], 200),
            message(1, 1, Some("a@x"), None, &[], 100),
        ];
        let rows = thread_rows(&messages, &HashSet::new());
        assert_eq!(rows.len(), 1, "未展开时一个线程只出一行");
        assert_eq!(rows[0].group_len, 2);
        assert_eq!(rows[0].index, 0, "折叠行用最新一封");
    }

    #[test]
    fn expanded_thread_lists_oldest_first() {
        let messages = vec![
            message(2, 1, Some("b@x"), Some("a@x"), &["a@x"], 200),
            message(1, 1, Some("a@x"), None, &[], 100),
        ];
        let key = thread_key(&messages[0]);
        let mut expanded = HashSet::new();
        expanded.insert(key);
        let rows = thread_rows(&messages, &expanded);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].index, 1, "展开后按时间正序，父在前");
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[1].index, 0);
        assert_eq!(rows[1].depth, 1);
    }

    #[test]
    fn deep_chain_shares_the_root() {
        // 根 → 子 → 孙：三者 References 的第一个都是根，所以同串。
        let messages = vec![
            message(3, 1, Some("c@x"), Some("b@x"), &["a@x", "b@x"], 300),
            message(2, 1, Some("b@x"), Some("a@x"), &["a@x"], 200),
            message(1, 1, Some("a@x"), None, &[], 100),
        ];
        let rows = thread_rows(&messages, &HashSet::new());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].group_len, 3);
    }

    #[test]
    fn root_missing_from_page_still_groups() {
        // 分页只加载了后半段：根不在页内，父子仍靠同一个根 id 聚在一起。
        let messages = vec![
            message(3, 1, Some("c@x"), Some("b@x"), &["a@x", "b@x"], 300),
            message(2, 1, Some("b@x"), Some("a@x"), &["a@x"], 200),
        ];
        let rows = thread_rows(&messages, &HashSet::new());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].group_len, 2);
    }

    #[test]
    fn different_accounts_never_merge() {
        let messages = vec![
            message(2, 2, Some("b@x"), Some("a@x"), &["a@x"], 200),
            message(1, 1, Some("a@x"), None, &[], 100),
        ];
        let rows = thread_rows(&messages, &HashSet::new());
        assert_eq!(rows.len(), 2, "跨账户不合并（O31）");
    }

    #[test]
    fn messages_without_ids_do_not_form_a_fake_thread() {
        let messages = vec![
            message(2, 1, None, None, &[], 200),
            message(1, 1, None, None, &[], 100),
        ];
        let rows = thread_rows(&messages, &HashSet::new());
        assert_eq!(rows.len(), 2, "都缺 id 时不能挤成一串");
    }

    #[test]
    fn groups_keep_the_position_of_the_newest_member() {
        // 单封 A（最新）、线程 B（中间 + 最旧）。
        let messages = vec![
            message(3, 1, Some("x@x"), None, &[], 300),
            message(2, 1, Some("b2@x"), Some("b1@x"), &["b1@x"], 200),
            message(1, 1, Some("b1@x"), None, &[], 100),
        ];
        let rows = thread_rows(&messages, &HashSet::new());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].index, 0, "最新那封单封在最前");
        assert_eq!(rows[1].group_len, 2);
        assert_eq!(rows[1].index, 1, "线程用组内最新的一封代表");
    }
}
