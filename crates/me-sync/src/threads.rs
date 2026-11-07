//! 线程头**离线回填**（v0.4 的 C5）。
//!
//! 新邮件在同步入库时直接写 `in_reply_to` / `references` 两列（见
//! `engine.rs` 的 `NewMessage` 构造）。历史邮件入库时这两列还不存在，
//! 但它们的原始报文体已经落在 `raw/*.eml.gz` 里——所以能**完全离线**回填：
//! 解压 → `parse_headers` → 写回两列。
//!
//! ## 三条边界
//!
//! 1. **不联网**：只读本地文件，因此**不占** `Governor` 的连接预算，
//!    也不该和正文补全抢带宽（D27）。
//! 2. **进度天然持久**：判据是 `"references" IS NULL`，跑到一半退出，
//!    下次启动接着跑。已回填但确实没有引用链的写 `"[]"`，不会重复入队。
//! 3. **覆盖范围有限**：`raw` 只在正文补全时写，所以只有「打开过/补全过」的
//!    历史邮件能成串。没补全的邮件接受单独成串（服务器重拉见 O30）。

use std::sync::atomic::Ordering;

use me_core::Event;
use tokio_util::sync::CancellationToken;

use crate::SyncService;

/// 每批回填多少封。纯本地 IO + 解析，200 是正文补全队列的同款批次大小。
const THREAD_BATCH: u32 = 200;

/// 一批回填结果：`(邮件, In-Reply-To, References)`。
type ThreadHeaders = (me_core::MessageId, Option<String>, Vec<String>);

impl<S, F, M> SyncService<S, F, M>
where
    S: me_core::port::StorePort + Clone,
    F: me_core::port::ImapSessionFactory + 'static,
    M: me_core::port::SmtpSender + 'static,
{
    /// 回填入口。**全局单飞**：启动时自动跑一次，用户也可能手动触发，
    /// 两次并发跑只是白白重解析同一批文件。
    pub(crate) async fn backfill_threads(&self, token: CancellationToken) {
        if self.inner.threads_running.swap(true, Ordering::SeqCst) {
            return;
        }
        let updated = self.run_thread_backlog(&token).await;
        self.inner.threads_running.store(false, Ordering::SeqCst);
        if updated > 0 {
            self.emit(Event::ThreadsBackfilled { updated }).await;
        }
    }

    /// 游标推进式的一轮回填，返回写回的邮件数。
    ///
    /// **用 `id > after` 游标而不是「取前 N 条」**：本轮读不到 raw 的邮件
    /// 会一直留在队列里，取前 N 条会让同一批被反复选中、整轮卡死。
    async fn run_thread_backlog(&self, token: &CancellationToken) -> u32 {
        let mut after: i64 = 0;
        let mut updated = 0u32;

        loop {
            if token.is_cancelled() {
                break;
            }
            let batch = match self
                .db(move |s| s.thread_backlog(after, THREAD_BATCH))
                .await
            {
                Ok(batch) => batch,
                Err(err) => {
                    tracing::warn!(error = %err.user_message(), "thread backlog query failed");
                    break;
                }
            };
            let Some(last) = batch.last().copied() else {
                break;
            };
            after = last.0;

            let parsed = self.parse_thread_batch(batch).await;
            if parsed.is_empty() {
                continue;
            }
            let count = parsed.len() as u32;
            match self
                .db(move |s| {
                    for (id, in_reply_to, references) in parsed {
                        s.set_thread_headers(id, in_reply_to.as_deref(), &references)?;
                    }
                    Ok(())
                })
                .await
            {
                Ok(()) => updated = updated.saturating_add(count),
                Err(err) => {
                    tracing::warn!(error = %err.user_message(), "thread headers write failed");
                    break;
                }
            }
        }

        updated
    }

    /// 解压 + 解析（文件 IO 与解析都在 `spawn_blocking` 里，别堵异步线程）。
    ///
    /// 读不到 raw 或解析失败的邮件**直接跳过**：两列保持 `NULL`，
    /// 下次启动会再试一次；本轮靠游标往前走，不会卡在同一封上。
    async fn parse_thread_batch(&self, batch: Vec<me_core::MessageId>) -> Vec<ThreadHeaders> {
        let paths = self.inner.paths.clone();
        tokio::task::spawn_blocking(move || {
            let mut out: Vec<ThreadHeaders> = Vec::with_capacity(batch.len());
            for id in batch {
                let raw = match me_store::cache::read_raw_gz(&paths, id) {
                    Ok(Some(raw)) => raw,
                    Ok(None) => continue,
                    Err(_) => continue,
                };
                let header = match me_proto::mime::parse_headers(&raw) {
                    Ok(header) => header,
                    Err(_) => continue,
                };
                out.push((
                    id,
                    header.in_reply_to,
                    header.references.unwrap_or_default(),
                ));
            }
            out
        })
        .await
        .unwrap_or_default()
    }

    /// 手动触发入口（`Command::BackfillThreads`）。
    pub(crate) fn start_threads_backfill(&self) {
        let service = self.clone();
        tokio::spawn(async move {
            service.backfill_threads(CancellationToken::new()).await;
        });
    }
}
