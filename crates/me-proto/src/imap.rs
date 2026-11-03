//! IMAP 适配：TLS 建连、能力探测、文件夹 / 头部 / 正文 / 标志 / 移动。
//!
//! 本模块是 `async-imap` 的唯一使用点；对外只暴露 `me-core::port` 中的类型。

use std::fmt::Debug;
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_imap::Authenticator;
use async_imap::Session;
use async_imap::extensions::idle::IdleResponse;
use async_imap::imap_proto::rfc4315::UidSetMember;
use async_imap::imap_proto::{
    MailboxDatum, NameAttribute, RequestId, Response, ResponseCode, SectionPath, Status,
};
use async_imap::types::Flag;
use futures_util::StreamExt;
use me_core::CoreError;
use me_core::model::{Account, AuthCredential, Flags, TlsMode};
use me_core::port::{
    FlagStoreMode, FolderInfo, HeaderFetch, IdleOutcome, ImapCapabilities, ImapSession,
    ImapSessionFactory, SelectedFolder, UidRange,
};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::bodystructure;
use crate::utf7;

const OP_TIMEOUT: Duration = Duration::from_secs(60);

const HEADER_FETCH_QUERY: &str =
    "(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER] BODYSTRUCTURE)";

/// 底层流：TLS 与明文连接统一装箱，使 `ImapConnection` 只有一个具体类型。
trait IoStream: AsyncRead + AsyncWrite + Unpin + Debug + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Debug + Send> IoStream for T {}

type BoxStream = Box<dyn IoStream>;
type ImapSessionInner = Session<BoxStream>;

/// 已认证的 IMAP 连接。
///
/// 非线程安全：同一账户同一时刻只允许一条连接（由 `me-sync` 的信号量保证）。
pub struct ImapConnection {
    /// `IDLE` 需要把 `Session` 交给 `Handle` 再取回，因此用 `Option` 表示
    /// 「连接处于可用 / 半状态（不可复用）」。半状态连接由调用方直接丢弃。
    session: Option<ImapSessionInner>,
    capabilities: ImapCapabilities,
}

/// 连接工厂，供 `me-sync` 以账户为粒度建立新会话。
#[derive(Debug, Clone, Copy, Default)]
pub struct ProtoFactory;

impl ProtoFactory {
    pub fn new() -> Self {
        Self
    }
}

impl ImapSessionFactory for ProtoFactory {
    type Session = ImapConnection;

    fn connect(
        &self,
        account: &Account,
        credential: &AuthCredential,
    ) -> impl Future<Output = Result<Self::Session, CoreError>> + Send {
        connect_imap(account, credential)
    }
}

// ---------------------------------------------------------------------------
// TLS
// ---------------------------------------------------------------------------

static TLS_CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();

/// 全进程唯一 ClientConfig：优先系统根证书，加载不到时回退 webpki 内置根。
/// 明确不提供“忽略证书错误”的选项。
fn tls_config() -> Result<Arc<ClientConfig>, CoreError> {
    if let Some(config) = TLS_CONFIG.get() {
        return Ok(config.clone());
    }

    // 显式安装 ring provider：即使依赖特性变化导致多个 provider 被编译进来，
    // 也不会在首次 TLS 连接时因无法解析默认 provider 而 panic。
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut roots = RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    let (added, ignored) = roots.add_parsable_certificates(native.certs);
    if added == 0 {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    tracing::debug!(
        native_certs_added = added,
        native_certs_ignored = ignored,
        native_errors = native.errors.len(),
        "IMAP TLS 根证书加载完成"
    );

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|err| CoreError::protocol(None::<String>, format!("TLS 配置失败: {err}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();

    let config = Arc::new(config);
    let _ = TLS_CONFIG.set(config.clone());
    Ok(config)
}

async fn tcp_connect(host: &str, port: u16) -> Result<TcpStream, CoreError> {
    with_timeout(async {
        TcpStream::connect((host, port))
            .await
            .map_err(CoreError::from)
    })
    .await
}

async fn tls_wrap(
    config: Arc<ClientConfig>,
    host: &str,
    stream: BoxStream,
) -> Result<BoxStream, CoreError> {
    let server_name = ServerName::try_from(host.to_owned())
        .map_err(|_| CoreError::protocol(None::<String>, "TLS 服务器主机名无效"))?;
    let connector = TlsConnector::from(config);
    let tls = with_timeout(async {
        connector
            .connect(server_name, stream)
            .await
            .map_err(CoreError::from)
    })
    .await?;
    Ok(Box::new(tls))
}

async fn read_greeting(client: &mut async_imap::Client<BoxStream>) -> Result<(), CoreError> {
    let response =
        with_timeout(async { client.read_response().await.map_err(CoreError::from) }).await?;
    if response.is_none() {
        return Err(CoreError::network(true, "IMAP 服务器未发送问候"));
    }
    Ok(())
}

/// XOAUTH2 的 SASL 响应：`user=<user>\x01auth=Bearer <token>\x01\x01`。
///
/// 首次挑战返回凭证；错误挑战返回空串，让服务器以 NO 结束握手。
struct Xoauth2Authenticator<'a> {
    user: &'a str,
    access_token: &'a str,
    responded: bool,
}

impl<'a> Xoauth2Authenticator<'a> {
    fn new(user: &'a str, access_token: &'a str) -> Self {
        Self {
            user,
            access_token,
            responded: false,
        }
    }
}

impl Authenticator for Xoauth2Authenticator<'_> {
    type Response = Vec<u8>;

    fn process(&mut self, challenge: &[u8]) -> Self::Response {
        if self.responded {
            // 错误挑战：不把 token 再次发给服务器。
            let _ = challenge;
            return Vec::new();
        }
        self.responded = true;
        let mut response = Vec::with_capacity(self.user.len() + self.access_token.len() + 32);
        response.extend_from_slice(b"user=");
        response.extend_from_slice(self.user.as_bytes());
        response.extend_from_slice(b"\x01auth=Bearer ");
        response.extend_from_slice(self.access_token.as_bytes());
        response.extend_from_slice(b"\x01\x01");
        response
    }
}

/// 建立并登录一条 IMAP 会话。
pub async fn connect_imap(
    account: &Account,
    credential: &AuthCredential,
) -> Result<ImapConnection, CoreError> {
    let endpoint = &account.imap;
    let config = tls_config()?;

    let client = match endpoint.tls {
        TlsMode::Implicit => {
            let tcp = tcp_connect(&endpoint.host, endpoint.port).await?;
            let stream = tls_wrap(config, &endpoint.host, Box::new(tcp)).await?;
            let mut client = async_imap::Client::new(stream);
            read_greeting(&mut client).await?;
            client
        }
        TlsMode::StartTls => {
            let tcp = tcp_connect(&endpoint.host, endpoint.port).await?;
            let stream: BoxStream = Box::new(tcp);
            let mut client = async_imap::Client::new(stream);
            read_greeting(&mut client).await?;
            with_timeout(async {
                client
                    .run_command_and_check_ok("STARTTLS", None)
                    .await
                    .map_err(map_imap_error)
            })
            .await?;
            let stream = client.into_inner();
            let stream = tls_wrap(config, &endpoint.host, stream).await?;
            // STARTTLS 升级后服务器不会重发问候。
            async_imap::Client::new(stream)
        }
        TlsMode::None => {
            let tcp = tcp_connect(&endpoint.host, endpoint.port).await?;
            let stream: BoxStream = Box::new(tcp);
            let mut client = async_imap::Client::new(stream);
            read_greeting(&mut client).await?;
            client
        }
    };

    let mut session = match credential {
        AuthCredential::Password { secret } => {
            with_timeout(async {
                client
                    .login(&account.auth_user, secret)
                    .await
                    .map_err(|(err, _client)| login_error(err))
            })
            .await?
        }
        AuthCredential::OAuth2 {
            email,
            access_token,
            ..
        } => {
            let authenticator = Xoauth2Authenticator::new(email, access_token);
            with_timeout(async {
                client
                    .authenticate("XOAUTH2", authenticator)
                    .await
                    .map_err(|(err, _client)| login_error(err))
            })
            .await?
        }
    };

    let raw_caps =
        with_timeout(async { session.capabilities().await.map_err(map_imap_error) }).await?;
    let capabilities = map_capabilities(&raw_caps);

    // RFC 2971：QQ / 163 等要求 ID；失败不影响登录。
    if capabilities.id {
        let id_result = with_timeout(async {
            session
                .id([
                    ("name", Some(me_core::APP_NAME)),
                    ("version", Some(env!("CARGO_PKG_VERSION"))),
                ])
                .await
                .map_err(map_imap_error)
        })
        .await;
        if let Err(err) = id_result {
            tracing::debug!(error = %err, "IMAP ID 命令失败，继续登录流程");
        }
    }

    Ok(ImapConnection {
        session: Some(session),
        capabilities,
    })
}

fn map_capabilities(caps: &async_imap::types::Capabilities) -> ImapCapabilities {
    ImapCapabilities {
        move_supported: caps.has_str("MOVE"),
        uidplus: caps.has_str("UIDPLUS"),
        special_use: caps.has_str("SPECIAL-USE"),
        id: caps.has_str("ID"),
        utf8_accept: caps.has_str("UTF8=ACCEPT") || caps.has_str("UTF8=ONLY"),
        idle: caps.has_str("IDLE"),
        xoauth2: caps.has_str("AUTH=XOAUTH2"),
    }
}

fn login_error(err: async_imap::error::Error) -> CoreError {
    use async_imap::error::Error as ImapError;
    match err {
        // 认证被拒绝（NO/BAD）统一视为凭证问题。
        ImapError::No(_) | ImapError::Bad(_) => {
            CoreError::auth("IMAP 登录失败，请检查用户名与授权码")
        }
        ImapError::Io(io) => CoreError::from(io),
        ImapError::ConnectionLost => CoreError::network(true, "IMAP 登录时连接中断"),
        other => map_imap_error(other),
    }
}

fn map_imap_error(err: async_imap::error::Error) -> CoreError {
    use async_imap::error::Error as ImapError;
    match err {
        ImapError::Io(io) => CoreError::from(io),
        ImapError::ConnectionLost => CoreError::network(true, "IMAP 连接中断"),
        ImapError::No(detail) | ImapError::Bad(detail) => {
            CoreError::protocol(None::<String>, sanitize_detail(&detail))
        }
        ImapError::Parse(_) => CoreError::protocol(None::<String>, "IMAP 响应解析失败"),
        ImapError::Validate(_) => CoreError::protocol(None::<String>, "IMAP 命令参数非法"),
        ImapError::Append => CoreError::protocol(None::<String>, "IMAP APPEND 失败"),
        _ => CoreError::protocol(None::<String>, "IMAP 协议错误"),
    }
}

fn status_error(status: &Status, information: Option<&str>) -> CoreError {
    let reason = match status {
        Status::No => "服务器拒绝请求",
        Status::Bad => "服务器认为命令非法",
        Status::Bye => "服务器断开连接(BYE)",
        _ => "IMAP 状态异常",
    };
    let information = information.map(sanitize_detail).unwrap_or_default();
    if information.is_empty() {
        CoreError::protocol(None::<String>, reason)
    } else {
        CoreError::protocol(None::<String>, format!("{reason}: {information}"))
    }
}

fn sanitize_detail(input: &str) -> String {
    input
        .chars()
        .map(|ch| if ch == '\r' || ch == '\n' { ' ' } else { ch })
        .take(200)
        .collect::<String>()
        .trim()
        .to_string()
}

async fn with_timeout<F, T>(future: F) -> Result<T, CoreError>
where
    F: Future<Output = Result<T, CoreError>>,
{
    match tokio::time::timeout(OP_TIMEOUT, future).await {
        Ok(result) => result,
        Err(_) => Err(CoreError::network(true, "IMAP 操作超时")),
    }
}

// ---------------------------------------------------------------------------
// 命令辅助
// ---------------------------------------------------------------------------

fn uid_range_text(range: UidRange) -> String {
    if range.is_empty() {
        return String::new();
    }
    match range.end {
        None => format!("{}:*", range.start),
        Some(end) if end == range.start => range.start.to_string(),
        Some(end) => format!("{}:{}", range.start, end),
    }
}

fn uid_list_text(uids: &[u32]) -> String {
    uids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn quote_mailbox(folder: &str) -> Result<String, CoreError> {
    let encoded = utf7::encode(folder);
    if encoded.contains('\r') || encoded.contains('\n') {
        return Err(CoreError::protocol(None::<String>, "文件夹名包含非法字符"));
    }
    let escaped = encoded.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!("\"{escaped}\""))
}

fn quote_search_value(value: &str) -> Result<String, CoreError> {
    if value.contains('\r') || value.contains('\n') {
        return Err(CoreError::protocol(
            None::<String>,
            "Message-ID 包含非法字符",
        ));
    }
    Ok(value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn flag_to_string(flag: Flag<'_>) -> String {
    match flag {
        Flag::Seen => "\\Seen".to_string(),
        Flag::Answered => "\\Answered".to_string(),
        Flag::Flagged => "\\Flagged".to_string(),
        Flag::Deleted => "\\Deleted".to_string(),
        Flag::Draft => "\\Draft".to_string(),
        Flag::Recent => "\\Recent".to_string(),
        Flag::MayCreate => "\\*".to_string(),
        Flag::Custom(name) => name.into_owned(),
    }
}

fn name_attribute_string(attribute: &NameAttribute<'_>) -> String {
    match attribute {
        NameAttribute::NoInferiors => "\\NoInferiors".to_string(),
        NameAttribute::NoSelect => "\\NoSelect".to_string(),
        NameAttribute::Marked => "\\Marked".to_string(),
        NameAttribute::Unmarked => "\\Unmarked".to_string(),
        NameAttribute::All => "\\All".to_string(),
        NameAttribute::Archive => "\\Archive".to_string(),
        NameAttribute::Drafts => "\\Drafts".to_string(),
        NameAttribute::Flagged => "\\Flagged".to_string(),
        NameAttribute::Junk => "\\Junk".to_string(),
        NameAttribute::Sent => "\\Sent".to_string(),
        NameAttribute::Trash => "\\Trash".to_string(),
        NameAttribute::Extension(raw) => raw.to_string(),
        _ => String::new(),
    }
}

fn expand_uid_set(set: &[UidSetMember]) -> Vec<u32> {
    let mut out = Vec::new();
    for member in set {
        match member {
            UidSetMember::Uid(uid) => out.push(*uid),
            UidSetMember::UidRange(range) => {
                if range.end().saturating_sub(*range.start()) <= 1_000_000 {
                    out.extend(range.clone());
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 原始命令（需要读取 APPENDUID / COPYUID 等未类型化的响应码）
// ---------------------------------------------------------------------------

async fn raw_command_tag(
    session: &mut ImapSessionInner,
    command: &str,
) -> Result<RequestId, CoreError> {
    with_timeout(async { session.run_command(command).await.map_err(map_imap_error) }).await
}

async fn raw_list(
    session: &mut ImapSessionInner,
    special_use: bool,
) -> Result<Vec<FolderInfo>, CoreError> {
    let command = if special_use {
        "LIST \"\" \"*\" RETURN (SPECIAL-USE)"
    } else {
        "LIST \"\" \"*\""
    };
    let tag = raw_command_tag(session, command).await?;
    let mut folders = Vec::new();

    loop {
        let response =
            with_timeout(async { session.read_response().await.map_err(CoreError::from) }).await?;
        let Some(response) = response else {
            return Err(CoreError::network(true, "IMAP 连接中断"));
        };
        match response.parsed() {
            Response::MailboxData(MailboxDatum::List(data)) => {
                let attributes = data
                    .name_attributes
                    .iter()
                    .map(name_attribute_string)
                    .collect();
                let delimiter = data
                    .delimiter
                    .as_deref()
                    .and_then(|value| value.chars().next());
                let raw_name = data.name.as_ref();
                let name = utf7::decode(raw_name).unwrap_or_else(|_| raw_name.to_string());
                folders.push(FolderInfo {
                    name,
                    delimiter,
                    attributes,
                });
            }
            Response::Done {
                tag: response_tag,
                status,
                outcome,
            } if response_tag == &tag => {
                if matches!(status, Status::Ok) {
                    return Ok(folders);
                }
                return Err(status_error(status, outcome.information.as_deref()));
            }
            _ => {}
        }
    }
}

async fn raw_append(
    session: &mut ImapSessionInner,
    folder: &str,
    raw: &[u8],
    flags: Flags,
) -> Result<Option<u32>, CoreError> {
    let mailbox = quote_mailbox(folder)?;
    let flag_text = if flags.bits() == 0 {
        String::new()
    } else {
        format!(" ({})", flags.to_imap().join(" "))
    };
    let command = format!("APPEND {mailbox}{flag_text} {{{}}}", raw.len());
    let tag = raw_command_tag(session, &command).await?;

    loop {
        let response =
            with_timeout(async { session.read_response().await.map_err(CoreError::from) }).await?;
        let Some(response) = response else {
            return Err(CoreError::network(true, "IMAP 连接中断"));
        };
        match response.parsed() {
            Response::Continue(_) => break,
            Response::Done {
                status, outcome, ..
            } => {
                return Err(status_error(status, outcome.information.as_deref()));
            }
            _ => {}
        }
    }

    with_timeout(async {
        let stream = session.as_mut();
        stream.write_all(raw).await.map_err(CoreError::from)?;
        stream.write_all(b"\r\n").await.map_err(CoreError::from)?;
        stream.flush().await.map_err(CoreError::from)
    })
    .await?;

    loop {
        let response =
            with_timeout(async { session.read_response().await.map_err(CoreError::from) }).await?;
        let Some(response) = response else {
            return Err(CoreError::network(true, "IMAP 连接中断"));
        };
        match response.parsed() {
            Response::Done {
                tag: response_tag,
                status,
                outcome,
            } if response_tag == &tag => {
                if !matches!(status, Status::Ok) {
                    return Err(status_error(status, outcome.information.as_deref()));
                }
                let append_uid = match outcome.code.as_ref() {
                    Some(ResponseCode::AppendUid(_, set)) => expand_uid_set(set).into_iter().next(),
                    _ => None,
                };
                return Ok(append_uid);
            }
            _ => {}
        }
    }
}

async fn raw_copy_like(
    session: &mut ImapSessionInner,
    command: &str,
) -> Result<Option<Vec<(u32, u32)>>, CoreError> {
    let tag = raw_command_tag(session, command).await?;
    loop {
        let response =
            with_timeout(async { session.read_response().await.map_err(CoreError::from) }).await?;
        let Some(response) = response else {
            return Err(CoreError::network(true, "IMAP 连接中断"));
        };
        match response.parsed() {
            Response::Done {
                tag: response_tag,
                status,
                outcome,
            } if response_tag == &tag => {
                if !matches!(status, Status::Ok) {
                    return Err(status_error(status, outcome.information.as_deref()));
                }
                let pairs = match outcome.code.as_ref() {
                    Some(ResponseCode::CopyUid(_, source, target)) => {
                        let source = expand_uid_set(source);
                        let target = expand_uid_set(target);
                        Some(source.into_iter().zip(target).collect())
                    }
                    _ => None,
                };
                return Ok(pairs);
            }
            _ => {}
        }
    }
}

async fn raw_ok(session: &mut ImapSessionInner, command: &str) -> Result<(), CoreError> {
    let tag = raw_command_tag(session, command).await?;
    loop {
        let response =
            with_timeout(async { session.read_response().await.map_err(CoreError::from) }).await?;
        let Some(response) = response else {
            return Err(CoreError::network(true, "IMAP 连接中断"));
        };
        match response.parsed() {
            Response::Done {
                tag: response_tag,
                status,
                outcome,
            } if response_tag == &tag => {
                if matches!(status, Status::Ok) {
                    return Ok(());
                }
                return Err(status_error(status, outcome.information.as_deref()));
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// ImapSession
// ---------------------------------------------------------------------------

impl ImapConnection {
    fn session_mut(&mut self) -> Result<&mut ImapSessionInner, CoreError> {
        self.session
            .as_mut()
            .ok_or_else(|| CoreError::network(false, "IMAP 连接处于不可复用状态"))
    }
}

impl ImapSession for ImapConnection {
    fn capabilities(&self) -> ImapCapabilities {
        self.capabilities
    }

    async fn list_folders(&mut self) -> Result<Vec<FolderInfo>, CoreError> {
        if self.capabilities.special_use {
            match raw_list(self.session_mut()?, true).await {
                Ok(folders) => return Ok(folders),
                Err(err) => {
                    tracing::debug!(error = %err, "LIST RETURN (SPECIAL-USE) 失败，回退普通 LIST");
                }
            }
        }
        raw_list(self.session_mut()?, false).await
    }

    async fn select(&mut self, folder: &str) -> Result<SelectedFolder, CoreError> {
        let name = utf7::encode(folder);
        let mailbox = with_timeout(async {
            self.session_mut()?
                .select(name)
                .await
                .map_err(map_imap_error)
        })
        .await?;
        Ok(SelectedFolder {
            uid_validity: mailbox.uid_validity.unwrap_or(0),
            uid_next: mailbox.uid_next.unwrap_or(0),
            exists: mailbox.exists,
        })
    }

    async fn fetch_headers(&mut self, range: UidRange) -> Result<Vec<HeaderFetch>, CoreError> {
        let uid_set = uid_range_text(range);
        if uid_set.is_empty() {
            return Ok(Vec::new());
        }
        let mut stream = Box::pin(
            with_timeout(async {
                self.session_mut()?
                    .uid_fetch(uid_set, HEADER_FETCH_QUERY)
                    .await
                    .map_err(map_imap_error)
            })
            .await?,
        );

        let mut fetched = Vec::new();
        while let Some(item) =
            with_timeout(async { stream.next().await.transpose().map_err(map_imap_error) }).await?
        {
            let Some(uid) = item.uid else {
                continue;
            };
            let flags = Flags::from_imap(item.flags().map(flag_to_string).collect::<Vec<_>>());
            fetched.push(HeaderFetch {
                uid,
                flags,
                internal_date: item.internal_date().map(|date| date.timestamp()),
                size: item.size,
                header: item.header().unwrap_or_default().to_vec(),
                has_attachment: item
                    .bodystructure()
                    .is_some_and(bodystructure::has_attachment),
            });
        }
        Ok(fetched)
    }

    async fn fetch_flags(&mut self, range: UidRange) -> Result<Vec<(u32, Flags)>, CoreError> {
        let uid_set = uid_range_text(range);
        if uid_set.is_empty() {
            return Ok(Vec::new());
        }
        let mut stream = Box::pin(
            with_timeout(async {
                self.session_mut()?
                    .uid_fetch(uid_set, "(UID FLAGS)")
                    .await
                    .map_err(map_imap_error)
            })
            .await?,
        );

        let mut fetched = Vec::new();
        while let Some(item) =
            with_timeout(async { stream.next().await.transpose().map_err(map_imap_error) }).await?
        {
            if let Some(uid) = item.uid {
                let flags = Flags::from_imap(item.flags().map(flag_to_string).collect::<Vec<_>>());
                fetched.push((uid, flags));
            }
        }
        Ok(fetched)
    }

    async fn store_flags(
        &mut self,
        uids: &[u32],
        flags: Flags,
        mode: FlagStoreMode,
    ) -> Result<(), CoreError> {
        if uids.is_empty() {
            return Ok(());
        }
        let flag_list = flags.to_imap().join(" ");
        let operation = match mode {
            FlagStoreMode::Add => "+FLAGS.SILENT",
            FlagStoreMode::Remove => "-FLAGS.SILENT",
            FlagStoreMode::Replace => "FLAGS.SILENT",
        };
        if flag_list.is_empty() && !matches!(mode, FlagStoreMode::Replace) {
            return Ok(());
        }
        let query = format!("{operation} ({flag_list})");
        let mut stream = Box::pin(
            with_timeout(async {
                self.session_mut()?
                    .uid_store(uid_list_text(uids), query)
                    .await
                    .map_err(map_imap_error)
            })
            .await?,
        );
        while let Some(item) =
            with_timeout(async { stream.next().await.transpose().map_err(map_imap_error) }).await?
        {
            let _ = item;
        }
        Ok(())
    }

    async fn append(
        &mut self,
        folder: &str,
        raw: &[u8],
        flags: Flags,
    ) -> Result<Option<u32>, CoreError> {
        raw_append(self.session_mut()?, folder, raw, flags).await
    }

    async fn move_to(
        &mut self,
        uids: &[u32],
        target: &str,
    ) -> Result<Option<Vec<(u32, u32)>>, CoreError> {
        if uids.is_empty() {
            return Ok(None);
        }
        if !self.capabilities.move_supported {
            return Err(CoreError::protocol(None::<String>, "服务器不支持 MOVE"));
        }
        let mailbox = quote_mailbox(target)?;
        let command = format!("UID MOVE {} {}", uid_list_text(uids), mailbox);
        raw_copy_like(self.session_mut()?, &command).await
    }

    async fn copy_to(
        &mut self,
        uids: &[u32],
        target: &str,
    ) -> Result<Option<Vec<(u32, u32)>>, CoreError> {
        if uids.is_empty() {
            return Ok(None);
        }
        let mailbox = quote_mailbox(target)?;
        let command = format!("UID COPY {} {}", uid_list_text(uids), mailbox);
        raw_copy_like(self.session_mut()?, &command).await
    }

    async fn uid_expunge(&mut self, uids: &[u32]) -> Result<(), CoreError> {
        if uids.is_empty() {
            return Ok(());
        }
        if !self.capabilities.uidplus {
            return Err(CoreError::protocol(None::<String>, "服务器不支持 UIDPLUS"));
        }
        let command = format!("UID EXPUNGE {}", uid_list_text(uids));
        raw_ok(self.session_mut()?, &command).await
    }

    async fn search_message_id(&mut self, message_id: &str) -> Result<Vec<u32>, CoreError> {
        let quoted = quote_search_value(message_id)?;
        let query = format!("HEADER Message-ID \"{quoted}\"");
        let found = with_timeout(async {
            self.session_mut()?
                .uid_search(&query)
                .await
                .map_err(map_imap_error)
        })
        .await?;
        let mut uids: Vec<u32> = found.into_iter().collect();
        uids.sort_unstable();
        Ok(uids)
    }

    async fn fetch_body(&mut self, uid: u32) -> Result<Vec<u8>, CoreError> {
        let mut stream = Box::pin(
            with_timeout(async {
                self.session_mut()?
                    .uid_fetch(uid.to_string(), "BODY.PEEK[]")
                    .await
                    .map_err(map_imap_error)
            })
            .await?,
        );
        match with_timeout(async { stream.next().await.transpose().map_err(map_imap_error) })
            .await?
        {
            Some(fetch) => fetch
                .body()
                .map(<[u8]>::to_vec)
                .ok_or_else(|| CoreError::protocol(None::<String>, "服务器未返回正文")),
            None => Err(CoreError::protocol(None::<String>, "服务器未返回正文")),
        }
    }

    async fn fetch_part(&mut self, uid: u32, part: &str) -> Result<Vec<u8>, CoreError> {
        let numbers: Vec<u32> = part
            .split('.')
            .map(|segment| {
                segment
                    .parse::<u32>()
                    .map_err(|_| CoreError::protocol(None::<String>, "非法 MIME 分段号"))
            })
            .collect::<Result<_, _>>()?;
        if numbers.is_empty() {
            return Err(CoreError::protocol(None::<String>, "非法 MIME 分段号"));
        }
        let section = numbers
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(".");
        let query = format!("BODY.PEEK[{section}]");
        let mut stream = Box::pin(
            with_timeout(async {
                self.session_mut()?
                    .uid_fetch(uid.to_string(), query)
                    .await
                    .map_err(map_imap_error)
            })
            .await?,
        );
        let path = SectionPath::Part(numbers, None);
        match with_timeout(async { stream.next().await.transpose().map_err(map_imap_error) })
            .await?
        {
            Some(fetch) => fetch
                .section(&path)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| CoreError::protocol(None::<String>, "服务器未返回分段内容")),
            None => Err(CoreError::protocol(None::<String>, "服务器未返回分段内容")),
        }
    }

    async fn search_all_uids(&mut self) -> Result<Vec<u32>, CoreError> {
        let found = with_timeout(async {
            self.session_mut()?
                .uid_search("UNDELETED")
                .await
                .map_err(map_imap_error)
        })
        .await?;
        let mut uids: Vec<u32> = found.into_iter().collect();
        uids.sort_unstable();
        Ok(uids)
    }

    async fn wait_for_changes(&mut self, timeout: Duration) -> Result<IdleOutcome, CoreError> {
        let session = self
            .session
            .take()
            .ok_or_else(|| CoreError::network(false, "IMAP 连接处于不可复用状态"))?;
        let mut handle = session.idle();
        with_timeout(async { handle.init().await.map_err(map_imap_error) }).await?;

        let (wait_future, stop) = handle.wait_with_timeout(timeout);
        let response = wait_future.await;
        drop(stop);

        let outcome = match response {
            Ok(IdleResponse::NewData(data)) => Ok(match data.parsed() {
                Response::MailboxData(MailboxDatum::Exists(count)) => IdleOutcome::Changed {
                    exists: Some(*count),
                    recent: None,
                },
                Response::MailboxData(MailboxDatum::Recent(count)) => IdleOutcome::Changed {
                    exists: None,
                    recent: Some(*count),
                },
                _ => IdleOutcome::Changed {
                    exists: None,
                    recent: None,
                },
            }),
            Ok(IdleResponse::Timeout) => Ok(IdleOutcome::Timeout),
            Ok(IdleResponse::ManualInterrupt) => Ok(IdleOutcome::ManualInterrupt),
            Err(err) => Err(map_imap_error(err)),
        };

        // 无论超时还是变化，都必须发送 DONE 才能复用连接；失败则保持半状态，
        // 由调用方丢弃连接。
        match outcome {
            Ok(outcome) => match handle.done().await {
                Ok(session) => {
                    self.session = Some(session);
                    Ok(outcome)
                }
                Err(err) => Err(map_imap_error(err)),
            },
            Err(err) => Err(err),
        }
    }

    async fn logout(&mut self) -> Result<(), CoreError> {
        with_timeout(async { self.session_mut()?.logout().await.map_err(map_imap_error) }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xoauth2_response_contains_token_once() {
        let mut auth = Xoauth2Authenticator::new("alice@example.com", "ya29.token");
        let first = Authenticator::process(&mut auth, b"");
        assert_eq!(
            first,
            b"user=alice@example.com\x01auth=Bearer ya29.token\x01\x01".to_vec()
        );
        // 错误挑战不得再次发送凭证。
        assert!(Authenticator::process(&mut auth, b"error-detail").is_empty());
    }
}
