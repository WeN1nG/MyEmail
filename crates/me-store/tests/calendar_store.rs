//! 日历仓储集成测试：真实 SQLite + v3 schema。
//!
//! 重点守护 §4.6 的查询形状：已结束的历史重复系列**不得**挤占当月事件
//! 的查询名额。这是 v0.3 设计评审中实测复现过的缺陷。

use me_core::calendar::{CalendarEventDraft, CalendarEventId, CalendarSeries, CalendarSource};
use me_core::port::{CalendarRepo, MetaRepo};
use me_store::Store;

fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");
    (dir, store)
}

fn draft(calendar: me_core::CalendarId, summary: &str, start: i64, end: i64) -> CalendarEventDraft {
    CalendarEventDraft {
        calendar_id: calendar,
        summary: summary.to_string(),
        description: None,
        location: None,
        start_utc: start,
        end_utc: end,
        all_day: false,
        timezone: None,
        rrule: None,
        exdates: Vec::new(),
        reminders: Vec::new(),
    }
}

const NOV_2026: i64 = 1_793_000_000; // 约 2026-11-01

#[test]
fn default_calendar_is_seeded_and_idempotent() {
    let (_dir, store) = open();
    let writable = store.writable();
    let calendars = writable.list_calendars().expect("list");
    assert_eq!(calendars.len(), 1, "新库应预置一个默认日历");
    assert!(calendars[0].is_default);
    assert!(calendars[0].visible);

    let again = writable.ensure_default_calendar().expect("ensure");
    assert_eq!(again.id, calendars[0].id);
    assert_eq!(writable.list_calendars().unwrap().len(), 1);
}

#[test]
fn create_and_read_back_event() {
    let (_dir, store) = open();
    let writable = store.writable();
    let calendar = writable.ensure_default_calendar().unwrap();

    let mut d = draft(calendar.id, "周会", NOV_2026, NOV_2026 + 3600);
    d.rrule = Some("FREQ=WEEKLY".into());
    d.reminders = vec![10, 30];
    d.exdates = vec![NOV_2026 + 7 * 86_400];

    let event = writable
        .create_event(&d, None, CalendarSource::Local)
        .expect("create");
    assert_eq!(event.summary, "周会");
    assert_eq!(event.reminders, vec![10, 30]);
    assert_eq!(event.exdates.len(), 1);
    assert_eq!(event.source, CalendarSource::Local);
    assert!(!event.uid.is_empty());

    let loaded = writable.get_event(event.id).unwrap().expect("exists");
    assert_eq!(loaded, event);
}

#[test]
fn ended_recurring_series_does_not_crowd_out_current_events() {
    let (_dir, store) = open();
    let writable = store.writable();
    let calendar = writable.ensure_default_calendar().unwrap();

    // 300 条 2015 年就结束的重复系列：按旧查询（升序 + LIMIT）会占满名额。
    let old_start = 1_420_070_400; // 2015-01-01
    let old_until = 1_451_606_400; // 2016-01-01
    for index in 0..300 {
        let mut d = draft(
            calendar.id,
            &format!("旧例会{index}"),
            old_start + index * 3600,
            old_start + index * 3600 + 3600,
        );
        d.rrule = Some("FREQ=WEEKLY;UNTIL=20160101T000000Z".into());
        writable
            .create_event(&d, Some(old_until), CalendarSource::Local)
            .unwrap();
    }

    // 5 条当月事件。
    for index in 0..5 {
        let d = draft(
            calendar.id,
            &format!("本月事件{index}"),
            NOV_2026 + index * 86_400,
            NOV_2026 + index * 86_400 + 3600,
        );
        writable
            .create_event(&d, None, CalendarSource::Local)
            .unwrap();
    }

    // 1 条无限重复（上界为 NULL），必须保留。
    let mut infinite = draft(calendar.id, "长期周会", old_start, old_start + 3600);
    infinite.rrule = Some("FREQ=WEEKLY".into());
    writable
        .create_event(&infinite, None, CalendarSource::Local)
        .unwrap();

    let from = NOV_2026;
    let to = NOV_2026 + 30 * 86_400;
    let series = writable
        .list_series_in_range(&[calendar.id], from, to, 200)
        .expect("range");

    let current = series
        .iter()
        .filter(|s| s.summary.starts_with("本月事件"))
        .count();
    assert_eq!(current, 5, "当月事件被历史重复系列挤掉了");
    assert!(
        series.iter().any(|s| s.summary == "长期周会"),
        "无限重复系列不应被上界过滤"
    );
    assert!(
        !series.iter().any(|s| s.summary.starts_with("旧例会")),
        "已结束的重复系列不应出现在可视范围"
    );
}

#[test]
fn update_bumps_sequence_and_keeps_source() {
    let (_dir, store) = open();
    let writable = store.writable();
    let calendar = writable.ensure_default_calendar().unwrap();
    let event = writable
        .create_event(
            &draft(calendar.id, "初版", NOV_2026, NOV_2026 + 3600),
            None,
            CalendarSource::Ai,
        )
        .unwrap();
    assert_eq!(event.sequence, 0);

    let mut d = draft(calendar.id, "改期", NOV_2026 + 7200, NOV_2026 + 10_800);
    d.location = Some("会议室 3".into());
    let updated = writable.update_event(event.id, &d, None).unwrap();

    assert_eq!(updated.summary, "改期");
    assert_eq!(updated.location.as_deref(), Some("会议室 3"));
    assert_eq!(updated.sequence, 1, "更新必须递增 sequence");
    assert_eq!(
        updated.source,
        CalendarSource::Ai,
        "更新不得改写来源，否则 AI 撤销会误删用户编辑过的事件"
    );
}

#[test]
fn soft_delete_removes_event_from_range() {
    let (_dir, store) = open();
    let writable = store.writable();
    let calendar = writable.ensure_default_calendar().unwrap();
    let event = writable
        .create_event(
            &draft(calendar.id, "待删", NOV_2026, NOV_2026 + 3600),
            None,
            CalendarSource::Local,
        )
        .unwrap();

    writable.soft_delete_event(event.id).unwrap();
    assert!(writable.get_event(event.id).unwrap().is_none());
    let series = writable
        .list_series_in_range(&[calendar.id], NOV_2026 - 86_400, NOV_2026 + 86_400, 200)
        .unwrap();
    assert!(series.is_empty());
    // 重复删除返回错误，避免上层误以为成功。
    assert!(writable.soft_delete_event(event.id).is_err());
}

#[test]
fn exceptions_are_scoped_per_event() {
    let (_dir, store) = open();
    let writable = store.writable();
    let calendar = writable.ensure_default_calendar().unwrap();
    let event = writable
        .create_event(
            &draft(calendar.id, "每周例会", NOV_2026, NOV_2026 + 3600),
            None,
            CalendarSource::Local,
        )
        .unwrap();

    let exception = me_core::CalendarException {
        recurrence_at: NOV_2026 + 7 * 86_400,
        cancelled: false,
        summary: Some("改期".into()),
        description: None,
        location: None,
        start_utc: Some(NOV_2026 + 8 * 86_400),
        end_utc: Some(NOV_2026 + 8 * 86_400 + 3600),
        all_day: None,
    };
    writable.insert_exception(event.id, &exception).unwrap();

    let rows = writable
        .list_exceptions(&[event.id, CalendarEventId(9999)])
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, event.id);
    assert_eq!(rows[0].1.summary.as_deref(), Some("改期"));
    assert_eq!(rows[0].1.start_utc, exception.start_utc);
}

#[test]
fn reminder_fire_is_recorded_once() {
    let (_dir, store) = open();
    let writable = store.writable();
    let calendar = writable.ensure_default_calendar().unwrap();
    let event = writable
        .create_event(
            &draft(calendar.id, "提醒", NOV_2026, NOV_2026 + 3600),
            None,
            CalendarSource::Local,
        )
        .unwrap();

    assert!(
        writable
            .record_reminder_fire(event.id, NOV_2026, 15)
            .unwrap(),
        "首次触发应写入"
    );
    assert!(
        !writable
            .record_reminder_fire(event.id, NOV_2026, 15)
            .unwrap(),
        "重复触发必须被去重，否则每 30 秒弹一次窗"
    );
}

#[test]
fn reminder_candidates_only_include_pending_series() {
    let (_dir, store) = open();
    let writable = store.writable();
    let calendar = writable.ensure_default_calendar().unwrap();

    let mut with_reminder = draft(calendar.id, "有提醒", NOV_2026, NOV_2026 + 3600);
    with_reminder.reminders = vec![15];
    writable
        .create_event(&with_reminder, None, CalendarSource::Local)
        .unwrap();
    writable
        .create_event(
            &draft(calendar.id, "无提醒", NOV_2026 + 3600, NOV_2026 + 7200),
            None,
            CalendarSource::Local,
        )
        .unwrap();

    let candidates = writable
        .list_reminder_candidates(NOV_2026 - 3600, NOV_2026 + 86_400, 200)
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].summary, "有提醒");
    assert_eq!(candidates[0].reminders, vec![15]);
}

#[test]
fn range_query_is_bounded_to_requested_calendars() {
    let (_dir, store) = open();
    let writable = store.writable();
    let default = writable.ensure_default_calendar().unwrap();
    let second = writable.create_calendar("工作", "#EF4444").unwrap();

    writable
        .create_event(
            &draft(default.id, "私人", NOV_2026, NOV_2026 + 3600),
            None,
            CalendarSource::Local,
        )
        .unwrap();
    writable
        .create_event(
            &draft(second.id, "工作会", NOV_2026, NOV_2026 + 3600),
            None,
            CalendarSource::Local,
        )
        .unwrap();

    let only_work: Vec<CalendarSeries> = writable
        .list_series_in_range(&[second.id], NOV_2026 - 86_400, NOV_2026 + 86_400, 200)
        .unwrap();
    assert_eq!(only_work.len(), 1);
    assert_eq!(only_work[0].summary, "工作会");
}

#[test]
fn schema_version_is_three() {
    let (_dir, store) = open();
    assert_eq!(store.writable().schema_version().unwrap(), 3);
}
