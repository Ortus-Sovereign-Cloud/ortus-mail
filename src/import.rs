//! Mailbox import (Admin › Migrations): copies people's mailboxes from the
//! old server over IMAP. Each account is a job: it signs in, walks the
//! folders, and copies what it has not copied yet, so a retry continues
//! where the last try stopped. Failures are listed with their reason; an
//! administrator can fix the password and retry.
//!
//! Passwords are sealed, and forgotten once an account is done.

use std::sync::Arc;
use std::time::Duration;

use ortus_core::api::{answer, body, ok, ApiError, Method, Route};
use ortus_core::auth::Access;
use ortus_core::instance::Instance;
use ortus_core::jobs::Job;
use ortus_core::storage::{BoxFuture, StoreResult};
use rust_web_server::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::store;

/// The job that imports one account.
pub const IMPORT_JOB: &str = "mail.import";

const TIMEOUT: Duration = Duration::from_secs(60);
const MAX_MESSAGE: usize = 64 * 1024 * 1024;
/// One answer from the old server, literals and all.
const MAX_RESPONSE: usize = 128 * 1024 * 1024;

fn now() -> i64 {
    ortus_core::program::now_ms()
}

// --- A small IMAP client ---

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// TLS for the old server: the usual roots, and `ORTUS_IMPORT_CA` (a PEM
/// file) for a server with its own certificate authority.
fn tls() -> tokio_rustls::TlsConnector {
    use tokio_rustls::rustls;
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Ok(path) = std::env::var("ORTUS_IMPORT_CA") {
        use rustls::pki_types::pem::PemObject;
        if let Ok(certs) = rustls::pki_types::CertificateDer::pem_file_iter(&path) {
            for cert in certs.flatten() {
                let _ = roots.add(cert);
            }
        }
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("TLS versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

struct Client {
    reader: BufReader<Box<dyn Stream>>,
    tag: u32,
}

/// One untagged or tagged line, with the literals it carried.
struct Line {
    text: String,
    literals: Vec<Vec<u8>>,
}

impl Client {
    async fn connect(host: &str, port: u16, security: &str) -> Result<Self, String> {
        let tcp = tokio::time::timeout(TIMEOUT, tokio::net::TcpStream::connect((host, port)))
            .await
            .map_err(|_| "connect_timeout".to_string())?
            .map_err(|_| "connect_failed".to_string())?;
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.to_string())
            .map_err(|_| "bad_host".to_string())?;
        let stream: Box<dyn Stream> = if security == "tls" {
            Box::new(
                tls()
                    .connect(name.clone(), tcp)
                    .await
                    .map_err(|_| "tls_failed".to_string())?,
            )
        } else {
            Box::new(tcp)
        };
        let mut client = Self {
            reader: BufReader::new(stream),
            tag: 0,
        };
        let greeting = client.line().await?;
        if !greeting.text.starts_with("* OK") {
            return Err("not_imap".into());
        }
        if security == "starttls" {
            client.command("STARTTLS").await?;
            let plain = client.reader.into_inner();
            let secured = tls()
                .connect(name, plain)
                .await
                .map_err(|_| "tls_failed".to_string())?;
            client.reader = BufReader::new(Box::new(secured));
        }
        Ok(client)
    }

    /// Reads one response line and any literals in it.
    async fn line(&mut self) -> Result<Line, String> {
        let mut text = String::new();
        let mut literals: Vec<Vec<u8>> = Vec::new();
        let mut total = 0usize;
        loop {
            // A line may be long (a big SEARCH answer): read it all, in parts.
            let mut part = Vec::new();
            loop {
                let read = tokio::time::timeout(
                    TIMEOUT,
                    (&mut self.reader)
                        .take(1 << 20)
                        .read_until(b'\n', &mut part),
                )
                .await
                .map_err(|_| "timeout".to_string())?
                .map_err(|_| "connection_lost".to_string())?;
                if read == 0 {
                    return Err("connection_lost".into());
                }
                if part.len() > MAX_RESPONSE {
                    return Err("response_too_big".into());
                }
                if part.ends_with(b"\n") {
                    break;
                }
            }
            let part = String::from_utf8_lossy(&part)
                .trim_end_matches(['\r', '\n'])
                .to_string();
            let size = part
                .strip_suffix('}')
                .and_then(|p| p.rsplit_once('{'))
                .and_then(|(_, n)| n.trim_end_matches('+').parse::<usize>().ok());
            text.push_str(&part);
            let Some(size) = size else {
                return Ok(Line { text, literals });
            };
            total = total.saturating_add(size);
            if size > MAX_MESSAGE || total > MAX_RESPONSE {
                return Err("message_too_big".into());
            }
            // Read as it comes, not allocated up front.
            let mut data = Vec::new();
            tokio::time::timeout(
                TIMEOUT,
                (&mut self.reader).take(size as u64).read_to_end(&mut data),
            )
            .await
            .map_err(|_| "timeout".to_string())?
            .map_err(|_| "connection_lost".to_string())?;
            if data.len() != size {
                return Err("connection_lost".into());
            }
            literals.push(data);
            text.push('\u{0}');
        }
    }

    /// Sends a command; answers the untagged lines before its OK.
    async fn command(&mut self, command: &str) -> Result<Vec<Line>, String> {
        self.tag += 1;
        let tag = format!("i{}", self.tag);
        let writer = self.reader.get_mut();
        writer
            .write_all(format!("{tag} {command}\r\n").as_bytes())
            .await
            .map_err(|_| "connection_lost".to_string())?;
        writer
            .flush()
            .await
            .map_err(|_| "connection_lost".to_string())?;
        self.answer(&tag, command).await
    }

    /// The untagged lines of a command, up to its tagged answer.
    async fn answer(&mut self, tag: &str, command: &str) -> Result<Vec<Line>, String> {
        let mut lines = Vec::new();
        let mut size = 0usize;
        loop {
            let line = self.line().await?;
            if let Some(rest) = line.text.strip_prefix(&format!("{tag} ")) {
                return if rest.starts_with("OK") {
                    Ok(lines)
                } else if command.starts_with("LOGIN") || command.starts_with("AUTHENTICATE") {
                    Err("login_failed".into())
                } else {
                    Err(format!(
                        "refused: {}",
                        rest.chars().take(120).collect::<String>()
                    ))
                };
            }
            size = size.saturating_add(
                line.text.len() + line.literals.iter().map(Vec::len).sum::<usize>(),
            );
            if size > MAX_RESPONSE || lines.len() > 1_000_000 {
                return Err("response_too_big".into());
            }
            lines.push(line);
        }
    }

    /// Signs in: AUTHENTICATE PLAIN when the server offers it (any
    /// characters in the password), else LOGIN (plain ASCII only).
    async fn sign_in(&mut self, login: &str, password: &str) -> Result<(), String> {
        use base64::Engine;
        let caps = self
            .command("CAPABILITY")
            .await?
            .iter()
            .map(|l| l.text.to_ascii_uppercase())
            .collect::<Vec<_>>()
            .join(" ");
        if caps.contains("AUTH=PLAIN") {
            let response =
                base64::engine::general_purpose::STANDARD.encode(format!("\0{login}\0{password}"));
            self.tag += 1;
            let tag = format!("i{}", self.tag);
            let writer = self.reader.get_mut();
            writer
                .write_all(format!("{tag} AUTHENTICATE PLAIN\r\n").as_bytes())
                .await
                .map_err(|_| "connection_lost".to_string())?;
            writer
                .flush()
                .await
                .map_err(|_| "connection_lost".to_string())?;
            let first = self.line().await?;
            if !first.text.starts_with('+') {
                return Err("login_failed".into());
            }
            let writer = self.reader.get_mut();
            writer
                .write_all(format!("{response}\r\n").as_bytes())
                .await
                .map_err(|_| "connection_lost".to_string())?;
            writer
                .flush()
                .await
                .map_err(|_| "connection_lost".to_string())?;
            return self.answer(&tag, "AUTHENTICATE").await.map(|_| ());
        }
        if !login.is_ascii() || !password.is_ascii() {
            return Err("password_not_ascii".into());
        }
        self.command(&format!("LOGIN {} {}", quoted(login), quoted(password)))
            .await
            .map(|_| ())
    }
}

/// An IMAP quoted string.
fn quoted(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A folder from LIST: its name and special use.
fn parse_list(line: &str) -> Option<(String, Option<String>)> {
    let rest = line.strip_prefix("* LIST ")?;
    let (flags, rest) = rest.strip_prefix('(')?.split_once(')')?;
    if flags.to_ascii_lowercase().contains("\\noselect") {
        return None;
    }
    let rest = rest.trim();
    // The delimiter ("/", ".", or NIL), then the name.
    let rest = if let Some(r) = rest.strip_prefix("NIL") {
        r
    } else {
        rest.strip_prefix('"')?.split_once('"')?.1
    };
    let name = rest.trim();
    let name = if let Some(q) = name.strip_prefix('"') {
        q.strip_suffix('"')?
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        name.to_string()
    };
    let special = ["\\Sent", "\\Drafts", "\\Junk", "\\Trash", "\\Archive"]
        .iter()
        .find(|s| flags.split_whitespace().any(|f| f.eq_ignore_ascii_case(s)))
        .map(|s| s.trim_start_matches('\\').to_ascii_lowercase());
    Some((name, special))
}

/// Which of our folders an old one goes to.
fn role_for(name: &str, special: Option<&str>) -> Option<&'static str> {
    if name.eq_ignore_ascii_case("INBOX") {
        return Some("inbox");
    }
    if let Some(s) = special {
        return match s {
            "sent" => Some("sent"),
            "drafts" => Some("drafts"),
            "junk" => Some("junk"),
            "trash" => Some("trash"),
            "archive" => Some("archive"),
            _ => None,
        };
    }
    let last = name
        .rsplit(['/', '.'])
        .next()
        .unwrap_or(name)
        .to_ascii_lowercase();
    match last.as_str() {
        "sent" | "sent items" | "sent messages" | "sent mail" => Some("sent"),
        "drafts" => Some("drafts"),
        "junk" | "spam" | "junk e-mail" | "junk email" => Some("junk"),
        "trash" | "deleted items" | "deleted messages" | "bin" => Some("trash"),
        "archive" | "archives" => Some("archive"),
        _ => None,
    }
}

/// The flags of a FETCH line we keep.
fn fetched_flags(text: &str) -> Vec<String> {
    let Some((_, rest)) = text.split_once("FLAGS (") else {
        return Vec::new();
    };
    let inner = rest.split(')').next().unwrap_or_default();
    inner
        .split_whitespace()
        .filter(|f| {
            ["\\Seen", "\\Answered", "\\Flagged", "\\Draft"]
                .iter()
                .any(|k| k.eq_ignore_ascii_case(f))
                || (!f.starts_with('\\') && !f.is_empty())
        })
        .map(str::to_string)
        .collect()
}

fn fetched_uid(text: &str) -> Option<i64> {
    let (_, rest) = text.split_once("UID ")?;
    rest.split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

// --- Store ---

#[derive(Clone, Debug, Serialize)]
pub struct Account {
    pub id: String,
    pub login: String,
    pub target: String,
    pub state: String,
    pub error: Option<String>,
    pub copied: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Import {
    pub id: String,
    pub host: String,
    pub port: i64,
    pub security: String,
    pub created_at: i64,
    pub accounts: Vec<Account>,
}

pub async fn list(instance: &Instance) -> StoreResult<Vec<Import>> {
    let store = instance.store.as_ref();
    let mut out = Vec::new();
    for r in store.query("SELECT id, host, port, security, created_at FROM mail_imports ORDER BY created_at DESC", &[]).await? {
        let id = r.text(0).unwrap_or_default().to_string();
        let accounts = store
            .query(
                "SELECT a.id, a.login, COALESCE(u.email, a.target_user), a.state, a.error, a.copied, a.updated_at FROM mail_import_accounts a LEFT JOIN core_users u ON u.id = a.target_user WHERE a.import_id = $1 ORDER BY a.login",
                &[id.as_str().into()],
            )
            .await?
            .iter()
            .map(|a| Account {
                id: a.text(0).unwrap_or_default().into(),
                login: a.text(1).unwrap_or_default().into(),
                target: a.text(2).unwrap_or_default().into(),
                state: a.text(3).unwrap_or_default().into(),
                error: a.text(4).map(str::to_string),
                copied: a.int(5).unwrap_or(0),
                updated_at: a.int(6).unwrap_or(0),
            })
            .collect();
        out.push(Import { id, host: r.text(1).unwrap_or_default().into(), port: r.int(2).unwrap_or(0), security: r.text(3).unwrap_or_default().into(), created_at: r.int(4).unwrap_or(0), accounts });
    }
    Ok(out)
}

async fn queue(tx: &mut dyn ortus_core::storage::AsyncTx, account: &str) -> StoreResult<()> {
    let payload = json!({ "account": account }).to_string();
    ortus_core::jobs::enqueue_in(
        tx,
        &ortus_core::jobs::NewJob {
            id: &format!("mail-import-{account}-{}", ortus_core::auth::random_hex(4)),
            kind: IMPORT_JOB,
            payload: &payload,
            run_at: now(),
            max_attempts: 1,
        },
        now(),
    )
    .await
    .map(|_| ())
}

/// Runs the import of one account (the job). Failures are recorded on the
/// account; the job itself does not retry (an administrator does).
pub async fn run(instance: &Arc<Instance>, job: &Job) -> Result<(), String> {
    let payload: Value = serde_json::from_str(&job.payload).map_err(|e| e.to_string())?;
    let account = payload["account"].as_str().unwrap_or_default().to_string();
    let store = instance.store.as_ref();
    let rows = store
        .query(
            "SELECT a.login, a.password, a.target_user, i.host, i.port, i.security FROM mail_import_accounts a JOIN mail_imports i ON i.id = a.import_id WHERE a.id = $1 AND a.state = 'queued'",
            &[account.as_str().into()],
        )
        .await
        .map_err(|e| e.to_string())?;
    let Some(r) = rows.first() else { return Ok(()) };
    let (login, sealed, target, host, port, security) = (
        r.text(0).unwrap_or_default().to_string(),
        r.text(1).unwrap_or_default().to_string(),
        r.text(2).unwrap_or_default().to_string(),
        r.text(3).unwrap_or_default().to_string(),
        r.int(4).unwrap_or(993),
        r.text(5).unwrap_or_default().to_string(),
    );
    let set = |state: &'static str, error: Option<String>| {
        let account = account.clone();
        async move {
            store
                .execute("UPDATE mail_import_accounts SET state = $1, error = $2, updated_at = $3 WHERE id = $4", &[state.into(), error.into(), now().into(), account.as_str().into()])
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
    };
    set("running", None).await?;
    // A sealed password only: a plain value here is never used.
    let password = if sealed.starts_with("enc:v1:") {
        instance.keys.open_text(&sealed).ok()
    } else {
        None
    };
    let Some(password) = password else {
        return set("failed", Some("password_unreadable".into())).await;
    };
    ortus_core::outbound::note(store, "mail_import", &host).await;
    // The copy runs on its own: one account can take hours, and the job
    // runner has other work (outgoing mail) waiting.
    let instance = Arc::clone(instance);
    tokio::spawn(async move {
        let result = copy_account(
            &instance,
            &account,
            &login,
            &password,
            &target,
            &host,
            port as u16,
            &security,
        )
        .await;
        let store = instance.store.as_ref();
        let (state, error, forget) = match result {
            Ok(()) => ("done", None, true),
            Err(e) => ("failed", Some(e), false),
        };
        let sql = if forget {
            "UPDATE mail_import_accounts SET state = $1, error = $2, password = NULL, updated_at = $3 WHERE id = $4"
        } else {
            "UPDATE mail_import_accounts SET state = $1, error = $2, updated_at = $3 WHERE id = $4"
        };
        if let Err(e) = store
            .execute(
                sql,
                &[
                    state.into(),
                    error.into(),
                    now().into(),
                    account.as_str().into(),
                ],
            )
            .await
        {
            eprintln!("ortus: mail: import {account}: {e}");
        }
    });
    Ok(())
}

/// The job that notices imports that stopped (the program restarted in
/// the middle): they fail, so they can be retried.
pub const IMPORT_WATCH_JOB: &str = "mail.import-watch";

/// An import that copied nothing for this long has stopped.
const STALLED: i64 = 15 * 60 * 1000;

pub async fn watch(instance: &Instance) -> Result<(), String> {
    instance
        .store
        .execute(
            "UPDATE mail_import_accounts SET state = 'failed', error = 'interrupted' WHERE state = 'running' AND updated_at < $1",
            &[(now() - STALLED).into()],
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[allow(clippy::too_many_arguments)]
async fn copy_account(
    instance: &Instance,
    account: &str,
    login: &str,
    password: &str,
    target: &str,
    host: &str,
    port: u16,
    security: &str,
) -> Result<(), String> {
    let store = instance.store.as_ref();
    let mut client = Client::connect(host, port, security).await?;
    client.sign_in(login, password).await?;
    let folders: Vec<(String, Option<String>)> = client
        .command("LIST \"\" \"*\"")
        .await?
        .iter()
        .filter_map(|l| {
            // A name the server sent as a literal.
            let (name_literal, text) = (l.literals.first(), &l.text);
            parse_list(text).map(|(name, special)| match (name.as_str(), name_literal) {
                ("\u{0}", Some(bytes)) => (String::from_utf8_lossy(bytes).into_owned(), special),
                _ => (name, special),
            })
        })
        .collect();
    for (name, special) in folders {
        // Our folder for it: a role's, or one of the person's own by name.
        let mailbox = match role_for(&name, special.as_deref()) {
            Some(role) => store::by_role(store, target, role)
                .await
                .map_err(|e| e.to_string())?,
            None => {
                let own = name.replace('.', "/");
                match store::create_mailbox(store, target, &own)
                    .await
                    .map_err(|e| e.to_string())?
                {
                    Some(m) => m,
                    None => store::mailboxes(store, target)
                        .await
                        .map_err(|e| e.to_string())?
                        .into_iter()
                        .find(|m| m.name == own)
                        .ok_or("folder_failed")?,
                }
            }
        };
        let selected = client
            .command(&format!("EXAMINE {}", quoted(&name)))
            .await?;
        let validity = selected
            .iter()
            .find_map(|l| {
                l.text
                    .split_once("[UIDVALIDITY ")
                    .and_then(|(_, r)| r.split(']').next())
                    .and_then(|v| v.trim().parse::<i64>().ok())
            })
            .unwrap_or(0);
        let exists = selected
            .iter()
            .find_map(|l| {
                l.text
                    .strip_prefix("* ")
                    .and_then(|r| r.strip_suffix(" EXISTS"))
                    .and_then(|n| n.parse::<i64>().ok())
            })
            .unwrap_or(0);
        if exists == 0 {
            continue;
        }
        // What this account copied from this folder before.
        let done: i64 = store
            .query("SELECT CAST(COALESCE(MAX(uid), 0) AS BIGINT) FROM mail_import_seen WHERE account = $1 AND folder = $2 AND validity = $3", &[account.into(), name.as_str().into(), validity.into()])
            .await
            .map_err(|e| e.to_string())?
            .first()
            .and_then(|r| r.int(0))
            .unwrap_or(0);
        let uids: Vec<i64> = client
            .command(&format!("UID SEARCH UID {}:*", done + 1))
            .await?
            .iter()
            .filter_map(|l| l.text.strip_prefix("* SEARCH"))
            .flat_map(|r| {
                r.split_whitespace()
                    .filter_map(|n| n.parse::<i64>().ok())
                    .collect::<Vec<_>>()
            })
            .filter(|u| *u > done)
            .collect();
        for uid in uids {
            let lines = client
                .command(&format!("UID FETCH {uid} (UID FLAGS BODY.PEEK[])"))
                .await?;
            let Some(line) = lines.iter().find(|l| {
                l.text.contains("FETCH")
                    && !l.literals.is_empty()
                    && fetched_uid(&l.text) == Some(uid)
            }) else {
                continue;
            };
            let flags = fetched_flags(&line.text);
            let refs: Vec<&str> = flags.iter().map(String::as_str).collect();
            store::deliver(
                instance,
                target,
                &mailbox.id,
                &line.literals[0],
                &refs,
                None,
                0,
            )
            .await
            .map_err(|e| format!("store: {e}"))?;
            store
                .execute("INSERT INTO mail_import_seen (account, folder, validity, uid) VALUES ($1, $2, $3, $4)", &[account.into(), name.as_str().into(), validity.into(), uid.into()])
                .await
                .map_err(|e| e.to_string())?;
            store
                .execute("UPDATE mail_import_accounts SET copied = copied + 1, updated_at = $1 WHERE id = $2", &[now().into(), account.into()])
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    let _ = client.command("LOGOUT").await;
    Ok(())
}

// --- Routes ---

#[derive(Deserialize)]
struct NewImport {
    host: String,
    port: Option<i64>,
    /// tls (993) or starttls (143)
    security: Option<String>,
    accounts: Vec<NewAccount>,
}

#[derive(Deserialize)]
struct NewAccount {
    login: String,
    password: String,
    /// The Ortus address the mail goes to.
    to: String,
}

fn bad(message: &str) -> ApiError {
    ApiError::bad_request(message.to_string())
}

async fn start(request: &Request, instance: &Arc<Instance>) -> Result<Response, ApiError> {
    let me = ortus_core::auth::current(request, instance)
        .await
        .ok_or_else(ApiError::sign_in)?;
    let input: NewImport = body(request)?;
    let host = input.host.trim().to_ascii_lowercase();
    if host.is_empty()
        || host.len() > 253
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err(bad("That is not a server name."));
    }
    let security = match input.security.as_deref().unwrap_or("tls") {
        s @ ("tls" | "starttls") => s,
        _ => {
            return Err(bad(
                "Security is tls or starttls; mail passwords never go in the clear.",
            ))
        }
    };
    let port = input
        .port
        .unwrap_or(if security == "tls" { 993 } else { 143 });
    if !(1..=65535).contains(&port) || input.accounts.is_empty() || input.accounts.len() > 5000 {
        return Err(bad("Give the port and at least one account."));
    }
    let store = instance.store.as_ref();
    let mut targets = Vec::new();
    for a in &input.accounts {
        let user = ortus_core::auth::accounts::by_email(store, a.to.trim())
            .await?
            .ok_or_else(|| {
                bad(&format!(
                    "There is no one here with the address {}.",
                    a.to.trim()
                ))
            })?;
        if a.login.trim().is_empty() || a.password.is_empty() {
            return Err(bad("Every account needs its login and password."));
        }
        if a.login
            .chars()
            .chain(a.password.chars())
            .any(char::is_control)
        {
            return Err(bad("A login or password has a control character in it."));
        }
        targets.push(user.id);
    }
    let id = format!("im_{}", ortus_core::auth::random_hex(12));
    let mut tx = store.begin().await?;
    tx.execute(
        "INSERT INTO mail_imports (id, host, port, security, created_by, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
        &[id.as_str().into(), host.as_str().into(), port.into(), security.into(), me.user.id.as_str().into(), now().into()],
    )
    .await?;
    for (a, target) in input.accounts.iter().zip(targets) {
        let account = format!("ia_{}", ortus_core::auth::random_hex(12));
        tx.execute(
            "INSERT INTO mail_import_accounts (id, import_id, login, password, target_user, state, copied, updated_at) VALUES ($1, $2, $3, $4, $5, 'queued', 0, $6)",
            &[account.as_str().into(), id.as_str().into(), a.login.trim().into(), instance.keys.seal_text(&a.password).into(), target.as_str().into(), now().into()],
        )
        .await?;
        queue(&mut *tx, &account).await?;
    }
    tx.commit().await?;
    ortus_core::auth::audit(
        instance,
        ortus_core::audit::Event::new("mail.import_started", ortus_core::audit::Outcome::Done)
            .by(&me.user.id)
            .on(&host)
            .with(json!({"accounts": input.accounts.len()})),
    )
    .await;
    Ok(ok(&json!({ "id": id })))
}

#[derive(Deserialize)]
struct Retry {
    /// A new password, when the old one was wrong.
    password: Option<String>,
}

async fn retry(
    request: &Request,
    params: &PathParams,
    instance: &Arc<Instance>,
) -> Result<Response, ApiError> {
    let me = ortus_core::auth::current(request, instance)
        .await
        .ok_or_else(ApiError::sign_in)?;
    let account = params.get("account").unwrap_or_default();
    let input: Retry = body(request)?;
    let store = instance.store.as_ref();
    let mut tx = store.begin().await?;
    if input
        .password
        .as_deref()
        .is_some_and(|p| p.chars().any(char::is_control))
    {
        return Err(bad("A password has a control character in it."));
    }
    let changed = match input.password.filter(|p| !p.is_empty()) {
        Some(p) => tx.execute("UPDATE mail_import_accounts SET state = 'queued', error = NULL, password = $1, updated_at = $2 WHERE id = $3 AND state = 'failed'", &[instance.keys.seal_text(&p).into(), now().into(), account.into()]).await?,
        None => tx.execute("UPDATE mail_import_accounts SET state = 'queued', error = NULL, updated_at = $1 WHERE id = $2 AND state = 'failed' AND password IS NOT NULL", &[now().into(), account.into()]).await?,
    };
    if changed != 1 {
        tx.rollback().await?;
        return Err(ApiError {
            status: 409,
            code: "not_failed",
            message: "Only a failed account can be retried.".into(),
        });
    }
    queue(&mut *tx, account).await?;
    tx.commit().await?;
    ortus_core::auth::audit(
        instance,
        ortus_core::audit::Event::new("mail.import_retried", ortus_core::audit::Outcome::Done)
            .by(&me.user.id)
            .on(account),
    )
    .await;
    Ok(ok(&json!({ "queued": true })))
}

/// The note asking an owner for their old password, in their language.
fn reconnect_text(language: Option<&str>, host: &str, login: &str, link: &str) -> (String, String) {
    match language.unwrap_or("en") {
        "pl" => ("Twoja stara skrzynka potrzebuje hasła".into(), format!("Nie udało się skopiować poczty z {host}: serwer odrzucił hasło konta {login}.\n\nPodaj hasło starego serwera tutaj, a kopiowanie ruszy dalej:\n{link}\n")),
        "uk" => ("Ваша стара скринька потребує пароля".into(), format!("Не вдалося скопіювати пошту з {host}: сервер не прийняв пароль облікового запису {login}.\n\nВведіть пароль старого сервера тут, і копіювання продовжиться:\n{link}\n")),
        "de" => ("Ihr altes Postfach braucht Ihr Passwort".into(), format!("Ihre E-Mails von {host} konnten nicht kopiert werden: Der Server hat das Passwort für {login} abgelehnt.\n\nGeben Sie hier das Passwort des alten Servers ein, dann geht das Kopieren weiter:\n{link}\n")),
        "fr" => ("Votre ancienne boîte a besoin de votre mot de passe".into(), format!("Votre courrier de {host} n’a pas pu être copié : le serveur a refusé le mot de passe de {login}.\n\nSaisissez ici le mot de passe de l’ancien serveur et la copie reprendra :\n{link}\n")),
        "ar" => ("صندوق بريدك القديم يحتاج إلى كلمة المرور".into(), format!("تعذّر نسخ بريدك من {host}: رفض الخادم كلمة مرور الحساب {login}.\n\nأدخل كلمة مرور الخادم القديم هنا ليستأنف النسخ:\n{link}\n")),
        _ => ("Your old mailbox needs your password".into(), format!("Your mail from {host} could not be copied: the server refused the password for {login}.\n\nEnter the old server's password here and copying goes on:\n{link}\n")),
    }
}

#[derive(Deserialize)]
struct AskOwners {
    accounts: Vec<String>,
}

/// Asks the owners of failed accounts for their old password: a message
/// in their Ortus inbox with a link to the page where they give it.
async fn ask_owners(request: &Request, instance: &Arc<Instance>) -> Result<Response, ApiError> {
    let me = ortus_core::auth::current(request, instance)
        .await
        .ok_or_else(ApiError::sign_in)?;
    let input: AskOwners = body(request)?;
    let store = instance.store.as_ref();
    let mut asked = 0;
    for account in input.accounts.iter().take(5000) {
        let rows = store
            .query(
                "SELECT a.target_user, a.login, i.host FROM mail_import_accounts a JOIN mail_imports i ON i.id = a.import_id WHERE a.id = $1 AND a.state = 'failed'",
                &[account.as_str().into()],
            )
            .await?;
        let Some(r) = rows.first() else { continue };
        let (target, login, host) = (
            r.text(0).unwrap_or_default().to_string(),
            r.text(1).unwrap_or_default().to_string(),
            r.text(2).unwrap_or_default().to_string(),
        );
        let Some(user) = ortus_core::auth::accounts::by_id(store, &target).await? else {
            continue;
        };
        let link = format!(
            "{}/mail/reconnect/{account}",
            instance.config.public_url.trim_end_matches('/')
        );
        let (subject, text) = reconnect_text(user.language.as_deref(), &host, &login, &link);
        let domain = user
            .email
            .rsplit_once('@')
            .map_or("ortus.local", |(_, d)| d);
        let encoded = |t: &str| {
            use base64::Engine;
            format!(
                "=?UTF-8?B?{}?=",
                base64::engine::general_purpose::STANDARD.encode(t)
            )
        };
        let raw = format!(
            "From: Ortus <postmaster@{domain}>\r\nTo: <{}>\r\nSubject: {}\r\nMessage-ID: <reconnect-{}@{domain}>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{}",
            user.email,
            encoded(&subject),
            ortus_core::auth::random_hex(8),
            text.replace('\n', "\r\n")
        );
        let inbox = store::by_role(store, &target, "inbox").await?;
        store::deliver(
            instance,
            &target,
            &inbox.id,
            raw.as_bytes(),
            &[],
            Some(&json!({"internal": true})),
            0,
        )
        .await?;
        asked += 1;
    }
    ortus_core::auth::audit(
        instance,
        ortus_core::audit::Event::new("mail.import_owners_asked", ortus_core::audit::Outcome::Done)
            .by(&me.user.id)
            .with(json!({"accounts": asked})),
    )
    .await;
    Ok(ok(&json!({ "asked": asked })))
}

/// The signed-in person's own failed imports.
async fn mine(request: &Request, instance: &Arc<Instance>) -> Result<Response, ApiError> {
    let me = ortus_core::auth::current(request, instance)
        .await
        .ok_or_else(ApiError::sign_in)?;
    let rows = instance
        .store
        .query(
            "SELECT a.id, a.login, i.host, a.state, a.error, a.copied FROM mail_import_accounts a JOIN mail_imports i ON i.id = a.import_id WHERE a.target_user = $1 ORDER BY a.updated_at DESC",
            &[me.user.id.as_str().into()],
        )
        .await?;
    let list: Vec<Value> = rows.iter().map(|r| json!({"id": r.text(0), "login": r.text(1), "host": r.text(2), "state": r.text(3), "error": r.text(4), "copied": r.int(5)})).collect();
    Ok(ok(&json!({ "accounts": list })))
}

/// The owner gives the old password themselves.
async fn retry_mine(
    request: &Request,
    params: &PathParams,
    instance: &Arc<Instance>,
) -> Result<Response, ApiError> {
    let me = ortus_core::auth::current(request, instance)
        .await
        .ok_or_else(ApiError::sign_in)?;
    let account = params.get("account").unwrap_or_default();
    let input: Retry = body(request)?;
    let Some(password) = input
        .password
        .filter(|p| !p.is_empty() && !p.chars().any(char::is_control))
    else {
        return Err(bad("Give the old server's password."));
    };
    let mut tx = instance.store.begin().await?;
    let changed = tx
        .execute(
            "UPDATE mail_import_accounts SET state = 'queued', error = NULL, password = $1, updated_at = $2 WHERE id = $3 AND target_user = $4 AND state = 'failed'",
            &[instance.keys.seal_text(&password).into(), now().into(), account.into(), me.user.id.as_str().into()],
        )
        .await?;
    if changed != 1 {
        tx.rollback().await?;
        return Err(ApiError::not_found(
            "There is no failed import of yours with that id.",
        ));
    }
    queue(&mut *tx, account).await?;
    tx.commit().await?;
    Ok(ok(&json!({ "queued": true })))
}

fn h_ask(
    request: Request,
    _: PathParams,
    _: ConnectionInfo,
    instance: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move { answer(&request, ask_owners(&request, &instance).await) })
}

fn h_mine(
    request: Request,
    _: PathParams,
    _: ConnectionInfo,
    instance: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move { answer(&request, mine(&request, &instance).await) })
}

fn h_retry_mine(
    request: Request,
    params: PathParams,
    _: ConnectionInfo,
    instance: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move { answer(&request, retry_mine(&request, &params, &instance).await) })
}

fn h_list(
    request: Request,
    _: PathParams,
    _: ConnectionInfo,
    instance: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move {
        let result: Result<Response, ApiError> =
            async { Ok(ok(&json!({ "imports": list(&instance).await? }))) }.await;
        answer(&request, result)
    })
}

fn h_start(
    request: Request,
    _: PathParams,
    _: ConnectionInfo,
    instance: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move { answer(&request, start(&request, &instance).await) })
}

fn h_retry(
    request: Request,
    params: PathParams,
    _: ConnectionInfo,
    instance: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move { answer(&request, retry(&request, &params, &instance).await) })
}

fn any_object() -> Value {
    json!({"type": "object"})
}

pub fn routes() -> Vec<Route> {
    let route = |method, path, id, summary, handler| Route {
        method,
        path,
        id,
        tag: "mail",
        summary,
        access: Access::Admin,
        response: any_object,
        handler,
    };
    let own = |method, path, id, summary, handler| Route {
        method,
        path,
        id,
        tag: "mail",
        summary,
        access: Access::Signed,
        response: any_object,
        handler,
    };
    vec![
        route(Method::Post, "/mail/admin/imports/ask-owners", "askImportOwners", "Ask the owners of failed accounts for their old password: {accounts: [id]}. A message with a link goes to each one's inbox.", h_ask),
        own(Method::Get, "/mail/imports/mine", "myMailImports", "The signed-in person's mailbox imports.", h_mine),
        own(Method::Post, "/mail/imports/mine/:account/retry", "retryMyMailImport", "Retry one's own failed import with the old server's password: {password}.", h_retry_mine),
        route(Method::Get, "/mail/admin/imports", "mailImports", "Mailbox imports from old servers, with each account's progress and failures.", h_list as ortus_core::api::Handler),
        route(Method::Post, "/mail/admin/imports", "startMailImport", "Import mailboxes over IMAP: {host, port, security (tls or starttls), accounts: [{login, password, to}]}.", h_start),
        route(Method::Post, "/mail/admin/imports/accounts/:account/retry", "retryMailImport", "Retry a failed account, with a new password if the old one was wrong: {password}.", h_retry),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_are_read_and_mapped() {
        assert_eq!(
            Some(("INBOX".into(), None)),
            parse_list("* LIST (\\HasNoChildren) \"/\" INBOX")
        );
        assert_eq!(
            Some(("Sent Items".into(), Some("sent".into()))),
            parse_list("* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent Items\"")
        );
        assert_eq!(
            Some(("Projects.2026".into(), None)),
            parse_list("* LIST () \".\" Projects.2026")
        );
        assert_eq!(None, parse_list("* LIST (\\Noselect) \"/\" \"[Gmail]\""));
        assert_eq!(Some("sent"), role_for("Sent Items", None));
        assert_eq!(Some("trash"), role_for("INBOX.Deleted Items", None));
        assert_eq!(Some("junk"), role_for("[Gmail]/Spam", Some("junk")));
        assert_eq!(None, role_for("Projects/2026", None));
    }

    #[test]
    fn fetch_lines_are_read() {
        let line = "* 3 FETCH (UID 17 FLAGS (\\Seen \\Recent $Forwarded flood) BODY[] {120}\u{0})";
        assert_eq!(Some(17), fetched_uid(line));
        assert_eq!(vec!["\\Seen", "$Forwarded", "flood"], fetched_flags(line));
        assert_eq!("\"a\\\"b\\\\c\"", quoted("a\"b\\c"));
    }
}
