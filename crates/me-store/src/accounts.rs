//! 账户仓储。
//!
//! 凭证不落库：`AccountDraft::auth_secret` 在写入时被忽略，只由 keyring 保存。

use me_core::CoreError;
use me_core::command::AccountDraft;
use me_core::model::{Account, AccountId, AuthMethod, Endpoint, OAuthProvider, TlsMode, now_unix};
use me_core::port::{AccountRepo, DeleteAccountOutcome};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::db::WritableStore;
use crate::{StoreResult, storage};

const SELECT_COLUMNS: &str = "id, email, display_name, imap_host, imap_port, imap_tls, \
                              smtp_host, smtp_port, smtp_tls, auth_user, auth_method, \
                              oauth_provider, oauth_subject, needs_reauth, idle_enabled, \
                              poll_interval_secs, token_expires_at";

fn map_account(row: &Row<'_>) -> rusqlite::Result<Account> {
    let imap_tls: String = row.get(5)?;
    let smtp_tls: String = row.get(8)?;
    let auth_method: String = row.get(10)?;
    let oauth_provider: Option<String> = row.get(11)?;
    Ok(Account {
        id: AccountId(row.get(0)?),
        email: row.get(1)?,
        display_name: row.get(2)?,
        imap: Endpoint {
            host: row.get(3)?,
            port: row.get::<_, i64>(4)? as u16,
            tls: TlsMode::parse(&imap_tls).unwrap_or(TlsMode::Implicit),
        },
        smtp: Endpoint {
            host: row.get(6)?,
            port: row.get::<_, i64>(7)? as u16,
            tls: TlsMode::parse(&smtp_tls).unwrap_or(TlsMode::Implicit),
        },
        auth_user: row.get(9)?,
        auth_method: AuthMethod::parse(&auth_method),
        oauth_provider: oauth_provider.as_deref().and_then(OAuthProvider::parse),
        oauth_subject: row.get(12)?,
        needs_reauth: row.get::<_, i64>(13)? != 0,
        idle_enabled: row.get::<_, i64>(14)? != 0,
        poll_interval_secs: row
            .get::<_, Option<i64>>(15)?
            .map(|value| u32::try_from(value).unwrap_or(0)),
        token_expires_at: row.get(16)?,
    })
}

/// 写入前校验认证方式与服务商的一致性（`me-store` 只做约束，不做业务规则）。
fn validate_account_draft(draft: &AccountDraft) -> Result<(), CoreError> {
    if draft.auth_method.is_oauth() != draft.oauth_provider.is_some() {
        return Err(storage("账户认证方式与 OAuth 服务商不匹配"));
    }
    if draft.auth_method.is_oauth()
        && (draft.imap.tls == TlsMode::None || draft.smtp.tls == TlsMode::None)
    {
        return Err(storage("OAuth2 账户不允许明文 TLS"));
    }
    Ok(())
}

pub(crate) fn list_accounts_conn(conn: &Connection) -> Result<Vec<Account>, CoreError> {
    let sql = format!("SELECT {SELECT_COLUMNS} FROM accounts ORDER BY id");
    let mut stmt = conn.prepare(&sql).store()?;
    let rows = stmt
        .query_map([], map_account)
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    Ok(rows)
}

pub(crate) fn get_account_conn(
    conn: &Connection,
    id: AccountId,
) -> Result<Option<Account>, CoreError> {
    let sql = format!("SELECT {SELECT_COLUMNS} FROM accounts WHERE id = ?1");
    conn.query_row(&sql, params![id.0], map_account)
        .optional()
        .store()
}

pub(crate) fn create_account_conn(
    conn: &Connection,
    draft: &AccountDraft,
) -> Result<Account, CoreError> {
    validate_account_draft(draft)?;
    // `draft.auth_secret` 故意不读取：凭证只进 keyring。
    conn.execute(
        "INSERT INTO accounts(
             email, display_name, imap_host, imap_port, imap_tls,
             smtp_host, smtp_port, smtp_tls, auth_user, auth_method,
             oauth_provider, oauth_subject, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            draft.email,
            draft.display_name,
            draft.imap.host,
            i64::from(draft.imap.port),
            draft.imap.tls.as_str(),
            draft.smtp.host,
            i64::from(draft.smtp.port),
            draft.smtp.tls.as_str(),
            draft.auth_user,
            draft.auth_method.as_str(),
            draft.oauth_provider.map(|provider| provider.as_str()),
            draft.oauth_subject,
            now_unix(),
        ],
    )
    .store()?;
    let id = AccountId(conn.last_insert_rowid());
    get_account_conn(conn, id)?.ok_or_else(|| storage("账户写入后无法读取"))
}

pub(crate) fn update_account_conn(
    conn: &Connection,
    id: AccountId,
    draft: &AccountDraft,
) -> Result<Account, CoreError> {
    validate_account_draft(draft)?;
    let changed = conn
        .execute(
            "UPDATE accounts SET
                 email = ?2, display_name = ?3,
                 imap_host = ?4, imap_port = ?5, imap_tls = ?6,
                 smtp_host = ?7, smtp_port = ?8, smtp_tls = ?9,
                 auth_user = ?10, auth_method = ?11,
                 oauth_provider = ?12, oauth_subject = ?13
             WHERE id = ?1",
            params![
                id.0,
                draft.email,
                draft.display_name,
                draft.imap.host,
                i64::from(draft.imap.port),
                draft.imap.tls.as_str(),
                draft.smtp.host,
                i64::from(draft.smtp.port),
                draft.smtp.tls.as_str(),
                draft.auth_user,
                draft.auth_method.as_str(),
                draft.oauth_provider.map(|provider| provider.as_str()),
                draft.oauth_subject,
            ],
        )
        .store()?;
    if changed == 0 {
        return Err(storage(format!("账户不存在: {}", id.0)));
    }
    get_account_conn(conn, id)?.ok_or_else(|| storage("账户更新后无法读取"))
}

pub(crate) fn count_drafts_conn(conn: &Connection, id: AccountId) -> Result<u32, CoreError> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM drafts WHERE account_id = ?1",
            params![id.0],
            |row| row.get(0),
        )
        .store()?;
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

pub(crate) fn set_account_reauth_conn(
    conn: &Connection,
    id: AccountId,
    needs_reauth: bool,
) -> Result<(), CoreError> {
    let changed = conn
        .execute(
            "UPDATE accounts SET needs_reauth = ?2 WHERE id = ?1",
            params![id.0, i64::from(needs_reauth)],
        )
        .store()?;
    if changed == 0 {
        return Err(storage(format!("账户不存在: {}", id.0)));
    }
    Ok(())
}

pub(crate) fn set_account_token_expiry_conn(
    conn: &Connection,
    id: AccountId,
    expires_at: Option<i64>,
) -> Result<(), CoreError> {
    let changed = conn
        .execute(
            "UPDATE accounts SET token_expires_at = ?2 WHERE id = ?1",
            params![id.0, expires_at],
        )
        .store()?;
    if changed == 0 {
        return Err(storage(format!("账户不存在: {}", id.0)));
    }
    Ok(())
}

pub(crate) fn set_account_realtime_conn(
    conn: &Connection,
    id: AccountId,
    idle_enabled: bool,
    poll_interval_secs: Option<u32>,
) -> Result<(), CoreError> {
    let changed = conn
        .execute(
            "UPDATE accounts SET idle_enabled = ?2, poll_interval_secs = ?3 WHERE id = ?1",
            params![
                id.0,
                i64::from(idle_enabled),
                poll_interval_secs.map(i64::from)
            ],
        )
        .store()?;
    if changed == 0 {
        return Err(storage(format!("账户不存在: {}", id.0)));
    }
    Ok(())
}

pub(crate) fn set_account_oauth_identity_conn(
    conn: &Connection,
    id: AccountId,
    provider: OAuthProvider,
    subject: Option<&str>,
) -> Result<(), CoreError> {
    let changed = conn
        .execute(
            "UPDATE accounts
                SET auth_method = 'oauth2', oauth_provider = ?2,
                    oauth_subject = ?3, needs_reauth = 0
              WHERE id = ?1",
            params![id.0, provider.as_str(), subject],
        )
        .store()?;
    if changed == 0 {
        return Err(storage(format!("账户不存在: {}", id.0)));
    }
    Ok(())
}

pub(crate) fn delete_account_conn(
    conn: &Connection,
    id: AccountId,
    delete_drafts: bool,
) -> Result<DeleteAccountOutcome, CoreError> {
    let drafts = count_drafts_conn(conn, id)?;
    if drafts > 0 && !delete_drafts {
        return Ok(DeleteAccountOutcome::RefusedDraftsExist(drafts));
    }
    // FTS 没有外键：账户级联删除 messages 前必须显式清理对应行。
    conn.execute(
        "DELETE FROM messages_fts
          WHERE rowid IN (SELECT id FROM messages WHERE account_id = ?1)",
        params![id.0],
    )
    .store()?;
    let changed = conn
        .execute("DELETE FROM accounts WHERE id = ?1", params![id.0])
        .store()?;
    if changed == 0 {
        Ok(DeleteAccountOutcome::NotFound)
    } else {
        Ok(DeleteAccountOutcome::Deleted)
    }
}

impl AccountRepo for WritableStore {
    fn list_accounts(&self) -> Result<Vec<Account>, CoreError> {
        let conn = self.lock();
        list_accounts_conn(&conn)
    }

    fn get_account(&self, id: AccountId) -> Result<Option<Account>, CoreError> {
        let conn = self.lock();
        get_account_conn(&conn, id)
    }

    fn create_account(&self, draft: &AccountDraft) -> Result<Account, CoreError> {
        let conn = self.lock();
        create_account_conn(&conn, draft)
    }

    fn update_account(&self, id: AccountId, draft: &AccountDraft) -> Result<Account, CoreError> {
        let conn = self.lock();
        update_account_conn(&conn, id, draft)
    }

    fn delete_account(
        &self,
        id: AccountId,
        delete_drafts: bool,
    ) -> Result<DeleteAccountOutcome, CoreError> {
        let conn = self.lock();
        delete_account_conn(&conn, id, delete_drafts)
    }

    fn count_drafts(&self, id: AccountId) -> Result<u32, CoreError> {
        let conn = self.lock();
        count_drafts_conn(&conn, id)
    }

    fn set_account_reauth(&self, id: AccountId, needs_reauth: bool) -> Result<(), CoreError> {
        let conn = self.lock();
        set_account_reauth_conn(&conn, id, needs_reauth)
    }

    fn set_account_token_expiry(
        &self,
        id: AccountId,
        expires_at: Option<i64>,
    ) -> Result<(), CoreError> {
        let conn = self.lock();
        set_account_token_expiry_conn(&conn, id, expires_at)
    }

    fn set_account_realtime(
        &self,
        id: AccountId,
        idle_enabled: bool,
        poll_interval_secs: Option<u32>,
    ) -> Result<(), CoreError> {
        let conn = self.lock();
        set_account_realtime_conn(&conn, id, idle_enabled, poll_interval_secs)
    }

    fn set_account_oauth_identity(
        &self,
        id: AccountId,
        provider: OAuthProvider,
        subject: Option<&str>,
    ) -> Result<(), CoreError> {
        let conn = self.lock();
        set_account_oauth_identity_conn(&conn, id, provider, subject)
    }
}
