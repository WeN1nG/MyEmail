//! schema 初始化与 v1 → v2 迁移。
//!
//! DDL 的唯一事实源是 `doc/数据结构.md` §4.2 / §4.3（v0.2 评审后回写）；
//! 迁移规则见 `doc/v02架构设计.md` §4.3 / §4.4。
//!
//! v2 只用「纯加列 + 新索引」，不改列类型/CHECK/NOT NULL。每个迁移步骤在
//! 单事务内执行；失败不写 `schema_version`，启动中止。

use std::path::{Path, PathBuf};

use me_core::CoreError;
use me_core::SCHEMA_VERSION;
use me_core::port::MetaRepo;
use rusqlite::{Connection, OptionalExtension, params};

use crate::db::WritableStore;
use crate::storage;

/// v2 DDL（v0.2 形态）。新库先建 v2，再建 v3 增量。
pub(crate) const SCHEMA_V2_DDL: &str = r#"
CREATE TABLE accounts (
    id            INTEGER PRIMARY KEY,
    email         TEXT NOT NULL UNIQUE,
    display_name  TEXT,
    imap_host     TEXT NOT NULL,
    imap_port     INTEGER NOT NULL,
    imap_tls      TEXT NOT NULL,        -- implicit | starttls | none
    smtp_host     TEXT NOT NULL,
    smtp_port     INTEGER NOT NULL,
    smtp_tls      TEXT NOT NULL,
    auth_user     TEXT NOT NULL,
    auth_method   TEXT NOT NULL DEFAULT 'password',  -- password | app_password | oauth2
    oauth_provider TEXT,                -- google | microsoft
    oauth_subject  TEXT,                -- 服务商稳定用户标识（可选）
    token_expires_at INTEGER,           -- unix 秒；仅展示/诊断，不参与令牌有效性判断
    needs_reauth   INTEGER NOT NULL DEFAULT 0,
    idle_enabled   INTEGER NOT NULL DEFAULT 1,
    poll_interval_secs INTEGER,         -- NULL = 跟随全局默认；非 NULL 时 60～3600
    created_at    INTEGER NOT NULL
);

CREATE TABLE folders (
    id           INTEGER PRIMARY KEY,
    account_id   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,         -- 服务器原名（modified UTF-7 解码后）
    delimiter    TEXT,
    role         TEXT NOT NULL DEFAULT 'other'
                 CHECK (role IN ('inbox','sent','drafts','trash','junk','archive','other')),
    uid_validity INTEGER NOT NULL DEFAULT 0,
    last_uid     INTEGER NOT NULL DEFAULT 0,
    deleted_on_server INTEGER NOT NULL DEFAULT 0,  -- 服务器已删除、本地待确认
    UNIQUE(account_id, name)
);
CREATE UNIQUE INDEX idx_folders_trash ON folders(account_id) WHERE role = 'trash';

CREATE TABLE messages (
    id             INTEGER PRIMARY KEY,
    account_id     INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    folder_id      INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    uid            INTEGER,             -- NULL = 本地已移动、服务器身份待确认
    message_id     TEXT,
    subject        TEXT,
    from_addr      TEXT,
    from_name      TEXT,
    to_addrs       TEXT,                -- JSON 数组
    cc_addrs       TEXT,                -- JSON 数组
    date           INTEGER,             -- unix 秒
    date_source    TEXT NOT NULL DEFAULT 'header',
    size           INTEGER,
    flags          INTEGER NOT NULL DEFAULT 0,
    snippet        TEXT,
    has_attachment INTEGER NOT NULL DEFAULT 0,
    body_path      TEXT,                -- NULL = 正文未缓存；非 NULL = 已写入 FTS
    UNIQUE(folder_id, uid)
);
CREATE INDEX idx_messages_folder_date ON messages(folder_id, date DESC);
CREATE INDEX idx_messages_account ON messages(account_id);
CREATE INDEX idx_messages_body_pending ON messages(account_id, folder_id, uid) WHERE body_path IS NULL;
CREATE INDEX idx_messages_message_id ON messages(folder_id, message_id);

CREATE TABLE attachments (
    id         INTEGER PRIMARY KEY,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    part_index INTEGER NOT NULL,
    filename   TEXT,
    mime_type  TEXT,
    size       INTEGER,
    local_path TEXT,
    is_inline  INTEGER NOT NULL DEFAULT 0,
    content_id TEXT,
    UNIQUE(message_id, part_index)
);

CREATE TABLE drafts (
    id           INTEGER PRIMARY KEY,
    account_id   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    raw_path     TEXT NOT NULL,
    message_id   TEXT NOT NULL,
    subject      TEXT,
    to_addrs     TEXT,                  -- JSON 数组
    bcc_addrs    TEXT,                  -- JSON 数组；不写入 RFC822
    status       TEXT NOT NULL CHECK (status IN ('draft','sent')),
    server_uid   INTEGER,
    server_uid_validity INTEGER,
    server_hash  TEXT,                  -- 上次同步的服务器规范化哈希
    local_hash   TEXT,                  -- 上次同步的本地规范化哈希
    sync_state   TEXT NOT NULL DEFAULT 'local_only',
    conflict_of  INTEGER,               -- 冲突副本指向原草稿（逻辑外键）
    last_synced_at INTEGER,
    sync_attempts INTEGER NOT NULL DEFAULT 0,
    deleted_at   INTEGER,               -- 本地删除墓碑
    last_error   TEXT,
    updated_at   INTEGER NOT NULL
);
CREATE INDEX idx_drafts_account_state ON drafts(account_id, sync_state);
CREATE UNIQUE INDEX idx_drafts_server_identity
    ON drafts(account_id, COALESCE(server_uid_validity, 0), server_uid)
    WHERE server_uid IS NOT NULL AND deleted_at IS NULL;
CREATE INDEX idx_drafts_message_id ON drafts(account_id, message_id);

CREATE VIRTUAL TABLE messages_fts USING fts5(
    subject, body, from_addr, to_addrs,
    tokenize='trigram'
);

CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);

-- 待同步标志：同一邮件的多次变更合并为最终态
CREATE TABLE pending_flags (
    message_id   INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    target_flags INTEGER NOT NULL,      -- 目标态，非增量
    attempts     INTEGER NOT NULL DEFAULT 0,
    last_error   TEXT,
    updated_at   INTEGER NOT NULL
);

-- 待同步操作：本地已生效、服务器尚未确认的移动 / 永久删除
CREATE TABLE pending_ops (
    id               INTEGER PRIMARY KEY,
    account_id       INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind             TEXT NOT NULL,     -- move_to_trash | delete_permanent | empty_folder
    message_id       INTEGER REFERENCES messages(id) ON DELETE SET NULL,
    source_folder_id INTEGER REFERENCES folders(id) ON DELETE SET NULL,
    source_folder_name TEXT,            -- 文件夹被删后仍可读
    source_uid       INTEGER,           -- 移动前所在文件夹的 UID
    target_folder_id INTEGER REFERENCES folders(id) ON DELETE SET NULL,
    attempts         INTEGER NOT NULL DEFAULT 0,
    last_error       TEXT,
    created_at       INTEGER NOT NULL
);
CREATE INDEX idx_pending_ops_account ON pending_ops(account_id, attempts);
"#;

/// v3 增量 DDL：**只新增表与索引**，不触碰 v1/v2 的既有表。
///
/// 新建库与 v2→v3 迁移共用这一份 SQL（`create_schema` 与 `migrate_v2_to_v3`
/// 都执行它），因此两条路径的物理结构必然一致，不存在迁移漂移。
///
/// 日历之外的表（天气 / AI）在 v0.3 建全但暂不使用：schema v3 在 M13 冻结，
/// 事后补表要付出一次 v4 迁移。空表不影响读写。
const SCHEMA_V3_DDL: &str = r#"
CREATE TABLE calendars (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL,
    color      TEXT NOT NULL DEFAULT '#3B82F6',
    visible    INTEGER NOT NULL DEFAULT 1 CHECK (visible IN (0,1)),
    is_default INTEGER NOT NULL DEFAULT 0 CHECK (is_default IN (0,1)),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX ux_calendars_default
    ON calendars(is_default) WHERE is_default = 1;

CREATE TABLE calendar_events (
    id          INTEGER PRIMARY KEY,
    calendar_id INTEGER NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
    uid         TEXT NOT NULL,
    summary     TEXT NOT NULL DEFAULT '',
    description TEXT,
    location    TEXT,
    start_utc   INTEGER NOT NULL,
    end_utc     INTEGER NOT NULL,
    all_day     INTEGER NOT NULL DEFAULT 0 CHECK (all_day IN (0,1)),
    timezone    TEXT,
    rrule       TEXT,
    recurrence_until INTEGER,
    exdates     TEXT NOT NULL DEFAULT '[]',
    reminders   TEXT NOT NULL DEFAULT '[]',
    status      TEXT NOT NULL DEFAULT 'confirmed'
                CHECK (status IN ('confirmed','tentative','cancelled')),
    source      TEXT NOT NULL DEFAULT 'local'
                CHECK (source IN ('local','ics','ai')),
    sequence    INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    deleted_at  INTEGER,
    CHECK (end_utc >= start_utc),
    CHECK (recurrence_until IS NULL OR recurrence_until >= start_utc)
);
CREATE UNIQUE INDEX ux_calendar_events_uid
    ON calendar_events(calendar_id, uid) WHERE deleted_at IS NULL;
CREATE INDEX ix_calendar_events_range
    ON calendar_events(calendar_id, start_utc, end_utc);
CREATE INDEX ix_calendar_events_recurring
    ON calendar_events(calendar_id, start_utc) WHERE rrule IS NOT NULL AND deleted_at IS NULL;
CREATE INDEX ix_calendar_events_updated
    ON calendar_events(updated_at);
CREATE INDEX ix_calendar_events_reminder
    ON calendar_events(start_utc) WHERE reminders <> '[]' AND deleted_at IS NULL;

CREATE TABLE calendar_event_exceptions (
    id            INTEGER PRIMARY KEY,
    event_id      INTEGER NOT NULL REFERENCES calendar_events(id) ON DELETE CASCADE,
    recurrence_at INTEGER NOT NULL,
    cancelled     INTEGER NOT NULL DEFAULT 0 CHECK (cancelled IN (0,1)),
    summary       TEXT,
    description   TEXT,
    location      TEXT,
    start_utc     INTEGER,
    end_utc       INTEGER,
    all_day       INTEGER,
    updated_at    INTEGER NOT NULL,
    UNIQUE(event_id, recurrence_at)
);

CREATE TABLE calendar_reminder_fires (
    event_id       INTEGER NOT NULL REFERENCES calendar_events(id) ON DELETE CASCADE,
    occurrence_at  INTEGER NOT NULL,
    minutes_before INTEGER NOT NULL,
    fired_at       INTEGER NOT NULL,
    PRIMARY KEY (event_id, occurrence_at, minutes_before)
);
CREATE INDEX ix_calendar_reminder_fires_time
    ON calendar_reminder_fires(fired_at);

-- v0.3 尚未接入天气 / AI，表结构先建全，避免二次 schema 迁移。
CREATE TABLE weather_locations (
    id                   INTEGER PRIMARY KEY,
    provider             TEXT NOT NULL,
    provider_location_id TEXT NOT NULL,
    name                 TEXT NOT NULL,
    admin1               TEXT,
    admin2               TEXT,
    country              TEXT,
    latitude             REAL NOT NULL CHECK (latitude BETWEEN -90 AND 90),
    longitude            REAL NOT NULL CHECK (longitude BETWEEN -180 AND 180),
    timezone             TEXT NOT NULL,
    is_default           INTEGER NOT NULL DEFAULT 0 CHECK (is_default IN (0,1)),
    sort_order           INTEGER NOT NULL DEFAULT 0,
    created_at           INTEGER NOT NULL
);
CREATE UNIQUE INDEX ux_weather_default
    ON weather_locations(is_default) WHERE is_default = 1;
CREATE UNIQUE INDEX ux_weather_location
    ON weather_locations(provider, provider_location_id);

CREATE TABLE weather_snapshots (
    location_id INTEGER PRIMARY KEY REFERENCES weather_locations(id) ON DELETE CASCADE,
    provider    TEXT NOT NULL,
    fetched_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    payload     TEXT NOT NULL,
    CHECK (expires_at >= fetched_at)
);

CREATE TABLE ai_conversations (
    id         INTEGER PRIMARY KEY,
    title      TEXT NOT NULL,
    provider   TEXT NOT NULL CHECK (provider IN ('anthropic','openai_compatible')),
    model      TEXT NOT NULL,
    -- 绑定的模型档案 id。v3 库升级时由 v3→v4 迁移补上（见 migrate_v3_to_v4）。
    model_id   TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX ix_ai_conversations_updated
    ON ai_conversations(updated_at DESC);

CREATE TABLE ai_messages (
    id              INTEGER PRIMARY KEY,
    conversation_id INTEGER NOT NULL REFERENCES ai_conversations(id) ON DELETE CASCADE,
    role            TEXT NOT NULL CHECK (role IN ('user','assistant')),
    content         TEXT NOT NULL DEFAULT '',
    context_refs    TEXT,
    status          TEXT NOT NULL DEFAULT 'complete'
                    CHECK (status IN ('streaming','complete','failed','cancelled')),
    error           TEXT,
    input_tokens    INTEGER,
    output_tokens   INTEGER,
    tool_trace      TEXT,
    -- 本轮任务清单（JSON）。v5 起新建库直接带上；v4 及更早的库由
    -- migrate_v4_to_v5 补列（见该函数）。
    plan            TEXT,
    created_at      INTEGER NOT NULL
);
CREATE INDEX ix_ai_messages_conversation
    ON ai_messages(conversation_id, id);

CREATE TABLE ai_entities (
    id             INTEGER PRIMARY KEY,
    kind           TEXT NOT NULL
                   CHECK (kind IN ('person','organization','location','project','event','other')),
    canonical_name TEXT NOT NULL,
    normalized_name TEXT NOT NULL,
    email          TEXT,
    account_id     INTEGER REFERENCES accounts(id) ON DELETE SET NULL,
    calendar_id    INTEGER REFERENCES calendars(id) ON DELETE SET NULL,
    attributes     TEXT NOT NULL DEFAULT '{}',
    source         TEXT NOT NULL
                   CHECK (source IN ('tool','user','ai_inferred','import')),
    confidence     REAL NOT NULL DEFAULT 1.0,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    last_seen_at   INTEGER NOT NULL
);
CREATE UNIQUE INDEX ux_ai_entities_name
    ON ai_entities(kind, normalized_name);
CREATE UNIQUE INDEX ux_ai_entities_email
    ON ai_entities(lower(email)) WHERE email IS NOT NULL;
CREATE INDEX ix_ai_entities_last_seen
    ON ai_entities(last_seen_at DESC);

CREATE TABLE ai_entity_aliases (
    entity_id       INTEGER NOT NULL REFERENCES ai_entities(id) ON DELETE CASCADE,
    alias           TEXT NOT NULL,
    normalized_alias TEXT NOT NULL,
    PRIMARY KEY (entity_id, normalized_alias)
);
CREATE INDEX ix_ai_entity_aliases_match
    ON ai_entity_aliases(normalized_alias);

CREATE TABLE ai_conversation_entities (
    conversation_id INTEGER NOT NULL REFERENCES ai_conversations(id) ON DELETE CASCADE,
    entity_id       INTEGER NOT NULL REFERENCES ai_entities(id) ON DELETE CASCADE,
    last_used_at    INTEGER NOT NULL,
    use_count       INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (conversation_id, entity_id)
);
CREATE INDEX ix_ai_conversation_entities_recent
    ON ai_conversation_entities(conversation_id, last_used_at DESC);

-- 默认日历：界面第一次打开就有可写目标，无需「先建日历再建事件」。
INSERT INTO calendars(name, color, visible, is_default, created_at, updated_at)
SELECT '我的日历', '#3B82F6', 1, 1,
       CAST(strftime('%s','now') AS INTEGER), CAST(strftime('%s','now') AS INTEGER)
 WHERE NOT EXISTS (SELECT 1 FROM calendars);
"#;

/// v1 → v2 的纯加列 + 索引 + 数据整理。
///
/// 注意：`messages` 清理依赖 `ON DELETE CASCADE`，调用方必须保持外键开启。
const MIGRATE_V1_TO_V2_SQL: &str = r#"
ALTER TABLE accounts ADD COLUMN oauth_provider TEXT;
ALTER TABLE accounts ADD COLUMN oauth_subject TEXT;
ALTER TABLE accounts ADD COLUMN token_expires_at INTEGER;
ALTER TABLE accounts ADD COLUMN needs_reauth INTEGER NOT NULL DEFAULT 0;
ALTER TABLE accounts ADD COLUMN idle_enabled INTEGER NOT NULL DEFAULT 1;
ALTER TABLE accounts ADD COLUMN poll_interval_secs INTEGER;

ALTER TABLE drafts ADD COLUMN bcc_addrs TEXT;
ALTER TABLE drafts ADD COLUMN server_uid_validity INTEGER;
ALTER TABLE drafts ADD COLUMN server_hash TEXT;
ALTER TABLE drafts ADD COLUMN local_hash TEXT;
ALTER TABLE drafts ADD COLUMN sync_state TEXT NOT NULL DEFAULT 'local_only';
ALTER TABLE drafts ADD COLUMN conflict_of INTEGER;
ALTER TABLE drafts ADD COLUMN last_synced_at INTEGER;
ALTER TABLE drafts ADD COLUMN sync_attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE drafts ADD COLUMN deleted_at INTEGER;

CREATE INDEX idx_drafts_account_state ON drafts(account_id, sync_state);
CREATE UNIQUE INDEX idx_drafts_server_identity
    ON drafts(account_id, COALESCE(server_uid_validity, 0), server_uid)
    WHERE server_uid IS NOT NULL AND deleted_at IS NULL;
CREATE INDEX idx_drafts_message_id ON drafts(account_id, message_id);

-- v0.1 草稿没有同步基线：有服务器 UID 的按本地优先重推，没有的等待首次确认。
UPDATE drafts
   SET sync_state = CASE WHEN server_uid IS NULL THEN 'local_only' ELSE 'local_dirty' END
 WHERE status = 'draft';
UPDATE drafts SET sync_state = 'local_only' WHERE status = 'sent';

-- 存在从未上传过的草稿时，首次启动要求一次性确认，避免静默上传。
INSERT INTO meta(key, value)
SELECT 'draft_bulk_upload_pending', 'true'
 WHERE EXISTS (
   SELECT 1 FROM drafts
    WHERE status = 'draft' AND server_uid IS NULL AND deleted_at IS NULL
 )
ON CONFLICT(key) DO UPDATE SET value = excluded.value;

-- v0.1 曾把服务器草稿当普通邮件同步；v0.2 草稿只走 drafts 表。
DELETE FROM messages_fts
 WHERE rowid IN (
   SELECT m.id FROM messages m
     JOIN folders f ON f.id = m.folder_id
    WHERE f.role = 'drafts'
 );
DELETE FROM messages
 WHERE id IN (
   SELECT m.id FROM messages m
     JOIN folders f ON f.id = m.folder_id
    WHERE f.role = 'drafts'
 );

UPDATE meta SET value = '2' WHERE key = 'schema_version';
"#;

/// 初始化或升级 schema，并返回可继续使用的连接。
///
/// 版本为 1 时：checkpoint WAL → 关闭连接 → 备份 `myemail.db.bak-v1` →
/// 重新打开 → 单事务迁移。任一步失败则不写版本号并返回错误。
pub(crate) fn ensure_schema(mut conn: Connection, db_path: &Path) -> Result<Connection, CoreError> {
    let has_meta: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| storage(format!("读取 schema 状态失败: {e}")))?;

    if !has_meta {
        create_schema(&mut conn)?;
        return Ok(conn);
    }

    match read_schema_version_conn(&conn)? {
        Some(version) if version == SCHEMA_VERSION => Ok(conn),
        Some(version) if version > SCHEMA_VERSION => Err(storage(format!(
            "数据库 schema_version={version} 高于本程序支持的 {SCHEMA_VERSION}，请升级应用"
        ))),
        Some(1) => {
            // 备份前先把 WAL 合回主库；备份只复制主文件。
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .map_err(|e| storage(format!("迁移前 WAL checkpoint 失败: {e}")))?;
            drop(conn);

            let backup = backup_v1(db_path)?;
            let mut conn = Connection::open(db_path)
                .map_err(|e| storage(format!("迁移时重新打开数据库失败: {e}")))?;
            crate::db::apply_pragmas(&conn)
                .map_err(|e| storage(format!("迁移时设置 PRAGMA 失败: {e}")))?;

            let started = std::time::Instant::now();
            migrate_v1_to_v2(&mut conn)?;
            migrate_v2_to_v3(&mut conn)?;
            migrate_v3_to_v4(&mut conn)?;
            migrate_v4_to_v5(&mut conn)?;
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                backup = %backup.display(),
                "database migrated v1 -> v5"
            );
            Ok(conn)
        }
        Some(2) => {
            let started = std::time::Instant::now();
            migrate_v2_to_v3(&mut conn)?;
            migrate_v3_to_v4(&mut conn)?;
            migrate_v4_to_v5(&mut conn)?;
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                "database migrated v2 -> v5"
            );
            Ok(conn)
        }
        Some(3) => {
            let started = std::time::Instant::now();
            migrate_v3_to_v4(&mut conn)?;
            migrate_v4_to_v5(&mut conn)?;
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                "database migrated v3 -> v5"
            );
            Ok(conn)
        }
        Some(4) => {
            let started = std::time::Instant::now();
            migrate_v4_to_v5(&mut conn)?;
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                "database migrated v4 -> v5"
            );
            Ok(conn)
        }
        Some(version) => Err(storage(format!(
            "数据库升级失败：schema_version={version} 需要迁移到 {SCHEMA_VERSION}，但没有可用迁移路径"
        ))),
        None => Err(storage("数据库升级失败：meta 表缺少 schema_version")),
    }
}

fn create_schema(conn: &mut Connection) -> Result<(), CoreError> {
    let tx = conn
        .transaction()
        .map_err(|e| storage(format!("开始 schema 事务失败: {e}")))?;
    tx.execute_batch(SCHEMA_V2_DDL)
        .map_err(|e| storage(format!("创建 schema 失败: {e}")))?;
    tx.execute_batch(SCHEMA_V3_DDL)
        .map_err(|e| storage(format!("创建 v3 schema 失败: {e}")))?;
    tx.execute(
        "INSERT INTO meta(key, value) VALUES ('schema_version', ?1)",
        params![SCHEMA_VERSION.to_string()],
    )
    .map_err(|e| storage(format!("写入 schema_version 失败: {e}")))?;
    tx.commit()
        .map_err(|e| storage(format!("提交 schema 失败: {e}")))?;
    Ok(())
}

fn backup_v1(db_path: &Path) -> Result<PathBuf, CoreError> {
    let mut name = db_path.as_os_str().to_owned();
    name.push(".bak-v1");
    let backup = PathBuf::from(name);
    if backup.exists() {
        tracing::info!(backup = %backup.display(), "v1 backup already exists; keeping it");
        return Ok(backup);
    }
    std::fs::copy(db_path, &backup).map_err(|e| {
        storage(format!(
            "迁移备份失败（{} → {}）: {e}",
            db_path.display(),
            backup.display()
        ))
    })?;
    Ok(backup)
}

fn migrate_v1_to_v2(conn: &mut Connection) -> Result<(), CoreError> {
    let tx = conn
        .transaction()
        .map_err(|e| storage(format!("开始 v1→v2 迁移事务失败: {e}")))?;

    tx.execute_batch(MIGRATE_V1_TO_V2_SQL)
        .map_err(|e| storage(format!("执行 v1→v2 迁移失败: {e}")))?;

    // 迁移必须保持 foreign_keys=ON；任何悬空子行都会在这里暴露。
    check_foreign_keys(&tx)?;

    let version: Option<String> = tx
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| storage(format!("迁移后读取 schema_version 失败: {e}")))?;
    if version.as_deref() != Some("2") {
        return Err(storage("迁移后 schema_version 未更新为 2"));
    }

    tx.commit()
        .map_err(|e| storage(format!("提交 v1→v2 迁移失败: {e}")))?;
    Ok(())
}

/// v2 → v3：纯新增表与索引，不改既有表，因此不需要备份文件。
///
/// 失败时整个事务回滚，`schema_version` 保持 2，下次启动可重试。
fn migrate_v2_to_v3(conn: &mut Connection) -> Result<(), CoreError> {
    // 注意目标版本写成固定的 3，而不是 SCHEMA_VERSION：
    // 若写 SCHEMA_VERSION，v2 库会被标成「已是最新」，v3→v4 的补列就被跳过了。
    const TARGET: u32 = 3;
    let tx = conn
        .transaction()
        .map_err(|e| storage(format!("开始 v2→v3 迁移事务失败: {e}")))?;

    tx.execute_batch(SCHEMA_V3_DDL)
        .map_err(|e| storage(format!("执行 v2→v3 迁移失败: {e}")))?;

    tx.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
        params![TARGET.to_string()],
    )
    .map_err(|e| storage(format!("写入 schema_version 失败: {e}")))?;

    check_foreign_keys(&tx)?;

    let version: Option<String> = tx
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| storage(format!("迁移后读取 schema_version 失败: {e}")))?;
    if version.as_deref() != Some("3") {
        return Err(storage("迁移后 schema_version 未更新为 3"));
    }

    tx.commit()
        .map_err(|e| storage(format!("提交 v2→v3 迁移失败: {e}")))?;
    Ok(())
}

/// v3 → v4：给会话补上模型档案 id。
///
/// 幂等：新建库的 `ai_conversations` 已经带 `model_id`（DDL 里直接有），
/// 而 v2 库刚跑完 v3 时同样带列。所以先查 `PRAGMA table_info`，缺了才加。
/// 靠版本号判断不可行——两条路径到这里时列的情况不同。
fn migrate_v3_to_v4(conn: &mut Connection) -> Result<(), CoreError> {
    // 目标版本写死 4：写 `SCHEMA_VERSION` 会让 v3 库被一步标成最新版本，
    // 后面的 v4→v5 补列就被跳过了（v2→v3 的注释里踩过同一个坑）。
    const TARGET: u32 = 4;
    let tx = conn
        .transaction()
        .map_err(|e| storage(format!("开始 v3→v4 迁移事务失败: {e}")))?;

    let has_column: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('ai_conversations') WHERE name = 'model_id')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| storage(format!("读取 ai_conversations 列信息失败: {e}")))?;
    if !has_column {
        tx.execute_batch("ALTER TABLE ai_conversations ADD COLUMN model_id TEXT;")
            .map_err(|e| storage(format!("执行 v3→v4 迁移失败: {e}")))?;
    }

    tx.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
        params![TARGET.to_string()],
    )
    .map_err(|e| storage(format!("写入 schema_version 失败: {e}")))?;

    check_foreign_keys(&tx)?;
    tx.commit()
        .map_err(|e| storage(format!("提交 v3→v4 迁移失败: {e}")))?;
    Ok(())
}

/// v4 → v5：给 `ai_messages` 补一列 `plan`（本轮任务清单 JSON）。
///
/// 纯加列，不改任何既有约束——`ai_messages` 上有 CHECK（role/status），
/// 动它们就得重建表，风险远大于收益。
///
/// 按列存在性判断而不是无条件 `ALTER`：新建库走 `create_schema`，
/// 它的 `SCHEMA_V3_DDL` 已经带上了这一列，重复 ALTER 会报「duplicate column」。
fn migrate_v4_to_v5(conn: &mut Connection) -> Result<(), CoreError> {
    const TARGET: u32 = 5;
    let tx = conn
        .transaction()
        .map_err(|e| storage(format!("开始 v4→v5 迁移事务失败: {e}")))?;

    let has_column: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('ai_messages') WHERE name = 'plan')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| storage(format!("读取 ai_messages 列信息失败: {e}")))?;
    if !has_column {
        tx.execute_batch("ALTER TABLE ai_messages ADD COLUMN plan TEXT;")
            .map_err(|e| storage(format!("执行 v4→v5 迁移失败: {e}")))?;
    }

    tx.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
        params![TARGET.to_string()],
    )
    .map_err(|e| storage(format!("写入 schema_version 失败: {e}")))?;

    check_foreign_keys(&tx)?;
    tx.commit()
        .map_err(|e| storage(format!("提交 v4→v5 迁移失败: {e}")))?;
    Ok(())
}

/// 迁移必须保持 `foreign_keys = ON`；任何悬空子行都会在这里暴露。
fn check_foreign_keys(tx: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
    let mut stmt = tx
        .prepare("PRAGMA foreign_key_check")
        .map_err(|e| storage(format!("迁移后外键检查失败: {e}")))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| storage(format!("迁移后外键检查失败: {e}")))?;
    if let Some(row) = rows
        .next()
        .map_err(|e| storage(format!("迁移后外键检查失败: {e}")))?
    {
        let table: String = row.get(0).unwrap_or_default();
        let rowid: i64 = row.get(1).unwrap_or_default();
        return Err(storage(format!(
            "迁移后外键检查失败：表 {table} 的第 {rowid} 行存在悬空引用"
        )));
    }
    Ok(())
}

pub(crate) fn read_schema_version_conn(conn: &Connection) -> Result<Option<u32>, CoreError> {
    read_setting_conn(conn, me_core::settings_key::SCHEMA_VERSION)
        .map(|value| value.and_then(|v| v.parse::<u32>().ok()))
}

fn read_setting_conn(conn: &Connection, key: &str) -> Result<Option<String>, CoreError> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(|e| storage(format!("读取 meta.{key} 失败: {e}")))
}

fn write_setting_conn(conn: &Connection, key: &str, value: &str) -> Result<(), CoreError> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map_err(|e| storage(format!("写入 meta.{key} 失败: {e}")))?;
    Ok(())
}

impl MetaRepo for WritableStore {
    fn schema_version(&self) -> Result<u32, CoreError> {
        let conn = self.lock();
        Ok(read_schema_version_conn(&conn)?.unwrap_or(0))
    }

    fn set_setting(&self, key: &str, value: &str) -> Result<(), CoreError> {
        let conn = self.lock();
        write_setting_conn(&conn, key, value)
    }

    fn get_setting(&self, key: &str) -> Result<Option<String>, CoreError> {
        let conn = self.lock();
        read_setting_conn(&conn, key)
    }

    fn available_bytes(&self) -> Result<Option<u64>, CoreError> {
        Ok(fs2::available_space(self.paths().root()).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工造一个 v3 库（`ai_conversations` 没有 `model_id`），验证升级会补列、
    /// 保留旧行、并且重复执行不会报「duplicate column」。
    #[test]
    fn v3_to_v4_adds_model_id_and_is_idempotent() {
        let mut conn = Connection::open_in_memory().expect("memory db");
        conn.execute_batch(
            r#"
            CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
            INSERT INTO meta(key, value) VALUES ('schema_version', '3');
            CREATE TABLE ai_conversations (
                id         INTEGER PRIMARY KEY,
                title      TEXT NOT NULL,
                provider   TEXT NOT NULL,
                model      TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            INSERT INTO ai_conversations(title, provider, model, created_at, updated_at)
            VALUES ('旧会话', 'anthropic', 'claude-sonnet-4-5', 1, 1);
            "#,
        )
        .expect("建 v3 库");

        migrate_v3_to_v4(&mut conn).expect("升级失败");

        let has_column: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('ai_conversations') \
                 WHERE name = 'model_id')",
                [],
                |row| row.get(0),
            )
            .expect("读取列");
        assert!(has_column, "v3 库升级后必须有 model_id 列");

        let version: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .expect("读版本");
        assert_eq!(version, "4");

        // 旧会话保留，`model_id` 为空：界面按「当前模型」回退。
        let (title, model_id): (String, Option<String>) = conn
            .query_row(
                "SELECT title, model_id FROM ai_conversations WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("读旧会话");
        assert_eq!(title, "旧会话");
        assert!(model_id.is_none());

        // 幂等：新建库 / v2 升级路径到这里时列已经有了。
        migrate_v3_to_v4(&mut conn).expect("重复执行应当成功");
    }

    /// v4 → v5：补 `ai_messages.plan`，老消息读出来是 NULL（= 没有清单）。
    #[test]
    fn v4_to_v5_adds_plan_column() {
        let mut conn = Connection::open_in_memory().expect("memory db");
        conn.execute_batch(
            r#"
            CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
            INSERT INTO meta(key, value) VALUES ('schema_version', '4');
            CREATE TABLE ai_conversations (
                id         INTEGER PRIMARY KEY,
                title      TEXT NOT NULL,
                provider   TEXT NOT NULL,
                model      TEXT NOT NULL,
                model_id   TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            INSERT INTO ai_conversations(title, provider, model, model_id, created_at, updated_at)
            VALUES ('旧会话', 'anthropic', 'claude', 'm1', 1, 1);
            CREATE TABLE ai_messages (
                id              INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                role            TEXT NOT NULL,
                content         TEXT NOT NULL DEFAULT '',
                context_refs    TEXT,
                status          TEXT NOT NULL DEFAULT 'complete',
                error           TEXT,
                input_tokens    INTEGER,
                output_tokens   INTEGER,
                tool_trace      TEXT,
                created_at      INTEGER NOT NULL
            );
            INSERT INTO ai_messages(conversation_id, role, content, created_at)
            VALUES (1, 'assistant', '旧回复', 1);
            "#,
        )
        .expect("建 v4 库");

        migrate_v4_to_v5(&mut conn).expect("升级失败");

        let has_column: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('ai_messages') \
                 WHERE name = 'plan')",
                [],
                |row| row.get(0),
            )
            .expect("读取列");
        assert!(has_column, "v4 库升级后必须有 plan 列");

        let version: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .expect("读版本");
        assert_eq!(version, "5");

        // 老消息保留，plan 为空：界面按「这一轮没有清单」处理。
        let (content, plan): (String, Option<String>) = conn
            .query_row(
                "SELECT content, plan FROM ai_messages WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("读旧消息");
        assert_eq!(content, "旧回复");
        assert!(plan.is_none());

        // 幂等：v3 的升级路径会连走两步，到这里列已经存在。
        migrate_v4_to_v5(&mut conn).expect("重复执行应当成功");
    }
}
