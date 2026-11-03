//! 实时同步：每账户 IDLE 监听、轮询兜底与监听连接预算。
//!
//! 设计约束（`doc/v02架构设计.md` §5.3）：
//! - 只监听 INBOX；29 分钟重新 IDLE。
//! - IDLE 事件去抖 2 秒；同步进行中只置一个 pending，不排队。
//! - 监听连接与操作连接独立计费；失败退避 30s → 2min → 5min，连续 3 次降级轮询。
//! - 轮询与手动同步共用操作连接；手动同步优先于实时同步。

use std::sync::atomic::Ordering;
use std::time::Duration;

use me_core::port::{IdleOutcome, ImapSession, ImapSessionFactory, SmtpSender, StorePort};
use me_core::{
    Account, AccountId, CoreError, ErrorScope, Event, MAX_POLL_SECS, MIN_POLL_SECS, RealtimeMode,
    now_unix,
};
use rand::Rng;
use tokio_util::sync::CancellationToken;

use crate::SyncService;
use crate::tasks::{BACKOFF_SECS, IDLE_DEBOUNCE_MS, IDLE_TIMEOUT_SECS};

enum WatchOutcome {
    /// watcher 被取消或全局实时关闭。
    Stopped,
    /// 服务器不支持 IDLE。
    Unsupported,
    Failed(CoreError),
}

impl<S, F, M> SyncService<S, F, M>
where
    S: StorePort + Clone,
    F: ImapSessionFactory + 'static,
    M: SmtpSender + 'static,
{
    pub(crate) async fn emit_realtime(
        &self,
        account: AccountId,
        mode: RealtimeMode,
        connected: bool,
        last_event_at: Option<i64>,
    ) {
        self.emit(Event::RealtimeStatus {
            account,
            mode,
            connected,
            last_event_at,
        })
        .await;
    }

    pub(crate) fn realtime_enabled(&self) -> bool {
        self.inner.realtime_enabled.load(Ordering::Relaxed)
    }

    /// 启动（或重启）某账户的 watcher。全局关闭时只回传 Off。
    pub(crate) fn start_watcher(&self, account: AccountId) {
        if !self.realtime_enabled() {
            let service = self.clone();
            tokio::spawn(async move {
                service
                    .emit_realtime(account, RealtimeMode::Off, false, None)
                    .await;
            });
            return;
        }

        let token = CancellationToken::new();
        if let Some(old) = self
            .inner
            .watchers
            .lock()
            .expect("watchers poisoned")
            .insert(account, token.clone())
        {
            old.cancel();
        }
        let service = self.clone();
        tokio::spawn(async move { service.watch_loop(account, token).await });
    }

    pub(crate) fn stop_watcher(&self, account: AccountId) {
        if let Some(token) = self
            .inner
            .watchers
            .lock()
            .expect("watchers poisoned")
            .remove(&account)
        {
            token.cancel();
            let service = self.clone();
            tokio::spawn(async move {
                service
                    .emit_realtime(account, RealtimeMode::Off, false, None)
                    .await;
            });
        }
    }

    pub(crate) fn stop_all_watchers(&self) {
        let entries: Vec<(AccountId, CancellationToken)> = self
            .inner
            .watchers
            .lock()
            .expect("watchers poisoned")
            .drain()
            .collect();
        for (account, token) in entries {
            token.cancel();
            let service = self.clone();
            tokio::spawn(async move {
                service
                    .emit_realtime(account, RealtimeMode::Off, false, None)
                    .await;
            });
        }
    }

    pub(crate) fn restart_watcher(&self, account: AccountId) {
        if let Some(token) = self
            .inner
            .watchers
            .lock()
            .expect("watchers poisoned")
            .remove(&account)
        {
            token.cancel();
        }
        self.start_watcher(account);
    }

    async fn restart_all_watchers(&self) {
        self.stop_all_watchers();
        let accounts = self.db(|s| s.list_accounts()).await.unwrap_or_default();
        for account in accounts {
            self.start_watcher(account.id);
        }
    }

    pub(crate) async fn handle_set_realtime(&self, enabled: bool) {
        self.inner
            .realtime_enabled
            .store(enabled, Ordering::Relaxed);
        let value = if enabled { "true" } else { "false" };
        let _ = self
            .db(move |s| s.set_setting(me_core::settings_key::REALTIME_SYNC, value))
            .await;
        if enabled {
            let accounts = self.db(|s| s.list_accounts()).await.unwrap_or_default();
            for account in accounts {
                self.start_watcher(account.id);
            }
        } else {
            self.stop_all_watchers();
        }
    }

    pub(crate) async fn handle_set_poll_interval(&self, secs: u32) {
        if !(MIN_POLL_SECS..=MAX_POLL_SECS).contains(&secs) {
            self.emit(Event::Error {
                scope: ErrorScope::Global,
                message: format!("轮询间隔必须在 {MIN_POLL_SECS}～{MAX_POLL_SECS} 秒之间"),
            })
            .await;
            return;
        }
        self.inner
            .poll_interval_secs
            .store(u64::from(secs), Ordering::Relaxed);
        let value = secs.to_string();
        let _ = self
            .db(move |s| s.set_setting(me_core::settings_key::POLL_INTERVAL_SECS, &value))
            .await;
        // 让正在 sleep 的轮询任务立即采用新间隔。
        self.restart_all_watchers().await;
    }

    pub(crate) async fn handle_set_account_realtime(
        &self,
        account: AccountId,
        idle_enabled: bool,
        poll_interval_secs: Option<u32>,
    ) {
        if let Some(secs) = poll_interval_secs
            && !(MIN_POLL_SECS..=MAX_POLL_SECS).contains(&secs)
        {
            self.emit(Event::Error {
                scope: ErrorScope::Account(account),
                message: format!("账户轮询间隔必须在 {MIN_POLL_SECS}～{MAX_POLL_SECS} 秒之间"),
            })
            .await;
            return;
        }
        if let Err(err) = self
            .db(move |s| s.set_account_realtime(account, idle_enabled, poll_interval_secs))
            .await
        {
            self.emit(Event::Error {
                scope: ErrorScope::Account(account),
                message: err.user_message(),
            })
            .await;
            return;
        }
        if let Ok(updated) = self.account(account).await {
            self.emit(Event::AccountUpdated { account: updated }).await;
        }
        self.restart_watcher(account);
    }

    fn effective_poll_secs(&self, account: &Account) -> u64 {
        let global = self.inner.poll_interval_secs.load(Ordering::Relaxed);
        let secs = account.poll_interval_secs.map(u64::from).unwrap_or(global);
        secs.clamp(u64::from(MIN_POLL_SECS), u64::from(MAX_POLL_SECS))
    }

    fn poll_delay(secs: u64) -> Duration {
        // ±10% 抖动，避免多账户同一秒打服务器。
        let percent = rand::thread_rng().gen_range(90..=110);
        Duration::from_millis(secs.saturating_mul(1000).saturating_mul(percent) / 100)
    }

    async fn watch_loop(&self, account_id: AccountId, token: CancellationToken) {
        let mut idle_failures = 0usize;
        let mut backoff_index = 0usize;

        loop {
            if token.is_cancelled() || !self.realtime_enabled() {
                self.emit_realtime(account_id, RealtimeMode::Off, false, None)
                    .await;
                return;
            }
            let account = match self.account(account_id).await {
                Ok(account) => account,
                Err(err) => {
                    self.emit(Event::Error {
                        scope: ErrorScope::Account(account_id),
                        message: err.user_message(),
                    })
                    .await;
                    return;
                }
            };

            if !account.idle_enabled {
                self.polling_loop(&account, &token).await;
                return;
            }

            match self.watch_idle_session(&account, &token).await {
                WatchOutcome::Stopped => {
                    self.emit_realtime(account_id, RealtimeMode::Off, false, None)
                        .await;
                    return;
                }
                WatchOutcome::Unsupported => {
                    tracing::info!(
                        account = %me_core::mask_email(&account.email),
                        "IMAP server does not advertise IDLE; falling back to polling"
                    );
                    self.polling_loop(&account, &token).await;
                    return;
                }
                WatchOutcome::Failed(err) => {
                    idle_failures += 1;
                    tracing::warn!(
                        account = %me_core::mask_email(&account.email),
                        error = %err.user_message(),
                        failures = idle_failures,
                        "IDLE watcher disconnected"
                    );
                    self.emit_realtime(account_id, RealtimeMode::Idle, false, None)
                        .await;
                    if idle_failures >= 3 {
                        tracing::warn!(
                            account = %me_core::mask_email(&account.email),
                            "IDLE failed 3 times; switching to polling"
                        );
                        self.polling_loop(&account, &token).await;
                        return;
                    }
                    let delay = BACKOFF_SECS[backoff_index.min(BACKOFF_SECS.len() - 1)];
                    backoff_index += 1;
                    if !sleep_or_cancel(&token, delay.saturating_mul(1000)).await {
                        self.emit_realtime(account_id, RealtimeMode::Off, false, None)
                            .await;
                        return;
                    }
                }
            }
        }
    }

    async fn watch_idle_session(
        &self,
        account: &Account,
        token: &CancellationToken,
    ) -> WatchOutcome {
        let credential = match self.credential(account).await {
            Ok(credential) => credential,
            Err(err) => return WatchOutcome::Failed(err),
        };
        let _guard = match self.inner.watch_governor.acquire(account.id).await {
            Ok(guard) => guard,
            Err(err) => return WatchOutcome::Failed(err),
        };
        let mut session = match self.inner.factory.connect(account, &credential).await {
            Ok(session) => session,
            Err(err) => return WatchOutcome::Failed(err),
        };

        if !session.capabilities().idle {
            let _ = session.logout().await;
            return WatchOutcome::Unsupported;
        }
        if let Err(err) = session.select("INBOX").await {
            let _ = session.logout().await;
            return WatchOutcome::Failed(err);
        }

        self.emit_realtime(account.id, RealtimeMode::Idle, true, None)
            .await;

        loop {
            if token.is_cancelled() || !self.realtime_enabled() {
                let _ = session.logout().await;
                return WatchOutcome::Stopped;
            }

            let waited = tokio::select! {
                _ = token.cancelled() => {
                    // IDLE 半状态连接不可复用；直接丢弃，不 logout。
                    return WatchOutcome::Stopped;
                }
                outcome = session.wait_for_changes(Duration::from_secs(IDLE_TIMEOUT_SECS)) => outcome,
            };

            match waited {
                Ok(IdleOutcome::Changed { .. }) => {
                    let now = now_unix();
                    self.emit_realtime(account.id, RealtimeMode::Idle, true, Some(now))
                        .await;
                    if !sleep_or_cancel(token, IDLE_DEBOUNCE_MS).await {
                        // 去抖期间被取消：连接已完成 DONE，可正常 logout。
                        let _ = session.logout().await;
                        return WatchOutcome::Stopped;
                    }
                    self.request_realtime_sync(account.id, token.clone());
                }
                Ok(IdleOutcome::Timeout | IdleOutcome::ManualInterrupt) => {
                    // wait_for_changes 已完成 DONE，连接可继续下一轮 IDLE。
                }
                Err(err) => {
                    let _ = session.logout().await;
                    return WatchOutcome::Failed(err);
                }
            }
        }
    }

    async fn polling_loop(&self, account: &Account, token: &CancellationToken) {
        self.emit_realtime(account.id, RealtimeMode::Polling, false, None)
            .await;
        loop {
            let delay = Self::poll_delay(self.effective_poll_secs(account));
            if !sleep_or_cancel(token, delay.as_millis() as u64).await {
                self.emit_realtime(account.id, RealtimeMode::Off, false, None)
                    .await;
                return;
            }
            if !self.realtime_enabled() {
                self.emit_realtime(account.id, RealtimeMode::Off, false, None)
                    .await;
                return;
            }
            self.request_realtime_sync(account.id, token.clone());
        }
    }

    /// IDLE/轮询触发的同步。手动同步优先：正在手动同步时不启动，也不取消它。
    fn request_realtime_sync(&self, account: AccountId, watcher_token: CancellationToken) {
        {
            let mut syncing = self
                .inner
                .realtime_syncing
                .lock()
                .expect("realtime syncing poisoned");
            if !syncing.insert(account) {
                self.inner
                    .realtime_pending
                    .lock()
                    .expect("realtime pending poisoned")
                    .insert(account);
                return;
            }
        }

        let service = self.clone();
        tokio::spawn(async move {
            loop {
                if watcher_token.is_cancelled() || !service.realtime_enabled() {
                    break;
                }
                let manual = service
                    .inner
                    .manual_syncing
                    .lock()
                    .expect("manual syncing poisoned")
                    .contains(&account);
                if manual {
                    break;
                }
                service.cancel_backfill(account);
                let token = service.inner.tasks.begin(account);
                service.sync_account(account, None, token).await;
                let pending = service
                    .inner
                    .realtime_pending
                    .lock()
                    .expect("realtime pending poisoned")
                    .remove(&account);
                if !pending {
                    break;
                }
            }
            service
                .inner
                .realtime_syncing
                .lock()
                .expect("realtime syncing poisoned")
                .remove(&account);
        });
    }
}

async fn sleep_or_cancel(token: &CancellationToken, millis: u64) -> bool {
    tokio::select! {
        _ = token.cancelled() => false,
        _ = tokio::time::sleep(Duration::from_millis(millis)) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventSender;
    use me_core::model::{AuthMethod, Endpoint, TlsMode};
    use std::sync::Arc;

    #[test]
    fn poll_delay_stays_within_ten_percent() {
        for _ in 0..32 {
            let delay = SyncService::<
                me_store::WritableStore,
                me_proto::ProtoFactory,
                me_proto::ProtoSmtp,
            >::poll_delay(300);
            assert!(delay >= Duration::from_millis(270_000));
            assert!(delay <= Duration::from_millis(330_000));
        }
    }

    fn account(poll: Option<u32>) -> Account {
        Account {
            id: AccountId(1),
            email: "alice@example.com".into(),
            display_name: None,
            imap: Endpoint::new("imap.example.com", 993, TlsMode::Implicit),
            smtp: Endpoint::new("smtp.example.com", 465, TlsMode::Implicit),
            auth_user: "alice@example.com".into(),
            auth_method: AuthMethod::Password,
            oauth_provider: None,
            oauth_subject: None,
            needs_reauth: false,
            idle_enabled: true,
            poll_interval_secs: poll,
            token_expires_at: None,
        }
    }

    #[tokio::test]
    async fn effective_poll_interval_clamps_and_follows_global() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = me_store::Store::open(dir.path()).expect("store");
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(4);
        let events = EventSender::new(event_tx, std::sync::Arc::new(|| {}));
        let service = SyncService::new(
            store.writable(),
            me_proto::ProtoFactory,
            me_proto::ProtoSmtp,
            store.paths(),
            events,
            Arc::new(crate::NoopBrowser),
        );

        service
            .inner
            .poll_interval_secs
            .store(u64::from(MIN_POLL_SECS - 1), Ordering::Relaxed);
        assert_eq!(
            service.effective_poll_secs(&account(None)),
            u64::from(MIN_POLL_SECS)
        );

        service
            .inner
            .poll_interval_secs
            .store(u64::from(MAX_POLL_SECS + 1), Ordering::Relaxed);
        assert_eq!(
            service.effective_poll_secs(&account(None)),
            u64::from(MAX_POLL_SECS)
        );

        service
            .inner
            .poll_interval_secs
            .store(300, Ordering::Relaxed);
        assert_eq!(service.effective_poll_secs(&account(Some(600))), 600);
        assert_eq!(
            service.effective_poll_secs(&account(Some(1))),
            u64::from(MIN_POLL_SECS)
        );
    }
}
