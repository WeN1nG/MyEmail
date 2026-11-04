//! 天气仓储：地点与天气快照。
//!
//! 只做存取与结构校验；「去重、上限、默认唯一」在存储层保证（唯一索引 +
//! 事务），其余业务语义（是否过期、能否刷新）在 `me-sync` 的用例里完成
//! （见 `doc/v03架构设计.md` §2.7）。

use me_core::CoreError;
use me_core::model::now_unix;
use me_core::port::WeatherRepo;
use me_core::weather::{
    MAX_WEATHER_LOCATIONS, StoredWeatherSnapshot, WeatherLocation, WeatherLocationCandidate,
    WeatherLocationId, WeatherSnapshot,
};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::StoreResult;
use crate::db::{ReadOnlyStore, WritableStore};

const LOCATION_COLUMNS: &str = "id, provider, provider_location_id, name, admin1, admin2, \
                              country, latitude, longitude, timezone, is_default, sort_order, \
                              created_at";

fn map_location(row: &Row<'_>) -> rusqlite::Result<WeatherLocation> {
    Ok(WeatherLocation {
        id: WeatherLocationId(row.get(0)?),
        provider: row.get(1)?,
        provider_location_id: row.get(2)?,
        name: row.get(3)?,
        admin1: row.get(4)?,
        admin2: row.get(5)?,
        country: row.get(6)?,
        latitude: row.get(7)?,
        longitude: row.get(8)?,
        timezone: row.get(9)?,
        is_default: row.get::<_, i64>(10)? != 0,
        sort_order: row.get(11)?,
        created_at: row.get(12)?,
    })
}

// ---------------------------------------------------------------------------
// 只读
// ---------------------------------------------------------------------------

pub(crate) fn list_weather_locations_conn(
    conn: &Connection,
) -> Result<Vec<WeatherLocation>, CoreError> {
    let sql = format!(
        "SELECT {LOCATION_COLUMNS} FROM weather_locations ORDER BY is_default DESC, sort_order ASC, id ASC"
    );
    let mut stmt = conn.prepare(&sql).store()?;
    let rows = stmt
        .query_map([], map_location)
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    Ok(rows)
}

pub(crate) fn get_weather_location_conn(
    conn: &Connection,
    id: WeatherLocationId,
) -> Result<Option<WeatherLocation>, CoreError> {
    let sql = format!("SELECT {LOCATION_COLUMNS} FROM weather_locations WHERE id = ?1");
    conn.query_row(&sql, params![id.0], map_location)
        .optional()
        .store()
}

pub(crate) fn default_weather_location_conn(
    conn: &Connection,
) -> Result<Option<WeatherLocation>, CoreError> {
    let sql = format!("SELECT {LOCATION_COLUMNS} FROM weather_locations WHERE is_default = 1");
    conn.query_row(&sql, [], map_location).optional().store()
}

/// 读快照。payload 损坏时按「没有缓存」处理，不让界面崩掉。
pub(crate) fn get_weather_snapshot_conn(
    conn: &Connection,
    location_id: WeatherLocationId,
) -> Result<Option<WeatherSnapshot>, CoreError> {
    let row: Option<(String, String, i64, i64)> = conn
        .query_row(
            "SELECT provider, payload, fetched_at, expires_at FROM weather_snapshots \
             WHERE location_id = ?1",
            params![location_id.0],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .store()?;
    let Some((provider, payload, fetched_at, expires_at)) = row else {
        return Ok(None);
    };
    match serde_json::from_str::<StoredWeatherSnapshot>(&payload) {
        // 版本对不上说明是旧格式的缓存：字段语义已经变了
        // （比如逐日预报的图标码从 `icon` 改成了 `iconDay`），
        // 继续用它只会显示错的天气。按「没有缓存」处理，让界面重新拉取。
        Ok(stored) if stored.v != StoredWeatherSnapshot::VERSION => {
            tracing::warn!(
                location_id = location_id.0,
                cached_version = stored.v,
                expected = StoredWeatherSnapshot::VERSION,
                "天气快照格式版本过期，按无缓存处理"
            );
            Ok(None)
        }
        Ok(stored) => Ok(Some(WeatherSnapshot {
            provider,
            fetched_at,
            expires_at,
            current: stored.snapshot.current,
            hourly: stored.snapshot.hourly,
            daily: stored.snapshot.daily,
        })),
        Err(err) => {
            tracing::warn!(
                location_id = location_id.0,
                %err,
                "天气快照 payload 解析失败，按无缓存处理"
            );
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// 写入
// ---------------------------------------------------------------------------

/// 插入地点。
///
/// - 同一 `(provider, provider_location_id)` 已存在时返回既有地点（不重复添加）；
/// - 地点数达到上限时拒绝；
/// - 第一个地点自动成为默认地点。
pub(crate) fn insert_weather_location_conn(
    conn: &mut Connection,
    candidate: &WeatherLocationCandidate,
) -> Result<WeatherLocation, CoreError> {
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM weather_locations WHERE provider = ?1 AND provider_location_id = ?2",
            params![candidate.provider, candidate.provider_location_id],
            |row| row.get(0),
        )
        .optional()
        .store()?;
    if let Some(id) = existing {
        let sql = format!("SELECT {LOCATION_COLUMNS} FROM weather_locations WHERE id = ?1");
        return conn.query_row(&sql, params![id], map_location).store();
    }

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM weather_locations", [], |row| {
            row.get(0)
        })
        .store()?;
    if count as usize >= MAX_WEATHER_LOCATIONS {
        return Err(CoreError::validation(format!(
            "最多只能保存 {MAX_WEATHER_LOCATIONS} 个地点，请先删除一个"
        )));
    }

    let now = now_unix();
    let sort_order = count;
    let is_default = count == 0;
    conn.execute(
        "INSERT INTO weather_locations(provider, provider_location_id, name, admin1, admin2, \
         country, latitude, longitude, timezone, is_default, sort_order, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            candidate.provider,
            candidate.provider_location_id,
            candidate.name,
            candidate.admin1,
            candidate.admin2,
            candidate.country,
            candidate.latitude,
            candidate.longitude,
            candidate.timezone,
            is_default as i64,
            sort_order,
            now,
        ],
    )
    .store()?;
    let id = WeatherLocationId(conn.last_insert_rowid());
    let sql = format!("SELECT {LOCATION_COLUMNS} FROM weather_locations WHERE id = ?1");
    conn.query_row(&sql, params![id.0], map_location).store()
}

pub(crate) fn remove_weather_location_conn(
    conn: &Connection,
    id: WeatherLocationId,
) -> Result<(), CoreError> {
    conn.execute("DELETE FROM weather_locations WHERE id = ?1", params![id.0])
        .store()?;
    Ok(())
}

/// 设为默认地点。`is_default` 上有部分唯一索引，必须在事务里先清零再置位。
pub(crate) fn set_default_weather_location_conn(
    conn: &mut Connection,
    id: WeatherLocationId,
) -> Result<(), CoreError> {
    let tx = conn.transaction().store()?;
    tx.execute("UPDATE weather_locations SET is_default = 0", [])
        .store()?;
    tx.execute(
        "UPDATE weather_locations SET is_default = 1 WHERE id = ?1",
        params![id.0],
    )
    .store()?;
    tx.commit().store()?;
    Ok(())
}

/// 写入快照（覆盖同地点的旧快照）。
pub(crate) fn save_weather_snapshot_conn(
    conn: &Connection,
    location_id: WeatherLocationId,
    snapshot: &WeatherSnapshot,
) -> Result<(), CoreError> {
    let payload = serde_json::to_string(&StoredWeatherSnapshot::new(snapshot.clone()))
        .map_err(|err| CoreError::storage(format!("序列化天气快照失败: {err}")))?;
    conn.execute(
        "INSERT INTO weather_snapshots(location_id, provider, fetched_at, expires_at, payload) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(location_id) DO UPDATE SET provider = excluded.provider, \
         fetched_at = excluded.fetched_at, expires_at = excluded.expires_at, \
         payload = excluded.payload",
        params![
            location_id.0,
            snapshot.provider,
            snapshot.fetched_at,
            snapshot.expires_at,
            payload,
        ],
    )
    .store()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 端口实现
// ---------------------------------------------------------------------------

impl WeatherRepo for WritableStore {
    fn list_weather_locations(&self) -> Result<Vec<WeatherLocation>, CoreError> {
        let conn = self.lock();
        list_weather_locations_conn(&conn)
    }

    fn get_weather_location(
        &self,
        id: WeatherLocationId,
    ) -> Result<Option<WeatherLocation>, CoreError> {
        let conn = self.lock();
        get_weather_location_conn(&conn, id)
    }

    fn get_default_weather_location(&self) -> Result<Option<WeatherLocation>, CoreError> {
        let conn = self.lock();
        default_weather_location_conn(&conn)
    }

    fn insert_weather_location(
        &self,
        candidate: &WeatherLocationCandidate,
    ) -> Result<WeatherLocation, CoreError> {
        let mut conn = self.lock();
        insert_weather_location_conn(&mut conn, candidate)
    }

    fn remove_weather_location(&self, id: WeatherLocationId) -> Result<(), CoreError> {
        let conn = self.lock();
        remove_weather_location_conn(&conn, id)
    }

    fn set_default_weather_location(&self, id: WeatherLocationId) -> Result<(), CoreError> {
        let mut conn = self.lock();
        set_default_weather_location_conn(&mut conn, id)
    }

    fn get_weather_snapshot(
        &self,
        location_id: WeatherLocationId,
    ) -> Result<Option<WeatherSnapshot>, CoreError> {
        let conn = self.lock();
        get_weather_snapshot_conn(&conn, location_id)
    }

    fn save_weather_snapshot(
        &self,
        location_id: WeatherLocationId,
        snapshot: &WeatherSnapshot,
    ) -> Result<(), CoreError> {
        let conn = self.lock();
        save_weather_snapshot_conn(&conn, location_id, snapshot)
    }
}

/// 只读端口：界面侧只能通过它读地点与快照。
impl ReadOnlyStore {
    pub(crate) fn weather_locations_readonly(&self) -> Result<Vec<WeatherLocation>, CoreError> {
        let conn = self.conn()?;
        list_weather_locations_conn(&conn)
    }

    pub(crate) fn weather_snapshot_readonly(
        &self,
        location_id: WeatherLocationId,
    ) -> Result<Option<WeatherSnapshot>, CoreError> {
        let conn = self.conn()?;
        get_weather_snapshot_conn(&conn, location_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_core::weather::{CurrentWeather, PROVIDER_QWEATHER, WeatherKind};

    fn candidate(name: &str, provider_id: &str) -> WeatherLocationCandidate {
        WeatherLocationCandidate {
            provider: PROVIDER_QWEATHER.to_string(),
            provider_location_id: provider_id.to_string(),
            name: name.to_string(),
            admin1: Some("北京市".to_string()),
            admin2: None,
            country: Some("中国".to_string()),
            latitude: 39.9,
            longitude: 116.4,
            timezone: "Asia/Shanghai".to_string(),
        }
    }

    fn snapshot(fetched_at: i64) -> WeatherSnapshot {
        WeatherSnapshot {
            provider: PROVIDER_QWEATHER.to_string(),
            fetched_at,
            expires_at: fetched_at + 1800,
            current: CurrentWeather {
                observed_at: fetched_at,
                temp_c: 21.5,
                feels_like_c: 20.0,
                humidity_percent: 45,
                wind_scale: 2,
                wind_dir: "西北风".into(),
                wind_speed_kmh: 9.0,
                precip_mm: 0.0,
                pressure_hpa: 1012.0,
                visibility_km: 20.0,
                cloud_percent: 10,
                kind: WeatherKind::Clear,
                text: "晴".into(),
            },
            hourly: Vec::new(),
            daily: Vec::new(),
        }
    }

    /// 建一个带完整 schema 的内存库（走 `Store::open`，保证迁移逻辑一致）。
    fn store() -> (tempfile::TempDir, crate::db::Store) {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = crate::db::Store::open(dir.path()).expect("打开库");
        (dir, store)
    }

    #[test]
    fn inserting_the_same_location_twice_is_idempotent() {
        let (_dir, store) = store();
        let writable = store.writable();
        let first =
            insert_weather_location_conn(&mut writable.lock(), &candidate("北京", "101010100"))
                .unwrap();
        let second =
            insert_weather_location_conn(&mut writable.lock(), &candidate("北京", "101010100"))
                .unwrap();
        assert_eq!(first.id, second.id, "同一地点不应重复插入");
        let all = list_weather_locations_conn(&writable.lock()).unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn first_location_becomes_default() {
        let (_dir, store) = store();
        let writable = store.writable();
        let first =
            insert_weather_location_conn(&mut writable.lock(), &candidate("北京", "101010100"))
                .unwrap();
        assert!(first.is_default);
        let second =
            insert_weather_location_conn(&mut writable.lock(), &candidate("上海", "101020100"))
                .unwrap();
        assert!(!second.is_default);
        let default = default_weather_location_conn(&writable.lock())
            .unwrap()
            .unwrap();
        assert_eq!(default.id, first.id);
    }

    #[test]
    fn setting_default_moves_the_flag() {
        let (_dir, store) = store();
        let writable = store.writable();
        let first =
            insert_weather_location_conn(&mut writable.lock(), &candidate("北京", "101010100"))
                .unwrap();
        let second =
            insert_weather_location_conn(&mut writable.lock(), &candidate("上海", "101020100"))
                .unwrap();
        set_default_weather_location_conn(&mut writable.lock(), second.id).unwrap();
        let default = default_weather_location_conn(&writable.lock())
            .unwrap()
            .unwrap();
        assert_eq!(default.id, second.id);
        let all = list_weather_locations_conn(&writable.lock()).unwrap();
        assert_eq!(all.iter().filter(|l| l.is_default).count(), 1);
        assert!(
            !all.iter().find(|l| l.id == first.id).unwrap().is_default,
            "旧默认地点应被取消默认"
        );
    }

    #[test]
    fn location_limit_is_enforced() {
        let (_dir, store) = store();
        let writable = store.writable();
        for index in 0..MAX_WEATHER_LOCATIONS {
            insert_weather_location_conn(
                &mut writable.lock(),
                &candidate("城市", &format!("id-{index}")),
            )
            .unwrap();
        }
        let err =
            insert_weather_location_conn(&mut writable.lock(), &candidate("多的", "id-extra"))
                .unwrap_err();
        assert!(err.to_string().contains("最多"), "应提示上限，实际 {err}");
    }

    #[test]
    fn snapshot_round_trips_and_overwrites() {
        let (_dir, store) = store();
        let writable = store.writable();
        let location =
            insert_weather_location_conn(&mut writable.lock(), &candidate("北京", "101010100"))
                .unwrap();

        assert!(
            get_weather_snapshot_conn(&writable.lock(), location.id)
                .unwrap()
                .is_none()
        );
        save_weather_snapshot_conn(&writable.lock(), location.id, &snapshot(1_000)).unwrap();
        let loaded = get_weather_snapshot_conn(&writable.lock(), location.id)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.fetched_at, 1_000);
        assert_eq!(loaded.current.temp_c, 21.5);

        save_weather_snapshot_conn(&writable.lock(), location.id, &snapshot(2_000)).unwrap();
        let loaded = get_weather_snapshot_conn(&writable.lock(), location.id)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.fetched_at, 2_000, "同地点只保留最新一份");
    }

    #[test]
    fn broken_payload_is_treated_as_no_cache() {
        let (_dir, store) = store();
        let writable = store.writable();
        let location =
            insert_weather_location_conn(&mut writable.lock(), &candidate("北京", "101010100"))
                .unwrap();
        writable
            .lock()
            .execute(
                "INSERT INTO weather_snapshots(location_id, provider, fetched_at, expires_at, payload) \
                 VALUES (?1, 'qweather', 1, 2, 'not-json')",
                params![location.id.0],
            )
            .unwrap();
        assert!(
            get_weather_snapshot_conn(&writable.lock(), location.id)
                .unwrap()
                .is_none()
        );
    }

    /// 旧版本快照必须作废。
    ///
    /// 版本 1 的逐日预报读错了服务商的字段名（把 `iconDay` 当 `icon`），
    /// 7 天天气全退化成「未知」。这类缓存继续用只会显示错的天气，
    /// 所以版本号对不上时按「没有缓存」处理，让界面重新拉取。
    #[test]
    fn outdated_snapshot_version_is_treated_as_no_cache() {
        let (_dir, store) = store();
        let writable = store.writable();
        let location =
            insert_weather_location_conn(&mut writable.lock(), &candidate("北京", "101010100"))
                .unwrap();
        // 造一个版本号过期的 payload：内容合法，但 `v` 是旧值。
        let mut stored = StoredWeatherSnapshot::new(snapshot(1_000));
        stored.v = StoredWeatherSnapshot::VERSION.saturating_sub(1);
        let payload = serde_json::to_string(&stored).unwrap();
        writable
            .lock()
            .execute(
                "INSERT INTO weather_snapshots(location_id, provider, fetched_at, expires_at, payload) \
                 VALUES (?1, 'qweather', 1000, 2000, ?2)",
                params![location.id.0, payload],
            )
            .unwrap();
        assert!(
            get_weather_snapshot_conn(&writable.lock(), location.id)
                .unwrap()
                .is_none(),
            "版本过期的快照应按无缓存处理"
        );
    }

    #[test]
    fn removing_a_location_drops_its_snapshot() {
        let (_dir, store) = store();
        let writable = store.writable();
        let location =
            insert_weather_location_conn(&mut writable.lock(), &candidate("北京", "101010100"))
                .unwrap();
        save_weather_snapshot_conn(&writable.lock(), location.id, &snapshot(1_000)).unwrap();
        remove_weather_location_conn(&writable.lock(), location.id).unwrap();
        assert!(
            list_weather_locations_conn(&writable.lock())
                .unwrap()
                .is_empty()
        );
        assert!(
            get_weather_snapshot_conn(&writable.lock(), location.id)
                .unwrap()
                .is_none()
        );
    }
}
