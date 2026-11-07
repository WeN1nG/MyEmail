//! AI 用例层集成测试：`Command` → 用例 → 仓储 → `Event`。
//!
//! 全部用脚本化的 `MockAiProvider`：不联网、不碰系统密钥库、不读真实 API。
//! 这里验证的是**编排**（流式、节流、工具循环、取消、错误分类与落库），
//! 协议解码已由 `me-proto` 的单元测试覆盖。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use me_core::ai::{
    AiChatRequest, AiConversationId, AiDelta, AiDeltaSink, AiFuture, AiProvider, AiProviderKind,
    AiToolCall, AiToolName, AiTurnResult, CancelFlag,
};
use me_core::port::{AiMemoryRepo, AiRepo, MetaRepo};
use me_core::{AiErrorKind, Command, CoreError, Event};
use me_store::Store;
use me_sync::SyncService;
use me_sync::events::EventSender;
use serde_json::json;
use tokio::sync::mpsc;

/// 一轮模型调用的脚本。
struct Turn {
    text: String,
    tool_calls: Vec<AiToolCall>,
    error: Option<AiErrorKind>,
    /// 返回前等待取消标志（模拟长时间输出）。
    hold: bool,
}

impl Turn {
    fn text(text: &str) -> Self {
        Self {
            text: text.to_string(),
            tool_calls: Vec::new(),
            error: None,
            hold: false,
        }
    }

    fn with_tool(call: AiToolCall) -> Self {
        Self {
            text: String::new(),
            tool_calls: vec![call],
            error: None,
            hold: false,
        }
    }
}

/// 构造一次工具调用。`parse_ok` 恒为真：这些测试关心的是参数**语义**，不是 JSON 坏掉的情况。
fn tool_call(id: &str, name: AiToolName, args: serde_json::Value) -> AiToolCall {
    AiToolCall {
        id: id.to_string(),
        name,
        arguments: args,
        parse_ok: true,
    }
}

/// 把一段文本折成一次 `plan_submit`。
///
/// PLAN 阶段只下发 `plan_submit` / `answer_directly`，所以任何「直接调业务工具」的
/// 剧本都必须先交一份计划——这正是这个功能想要的约束，测试也照此写。
fn plan_submit_for(tools: &[AiToolName]) -> AiToolCall {
    let steps: Vec<serde_json::Value> = tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let (capability, args, title) = match tool {
                AiToolName::EmailSearch => ("mail", json!({ "days": 7 }), "检索近 7 天的邮件"),
                AiToolName::MailRead => ("mail", json!({ "message_id": 1 }), "读取邮件正文"),
                AiToolName::MemoryUpsert => (
                    "mail",
                    json!({ "kind": "person", "canonical_name": "张三" }),
                    "记住这个人",
                ),
                AiToolName::CalendarList => ("calendar", json!({ "keyword": "复盘" }), "查看日程"),
                AiToolName::CalendarCreate => (
                    "calendar",
                    json!({ "title": "周五复盘", "start": "2026-11-06T15:00:00+08:00" }),
                    "创建日程",
                ),
                AiToolName::CalendarDelete => ("calendar", json!({ "event_id": 1 }), "删除日程"),
                AiToolName::WeatherGet => ("weather", json!({ "location": "成都" }), "查询天气"),
                AiToolName::MusicSearch => ("music", json!({ "query": "晴天" }), "搜索曲库"),
                _ => ("mail", json!({ "query": "报价单" }), "检索邮件"),
            };
            json!({
                "id": index + 1,
                "title": title,
                "kind": "tool",
                "capability": capability,
                "tool": tool.as_str(),
                "args": args,
                "expect": "拿到结果"
            })
        })
        .collect();
    tool_call(
        "plan",
        AiToolName::PlanSubmit,
        json!({
            "motive": "先给出计划再执行",
            "goal": "完成用户这一次的请求",
            "steps": steps
        }),
    )
}

/// 读回落库的最后一条助手消息（断言「终止态」用）。
fn last_assistant_message(store: &Store, conversation: AiConversationId) -> me_core::ai::AiMessage {
    let writable = store.writable();
    let messages = writable
        .list_messages(conversation, None, 20)
        .expect("list messages");
    messages
        .into_iter()
        .rev()
        .find(|message| message.role == me_core::ai::AiRole::Assistant)
        .expect("至少要有一条助手消息")
}

struct MockAiProvider {
    script: Mutex<VecDeque<Turn>>,
    /// 每轮请求携带的 `tools` 数量（用于验证开关真的收紧了请求）。
    tool_counts: Mutex<Vec<usize>>,
    /// 每轮请求里的工具名（用于验证「重新生成不给写工具」这类精确断言）。
    tool_names: Mutex<Vec<Vec<String>>>,
    /// 每轮请求里的消息条数（用于验证工具结果确实回灌给模型）。
    message_counts: Mutex<Vec<usize>>,
    /// 每轮请求用的 `(protocol, model)`（用于验证切档案真的换了参数）。
    targets: Mutex<Vec<(String, String)>>,
}

impl MockAiProvider {
    fn new(turns: Vec<Turn>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(turns.into()),
            tool_counts: Mutex::new(Vec::new()),
            tool_names: Mutex::new(Vec::new()),
            message_counts: Mutex::new(Vec::new()),
            targets: Mutex::new(Vec::new()),
        })
    }
}

impl AiProvider for MockAiProvider {
    fn stream_chat(
        &self,
        request: AiChatRequest,
        sink: AiDeltaSink,
        cancel: CancelFlag,
    ) -> AiFuture<Result<AiTurnResult, CoreError>> {
        let tool_count = request.tools.len();
        let message_count = request.messages.len();
        if let Ok(mut counts) = self.tool_counts.lock() {
            counts.push(tool_count);
        }
        if let Ok(mut names) = self.tool_names.lock() {
            names.push(
                request
                    .tools
                    .iter()
                    .map(|spec| spec.name.as_str().to_string())
                    .collect(),
            );
        }
        if let Ok(mut counts) = self.message_counts.lock() {
            counts.push(message_count);
        }
        if let Ok(mut targets) = self.targets.lock() {
            targets.push((
                request.config.kind.as_str().to_string(),
                request.config.model.clone(),
            ));
        }
        let turn = self.script.lock().expect("script").pop_front();
        Box::pin(async move {
            let Some(turn) = turn else {
                return Err(CoreError::ai(
                    AiErrorKind::Limit,
                    None::<String>,
                    "脚本已用尽",
                ));
            };
            if let Some(kind) = turn.error {
                return Err(CoreError::ai(kind, None::<String>, "脚本化错误"));
            }
            if turn.hold {
                // 一直等到被取消（服务端 select 会 drop 这个 future）。
                for _ in 0..200 {
                    if cancel.is_cancelled() {
                        return Err(CoreError::cancelled());
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                return Err(CoreError::cancelled());
            }
            // 按两段吐字，验证增量拼接。
            let mut chars = turn.text.chars();
            let head: String = chars.by_ref().take(turn.text.chars().count() / 2).collect();
            let tail: String = chars.collect();
            if !head.is_empty() {
                sink(AiDelta::Text(head));
            }
            if !tail.is_empty() {
                sink(AiDelta::Text(tail));
            }
            Ok(AiTurnResult {
                text: turn.text,
                tool_calls: turn.tool_calls,
                input_tokens: Some(11),
                output_tokens: Some(22),
                stop_reason: None,
            })
        })
    }
}

fn start_service(
    store: &Store,
    provider: Arc<MockAiProvider>,
    privacy_ack: bool,
) -> (mpsc::UnboundedSender<Command>, mpsc::Receiver<Event>) {
    let (command_tx, command_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, event_rx) = mpsc::channel::<Event>(1024);
    let events = EventSender::new(event_tx, Arc::new(|| {}));
    let mut service = SyncService::new(
        store.writable(),
        me_proto::ProtoFactory,
        me_proto::ProtoSmtp,
        store.paths(),
        events,
        Arc::new(me_sync::NoopBrowser),
    );
    // 模型档案是请求参数的唯一来源：key 就是其中一个普通字段。
    let mut profile = me_core::ai::AiModel::preset(
        AiProviderKind::Anthropic,
        "m1".to_string(),
        "测试模型".to_string(),
    );
    profile.api_key = "sk-test".to_string();
    service = service.with_ai_provider(provider);
    service = service.with_ai_models(vec![profile], Some("m1".to_string()));
    if privacy_ack {
        service = service.with_ai_privacy_ack();
    }
    tokio::spawn(async move { service.run(command_rx).await });
    (command_tx, event_rx)
}

/// 只起服务、不注入档案：用来走「从 meta 读模型档案」这条启动路径。
///
/// `start_service` 会先 `with_ai_models`，而用例层看到内存里已有档案就直接返回，
/// 于是 meta 里的脏数据永远不会被读到——测「解析失败」必须绕开那个注入。
/// 建一个会话并等界面拿到通知——每个用例开头都是这三行。
async fn new_conversation(
    commands: &mpsc::UnboundedSender<Command>,
    rx: &mut mpsc::Receiver<Event>,
    store: &Store,
) -> AiConversationId {
    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(rx, |event| matches!(event, Event::AiConversationsChanged)).await;
    latest_conversation(store)
}

fn start_service_bare(store: &Store) -> (mpsc::UnboundedSender<Command>, mpsc::Receiver<Event>) {
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

fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");
    (dir, store)
}

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
            Ok(None) => panic!("事件通道关闭"),
            Err(_) => panic!("等待事件超时"),
        }
    }
}

/// 轮询到会话的 `model_id` 变成期望值。避免和「发送完成」发出的会话变更事件混淆。
async fn wait_conversation_model(store: &Store, id: AiConversationId, expect: &str) {
    for _ in 0..200 {
        let found = store
            .writable()
            .list_conversations(None, 10)
            .ok()
            .and_then(|list| list.into_iter().find(|item| item.id == id))
            .and_then(|item| item.model_id);
        if found.as_deref() == Some(expect) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("会话模型未切到 {expect}");
}

fn latest_conversation(store: &Store) -> AiConversationId {
    store
        .writable()
        .list_conversations(None, 10)
        .expect("list")
        .first()
        .expect("至少一个会话")
        .id
}

#[tokio::test]
async fn streams_a_reply_and_persists_it_once() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![Turn::text("你好，这是回复。")]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "介绍一下你自己".to_string(),
            context: Vec::new(),
        })
        .expect("send");

    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiStreamStarted { .. })
    })
    .await;
    let mut deltas = String::new();
    loop {
        let event = wait_for(&mut rx, |event| {
            matches!(
                event,
                Event::AiStreamDelta { .. } | Event::AiMessageCompleted { .. }
            )
        })
        .await;
        match event {
            Event::AiStreamDelta { delta, .. } => deltas.push_str(&delta),
            Event::AiMessageCompleted { status, .. } => {
                assert_eq!(status, me_core::ai::AiMessageStatus::Complete);
                break;
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert_eq!(deltas, "你好，这是回复。");

    let messages = store
        .writable()
        .list_messages(conversation, None, 10)
        .expect("list");
    assert_eq!(messages.len(), 2, "user + assistant 各一条");
    assert_eq!(messages[0].role, me_core::ai::AiRole::User);
    assert_eq!(messages[1].content, "你好，这是回复。");
    assert_eq!(messages[1].input_tokens, Some(11));
    assert_eq!(messages[1].status, me_core::ai::AiMessageStatus::Complete);

    // 自动标题取首条用户消息。
    let conversation_row = store
        .writable()
        .get_conversation(conversation)
        .expect("get")
        .expect("存在");
    assert_eq!(conversation_row.title, "介绍一下你自己");
}

#[tokio::test]
async fn tool_round_writes_entity_and_feeds_result_back() {
    let (_dir, store) = open();
    let call = AiToolCall {
        id: "call_1".to_string(),
        name: AiToolName::MemoryUpsert,
        arguments: json!({
            "kind": "person",
            "canonical_name": "张三",
            "aliases": ["老张"],
            "email": "zhang@example.com"
        }),
        parse_ok: true,
    };
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::MemoryUpsert])),
        Turn::with_tool(call),
        Turn::text("记住了。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "记住张三是我同事，邮箱 zhang@example.com".to_string(),
            context: Vec::new(),
        })
        .expect("send");

    // 一次收集到完成事件：工具事件、实体事件、完成事件的顺序并不固定。
    let mut saw_tool_ok = false;
    let mut saw_entities_changed = false;
    loop {
        let event = wait_for(&mut rx, |event| {
            matches!(
                event,
                Event::AiToolCompleted { .. }
                    | Event::AiEntitiesChanged
                    | Event::AiMessageCompleted { .. }
                    | Event::AiStreamFailed { .. }
                    | Event::Error { .. }
            )
        })
        .await;
        match event {
            Event::AiToolCompleted {
                tool,
                status,
                summary,
                ..
            } => {
                assert_eq!(tool, AiToolName::MemoryUpsert);
                assert_eq!(status, me_core::ai::AiToolStatus::Ok, "工具失败：{summary}");
                saw_tool_ok = true;
            }
            Event::AiEntitiesChanged => saw_entities_changed = true,
            Event::AiMessageCompleted { status, .. } => {
                assert_eq!(status, me_core::ai::AiMessageStatus::Complete);
                break;
            }
            other => panic!("工具流程失败：{other:?}"),
        }
    }
    assert!(saw_tool_ok, "应收到工具成功事件");
    assert!(saw_entities_changed, "记忆写入后应通知 UI 刷新实体");

    let entities = store.writable().list_entities(10).expect("entities");
    assert_eq!(entities.len(), 1);
    assert_eq!(entities[0].canonical_name, "张三");
    assert_eq!(entities[0].aliases, vec!["老张".to_string()]);

    // 三轮请求：提交计划 → 执行工具 → 收尾。工具结果必须回灌给模型。
    let counts = provider.message_counts.lock().unwrap().clone();
    assert_eq!(counts.len(), 3, "工具循环应调用三次模型");
    assert!(counts[2] > counts[1], "工具结果必须回灌给模型");

    let messages = store
        .writable()
        .list_messages(conversation, None, 10)
        .expect("list");
    let assistant = messages
        .iter()
        .find(|message| message.role == me_core::ai::AiRole::Assistant)
        .expect("助手消息");
    assert_eq!(assistant.content, "记住了。");
    assert_eq!(assistant.tool_trace.len(), 1);
    assert_eq!(assistant.tool_trace[0].tool, AiToolName::MemoryUpsert);
}

#[tokio::test]
async fn cancelling_keeps_partial_content_and_marks_cancelled() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![Turn {
        text: "这段不会出现".to_string(),
        tool_calls: Vec::new(),
        error: None,
        hold: true,
    }]);
    let (commands, mut rx) = start_service(&store, provider, true);

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "写一篇长文".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiStreamStarted { .. })
    })
    .await;

    commands
        .send(Command::CancelAiStream { conversation })
        .expect("send");
    let event = wait_for(&mut rx, |event| {
        matches!(
            event,
            Event::AiMessageCompleted { .. } | Event::AiStreamFailed { .. }
        )
    })
    .await;
    match event {
        Event::AiMessageCompleted { status, .. } => {
            assert_eq!(status, me_core::ai::AiMessageStatus::Cancelled);
        }
        other => panic!("取消应落到 cancelled，实际 {other:?}"),
    }

    let messages = store
        .writable()
        .list_messages(conversation, None, 10)
        .expect("list");
    let assistant = messages
        .iter()
        .find(|message| message.role == me_core::ai::AiRole::Assistant)
        .expect("助手消息");
    assert_eq!(assistant.status, me_core::ai::AiMessageStatus::Cancelled);
}

#[tokio::test]
async fn auth_error_is_reported_not_retryable() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![Turn {
        text: String::new(),
        tool_calls: Vec::new(),
        error: Some(AiErrorKind::Auth),
        hold: false,
    }]);
    let (commands, mut rx) = start_service(&store, provider, true);

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "你好".to_string(),
            context: Vec::new(),
        })
        .expect("send");

    let event = wait_for(&mut rx, |event| {
        matches!(event, Event::AiStreamFailed { .. })
    })
    .await;
    match event {
        Event::AiStreamFailed {
            retryable,
            error_text,
            ..
        } => {
            assert!(!retryable, "401 不应标记为可重试");
            assert!(error_text.contains("API Key"), "文案应指向 Key 配置");
        }
        other => panic!("unexpected: {other:?}"),
    }

    let messages = store
        .writable()
        .list_messages(conversation, None, 10)
        .expect("list");
    let assistant = messages
        .iter()
        .find(|message| message.role == me_core::ai::AiRole::Assistant)
        .expect("助手消息");
    assert_eq!(assistant.status, me_core::ai::AiMessageStatus::Failed);
}

#[tokio::test]
async fn privacy_ack_is_required_before_any_request() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![Turn::text("不应被调用")]);
    let (commands, mut rx) = start_service(&store, provider.clone(), false);

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "你好".to_string(),
            context: Vec::new(),
        })
        .expect("send");

    let event = wait_for(&mut rx, |event| matches!(event, Event::Error { .. })).await;
    match event {
        Event::Error { message, .. } => assert!(message.contains("隐私")),
        other => panic!("unexpected: {other:?}"),
    }
    assert!(
        provider.message_counts.lock().unwrap().is_empty(),
        "未确认隐私时不得发起请求"
    );
    assert!(
        store
            .writable()
            .list_messages(conversation, None, 10)
            .expect("list")
            .is_empty(),
        "被拒绝的发送不应落库"
    );
}

#[tokio::test]
async fn disabled_tools_are_removed_from_the_request() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::text("好的"),
    ]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);

    // 工具开关现在是模型档案的一部分，不是全局设置。
    let mut profile = me_core::ai::AiModel::preset(
        AiProviderKind::Anthropic,
        "m1".to_string(),
        "测试模型".to_string(),
    );
    profile.tools = me_core::ai::AiToolPermissions {
        weather: false,
        calendar_create: false,
        ..me_core::ai::AiToolPermissions::default()
    };
    // 整个档案参与保存：key 也得带上，否则覆盖注入的那份就没 key 了。
    profile.api_key = "sk-test".to_string();
    commands
        .send(Command::SaveAiModel { model: profile })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConfigChanged { .. })
    })
    .await;

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "你好".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let counts = provider.tool_counts.lock().unwrap().clone();
    // 第一轮是 PLAN 阶段（只有 plan_submit / answer_directly）；
    // 第二轮是执行阶段：16 个工具 - 天气两个 - 创建日程一个 - 两个计划工具 = 11。
    assert_eq!(
        counts,
        vec![2, 11],
        "执行阶段应剩十一个业务工具，实际：{counts:?}"
    );
}

#[tokio::test]
async fn duplicate_model_name_is_rejected_and_list_is_unchanged() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![]);
    let (commands, mut rx) = start_service(&store, provider, true);

    // 注入的档案名是「测试模型」，再存一个同名（大小写不同）必须被拒。
    let mut clash = me_core::ai::AiModel::preset(
        AiProviderKind::Anthropic,
        "m2".to_string(),
        " 测试模型 ".to_string(),
    );
    clash.name = " 测试模型 ".to_string();
    commands
        .send(Command::SaveAiModel { model: clash })
        .expect("send");

    // 只等 Error：启动时还会推一条 AiConfigChanged，用「二选一」会提前返回。
    let event = wait_for(&mut rx, |event| matches!(event, Event::Error { .. })).await;
    match event {
        Event::Error { message, .. } => assert!(message.contains("已存在"), "{message}"),
        other => panic!("重名应当报错，实际收到 {other:?}"),
    }

    // 被拒之后不会再有配置快照（正是「什么都没发生」），所以直接查落库结果。
    let raw = store
        .writable()
        .get_setting(me_core::settings_key::AI_MODELS)
        .expect("读 meta");
    let models: Vec<me_core::ai::AiModel> = raw
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_default();
    assert!(
        models.iter().all(|model| model.id != "m2"),
        "被拒的档案不得落库：{models:?}"
    );
}

#[tokio::test]
async fn switching_conversation_model_changes_the_request_target() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![Turn::text("第一答"), Turn::text("第二答")]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);

    // 再加一套 OpenAI 兼容档案，并设成当前模型。
    let mut local = me_core::ai::AiModel::preset(
        AiProviderKind::OpenAiCompatible,
        "m2".to_string(),
        "本地模型".to_string(),
    );
    local.base_url = "http://127.0.0.1:11434/v1".to_string();
    local.model = "qwen-plus".to_string();
    commands
        .send(Command::SaveAiModel { model: local })
        .expect("send");
    // 等「列表里真的有 m2」：启动时那条旧快照也可能先到。
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConfigChanged { models, .. }
            if models.iter().any(|model| model.id == "m2"))
    })
    .await;

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    // 新建对话跟随「当前模型」= m2。
    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "你是谁".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    // 切回 m1（Anthropic）再问一次。
    commands
        .send(Command::SetConversationAiModel {
            conversation,
            model: "m1".to_string(),
        })
        .expect("send");
    // 直接盯库：发送完成也会推 AiConversationsChanged，只看事件会提前放行。
    wait_conversation_model(&store, conversation, "m1").await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "再说一次".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let targets = provider.targets.lock().unwrap().clone();
    assert_eq!(targets.len(), 2, "两次发送各一轮请求");
    assert_eq!(
        targets[0].0, "openai_compatible",
        "新建对话应跟随当前模型 m2"
    );
    assert_eq!(targets[0].1, "qwen-plus");
    assert_eq!(targets[1].0, "anthropic", "切换后应走 m1 的协议");
    assert_eq!(targets[1].1, "claude-sonnet-4-5");
}

/// Key 就是普通参数：保存档案时一起进 `meta.ai_models`，重启后照常读回。
#[tokio::test]
async fn api_key_is_persisted_with_the_rest_of_the_profile() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![]);
    let (commands, mut rx) = start_service(&store, provider, true);

    let mut profile = me_core::ai::AiModel::preset(
        AiProviderKind::OpenAiCompatible,
        "m3".to_string(),
        "车间模型".to_string(),
    );
    profile.base_url = "http://127.0.0.1:11434/v1".to_string();
    profile.model = "qwen3".to_string();
    profile.api_key = "sk-workshop-123".to_string();
    profile.max_input_chars = 30_000;
    profile.max_output_tokens = 1_024;
    commands
        .send(Command::SaveAiModel { model: profile })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConfigChanged { models, .. }
            if models.iter().any(|model| model.id == "m3"))
    })
    .await;

    let raw = store
        .writable()
        .get_setting(me_core::settings_key::AI_MODELS)
        .expect("读 meta")
        .expect("必须有档案");
    let models: Vec<me_core::ai::AiModel> =
        serde_json::from_str(&raw).expect("meta 里的档案必须能解析");
    let saved = models
        .iter()
        .find(|model| model.id == "m3")
        .expect("m3 应当落库");

    // 一组参数整存整取，key 与端点、预算一视同仁。
    assert_eq!(saved.api_key, "sk-workshop-123");
    assert_eq!(saved.base_url, "http://127.0.0.1:11434/v1");
    assert_eq!(saved.model, "qwen3");
    assert_eq!(saved.max_input_chars, 30_000);
    assert_eq!(saved.max_output_tokens, 1_024);
    assert!(saved.has_key());
}

/// 「重新生成」只能重放没有副作用的工具：`calendar_create` 必须不在本轮清单里。
#[tokio::test]
async fn regenerate_offers_only_replay_safe_tools() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::text("第一次回答"),
        Turn::text("第二次回答"),
    ]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "你好".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    commands
        .send(Command::RegenerateAiMessage { conversation })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let names = provider.tool_names.lock().unwrap().clone();
    // 三轮请求：PLAN（2 个）→ 执行（全部）→ 重新生成（只剩可重放的）。
    assert_eq!(names.len(), 3, "PLAN + 执行 + 重新生成：{names:?}");
    let execute = &names[1];
    assert!(
        execute.iter().any(|name| name == "calendar_create"),
        "执行阶段要带全部工具：{execute:?}"
    );
    // 五个有副作用的工具都不可重放：重新生成不能动日历，也不能真的切歌。
    for write_tool in [
        "calendar_create",
        "calendar_update",
        "calendar_delete",
        "music_play",
        "music_control",
    ] {
        assert!(
            execute.iter().any(|name| name == write_tool),
            "执行阶段要带 {write_tool}：{execute:?}"
        );
        assert!(
            !names[2].iter().any(|name| name == write_tool),
            "重新生成不得下发 {write_tool}：{:?}",
            names[2]
        );
    }
    assert!(
        names[2]
            .iter()
            .all(|name| name == "plan_submit" || name == "answer_directly"),
        "重新生成从 PLAN 阶段重来，只该给两个引擎工具：{:?}",
        names[2]
    );
}

/// 模型返回一个「本轮已关闭」的工具名时，执行期必须拒绝，且不产生副作用。
#[tokio::test]
async fn disabled_tool_is_refused_at_execution_time() {
    use me_core::ai::{AiModel, AiToolPermissions, AiToolStatus};
    use me_core::port::CalendarRepo;

    let (_dir, store) = open();
    let call = AiToolCall {
        id: "call_cal".to_string(),
        name: AiToolName::CalendarCreate,
        arguments: json!({
            "title": "不应创建",
            "start": "2026-11-06T15:00:00+08:00",
            "end": "2026-11-06T16:00:00+08:00"
        }),
        parse_ok: true,
    };
    let provider = MockAiProvider::new(vec![
        // 计划里不能出现被关掉的工具，否则在计划期就被拒了——那测的就不是执行期拦截。
        Turn::with_tool(plan_submit_for(&[AiToolName::CalendarList])),
        Turn::with_tool(call),
        Turn::text("这个工具被关掉了，没法创建。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);

    // 关掉日程工具后再建会话，让这一轮请求里没有 calendar_create。
    let mut profile = AiModel::preset(
        AiProviderKind::Anthropic,
        "m1".to_string(),
        "测试模型".to_string(),
    );
    profile.api_key = "sk-test".to_string();
    profile.tools = AiToolPermissions {
        calendar_create: false,
        ..AiToolPermissions::default()
    };
    commands
        .send(Command::SaveAiModel { model: profile })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConfigChanged { .. })
    })
    .await;

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "明天下午三点开会".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let messages = store
        .writable()
        .list_messages(conversation, None, 10)
        .expect("list");
    let assistant = messages
        .iter()
        .rev()
        .find(|message| message.role == me_core::ai::AiRole::Assistant)
        .expect("有回复");
    let trace = assistant
        .tool_trace
        .iter()
        .find(|trace| trace.tool == AiToolName::CalendarCreate)
        .expect("应当留下一次被拒绝的工具调用");
    assert_eq!(trace.status, AiToolStatus::Error);
    assert!(
        trace.summary.contains("已关闭"),
        "摘要要说清原因：{}",
        trace.summary
    );

    let calendars: Vec<me_core::CalendarId> = store
        .writable()
        .list_calendars()
        .expect("list calendars")
        .into_iter()
        .map(|calendar| calendar.id)
        .collect();
    let series = store
        .writable()
        .list_series_in_range(&calendars, 0, i64::MAX, 10)
        .expect("list events");
    assert!(series.is_empty(), "被关闭的工具不得创建任何日程");
}

/// 档案 JSON 坏了不能把用户配置静默清空：不建默认档案、不覆盖写、原值保留。
///
/// 这条守的是权限模型的地基：一旦解析失败被兜底成默认值，用户关掉的工具会被重新打开，
/// 而且自己填的端点与 key 会凭空消失。
#[tokio::test]
async fn corrupt_model_json_keeps_original_profiles() {
    let (_dir, store) = open();
    let corrupt = r#"[{"id":"m1","name":"我的模型","kind":"anthropic""#;
    store
        .writable()
        .set_setting(me_core::settings_key::AI_MODELS, corrupt)
        .expect("seed");

    let (_commands, mut rx) = start_service_bare(&store);

    let config = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConfigChanged { .. })
    })
    .await;
    if let Event::AiConfigChanged { models, .. } = config {
        assert!(models.is_empty(), "解析失败不该凭空造出默认档案");
    } else {
        panic!("第一件事应当是配置快照");
    }

    let error = wait_for(&mut rx, |event| matches!(event, Event::Error { .. })).await;
    if let Event::Error { message, .. } = error {
        assert!(message.contains("读取失败"), "要说清是读取失败：{message}");
    } else {
        panic!("解析失败必须有一条错误说明");
    }

    let raw = store
        .writable()
        .get_setting(me_core::settings_key::AI_MODELS)
        .expect("read");
    assert_eq!(raw.as_deref(), Some(corrupt), "原值不能被覆盖");
}

/// 只给时间窗（不给关键词/发件人/主题/收件人）也必须能检索。
///
/// 这是「有什么重要的邮件」那一轮的根因之一：模型写 `unread_only` + `after`
/// 完全符合 schema，实现却只认四个文本条件，于是它换着说法撞了 6 次。
#[tokio::test]
async fn email_search_accepts_time_window_without_keyword() {
    let (_dir, store) = open();
    let call = tool_call(
        "c1",
        AiToolName::EmailSearch,
        json!({
            "motive": "用户问最近有什么邮件，按时间窗检索",
            "days": 7,
            "limit": 5
        }),
    );
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::with_tool(call),
        Turn::text("近 7 天没有新的邮件。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider, true);

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "最近有什么邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");

    let completed = wait_for(&mut rx, |event| {
        matches!(event, Event::AiToolCompleted { .. })
    })
    .await;
    let Event::AiToolCompleted {
        status, summary, ..
    } = completed
    else {
        panic!("期望工具完成事件");
    };
    assert_eq!(
        status,
        me_core::ai::AiToolStatus::Ok,
        "时间窗是合法检索条件，实际：{summary}"
    );
    assert!(
        !summary.contains("至少需要一个检索条件"),
        "不应再出现旧报错文案，实际：{summary}"
    );
}

/// 一个条件都不给时才报错，且文案要把**全部**可用条件列出来。
///
/// 只说「至少需要一个检索条件」等于没说：模型不知道该补什么，只能猜。
#[tokio::test]
async fn email_search_without_any_condition_lists_them_all() {
    let (_dir, store) = open();
    let call = tool_call(
        "c1",
        AiToolName::EmailSearch,
        json!({
            "motive": "用户问邮件，但没说要查什么",
            "limit": 5
        }),
    );
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::with_tool(call),
        Turn::text("没给检索条件，我不该硬查。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider, true);

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "看看邮箱".to_string(),
            context: Vec::new(),
        })
        .expect("send");

    let completed = wait_for(&mut rx, |event| {
        matches!(event, Event::AiToolCompleted { .. })
    })
    .await;
    let Event::AiToolCompleted {
        status, summary, ..
    } = completed
    else {
        panic!("期望工具完成事件");
    };
    assert_eq!(status, me_core::ai::AiToolStatus::Error);
    for expected in ["flagged_only", "days", "unread_only", "folder", "query"] {
        assert!(
            summary.contains(expected),
            "报错文案必须列出可用条件 `{expected}`，实际：{summary}"
        );
    }
}

/// 同参数 + 校验类错误，第二次起不再真正执行。
///
/// 那次事故的形态：模型连发 8 次 `email_search`，6 次拿到同一个「至少需要一个检索条件」。
/// 关键点是**只改 `motive` 不算改参数**——指纹剔除了 `motive` 与 `step_id`。
#[tokio::test]
async fn same_args_after_validation_error_is_not_retried() {
    let (_dir, store) = open();
    // 三个调用参数完全一致，只有 id 与 motive 不同（模拟「换个说法再来一次」）。
    let call = |id: &str, motive: &str| {
        tool_call(
            id,
            AiToolName::EmailSearch,
            json!({ "motive": motive, "limit": 5 }),
        )
    };
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::with_tool(call("c1", "查重要邮件")),
        Turn::with_tool(call("c2", "查看重要邮件，未读优先")),
        Turn::with_tool(call("c3", "再查一次")),
        Turn::text("我没查到，需要你给个更具体的条件。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider, true);

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "有什么重要的邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");

    let mut completed = Vec::new();
    for _ in 0..3 {
        let event = wait_for(&mut rx, |event| {
            matches!(event, Event::AiToolCompleted { .. })
        })
        .await;
        let Event::AiToolCompleted {
            summary, status, ..
        } = event
        else {
            panic!("期望工具完成事件");
        };
        completed.push((summary, status));
    }

    assert_eq!(completed.len(), 3);
    assert!(
        completed[0].0.contains("至少需要一个检索条件"),
        "第一次应当真的执行并拿到校验错误，实际：{}",
        completed[0].0
    );
    for (index, (summary, status)) in completed.iter().enumerate().skip(1) {
        assert_eq!(
            *status,
            me_core::ai::AiToolStatus::Error,
            "被拦下的调用仍然要记成 error"
        );
        assert_eq!(
            summary,
            "相同参数已失败过，未执行",
            "第 {} 次应当被熔断拦下，实际：{summary}",
            index + 1
        );
    }
    // 三次里只有第一次真的进了工具实现，其余两次是纯内存拦截。
    let executed = completed
        .iter()
        .filter(|(summary, _)| summary != "相同参数已失败过，未执行")
        .count();
    assert_eq!(executed, 1, "同参数只应真正执行一次");
}

/// 参数**真的变了**就必须放行：熔断不能把「换个条件再试」也一起挡掉。
#[tokio::test]
async fn changed_args_are_not_blocked_by_the_circuit_breaker() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        // 第一次无条件 → 失败并被记入熔断表。
        Turn::with_tool(tool_call(
            "c1",
            AiToolName::EmailSearch,
            json!({ "motive": "先查一次", "limit": 5 }),
        )),
        // 换了参数（时间窗）→ 必须放行，锁表只认「完全相同的参数」。
        Turn::with_tool(tool_call(
            "c2",
            AiToolName::EmailSearch,
            json!({ "motive": "换时间窗再查", "days": 30 }),
        )),
        Turn::text("两次都查完了。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider, true);

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "找报价单".to_string(),
            context: Vec::new(),
        })
        .expect("send");

    let mut summaries = Vec::new();
    for _ in 0..2 {
        let event = wait_for(&mut rx, |event| {
            matches!(event, Event::AiToolCompleted { .. })
        })
        .await;
        let Event::AiToolCompleted { summary, .. } = event else {
            panic!("期望工具完成事件");
        };
        summaries.push(summary);
    }
    assert!(
        summaries[0].contains("至少需要一个检索条件"),
        "第一次应当失败并进入熔断表，实际：{}",
        summaries[0]
    );
    assert_ne!(
        summaries[1], "相同参数已失败过，未执行",
        "参数变了就必须放行，实际：{}",
        summaries[1]
    );
    assert!(
        summaries[1].contains("0 命中"),
        "第二次是真的执行了（时间窗合法、库里没有邮件），实际：{}",
        summaries[1]
    );
}

/// 调用过工具却整轮没写正文：先催一次，仍然空白就用执行回执兜底，绝不留白。
///
/// 那次事故的落库结果是 `content` 为 13 个换行、`status` 却是 `complete`——
/// 用户看到一块空白，而且没有任何可排查的痕迹。
#[tokio::test]
async fn blank_answer_falls_back_to_execution_summary() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::with_tool(tool_call(
            "c1",
            AiToolName::EmailSearch,
            json!({ "motive": "按时间窗查最近邮件", "days": 7 }),
        )),
        // 第一次空白：引擎应当催它一次。
        Turn::text("\n\n\n"),
        // 第二次仍空白：走兜底，不再催（再催就是逼它编）。
        Turn::text("\n\n\n"),
    ]);
    let (commands, mut rx) = start_service(&store, provider, true);

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "有什么重要的邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");

    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let message = last_assistant_message(&store, conversation);
    assert!(
        message.content.contains("这次没有拿到可用的结果"),
        "兜底摘要必须以「没拿到结果」开头，实际：{}",
        message.content
    );
    assert!(
        message.content.contains("检索近 7 天"),
        "兜底摘要要列出清单里真正执行过的步骤，实际：{}",
        message.content
    );
    assert!(
        message
            .error
            .as_deref()
            .is_some_and(|text| text.contains("模型未输出正文")),
        "要留下可排查的痕迹，实际：{:?}",
        message.error
    );
}

/// 正常写出正文时**不能**被兜底覆盖。
#[tokio::test]
async fn normal_answer_is_not_replaced_by_fallback() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::with_tool(tool_call(
            "c1",
            AiToolName::EmailSearch,
            json!({ "motive": "按时间窗查最近邮件", "days": 7 }),
        )),
        Turn::text("最近 7 天没有新邮件，收件箱是空的。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider, true);

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "最近有邮件吗".to_string(),
            context: Vec::new(),
        })
        .expect("send");

    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let message = last_assistant_message(&store, conversation);
    assert!(
        message.content.contains("最近 7 天没有新邮件"),
        "正常回答必须原样保留，实际：{}",
        message.content
    );
    assert!(
        message.error.is_none(),
        "正常回答不该有 error，实际：{:?}",
        message.error
    );
}

/// 动机要随事件一起发出来：界面在工具**执行中**就要能显示「为什么调它」。
#[tokio::test]
async fn tool_events_carry_motive() {
    use me_core::ai::MotiveSource;

    let (_dir, store) = open();
    let call = AiToolCall {
        id: "c1".to_string(),
        name: AiToolName::EmailSearch,
        arguments: json!({
            "motive": "用户要本周报价单，执行第 1 步检索",
            "query": "报价单"
        }),
        parse_ok: true,
    };
    let provider = MockAiProvider::new(vec![Turn::with_tool(call), Turn::text("找到了。")]);
    let (commands, mut rx) = start_service(&store, provider, true);

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "把本周报价单找出来".to_string(),
            context: Vec::new(),
        })
        .expect("send");

    let started = wait_for(&mut rx, |event| {
        matches!(event, Event::AiToolStarted { .. })
    })
    .await;
    if let Event::AiToolStarted {
        motive,
        motive_source,
        ..
    } = started
    {
        assert_eq!(
            motive.as_deref(),
            Some("用户要本周报价单，执行第 1 步检索"),
            "开始事件就要带动机"
        );
        assert_eq!(motive_source, MotiveSource::Model);
    } else {
        panic!("期望工具开始事件");
    }

    let completed = wait_for(&mut rx, |event| {
        matches!(event, Event::AiToolCompleted { .. })
    })
    .await;
    if let Event::AiToolCompleted {
        motive,
        motive_source,
        ..
    } = completed
    {
        assert_eq!(
            motive.as_deref(),
            Some("用户要本周报价单，执行第 1 步检索"),
            "完成事件与开始事件必须同源"
        );
        assert_eq!(motive_source, MotiveSource::Model);
    } else {
        panic!("期望工具完成事件");
    }
}

/// 模型漏写动机时：照常执行，只把来源标成 default（不额外消耗一次调用预算）。
#[tokio::test]
async fn missing_motive_still_executes_once() {
    use me_core::ai::{AiMessageStatus, AiRole, MotiveSource};

    let (_dir, store) = open();
    let call = AiToolCall {
        id: "c1".to_string(),
        name: AiToolName::EmailSearch,
        arguments: json!({ "query": "报价单" }),
        parse_ok: true,
    };
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::with_tool(call),
        Turn::text("查了，没有结果。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider, true);

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "有报价单吗".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let messages = store
        .writable()
        .list_messages(conversation, None, 10)
        .expect("list");
    let assistant = messages
        .iter()
        .rev()
        .find(|message| message.role == AiRole::Assistant)
        .expect("有回复");
    assert_eq!(assistant.status, AiMessageStatus::Complete);
    assert_eq!(
        assistant.tool_trace.len(),
        1,
        "只该执行一次，不能为补动机重试"
    );
    let trace = &assistant.tool_trace[0];
    assert_eq!(trace.status, me_core::ai::AiToolStatus::Ok);
    assert!(trace.motive.is_none(), "没写就是没写，不要编一段");
    assert_eq!(trace.motive_source, MotiveSource::Default);
}

/// 清单要能驱动循环：建清单 → 干活 → 收尾，最终清单完整落库。
#[tokio::test]
async fn plan_is_built_updated_and_persisted() {
    use me_core::ai::AiMessageStatus;
    use me_core::ai_plan::PlanStatus;

    let (_dir, store) = open();
    let create_plan = AiToolCall {
        id: "c1".to_string(),
        name: AiToolName::PlanUpdate,
        arguments: json!({
            "motive": "用户要报价单并确认周五会议，先拆成两步",
            "goal": "找出本周报价单并确认周五会议",
            "steps": [
                {"id": 1, "capability": "mail", "title": "检索本周报价单", "status": "pending"},
                {"id": 2, "capability": "calendar", "title": "查周五是否已有会议", "status": "pending"}
            ]
        }),
        parse_ok: true,
    };
    let search = AiToolCall {
        id: "c2".to_string(),
        name: AiToolName::EmailSearch,
        arguments: json!({
            "motive": "执行第 1 步：检索报价单",
            "query": "报价单"
        }),
        parse_ok: true,
    };
    // 只回传改动的步骤：第 2 步没提，必须保留。
    let finish_steps = AiToolCall {
        id: "c3".to_string(),
        name: AiToolName::PlanUpdate,
        arguments: json!({
            "motive": "两步都做完了，收尾",
            "steps": [
                {"id": 1, "capability": "mail", "title": "检索本周报价单", "status": "done",
                 "evidence": "本地缓存里没有命中"},
                {"id": 2, "capability": "calendar", "title": "查周五是否已有会议", "status": "done",
                 "evidence": "周五 15:00 有一条「周会」"}
            ]
        }),
        parse_ok: true,
    };
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(create_plan),
        Turn::with_tool(search),
        Turn::with_tool(finish_steps),
        Turn::text("都查完了。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider, true);

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "帮我找出本周的报价单，再看看周五有没有会".to_string(),
            context: Vec::new(),
        })
        .expect("send");

    // 清单在**流式期间**就要推给界面，不能等落库。
    let first = wait_for(&mut rx, |event| {
        matches!(event, Event::AiPlanUpdated { .. })
    })
    .await;
    if let Event::AiPlanUpdated { plan, .. } = first {
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.open_steps(), 2);
        assert_eq!(plan.goal, "找出本周报价单并确认周五会议");
    } else {
        panic!("期望清单更新事件");
    }

    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let messages = store
        .writable()
        .list_messages(conversation, None, 10)
        .expect("list");
    let assistant = messages
        .iter()
        .rev()
        .find(|message| message.role == me_core::ai::AiRole::Assistant)
        .expect("有回复");
    assert_eq!(assistant.status, AiMessageStatus::Complete);
    let plan = assistant.plan.as_ref().expect("清单必须落库");
    assert_eq!(plan.steps.len(), 2);
    assert_eq!(plan.steps[0].status, PlanStatus::Done);
    assert_eq!(
        plan.steps[0].evidence.as_deref(),
        Some("本地缓存里没有命中")
    );
    assert_eq!(plan.steps[1].status, PlanStatus::Done, "第 2 步也要收尾");
    assert_eq!(plan.open_steps(), 0);
}

/// 清单还开着但模型停了：引擎要补一句提醒把它拉回正轨，而不是就地收尾。
#[tokio::test]
async fn open_plan_gets_a_nudge_instead_of_ending() {
    let (_dir, store) = open();
    let create_plan = AiToolCall {
        id: "c1".to_string(),
        name: AiToolName::PlanUpdate,
        arguments: json!({
            "motive": "拆成两步",
            "goal": "查邮件再查日程",
            "steps": [
                {"id": 1, "capability": "mail", "title": "检索邮件", "status": "running"},
                {"id": 2, "capability": "calendar", "title": "查日程", "status": "pending"}
            ]
        }),
        parse_ok: true,
    };
    let close_plan = AiToolCall {
        id: "c2".to_string(),
        name: AiToolName::PlanUpdate,
        arguments: json!({
            "motive": "两步都做完了",
            "steps": [
                {"id": 1, "capability": "mail", "title": "检索邮件", "status": "done"},
                {"id": 2, "capability": "calendar", "title": "查日程", "status": "skipped",
                 "evidence": "用户没说是哪一天"}
            ]
        }),
        parse_ok: true,
    };
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(create_plan),
        // 模型以为说完计划就完事了：这一轮没有任何工具调用。
        Turn::text("好的，我先看看。"),
        Turn::with_tool(close_plan),
        Turn::text("查完了。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "查一下邮件和日程".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let counts = provider.message_counts.lock().unwrap().clone();
    assert!(counts.len() >= 3, "应当有第三轮：被提醒后继续");
    assert!(
        counts[2] > counts[1],
        "提醒要作为一条消息追加进请求：{:?}",
        counts
    );

    let messages = store
        .writable()
        .list_messages(conversation, None, 10)
        .expect("list");
    let assistant = messages
        .iter()
        .rev()
        .find(|message| message.role == me_core::ai::AiRole::Assistant)
        .expect("有回复");
    let plan = assistant.plan.as_ref().expect("清单必须落库");
    assert_eq!(plan.open_steps(), 0, "催过一次之后清单要收尾");
}

/// 取消时只把「进行中」的步标跳过，还没开始的步保持 pending。
#[tokio::test]
async fn cancelling_skips_only_the_running_step() {
    use me_core::ai::AiMessageStatus;
    use me_core::ai_plan::PlanStatus;

    let (_dir, store) = open();
    let create_plan = AiToolCall {
        id: "c1".to_string(),
        name: AiToolName::PlanUpdate,
        arguments: json!({
            "motive": "拆成两步",
            "goal": "先查邮件再查日程",
            "steps": [
                {"id": 1, "capability": "mail", "title": "检索邮件", "status": "running"},
                {"id": 2, "capability": "calendar", "title": "查日程", "status": "pending"}
            ]
        }),
        parse_ok: true,
    };
    let mut hold = Turn::text("");
    hold.hold = true;
    let provider = MockAiProvider::new(vec![Turn::with_tool(create_plan), hold]);
    let (commands, mut rx) = start_service(&store, provider, true);

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "查邮件和日程".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    // 第二轮卡住时点「停止」。
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiPlanUpdated { .. })
    })
    .await;
    commands
        .send(Command::CancelAiStream { conversation })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let messages = store
        .writable()
        .list_messages(conversation, None, 10)
        .expect("list");
    let assistant = messages
        .iter()
        .rev()
        .find(|message| message.role == me_core::ai::AiRole::Assistant)
        .expect("有回复");
    assert_eq!(assistant.status, AiMessageStatus::Cancelled);
    let plan = assistant.plan.as_ref().expect("取消也要把清单存下来");
    assert_eq!(
        plan.steps[0].status,
        PlanStatus::Skipped,
        "进行中的步要跳过"
    );
    assert_eq!(
        plan.steps[1].status,
        PlanStatus::Pending,
        "没开始的步保持待执行，用户可能接着说继续"
    );
}

/// `calendar_delete` 只发确认请求：库里那条日程必须**还在**。
///
/// 这条测试守的是最容易被「顺手实现」破坏的语义——工具层直接调
/// `delete_event` 会让模型一句话删掉用户的日程，而用户根本没看见。
#[tokio::test]
async fn calendar_delete_requests_confirmation_instead_of_deleting() {
    use me_core::calendar::{CalendarEventDraft, CalendarSource};
    use me_core::port::CalendarRepo;

    let (_dir, store) = open();
    let calendar = store
        .writable()
        .ensure_default_calendar()
        .expect("默认日历");
    let event = store
        .writable()
        .create_event(
            &CalendarEventDraft {
                calendar_id: calendar.id,
                summary: "周五复盘".to_string(),
                description: None,
                location: None,
                start_utc: 1_800_000_000,
                end_utc: 1_800_003_600,
                all_day: false,
                timezone: None,
                rrule: None,
                exdates: Vec::new(),
                reminders: Vec::new(),
            },
            None,
            CalendarSource::Ai,
        )
        .expect("建日程");

    let call = AiToolCall {
        id: "call_del".to_string(),
        name: AiToolName::CalendarDelete,
        arguments: json!({ "motive": "用户要删掉周五复盘", "event_id": event.id.0 }),
        parse_ok: true,
    };
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::CalendarDelete])),
        Turn::with_tool(call),
        Turn::text("已经请你确认删除，确认前这条还在。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider, true);

    commands
        .send(Command::CreateAiConversation { model: None })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiConversationsChanged)
    })
    .await;
    let conversation = latest_conversation(&store);

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "删掉周五的复盘".to_string(),
            context: Vec::new(),
        })
        .expect("send");

    let pending = wait_for(&mut rx, |event| {
        matches!(event, Event::AiToolCalendarDeletePending { .. })
    })
    .await;
    match pending {
        Event::AiToolCalendarDeletePending {
            event: pending_id,
            title,
            ..
        } => {
            assert_eq!(pending_id, event.id, "确认卡片要指向被删的那一条");
            assert_eq!(title, "周五复盘");
        }
        other => panic!("应当发出待确认事件，实际收到 {other:?}"),
    }

    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    assert!(
        store.writable().get_event(event.id).expect("查").is_some(),
        "用户没确认之前，日程不能被删掉"
    );
}

// ---------------------------------------------------------------------------
// PLAN 阶段：强制清单
// ---------------------------------------------------------------------------

/// 首轮只下发两个工具：模型物理上没有 `email_search` 可调。
#[tokio::test]
async fn plan_phase_only_offers_the_two_planning_tools() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::text("查完了。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "有什么重要的邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let names = provider.tool_names.lock().unwrap().clone();
    assert_eq!(
        names[0],
        vec!["plan_submit".to_string(), "answer_directly".to_string()],
        "首轮不能给模型任何业务工具：{:?}",
        names[0]
    );
    assert!(
        !names[1].iter().any(|name| name == "plan_submit"),
        "执行阶段不能再下发 plan_submit，否则重列上限形同虚设：{:?}",
        names[1]
    );
    assert!(
        names[1].iter().any(|name| name == "email_search"),
        "执行阶段要能查邮件：{:?}",
        names[1]
    );
}

/// 计划阶段凭历史记忆去调业务工具：拒绝执行，且**不消耗**工具预算。
#[tokio::test]
async fn business_tool_is_refused_during_the_plan_phase() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        // 第 1 轮就想直接查——旧行为正是这样，一分钟发了 8 次。
        Turn::with_tool(tool_call(
            "c1",
            AiToolName::EmailSearch,
            json!({ "motive": "先查一下再说", "days": 7 }),
        )),
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::text("查完了。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "有什么重要的邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");

    let refused = wait_for(&mut rx, |event| {
        matches!(event, Event::AiToolCompleted { .. })
    })
    .await;
    let Event::AiToolCompleted { summary, .. } = refused else {
        panic!("期望工具完成事件");
    };
    assert_eq!(summary, "计划阶段不能执行工具");

    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let message = last_assistant_message(&store, conversation);
    let plan = message.plan.expect("最终应当有一份计划");
    assert_eq!(plan.version, 2);
    assert_eq!(plan.steps.len(), 1);
}

/// 模型两轮都不交计划：引擎合成单步清单收场，绝不留白。
#[tokio::test]
async fn model_that_never_submits_gets_a_synthetic_plan() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        // 空头承诺：引擎会追一句「请先提交计划」。
        Turn::text("我先看看你的邮件。"),
        // 还是不给计划：轮到引擎收场。
        Turn::text("我看了一下，没找到重要邮件。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "有什么重要的邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let message = last_assistant_message(&store, conversation);
    assert!(
        message.content.contains("没找到重要邮件"),
        "模型的话要保留：{}",
        message.content
    );
    assert!(
        message
            .error
            .as_deref()
            .is_some_and(|text| text.contains("未提交计划")),
        "要留下可排查的痕迹：{:?}",
        message.error
    );
    let plan = message.plan.expect("引擎兜底要合成一份单步清单");
    assert_eq!(plan.steps.len(), 1, "兜底清单只有一步");
    assert_eq!(plan.steps[0].title, "直接回答");
}

/// 显式豁免：`answer_directly` 之后不再给工具，也不硬凑一份计划。
#[tokio::test]
async fn answer_directly_skips_the_plan() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(tool_call(
            "a1",
            AiToolName::AnswerDirectly,
            json!({ "motive": "用户在问怎么设置签名", "reason": "纯知识问答，不需要查数据" }),
        )),
        Turn::text("在设置里改签名即可。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "怎么改签名".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let names = provider.tool_names.lock().unwrap().clone();
    assert!(
        names[1].is_empty(),
        "豁免后不该再给任何工具：{:?}",
        names[1]
    );

    let message = last_assistant_message(&store, conversation);
    assert!(message.plan.is_none(), "豁免不该凭空造一份清单");
    assert!(message.content.contains("签名"));
    assert!(message.error.is_none());
}

/// 工具调用要绑到清单里的步：状态与次数由引擎写，不靠模型自觉。
#[tokio::test]
async fn tool_calls_bind_to_plan_steps_and_record_attempts() {
    let (_dir, store) = open();
    let plan = tool_call(
        "plan",
        AiToolName::PlanSubmit,
        json!({
            "motive": "分两步查",
            "goal": "找出值得优先处理的邮件",
            "steps": [
                {"id": 1, "title": "检索星标邮件", "kind": "tool", "capability": "mail",
                 "tool": "email_search", "args": {"flagged_only": true}, "expect": "星标邮件列表"},
                {"id": 2, "title": "检索近 7 天未读", "kind": "tool", "capability": "mail",
                 "tool": "email_search", "args": {"unread_only": true, "days": 7},
                 "expect": "未读邮件列表"}
            ]
        }),
    );
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan),
        // 显式指定第 2 步：引擎要把这一次的结果记到第 2 步上。
        Turn::with_tool(tool_call(
            "c2",
            AiToolName::EmailSearch,
            json!({ "motive": "执行第 2 步", "step_id": 2, "unread_only": true, "days": 7 }),
        )),
        Turn::text("近 7 天有 2 封未读。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "有什么重要的邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let message = last_assistant_message(&store, conversation);
    let plan = message.plan.expect("有清单");
    assert_eq!(plan.steps[1].status, me_core::ai_plan::PlanStatus::Done);
    assert_eq!(plan.steps[1].attempts, 1, "一次执行记一次尝试");
    assert!(plan.steps[1].evidence.is_some(), "执行回执由引擎写");
    assert_eq!(
        plan.steps[0].status,
        me_core::ai_plan::PlanStatus::Pending,
        "没被执行的步保持待执行，不能替它圆"
    );

    let trace = message
        .tool_trace
        .iter()
        .find(|trace| trace.call_id == "c2")
        .expect("第 2 步的 trace");
    assert_eq!(trace.step_id, Some(2));
    assert_eq!(trace.attempt, Some(1));
}

/// 计划期就把「参数不对」拦下来，并说清缺什么。
#[tokio::test]
async fn invalid_plan_is_rejected_at_plan_time() {
    let (_dir, store) = open();
    let bad = tool_call(
        "bad",
        AiToolName::PlanSubmit,
        json!({
            "motive": "先给计划",
            "goal": "查重要邮件",
            "steps": [{
                "id": 1, "title": "检索邮件", "kind": "tool", "capability": "mail",
                "tool": "email_search",
                // 只给 limit：旧实现在执行期才报「至少需要一个检索条件」，
                // 模型看不出缺什么，于是原样重发 6 次。
                "args": { "limit": 5 },
                "expect": "列出邮件"
            }]
        }),
    );
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(bad),
        // 改对之后再交一次。
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::text("查完了。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "有什么重要的邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let message = last_assistant_message(&store, conversation);
    let plan = message.plan.expect("改对之后应当有清单");
    assert_eq!(
        plan.steps[0].args,
        Some(json!({ "days": 7 })),
        "落库的是**改对之后**的那一份"
    );
}

// ---------------------------------------------------------------------------
// REFLECT / REPLAN
// ---------------------------------------------------------------------------

/// 自检用的 JSON。
fn selfcheck(verdict: &str, diagnosis: &str, hint: &str) -> Turn {
    Turn::text(&format!(
        "{{\"verdict\":\"{verdict}\",\"failed_step\":1,\"diagnosis\":\"{diagnosis}\",\
\"reason\":\"检索条件一直没给对，三次都报同一个错\",\"replan_hint\":\"{hint}\"}}"
    ))
}

/// 一步连续失败三次 → 自检 → 重列 → 换参数重来。
#[tokio::test]
async fn failed_step_triggers_reflect_then_replan() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        // 三次都只给 limit（没有检索条件），参数各不相同所以不会被指纹熔断。
        Turn::with_tool(tool_call(
            "c1",
            AiToolName::EmailSearch,
            json!({ "motive": "查重要邮件", "limit": 5 }),
        )),
        Turn::with_tool(tool_call(
            "c2",
            AiToolName::EmailSearch,
            json!({ "motive": "换个数量再查", "limit": 6 }),
        )),
        Turn::with_tool(tool_call(
            "c3",
            AiToolName::EmailSearch,
            json!({ "motive": "再试一次", "limit": 7 }),
        )),
        // 自检：模型承认没做成，并给出改法。
        selfcheck("failed", "wrong_args", "改成按最近 7 天未读查"),
        // 重列：换成有时间窗的参数。
        Turn::with_tool(tool_call(
            "plan2",
            AiToolName::PlanSubmit,
            json!({
                "motive": "换一个思路",
                "goal": "完成用户这一次的请求",
                "steps": [{
                    "id": 1, "title": "检索近 7 天未读", "kind": "tool", "capability": "mail",
                    "tool": "email_search", "args": { "unread_only": true, "days": 7 },
                    "expect": "列出未读邮件"
                }]
            }),
        )),
        Turn::with_tool(tool_call(
            "c4",
            AiToolName::EmailSearch,
            json!({ "motive": "执行第 1 步", "unread_only": true, "days": 7 }),
        )),
        Turn::text("最近 7 天没有未读邮件。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "有什么重要的邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let message = last_assistant_message(&store, conversation);
    assert!(
        message.content.contains("没有未读"),
        "最终答复要保留：{}",
        message.content
    );
    let plan = message.plan.expect("要有清单");
    assert_eq!(plan.replans.len(), 1, "重列一次：{:?}", plan.replans);
    assert_eq!(
        plan.replans[0].diagnosis,
        me_core::ai_plan::Diagnosis::WrongArgs
    );
    assert_eq!(plan.history.len(), 1, "旧计划要进 history");
    assert_eq!(
        plan.steps[0].args,
        Some(json!({ "unread_only": true, "days": 7 })),
        "落库的是重列之后的计划"
    );
}

/// 重列如果原样重试失败过的调用，直接被砍掉，不再真跑一遍。
#[tokio::test]
async fn replan_repeating_a_failed_call_is_dropped() {
    let (_dir, store) = open();
    let read = |id: &str, message_id: u64| {
        Turn::with_tool(tool_call(
            id,
            AiToolName::MailRead,
            json!({ "motive": "读正文", "message_id": message_id }),
        ))
    };
    let plan_read = |id: &str, message_id: u64| {
        Turn::with_tool(tool_call(
            id,
            AiToolName::PlanSubmit,
            json!({
                "motive": "读三封邮件",
                "goal": "读出三封邮件的要点",
                "steps": [{
                    "id": 1, "title": "读邮件正文", "kind": "tool", "capability": "mail",
                    "tool": "mail_read", "args": { "message_id": message_id },
                    "expect": "拿到正文"
                }]
            }),
        ))
    };
    let provider = MockAiProvider::new(vec![
        plan_read("plan1", 9_991),
        // 三次读不到（本地没有这些邮件），参数不同所以都能真正执行。
        read("r1", 9_991),
        read("r2", 9_992),
        read("r3", 9_993),
        selfcheck("failed", "missing_data", "先让用户打开这几封邮件"),
        // 重列：又写回 9992 —— 与历史失败指纹完全相同，必须被剔除。
        plan_read("plan2", 9_992),
        Turn::text("读不到正文，需要你先在邮件列表里打开这几封。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "把这三封邮件的要点读出来".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let message = last_assistant_message(&store, conversation);
    assert!(
        message.content.contains("读不到正文"),
        "最终答复要保留：{}",
        message.content
    );
    let plan = message.plan.expect("旧清单要留着给用户看");
    assert!(
        plan.replans.is_empty(),
        "一步不剩的重列不该记一次成功：{:?}",
        plan.replans
    );
    assert_eq!(plan.steps[0].status, me_core::ai_plan::PlanStatus::Failed);
}
