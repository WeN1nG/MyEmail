//! 日历用例层集成测试：`Command` → 用例 → 仓储 → `Event`。
//!
//! 不涉及网络：日历与邮件协议无关，服务启动后即可用。

use std::sync::Arc;
use std::time::Duration;

use me_core::calendar::{CalendarEventDraft, CalendarId};
use me_core::model::now_unix;
use me_core::port::CalendarRepo;
use me_core::{Command, Event};
use me_store::Store;
use me_sync::SyncService;
use me_sync::events::EventSender;
use tokio::sync::mpsc;

fn start_service(store: &Store) -> (mpsc::UnboundedSender<Command>, mpsc::Receiver<Event>) {
    let (command_tx, command_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, event_rx) = mpsc::channel::<Event>(1024);
    let events = EventSender::new(event_tx, Arc::new(|| {}));
    let service = SyncService::new(
        store.writable(),
        me_proto::ProtoFactory,
        me_proto::ProtoSmtp,
        store.paths(),
        events,
        Arc::new(me_sync::NoopBrowser),
    );
    tokio::spawn(async move { service.run(command_rx).await });
    (command_tx, event_rx)
}

/// 等待满足条件的事件；超时即失败（避免测试挂死）。
async fn wait_for(
    rx: &mut mpsc::Receiver<Event>,
    mut predicate: impl FnMut(&Event) -> bool,
) -> Event {
    let deadline = Duration::from_secs(5);
    let started = std::time::Instant::now();
    loop {
        let remaining = deadline
            .checked_sub(started.elapsed())
            .expect("等待事件超时");
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(event)) => {
                if predicate(&event) {
                    return event;
                }
            }
            Ok(None) => panic!("事件通道已关闭"),
            Err(_) => panic!("等待事件超时"),
        }
    }
}

fn draft(calendar: CalendarId, summary: &str) -> CalendarEventDraft {
    let start = now_unix() + 3_600;
    CalendarEventDraft {
        calendar_id: calendar,
        summary: summary.to_string(),
        description: None,
        location: None,
        start_utc: start,
        end_utc: start + 3_600,
        all_day: false,
        timezone: None,
        rrule: None,
        exdates: Vec::new(),
        reminders: Vec::new(),
    }
}

#[tokio::test]
async fn create_event_through_command_persists_and_emits() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (tx, mut rx) = start_service(&store);

    // 启动即应广播日历集合（含默认日历）。
    let event = wait_for(&mut rx, |event| {
        matches!(event, Event::CalendarsChanged { .. })
    })
    .await;
    let Event::CalendarsChanged { calendars } = event else {
        unreachable!()
    };
    assert_eq!(calendars.len(), 1);
    let calendar = calendars[0].id;

    tx.send(Command::CreateCalendarEvent {
        draft: draft(calendar, "季度评审"),
    })
    .unwrap();

    let event = wait_for(&mut rx, |event| {
        matches!(event, Event::CalendarEventsChanged { .. })
    })
    .await;
    let Event::CalendarEventsChanged { calendar: changed } = event else {
        unreachable!()
    };
    assert_eq!(changed, calendar);

    // 落库可见，且来源为 local、上界为空（无重复规则）。
    let stored = store
        .writable()
        .list_series_in_range(&[calendar], now_unix(), now_unix() + 86_400, 200)
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].summary, "季度评审");
    assert_eq!(stored[0].source, me_core::CalendarSource::Local);
    assert!(stored[0].recurrence_until.is_none());
}

#[tokio::test]
async fn recurring_event_computes_recurrence_until() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (tx, mut rx) = start_service(&store);
    wait_for(&mut rx, |event| {
        matches!(event, Event::CalendarsChanged { .. })
    })
    .await;
    let calendar = store.writable().ensure_default_calendar().unwrap().id;

    let mut d = draft(calendar, "每周例会");
    d.rrule = Some("FREQ=WEEKLY;COUNT=4".into());
    tx.send(Command::CreateCalendarEvent { draft: d }).unwrap();
    wait_for(&mut rx, |event| {
        matches!(event, Event::CalendarEventsChanged { .. })
    })
    .await;

    let stored = store
        .writable()
        .list_series_in_range(&[calendar], now_unix(), now_unix() + 86_400, 200)
        .unwrap();
    assert_eq!(stored.len(), 1);
    // COUNT=4、每周一次 → 上界 = 起点 + 21 天；写库时必须与事件同事务算好。
    let expected = stored[0].start_utc + 21 * 86_400;
    assert_eq!(stored[0].recurrence_until, Some(expected));
}

#[tokio::test]
async fn empty_summary_is_rejected_with_user_message() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (tx, mut rx) = start_service(&store);
    wait_for(&mut rx, |event| {
        matches!(event, Event::CalendarsChanged { .. })
    })
    .await;
    let calendar = store.writable().ensure_default_calendar().unwrap().id;

    tx.send(Command::CreateCalendarEvent {
        draft: draft(calendar, "   "),
    })
    .unwrap();

    let event = wait_for(&mut rx, |event| matches!(event, Event::Error { .. })).await;
    let Event::Error { message, .. } = event else {
        unreachable!()
    };
    assert_eq!(message, "日程标题不能为空");

    // 校验失败不得写库。
    let stored = store
        .writable()
        .list_series_in_range(&[calendar], now_unix(), now_unix() + 86_400, 200)
        .unwrap();
    assert!(stored.is_empty());
}

#[tokio::test]
async fn delete_event_soft_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (tx, mut rx) = start_service(&store);
    wait_for(&mut rx, |event| {
        matches!(event, Event::CalendarsChanged { .. })
    })
    .await;
    let calendar = store.writable().ensure_default_calendar().unwrap().id;

    let created = store
        .writable()
        .create_event(
            &draft(calendar, "临时"),
            None,
            me_core::CalendarSource::Local,
        )
        .unwrap();

    tx.send(Command::DeleteCalendarEvent { id: created.id })
        .unwrap();
    wait_for(&mut rx, |event| {
        matches!(event, Event::CalendarEventsChanged { .. })
    })
    .await;

    assert!(store.writable().get_event(created.id).unwrap().is_none());
}

#[tokio::test]
async fn create_calendar_broadcasts_new_set() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (tx, mut rx) = start_service(&store);
    wait_for(&mut rx, |event| {
        matches!(event, Event::CalendarsChanged { .. })
    })
    .await;

    tx.send(Command::CreateCalendar {
        name: "工作".into(),
        color: "#EF4444".into(),
    })
    .unwrap();

    let event = wait_for(
        &mut rx,
        |event| matches!(event, Event::CalendarsChanged { calendars } if calendars.len() == 2),
    )
    .await;
    let Event::CalendarsChanged { calendars } = event else {
        unreachable!()
    };
    assert_eq!(calendars.len(), 2);
    assert!(calendars.iter().any(|item| item.name == "工作"));
}
