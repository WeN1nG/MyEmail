//! 连接管理：单写连接 + 只读连接池，以及存储门面。
//!
//! 每次取连接都执行四条 PRAGMA；启动时 `PRAGMA quick_check`；
//! WAL 超过 64 MB 时执行 `wal_checkpoint(TRUNCATE)`。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use me_core::CoreError;
use me_core::port::ReadOnlyRepo;
use r2d2::Pool;
use rusqlite::{Connection, OpenFlags};

use crate::cache::CachePaths;
use crate::{migrations, storage};

/// WAL 触发 checkpoint 的阈值（64 MB）。
const WAL_CHECKPOINT_BYTES: u64 = 64 * 1024 * 1024;

/// 只读连接池上限。写连接永远只有一个。
const READ_POOL_SIZE: u32 = 4;

type SqlitePool = Pool<SqliteConnectionManager>;
pub(crate) type PooledConn = r2d2::PooledConnection<SqliteConnectionManager>;

/// 只读连接池的连接工厂。
///
/// 为什么自己实现而不用 `r2d2_sqlite`：那个 crate 对 `rusqlite` 的版本要求是
/// 精确配对的（0.35 → `rusqlite ^0.40`），而 `rusqlite 0.41` 才带上打过补丁的
/// `libsqlite3-sys 0.39`（RUSTSEC-2026-0459 / CVE-2026-119375：SQLite < 3.54.0
/// 读 WAL 时可能返回未初始化的页缓存内存）。r2d2_sqlite 一旦没跟上，旧版
/// `libsqlite3-sys` 就会被它继续拖进依赖树，`cargo deny` 永远红。
///
/// 这里的行为与 `SqliteConnectionManager::file(path).with_init(apply_pragmas)`
/// **完全一致**：`OpenFlags::default()`（读写 + 创建 + URI + NO_MUTEX）打开，
/// 每次新建连接后套一遍 PRAGMA；`is_valid` 用 `SELECT 1` 探活，
/// `has_broken` 恒为 false——都是 r2d2_sqlite 的原样语义，不做额外判断。
#[derive(Debug)]
pub(crate) struct SqliteConnectionManager {
    path: PathBuf,
}

impl r2d2::ManageConnection for SqliteConnectionManager {
    type Connection = Connection;
    type Error = rusqlite::Error;

    fn connect(&self) -> Result<Connection, rusqlite::Error> {
        let conn = Connection::open_with_flags(&self.path, OpenFlags::default())?;
        apply_pragmas(&conn)?;
        Ok(conn)
    }

    fn is_valid(&self, conn: &mut Connection) -> Result<(), rusqlite::Error> {
        conn.execute_batch("SELECT 1;")
    }

    fn has_broken(&self, _: &mut Connection) -> bool {
        false
    }
}

pub(crate) struct StoreInner {
    pub(crate) paths: CachePaths,
    /// 全局唯一写连接。所有写操作在此互斥。
    write: Mutex<Connection>,
    /// 只读连接池，供 `me-app` 的 `ReadOnlyRepo` 使用。
    pool: SqlitePool,
}

impl StoreInner {
    fn lock_write(&self) -> MutexGuard<'_, Connection> {
        // 写连接上任何 panic 都意味着程序状态不可信；恢复锁以便后续诊断，
        // 但不再吞掉错误：SQLite 自身仍会以错误码报告不一致。
        self.write
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn read_conn(&self) -> Result<PooledConn, CoreError> {
        self.pool
            .get()
            .map_err(|e| storage(format!("获取只读连接失败: {e}")))
    }
}

/// 存储门面。`me-app` 只持有 `ReadOnlyStore`，写句柄只交给 `me-sync`。
#[derive(Clone)]
pub struct Store {
    inner: Arc<StoreInner>,
}

/// 写句柄：实现全部写仓储端口。
#[derive(Clone)]
pub struct WritableStore {
    pub(crate) inner: Arc<StoreInner>,
}

/// 只读句柄：实现 `ReadOnlyRepo`（键集分页、单次 `LIMIT <= 200`）。
#[derive(Clone)]
pub struct ReadOnlyStore {
    pub(crate) inner: Arc<StoreInner>,
}

impl Store {
    /// 打开（或创建）存储。完成 schema 初始化、PRAGMA 与完整性检查。
    pub fn open(root: impl AsRef<Path>) -> Result<Store, CoreError> {
        let paths = CachePaths::new(root.as_ref().to_path_buf())
            .map_err(|e| storage(format!("创建缓存目录失败: {e}")))?;

        let conn = Connection::open(paths.db_file())
            .map_err(|e| storage(format!("打开数据库失败: {e}")))?;
        apply_pragmas(&conn).map_err(|e| storage(format!("设置 PRAGMA 失败: {e}")))?;
        quick_check(&conn)?;
        // v1 库会在这里备份并迁移到 v2；新库直接建 v2。
        let conn = migrations::ensure_schema(conn, &paths.db_file())?;
        maybe_checkpoint_wal(&conn, &paths)?;

        let manager = SqliteConnectionManager {
            path: paths.db_file(),
        };
        let pool = Pool::builder()
            .max_size(READ_POOL_SIZE)
            .build(manager)
            .map_err(|e| storage(format!("创建只读连接池失败: {e}")))?;

        Ok(Store {
            inner: Arc::new(StoreInner {
                paths,
                write: Mutex::new(conn),
                pool,
            }),
        })
    }

    pub fn writable(&self) -> WritableStore {
        WritableStore {
            inner: Arc::clone(&self.inner),
        }
    }

    pub fn read_only(&self) -> ReadOnlyStore {
        ReadOnlyStore {
            inner: Arc::clone(&self.inner),
        }
    }

    pub fn paths(&self) -> CachePaths {
        self.inner.paths.clone()
    }

    /// 剩余磁盘是否满足后台补全的最低要求（>= 500 MB）。
    ///
    /// 无法读取时保守返回 `true`，避免因平台限制完全停用缓存。
    pub fn disk_space_ok(&self) -> bool {
        self.available_bytes()
            .map(|bytes| bytes >= me_core::MIN_FREE_BYTES)
            .unwrap_or(true)
    }

    /// 缓存目录所在文件系统的可用字节数。跨平台由 `fs2` 提供。
    pub fn available_bytes(&self) -> Option<u64> {
        fs2::available_space(self.inner.paths.root()).ok()
    }
}

impl WritableStore {
    /// 写连接互斥锁。所有写仓储方法通过它串行化。
    pub(crate) fn lock(&self) -> MutexGuard<'_, Connection> {
        self.inner.lock_write()
    }

    pub(crate) fn paths(&self) -> &CachePaths {
        &self.inner.paths
    }
}

impl ReadOnlyStore {
    pub(crate) fn conn(&self) -> Result<PooledConn, CoreError> {
        self.inner.read_conn()
    }
}

// ---------------------------------------------------------------------------
// PRAGMA / 完整性检查
// ---------------------------------------------------------------------------

/// 每个连接都必须执行的初始化语句。
pub(crate) fn apply_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;\n\
         PRAGMA journal_mode = WAL;\n\
         PRAGMA busy_timeout = 5000;\n\
         PRAGMA synchronous = NORMAL;",
    )
}

fn quick_check(conn: &Connection) -> Result<(), CoreError> {
    let result: String = conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|e| storage(format!("数据库完整性检查失败: {e}")))?;
    if result.eq_ignore_ascii_case("ok") {
        Ok(())
    } else {
        Err(storage(format!("数据库完整性检查失败: {result}")))
    }
}

fn wal_file(paths: &CachePaths) -> PathBuf {
    let mut name = paths.db_file().into_os_string();
    name.push("-wal");
    PathBuf::from(name)
}

fn maybe_checkpoint_wal(conn: &Connection, paths: &CachePaths) -> Result<(), CoreError> {
    let size = fs::metadata(wal_file(paths)).map(|m| m.len()).unwrap_or(0);
    if size > WAL_CHECKPOINT_BYTES {
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .map_err(|e| storage(format!("WAL checkpoint 失败: {e}")))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ReadOnlyRepo
// ---------------------------------------------------------------------------

impl ReadOnlyRepo for ReadOnlyStore {
    fn list_accounts_readonly(&self) -> Result<Vec<me_core::Account>, CoreError> {
        let conn = self.conn()?;
        crate::accounts::list_accounts_conn(&conn)
    }

    fn list_folders_readonly(
        &self,
        account: me_core::AccountId,
    ) -> Result<Vec<me_core::Folder>, CoreError> {
        let conn = self.conn()?;
        crate::folders::list_folders_conn(&conn, account)
    }

    fn list_messages_readonly(
        &self,
        folder: me_core::FolderId,
        cursor: Option<(i64, i64)>,
        limit: u32,
    ) -> Result<Vec<me_core::MessageSummary>, CoreError> {
        let conn = self.conn()?;
        crate::messages::list_messages_conn(&conn, folder, cursor, limit)
    }

    fn unread_count_readonly(&self, folder: me_core::FolderId) -> Result<u32, CoreError> {
        let conn = self.conn()?;
        crate::messages::unread_count_conn(&conn, folder)
    }

    fn get_message_readonly(
        &self,
        id: me_core::MessageId,
    ) -> Result<Option<me_core::MessageSummary>, CoreError> {
        let conn = self.conn()?;
        crate::messages::get_message_conn(&conn, id)
    }

    fn list_drafts_readonly(
        &self,
        account: Option<me_core::AccountId>,
    ) -> Result<Vec<me_core::Draft>, CoreError> {
        let conn = self.conn()?;
        crate::drafts::list_drafts_conn(&conn, account)
    }

    fn get_draft_readonly(
        &self,
        id: me_core::DraftId,
    ) -> Result<Option<me_core::Draft>, CoreError> {
        let conn = self.conn()?;
        crate::drafts::get_draft_conn(&conn, id)
    }

    fn list_contacts_readonly(
        &self,
        prefix: &str,
        limit: u32,
    ) -> Result<Vec<me_core::Contact>, CoreError> {
        let conn = self.conn()?;
        crate::contacts::list_contacts_conn(&conn, prefix, limit)
    }

    fn list_unified_inbox_readonly(
        &self,
        cursor: Option<(i64, i64)>,
        limit: u32,
    ) -> Result<Vec<me_core::MessageSummary>, CoreError> {
        let conn = self.conn()?;
        crate::messages::list_unified_inbox_conn(&conn, cursor, limit)
    }

    fn unified_unread_count_readonly(&self) -> Result<u32, CoreError> {
        let conn = self.conn()?;
        crate::messages::unified_unread_count_conn(&conn)
    }

    fn list_calendars_readonly(&self) -> Result<Vec<me_core::Calendar>, CoreError> {
        self.calendars_readonly()
    }

    fn list_calendar_series_readonly(
        &self,
        calendars: &[me_core::CalendarId],
        from_utc: i64,
        to_utc: i64,
        limit: u32,
    ) -> Result<Vec<me_core::CalendarSeries>, CoreError> {
        self.calendar_series_readonly(calendars, from_utc, to_utc, limit)
    }

    fn list_calendar_exceptions_readonly(
        &self,
        events: &[me_core::CalendarEventId],
    ) -> Result<Vec<(me_core::CalendarEventId, me_core::CalendarException)>, CoreError> {
        self.calendar_exceptions_readonly(events)
    }

    fn get_calendar_event_readonly(
        &self,
        id: me_core::CalendarEventId,
    ) -> Result<Option<me_core::CalendarEvent>, CoreError> {
        self.calendar_event_readonly(id)
    }

    fn list_weather_locations_readonly(&self) -> Result<Vec<me_core::WeatherLocation>, CoreError> {
        self.weather_locations_readonly()
    }

    fn get_weather_snapshot_readonly(
        &self,
        location_id: me_core::WeatherLocationId,
    ) -> Result<Option<me_core::WeatherSnapshot>, CoreError> {
        self.weather_snapshot_readonly(location_id)
    }

    fn list_ai_conversations_readonly(
        &self,
        before: Option<me_core::AiConversationId>,
        limit: u32,
    ) -> Result<Vec<me_core::AiConversation>, CoreError> {
        crate::ai::list_conversations_readonly(self, before, limit)
    }

    fn list_ai_messages_readonly(
        &self,
        conversation: me_core::AiConversationId,
        before: Option<me_core::AiMessageId>,
        limit: u32,
    ) -> Result<Vec<me_core::AiMessage>, CoreError> {
        crate::ai::list_messages_readonly(self, conversation, before, limit)
    }

    fn get_ai_message_readonly(
        &self,
        id: me_core::AiMessageId,
    ) -> Result<Option<me_core::AiMessage>, CoreError> {
        crate::ai::get_message_readonly(self, id)
    }

    fn list_ai_entities_readonly(&self, limit: u32) -> Result<Vec<me_core::AiEntity>, CoreError> {
        crate::ai::list_entities_readonly(self, limit)
    }

    fn count_ai_entities_readonly(&self) -> Result<u32, CoreError> {
        crate::ai::count_entities_readonly(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_least(version: &str, min: (u32, u32, u32)) -> bool {
        let mut parts = version
            .split('.')
            .map(|part| part.trim().parse::<u32>().unwrap_or(0));
        let got = (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        );
        got >= min
    }

    /// 打包进去的 SQLite 必须 >= 3.54.0。
    ///
    /// `cargo deny` 只能看 crate 版本号，而真正决定安全性的是**编进来的 C 源码**：
    /// 只要有人把 `bundled` 关掉（改成链接系统库），或者降级 `libsqlite3-sys`，
    /// crate 版本检查都可能看不出来。这条断言直接问运行时，把两种情况都挡住。
    ///
    /// 3.54.0 是 RUSTSEC-2026-0459 / CVE-2026-119375 的修复线：旧版在打开
    /// 「页大小与库不一致的 -wal」时，可能把进程内存里的旧字节当查询结果返回。
    #[test]
    fn bundled_sqlite_is_patched_for_cve_2026_119375() {
        let conn = Connection::open_in_memory().expect("内存库");
        let version: String = conn
            .query_row("SELECT sqlite_version()", [], |row| row.get(0))
            .expect("读版本");
        assert!(
            at_least(&version, (3, 54, 0)),
            "SQLite {version} 低于 3.54.0（RUSTSEC-2026-0459）：\
确认 rusqlite 的 bundled 特性没被关掉，且 libsqlite3-sys >= 0.39"
        );
    }
}
