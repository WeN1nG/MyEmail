//! 日历仓储：日历、事件、单次例外、提醒去重。
//!
//! 只做存取与枚举校验；「算 `recurrence_until`、去重、来源归属」在 `me-sync`
//! 的用例里完成（见 `doc/v03架构设计.md` §2.7）。
//!
//! 范围查询按 §4.6 拆成「一次性事件」与「未结束的重复系列」两条：
//! 重复事件的 `start_utc` 可能远早于可视范围，合并成一条升序查询会被历史
//! 重复事件占满 `LIMIT`，导致当月事件一条都取不到。

use me_core::CoreError;
use me_core::calendar::{
    Calendar, CalendarEvent, CalendarEventDraft, CalendarEventId, CalendarEventStatus,
    CalendarException, CalendarId, CalendarSeries, CalendarSource,
};
use me_core::model::now_unix;
use me_core::port::CalendarRepo;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};

use crate::db::{ReadOnlyStore, WritableStore};
use crate::{StoreResult, storage};

const CALENDAR_COLUMNS: &str = "id, name, color, visible, is_default, created_at, updated_at";

const EVENT_COLUMNS: &str = "id, calendar_id, uid, summary, description, location, start_utc, \
                             end_utc, all_day, timezone, rrule, recurrence_until, exdates, \
                             reminders, status, source, sequence, created_at, updated_at, \
                             deleted_at";

const SERIES_COLUMNS: &str = "id, calendar_id, summary, start_utc, end_utc, all_day, timezone, \
                              rrule, recurrence_until, exdates, reminders, source";

/// 单次 `IN (...)` 的最大日历数。§4.6 约定最多 8 个。
const MAX_CALENDARS: usize = 8;

fn parse_i64_array(raw: Option<&str>) -> Vec<i64> {
    raw.and_then(|text| serde_json::from_str::<Vec<i64>>(text).ok())
        .unwrap_or_default()
}

fn parse_u32_array(raw: Option<&str>) -> Vec<u32> {
    raw.and_then(|text| serde_json::from_str::<Vec<u32>>(text).ok())
        .unwrap_or_default()
}

/// 只编码数字数组，不引入 `serde` 依赖。
fn encode_i64_array(values: &[i64]) -> String {
    let inner = values
        .iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!("[{inner}]")
}

fn encode_u32_array(values: &[u32]) -> String {
    let inner = values
        .iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!("[{inner}]")
}

fn map_calendar(row: &Row<'_>) -> rusqlite::Result<Calendar> {
    Ok(Calendar {
        id: CalendarId(row.get(0)?),
        name: row.get(1)?,
        color: row.get(2)?,
        visible: row.get::<_, i64>(3)? != 0,
        is_default: row.get::<_, i64>(4)? != 0,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

fn map_event(row: &Row<'_>) -> rusqlite::Result<CalendarEvent> {
    let status: String = row.get(14)?;
    let source: String = row.get(15)?;
    let exdates: Option<String> = row.get(12)?;
    let reminders: Option<String> = row.get(13)?;
    Ok(CalendarEvent {
        id: CalendarEventId(row.get(0)?),
        calendar_id: CalendarId(row.get(1)?),
        uid: row.get(2)?,
        summary: row.get(3)?,
        description: row.get(4)?,
        location: row.get(5)?,
        start_utc: row.get(6)?,
        end_utc: row.get(7)?,
        all_day: row.get::<_, i64>(8)? != 0,
        timezone: row.get(9)?,
        rrule: row.get(10)?,
        recurrence_until: row.get(11)?,
        exdates: parse_i64_array(exdates.as_deref()),
        reminders: parse_u32_array(reminders.as_deref()),
        // 合法值由写入路径与本层 CHECK 保证；损坏数据保守回落到 confirmed / local。
        status: CalendarEventStatus::parse(&status).unwrap_or(CalendarEventStatus::Confirmed),
        source: CalendarSource::parse(&source).unwrap_or(CalendarSource::Local),
        sequence: row.get(16)?,
        created_at: row.get(17)?,
        updated_at: row.get(18)?,
        deleted_at: row.get(19)?,
    })
}

fn map_series(row: &Row<'_>) -> rusqlite::Result<CalendarSeries> {
    let source: String = row.get(11)?;
    let exdates: Option<String> = row.get(9)?;
    let reminders: Option<String> = row.get(10)?;
    Ok(CalendarSeries {
        id: CalendarEventId(row.get(0)?),
        calendar_id: CalendarId(row.get(1)?),
        summary: row.get(2)?,
        start_utc: row.get(3)?,
        end_utc: row.get(4)?,
        all_day: row.get::<_, i64>(5)? != 0,
        timezone: row.get(6)?,
        rrule: row.get(7)?,
        recurrence_until: row.get(8)?,
        exdates: parse_i64_array(exdates.as_deref()),
        reminders: parse_u32_array(reminders.as_deref()),
        source: CalendarSource::parse(&source).unwrap_or(CalendarSource::Local),
    })
}

/// 把 `calendar_id IN (...)` 的占位符与参数拼出来。
fn calendar_filter(calendars: &[CalendarId]) -> (String, Vec<Value>) {
    let limited = &calendars[..calendars.len().min(MAX_CALENDARS)];
    let placeholders = vec!["?"; limited.len()].join(",");
    let values = limited
        .iter()
        .map(|id| Value::Integer(id.0))
        .collect::<Vec<_>>();
    (placeholders, values)
}

pub(crate) fn list_calendars_conn(conn: &Connection) -> Result<Vec<Calendar>, CoreError> {
    let sql = format!("SELECT {CALENDAR_COLUMNS} FROM calendars ORDER BY is_default DESC, id ASC");
    let mut stmt = conn.prepare(&sql).store()?;
    let rows = stmt
        .query_map([], map_calendar)
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    Ok(rows)
}

pub(crate) fn default_calendar_conn(conn: &Connection) -> Result<Option<Calendar>, CoreError> {
    let sql = format!("SELECT {CALENDAR_COLUMNS} FROM calendars WHERE is_default = 1");
    conn.query_row(&sql, [], map_calendar).optional().store()
}

/// §4.6：一次性事件走精确范围；重复事件走「未结束」判定。
pub(crate) fn list_series_conn(
    conn: &Connection,
    calendars: &[CalendarId],
    from_utc: i64,
    to_utc: i64,
    limit: u32,
) -> Result<Vec<CalendarSeries>, CoreError> {
    if calendars.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let (placeholders, ids) = calendar_filter(calendars);
    let limit = limit.min(200) as i64;

    let mut out: Vec<CalendarSeries> = Vec::new();

    // ① 一次性事件：与可视范围有交集即可。
    let single_sql = format!(
        "SELECT {SERIES_COLUMNS} FROM calendar_events \
         WHERE calendar_id IN ({placeholders}) AND deleted_at IS NULL \
           AND status <> 'cancelled' AND rrule IS NULL \
           AND end_utc >= ? AND start_utc <= ? \
         ORDER BY start_utc ASC LIMIT ?"
    );
    let mut single_params = ids.clone();
    single_params.push(Value::Integer(from_utc));
    single_params.push(Value::Integer(to_utc));
    single_params.push(Value::Integer(limit));
    let mut stmt = conn.prepare(&single_sql).store()?;
    out.extend(
        stmt.query_map(params_from_iter(single_params), map_series)
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?,
    );

    // ② 重复事件：`start_utc` 通常早于可视范围，只能用「上界未过期」判断。
    //    降序取最近的一批，避免最早的历史系列挤占名额。
    let recurring_sql = format!(
        "SELECT {SERIES_COLUMNS} FROM calendar_events \
         WHERE calendar_id IN ({placeholders}) AND deleted_at IS NULL \
           AND status <> 'cancelled' AND rrule IS NOT NULL \
           AND start_utc <= ? AND (recurrence_until IS NULL OR recurrence_until >= ?) \
         ORDER BY start_utc DESC LIMIT ?"
    );
    let mut recurring_params = ids;
    recurring_params.push(Value::Integer(to_utc));
    recurring_params.push(Value::Integer(from_utc));
    recurring_params.push(Value::Integer(limit));
    let mut stmt = conn.prepare(&recurring_sql).store()?;
    out.extend(
        stmt.query_map(params_from_iter(recurring_params), map_series)
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?,
    );

    out.sort_by_key(|series| series.start_utc);
    Ok(out)
}

pub(crate) fn list_exceptions_conn(
    conn: &Connection,
    events: &[CalendarEventId],
) -> Result<Vec<(CalendarEventId, CalendarException)>, CoreError> {
    if events.is_empty() {
        return Ok(Vec::new());
    }
    let limited = &events[..events.len().min(200)];
    let placeholders = vec!["?"; limited.len()].join(",");
    let ids = limited
        .iter()
        .map(|id| Value::Integer(id.0))
        .collect::<Vec<_>>();
    let sql = format!(
        "SELECT event_id, recurrence_at, cancelled, summary, description, location, start_utc, \
                end_utc, all_day \
           FROM calendar_event_exceptions WHERE event_id IN ({placeholders}) \
          ORDER BY event_id, recurrence_at"
    );
    let mut stmt = conn.prepare(&sql).store()?;
    let rows = stmt
        .query_map(params_from_iter(ids), |row| {
            Ok((
                CalendarEventId(row.get(0)?),
                CalendarException {
                    recurrence_at: row.get(1)?,
                    cancelled: row.get::<_, i64>(2)? != 0,
                    summary: row.get(3)?,
                    description: row.get(4)?,
                    location: row.get(5)?,
                    start_utc: row.get(6)?,
                    end_utc: row.get(7)?,
                    all_day: row.get::<_, Option<i64>>(8)?.map(|v| v != 0),
                },
            ))
        })
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    Ok(rows)
}

pub(crate) fn get_event_conn(
    conn: &Connection,
    id: CalendarEventId,
) -> Result<Option<CalendarEvent>, CoreError> {
    let sql =
        format!("SELECT {EVENT_COLUMNS} FROM calendar_events WHERE id = ?1 AND deleted_at IS NULL");
    conn.query_row(&sql, params![id.0], map_event)
        .optional()
        .store()
}

pub(crate) fn reminder_candidates_conn(
    conn: &Connection,
    from_utc: i64,
    window_end: i64,
    limit: u32,
) -> Result<Vec<CalendarSeries>, CoreError> {
    let limit = limit.min(200) as i64;
    let mut out = Vec::new();

    let single_sql = format!(
        "SELECT {SERIES_COLUMNS} FROM calendar_events \
         WHERE deleted_at IS NULL AND status <> 'cancelled' AND reminders <> '[]' \
           AND rrule IS NULL AND end_utc >= ? AND start_utc <= ? \
         ORDER BY start_utc ASC LIMIT ?"
    );
    let mut stmt = conn.prepare(&single_sql).store()?;
    out.extend(
        stmt.query_map(params![from_utc, window_end, limit], map_series)
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?,
    );

    let recurring_sql = format!(
        "SELECT {SERIES_COLUMNS} FROM calendar_events \
         WHERE deleted_at IS NULL AND status <> 'cancelled' AND reminders <> '[]' \
           AND rrule IS NOT NULL \
           AND start_utc <= ? AND (recurrence_until IS NULL OR recurrence_until >= ?) \
         ORDER BY start_utc DESC LIMIT ?"
    );
    let mut stmt = conn.prepare(&recurring_sql).store()?;
    out.extend(
        stmt.query_map(params![window_end, from_utc, limit], map_series)
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?,
    );

    Ok(out)
}

fn insert_event(
    conn: &Connection,
    draft: &CalendarEventDraft,
    recurrence_until: Option<i64>,
    source: CalendarSource,
) -> Result<CalendarEvent, CoreError> {
    let now = now_unix();
    let uid = format!("{}@myemail.local", uuid::Uuid::new_v4());
    conn.execute(
        "INSERT INTO calendar_events(
             calendar_id, uid, summary, description, location, start_utc, end_utc, all_day,
             timezone, rrule, recurrence_until, exdates, reminders, status, source, sequence,
             created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 'confirmed', ?14, 0, ?15, ?15)",
        params![
            draft.calendar_id.0,
            uid,
            draft.summary,
            draft.description,
            draft.location,
            draft.start_utc,
            draft.end_utc,
            draft.all_day as i64,
            draft.timezone,
            draft.rrule,
            recurrence_until,
            encode_i64_array(&draft.exdates),
            encode_u32_array(&draft.reminders),
            source.as_str(),
            now,
        ],
    )
    .store()?;
    let id = CalendarEventId(conn.last_insert_rowid());
    get_event_conn(conn, id)?.ok_or_else(|| storage("事件写入后无法读回（并发删除？）"))
}

impl CalendarRepo for WritableStore {
    fn ensure_default_calendar(&self) -> Result<Calendar, CoreError> {
        let conn = self.lock();
        if let Some(calendar) = default_calendar_conn(&conn)? {
            return Ok(calendar);
        }
        let now = now_unix();
        conn.execute(
            "INSERT INTO calendars(name, color, visible, is_default, created_at, updated_at) \
             VALUES ('我的日历', '#3B82F6', 1, 1, ?1, ?1)",
            params![now],
        )
        .store()?;
        let id = CalendarId(conn.last_insert_rowid());
        let sql = format!("SELECT {CALENDAR_COLUMNS} FROM calendars WHERE id = ?1");
        conn.query_row(&sql, params![id.0], map_calendar).store()
    }

    fn list_calendars(&self) -> Result<Vec<Calendar>, CoreError> {
        let conn = self.lock();
        list_calendars_conn(&conn)
    }

    fn create_calendar(&self, name: &str, color: &str) -> Result<Calendar, CoreError> {
        let conn = self.lock();
        let now = now_unix();
        conn.execute(
            "INSERT INTO calendars(name, color, visible, is_default, created_at, updated_at) \
             VALUES (?1, ?2, 1, 0, ?3, ?3)",
            params![name, color, now],
        )
        .store()?;
        let id = CalendarId(conn.last_insert_rowid());
        let sql = format!("SELECT {CALENDAR_COLUMNS} FROM calendars WHERE id = ?1");
        conn.query_row(&sql, params![id.0], map_calendar).store()
    }

    fn set_calendar_visible(&self, id: CalendarId, visible: bool) -> Result<(), CoreError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE calendars SET visible = ?2, updated_at = ?3 WHERE id = ?1",
            params![id.0, visible as i64, now_unix()],
        )
        .store()?;
        Ok(())
    }

    fn rename_calendar(&self, id: CalendarId, name: &str) -> Result<(), CoreError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE calendars SET name = ?2, updated_at = ?3 WHERE id = ?1",
            params![id.0, name, now_unix()],
        )
        .store()?;
        Ok(())
    }

    fn create_event(
        &self,
        draft: &CalendarEventDraft,
        recurrence_until: Option<i64>,
        source: CalendarSource,
    ) -> Result<CalendarEvent, CoreError> {
        let conn = self.lock();
        insert_event(&conn, draft, recurrence_until, source)
    }

    fn update_event(
        &self,
        id: CalendarEventId,
        draft: &CalendarEventDraft,
        recurrence_until: Option<i64>,
    ) -> Result<CalendarEvent, CoreError> {
        let conn = self.lock();
        let affected = conn
            .execute(
                "UPDATE calendar_events SET \
                     calendar_id = ?2, summary = ?3, description = ?4, location = ?5, \
                     start_utc = ?6, end_utc = ?7, all_day = ?8, timezone = ?9, rrule = ?10, \
                     recurrence_until = ?11, exdates = ?12, reminders = ?13, \
                     sequence = sequence + 1, updated_at = ?14 \
                 WHERE id = ?1 AND deleted_at IS NULL",
                params![
                    id.0,
                    draft.calendar_id.0,
                    draft.summary,
                    draft.description,
                    draft.location,
                    draft.start_utc,
                    draft.end_utc,
                    draft.all_day as i64,
                    draft.timezone,
                    draft.rrule,
                    recurrence_until,
                    encode_i64_array(&draft.exdates),
                    encode_u32_array(&draft.reminders),
                    now_unix(),
                ],
            )
            .store()?;
        if affected == 0 {
            return Err(storage(format!("事件 {id} 不存在或已删除")));
        }
        get_event_conn(&conn, id)?.ok_or_else(|| storage(format!("事件 {id} 更新后无法读回")))
    }

    fn soft_delete_event(&self, id: CalendarEventId) -> Result<(), CoreError> {
        let conn = self.lock();
        let now = now_unix();
        let affected = conn
            .execute(
                "UPDATE calendar_events SET deleted_at = ?2, updated_at = ?2 \
                 WHERE id = ?1 AND deleted_at IS NULL",
                params![id.0, now],
            )
            .store()?;
        if affected == 0 {
            return Err(storage(format!("事件 {id} 不存在或已删除")));
        }
        Ok(())
    }

    fn get_event(&self, id: CalendarEventId) -> Result<Option<CalendarEvent>, CoreError> {
        let conn = self.lock();
        get_event_conn(&conn, id)
    }

    fn list_series_in_range(
        &self,
        calendars: &[CalendarId],
        from_utc: i64,
        to_utc: i64,
        limit: u32,
    ) -> Result<Vec<CalendarSeries>, CoreError> {
        let conn = self.lock();
        list_series_conn(&conn, calendars, from_utc, to_utc, limit)
    }

    fn list_exceptions(
        &self,
        events: &[CalendarEventId],
    ) -> Result<Vec<(CalendarEventId, CalendarException)>, CoreError> {
        let conn = self.lock();
        list_exceptions_conn(&conn, events)
    }

    fn insert_exception(
        &self,
        event: CalendarEventId,
        exception: &CalendarException,
    ) -> Result<(), CoreError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO calendar_event_exceptions(
                 event_id, recurrence_at, cancelled, summary, description, location,
                 start_utc, end_utc, all_day, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(event_id, recurrence_at) DO UPDATE SET
                 cancelled = excluded.cancelled,
                 summary = excluded.summary,
                 description = excluded.description,
                 location = excluded.location,
                 start_utc = excluded.start_utc,
                 end_utc = excluded.end_utc,
                 all_day = excluded.all_day,
                 updated_at = excluded.updated_at",
            params![
                event.0,
                exception.recurrence_at,
                exception.cancelled as i64,
                exception.summary,
                exception.description,
                exception.location,
                exception.start_utc,
                exception.end_utc,
                exception.all_day.map(|value| value as i64),
                now_unix(),
            ],
        )
        .store()?;
        Ok(())
    }

    fn list_reminder_candidates(
        &self,
        from_utc: i64,
        window_end: i64,
        limit: u32,
    ) -> Result<Vec<CalendarSeries>, CoreError> {
        let conn = self.lock();
        reminder_candidates_conn(&conn, from_utc, window_end, limit)
    }

    fn record_reminder_fire(
        &self,
        event: CalendarEventId,
        occurrence_at: i64,
        minutes_before: u32,
    ) -> Result<bool, CoreError> {
        let conn = self.lock();
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO calendar_reminder_fires(
                     event_id, occurrence_at, minutes_before, fired_at
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![event.0, occurrence_at, minutes_before as i64, now_unix()],
            )
            .store()?;
        Ok(inserted == 1)
    }
}

impl ReadOnlyStore {
    pub(crate) fn calendars_readonly(&self) -> Result<Vec<Calendar>, CoreError> {
        let conn = self.conn()?;
        list_calendars_conn(&conn)
    }

    pub(crate) fn calendar_series_readonly(
        &self,
        calendars: &[CalendarId],
        from_utc: i64,
        to_utc: i64,
        limit: u32,
    ) -> Result<Vec<CalendarSeries>, CoreError> {
        let conn = self.conn()?;
        list_series_conn(&conn, calendars, from_utc, to_utc, limit)
    }

    pub(crate) fn calendar_exceptions_readonly(
        &self,
        events: &[CalendarEventId],
    ) -> Result<Vec<(CalendarEventId, CalendarException)>, CoreError> {
        let conn = self.conn()?;
        list_exceptions_conn(&conn, events)
    }

    pub(crate) fn calendar_event_readonly(
        &self,
        id: CalendarEventId,
    ) -> Result<Option<CalendarEvent>, CoreError> {
        let conn = self.conn()?;
        get_event_conn(&conn, id)
    }
}
