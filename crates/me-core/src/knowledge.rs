//! 知识库领域层：文档模型、检索端口与全部纯函数。
//!
//! 这个模块**不做 IO、不碰数据库**，所以规则可以脱离运行时单测：
//! 分块、有效性判定、PDF 分批、费用估算、实体匹配、RRF 融合全在这里。
//!
//! 三条最容易写错、也最容易被当成「能跑就行」的约定，都在本模块里钉住：
//!
//! 1. **语块硬上限 480 字**。bge-small-zh 用中文逐字 tokenizer，600 字 ≈ 600 token，
//!    超过 512 的尾部会被**静默截断**——块尾文字永远不参与检索，且没有任何报错。
//! 2. **本地只做「装不下时的物理切分」**，语块边界与长度归模型（决策 O-19）。
//!    `split_structured` 只服务 md / txt / docx / 邮件 / AI 写入，不服务 PDF / PPTX。
//! 3. **查询与写入必须走同一个 `fts_fold`**（见 `crate::text`），否则中文 2 字词 0 命中。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::CoreError;
use crate::MessageId;
use crate::text::{fts_fold, is_cjk, is_common_punct};

// ---------------------------------------------------------------------------
// 标识
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct KnowledgeDocId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct KnowledgeChunkId(pub i64);

impl std::fmt::Display for KnowledgeDocId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "kb#{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// 枚举
// ---------------------------------------------------------------------------

/// 文档来源。`mail` 的正文不落 jsonl（决策 O-27），重建时直接从 `messages` 取。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KbOrigin {
    File,
    Mail,
    Ai,
}

impl KbOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            KbOrigin::File => "file",
            KbOrigin::Mail => "mail",
            KbOrigin::Ai => "ai",
        }
    }

    pub fn parse(value: &str) -> Option<KbOrigin> {
        match value {
            "file" => Some(KbOrigin::File),
            "mail" => Some(KbOrigin::Mail),
            "ai" => Some(KbOrigin::Ai),
            _ => None,
        }
    }

    /// 是否要把语块导出成 `kb/chunks/<doc_id>.jsonl`。
    ///
    /// 邮件不写：正文本来就在库里，5000 封邮件就是 5000 个几 KB 的小文件，
    /// 纯属重复，还会拖慢目录遍历与备份。
    pub fn writes_chunk_file(self) -> bool {
        matches!(self, KbOrigin::File | KbOrigin::Ai)
    }

    pub fn label(self) -> &'static str {
        match self {
            KbOrigin::File => "本地文件",
            KbOrigin::Mail => "邮件",
            KbOrigin::Ai => "AI 写入",
        }
    }
}

/// 文档类型。字符串与数据库 `kb_documents.kind` 一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KbKind {
    Md,
    Txt,
    Docx,
    Pdf,
    Pptx,
    Mail,
    Text,
}

impl KbKind {
    pub fn as_str(self) -> &'static str {
        match self {
            KbKind::Md => "md",
            KbKind::Txt => "txt",
            KbKind::Docx => "docx",
            KbKind::Pdf => "pdf",
            KbKind::Pptx => "pptx",
            KbKind::Mail => "mail",
            KbKind::Text => "text",
        }
    }

    pub fn parse(value: &str) -> Option<KbKind> {
        match value {
            "md" | "markdown" => Some(KbKind::Md),
            "txt" => Some(KbKind::Txt),
            "docx" => Some(KbKind::Docx),
            "pdf" => Some(KbKind::Pdf),
            "pptx" => Some(KbKind::Pptx),
            "mail" => Some(KbKind::Mail),
            "text" => Some(KbKind::Text),
            _ => None,
        }
    }

    /// 从扩展名判断；老格式（`doc` / `ppt`）先转换再导入，所以不在这里返回。
    pub fn from_extension(ext: &str) -> Option<KbKind> {
        match ext.to_ascii_lowercase().as_str() {
            "md" | "markdown" => Some(KbKind::Md),
            "txt" => Some(KbKind::Txt),
            "docx" => Some(KbKind::Docx),
            "pdf" => Some(KbKind::Pdf),
            "pptx" => Some(KbKind::Pptx),
            _ => None,
        }
    }

    /// 是否需要大模型参与（PDF / PPTX 的内容识别全量交给模型，决策 O-18）。
    pub fn needs_llm(self) -> bool {
        matches!(self, KbKind::Pdf | KbKind::Pptx)
    }

    pub fn label(self) -> &'static str {
        match self {
            KbKind::Md => "Markdown",
            KbKind::Txt => "文本",
            KbKind::Docx => "Word",
            KbKind::Pdf => "PDF",
            KbKind::Pptx => "PPT",
            KbKind::Mail => "邮件",
            KbKind::Text => "纯文本",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KbStatus {
    Pending,
    Extracted,
    Chunked,
    Embedded,
    Skipped,
    Failed,
}

impl KbStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            KbStatus::Pending => "pending",
            KbStatus::Extracted => "extracted",
            KbStatus::Chunked => "chunked",
            KbStatus::Embedded => "embedded",
            KbStatus::Skipped => "skipped",
            KbStatus::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Option<KbStatus> {
        match value {
            "pending" => Some(KbStatus::Pending),
            "extracted" => Some(KbStatus::Extracted),
            "chunked" => Some(KbStatus::Chunked),
            "embedded" => Some(KbStatus::Embedded),
            "skipped" => Some(KbStatus::Skipped),
            "failed" => Some(KbStatus::Failed),
            _ => None,
        }
    }

    /// 是否还需要 worker 处理（`skipped` / `failed` / `embedded` 都不再自动重跑）。
    pub fn is_open(self) -> bool {
        matches!(
            self,
            KbStatus::Pending | KbStatus::Extracted | KbStatus::Chunked
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            KbStatus::Pending => "排队中",
            KbStatus::Extracted => "已提取",
            KbStatus::Chunked => "已分块",
            KbStatus::Embedded => "已索引",
            KbStatus::Skipped => "未索引",
            KbStatus::Failed => "失败",
        }
    }
}

/// 摄入阶段。只用于进度事件与错误定位（「卡在哪一步」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KbStage {
    Queued,
    Extracting,
    Splitting,
    Chunking,
    Embedding,
    Done,
}

impl KbStage {
    pub fn as_str(self) -> &'static str {
        match self {
            KbStage::Queued => "queued",
            KbStage::Extracting => "extracting",
            KbStage::Splitting => "splitting",
            KbStage::Chunking => "chunking",
            KbStage::Embedding => "embedding",
            KbStage::Done => "done",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            KbStage::Queued => "排队",
            KbStage::Extracting => "提取内容",
            KbStage::Splitting => "拆分",
            KbStage::Chunking => "分块",
            KbStage::Embedding => "向量化",
            KbStage::Done => "完成",
        }
    }
}

/// 判定为无效内容的原因。**这是正常结果，不是错误**（决策 O-18）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkipReason {
    Empty,
    TooShort,
    Garbled,
    /// 扫描件且无可读内容。全量交给模型后本地不再产生它，
    /// 但模型可能回一句「该文档为扫描件」——那时用它回填，UI 文案统一。
    ScannedPdf,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::Empty => "empty",
            SkipReason::TooShort => "too_short",
            SkipReason::Garbled => "garbled",
            SkipReason::ScannedPdf => "scanned_pdf",
        }
    }

    pub fn parse(value: &str) -> Option<SkipReason> {
        match value {
            "empty" => Some(SkipReason::Empty),
            "too_short" => Some(SkipReason::TooShort),
            "garbled" => Some(SkipReason::Garbled),
            "scanned_pdf" => Some(SkipReason::ScannedPdf),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SkipReason::Empty => "内容为空",
            SkipReason::TooShort => "内容过短（可能是封面或空模板）",
            SkipReason::Garbled => "提取到的是乱码",
            SkipReason::ScannedPdf => "扫描件且没有可读文字",
        }
    }
}

/// 检索模式。模型没就绪时降级成 `fts_only`——**这是正常路径，不是异常**。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetrievalMode {
    /// 关键词 + 语义融合（模型就绪时的正常路径）。
    #[default]
    Hybrid,
    /// 只有关键词（模型未就绪）。
    FtsOnly,
}

impl RetrievalMode {
    pub fn label(self) -> &'static str {
        match self {
            RetrievalMode::Hybrid => "混合检索",
            RetrievalMode::FtsOnly => "仅关键词",
        }
    }
}

// ---------------------------------------------------------------------------
// 记录
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct KnowledgeDoc {
    pub id: KnowledgeDocId,
    pub origin: KbOrigin,
    pub source_ref: String,
    pub title: String,
    pub kind: KbKind,
    pub mail_id: Option<MessageId>,
    pub size_bytes: u64,
    pub content_hash: String,
    pub status: KbStatus,
    pub skip_reason: Option<SkipReason>,
    pub chunk_count: u32,
    pub char_count: u32,
    pub splitter: Option<String>,
    pub embed_model: Option<String>,
    pub embed_dim: u32,
    pub enabled: bool,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct KnowledgeChunk {
    pub id: KnowledgeChunkId,
    pub doc_id: KnowledgeDocId,
    pub ord: u32,
    pub text: String,
    pub char_count: u32,
    pub locator: Option<String>,
    pub embedded: bool,
}

/// 检索命中。`score` 由各通路自己填（FTS 用排名、KNN 用距离），
/// 融合后由 `rrf_fuse` 换成 RRF 分。
#[derive(Debug, Clone)]
pub struct ChunkHit {
    pub chunk: KnowledgeChunkId,
    pub doc: KnowledgeDocId,
    pub title: String,
    pub ord: u32,
    pub text: String,
    pub locator: Option<String>,
    pub score: f32,
}

#[derive(Debug, Clone)]
pub struct ScoredHit {
    pub hit: ChunkHit,
    pub score: f32,
}

/// 新建/更新一份文档时要写进去的字段。`id` 由存储层给。
#[derive(Debug, Clone)]
pub struct NewKnowledgeDoc {
    pub origin: KbOrigin,
    pub source_ref: String,
    pub title: String,
    pub kind: KbKind,
    pub mail_id: Option<MessageId>,
    pub size_bytes: u64,
    pub content_hash: String,
}

/// 一待写入的语块（模型或本地拆分的产物）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewChunk {
    pub text: String,
    pub locator: Option<String>,
}

impl NewChunk {
    pub fn new(text: impl Into<String>, locator: Option<String>) -> NewChunk {
        NewChunk {
            text: text.into(),
            locator,
        }
    }

    pub fn chars(&self) -> u32 {
        self.text.chars().count() as u32
    }
}

#[derive(Debug, Clone, Default)]
pub struct KbFilter {
    pub origin: Option<KbOrigin>,
    pub status: Option<KbStatus>,
    /// 标题模糊匹配（`LIKE`），不是全文检索。
    pub query: Option<String>,
}

/// 邮件入库的候选：只带「够不够格入库」要用的字段，不整份 `MessageSummary`。
///
/// 正文从 `body_path` 指向的文件读——库里的正文列已经是快照，文件才是全文。
#[derive(Debug, Clone)]
pub struct MailSeed {
    pub message: MessageId,
    pub subject: Option<String>,
    pub from_name: Option<String>,
    pub date: i64,
    pub body_path: String,
}

/// 检索命中（发给界面用）。`text` 已经截到 [`MAX_CHUNK_CHARS`]。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KbSearchHit {
    pub doc: KnowledgeDocId,
    pub title: String,
    pub ord: u32,
    pub locator: Option<String>,
    pub text: String,
    pub score: f32,
}

/// 知识库相关的设置快照。随 `Event::SettingsLoaded` 下发，界面不用回查 meta。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeSettings {
    /// 邮件自动入库容量（封）。`0` = 关闭。
    pub mail_capacity: u32,
    /// 已确认过「PDF/PPT 会发给模型并产生费用」。
    pub llm_ack: bool,
    /// 用户点过「暂不启用语义检索」。
    pub embed_declined: bool,
    /// 已就绪的嵌入模型 id；`None` = 只能用关键词检索。
    pub embed_model: Option<String>,
    /// 老格式转换后端：`soffice` / `office` / `none`。`None` = 还没探测过。
    pub converter: Option<String>,
}

/// 默认容量（封）。**全局总量，不是每账户**（决策 O-23）。
pub const DEFAULT_MAIL_CAPACITY: u32 = 200;

impl Default for KnowledgeSettings {
    fn default() -> Self {
        KnowledgeSettings {
            mail_capacity: DEFAULT_MAIL_CAPACITY,
            llm_ack: false,
            embed_declined: false,
            embed_model: None,
            converter: None,
        }
    }
}

impl KnowledgeSettings {
    /// 语义检索是否可用（模型已下载并校验通过）。
    pub fn semantic_ready(&self) -> bool {
        self.embed_model.is_some()
    }
}

/// 知识库的整体进度（UI 顶部那一行）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KnowledgeStats {
    /// 已启用自动入库的容量（封）；0 = 关闭。
    pub mail_capacity: u32,
    /// 窗口内已入库的邮件文档数。
    pub mail_docs: u32,
    /// 窗口内已完成向量化（或仅分块）的邮件文档数。
    pub mail_ready: u32,
    /// 窗口内判定为「不值得索引」的邮件数。
    pub mail_skipped: u32,
    /// 本地文件 + AI 写入的文档数。
    pub file_docs: u32,
    /// 全部语块数。
    pub chunks: u32,
    /// 各账户在窗口里占了多少封（`email` → 封数），用于「其中 work@ 150 封」。
    pub per_account: Vec<(String, u32)>,
}

// ---------------------------------------------------------------------------
// 端口
// ---------------------------------------------------------------------------

/// 嵌入器。**同步阻塞**——candle 推理本来就是阻塞的，由 me-sync 在
/// `spawn_blocking` 里调用，不把 tokio 带进领域层。
///
/// 索引与查询是**两个方法**：BGE 要给 query 加指令前缀、document 不加。
/// 合成一个 `embed()` 迟早会有人混用，召回质量下滑且不报错。
pub trait Embedder: Send + Sync {
    fn model_id(&self) -> &str;
    fn dim(&self) -> usize;
    /// 索引路径：不加指令前缀。
    fn embed_documents(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, CoreError>;
    /// 查询路径：加 [`BGE_QUERY_PREFIX`]。两者不能混用。
    fn embed_query(&self, query: &str) -> Result<Vec<f32>, CoreError>;
}

/// 知识库仓储。所有方法同步执行（与 me-store 的其他端口一致）。
///
/// **`delete` 的语义是深删**：同时清 `kb_chunks` / `kb_chunks_fts` /
/// `kbvec.vec_chunks`（决策 O-26）。调用方不需要知道有四张表。
pub trait KnowledgeRepo: Send + Sync {
    fn upsert_document(&self, doc: &NewKnowledgeDoc) -> Result<KnowledgeDocId, CoreError>;
    fn set_status(
        &self,
        id: KnowledgeDocId,
        status: KbStatus,
        error: Option<&str>,
    ) -> Result<(), CoreError>;
    fn set_skip_reason(&self, id: KnowledgeDocId, reason: SkipReason) -> Result<(), CoreError>;
    fn replace_chunks(&self, id: KnowledgeDocId, chunks: &[NewChunk]) -> Result<(), CoreError>;
    fn put_vectors(&self, items: &[(KnowledgeChunkId, Vec<f32>)]) -> Result<(), CoreError>;
    fn knn(&self, query: &[f32], limit: usize) -> Result<Vec<ChunkHit>, CoreError>;
    fn fts(&self, query: &str, limit: usize) -> Result<Vec<ChunkHit>, CoreError>;
    fn list(
        &self,
        filter: &KbFilter,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<KnowledgeDoc>, CoreError>;
    fn get(&self, id: KnowledgeDocId) -> Result<Option<KnowledgeDoc>, CoreError>;
    fn chunks(
        &self,
        id: KnowledgeDocId,
        from: u32,
        count: u32,
    ) -> Result<Vec<KnowledgeChunk>, CoreError>;
    /// **深删**（四步，顺序固定）。所有删除路径只准走它。
    fn delete(&self, id: KnowledgeDocId) -> Result<(), CoreError>;
    fn link_entities(&self, id: KnowledgeDocId, links: &[(i64, u32)]) -> Result<(), CoreError>;
    fn doc_entities(&self, id: KnowledgeDocId) -> Result<Vec<i64>, CoreError>;
    /// 取一批**待提取拆分**的文档（只取 `pending`，按 created_at）。
    ///
    /// 刻意**不含 `chunked`**：`chunked` 表示「已分块、还差向量」，
    /// 向量要等嵌入模型就绪。若把它也算进待处理队列，模型没就绪时
    /// 状态不会变，worker 会永远重复取同一份文档原地打转。
    fn pending_docs(&self, limit: u32) -> Result<Vec<KnowledgeDoc>, CoreError>;
    /// 取一批**已分块但还没向量**的文档（`chunked`，按 created_at）。
    ///
    /// 模型就绪后走这条路补向量；模型没就绪时它一条都不返回。
    fn docs_needing_vectors(&self, limit: u32) -> Result<Vec<KnowledgeDoc>, CoreError>;
    /// 取一份文档里还没向量化的语块。
    fn chunks_needing_vectors(
        &self,
        id: KnowledgeDocId,
        limit: u32,
    ) -> Result<Vec<KnowledgeChunk>, CoreError>;
    fn mark_embedded(&self, chunks: &[KnowledgeChunkId]) -> Result<(), CoreError>;
    /// 删邮件前必须先跑：把它名下的知识库文档逐份深删。返回删掉的文档数。
    fn purge_mail(&self, messages: &[MessageId]) -> Result<u32, CoreError>;
    /// 删账户前必须调：清掉该账户名下所有邮件文档（含向量与 FTS）。
    fn purge_mail_account(&self, account: crate::AccountId) -> Result<u32, CoreError>;
    /// 容量窗口内的消息 id（`body_path IS NOT NULL`，按 `date` 倒序，取前 N）。
    /// **全局总量，不是每账户**（决策 O-23）。
    fn mail_window(&self, capacity: u32) -> Result<Vec<MessageId>, CoreError>;
    /// 同上的带字段版本：入库时要标题、发件人、日期与正文路径。
    fn mail_seeds(&self, capacity: u32) -> Result<Vec<MailSeed>, CoreError>;
    /// 写回 `embed_model` / `embed_dim`（向量化完成时调用）。
    fn set_embedding(&self, id: KnowledgeDocId, model: &str, dim: u32) -> Result<(), CoreError>;
    /// 改标题 / 启用状态。`None` 表示不改（不要用空串表示「不改」）。
    fn update_doc(
        &self,
        id: KnowledgeDocId,
        title: Option<&str>,
        enabled: Option<bool>,
    ) -> Result<(), CoreError>;
    /// 超出容量窗口的邮件文档 id（`capacity = 0` 时返回全部邮件文档）。
    fn mail_docs_outside_window(&self, capacity: u32) -> Result<Vec<KnowledgeDocId>, CoreError>;
    fn stats(&self, capacity: u32) -> Result<KnowledgeStats, CoreError>;
    /// 把「标了 embedded=1 但向量库里没有向量」的块重置成 0（启动自检，O-22）。
    fn reset_orphan_embeddings(&self) -> Result<u64, CoreError>;
}

// ---------------------------------------------------------------------------
// 分块参数
// ---------------------------------------------------------------------------

/// 目标语块长度（**字**，不是 token）。
pub const TARGET_CHUNK_CHARS: usize = 400;
/// 硬上限。超了强制切。**不要改成 600**：bge 的 512 token 上限会静默截断尾部。
pub const MAX_CHUNK_CHARS: usize = 480;
/// 相邻块重叠，避免切断语义。
pub const CHUNK_OVERLAP_CHARS: usize = 60;

/// 单封邮件正文低于这个字数就不索引（空邮件、纯通知、回执）。
pub const MIN_MAIL_BODY_CHARS: usize = 200;

/// 本地判定的最短有效长度（去空白后）。封面页 / 空模板会落在这里。
pub const MIN_EFFECTIVE_CHARS: usize = 40;

/// `kb_add` 单次接受的文本上限。模型侧与 me-sync 侧各校验一次。
pub const MAX_KB_TEXT_CHARS: usize = 8000;
/// 文档标题上限。
pub const MAX_KB_TITLE_CHARS: usize = 120;

// ---------------------------------------------------------------------------
// 有效性判定
// ---------------------------------------------------------------------------

/// 判定本地提取出来的纯文本是否「包含信息」。**确定性规则，不用 LLM 判断**。
///
/// **只服务本地拆分路径**（md / txt / docx / 邮件 / AI 写入）。PDF 与 PPTX
/// 不走这里——它们的内容识别全量交给模型（决策 O-18），本地判一遍等于抢模型的活，
/// 而且规则判不准扫描件与图表页。
pub fn is_meaningful(text: &str) -> Result<(), SkipReason> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(SkipReason::Empty);
    }
    let total = trimmed.chars().count();
    let effective = trimmed.chars().filter(|c| !c.is_whitespace()).count();
    if effective < MIN_EFFECTIVE_CHARS {
        return Err(SkipReason::TooShort);
    }
    let meaningful = trimmed
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace() || is_cjk(*c) || is_common_punct(*c))
        .count();
    // 提取出占位符 / 乱码时，可识别字符占比会明显偏低。
    if (meaningful as f32) / (total as f32) < 0.30 {
        return Err(SkipReason::Garbled);
    }
    let control = trimmed
        .chars()
        .filter(|c| c.is_control() && *c != '\n' && *c != '\t')
        .count();
    if (control as f32) / (total as f32) > 0.05 {
        return Err(SkipReason::Garbled);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 本地结构拆分（md / txt / docx / 邮件 / AI 写入）
// ---------------------------------------------------------------------------

/// 按段落切分并累加到目标长度。**只在本地拆分路径用**。
///
/// 规则：段落内先保证不超 [`MAX_CHUNK_CHARS`]（超了按句末标点硬切），
/// 再贪心地往当前块里塞，塞到 [`TARGET_CHUNK_CHARS`] 为止；块与块之间留
/// [`CHUNK_OVERLAP_CHARS`] 字重叠。
///
/// `locator`：Markdown 用**标题路径**（`# 一级 > ## 二级`），其余用「第 N 段」。
/// 这是本地能给的最准定位；PDF / PPTX 的页码由模型写。
pub fn split_structured(text: &str) -> Vec<NewChunk> {
    let mut chunks: Vec<NewChunk> = Vec::new();
    let mut current = String::new();
    let mut heading = String::new();
    let mut para_index = 0u32;

    let flush = |current: &mut String, chunks: &mut Vec<NewChunk>, locator: &str| {
        let body = current.trim();
        if body.is_empty() {
            current.clear();
            return;
        }
        let text = body.to_string();
        chunks.push(NewChunk::new(
            text,
            if locator.is_empty() {
                None
            } else {
                Some(locator.to_string())
            },
        ));
        current.clear();
    };

    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Markdown 标题：更新路径，并且**始终新起一块**（标题不该挂在上一块的尾巴上）。
        if trimmed.starts_with('#') {
            let hashes = trimmed.chars().take_while(|c| *c == '#').count();
            if hashes <= 6 && trimmed.chars().nth(hashes) == Some(' ') {
                flush(&mut current, &mut chunks, &heading);
                let title = trimmed[hashes..].trim().to_string();
                let mut parts: Vec<String> = heading
                    .split(" > ")
                    .take(hashes.saturating_sub(1))
                    .map(|part| part.to_string())
                    .collect();
                parts.push(title);
                heading = parts.join(" > ");
                para_index = 0;
                continue;
            }
        }

        para_index += 1;
        let locator = if heading.is_empty() {
            format!("第 {para_index} 段")
        } else {
            heading.clone()
        };

        for piece in hard_split(trimmed, MAX_CHUNK_CHARS) {
            let piece_len = piece.chars().count();
            let current_len = current.chars().count();
            if current_len > 0 && current_len + 1 + piece_len > TARGET_CHUNK_CHARS {
                flush(&mut current, &mut chunks, &locator);
                // 重叠：把上一块尾部带过来，代价是少量重复，收益是不切断语义。
                if let Some(previous) = chunks.last() {
                    let tail = tail_chars(&previous.text, CHUNK_OVERLAP_CHARS);
                    if tail.chars().count() + piece_len <= MAX_CHUNK_CHARS {
                        current.push_str(&tail);
                        current.push('\n');
                    }
                }
            }
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(&piece);
        }
    }
    flush(&mut current, &mut chunks, &heading);
    chunks
}

/// 取字符串末尾 `n` 个字符（按字符而不是字节，避免切坏中文）。
fn tail_chars(text: &str, n: usize) -> String {
    let total = text.chars().count();
    if total <= n {
        return text.to_string();
    }
    text.chars().skip(total - n).collect()
}

/// 把超长文本切成不超过 `max` 字的片段：优先在句末标点处断开。
fn hard_split(text: &str, max: usize) -> Vec<String> {
    if text.chars().count() <= max {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut len = 0usize;
    for ch in text.chars() {
        current.push(ch);
        len += 1;
        if len >= max {
            // 往回退到一个句末标点，退不到就硬切（宁可断句也不超上限）。
            let cut = last_sentence_break(&current).unwrap_or(current.len());
            let (head, tail) = current.split_at(cut);
            if !head.trim().is_empty() {
                out.push(head.trim().to_string());
            }
            let rest = tail.to_string();
            len = rest.chars().count();
            current = rest;
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

/// `current` 里最后一个「可以作为断点」的字节位置（断点含标点本身）。
fn last_sentence_break(text: &str) -> Option<usize> {
    let mut best = None;
    for (idx, ch) in text.char_indices() {
        if matches!(ch, '。' | '！' | '？' | '；' | '\n' | '.' | '!' | '?' | ';') {
            best = Some(idx + ch.len_utf8());
        }
    }
    // 断点太靠前会让块变得很短，收益不大；退化为硬切。
    let cut = best?;
    if cut < text.len() / 3 {
        None
    } else {
        Some(cut)
    }
}

// ---------------------------------------------------------------------------
// PDF 分批与费用估算
// ---------------------------------------------------------------------------

/// 单批页数上限（Anthropic 硬限 100 页）。
pub const PDF_MAX_BATCH_PAGES: u32 = 100;
/// 请求体上限 32 MB（base64 之后）。
pub const PDF_MAX_REQUEST_BYTES: u64 = 32 << 20;
/// 单批原文件上限 = 32 MB ÷ 4/3，取 24 MB 给 JSON 外壳留余量（决策 O-28）。
pub const PDF_MAX_RAW_BYTES: u64 = 24 << 20;
/// 本地切分上限；超过它才拒绝导入。
pub const PDF_MAX_LOCAL_BYTES: u64 = 512 << 20;
/// 其它类型的大小上限。
pub const OTHER_MAX_BYTES: u64 = 50 << 20;
/// PPTX 文本分批阈值（**请求体积**，不是语块长度）。
pub const PPTX_TEXT_BATCH_CHARS: usize = 4000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PdfBatch {
    /// 1 基的第一页。
    pub first_page: u32,
    pub pages: u32,
}

impl PdfBatch {
    pub fn last_page(&self) -> u32 {
        self.first_page + self.pages - 1
    }
}

/// 规划 PDF 的批次。**只在装不下时才切**：不超过 100 页且不超过 24 MB 时返回单批。
///
/// 体积先于页数成为约束时的处理：按「平均每页字节」反推每批页数，
/// 退化成更小的批（扫描件常见）。判断的是**原文件字节**，
/// 24 MB 这个数已经是「32 MB 请求体 ÷ 4/3」的结果，别在这里再乘一次 1.34。
pub fn plan_pdf_batches(total_pages: u32, file_bytes: u64) -> Vec<PdfBatch> {
    if total_pages == 0 {
        return Vec::new();
    }
    let per_batch = pages_per_batch(total_pages, file_bytes);
    let mut out = Vec::new();
    let mut first = 1u32;
    while first <= total_pages {
        let pages = per_batch.min(total_pages - first + 1);
        out.push(PdfBatch {
            first_page: first,
            pages,
        });
        first += pages;
    }
    out
}

fn pages_per_batch(total_pages: u32, file_bytes: u64) -> u32 {
    let by_pages = PDF_MAX_BATCH_PAGES;
    if file_bytes <= PDF_MAX_RAW_BYTES || total_pages == 0 {
        return by_pages;
    }
    let avg_page = file_bytes / u64::from(total_pages.max(1));
    if avg_page == 0 {
        return by_pages;
    }
    let by_size = (PDF_MAX_RAW_BYTES / avg_page).max(1);
    let by_size = u32::try_from(by_size).unwrap_or(by_pages);
    by_size.min(by_pages)
}

/// 把 PPTX 的纯文本按段落边界切成若干批（每批 ≤ [`PPTX_TEXT_BATCH_CHARS`]）。
///
/// 切点落在段落边界；**不在句中切**。返回的每批会带上 `part/total` 交给模型。
pub fn split_text_batches(text: &str) -> Vec<String> {
    if text.chars().count() <= PPTX_TEXT_BATCH_CHARS {
        return vec![text.to_string()];
    }
    let mut batches = Vec::new();
    let mut current = String::new();
    let mut len = 0usize;
    for line in text.lines() {
        let line_len = line.chars().count();
        if len > 0 && len + 1 + line_len > PPTX_TEXT_BATCH_CHARS {
            batches.push(current.trim_end().to_string());
            current.clear();
            len = 0;
        }
        if line_len > PPTX_TEXT_BATCH_CHARS {
            // 单段就超阈值：只能硬切，否则这一批永远装不下。
            for piece in hard_split(line, PPTX_TEXT_BATCH_CHARS) {
                if !current.is_empty() {
                    batches.push(current.trim_end().to_string());
                    current.clear();
                    len = 0;
                }
                batches.push(piece);
            }
            continue;
        }
        if !current.is_empty() {
            current.push('\n');
            len += 1;
        }
        current.push_str(line);
        len += line_len;
    }
    if !current.trim().is_empty() {
        batches.push(current.trim().to_string());
    }
    if batches.is_empty() {
        batches.push(String::new());
    }
    batches
}

/// 每页 token 的粗估区间。只用来给用户一个量级，**不当计费依据**。
pub const TOKENS_PER_PAGE_LOW: u32 = 1500;
pub const TOKENS_PER_PAGE_HIGH: u32 = 3000;

/// 按页数给 token 区间。给区间而不是一个数：不同模型、不同版面差异很大，
/// 假精确比区间更误导。
pub fn estimate_pdf_tokens(pages: u32) -> (u32, u32) {
    (
        pages.saturating_mul(TOKENS_PER_PAGE_LOW),
        pages.saturating_mul(TOKENS_PER_PAGE_HIGH),
    )
}

/// 内置参考价（美元 / 百万 token，(输入, 输出)）。**价格随时会变，只作量级参考。**
///
/// 按模型名**前缀**匹配；匹配不到的模型不显示美元数，只说 token 区间 +
/// 「以服务商账单为准」。宁可少说，不要瞎说。
pub fn reference_price_per_mtok(model: &str) -> Option<(f64, f64)> {
    let name = model.to_ascii_lowercase();
    let table: [(&str, f64, f64); 6] = [
        ("claude-opus", 15.0, 75.0),
        ("claude-sonnet", 3.0, 15.0),
        ("claude-haiku", 0.8, 4.0),
        ("gpt-4o-mini", 0.15, 0.6),
        ("gpt-4o", 2.5, 10.0),
        ("gpt-4.1", 2.0, 8.0),
    ];
    table
        .iter()
        .find(|(prefix, _, _)| name.contains(prefix))
        .map(|(_, input, output)| (*input, *output))
}

/// 估算一次 PDF/PPTX 拆分的美元区间。输出按「每页约 1/4 页字数的 JSON」粗估。
pub fn estimate_split_cost_usd(
    model: &str,
    tokens_low: u32,
    tokens_high: u32,
) -> Option<(f64, f64)> {
    let (input_price, output_price) = reference_price_per_mtok(model)?;
    let low = f64::from(tokens_low) / 1_000_000.0 * input_price
        + f64::from(tokens_low) / 4.0 / 1_000_000.0 * output_price;
    let high = f64::from(tokens_high) / 1_000_000.0 * input_price
        + f64::from(tokens_high) / 4.0 / 1_000_000.0 * output_price;
    Some((low, high))
}

// ---------------------------------------------------------------------------
// 拆分请求（PDF / PPTX 共用一条通道）
// ---------------------------------------------------------------------------

/// 送进模型「拆语块」的两种输入（决策 O-25）。
/// **PDF 与 PPTX 走同一个方法**，只是载荷不同：都不走对话历史、不走工具循环。
#[derive(Debug, Clone)]
pub enum SplitInput {
    /// PDF 原始字节。本地已按页切好（或整份未切）。
    /// `first_page` 是批偏移，用于把模型返回的「第 N 页」换算成全局页码。
    Pdf { bytes: Vec<u8>, first_page: u32 },
    /// 纯文本（PPTX 的 `<a:t>` 机械抽取结果）。分批时给 part/total。
    Text { text: String, part: u32, total: u32 },
}

/// 拆分请求。**带上服务商参数与凭证**：`AiProvider` 实例本身不持有配置
/// （一次请求一套参数），不带的话 provider 连往哪发都不知道。
#[derive(Debug)]
pub struct DocumentSplitRequest {
    pub config: crate::ai::AiProviderConfig,
    /// 回环 OpenAI 兼容端点可以为 `None`。
    pub credential: Option<crate::ai::AiCredential>,
    pub input: SplitInput,
    pub model: String,
    /// 单批语块条数上限：防止模型一次吐几千块把 `max_tokens` 打满、JSON 截断成废串。
    pub max_chunks: u32,
}

#[derive(Debug, Clone)]
pub struct SplitOutcome {
    pub chunks: Vec<SplitChunk>,
    /// 模型侧是否有截断迹象（`finish_reason = length`）：
    /// 用来决定「不重试也别假装成功」。
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct SplitChunk {
    pub text: String,
    /// PDF 是「第 N 页」，PPTX 是「第 N 张幻灯片」。由模型写，本地只做批偏移换算。
    pub locator: Option<String>,
}

/// 单批最多要多少块。400 页的 PDF 按 100 页一批、每页 3 块算，一批约 300 块；
/// 上限放到 400 是为了留余量，同时挡住「一次吐几千块」。
pub const MAX_SPLIT_CHUNKS: u32 = 400;

/// 提示词：读文档 + 切语块，一次调用完成（省掉第二次调用）。
///
/// **长度写 300~450，不写 600**：模型产出的语块会被直接送进嵌入，
/// 超过 480 字就撞上 bge 的 512 token 上限被静默截断（决策 O-21）。
pub const KB_PDF_SPLIT_PROMPT: &str = "读这份文档，把它整理成若干语义完整的知识片段。
要求：
- 每片 300~450 字，覆盖全文，不遗漏、不重复
- 不新增内容、不改写原意，忠实于原文
- 表格与图表用文字概述其结论
- 保留章节标题作为片段开头
只输出 JSON：{\"chunks\":[{\"text\":\"...\",\"locator\":\"第 N 页\"}]}，不要任何解释。";

/// PPTX 版提示词。位次信息必须给到模型——分批后每批的开头/结尾可能不完整。
pub const KB_TEXT_SPLIT_PROMPT: &str =
    "这是一份演示文稿的全部文字（按幻灯片顺序机械抽取，可能缺少版面信息）。
把它整理成若干语义完整的知识片段。
要求：
- 每片 300~450 字，覆盖全文，不遗漏、不重复
- 不新增内容、不改写原意；表格与图表用文字概述其结论
- 保留幻灯片标题作为片段开头
- 若这是分批输入的第 i/N 批，开头或结尾可能不完整，按你看到的内容拆分即可
只输出 JSON：{\"chunks\":[{\"text\":\"...\",\"locator\":\"第 N 张幻灯片\"}]}，不要任何解释。";

/// BGE v1.5 官方建议的查询指令前缀（**只给 query，document 不加**）。
pub const BGE_QUERY_PREFIX: &str = "为这个句子生成表示以用于检索相关文章：";

/// 嵌入模型目录名与维度。换模型必须整库重建（决策 O-10）。
pub const EMBED_MODEL_ID: &str = "bge-small-zh-v1.5";
pub const EMBED_DIM: usize = 512;

/// 默认下载源（ModelScope）。`meta.kb_embed_model_url` 存的是**这个前缀**，
/// 允许用户改成内网镜像；三个文件按 `<base>&FilePath=<name>` 拼。
pub const EMBED_MODEL_BASE_URL: &str =
    "https://modelscope.cn/api/v1/models/AI-ModelScope/bge-small-zh-v1.5/repo?Revision=master";

/// 模型文件大小与 sha256（取自 ModelScope 的仓库文件清单）。
/// **只校验 `model.safetensors`**：它是 95 MB 的主体，另外两个是小配置，
/// 下载后能解析就算通过（配置被篡改也跑不起来，会解析失败）。
pub const EMBED_MODEL_BYTES: u64 = 95_827_648;
pub const EMBED_MODEL_SHA256: &str =
    "354763b9b1357bc9c44f62c6be2276321081ed2567773608c0d0785b61d5a026";

/// 下载并校验通过后放在 `models/<id>/` 里的文件。
pub const EMBED_MODEL_FILES: [&str; 3] = ["model.safetensors", "tokenizer.json", "config.json"];

// ---------------------------------------------------------------------------
// 实体匹配与 RRF
// ---------------------------------------------------------------------------

/// 在文本里找**已知**实体（文档 → 已知实体，不是从文档发现新实体）。
///
/// 用简单包含匹配：实体几百个、语块几万条，release 下是百毫秒级，
/// 不值得为它引入 Aho-Corasick。
///
/// `aliases` 必须**按长度降序**排好再传进来，否则中文里「张三」会先命中
/// 「张三丰」的一部分。这个函数内部不会再排一次（调用方通常复用一个列表）。
pub fn match_entities(text_lower: &str, aliases: &[(String, i64)]) -> Vec<i64> {
    let mut hits = Vec::new();
    for (alias, entity) in aliases {
        if alias.is_empty() {
            continue;
        }
        if text_lower.contains(alias.as_str()) && !hits.contains(entity) {
            hits.push(*entity);
        }
    }
    hits
}

/// RRF 融合：只看排名不看分数，天然规避「BM25 分数与 cosine 距离量纲不同」。
///
/// `k` 越小越偏重头部（top-1 很重），越大越偏长尾。**60 是文献默认值，不是定论**——
/// 用评测集跑 k ∈ {10, 30, 60, 100} 再定。
pub fn rrf_fuse(fts: &[ChunkHit], vector: &[ChunkHit], k: f32, limit: usize) -> Vec<ScoredHit> {
    let mut scores: HashMap<KnowledgeChunkId, f32> = HashMap::new();
    let mut hits: HashMap<KnowledgeChunkId, ChunkHit> = HashMap::new();
    for (rank, hit) in fts.iter().enumerate() {
        *scores.entry(hit.chunk).or_default() += 1.0 / (k + rank as f32 + 1.0);
        hits.entry(hit.chunk).or_insert_with(|| hit.clone());
    }
    for (rank, hit) in vector.iter().enumerate() {
        *scores.entry(hit.chunk).or_default() += 1.0 / (k + rank as f32 + 1.0);
        hits.entry(hit.chunk).or_insert_with(|| hit.clone());
    }
    let mut fused: Vec<ScoredHit> = scores
        .into_iter()
        .filter_map(|(chunk, score)| hits.remove(&chunk).map(|hit| ScoredHit { hit, score }))
        .collect();
    fused.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    diversify_by_document(&mut fused, limit);
    fused
}

/// 文档级分散：同一份文档在 top-K 里最多占几个位置。
///
/// **为什么必须做**：融合后的排名常常被一篇文档占满——实测「设备故障响应的
/// 时限与服务级别如何对应」这一问，top-8 里有 6 块来自《服务手册》，
/// 而真正需要《采购合同》（质保与违约那一条）被挤了出去。
/// 跨文档的问题就此答错：模型只看得见一篇文档的视角。
///
/// 做法是**先按分数取，但每份文档最多留 [`PER_DOC_CAP`] 块**，
/// 被挤掉的块按分数排在后面补位——所以 top-K 的长度不变，
/// 只是顺序从「纯按分」变成「每篇文档先保一块，再按分补」。
///
/// 上限不是 1：同一份文档的相邻块常常是同一段话的延续
/// （例如合同第二条、第三条），完全打散会让上下文支离破碎。
pub const PER_DOC_CAP: usize = 3;

fn diversify_by_document(fused: &mut Vec<ScoredHit>, limit: usize) {
    if fused.len() <= limit {
        return;
    }
    // 分组：按文档把已排序的块串起来，每篇文档内部保持分数序。
    let mut groups: Vec<(i64, Vec<ScoredHit>)> = Vec::new();
    let mut index_by_doc: HashMap<i64, usize> = HashMap::new();
    for hit in fused.drain(..) {
        match index_by_doc.get(&hit.hit.doc.0) {
            Some(index) => groups[*index].1.push(hit),
            None => {
                index_by_doc.insert(hit.hit.doc.0, groups.len());
                groups.push((hit.hit.doc.0, vec![hit]));
            }
        }
    }

    // 轮转取：第 1 轮每篇文档各取 1 块，第 2 轮各取第 2 块……
    // 到 [`PER_DOC_CAP`] 轮为止。这样每一轮都把「文档覆盖面」最大化，
    // 而不是让某一篇高分文档先占满所有位置。
    let mut kept: Vec<ScoredHit> = Vec::with_capacity(limit);
    for round in 0..PER_DOC_CAP {
        for (_, group) in groups.iter() {
            if kept.len() >= limit {
                break;
            }
            if let Some(hit) = group.get(round) {
                kept.push(hit.clone());
            }
        }
    }
    // 轮转取完还有空位（文档数少 / 块不够），按分数补满——
    // top-K 的长度不能变，否则上下文预算就白白缩水了。
    if kept.len() < limit {
        let taken: std::collections::HashSet<KnowledgeChunkId> =
            kept.iter().map(|hit| hit.hit.chunk).collect();
        let mut rest: Vec<ScoredHit> = Vec::new();
        for hit in groups.iter().flat_map(|(_, group)| group.iter()) {
            if !taken.contains(&hit.hit.chunk) {
                rest.push(hit.clone());
            }
        }
        for hit in rest {
            if kept.len() >= limit {
                break;
            }
            kept.push(hit);
        }
    }
    *fused = kept;
}

/// 本轮检索的上下文块数上限与总字数预算（复用 `build_chat_request` 的字符预算）。
pub const RAG_TOP_K: usize = 8;
pub const RAG_MAX_CONTEXT_CHARS: usize = 5000;
/// 单侧的候选数（FTS 与 KNN 各取这么多再融合）。
pub const RAG_CANDIDATES: usize = 40;
/// RRF 的默认 k。
pub const RRF_K: f32 = 60.0;

/// 把检索结果拼成给模型看的上下文。每块单独截到 [`MAX_CHUNK_CHARS`]，
/// 总预算 [`RAG_MAX_CONTEXT_CHARS`]，超了少取几条（**不是截断最后一条**——
/// 半块内容比少一块更容易误导模型）。
pub fn render_rag_context(hits: &[ScoredHit]) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for (index, scored) in hits.iter().enumerate().take(RAG_TOP_K) {
        let text: String = scored
            .hit
            .text
            .chars()
            .take(MAX_CHUNK_CHARS)
            .collect::<String>()
            .trim()
            .to_string();
        if text.is_empty() {
            continue;
        }
        let locator = scored
            .hit
            .locator
            .as_deref()
            .map(|value| format!(" · {value}"))
            .unwrap_or_default();
        let header = format!("[kb:{}] {}{locator}\n", scored.hit.doc.0, scored.hit.title);
        let cost = header.chars().count() + text.chars().count() + 2;
        if used + cost > RAG_MAX_CONTEXT_CHARS {
            break;
        }
        used += cost;
        out.push_str(&header);
        out.push_str(&text);
        out.push_str("\n\n");
        if index + 1 >= RAG_TOP_K {
            break;
        }
    }
    out.trim_end().to_string()
}

/// 把 `fts_fold` 暴露给存储层（写入 FTS 时用），避免两处各写一份口径。
pub fn fold_for_fts(text: &str) -> String {
    fts_fold(text)
}

/// 把模型返回的文本解析成语块列表。
///
/// 模型经常会把 JSON 包在 ```json 代码块里，或者前面加一句「好的，以下是…」，
/// 所以取**第一个 `{` 到最后一个 `}`**，不做严格前缀匹配。
///
/// 解析失败时返回 `Err(原文前 200 字)`：PDF 单批很贵，失败不重试，
/// 这段原文是排查的唯一线索，要能带进 `kb_documents.error`。
pub fn parse_split_response(text: &str, max_chunks: u32) -> Result<Vec<SplitChunk>, String> {
    let start = text.find('{').ok_or_else(|| snippet(text))?;
    let end = text.rfind('}').ok_or_else(|| snippet(text))?;
    if end <= start {
        return Err(snippet(text));
    }
    let slice = &text[start..=end];
    let value: serde_json::Value = serde_json::from_str(slice).map_err(|_| snippet(text))?;
    let items = value
        .get("chunks")
        .and_then(|chunks| chunks.as_array())
        .ok_or_else(|| snippet(text))?;

    let mut out = Vec::new();
    for item in items {
        if out.len() >= max_chunks as usize {
            break;
        }
        let body = item
            .get("text")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .trim();
        if body.is_empty() {
            continue;
        }
        // 模型偶尔超长。这里截到硬上限：块尾被 bge 静默截断比少几个字更糟
        // （截断不报错，而且那块文字永远不参与检索）。
        let body: String = body.chars().take(MAX_CHUNK_CHARS).collect();
        let locator = item
            .get("locator")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.chars().take(60).collect::<String>());
        out.push(SplitChunk {
            text: body,
            locator,
        });
    }
    Ok(out)
}

fn snippet(text: &str) -> String {
    text.trim().chars().take(200).collect()
}

/// 把模型写的「第 N 页」换算成全局页码。批偏移只加在数字上，不破坏原有措辞。
///
/// `first_page` 是这一批的第一页（1 基）。批内第 1 页 → 全局 `first_page`。
pub fn shift_locator(locator: &str, first_page: u32) -> String {
    if first_page <= 1 {
        return locator.to_string();
    }
    let digits: String = locator
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let Ok(page) = digits.parse::<u32>() else {
        return locator.to_string();
    };
    let shifted = page.saturating_sub(1) + first_page;
    // 只替换第一段数字，其余（「第」「页」）原样保留。
    let mut out = String::with_capacity(locator.len() + 4);
    let mut replaced = false;
    for ch in locator.chars() {
        if ch.is_ascii_digit() {
            if !replaced {
                out.push_str(&shifted.to_string());
                replaced = true;
            }
            continue;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_short_are_skipped() {
        assert_eq!(is_meaningful("   \n\t "), Err(SkipReason::Empty));
        assert_eq!(is_meaningful("短标题"), Err(SkipReason::TooShort));
    }

    #[test]
    fn normal_text_passes() {
        let text = "这是一段正常的会议纪要，包含足够的字数用于通过有效性判定。\
                    我们讨论了预算、合同与人员安排，并记录了下周的交付计划。";
        assert!(is_meaningful(text).is_ok());
    }

    #[test]
    fn garbled_text_is_rejected() {
        // 大量不可识别字符（私用区），可识别字符占比极低。
        let garbled: String = std::iter::repeat_n('\u{E000}', 120).collect();
        assert_eq!(is_meaningful(&garbled), Err(SkipReason::Garbled));
    }

    #[test]
    fn control_characters_are_rejected() {
        let mut text = String::new();
        for _ in 0..60 {
            text.push('\u{1}');
        }
        text.push_str("正常文字正常文字正常文字正常文字正常文字正常文字");
        assert_eq!(is_meaningful(&text), Err(SkipReason::Garbled));
    }

    #[test]
    fn split_respects_hard_limit() {
        let long = "中文".repeat(400); // 800 字
        let chunks = split_structured(&long);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(
                chunk.chars() as usize <= MAX_CHUNK_CHARS,
                "块长 {} 超过上限",
                chunk.chars()
            );
        }
    }

    #[test]
    fn split_marks_markdown_headings_as_locator() {
        let text =
            "# 采购合同\n\n甲方与乙方就设备采购达成一致。\n\n## 付款\n\n货到验收后 30 日内付款。";
        let chunks = split_structured(text);
        assert!(
            chunks
                .iter()
                .any(|c| c.locator.as_deref() == Some("采购合同"))
        );
        assert!(
            chunks
                .iter()
                .any(|c| c.locator.as_deref() == Some("采购合同 > 付款"))
        );
    }

    #[test]
    fn split_keeps_short_text_in_one_chunk() {
        let chunks = split_structured("只有一段话。");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "只有一段话。");
    }

    #[test]
    fn pdf_batches_small_file_is_single_batch() {
        let batches = plan_pdf_batches(50, 10 << 20);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].first_page, 1);
        assert_eq!(batches[0].pages, 50);
    }

    #[test]
    fn pdf_batches_by_pages_when_size_is_fine() {
        let batches = plan_pdf_batches(300, 5 << 20);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].last_page(), 100);
        assert_eq!(batches[2].first_page, 201);
        assert_eq!(batches[2].last_page(), 300);
    }

    #[test]
    fn pdf_batches_by_size_for_scan() {
        // 300 页 / 200 MB：平均每页约 0.67 MB，24 MB 只装得下 36 页。
        let batches = plan_pdf_batches(300, 200 << 20);
        assert!(batches.len() >= 8, "实得 {} 批", batches.len());
        let pages: u32 = batches.iter().map(|b| b.pages).sum();
        assert_eq!(pages, 300);
    }

    #[test]
    fn pdf_batches_are_contiguous() {
        let batches = plan_pdf_batches(1234, 300 << 20);
        let mut next = 1;
        for batch in &batches {
            assert_eq!(batch.first_page, next);
            next += batch.pages;
        }
        assert_eq!(next, 1235);
    }

    #[test]
    fn token_estimate_is_a_range() {
        let (low, high) = estimate_pdf_tokens(300);
        assert_eq!(low, 450_000);
        assert_eq!(high, 900_000);
        assert!(low < high);
    }

    #[test]
    fn price_table_matches_by_prefix_and_unknown_is_none() {
        assert!(reference_price_per_mtok("claude-sonnet-4-5").is_some());
        assert!(reference_price_per_mtok("agnes-2.5-flash").is_none());
        assert!(estimate_split_cost_usd("agnes-2.5-flash", 1000, 2000).is_none());
        let (low, high) = estimate_split_cost_usd("claude-sonnet-4-5", 1_000_000, 2_000_000)
            .expect("内置价表应命中 sonnet");
        assert!(low < high);
    }

    #[test]
    fn text_batches_split_on_line_boundaries() {
        let mut text = String::new();
        for index in 0..500 {
            text.push_str(&format!(
                "第 {index} 段会议纪要内容，包含预算与合同信息。\n"
            ));
        }
        let batches = split_text_batches(&text);
        assert!(batches.len() > 1);
        for batch in &batches {
            assert!(batch.chars().count() <= PPTX_TEXT_BATCH_CHARS);
        }
    }

    #[test]
    fn entity_matching_prefers_longer_aliases() {
        // 调用方保证按长度降序：先「张三丰」，再「张三」。
        let aliases = vec![("张三丰".to_string(), 2i64), ("张三".to_string(), 1i64)];
        let hits = match_entities("会议记录：张三丰到场", &aliases);
        assert_eq!(hits, vec![2, 1]);
        let hits = match_entities("会议记录：张三到场", &aliases);
        assert_eq!(hits, vec![1]);
    }

    fn hit(chunk: i64, doc: i64, ord: u32) -> ChunkHit {
        ChunkHit {
            chunk: KnowledgeChunkId(chunk),
            doc: KnowledgeDocId(doc),
            title: format!("文档 {doc}"),
            ord,
            text: format!("块 {chunk}"),
            locator: None,
            score: 0.0,
        }
    }

    #[test]
    fn rrf_prefers_items_ranked_by_both_paths() {
        let fts = vec![hit(1, 10, 0), hit(2, 10, 1)];
        let vec = vec![hit(2, 10, 1), hit(3, 10, 2)];
        let fused = rrf_fuse(&fts, &vec, RRF_K, 8);
        assert_eq!(fused[0].hit.chunk, KnowledgeChunkId(2));
        assert_eq!(fused.len(), 3);
    }

    #[test]
    fn rag_context_respects_budget() {
        let mut hits = Vec::new();
        for index in 0..20 {
            let mut item = hit(index as i64, index as i64, 0);
            item.text = "内容".repeat(200);
            hits.push(ScoredHit {
                hit: item,
                score: 1.0,
            });
        }
        let context = render_rag_context(&hits);
        assert!(context.chars().count() <= RAG_MAX_CONTEXT_CHARS);
        assert!(context.contains("[kb:0]"));
    }

    #[test]
    fn enum_round_trips() {
        for origin in [KbOrigin::File, KbOrigin::Mail, KbOrigin::Ai] {
            assert_eq!(KbOrigin::parse(origin.as_str()), Some(origin));
        }
        for status in [
            KbStatus::Pending,
            KbStatus::Extracted,
            KbStatus::Chunked,
            KbStatus::Embedded,
            KbStatus::Skipped,
            KbStatus::Failed,
        ] {
            assert_eq!(KbStatus::parse(status.as_str()), Some(status));
        }
        for kind in [
            KbKind::Md,
            KbKind::Txt,
            KbKind::Docx,
            KbKind::Pdf,
            KbKind::Pptx,
            KbKind::Mail,
            KbKind::Text,
        ] {
            assert_eq!(KbKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(
            SkipReason::parse("scanned_pdf"),
            Some(SkipReason::ScannedPdf)
        );
    }

    #[test]
    fn mail_origin_does_not_write_chunk_files() {
        assert!(!KbOrigin::Mail.writes_chunk_file());
        assert!(KbOrigin::File.writes_chunk_file());
        assert!(KbOrigin::Ai.writes_chunk_file());
    }

    #[test]
    fn split_response_parses_fenced_json() {
        let raw = "好的，以下是结果：\n```json\n{\"chunks\":[{\"text\":\"第一片\",\"locator\":\"第 1 页\"},\
                   {\"text\":\"第二片\",\"locator\":\"第 2 页\"}]}\n```";
        let chunks = parse_split_response(raw, 10).expect("应能解析");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "第一片");
        assert_eq!(chunks[1].locator.as_deref(), Some("第 2 页"));
    }

    #[test]
    fn split_response_empty_array_is_not_an_error() {
        // 模型判断「这份文档没有可读内容」时会返回空数组：这是 skipped，不是失败。
        let chunks = parse_split_response("{\"chunks\":[]}", 10).expect("空数组要当正常结果");
        assert!(chunks.is_empty());
    }

    #[test]
    fn split_response_rejects_garbage_with_snippet() {
        let err = parse_split_response("抱歉，我无法处理这份文档。", 10).unwrap_err();
        assert!(err.contains("无法处理"), "错误里要带原文片段：{err}");
        assert!(err.chars().count() <= 200);
    }

    #[test]
    fn split_response_caps_count_and_length() {
        let long: String = "长".repeat(MAX_CHUNK_CHARS + 200);
        let raw = format!(
            "{{\"chunks\":[{{\"text\":\"{long}\"}},{{\"text\":\"二\"}},{{\"text\":\"三\"}}]}}"
        );
        let chunks = parse_split_response(&raw, 2).expect("解析");
        assert_eq!(chunks.len(), 2, "超过 max_chunks 的部分要丢掉");
        assert_eq!(
            chunks[0].text.chars().count(),
            MAX_CHUNK_CHARS,
            "超长块必须截到硬上限"
        );
    }

    #[test]
    fn locator_shift_adds_batch_offset_only_to_digits() {
        assert_eq!(shift_locator("第 3 页", 101), "第 103 页");
        assert_eq!(shift_locator("第 1 页", 1), "第 1 页");
        assert_eq!(shift_locator("附件", 50), "附件");
        assert_eq!(shift_locator("Page 2", 11), "Page 12");
    }

    /// 文档级分散：同一份文档不能吃掉全部 top-K。
    ///
    /// 这条锁住的是**跨文档查询**的正确性：不分散的话，一篇文档会把
    /// top-8 占满，另一篇真正需要的文档被挤出去，模型只看得见一个视角。
    #[test]
    fn rrf_diversifies_across_documents() {
        let hits: Vec<ChunkHit> = (0..10)
            .map(|index| ChunkHit {
                chunk: KnowledgeChunkId(index),
                doc: KnowledgeDocId(if index < 6 { 1 } else { 2 }),
                title: if index < 6 {
                    "甲".to_string()
                } else {
                    "乙".to_string()
                },
                ord: index as u32,
                text: format!("第 {index} 块"),
                locator: None,
                score: 0.0,
            })
            .collect();
        let fused = rrf_fuse(&hits, &[], RRF_K, 8);
        let second_doc = fused.iter().filter(|hit| hit.hit.doc.0 == 2).count();
        assert!(
            second_doc > 0,
            "第二份文档必须留在 top-K 里，否则跨文档查询答不出来"
        );
        assert_eq!(fused.len(), 8, "top-K 的长度不能变");
    }

    /// 文档多的时候，轮转取让覆盖面最大化：
    /// 10 份文档各 3 块、top-K = 8 → 应该拿到 8 份**不同**文档。
    #[test]
    fn rrf_diversification_maximizes_document_coverage() {
        let hits: Vec<ChunkHit> = (0..30)
            .map(|index| ChunkHit {
                chunk: KnowledgeChunkId(index),
                doc: KnowledgeDocId(index / 3 + 1),
                title: format!("文档 {}", index / 3 + 1),
                ord: (index % 3) as u32,
                text: format!("第 {index} 块"),
                locator: None,
                score: 0.0,
            })
            .collect();
        let fused = rrf_fuse(&hits, &[], RRF_K, 8);
        let distinct: std::collections::HashSet<i64> =
            fused.iter().map(|hit| hit.hit.doc.0).collect();
        assert_eq!(
            distinct.len(),
            8,
            "top-8 应该来自 8 份不同文档，实得 {} 份",
            distinct.len()
        );
    }

    /// 分散不能丢块：被挤掉的按分数补进剩余空位。
    #[test]
    fn rrf_diversification_keeps_every_slot_filled() {
        let hits: Vec<ChunkHit> = (0..5)
            .map(|index| ChunkHit {
                chunk: KnowledgeChunkId(index),
                doc: KnowledgeDocId(1),
                title: "甲".to_string(),
                ord: index as u32,
                text: format!("第 {index} 块"),
                locator: None,
                score: 0.0,
            })
            .collect();
        let fused = rrf_fuse(&hits, &[], RRF_K, 8);
        assert_eq!(fused.len(), 5, "块不够时不该凭空少几块");
    }
}
