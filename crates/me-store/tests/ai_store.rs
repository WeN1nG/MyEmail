//! AI 仓储集成测试：真实 SQLite + v3 schema。
//!
//! 守护四件事：
//! 1. 会话/消息的键集分页与**时间正序**返回（UI 直接按返回顺序渲染）；
//! 2. 流式占位行的两条出口——正常落库与孤儿修复；
//! 3. `context_refs` / `tool_trace` 的 JSON 往返（含未知工具名降级）；
//! 4. 实体注册表的去重、别名合并、会话关联与 200 条上限。

use me_core::MessageId;
use me_core::ai::{
    AiContextMode, AiContextRef, AiConversationId, AiEntityKind, AiEntitySource, AiMessageId,
    AiMessageStatus, AiProviderConfig, AiProviderKind, AiRole, AiToolName, AiToolStatus,
    AiToolTrace, MAX_ENTITIES, MAX_MESSAGE_CHARS, MotiveSource, NewAiEntity, NewAiMessage,
};
use me_core::port::{AiMemoryRepo, AiRepo, CalendarRepo, ReadOnlyRepo};
use me_store::{Store, auto_title};
use serde_json::json;

fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");
    (dir, store)
}

fn config() -> AiProviderConfig {
    AiProviderConfig {
        kind: AiProviderKind::Anthropic,
        base_url: "https://api.anthropic.com".to_string(),
        model: "claude-sonnet-4-5".to_string(),
        max_output_tokens: 4096,
    }
}

fn context_ref(message: MessageId) -> AiContextRef {
    AiContextRef {
        message_id: message,
        subject: "周会纪要".to_string(),
        from: "a@example.com".to_string(),
        date: Some(1_700_000_000),
        mode: AiContextMode::FullBody,
        excerpt: "下周一上午十点开会".to_string(),
    }
}

fn user_message(conversation: AiConversationId, content: &str) -> NewAiMessage {
    NewAiMessage {
        conversation,
        role: AiRole::User,
        content: content.to_string(),
        context_refs: vec![context_ref(MessageId(7))],
        status: AiMessageStatus::Complete,
        created_at: 100,
    }
}

#[test]
fn conversation_crud_and_keyset_pagination() {
    let (_dir, store) = open();
    let writable = store.writable();

    let first = writable
        .create_conversation("第一个", Some("m1"), &config())
        .expect("create");
    assert_eq!(first.message_count, 0);
    assert_eq!(first.provider, AiProviderKind::Anthropic);

    let second = writable
        .create_conversation("第二个", Some("m1"), &config())
        .expect("create");
    writable.touch_conversation(second.id, None).expect("touch");

    let page = writable.list_conversations(None, 10).expect("list");
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].id, second.id, "按 updated_at 倒序");

    let next = writable
        .list_conversations(Some(page[0].id), 10)
        .expect("list");
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].id, first.id);

    writable
        .rename_conversation(first.id, "改名后")
        .expect("rename");
    assert_eq!(
        writable
            .get_conversation(first.id)
            .expect("get")
            .expect("存在")
            .title,
        "改名后"
    );

    writable.delete_conversation(second.id).expect("delete");
    assert_eq!(
        writable.list_conversations(None, 10).expect("list").len(),
        1
    );
}

#[test]
fn empty_title_is_rejected() {
    let (_dir, store) = open();
    let writable = store.writable();
    assert!(
        writable
            .create_conversation("   ", Some("m1"), &config())
            .is_err()
    );
}

#[test]
fn messages_keep_chronological_order_and_round_trip_json() {
    let (_dir, store) = open();
    let writable = store.writable();
    let conversation = writable
        .create_conversation("测试", Some("m1"), &config())
        .expect("create");

    let user = writable
        .append_message(&user_message(conversation.id, "这封邮件说什么"))
        .expect("append");
    assert_eq!(user.context_refs.len(), 1);
    assert_eq!(user.context_refs[0].message_id, MessageId(7));

    let assistant_id = writable
        .begin_assistant_message(conversation.id)
        .expect("begin");

    let trace = AiToolTrace {
        call_id: "call_1".to_string(),
        tool: AiToolName::EmailSearch,
        status: AiToolStatus::Ok,
        summary: "检索到 3 封".to_string(),
        duration_ms: 42,
        motive: Some("用户要查上周的报价单".to_string()),
        motive_source: MotiveSource::Model,
        step_id: None,
        attempt: None,
        cards: Vec::new(),
    };
    writable
        .finish_assistant_message(
            assistant_id,
            "这封邮件在说下周开会。",
            AiMessageStatus::Complete,
            Some((Some(120), Some(40))),
            std::slice::from_ref(&trace),
            None,
            None,
            None,
        )
        .expect("finish");

    let messages = writable
        .list_messages(conversation.id, None, 10)
        .expect("list");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, AiRole::User);
    assert_eq!(messages[1].role, AiRole::Assistant);
    assert_eq!(messages[0].content, "这封邮件说什么");
    assert_eq!(messages[1].content, "这封邮件在说下周开会。");
    assert_eq!(messages[1].input_tokens, Some(120));
    assert_eq!(messages[1].output_tokens, Some(40));
    assert_eq!(messages[1].tool_trace.len(), 1);
    assert_eq!(messages[1].tool_trace[0].tool, AiToolName::EmailSearch);
    assert_eq!(messages[1].status, AiMessageStatus::Complete);

    // 会话列表的消息数会跟着变。
    let listed = writable.list_conversations(None, 10).expect("list");
    assert_eq!(listed[0].message_count, 2);
}

#[test]
fn streaming_placeholder_is_repaired_on_startup() {
    let (_dir, store) = open();
    let writable = store.writable();
    let conversation = writable
        .create_conversation("崩溃恢复", Some("m1"), &config())
        .expect("create");
    writable
        .append_message(&user_message(conversation.id, "在吗"))
        .expect("append");
    let pending = writable
        .begin_assistant_message(conversation.id)
        .expect("begin");

    // 模拟应用异常退出：行还停在 streaming。
    assert_eq!(writable.fail_orphan_streaming_messages().expect("fix"), 1);
    let messages = writable
        .list_messages(conversation.id, None, 10)
        .expect("list");
    assert_eq!(messages[1].id, pending);
    assert_eq!(messages[1].status, AiMessageStatus::Failed);
    assert!(messages[1].error.as_deref().unwrap_or("").contains("中断"));
    // 再跑一次不再改动。
    assert_eq!(writable.fail_orphan_streaming_messages().expect("fix"), 0);
}

#[test]
fn oversized_reply_is_truncated_and_marked_failed() {
    let (_dir, store) = open();
    let writable = store.writable();
    let conversation = writable
        .create_conversation("超长", Some("m1"), &config())
        .expect("create");
    let id = writable
        .begin_assistant_message(conversation.id)
        .expect("begin");
    let huge = "字".repeat(MAX_MESSAGE_CHARS + 100);
    writable
        .finish_assistant_message(
            id,
            &huge,
            AiMessageStatus::Complete,
            None,
            &[],
            None,
            None,
            None,
        )
        .expect("finish");

    let message = store
        .read_only()
        .get_ai_message_readonly(id)
        .expect("get")
        .expect("存在");
    assert_eq!(message.status, AiMessageStatus::Failed);
    assert!(message.content.chars().count() <= MAX_MESSAGE_CHARS + 1);
    assert!(message.error.is_some());
}

#[test]
fn unknown_tool_names_in_trace_are_dropped_not_fatal() {
    let (_dir, store) = open();
    let writable = store.writable();
    let conversation = writable
        .create_conversation("降级", Some("m1"), &config())
        .expect("create");
    let id = writable
        .begin_assistant_message(conversation.id)
        .expect("begin");
    let trace = AiToolTrace {
        call_id: "c1".to_string(),
        tool: AiToolName::WeatherGet,
        status: AiToolStatus::Error,
        summary: "超时".to_string(),
        duration_ms: 20000,
        motive: None,
        motive_source: MotiveSource::Default,
        step_id: None,
        attempt: None,
        cards: Vec::new(),
    };
    writable
        .finish_assistant_message(
            id,
            "",
            AiMessageStatus::Failed,
            None,
            &[trace],
            None,
            None,
            Some("失败"),
        )
        .expect("finish");

    // 直接改库模拟「未来版本写入了新工具名」的历史数据：读取时必须降级而不是炸掉。
    let message = store
        .read_only()
        .get_ai_message_readonly(id)
        .expect("get")
        .unwrap();
    assert_eq!(message.tool_trace.len(), 1);
    assert_eq!(message.status, AiMessageStatus::Failed);
}

#[test]
fn deleting_conversation_cascades_messages_only() {
    let (_dir, store) = open();
    let writable = store.writable();
    let conversation = writable
        .create_conversation("删除", Some("m1"), &config())
        .expect("create");
    writable
        .append_message(&user_message(conversation.id, "你好"))
        .expect("append");
    let entity = writable
        .upsert_entity(&NewAiEntity {
            kind: AiEntityKind::Person,
            canonical_name: "张三".to_string(),
            aliases: vec!["老张".to_string()],
            email: Some("zhang@example.com".to_string()),
            account: None,
            calendar: None,
            attributes: json!({}),
            source: AiEntitySource::User,
            confidence: 1.0,
        })
        .expect("upsert");
    writable
        .link_conversation_entity(conversation.id, entity.id, 10)
        .expect("link");

    writable
        .delete_conversation(conversation.id)
        .expect("delete");

    assert!(
        writable
            .list_messages(conversation.id, None, 10)
            .expect("list")
            .is_empty()
    );
    assert_eq!(
        writable.list_entities(10).expect("entities").len(),
        1,
        "删除会话不得删掉全局实体"
    );
    assert!(
        writable
            .list_conversation_entities(conversation.id, 10)
            .expect("links")
            .is_empty()
    );
}

#[test]
fn entity_upsert_dedupes_by_name_and_merges_aliases() {
    let (_dir, store) = open();
    let writable = store.writable();
    let base = NewAiEntity {
        kind: AiEntityKind::Person,
        canonical_name: "张三".to_string(),
        aliases: vec!["老张".to_string()],
        email: None,
        account: None,
        calendar: None,
        attributes: json!({"team": "研发"}),
        source: AiEntitySource::User,
        confidence: 1.0,
    };
    let first = writable.upsert_entity(&base).expect("insert");
    assert_eq!(first.canonical_name, "张三");
    assert_eq!(first.aliases, vec!["老张".to_string()]);

    let again = writable
        .upsert_entity(&NewAiEntity {
            aliases: vec!["张工".to_string(), "老张".to_string()],
            ..base.clone()
        })
        .expect("merge");
    assert_eq!(again.id, first.id, "同名实体必须复用同一行");
    assert_eq!(again.aliases.len(), 2, "别名去重后应为两个");

    // 按别名能查到。
    let found = writable.find_entities_by_alias("张工", 5).expect("find");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, first.id);

    // 邮箱跨类型唯一。
    let by_email = writable
        .upsert_entity(&NewAiEntity {
            kind: AiEntityKind::Organization,
            canonical_name: "某某公司".to_string(),
            aliases: Vec::new(),
            email: Some("ZHANG@example.com".to_string()),
            account: None,
            calendar: None,
            attributes: json!({}),
            source: AiEntitySource::AiInferred,
            confidence: 0.8,
        })
        .expect("insert email");
    let merged = writable
        .upsert_entity(&NewAiEntity {
            kind: AiEntityKind::Person,
            canonical_name: "张三".to_string(),
            aliases: Vec::new(),
            email: Some("zhang@example.com".to_string()),
            account: None,
            calendar: None,
            attributes: json!({}),
            source: AiEntitySource::User,
            confidence: 1.0,
        })
        .expect("dedupe by email");
    assert_eq!(merged.id, by_email.id, "邮箱命中时优先按邮箱归并");
}

#[test]
fn entity_links_track_recent_use() {
    let (_dir, store) = open();
    let writable = store.writable();
    let conversation = writable
        .create_conversation("实体", Some("m1"), &config())
        .expect("create");
    let entity = writable
        .upsert_entity(&NewAiEntity {
            kind: AiEntityKind::Location,
            canonical_name: "上海".to_string(),
            aliases: Vec::new(),
            email: None,
            account: None,
            calendar: None,
            attributes: json!({}),
            source: AiEntitySource::Tool,
            confidence: 1.0,
        })
        .expect("upsert");

    writable
        .link_conversation_entity(conversation.id, entity.id, 100)
        .expect("link");
    writable
        .link_conversation_entity(conversation.id, entity.id, 200)
        .expect("link again");

    let linked = writable
        .list_conversation_entities(conversation.id, 10)
        .expect("list");
    assert_eq!(linked.len(), 1);
    assert_eq!(linked[0].last_seen_at, entity.last_seen_at);
}

#[test]
fn entity_cap_is_enforced() {
    let (_dir, store) = open();
    let writable = store.writable();
    for index in 0..MAX_ENTITIES {
        writable
            .upsert_entity(&NewAiEntity {
                kind: AiEntityKind::Other,
                canonical_name: format!("实体{index}"),
                aliases: Vec::new(),
                email: None,
                account: None,
                calendar: None,
                attributes: json!({}),
                source: AiEntitySource::AiInferred,
                confidence: 1.0,
            })
            .expect("insert");
    }
    assert_eq!(writable.count_entities().expect("count"), MAX_ENTITIES);

    let err = writable
        .upsert_entity(&NewAiEntity {
            kind: AiEntityKind::Other,
            canonical_name: "溢出".to_string(),
            aliases: Vec::new(),
            email: None,
            account: None,
            calendar: None,
            attributes: json!({}),
            source: AiEntitySource::AiInferred,
            confidence: 1.0,
        })
        .unwrap_err();
    assert!(format!("{err}").contains("记忆已满"));

    // 已有实体仍可更新（上限只拦新增）。
    writable
        .upsert_entity(&NewAiEntity {
            kind: AiEntityKind::Other,
            canonical_name: "实体0".to_string(),
            aliases: Vec::new(),
            email: None,
            account: None,
            calendar: None,
            attributes: json!({}),
            source: AiEntitySource::AiInferred,
            confidence: 1.0,
        })
        .expect("update existing");
}

#[test]
fn entity_validation_rejects_long_attributes() {
    let (_dir, store) = open();
    let writable = store.writable();
    let err = writable
        .upsert_entity(&NewAiEntity {
            kind: AiEntityKind::Other,
            canonical_name: "x".to_string(),
            aliases: Vec::new(),
            email: None,
            account: None,
            calendar: None,
            attributes: json!({"note": "x".repeat(2000)}),
            source: AiEntitySource::User,
            confidence: 1.0,
        })
        .unwrap_err();
    assert!(format!("{err}").contains("属性过大"));
}

#[test]
fn entity_carries_calendar_links() {
    let (_dir, store) = open();
    let writable = store.writable();
    let calendar = writable.ensure_default_calendar().expect("calendar");
    let entity = writable
        .upsert_entity(&NewAiEntity {
            kind: AiEntityKind::Event,
            canonical_name: "周会".to_string(),
            aliases: Vec::new(),
            email: None,
            // 账户 id 有外键约束；这里只验证日历关联（账户关联由工具登记时填）。
            account: None,
            calendar: Some(calendar.id),
            attributes: json!({}),
            source: AiEntitySource::Tool,
            confidence: 1.0,
        })
        .expect("upsert");
    assert_eq!(entity.calendar, Some(calendar.id));
    assert_eq!(entity.account, None);
}

#[test]
fn readonly_handle_sees_the_same_data() {
    let (_dir, store) = open();
    let writable = store.writable();
    let conversation = writable
        .create_conversation("只读", Some("m1"), &config())
        .expect("create");
    writable
        .append_message(&user_message(conversation.id, "同步可见"))
        .expect("append");
    writable
        .upsert_entity(&NewAiEntity {
            kind: AiEntityKind::Other,
            canonical_name: "只读实体".to_string(),
            aliases: Vec::new(),
            email: None,
            account: None,
            calendar: None,
            attributes: json!({}),
            source: AiEntitySource::User,
            confidence: 1.0,
        })
        .expect("upsert");

    let readonly = store.read_only();
    let conversations = readonly
        .list_ai_conversations_readonly(None, 10)
        .expect("list");
    assert_eq!(conversations.len(), 1);
    let messages = readonly
        .list_ai_messages_readonly(conversation.id, None, 10)
        .expect("list");
    assert_eq!(messages.len(), 1);
    assert_eq!(
        readonly
            .get_ai_message_readonly(messages[0].id)
            .expect("get")
            .unwrap()
            .content,
        "同步可见"
    );
    assert_eq!(
        readonly.list_ai_entities_readonly(10).expect("list").len(),
        1
    );
    assert_eq!(readonly.count_ai_entities_readonly().expect("count"), 1);
}

#[test]
fn auto_title_flattens_newlines_and_caps_length() {
    assert_eq!(auto_title("第一行\n第二行"), "第一行 第二行");
    assert_eq!(auto_title("   "), "新对话");
    let long = "很长的标题".repeat(20);
    assert!(auto_title(&long).chars().count() <= 31);
}

#[test]
fn message_pagination_walks_backwards() {
    let (_dir, store) = open();
    let writable = store.writable();
    let conversation = writable
        .create_conversation("分页", Some("m1"), &config())
        .expect("create");
    for index in 0..5 {
        writable
            .append_message(&NewAiMessage {
                conversation: conversation.id,
                role: AiRole::User,
                content: format!("第{index}条"),
                context_refs: Vec::new(),
                status: AiMessageStatus::Complete,
                created_at: index,
            })
            .expect("append");
    }
    let newest = writable
        .list_messages(conversation.id, None, 2)
        .expect("page");
    assert_eq!(newest.len(), 2);
    assert_eq!(newest[0].content, "第3条");
    assert_eq!(newest[1].content, "第4条");

    let older = writable
        .list_messages(conversation.id, Some(AiMessageId(newest[0].id.0)), 2)
        .expect("page");
    assert_eq!(older[0].content, "第1条");
    assert_eq!(older[1].content, "第2条");
}

#[test]
fn conversation_keeps_model_binding_across_rename() {
    let (_dir, store) = open();
    let writable = store.writable();
    let conversation = writable
        .create_conversation("绑定模型", Some("m2"), &config())
        .expect("create");
    assert_eq!(conversation.model_id.as_deref(), Some("m2"));

    // 改名不该动绑定：用户改标题不代表换模型。
    writable
        .rename_conversation(conversation.id, "换个名字")
        .expect("rename");
    let reloaded = writable
        .list_conversations(None, 10)
        .expect("list")
        .into_iter()
        .find(|item| item.id == conversation.id)
        .expect("存在");
    assert_eq!(reloaded.title, "换个名字");
    assert_eq!(reloaded.model_id.as_deref(), Some("m2"));
}

#[test]
fn switching_conversation_model_refreshes_snapshot_columns() {
    let (_dir, store) = open();
    let writable = store.writable();
    let conversation = writable
        .create_conversation("切换模型", Some("m1"), &config())
        .expect("create");

    let mut local = config();
    local.kind = AiProviderKind::OpenAiCompatible;
    local.base_url = "http://127.0.0.1:11434/v1".to_string();
    local.model = "qwen-plus".to_string();
    writable
        .set_conversation_model(conversation.id, "m1", &local)
        .expect("switch");

    let reloaded = writable
        .list_conversations(None, 10)
        .expect("list")
        .into_iter()
        .find(|item| item.id == conversation.id)
        .expect("存在");
    assert_eq!(reloaded.model_id.as_deref(), Some("m1"));
    // 快照也必须跟着换：否则历史消息旁的标注会指向旧模型。
    assert_eq!(reloaded.provider, AiProviderKind::OpenAiCompatible);
    assert_eq!(reloaded.model, "qwen-plus");

    // 切到不存在的会话要报错，而不是静默成功。
    assert!(
        writable
            .set_conversation_model(AiConversationId(9999), "m1", &local)
            .is_err()
    );
}

/// 清单要能原样往返：界面上那几条进度全是从这里读出来的。
#[test]
fn plan_round_trips_through_the_message_row() {
    use me_core::ai_plan::{Capability, Plan, PlanStatus, PlanStep};

    let (_dir, store) = open();
    let writable = store.writable();
    let conversation = writable
        .create_conversation("清单", Some("m1"), &config())
        .expect("create");
    let id = writable
        .begin_assistant_message(conversation.id)
        .expect("begin");

    let plan = Plan {
        id: "p_1760000000".to_string(),
        goal: "找出本周报价单并确认周五会议".to_string(),
        steps: vec![
            PlanStep::new(1, "检索报价单", Capability::Mail, PlanStatus::Done)
                .with_evidence("命中 2 封"),
            PlanStep::new(2, "查周五 15:00", Capability::Calendar, PlanStatus::Pending),
        ],
        created_at: 1_760_000_000,
        updated_at: 1_760_000_010,
        ..Plan::default()
    };
    writable
        .finish_assistant_message(
            id,
            "正在做。",
            AiMessageStatus::Complete,
            None,
            &[],
            Some(&plan),
            None,
            None,
        )
        .expect("finish");

    let loaded = store
        .read_only()
        .get_ai_message_readonly(id)
        .expect("get")
        .expect("存在");
    assert_eq!(loaded.plan.as_ref(), Some(&plan), "清单要逐字段一致");
    assert_eq!(loaded.plan.expect("有清单").open_steps(), 1);
}

/// 没有清单的消息读出来必须是 `None`，不能凭空造一个空清单（界面会画出空面板）。
#[test]
fn message_without_plan_reads_as_none() {
    let (_dir, store) = open();
    let writable = store.writable();
    let conversation = writable
        .create_conversation("无清单", Some("m1"), &config())
        .expect("create");
    let id = writable
        .begin_assistant_message(conversation.id)
        .expect("begin");
    writable
        .finish_assistant_message(
            id,
            "你好",
            AiMessageStatus::Complete,
            None,
            &[],
            None,
            None,
            None,
        )
        .expect("finish");

    let loaded = store
        .read_only()
        .get_ai_message_readonly(id)
        .expect("get")
        .expect("存在");
    assert!(loaded.plan.is_none());
}

// ---------------------------------------------------------------------------
// v8：分段思考与工具卡片
// ---------------------------------------------------------------------------

use me_core::ai::{MailCardRef, ThinkingBlock, ThinkingSegment};

fn blank_thinking_block() -> ThinkingBlock {
    ThinkingBlock::default()
}

fn sample_thinking() -> ThinkingBlock {
    ThinkingBlock {
        segments: vec![
            ThinkingSegment {
                index: 1,
                text: "先想清楚要查什么".to_string(),
                followed_by_tool: true,
            },
            ThinkingSegment {
                index: 2,
                text: "结果回来了，可以收尾".to_string(),
                followed_by_tool: false,
            },
        ],
        truncated: true,
    }
}

/// 思考块（含 `truncated` 标记）要能完整往返，段序不能变。
#[test]
fn thinking_round_trips_with_truncated_flag() {
    let (_dir, store) = open();
    let writable = store.writable();
    let conversation = writable
        .create_conversation("思考往返", Some("m1"), &config())
        .expect("create");
    let id = writable
        .begin_assistant_message(conversation.id)
        .expect("begin");
    let thinking = sample_thinking();
    writable
        .finish_assistant_message(
            id,
            "答案",
            AiMessageStatus::Complete,
            None,
            &[],
            None,
            Some(&thinking),
            None,
        )
        .expect("finish");

    let loaded = store
        .read_only()
        .get_ai_message_readonly(id)
        .expect("get")
        .expect("存在");
    assert_eq!(loaded.thinking, Some(thinking));
    assert_eq!(loaded.content, "答案", "思考不能污染正文");
}

/// 老库没有 thinking 列 → NULL → `None`，不能报错。
#[test]
fn old_message_without_thinking_reads_none() {
    let (_dir, store) = open();
    let writable = store.writable();
    let conversation = writable
        .create_conversation("老消息", Some("m1"), &config())
        .expect("create");
    let id = writable
        .begin_assistant_message(conversation.id)
        .expect("begin");
    writable
        .finish_assistant_message(
            id,
            "你好",
            AiMessageStatus::Complete,
            None,
            &[],
            None,
            None,
            None,
        )
        .expect("finish");
    let loaded = store
        .read_only()
        .get_ai_message_readonly(id)
        .expect("get")
        .expect("存在");
    assert_eq!(loaded.thinking, None);
}

/// `Some(空块)` 与 `None` 是两种状态：前者说明服务商返回过思考但内容为空，
/// 后者是压根没这功能。parse_thinking 不能把前者 filter 掉。
#[test]
fn empty_thinking_block_is_distinguishable_from_none() {
    let (_dir, store) = open();
    let writable = store.writable();
    let conversation = writable
        .create_conversation("空思考", Some("m1"), &config())
        .expect("create");
    let id = writable
        .begin_assistant_message(conversation.id)
        .expect("begin");
    writable
        .finish_assistant_message(
            id,
            "答案",
            AiMessageStatus::Complete,
            None,
            &[],
            None,
            Some(&blank_thinking_block()),
            None,
        )
        .expect("finish");
    let loaded = store
        .read_only()
        .get_ai_message_readonly(id)
        .expect("get")
        .expect("存在");
    assert_eq!(loaded.thinking, Some(ThinkingBlock::default()));
}

/// 卡片随 trace 一起落库、一起读回：历史回看时才显示得出来。
#[test]
fn tool_trace_cards_round_trip() {
    let (_dir, store) = open();
    let writable = store.writable();
    let conversation = writable
        .create_conversation("卡片往返", Some("m1"), &config())
        .expect("create");
    let id = writable
        .begin_assistant_message(conversation.id)
        .expect("begin");
    let trace = AiToolTrace {
        call_id: "c1".to_string(),
        tool: AiToolName::EmailSearch,
        status: AiToolStatus::Ok,
        summary: "检索到 2 封".to_string(),
        duration_ms: 12,
        motive: None,
        motive_source: MotiveSource::Default,
        step_id: Some(2),
        attempt: Some(1),
        cards: vec![
            MailCardRef {
                message: MessageId(11),
                reason: "已星标".to_string(),
            },
            MailCardRef {
                message: MessageId(12),
                reason: "近 7 天未读".to_string(),
            },
        ],
    };
    writable
        .finish_assistant_message(
            id,
            "找到两封",
            AiMessageStatus::Complete,
            None,
            std::slice::from_ref(&trace),
            None,
            None,
            None,
        )
        .expect("finish");

    let loaded = store
        .read_only()
        .get_ai_message_readonly(id)
        .expect("get")
        .expect("存在");
    assert_eq!(loaded.tool_trace.len(), 1);
    assert_eq!(loaded.tool_trace[0].cards.len(), 2);
    assert_eq!(loaded.tool_trace[0].cards[0].reason, "已星标");
    assert_eq!(loaded.tool_trace[0].step_id, Some(2));
}

/// 老 JSON 没有 cards 键 → 空数组，不能整条 trace 被丢掉。
#[test]
fn old_tool_trace_without_cards_reads_empty() {
    let (_dir, store) = open();
    let writable = store.writable();
    let conversation = writable
        .create_conversation("老 trace", Some("m1"), &config())
        .expect("create");
    let id = writable
        .begin_assistant_message(conversation.id)
        .expect("begin");
    writable
        .finish_assistant_message(
            id,
            "答案",
            AiMessageStatus::Complete,
            None,
            &[],
            None,
            None,
            None,
        )
        .expect("finish");
    // 手工写一段没有 cards 键的老 JSON（绕开领域类型）。
    let conn = rusqlite::Connection::open(store.paths().db_file()).expect("open db");
    conn.execute(
        "UPDATE ai_messages SET tool_trace = ?2 WHERE id = ?1",
        rusqlite::params![
            id.0,
            r#"[{"call_id":"old","tool":"email_search","status":"ok","summary":"命中 1 封","duration_ms":3}]"#
        ],
    )
    .expect("写老 JSON");
    drop(conn);
    let loaded = store
        .read_only()
        .get_ai_message_readonly(id)
        .expect("get")
        .expect("存在");
    assert_eq!(loaded.tool_trace.len(), 1, "老 trace 不能被丢弃");
    assert!(loaded.tool_trace[0].cards.is_empty());
}

/// 检查点：崩溃后内容要留下，迟到的检查点不能覆盖已收尾的消息。
#[test]
fn checkpoint_preserves_content_across_orphan_recovery() {
    let (_dir, store) = open();
    let writable = store.writable();
    let conversation = writable
        .create_conversation("检查点", Some("m1"), &config())
        .expect("create");
    let id = writable
        .begin_assistant_message(conversation.id)
        .expect("begin");
    let thinking = sample_thinking();
    writable
        .checkpoint_assistant_message(id, "写到一半的内容", &[], None, Some(&thinking))
        .expect("checkpoint");

    // 模拟崩溃：孤儿收尾只改 status / error，内容必须原样保留。
    assert_eq!(
        writable.fail_orphan_streaming_messages().expect("orphan"),
        1
    );
    let loaded = store
        .read_only()
        .get_ai_message_readonly(id)
        .expect("get")
        .expect("存在");
    assert_eq!(loaded.status, AiMessageStatus::Failed);
    assert_eq!(loaded.content, "写到一半的内容");
    assert_eq!(loaded.thinking, Some(thinking));
}

/// 已经正常收尾的消息不能被迟到的检查点改写。
#[test]
fn checkpoint_does_not_overwrite_a_finished_message() {
    let (_dir, store) = open();
    let writable = store.writable();
    let conversation = writable
        .create_conversation("迟到检查点", Some("m1"), &config())
        .expect("create");
    let id = writable
        .begin_assistant_message(conversation.id)
        .expect("begin");
    writable
        .finish_assistant_message(
            id,
            "正式答案",
            AiMessageStatus::Complete,
            None,
            &[],
            None,
            None,
            None,
        )
        .expect("finish");
    writable
        .checkpoint_assistant_message(id, "迟到的增量", &[], None, None)
        .expect("checkpoint 应静默无操作");
    let loaded = store
        .read_only()
        .get_ai_message_readonly(id)
        .expect("get")
        .expect("存在");
    assert_eq!(loaded.content, "正式答案");
    assert_eq!(loaded.status, AiMessageStatus::Complete);
}
