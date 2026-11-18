//! 系统通知的**发送与节流**。
//!
//! 为什么在这里而不在 `me-sync`：只有界面知道「窗口是不是在前台」，也只有界面
//! 能拿到用户刚在设置页改过的开关。`me-sync` 只负责把开关落进 `meta`。
//!
//! 两条硬约束：
//!
//! 1. **必须冷却**。`notify-rust` 在 Windows 上每条都进通知中心，同步 10 个
//!    文件夹就会弹 10 条；同一个类别在一个窗口内只发一条。
//! 2. **不在主线程等平台调用**。`show()` 是同步的 WinRT 调用，放到独立线程里发，
//!    失败了也只记日志——通知发不出去不该影响任何业务路径。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use me_core::notify::{NotificationKind, NotificationPrefs};

/// 同一类别的冷却窗口。`None` = 不冷却（每次一条）。
///
/// 「AI 回复」不冷却：用户就是想知道「这次问完了」，压掉反而让人以为卡住。
/// 新邮件 5 分钟、同步失败 30 分钟——这两个是「一段时间内只值得说一次」的事。
fn cooldown(kind: NotificationKind) -> Option<Duration> {
    match kind {
        NotificationKind::NewMail => Some(Duration::from_secs(5 * 60)),
        NotificationKind::SyncFailed => Some(Duration::from_secs(30 * 60)),
        NotificationKind::AiReply
        | NotificationKind::KnowledgeIndexed
        | NotificationKind::EmbedModelReady => None,
    }
}

/// 每个类别上一次**真正发出**的时刻。
///
/// 用 `Vec<(Kind, Instant)>` 而不是 `HashMap`：最多 5 条，线性查找比哈希快，
/// 还能 `const` 初始化（`Mutex::new(Vec::new())`），不需要 `OnceLock` 包一层。
static LAST: Mutex<Vec<(NotificationKind, Instant)>> = Mutex::new(Vec::new());

/// 纯判定：这会儿该不该发。**不发真实通知**，便于单测。
///
/// 三件事集中在一处：前台不发、类别关掉不发、冷却窗口内不发。
pub fn should_notify(
    prefs: &NotificationPrefs,
    kind: NotificationKind,
    foreground: bool,
    last: Option<Instant>,
    now: Instant,
) -> bool {
    suppression(prefs, kind, foreground, last, now).is_none()
}

/// `should_notify` 的「为什么没发」版本：返回 `None` = 该发。
///
/// 分开是因为**跳过是静默的**——不记原因的话，「通知没弹」只能靠猜。
/// 文案会进日志，所以写成能直接读懂的一句。
fn suppression(
    prefs: &NotificationPrefs,
    kind: NotificationKind,
    foreground: bool,
    last: Option<Instant>,
    now: Instant,
) -> Option<&'static str> {
    // 应用在前台时有应用内 toast，再弹系统通知就是重复打扰。
    if foreground {
        return Some("窗口在前台");
    }
    if !prefs.is_on(kind) {
        return Some("该类别已被关掉");
    }
    match cooldown(kind) {
        None => None,
        Some(window) => match last {
            Some(at) if now.saturating_duration_since(at) < window => Some("冷却窗口内"),
            _ => None,
        },
    }
}

/// 尝试发一条系统通知，返回是否真的发出去了（调用方据此决定要不要回显）。
///
/// 冷却时间**在真正发送之前**就记下：发送失败也不该立刻重试，
/// 否则一次断网会连着弹一串失败通知。
pub fn dispatch(
    prefs: &NotificationPrefs,
    kind: NotificationKind,
    foreground: bool,
    title: &str,
    body: &str,
) -> bool {
    let now = Instant::now();
    let previous = {
        let last = lock();
        last.iter()
            .find(|(existing, _)| *existing == kind)
            .map(|(_, at)| *at)
    };
    if !should_notify(prefs, kind, foreground, previous, now) {
        // 再问一次「为什么」只为日志：判定与文案分两个函数，避免
        // 为了日志把 `should_notify` 的签名改成返回字符串。
        let reason = suppression(prefs, kind, foreground, previous, now).unwrap_or("未知");
        tracing::info!(kind = kind.as_str(), reason, "系统通知：跳过");
        return false;
    }
    {
        let mut last = lock();
        match last.iter_mut().find(|(existing, _)| *existing == kind) {
            Some(entry) => entry.1 = now,
            None => last.push((kind, now)),
        }
    }

    tracing::info!(kind = kind.as_str(), %title, %body, "系统通知：发出");
    let (title, body) = (title.to_string(), body.to_string());
    show(title, body);
    true
}

/// 真正弹窗，**放在独立线程**：WinRT 的 `show()` 是同步的，不该让主线程等。
///
/// 测试里换成空实现——单测不该在开发机上弹真通知（`cargo test` 会跑到
/// `dispatch` 的节流用例）。
#[cfg(not(test))]
fn show(title: String, body: String) {
    std::thread::spawn(move || {
        if let Err(err) = notify_rust::Notification::new()
            .summary(&title)
            .body(&body)
            .show()
        {
            // **warn 而不是 debug**：平台调用失败是「用户以为功能没做」的
            // 第一嫌疑，必须能在默认级别日志里看到。
            tracing::warn!(error = %err, "系统通知发送失败");
        }
    });
}

#[cfg(test)]
fn show(title: String, body: String) {
    // 只保留参数所有权，不发任何东西。
    drop((title, body));
}

/// 中毒的锁不该让整个应用崩：这个表只是节流用的，丢了最多多弹一条。
fn lock() -> std::sync::MutexGuard<'static, Vec<(NotificationKind, Instant)>> {
    LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefs_all_on() -> NotificationPrefs {
        NotificationPrefs::default()
    }

    #[test]
    fn foreground_never_notifies() {
        assert!(!should_notify(
            &prefs_all_on(),
            NotificationKind::NewMail,
            true,
            None,
            Instant::now()
        ));
    }

    #[test]
    fn disabled_kind_never_notifies() {
        let mut prefs = prefs_all_on();
        prefs.set(NotificationKind::NewMail, false);
        assert!(!should_notify(
            &prefs,
            NotificationKind::NewMail,
            false,
            None,
            Instant::now()
        ));
        // 别的类别不受影响。
        assert!(should_notify(
            &prefs,
            NotificationKind::SyncFailed,
            false,
            None,
            Instant::now()
        ));
    }

    #[test]
    fn new_mail_is_throttled_for_five_minutes() {
        let now = Instant::now();
        let just_sent = now.checked_sub(Duration::from_secs(60)).unwrap();
        assert!(!should_notify(
            &prefs_all_on(),
            NotificationKind::NewMail,
            false,
            Some(just_sent),
            now
        ));
        let stale = now.checked_sub(Duration::from_secs(6 * 60)).unwrap();
        assert!(should_notify(
            &prefs_all_on(),
            NotificationKind::NewMail,
            false,
            Some(stale),
            now
        ));
    }

    #[test]
    fn ai_reply_is_never_throttled() {
        let now = Instant::now();
        let just_sent = now.checked_sub(Duration::from_millis(1)).unwrap();
        assert!(should_notify(
            &prefs_all_on(),
            NotificationKind::AiReply,
            false,
            Some(just_sent),
            now
        ));
    }

    #[test]
    fn dispatch_records_and_throttles() {
        // 用 `SyncFailed`（30 分钟窗口）避免和其他测试抢同一类别的全局状态。
        let ok = dispatch(
            &prefs_all_on(),
            NotificationKind::SyncFailed,
            false,
            "测试标题",
            "测试正文",
        );
        assert!(ok, "首次应发出");
        let again = dispatch(
            &prefs_all_on(),
            NotificationKind::SyncFailed,
            false,
            "测试标题",
            "测试正文",
        );
        assert!(!again, "冷却窗口内不应重复发出");
    }
}
