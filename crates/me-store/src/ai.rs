//! AI 仓储：会话、消息、实体注册表。
//!
//! 只做存取与结构约束（长度、JSON 往返、级联、上限），不构建请求、不裁剪上下文、
//! 不执行工具、不存密钥（见 `doc/v03架构设计.md` §3.4）。
//!
//! 两处有意放在存储层、而不是用例层的约束：
//! - **实体上限 200**：达到上限时插入必须失败，靠事务内 `COUNT` 保证并发下也不会
//!   突破；放到用例层会出现「两个工具同时写」的竞态。
//! - **助手消息 256 KiB 截断**：超限时把状态改成 `failed` 并写明原因，
//!   否则一条超长回复会把整页渲染拖死。

use std::collections::HashMap;

use me_core::ai::{
    AiConversation, AiConversationId, AiEntity, AiEntityKind, AiEntitySource, AiMemoryRepo,
    AiMessage, AiMessageId, AiMessageStatus, AiProviderConfig, AiProviderKind, AiRepo, AiRole,
    AiToolName, AiToolStatus, AiToolTrace, MAX_ENTITIES, MAX_MESSAGE_CHARS, MailCardRef,
    MotiveSource, NewAiEntity, NewAiMessage, ThinkingBlock, normalize_entity_name,
};
use me_core::ai_plan::Plan;
use me_core::model::now_unix;
use me_core::{CoreError, truncate_chars};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::de::DeserializeOwned;

use crate::StoreResult;
use crate::db::{ReadOnlyStore, WritableStore};

const CONVERSATION_COLUMNS: &str = "c.id, c.title, c.provider, c.model, c.model_id, c.created_at, \
     c.updated_at, \
     (SELECT COUNT(*) FROM ai_messages m WHERE m.conversation_id = c.id)";

/// 列序**位置敏感**：`map_message` 用 `row.get(N)` 硬编码索引，新增列一律
/// 追加在 `created_at` 之后，插在中间会让后续所有字段错位。
const MESSAGE_COLUMNS: &str = "id, conversation_id, role, content, context_refs, status, error, \
     input_tokens, output_tokens, tool_trace, plan, created_at, thinking";

const ENTITY_COLUMNS: &str = "e.id, e.kind, e.canonical_name, e.email, e.account_id, e.calendar_id, \
     e.attributes, e.source, e.confidence, e.created_at, e.updated_at, e.last_seen_at";

fn map_conversation(row: &Row<'_>) -> rusqlite::Result<AiConversation> {
    let provider: String = row.get(2)?;
    Ok(AiConversation {
        id: AiConversationId(row.get(0)?),
        title: row.get(1)?,
        provider: AiProviderKind::parse(&provider).unwrap_or(AiProviderKind::Anthropic),
        model: row.get(3)?,
        model_id: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
        message_count: row.get::<_, i64>(7).unwrap_or(0) as u32,
    })
}

/// 容忍损坏的 JSON 列：历史数据坏掉时按「空」处理，不让整个聊天页打不开。
fn parse_list<T: DeserializeOwned>(raw: Option<String>) -> Vec<T> {
    raw.and_then(|text| serde_json::from_str::<Vec<T>>(&text).ok())
        .unwrap_or_default()
}

fn map_tool_name(raw: &str) -> Option<AiToolName> {
    AiToolName::parse(raw)
}

fn map_message(row: &Row<'_>) -> rusqlite::Result<AiMessage> {
    let role: String = row.get(2)?;
    let status: String = row.get(5)?;
    let tool_trace: Vec<ToolTraceRow> = parse_list(row.get(9)?);
    Ok(AiMessage {
        id: AiMessageId(row.get(0)?),
        conversation: AiConversationId(row.get(1)?),
        role: AiRole::parse(&role).unwrap_or(AiRole::User),
        content: row.get(3)?,
        context_refs: parse_list(row.get(4)?),
        status: AiMessageStatus::parse(&status).unwrap_or(AiMessageStatus::Complete),
        error: row.get(6)?,
        input_tokens: row.get::<_, Option<i64>>(7)?.map(|n| n as u32),
        output_tokens: row.get::<_, Option<i64>>(8)?.map(|n| n as u32),
        tool_trace: tool_trace
            .into_iter()
            .filter_map(|t| t.into_trace())
            .collect(),
        // 坏 JSON 按「没有清单」处理：这条消息本来也还能当普通回复读，
        // 为了一段进度条把整个会话打不开不值得。
        plan: parse_plan(row.get::<_, Option<String>>(10)?),
        created_at: row.get(11)?,
        // 新列追加在尾部，所以既有索引一个都不用动。
        thinking: parse_thinking(row.get::<_, Option<String>>(12)?),
    })
}

/// 思考块的容错解析：坏 JSON 或不是对象，都按「没有思考」处理。
///
/// **不记日志**：思考是纯展示项，坏了就当没有；像 `parse_plan` 那样每次
/// 都 warn，会在用户每次打开旧会话时刷屏。
///
/// 刻意保留 `Some(空块)` 与 `None` 的区别：前者说明服务商返回过思考但内容
/// 为空，后者是压根没这功能。UI 再决定要不要画区块（见 `show_thinking`）。
fn parse_thinking(raw: Option<String>) -> Option<ThinkingBlock> {
    let raw = raw?;
    serde_json::from_str::<ThinkingBlock>(&raw).ok()
}

/// 清单列的容错解析。`None` 不是错误：绝大多数消息本来就没有清单。
fn parse_plan(raw: Option<String>) -> Option<Plan> {
    let raw = raw?;
    match serde_json::from_str::<Plan>(&raw) {
        Ok(plan) => Some(plan),
        Err(err) => {
            tracing::warn!(error = %err, "ai_messages.plan 解析失败，按无清单处理");
            None
        }
    }
}

/// `tool_trace` 的磁盘形态。与领域类型分开，是为了让「未知工具名 / 未知状态」
/// 在读取时降级而不是整行解析失败。
#[derive(serde::Serialize, serde::Deserialize)]
struct ToolTraceRow {
    call_id: String,
    tool: String,
    status: String,
    summary: String,
    duration_ms: u32,
    /// 老行没有这两个字段：`#[serde(default)]` 让它们读成 `None` / `model`。
    #[serde(default)]
    motive: Option<String>,
    #[serde(default)]
    motive_source: String,
    /// v2 起才有：这次调用绑定第几步、第几次尝试。老行读成 `None`。
    #[serde(default)]
    step_id: Option<u8>,
    #[serde(default)]
    attempt: Option<u8>,
    /// v8 起才有：这次检索挑出来的邮件卡片（id + 命中理由）。老行读成空数组。
    #[serde(default)]
    cards: Vec<MailCardRef>,
}

impl ToolTraceRow {
    fn from_trace(trace: &AiToolTrace) -> Self {
        Self {
            call_id: trace.call_id.clone(),
            tool: trace.tool.as_str().to_string(),
            status: trace.status.as_str().to_string(),
            summary: trace.summary.clone(),
            duration_ms: trace.duration_ms,
            motive: trace.motive.clone(),
            motive_source: trace.motive_source.as_str().to_string(),
            step_id: trace.step_id,
            attempt: trace.attempt,
            cards: trace.cards.clone(),
        }
    }

    fn into_trace(self) -> Option<AiToolTrace> {
        Some(AiToolTrace {
            call_id: self.call_id,
            tool: map_tool_name(&self.tool)?,
            status: AiToolStatus::parse(&self.status)?,
            summary: self.summary,
            duration_ms: self.duration_ms,
            motive: self.motive,
            // 未知来源按「模型说的」处理：老数据没有这个字段，而它的 motive 本来就是
            // 模型写的；真缺失时 motive 也是 None，界面不会误显示成默认值。
            motive_source: MotiveSource::parse(&self.motive_source).unwrap_or_default(),
            step_id: self.step_id,
            attempt: self.attempt,
            cards: self.cards,
        })
    }
}

fn map_entity_base(row: &Row<'_>) -> rusqlite::Result<AiEntity> {
    let kind: String = row.get(1)?;
    let source: String = row.get(7)?;
    let attributes: Option<String> = row.get(6)?;
    Ok(AiEntity {
        id: row.get(0)?,
        kind: AiEntityKind::parse(&kind).unwrap_or(AiEntityKind::Other),
        canonical_name: row.get(2)?,
        aliases: Vec::new(),
        email: row.get(3)?,
        account: row.get::<_, Option<i64>>(4)?.map(me_core::AccountId),
        calendar: row.get::<_, Option<i64>>(5)?.map(me_core::CalendarId),
        attributes: attributes
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_else(|| serde_json::json!({})),
        source: AiEntitySource::parse(&source).unwrap_or(AiEntitySource::Tool),
        confidence: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        last_seen_at: row.get(11)?,
    })
}

/// 批量补别名，避免逐条查询（`limit ≤ 200`，但仍只做一次 IN 查询）。
fn load_aliases(conn: &Connection, entities: &mut [AiEntity]) -> Result<(), CoreError> {
    if entities.is_empty() {
        return Ok(());
    }
    let placeholders = std::iter::repeat_n("?", entities.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT entity_id, alias FROM ai_entity_aliases WHERE entity_id IN ({placeholders}) \
         ORDER BY entity_id, alias"
    );
    let mut stmt = conn.prepare(&sql).store()?;
    let ids: Vec<i64> = entities.iter().map(|entity| entity.id).collect();
    let rows = stmt
        .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .store()?;
    let mut grouped: HashMap<i64, Vec<String>> = HashMap::new();
    for row in rows {
        let (id, alias) = row.store()?;
        grouped.entry(id).or_default().push(alias);
    }
    for entity in entities.iter_mut() {
        if let Some(aliases) = grouped.remove(&entity.id) {
            entity.aliases = aliases;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 会话
// ---------------------------------------------------------------------------

pub(crate) fn list_conversations_conn(
    conn: &Connection,
    before: Option<AiConversationId>,
    limit: u32,
) -> Result<Vec<AiConversation>, CoreError> {
    let limit = limit.clamp(1, 200);
    let sql = format!(
        "SELECT {CONVERSATION_COLUMNS} FROM ai_conversations c \
         WHERE ?1 IS NULL \
            OR c.updated_at < (SELECT updated_at FROM ai_conversations WHERE id = ?1) \
            OR (c.updated_at = (SELECT updated_at FROM ai_conversations WHERE id = ?1) AND c.id < ?1) \
         ORDER BY c.updated_at DESC, c.id DESC LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql).store()?;
    let rows = stmt
        .query_map(params![before.map(|id| id.0), limit], map_conversation)
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    Ok(rows)
}

pub(crate) fn get_conversation_conn(
    conn: &Connection,
    id: AiConversationId,
) -> Result<Option<AiConversation>, CoreError> {
    let sql = format!("SELECT {CONVERSATION_COLUMNS} FROM ai_conversations c WHERE c.id = ?1");
    conn.query_row(&sql, params![id.0], map_conversation)
        .optional()
        .store()
}

pub(crate) fn list_messages_conn(
    conn: &Connection,
    conversation: AiConversationId,
    before: Option<AiMessageId>,
    limit: u32,
) -> Result<Vec<AiMessage>, CoreError> {
    let limit = limit.clamp(1, 200);
    let sql = format!(
        "SELECT {MESSAGE_COLUMNS} FROM ai_messages \
         WHERE conversation_id = ?1 AND (?2 IS NULL OR id < ?2) \
         ORDER BY id DESC LIMIT ?3"
    );
    let mut stmt = conn.prepare(&sql).store()?;
    let mut rows = stmt
        .query_map(
            params![conversation.0, before.map(|id| id.0), limit],
            map_message,
        )
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    // 查询按 id DESC 取最新一页；调用方需要时间正序。
    rows.reverse();
    Ok(rows)
}

pub(crate) fn get_message_conn(
    conn: &Connection,
    id: AiMessageId,
) -> Result<Option<AiMessage>, CoreError> {
    let sql = format!("SELECT {MESSAGE_COLUMNS} FROM ai_messages WHERE id = ?1");
    conn.query_row(&sql, params![id.0], map_message)
        .optional()
        .store()
}

fn create_conversation_tx(
    conn: &mut Connection,
    title: &str,
    model_id: Option<&str>,
    config: &AiProviderConfig,
) -> Result<AiConversation, CoreError> {
    let title = validate_title(title)?;
    let now = now_unix();
    let tx = conn
        .transaction()
        .map_err(|e| crate::storage(format!("开始会话事务失败: {e}")))?;
    tx.execute(
        "INSERT INTO ai_conversations(title, provider, model, model_id, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![
            title,
            config.kind.as_str(),
            config.model.trim(),
            model_id,
            now
        ],
    )
    .store()?;
    let id = AiConversationId(tx.last_insert_rowid());
    tx.commit()
        .map_err(|e| crate::storage(format!("提交会话事务失败: {e}")))?;
    Ok(AiConversation {
        id,
        title,
        model_id: model_id.map(str::to_string),
        provider: config.kind,
        model: config.model.trim().to_string(),
        created_at: now,
        updated_at: now,
        message_count: 0,
    })
}

/// 把会话切到另一套参数。
///
/// 同时刷新 `provider` / `model` 快照：否则「切了模型但历史标注仍写着旧模型」，
/// 用户无法判断某条回复到底是谁答的。
fn set_conversation_model_tx(
    conn: &Connection,
    id: AiConversationId,
    model_id: &str,
    config: &AiProviderConfig,
    bump_updated: bool,
) -> Result<(), CoreError> {
    let sql = if bump_updated {
        "UPDATE ai_conversations SET model_id = ?2, provider = ?3, model = ?4, updated_at = ?5 \
         WHERE id = ?1"
    } else {
        "UPDATE ai_conversations SET model_id = ?2, provider = ?3, model = ?4 WHERE id = ?1"
    };
    let now = now_unix();
    let changed = conn
        .execute(
            sql,
            params![
                id.0,
                model_id,
                config.kind.as_str(),
                config.model.trim(),
                now
            ],
        )
        .store()?;
    if changed == 0 {
        return Err(CoreError::validation("会话不存在"));
    }
    Ok(())
}

fn validate_title(title: &str) -> Result<String, CoreError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(CoreError::validation("会话标题不能为空"));
    }
    Ok(truncate_chars(title, 80))
}

/// 自动标题：取首条用户消息前 30 字符，去掉换行。
pub fn auto_title(content: &str) -> String {
    let flat: String = content
        .chars()
        .map(|ch| {
            if ch == '\n' || ch == '\r' || ch == '\t' {
                ' '
            } else {
                ch
            }
        })
        .collect();
    let title = truncate_chars(flat.trim(), 30);
    if title.trim().is_empty() {
        "新对话".to_string()
    } else {
        title
    }
}

// ---------------------------------------------------------------------------
// 消息
// ---------------------------------------------------------------------------

fn append_message_tx(
    conn: &mut Connection,
    message: &NewAiMessage,
) -> Result<AiMessage, CoreError> {
    let refs = serde_json::to_string(&message.context_refs)
        .map_err(|_| CoreError::validation("引用内容无法序列化"))?;
    let content = truncate_chars(&message.content, MAX_MESSAGE_CHARS);
    let tx = conn
        .transaction()
        .map_err(|e| crate::storage(format!("开始消息事务失败: {e}")))?;
    tx.execute(
        "INSERT INTO ai_messages(conversation_id, role, content, context_refs, status, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            message.conversation.0,
            message.role.as_str(),
            content,
            refs,
            message.status.as_str(),
            message.created_at
        ],
    )
    .store()?;
    let id = AiMessageId(tx.last_insert_rowid());
    tx.commit()
        .map_err(|e| crate::storage(format!("提交消息事务失败: {e}")))?;

    Ok(AiMessage {
        id,
        conversation: message.conversation,
        role: message.role,
        content,
        context_refs: message.context_refs.clone(),
        status: message.status,
        error: None,
        input_tokens: None,
        output_tokens: None,
        tool_trace: Vec::new(),
        plan: None,
        created_at: message.created_at,
        thinking: None,
    })
}

fn begin_assistant_tx(
    conn: &mut Connection,
    conversation: AiConversationId,
) -> Result<AiMessageId, CoreError> {
    let now = now_unix();
    let tx = conn
        .transaction()
        .map_err(|e| crate::storage(format!("开始助手消息事务失败: {e}")))?;
    tx.execute(
        "INSERT INTO ai_messages(conversation_id, role, content, status, created_at) \
         VALUES (?1, 'assistant', '', 'streaming', ?2)",
        params![conversation.0, now],
    )
    .store()?;
    let id = tx.last_insert_rowid();
    tx.execute(
        "UPDATE ai_conversations SET updated_at = ?2 WHERE id = ?1",
        params![conversation.0, now],
    )
    .store()?;
    tx.commit()
        .map_err(|e| crate::storage(format!("提交助手消息事务失败: {e}")))?;
    Ok(AiMessageId(id))
}

// 与端口定义同一条放宽理由：一次性写完整条消息，见 `AiRepo::finish_assistant_message`。
#[allow(clippy::too_many_arguments)]
fn finish_assistant_tx(
    conn: &mut Connection,
    id: AiMessageId,
    content: &str,
    status: AiMessageStatus,
    usage: Option<(Option<u32>, Option<u32>)>,
    tool_trace: &[AiToolTrace],
    plan: Option<&Plan>,
    thinking: Option<&ThinkingBlock>,
    error: Option<&str>,
) -> Result<(), CoreError> {
    let over_limit = content.chars().count() > MAX_MESSAGE_CHARS;
    let content = truncate_chars(content, MAX_MESSAGE_CHARS);
    let (status, error) = if over_limit {
        (AiMessageStatus::Failed, Some("回复超过单条上限，已截断"))
    } else {
        (status, error)
    };
    let trace_rows: Vec<ToolTraceRow> = tool_trace.iter().map(ToolTraceRow::from_trace).collect();
    let trace_json =
        serde_json::to_string(&trace_rows).map_err(|_| CoreError::storage("工具摘要无法序列化"))?;
    let plan_json = match plan {
        Some(plan) => {
            Some(serde_json::to_string(plan).map_err(|_| CoreError::storage("清单无法序列化"))?)
        }
        None => None,
    };
    let (input, output) = usage.unwrap_or((None, None));
    let thinking_json = match thinking {
        Some(block) => Some(
            serde_json::to_string(block).map_err(|_| CoreError::storage("思考过程无法序列化"))?,
        ),
        None => None,
    };

    conn.execute(
        "UPDATE ai_messages SET content = ?2, status = ?3, error = ?4, input_tokens = ?5, \
                output_tokens = ?6, tool_trace = ?7, plan = ?8, thinking = ?9 WHERE id = ?1",
        params![
            id.0,
            content,
            status.as_str(),
            error,
            input.map(|n| n as i64),
            output.map(|n| n as i64),
            trace_json,
            plan_json,
            thinking_json
        ],
    )
    .store()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 实体
// ---------------------------------------------------------------------------

fn count_entities_conn(conn: &Connection) -> Result<u32, CoreError> {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM ai_entities", [], |row| row.get(0))
        .store()?;
    Ok(count as u32)
}

fn insert_aliases(conn: &Connection, entity_id: i64, aliases: &[String]) -> Result<(), CoreError> {
    for alias in aliases {
        let normalized = normalize_entity_name(alias);
        if normalized.is_empty() {
            continue;
        }
        conn.execute(
            "INSERT OR IGNORE INTO ai_entity_aliases(entity_id, alias, normalized_alias) \
             VALUES (?1, ?2, ?3)",
            params![entity_id, alias.trim(), normalized],
        )
        .store()?;
    }
    Ok(())
}

fn validate_entity(entity: &NewAiEntity) -> Result<String, CoreError> {
    let normalized = normalize_entity_name(&entity.canonical_name);
    if normalized.is_empty() {
        return Err(CoreError::validation("实体名称不能为空"));
    }
    if entity.canonical_name.chars().count() > 120 {
        return Err(CoreError::validation("实体名称过长（上限 120 字符）"));
    }
    if entity.aliases.len() > 8 {
        return Err(CoreError::validation("别名最多 8 个"));
    }
    if entity.aliases.iter().any(|a| a.chars().count() > 120) {
        return Err(CoreError::validation("别名过长（上限 120 字符）"));
    }
    if let Some(email) = &entity.email
        && (email.chars().count() > 320 || email.contains(char::is_whitespace))
    {
        return Err(CoreError::validation("邮箱格式不合法"));
    }
    let attributes = entity.attributes.to_string();
    if attributes.len() > 1024 {
        return Err(CoreError::validation("属性过大（上限 1 KB）"));
    }
    Ok(normalized)
}

fn upsert_entity_tx(conn: &mut Connection, entity: &NewAiEntity) -> Result<AiEntity, CoreError> {
    let normalized = validate_entity(entity)?;
    let email = entity
        .email
        .as_deref()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());
    let attributes = entity.attributes.to_string();
    let now = now_unix();

    let tx = conn
        .transaction()
        .map_err(|e| crate::storage(format!("开始实体事务失败: {e}")))?;

    // 去重顺序：邮箱优先（跨类型唯一），其次 (kind, normalized_name)。
    let existing: Option<i64> = match email.as_deref() {
        Some(email) => tx
            .query_row(
                "SELECT id FROM ai_entities WHERE lower(email) = ?1",
                params![email],
                |row| row.get(0),
            )
            .optional()
            .store()?,
        None => None,
    };
    let existing = match existing {
        Some(id) => Some(id),
        None => tx
            .query_row(
                "SELECT id FROM ai_entities WHERE kind = ?1 AND normalized_name = ?2",
                params![entity.kind.as_str(), normalized],
                |row| row.get(0),
            )
            .optional()
            .store()?,
    };

    let id = match existing {
        Some(id) => {
            tx.execute(
                "UPDATE ai_entities SET last_seen_at = ?2, updated_at = ?2, \
                        email = COALESCE(email, ?3), attributes = ?4 WHERE id = ?1",
                params![id, now, email, attributes],
            )
            .store()?;
            id
        }
        None => {
            let count: i64 = tx
                .query_row("SELECT COUNT(*) FROM ai_entities", [], |row| row.get(0))
                .store()?;
            if count as u32 >= MAX_ENTITIES {
                return Err(CoreError::validation(format!(
                    "记忆已满（{MAX_ENTITIES} 条），请在设置中清理后再记录"
                )));
            }
            tx.execute(
                "INSERT INTO ai_entities(kind, canonical_name, normalized_name, email, account_id, \
                        calendar_id, attributes, source, confidence, created_at, updated_at, last_seen_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?10)",
                params![
                    entity.kind.as_str(),
                    entity.canonical_name.trim(),
                    normalized,
                    email,
                    entity.account.map(|a| a.0),
                    entity.calendar.map(|c| c.0),
                    attributes,
                    entity.source.as_str(),
                    f64::from(entity.confidence),
                    now
                ],
            )
            .store()?;
            tx.last_insert_rowid()
        }
    };

    insert_aliases(&tx, id, &entity.aliases)?;

    let sql = format!("SELECT {ENTITY_COLUMNS} FROM ai_entities e WHERE e.id = ?1");
    let mut saved = tx.query_row(&sql, params![id], map_entity_base).store()?;
    load_aliases(&tx, std::slice::from_mut(&mut saved))?;
    tx.commit()
        .map_err(|e| crate::storage(format!("提交实体事务失败: {e}")))?;
    Ok(saved)
}

// ---------------------------------------------------------------------------
// trait 实现
// ---------------------------------------------------------------------------

impl AiRepo for WritableStore {
    fn list_conversations(
        &self,
        before: Option<AiConversationId>,
        limit: u32,
    ) -> Result<Vec<AiConversation>, CoreError> {
        let conn = self.lock();
        list_conversations_conn(&conn, before, limit)
    }

    fn create_conversation(
        &self,
        title: &str,
        model_id: Option<&str>,
        config: &AiProviderConfig,
    ) -> Result<AiConversation, CoreError> {
        let mut conn = self.lock();
        create_conversation_tx(&mut conn, title, model_id, config)
    }

    fn set_conversation_model(
        &self,
        id: AiConversationId,
        model_id: &str,
        config: &AiProviderConfig,
    ) -> Result<(), CoreError> {
        let conn = self.lock();
        // 切模型也算「动过这个会话」，排到列表最前；但不改标题。
        set_conversation_model_tx(&conn, id, model_id, config, true)
    }

    fn rename_conversation(&self, id: AiConversationId, title: &str) -> Result<(), CoreError> {
        let title = validate_title(title)?;
        let conn = self.lock();
        let changed = conn
            .execute(
                "UPDATE ai_conversations SET title = ?2 WHERE id = ?1",
                params![id.0, title],
            )
            .store()?;
        if changed == 0 {
            return Err(CoreError::validation("会话不存在"));
        }
        Ok(())
    }

    fn delete_conversation(&self, id: AiConversationId) -> Result<(), CoreError> {
        let conn = self.lock();
        conn.execute("DELETE FROM ai_conversations WHERE id = ?1", params![id.0])
            .store()?;
        Ok(())
    }

    fn clear_conversations(&self) -> Result<u32, CoreError> {
        let conn = self.lock();
        let removed = conn.execute("DELETE FROM ai_conversations", []).store()?;
        Ok(removed as u32)
    }

    fn get_conversation(&self, id: AiConversationId) -> Result<Option<AiConversation>, CoreError> {
        let conn = self.lock();
        get_conversation_conn(&conn, id)
    }

    fn list_messages(
        &self,
        conversation: AiConversationId,
        before: Option<AiMessageId>,
        limit: u32,
    ) -> Result<Vec<AiMessage>, CoreError> {
        let conn = self.lock();
        list_messages_conn(&conn, conversation, before, limit)
    }

    fn append_message(&self, message: &NewAiMessage) -> Result<AiMessage, CoreError> {
        let mut conn = self.lock();
        append_message_tx(&mut conn, message)
    }

    fn delete_message(&self, id: AiMessageId) -> Result<(), CoreError> {
        let conn = self.lock();
        conn.execute("DELETE FROM ai_messages WHERE id = ?1", params![id.0])
            .store()?;
        Ok(())
    }

    fn begin_assistant_message(
        &self,
        conversation: AiConversationId,
    ) -> Result<AiMessageId, CoreError> {
        let mut conn = self.lock();
        begin_assistant_tx(&mut conn, conversation)
    }

    // 与端口定义保持同一条放宽理由：一次性写完整条消息。
    #[allow(clippy::too_many_arguments)]
    fn finish_assistant_message(
        &self,
        id: AiMessageId,
        content: &str,
        status: AiMessageStatus,
        usage: Option<(Option<u32>, Option<u32>)>,
        tool_trace: &[AiToolTrace],
        plan: Option<&Plan>,
        thinking: Option<&ThinkingBlock>,
        error: Option<&str>,
    ) -> Result<(), CoreError> {
        let mut conn = self.lock();
        finish_assistant_tx(
            &mut conn, id, content, status, usage, tool_trace, plan, thinking, error,
        )
    }

    fn checkpoint_assistant_message(
        &self,
        id: AiMessageId,
        content: &str,
        tool_trace: &[AiToolTrace],
        plan: Option<&Plan>,
        thinking: Option<&ThinkingBlock>,
    ) -> Result<(), CoreError> {
        let mut conn = self.lock();
        let tx = conn
            .transaction()
            .map_err(|e| crate::storage(format!("开始检查点事务失败: {e}")))?;
        let trace_rows: Vec<ToolTraceRow> =
            tool_trace.iter().map(ToolTraceRow::from_trace).collect();
        let trace_json = serde_json::to_string(&trace_rows)
            .map_err(|_| CoreError::storage("工具摘要无法序列化"))?;
        let plan_json = plan
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| CoreError::storage("清单无法序列化"))?;
        let thinking_json = thinking
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| CoreError::storage("思考过程无法序列化"))?;
        tx.execute(
            "UPDATE ai_messages SET content = ?2, tool_trace = ?3, plan = ?4, thinking = ?5 \
                    WHERE id = ?1 AND status = 'streaming'",
            params![
                id.0,
                truncate_chars(content, MAX_MESSAGE_CHARS),
                trace_json,
                plan_json,
                thinking_json
            ],
        )
        .store()?;
        tx.commit()
            .map_err(|e| crate::storage(format!("提交检查点失败: {e}")))?;
        Ok(())
    }

    fn touch_conversation(
        &self,
        id: AiConversationId,
        title: Option<&str>,
    ) -> Result<(), CoreError> {
        let now = now_unix();
        let conn = self.lock();
        match title {
            Some(title) => {
                let title = validate_title(title)?;
                conn.execute(
                    "UPDATE ai_conversations SET updated_at = ?2, title = ?3 WHERE id = ?1",
                    params![id.0, now, title],
                )
                .store()?;
            }
            None => {
                conn.execute(
                    "UPDATE ai_conversations SET updated_at = ?2 WHERE id = ?1",
                    params![id.0, now],
                )
                .store()?;
            }
        }
        Ok(())
    }

    fn fail_orphan_streaming_messages(&self) -> Result<u32, CoreError> {
        let conn = self.lock();
        // 先把「上一轮没写完的清单」读出来改好，再统一改状态。
        // 顺序不能反过来：改完 status 就找不到这些行了。
        //
        // 为什么不在 SQL 里改：清单是 JSON 数组里的某个字段，
        // `json_set` 需要 JSON1 扩展，且无法表达「只改非终态步骤」。
        let orphans: Vec<(i64, Option<String>)> = {
            let mut stmt = conn
                .prepare("SELECT id, plan FROM ai_messages WHERE status = 'streaming'")
                .store()?;
            let rows = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .store()?;
            rows.collect::<Result<Vec<_>, _>>().store()?
        };
        for (id, raw) in &orphans {
            let Some(mut plan) = parse_plan(raw.clone()) else {
                continue;
            };
            // 没改到任何一步就不写回，省一次 UPDATE。
            if !plan.skip_open("上次回复被中断，这一步没有执行") {
                continue;
            }
            let json =
                serde_json::to_string(&plan).map_err(|_| CoreError::storage("清单无法序列化"))?;
            conn.execute(
                "UPDATE ai_messages SET plan = ?2 WHERE id = ?1",
                params![id, json],
            )
            .store()?;
        }

        let fixed = conn
            .execute(
                "UPDATE ai_messages SET status = 'failed', error = '上次回复被中断' \
                 WHERE status = 'streaming'",
                [],
            )
            .store()?;
        Ok(fixed as u32)
    }
}

impl AiMemoryRepo for WritableStore {
    fn upsert_entity(&self, entity: &NewAiEntity) -> Result<AiEntity, CoreError> {
        let mut conn = self.lock();
        upsert_entity_tx(&mut conn, entity)
    }

    fn find_entities_by_alias(&self, alias: &str, limit: u32) -> Result<Vec<AiEntity>, CoreError> {
        let normalized = normalize_entity_name(alias);
        if normalized.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock();
        let limit = limit.clamp(1, 200) as i64;
        let sql = format!(
            "SELECT {ENTITY_COLUMNS} FROM ai_entities e \
             WHERE e.normalized_name = ?1 \
                OR EXISTS (SELECT 1 FROM ai_entity_aliases a \
                           WHERE a.entity_id = e.id AND a.normalized_alias = ?1) \
             ORDER BY e.last_seen_at DESC LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql).store()?;
        let mut rows = stmt
            .query_map(params![normalized, limit], map_entity_base)
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?;
        load_aliases(&conn, &mut rows)?;
        Ok(rows)
    }

    fn list_conversation_entities(
        &self,
        conversation: AiConversationId,
        limit: u32,
    ) -> Result<Vec<AiEntity>, CoreError> {
        let conn = self.lock();
        let limit = limit.clamp(1, 200) as i64;
        let sql = format!(
            "SELECT {ENTITY_COLUMNS} FROM ai_entities e \
             JOIN ai_conversation_entities ce ON ce.entity_id = e.id \
             WHERE ce.conversation_id = ?1 \
             ORDER BY ce.last_used_at DESC, e.id DESC LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql).store()?;
        let mut rows = stmt
            .query_map(params![conversation.0, limit], map_entity_base)
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?;
        load_aliases(&conn, &mut rows)?;
        Ok(rows)
    }

    fn link_conversation_entity(
        &self,
        conversation: AiConversationId,
        entity: i64,
        now: i64,
    ) -> Result<(), CoreError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO ai_conversation_entities(conversation_id, entity_id, last_used_at, use_count) \
             VALUES (?1, ?2, ?3, 1) \
             ON CONFLICT(conversation_id, entity_id) DO UPDATE SET \
                 last_used_at = excluded.last_used_at, use_count = use_count + 1",
            params![conversation.0, entity, now],
        )
        .store()?;
        Ok(())
    }

    fn touch_entity(&self, entity: i64, now: i64) -> Result<(), CoreError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE ai_entities SET last_seen_at = ?2 WHERE id = ?1",
            params![entity, now],
        )
        .store()?;
        Ok(())
    }

    fn list_entities(&self, limit: u32) -> Result<Vec<AiEntity>, CoreError> {
        let conn = self.lock();
        let limit = limit.clamp(1, 200) as i64;
        let sql = format!(
            "SELECT {ENTITY_COLUMNS} FROM ai_entities e \
             ORDER BY e.last_seen_at DESC, e.id DESC LIMIT ?1"
        );
        let mut stmt = conn.prepare(&sql).store()?;
        let mut rows = stmt
            .query_map(params![limit], map_entity_base)
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?;
        load_aliases(&conn, &mut rows)?;
        Ok(rows)
    }

    fn count_entities(&self) -> Result<u32, CoreError> {
        let conn = self.lock();
        count_entities_conn(&conn)
    }

    fn delete_entity(&self, entity: i64) -> Result<(), CoreError> {
        let conn = self.lock();
        conn.execute("DELETE FROM ai_entities WHERE id = ?1", params![entity])
            .store()?;
        Ok(())
    }

    fn clear_entities(&self) -> Result<u32, CoreError> {
        let conn = self.lock();
        let removed = conn.execute("DELETE FROM ai_entities", []).store()?;
        Ok(removed as u32)
    }
}

// ---------------------------------------------------------------------------
// 只读辅助（供 ReadOnlyRepo 使用）
// ---------------------------------------------------------------------------

pub(crate) fn list_conversations_readonly(
    store: &ReadOnlyStore,
    before: Option<AiConversationId>,
    limit: u32,
) -> Result<Vec<AiConversation>, CoreError> {
    let conn = store.conn()?;
    list_conversations_conn(&conn, before, limit)
}

pub(crate) fn list_messages_readonly(
    store: &ReadOnlyStore,
    conversation: AiConversationId,
    before: Option<AiMessageId>,
    limit: u32,
) -> Result<Vec<AiMessage>, CoreError> {
    let conn = store.conn()?;
    list_messages_conn(&conn, conversation, before, limit)
}

pub(crate) fn get_message_readonly(
    store: &ReadOnlyStore,
    id: AiMessageId,
) -> Result<Option<AiMessage>, CoreError> {
    let conn = store.conn()?;
    get_message_conn(&conn, id)
}

pub(crate) fn list_entities_readonly(
    store: &ReadOnlyStore,
    limit: u32,
) -> Result<Vec<AiEntity>, CoreError> {
    let conn = store.conn()?;
    let limit = limit.clamp(1, 200) as i64;
    let sql = format!(
        "SELECT {ENTITY_COLUMNS} FROM ai_entities e \
         ORDER BY e.last_seen_at DESC, e.id DESC LIMIT ?1"
    );
    let mut stmt = conn.prepare(&sql).store()?;
    let mut rows = stmt
        .query_map(params![limit], map_entity_base)
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    load_aliases(&conn, &mut rows)?;
    Ok(rows)
}

pub(crate) fn count_entities_readonly(store: &ReadOnlyStore) -> Result<u32, CoreError> {
    let conn = store.conn()?;
    count_entities_conn(&conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_core::ai_plan::{Capability, PlanStatus, PlanStep};
    use me_core::port::ReadOnlyRepo;

    /// 上次崩溃留下的 `streaming` 行：状态改成 failed 的同时，
    /// 清单里没做完的步也要标成跳过——否则界面会永远有一个转圈的「进行中」。
    ///
    /// 放在 crate 内部而不是 `tests/`：需要直接改库把状态退回 streaming，
    /// 为此在 `WritableStore` 上开一个「随便执行 SQL」的公开方法不值得。
    #[test]
    fn orphan_streaming_message_skips_open_plan_steps() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::Store::open(dir.path()).expect("open");
        let writable = store.writable();
        let config = me_core::ai::AiProviderConfig::preset(me_core::ai::AiProviderKind::Anthropic);
        let conversation = writable
            .create_conversation("中断", Some("m1"), &config)
            .expect("create");
        let id = writable
            .begin_assistant_message(conversation.id)
            .expect("begin");

        let plan = Plan {
            id: "p_1".to_string(),
            goal: "跑两步".to_string(),
            steps: vec![
                PlanStep::new(1, "第一步", Capability::Mail, PlanStatus::Done)
                    .with_evidence("已完成"),
                PlanStep::new(2, "第二步", Capability::Weather, PlanStatus::Running),
            ],
            created_at: 1,
            updated_at: 1,
            ..Plan::default()
        };
        writable
            .finish_assistant_message(
                id,
                "",
                AiMessageStatus::Complete,
                None,
                &[],
                Some(&plan),
                None,
                None,
            )
            .expect("finish");

        // 退回 streaming，模拟「写到一半断电」。
        writable
            .lock()
            .execute(
                "UPDATE ai_messages SET status = 'streaming' WHERE id = ?1",
                params![id.0],
            )
            .store()
            .expect("改回 streaming");

        let fixed = writable.fail_orphan_streaming_messages().expect("修复");
        assert_eq!(fixed, 1);

        let loaded = store
            .read_only()
            .get_ai_message_readonly(id)
            .expect("get")
            .expect("存在");
        assert_eq!(loaded.status, AiMessageStatus::Failed);
        let plan = loaded.plan.expect("清单仍在");
        assert_eq!(plan.steps[0].status, PlanStatus::Done, "已完成的步不动");
        assert_eq!(plan.steps[0].evidence.as_deref(), Some("已完成"));
        assert_eq!(plan.steps[1].status, PlanStatus::Skipped);
        assert_eq!(plan.open_steps(), 0);
    }
}
