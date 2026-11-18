//! 系统通知：**类别与偏好**。
//!
//! 真正的平台调用在 `me-app/src/notify.rs`——领域层不碰 `notify-rust`，
//! 也不该知道「窗口是不是在前台」。这里只放三样东西：
//!
//! 1. `NotificationKind`：有哪几类通知（也是 `meta` 键与设置项的来源）
//! 2. `NotificationPrefs`：每类一个开关，**默认全开**
//! 3. 类别与 `meta` 键、界面文案的映射（只此一份，别在设置页手抄一遍）

use serde::{Deserialize, Serialize};

/// 通知类别。
///
/// 新增类别时三处要一起改：`as_str` / `meta_key` / `label`，
/// 以及 `ALL`（设置页按它排顺序）。漏了 `meta_key` 会编译不过，
/// 但漏了 `ALL` 只是设置页少一个开关——所以 `ALL` 直接列全。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NotificationKind {
    /// 同步拉到新的未读邮件（收件箱）。
    NewMail,
    /// 连续多次同步失败。
    SyncFailed,
    /// AI 回复完成。
    AiReply,
    /// 知识库单份文档索引完成 / 队列清空（批 4 接上）。
    KnowledgeIndexed,
    /// 外置嵌入模型下载完成（批 7 接上）。
    EmbedModelReady,
}

impl NotificationKind {
    /// 设置页的展示顺序。**前三个已接线**，后两个等对应批次接上；
    /// 先列进枚举是为了让 `meta` 键与序列化格式早定，免得将来改格式。
    pub const ALL: [NotificationKind; 5] = [
        NotificationKind::NewMail,
        NotificationKind::SyncFailed,
        NotificationKind::AiReply,
        NotificationKind::KnowledgeIndexed,
        NotificationKind::EmbedModelReady,
    ];

    /// **已经有触发点**、可以出现在设置页的类别。
    ///
    /// 与 `ALL` 分开是为了不骗人：知识库两条要等批 4 / 批 7 接上，
    /// 现在就摆一个勾选框，勾了也不会发生任何事。那边接管线时把它们加进来。
    pub const WIRED: [NotificationKind; 3] = [
        NotificationKind::NewMail,
        NotificationKind::SyncFailed,
        NotificationKind::AiReply,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            NotificationKind::NewMail => "new_mail",
            NotificationKind::SyncFailed => "sync_failed",
            NotificationKind::AiReply => "ai_reply",
            NotificationKind::KnowledgeIndexed => "knowledge_indexed",
            NotificationKind::EmbedModelReady => "embed_model_ready",
        }
    }

    /// `meta` 里的键。**这个词面是持久化格式**，改了等于把用户已有的
    /// 开关全丢掉（读不到就按默认全开），别为了好看去动它。
    pub fn meta_key(self) -> String {
        format!("notify_{}", self.as_str())
    }

    pub fn label(self) -> &'static str {
        match self {
            NotificationKind::NewMail => "新邮件",
            NotificationKind::SyncFailed => "同步失败",
            NotificationKind::AiReply => "AI 回复完成",
            NotificationKind::KnowledgeIndexed => "知识库索引完成",
            NotificationKind::EmbedModelReady => "嵌入模型下载完成",
        }
    }
}

/// 各类别的开关。**存「关掉的」而不是「打开的」**：
///
/// - 默认全开 = 空表，`Default` 天然正确，新加的类别自动是开着的
/// - 老用户升级后不会因为多了一个类别就突然收不到通知
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NotificationPrefs {
    off: Vec<NotificationKind>,
}

impl NotificationPrefs {
    pub fn is_on(&self, kind: NotificationKind) -> bool {
        !self.off.contains(&kind)
    }

    pub fn set(&mut self, kind: NotificationKind, on: bool) {
        self.off.retain(|existing| *existing != kind);
        if !on {
            self.off.push(kind);
        }
    }

    /// 从「键 → 值」构造。值缺失按**开**处理（默认全开），
    /// 只有明确写了 `0` / `false` 才算关。
    pub fn from_meta(values: &[(NotificationKind, Option<String>)]) -> Self {
        let mut prefs = Self::default();
        for (kind, value) in values {
            let on = !matches!(
                value.as_deref().map(str::trim),
                Some("0") | Some("false") | Some("off")
            );
            prefs.set(*kind, on);
        }
        prefs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_everything_on() {
        let prefs = NotificationPrefs::default();
        for kind in NotificationKind::ALL {
            assert!(prefs.is_on(kind), "{kind:?} 默认应为开");
        }
    }

    #[test]
    fn set_off_then_on_round_trips() {
        let mut prefs = NotificationPrefs::default();
        prefs.set(NotificationKind::NewMail, false);
        assert!(!prefs.is_on(NotificationKind::NewMail));
        assert!(prefs.is_on(NotificationKind::SyncFailed));
        prefs.set(NotificationKind::NewMail, true);
        assert!(prefs.is_on(NotificationKind::NewMail));
        // 重复关同一个类别不能把表撑出多份。
        prefs.set(NotificationKind::NewMail, false);
        prefs.set(NotificationKind::NewMail, false);
        assert_eq!(prefs.off.len(), 1);
    }

    #[test]
    fn meta_values_only_turn_things_off_explicitly() {
        let prefs = NotificationPrefs::from_meta(&[
            (NotificationKind::NewMail, Some("0".to_string())),
            (NotificationKind::SyncFailed, Some("true".to_string())),
            (NotificationKind::AiReply, None),
        ]);
        assert!(!prefs.is_on(NotificationKind::NewMail));
        assert!(prefs.is_on(NotificationKind::SyncFailed));
        assert!(prefs.is_on(NotificationKind::AiReply), "缺失按默认开");
    }

    #[test]
    fn meta_key_is_stable() {
        // 持久化格式：改这里等于丢掉用户已有的开关。
        assert_eq!(NotificationKind::NewMail.meta_key(), "notify_new_mail");
        assert_eq!(
            NotificationKind::EmbedModelReady.meta_key(),
            "notify_embed_model_ready"
        );
    }

    #[test]
    fn wired_kinds_are_a_subset_of_all() {
        // 设置页只画 `WIRED`：漏进 `ALL` 之外的类别会静默不显示，
        // 所以这里钉住「每个已接线的类别都真的在元列表里」。
        for kind in NotificationKind::WIRED {
            assert!(
                NotificationKind::ALL.contains(&kind),
                "{kind:?} 不在 ALL 里"
            );
            assert!(!kind.label().is_empty());
        }
    }
}
