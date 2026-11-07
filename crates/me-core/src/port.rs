//! 端口 trait：领域层对协议层与存储层的唯一契约。
//!
//! 设计说明（与架构文档的一处有意收敛）：
//! - **网络端口**（`ImapSession` / `ImapSessionFactory` / `SmtpSender`）使用
//!   `async fn` in trait（RPITIT），不引入 `async-trait`。
//! - **仓储端口**（`*Repo`）是**同步** trait：`me-store` 被明确禁止依赖 tokio，
//!   异步侧由 `me-sync` 用 `spawn_blocking` 调用。若把仓储写成 async，就必须把
//!   运行时色彩带进 `me-store`，与分层规则冲突。
//!
//! 任何 trait 都不得出现 `async-imap` / `lettre` / `rusqlite` 的类型。

use std::future::Future;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ai::{AiConversation, AiConversationId, AiEntity, AiMessage, AiMessageId};
use crate::calendar::{
    Calendar, CalendarEvent, CalendarEventDraft, CalendarEventId, CalendarException, CalendarId,
    CalendarSeries, CalendarSource,
};
use crate::command::AccountDraft;
use crate::error::CoreError;
use crate::model::{
    Account, AccountId, Address, Attachment, AuthCredential, Contact, ContactId, Draft,
    DraftContent, DraftId, DraftStatus, DraftSyncState, Flags, Folder, FolderId, FolderRole,
    MessageId, MessageSummary, NewMessage, OutgoingMessage, SavedSearch, SavedSearchId,
    SearchQuery,
};
use crate::weather::{WeatherLocation, WeatherLocationId, WeatherSnapshot};

// ---------------------------------------------------------------------------
// IMAP
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImapCapabilities {
    pub move_supported: bool,
    pub uidplus: bool,
    pub special_use: bool,
    /// 是否可在登录后发送 `ID`（QQ / 163 需要）。
    pub id: bool,
    pub utf8_accept: bool,
    /// RFC 2177 `IDLE`；不支持时 `me-sync` 降级为轮询。
    pub idle: bool,
    /// `AUTH=XOAUTH2`；OAuth2 账户登录前应确认。
    pub xoauth2: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderInfo {
    /// modified UTF-7 解码后的 UTF-8 名称。
    pub name: String,
    pub delimiter: Option<char>,
    /// `SPECIAL-USE` 属性原始字符串，如 `\Inbox`、`\Trash`。
    pub attributes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SelectedFolder {
    pub uid_validity: u32,
    pub uid_next: u32,
    pub exists: u32,
}

/// UID 区间。`end = None` 表示 `*`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UidRange {
    pub start: u32,
    pub end: Option<u32>,
}

impl UidRange {
    pub fn from(start: u32) -> Self {
        Self { start, end: None }
    }

    pub fn bounded(start: u32, end: u32) -> Self {
        Self {
            start,
            end: Some(end),
        }
    }

    pub fn is_empty(self) -> bool {
        matches!(self.end, Some(end) if end < self.start)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderFetch {
    pub uid: u32,
    pub flags: Flags,
    /// `INTERNALDATE` 的 unix 秒。
    pub internal_date: Option<i64>,
    pub size: Option<u32>,
    /// 原始头部字节（`BODY.PEEK[HEADER]`）。
    pub header: Vec<u8>,
    /// 由 `BODYSTRUCTURE` 判定。
    pub has_attachment: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagStoreMode {
    Add,
    Remove,
    Replace,
}

/// `IDLE` 一轮的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleOutcome {
    Changed {
        exists: Option<u32>,
        recent: Option<u32>,
    },
    Timeout,
    ManualInterrupt,
}

/// 单个已认证的 IMAP 会话。非线程安全：同一账户同一时刻只允许一条连接。
///
/// 返回类型写成 `impl Future + Send` 而非 `async fn`：多线程 tokio 运行时要求
/// 任务 future 为 `Send`，而 `async fn` in trait 无法表达该约束。这仍是 RPITIT，
/// 不引入 `async-trait`。
pub trait ImapSession: Send {
    fn capabilities(&self) -> ImapCapabilities;

    /// `LIST "" "*"`，请求 `SPECIAL-USE`。名称已解码为 UTF-8。
    fn list_folders(&mut self) -> impl Future<Output = Result<Vec<FolderInfo>, CoreError>> + Send;

    /// `SELECT`，返回 `UIDVALIDITY` / `UIDNEXT` / `EXISTS`。
    fn select(
        &mut self,
        folder: &str,
    ) -> impl Future<Output = Result<SelectedFolder, CoreError>> + Send;

    /// `UID FETCH <range> (UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER] BODYSTRUCTURE)`。
    /// 必须使用 `BODY.PEEK[]`，禁止会改变已读状态的 `BODY[]`。
    fn fetch_headers(
        &mut self,
        range: UidRange,
    ) -> impl Future<Output = Result<Vec<HeaderFetch>, CoreError>> + Send;

    /// 分页标志回拉：`UID FETCH <range> (UID FLAGS)`。
    fn fetch_flags(
        &mut self,
        range: UidRange,
    ) -> impl Future<Output = Result<Vec<(u32, Flags)>, CoreError>> + Send;

    fn store_flags(
        &mut self,
        uids: &[u32],
        flags: Flags,
        mode: FlagStoreMode,
    ) -> impl Future<Output = Result<(), CoreError>> + Send;

    /// `APPEND`。返回 `APPENDUID`（服务器支持 UIDPLUS 时）。
    fn append(
        &mut self,
        folder: &str,
        raw: &[u8],
        flags: Flags,
    ) -> impl Future<Output = Result<Option<u32>, CoreError>> + Send;

    /// `MOVE`。返回 `COPYUID` 映射（`(源 UID, 目标 UID)`），服务器不支持 UIDPLUS 时为 `None`。
    fn move_to(
        &mut self,
        uids: &[u32],
        target: &str,
    ) -> impl Future<Output = Result<Option<Vec<(u32, u32)>>, CoreError>> + Send;

    /// `COPY`，返回 `COPYUID` 映射。
    fn copy_to(
        &mut self,
        uids: &[u32],
        target: &str,
    ) -> impl Future<Output = Result<Option<Vec<(u32, u32)>>, CoreError>> + Send;

    /// `UID EXPUNGE`，仅在服务器通告 `UIDPLUS` 时调用。
    fn uid_expunge(&mut self, uids: &[u32]) -> impl Future<Output = Result<(), CoreError>> + Send;

    /// `UID SEARCH HEADER Message-ID <id>`。用于无 UIDPLUS 的移动确认。
    fn search_message_id(
        &mut self,
        message_id: &str,
    ) -> impl Future<Output = Result<Vec<u32>, CoreError>> + Send;

    /// `UID FETCH <uid> BODY.PEEK[]`。
    fn fetch_body(&mut self, uid: u32) -> impl Future<Output = Result<Vec<u8>, CoreError>> + Send;

    /// `UID FETCH <uid> BODY.PEEK[<part>]`。
    fn fetch_part(
        &mut self,
        uid: u32,
        part: &str,
    ) -> impl Future<Output = Result<Vec<u8>, CoreError>> + Send;

    /// `UID SEARCH UNDELETED`。用于草稿全量扫描（草稿不做 UIDNEXT 增量）。
    fn search_all_uids(&mut self) -> impl Future<Output = Result<Vec<u32>, CoreError>> + Send;

    /// 进入 `IDLE` 并等待变化，最多 `timeout`。
    ///
    /// 超时后必须发送 `DONE` 并保持连接可复用；取消时调用方应丢弃连接。
    fn wait_for_changes(
        &mut self,
        timeout: std::time::Duration,
    ) -> impl Future<Output = Result<IdleOutcome, CoreError>> + Send;

    fn logout(&mut self) -> impl Future<Output = Result<(), CoreError>> + Send;
}

/// 连接工厂。每个账户每次任务建立一条新会话，任务结束即 `logout`。
pub trait ImapSessionFactory: Send + Sync {
    type Session: ImapSession + Send;

    fn connect(
        &self,
        account: &Account,
        credential: &AuthCredential,
    ) -> impl Future<Output = Result<Self::Session, CoreError>> + Send;
}

// ---------------------------------------------------------------------------
// SMTP
// ---------------------------------------------------------------------------

pub trait SmtpSender: Send + Sync {
    /// 单次投递，不重试、不排队。失败由调用方决定保留草稿与提示。
    fn send(
        &self,
        account: &Account,
        credential: &AuthCredential,
        message: &OutgoingMessage,
    ) -> impl Future<Output = Result<(), CoreError>> + Send;
}

/// 系统浏览器打开端口。只接受 `http` / `https`；由 `me-app` 注入实现。
pub trait BrowserOpener: Send + Sync {
    fn open(&self, url: &str) -> Result<(), CoreError>;
}

// ---------------------------------------------------------------------------
// 仓储：账户 / 文件夹
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteAccountOutcome {
    Deleted,
    /// 仍有草稿且未显式要求级联删除。
    RefusedDraftsExist(u32),
    NotFound,
}

pub trait AccountRepo {
    fn list_accounts(&self) -> Result<Vec<Account>, CoreError>;
    fn get_account(&self, id: AccountId) -> Result<Option<Account>, CoreError>;
    /// 创建账户。`auth_secret` 不落库，由调用方写入 keyring。
    fn create_account(&self, draft: &AccountDraft) -> Result<Account, CoreError>;
    fn update_account(&self, id: AccountId, draft: &AccountDraft) -> Result<Account, CoreError>;
    fn delete_account(
        &self,
        id: AccountId,
        delete_drafts: bool,
    ) -> Result<DeleteAccountOutcome, CoreError>;
    fn count_drafts(&self, id: AccountId) -> Result<u32, CoreError>;
    /// 记录刷新令牌失效，UI 显示「需要重新授权」。
    fn set_account_reauth(&self, id: AccountId, needs_reauth: bool) -> Result<(), CoreError>;
    /// 仅用于展示/诊断；不参与令牌有效性判断。
    fn set_account_token_expiry(
        &self,
        id: AccountId,
        expires_at: Option<i64>,
    ) -> Result<(), CoreError>;
    /// 单账户实时同步覆盖：`poll_interval_secs=None` 表示跟随全局。
    fn set_account_realtime(
        &self,
        id: AccountId,
        idle_enabled: bool,
        poll_interval_secs: Option<u32>,
    ) -> Result<(), CoreError>;
    /// OAuth 授权成功后写入 provider/subject，并把认证方式置为 oauth2。
    fn set_account_oauth_identity(
        &self,
        id: AccountId,
        provider: crate::model::OAuthProvider,
        subject: Option<&str>,
    ) -> Result<(), CoreError>;
}

pub trait FolderRepo {
    fn list_folders(&self, account: AccountId) -> Result<Vec<Folder>, CoreError>;
    fn get_folder(&self, id: FolderId) -> Result<Option<Folder>, CoreError>;
    fn find_folder(&self, account: AccountId, name: &str) -> Result<Option<Folder>, CoreError>;
    /// 按 `(account, name)` upsert；已存在时刷新 delimiter / role / 清除 deleted 标记。
    fn upsert_folder(&self, account: AccountId, info: &FolderInfo) -> Result<Folder, CoreError>;
    fn rename_folder(&self, id: FolderId, new_name: &str) -> Result<(), CoreError>;
    fn set_folder_role(&self, id: FolderId, role: FolderRole) -> Result<(), CoreError>;
    fn set_folder_uid_state(
        &self,
        id: FolderId,
        uid_validity: u32,
        last_uid: u32,
    ) -> Result<(), CoreError>;
    fn set_folder_deleted(&self, id: FolderId, deleted: bool) -> Result<(), CoreError>;
    fn delete_folder(&self, id: FolderId) -> Result<(), CoreError>;
}

// ---------------------------------------------------------------------------
// 仓储：邮件
// ---------------------------------------------------------------------------

/// 正文补全队列项。执行侧先按 `folder` 分组，再按 `uid` 升序处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyJob {
    pub message: MessageId,
    pub folder: FolderId,
    pub uid: u32,
}

/// 重建索引时需要的行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRow {
    pub id: MessageId,
    pub subject: Option<String>,
    pub from_addr: Option<String>,
    pub to_addrs: String,
    pub body_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BodyCoverage {
    pub indexed: u32,
    pub total: u32,
}

/// 本地移动前的服务器位置（写入 `pending_ops`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveOrigin {
    pub message: MessageId,
    pub source_folder: FolderId,
    pub source_folder_name: String,
    pub source_uid: u32,
}

pub trait MessageRepo {
    /// 单事务：批量插入 + FTS 头部列 + `last_uid` 推进。
    /// 插入前按 `(folder_id, message_id)` 去重：命中 `uid IS NULL` 的占位行时只补 UID。
    fn insert_messages(
        &self,
        account: AccountId,
        folder: FolderId,
        messages: &[NewMessage],
        new_last_uid: u32,
    ) -> Result<Vec<MessageSummary>, CoreError>;

    fn list_messages(
        &self,
        folder: FolderId,
        cursor: Option<(i64, i64)>,
        limit: u32,
    ) -> Result<Vec<MessageSummary>, CoreError>;

    fn get_message(&self, id: MessageId) -> Result<Option<MessageSummary>, CoreError>;

    /// 查找头部文本或附件名中含 U+FFFD 的邮件：旧版本按 UTF-8 解码 GBK/GB18030 等
    /// 非 UTF-8 邮件后留下的替换字符。用于启动时从原始邮件修复。
    fn messages_with_broken_headers(&self, limit: u32) -> Result<Vec<MessageSummary>, CoreError>;

    /// 用原始邮件重新解析出的头部覆盖摘要，并同步 FTS 头部列。
    fn update_message_headers(
        &self,
        id: MessageId,
        subject: Option<&str>,
        from: Option<&Address>,
        to: &[Address],
        cc: &[Address],
    ) -> Result<(), CoreError>;

    fn unread_count(&self, folder: FolderId) -> Result<u32, CoreError>;

    fn body_coverage(
        &self,
        accounts: &[AccountId],
        folders: &[FolderId],
    ) -> Result<BodyCoverage, CoreError>;

    /// 本地标志变更：单事务写 `messages.flags` + `pending_flags` 目标态。
    fn apply_local_flags(
        &self,
        ids: &[MessageId],
        op: crate::model::FlagOp,
    ) -> Result<Vec<(MessageId, Flags)>, CoreError>;

    /// 服务器标志回拉：有 `pending_flags` 的行跳过；返回实际变更。
    fn apply_server_flags(
        &self,
        folder: FolderId,
        items: &[(u32, Flags)],
    ) -> Result<Vec<(MessageId, Flags)>, CoreError>;

    /// `UIDVALIDITY` 重建：清空该文件夹邮件与对应 FTS 行，返回删除数量。
    fn clear_folder(&self, folder: FolderId) -> Result<u32, CoreError>;

    /// 本地移入废件箱：改 `folder_id`、`uid = NULL`，并写入 `pending_ops`。
    fn move_to_trash(&self, origins: &[MoveOrigin], trash: FolderId) -> Result<(), CoreError>;

    /// 永久删除：先写 `pending_ops`，再删 `messages` 行。
    fn delete_permanent(&self, ids: &[MessageId]) -> Result<Vec<MoveOrigin>, CoreError>;

    fn body_jobs(&self, account: AccountId, limit: u32) -> Result<Vec<BodyJob>, CoreError>;

    fn pending_body_count(&self, account: AccountId) -> Result<u32, CoreError>;

    /// 线程头回填队列（v0.4 的 C5）：`body_path` 已就绪、但两列都还是 NULL 的邮件。
    ///
    /// **按 `id > after` 游标推进**，不用「取前 N 条」：本轮缺 raw 文件的邮件
    /// 会一直留在队列里，取前 N 条会让同一批永远被重复选中、整轮卡死。
    /// `after = 0` 表示从头开始。
    fn thread_backlog(&self, after: i64, limit: u32) -> Result<Vec<MessageId>, CoreError>;

    /// 回填结果落库。`references` 为空写 `"[]"`（**不是 NULL**）——
    /// 队列判据是 `"references" IS NULL`，空链写 NULL 会让这封邮件永远留在队列里。
    fn set_thread_headers(
        &self,
        id: MessageId,
        in_reply_to: Option<&str>,
        references: &[String],
    ) -> Result<(), CoreError>;

    /// 正文落盘后：同事务更新 `body_path` / `snippet` 与 FTS `body`。
    fn set_body_cached(
        &self,
        id: MessageId,
        body_path: &str,
        text: &str,
        snippet: Option<&str>,
    ) -> Result<(), CoreError>;

    /// 清理缓存：同事务把 `body_path` 置 NULL、清空 FTS `body`；返回受影响行数。
    fn reset_body_cache(&self, account: Option<AccountId>) -> Result<u32, CoreError>;

    /// 重建索引用：按账户（或全部）列出索引行。
    fn index_rows(&self, account: Option<AccountId>) -> Result<Vec<IndexRow>, CoreError>;

    /// 缓存清理用：账户下所有邮件 id。
    fn message_ids(&self, account: AccountId) -> Result<Vec<MessageId>, CoreError>;

    /// 服务器确认移动后回填 UID。
    fn confirm_moved_uid(
        &self,
        folder: FolderId,
        message_id: &str,
        new_uid: u32,
    ) -> Result<Option<MessageId>, CoreError>;
}

// ---------------------------------------------------------------------------
// 仓储：附件 / 草稿 / 待同步 / 搜索 / 元数据
// ---------------------------------------------------------------------------

pub trait AttachmentRepo {
    fn list_attachments(&self, message: MessageId) -> Result<Vec<Attachment>, CoreError>;
    /// 按 `(message_id, part_index)` upsert 元数据（不触碰 `local_path`）。
    fn upsert_attachments(
        &self,
        message: MessageId,
        attachments: &[Attachment],
    ) -> Result<(), CoreError>;
    fn set_attachment_path(
        &self,
        message: MessageId,
        part_index: u32,
        local_path: &Path,
    ) -> Result<(), CoreError>;
    /// 附件缓存重定位：把位于 `old_dir` 下的 `local_path` 前缀替换为 `new_dir`。
    /// 返回更新的行数；不在 `old_dir` 下的路径保持不变。
    fn rewrite_attachment_paths(&self, old_dir: &Path, new_dir: &Path) -> Result<u32, CoreError>;
    fn get_attachment(
        &self,
        message: MessageId,
        part_index: u32,
    ) -> Result<Option<Attachment>, CoreError>;
}

/// 草稿同步候选（活动草稿或删除墓碑）。纯数据，不暴露 SQL 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftSyncItem {
    pub id: DraftId,
    pub message_id: String,
    pub server_uid: Option<u32>,
    pub server_uid_validity: Option<u32>,
    pub local_hash: Option<String>,
    pub server_hash: Option<String>,
    pub state: DraftSyncState,
    /// `deleted_at IS NOT NULL`：墓碑行，只参与服务器删除确认。
    pub deleted: bool,
    pub sync_attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDraft {
    pub account: AccountId,
    pub message_id: String,
    pub subject: Option<String>,
    pub to: Vec<crate::model::Address>,
    /// 仅本地保存，不写入 RFC822。
    pub bcc: Vec<crate::model::Address>,
    pub raw_path: String,
}

pub trait DraftRepo {
    fn create_draft(&self, draft: &NewDraft) -> Result<Draft, CoreError>;
    fn update_draft(
        &self,
        id: DraftId,
        subject: Option<&str>,
        to: &[crate::model::Address],
        bcc: &[crate::model::Address],
        raw_path: &str,
    ) -> Result<Draft, CoreError>;
    fn get_draft(&self, id: DraftId) -> Result<Option<Draft>, CoreError>;
    fn list_drafts(&self, account: Option<AccountId>) -> Result<Vec<Draft>, CoreError>;
    fn set_draft_server_uid(&self, id: DraftId, uid: Option<u32>) -> Result<(), CoreError>;
    fn set_draft_status(
        &self,
        id: DraftId,
        status: DraftStatus,
        last_error: Option<&str>,
    ) -> Result<(), CoreError>;
    fn delete_draft(&self, id: DraftId) -> Result<(), CoreError>;
    fn list_draft_dirs(&self) -> Result<Vec<(DraftId, PathBuf)>, CoreError>;

    // ------------------------------------------------------------------
    // v0.2 草稿双向同步
    // ------------------------------------------------------------------

    /// 返回活动候选与删除墓碑；用 `DraftSyncItem.deleted` 区分两类。
    fn list_draft_sync_candidates(
        &self,
        account: AccountId,
    ) -> Result<Vec<DraftSyncItem>, CoreError>;

    /// 单条草稿的同步基线（含已 Synced 的行）。
    fn get_draft_sync_item(&self, id: DraftId) -> Result<Option<DraftSyncItem>, CoreError>;

    /// 按 `(account, UIDVALIDITY, UID)` 查找；`uid_validity` 未知时不参与等值匹配。
    fn find_draft_by_server_uid(
        &self,
        account: AccountId,
        uid_validity: u32,
        uid: u32,
    ) -> Result<Option<Draft>, CoreError>;

    /// 按 Message-ID 兜底匹配；最多返回 2 条，>1 表示不能自动合并。
    fn find_drafts_by_message_id(
        &self,
        account: AccountId,
        message_id: &str,
    ) -> Result<Vec<Draft>, CoreError>;

    fn mark_draft_synced(
        &self,
        id: DraftId,
        server_uid: Option<u32>,
        uid_validity: Option<u32>,
        local_hash: &str,
        server_hash: &str,
    ) -> Result<(), CoreError>;

    /// 写入前校验 `sync_state` 合法值。
    fn set_draft_sync_state(
        &self,
        id: DraftId,
        state: DraftSyncState,
        error: Option<&str>,
    ) -> Result<(), CoreError>;

    /// 失败计数 +1，返回新的计数；达到 3 后调用方停止自动重试。
    fn bump_draft_sync_attempt(&self, id: DraftId, error: &str) -> Result<u32, CoreError>;
    fn clear_draft_sync_attempts(&self, id: DraftId) -> Result<(), CoreError>;

    /// 创建冲突副本；`conflict_of` 指向原草稿，副本无服务器身份。
    fn create_conflict_copy(
        &self,
        source: DraftId,
        content: &DraftContent,
    ) -> Result<DraftId, CoreError>;

    /// 本地删除墓碑：保留行与目录，等服务器删除确认。
    fn tombstone_draft(&self, id: DraftId) -> Result<(), CoreError>;
    /// 墓碑确认完成后物理删除。
    fn finalize_deleted_draft(&self, id: DraftId) -> Result<(), CoreError>;
    /// UIDVALIDITY 变化：清空服务器身份与 server_hash，保留 local_hash 基线。
    fn reset_draft_server_identity(&self, account: AccountId) -> Result<u32, CoreError>;
    /// 一次性确认后把从未上传的本地草稿转入待推送状态。
    fn promote_local_only_to_dirty(&self, account: Option<AccountId>) -> Result<u32, CoreError>;
    /// 「保留两份」：冲突副本转为独立本地草稿（清除 conflict_of）。
    fn promote_conflict_copy(&self, id: DraftId) -> Result<(), CoreError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingOpKind {
    MoveToTrash,
    DeletePermanent,
    EmptyFolder,
}

impl PendingOpKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PendingOpKind::MoveToTrash => "move_to_trash",
            PendingOpKind::DeletePermanent => "delete_permanent",
            PendingOpKind::EmptyFolder => "empty_folder",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "move_to_trash" => Some(PendingOpKind::MoveToTrash),
            "delete_permanent" => Some(PendingOpKind::DeletePermanent),
            "empty_folder" => Some(PendingOpKind::EmptyFolder),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingOp {
    pub id: i64,
    pub account: AccountId,
    pub kind: PendingOpKind,
    pub message: Option<MessageId>,
    pub source_folder: Option<FolderId>,
    pub source_folder_name: Option<String>,
    pub source_uid: Option<u32>,
    pub target_folder: Option<FolderId>,
    pub attempts: u32,
    pub last_error: Option<String>,
}

pub trait PendingRepo {
    /// 合并写入目标态（`UPSERT`，不累加）。
    fn enqueue_flags(&self, message: MessageId, target: Flags) -> Result<(), CoreError>;
    fn pending_flags(&self, limit: u32) -> Result<Vec<(MessageId, Flags, u32)>, CoreError>;
    fn clear_pending_flags(&self, message: MessageId) -> Result<(), CoreError>;
    fn bump_pending_flags_attempt(&self, message: MessageId, error: &str) -> Result<(), CoreError>;

    fn enqueue_op(&self, op: &PendingOp) -> Result<i64, CoreError>;
    fn pending_ops(&self, account: AccountId, limit: u32) -> Result<Vec<PendingOp>, CoreError>;
    fn complete_op(&self, id: i64) -> Result<(), CoreError>;
    fn bump_op_attempt(&self, id: i64, error: &str) -> Result<(), CoreError>;
    fn cleanup_ops_for_folder(&self, folder: FolderId) -> Result<u32, CoreError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOutcome {
    pub query_id: u64,
    pub messages: Vec<MessageSummary>,
    pub total: u32,
    pub body_indexed: u32,
    pub body_total: u32,
}

pub trait SearchRepo {
    fn search(&self, query: &SearchQuery) -> Result<SearchOutcome, CoreError>;
    /// 清空并重建 FTS；返回重建条数。
    fn rebuild_index(&self) -> Result<u32, CoreError>;
}

/// 保存的搜索的仓储（`saved_searches` 表）。
///
/// **只存条件、不做自动动作**（D42）——自动规则推迟到用户真的用了一阵子之后再说。
pub trait SavedSearchRepo {
    /// 按 `last_used_at` 倒序列出全部保存的搜索。
    fn list_saved_searches(&self) -> Result<Vec<SavedSearch>, CoreError>;
    /// 读出一条保存的搜索（执行时用）。
    fn get_saved_search(&self, id: SavedSearchId) -> Result<Option<SavedSearch>, CoreError>;
    /// 新增一条。名称重复时返回 `CoreError::Storage`。
    fn insert_saved_search(&self, search: &SavedSearch) -> Result<SavedSearchId, CoreError>;
    /// 删除一条。不存在时**不报错**（幂等）。
    fn delete_saved_search(&self, id: SavedSearchId) -> Result<(), CoreError>;
    /// 更新 `last_used_at`（点击执行时调用）。
    fn touch_saved_search(&self, id: SavedSearchId) -> Result<(), CoreError>;
}

/// 联系人的仓储（`contacts` 表）。
///
/// 联系人是**本地派生数据**：由正文补全落库 / 发信归档时累积。
/// 永不出网、不参与 AI 请求；`ClearCache` **不清理**它（与 D30 一致）。
pub trait ContactRepo {
    /// 批量 UPSERT。`addr` 小写归一化后去重——邮箱大小写不敏感，
    /// 但显示时保留原样。已存在的只更新 `last_seen` 与 `seen_count`。
    fn upsert_contacts(&self, contacts: &[Contact]) -> Result<(), CoreError>;
    /// 按前缀列出联系人（补全下拉用）。
    ///
    /// 排序：`last_seen` 最近 > `seen_count` 高 > 姓名包含匹配 > 邮箱前缀匹配。
    fn list_contacts(&self, prefix: &str, limit: u32) -> Result<Vec<Contact>, CoreError>;
    /// 删除单个联系人（用户手动删除）。
    fn remove_contact(&self, id: ContactId) -> Result<(), CoreError>;
}

pub trait MetaRepo {
    fn schema_version(&self) -> Result<u32, CoreError>;
    fn set_setting(&self, key: &str, value: &str) -> Result<(), CoreError>;
    fn get_setting(&self, key: &str) -> Result<Option<String>, CoreError>;
    /// 缓存目录所在文件系统的可用字节数；平台不支持时返回 `None`。
    fn available_bytes(&self) -> Result<Option<u64>, CoreError>;
}

/// `me-app` 唯一可见的存储句柄：只读、键集分页、单次 `LIMIT ≤ 200`。
pub trait ReadOnlyRepo: Send + Sync {
    fn list_accounts_readonly(&self) -> Result<Vec<Account>, CoreError>;
    fn list_folders_readonly(&self, account: AccountId) -> Result<Vec<Folder>, CoreError>;
    fn list_messages_readonly(
        &self,
        folder: FolderId,
        cursor: Option<(i64, i64)>,
        limit: u32,
    ) -> Result<Vec<MessageSummary>, CoreError>;
    fn unread_count_readonly(&self, folder: FolderId) -> Result<u32, CoreError>;
    fn get_message_readonly(&self, id: MessageId) -> Result<Option<MessageSummary>, CoreError>;
    fn list_drafts_readonly(&self, account: Option<AccountId>) -> Result<Vec<Draft>, CoreError>;
    fn get_draft_readonly(&self, id: DraftId) -> Result<Option<Draft>, CoreError>;

    /// 按前缀查联系人（撰写页补全下拉用）。
    ///
    /// **只读、单次 `LIMIT ≤ 200`**——与本 trait 的其它方法同一套约束。
    fn list_contacts_readonly(&self, prefix: &str, limit: u32) -> Result<Vec<Contact>, CoreError>;

    /// 统一收件箱：各账户 `role='inbox'` 文件夹的并集，按时间倒序。
    ///
    /// **跨账户聚合必须在 store 层一次查完**——UI 端拉完各账户再本地合并
    /// 会破坏键集分页，也违反 `LIMIT ≤ 200` 的硬规则。
    fn list_unified_inbox_readonly(
        &self,
        cursor: Option<(i64, i64)>,
        limit: u32,
    ) -> Result<Vec<MessageSummary>, CoreError>;
    /// 统一收件箱的未读计数。
    fn unified_unread_count_readonly(&self) -> Result<u32, CoreError>;

    // 日历：主线程只读，单次 `LIMIT <= 200`。
    fn list_calendars_readonly(&self) -> Result<Vec<Calendar>, CoreError>;
    fn list_calendar_series_readonly(
        &self,
        calendars: &[CalendarId],
        from_utc: i64,
        to_utc: i64,
        limit: u32,
    ) -> Result<Vec<CalendarSeries>, CoreError>;
    fn list_calendar_exceptions_readonly(
        &self,
        events: &[CalendarEventId],
    ) -> Result<Vec<(CalendarEventId, CalendarException)>, CoreError>;
    fn get_calendar_event_readonly(
        &self,
        id: CalendarEventId,
    ) -> Result<Option<CalendarEvent>, CoreError>;

    // 天气：地点与快照只读；写入一律走 `Command`。
    fn list_weather_locations_readonly(&self) -> Result<Vec<WeatherLocation>, CoreError>;
    fn get_weather_snapshot_readonly(
        &self,
        location_id: WeatherLocationId,
    ) -> Result<Option<WeatherSnapshot>, CoreError>;

    // AI：会话/消息/实体只读；流式期间 UI 靠这些方法重载最终内容。
    fn list_ai_conversations_readonly(
        &self,
        before: Option<AiConversationId>,
        limit: u32,
    ) -> Result<Vec<AiConversation>, CoreError>;
    fn list_ai_messages_readonly(
        &self,
        conversation: AiConversationId,
        before: Option<AiMessageId>,
        limit: u32,
    ) -> Result<Vec<AiMessage>, CoreError>;
    fn get_ai_message_readonly(&self, id: AiMessageId) -> Result<Option<AiMessage>, CoreError>;
    fn list_ai_entities_readonly(&self, limit: u32) -> Result<Vec<AiEntity>, CoreError>;
    fn count_ai_entities_readonly(&self) -> Result<u32, CoreError>;
}

// ---------------------------------------------------------------------------
// 兼容别名（文档中的命名）
// ---------------------------------------------------------------------------

pub use DraftRepo as DraftRepository;
pub use FolderRepo as FolderRepository;
pub use MessageRepo as MessageRepository;

/// 用于序列化测量/日志的辅助结构。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftSummary {
    pub id: DraftId,
    pub account: AccountId,
    pub subject: Option<String>,
    pub status: DraftStatus,
    pub updated_at: i64,
}
// ---------------------------------------------------------------------------
// 日历
// ---------------------------------------------------------------------------

/// 日历与事件的仓储端口。
///
/// **写入只允许由 `me-sync` 的用例调用**（见 `doc/v03架构设计.md` §2.7）：
/// `me-app` 只能通过 [`ReadOnlyRepo`] 的只读方法读取；界面侧的写操作一律发
/// `Command`。这样「算 `recurrence_until`、去重、`source` 归属」才是构造上成立的。
pub trait CalendarRepo {
    /// 取默认日历；不存在时按 `默认日历` + 默认配色创建。
    fn ensure_default_calendar(&self) -> Result<Calendar, CoreError>;
    fn list_calendars(&self) -> Result<Vec<Calendar>, CoreError>;
    fn create_calendar(&self, name: &str, color: &str) -> Result<Calendar, CoreError>;
    fn set_calendar_visible(&self, id: CalendarId, visible: bool) -> Result<(), CoreError>;
    fn rename_calendar(&self, id: CalendarId, name: &str) -> Result<(), CoreError>;

    /// 插入事件。`recurrence_until` 由 `me_core::calendar::recurrence_until` 计算，
    /// 必须与事件同事务写入，避免出现「有 rrule 但没有上界」的行。
    fn create_event(
        &self,
        draft: &CalendarEventDraft,
        recurrence_until: Option<i64>,
        source: CalendarSource,
    ) -> Result<CalendarEvent, CoreError>;
    /// 更新事件本体（`scope = AllEvents`）。保留原 `source` 与 `sequence + 1`。
    fn update_event(
        &self,
        id: CalendarEventId,
        draft: &CalendarEventDraft,
        recurrence_until: Option<i64>,
    ) -> Result<CalendarEvent, CoreError>;
    /// 软删除（写 `deleted_at` 墓碑）。
    fn soft_delete_event(&self, id: CalendarEventId) -> Result<(), CoreError>;
    /// 撤销软删除：只把 `deleted_at` 清回 `NULL`（并刷新 `updated_at`）。
    ///
    /// 刻意**不动** `status` / `sequence` / `summary`：恢复不是修改，
    /// 与 [`CalendarRepo::soft_delete_event`] 保持对称。
    fn restore_event(&self, id: CalendarEventId) -> Result<CalendarEvent, CoreError>;
    fn get_event(&self, id: CalendarEventId) -> Result<Option<CalendarEvent>, CoreError>;

    /// 可视范围内的「主行」集合：一次性事件 + 尚未结束的重复系列（见 §4.6）。
    /// 展开由 `me_core::calendar::expand_event` 在调用方完成。
    fn list_series_in_range(
        &self,
        calendars: &[CalendarId],
        from_utc: i64,
        to_utc: i64,
        limit: u32,
    ) -> Result<Vec<CalendarSeries>, CoreError>;

    /// 批量取例外（单次改期/取消）。事件为空时返回空。
    fn list_exceptions(
        &self,
        events: &[CalendarEventId],
    ) -> Result<Vec<(CalendarEventId, CalendarException)>, CoreError>;

    /// 写入/覆盖一次例外。同一 `(event, recurrence_at)` 只保留最新一条。
    fn insert_exception(
        &self,
        event: CalendarEventId,
        exception: &CalendarException,
    ) -> Result<(), CoreError>;

    /// 提醒候选：`reminders <> '[]'` 且未结束的系列。
    fn list_reminder_candidates(
        &self,
        from_utc: i64,
        window_end: i64,
        limit: u32,
    ) -> Result<Vec<CalendarSeries>, CoreError>;

    /// 记录一次提醒触发；已存在时返回 `false`（幂等，防重复弹窗）。
    fn record_reminder_fire(
        &self,
        event: CalendarEventId,
        occurrence_at: i64,
        minutes_before: u32,
    ) -> Result<bool, CoreError>;
}

// ---------------------------------------------------------------------------
// 组合端口
// ---------------------------------------------------------------------------

/// `me-sync` 所需写句柄的组合。由 `me-store::WritableStore` 实现。
pub trait StorePort:
    AccountRepo
    + FolderRepo
    + MessageRepo
    + AttachmentRepo
    + DraftRepo
    + PendingRepo
    + SearchRepo
    + SavedSearchRepo
    + ContactRepo
    + CalendarRepo
    + WeatherRepo
    + crate::ai::AiRepo
    + crate::ai::AiMemoryRepo
    + MetaRepo
    + Clone
    + Send
    + Sync
    + 'static
{
}

impl<T> StorePort for T where
    T: AccountRepo
        + FolderRepo
        + MessageRepo
        + AttachmentRepo
        + DraftRepo
        + PendingRepo
        + SearchRepo
        + SavedSearchRepo
        + ContactRepo
        + CalendarRepo
        + WeatherRepo
        + crate::ai::AiRepo
        + crate::ai::AiMemoryRepo
        + MetaRepo
        + Clone
        + Send
        + Sync
        + 'static
{
}

/// 天气仓储端口定义在 `me_core::weather`，此处转出以便统一从 `port` 导入。
pub use crate::weather::WeatherRepo;

/// AI 端口定义在 `me_core::ai`，此处转出以便统一从 `port` 导入。
pub use crate::ai::{AiFuture, AiMemoryRepo, AiProvider, AiRepo, CancelFlag};
