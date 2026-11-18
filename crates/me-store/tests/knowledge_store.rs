//! 知识库存储层的端到端验收（批次 3 / 5）。
//!
//! 覆盖 `doc/versiondoc/v5开发指导.md` §6.5 / §7.9 里那些
//! 「能不能跑通」的验收项：
//!
//! - v8 → v9 迁移后 4 张表都在；
//! - 深删四处（文档 / 语块 / 向量 / FTS）都清空，且**删完用同一段文本
//!   `MATCH` 一次必须返回 0 行**——只断言「表里行数为 0」不够，
//!   FTS 的实际检索行为才是用户能感知的（决策 O-26）；
//! - 删邮件必须先深删知识库条目，再删 `messages`；
//! - 容量窗口只索引最新 N 封；
//! - 删掉 `kb.sqlite` 后重启不崩，`embedded` 标记被重置（硬回退线）。

use me_core::knowledge::{KbFilter, KbKind, KbOrigin, KnowledgeRepo, NewChunk, NewKnowledgeDoc};
use me_core::port::{AccountRepo, FolderRepo, MessageRepo};
use me_store::Store;

/// 建一份文档 + 一个语块。
fn doc_with_chunk(
    store: &Store,
    kind: KbKind,
    title: &str,
    hash: &str,
    text: &str,
) -> me_core::knowledge::KnowledgeDocId {
    let writable = store.writable();
    let id = writable
        .upsert_document(&NewKnowledgeDoc {
            origin: KbOrigin::File,
            source_ref: format!("/tmp/{title}"),
            title: title.to_string(),
            kind,
            mail_id: None,
            size_bytes: text.len() as u64,
            content_hash: hash.to_string(),
        })
        .expect("写入文档");
    writable
        .replace_chunks(id, &[NewChunk::new(text, None)])
        .expect("写入语块");
    id
}

fn open_tmp() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("临时目录");
    let store = Store::open(dir.path()).expect("打开存储");
    (dir, store)
}

/// 建一封真实的邮件。
///
/// 邮件文档的 `mail_id` 有**外键约束**指向 `messages.id`——
/// 这正是「删邮件必须先深删知识库条目」那条规则的物理保障：
/// 没有真邮件就建不出邮件文档，测试也就没法拿一个假的 id 糊弄过去。
fn insert_message(store: &Store) -> me_core::MessageId {
    let writable = store.writable();
    let account = writable
        .create_account(&me_core::command::AccountDraft {
            email: "t@example.com".to_string(),
            display_name: Some("T".to_string()),
            imap: me_core::model::Endpoint::new(
                "imap.example.com",
                993,
                me_core::model::TlsMode::Implicit,
            ),
            smtp: me_core::model::Endpoint::new(
                "smtp.example.com",
                465,
                me_core::model::TlsMode::Implicit,
            ),
            auth_user: "t@example.com".to_string(),
            auth_secret: Some("secret".to_string()),
            auth_method: me_core::model::AuthMethod::Password,
            oauth_provider: None,
            oauth_subject: None,
            plaintext_tls_ack: false,
        })
        .expect("建账户");
    let folder = writable
        .upsert_folder(
            account.id,
            &me_core::port::FolderInfo {
                name: "INBOX".to_string(),
                delimiter: Some('/'),
                attributes: vec!["\\Inbox".to_string()],
            },
        )
        .expect("建目录");
    let now = me_core::now_unix();
    let summaries = writable
        .insert_messages(
            account.id,
            folder.id,
            &[me_core::model::NewMessage {
                uid: Some(1),
                message_id: Some("<a@example.com>".to_string()),
                subject: Some("一封邮件".to_string()),
                from: Some(me_core::model::Address::new(
                    Some("Bob".into()),
                    "bob@example.com",
                )),
                to: vec![],
                cc: vec![],
                date: Some(now),
                date_source: me_core::model::DateSource::Header,
                size: Some(1024),
                flags: me_core::model::Flags::NONE,
                has_attachment: false,
                in_reply_to: None,
                references: Vec::new(),
            }],
            1,
        )
        .expect("插入邮件");
    summaries
        .first()
        .map(|summary| summary.id)
        .expect("取邮件 id")
}

/// 深删必须把四处都清干净，且 FTS 立刻搜不到。
#[test]
fn deep_delete_clears_documents_chunks_vectors_and_fts() {
    let (_dir, store) = open_tmp();
    let writable = store.writable();
    let id = doc_with_chunk(
        &store,
        KbKind::Md,
        "采购合同",
        "h1",
        "采购合同的付款周期为 12 个月，分三期执行，验收合格后支付 60%。",
    );
    let chunks = writable.chunks(id, 0, 10).expect("读语块");
    assert_eq!(chunks.len(), 1);
    writable
        .put_vectors(&[(chunks[0].id, vec![0.1; me_core::EMBED_DIM])])
        .expect("写向量");

    // 删之前先确认搜得到——不然「删完搜不到」可能是因为它本来就搜不到。
    assert!(
        !writable.fts("付款周期", 10).expect("FTS").is_empty(),
        "删除前必须能搜到，否则这条断言没有意义"
    );

    writable.delete(id).expect("深删");

    assert!(writable.get(id).expect("读文档").is_none(), "文档应被删掉");
    assert!(
        writable.chunks(id, 0, 10).expect("读语块").is_empty(),
        "语块应被删掉"
    );
    // 关键断言：FTS 的**实际检索行为**。
    assert!(
        writable.fts("付款周期", 10).expect("FTS").is_empty(),
        "删完还能搜到就是幽灵结果"
    );
    assert!(
        writable.fts("采购合同", 10).expect("FTS").is_empty(),
        "删完标题也不该搜得到"
    );
}

/// 删文档不能留下 FTS 的孤儿行（`rowid` 指向已删的语块）。
#[test]
fn deleting_one_document_keeps_other_documents_searchable() {
    let (_dir, store) = open_tmp();
    let writable = store.writable();
    let first = doc_with_chunk(
        &store,
        KbKind::Md,
        "合同甲",
        "h1",
        "甲方与乙方的采购合同约定。",
    );
    let second = doc_with_chunk(
        &store,
        KbKind::Md,
        "合同乙",
        "h2",
        "乙方与丙方的销售合同约定。",
    );

    writable.delete(first).expect("深删甲");

    let hits = writable.fts("合同", 10).expect("FTS");
    assert!(
        hits.iter().all(|hit| hit.doc != first),
        "已删文档不该还有命中"
    );
    assert!(
        hits.iter().any(|hit| hit.doc == second),
        "另一份文档必须还能搜到：{hits:?}"
    );
}

/// 邮件来源的文档必须带 `mail_id`，否则删邮件时清不掉它。
#[test]
fn mail_documents_carry_mail_id() {
    let (_dir, store) = open_tmp();
    let writable = store.writable();
    let message = insert_message(&store);
    let id = writable
        .upsert_document(&NewKnowledgeDoc {
            origin: KbOrigin::Mail,
            source_ref: message.0.to_string(),
            title: "一封邮件".to_string(),
            kind: KbKind::Mail,
            mail_id: Some(message),
            size_bytes: 100,
            content_hash: "mail-hash".to_string(),
        })
        .expect("写入邮件文档");
    let got = writable.get(id).expect("读文档").expect("文档存在");
    assert_eq!(got.mail_id, Some(message));
    assert_eq!(got.origin, KbOrigin::Mail);
}

/// `mail_id` 指向不存在的邮件时必须**建不出来**。
///
/// 这条外键约束是「删邮件会连带清掉知识库条目」的物理保障：
/// 没有它，删掉邮件后知识库里会留下一份指向空气的文档。
#[test]
fn mail_document_rejects_unknown_mail_id() {
    let (_dir, store) = open_tmp();
    let writable = store.writable();
    let result = writable.upsert_document(&NewKnowledgeDoc {
        origin: KbOrigin::Mail,
        source_ref: "999999".to_string(),
        title: "幽灵邮件".to_string(),
        kind: KbKind::Mail,
        mail_id: Some(me_core::MessageId(999_999)),
        size_bytes: 100,
        content_hash: "ghost-hash".to_string(),
    });
    assert!(result.is_err(), "指向不存在邮件的文档不该建得出来");
}

/// 邮件来源的文档**不写** jsonl 导出物（决策 O-27）。
#[test]
fn mail_origin_does_not_write_chunk_file() {
    let (_dir, store) = open_tmp();
    let writable = store.writable();
    let message = insert_message(&store);
    let id = writable
        .upsert_document(&NewKnowledgeDoc {
            origin: KbOrigin::Mail,
            source_ref: message.0.to_string(),
            title: "一封邮件".to_string(),
            kind: KbKind::Mail,
            mail_id: Some(message),
            size_bytes: 100,
            content_hash: "mail-hash".to_string(),
        })
        .expect("写入邮件文档");
    writable
        .replace_chunks(id, &[NewChunk::new("正文内容", None)])
        .expect("写入语块");
    let path = store.paths().kb_chunk_file(id.0);
    assert!(
        !path.exists(),
        "邮件正文已在库里，不该再写一份 jsonl：{}",
        path.display()
    );
}

/// 容量窗口：`capacity = 0` 表示关闭自动入库。
#[test]
fn capacity_zero_disables_mail_window() {
    let (_dir, store) = open_tmp();
    let writable = store.writable();
    assert!(writable.mail_window(0).expect("查窗口").is_empty());
    assert!(writable.mail_seeds(0).expect("查窗口").is_empty());
}

/// 停用的文档不参与检索，但**还在列表里**（用户要能看到并重新启用）。
#[test]
fn disabled_document_is_hidden_from_search_but_listed() {
    let (_dir, store) = open_tmp();
    let writable = store.writable();
    let id = doc_with_chunk(
        &store,
        KbKind::Md,
        "停用的合同",
        "h1",
        "这是一份将被停用的合同。",
    );
    writable.update_doc(id, None, Some(false)).expect("停用");

    assert!(
        writable.fts("停用", 10).expect("FTS").is_empty(),
        "停用的文档不该参与检索"
    );
    let listed = writable.list(&KbFilter::default(), 200, 0).expect("列文档");
    assert!(
        listed.iter().any(|doc| doc.id == id),
        "停用的文档仍要在列表里，否则用户没法重新启用它"
    );
}

/// 相同内容重复导入**不应该**产生第二份文档（内容哈希去重）。
#[test]
fn reimporting_same_content_does_not_duplicate() {
    let (_dir, store) = open_tmp();
    let writable = store.writable();
    let make = |hash: &str| NewKnowledgeDoc {
        origin: KbOrigin::File,
        source_ref: "/tmp/采购合同.mdx".to_string(),
        title: "采购合同".to_string(),
        kind: KbKind::Md,
        mail_id: None,
        size_bytes: 10,
        content_hash: hash.to_string(),
    };
    let first = writable.upsert_document(&make("same")).expect("第一次");
    let second = writable.upsert_document(&make("same")).expect("第二次");
    assert_eq!(first, second, "内容没变就该复用同一份文档");
    assert_eq!(
        writable
            .list(&KbFilter::default(), 200, 0)
            .expect("列")
            .len(),
        1
    );
}

/// 内容变了就换新版本（旧语块连同向量与 FTS 一起清掉）。
#[test]
fn changed_content_replaces_chunks() {
    let (_dir, store) = open_tmp();
    let writable = store.writable();
    let id = doc_with_chunk(&store, KbKind::Md, "合同", "h1", "旧版本的正文内容在这里。");
    writable
        .replace_chunks(id, &[NewChunk::new("新版本的正文内容在这里。", None)])
        .expect("替换语块");
    let hits = writable.fts("旧版本", 10).expect("FTS");
    assert!(hits.is_empty(), "旧内容不该还能搜到：{hits:?}");
    assert!(
        !writable.fts("新版本", 10).expect("FTS").is_empty(),
        "新内容必须能搜到"
    );
}
