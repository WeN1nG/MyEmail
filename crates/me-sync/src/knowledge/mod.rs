//! 知识库：摄入管线、命令处理、检索与进度事件。
//!
//! 分工：
//! - [`extract`] 只「搬字节」：文本读取、docx/pptx 抽取、PDF 无损切页、老格式转换；
//! - [`embed`] 本机嵌入（candle）与模型下载；
//! - 本模块是**唯一的编排者**：状态机、队列、限流、事件、检索融合都在这。
//!
//! 三条不变量：
//!
//! 1. **摄入全局只跑一个**（`Semaphore(1)`）。它是 CPU 密集（分词 + 推理）加磁盘写，
//!    并发只会互相拖慢，还会让进度条乱跳。
//! 2. **`kb_documents.status = 'pending'` 本身就是队列**，worker 每次取一条，
//!    断点续传天然成立（进程被杀，重启接着跑）。
//! 3. **删除只走 `KnowledgeRepo::delete`**（深删四步）。`mail_id` 的 CASCADE
//!    只兜「别留孤儿文档」，向量与 FTS 都得自己清。

pub mod embed;
pub mod extract;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use me_core::knowledge::{
    DEFAULT_MAIL_CAPACITY, EMBED_MODEL_BASE_URL, Embedder, KbKind, KbOrigin, KbSearchHit, KbStage,
    KbStatus, KnowledgeChunkId, KnowledgeDoc, KnowledgeDocId, KnowledgeSettings, KnowledgeStats,
    MAX_KB_TEXT_CHARS, MAX_KB_TITLE_CHARS, MIN_MAIL_BODY_CHARS, NewChunk, NewKnowledgeDoc,
    RAG_CANDIDATES, RRF_K, RetrievalMode, ScoredHit, SkipReason, SplitInput, rrf_fuse,
    split_structured,
};
use me_core::{Command, CoreError, ErrorScope, Event, MessageId};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::SyncService;

/// 向量化批大小。candle 单条 20~50 ms，16 条一批既有吞吐又不至于让进度条卡住。
const EMBED_BATCH: u32 = 16;

/// 每份文档最多投多少块进上下文（工具 `kb_read` 的默认页大小）。
const READ_CHUNK_PAGE: u32 = 3;

/// 摄入相关的进程内状态。放在 `Inner` 里一起构造，避免散落的静态变量。
pub struct KnowledgeState {
    /// 已加载的嵌入器。`None` = 只能用关键词检索（**正常路径**，不是错误）。
    pub(crate) embedder: RwLock<Option<Arc<dyn Embedder>>>,
    /// 全局只允许一个摄入任务（文档说的 `Semaphore(1)`）。
    pub(crate) gate: Arc<Semaphore>,
    /// worker 是否在跑。用 swap 做单飞，避免每次导入都多起一个循环。
    pub(crate) worker: AtomicBool,
    /// 正在进行的模型下载（用于取消）。
    pub(crate) download: Mutex<Option<CancellationToken>>,
    /// 老格式转换器探测结果缓存。
    pub(crate) converter: Mutex<Option<extract::Converter>>,
    /// 用户是否确认过「PDF/PPT 会发给模型并产生费用」。
    pub(crate) llm_ack: AtomicBool,
    /// 用户是否点过「暂不启用语义检索」。
    pub(crate) embed_declined: AtomicBool,
    /// 邮件自动入库容量（缓存在内存里，读 meta 太频繁）。
    pub(crate) capacity: AtomicU32,
}

impl Default for KnowledgeState {
    fn default() -> Self {
        KnowledgeState {
            embedder: RwLock::new(None),
            gate: Arc::new(Semaphore::new(1)),
            worker: AtomicBool::new(false),
            download: Mutex::new(None),
            converter: Mutex::new(None),
            llm_ack: AtomicBool::new(false),
            embed_declined: AtomicBool::new(false),
            capacity: AtomicU32::new(DEFAULT_MAIL_CAPACITY),
        }
    }
}

/// 摄入结束后要停在哪一步的判定。
enum Prepared {
    Chunked,
    Skipped(SkipReason),
}

impl<S, F, M> SyncService<S, F, M>
where
    S: me_core::port::StorePort + Clone,
    F: me_core::port::ImapSessionFactory + 'static,
    M: me_core::port::SmtpSender + 'static,
{
    // -----------------------------------------------------------------------
    // 设置
    // -----------------------------------------------------------------------

    /// 启动时读回知识库设置（容量、确认标记、模型是否就绪、转换器）。
    pub(crate) async fn load_knowledge_settings(&self) {
        let values = self
            .db(|s| {
                Ok((
                    s.get_setting(me_core::settings_key::KB_MAIL_CAPACITY)?,
                    s.get_setting(me_core::settings_key::KB_LLM_ACK)?,
                    s.get_setting(me_core::settings_key::KB_EMBED_DECLINED)?,
                    s.get_setting(me_core::settings_key::KB_EMBED_MODEL)?,
                    s.get_setting(me_core::settings_key::KB_CONVERTER)?,
                ))
            })
            .await
            .unwrap_or((None, None, None, None, None));

        let capacity = values
            .0
            .as_deref()
            .and_then(|raw| raw.parse::<u32>().ok())
            .unwrap_or(DEFAULT_MAIL_CAPACITY);
        self.inner
            .knowledge
            .capacity
            .store(capacity, Ordering::SeqCst);
        self.inner
            .knowledge
            .llm_ack
            .store(values.1.as_deref() == Some("1"), Ordering::SeqCst);
        self.inner
            .knowledge
            .embed_declined
            .store(values.2.as_deref() == Some("1"), Ordering::SeqCst);
        if let Some(value) = values.4.as_deref() {
            *self
                .inner
                .knowledge
                .converter
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some(extract::Converter::parse(value));
        }

        // 模型就绪才加载。加载失败只降级成「仅关键词检索」，不阻断启动——
        // 这条是「删掉 models/ 应用照常启动」的硬回退线。
        if values.3.as_deref() == Some(me_core::EMBED_MODEL_ID) {
            let dir = embed::model_dir(self.inner.paths.root());
            if embed::model_ready(&dir) {
                let path = dir.clone();
                match tokio::task::spawn_blocking(move || embed::CandleEmbedder::load(&path)).await
                {
                    Ok(Ok(embedder)) => self.install_embedder(Arc::new(embedder)),
                    Ok(Err(err)) => {
                        tracing::warn!(error = %err, "嵌入模型加载失败，退回仅关键词检索")
                    }
                    Err(err) => tracing::warn!(error = %err, "嵌入模型加载任务失败"),
                }
            } else {
                tracing::warn!("meta 里标了嵌入模型，但模型文件不完整，退回仅关键词检索");
            }
        }
    }

    pub(crate) fn knowledge_settings(&self) -> KnowledgeSettings {
        KnowledgeSettings {
            mail_capacity: self.inner.knowledge.capacity.load(Ordering::SeqCst),
            llm_ack: self.inner.knowledge.llm_ack.load(Ordering::SeqCst),
            embed_declined: self.inner.knowledge.embed_declined.load(Ordering::SeqCst),
            embed_model: self
                .embedder()
                .map(|embedder| embedder.model_id().to_string()),
            converter: self
                .inner
                .knowledge
                .converter
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .map(|converter| converter.as_str().to_string()),
        }
    }

    fn install_embedder(&self, embedder: Arc<dyn Embedder>) {
        if let Ok(mut guard) = self.inner.knowledge.embedder.write() {
            *guard = Some(embedder);
        }
    }

    pub(crate) fn embedder(&self) -> Option<Arc<dyn Embedder>> {
        self.inner
            .knowledge
            .embedder
            .read()
            .ok()
            .and_then(|guard| guard.clone())
    }

    fn capacity(&self) -> u32 {
        self.inner.knowledge.capacity.load(Ordering::SeqCst)
    }

    /// 当前可用的「能读文档的模型」：`(provider, model, credential)`。
    fn llm_for_documents(&self) -> Option<(Arc<dyn me_core::AiProvider>, me_core::ai::AiModel)> {
        let provider = self.ai_provider()?;
        let (models, active) = self.models_snapshot();
        let model = active
            .as_deref()
            .and_then(|id| models.iter().find(|model| model.id == id))
            .or_else(|| models.first())?
            .clone();
        Some((provider, model))
    }

    /// 有没有「能吃文档」的模型。**没有就明确失败**，不降级成本地切分（决策 O-03）。
    fn document_model_ready(&self) -> bool {
        self.llm_for_documents()
            .map(|(_, model)| model.pdf_input && model.has_key())
            .unwrap_or(false)
    }

    // -----------------------------------------------------------------------
    // 命令分发
    // -----------------------------------------------------------------------

    /// 处理知识库命令。返回 `false` 表示这条命令不属于知识库。
    pub(crate) fn handle_knowledge_command(&self, command: Command) {
        match command {
            Command::ImportKnowledgeFile { path } => self.spawn_import(path),
            Command::ProbeKnowledgeFile { path } => {
                let service = self.clone();
                tokio::spawn(async move { service.probe_file(path).await });
            }
            Command::DeleteKnowledgeDoc { doc } => {
                let service = self.clone();
                tokio::spawn(async move {
                    match service.db(move |s| s.delete(doc)).await {
                        Ok(()) => {
                            service.emit_knowledge_changed().await;
                        }
                        Err(err) => service.report_knowledge_error(err).await,
                    }
                });
            }
            Command::ConfirmKnowledgeDelete { doc } => {
                // 与 DeleteKnowledgeDoc 同一条路径：确认卡只是界面上的门，
                // 到了后端就只有一个「深删」动作。
                let service = self.clone();
                tokio::spawn(async move {
                    match service.db(move |s| s.delete(doc)).await {
                        Ok(()) => service.emit_knowledge_changed().await,
                        Err(err) => service.report_knowledge_error(err).await,
                    }
                });
            }
            Command::RebuildKnowledgeDoc { doc } => {
                let service = self.clone();
                tokio::spawn(async move {
                    // 先清语块（含向量与 FTS），再标 pending：worker 看到 pending 会重跑全流程。
                    let outcome = service
                        .db(move |s| {
                            s.replace_chunks(doc, &[])?;
                            s.set_status(doc, KbStatus::Pending, None)
                        })
                        .await;
                    match outcome {
                        Ok(()) => {
                            service.emit_knowledge_changed().await;
                            service.ensure_worker();
                        }
                        Err(err) => service.report_knowledge_error(err).await,
                    }
                });
            }
            Command::UpdateKnowledgeDoc {
                doc,
                title,
                enabled,
            } => {
                let service = self.clone();
                tokio::spawn(async move {
                    match service
                        .db(move |s| s.update_doc(doc, title.as_deref(), enabled))
                        .await
                    {
                        Ok(()) => service.emit_knowledge_changed().await,
                        Err(err) => service.report_knowledge_error(err).await,
                    }
                });
            }
            Command::SearchKnowledge { query, limit } => {
                let service = self.clone();
                tokio::spawn(async move {
                    let limit = limit.clamp(1, 50) as usize;
                    match service.retrieve(&query, limit).await {
                        Ok((hits, mode)) => {
                            let hits: Vec<KbSearchHit> = hits.iter().map(to_search_hit).collect();
                            service
                                .emit(Event::KnowledgeSearchResults {
                                    query,
                                    hits,
                                    mode,
                                    error: None,
                                })
                                .await;
                        }
                        Err(err) => {
                            service
                                .emit(Event::KnowledgeSearchResults {
                                    query,
                                    hits: Vec::new(),
                                    mode: RetrievalMode::FtsOnly,
                                    error: Some(err.user_message()),
                                })
                                .await;
                        }
                    }
                });
            }
            Command::DownloadEmbedModel => self.spawn_download(),
            Command::CancelEmbedModelDownload => {
                if let Some(token) = self
                    .inner
                    .knowledge
                    .download
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                {
                    token.cancel();
                }
            }
            Command::SetKnowledgeMailCapacity { capacity } => {
                let service = self.clone();
                tokio::spawn(async move {
                    let capacity = capacity.min(50_000);
                    if let Err(err) = service
                        .db(move |s| {
                            s.set_setting(
                                me_core::settings_key::KB_MAIL_CAPACITY,
                                &capacity.to_string(),
                            )
                        })
                        .await
                    {
                        service.report_knowledge_error(err).await;
                        return;
                    }
                    service
                        .inner
                        .knowledge
                        .capacity
                        .store(capacity, Ordering::SeqCst);
                    // 容量调小时立即清理：否则「容量」只是一个空承诺。
                    service.cleanup_mail_docs().await;
                    service.sync_mail_documents().await;
                    service.emit_knowledge_changed().await;
                });
            }
            Command::SetKbLlmAck { accepted } => {
                let service = self.clone();
                tokio::spawn(async move {
                    let result = if accepted {
                        service
                            .db(|s| s.set_setting(me_core::settings_key::KB_LLM_ACK, "1"))
                            .await
                    } else {
                        service
                            .db(|s| s.delete_setting(me_core::settings_key::KB_LLM_ACK))
                            .await
                    };
                    match result {
                        Ok(()) => {
                            service
                                .inner
                                .knowledge
                                .llm_ack
                                .store(accepted, Ordering::SeqCst);
                            service.emit_knowledge_changed().await;
                        }
                        Err(err) => service.report_knowledge_error(err).await,
                    }
                });
            }
            Command::SetKbEmbedDeclined { declined } => {
                let service = self.clone();
                tokio::spawn(async move {
                    let result = if declined {
                        service
                            .db(|s| s.set_setting(me_core::settings_key::KB_EMBED_DECLINED, "1"))
                            .await
                    } else {
                        service
                            .db(|s| s.delete_setting(me_core::settings_key::KB_EMBED_DECLINED))
                            .await
                    };
                    if let Err(err) = result {
                        service.report_knowledge_error(err).await;
                        return;
                    }
                    service
                        .inner
                        .knowledge
                        .embed_declined
                        .store(declined, Ordering::SeqCst);
                    service.emit_knowledge_changed().await;
                });
            }
            Command::RecheckKbConverter => {
                let service = self.clone();
                tokio::spawn(async move {
                    let converter = tokio::task::spawn_blocking(extract::detect_converter)
                        .await
                        .unwrap_or(extract::Converter::None);
                    *service
                        .inner
                        .knowledge
                        .converter
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(converter);
                    let _ = service
                        .db(move |s| {
                            s.set_setting(me_core::settings_key::KB_CONVERTER, converter.as_str())
                        })
                        .await;
                    service.emit_knowledge_changed().await;
                });
            }
            _ => {}
        }
    }

    /// 这条命令是不是知识库的（供 `dispatch` 判断要不要继续往下走）。
    pub(crate) fn is_knowledge_command(command: &Command) -> bool {
        matches!(
            command,
            Command::ImportKnowledgeFile { .. }
                | Command::ProbeKnowledgeFile { .. }
                | Command::DeleteKnowledgeDoc { .. }
                | Command::ConfirmKnowledgeDelete { .. }
                | Command::RebuildKnowledgeDoc { .. }
                | Command::UpdateKnowledgeDoc { .. }
                | Command::SearchKnowledge { .. }
                | Command::DownloadEmbedModel
                | Command::CancelEmbedModelDownload
                | Command::SetKnowledgeMailCapacity { .. }
                | Command::SetKbLlmAck { .. }
                | Command::RecheckKbConverter
                | Command::SetKbEmbedDeclined { .. }
        )
    }

    async fn report_knowledge_error(&self, err: CoreError) {
        self.emit(Event::Error {
            scope: ErrorScope::Global,
            message: err.user_message(),
        })
        .await;
    }

    // -----------------------------------------------------------------------
    // 统计与事件
    // -----------------------------------------------------------------------

    async fn stats(&self) -> KnowledgeStats {
        let capacity = self.capacity();
        self.db(move |s| s.stats(capacity))
            .await
            .unwrap_or(KnowledgeStats {
                mail_capacity: capacity,
                ..Default::default()
            })
    }

    pub(crate) async fn emit_knowledge_changed(&self) {
        let stats = self.stats().await;
        self.emit(Event::KnowledgeChanged { stats }).await;
    }

    // -----------------------------------------------------------------------
    // 导入与探测
    // -----------------------------------------------------------------------

    fn spawn_import(&self, path: String) {
        let service = self.clone();
        tokio::spawn(async move {
            if let Err(err) = service.import_file(PathBuf::from(path)).await {
                service.report_knowledge_error(err).await;
            }
        });
    }

    async fn import_file(&self, path: PathBuf) -> Result<(), CoreError> {
        let kind = extract::classify(&path)?;
        let (path, kind) = match kind {
            extract::ImportKind::Direct(kind) => (path, kind),
            extract::ImportKind::LegacyDoc => {
                let tmp = self.inner.paths.kb_tmp_dir().to_path_buf();
                (
                    extract::convert_legacy(&path, KbKind::Docx, &tmp).await?,
                    KbKind::Docx,
                )
            }
            extract::ImportKind::LegacyPpt => {
                let tmp = self.inner.paths.kb_tmp_dir().to_path_buf();
                (
                    extract::convert_legacy(&path, KbKind::Pptx, &tmp).await?,
                    KbKind::Pptx,
                )
            }
        };

        let size = std::fs::metadata(&path)
            .map_err(|e| CoreError::storage(format!("读取文件信息失败: {e}")))?
            .len();
        extract::check_size(kind, size)?;

        // PDF / PPTX 必须有大模型：**没有就明确失败，不降级成本地切分**（决策 O-03）。
        if kind.needs_llm() {
            if !self.inner.knowledge.llm_ack.load(Ordering::SeqCst) {
                return Err(CoreError::validation(
                    "PDF / PPT 需要先确认「会发送给模型分析」，请在导入对话框里确认后再试",
                ));
            }
            if !self.document_model_ready() {
                return Err(CoreError::validation(
                    "当前大模型不具备识别能力，无法导入知识库，请更换多模态模型",
                ));
            }
        }

        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| CoreError::storage(format!("读取文件失败: {e}")))?;
        let hash = sha256_bytes(&bytes);
        let title = file_title(&path);
        let source_ref = path.to_string_lossy().to_string();
        let size_bytes = bytes.len() as u64;
        let doc = NewKnowledgeDoc {
            origin: KbOrigin::File,
            source_ref,
            title,
            kind,
            mail_id: None,
            size_bytes,
            content_hash: hash,
        };
        let doc_id = self.db(move |s| s.upsert_document(&doc)).await?;
        self.emit_knowledge_changed().await;
        self.ensure_worker();
        let _ = doc_id;
        Ok(())
    }

    /// 探测：只读元信息，算出页数 / 批数 / token 区间 / 参考费用。
    ///
    /// 这些数**本地就能拿到**（`Document::get_pages()`），不用调模型。
    /// 界面靠它在出账单之前拦住用户（决策 O-18 的第二级确认）。
    async fn probe_file(&self, path: String) {
        let path = PathBuf::from(path);
        let response = self.probe_inner(&path).await;
        match response {
            Ok((kind, bytes, pages, batches, tokens, cost, blocked)) => {
                self.emit(Event::KnowledgeFileProbed {
                    path: path.to_string_lossy().to_string(),
                    kind,
                    bytes,
                    pages,
                    batches,
                    tokens_low: tokens.0,
                    tokens_high: tokens.1,
                    cost_usd: cost,
                    blocked,
                })
                .await;
            }
            Err(err) => self.report_knowledge_error(err).await,
        }
    }

    #[allow(clippy::type_complexity)]
    async fn probe_inner(
        &self,
        path: &Path,
    ) -> Result<
        (
            KbKind,
            u64,
            u32,
            u32,
            (u32, u32),
            Option<(f32, f32)>,
            Option<String>,
        ),
        CoreError,
    > {
        let classified = extract::classify(path)?;
        let kind = match classified {
            extract::ImportKind::Direct(kind) => kind,
            // 老格式的页数要转换后才知道：这里不为了「预估」去跑 8 分钟的转换，
            // 一律报 1 批，让用户在转换完成后再看实际批数。
            extract::ImportKind::LegacyDoc => {
                return Ok((KbKind::Docx, 0, 0, 1, (0, 0), None, None));
            }
            extract::ImportKind::LegacyPpt => {
                return Ok((KbKind::Pptx, 0, 0, 1, (0, 0), None, None));
            }
        };
        let bytes = std::fs::metadata(path)
            .map_err(|e| CoreError::storage(format!("读取文件信息失败: {e}")))?
            .len();
        extract::check_size(kind, bytes)?;

        let blocked = if kind.needs_llm() && !self.document_model_ready() {
            Some("当前大模型不具备识别能力，无法导入知识库，请更换多模态模型".to_string())
        } else {
            None
        };

        let model = self
            .llm_for_documents()
            .map(|(_, model)| model.model)
            .unwrap_or_default();

        let (pages, batches, tokens) = match kind {
            KbKind::Pdf => {
                let path = path.to_path_buf();
                let content = tokio::fs::read(&path)
                    .await
                    .map_err(|e| CoreError::storage(format!("读取 PDF 失败: {e}")))?;
                let total = tokio::task::spawn_blocking(move || extract::pdf_page_count(&content))
                    .await
                    .map_err(|e| CoreError::storage(format!("解析 PDF 任务失败: {e}")))??;
                let batches = extract::pdf_batches(total, bytes).len() as u32;
                let tokens = me_core::knowledge::estimate_pdf_tokens(total);
                (total, batches, tokens)
            }
            KbKind::Pptx => {
                let content = tokio::fs::read(path)
                    .await
                    .map_err(|e| CoreError::storage(format!("读取 PPT 失败: {e}")))?;
                let text = tokio::task::spawn_blocking(move || extract::pptx_text(&content))
                    .await
                    .map_err(|e| CoreError::storage(format!("解析 PPT 任务失败: {e}")))??;
                let chars = text.chars().count() as u32;
                let batches = extract::text_batches(&text).len() as u32;
                // 中文一字约一个 token，给个区间就够了。
                (0, batches, (chars, chars.saturating_mul(2)))
            }
            _ => (0, 1, (0, 0)),
        };
        let cost = me_core::knowledge::estimate_split_cost_usd(&model, tokens.0, tokens.1)
            .map(|(low, high)| (low as f32, high as f32));
        Ok((kind, bytes, pages, batches, tokens, cost, blocked))
    }

    // -----------------------------------------------------------------------
    // worker 与状态机
    // -----------------------------------------------------------------------

    /// 确保 worker 在跑。**单飞**：已经在跑就直接返回，不再起一个循环。
    pub(crate) fn ensure_worker(&self) {
        if self.inner.knowledge.worker.swap(true, Ordering::SeqCst) {
            return;
        }
        let service = self.clone();
        tokio::spawn(async move {
            // 全局只允许一个摄入任务在跑：它与同步、AI 请求抢的是同一台机器的 CPU。
            let gate = Arc::clone(&service.inner.knowledge.gate);
            let _permit = gate.acquire_owned().await;
            loop {
                // 两类待办，分开取：
                // 1) `pending` —— 还没提取拆分，走完整流程；
                // 2) `chunked` 但还没向量 —— 只补向量，**要等模型就绪**。
                //
                // 混在一起取会死循环：模型没就绪时 `chunked` 的状态不变，
                // worker 永远取到同一份文档原地打转，后面的排不上队。
                let next = service.db(|s| s.pending_docs(1)).await;
                let doc = match next {
                    Ok(docs) => docs.into_iter().next(),
                    Err(err) => {
                        tracing::warn!(error = %err, "读取待处理文档失败");
                        None
                    }
                };
                if let Some(doc) = doc {
                    service.ingest_one(doc).await;
                    // 每处理完一份就让出一次，别把同步与 UI 饿死。
                    tokio::task::yield_now().await;
                    continue;
                }

                // 没有待拆分的了，才看补向量。
                // **模型没就绪就到此为止**：反复查也查不到新东西。
                if service.embedder().is_none() {
                    break;
                }
                let next = service.db(|s| s.docs_needing_vectors(1)).await;
                let doc = match next {
                    Ok(docs) => docs.into_iter().next(),
                    Err(err) => {
                        tracing::warn!(error = %err, "读取待向量化文档失败");
                        None
                    }
                };
                let Some(doc) = doc else { break };
                service.embed_only(doc).await;
                tokio::task::yield_now().await;
            }
            service
                .inner
                .knowledge
                .worker
                .store(false, Ordering::SeqCst);
            service.emit_knowledge_changed().await;
        });
    }

    /// 只补向量（文档已分块）。模型没就绪时什么都不做。
    async fn embed_only(&self, doc: KnowledgeDoc) {
        if self.embedder().is_none() {
            return;
        }
        if let Err((err, stage)) = self.embed_chunks(&doc).await {
            let message = err.user_message();
            let stored = message.clone();
            let _ = self
                .db(move |s| s.set_status(doc.id, KbStatus::Failed, Some(&stored)))
                .await;
            self.emit(Event::KnowledgeDocFailed {
                doc: doc.id,
                message,
                stage,
            })
            .await;
        }
    }

    async fn ingest_one(&self, doc: KnowledgeDoc) {
        let doc_id = doc.id;
        let outcome = match doc.status {
            KbStatus::Pending => match self.prepare_chunks(&doc).await {
                Ok(Prepared::Chunked) => Ok(()),
                Ok(Prepared::Skipped(reason)) => {
                    let _ = self.db(move |s| s.set_skip_reason(doc_id, reason)).await;
                    self.emit(Event::KnowledgeDocSkipped {
                        doc: doc_id,
                        reason,
                    })
                    .await;
                    return;
                }
                Err(err) => Err((err, KbStage::Splitting)),
            },
            _ => Ok(()),
        };

        if let Err((err, stage)) = outcome {
            let message = err.user_message();
            let stored = message.clone();
            let _ = self
                .db(move |s| s.set_status(doc_id, KbStatus::Failed, Some(&stored)))
                .await;
            self.emit(Event::KnowledgeDocFailed {
                doc: doc_id,
                message,
                stage,
            })
            .await;
            self.emit_knowledge_changed().await;
            return;
        }

        // 重新读一次状态：prepare 可能把它推到了 chunked。
        let fresh = self.db(move |s| s.get(doc_id)).await.ok().flatten();
        let Some(fresh) = fresh else { return };
        if fresh.status == KbStatus::Chunked
            && let Err((err, stage)) = self.embed_chunks(&fresh).await
        {
            let message = err.user_message();
            let stored = message.clone();
            let _ = self
                .db(move |s| s.set_status(doc_id, KbStatus::Failed, Some(&stored)))
                .await;
            self.emit(Event::KnowledgeDocFailed {
                doc: doc_id,
                message,
                stage,
            })
            .await;
        }
        self.emit_knowledge_changed().await;
    }

    /// 提取 + 拆分 + 落库。产出 `Chunked` 或 `Skipped`。
    async fn prepare_chunks(&self, doc: &KnowledgeDoc) -> Result<Prepared, CoreError> {
        self.emit(Event::KnowledgeProgress {
            doc: doc.id,
            stage: KbStage::Extracting,
            done: 0,
            total: 1,
        })
        .await;

        let chunks = match doc.kind {
            KbKind::Md | KbKind::Txt => {
                let path = PathBuf::from(&doc.source_ref);
                let text = tokio::task::spawn_blocking(move || extract::read_text_file(&path))
                    .await
                    .map_err(|e| CoreError::storage(format!("读取文本任务失败: {e}")))??;
                match me_core::knowledge::is_meaningful(&text) {
                    Ok(()) => split_structured(&text),
                    Err(reason) => return Ok(Prepared::Skipped(reason)),
                }
            }
            KbKind::Docx => {
                let path = PathBuf::from(&doc.source_ref);
                let bytes = tokio::fs::read(&path)
                    .await
                    .map_err(|e| CoreError::storage(format!("读取 docx 失败: {e}")))?;
                let text = tokio::task::spawn_blocking(move || extract::docx_text(&bytes))
                    .await
                    .map_err(|e| CoreError::storage(format!("解析 docx 任务失败: {e}")))??;
                match me_core::knowledge::is_meaningful(&text) {
                    Ok(()) => split_structured(&text),
                    Err(reason) => return Ok(Prepared::Skipped(reason)),
                }
            }
            KbKind::Pdf | KbKind::Pptx => self.split_with_llm(doc).await?,
            KbKind::Mail => {
                let paths = self.inner.paths.clone();
                let message = doc
                    .mail_id
                    .ok_or_else(|| CoreError::storage("邮件来源的文档缺少 mail_id"))?;
                let body = tokio::task::spawn_blocking(move || {
                    me_store::cache::read_body(&paths, message)
                })
                .await
                .map_err(|e| CoreError::storage(format!("读取正文任务失败: {e}")))??;
                let Some(body) = body else {
                    return Ok(Prepared::Skipped(SkipReason::Empty));
                };
                // 正文太短不值得索引（空邮件、纯通知、回执）。这条在**读正文时**判，
                // SQL 选不出来，所以窗口大小与最终 indexed 数天然不等。
                if body.chars().filter(|c| !c.is_whitespace()).count() < MIN_MAIL_BODY_CHARS {
                    return Ok(Prepared::Skipped(SkipReason::TooShort));
                }
                split_structured(&body)
            }
            KbKind::Text => {
                // AI 写入在 `kb_add` 里就分好块了；到这里说明状态被外部改乱了。
                return Err(CoreError::storage(
                    "AI 写入的文档不该排队重新提取（内容不在文件里）",
                ));
            }
        };

        if chunks.is_empty() {
            return Ok(Prepared::Skipped(SkipReason::Empty));
        }

        // 实体匹配：语块刚就绪、向量化之前（决策 §8.2）。
        let entity_links = self.match_document_entities(&chunks).await;
        self.write_chunk_file(doc, &chunks).await?;

        let doc_id = doc.id;
        let total = chunks.len() as u32;
        self.emit(Event::KnowledgeProgress {
            doc: doc_id,
            stage: KbStage::Chunking,
            done: total,
            total,
        })
        .await;
        self.db(move |s| s.replace_chunks(doc_id, &chunks)).await?;
        if let Some(links) = entity_links {
            let doc_id = doc.id;
            let _ = self.db(move |s| s.link_entities(doc_id, &links)).await;
        }
        Ok(Prepared::Chunked)
    }

    /// PDF / PPTX：分批交给 `split_document`，合并语块并重排 `ord` / `locator`。
    async fn split_with_llm(&self, doc: &KnowledgeDoc) -> Result<Vec<NewChunk>, CoreError> {
        let Some((provider, model)) = self.llm_for_documents() else {
            return Err(CoreError::validation(
                "当前大模型不具备识别能力，无法导入知识库，请更换多模态模型",
            ));
        };
        if !model.pdf_input || !model.has_key() {
            return Err(CoreError::validation(
                "当前大模型不具备识别能力，无法导入知识库，请更换多模态模型",
            ));
        }
        let config = model.to_config();

        match doc.kind {
            KbKind::Pdf => {
                let path = PathBuf::from(&doc.source_ref);
                let content = tokio::fs::read(&path)
                    .await
                    .map_err(|e| CoreError::storage(format!("读取 PDF 失败: {e}")))?;
                let bytes_len = content.len() as u64;
                let pages = {
                    let content = content.clone();
                    tokio::task::spawn_blocking(move || extract::pdf_page_count(&content))
                        .await
                        .map_err(|e| CoreError::storage(format!("解析 PDF 任务失败: {e}")))??
                };
                let batches = extract::pdf_batches(pages, bytes_len);
                let total_batches = batches.len() as u32;
                let mut out = Vec::new();

                for (index, batch) in batches.iter().enumerate() {
                    let part = index as u32 + 1;
                    self.emit(Event::KnowledgeProgress {
                        doc: doc.id,
                        stage: KbStage::Splitting,
                        done: part - 1,
                        total: total_batches,
                    })
                    .await;

                    // 只有一批时**完全不切**：把原始字节直接发出去（未超阈值不切）。
                    let pdf_bytes = if total_batches == 1 {
                        content.clone()
                    } else {
                        let source = content.clone();
                        let batch = *batch;

                        tokio::task::spawn_blocking(move || {
                            let bytes = extract::extract_pdf_pages(&source, batch)?;
                            extract::verify_pdf_pages(&bytes, batch.pages)?;
                            Ok::<Vec<u8>, CoreError>(bytes)
                        })
                        .await
                        .map_err(|e| CoreError::storage(format!("切页任务失败: {e}")))??
                    };

                    let request = me_core::knowledge::DocumentSplitRequest {
                        config: config.clone(),
                        credential: Self::ai_credential(&model),
                        input: SplitInput::Pdf {
                            bytes: pdf_bytes,
                            first_page: batch.first_page,
                        },
                        model: config.model.clone(),
                        max_chunks: me_core::knowledge::MAX_SPLIT_CHUNKS,
                    };
                    let outcome = provider.split_document(request).await?;
                    if outcome.truncated && outcome.chunks.is_empty() {
                        // 截断且一块都没拿到：重试只会重复花钱，直接失败。
                        return Err(CoreError::validation(
                            "模型输出被截断且没有可用语块，请换模型或把文件拆小后重试",
                        ));
                    }
                    for chunk in outcome.chunks {
                        let locator = chunk.locator.map(|value| {
                            me_core::knowledge::shift_locator(&value, batch.first_page)
                        });
                        out.push(NewChunk::new(chunk.text, locator));
                    }
                }
                Ok(out)
            }
            KbKind::Pptx => {
                let path = PathBuf::from(&doc.source_ref);
                let content = tokio::fs::read(&path)
                    .await
                    .map_err(|e| CoreError::storage(format!("读取 PPT 失败: {e}")))?;
                let text = tokio::task::spawn_blocking(move || extract::pptx_text(&content))
                    .await
                    .map_err(|e| CoreError::storage(format!("解析 PPT 任务失败: {e}")))??;
                let batches = extract::text_batches(&text);
                let total_batches = batches.len() as u32;
                let mut out = Vec::new();
                for (index, batch) in batches.iter().enumerate() {
                    let part = index as u32 + 1;
                    self.emit(Event::KnowledgeProgress {
                        doc: doc.id,
                        stage: KbStage::Splitting,
                        done: part - 1,
                        total: total_batches,
                    })
                    .await;
                    let request = me_core::knowledge::DocumentSplitRequest {
                        config: config.clone(),
                        credential: Self::ai_credential(&model),
                        input: SplitInput::Text {
                            text: batch.clone(),
                            part,
                            total: total_batches,
                        },
                        model: config.model.clone(),
                        max_chunks: me_core::knowledge::MAX_SPLIT_CHUNKS,
                    };
                    let outcome = provider.split_document(request).await?;
                    for chunk in outcome.chunks {
                        out.push(NewChunk::new(chunk.text, chunk.locator));
                    }
                }
                Ok(out)
            }
            _ => Err(CoreError::storage("只有 PDF / PPTX 走模型拆分")),
        }
    }

    /// 向量化。**模型没就绪就停在 chunked**——FTS 照样能搜，这是正常降级。
    async fn embed_chunks(&self, doc: &KnowledgeDoc) -> Result<(), (CoreError, KbStage)> {
        let Some(embedder) = self.embedder() else {
            return Ok(());
        };
        let model_id = embedder.model_id().to_string();
        let dim = embedder.dim() as u32;
        let doc_id = doc.id;

        // 总数只用来画进度条（`chunk_count` 在 replace_chunks 时已写好）。
        let total = doc.chunk_count.max(1);
        let mut done = 0u32;
        let mut truncated = 0u32;

        loop {
            let rows = self
                .db(move |s| s.chunks_needing_vectors(doc_id, EMBED_BATCH))
                .await
                .map_err(|err| (err, KbStage::Embedding))?;
            if rows.is_empty() {
                break;
            }
            let texts: Vec<String> = rows.iter().map(|chunk| chunk.text.clone()).collect();
            for text in &texts {
                if text.chars().count() > me_core::knowledge::MAX_CHUNK_CHARS {
                    truncated += 1;
                }
            }
            let embedder = Arc::clone(&embedder);
            let vectors = tokio::task::spawn_blocking(move || {
                let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
                embedder.embed_documents(&refs)
            })
            .await
            .map_err(|err| {
                (
                    CoreError::storage(format!("嵌入任务失败: {err}")),
                    KbStage::Embedding,
                )
            })?
            .map_err(|err| (err, KbStage::Embedding))?;

            let mut pairs: Vec<(KnowledgeChunkId, Vec<f32>)> = Vec::with_capacity(rows.len());
            let mut ids = Vec::with_capacity(rows.len());
            for (chunk, vector) in rows.iter().zip(vectors) {
                pairs.push((chunk.id, vector));
                ids.push(chunk.id);
            }

            // **顺序固定：先写向量，再标完成**（决策 O-22）。
            // WAL 跨 ATTACH 库提交不原子，反过来崩溃会留下「标了完成但没向量」
            // 的块，而且永远不会被重跑。
            self.db(move |s| s.put_vectors(&pairs))
                .await
                .map_err(|err| (err, KbStage::Embedding))?;
            self.db(move |s| s.mark_embedded(&ids))
                .await
                .map_err(|err| (err, KbStage::Embedding))?;

            done += rows.len() as u32;
            self.emit(Event::KnowledgeProgress {
                doc: doc_id,
                stage: KbStage::Embedding,
                done: done.min(total),
                total,
            })
            .await;
            // 每批让出一次：嵌入是 CPU 密集的，一直占着会拖慢同步与 UI。
            tokio::task::yield_now().await;
        }

        if truncated > 0 {
            // 不为 0 说明分块参数算错了（块被 bge 静默截断，尾部文字永不参与检索）。
            tracing::warn!(
                truncated,
                doc = doc_id.0,
                "有语块超过 {} 字上限，超出部分不会参与检索",
                me_core::knowledge::MAX_CHUNK_CHARS
            );
        }

        self.db(move |s| {
            s.set_embedding(doc_id, &model_id, dim)?;
            s.set_status(doc_id, KbStatus::Embedded, None)
        })
        .await
        .map_err(|err| (err, KbStage::Embedding))?;
        self.emit(Event::KnowledgeProgress {
            doc: doc_id,
            stage: KbStage::Done,
            done: total,
            total,
        })
        .await;
        Ok(())
    }

    /// 语块导出物：`file` / `ai` 才写（决策 O-27），先写 staging 再 rename。
    async fn write_chunk_file(
        &self,
        doc: &KnowledgeDoc,
        chunks: &[NewChunk],
    ) -> Result<(), CoreError> {
        if !doc.origin.writes_chunk_file() {
            return Ok(());
        }
        let final_path = self.inner.paths.kb_chunk_file(doc.id.0);
        let staging = self.inner.paths.kb_staging_file(doc.id.0);
        let mut body = String::new();
        for (index, chunk) in chunks.iter().enumerate() {
            let line = serde_json::json!({
                "ord": index,
                "locator": chunk.locator,
                "chars": chunk.chars(),
                "text": chunk.text,
            });
            body.push_str(&line.to_string());
            body.push('\n');
        }
        tokio::task::spawn_blocking(move || -> Result<(), CoreError> {
            use std::io::Write;
            let mut file = std::fs::File::create(&staging)
                .map_err(|e| CoreError::storage(format!("写入语块临时文件失败: {e}")))?;
            file.write_all(body.as_bytes())
                .map_err(|e| CoreError::storage(format!("写入语块临时文件失败: {e}")))?;
            // flush + sync_all 再 rename：否则断电后可能 rename 出来一个空文件。
            file.sync_all()
                .map_err(|e| CoreError::storage(format!("落盘语块临时文件失败: {e}")))?;
            std::fs::rename(&staging, &final_path)
                .map_err(|e| CoreError::storage(format!("替换语块文件失败: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| CoreError::storage(format!("写语块任务失败: {e}")))??;
        Ok(())
    }

    /// 实体匹配：文档 → 已知实体。返回 `(entity_id, 命中语块数)`。
    async fn match_document_entities(&self, chunks: &[NewChunk]) -> Option<Vec<(i64, u32)>> {
        let aliases = self
            .db(|s| {
                let entities = s.list_entities(u32::MAX / 2)?;
                let mut pairs: Vec<(String, i64)> = Vec::new();
                for entity in entities {
                    let name = me_core::ai::normalize_entity_name(&entity.canonical_name);
                    if !name.is_empty() {
                        pairs.push((name.to_lowercase(), entity.id));
                    }
                    for alias in entity.aliases {
                        let alias = me_core::ai::normalize_entity_name(&alias);
                        if !alias.is_empty() {
                            pairs.push((alias.to_lowercase(), entity.id));
                        }
                    }
                }
                // 按长度降序：中文里「张三」会先命中「张三丰」的一部分。
                pairs.sort_by_key(|item| std::cmp::Reverse(item.0.chars().count()));
                Ok(pairs)
            })
            .await
            .ok()?;
        if aliases.is_empty() {
            return None;
        }
        let mut hits: std::collections::HashMap<i64, u32> = std::collections::HashMap::new();
        for chunk in chunks {
            let text = chunk.text.to_lowercase();
            for entity in me_core::knowledge::match_entities(&text, &aliases) {
                *hits.entry(entity).or_insert(0) += 1;
            }
        }
        if hits.is_empty() {
            return None;
        }
        let mut out: Vec<(i64, u32)> = hits.into_iter().collect();
        out.sort_by_key(|item| std::cmp::Reverse(item.1));
        Some(out)
    }

    // -----------------------------------------------------------------------
    // 邮件自动入库（批次 5）
    // -----------------------------------------------------------------------

    /// 按容量窗口把邮件排进队列。**由同步收尾与正文补全末尾调用**。
    pub(crate) async fn sync_mail_documents(&self) {
        let capacity = self.capacity();
        if capacity == 0 {
            self.cleanup_mail_docs().await;
            return;
        }
        let seeds = self
            .db(move |s| s.mail_seeds(capacity))
            .await
            .unwrap_or_default();
        if seeds.is_empty() {
            return;
        }

        let paths = self.inner.paths.clone();
        let mut queued = 0usize;
        for seed in seeds {
            let message = seed.message;
            let body = tokio::task::spawn_blocking({
                let paths = paths.clone();
                move || me_store::cache::read_body(&paths, message)
            })
            .await
            .ok()
            .and_then(|result| result.ok())
            .flatten();
            let Some(body) = body else { continue };

            let hash = sha256_bytes(body.as_bytes());
            let title = match seed.subject.as_deref().map(str::trim) {
                Some(subject) if !subject.is_empty() => subject.to_string(),
                _ => format!(
                    "(无主题) · {}",
                    seed.from_name.as_deref().unwrap_or("未知发件人")
                ),
            };
            let size = body.len() as u64;
            let doc = NewKnowledgeDoc {
                origin: KbOrigin::Mail,
                source_ref: seed.message.0.to_string(),
                title,
                kind: KbKind::Mail,
                mail_id: Some(seed.message),
                size_bytes: size,
                content_hash: hash,
            };
            let doc_id = match self.db(move |s| s.upsert_document(&doc)).await {
                Ok(id) => id,
                Err(err) => {
                    tracing::debug!(error = %err, "邮件入库 upsert 失败");
                    continue;
                }
            };
            queued += 1;

            // 正文太短的直接标 skipped：否则 worker 每次都要读一遍正文再丢。
            if body.chars().filter(|c| !c.is_whitespace()).count() < MIN_MAIL_BODY_CHARS {
                let _ = self
                    .db(move |s| s.set_skip_reason(doc_id, SkipReason::TooShort))
                    .await;
            }
        }

        // 清理放在入库之后：窗口刚变过，此时的「窗口外」才是最终答案。
        self.cleanup_mail_docs().await;
        if queued > 0 {
            self.ensure_worker();
        }
    }

    /// 把超出容量窗口的邮件文档连带向量一起删掉。
    async fn cleanup_mail_docs(&self) {
        let capacity = self.capacity();
        let ids = self
            .db(move |s| s.mail_docs_outside_window(capacity))
            .await
            .unwrap_or_default();
        if ids.is_empty() {
            return;
        }
        let count = ids.len();
        for id in ids {
            let _ = self.db(move |s| s.delete(id)).await;
        }
        tracing::info!(count, "知识库容量清理：移除窗口外的邮件索引");
    }

    /// 删邮件前的清理钩子：所有删邮件的路径都要先调它（决策 O-26）。
    pub(crate) async fn purge_mail_documents(&self, messages: &[MessageId]) {
        if messages.is_empty() {
            return;
        }
        let messages = messages.to_vec();
        let removed = self.db(move |s| s.purge_mail(&messages)).await.unwrap_or(0);
        if removed > 0 {
            self.emit_knowledge_changed().await;
        }
    }

    // -----------------------------------------------------------------------
    // 检索
    // -----------------------------------------------------------------------

    /// 混合检索：FTS + KNN → RRF。模型没就绪就只跑 FTS（正常路径）。
    pub(crate) async fn retrieve(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<(Vec<ScoredHit>, RetrievalMode), CoreError> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok((Vec::new(), RetrievalMode::FtsOnly));
        }
        let fts_query = trimmed.to_string();
        let fts = self.db(move |s| s.fts(&fts_query, RAG_CANDIDATES)).await?;

        let Some(embedder) = self.embedder() else {
            // 降级：没有向量就按 FTS 排名给分，界面照样能用。
            let hits = fts
                .into_iter()
                .take(limit)
                .enumerate()
                .map(|(rank, mut hit)| {
                    hit.score = 1.0 / (rank as f32 + 1.0);
                    ScoredHit { hit, score: 1.0 }
                })
                .collect::<Vec<_>>();
            return Ok((hits, RetrievalMode::FtsOnly));
        };

        let query_text = trimmed.to_string();
        let vectors = tokio::task::spawn_blocking(move || embedder.embed_query(&query_text))
            .await
            .map_err(|e| CoreError::storage(format!("查询嵌入任务失败: {e}")))??;
        let knn = self.db(move |s| s.knn(&vectors, RAG_CANDIDATES)).await?;
        Ok((rrf_fuse(&fts, &knn, RRF_K, limit), RetrievalMode::Hybrid))
    }

    // -----------------------------------------------------------------------
    // 模型下载
    // -----------------------------------------------------------------------

    fn spawn_download(&self) {
        let service = self.clone();
        tokio::spawn(async move {
            let token = CancellationToken::new();
            {
                let mut slot = service
                    .inner
                    .knowledge
                    .download
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if slot.is_some() {
                    return; // 已经在下载
                }
                *slot = Some(token.clone());
            }

            let dir = embed::model_dir(service.inner.paths.root());
            let base = service
                .db(|s| s.get_setting(me_core::settings_key::KB_EMBED_MODEL_URL))
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| EMBED_MODEL_BASE_URL.to_string());
            let client = match reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(15))
                .timeout(std::time::Duration::from_secs(300))
                .build()
            {
                Ok(client) => client,
                Err(err) => {
                    service
                        .emit(Event::EmbedModelFailed {
                            message: format!("创建下载客户端失败: {err}"),
                        })
                        .await;
                    return;
                }
            };

            let emit = service.clone();
            let result = tokio::select! {
                _ = token.cancelled() => Err(CoreError::validation("下载已取消")),
                result = embed::download_model(&client, &base, &dir, move |done, total| {
                    let emit = emit.clone();
                    tokio::spawn(async move {
                        emit.emit(Event::EmbedModelDownloadProgress { done, total })
                            .await;
                    });
                }) => result,
            };

            {
                let mut slot = service
                    .inner
                    .knowledge
                    .download
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *slot = None;
            }

            match result {
                Ok(()) => {
                    let path = dir.clone();
                    let loaded =
                        tokio::task::spawn_blocking(move || embed::CandleEmbedder::load(&path))
                            .await;
                    match loaded {
                        Ok(Ok(embedder)) => {
                            service.install_embedder(Arc::new(embedder));
                            let _ = service
                                .db(|s| {
                                    s.set_setting(
                                        me_core::settings_key::KB_EMBED_MODEL,
                                        me_core::EMBED_MODEL_ID,
                                    )
                                })
                                .await;
                            service
                                .emit(Event::EmbedModelReady {
                                    model: me_core::EMBED_MODEL_ID.to_string(),
                                    dim: me_core::EMBED_DIM as u32,
                                })
                                .await;
                            service.emit_knowledge_changed().await;
                            // 已有语块还没向量：重排队一次（不改状态，直接唤醒 worker
                            // 不会重跑 chunked 的文档，所以这里显式扫一遍）。
                            service.embed_pending_documents().await;
                        }
                        Ok(Err(err)) => {
                            service
                                .emit(Event::EmbedModelFailed {
                                    message: err.user_message(),
                                })
                                .await;
                        }
                        Err(err) => {
                            service
                                .emit(Event::EmbedModelFailed {
                                    message: format!("模型加载任务失败: {err}"),
                                })
                                .await;
                        }
                    }
                }
                Err(err) => {
                    service
                        .emit(Event::EmbedModelFailed {
                            message: err.user_message(),
                        })
                        .await;
                }
            }
        });
    }

    /// 把「已分块但还没向量化」的文档重新排进队列（模型刚就绪时用）。
    async fn embed_pending_documents(&self) {
        // 只看「已分块但还没向量」的文档。别用 `list()` 全扫：
        // 列表是按界面筛选排的，与「哪些等着补向量」无关。
        let docs = self
            .db(|s| s.docs_needing_vectors(200))
            .await
            .unwrap_or_default();
        if !docs.is_empty() {
            self.ensure_worker();
        }
    }

    // -----------------------------------------------------------------------
    // AI 工具用：读、列、写、删
    // -----------------------------------------------------------------------

    /// 检索工具。返回 `(给模型的文本, 本轮命中的语块引用)`。
    ///
    /// 引用列表是给**引用校验**用的：模型回答里写 `[kb:12]` 时，
    /// 12 必须出现在这个列表里，否则那个锚点是编造的（决策 O-13）。
    pub(crate) async fn knowledge_search_tool(
        &self,
        query: &str,
        limit: usize,
        kind: Option<KbKind>,
        entity: Option<&str>,
    ) -> Result<(String, Vec<me_core::ai::KbRef>), CoreError> {
        let (hits, mode) = self.retrieve(query, limit.clamp(1, 20)).await?;
        let mut lines = vec![format!("检索模式：{}", mode.label())];
        if let Some(entity) = entity.filter(|value| !value.trim().is_empty()) {
            lines.push(format!("实体过滤：{entity}"));
        }
        if hits.is_empty() {
            lines.push("没有找到相关内容。".to_string());
            return Ok((lines.join("\n"), Vec::new()));
        }
        // 来源筛选（kind）必须在渲染预算**之前**做：预算算的是最终送进
        // 上下文的字符数，被筛掉的块不该占额度。
        let mut kept = Vec::new();
        for scored in hits {
            if let Some(kind) = kind
                && !self
                    .db({
                        let doc = scored.hit.doc;
                        move |s| s.get(doc)
                    })
                    .await
                    .ok()
                    .flatten()
                    .map(|doc| doc.kind == kind)
                    .unwrap_or(true)
            {
                continue;
            }
            kept.push(scored);
        }
        // 预算与截断统一走 `render_rag_context`：每块 ≤480 字、总量 ≤5000 字、
        // 超了少取几条（§9.1 上下文预算、O-21 块长上限）。
        let context = me_core::knowledge::render_rag_context(&kept);
        if context.is_empty() {
            lines.push("没有找到相关内容。".to_string());
            return Ok((lines.join("\n"), Vec::new()));
        }
        lines.push(context);
        // 引用只登记**真正送进上下文**的那些块：被预算挤掉的块模型没看见，
        // 不该被它引用。
        let refs = kept
            .iter()
            .take(me_core::knowledge::RAG_TOP_K)
            .map(|scored| me_core::ai::KbRef {
                doc: scored.hit.doc.0,
                title: scored.hit.title.clone(),
                ord: scored.hit.ord,
            })
            .collect();
        Ok((lines.join("\n\n"), refs))
    }

    /// 读文档。返回 `(给模型的文本, 本轮读到的语块引用)`。
    pub(crate) async fn knowledge_read_tool(
        &self,
        doc: KnowledgeDocId,
        from: u32,
        count: u32,
    ) -> Result<(String, Vec<me_core::ai::KbRef>), CoreError> {
        let count = count.clamp(1, 10);
        let fetched = self.db(move |s| s.get(doc)).await?;
        let Some(doc_row) = fetched else {
            return Ok((format!("文档 {doc} 不存在（可能已被删除）"), Vec::new()));
        };
        let chunks = self.db(move |s| s.chunks(doc, from, count)).await?;
        let mut out = format!(
            "[kb:{}] {}（{}，共 {} 块）\n",
            doc.0,
            doc_row.title,
            doc_row.kind.label(),
            doc_row.chunk_count
        );
        if chunks.is_empty() {
            out.push_str("这一段没有语块。\n");
            return Ok((out, Vec::new()));
        }
        let mut refs = Vec::new();
        for chunk in chunks {
            let locator = chunk
                .locator
                .as_deref()
                .map(|value| format!(" · {value}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "\n#{}{locator}\n{}\n",
                chunk.ord,
                chunk.text.chars().take(480).collect::<String>()
            ));
            refs.push(me_core::ai::KbRef {
                doc: doc.0,
                title: doc_row.title.clone(),
                ord: chunk.ord,
            });
        }
        Ok((out, refs))
    }

    pub(crate) async fn knowledge_list_tool(
        &self,
        query: Option<&str>,
        status: Option<KbStatus>,
        limit: u32,
    ) -> Result<String, CoreError> {
        let filter = me_core::knowledge::KbFilter {
            origin: None,
            status,
            query: query.map(|value| value.to_string()),
        };
        let docs = self
            .db(move |s| s.list(&filter, limit.clamp(1, 50), 0))
            .await?;
        if docs.is_empty() {
            return Ok("知识库里没有符合条件的文档。".to_string());
        }
        let mut out = String::from("文档列表：\n");
        for doc in docs {
            out.push_str(&format!(
                "[kb:{}] {}（{}，{}，{} 块，{}）\n",
                doc.id.0,
                doc.title,
                doc.kind.label(),
                doc.origin.label(),
                doc.chunk_count,
                doc.status.label()
            ));
        }
        Ok(out)
    }

    /// `kb_add`：**只收文本，不收路径**（否则模型可以读任意本地文件）。
    ///
    /// 直接写，不弹确认（决策 O-07）：靠 `origin='ai'` 可筛选 + 一键清掉兜底。
    pub(crate) async fn knowledge_add_tool(
        &self,
        title: &str,
        text: &str,
    ) -> Result<String, CoreError> {
        let title: String = title.trim().chars().take(MAX_KB_TITLE_CHARS).collect();
        if title.is_empty() {
            return Err(CoreError::validation("标题不能为空"));
        }
        if text.chars().count() > MAX_KB_TEXT_CHARS {
            return Err(CoreError::validation(format!(
                "内容过长（上限 {MAX_KB_TEXT_CHARS} 字），请分成几条写入"
            )));
        }
        if text.trim().is_empty() {
            return Err(CoreError::validation("内容不能为空"));
        }

        let hash = sha256_bytes(text.as_bytes());
        let now = me_core::now_unix();
        let slug = format!("ai-{now}-{}", &hash[..12.min(hash.len())]);
        let doc = NewKnowledgeDoc {
            origin: KbOrigin::Ai,
            source_ref: slug,
            title: title.clone(),
            kind: KbKind::Text,
            mail_id: None,
            size_bytes: text.len() as u64,
            content_hash: hash,
        };
        let doc_id = self.db(move |s| s.upsert_document(&doc)).await?;
        // `Text` 来源没有原文件可回读，所以**当场分块**：否则重建时会没有内容。
        let chunks = split_structured(text);
        if chunks.is_empty() {
            return Err(CoreError::validation("内容没有可索引的文字"));
        }
        let doc_row = self
            .db(move |s| s.get(doc_id))
            .await?
            .ok_or_else(|| CoreError::storage("刚写入的文档读不回来"))?;
        let prepared = Prepared::Chunked;
        let _ = prepared;
        self.write_chunk_file(&doc_row, &chunks).await?;
        let links = self.match_document_entities(&chunks).await;
        self.db(move |s| s.replace_chunks(doc_id, &chunks)).await?;
        if let Some(links) = links {
            let _ = self.db(move |s| s.link_entities(doc_id, &links)).await;
        }
        self.ensure_worker();
        self.emit_knowledge_changed().await;
        Ok(format!("已写入知识库：[kb:{}] {title}", doc_id.0))
    }

    pub(crate) async fn knowledge_update_tool(
        &self,
        doc: KnowledgeDocId,
        title: Option<&str>,
        enabled: Option<bool>,
    ) -> Result<String, CoreError> {
        let exists = self.db(move |s| s.get(doc)).await?;
        if exists.is_none() {
            return Ok(format!("文档 {doc} 不存在"));
        }
        let title = title.map(|value| value.to_string());
        self.db(move |s| s.update_doc(doc, title.as_deref(), enabled))
            .await?;
        self.emit_knowledge_changed().await;
        Ok(format!("已更新 [kb:{}]", doc.0))
    }

    /// `kb_delete`：**只发请求**，用户在卡片上确认后才真删（照 `calendar_delete`）。
    pub(crate) async fn knowledge_delete_request(&self, doc: KnowledgeDocId) {
        let fetched = self.db(move |s| s.get(doc)).await.ok().flatten();
        let (title, chunks) = match fetched {
            Some(row) => (row.title, row.chunk_count),
            None => (format!("文档 {}", doc.0), 0),
        };
        self.emit(Event::KnowledgeDeleteRequested { doc, title, chunks })
            .await;
    }

    /// 详情页/工具共用的分页大小。
    pub(crate) fn read_chunk_page() -> u32 {
        READ_CHUNK_PAGE
    }
}

fn to_search_hit(scored: &ScoredHit) -> KbSearchHit {
    KbSearchHit {
        doc: scored.hit.doc,
        title: scored.hit.title.clone(),
        ord: scored.hit.ord,
        locator: scored.hit.locator.clone(),
        text: scored
            .hit
            .text
            .chars()
            .take(me_core::knowledge::MAX_CHUNK_CHARS)
            .collect(),
        score: scored.score,
    }
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn file_title(path: &Path) -> String {
    path.file_stem()
        .map(|value| value.to_string_lossy().to_string())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "未命名文档".to_string())
}
