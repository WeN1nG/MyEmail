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
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;

use crate::cache::CachePaths;
use crate::{migrations, storage};

/// WAL 触发 checkpoint 的阈值（64 MB）。
const WAL_CHECKPOINT_BYTES: u64 = 64 * 1024 * 1024;

/// 只读连接池上限。写连接永远只有一个。
const READ_POOL_SIZE: u32 = 4;

type SqlitePool = Pool<SqliteConnectionManager>;
pub(crate) type PooledConn = r2d2::PooledConnection<SqliteConnectionManager>;

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

        let manager = SqliteConnectionManager::file(paths.db_file()).with_init(|conn| {
            apply_pragmas(conn)?;
            Ok(())
        });
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
}
