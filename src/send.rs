//! Sending mail: from a person's own address, past the label policy,
//! signed with the domain's DKIM key, a copy in Sent; recipients here get
//! it at once, everyone else through the outbound queue. Each remote
//! domain is a job that runs outside the database transaction (it waits on
//! other servers): MX, then the host's address, through our resolver;
//! opportunistic STARTTLS; temporary failures retried for days, permanent
//! ones (and the last try) bounced back to the sender.
//!
//! `ORTUS_SMTP_OUTBOUND_PORT` changes the remote port and
//! `ORTUS_SMTP_ALLOW_PRIVATE=1` lets mail go to private addresses (tests only).

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;

use hickory_resolver::proto::rr::RData;
use lettre::address::Envelope;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::transport::smtp::extension::ClientId;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use mail_auth::dkim::DkimSigner;
use mail_auth::headers::HeaderWriter;
use ortus_core::instance::Instance;
use ortus_core::jobs::Job;
use ortus_core::policy::{self, Action, Ask, Audience, Channel, Decision};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::store;

/// The job delivering one message to one remote domain.
pub const DELIVER_JOB: &str = "mail.deliver";

fn now() -> i64 {
    ortus_core::program::now_ms()
}

/// Why nothing was sent.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum Refused {
    /// The From address is not the sender's own.
    NotYourAddress,
    /// No recipient, or one that is not an address.
    BadRecipient { address: String },
    /// The label does not let it go to this recipient.
    Policy { address: String, decision: Decision },
    /// It may go once a reason is given.
    NeedsReason { address: String, label: String },
    /// Storing or queueing failed.
    Failed,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Sent {
    /// The copy in the sender's Sent mailbox.
    pub message: String,
    pub delivered_here: usize,
    pub queued: usize,
}

pub struct Outgoing<'a> {
    pub user: &'a str,
    pub from: &'a str,
    pub to: &'a [String],
    /// The whole message, headers and body, as RFC 5322.
    pub raw: &'a [u8],
    pub label: Option<&'a str>,
    pub reason: Option<&'a str>,
}

fn domain_of(address: &str) -> String {
    address
        .rsplit_once('@')
        .map(|(_, d)| d.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Signs `raw` with the DKIM key of `domain`, when the domain has one.
async fn sign(instance: &Instance, domain: &str, raw: &[u8]) -> Vec<u8> {
    let rows = instance
        .store
        .query(
            "SELECT dkim_key, dkim_public FROM mail_domains WHERE domain = $1",
            &[domain.into()],
        )
        .await
        .unwrap_or_default();
    let Some(row) = rows.first() else {
        return raw.to_vec();
    };
    let sealed: serde_json::Value = row
        .text(0)
        .and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or_default();
    let keys: Option<crate::domains::DkimKeys> =
        row.text(1).and_then(|t| serde_json::from_str(t).ok());
    let (Some(rsa), Some(keys)) = (
        sealed["rsa"]
            .as_str()
            .and_then(|s| instance.keys.open(s.as_bytes()).ok()),
        keys,
    ) else {
        return raw.to_vec();
    };
    #[allow(deprecated)]
    let Ok(key) = mail_auth::crypto::RsaKey::<mail_auth::crypto::Sha256>::from_pkcs8_der(&rsa) else {
        return raw.to_vec();
    };
    let signer = DkimSigner::from_key(key)
        .domain(domain)
        .selector(keys.rsa_selector)
        .headers([
            "From",
            "To",
            "Cc",
            "Subject",
            "Date",
            "Message-ID",
            "In-Reply-To",
            "References",
            "Content-Type",
            "MIME-Version",
        ]);
    match signer.sign(raw) {
        Ok(signature) => {
            let mut signed = signature.to_header().into_bytes();
            signed.extend_from_slice(raw);
            signed
        }
        Err(e) => {
            eprintln!("ortus: mail: DKIM signing for {domain}: {e}");
            raw.to_vec()
        }
    }
}

/// The message without its Bcc header: Bcc never leaves with it.
pub fn strip_bcc(raw: &[u8]) -> Vec<u8> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(raw.len(), |p| p + 2);
    let head = String::from_utf8_lossy(&raw[..end]);
    let mut out = String::new();
    let mut skipping = false;
    for line in head.split_inclusive("\r\n") {
        if !line.starts_with([' ', '\t']) {
            skipping = line.to_ascii_lowercase().starts_with("bcc:");
        }
        if !skipping {
            out.push_str(line);
        }
    }
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(&raw[end..]);
    bytes
}

/// Every label in a message: `X-Ortus-Label` headers anywhere in it,
/// folded or not, including forwarded messages inside it.
pub fn labels_in(raw: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(message) = mail_parser::MessageParser::default().parse(raw) {
        for part in message.parts() {
            for header in part.headers().all("X-Ortus-Label") {
                if let Some(text) = header.value().as_text() {
                    let label = text.trim().to_ascii_lowercase();
                    if !label.is_empty() && !out.contains(&label) {
                        out.push(label);
                    }
                }
            }
        }
    }
    out
}

/// The addresses in a message's From and Sender headers.
fn from_addresses(raw: &[u8]) -> Vec<String> {
    let Some(message) = mail_parser::MessageParser::default().parse(raw) else {
        return Vec::new();
    };
    [message.from(), message.sender()]
        .into_iter()
        .flatten()
        .flat_map(|l| {
            l.mailboxes()
                .filter_map(|m| m.address().map(str::to_ascii_lowercase))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Who gets a message: people here (their user ids) and addresses away,
/// by domain.
struct Routed {
    from: String,
    here: Vec<String>,
    away: BTreeMap<String, Vec<String>>,
}

/// Checks the sender and every recipient, and asks the label policy about
/// everyone outside: `record` writes the decisions to the audit log (when
/// sending), otherwise nothing is kept (while composing).
async fn route(instance: &Instance, out: &Outgoing<'_>, record: bool) -> Result<Routed, Refused> {
    let store = instance.store.as_ref();
    let from = out.from.trim().to_ascii_lowercase();
    if store::recipient(store, &from)
        .await
        .ok()
        .flatten()
        .as_deref()
        != Some(out.user)
    {
        return Err(Refused::NotYourAddress);
    }
    // The message says who it is from too: every such address must be the
    // sender's own, or a colleague could be impersonated, signed and all.
    for address in from_addresses(out.raw) {
        if store::recipient(store, &address)
            .await
            .ok()
            .flatten()
            .as_deref()
            != Some(out.user)
        {
            return Err(Refused::NotYourAddress);
        }
    }
    if out.to.is_empty() {
        return Err(Refused::BadRecipient {
            address: String::new(),
        });
    }
    let mut labels: Vec<String> = out
        .label
        .map(|l| l.to_ascii_lowercase())
        .into_iter()
        .collect();
    for label in labels_in(out.raw) {
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    let mut here = Vec::new();
    let mut away: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for address in out.to {
        let address = address.trim().to_ascii_lowercase();
        if !address.contains('@')
            || address.contains(char::is_whitespace)
            || domain_of(&address).is_empty()
        {
            return Err(Refused::BadRecipient { address });
        }
        if let Ok(Some(user)) = store::recipient(store, &address).await {
            here.push(user);
            continue;
        }
        // Only labelled mail is judged (a label chosen in compose, or an
        // X-Ortus-Label header from a mail app): unlabelled mail goes out
        // as mail always has.
        if labels.is_empty() {
            away.entry(domain_of(&address)).or_default().push(address);
            continue;
        }
        // Each label in it must let it go there.
        let audience = Audience::Outside(address.clone());
        for label in &labels {
            let ask = Ask {
                actor: out.user,
                action: Action::Share,
                label: Some(label),
                audience: &audience,
                channel: Channel::Mail,
                object: "mail:outgoing",
                reason: out.reason,
            };
            let decision = if record {
                policy::decide(instance, &ask).await
            } else {
                policy::judge(store, &ask)
                    .await
                    .map(|(decision, _, _)| decision)
            }
            .map_err(|_| Refused::Failed)?;
            match decision {
                Decision::Allow { .. } => {}
                Decision::NeedsReason { label } => {
                    return Err(Refused::NeedsReason { address, label })
                }
                decision @ Decision::Deny { .. } => {
                    return Err(Refused::Policy { address, decision })
                }
            }
        }
        away.entry(domain_of(&address)).or_default().push(address);
    }
    Ok(Routed { from, here, away })
}

/// Whether a message may go as it is, without sending or recording
/// anything: what compose shows before the person presses Send.
pub async fn check(instance: &Instance, out: &Outgoing<'_>) -> Result<(), Refused> {
    route(instance, out, false).await.map(|_| ())
}

/// Sends a message. Nothing goes anywhere unless every recipient may get it.
pub async fn send(instance: &Instance, out: &Outgoing<'_>) -> Result<Sent, Refused> {
    let store = instance.store.as_ref();
    let Routed { from, here, away } = route(instance, out, true).await?;
    let signed = sign(instance, &domain_of(&from), out.raw).await;
    let sent_box = store::by_role(store, out.user, "sent")
        .await
        .map_err(|_| Refused::Failed)?;
    let copy = store::deliver(
        instance,
        out.user,
        &sent_box.id,
        &signed,
        &["\\Seen"],
        None,
        0,
    )
    .await
    .map_err(|_| Refused::Failed)?;
    // All or nothing: if anything below fails, what was stored is taken
    // back and the sender may try again without anyone getting it twice.
    let mut stored: Vec<(String, String)> = vec![(out.user.to_string(), copy.message.clone())];
    let undo = |stored: Vec<(String, String)>| async move {
        for (user, message) in stored {
            let _ = store::remove_message(instance, &user, &message).await;
        }
        Refused::Failed
    };
    let mut delivered_here = 0;
    let mut full: Vec<String> = Vec::new();
    for user in &here {
        // A colleague whose mailbox is full does not get it; the sender hears.
        if !store::fits(store, user, signed.len()).await.unwrap_or(true) {
            full.push(user.clone());
            continue;
        }
        let inbox = match store::by_role(store, user, "inbox").await {
            Ok(inbox) => inbox,
            Err(_) => return Err(undo(stored).await),
        };
        match store::deliver(
            instance,
            user,
            &inbox.id,
            &signed,
            &[],
            Some(&json!({ "internal": true })),
            0,
        )
        .await
        {
            Ok(delivered) => {
                stored.push((user.clone(), delivered.message));
                delivered_here += 1;
                // An invitation from a colleague's mail app reaches their calendar.
                if crate::invites::calendar_part(&signed).is_some() {
                    crate::invites::process(instance, user, &from, &signed).await;
                }
            }
            Err(_) => return Err(undo(stored).await),
        }
    }
    // Every outside domain in one transaction.
    let queued: usize = away.values().map(Vec::len).sum();
    let queue = async {
        let mut tx = store.begin().await?;
        for (domain, recipients) in &away {
            let job = Delivery { user: out.user.to_string(), message: copy.message.clone(), from: from.clone(), domain: domain.clone(), to: recipients.clone() };
            for recipient in recipients {
                tx.execute(
                    "INSERT INTO mail_outbound (id, user_id, message, recipient, domain, state, attempts, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, 'queued', 0, $6, $6)",
                    &[format!("ob_{}", ortus_core::auth::random_hex(12)).into(), out.user.into(), copy.message.as_str().into(), recipient.as_str().into(), domain.as_str().into(), now().into()],
                )
                .await?;
            }
            let id = format!("deliver-{}-{domain}", copy.message);
            let payload = serde_json::to_string(&job).unwrap_or_default();
            ortus_core::jobs::enqueue_in(&mut *tx, &ortus_core::jobs::NewJob { id: &id, kind: DELIVER_JOB, payload: &payload, run_at: now(), max_attempts: 16 }, now()).await?;
        }
        tx.commit().await
    }
    .await;
    if queue.is_err() {
        return Err(undo(stored).await);
    }
    if !full.is_empty() {
        let mut names = Vec::new();
        for user in &full {
            if let Ok(Some(u)) = ortus_core::auth::accounts::by_id(store, user).await {
                names.push(u.email);
            }
        }
        let domain = domain_of(&from);
        let note = format!(
            "From: Mail Delivery <postmaster@{domain}>\r\nTo: <{from}>\r\nSubject: Not delivered: mailbox full\r\nMessage-ID: <full-{}@{domain}>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nYour message could not be delivered to {} because the mailbox is full.\r\n",
            ortus_core::auth::random_hex(8),
            names.join(", ")
        );
        if let Ok(inbox) = store::by_role(store, out.user, "inbox").await {
            let _ = store::deliver(
                instance,
                out.user,
                &inbox.id,
                note.as_bytes(),
                &[],
                Some(&json!({ "internal": true, "bounce": true })),
                0,
            )
            .await;
        }
    }
    Ok(Sent {
        message: copy.message,
        delivered_here,
        queued,
    })
}

#[derive(Serialize, Deserialize)]
struct Delivery {
    user: String,
    message: String,
    from: String,
    domain: String,
    to: Vec<String>,
}

/// The servers that take mail for `domain`, best first, as (name, address).
async fn exchangers(domain: &str) -> Result<Vec<(String, IpAddr)>, String> {
    let resolver = crate::domains::resolver()?;
    let mut hosts: Vec<(u16, String)> = match resolver.mx_lookup(format!("{domain}.")).await {
        Ok(found) => found
            .answers()
            .iter()
            .filter_map(|r| match &r.data {
                RData::MX(mx) => Some((
                    mx.preference,
                    mx.exchange.to_ascii().trim_end_matches('.').to_string(),
                )),
                _ => None,
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    if hosts.is_empty() {
        // No MX: the domain itself (RFC 5321, 5.1).
        hosts.push((0, domain.to_string()));
    }
    if hosts.iter().any(|(_, h)| h.is_empty() || h == ".") {
        return Err("the domain takes no mail (null MX)".into());
    }
    hosts.sort();
    let mut out = Vec::new();
    let inside_allowed = std::env::var("ORTUS_SMTP_ALLOW_PRIVATE").is_ok_and(|v| v == "1");
    let mut refused_inside = false;
    for (_, host) in hosts {
        if let Ok(ips) = resolver.lookup_ip(format!("{host}.")).await {
            // Never into our own network: a domain whose MX points at a
            // private address would make us talk to servers there.
            match ips.iter().find(|ip| inside_allowed || public(*ip)) {
                Some(ip) => out.push((host, ip)),
                None => refused_inside |= ips.iter().next().is_some(),
            }
        }
    }
    if out.is_empty() {
        return Err(if refused_inside {
            format!("the mail servers of {domain} are on a private network")
        } else {
            format!("no address for the mail servers of {domain}")
        });
    }
    Ok(out)
}

/// Whether an address is on the public internet (not loopback, private,
/// link-local, shared, documentation, multicast or unspecified).
pub fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || (a == 198 && (b == 18 || b == 19))
                || a >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || first == 0x2001 && v6.segments()[1] == 0x0db8)
        }
    }
}

/// Whether a delivery error is permanent (5xx).
enum Outcome {
    Delivered,
    Permanent(String),
    Temporary(String),
}

async fn try_deliver(
    host: &str,
    ip: IpAddr,
    hello: &str,
    envelope: &Envelope,
    raw: &[u8],
) -> Outcome {
    let port = std::env::var("ORTUS_SMTP_OUTBOUND_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(25);
    // Opportunistic TLS: encrypted whenever the server offers it; the
    // certificate is not required to verify (MTA-STS will require it).
    let tls = match TlsParameters::builder(host.to_string())
        .dangerous_accept_invalid_certs(true)
        .build_rustls()
    {
        Ok(parameters) => Tls::Opportunistic(parameters),
        Err(_) => Tls::None,
    };
    let transport = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(ip.to_string())
        .port(port)
        .tls(tls)
        .hello_name(ClientId::Domain(hello.to_string()))
        .timeout(Some(std::time::Duration::from_secs(60)))
        .build();
    match transport.send_raw(envelope, raw).await {
        Ok(_) => Outcome::Delivered,
        Err(e) if e.is_permanent() => Outcome::Permanent(e.to_string()),
        Err(e) => Outcome::Temporary(e.to_string()),
    }
}

/// Tells the sender a message could not be delivered (a bounce in their inbox).
async fn bounce(instance: &Instance, delivery: &Delivery, why: &str) {
    let host = crate::domains::Hosts::of(instance).mail;
    let subject = summary_subject(instance, &delivery.user, &delivery.message).await;
    let text = format!(
        "From: Mail Delivery System <mailer-daemon@{host}>\r\nTo: {}\r\nSubject: Undelivered: {subject}\r\nDate: {}\r\nAuto-Submitted: auto-replied\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nYour message could not be delivered to:\r\n\r\n  {}\r\n\r\nThe receiving server said:\r\n\r\n  {}\r\n",
        delivery.from,
        mail_parser::DateTime::from_timestamp(now() / 1000).to_rfc822(),
        delivery.to.join("\r\n  "),
        why.replace(['\r', '\n'], " ")
    );
    if let Ok(inbox) = store::by_role(instance.store.as_ref(), &delivery.user, "inbox").await {
        let _ = store::deliver(
            instance,
            &delivery.user,
            &inbox.id,
            text.as_bytes(),
            &[],
            Some(&json!({ "internal": true, "bounce": true })),
            0,
        )
        .await;
    }
}

async fn summary_subject(instance: &Instance, user: &str, message: &str) -> String {
    let rows = instance
        .store
        .query(
            "SELECT subject FROM mail_messages WHERE id = $1 AND user_id = $2",
            &[message.into(), user.into()],
        )
        .await
        .unwrap_or_default();
    rows.first()
        .and_then(|r| r.text(0))
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

async fn record(instance: &Instance, delivery: &Delivery, state: &str, error: Option<&str>) {
    record_at(instance, delivery, state, error, None).await
}

/// Records an attempt, with the receiving server when one answered.
async fn record_at(
    instance: &Instance,
    delivery: &Delivery,
    state: &str,
    error: Option<&str>,
    mx: Option<&str>,
) {
    for recipient in &delivery.to {
        let _ = instance
            .store
            .execute(
                "UPDATE mail_outbound SET state = $1, attempts = attempts + 1, last_error = $2, updated_at = $3, mx = COALESCE($6, mx) WHERE message = $4 AND recipient = $5",
                &[state.into(), error.map(str::to_string).into(), now().into(), delivery.message.as_str().into(), recipient.as_str().into(), mx.map(str::to_string).into()],
            )
            .await;
    }
}

/// The job: delivers one message to one remote domain.
pub async fn deliver(instance: Arc<Instance>, job: &Job) -> Result<(), String> {
    let delivery: Delivery = serde_json::from_str(&job.payload).map_err(|e| e.to_string())?;
    let last_try = job.attempt >= job.max_attempts;
    let Some(raw) = store::raw(&instance, &delivery.user, &delivery.message)
        .await
        .map_err(|e| e.to_string())?
    else {
        // The sender deleted it before it left: nothing to send.
        record(&instance, &delivery, "cancelled", None).await;
        return Ok(());
    };
    let envelope = Envelope::new(
        Some(delivery.from.parse().map_err(|e| format!("{e}"))?),
        delivery.to.iter().filter_map(|t| t.parse().ok()).collect(),
    )
    .map_err(|e| e.to_string())?;
    let hello = crate::domains::Hosts::of(&instance).mail;
    let servers = match exchangers(&delivery.domain).await {
        Ok(servers) => servers,
        Err(e) if last_try => {
            record(&instance, &delivery, "bounced", Some(&e)).await;
            bounce(&instance, &delivery, &e).await;
            return Ok(());
        }
        Err(e) => {
            record(&instance, &delivery, "deferred", Some(&e)).await;
            return Err(e);
        }
    };
    let mut last_error = String::new();
    for (host, ip) in servers {
        ortus_core::outbound::note(instance.store.as_ref(), "mail", &host).await;
        match try_deliver(&host, ip, &hello, &envelope, &raw).await {
            Outcome::Delivered => {
                record_at(&instance, &delivery, "sent", None, Some(&host)).await;
                return Ok(());
            }
            Outcome::Permanent(e) => {
                record_at(&instance, &delivery, "bounced", Some(&e), Some(&host)).await;
                bounce(&instance, &delivery, &e).await;
                return Ok(());
            }
            Outcome::Temporary(e) => last_error = format!("{host}: {e}"),
        }
    }
    if last_try {
        record(&instance, &delivery, "bounced", Some(&last_error)).await;
        bounce(
            &instance,
            &delivery,
            &format!("no server took it for days; the last said: {last_error}"),
        )
        .await;
        return Ok(());
    }
    record(&instance, &delivery, "deferred", Some(&last_error)).await;
    Err(last_error)
}
