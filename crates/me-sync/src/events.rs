//! 事件发送：有界通道 + 进度合并 + 唤醒回调。
//!
//! - 进度事件（`SyncProgress` / `IndexProgress`）在通道满时直接丢弃；由发送侧按
//!   ≥200 ms 或每 50 封节流，避免十万封补全时事件堆积。
//! - 关键事件不丢弃：通道满时等待，形成背压。
//! - 每次发送后调用唤醒回调，保证空闲窗口也能刷新。

use std::sync::Arc;
use std::time::{Duration, Instant};

use me_core::Event;
use tokio::sync::mpsc;

/// 唤醒回调由 `me-app` 注入（内部调用 `ctx.request_repaint()`）；me-sync 不依赖 egui。
pub type WakeFn = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
pub struct EventSender {
    tx: mpsc::Sender<Event>,
    wake: WakeFn,
}

impl EventSender {
    pub fn new(tx: mpsc::Sender<Event>, wake: WakeFn) -> Self {
        Self { tx, wake }
    }

    /// 发送事件。进度事件满则丢弃；关键事件满则等待。
    pub async fn send(&self, event: Event) {
        if event.is_progress() {
            if self.tx.try_send(event).is_err() {
                return;
            }
        } else if let Err(mpsc::error::TrySendError::Full(event)) = self.tx.try_send(event) {
            // 通道满：等待 UI 消费。关键事件不得丢弃。
            if self.tx.send(event).await.is_err() {
                return;
            }
        }
        (self.wake)();
    }

    /// 非阻塞发送；仅用于可以不保证送达的进度事件。
    pub fn send_progress(&self, event: Event) {
        debug_assert!(event.is_progress());
        if self.tx.try_send(event).is_ok() {
            (self.wake)();
        }
    }

    pub fn wake(&self) {
        (self.wake)();
    }
}

/// 进度节流：每 ≥200 ms 或每 50 封最多发一次。
pub struct ProgressThrottle {
    last_emit: Instant,
    count_since_emit: u32,
    interval: Duration,
    batch: u32,
}

impl Default for ProgressThrottle {
    fn default() -> Self {
        Self {
            last_emit: Instant::now() - Duration::from_secs(1),
            count_since_emit: 0,
            interval: Duration::from_millis(200),
            batch: 50,
        }
    }
}

impl ProgressThrottle {
    pub fn tick(&mut self) -> bool {
        self.count_since_emit = self.count_since_emit.saturating_add(1);
        self.flush_if_due(false)
    }

    /// 强制检查是否应该发送；`force=true` 时忽略时间与数量阈值。
    pub fn flush_if_due(&mut self, force: bool) -> bool {
        if force || self.count_since_emit >= self.batch || self.last_emit.elapsed() >= self.interval
        {
            self.last_emit = Instant::now();
            self.count_since_emit = 0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn progress_is_dropped_when_full() {
        let (tx, mut rx) = mpsc::channel(1);
        let sender = EventSender::new(tx, Arc::new(|| {}));
        sender.send(Event::IndexRebuilt { indexed: 0 }).await;
        // 通道已满，进度事件应被丢弃且不阻塞。
        sender.send_progress(Event::IndexProgress {
            account: me_core::AccountId(1),
            indexed: 1,
            total: 2,
        });
        assert!(rx.recv().await.is_some());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn throttle_emits_on_batch() {
        let mut throttle = ProgressThrottle::default();
        let mut emitted = 0;
        for _ in 0..50 {
            if throttle.tick() {
                emitted += 1;
            }
        }
        assert_eq!(emitted, 1);
    }
}
