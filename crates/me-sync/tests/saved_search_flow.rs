//! 保存的搜索的用例层集成测试：`Command` → 用例 → 仓储 → `Event`。
//!
//! 不联网、不碰 IMAP——这里验证的是**编排**：命令被正确 dispatch、
//! 事件按预期下发、名称重复时报错、执行后 `last_used_at` 更新。

use std::sync::Arc;
use std::time::Duration;

use me_core::model::SavedSearchQuery;
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

async fn wait_for(
    rx: &mut mpsc::Receiver<Event>,
    mut predicate: impl FnMut(&Event) -> bool,
) -> Event {
    let deadline = Duration::from_secs(5);
    let started = std::time::Instant::now();
    loop {
        let remaining = deadline.checked_sub(started.elapsed()).expect("等待超时");
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(event)) => {
                if predicate(&event) {
                    return event;
                }
            }
            Ok(None) => panic!("事件通道提前关闭"),
            Err(_) => panic!("等待事件超时"),
        }
    }
}

fn sample_query() -> SavedSearchQuery {
    SavedSearchQuery {
        text: "报价".to_string(),
        from: Some("zhang".to_string()),
        subject: Some("预算".to_string()),
        unread_only: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn saving_a_search_emits_the_updated_list() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");
    let (tx, mut rx) = start_service(&store);

    tx.send(Command::SaveSearch {
        name: "张老板的报价".to_string(),
        query: sample_query(),
    })
    .expect("send");

    let event = wait_for(&mut rx, |e| matches!(e, Event::SavedSearchesChanged { .. })).await;

    match event {
        Event::SavedSearchesChanged { searches } => {
            assert_eq!(searches.len(), 1, "保存后列表应有一条");
            assert_eq!(searches[0].name, "张老板的报价");
            // 条件必须原样保存（只存条件，不含分页态）。
            assert_eq!(searches[0].query.text, "报价");
            assert_eq!(searches[0].query.from.as_deref(), Some("zhang"));
            assert_eq!(searches[0].query.subject.as_deref(), Some("预算"));
            assert!(searches[0].query.unread_only);
        }
        other => panic!("期望 SavedSearchesChanged，实际 {other:?}"),
    }
}

#[tokio::test]
async fn deleting_a_search_emits_the_shrunk_list() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");
    let (tx, mut rx) = start_service(&store);

    tx.send(Command::SaveSearch {
        name: "待删".to_string(),
        query: sample_query(),
    })
    .expect("send");
    let event = wait_for(&mut rx, |e| matches!(e, Event::SavedSearchesChanged { .. })).await;
    let id = match event {
        Event::SavedSearchesChanged { searches } => searches[0].id,
        other => panic!("期望 SavedSearchesChanged，实际 {other:?}"),
    };

    tx.send(Command::DeleteSearch { id }).expect("send");
    let event = wait_for(
        &mut rx,
        |e| matches!(e, Event::SavedSearchesChanged { searches } if searches.is_empty()),
    )
    .await;
    match event {
        Event::SavedSearchesChanged { searches } => assert!(searches.is_empty()),
        other => panic!("期望 SavedSearchesChanged，实际 {other:?}"),
    }
}

#[tokio::test]
async fn duplicate_name_reports_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");
    let (tx, mut rx) = start_service(&store);

    tx.send(Command::SaveSearch {
        name: "同名".to_string(),
        query: sample_query(),
    })
    .expect("send");
    wait_for(&mut rx, |e| matches!(e, Event::SavedSearchesChanged { .. })).await;

    tx.send(Command::SaveSearch {
        name: "同名".to_string(),
        query: sample_query(),
    })
    .expect("send");

    let event = wait_for(&mut rx, |e| matches!(e, Event::Error { .. })).await;
    match event {
        Event::Error { message, .. } => {
            assert!(
                message.contains("名字"),
                "错误信息应说明是名字重复，实际：{message}"
            );
        }
        other => panic!("期望 Event::Error，实际 {other:?}"),
    }
}

#[tokio::test]
async fn empty_name_is_rejected_before_touching_storage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");
    let (tx, mut rx) = start_service(&store);

    tx.send(Command::SaveSearch {
        name: "   ".to_string(),
        query: sample_query(),
    })
    .expect("send");

    let event = wait_for(&mut rx, |e| matches!(e, Event::Error { .. })).await;
    match event {
        Event::Error { message, .. } => assert!(message.contains("名称")),
        other => panic!("期望 Event::Error，实际 {other:?}"),
    }
}

#[tokio::test]
async fn running_a_saved_search_touches_it_and_searches() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");
    let (tx, mut rx) = start_service(&store);

    tx.send(Command::SaveSearch {
        name: "会执行的".to_string(),
        query: sample_query(),
    })
    .expect("send");
    let event = wait_for(&mut rx, |e| matches!(e, Event::SavedSearchesChanged { .. })).await;
    let id = match event {
        Event::SavedSearchesChanged { searches } => searches[0].id,
        other => panic!("期望 SavedSearchesChanged，实际 {other:?}"),
    };

    tx.send(Command::RunSavedSearch { id }).expect("send");

    // 执行会先 touch（下发列表），再搜索（下发结果）。
    let mut touched = false;
    let mut searched = false;
    let deadline = Duration::from_secs(5);
    let started = std::time::Instant::now();
    while !searched {
        let remaining = deadline.checked_sub(started.elapsed()).expect("等待超时");
        let event = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("等待超时")
            .expect("通道关闭");
        match event {
            Event::SavedSearchesChanged { .. } => touched = true,
            Event::SearchResults { .. } => searched = true,
            _ => {}
        }
    }
    assert!(touched, "执行保存的搜索应先 touch（更新 last_used_at）");
}

#[tokio::test]
async fn running_a_missing_search_reports_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");
    let (tx, mut rx) = start_service(&store);

    tx.send(Command::RunSavedSearch {
        id: me_core::model::SavedSearchId(9999),
    })
    .expect("send");

    let event = wait_for(&mut rx, |e| matches!(e, Event::Error { .. })).await;
    match event {
        Event::Error { message, .. } => assert!(message.contains("不存在")),
        other => panic!("期望 Event::Error，实际 {other:?}"),
    }
}
