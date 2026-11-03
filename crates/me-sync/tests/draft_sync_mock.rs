//! 草稿双向同步集成测试：真实 store + mock IMAP Drafts。
//!
//! 覆盖：本地推送、服务器导入、双方修改冲突、墓碑删除、Drafts 不进入 messages。

use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use me_core::command::AccountDraft;
use me_core::model::{
    Account, AccountId, Address, AuthMethod, DraftId, DraftSyncState, Endpoint, FolderRole, TlsMode,
};
use me_core::port::{AccountRepo, DraftRepo, FolderRepo, MessageRepo, NewDraft};
use me_core::{AuthCredential, Command, DraftConflictResolution, Event};
use me_store::{Store, cache};
use me_sync::SyncService;
use me_sync::events::EventSender;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

const UID_VALIDITY: u32 = 7;

#[derive(Default)]
struct MockState {
    drafts: Mutex<BTreeMap<u32, Vec<u8>>>,
    deleted: Mutex<HashSet<u32>>,
    next_uid: AtomicU32,
    commands: Mutex<Vec<String>>,
    uidplus: std::sync::atomic::AtomicBool,
}

impl MockState {
    fn new() -> Self {
        Self {
            uidplus: std::sync::atomic::AtomicBool::new(true),
            ..Self::default()
        }
    }

    fn add_raw(&self, raw: Vec<u8>) -> u32 {
        let uid = self.next_uid.fetch_add(1, Ordering::SeqCst) + 1;
        self.drafts.lock().unwrap().insert(uid, raw);
        uid
    }

    fn add_draft(&self, message_id: &str, subject: &str, body: &str) -> u32 {
        self.add_raw(draft_raw(message_id, subject, body))
    }

    fn remove(&self, uid: u32) {
        self.drafts.lock().unwrap().remove(&uid);
        self.deleted.lock().unwrap().remove(&uid);
    }

    fn visible_uids(&self) -> Vec<u32> {
        let deleted = self.deleted.lock().unwrap();
        self.drafts
            .lock()
            .unwrap()
            .keys()
            .copied()
            .filter(|uid| !deleted.contains(uid))
            .collect()
    }

    fn draft_count(&self) -> usize {
        self.drafts.lock().unwrap().len()
    }
}

fn draft_raw(message_id: &str, subject: &str, body: &str) -> Vec<u8> {
    format!(
        "From: Alice <alice@example.com>\r\n\
         To: Bob <bob@example.com>\r\n\
         Subject: {subject}\r\n\
         Date: Tue, 14 Jan 2025 10:20:30 +0800\r\n\
         Message-ID: <{message_id}>\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Transfer-Encoding: 7bit\r\n\
         \r\n\
         {body}\r\n"
    )
    .into_bytes()
}

async fn spawn_mock(state: Arc<MockState>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let state = state.clone();
            tokio::spawn(async move { handle_connection(socket, state).await });
        }
    });
    addr
}

async fn handle_connection(socket: TcpStream, state: Arc<MockState>) {
    let (read_half, mut writer) = socket.into_split();
    if writer.write_all(b"* OK mock ready\r\n").await.is_err() {
        return;
    }
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
            break;
        }
        let command = line.trim_end().to_string();
        if command.is_empty() {
            continue;
        }
        state.commands.lock().unwrap().push(command.clone());
        let (tag, rest) = command.split_once(' ').unwrap_or((&command, ""));
        let upper = rest.to_ascii_uppercase();

        if upper.starts_with("CAPABILITY") {
            let uidplus = if state.uidplus.load(Ordering::SeqCst) {
                " UIDPLUS"
            } else {
                ""
            };
            if writer
                .write_all(
                    format!(
                        "* CAPABILITY IMAP4rev1{uidplus} MOVE SPECIAL-USE\r\n{tag} OK CAPABILITY\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .is_err()
            {
                return;
            }
        } else if upper.starts_with("LOGIN") {
            let _ = writer
                .write_all(format!("{tag} OK LOGIN completed\r\n").as_bytes())
                .await;
        } else if upper.starts_with("LIST") {
            let _ = writer
                .write_all(
                    format!(
                        "* LIST (\\HasNoChildren \\Inbox) \"/\" \"INBOX\"\r\n\
                         * LIST (\\HasNoChildren \\Drafts) \"/\" \"Drafts\"\r\n\
                         {tag} OK LIST completed\r\n"
                    )
                    .as_bytes(),
                )
                .await;
        } else if upper.starts_with("SELECT") {
            let is_drafts = upper.contains("DRAFTS");
            let exists = if is_drafts {
                state.visible_uids().len()
            } else {
                0
            };
            let uid_next = if is_drafts {
                state.next_uid.load(Ordering::SeqCst) + 1
            } else {
                1
            };
            let _ = writer
                .write_all(
                    format!(
                        "* {exists} EXISTS\r\n\
                         * OK [UIDVALIDITY {}] ok\r\n\
                         * OK [UIDNEXT {uid_next}] ok\r\n\
                         {tag} OK [READ-WRITE] SELECT completed\r\n",
                        if is_drafts { UID_VALIDITY } else { 1 }
                    )
                    .as_bytes(),
                )
                .await;
        } else if upper.starts_with("UID SEARCH") {
            let uids = if upper.contains("HEADER") {
                state.visible_uids().into_iter().max().into_iter().collect()
            } else {
                state.visible_uids()
            };
            let list = uids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(" ");
            let _ = writer
                .write_all(format!("* SEARCH {list}\r\n{tag} OK SEARCH completed\r\n").as_bytes())
                .await;
        } else if upper.starts_with("UID FETCH") && upper.contains("BODY.PEEK[]") {
            let uid = rest
                .split_whitespace()
                .nth(2)
                .and_then(|value| value.parse::<u32>().ok());
            let raw = uid.and_then(|uid| state.drafts.lock().unwrap().get(&uid).cloned());
            if let Some(raw) = raw {
                let mut out = format!(
                    "* 1 FETCH (UID {} BODY[] {{{}}}\r\n",
                    uid.unwrap(),
                    raw.len()
                )
                .into_bytes();
                out.extend_from_slice(&raw);
                out.extend_from_slice(format!(")\r\n{tag} OK FETCH completed\r\n").as_bytes());
                let _ = writer.write_all(&out).await;
            } else {
                let _ = writer
                    .write_all(format!("{tag} OK FETCH completed\r\n").as_bytes())
                    .await;
            }
        } else if upper.starts_with("APPEND") {
            let size = rest
                .rsplit_once('{')
                .and_then(|(_, tail)| tail.trim_end_matches('}').parse::<usize>().ok())
                .unwrap_or(0);
            let _ = writer.write_all(b"+ Ready for literal data\r\n").await;
            let mut data = vec![0u8; size];
            if reader.read_exact(&mut data).await.is_err() {
                return;
            }
            let mut crlf = [0u8; 2];
            let _ = reader.read_exact(&mut crlf).await;
            let uid = state.add_raw(data);
            if state.uidplus.load(Ordering::SeqCst) {
                let _ = writer
                    .write_all(
                        format!("{tag} OK [APPENDUID {UID_VALIDITY} {uid}] APPEND completed\r\n")
                            .as_bytes(),
                    )
                    .await;
            } else {
                let _ = writer
                    .write_all(format!("{tag} OK APPEND completed\r\n").as_bytes())
                    .await;
            }
        } else if upper.starts_with("UID STORE") {
            if let Some(uid) = first_uid(rest, 2) {
                state.deleted.lock().unwrap().insert(uid);
            }
            let _ = writer
                .write_all(format!("{tag} OK STORE completed\r\n").as_bytes())
                .await;
        } else if upper.starts_with("UID EXPUNGE") {
            let deleted: Vec<u32> = state.deleted.lock().unwrap().drain().collect();
            for uid in deleted {
                state.remove(uid);
            }
            let _ = writer
                .write_all(format!("{tag} OK EXPUNGE completed\r\n").as_bytes())
                .await;
        } else if upper.starts_with("UID MOVE") {
            if let Some(uid) = first_uid(rest, 2) {
                state.remove(uid);
            }
            let _ = writer
                .write_all(
                    format!("{tag} OK [COPYUID {UID_VALIDITY} 1 2] MOVE completed\r\n").as_bytes(),
                )
                .await;
        } else if upper.starts_with("LOGOUT") {
            let _ = writer
                .write_all(format!("* BYE\r\n{tag} OK LOGOUT\r\n").as_bytes())
                .await;
            break;
        } else {
            let _ = writer.write_all(format!("{tag} OK\r\n").as_bytes()).await;
        }
    }
}

fn first_uid(rest: &str, index: usize) -> Option<u32> {
    rest.split_whitespace()
        .nth(index)?
        .split(',')
        .next()?
        .parse()
        .ok()
}

fn create_account(store: &Store, addr: SocketAddr) -> Account {
    store
        .writable()
        .create_account(&AccountDraft {
            email: "alice@example.com".into(),
            display_name: Some("Alice".into()),
            imap: Endpoint::new("127.0.0.1", addr.port(), TlsMode::None),
            smtp: Endpoint::new("127.0.0.1", 2525, TlsMode::None),
            auth_user: "alice@example.com".into(),
            auth_secret: Some("secret".into()),
            auth_method: AuthMethod::Password,
            oauth_provider: None,
            oauth_subject: None,
            plaintext_tls_ack: true,
        })
        .expect("account")
}

fn start_service(
    store: &Store,
    account: &Account,
) -> (mpsc::UnboundedSender<Command>, mpsc::Receiver<Event>) {
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
    service.set_session_credential(account.id, AuthCredential::password("secret"));
    tokio::spawn(async move { service.run(command_rx).await });
    (command_tx, event_rx)
}

fn create_local_draft(
    store: &Store,
    account: AccountId,
    message_id: &str,
    subject: &str,
    body: &str,
) -> DraftId {
    let paths = store.paths();
    let to = vec![Address::bare("bob@example.com")];
    let draft = store
        .writable()
        .create_draft(&NewDraft {
            account,
            message_id: message_id.into(),
            subject: Some(subject.into()),
            to: to.clone(),
            bcc: vec![],
            raw_path: String::new(),
        })
        .expect("create draft");
    let raw = draft_raw(message_id, subject, body);
    let path = cache::write_draft_raw(&paths, draft.id, &raw).expect("write raw");
    store
        .writable()
        .update_draft(draft.id, Some(subject), &to, &[], &path.to_string_lossy())
        .expect("update draft");
    store
        .writable()
        .set_draft_sync_state(draft.id, DraftSyncState::LocalDirty, None)
        .expect("state");
    draft.id
}

async fn wait_until(mut check: impl FnMut() -> bool, what: &str) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timeout waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_dirty_draft_is_pushed_and_marked_synced() {
    let state = Arc::new(MockState::new());
    let addr = spawn_mock(state.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let account = create_account(&store, addr);
    let id = create_local_draft(
        &store,
        account.id,
        "push@example.com",
        "Push me",
        "local body",
    );
    let (_command_tx, _event_rx) = start_service(&store, &account);

    wait_until(
        || {
            store
                .writable()
                .get_draft(id)
                .unwrap()
                .is_some_and(|draft| {
                    draft.sync_state == DraftSyncState::Synced && draft.server_uid.is_some()
                })
        },
        "draft pushed",
    )
    .await;
    assert_eq!(state.draft_count(), 1);
    let server_raw = state
        .drafts
        .lock()
        .unwrap()
        .values()
        .next()
        .cloned()
        .unwrap();
    let parsed = me_proto::mime::parse_draft(&server_raw).expect("server draft parses");
    assert_eq!(parsed.header.subject.as_deref(), Some("Push me"));
    assert!(parsed.body.text.contains("local body"));
    let text = String::from_utf8_lossy(&server_raw);
    assert!(!text.contains("\r\nBcc:"));

    let draft = store.writable().get_draft(id).unwrap().unwrap();
    assert_eq!(draft.server_uid_validity, Some(UID_VALIDITY));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_draft_is_imported_and_not_written_to_messages() {
    let state = Arc::new(MockState::new());
    state.add_draft("import@example.com", "Server draft", "server body");
    let addr = spawn_mock(state.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let account = create_account(&store, addr);
    let (_command_tx, _event_rx) = start_service(&store, &account);

    wait_until(
        || {
            store
                .writable()
                .list_drafts(Some(account.id))
                .unwrap()
                .iter()
                .any(|draft| draft.sync_state == DraftSyncState::Synced)
        },
        "server draft imported",
    )
    .await;

    let drafts = store.writable().list_drafts(Some(account.id)).unwrap();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].sync_state, DraftSyncState::Synced);
    assert_eq!(drafts[0].message_id, "import@example.com");
    let raw = cache::read_draft_raw(&store.paths(), drafts[0].id)
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&raw).contains("server body"));

    let drafts_folder = store
        .writable()
        .list_folders(account.id)
        .unwrap()
        .into_iter()
        .find(|folder| folder.role == FolderRole::Drafts)
        .expect("drafts folder");
    assert!(
        store
            .writable()
            .list_messages(drafts_folder.id, None, 100)
            .unwrap()
            .is_empty(),
        "drafts must not be stored as messages"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_sides_changed_creates_conflict_copy() {
    let state = Arc::new(MockState::new());
    let addr = spawn_mock(state.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let account = create_account(&store, addr);
    let id = create_local_draft(
        &store,
        account.id,
        "conflict@example.com",
        "base",
        "base body",
    );
    let (command_tx, _event_rx) = start_service(&store, &account);

    wait_until(
        || {
            store
                .writable()
                .get_draft(id)
                .unwrap()
                .is_some_and(|draft| draft.sync_state == DraftSyncState::Synced)
        },
        "baseline synced",
    )
    .await;
    let old_uid = store
        .writable()
        .get_draft(id)
        .unwrap()
        .unwrap()
        .server_uid
        .unwrap();

    // 本地修改。
    let paths = store.paths();
    let raw = draft_raw("conflict@example.com", "local v2", "local v2 body");
    let path = cache::write_draft_raw(&paths, id, &raw).unwrap();
    let to = vec![Address::bare("bob@example.com")];
    store
        .writable()
        .update_draft(id, Some("local v2"), &to, &[], &path.to_string_lossy())
        .unwrap();
    store
        .writable()
        .set_draft_sync_state(id, DraftSyncState::LocalDirty, None)
        .unwrap();

    // 服务器修改：其他客户端 APPEND 新版本并删除旧 UID。
    state.remove(old_uid);
    state.add_draft("conflict@example.com", "server v2", "server v2 body");

    command_tx.send(Command::SyncAccount(account.id)).unwrap();
    wait_until(
        || {
            store
                .writable()
                .get_draft(id)
                .unwrap()
                .is_some_and(|draft| draft.sync_state == DraftSyncState::Conflict)
        },
        "conflict detected",
    )
    .await;

    let drafts = store
        .writable()
        .find_drafts_by_message_id(account.id, "conflict@example.com")
        .unwrap();
    assert_eq!(drafts.len(), 2);
    let copy = drafts
        .iter()
        .find(|draft| draft.conflict_of == Some(id))
        .expect("conflict copy");
    assert_eq!(copy.sync_state, DraftSyncState::Conflict);
    assert!(copy.subject.as_deref().unwrap().starts_with("[冲突副本]"));
    let copy_raw = cache::read_draft_raw(&paths, copy.id).unwrap().unwrap();
    assert!(String::from_utf8_lossy(&copy_raw).contains("server v2 body"));

    // 「保留两份」：冲突副本转为独立本地草稿，进入普通列表。
    command_tx
        .send(Command::ResolveDraftConflict {
            draft: id,
            resolution: DraftConflictResolution::KeepBoth,
        })
        .unwrap();
    wait_until(
        || {
            store
                .writable()
                .list_drafts(Some(account.id))
                .unwrap()
                .len()
                == 2
        },
        "conflict copy promoted",
    )
    .await;
    let promoted = store.writable().get_draft(copy.id).unwrap().unwrap();
    assert_eq!(promoted.conflict_of, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_synced_draft_removes_server_copy() {
    let state = Arc::new(MockState::new());
    let addr = spawn_mock(state.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let account = create_account(&store, addr);
    let id = create_local_draft(
        &store,
        account.id,
        "delete@example.com",
        "Delete me",
        "body",
    );
    let (command_tx, _event_rx) = start_service(&store, &account);

    wait_until(
        || {
            store
                .writable()
                .get_draft(id)
                .unwrap()
                .is_some_and(|draft| draft.sync_state == DraftSyncState::Synced)
        },
        "draft pushed",
    )
    .await;
    assert_eq!(state.draft_count(), 1);

    command_tx.send(Command::DeleteDraft { id }).unwrap();
    wait_until(
        || store.writable().get_draft(id).unwrap().is_none(),
        "draft deleted",
    )
    .await;
    assert_eq!(state.draft_count(), 0, "server copy must be expunged");

    let commands = state
        .commands
        .lock()
        .unwrap()
        .clone()
        .join("\n")
        .to_ascii_uppercase();
    assert!(commands.contains("+FLAGS.SILENT (\\DELETED)"));
    assert!(commands.contains("UID EXPUNGE"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_uidplus_deletion_keeps_tombstone() {
    let state = Arc::new(MockState::new());
    state
        .uidplus
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let addr = spawn_mock(state.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let account = create_account(&store, addr);
    let id = create_local_draft(
        &store,
        account.id,
        "nouidplus@example.com",
        "No UIDPLUS",
        "body",
    );
    let (command_tx, _event_rx) = start_service(&store, &account);

    wait_until(
        || {
            store
                .writable()
                .get_draft(id)
                .unwrap()
                .is_some_and(|draft| {
                    draft.sync_state == DraftSyncState::Synced && draft.server_uid.is_some()
                })
        },
        "draft pushed without APPENDUID",
    )
    .await;

    command_tx.send(Command::DeleteDraft { id }).unwrap();
    wait_until(
        || {
            store
                .writable()
                .get_draft(id)
                .unwrap()
                .is_some_and(|draft| {
                    draft.sync_state == DraftSyncState::DeletedLocal && draft.last_error.is_some()
                })
        },
        "tombstone kept after unsupported expunge",
    )
    .await;

    assert_eq!(state.draft_count(), 1, "server copy must not be expunged");
    let commands = state
        .commands
        .lock()
        .unwrap()
        .join("\n")
        .to_ascii_uppercase();
    assert!(commands.contains("+FLAGS.SILENT (\\DELETED)"));
    assert!(!commands.contains("UID EXPUNGE"));
}
