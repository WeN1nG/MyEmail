//! 统一收件箱的聚合查询测试。
//!
//! 验证：
//! - 聚合范围是各账户 `role='inbox'` 文件夹的**并集**；
//! - **不含**非收件箱文件夹（已发送 / 草稿 / 废件箱…）；
//! - **排除 `uid IS NULL`** 的行（本地已移动、服务器身份待确认）；
//! - 跨账户按 `date DESC, id DESC` 排序；
//! - 键集分页不重复、不跳行；
//! - 未读计数用**同一个** `flags & 1` 位。

use me_core::model::{AccountId, FolderId, FolderRole, MessageId};
use me_core::port::{MessageRepo, ReadOnlyRepo};
use me_store::Store;
use rusqlite::params;

/// 建一个带两个账户、每个账户若干文件夹的库。
fn seed(dir: &std::path::Path) {
    drop(Store::open(dir).expect("open"));
    let conn = rusqlite::Connection::open(dir.join("myemail.db")).expect("open");
    conn.execute_batch(
        "INSERT INTO accounts(email, imap_host, imap_port, imap_tls, smtp_host, smtp_port, smtp_tls,
                             auth_user, created_at)
         VALUES ('a@example.com', 'h', 993, 'implicit', 's', 465, 'implicit', 'a@example.com', 1),
                ('b@example.com', 'h', 993, 'implicit', 's', 465, 'implicit', 'b@example.com', 1);
         INSERT INTO folders(account_id, name, role)
         SELECT id, 'INBOX', 'inbox' FROM accounts;
         INSERT INTO folders(account_id, name, role)
         SELECT id, 'Sent', 'sent' FROM accounts;
         INSERT INTO folders(account_id, name, role)
         SELECT id, 'Trash', 'trash' FROM accounts;",
    )
    .expect("seed folders");
}

/// 按账户与角色查 folder id——**不要硬编码 id**。
///
/// `INSERT ... SELECT` 的插入顺序按 SELECT 的结果走，
/// 硬编码 id 会随 schema 变化而错位（第一版就踩了这个坑）。
fn folder_id_of(conn: &rusqlite::Connection, account: i64, role: &str) -> i64 {
    conn.query_row(
        "SELECT id FROM folders WHERE account_id = ?1 AND role = ?2 ORDER BY id LIMIT 1",
        params![account, role],
        |row| row.get(0),
    )
    .expect("query folder id")
}

/// 插一封邮件。`uid` 为 `None` 表示「本地已移动、服务器身份待确认」。
fn insert_message(
    conn: &rusqlite::Connection,
    account: i64,
    folder: i64,
    uid: Option<i64>,
    subject: &str,
    date: i64,
    flags: i64,
) {
    conn.execute(
        "INSERT INTO messages(account_id, folder_id, uid, subject, date, date_source, flags)
         VALUES (?1, ?2, ?3, ?4, ?5, 'header', ?6)",
        params![account, folder, uid, subject, date, flags],
    )
    .expect("insert message");
}

#[test]
fn unified_inbox_spans_all_accounts() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed(dir.path());
    let conn = rusqlite::Connection::open(dir.path().join("myemail.db")).expect("open");
    let store = Store::open(dir.path()).expect("reopen");

    let inbox_a = folder_id_of(&conn, 1, "inbox");
    let inbox_b = folder_id_of(&conn, 2, "inbox");
    insert_message(&conn, 1, inbox_a, Some(1), "来自账户一", 100, 0);
    insert_message(&conn, 2, inbox_b, Some(1), "来自账户二", 200, 0);

    let list = store
        .read_only()
        .list_unified_inbox_readonly(None, 200)
        .expect("list");
    assert_eq!(list.len(), 2, "两个账户的收件箱都该出现");
    // 按时间倒序。
    assert_eq!(list[0].subject.as_deref(), Some("来自账户二"));
    assert_eq!(list[1].subject.as_deref(), Some("来自账户一"));
}

#[test]
fn unified_inbox_excludes_non_inbox_folders() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed(dir.path());
    let conn = rusqlite::Connection::open(dir.path().join("myemail.db")).expect("open");
    let store = Store::open(dir.path()).expect("reopen");

    insert_message(
        &conn,
        1,
        folder_id_of(&conn, 1, "inbox"),
        Some(1),
        "收件箱里的",
        100,
        0,
    );
    insert_message(
        &conn,
        1,
        folder_id_of(&conn, 1, "sent"),
        Some(2),
        "已发送里的",
        300,
        0,
    );
    insert_message(
        &conn,
        1,
        folder_id_of(&conn, 1, "trash"),
        Some(3),
        "废件箱里的",
        400,
        0,
    );

    let list = store
        .read_only()
        .list_unified_inbox_readonly(None, 200)
        .expect("list");
    assert_eq!(list.len(), 1, "只有收件箱该出现");
    assert_eq!(list[0].subject.as_deref(), Some("收件箱里的"));
}

#[test]
fn unified_inbox_excludes_rows_without_server_identity() {
    // `uid IS NULL` = 本地已移动、服务器身份待确认——
    // 它在服务器上还没有位置，不该出现在聚合视图里。
    let dir = tempfile::tempdir().expect("tempdir");
    seed(dir.path());
    let conn = rusqlite::Connection::open(dir.path().join("myemail.db")).expect("open");
    let store = Store::open(dir.path()).expect("reopen");

    let inbox = folder_id_of(&conn, 1, "inbox");
    insert_message(&conn, 1, inbox, Some(1), "有服务器身份", 100, 0);
    insert_message(&conn, 1, inbox, None, "本地已移动", 200, 0);

    let list = store
        .read_only()
        .list_unified_inbox_readonly(None, 200)
        .expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].subject.as_deref(), Some("有服务器身份"));
}

#[test]
fn unified_inbox_orders_by_date_then_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed(dir.path());
    let conn = rusqlite::Connection::open(dir.path().join("myemail.db")).expect("open");
    let store = Store::open(dir.path()).expect("reopen");

    let inbox_a = folder_id_of(&conn, 1, "inbox");
    let inbox_b = folder_id_of(&conn, 2, "inbox");
    insert_message(&conn, 1, inbox_a, Some(1), "较早", 100, 0);
    insert_message(&conn, 2, inbox_b, Some(1), "较晚", 200, 0);
    // 同一时刻的两封——靠 `id DESC` 稳定排序（后插入的在前）。
    insert_message(&conn, 1, inbox_a, Some(2), "同时甲", 300, 0);
    insert_message(&conn, 2, inbox_b, Some(2), "同时乙", 300, 0);

    let list = store
        .read_only()
        .list_unified_inbox_readonly(None, 200)
        .expect("list");
    assert_eq!(list.len(), 4);
    assert_eq!(list[0].date, Some(300));
    assert_eq!(list[1].date, Some(300));
    // 同时刻的两封：id 大的在前。
    assert!(list[0].id > list[1].id, "同日期应按 id DESC");
    assert_eq!(list[2].date, Some(200));
    assert_eq!(list[3].date, Some(100));
}

#[test]
fn unified_inbox_keyset_pagination_does_not_repeat_or_skip() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed(dir.path());
    let conn = rusqlite::Connection::open(dir.path().join("myemail.db")).expect("open");
    let store = Store::open(dir.path()).expect("reopen");

    let inbox = folder_id_of(&conn, 1, "inbox");
    for i in 0..10 {
        insert_message(
            &conn,
            1,
            inbox,
            Some(i + 1),
            &format!("邮件{i}"),
            1000 + i,
            0,
        );
    }

    let first = store
        .read_only()
        .list_unified_inbox_readonly(None, 4)
        .expect("first");
    assert_eq!(first.len(), 4);

    // 逐页翻到底：不重复、不跳行、顺序严格递减。
    let mut all: Vec<(i64, i64)> = Vec::new();
    let mut cursor = None;
    loop {
        let page = store
            .read_only()
            .list_unified_inbox_readonly(cursor, 4)
            .expect("page");
        if page.is_empty() {
            break;
        }
        assert!(page.len() <= 4, "每页不得超过 limit");
        for message in &page {
            all.push((message.date.unwrap_or(0), message.id.0));
        }
        cursor = page.last().map(|m| (m.date.unwrap_or(0), m.id.0));
        assert!(all.len() <= 10, "翻页不该多出重复行");
    }

    assert_eq!(all.len(), 10, "十封邮件应被完整翻出来，不跳行");
    let unique: std::collections::HashSet<i64> = all.iter().map(|(_, id)| *id).collect();
    assert_eq!(unique.len(), 10, "分页不该重复");

    // 顺序严格递减（`date DESC, id DESC`）。
    for pair in all.windows(2) {
        assert!(
            pair[0] > pair[1],
            "跨页排序被破坏：{:?} 之后出现 {:?}",
            pair[0],
            pair[1]
        );
    }
}

#[test]
fn unified_unread_count_uses_the_seen_bit() {
    // `flags & 1` 是 `\Seen` 位——`(flags & 1) = 0` 才是未读。
    let dir = tempfile::tempdir().expect("tempdir");
    seed(dir.path());
    let conn = rusqlite::Connection::open(dir.path().join("myemail.db")).expect("open");
    let store = Store::open(dir.path()).expect("reopen");

    let inbox_a = folder_id_of(&conn, 1, "inbox");
    let inbox_b = folder_id_of(&conn, 2, "inbox");
    insert_message(&conn, 1, inbox_a, Some(1), "未读", 100, 0);
    insert_message(&conn, 2, inbox_b, Some(1), "已读", 200, 1);
    insert_message(
        &conn,
        1,
        folder_id_of(&conn, 1, "sent"),
        Some(2),
        "已发送里的未读",
        300,
        0,
    );
    insert_message(&conn, 1, inbox_a, None, "本地已移动的未读", 400, 0);

    let count = store
        .read_only()
        .unified_unread_count_readonly()
        .expect("count");
    assert_eq!(count, 1, "只有收件箱里那封未读该被计入");
}

#[test]
fn unified_unread_count_matches_the_per_folder_count() {
    // 同一个 `flags & 1` 位——聚合计数与单文件夹计数口径必须一致。
    let dir = tempfile::tempdir().expect("tempdir");
    seed(dir.path());
    let conn = rusqlite::Connection::open(dir.path().join("myemail.db")).expect("open");
    let store = Store::open(dir.path()).expect("reopen");

    let inbox_a = folder_id_of(&conn, 1, "inbox");
    let inbox_b = folder_id_of(&conn, 2, "inbox");
    insert_message(&conn, 1, inbox_a, Some(1), "未读一", 100, 0);
    insert_message(&conn, 1, inbox_a, Some(2), "未读二", 200, 0);
    insert_message(&conn, 2, inbox_b, Some(1), "已读", 300, 1);

    let unified = store
        .read_only()
        .unified_unread_count_readonly()
        .expect("unified");
    let folder = store
        .read_only()
        .unread_count_readonly(FolderId(inbox_a))
        .expect("folder");
    assert_eq!(unified, folder, "两个口径必须一致");
    assert_eq!(unified, 2);
}

#[test]
fn unified_inbox_respects_the_page_size_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed(dir.path());
    let conn = rusqlite::Connection::open(dir.path().join("myemail.db")).expect("open");
    let store = Store::open(dir.path()).expect("reopen");

    let inbox = folder_id_of(&conn, 1, "inbox");
    for i in 0..5 {
        insert_message(
            &conn,
            1,
            inbox,
            Some(i + 1),
            &format!("邮件{i}"),
            1000 + i,
            0,
        );
    }

    // 超上限要被夹到 `MAX_PAGE_SIZE`（200），但更小的值按实际来。
    let list = store
        .read_only()
        .list_unified_inbox_readonly(None, 2)
        .expect("list");
    assert_eq!(list.len(), 2, "limit 应被尊重");
}

#[test]
fn unified_inbox_is_empty_when_there_are_no_inboxes() {
    let dir = tempfile::tempdir().expect("tempdir");
    drop(Store::open(dir.path()).expect("open"));
    let store = Store::open(dir.path()).expect("reopen");

    let list = store
        .read_only()
        .list_unified_inbox_readonly(None, 200)
        .expect("list");
    assert!(list.is_empty());
    let count = store
        .read_only()
        .unified_unread_count_readonly()
        .expect("count");
    assert_eq!(count, 0);
}

#[test]
fn message_summary_keeps_its_account_for_the_aggregate_view() {
    // 聚合视图要显示「账户」列——`MessageSummary.account` 必须是对的。
    let dir = tempfile::tempdir().expect("tempdir");
    seed(dir.path());
    let conn = rusqlite::Connection::open(dir.path().join("myemail.db")).expect("open");
    let store = Store::open(dir.path()).expect("reopen");

    let inbox_b = folder_id_of(&conn, 2, "inbox");
    insert_message(&conn, 2, inbox_b, Some(1), "来自第二个账户", 200, 0);

    let list = store
        .read_only()
        .list_unified_inbox_readonly(None, 200)
        .expect("list");
    assert_eq!(list[0].account, AccountId(2));
    assert_eq!(list[0].folder.0, inbox_b);
}

#[test]
fn reset_body_cache_keeps_messages_but_not_accounts() {
    // 顺带确认「清缓存」不会把统一收件箱清空——它只清正文。
    let dir = tempfile::tempdir().expect("tempdir");
    seed(dir.path());
    let conn = rusqlite::Connection::open(dir.path().join("myemail.db")).expect("open");
    insert_message(
        &conn,
        1,
        folder_id_of(&conn, 1, "inbox"),
        Some(1),
        "还在",
        100,
        0,
    );
    drop(conn);

    let store = Store::open(dir.path()).expect("reopen");
    store.writable().reset_body_cache(None).expect("reset");

    let list = store
        .read_only()
        .list_unified_inbox_readonly(None, 200)
        .expect("list");
    assert_eq!(list.len(), 1, "清缓存只清正文，摘要还在");
    assert_eq!(list[0].id, MessageId(1));
}

#[test]
fn folder_role_inbox_is_what_qualifies() {
    // 明确「收件箱」是**文件夹角色**——一个账户可以有多个 inbox 文件夹。
    let dir = tempfile::tempdir().expect("tempdir");
    seed(dir.path());
    let conn = rusqlite::Connection::open(dir.path().join("myemail.db")).expect("open");
    let store = Store::open(dir.path()).expect("reopen");

    // 给账户 1 加第二个 inbox 文件夹（SPECIAL-USE 启发式可能标错）。
    conn.execute(
        "INSERT INTO folders(account_id, name, role) VALUES (1, '另一个收件箱', 'inbox')",
        [],
    )
    .expect("insert folder");
    let second_inbox: i64 = conn
        .query_row(
            "SELECT id FROM folders WHERE account_id = 1 AND name = '另一个收件箱'",
            [],
            |row| row.get(0),
        )
        .expect("query folder");

    insert_message(
        &conn,
        1,
        folder_id_of(&conn, 1, "inbox"),
        Some(1),
        "第一个收件箱",
        100,
        0,
    );
    insert_message(&conn, 1, second_inbox, Some(1), "第二个收件箱", 200, 0);

    let list = store
        .read_only()
        .list_unified_inbox_readonly(None, 200)
        .expect("list");
    assert_eq!(list.len(), 2, "两个 inbox 角色文件夹都该被计入");

    // 角色确实是 Inbox。
    let role: String = conn
        .query_row(
            "SELECT role FROM folders WHERE id = ?1",
            params![second_inbox],
            |row| row.get(0),
        )
        .expect("query role");
    assert_eq!(role, FolderRole::Inbox.as_str());
}
