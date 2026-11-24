//! IMAP for mail apps (Thunderbird, Apple Mail, Outlook, phones): the
//! subset real clients use, over TLS, with app passwords.
//!
//! Port 993 (TLS from the start, when a certificate is set) and 143
//! (STARTTLS; no password before it): `ORTUS_IMAPS_LISTEN`,
//! `ORTUS_IMAP_LISTEN`. Capabilities: IMAP4rev1, IDLE, UIDPLUS, MOVE,
//! UNSELECT, LITERAL+, SPECIAL-USE, ENABLE, CONDSTORE (modification
//! sequences in FETCH and SELECT).

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use mail_parser::{AddressList, MessageParser};
use ortus_core::instance::Instance;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use crate::store::{self, Mailbox};

const MAX_LINE: usize = 16_384;
const MAX_LITERAL: usize = 64 * 1024 * 1024;
const IDLE_CHECK: Duration = Duration::from_secs(5);
const INACTIVITY: Duration = Duration::from_secs(30 * 60);
/// Connections at once on one port.
const MAX_CONNECTIONS: usize = 1000;
/// Connections at once from one address (an IPv6 /64 is one). Mail apps
/// keep a few each (one waiting per folder), and an office behind one NAT
/// shares an address, so this is generous; it stops one machine taking
/// every connection.
pub const PER_ADDRESS: usize = 100;
/// A command, literals included, before signing in.
const MAX_BEFORE_SIGN_IN: usize = 64 * 1024;
const HANDSHAKE: Duration = Duration::from_secs(30);

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

type Reader = BufReader<Box<dyn Stream>>;

/// The role task: IMAP on 143 (STARTTLS), and 993 (TLS) with a certificate.
pub fn run(
    instance: Arc<Instance>,
    mut stop: ortus_core::app::Stop,
) -> ortus_core::storage::BoxFuture<'static, Result<(), String>> {
    Box::pin(async move {
        let tls = crate::smtp::tls_config();
        let plain = std::env::var("ORTUS_IMAP_LISTEN").unwrap_or_else(|_| "0.0.0.0:143".into());
        let listener = TcpListener::bind(&plain)
            .await
            .map_err(|e| format!("IMAP on {plain}: {e}"))?;
        println!("ortus: mail: IMAP on {plain}");
        if tls.is_some() {
            let secure =
                std::env::var("ORTUS_IMAPS_LISTEN").unwrap_or_else(|_| "0.0.0.0:993".into());
            let tls_listener = TcpListener::bind(&secure)
                .await
                .map_err(|e| format!("IMAP on {secure}: {e}"))?;
            println!("ortus: mail: IMAP with TLS on {secure}");
            let (instance2, mut stop2) = (Arc::clone(&instance), stop.clone());
            tokio::spawn(async move {
                let _ = serve(instance2, tls_listener, &mut stop2, true).await;
            });
        }
        serve(instance, listener, &mut stop, false).await
    })
}

/// Accepts IMAP connections until `stop` turns true; `implicit_tls` for 993.
pub async fn serve(
    instance: Arc<Instance>,
    listener: TcpListener,
    stop: &mut ortus_core::app::Stop,
    implicit_tls: bool,
) -> Result<(), String> {
    let tls = crate::smtp::tls_config();
    let open = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let per_address: crate::smtp::Addresses = Arc::default();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((socket, peer)) = accepted else {
                    // Out of file descriptors, say: wait instead of spinning.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                };
                let Ok(permit) = Arc::clone(&open).try_acquire_owned() else { continue };
                let Some(slot) = crate::smtp::Slot::take(&per_address, peer.ip(), PER_ADDRESS) else { continue };
                let (instance, tls) = (Arc::clone(&instance), tls.clone());
                tokio::spawn(async move {
                    let _permit = permit;
                    let _slot = slot;
                    if let Err(e) = Session::new(instance, peer, tls).run(socket, implicit_tls).await {
                        if !e.contains("went away") && !e.contains("close_notify") {
                            eprintln!("ortus: mail: IMAP from {peer}: {e}");
                        }
                    }
                });
            }
            _ = stop.changed() => return Ok(()),
        }
    }
}

// --- Parsing ---

/// One argument of a command.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Atom(String),
    Text(Vec<u8>),
    List(Vec<Token>),
}

impl Token {
    fn text(&self) -> String {
        match self {
            Token::Atom(a) => a.clone(),
            Token::Text(t) => String::from_utf8_lossy(t).into_owned(),
            Token::List(_) => String::new(),
        }
    }

    fn bytes(&self) -> Vec<u8> {
        match self {
            Token::Atom(a) => a.as_bytes().to_vec(),
            Token::Text(t) => t.clone(),
            Token::List(_) => Vec::new(),
        }
    }
}

/// Reads one command (a line, with any literals in it) as raw bytes, at
/// most `limit` bytes in all. Literals `{n}` are acknowledged with a
/// continuation, `{n+}` are not.
async fn read_command(reader: &mut Reader, limit: usize) -> Result<Option<Vec<u8>>, String> {
    let mut command = Vec::new();
    loop {
        let mut line = Vec::new();
        let read = (&mut *reader)
            .take(MAX_LINE as u64 + 2)
            .read_until(b'\n', &mut line)
            .await
            .map_err(|e| e.to_string())?;
        if read == 0 {
            return Ok(None);
        }
        if !line.ends_with(b"\n") {
            return Err("line too long".into());
        }
        while line.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
            line.pop();
        }
        // A literal ends the line: {n} or {n+}.
        let literal = (|| {
            if !line.ends_with(b"}") {
                return None;
            }
            let start = line.iter().rposition(|b| *b == b'{')?;
            let inner = std::str::from_utf8(&line[start + 1..line.len() - 1]).ok()?;
            let (number, plus) = inner
                .strip_suffix('+')
                .map_or((inner, false), |n| (n, true));
            Some((number.parse::<usize>().ok()?, plus))
        })();
        command.extend_from_slice(&line);
        let Some((size, plus)) = literal else {
            return Ok(Some(command));
        };
        if size > MAX_LITERAL || command.len().saturating_add(size) > limit {
            return Err("literal too big".into());
        }
        if !plus {
            reader
                .get_mut()
                .write_all(b"+ Ready\r\n")
                .await
                .map_err(|e| e.to_string())?;
            reader.get_mut().flush().await.map_err(|e| e.to_string())?;
        }
        command.extend_from_slice(b"\r\n");
        let before = command.len();
        (&mut *reader)
            .take(size as u64)
            .read_to_end(&mut command)
            .await
            .map_err(|e| e.to_string())?;
        if command.len() - before != size {
            return Err("the client went away".into());
        }
    }
}

/// Splits a command into tokens. Literals appear in the raw command as
/// `{n}\r\n<n bytes>`.
fn tokens(input: &[u8]) -> Result<Vec<Token>, String> {
    fn parse(input: &[u8], at: &mut usize, depth: usize) -> Result<Vec<Token>, String> {
        let mut out = Vec::new();
        while *at < input.len() {
            match input[*at] {
                b' ' => *at += 1,
                b')' => {
                    if depth == 0 {
                        return Err("unbalanced )".into());
                    }
                    *at += 1;
                    return Ok(out);
                }
                b'(' => {
                    *at += 1;
                    if depth > 8 {
                        return Err("nested too deep".into());
                    }
                    out.push(Token::List(parse(input, at, depth + 1)?));
                }
                b'"' => {
                    *at += 1;
                    let mut text = Vec::new();
                    loop {
                        let Some(&b) = input.get(*at) else {
                            return Err("open quote".into());
                        };
                        *at += 1;
                        match b {
                            b'\\' => {
                                text.push(*input.get(*at).ok_or("open quote")?);
                                *at += 1;
                            }
                            b'"' => break,
                            _ => text.push(b),
                        }
                    }
                    out.push(Token::Text(text));
                }
                b'{' => {
                    let close = input[*at..]
                        .iter()
                        .position(|b| *b == b'}')
                        .ok_or("bad literal")?
                        + *at;
                    let size: usize = std::str::from_utf8(&input[*at + 1..close])
                        .ok()
                        .and_then(|n| n.trim_end_matches('+').parse().ok())
                        .ok_or("bad literal")?;
                    let start = close + 3; // }\r\n
                    let end = start.checked_add(size).ok_or("bad literal")?;
                    if end > input.len() {
                        return Err("short literal".into());
                    }
                    out.push(Token::Text(input[start..end].to_vec()));
                    *at = end;
                }
                _ => {
                    // An atom; brackets keep their spaces (BODY[HEADER.FIELDS (A B)]).
                    let start = *at;
                    let mut brackets = 0usize;
                    while *at < input.len() {
                        let b = input[*at];
                        if b == b'[' {
                            brackets += 1;
                        } else if b == b']' {
                            brackets = brackets.saturating_sub(1);
                        } else if brackets == 0 && (b == b' ' || b == b'(' || b == b')') {
                            break;
                        }
                        *at += 1;
                    }
                    out.push(Token::Atom(
                        String::from_utf8_lossy(&input[start..*at]).into_owned(),
                    ));
                }
            }
        }
        if depth > 0 {
            return Err("unclosed (".into());
        }
        Ok(out)
    }
    let mut at = 0;
    parse(input, &mut at, 0)
}

/// A sequence set (`1:5,7,9:*`) as inclusive ranges; `*` is `max`.
fn sequence_set(set: &str, max: i64) -> Option<Vec<(i64, i64)>> {
    let value = |v: &str| {
        if v == "*" {
            Some(max)
        } else {
            v.parse::<i64>().ok().filter(|n| *n > 0)
        }
    };
    set.split(',')
        .map(|part| match part.split_once(':') {
            Some((a, b)) => {
                let (a, b) = (value(a)?, value(b)?);
                Some((a.min(b), a.max(b)))
            }
            None => value(part).map(|v| (v, v)),
        })
        .collect()
}

fn contains(ranges: &[(i64, i64)], n: i64) -> bool {
    ranges.iter().any(|(a, b)| *a <= n && n <= *b)
}

// --- Writing ---

/// An IMAP string: quoted when simple, a literal otherwise.
fn string(value: &[u8]) -> Vec<u8> {
    let simple = value.len() < 512 && value.iter().all(|b| (0x20..0x7f).contains(b));
    if simple {
        let mut out = vec![b'"'];
        for b in value {
            if *b == b'"' || *b == b'\\' {
                out.push(b'\\');
            }
            out.push(*b);
        }
        out.push(b'"');
        out
    } else {
        let mut out = format!("{{{}}}\r\n", value.len()).into_bytes();
        out.extend_from_slice(value);
        out
    }
}

/// Message content always goes as a literal, as clients expect.
fn literal(value: &[u8]) -> Vec<u8> {
    let mut out = format!("{{{}}}\r\n", value.len()).into_bytes();
    out.extend_from_slice(value);
    out
}

fn nstring(value: Option<&str>) -> Vec<u8> {
    value.map_or_else(|| b"NIL".to_vec(), |v| string(v.as_bytes()))
}

fn addresses(list: Option<&AddressList<'_>>) -> Vec<u8> {
    let Some(list) = list else {
        return b"NIL".to_vec();
    };
    let mut out = b"(".to_vec();
    for mailbox in list.mailboxes() {
        let address = mailbox.address().unwrap_or_default();
        let (local, host) = address.rsplit_once('@').unwrap_or((address, ""));
        out.push(b'(');
        out.extend(nstring(mailbox.name()));
        out.extend_from_slice(b" NIL ");
        out.extend(string(local.as_bytes()));
        out.push(b' ');
        out.extend(string(host.as_bytes()));
        out.push(b')');
    }
    if out.len() == 1 {
        return b"NIL".to_vec();
    }
    out.push(b')');
    out
}

/// ENVELOPE (RFC 3501, 7.4.2).
fn envelope(raw: &[u8]) -> Vec<u8> {
    let Some(message) = MessageParser::default().parse(raw) else {
        return b"(NIL NIL NIL NIL NIL NIL NIL NIL NIL NIL)".to_vec();
    };
    let header = |name: &str| -> Option<String> {
        let head_end = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or(raw.len());
        String::from_utf8_lossy(&raw[..head_end])
            .lines()
            .find_map(|l| {
                l.split_once(':')
                    .filter(|(n, _)| n.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v.trim().to_string())
            })
    };
    let from = message.from();
    let mut out = b"(".to_vec();
    out.extend(nstring(header("Date").as_deref()));
    out.push(b' ');
    out.extend(nstring(message.subject()));
    out.push(b' ');
    out.extend(addresses(from.as_ref()));
    out.push(b' ');
    out.extend(addresses(message.sender().or(from).as_ref()));
    out.push(b' ');
    out.extend(addresses(message.reply_to().or(from).as_ref()));
    for list in [message.to(), message.cc(), message.bcc()] {
        out.push(b' ');
        out.extend(addresses(list.as_ref()));
    }
    out.push(b' ');
    out.extend(nstring(header("In-Reply-To").as_deref()));
    out.push(b' ');
    out.extend(nstring(header("Message-ID").as_deref()));
    out.push(b')');
    out
}

/// BODYSTRUCTURE of one part (RFC 3501, 7.4.2), recursively.
fn structure(part: &mail_parser::MessagePart<'_>) -> Vec<u8> {
    let content_type = part.content_type();
    let (kind, sub) =
        content_type
            .as_ref()
            .map_or(("text".to_string(), "plain".to_string()), |c| {
                (
                    c.ctype().to_ascii_lowercase(),
                    c.subtype().unwrap_or("plain").to_ascii_lowercase(),
                )
            });
    if part.is_multipart() {
        let mut out = b"(".to_vec();
        for child in part.children() {
            out.extend(structure(&child));
        }
        out.push(b' ');
        out.extend(string(sub.to_uppercase().as_bytes()));
        out.push(b')');
        return out;
    }
    let mut params = Vec::new();
    if let Some(c) = &content_type {
        for (name, value) in c.attributes() {
            params.push((name.to_string(), value.to_string()));
        }
    }
    let params = if params.is_empty() {
        b"NIL".to_vec()
    } else {
        let mut p = b"(".to_vec();
        for (i, (name, value)) in params.iter().enumerate() {
            if i > 0 {
                p.push(b' ');
            }
            p.extend(string(name.to_uppercase().as_bytes()));
            p.push(b' ');
            p.extend(string(value.as_bytes()));
        }
        p.push(b')');
        p
    };
    let body = part.raw_body();
    let encoding = part
        .content_transfer_encoding()
        .unwrap_or("7BIT")
        .to_uppercase();
    let mut out = b"(".to_vec();
    out.extend(string(kind.to_uppercase().as_bytes()));
    out.push(b' ');
    out.extend(string(sub.to_uppercase().as_bytes()));
    out.push(b' ');
    out.extend(params);
    out.push(b' ');
    out.extend(nstring(part.content_id()));
    out.push(b' ');
    out.extend(nstring(part.content_description()));
    out.push(b' ');
    out.extend(string(encoding.as_bytes()));
    out.extend(format!(" {}", body.len()).into_bytes());
    if kind == "text" {
        out.extend(format!(" {}", body.iter().filter(|b| **b == b'\n').count()).into_bytes());
    }
    out.push(b')');
    out
}

/// A part by its IMAP section number (`1`, `2.1`).
fn section_part<'m>(
    message: &'m mail_parser::Message<'m>,
    path: &str,
) -> Option<mail_parser::MessagePart<'m>> {
    let mut part = message.root_part();
    for number in path.split('.') {
        let index: usize = number.parse().ok()?;
        if part.is_multipart() {
            part = part.children().nth(index.checked_sub(1)?)?;
        } else if index != 1 {
            return None;
        }
    }
    Some(part)
}

/// The bytes of `BODY[section]`.
fn section(raw: &[u8], spec: &str) -> Vec<u8> {
    let upper = spec.to_ascii_uppercase();
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(raw.len(), |p| p + 4);
    match upper.as_str() {
        "" => raw.to_vec(),
        "HEADER" => raw[..head_end].to_vec(),
        "TEXT" => raw[head_end.min(raw.len())..].to_vec(),
        _ if upper.starts_with("HEADER.FIELDS") => {
            let not = upper.starts_with("HEADER.FIELDS.NOT");
            let wanted: HashSet<String> = upper
                .split_once('(')
                .map(|(_, rest)| {
                    rest.trim_end_matches(')')
                        .split_whitespace()
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let headers = String::from_utf8_lossy(&raw[..head_end]).into_owned();
            let mut out = String::new();
            let mut keep = false;
            for line in headers.split("\r\n") {
                if line.is_empty() {
                    break;
                }
                if !line.starts_with([' ', '\t']) {
                    let name = line
                        .split(':')
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .to_ascii_uppercase();
                    keep = wanted.contains(&name) != not;
                }
                if keep {
                    out.push_str(line);
                    out.push_str("\r\n");
                }
            }
            out.push_str("\r\n");
            out.into_bytes()
        }
        _ => {
            let Some(message) = MessageParser::default().parse(raw) else {
                return Vec::new();
            };
            let (path, what) = upper
                .split_once(".M")
                .map_or((upper.as_str(), ""), |(p, _)| (p, "MIME"));
            let path = path.trim_end_matches(".HEADER").trim_end_matches(".TEXT");
            match section_part(&message, path) {
                Some(part) if what == "MIME" || upper.ends_with(".HEADER") => {
                    part.raw_headers().to_vec()
                }
                Some(part) => part.raw_body().to_vec(),
                None => Vec::new(),
            }
        }
    }
}

fn internal_date(ms: i64) -> String {
    let date = mail_parser::DateTime::from_timestamp(ms / 1000);
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{:02}-{}-{:04} {:02}:{:02}:{:02} +0000",
        date.day,
        MONTHS[usize::from(date.month.clamp(1, 12)) - 1],
        date.year,
        date.hour,
        date.minute,
        date.second
    )
}

/// An IMAP date (`20-Nov-2026`) as Unix ms at its start.
fn search_date(text: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let mut parts = text.trim_matches('"').split('-');
    let day: i64 = parts.next()?.parse().ok()?;
    let month_name = parts.next()?;
    let month = MONTHS
        .iter()
        .position(|m| m.eq_ignore_ascii_case(month_name))? as i64
        + 1;
    let year: i64 = parts.next()?.parse().ok()?;
    // Days from civil (Howard Hinnant).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400_000)
}

// --- The session ---

struct Selected {
    mailbox: Mailbox,
    read_only: bool,
    /// UIDs by sequence number (index + 1), as this client last saw them.
    uids: Vec<i64>,
    /// The mailbox's modification sequence when the client last heard.
    modseq: i64,
}

struct Session {
    instance: Arc<Instance>,
    peer: SocketAddr,
    tls: Option<Arc<tokio_rustls::rustls::ServerConfig>>,
    secure: bool,
    user: Option<String>,
    selected: Option<Selected>,
}

fn special_use(role: Option<&str>) -> &'static str {
    match role {
        Some("drafts") => " \\Drafts",
        Some("sent") => " \\Sent",
        Some("junk") => " \\Junk",
        Some("trash") => " \\Trash",
        Some("archive") => " \\Archive",
        _ => "",
    }
}

/// A list pattern (`*`, `%`, `INBOX`, `Archive/%`) against a name, in
/// time proportional to their lengths multiplied (no backtracking).
fn matches_pattern(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().take(512).collect();
    let n: Vec<char> = name.chars().collect();
    // can[j]: the pattern so far matches the first j characters of the name.
    let mut can = vec![false; n.len() + 1];
    can[0] = true;
    for c in &p {
        let mut next = vec![false; n.len() + 1];
        match c {
            '*' => {
                let mut any = false;
                for j in 0..=n.len() {
                    any |= can[j];
                    next[j] = any;
                }
            }
            '%' => {
                // Like * but not across the hierarchy delimiter.
                let mut any = false;
                for j in 0..=n.len() {
                    if j > 0 && n[j - 1] == '/' {
                        any = false;
                    }
                    any |= can[j];
                    next[j] = any;
                }
            }
            c => {
                for j in 1..=n.len() {
                    next[j] = can[j - 1] && n[j - 1].eq_ignore_ascii_case(c);
                }
            }
        }
        can = next;
    }
    can[n.len()]
}

impl Session {
    fn new(
        instance: Arc<Instance>,
        peer: SocketAddr,
        tls: Option<Arc<tokio_rustls::rustls::ServerConfig>>,
    ) -> Self {
        Self {
            instance,
            peer,
            tls,
            secure: false,
            user: None,
            selected: None,
        }
    }

    fn capabilities(&self) -> String {
        let mut caps = vec![
            "IMAP4rev1",
            "LITERAL+",
            "IDLE",
            "UIDPLUS",
            "MOVE",
            "UNSELECT",
            "SPECIAL-USE",
            "ENABLE",
            "CONDSTORE",
            "ID",
        ];
        if self.secure {
            caps.push("AUTH=PLAIN");
            caps.push("SASL-IR");
        } else {
            if self.tls.is_some() {
                caps.push("STARTTLS");
            }
            caps.push("LOGINDISABLED");
        }
        caps.join(" ")
    }

    async fn run(
        mut self,
        socket: tokio::net::TcpStream,
        implicit_tls: bool,
    ) -> Result<(), String> {
        let stream: Box<dyn Stream> = if implicit_tls {
            let config = self.tls.clone().ok_or("993 needs a TLS certificate")?;
            self.secure = true;
            Box::new(
                tokio::time::timeout(
                    HANDSHAKE,
                    tokio_rustls::TlsAcceptor::from(config).accept(socket),
                )
                .await
                .map_err(|_| "the client went away in the TLS handshake".to_string())?
                .map_err(|e| e.to_string())?,
            )
        } else {
            Box::new(socket)
        };
        let mut reader = BufReader::new(stream);
        let greeting = format!(
            "* OK [CAPABILITY {}] Ortus IMAP ready\r\n",
            self.capabilities()
        );
        send(&mut reader, greeting.as_bytes()).await?;
        loop {
            let command = match tokio::time::timeout(
                INACTIVITY,
                read_command(
                    &mut reader,
                    if self.user.is_some() {
                        MAX_LITERAL
                    } else {
                        MAX_BEFORE_SIGN_IN
                    },
                ),
            )
            .await
            {
                Ok(Ok(Some(command))) => command,
                Ok(Ok(None)) => return Err("the client went away".into()),
                Ok(Err(e)) => {
                    let _ = send(&mut reader, b"* BYE That was too long\r\n").await;
                    return Err(e);
                }
                Err(_) => {
                    let _ = send(&mut reader, b"* BYE Idle for too long\r\n").await;
                    return Ok(());
                }
            };
            let Ok(parts) = tokens(&command) else {
                send(&mut reader, b"* BAD That could not be read\r\n").await?;
                continue;
            };
            let mut parts = parts.into_iter();
            let Some(Token::Atom(tag)) = parts.next() else {
                send(&mut reader, b"* BAD No tag\r\n").await?;
                continue;
            };
            let Some(Token::Atom(verb)) = parts.next() else {
                send(&mut reader, format!("{tag} BAD No command\r\n").as_bytes()).await?;
                continue;
            };
            let args: Vec<Token> = parts.collect();
            let mut verb = verb.to_ascii_uppercase();
            let mut uid = false;
            let mut args = args;
            if verb == "UID" && !args.is_empty() {
                uid = true;
                verb = args.remove(0).text().to_ascii_uppercase();
            }
            match verb.as_str() {
                "STARTTLS" if !self.secure && self.tls.is_some() => {
                    send(
                        &mut reader,
                        format!("{tag} OK Begin TLS now\r\n").as_bytes(),
                    )
                    .await?;
                    // Anything sent before the handshake is dropped.
                    let plain = reader.into_inner();
                    let config = self.tls.clone().ok_or("no TLS")?;
                    let accepted = tokio::time::timeout(
                        HANDSHAKE,
                        tokio_rustls::TlsAcceptor::from(config).accept(plain),
                    )
                    .await
                    .map_err(|_| "the client went away in the TLS handshake".to_string())?
                    .map_err(|e| e.to_string())?;
                    reader = BufReader::new(Box::new(accepted));
                    self.secure = true;
                }
                "LOGOUT" => {
                    send(
                        &mut reader,
                        format!("* BYE See you\r\n{tag} OK Logged out\r\n").as_bytes(),
                    )
                    .await?;
                    return Ok(());
                }
                "IDLE" if self.user.is_some() => self.idle(&tag, &mut reader).await?,
                "AUTHENTICATE" if args.len() == 1 && self.secure && self.user.is_none() => {
                    // No initial response: ask for it.
                    send(&mut reader, b"+ \r\n").await?;
                    let mut answer = String::new();
                    tokio::time::timeout(
                        HANDSHAKE,
                        (&mut reader).take(MAX_LINE as u64).read_line(&mut answer),
                    )
                    .await
                    .map_err(|_| "the client went away".to_string())?
                    .map_err(|e| e.to_string())?;
                    let answer = answer.trim();
                    let reply = if answer == "*" {
                        format!("{tag} BAD Cancelled\r\n").into_bytes()
                    } else {
                        let mut args = args.clone();
                        args.push(Token::Atom(answer.to_string()));
                        self.command(&tag, &verb, &args, uid).await
                    };
                    send(&mut reader, &reply).await?;
                }
                _ => {
                    let reply = self.command(&tag, &verb, &args, uid).await;
                    send(&mut reader, &reply).await?;
                }
            }
        }
    }

    /// Runs one command; answers everything to send, ending with the tagged line.
    async fn command(&mut self, tag: &str, verb: &str, args: &[Token], uid: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let done = |out: &mut Vec<u8>, status: &str, text: &str| {
            out.extend(format!("{tag} {status} {text}\r\n").into_bytes())
        };
        match verb {
            "CAPABILITY" => {
                out.extend(format!("* CAPABILITY {}\r\n", self.capabilities()).into_bytes());
                done(&mut out, "OK", "CAPABILITY done");
            }
            "NOOP" | "CHECK" => {
                out.extend(self.changes().await);
                done(&mut out, "OK", "NOOP done");
            }
            "ID" => {
                out.extend_from_slice(b"* ID (\"name\" \"Ortus\")\r\n");
                done(&mut out, "OK", "ID done");
            }
            "ENABLE" => {
                done(&mut out, "OK", "Nothing more to enable");
            }
            "LOGIN" | "AUTHENTICATE" if self.user.is_some() => {
                done(&mut out, "BAD", "Already signed in")
            }
            "LOGIN" | "AUTHENTICATE" if !self.secure => {
                done(&mut out, "NO", "[PRIVACYREQUIRED] STARTTLS first")
            }
            "LOGIN" => {
                let (Some(user), Some(password)) = (args.first(), args.get(1)) else {
                    done(&mut out, "BAD", "LOGIN user password");
                    return out;
                };
                self.sign_in(&mut out, tag, &user.text(), &password.text())
                    .await;
            }
            "AUTHENTICATE" => {
                let mechanism = args.first().map(Token::text).unwrap_or_default();
                let Some(initial) = args.get(1) else {
                    done(&mut out, "BAD", "AUTHENTICATE PLAIN needs a response");
                    return out;
                };
                if !mechanism.eq_ignore_ascii_case("PLAIN") {
                    done(&mut out, "NO", "PLAIN only");
                    return out;
                }
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(initial.text().trim())
                    .unwrap_or_default();
                let mut parts = decoded.split(|b| *b == 0);
                let (_, user, password) = (
                    parts.next(),
                    parts.next().unwrap_or_default(),
                    parts.next().unwrap_or_default(),
                );
                self.sign_in(
                    &mut out,
                    tag,
                    &String::from_utf8_lossy(user),
                    &String::from_utf8_lossy(password),
                )
                .await;
            }
            _ if self.user.is_none() => done(&mut out, "NO", "Sign in first"),
            "LIST" | "LSUB" => {
                let pattern = args.get(1).map(Token::text).unwrap_or_else(|| "*".into());
                let reference = args.first().map(Token::text).unwrap_or_default();
                let wanted = format!("{reference}{pattern}");
                if pattern.is_empty() {
                    out.extend_from_slice(b"* LIST (\\Noselect) \"/\" \"\"\r\n");
                } else {
                    for mailbox in self.mailboxes().await {
                        if matches_pattern(&wanted, &mailbox.name) {
                            out.extend(
                                format!(
                                    "* {verb} (\\HasNoChildren{}) \"/\" ",
                                    special_use(mailbox.role.as_deref())
                                )
                                .into_bytes(),
                            );
                            out.extend(string(mailbox.name.as_bytes()));
                            out.extend_from_slice(b"\r\n");
                        }
                    }
                }
                done(&mut out, "OK", &format!("{verb} done"));
            }
            "STATUS" => {
                let name = args.first().map(Token::text).unwrap_or_default();
                let Some(mailbox) = self.find(&name).await else {
                    done(&mut out, "NO", "[NONEXISTENT] No such mailbox");
                    return out;
                };
                let store = self.instance.store.as_ref();
                let entries = store::entries(store, &mailbox.id, 0, i64::MAX)
                    .await
                    .unwrap_or_default();
                let unseen = entries
                    .iter()
                    .filter(|e| !e.flags.iter().any(|f| f == "\\Seen"))
                    .count();
                let mut items = Vec::new();
                if let Some(Token::List(wanted)) = args.get(1) {
                    for item in wanted {
                        match item.text().to_ascii_uppercase().as_str() {
                            "MESSAGES" => items.push(format!("MESSAGES {}", entries.len())),
                            "RECENT" => items.push("RECENT 0".into()),
                            "UIDNEXT" => items.push(format!("UIDNEXT {}", mailbox.uid_next)),
                            "UIDVALIDITY" => {
                                items.push(format!("UIDVALIDITY {}", mailbox.uid_validity))
                            }
                            "UNSEEN" => items.push(format!("UNSEEN {unseen}")),
                            "HIGHESTMODSEQ" => {
                                items.push(format!("HIGHESTMODSEQ {}", mailbox.modseq))
                            }
                            _ => {}
                        }
                    }
                }
                out.extend_from_slice(b"* STATUS ");
                out.extend(string(mailbox.name.as_bytes()));
                out.extend(format!(" ({})\r\n", items.join(" ")).into_bytes());
                done(&mut out, "OK", "STATUS done");
            }
            "SELECT" | "EXAMINE" => {
                let name = args.first().map(Token::text).unwrap_or_default();
                let Some(mailbox) = self.find(&name).await else {
                    self.selected = None;
                    done(&mut out, "NO", "[NONEXISTENT] No such mailbox");
                    return out;
                };
                let store = self.instance.store.as_ref();
                let entries = store::entries(store, &mailbox.id, 0, i64::MAX)
                    .await
                    .unwrap_or_default();
                let unseen = entries
                    .iter()
                    .position(|e| !e.flags.iter().any(|f| f == "\\Seen"));
                out.extend_from_slice(
                    b"* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft $Junk $NotJunk)\r\n",
                );
                out.extend(format!("* {} EXISTS\r\n* 0 RECENT\r\n", entries.len()).into_bytes());
                if let Some(first) = unseen {
                    out.extend(
                        format!("* OK [UNSEEN {}] First unseen\r\n", first + 1).into_bytes(),
                    );
                }
                out.extend(format!("* OK [UIDVALIDITY {}] UIDs valid\r\n* OK [UIDNEXT {}] Next UID\r\n* OK [HIGHESTMODSEQ {}] Changes\r\n", mailbox.uid_validity, mailbox.uid_next, mailbox.modseq).into_bytes());
                out.extend_from_slice(b"* OK [PERMANENTFLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft $Junk $NotJunk \\*)] Flags kept\r\n");
                let read_only = verb == "EXAMINE";
                self.selected = Some(Selected {
                    modseq: mailbox.modseq,
                    uids: entries.iter().map(|e| e.uid).collect(),
                    mailbox,
                    read_only,
                });
                done(
                    &mut out,
                    "OK",
                    if read_only {
                        "[READ-ONLY] EXAMINE done"
                    } else {
                        "[READ-WRITE] SELECT done"
                    },
                );
            }
            "CREATE" => {
                let name = args.first().map(Token::text).unwrap_or_default();
                let made = store::create_mailbox(
                    self.instance.store.as_ref(),
                    self.user.as_deref().unwrap_or_default(),
                    &name,
                )
                .await
                .ok()
                .flatten();
                if made.is_some() {
                    done(&mut out, "OK", "CREATE done")
                } else {
                    done(&mut out, "NO", "[ALREADYEXISTS] Could not create that")
                }
            }
            "DELETE" => {
                let name = args.first().map(Token::text).unwrap_or_default();
                let gone = store::delete_mailbox(
                    self.instance.store.as_ref(),
                    self.user.as_deref().unwrap_or_default(),
                    &name,
                )
                .await
                .unwrap_or(false);
                if gone {
                    done(&mut out, "OK", "DELETE done")
                } else {
                    done(&mut out, "NO", "Only folders of your own can go")
                }
            }
            "RENAME" => {
                let (from, to) = (
                    args.first().map(Token::text).unwrap_or_default(),
                    args.get(1).map(Token::text).unwrap_or_default(),
                );
                let renamed = store::rename_mailbox(
                    self.instance.store.as_ref(),
                    self.user.as_deref().unwrap_or_default(),
                    &from,
                    &to,
                )
                .await
                .unwrap_or(false);
                if renamed {
                    done(&mut out, "OK", "RENAME done")
                } else {
                    done(&mut out, "NO", "Could not rename that")
                }
            }
            "SUBSCRIBE" | "UNSUBSCRIBE" => done(&mut out, "OK", &format!("{verb} done")),
            "APPEND" => {
                let name = args.first().map(Token::text).unwrap_or_default();
                let Some(mailbox) = self.find(&name).await else {
                    done(&mut out, "NO", "[TRYCREATE] No such mailbox");
                    return out;
                };
                let flags: Vec<String> = args
                    .iter()
                    .find_map(|a| {
                        if let Token::List(f) = a {
                            Some(f.iter().map(Token::text).collect())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                let Some(message) = args.last().map(Token::bytes).filter(|m| !m.is_empty()) else {
                    done(&mut out, "BAD", "APPEND needs the message");
                    return out;
                };
                let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
                if message.len() > crate::smtp::max_size() {
                    done(&mut out, "NO", "[TOOBIG] That message is too big");
                    return out;
                }
                if !store::fits(
                    self.instance.store.as_ref(),
                    self.user.as_deref().unwrap_or_default(),
                    message.len(),
                )
                .await
                .unwrap_or(true)
                {
                    done(&mut out, "NO", "[OVERQUOTA] Your mailbox is full");
                    return out;
                }
                match store::deliver(
                    &self.instance,
                    self.user.as_deref().unwrap_or_default(),
                    &mailbox.id,
                    &message,
                    &flags,
                    None,
                    0,
                )
                .await
                {
                    Ok(delivered) => done(
                        &mut out,
                        "OK",
                        &format!(
                            "[APPENDUID {} {}] APPEND done",
                            mailbox.uid_validity, delivered.uid
                        ),
                    ),
                    Err(_) => done(&mut out, "NO", "Could not store it"),
                }
            }
            _ if self.selected.is_none() => done(&mut out, "BAD", "Select a mailbox first"),
            "CLOSE" | "UNSELECT" => {
                if verb == "CLOSE" {
                    let _ = self.expunge_deleted(None).await;
                }
                self.selected = None;
                done(&mut out, "OK", &format!("{verb} done"));
            }
            "EXPUNGE" => {
                let only = if uid {
                    args.first().and_then(|a| sequence_set(&a.text(), i64::MAX))
                } else {
                    None
                };
                out.extend(self.expunge_deleted(only).await);
                done(&mut out, "OK", "EXPUNGE done");
            }
            "FETCH" => {
                let (Some(set), Some(items)) = (args.first(), args.get(1)) else {
                    done(&mut out, "BAD", "FETCH set items");
                    return out;
                };
                match self.fetch(&set.text(), items, uid).await {
                    Ok(data) => {
                        out.extend(data);
                        done(&mut out, "OK", "FETCH done");
                    }
                    Err(e) => done(&mut out, "BAD", &e),
                }
            }
            "STORE" => {
                // CONDSTORE: STORE set (UNCHANGEDSINCE n) how flags.
                let mut args = args.to_vec();
                let mut unchanged_since: Option<i64> = None;
                if let Some(Token::List(modifiers)) = args.get(1).cloned() {
                    args.remove(1);
                    let words: Vec<String> = modifiers.iter().map(Token::text).collect();
                    if let Some(i) = words
                        .iter()
                        .position(|w| w.eq_ignore_ascii_case("UNCHANGEDSINCE"))
                    {
                        unchanged_since = words.get(i + 1).and_then(|n| n.parse().ok());
                        if unchanged_since.is_none() {
                            done(&mut out, "BAD", "UNCHANGEDSINCE needs a number");
                            return out;
                        }
                    }
                }
                let (Some(set), Some(how), Some(flags)) = (args.first(), args.get(1), args.get(2))
                else {
                    done(&mut out, "BAD", "STORE set how flags");
                    return out;
                };
                if self.selected.as_ref().is_some_and(|s| s.read_only) {
                    done(&mut out, "NO", "[READ-ONLY] EXAMINE is read-only");
                    return out;
                }
                let flags: Vec<String> = match flags {
                    Token::List(list) => list.iter().map(Token::text).collect(),
                    other => vec![other.text()],
                };
                out.extend(
                    self.store_flags(&set.text(), &how.text(), &flags, uid, unchanged_since)
                        .await,
                );
                done(&mut out, "OK", "STORE done");
            }
            "COPY" | "MOVE" => {
                let (Some(set), Some(target)) = (args.first(), args.get(1)) else {
                    done(&mut out, "BAD", &format!("{verb} set mailbox"));
                    return out;
                };
                let Some(target) = self.find(&target.text()).await else {
                    done(&mut out, "NO", "[TRYCREATE] No such mailbox");
                    return out;
                };
                if verb == "MOVE" && self.selected.as_ref().is_some_and(|s| s.read_only) {
                    done(&mut out, "NO", "[READ-ONLY] EXAMINE is read-only");
                    return out;
                }
                let (data, code) = self.copy(&set.text(), &target, uid, verb == "MOVE").await;
                out.extend(data);
                done(&mut out, "OK", &format!("{code}{verb} done"));
            }
            "SEARCH" => {
                let found = self.search(args, uid).await;
                out.extend(
                    format!(
                        "* SEARCH{}\r\n",
                        found.iter().map(|n| format!(" {n}")).collect::<String>()
                    )
                    .into_bytes(),
                );
                done(&mut out, "OK", "SEARCH done");
            }
            _ => done(&mut out, "BAD", "Not here"),
        }
        out
    }

    async fn sign_in(&mut self, out: &mut Vec<u8>, tag: &str, user: &str, password: &str) {
        match ortus_core::auth::app_passwords::authenticate(&self.instance, user, password, &self.peer.ip().to_string()).await {
            Ok(account) => {
                self.user = Some(account.id);
                out.extend(format!("{tag} OK [CAPABILITY {}] Signed in\r\n", self.capabilities()).into_bytes());
            }
            Err(ortus_core::auth::app_passwords::Refused::Blocked(_)) => out.extend(format!("{tag} NO [UNAVAILABLE] Too many tries; wait a while\r\n").into_bytes()),
            Err(ortus_core::auth::app_passwords::Refused::Wrong) => out.extend(format!("{tag} NO [AUTHENTICATIONFAILED] That address and app password do not match\r\n").into_bytes()),
        }
    }

    async fn mailboxes(&self) -> Vec<Mailbox> {
        store::mailboxes(
            self.instance.store.as_ref(),
            self.user.as_deref().unwrap_or_default(),
        )
        .await
        .unwrap_or_default()
    }

    async fn find(&self, name: &str) -> Option<Mailbox> {
        self.mailboxes().await.into_iter().find(|m| {
            m.name == name
                || (m.role.as_deref() == Some("inbox") && name.eq_ignore_ascii_case("INBOX"))
        })
    }

    /// (sequence number, UID) of the selected mailbox's messages in `set`.
    fn resolve(&self, set: &str, uid: bool) -> Vec<(usize, i64)> {
        let Some(selected) = &self.selected else {
            return Vec::new();
        };
        let max = if uid {
            selected.uids.last().copied().unwrap_or(0)
        } else {
            selected.uids.len() as i64
        };
        let Some(ranges) = sequence_set(set, max.max(1)) else {
            return Vec::new();
        };
        selected
            .uids
            .iter()
            .enumerate()
            .filter(|(i, u)| contains(&ranges, if uid { **u } else { *i as i64 + 1 }))
            .map(|(i, u)| (i + 1, *u))
            .collect()
    }

    async fn fetch(&mut self, set: &str, items: &Token, uid: bool) -> Result<Vec<u8>, String> {
        let mut wanted: Vec<String> = match items {
            Token::List(list) => list.iter().map(Token::text).collect(),
            other => match other.text().to_ascii_uppercase().as_str() {
                "ALL" => vec![
                    "FLAGS".into(),
                    "INTERNALDATE".into(),
                    "RFC822.SIZE".into(),
                    "ENVELOPE".into(),
                ],
                "FAST" => vec!["FLAGS".into(), "INTERNALDATE".into(), "RFC822.SIZE".into()],
                "FULL" => vec![
                    "FLAGS".into(),
                    "INTERNALDATE".into(),
                    "RFC822.SIZE".into(),
                    "ENVELOPE".into(),
                    "BODY".into(),
                ],
                _ => vec![other.text()],
            },
        };
        if uid && !wanted.iter().any(|w| w.eq_ignore_ascii_case("UID")) {
            wanted.insert(0, "UID".into());
        }
        let selected = self.selected.as_ref().ok_or("no mailbox")?;
        let (mailbox, read_only) = (selected.mailbox.id.clone(), selected.read_only);
        let user = self.user.clone().unwrap_or_default();
        let store = self.instance.store.as_ref();
        let mut out = Vec::new();
        for (seq, message_uid) in self.resolve(set, uid) {
            let Some(entry) = store::entry_by_uid(store, &mailbox, message_uid)
                .await
                .map_err(|e| e.to_string())?
            else {
                continue;
            };
            let needs_raw = wanted.iter().any(|w| {
                let w = w.to_ascii_uppercase();
                w.starts_with("BODY")
                    || w.starts_with("RFC822") && w != "RFC822.SIZE"
                    || w == "ENVELOPE"
            });
            let raw = if needs_raw {
                store::raw(&self.instance, &user, &entry.message)
                    .await
                    .map_err(|e| e.to_string())?
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let mut parts: Vec<Vec<u8>> = Vec::new();
            let mut seen_now = false;
            for item in &wanted {
                let upper = item.to_ascii_uppercase();
                match upper.as_str() {
                    "UID" => parts.push(format!("UID {}", entry.uid).into_bytes()),
                    "FLAGS" => {
                        parts.push(format!("FLAGS ({})", entry.flags.join(" ")).into_bytes())
                    }
                    "INTERNALDATE" => parts.push(
                        format!("INTERNALDATE \"{}\"", internal_date(entry.received_at))
                            .into_bytes(),
                    ),
                    "RFC822.SIZE" => parts.push(format!("RFC822.SIZE {}", entry.size).into_bytes()),
                    "MODSEQ" => parts.push(format!("MODSEQ ({})", entry.modseq).into_bytes()),
                    "ENVELOPE" => {
                        let mut p = b"ENVELOPE ".to_vec();
                        p.extend(envelope(&raw));
                        parts.push(p);
                    }
                    "BODY" | "BODYSTRUCTURE" => {
                        let mut p = format!("{upper} ").into_bytes();
                        match MessageParser::default().parse(&raw) {
                            Some(message) => p.extend(structure(&message.root_part())),
                            None => p.extend_from_slice(
                                b"(\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 0 0)",
                            ),
                        }
                        parts.push(p);
                    }
                    "RFC822" | "RFC822.HEADER" | "RFC822.TEXT" => {
                        let spec = match upper.as_str() {
                            "RFC822" => "",
                            "RFC822.HEADER" => "HEADER",
                            _ => "TEXT",
                        };
                        let mut p = format!("{upper} ").into_bytes();
                        p.extend(literal(&section(&raw, spec)));
                        parts.push(p);
                        seen_now |= upper != "RFC822.HEADER";
                    }
                    _ if upper.starts_with("BODY[") || upper.starts_with("BODY.PEEK[") => {
                        let peek = upper.starts_with("BODY.PEEK[");
                        let (Some(open), Some(close)) = (item.find('['), item.rfind(']')) else {
                            return Err(format!("{item} is not something to fetch"));
                        };
                        if close < open {
                            return Err(format!("{item} is not something to fetch"));
                        }
                        let spec = &item[open + 1..close];
                        let mut bytes = section(&raw, spec);
                        // A partial fetch: <start.length>.
                        let mut name = format!("BODY[{spec}]");
                        if let Some(partial) = item[close + 1..]
                            .strip_prefix('<')
                            .and_then(|p| p.strip_suffix('>'))
                        {
                            let (start, length) = partial
                                .split_once('.')
                                .map_or((partial, None), |(s, l)| (s, Some(l)));
                            let start: usize = start.parse().unwrap_or(0).min(bytes.len());
                            let end = length
                                .and_then(|l| l.parse::<usize>().ok())
                                .map_or(bytes.len(), |l| start.saturating_add(l).min(bytes.len()));
                            bytes = bytes[start..end].to_vec();
                            name = format!("BODY[{spec}]<{start}>");
                        }
                        let mut p = format!("{name} ").into_bytes();
                        p.extend(literal(&bytes));
                        parts.push(p);
                        seen_now |= !peek;
                    }
                    _ => return Err(format!("{item} is not something to fetch")),
                }
            }
            if seen_now && !read_only && !entry.flags.iter().any(|f| f == "\\Seen") {
                let mut flags: Vec<&str> = entry.flags.iter().map(String::as_str).collect();
                flags.push("\\Seen");
                let _ = store::set_flags(store, &mailbox, entry.uid, &flags).await;
                parts.push(format!("FLAGS ({})", flags.join(" ")).into_bytes());
            }
            out.extend(format!("* {seq} FETCH (").into_bytes());
            for (i, part) in parts.iter().enumerate() {
                if i > 0 {
                    out.push(b' ');
                }
                out.extend_from_slice(part);
            }
            out.extend_from_slice(b")\r\n");
        }
        Ok(out)
    }

    async fn store_flags(
        &mut self,
        set: &str,
        how: &str,
        flags: &[String],
        uid: bool,
        unchanged_since: Option<i64>,
    ) -> Vec<u8> {
        let silent = how.to_ascii_uppercase().ends_with(".SILENT");
        let how = how.to_ascii_uppercase();
        let mailbox = self
            .selected
            .as_ref()
            .map(|s| s.mailbox.id.clone())
            .unwrap_or_default();
        let store = self.instance.store.as_ref();
        let mut out = Vec::new();
        for (seq, message_uid) in self.resolve(set, uid) {
            let Ok(Some(entry)) = store::entry_by_uid(store, &mailbox, message_uid).await else {
                continue;
            };
            // Changed since the client last looked: left alone (RFC 7162).
            if unchanged_since.is_some_and(|since| entry.modseq > since) {
                continue;
            }
            let mut now: Vec<String> = entry.flags.clone();
            if how.starts_with('+') {
                for f in flags {
                    if !now.iter().any(|n| n.eq_ignore_ascii_case(f)) {
                        now.push(f.clone());
                    }
                }
            } else if how.starts_with('-') {
                now.retain(|n| !flags.iter().any(|f| f.eq_ignore_ascii_case(n)));
            } else {
                now = flags.to_vec();
            }
            let refs: Vec<&str> = now.iter().map(String::as_str).collect();
            let _ = store::set_flags(store, &mailbox, entry.uid, &refs).await;
            if !silent {
                let uid_part = if uid {
                    format!("UID {} ", entry.uid)
                } else {
                    String::new()
                };
                out.extend(
                    format!(
                        "* {seq} FETCH ({uid_part}FLAGS ({}))\r\n",
                        store::normal_flags(&refs)
                    )
                    .into_bytes(),
                );
            }
        }
        if let Some(selected) = &mut self.selected {
            if let Ok(fresh) =
                store::mailboxes(store, self.user.as_deref().unwrap_or_default()).await
            {
                if let Some(m) = fresh.into_iter().find(|m| m.id == selected.mailbox.id) {
                    selected.modseq = m.modseq;
                }
            }
        }
        out
    }

    async fn copy(
        &mut self,
        set: &str,
        target: &Mailbox,
        uid: bool,
        moving: bool,
    ) -> (Vec<u8>, String) {
        let mailbox = self
            .selected
            .as_ref()
            .map(|s| s.mailbox.id.clone())
            .unwrap_or_default();
        let store = self.instance.store.as_ref();
        let (mut from, mut to) = (Vec::new(), Vec::new());
        let picked = self.resolve(set, uid);
        for (_, message_uid) in &picked {
            if let Ok(Some(new_uid)) = store::copy(store, &mailbox, *message_uid, &target.id).await
            {
                from.push(message_uid.to_string());
                to.push(new_uid.to_string());
            }
        }
        let code = if from.is_empty() {
            String::new()
        } else {
            format!(
                "[COPYUID {} {} {}] ",
                target.uid_validity,
                from.join(","),
                to.join(",")
            )
        };
        let mut out = Vec::new();
        if moving {
            // Only what was copied leaves.
            let only: Vec<(i64, i64)> = from
                .iter()
                .filter_map(|u| u.parse::<i64>().ok())
                .map(|u| (u, u))
                .collect();
            if !only.is_empty() {
                out.extend(self.remove_uids(&only).await);
            }
            if !code.is_empty() {
                out = [format!("* OK {code}Moved\r\n").into_bytes(), out].concat();
            }
            return (out, String::new());
        }
        (out, code)
    }

    /// Removes the messages with these UIDs from the selected mailbox,
    /// answering EXPUNGE lines (highest sequence first, so numbers hold).
    async fn remove_uids(&mut self, ranges: &[(i64, i64)]) -> Vec<u8> {
        let Some(selected) = &mut self.selected else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut gone = Vec::new();
        for (i, u) in selected.uids.iter().enumerate().rev() {
            if contains(ranges, *u)
                && store::expunge(&self.instance, &selected.mailbox.id, *u)
                    .await
                    .unwrap_or(false)
            {
                out.extend(format!("* {} EXPUNGE\r\n", i + 1).into_bytes());
                gone.push(*u);
            }
        }
        selected.uids.retain(|u| !gone.contains(u));
        out
    }

    async fn expunge_deleted(&mut self, only: Option<Vec<(i64, i64)>>) -> Vec<u8> {
        let Some(selected) = &self.selected else {
            return Vec::new();
        };
        if selected.read_only {
            return Vec::new();
        }
        let entries = store::entries(
            self.instance.store.as_ref(),
            &selected.mailbox.id,
            0,
            i64::MAX,
        )
        .await
        .unwrap_or_default();
        let deleted: Vec<(i64, i64)> = entries
            .iter()
            .filter(|e| e.flags.iter().any(|f| f.eq_ignore_ascii_case("\\Deleted")))
            .filter(|e| only.as_ref().is_none_or(|r| contains(r, e.uid)))
            .map(|e| (e.uid, e.uid))
            .collect();
        if deleted.is_empty() {
            return Vec::new();
        }
        self.remove_uids(&deleted).await
    }

    /// What changed in the selected mailbox since the client last heard:
    /// expunged messages, new ones, changed flags.
    async fn changes(&mut self) -> Vec<u8> {
        let Some(selected) = &mut self.selected else {
            return Vec::new();
        };
        let store = self.instance.store.as_ref();
        let now = store::uids(store, &selected.mailbox.id)
            .await
            .unwrap_or_default();
        let mut out = Vec::new();
        for (i, u) in selected.uids.clone().iter().enumerate().rev() {
            if !now.contains(u) {
                out.extend(format!("* {} EXPUNGE\r\n", i + 1).into_bytes());
                selected.uids.remove(i);
            }
        }
        let known: HashSet<i64> = selected.uids.iter().copied().collect();
        let added: Vec<i64> = now.iter().filter(|u| !known.contains(u)).copied().collect();
        if !added.is_empty() {
            selected.uids.extend(added.iter().copied());
            out.extend(format!("* {} EXISTS\r\n", selected.uids.len()).into_bytes());
        }
        for entry in store::changed_since(store, &selected.mailbox.id, selected.modseq)
            .await
            .unwrap_or_default()
        {
            if added.contains(&entry.uid) {
                // New to this client: EXISTS told it, it fetches flags itself.
            } else if let Some(seq) = selected.uids.iter().position(|u| *u == entry.uid) {
                out.extend(
                    format!(
                        "* {} FETCH (UID {} FLAGS ({}))\r\n",
                        seq + 1,
                        entry.uid,
                        entry.flags.join(" ")
                    )
                    .into_bytes(),
                );
            }
            selected.modseq = selected.modseq.max(entry.modseq);
        }
        out
    }

    async fn idle(&mut self, tag: &str, reader: &mut Reader) -> Result<(), String> {
        send(reader, b"+ idling\r\n").await?;
        let until = tokio::time::Instant::now() + INACTIVITY;
        loop {
            if tokio::time::Instant::now() > until {
                send(reader, b"* BYE Idle for too long\r\n").await?;
                return Err("the client went away".into());
            }
            let mut line = String::new();
            match tokio::time::timeout(
                IDLE_CHECK,
                (&mut *reader).take(MAX_LINE as u64).read_line(&mut line),
            )
            .await
            {
                Ok(Ok(0)) => return Err("the client went away".into()),
                Ok(Ok(_)) => {
                    let done = line.trim().eq_ignore_ascii_case("DONE");
                    send(
                        reader,
                        format!(
                            "{tag} {}\r\n",
                            if done {
                                "OK IDLE done"
                            } else {
                                "BAD Expected DONE"
                            }
                        )
                        .as_bytes(),
                    )
                    .await?;
                    return Ok(());
                }
                Ok(Err(e)) => return Err(e.to_string()),
                Err(_) => {
                    let changes = self.changes().await;
                    if !changes.is_empty() {
                        send(reader, &changes).await?;
                    }
                }
            }
        }
    }

    /// SEARCH: the keys apps use. Answers sequence numbers as this session
    /// knows them, or UIDs.
    async fn search(&mut self, args: &[Token], uid: bool) -> Vec<i64> {
        let Some(selected) = &self.selected else {
            return Vec::new();
        };
        let store = self.instance.store.as_ref();
        let entries: std::collections::HashMap<i64, store::Entry> =
            store::entries(store, &selected.mailbox.id, 0, i64::MAX)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|e| (e.uid, e))
                .collect();
        let args: Vec<Token> = args
            .iter()
            .filter(|a| !matches!(a, Token::Atom(s) if s.eq_ignore_ascii_case("CHARSET") || s.eq_ignore_ascii_case("UTF-8") || s.eq_ignore_ascii_case("US-ASCII")))
            .take(200)
            .cloned()
            .collect();
        let keys = format!("{args:?}").to_ascii_uppercase();
        let needs_raw = ["BODY", "TEXT", "\"TO\"", "\"CC\"", "HEADER"]
            .iter()
            .any(|k| keys.contains(k));
        let user = self.user.clone().unwrap_or_default();
        let max_seq = selected.uids.len() as i64;
        let max_uid = selected.uids.last().copied().unwrap_or(0);
        let mut results = Vec::new();
        for (i, message_uid) in selected.uids.clone().iter().enumerate() {
            let Some(entry) = entries.get(message_uid) else {
                continue;
            };
            let seq = i as i64 + 1;
            let raw = if needs_raw {
                String::from_utf8_lossy(
                    &store::raw(&self.instance, &user, &entry.message)
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or_default(),
                )
                .to_ascii_lowercase()
            } else {
                String::new()
            };
            let look = Look {
                e: entry,
                seq,
                raw: &raw,
                max_seq,
                max_uid,
            };
            let mut at = 0;
            let mut all = true;
            while at < args.len() {
                match matches(&args, at, &look, 0) {
                    Some((ok, next)) => {
                        all &= ok;
                        at = next;
                    }
                    None => {
                        all = false;
                        break;
                    }
                }
            }
            if all {
                results.push(if uid { entry.uid } else { seq });
            }
        }
        results
    }
}

/// One message as SEARCH looks at it.
struct Look<'a> {
    e: &'a store::Entry,
    seq: i64,
    /// The raw message, lowercased (only when a key needs it).
    raw: &'a str,
    max_seq: i64,
    max_uid: i64,
}

/// Whether the search key at `at` matches; answers where the next one
/// starts. `None` for a key that is cut short or nested too deep.
fn matches(args: &[Token], at: usize, look: &Look<'_>, depth: usize) -> Option<(bool, usize)> {
    if depth > 32 {
        return None;
    }
    let key = args.get(at)?.text().to_ascii_uppercase();
    let arg = |n: usize| args.get(at + n).map(Token::text);
    let e = look.e;
    let has = |flag: &str| e.flags.iter().any(|f| f.eq_ignore_ascii_case(flag));
    let has_text = |hay: &str, needle: &str| hay.to_lowercase().contains(&needle.to_lowercase());
    let header = |name: &str| -> String {
        let head_end = look.raw.find("\r\n\r\n").unwrap_or(look.raw.len());
        look.raw[..head_end]
            .lines()
            .find(|l| l.starts_with(&format!("{name}:")))
            .unwrap_or_default()
            .to_string()
    };
    Some(match key.as_str() {
        "ALL" => (true, at + 1),
        "SEEN" => (has("\\Seen"), at + 1),
        "UNSEEN" | "NEW" => (!has("\\Seen"), at + 1),
        "FLAGGED" => (has("\\Flagged"), at + 1),
        "UNFLAGGED" => (!has("\\Flagged"), at + 1),
        "ANSWERED" => (has("\\Answered"), at + 1),
        "UNANSWERED" => (!has("\\Answered"), at + 1),
        "DELETED" => (has("\\Deleted"), at + 1),
        "UNDELETED" => (!has("\\Deleted"), at + 1),
        "DRAFT" => (has("\\Draft"), at + 1),
        "UNDRAFT" => (!has("\\Draft"), at + 1),
        "RECENT" | "OLD" => (key == "OLD", at + 1),
        "KEYWORD" => (has(&arg(1)?), at + 2),
        "UNKEYWORD" => (!has(&arg(1)?), at + 2),
        "FROM" => (
            has_text(
                &format!(
                    "{} {}",
                    e.from_name.clone().unwrap_or_default(),
                    e.from_addr.clone().unwrap_or_default()
                ),
                &arg(1)?,
            ),
            at + 2,
        ),
        "SUBJECT" => (has_text(&e.subject, &arg(1)?), at + 2),
        "TO" | "CC" => (
            has_text(&header(&key.to_ascii_lowercase()), &arg(1)?),
            at + 2,
        ),
        "HEADER" => {
            let field = header(&arg(1)?.to_ascii_lowercase());
            (!field.is_empty() && has_text(&field, &arg(2)?), at + 3)
        }
        "BODY" | "TEXT" => (has_text(look.raw, &arg(1)?), at + 2),
        "SINCE" => (
            search_date(&arg(1)?).is_some_and(|d| e.received_at >= d),
            at + 2,
        ),
        "BEFORE" => (
            search_date(&arg(1)?).is_some_and(|d| e.received_at < d),
            at + 2,
        ),
        "ON" => (
            search_date(&arg(1)?)
                .is_some_and(|d| e.received_at >= d && e.received_at < d + 86_400_000),
            at + 2,
        ),
        "LARGER" => (arg(1)?.parse::<i64>().is_ok_and(|n| e.size > n), at + 2),
        "SMALLER" => (arg(1)?.parse::<i64>().is_ok_and(|n| e.size < n), at + 2),
        "UID" => (
            sequence_set(&arg(1)?, look.max_uid.max(1)).is_some_and(|r| contains(&r, e.uid)),
            at + 2,
        ),
        "NOT" => {
            let (ok, next) = matches(args, at + 1, look, depth + 1)?;
            (!ok, next)
        }
        "OR" => {
            let (a, next) = matches(args, at + 1, look, depth + 1)?;
            let (b, after) = matches(args, next, look, depth + 1)?;
            (a || b, after)
        }
        _ => match &args[at] {
            Token::List(inner) => {
                let mut i = 0;
                let mut all = true;
                while i < inner.len() {
                    let (ok, next) = matches(inner, i, look, depth + 1)?;
                    all &= ok;
                    i = next;
                }
                (all, at + 1)
            }
            // A sequence set.
            _ => (
                sequence_set(&key, look.max_seq.max(1)).is_some_and(|r| contains(&r, look.seq)),
                at + 1,
            ),
        },
    })
}

async fn send(reader: &mut Reader, bytes: &[u8]) -> Result<(), String> {
    reader
        .get_mut()
        .write_all(bytes)
        .await
        .map_err(|e| e.to_string())?;
    reader.get_mut().flush().await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_tokenised() {
        let t =
            tokens(b"a1 FETCH 1:* (UID FLAGS BODY.PEEK[HEADER.FIELDS (DATE FROM)] BODY[]<0.100>)")
                .unwrap();
        assert_eq!(Token::Atom("a1".into()), t[0]);
        let Token::List(items) = &t[3] else {
            panic!("{t:?}")
        };
        assert_eq!(
            Token::Atom("BODY.PEEK[HEADER.FIELDS (DATE FROM)]".into()),
            items[2]
        );
        assert_eq!(Token::Atom("BODY[]<0.100>".into()), items[3]);
        let t = tokens(b"a2 LOGIN \"olena@ministry.example\" {5}\r\nab\"cd").unwrap();
        assert_eq!(Token::Text(b"ab\"cd".to_vec()), t[3]);
        assert!(tokens(b"a3 FETCH 1 (UID").is_err());
    }

    #[test]
    fn sets_and_patterns() {
        assert_eq!(
            Some(vec![(1, 5), (7, 7), (9, 12)]),
            sequence_set("1:5,7,9:*", 12)
        );
        assert_eq!(None, sequence_set("0", 3));
        assert!(matches_pattern("*", "Archive/2026"));
        assert!(!matches_pattern("%", "Archive/2026"));
        assert!(matches_pattern("Archive/%", "Archive/2026"));
        assert!(matches_pattern("inbox", "INBOX"));
    }

    #[test]
    fn strings_are_quoted_or_literal() {
        assert_eq!(b"\"say \\\"hi\\\"\"".to_vec(), string(b"say \"hi\""));
        assert_eq!(b"{3}\r\n\xc3\xa9!".to_vec(), string("é!".as_bytes()));
        assert_eq!(Some(1_795_132_800_000), search_date("20-Nov-2026"));
    }
}
