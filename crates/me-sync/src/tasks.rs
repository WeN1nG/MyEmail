//! 任务治理：`TaskId` 注册、取消令牌、连接预算、退避常量。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use me_core::{AccountId, CoreError};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

/// 退避序列：30s → 2min → 5min。
pub const BACKOFF_SECS: [u64; 3] = [30, 120, 300];

/// 跨账户全局操作连接预算。
pub const GLOBAL_CONNECTIONS: usize = 2;

/// 跨账户全局 IDLE 监听连接预算（与操作连接独立计费）。
pub const GLOBAL_WATCH_CONNECTIONS: usize = 4;

/// 单轮 IDLE 最长时长；到点后 DONE 并重新 IDLE，避免服务器空闲超时。
pub const IDLE_TIMEOUT_SECS: u64 = 29 * 60;

/// IDLE 事件去抖窗口。
pub const IDLE_DEBOUNCE_MS: u64 = 2_000;

/// 单账户取消令牌注册表。同一账户重复触发同步时，旧任务被取消。
#[derive(Clone, Default)]
pub struct TaskRegistry {
    inner: Arc<Mutex<HashMap<AccountId, CancellationToken>>>,
}

impl TaskRegistry {
    pub fn begin(&self, account: AccountId) -> CancellationToken {
        let token = CancellationToken::new();
        let mut map = self.inner.lock().expect("task registry poisoned");
        if let Some(old) = map.insert(account, token.clone()) {
            old.cancel();
        }
        token
    }

    pub fn cancel(&self, account: AccountId) {
        if let Some(token) = self
            .inner
            .lock()
            .expect("task registry poisoned")
            .get(&account)
        {
            token.cancel();
        }
    }

    pub fn cancel_all(&self) {
        for token in self.inner.lock().expect("task registry poisoned").values() {
            token.cancel();
        }
    }
}

/// 连接预算：全局 2 条，单账户 1 条。
///
/// 获取顺序固定为「全局许可 → 账户锁」，避免环等待。
#[derive(Clone)]
pub struct Governor {
    global: Arc<Semaphore>,
    accounts: Arc<Mutex<HashMap<AccountId, Arc<AsyncMutex<()>>>>>,
}

impl Default for Governor {
    fn default() -> Self {
        Self::new(GLOBAL_CONNECTIONS)
    }
}

pub struct ConnectionGuard {
    _global: OwnedSemaphorePermit,
    _account: OwnedMutexGuard<()>,
}

impl Governor {
    pub fn new(global_permits: usize) -> Self {
        Self {
            global: Arc::new(Semaphore::new(global_permits)),
            accounts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn acquire(&self, account: AccountId) -> Result<ConnectionGuard, CoreError> {
        let global = self
            .global
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| CoreError::storage("connection semaphore closed"))?;
        let lock = {
            let mut map = self.accounts.lock().expect("governor poisoned");
            map.entry(account)
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let account_guard = lock.lock_owned().await;
        Ok(ConnectionGuard {
            _global: global,
            _account: account_guard,
        })
    }
}

/// `TaskId` 注册表（架构文档要求；当前主要用账户级取消令牌）。
#[derive(Clone, Default)]
pub struct TaskIds {
    next: Arc<std::sync::atomic::AtomicU64>,
}

impl TaskIds {
    pub fn next(&self) -> me_core::TaskId {
        me_core::TaskId(self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn same_account_is_serialized() {
        let governor = Governor::default();
        let first = governor.acquire(AccountId(1)).await.unwrap();
        let blocked = tokio::time::timeout(
            std::time::Duration::from_millis(30),
            governor.acquire(AccountId(1)),
        )
        .await;
        assert!(
            blocked.is_err(),
            "second connection for same account must block"
        );
        drop(first);
        let second = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            governor.acquire(AccountId(1)),
        )
        .await;
        assert!(second.is_ok());
    }
}
