//! AI 音乐工具的执行桥。
//!
//! 音乐是**唯一**一处工具执行不在 `me-sync` 的地方：播放器是 UI 线程侧的本地线程
//! （`AudioPlayer::send(AudioCommand)`），走的是进程内的音频命令通道，而不是
//! Command/Event。所以这里只做三件事：
//!
//! 1. 发一条 `Event::AiMusicCommand` 把「要做什么」告诉 UI；
//! 2. 挂一个 `oneshot` 等 UI 回执（`Command::AiMusicResult`）；
//! 3. 超时就返回一句人话，并把等待项清掉——否则用户点了「停止」之后，
//!    一个迟到的回执会莫名其妙地唤醒下一次调用。
//!
//! 超时按「工具错误结果」回报给模型，**不让整轮失败**：模型可以换个说法，
//! 用户看到的是「媒体库还在扫描」而不是「服务不可用」。

use std::time::Duration;

use me_core::music::{MusicOp, MusicOutcome};
use me_core::port::StorePort;
use me_core::{CoreError, Event};

use crate::SyncService;

/// UI 侧的等待上限。扫描大目录时可能真的来不及，所以文案要能解释「正在扫描」。
const MUSIC_TIMEOUT: Duration = Duration::from_secs(3);

impl<S, F, M> SyncService<S, F, M>
where
    S: StorePort + Clone,
    F: me_core::port::ImapSessionFactory + 'static,
    M: me_core::port::SmtpSender + 'static,
{
    /// 把一次音乐操作交给 UI，并等它回话。
    ///
    /// 返回 `(给模型的结果文本, 活动条摘要)`，与其他工具保持一致。
    pub(crate) async fn run_music_op(
        &self,
        conversation: me_core::ai::AiConversationId,
        call_id: &str,
        op: MusicOp,
    ) -> Result<(String, String), CoreError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let Ok(mut pending) = self.inner.ai_music_pending.lock() else {
                return Err(CoreError::storage("音乐执行器状态异常"));
            };
            // 同一个 call_id 不会重复出现；若出现（重试），后来的覆盖前面的。
            pending.insert(call_id.to_string(), tx);
        }

        self.emit(Event::AiMusicCommand {
            conversation,
            call_id: call_id.to_string(),
            op,
        })
        .await;

        let outcome = match tokio::time::timeout(MUSIC_TIMEOUT, rx).await {
            Ok(Ok(outcome)) => outcome,
            // 超时或通道断掉（UI 关窗、线程退出）：都要把等待项清掉。
            _ => {
                self.drop_music_pending(call_id);
                MusicOutcome::fail(
                    "音乐播放器 3 秒内没有响应：可能正在扫描曲库、没有选择音乐目录，或者音频设备不可用",
                )
            }
        };

        if outcome.ok {
            Ok((outcome.summary.clone(), outcome.summary))
        } else {
            Err(CoreError::validation(outcome.summary))
        }
    }

    /// UI 回执入口：唤醒等待中的工具调用。找不到就静默丢弃（已经超时了）。
    pub(crate) fn complete_music_op(&self, call_id: &str, outcome: MusicOutcome) {
        let sender = self
            .inner
            .ai_music_pending
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(call_id));
        if let Some(sender) = sender {
            let _ = sender.send(outcome);
        }
    }

    fn drop_music_pending(&self, call_id: &str) {
        if let Ok(mut pending) = self.inner.ai_music_pending.lock() {
            pending.remove(call_id);
        }
    }
}
