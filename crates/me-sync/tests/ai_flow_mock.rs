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
    /// 这一轮产出的思考内容（通过 sink 发 `AiDelta::Thinking`）。
    thinking: String,
    tool_calls: Vec<AiToolCall>,
    error: Option<AiErrorKind>,
    /// 返回前等待取消标志（模拟长时间输出）。
    hold: bool,
}

impl Turn {
    fn text(text: &str) -> Self {
        Self {
            text: text.to_string(),
            thinking: String::new(),
            tool_calls: Vec::new(),
            error: None,
            hold: false,
        }
    }

    fn with_tool(call: AiToolCall) -> Self {
        Self {
            text: String::new(),
            thinking: String::new(),
            tool_calls: vec![call],
            error: None,
            hold: false,
        }
    }

    /// 给这一轮加上思考内容。用于验证分段、双通道 seq 与落库。
    fn thinking(mut self, text: &str) -> Self {
        self.thinking = text.to_string();
        self
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
    /// 每轮请求里**用户消息**的文本（用于验证引擎给模型说了什么）。
    message_bodies: Mutex<Vec<String>>,
    /// 每轮请求的**系统提示**（用于验证阶段化的补充真的发出去了）。
    systems: Mutex<Vec<String>>,
    /// 模型**请求调用**的工具序列（跨轮按顺序铺平）。
    ///
    /// 回归语料用它断言「这次到底走了哪条路」：只看 `tool_trace` 会漏掉
    /// 清单类工具与相同时段被拦截的调用，序列才对得上。
    tool_sequence: Mutex<Vec<String>>,
}

impl MockAiProvider {
    fn new(turns: Vec<Turn>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(turns.into()),
            tool_counts: Mutex::new(Vec::new()),
            tool_names: Mutex::new(Vec::new()),
            message_counts: Mutex::new(Vec::new()),
            targets: Mutex::new(Vec::new()),
            message_bodies: Mutex::new(Vec::new()),
            systems: Mutex::new(Vec::new()),
            tool_sequence: Mutex::new(Vec::new()),
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
        // 记下这一轮用户角色的全部文本：引擎给模型的提示都在这里。
        let body = request
            .messages
            .iter()
            .filter_map(|message| match message {
                me_core::ai::AiWireMessage::User { content } => Some(content.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if let Ok(mut bodies) = self.message_bodies.lock() {
            bodies.push(body);
        }
        if let Ok(mut systems) = self.systems.lock() {
            systems.push(request.system.clone());
        }
        let turn = self.script.lock().expect("script").pop_front();
        // 记下模型这一轮**想**调什么：序列断言看的是它，不是引擎最终执行了什么
        // （被阶段拦截、被熔断的调用同样属于「这次的路径」）。
        if let Some(turn) = turn.as_ref()
            && let Ok(mut sequence) = self.tool_sequence.lock()
        {
            for call in &turn.tool_calls {
                sequence.push(call.name.as_str().to_string());
            }
        }
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
            if !turn.thinking.is_empty() {
                sink(AiDelta::Thinking(turn.thinking.clone()));
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

/// 造一个带账户、收件箱与若干邮件的库，用来验证检索路径。
///
/// 走 `me-store` 的公开 port（`create_account` / `upsert_folder` /
/// `insert_messages`）而不是直接写 SQL：测试不该绕过存储层。
/// 同步出来的邮件没有正文，但**头部**
/// （主题、发件人、日期、已读/星标位）齐全，检索用得到。
///
/// 邮件每项是 `(主题, 发件人, 日期, 标志位)`。
fn seed_mail(store: &Store, items: &[(&str, &str, i64, u32)]) -> Vec<me_core::model::MessageId> {
    let writable = store.writable();
    let account = me_core::port::AccountRepo::create_account(
        &writable,
        &me_core::command::AccountDraft {
            email: "seed@example.com".to_string(),
            display_name: None,
            imap: me_core::model::Endpoint {
                host: "imap.example.com".to_string(),
                port: 993,
                tls: me_core::model::TlsMode::Implicit,
            },
            smtp: me_core::model::Endpoint {
                host: "smtp.example.com".to_string(),
                port: 465,
                tls: me_core::model::TlsMode::Implicit,
            },
            auth_user: "seed@example.com".to_string(),
            auth_secret: Some("secret".to_string()),
            auth_method: me_core::model::AuthMethod::Password,
            oauth_provider: None,
            oauth_subject: None,
            plaintext_tls_ack: false,
        },
    )
    .expect("create account");
    let folder = me_core::port::FolderRepo::upsert_folder(
        &writable,
        account.id,
        &me_core::port::FolderInfo {
            name: "INBOX".to_string(),
            delimiter: Some('/'),
            attributes: vec!["\\Inbox".to_string()],
        },
    )
    .expect("upsert inbox");
    let messages: Vec<me_core::model::NewMessage> = items
        .iter()
        .map(|(subject, from, date, flags)| me_core::model::NewMessage {
            uid: None,
            message_id: None,
            subject: Some((*subject).to_string()),
            from: Some(me_core::model::Address::new(None, *from)),
            to: Vec::new(),
            cc: Vec::new(),
            date: Some(*date),
            date_source: me_core::model::DateSource::Header,
            size: None,
            flags: me_core::model::Flags::from_bits(*flags),
            has_attachment: false,
            in_reply_to: None,
            references: Vec::new(),
        })
        .collect();
    // 全限定调用：`AiRepo` 也有 `list_messages`，两个 trait 的方法名撞了。
    me_core::port::MessageRepo::insert_messages(&writable, account.id, folder.id, &messages, 0)
        .expect("insert messages")
        .into_iter()
        .map(|summary| summary.id)
        .collect()
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
    // 模型只说了「记住了。」就收尾，没按规范写 `[结果]` 段落。
    // 规范（系统提示第 9 条）要求「本轮调用过工具时，回复最后必须以 [结果]
    // 段落收尾」，所以引擎按清单与回执补一段四行段落。
    assert!(
        assistant.content.starts_with("记住了。"),
        "模型自己的答复要保留：{}",
        assistant.content
    );
    assert!(
        assistant.content.contains("[结果]\n状态："),
        "引擎要补上 [结果] 段落：{}",
        assistant.content
    );
    assert!(
        assistant.content.contains("做了什么：记录记忆"),
        "[结果] 要说清做了什么：{}",
        assistant.content
    );
    assert_eq!(assistant.tool_trace.len(), 1);
    assert_eq!(assistant.tool_trace[0].tool, AiToolName::MemoryUpsert);
}

#[tokio::test]
async fn cancelling_keeps_partial_content_and_marks_cancelled() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![Turn {
        text: "这段不会出现".to_string(),
        tool_calls: Vec::new(),
        thinking: String::new(),
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
        thinking: String::new(),
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
        // 第二次仍空白：再催一次（`MAX_FINAL_RETRIES = 2`）。
        Turn::text("\n\n\n"),
        // 第三次仍空白：不再催，直接直出工具正文。
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
    // 2026-11-09 起：工具成功返回过正文，兜底就**直出**它，不再是一句
    // 「这次没有拿到可用的结果」——数据在手却交不出去是那次事故的核心。
    assert!(
        message
            .content
            .contains("模型没能整理这次的结果，以下是查询原文"),
        "兜底要直出工具正文，实际：{}",
        message.content
    );
    assert!(
        message.content.contains("检索近 7 天") || message.content.contains("没有找"),
        "直出的正文应当是检索工具真正返回的那段，实际：{}",
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

/// 一句话只写一条日程：同名第二次创建要被拦下。
///
/// 2026-11-10 的实测现场：模型把「下周四」算成 11-13（周五），复核时又算成
/// 11-19（周四），**两条都进了库**——用户既没要求两条，也看不出哪条才是真的。
/// 工具自带的「同标题 ±5 分钟」去重挡不住这种情况：换个日期就绕过去了。
#[tokio::test]
async fn second_create_with_the_same_title_is_refused() {
    use me_core::port::{CalendarRepo, ReadOnlyRepo};

    let (_dir, store) = open();
    let calendar = store
        .writable()
        .ensure_default_calendar()
        .expect("默认日历");

    let make_call = |id: &str, step: u8, start: &str, end: &str| {
        tool_call(
            id,
            AiToolName::CalendarCreate,
            json!({
                "motive": "用户要建下周四下午两点与张总的会",
                "title": "与张总开会",
                "start": start,
                "end": end,
                "step_id": step,
            }),
        )
    };
    let plan = tool_call(
        "plan",
        AiToolName::PlanSubmit,
        json!({
            "motive": "先给出计划再执行",
            "goal": "建下周四下午两点与张总的会",
            "steps": [
                {
                    "id": 1,
                    "title": "创建会议日程",
                    "kind": "tool",
                    "capability": "calendar",
                    "tool": "calendar_create",
                    "args": {
                        "title": "与张总开会",
                        "start": "2026-11-13T14:00:00+08:00",
                        "end": "2026-11-13T15:00:00+08:00"
                    },
                    "expect": "日程创建成功"
                },
                {
                    "id": 2,
                    "title": "再建一次（复核时间）",
                    "kind": "tool",
                    "capability": "calendar",
                    "tool": "calendar_create",
                    "args": {
                        "title": "与张总开会",
                        "start": "2026-11-19T14:00:00+08:00",
                        "end": "2026-11-19T15:00:00+08:00"
                    },
                    "expect": "日程创建成功"
                }
            ]
        }),
    );
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan),
        Turn::with_tool(make_call(
            "call_a",
            1,
            "2026-11-13T14:00:00+08:00",
            "2026-11-13T15:00:00+08:00",
        )),
        Turn::with_tool(make_call(
            "call_b",
            2,
            "2026-11-19T14:00:00+08:00",
            "2026-11-19T15:00:00+08:00",
        )),
        Turn::text("会已经建好了：2026-11-13 14:00 与张总开会。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "下周四下午两点跟张总开会，一小时".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let series = store
        .read_only()
        .list_calendar_series_readonly(&[calendar.id], 0, i64::MAX / 4, 50)
        .expect("列日程");
    let same: Vec<_> = series
        .iter()
        .filter(|item| item.summary == "与张总开会")
        .collect();
    let message = last_assistant_message(&store, conversation);
    assert_eq!(
        same.len(),
        1,
        "同一句话只该留下一条日程，实际 {}: {:?}",
        same.len(),
        same.iter().map(|item| item.start_utc).collect::<Vec<_>>()
    );
    // 第二次被拒的原因要能被模型读懂（它得据此决定「改」还是「放弃」）。
    assert!(
        message
            .tool_trace
            .iter()
            .any(|trace| trace.summary.contains("不再重复创建")),
        "被拦下的那次要留下说明：{:?}",
        message
            .tool_trace
            .iter()
            .map(|trace| trace.summary.clone())
            .collect::<Vec<_>>()
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
    // 库是空的 → 0 命中 → 引擎放宽条件（7 天 → 30 天）再试一次。
    // 放宽那一次也是一次**物理尝试**，所以 attempts 是 2 而不是 1：
    // 「3 次机会」是真实花掉的次数，不是模型发了几次调用。
    assert_eq!(
        plan.steps[1].attempts, 2,
        "放宽重试也算一次尝试：{:?}",
        plan.steps[1]
    );
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
    // attempt 是「这是这一步的第几次尝试」：放宽重试用掉了一次，所以是 2。
    assert_eq!(trace.attempt, Some(2));
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
    // 落库的是**真正跑过**的条件：0 命中后引擎把时间窗从 7 天放宽到 30 天再试，
    // 清单里写的是放宽后的那份（用户看到的是真实执行的条件）。
    assert_eq!(
        plan.steps[0].args,
        Some(json!({ "unread_only": true, "days": 30 })),
        "落库的是放宽之后真正跑过的条件"
    );
    assert!(
        plan.steps[0]
            .evidence
            .as_deref()
            .is_some_and(|text| text.contains("放宽")),
        "回执要说明条件被放宽过：{:?}",
        plan.steps[0].evidence
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

// ---------------------------------------------------------------------------
// 引擎级重试与放宽
// ---------------------------------------------------------------------------

/// 0 命中不是失败：引擎自己放宽条件再试一次，清单里写的是**真正跑过**的条件。
#[tokio::test]
async fn empty_result_widens_the_condition_and_retries() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::with_tool(tool_call(
            "c1",
            AiToolName::EmailSearch,
            json!({ "motive": "查最近 7 天未读", "unread_only": true, "days": 7 }),
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
    let plan = message.plan.expect("有清单");
    let step = &plan.steps[0];
    assert_eq!(
        step.status,
        me_core::ai_plan::PlanStatus::Done,
        "0 命中是合法结果，不是失败：{step:?}"
    );
    assert!(
        step.failure.is_none(),
        "0 命中不该记失败：{:?}",
        step.failure
    );
    // 时间窗从 7 天放宽到 30 天。
    assert_eq!(
        step.args,
        Some(json!({ "unread_only": true, "days": 30 })),
        "清单里写的是真正跑过的条件"
    );
    assert!(
        step.evidence
            .as_deref()
            .is_some_and(|text| text.contains("放宽")),
        "回执要说明条件被放宽过：{:?}",
        step.evidence
    );
    assert_eq!(step.attempts, 2, "放宽那一次也是一次尝试：{step:?}");
}

/// 判据组单独也能检索：它是「一组带名字的条件」，不需要再补一个关键词。
///
/// 这一条守护的是**通用机制**而不是某个具体口径：
/// 模型把定性需求（重要/紧急/该跟进的…）翻译成几条判据，
/// 引擎按判据逐条查、取并集——引擎自己不知道也不需要知道
/// 「重要」等于哪几条。
#[tokio::test]
async fn criteria_group_alone_is_a_valid_search_condition() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::with_tool(tool_call(
            "c1",
            AiToolName::EmailSearch,
            json!({
                "motive": "按判据查邮件",
                "criteria": [
                    {"label": "已星标", "flagged_only": true},
                    {"label": "近 7 天未读", "unread_only": true, "days": 7}
                ]
            }),
        )),
        Turn::text("按这两条判据没找到。"),
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
    // 关键：不该报「至少需要一个检索条件」。
    assert!(
        !message.content.contains("至少需要一个检索条件"),
        "判据组本身就是条件：{}",
        message.content
    );
    let plan = message.plan.expect("有清单");
    assert_eq!(
        plan.steps[0].status,
        me_core::ai_plan::PlanStatus::Done,
        "这一步要正常走完：{:?}",
        plan.steps[0]
    );
}

/// 判据组取**并集**，且每条结果标注命中了哪条判据。
///
/// 这是通用机制的核心行为：
/// - 判据之间是「或」——两条判据各自命中的邮件都要出现；
/// - 靠前的判据排在前面（判据顺序即优先级）；
/// - 每封邮件带「命中判据」的名字，用户据此判断模型的理解对不对。
///
/// 注意这里刻意用**与事故无关**的判据名（「标记过的」「带附件的」），
/// 证明这套机制与具体需求无关。
#[tokio::test]
async fn criteria_group_unions_results_and_labels_each_hit() {
    let (_dir, store) = open();
    let now = me_core::now_unix();
    let ids = seed_mail(
        &store,
        &[
            // 星标（已读）：命中「标记过的」。
            ("标记那封", "a@example.com", now - 3600, 0b100),
            // 未读：命中「未读的」。
            ("未读那封", "b@example.com", now - 7200, 0b000),
            // 已读且没星标：两条判据都不该命中它。
            ("无关那封", "c@example.com", now - 10800, 0b001),
        ],
    );
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::with_tool(tool_call(
            "c1",
            AiToolName::EmailSearch,
            json!({
                "motive": "按两条判据查",
                "criteria": [
                    {"label": "标记过的", "flagged_only": true},
                    {"label": "未读的", "unread_only": true}
                ]
            }),
        )),
        Turn::text("找到两封。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "帮我找出值得优先看的邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    // 邮件卡片事件在执行期就发出来了，先接住它。
    let event = wait_for(&mut rx, |event| {
        matches!(event, Event::AiToolEmailResults { .. })
    })
    .await;
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    // 卡片：两条判据各命中一封，理由就是判据名。
    let Event::AiToolEmailResults { cards, .. } = event else {
        panic!("要邮件卡片事件");
    };
    assert_eq!(cards.len(), 2, "两条判据各命中一封：{cards:?}");
    let reasons: Vec<&str> = cards.iter().map(|card| card.reason.as_str()).collect();
    assert!(
        reasons.contains(&"标记过的"),
        "要标注命中了哪条判据：{reasons:?}"
    );
    assert!(
        reasons.contains(&"未读的"),
        "要标注命中了哪条判据：{reasons:?}"
    );
    // 靠前的判据排前面。
    assert_eq!(cards[0].reason, "标记过的", "判据顺序即优先级：{cards:?}");
    // 两条判据都不命中的那封不该出现。
    assert!(
        !cards.iter().any(|card| card.message == ids[2]),
        "没命中任何判据的邮件不该出现：{cards:?}"
    );
}

/// `must` 框范围、`boost` 定顺序：这是「优先处理」不再退化成时间窗的落点。
///
/// 2026-11-10 实测里「优先处理」的卡片理由全是「近 7 天」——因为当时只有并集，
/// 命中的先后无法表达「先看哪封」。现在：
/// - must（近 30 天）取交集：范围外的邮件即使命中 boost 也不出现；
/// - boost（已星标）只加分：范围内命中的排前面，没命中的仍然出现。
#[tokio::test]
async fn criteria_must_narrows_scope_and_boost_only_orders() {
    let (_dir, store) = open();
    let now = me_core::now_unix();
    let day = 86_400;
    let ids = seed_mail(
        &store,
        &[
            // 范围内 + 已星标：该排第一。
            ("星标且在范围内", "a@example.com", now - 3600, 0b100),
            // 范围内、没星标：该出现，但排在后面。
            ("普通但在范围内", "b@example.com", now - 7200, 0b001),
            // 已星标但在范围外（40 天前）：must 应把它挡在外面。
            ("星标但太久", "c@example.com", now - 40 * day, 0b100),
        ],
    );
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::with_tool(tool_call(
            "c1",
            AiToolName::EmailSearch,
            json!({
                "motive": "先框范围再按优先级排",
                "criteria": [
                    {"label": "近 30 天", "mode": "must", "days": 30},
                    {"label": "已星标", "mode": "boost", "flagged_only": true}
                ]
            }),
        )),
        Turn::text("范围内星标的排前面。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "最近一个月里，优先看星标的邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let event = wait_for(&mut rx, |event| {
        matches!(event, Event::AiToolEmailResults { .. })
    })
    .await;
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let Event::AiToolEmailResults { cards, .. } = event else {
        panic!("要邮件卡片事件");
    };
    assert_eq!(cards.len(), 2, "范围外的那封不能被 boost 拉进来：{cards:?}");
    assert!(
        !cards.iter().any(|card| card.message == ids[2]),
        "must 是硬范围：旧邮件不该出现"
    );
    assert_eq!(
        cards[0].message, ids[0],
        "范围内命中 boost 的排第一：{cards:?}"
    );
    assert_eq!(
        cards[0].reason, "已星标",
        "卡片理由取命中的那条判据：{cards:?}"
    );
    assert_eq!(
        cards[1].message, ids[1],
        "没命中 boost 的仍然出现，只是排后面：{cards:?}"
    );

    // 模型侧的回执要点明范围与排序依据，否则它无法判断这次是否按理解执行。
    let message = last_assistant_message(&store, conversation);
    let trace = message.tool_trace.first().expect("有轨迹");
    assert_eq!(trace.summary, "命中 2 封", "{trace:?}");
}

/// `important_only` 不再存在：它曾经是「重要 = 星标或近7天未读」这个
/// 具体口径的产物。留着它就等于系统替所有用户决定了「什么算重要」——
/// 那正是本次要修掉的东西。
#[tokio::test]
async fn important_only_is_no_longer_accepted() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::with_tool(tool_call(
            "c1",
            AiToolName::EmailSearch,
            json!({ "motive": "查邮件", "important_only": true }),
        )),
        Turn::text("参数不对，我换个方式查。"),
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
    // 工具层要拒掉这个参数，并告诉模型正确做法是写判据。
    let trace = message
        .tool_trace
        .iter()
        .find(|trace| trace.tool == AiToolName::EmailSearch)
        .expect("要留下 email_search 的轨迹");
    assert_eq!(trace.status, me_core::ai::AiToolStatus::Error);
    assert!(
        trace.summary.contains("important_only"),
        "要明确点名这个参数没了：{}",
        trace.summary
    );
    assert!(
        trace.summary.contains("criteria"),
        "要告诉模型正确的替代做法是写判据：{}",
        trace.summary
    );
}

/// 端到端回放那次事故：模型完全不配合（第一版的行为）。
///
/// 旧行为：8 次 `email_search`、6 次报「至少需要一个检索条件」、正文 13 个换行。
/// 现在：首轮只给两个计划工具，业务工具根本调不到；模型不配合时引擎合成
/// 单步清单收场，正文一定有内容，界面上看得到「做了什么」。
#[tokio::test]
async fn the_original_incident_now_produces_a_visible_answer() {
    let (_dir, store) = open();
    // 模型完全按旧行为来：想直接调业务工具，参数也不给条件。
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(tool_call(
            "old1",
            AiToolName::EmailSearch,
            json!({ "motive": "查看重要邮件" }),
        )),
        Turn::with_tool(tool_call(
            "old2",
            AiToolName::EmailSearch,
            json!({ "motive": "查看重要邮件，未读优先" }),
        )),
        // 引擎催「还没回答」，模型仍然空白。
        Turn::text("\n\n\n\n\n\n\n\n\n\n\n\n\n"),
        // 再催一次，还是空白。
        Turn::text("\n\n\n\n\n\n\n\n\n\n\n\n\n"),
        // 引擎不再催了（机会用完），模型第三次空白。
        Turn::text("\n\n\n\n\n\n\n\n\n\n\n\n\n"),
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
    // 收集事件直到消息完成（带超时保护，避免脚本不够时死等）。
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut completed = false;
    while std::time::Instant::now() < deadline && !completed {
        let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await
        else {
            continue;
        };
        if matches!(event, Event::AiMessageCompleted { .. }) {
            completed = true;
        }
    }
    assert!(completed, "这一轮应该正常收尾");

    let message = last_assistant_message(&store, conversation);
    // 1) 正文不是空白：这是旧事故最刺眼的症状。
    assert!(
        message.content.trim().chars().count() > 0,
        "正文不能是空白（旧事故是 13 个换行）"
    );
    // 2) 有一份清单，用户能看到 AI 想做什么。
    let plan = message.plan.expect("必须有一份清单");
    assert!(!plan.steps.is_empty(), "清单不能是空的");
    // 3) 业务工具在 PLAN 阶段**一次都没真正执行**。
    //
    // trace 里要有记录（用户要能看到「这次被拦下了」），但状态必须是 Error。
    let blocked = message
        .tool_trace
        .iter()
        .filter(|trace| trace.tool == AiToolName::EmailSearch)
        .collect::<Vec<_>>();
    assert!(
        !blocked.is_empty(),
        "模型确实尝试了业务工具：{:?}",
        message.tool_trace
    );
    for trace in &blocked {
        assert_eq!(
            trace.summary, "计划阶段不能执行工具",
            "业务工具不该被真正执行：{trace:?}"
        );
    }
    // 4) 留下了可排查的痕迹。
    assert!(
        message.error.is_some(),
        "要记录为什么不正常：{:?}",
        message.error
    );
}

/// 2026-11-07 的第二次事故：收尾阶段模型仍想调工具。
///
/// 收尾阶段引擎**不下发任何工具**，模型却仍把 `plan_update` 写了进来——
/// 这次它写在了正文里（协议层会把它还原成一次真正的调用）。
///
/// 引擎不能执行它：执行了也只会得到一句「这个阶段不需要工具」，白烧一轮。
/// 正确做法是再问一次「直接给答复」，然后把用户的答复收下来。
#[tokio::test]
async fn final_phase_tool_call_is_not_executed_but_asked_for_the_answer() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        // 1) 计划期：交一份清单。
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        // 2) 执行期：检索邮件。这步做完，清单就空了 → 进入收尾阶段。
        Turn::with_tool(tool_call(
            "search",
            AiToolName::EmailSearch,
            json!({ "motive": "按时间窗查最近邮件", "days": 7 }),
        )),
        // 3) 收尾阶段：模型仍想调 plan_update（事故现场）。
        Turn::with_tool(tool_call(
            "final_phase_update",
            AiToolName::PlanUpdate,
            json!({
                "motive": "这一步做完了",
                "steps": [{
                    "id": 1, "title": "检索近 7 天的邮件",
                    "status": "done", "evidence": "检索到 2 封"
                }]
            }),
        )),
        // 4) 引擎追问「直接给答复」，模型这次真的回答了。
        Turn::text(
            "近 7 天共 2 封投递提醒。\n\n[结果]\n状态：已完成\n做了什么：检索了近 7 天的邮件\n关键信息：共 2 封\n未完成：无",
        ),
    ]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "看看我的邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut completed = false;
    while std::time::Instant::now() < deadline && !completed {
        let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await
        else {
            continue;
        };
        if matches!(event, Event::AiMessageCompleted { .. }) {
            completed = true;
        }
    }
    assert!(completed, "这一轮应该正常收尾");

    let message = last_assistant_message(&store, conversation);
    // 1) 收尾阶段那次 plan_update **没有被执行**，但留下了痕迹。
    //
    // 2026-11-09 之前这里连 trace 都没有：事后无法区分「模型想调工具被丢弃」
    // 与「模型什么都没输出」，只能靠 `thinking` 猜。
    let dropped = message
        .tool_trace
        .iter()
        .find(|trace| trace.call_id == "final_phase_update")
        .expect("被丢弃的调用要留下 trace，否则事后无法判断发生了什么");
    assert_eq!(
        dropped.status,
        me_core::ai::AiToolStatus::Skipped,
        "收尾阶段的工具调用不该被执行：{dropped:?}"
    );
    // 2) 引擎追问了一次，并且明确指出「不要再更新清单」。
    let bodies = provider.message_bodies.lock().unwrap().clone();
    assert!(
        bodies.iter().any(|body| body.contains("不要再更新清单")
            || body.contains("现在是收尾阶段，不需要再调用工具")),
        "引擎要追问一次：{bodies:?}"
    );
    // 3) 收尾阶段的系统提示里明说了「不许调工具」。
    let systems = provider.systems.lock().unwrap().clone();
    assert!(
        systems
            .iter()
            .any(|system| system.contains("现在处于收尾阶段：不要调用任何工具")),
        "收尾阶段的系统提示要说明不许调工具：{systems:?}"
    );
    // 4) 用户的答复被收下来了，而且带着 [结果] 段落。
    assert!(
        message.content.contains("近 7 天共 2 封投递提醒"),
        "答复要保留：{}",
        message.content
    );
    assert!(
        message.content.contains("[结果]"),
        "答复要带 [结果] 段落：{}",
        message.content
    );
}

/// 收尾阶段模型连续两次都想调工具：**只放行一次记账**，之后收回。
///
/// 每次追问都是一次模型往返，而请求密度顶上去会撞限流线——
/// 2026-11-07 的事故就是这么把整轮对话拖垮的。所以补答预算封顶，
/// 而 `plan_update` 这个出口也只开一次。
#[tokio::test]
async fn final_phase_bookkeeping_is_allowed_at_most_once() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::with_tool(tool_call(
            "search",
            AiToolName::EmailSearch,
            json!({ "motive": "按时间窗查最近邮件", "days": 7 }),
        )),
        Turn::with_tool(tool_call(
            "final_1",
            AiToolName::PlanUpdate,
            json!({
                "motive": "做完了",
                "steps": [{
                    "id": 1, "title": "检索近 7 天的邮件",
                    "status": "done", "evidence": "检索到 2 封"
                }]
            }),
        )),
        // 这一次会被**放行执行**：上一轮观测到了「想记账」，出口开一次。
        Turn::with_tool(tool_call(
            "final_2",
            AiToolName::PlanUpdate,
            json!({
                "motive": "还是做完了",
                "steps": [{
                    "id": 1, "title": "检索近 7 天的邮件",
                    "status": "done", "evidence": "检索到 2 封"
                }]
            }),
        )),
        // 出口用完之后收回工具，模型只能给答复。
        Turn::text(
            "近 7 天共 2 封投递提醒。\n\n[结果]\n状态：已完成\n做了什么：检索了近 7 天的邮件\n关键信息：共 2 封\n未完成：无",
        ),
    ]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "看看我的邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut completed = false;
    while std::time::Instant::now() < deadline && !completed {
        let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await
        else {
            continue;
        };
        if matches!(event, Event::AiMessageCompleted { .. }) {
            completed = true;
        }
    }
    assert!(completed, "这一轮应该正常收尾");

    let bodies = provider.message_bodies.lock().unwrap().clone();
    // 只数**追问那句话**：每条 body 都带着前面所有消息，用宽泛的
    // `contains("不要再更新清单")` 会把切收尾阶段的提示也数进去。
    let nudges = bodies
        .iter()
        .filter(|body| {
            body.contains("清单已经由引擎记好了，不要再更新清单。请直接把最终答复写给用户")
        })
        .count();
    assert!(
        nudges <= 2,
        "追问次数要封顶（`MAX_FINAL_RETRIES = 2`），实际 {nudges}：{bodies:?}"
    );

    let message = last_assistant_message(&store, conversation);
    let trace = |call_id: &str| {
        message
            .tool_trace
            .iter()
            .find(|trace| trace.call_id == call_id)
    };
    // 第一次：没下发工具，记录下来但不执行。
    assert_eq!(
        trace("final_1").map(|trace| trace.status),
        Some(me_core::ai::AiToolStatus::Skipped),
        "第一次应当被丢弃：{:?}",
        message.tool_trace
    );
    // 第二次：出口开过一次——请求里**只下发了 plan_update** 这一个工具。
    //
    // 为什么看请求而不是看 trace：清单类工具成功时不写 trace（既有展示约定，
    // 否则活动区会被「更新清单」刷满）；而它想改的内容（把已 done 的步标 done、
    // 覆盖引擎写下的 evidence）本来就该被 `validate_plan` 挡住。
    // 放行它的目的是把「想记账」这个念头消费掉，不是让它改写事实。
    assert!(
        trace("final_2").is_none(),
        "放行的那次不该再被记成跳过：{:?}",
        message.tool_trace
    );
    let names = provider.tool_names.lock().unwrap().clone();
    assert!(
        names
            .iter()
            .any(|round| round.len() == 1 && round[0] == "plan_update"),
        "观测到想记账后，收尾期要只下发 plan_update：{names:?}"
    );
    // 答复仍然保留。
    assert!(
        message.content.contains("近 7 天共 2 封投递提醒"),
        "答复要保留：{}",
        message.content
    );
}

/// **通用性回归**：换一个定性词，机制照样跑得通。
///
/// 这一条是本次改造的核心断言。早先的实现把「什么算重要」**写死**进系统
/// （提示词第 13 条 + `important_only` 参数 + `is:important` 速记），
/// 于是模型不需要理解需求，只需要套模板——换个需求就全错。
///
/// 现在定性词由模型自己翻译成判据。这里刻意用与「重要」无关的两个词
/// （「紧急」「这周该回的」），并给出**完全不同的判据组合**，
/// 证明系统里没有任何一个预置口径在替模型做决定。
#[tokio::test]
async fn any_qualitative_requirement_is_translated_by_the_model() {
    for (question, criteria) in [
        (
            "有什么紧急的",
            json!([
                {"label": "今天到的", "days": 1},
                {"label": "带附件的", "has_attachment": true}
            ]),
        ),
        (
            "这周该回的",
            json!([
                {"label": "我发出的", "from": "me@example.com"},
                {"label": "本周未读的", "unread_only": true, "days": 7}
            ]),
        ),
    ] {
        let (_dir, store) = open();
        let provider = MockAiProvider::new(vec![
            Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
            Turn::with_tool(tool_call(
                "c1",
                AiToolName::EmailSearch,
                json!({ "motive": "按判据查", "criteria": criteria }),
            )),
            Turn::text("按这些判据找过了。"),
        ]);
        let (commands, mut rx) = start_service(&store, provider.clone(), true);
        let conversation = new_conversation(&commands, &mut rx, &store).await;

        commands
            .send(Command::SendAiMessage {
                conversation,
                content: question.to_string(),
                context: Vec::new(),
            })
            .expect("send");
        let _ = wait_for(&mut rx, |event| {
            matches!(event, Event::AiMessageCompleted { .. })
        })
        .await;

        let message = last_assistant_message(&store, conversation);
        let trace = message
            .tool_trace
            .iter()
            .find(|trace| trace.tool == AiToolName::EmailSearch)
            .expect("要有 email_search 的轨迹");
        assert_eq!(
            trace.status,
            me_core::ai::AiToolStatus::Ok,
            "「{question}」的判据应当能正常执行：{trace:?}"
        );
        // 判据名出现在清单条上——用户据此判断模型有没有理解错。
        let plan = message.plan.expect("有清单");
        let digest = plan.steps[0].args_digest().unwrap_or_default();
        assert!(
            digest.contains("判据"),
            "「{question}」的判据要显示给用户：{digest}"
        );
    }
}

/// 四个新事件都要发出去：界面靠它们显示尝试角标、自检结论与重列轮次。
#[tokio::test]
async fn plan_events_are_emitted() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::with_tool(tool_call(
            "c1",
            AiToolName::EmailSearch,
            json!({ "motive": "查最近 7 天未读", "unread_only": true, "days": 7 }),
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

    // 收集所有事件直到消息完成。
    let mut attempts = 0;
    let mut finished = false;
    loop {
        let event = wait_for(&mut rx, |_| true).await;
        match &event {
            Event::AiPlanStepAttempt { .. } => attempts += 1,
            Event::AiPlanFinished { .. } => finished = true,
            Event::AiMessageCompleted { .. } => break,
            _ => {}
        }
    }

    assert!(attempts >= 1, "每次尝试都要发事件");
    assert!(finished, "收尾要发 AiPlanFinished");
}

/// 跨轮延续：上一轮没做完的清单，第二轮要能看到「还剩哪几步」。
///
/// 没有这个，用户说「继续」时模型会从头规划一遍，用户会以为自己前面做的白费了。
#[tokio::test]
async fn unfinished_plan_carries_over_to_the_next_turn() {
    let (_dir, store) = open();
    // 第一轮：清单里两步，但模型只做了第一步就停。
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(tool_call(
            "plan1",
            AiToolName::PlanSubmit,
            json!({
                "motive": "分两步查",
                "goal": "把重要的邮件整理出来",
                "steps": [
                    {"id": 1, "title": "检索星标邮件", "kind": "tool", "capability": "mail",
                     "tool": "email_search", "args": {"flagged_only": true}, "expect": "列出星标邮件"},
                    {"id": 2, "title": "读第一封正文", "kind": "tool", "capability": "mail",
                     "tool": "mail_read", "args": {"message_id": 1}, "expect": "拿到正文"}
                ]
            }),
        )),
        Turn::with_tool(tool_call(
            "c1",
            AiToolName::EmailSearch,
            json!({ "motive": "执行第 1 步", "step_id": 1, "flagged_only": true }),
        )),
        // 第一轮到这里就停了（清单还有第 2 步没做）。
        Turn::text("找到了 1 封星标邮件，正文还没读。"),
        // 第二轮：用户说「继续」。
        Turn::with_tool(tool_call(
            "plan2",
            AiToolName::PlanSubmit,
            json!({
                "motive": "接着上一步做",
                "goal": "把重要的邮件整理出来",
                "steps": [
                    {"id": 1, "title": "读第一封正文", "kind": "tool", "capability": "mail",
                     "tool": "mail_read", "args": {"message_id": 1}, "expect": "拿到正文"}
                ]
            }),
        )),
        Turn::with_tool(tool_call(
            "c2",
            AiToolName::MailRead,
            json!({ "motive": "执行第 1 步", "step_id": 1, "message_id": 1 }),
        )),
        Turn::text("正文读完了。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    // 第一轮。
    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "帮我整理重要的邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    // 第二轮：用户说「继续」。
    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "继续".to_string(),
            context: Vec::new(),
        })
        .expect("send");

    // 等第二轮跑完。
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await
        else {
            continue;
        };
        if let Event::AiMessageCompleted { .. } = event {
            break;
        }
    }

    // 直接查第二轮的请求：MockAiProvider 记录了每轮的消息内容。
    let rounds = provider.message_bodies.lock().unwrap().clone();
    assert!(
        rounds
            .iter()
            .any(|body| body.contains("还没做完的清单") && body.contains("读第一封正文")),
        "第二轮要告诉模型上一轮还剩什么：{rounds:?}"
    );
}

// ---------------------------------------------------------------------------
// 思考双通道：分段、全序 seq、落库
// ---------------------------------------------------------------------------

/// 收集一轮回复里的全部流式增量，直到终止事件。
async fn collect_deltas(
    rx: &mut mpsc::Receiver<Event>,
) -> Vec<(u64, me_core::command::AiDeltaKind, u8, String)> {
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(Event::AiStreamDelta {
                seq,
                kind,
                segment,
                delta,
                ..
            })) => out.push((seq, kind, segment, delta)),
            Ok(Some(Event::AiMessageCompleted { .. })) => return out,
            Ok(Some(_)) => continue,
            Ok(None) => panic!("事件通道关闭"),
            Err(_) => panic!("等待流式增量超时"),
        }
    }
}

/// 三段思考（每次工具调用切一段）要正确落库，且思考不能混进正文。
#[tokio::test]
async fn thinking_is_segmented_per_tool_round_and_persisted() {
    use me_core::command::AiDeltaKind;

    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])).thinking("先想清楚要查什么"),
        Turn::with_tool(tool_call(
            "c1",
            AiToolName::EmailSearch,
            json!({ "motive": "查最近的邮件", "days": 7 }),
        ))
        .thinking("结果回来了，看看有没有命中"),
        Turn::text("近 7 天没有新的邮件。").thinking("可以收尾了"),
    ]);
    let (commands, mut rx) = start_service(&store, provider, true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "最近有什么邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let deltas = collect_deltas(&mut rx).await;

    // 1) 两条通道都真的发了增量，且思考有三段。
    let thinking: Vec<_> = deltas
        .iter()
        .filter(|(_, kind, _, _)| *kind == AiDeltaKind::Thinking)
        .collect();
    assert!(!thinking.is_empty(), "思考增量不能丢：{deltas:?}");
    assert!(
        deltas
            .iter()
            .any(|(_, kind, _, _)| *kind == AiDeltaKind::Answer),
        "回答增量也要在：{deltas:?}"
    );
    let segments: Vec<u8> = thinking.iter().map(|(_, _, seg, _)| *seg).collect();
    assert!(
        segments.windows(2).all(|pair| pair[0] <= pair[1]),
        "段号不能回退：{segments:?}"
    );

    // 2) seq 全局单调：拆两个 throttle 最容易在这里撞号。
    let seqs: Vec<u64> = deltas.iter().map(|(seq, ..)| *seq).collect();
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "思考与回答必须共用一条全序，不能各从 0 开始：{seqs:?}"
    );

    // 3) 落库：三段、段序、跟随工具标记、正文不含思考。
    let message = last_assistant_message(&store, conversation);
    let block = message.thinking.expect("思考要落库");
    assert_eq!(block.segments.len(), 3, "每轮工具调用切一段：{block:?}");
    assert_eq!(
        block.segments.iter().map(|s| s.index).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(block.segments[0].text, "先想清楚要查什么");
    assert!(block.segments[0].followed_by_tool);
    assert!(!block.segments[2].followed_by_tool);
    assert!(!block.truncated);
    assert!(
        !message.content.contains("先想清楚"),
        "思考不能混进正文：{}",
        message.content
    );
}

/// 没有工具调用的轮次，思考也要收成最后一段落库（不能整段丢掉）。
#[tokio::test]
async fn thinking_without_tools_still_persists_as_one_segment() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(tool_call(
            "direct",
            AiToolName::AnswerDirectly,
            json!({ "reason": "纯闲聊", "content": "你好" }),
        ))
        .thinking("这不需要工具"),
        Turn::text("你好！").thinking("直接回答就好"),
    ]);
    let (commands, mut rx) = start_service(&store, provider, true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "你好".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = collect_deltas(&mut rx).await;

    let message = last_assistant_message(&store, conversation);
    let block = message.thinking.expect("思考要落库");
    assert!(!block.segments.is_empty(), "至少要有一段：{block:?}");
    assert!(
        block
            .segments
            .iter()
            .any(|s| s.text.contains("这不需要工具")),
        "第一轮的思考不能被切段逻辑丢掉：{block:?}"
    );
}

/// 检索卡片要随 trace 落库：历史回看时它们才能重新显示出来。
#[tokio::test]
async fn email_cards_ride_along_with_the_persisted_trace() {
    let (_dir, store) = open();
    let now = me_core::now_unix();
    let ids = seed_mail(
        &store,
        &[
            ("标记那封", "a@example.com", now - 3600, 0b100),
            ("未读那封", "b@example.com", now - 7200, 0b000),
            ("无关那封", "c@example.com", now - 10800, 0b001),
        ],
    );
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::with_tool(tool_call(
            "c1",
            AiToolName::EmailSearch,
            json!({
                "motive": "按两条判据查",
                "criteria": [
                    {"label": "标记过的", "flagged_only": true},
                    {"label": "未读的", "unread_only": true}
                ]
            }),
        )),
        Turn::text("找到两封。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider, true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "帮我找出值得优先看的邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let message = last_assistant_message(&store, conversation);
    let trace = message
        .tool_trace
        .iter()
        .find(|trace| trace.tool == AiToolName::EmailSearch)
        .expect("要有 email_search 的 trace");
    assert_eq!(trace.cards.len(), 2, "卡片要落库：{trace:?}");
    let ids_in_cards: Vec<_> = trace.cards.iter().map(|card| card.message).collect();
    assert!(ids_in_cards.contains(&ids[0]));
    assert!(ids_in_cards.contains(&ids[1]));
    assert!(!ids_in_cards.contains(&ids[2]));
    assert!(
        trace.cards.iter().any(|card| card.reason == "标记过的"),
        "命中理由也要带上：{trace:?}"
    );
}

/// 失败的清单类调用（plan_submit）必须留下痕迹。
///
/// 2026-11-07 的事故：模型为了一份清单连续改了 4 次 capability 参数，
/// 4 次工具往返全废在参数上——而 plan_submit 是 bookkeeping 工具，
/// 失败时既不发事件也不进 trace，用户界面上一个字都看不到。
///
/// 成功的 plan_submit 仍然不显示（清单条本身就是它的展示位）。
#[tokio::test]
async fn failed_plan_submit_is_visible_in_the_trace() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        // 第一份清单：第 2 步的 tool 写错 → 参数校验失败。
        Turn::with_tool(AiToolCall {
            id: "bad_plan".to_string(),
            name: AiToolName::PlanSubmit,
            parse_ok: true,
            arguments: json!({
                "motive": "先建个清单",
                "goal": "查看邮件",
                "steps": [
                    {
                        "id": 1,
                        "title": "检索近 7 天邮件",
                        "kind": "tool",
                        "capability": "mail",
                        "tool": "email_search",
                        "args": {"days": 7},
                        "expect": "拿到邮件列表"
                    },
                    {
                        "id": 2,
                        "title": "归纳成一段结论",
                        "kind": "tool",
                        "capability": "mail",
                        "tool": "not_a_real_tool",
                        "expect": "给出一段可读的归纳"
                    }
                ]
            }),
        }),
        // 第二份清单：参数正确 → 成功。
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::with_tool(tool_call(
            "c1",
            AiToolName::EmailSearch,
            json!({ "motive": "查最近的邮件", "days": 7 }),
        )),
        Turn::text("近 7 天没有新的邮件。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider, true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "看看我的邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let message = last_assistant_message(&store, conversation);
    let submits: Vec<_> = message
        .tool_trace
        .iter()
        .filter(|trace| trace.tool == AiToolName::PlanSubmit)
        .collect();
    assert!(
        !submits.is_empty(),
        "失败的 plan_submit 必须出现在 trace 里，用户才知道模型试了几次：{:?}",
        message.tool_trace
    );
    assert!(
        submits
            .iter()
            .any(|trace| trace.status == me_core::ai::AiToolStatus::Error),
        "失败的 plan_submit 状态应为 error：{submits:?}"
    );
}

/// 事故里那三种「差一点点」的计划参数，现在应当**一次就通过**。
///
/// 2026-11-07 的真实对话：模型连续 5 次 `plan_submit` 被拒——
/// `date_from`（应为 `after`）、判据漏写 `label`、推理步却带了 `tool`。
/// 每次被拒都是一次模型往返，5 次往返把请求密度顶到限流线上，
/// 整轮以「请求过于频繁」失败，而模型其实已经查到了 20 封邮件。
///
/// 这条测试锁死「不再被拒」：一份带这些毛病的计划必须**一次建清单成功**。
#[tokio::test]
async fn almost_right_plan_is_accepted_without_a_retry() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(AiToolCall {
            id: "plan".to_string(),
            name: AiToolName::PlanSubmit,
            parse_ok: true,
            arguments: json!({
                "motive": "先建个清单",
                "goal": "看看邮件并生成画像",
                "steps": [
                    {
                        "id": 1,
                        "title": "检索近 30 天邮件",
                        "kind": "tool",
                        // 少了 capability —— 引擎按工具名反推。
                        "tool": "email_search",
                        // date_from 是错的名字 —— 引擎改成 after。
                        "args": {"date_from": "2026-10-01"},
                        // 判据漏了 label —— 引擎按条件反推名字。
                        "expect": "拿到邮件列表"
                    },
                    {
                        "id": 2,
                        "title": "归纳出用户画像",
                        // 推理步却带了 tool —— 引擎按工具步处理。
                        "kind": "reason",
                        "tool": "email_search",
                        "expect": "给出一段画像"
                    }
                ]
            }),
        }),
        Turn::with_tool(tool_call(
            "c1",
            AiToolName::EmailSearch,
            json!({ "motive": "查最近的邮件", "days": 30 }),
        )),
        Turn::text("近 30 天有 2 封。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider, true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "看看我的邮件 生成我的用户画像".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let message = last_assistant_message(&store, conversation);
    let submits: Vec<_> = message
        .tool_trace
        .iter()
        .filter(|trace| trace.tool == AiToolName::PlanSubmit)
        .collect();
    assert!(
        submits
            .iter()
            .all(|trace| trace.status != me_core::ai::AiToolStatus::Error),
        "这份计划不该被拒：{submits:?}"
    );
    // 清单建起来了，而且走到了业务工具调用。
    assert!(
        message
            .tool_trace
            .iter()
            .any(|trace| trace.tool == AiToolName::EmailSearch
                && trace.status == me_core::ai::AiToolStatus::Ok),
        "清单建好后应当继续执行：{:?}",
        message.tool_trace
    );
}

/// 模型连续交不出合格清单时，引擎不该一直等它。
///
/// 2026-11-07 的真实对话：模型连续 5 次 `plan_submit` 被拒（每次都是参数
/// 写错），5 次模型往返把请求密度顶到限流线上，后面的请求全被 429 打掉，
/// 整轮以「请求过于频繁」失败——**而模型其实已经查到了 20 封邮件**。
///
/// 闸门的作用：连续两次交不出清单，引擎就自己代建一份，让流程继续往下走。
/// 清单不完美，总比整轮失败好。
#[tokio::test]
async fn repeated_plan_rejections_do_not_stall_the_turn() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![
        // 前两份清单参数都是错的（工具名不存在，猜不出意图，只能报错）。
        Turn::with_tool(bad_plan()),
        Turn::with_tool(bad_plan()),
        // 第三轮模型改用正确参数——但闸门已经生效，引擎自己建了清单，
        // 流程应该继续到业务工具调用。
        Turn::with_tool(tool_call(
            "c1",
            AiToolName::EmailSearch,
            json!({ "motive": "查最近的邮件", "days": 7 }),
        )),
        Turn::text("近 7 天有 2 封。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider, true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "看看我的邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let message = last_assistant_message(&store, conversation);
    assert!(
        message
            .tool_trace
            .iter()
            .any(|trace| trace.tool == AiToolName::EmailSearch
                && trace.status == me_core::ai::AiToolStatus::Ok),
        "闸门之后仍要走完业务调用：{:?}",
        message.tool_trace
    );
    assert_eq!(
        message.status,
        me_core::ai::AiMessageStatus::Complete,
        "不该整轮失败：{:?}",
        message.error
    );
}

/// 一份参数必然被拒的计划（工具名不存在）。
fn bad_plan() -> AiToolCall {
    AiToolCall {
        id: "bad_plan".to_string(),
        name: AiToolName::PlanSubmit,
        parse_ok: true,
        arguments: json!({
            "motive": "先建个清单",
            "goal": "查看邮件",
            "steps": [
                {
                    "id": 1,
                    "title": "检索近 7 天邮件",
                    "kind": "tool",
                    "capability": "mail",
                    "tool": "not_a_real_tool",
                    "expect": "拿到邮件列表"
                }
            ]
        }),
    }
}

/// 纯按时间窗检索时，卡片的命中理由要有区分度。
///
/// 2026-11-07 的事故：模型用 `days:7` 检索，20 张卡片的理由全是同一句
/// 「匹配筛选条件」——用户看不出任何区别，也无法判断模型有没有理解错。
#[tokio::test]
async fn time_window_search_gives_cards_a_readable_reason() {
    let (_dir, store) = open();
    let now = me_core::now_unix();
    seed_mail(
        &store,
        &[
            ("近的那封", "a@example.com", now - 3600, 0b100),
            ("远的那封", "b@example.com", now - 86400 * 3, 0b000),
        ],
    );
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        Turn::with_tool(tool_call(
            "c1",
            AiToolName::EmailSearch,
            json!({ "motive": "看最近的邮件", "days": 7 }),
        )),
        Turn::text("近 7 天有 2 封。"),
    ]);
    let (commands, mut rx) = start_service(&store, provider, true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "看看最近的邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let message = last_assistant_message(&store, conversation);
    let trace = message
        .tool_trace
        .iter()
        .find(|trace| trace.tool == AiToolName::EmailSearch)
        .expect("要有 email_search 的 trace");
    assert!(!trace.cards.is_empty(), "应当有卡片：{trace:?}");
    for card in &trace.cards {
        assert!(
            card.reason.contains("7 天") || card.reason.contains("近"),
            "时间窗检索的理由要能看出是按时间筛的：{:?}",
            card.reason
        );
        assert_ne!(
            card.reason, "匹配筛选条件",
            "不能退回无信息量的兜底文案：{:?}",
            card.reason
        );
    }
}

/// 2026-11-09 的事故回归：用户只说「天气怎么样」，不能因为没地点就查不出结果。
///
/// 当时 `weather_get` 在既没有 `location` 也没有 `location_id` 时直接返回
/// 「请提供城市名（location）或已保存地点的 location_id」，而用户其实早就把
/// 双流设成了默认地点——`is_default` 只被拼进「，默认」三个字，没有参与任何决策。
#[tokio::test]
async fn weather_without_a_location_falls_back_to_the_default_one() {
    use me_core::port::WeatherRepo;
    use me_core::weather::{
        CurrentWeather, DailyForecast, PROVIDER_QWEATHER, WeatherKind, WeatherLocationCandidate,
        WeatherSnapshot,
    };

    let (_dir, store) = open();

    // 库里有一个默认关注地点：双流。
    let location = store
        .writable()
        .insert_weather_location(&WeatherLocationCandidate {
            provider: PROVIDER_QWEATHER.to_string(),
            provider_location_id: "101270106".to_string(),
            name: "双流".to_string(),
            admin1: Some("四川省".to_string()),
            admin2: Some("成都".to_string()),
            country: Some("中国".to_string()),
            latitude: 30.57324,
            longitude: 103.92271,
            timezone: "Asia/Shanghai".to_string(),
        })
        .expect("插入地点");
    store
        .writable()
        .set_default_weather_location(location.id)
        .expect("设为默认");

    // 快照还在有效期内：不会联网，也就不用配置凭证。
    let now = me_core::now_unix();
    store
        .writable()
        .save_weather_snapshot(
            location.id,
            &WeatherSnapshot {
                provider: PROVIDER_QWEATHER.to_string(),
                fetched_at: now,
                expires_at: now + 3600,
                current: CurrentWeather {
                    observed_at: now,
                    temp_c: 16.0,
                    feels_like_c: 13.0,
                    humidity_percent: 48,
                    wind_scale: 3,
                    wind_dir: "北风".to_string(),
                    wind_speed_kmh: 13.0,
                    precip_mm: 0.0,
                    pressure_hpa: 1010.0,
                    visibility_km: 20.0,
                    cloud_percent: 80,
                    kind: WeatherKind::Cloudy,
                    text: "阴".to_string(),
                },
                hourly: Vec::new(),
                daily: vec![DailyForecast {
                    date: "2026-11-09".to_string(),
                    sunrise: Some("07:20".to_string()),
                    sunset: Some("18:05".to_string()),
                    temp_max_c: 18.0,
                    temp_min_c: 9.0,
                    precip_mm: 0.0,
                    kind: WeatherKind::Cloudy,
                    text: "阴".to_string(),
                    icon_day: "104".to_string(),
                    icon_night: "154".to_string(),
                    text_night: "阴".to_string(),
                }],
            },
        )
        .expect("写入快照");

    let provider = MockAiProvider::new(vec![
        // 1) 计划：只写一步，**故意不带地点**——事故现场就是这样。
        Turn::with_tool(tool_call(
            "plan",
            AiToolName::PlanSubmit,
            json!({
                "motive": "用户没说地点，直接查天气",
                "goal": "查询天气情况",
                "steps": [{
                    "id": 1,
                    "title": "查询天气",
                    "kind": "tool",
                    "capability": "weather",
                    "tool": "weather_get",
                    "args": { "days": 3 },
                    "expect": "拿到当前与近三天天气"
                }]
            }),
        )),
        // 2) 执行：weather_get 只带 days，不带 location / location_id。
        Turn::with_tool(tool_call(
            "weather",
            AiToolName::WeatherGet,
            json!({ "days": 3, "motive": "查询默认关注地点的天气" }),
        )),
        Turn::text(
            "双流当前 16°C，阴。\n\n[结果]\n状态：已完成\n做了什么：查询默认关注地点双流的天气\n关键信息：16°C，阴\n未完成：无",
        ),
    ]);
    let (commands, mut rx) = start_service(&store, provider, true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "天气怎么样".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let message = last_assistant_message(&store, conversation);
    let trace = message
        .tool_trace
        .iter()
        .find(|trace| trace.tool == AiToolName::WeatherGet)
        .expect("要真的调用 weather_get");
    assert_eq!(
        trace.status,
        me_core::ai::AiToolStatus::Ok,
        "没指定地点不该报错：{trace:?}"
    );
    assert!(
        trace.summary.contains("双流"),
        "应当落到默认地点双流：{trace:?}"
    );
    assert!(
        message.content.contains("双流"),
        "答复里要写明查的是默认地点：{}",
        message.content
    );
    // trace 里要留下**实际传的参数**：计划里写的是 `{}`，执行时传的是
    // `{days:3}`——两者不一致时，事后只能靠这个字段对齐（2026-11-09 之前没有）。
    assert!(
        trace
            .args
            .as_deref()
            .is_some_and(|args| args.contains("days")),
        "trace 要记下实际参数：{trace:?}"
    );
    // `motive` 不进 args：它是「为什么调」，不是业务参数。
    assert!(
        !trace.args.as_deref().unwrap_or_default().contains("motive"),
        "args 要剔除 motive：{trace:?}"
    );
    // 清单这一步是真完成，不是「失败被标 done」。
    let plan = message.plan.as_ref().expect("应当有清单");
    assert_eq!(plan.steps[0].status, me_core::ai_plan::PlanStatus::Done);
    assert!(
        plan.steps[0].failure.is_none(),
        "{:?}",
        plan.steps[0].failure
    );
}

/// 无进展闸：连续几轮只记账、不拿业务数据，就该提前收尾。
///
/// 催促各有各的上限（计划 2 次、重交 2 次、重列 2 次、补答 2 次），但它们是
/// **并列**的：一段对话可以在几个阶段之间来回蹭，每轮都是一次模型往返 +
/// 400ms 强制间隔。这条闸只看「这一轮有没有真的拿到业务数据」，
/// 是唯一能同时压住所有催促路径的那一条。
#[tokio::test]
async fn stalled_progress_stops_the_loop_early() {
    let (_dir, store) = open();
    // 每轮都说话（所以不会走「空正文」那条催促），但只调清单类工具。
    let stall = |id: &str| Turn {
        text: "第 1 步仍在执行中，等待工具回执。".to_string(),
        thinking: String::new(),
        tool_calls: vec![tool_call(
            id,
            AiToolName::PlanUpdate,
            json!({
                "motive": "标记为执行中",
                "steps": [{
                    "id": 1,
                    "title": "检索近 7 天的邮件",
                    "status": "running",
                    "evidence": "尚未返回"
                }]
            }),
        )],
        error: None,
        hold: false,
    };
    let provider = MockAiProvider::new(vec![
        Turn::with_tool(plan_submit_for(&[AiToolName::EmailSearch])),
        stall("stall-1"),
        stall("stall-2"),
        stall("stall-3"),
    ]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "看看我的邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let message = last_assistant_message(&store, conversation);
    assert!(
        message
            .error
            .as_deref()
            .is_some_and(|text| text.contains("没有拿到新的工具结果")),
        "要留下提前收尾的痕迹：{:?}",
        message.error
    );
    // 三轮空转之后必须停：没有第 4 次模型往返。
    assert!(
        provider.message_bodies.lock().unwrap().len() <= 4,
        "达到上限就该收尾，不该继续追问"
    );
    // 一次业务工具都没调过——这正是「没进展」的定义。
    assert!(
        !message
            .tool_trace
            .iter()
            .any(|trace| trace.tool == AiToolName::EmailSearch),
        "这一轮本来就没查过邮件：{:?}",
        message.tool_trace
    );
}

// ===========================================================================
// 回归语料（第三批）
// ===========================================================================
//
// 前两批加了很多闸，但一直是「跑得过测试」而说不出「跑了几轮、走了哪条路」。
// 这里把常见路径固化成脚本，每条断言四件事：
//
//   1. **总轮数**——模型往返几次（每轮一次请求 + 400ms 强制间隔）
//   2. **工具序列**——模型请求调用的顺序（不是引擎最终执行了什么）
//   3. **最终正文含关键字段**——用户到底有没有拿到数据
//   4. **清单终态与 `[结果]` 说同一件事**——最容易复发的那类矛盾
//
// 第 4 条是重点：清单全绿而正文自称「部分完成」（或反过来）在 2026-11-09
// 出过两次。语料里的模型回答**故意不写 `[结果]`**，让引擎去合成——
// 这样测的是引擎的口径，而不是测试自己写的文案。

/// 一条语料的期望。
struct Expect {
    name: &'static str,
    /// 模型请求调用的工具序列（按时间顺序）。
    tools: &'static [&'static str],
    /// 总轮数上限。
    max_rounds: usize,
    /// 最终正文必须出现的字段。
    contains: &'static [&'static str],
}

/// 等到这一轮收尾（完成或失败都算收尾）。
async fn wait_for_completion(rx: &mut mpsc::Receiver<Event>, seconds: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Some(Event::AiMessageCompleted { .. })) | Ok(Some(Event::AiStreamFailed { .. })) => {
                return true;
            }
            Ok(Some(_)) => {}
            Ok(None) => return false,
            Err(_) => {}
        }
    }
    false
}

/// 跑一条语料并断言四件事。
async fn run_case(
    store: &Store,
    turns: Vec<Turn>,
    input: &str,
    expect: Expect,
) -> me_core::ai::AiMessage {
    let provider = MockAiProvider::new(turns);
    let (commands, mut rx) = start_service(store, provider.clone(), true);
    let conversation = new_conversation(&commands, &mut rx, store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: input.to_string(),
            context: Vec::new(),
        })
        .expect("send");
    assert!(
        wait_for_completion(&mut rx, 30).await,
        "[{}] 这一轮没有收尾",
        expect.name
    );

    let message = last_assistant_message(store, conversation);

    // 1) 轮数
    let rounds = provider.message_bodies.lock().unwrap().len();
    assert!(
        rounds <= expect.max_rounds,
        "[{}] 用了 {rounds} 轮，上限 {}",
        expect.name,
        expect.max_rounds
    );

    // 2) 工具序列（前缀匹配）
    //
    // 用前缀而不是全等：工具失败之后引擎会走「自检 → 重列」，那里会多出
    // 一次 `plan_submit`。那是补救路径，不是主路径跑偏——语料要钉的是
    // 「这次走了哪条主路」，不是「一次都不许补救」。
    let tools = provider.tool_sequence.lock().unwrap().clone();
    let expected: Vec<String> = expect.tools.iter().map(|name| name.to_string()).collect();
    assert!(
        tools.starts_with(&expected),
        "[{}] 工具序列不对：期望以 {expected:?} 开头，实际 {tools:?}",
        expect.name
    );

    // 3) 正文关键字段
    for needle in expect.contains {
        assert!(
            message.content.contains(needle),
            "[{}] 正文里没有「{needle}」：{}",
            expect.name,
            message.content
        );
    }

    // 4) 清单终态与 [结果] 一致
    if let Some(plan) = message.plan.as_ref() {
        let (done, total) = plan.progress();
        let plan_ok = done == total && !plan.any_failed();
        assert!(
            message.content.contains("[结果]"),
            "[{}] 有清单就必须有 [结果] 段落：{}",
            expect.name,
            message.content
        );
        let claimed_ok = message.content.contains("状态：已完成");
        assert_eq!(
            claimed_ok,
            plan_ok,
            "[{}] 清单与 [结果] 说的不是一回事：清单 {done}/{total} 失败={}，正文：{}",
            expect.name,
            plan.any_failed(),
            message.content
        );
    }
    message
}

/// 手写一份单步计划。`plan_submit_for` 覆盖不到的参数组合（例如
/// `music_state` 无参数）用它。
fn plan_for(
    tool: AiToolName,
    capability: &str,
    title: &str,
    args: serde_json::Value,
) -> AiToolCall {
    tool_call(
        "plan",
        AiToolName::PlanSubmit,
        json!({
            "motive": "先给出计划再执行",
            "goal": "完成用户这一次的请求",
            "steps": [{
                "id": 1,
                "title": title,
                "kind": "tool",
                "capability": capability,
                "tool": tool.as_str(),
                "args": args,
                "expect": "拿到结果"
            }]
        }),
    )
}

/// 一个「标准现场」：库里有一个（或两个）关注地点，快照在有效期内。
///
/// 快照有效意味着**不会联网**，语料因此不依赖网络与凭证。
fn seed_weather_place(
    store: &Store,
    name: &str,
    admin2: &str,
    temp_c: f64,
    make_default: bool,
) -> me_core::weather::WeatherLocationId {
    use me_core::port::WeatherRepo;
    use me_core::weather::{
        CurrentWeather, DailyForecast, PROVIDER_QWEATHER, WeatherKind, WeatherLocationCandidate,
        WeatherSnapshot,
    };

    let location = store
        .writable()
        .insert_weather_location(&WeatherLocationCandidate {
            provider: PROVIDER_QWEATHER.to_string(),
            provider_location_id: format!("id-{name}"),
            name: name.to_string(),
            admin1: Some("四川省".to_string()),
            admin2: Some(admin2.to_string()),
            country: Some("中国".to_string()),
            latitude: 30.5,
            longitude: 104.0,
            timezone: "Asia/Shanghai".to_string(),
        })
        .expect("插入地点");
    if make_default {
        store
            .writable()
            .set_default_weather_location(location.id)
            .expect("设为默认");
    }
    let now = me_core::now_unix();
    store
        .writable()
        .save_weather_snapshot(
            location.id,
            &WeatherSnapshot {
                provider: PROVIDER_QWEATHER.to_string(),
                fetched_at: now,
                expires_at: now + 3600,
                current: CurrentWeather {
                    observed_at: now,
                    temp_c,
                    feels_like_c: temp_c - 3.0,
                    humidity_percent: 48,
                    wind_scale: 3,
                    wind_dir: "北风".to_string(),
                    wind_speed_kmh: 13.0,
                    precip_mm: 0.0,
                    pressure_hpa: 1010.0,
                    visibility_km: 20.0,
                    cloud_percent: 80,
                    kind: WeatherKind::Cloudy,
                    text: "阴".to_string(),
                },
                hourly: Vec::new(),
                daily: vec![DailyForecast {
                    date: "2026-11-09".to_string(),
                    sunrise: Some("07:20".to_string()),
                    sunset: Some("18:05".to_string()),
                    temp_max_c: temp_c + 2.0,
                    temp_min_c: temp_c - 7.0,
                    precip_mm: 0.0,
                    kind: WeatherKind::Cloudy,
                    text: "阴".to_string(),
                    icon_day: "104".to_string(),
                    icon_night: "154".to_string(),
                    text_night: "阴".to_string(),
                }],
            },
        )
        .expect("写入快照");
    location.id
}

/// 语料 1：天气·用户没说地点，但有默认关注地点。
///
/// 走「默认兜底」：不该报错、不该问用户、也不该放弃。
#[tokio::test]
async fn corpus_weather_falls_back_to_the_default_place() {
    let (_dir, store) = open();
    seed_weather_place(&store, "双流", "成都", 16.0, true);

    let turns = vec![
        Turn::with_tool(plan_for(
            AiToolName::WeatherGet,
            "weather",
            "查询天气",
            json!({}),
        )),
        Turn::with_tool(tool_call(
            "weather",
            AiToolName::WeatherGet,
            json!({ "days": 3, "motive": "查询默认地点的天气" }),
        )),
        // 故意不写 [结果]：由引擎合成，这样测的是引擎口径。
        Turn::text("双流现在 16°C，阴。"),
    ];
    let message = run_case(
        &store,
        turns,
        "天气怎么样",
        Expect {
            name: "weather-default",
            tools: &["plan_submit", "weather_get"],
            max_rounds: 3,
            contains: &["16", "阴", "[结果]", "状态：已完成"],
        },
    )
    .await;
    assert!(
        message.content.contains("双流"),
        "答复要写明查的是哪个地点：{}",
        message.content
    );
}

/// 语料 2：天气·用户明确说了城市。
///
/// 与语料 1 成对：**说了地点就用说的那个，不再问**。两条一起才说明
/// 「默认兜底」不是「无脑用默认」。
///
/// 注：`weather_locations` 的第一条恒为默认（`is_default = count == 0`），
/// 所以「多个关注地点且全都不是默认」不可构造；真正的多候选只发生在
/// 联网按名字搜索时，离线语料覆盖不到。
#[tokio::test]
async fn corpus_weather_uses_the_named_place() {
    let (_dir, store) = open();
    seed_weather_place(&store, "双流", "成都", 16.0, true);
    seed_weather_place(&store, "自贡", "自贡", 19.0, false);

    let turns = vec![
        Turn::with_tool(plan_for(
            AiToolName::WeatherGet,
            "weather",
            "查询自贡的天气",
            json!({ "location": "自贡" }),
        )),
        Turn::with_tool(tool_call(
            "weather",
            AiToolName::WeatherGet,
            json!({ "location": "自贡", "days": 1, "motive": "用户点名要自贡" }),
        )),
        Turn::text("自贡现在 19°C，阴。"),
    ];
    let message = run_case(
        &store,
        turns,
        "自贡天气怎么样",
        Expect {
            name: "weather-named",
            tools: &["plan_submit", "weather_get"],
            max_rounds: 3,
            contains: &["自贡", "19", "[结果]", "状态：已完成"],
        },
    )
    .await;
    assert!(
        !message.content.contains("哪一个"),
        "用户已经说了地点，不该再问：{}",
        message.content
    );
}

/// 语料 3：邮件·0 命中。
///
/// 0 命中是**合法的业务结果**，不是失败：清单要记完成，`[结果]` 也要说完成。
#[tokio::test]
async fn corpus_mail_empty_result_is_not_a_failure() {
    let (_dir, store) = open();

    let turns = vec![
        Turn::with_tool(plan_for(
            AiToolName::EmailSearch,
            "mail",
            "检索近 7 天的未读邮件",
            json!({ "days": 7, "unread_only": true }),
        )),
        Turn::with_tool(tool_call(
            "search",
            AiToolName::EmailSearch,
            json!({ "days": 7, "motive": "查最近一周的未读" }),
        )),
        Turn::text("近 7 天没有符合条件的邮件。"),
    ];
    run_case(
        &store,
        turns,
        "最近一周有什么未读邮件",
        Expect {
            name: "mail-empty",
            tools: &["plan_submit", "email_search"],
            max_rounds: 3,
            contains: &["没有", "[结果]", "状态：已完成"],
        },
    )
    .await;
}

/// 语料 4：邮件·多条件命中。
#[tokio::test]
async fn corpus_mail_with_results_reports_the_count() {
    let (_dir, store) = open();
    seed_mail(
        &store,
        &[
            ("周报", "boss@example.com", 1_800_000_000, 0),
            ("报价单", "sales@example.com", 1_800_000_100, 0),
            ("复盘", "team@example.com", 1_800_000_200, 0),
        ],
    );

    let turns = vec![
        Turn::with_tool(plan_for(
            AiToolName::EmailSearch,
            "mail",
            "检索最近的邮件",
            json!({ "days": 30 }),
        )),
        Turn::with_tool(tool_call(
            "search",
            AiToolName::EmailSearch,
            json!({ "days": 30, "motive": "查最近 30 天" }),
        )),
        Turn::text("最近 30 天有 3 封邮件，包括周报、报价单和复盘。"),
    ];
    run_case(
        &store,
        turns,
        "最近有什么邮件",
        Expect {
            name: "mail-results",
            tools: &["plan_submit", "email_search"],
            max_rounds: 3,
            contains: &["3 封", "[结果]", "状态：已完成"],
        },
    )
    .await;
}

/// 语料 5：日历·删除**只发确认卡，不直接删**。
#[tokio::test]
async fn corpus_calendar_delete_only_asks_for_confirmation() {
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

    let turns = vec![
        Turn::with_tool(plan_for(
            AiToolName::CalendarDelete,
            "calendar",
            "删除周五的复盘",
            json!({ "event_id": event.id.0 }),
        )),
        Turn::with_tool(tool_call(
            "delete",
            AiToolName::CalendarDelete,
            json!({ "event_id": event.id.0, "motive": "用户要删掉它" }),
        )),
        Turn::text("已经请你确认删除，确认前这条还在。"),
    ];
    let message = run_case(
        &store,
        turns,
        "删掉周五的复盘",
        Expect {
            name: "calendar-delete",
            tools: &["plan_submit", "calendar_delete"],
            max_rounds: 3,
            contains: &["确认"],
        },
    )
    .await;
    // 没有真的删：事件还在库里。
    let still_there = store
        .writable()
        .get_event(event.id)
        .expect("读日程")
        .is_some();
    assert!(still_there, "确认前不能真删：{}", message.content);
}

/// 语料 6：音乐·播放器没有响应（超时）。
///
/// 桥接层必须把它变成一句**可读的工具错误**，整轮不能因此失败——
/// 否则「播放器正在扫曲库」会把一条正常的对话打成失败。
#[tokio::test]
async fn corpus_music_timeout_is_a_readable_tool_error() {
    let (_dir, store) = open();

    let turns = vec![
        Turn::with_tool(plan_for(
            AiToolName::MusicState,
            "music",
            "查看播放状态",
            json!({}),
        )),
        Turn::with_tool(tool_call(
            "music",
            AiToolName::MusicState,
            json!({ "motive": "用户想知道在放什么" }),
        )),
        Turn::text("播放器没有响应，可能还在扫描曲库。"),
        // 工具失败会多走一次「自检 → 收尾」，所以这里要多备一轮。
        Turn::text("播放器没有响应，可能还在扫描曲库。"),
        Turn::text("播放器没有响应，可能还在扫描曲库。"),
    ];
    run_case(
        &store,
        turns,
        "现在在放什么歌",
        Expect {
            name: "music-timeout",
            tools: &["plan_submit", "music_state"],
            // 失败路径固定多一轮自检：plan → execute → reflect → final。
            max_rounds: 5,
            contains: &["没有响应", "[结果]"],
        },
    )
    .await;
}

/// 语料 7：`criteria` 与普通条件混写 → 计划期就被点名，改完才放行。
///
/// 2026-11-09 之前这条是**静默失败**：执行期看到 `criteria` 就整条走判据路径，
/// 顶层的 `days` / `query` 一个都不用、也不报错，用户拿到的结果比预期宽。
/// 这条语料钉的是「混写进不了执行阶段」。
#[tokio::test]
async fn corpus_criteria_mixed_with_plain_conditions_is_rejected_then_fixed() {
    let (_dir, store) = open();

    let mixed = tool_call(
        "plan-mixed",
        AiToolName::PlanSubmit,
        json!({
            "motive": "先给出计划再执行",
            "goal": "找出重要的邮件",
            "steps": [{
                "id": 1,
                "title": "找重要邮件",
                "kind": "tool",
                "capability": "mail",
                "tool": "email_search",
                "args": {
                    "days": 7,
                    "criteria": [{ "label": "已星标", "flagged_only": true }]
                },
                "expect": "拿到结果"
            }]
        }),
    );
    let fixed = plan_for(
        AiToolName::EmailSearch,
        "mail",
        "找重要邮件",
        json!({ "days": 7 }),
    );

    let turns = vec![
        // 第一次：混写，必须被计划期拒掉。
        Turn::with_tool(mixed),
        // 第二次：改成单一条件，放行。
        Turn::with_tool(fixed),
        Turn::with_tool(tool_call(
            "search",
            AiToolName::EmailSearch,
            json!({ "days": 7, "motive": "找最近 7 天的邮件" }),
        )),
        Turn::text("最近 7 天没有符合条件的邮件。"),
    ];
    let provider = MockAiProvider::new(turns);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "有没有重要的邮件".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    assert!(wait_for_completion(&mut rx, 30).await, "这一轮没有收尾");

    // 序列里必须有**两次** `plan_submit`：第一次被拒、第二次才通过。
    let tools = provider.tool_sequence.lock().unwrap().clone();
    assert_eq!(
        tools.iter().filter(|name| *name == "plan_submit").count(),
        2,
        "混写的计划必须被拒一次：{tools:?}"
    );

    let message = last_assistant_message(&store, conversation);

    // 拒绝理由要点名混写：只说「参数不对」的话，模型会原样再交一次。
    // 理由在 tool_result 里（不是 user 消息），所以看 trace。
    let rejected = message
        .tool_trace
        .iter()
        .find(|trace| trace.tool == AiToolName::PlanSubmit)
        .expect("被拒的 plan_submit 必须留下 trace");
    assert_eq!(
        rejected.status,
        me_core::ai::AiToolStatus::Error,
        "混写的计划必须是失败态：{rejected:?}"
    );
    assert!(
        rejected.summary.contains("混写") || rejected.summary.contains("criteria"),
        "拒绝理由要说清是混写：{rejected:?}"
    );

    // 最终仍然拿到结果（说明纠正之后流程能继续）。
    assert!(
        message.content.contains("[结果]"),
        "纠正后应当能收尾：{}",
        message.content
    );
}

/// 反复出现的失败要沉淀成「教训」，在**下一轮**就随系统提示回灌。
///
/// 模型不会跨轮学习：同一个参数错误换个会话会原样重犯。提示词里叮嘱
/// 「别犯同样的错」没用（它看不见自己上次怎么错的），只有把历史事实喂回去才有用。
/// 这条测试守的是「引擎负责记，模型负责避」这条分工。
#[tokio::test]
async fn repeated_tool_failure_becomes_a_lesson_in_the_next_system_prompt() {
    let (_dir, store) = open();
    let provider = MockAiProvider::new(vec![Turn::text("知道了。")]);
    let (commands, mut rx) = start_service(&store, provider.clone(), true);
    let conversation = new_conversation(&commands, &mut rx, &store).await;

    // 先落一条「上一轮同一种失败出现了两次」的助手消息。
    // 引擎只认已落库的事实，不读内存里的计数。
    let failed = |call_id: &str, summary: &str| me_core::ai::AiToolTrace {
        call_id: call_id.to_string(),
        tool: AiToolName::CalendarCreate,
        status: me_core::ai::AiToolStatus::Error,
        summary: summary.to_string(),
        duration_ms: 1,
        args: None,
        motive: None,
        motive_source: Default::default(),
        step_id: None,
        attempt: None,
        cards: Vec::new(),
    };
    let writable = store.writable();
    let message = writable
        .begin_assistant_message(conversation)
        .expect("插入助手消息");
    writable
        .finish_assistant_message(
            message,
            "失败了",
            me_core::ai::AiMessageStatus::Complete,
            None,
            &[
                failed("c1", "第 1 步的日程时间不合法：开始时间必须早于结束时间"),
                failed("c2", "第 3 步的日程时间不合法：开始时间必须早于结束时间"),
            ],
            None,
            None,
            None,
        )
        .expect("落库");

    commands
        .send(Command::SendAiMessage {
            conversation,
            content: "帮我看下这周的安排".to_string(),
            context: Vec::new(),
        })
        .expect("send");
    let _ = wait_for(&mut rx, |event| {
        matches!(event, Event::AiMessageCompleted { .. })
    })
    .await;

    let systems = provider.systems.lock().expect("lock").clone();
    let system = systems.first().expect("有请求");
    assert!(
        system.contains("以往踩过的坑"),
        "反复失败要回灌成教训：{system}"
    );
    assert!(
        system.contains("calendar_create"),
        "教训要点名是哪个工具：{system}"
    );
    // 归一化后步号不同也应归为同一类——否则永远凑不出「反复」。
    assert!(
        system.contains("时间不合法"),
        "教训要写清是什么错：{system}"
    );
}
