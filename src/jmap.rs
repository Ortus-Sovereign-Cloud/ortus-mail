//! JMAP (RFC 8620 Core, RFC 8621 Mail): what modern mail apps and the
//! web inbox speak. Apps sign in with `Authorization` (a session token or
//! an app password, `Access::Header`); the web client calls the same
//! methods with its session at `/mail/web/jmap`.
//!
//! The account is the person; a JMAP Email is one stored message, in one
//! or more mailboxes, with keywords from its IMAP flags. States change with
//! any change to the person's mail; `/changes` answers
//! `cannotCalculateChanges` when they differ, and apps fetch again.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use mail_parser::MessageParser;
use ortus_core::api::{answer, ok, ApiError, Method, Route};
use ortus_core::auth::accounts::User;
use ortus_core::auth::Access;
use ortus_core::instance::Instance;
use ortus_core::storage::BoxFuture;
use rust_web_server::prelude::*;
use serde_json::{json, Map, Value};

use crate::{send, store};

const CORE: &str = "urn:ietf:params:jmap:core";
const MAIL: &str = "urn:ietf:params:jmap:mail";
const SUBMISSION: &str = "urn:ietf:params:jmap:submission";
const MAX_CALLS: usize = 32;
const MAX_OBJECTS: usize = 500;
const MAX_UPLOAD: usize = 50 * 1024 * 1024;

/// IMAP flags and JMAP keywords.
fn keyword_of(flag: &str) -> String {
    match flag {
        "\\Seen" => "$seen".into(),
        "\\Flagged" => "$flagged".into(),
        "\\Answered" => "$answered".into(),
        "\\Draft" => "$draft".into(),
        "\\Deleted" => "$deleted".into(),
        other => other.to_ascii_lowercase(),
    }
}

fn flag_of(keyword: &str) -> String {
    match keyword.to_ascii_lowercase().as_str() {
        "$seen" => "\\Seen".into(),
        "$flagged" => "\\Flagged".into(),
        "$answered" => "\\Answered".into(),
        "$draft" => "\\Draft".into(),
        "$deleted" => "\\Deleted".into(),
        _ => keyword.to_string(),
    }
}

fn valid_keyword(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 255
        && k.bytes()
            .all(|b| (0x21..0x7f).contains(&b) && !b"(){]%*\"\\".contains(&b))
}

/// Something went wrong with one method call.
struct MethodError(&'static str, Option<String>);

impl MethodError {
    fn invalid(text: impl Into<String>) -> Self {
        Self("invalidArguments", Some(text.into()))
    }
    fn json(&self) -> Value {
        match &self.1 {
            Some(d) => json!({"type": self.0, "description": d}),
            None => json!({"type": self.0}),
        }
    }
}

impl From<ortus_core::storage::StoreError> for MethodError {
    fn from(e: ortus_core::storage::StoreError) -> Self {
        eprintln!("ortus: mail: jmap: {e}");
        Self("serverFail", None)
    }
}

type Answer = Result<Value, MethodError>;

struct Ctx<'a> {
    instance: &'a Arc<Instance>,
    user: &'a User,
}

impl Ctx<'_> {
    fn store(&self) -> &dyn ortus_core::storage::AsyncStore {
        self.instance.store.as_ref()
    }
}

/// The state of a person's mail: changes whenever anything in it does
/// (folders, their names and sequences, messages, snoozes).
pub async fn state(
    instance: &Instance,
    user_id: &str,
) -> Result<String, ortus_core::storage::StoreError> {
    use std::hash::{Hash, Hasher};
    let store = instance.store.as_ref();
    store::mailboxes(store, user_id).await?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for r in store
        .query(
            "SELECT id, name, modseq, uid_next FROM mail_mailboxes WHERE user_id = $1 ORDER BY id",
            &[user_id.into()],
        )
        .await?
    {
        (r.text(0), r.text(1), r.int(2), r.int(3)).hash(&mut h);
    }
    for r in store
        .query("SELECT CAST(COUNT(*) AS BIGINT) FROM mail_entries e JOIN mail_mailboxes m ON m.id = e.mailbox_id WHERE m.user_id = $1", &[user_id.into()])
        .await?
    {
        r.int(0).hash(&mut h);
    }
    for r in store
        .query(
            "SELECT message, until FROM mail_snoozed WHERE user_id = $1 ORDER BY message",
            &[user_id.into()],
        )
        .await?
    {
        (r.text(0), r.int(1)).hash(&mut h);
    }
    Ok(format!("{:016x}", h.finish()))
}

// --- Mailbox ---

async fn mailbox_get(ctx: &Ctx<'_>, args: &Value) -> Answer {
    let boxes = store::mailboxes(ctx.store(), &ctx.user.id).await?;
    let wanted: Option<Vec<String>> = ids_arg(args, "ids")?;
    let mut list = Vec::new();
    let mut not_found = Vec::new();
    let mut counts: HashMap<String, (i64, i64, i64, i64)> = HashMap::new();
    for row in ctx
        .store()
        .query(
            "SELECT e.mailbox_id, CAST(COUNT(*) AS BIGINT), CAST(SUM(CASE WHEN e.flags LIKE '%\\Seen%' THEN 0 ELSE 1 END) AS BIGINT), CAST(COUNT(DISTINCT m.thread_id) AS BIGINT) FROM mail_entries e JOIN mail_messages m ON m.id = e.message JOIN mail_mailboxes b ON b.id = e.mailbox_id WHERE b.user_id = $1 GROUP BY e.mailbox_id",
            &[ctx.user.id.as_str().into()],
        )
        .await?
    {
        let id = row.text(0).unwrap_or_default().to_string();
        counts.insert(id, (row.int(1).unwrap_or(0), row.int(2).unwrap_or(0), row.int(3).unwrap_or(0), 0));
    }
    for row in ctx
        .store()
        .query(
            "SELECT e.mailbox_id, CAST(COUNT(DISTINCT m.thread_id) AS BIGINT) FROM mail_entries e JOIN mail_messages m ON m.id = e.message JOIN mail_mailboxes b ON b.id = e.mailbox_id WHERE b.user_id = $1 AND e.flags NOT LIKE '%\\Seen%' GROUP BY e.mailbox_id",
            &[ctx.user.id.as_str().into()],
        )
        .await?
    {
        if let Some(c) = counts.get_mut(row.text(0).unwrap_or_default()) {
            c.3 = row.int(1).unwrap_or(0);
        }
    }
    let order = |role: Option<&str>| {
        store::DEFAULT_MAILBOXES
            .iter()
            .position(|(r, _)| Some(*r) == role)
            .map_or(100, |p| p as i64 + 1)
    };
    let all: Vec<Value> = boxes
        .iter()
        .map(|m| {
            let (total, unread, threads, unread_threads) = counts.get(&m.id).copied().unwrap_or_default();
            json!({
                "id": m.id, "name": m.name, "parentId": null, "role": m.role,
                "sortOrder": order(m.role.as_deref()),
                "totalEmails": total, "unreadEmails": unread,
                "totalThreads": threads, "unreadThreads": unread_threads,
                "myRights": {"mayReadItems": true, "mayAddItems": true, "mayRemoveItems": true, "maySetSeen": true, "maySetKeywords": true,
                    "mayCreateChild": false, "mayRename": m.role.is_none(), "mayDelete": m.role.is_none(), "maySubmit": true},
                "isSubscribed": true,
            })
        })
        .collect();
    match wanted {
        None => list = all,
        Some(ids) => {
            for id in ids {
                match all.iter().find(|m| m["id"] == id.as_str()) {
                    Some(m) => list.push(m.clone()),
                    None => not_found.push(id),
                }
            }
        }
    }
    let list = only_properties(list, args);
    Ok(
        json!({"accountId": ctx.user.id, "state": state(ctx.instance, &ctx.user.id).await?, "list": list, "notFound": not_found}),
    )
}

async fn mailbox_query(ctx: &Ctx<'_>, _args: &Value) -> Answer {
    let boxes = store::mailboxes(ctx.store(), &ctx.user.id).await?;
    let ids: Vec<&str> = boxes.iter().map(|m| m.id.as_str()).collect();
    Ok(
        json!({"accountId": ctx.user.id, "queryState": state(ctx.instance, &ctx.user.id).await?, "canCalculateChanges": false, "position": 0, "ids": ids, "total": ids.len()}),
    )
}

async fn mailbox_set(
    ctx: &Ctx<'_>,
    args: &Value,
    created_ids: &mut HashMap<String, String>,
) -> Answer {
    let old_state = state(ctx.instance, &ctx.user.id).await?;
    check_if_in_state(args, &old_state)?;
    let mut created = Map::new();
    let mut not_created = Map::new();
    if let Some(create) = args.get("create").and_then(Value::as_object) {
        for (key, value) in create {
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if value.get("parentId").is_some_and(|p| !p.is_null()) {
                not_created.insert(key.clone(), json!({"type": "invalidProperties", "properties": ["parentId"], "description": "Folders are not nested here."}));
                continue;
            }
            match store::create_mailbox(ctx.store(), &ctx.user.id, name).await? {
                Some(m) => {
                    created_ids.insert(key.clone(), m.id.clone());
                    created.insert(key.clone(), json!({"id": m.id}));
                }
                None => {
                    not_created.insert(
                        key.clone(),
                        json!({"type": "invalidProperties", "properties": ["name"]}),
                    );
                }
            }
        }
    }
    let boxes = store::mailboxes(ctx.store(), &ctx.user.id).await?;
    let mut updated = Map::new();
    let mut not_updated = Map::new();
    if let Some(update) = args.get("update").and_then(Value::as_object) {
        for (id, patch) in update {
            let Some(mailbox) = boxes.iter().find(|m| &m.id == id) else {
                not_updated.insert(id.clone(), json!({"type": "notFound"}));
                continue;
            };
            let mut ok = true;
            for (property, value) in patch.as_object().into_iter().flatten() {
                match property.as_str() {
                    "name" => {
                        ok &= value.as_str().is_some_and(|n| n == mailbox.name)
                            || store::rename_mailbox(
                                ctx.store(),
                                &ctx.user.id,
                                &mailbox.name,
                                value.as_str().unwrap_or_default(),
                            )
                            .await?
                    }
                    "isSubscribed" | "sortOrder" => {}
                    _ => ok = false,
                }
            }
            if ok {
                updated.insert(id.clone(), Value::Null);
            } else {
                not_updated.insert(id.clone(), json!({"type": "invalidProperties"}));
            }
        }
    }
    let mut destroyed = Vec::new();
    let mut not_destroyed = Map::new();
    // A folder with mail goes only when the app says its mail may go too.
    let remove_emails = args
        .get("onDestroyRemoveEmails")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    for id in ids_arg(args, "destroy")?.unwrap_or_default() {
        let has_mail = !ctx
            .store()
            .query(
                "SELECT uid FROM mail_entries WHERE mailbox_id = $1 LIMIT 1",
                &[id.as_str().into()],
            )
            .await?
            .is_empty();
        match boxes.iter().find(|m| m.id == id) {
            Some(m) if m.role.is_none() && has_mail && !remove_emails => {
                not_destroyed.insert(id, json!({"type": "mailboxHasEmail"}));
            }
            Some(m)
                if m.role.is_none()
                    && store::delete_mailbox(ctx.store(), &ctx.user.id, &m.name).await? =>
            {
                destroyed.push(id)
            }
            Some(_) => {
                not_destroyed.insert(
                    id,
                    json!({"type": "forbidden", "description": "The default folders stay."}),
                );
            }
            None => {
                not_destroyed.insert(id, json!({"type": "notFound"}));
            }
        }
    }
    Ok(
        json!({"accountId": ctx.user.id, "oldState": old_state, "newState": state(ctx.instance, &ctx.user.id).await?,
        "created": created, "notCreated": not_created, "updated": updated, "notUpdated": not_updated, "destroyed": destroyed, "notDestroyed": not_destroyed}),
    )
}

// --- Email ---

/// What the store knows of one of the person's messages, without its file.
struct Stored {
    id: String,
    thread: String,
    blob: String,
    size: i64,
    received_at: i64,
    subject: String,
    preview: String,
    attachments: i64,
    mailboxes: Vec<(String, i64)>,
    keywords: Vec<String>,
    category: String,
    verified: bool,
}

/// Whether the sender is who they say: mail from inside the installation,
/// or DKIM aligned with the From domain (DMARC).
fn verified(auth: Option<&str>) -> bool {
    let Some(auth) = auth.and_then(|a| serde_json::from_str::<Value>(a).ok()) else {
        return false;
    };
    auth["internal"] == true || auth["dmarc"] == "pass"
}

async fn stored(ctx: &Ctx<'_>, ids: &[String]) -> Result<Vec<Stored>, MethodError> {
    let mut out = Vec::new();
    for id in ids {
        let rows = ctx
            .store()
            .query(
                "SELECT id, thread_id, blob, size, received_at, subject, preview, attachments, category, auth FROM mail_messages WHERE id = $1 AND user_id = $2",
                &[id.as_str().into(), ctx.user.id.as_str().into()],
            )
            .await?;
        let Some(r) = rows.first() else { continue };
        let entries = ctx.store().query("SELECT mailbox_id, uid, flags FROM mail_entries WHERE message = $1 ORDER BY mailbox_id", &[id.as_str().into()]).await?;
        if entries.is_empty() {
            continue;
        }
        let keywords: Vec<String> = entries[0]
            .text(2)
            .unwrap_or_default()
            .split(' ')
            .filter(|f| !f.is_empty())
            .map(keyword_of)
            .collect();
        out.push(Stored {
            id: id.clone(),
            thread: r.text(1).unwrap_or_default().into(),
            blob: r.text(2).unwrap_or_default().into(),
            size: r.int(3).unwrap_or(0),
            received_at: r.int(4).unwrap_or(0),
            subject: r.text(5).unwrap_or_default().into(),
            preview: r.text(6).unwrap_or_default().into(),
            attachments: r.int(7).unwrap_or(0),
            category: r.text(8).unwrap_or("important").into(),
            verified: verified(r.text(9)),
            mailboxes: entries
                .iter()
                .map(|e| {
                    (
                        e.text(0).unwrap_or_default().to_string(),
                        e.int(1).unwrap_or(0),
                    )
                })
                .collect(),
            keywords,
        })
    }
    Ok(out)
}

fn utc(ms: i64) -> String {
    let d = mail_parser::DateTime::from_timestamp(ms.div_euclid(1000));
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        d.year, d.month, d.day, d.hour, d.minute, d.second
    )
}

fn addresses(list: Option<mail_parser::AddressList<'_>>) -> Value {
    match list {
        None => Value::Null,
        Some(list) => Value::Array(
            list.mailboxes()
                .map(|m| json!({"name": m.name(), "email": m.address()}))
                .collect(),
        ),
    }
}

fn body_part(message_id: &str, part: &mail_parser::MessagePart<'_>) -> Value {
    let index = part.id();
    let ct = part.content_type();
    let kind = ct.as_ref().map_or("text/plain".to_string(), |c| {
        format!("{}/{}", c.ctype(), c.subtype().unwrap_or("plain")).to_ascii_lowercase()
    });
    json!({
        "partId": index.to_string(),
        "blobId": format!("{message_id}.{index}"),
        "size": part.decoded_len(),
        "type": kind,
        "charset": ct.as_ref().and_then(|c| c.attribute("charset")),
        "name": part.attachment_name(),
        "disposition": part.content_disposition().map(|d| d.ctype().to_ascii_lowercase()),
        "cid": part.content_id(),
    })
}

/// Where a message is (mailbox, UID) and its keywords.
type Placed = (Vec<(String, i64)>, Vec<String>);

/// Every message of the person's that is in a folder, newest first, in
/// two queries.
async fn stored_all(ctx: &Ctx<'_>) -> Result<Vec<Stored>, MethodError> {
    let user = ctx.user.id.as_str();
    let mut entries: HashMap<String, Placed> = HashMap::new();
    for e in ctx
        .store()
        .query("SELECT e.message, e.mailbox_id, e.uid, e.flags FROM mail_entries e JOIN mail_mailboxes b ON b.id = e.mailbox_id WHERE b.user_id = $1 ORDER BY e.mailbox_id", &[user.into()])
        .await?
    {
        let slot = entries.entry(e.text(0).unwrap_or_default().to_string()).or_default();
        if slot.0.is_empty() {
            slot.1 = e.text(3).unwrap_or_default().split(' ').filter(|f| !f.is_empty()).map(keyword_of).collect();
        }
        slot.0.push((e.text(1).unwrap_or_default().to_string(), e.int(2).unwrap_or(0)));
    }
    let mut out = Vec::new();
    for r in ctx
        .store()
        .query("SELECT id, thread_id, blob, size, received_at, subject, preview, attachments, category, auth FROM mail_messages WHERE user_id = $1 ORDER BY received_at DESC, id", &[user.into()])
        .await?
    {
        let id = r.text(0).unwrap_or_default().to_string();
        let Some((mailboxes, keywords)) = entries.remove(&id) else { continue };
        out.push(Stored {
            id,
            thread: r.text(1).unwrap_or_default().into(),
            blob: r.text(2).unwrap_or_default().into(),
            size: r.int(3).unwrap_or(0),
            received_at: r.int(4).unwrap_or(0),
            subject: r.text(5).unwrap_or_default().into(),
            preview: r.text(6).unwrap_or_default().into(),
            attachments: r.int(7).unwrap_or(0),
            category: r.text(8).unwrap_or("important").into(),
            verified: verified(r.text(9)),
            mailboxes,
            keywords,
        });
    }
    Ok(out)
}

const EMAIL_DEFAULT: &[&str] = &[
    "id",
    "blobId",
    "threadId",
    "mailboxIds",
    "keywords",
    "size",
    "receivedAt",
    "messageId",
    "inReplyTo",
    "references",
    "sender",
    "from",
    "to",
    "cc",
    "bcc",
    "replyTo",
    "subject",
    "sentAt",
    "hasAttachment",
    "preview",
    "bodyValues",
    "textBody",
    "htmlBody",
    "attachments",
];

async fn email_get(ctx: &Ctx<'_>, args: &Value) -> Answer {
    let ids = ids_arg(args, "ids")?.ok_or_else(|| MethodError::invalid("Email/get needs ids"))?;
    if ids.len() > MAX_OBJECTS {
        return Err(MethodError("requestTooLarge", None));
    }
    let properties: Vec<String> = match args.get("properties") {
        Some(Value::Array(p)) => p
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => EMAIL_DEFAULT.iter().map(|p| p.to_string()).collect(),
    };
    let flag = |name: &str| args.get(name).and_then(Value::as_bool).unwrap_or(false);
    let max_bytes = args
        .get("maxBodyValueBytes")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let needs_file = properties.iter().any(|p| {
        !matches!(
            p.as_str(),
            "id" | "blobId"
                | "threadId"
                | "mailboxIds"
                | "keywords"
                | "size"
                | "receivedAt"
                | "subject"
                | "preview"
                | "hasAttachment"
                | "ortusCategory"
                | "ortusVerified"
                | "ortusSnoozedUntil"
        )
    });
    let found = stored(ctx, &ids).await?;
    let mut list = Vec::new();
    for s in &found {
        let mut email = Map::new();
        email.insert("id".into(), json!(s.id));
        let raw = if needs_file {
            ctx.instance
                .files
                .get(&s.blob)
                .await
                .ok()
                .flatten()
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let parsed = if needs_file {
            MessageParser::default().parse(&raw)
        } else {
            None
        };
        for p in &properties {
            let value = match p.as_str() {
                "id" => continue,
                "blobId" => json!(s.id),
                "threadId" => json!(s.thread),
                "mailboxIds" => Value::Object(
                    s.mailboxes
                        .iter()
                        .map(|(m, _)| (m.clone(), json!(true)))
                        .collect(),
                ),
                "keywords" => Value::Object(
                    s.keywords
                        .iter()
                        .map(|k| (k.clone(), json!(true)))
                        .collect(),
                ),
                "size" => json!(s.size),
                "receivedAt" => json!(utc(s.received_at)),
                "subject" => json!(s.subject),
                "preview" => json!(s.preview),
                "hasAttachment" => json!(s.attachments > 0),
                "ortusCategory" => json!(s.category),
                "ortusVerified" => json!(s.verified),
                "ortusSnoozedUntil" => store::snoozed_until(ctx.store(), &ctx.user.id, &s.id)
                    .await?
                    .map_or(Value::Null, |t| json!(utc(t))),
                _ => {
                    let Some(m) = &parsed else {
                        email.insert(p.clone(), Value::Null);
                        continue;
                    };
                    match p.as_str() {
                        "messageId" => m.message_id().map_or(Value::Null, |v| json!([v])),
                        "inReplyTo" => m
                            .in_reply_to()
                            .map_or(Value::Null, |v| json!(v.iter().collect::<Vec<_>>())),
                        "references" => m
                            .references()
                            .map_or(Value::Null, |v| json!(v.iter().collect::<Vec<_>>())),
                        "sender" => addresses(m.sender()),
                        "from" => addresses(m.from()),
                        "to" => addresses(m.to()),
                        "cc" => addresses(m.cc()),
                        "bcc" => addresses(m.bcc()),
                        "replyTo" => addresses(m.reply_to()),
                        "sentAt" => m.date().map_or(Value::Null, |d| json!(d.to_rfc3339())),
                        // The text to show: the plain part, or the HTML one as text.
                        "ortusText" => {
                            json!(m.body_text(0).map(|t| t.into_owned()).unwrap_or_default())
                        }
                        "textBody" | "htmlBody" | "attachments" | "bodyValues" => {
                            let text: Vec<mail_parser::MessagePart<'_>> = m.text_body().collect();
                            let html: Vec<mail_parser::MessagePart<'_>> = m.html_body().collect();
                            let attached: Vec<mail_parser::MessagePart<'_>> =
                                m.attachments().collect();
                            match p.as_str() {
                                "textBody" => json!(text
                                    .iter()
                                    .map(|part| body_part(&s.id, part))
                                    .collect::<Vec<_>>()),
                                "htmlBody" => json!(html
                                    .iter()
                                    .map(|part| body_part(&s.id, part))
                                    .collect::<Vec<_>>()),
                                "attachments" => json!(attached
                                    .iter()
                                    .map(|part| body_part(&s.id, part))
                                    .collect::<Vec<_>>()),
                                _ => {
                                    let mut values = Map::new();
                                    let mut chosen: Vec<&mail_parser::MessagePart<'_>> = Vec::new();
                                    if flag("fetchTextBodyValues") || flag("fetchAllBodyValues") {
                                        chosen.extend(text.iter().filter(|p| p.is_text()));
                                    }
                                    if flag("fetchHTMLBodyValues") || flag("fetchAllBodyValues") {
                                        chosen.extend(html.iter().filter(|p| p.is_text()));
                                    }
                                    for part in chosen {
                                        let mut value =
                                            part.text().map(|t| t.into_owned()).unwrap_or_default();
                                        let mut truncated = false;
                                        if max_bytes > 0 && value.len() > max_bytes {
                                            let mut end = max_bytes;
                                            while !value.is_char_boundary(end) {
                                                end -= 1;
                                            }
                                            value.truncate(end);
                                            truncated = true;
                                        }
                                        values.insert(part.id().to_string(), json!({"value": value, "isEncodingProblem": false, "isTruncated": truncated}));
                                    }
                                    Value::Object(values)
                                }
                            }
                        }
                        _ => Value::Null,
                    }
                }
            };
            email.insert(p.clone(), value);
        }
        list.push(Value::Object(email));
    }
    let not_found: Vec<&String> = ids
        .iter()
        .filter(|id| !found.iter().any(|s| &s.id == *id))
        .collect();
    Ok(
        json!({"accountId": ctx.user.id, "state": state(ctx.instance, &ctx.user.id).await?, "list": list, "notFound": not_found}),
    )
}

/// Whether a stored message matches a JMAP filter (condition or operator).
fn matches(
    filter: &Value,
    s: &Stored,
    text: &str,
    from: &str,
    recipients: &str,
) -> Result<bool, MethodError> {
    if let Some(op) = filter.get("operator").and_then(Value::as_str) {
        let conditions = filter
            .get("conditions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut results = Vec::new();
        for c in &conditions {
            results.push(matches(c, s, text, from, recipients)?);
        }
        return match op {
            "AND" => Ok(results.iter().all(|r| *r)),
            "OR" => Ok(results.iter().any(|r| *r)),
            "NOT" => Ok(!results.iter().any(|r| *r)),
            _ => Err(MethodError("unsupportedFilter", Some(op.into()))),
        };
    }
    let has = |hay: &str, needle: &Value| {
        needle
            .as_str()
            .is_some_and(|n| hay.to_lowercase().contains(&n.to_lowercase()))
    };
    for (key, value) in filter.as_object().into_iter().flatten() {
        let ok = match key.as_str() {
            "inMailbox" => value
                .as_str()
                .is_some_and(|m| s.mailboxes.iter().any(|(id, _)| id == m)),
            "inMailboxOtherThan" => value.as_array().is_some_and(|not| {
                s.mailboxes
                    .iter()
                    .any(|(id, _)| !not.iter().any(|n| n == id.as_str()))
            }),
            "hasKeyword" => value
                .as_str()
                .is_some_and(|k| s.keywords.iter().any(|x| x.eq_ignore_ascii_case(k))),
            "notKeyword" => value
                .as_str()
                .is_some_and(|k| !s.keywords.iter().any(|x| x.eq_ignore_ascii_case(k))),
            "subject" => has(&s.subject, value),
            "from" => has(from, value),
            "to" | "cc" | "bcc" => has(recipients, value),
            "text" | "body" => has(text, value) || has(&s.subject, value) || has(from, value),
            "before" => value
                .as_str()
                .and_then(parse_utc)
                .is_some_and(|t| s.received_at < t),
            "after" => value
                .as_str()
                .and_then(parse_utc)
                .is_some_and(|t| s.received_at >= t),
            "minSize" => value.as_i64().is_some_and(|n| s.size >= n),
            "maxSize" => value.as_i64().is_some_and(|n| s.size < n),
            "hasAttachment" => value.as_bool().is_some_and(|b| (s.attachments > 0) == b),
            "ortusCategory" => value.as_str().is_some_and(|c| s.category == c),
            _ => return Err(MethodError("unsupportedFilter", Some(key.clone()))),
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `2026-11-20T09:00:00Z` as Unix ms.
fn parse_utc(text: &str) -> Option<i64> {
    let dt = mail_parser::DateTime::parse_rfc3339(text)?;
    Some(dt.to_timestamp() * 1000)
}

async fn email_query(ctx: &Ctx<'_>, args: &Value) -> Answer {
    let filter = args.get("filter").cloned().unwrap_or(Value::Null);
    let filter_text = filter.to_string();
    let needs_body = filter_text.contains("\"text\"") || filter_text.contains("\"body\"");
    let info: HashMap<String, (String, String)> = ctx
        .store()
        .query("SELECT id, COALESCE(from_name, '') || ' ' || COALESCE(from_addr, ''), recipients FROM mail_messages WHERE user_id = $1", &[ctx.user.id.as_str().into()])
        .await?
        .iter()
        .map(|r| (r.text(0).unwrap_or_default().to_string(), (r.text(1).unwrap_or_default().to_string(), r.text(2).unwrap_or_default().to_string())))
        .collect();
    let mut all = stored_all(ctx).await?;
    let ascending = args
        .get("sort")
        .and_then(Value::as_array)
        .and_then(|s| s.first())
        .and_then(|s| s.get("isAscending"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(sort) = args
        .get("sort")
        .and_then(Value::as_array)
        .and_then(|s| s.first())
    {
        match sort.get("property").and_then(Value::as_str) {
            Some("receivedAt") | None => {}
            Some("size") => all.sort_by_key(|s| -s.size),
            Some("subject") => all.sort_by_key(|s| std::cmp::Reverse(s.subject.to_lowercase())),
            Some(_) => return Err(MethodError("unsupportedSort", None)),
        }
    }
    if ascending {
        all.reverse();
    }
    let collapse = args
        .get("collapseThreads")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut seen_threads = HashSet::new();
    let mut hits = Vec::new();
    // Reading message files is slow (S3): a body search looks through the
    // newest messages only.
    let mut bodies_left = 1000usize;
    for s in &all {
        let (from, recipients) = info.get(&s.id).cloned().unwrap_or_default();
        if needs_body {
            if bodies_left == 0 {
                break;
            }
            bodies_left -= 1;
        }
        let text = if needs_body {
            let raw = ctx
                .instance
                .files
                .get(&s.blob)
                .await
                .ok()
                .flatten()
                .unwrap_or_default();
            MessageParser::default()
                .parse(&raw)
                .map(|m| m.text_bodies().collect::<Vec<_>>().join("\n"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        if !filter.is_null() && !matches(&filter, s, &text, &from, &recipients)? {
            continue;
        }
        if collapse && !seen_threads.insert(s.thread.clone()) {
            continue;
        }
        hits.push(s.id.clone());
    }
    let total = hits.len();
    let mut position = args.get("position").and_then(Value::as_i64).unwrap_or(0);
    if let Some(anchor) = args.get("anchor").and_then(Value::as_str) {
        let at = hits
            .iter()
            .position(|h| h == anchor)
            .ok_or(MethodError("anchorNotFound", None))? as i64;
        position = (at
            + args
                .get("anchorOffset")
                .and_then(Value::as_i64)
                .unwrap_or(0))
        .max(0);
    }
    if position < 0 {
        position = (total as i64 + position).max(0);
    }
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(100, |l| (l as usize).min(MAX_OBJECTS));
    let page: Vec<&String> = hits.iter().skip(position as usize).take(limit).collect();
    Ok(
        json!({"accountId": ctx.user.id, "queryState": state(ctx.instance, &ctx.user.id).await?, "canCalculateChanges": false, "position": position, "ids": page, "total": total, "limit": limit}),
    )
}

/// Applies a JMAP Email update (whole `keywords`/`mailboxIds`, or patches
/// like `keywords/$seen`) to one stored message.
async fn email_update(
    ctx: &Ctx<'_>,
    s: &Stored,
    patch: &Map<String, Value>,
    created_ids: &HashMap<String, String>,
) -> Result<(), Value> {
    let boxes = store::mailboxes(ctx.store(), &ctx.user.id)
        .await
        .map_err(|_| json!({"type": "serverFail"}))?;
    let resolve = |id: &str| -> String {
        id.strip_prefix('#')
            .and_then(|k| created_ids.get(k))
            .cloned()
            .unwrap_or_else(|| id.to_string())
    };
    let mut keywords: Vec<String> = s.keywords.clone();
    let mut mailboxes: Vec<String> = s.mailboxes.iter().map(|(m, _)| m.clone()).collect();
    // Some(Some(t)): snooze until t; Some(None): wake it now.
    let mut snooze: Option<Option<i64>> = None;
    for (path, value) in patch {
        if path == "ortusSnoozedUntil" {
            snooze = Some(match value {
                Value::Null => None,
                v => Some(v.as_str().and_then(parse_utc).filter(|t| *t > ortus_core::program::now_ms()).ok_or_else(|| json!({"type": "invalidProperties", "properties": ["ortusSnoozedUntil"]}))?),
            });
        } else if path == "keywords" {
            keywords = value
                .as_object()
                .ok_or_else(|| json!({"type": "invalidPatch"}))?
                .keys()
                .cloned()
                .collect();
        } else if let Some(k) = path.strip_prefix("keywords/") {
            keywords.retain(|x| !x.eq_ignore_ascii_case(k));
            if value.as_bool() == Some(true) {
                keywords.push(k.to_ascii_lowercase());
            }
        } else if path == "mailboxIds" {
            mailboxes = value
                .as_object()
                .ok_or_else(|| json!({"type": "invalidPatch"}))?
                .keys()
                .map(|k| resolve(k))
                .collect();
        } else if let Some(m) = path.strip_prefix("mailboxIds/") {
            let m = resolve(m);
            mailboxes.retain(|x| *x != m);
            if value.as_bool() == Some(true) {
                mailboxes.push(m);
            }
        } else {
            return Err(json!({"type": "invalidProperties", "properties": [path]}));
        }
    }
    if keywords.iter().any(|k| !valid_keyword(k)) {
        return Err(json!({"type": "invalidProperties", "properties": ["keywords"]}));
    }
    if mailboxes.is_empty() {
        return Err(
            json!({"type": "invalidProperties", "properties": ["mailboxIds"], "description": "A message stays in at least one folder."}),
        );
    }
    if mailboxes.iter().any(|m| !boxes.iter().any(|b| &b.id == m)) {
        return Err(json!({"type": "invalidProperties", "properties": ["mailboxIds"]}));
    }
    let flags: Vec<String> = keywords.iter().map(|k| flag_of(k)).collect();
    let flag_refs: Vec<&str> = flags.iter().map(String::as_str).collect();
    let fail = |_| json!({"type": "serverFail"});
    if let Some(snooze) = snooze {
        match snooze {
            Some(until) => {
                store::snooze(ctx.instance, &ctx.user.id, &s.id, until)
                    .await
                    .map_err(fail)?;
            }
            None => {
                store::unsnooze(ctx.store(), &ctx.user.id, &s.id)
                    .await
                    .map_err(fail)?;
                let inbox = store::by_role(ctx.store(), &ctx.user.id, "inbox")
                    .await
                    .map_err(fail)?;
                store::move_message(ctx.instance, &ctx.user.id, &s.id, &inbox.id)
                    .await
                    .map_err(fail)?;
            }
        }
        return set_keywords(ctx, &s.id, &flag_refs).await;
    }
    // Into new folders first (a copy keeps the flags), then out of old ones.
    let (first_box, first_uid) = s.mailboxes[0].clone();
    for m in &mailboxes {
        if !s.mailboxes.iter().any(|(x, _)| x == m) {
            store::copy(ctx.store(), &first_box, first_uid, m)
                .await
                .map_err(fail)?;
        }
    }
    for (m, uid) in &s.mailboxes {
        if !mailboxes.contains(m) {
            store::expunge(ctx.instance, m, *uid).await.map_err(fail)?;
        }
    }
    set_keywords(ctx, &s.id, &flag_refs).await
}

/// Sets the flags of every entry of a message to `flag_refs`.
async fn set_keywords(ctx: &Ctx<'_>, message: &str, flag_refs: &[&str]) -> Result<(), Value> {
    let fail = |_| json!({"type": "serverFail"});
    for row in ctx
        .store()
        .query(
            "SELECT mailbox_id, uid, flags FROM mail_entries WHERE message = $1",
            &[message.into()],
        )
        .await
        .map_err(fail)?
    {
        if row.text(2).unwrap_or_default() != store::normal_flags(flag_refs) {
            store::set_flags(
                ctx.store(),
                row.text(0).unwrap_or_default(),
                row.int(1).unwrap_or(0),
                flag_refs,
            )
            .await
            .map_err(fail)?;
        }
    }
    Ok(())
}

async fn destroy_email(ctx: &Ctx<'_>, s: &Stored) -> Result<(), MethodError> {
    for (m, uid) in &s.mailboxes {
        store::expunge(ctx.instance, m, *uid).await?;
    }
    Ok(())
}

/// A header text as RFC 2047 encoded words when it is not plain ASCII.
fn encoded(text: &str) -> String {
    use base64::Engine;
    if text.is_ascii() {
        return text.to_string();
    }
    // Encoded words stay short: split on character boundaries.
    let mut words = Vec::new();
    let mut chunk = String::new();
    for c in text.chars() {
        if chunk.len() + c.len_utf8() > 45 {
            words.push(format!(
                "=?UTF-8?B?{}?=",
                base64::engine::general_purpose::STANDARD.encode(&chunk)
            ));
            chunk.clear();
        }
        chunk.push(c);
    }
    if !chunk.is_empty() {
        words.push(format!(
            "=?UTF-8?B?{}?=",
            base64::engine::general_purpose::STANDARD.encode(&chunk)
        ));
    }
    words.join("\r\n ")
}

/// A new message (a draft, usually) from the JMAP properties, with the
/// attachments it names (uploaded blobs).
async fn compose(ctx: &Ctx<'_>, email: &Value) -> Result<Vec<u8>, Value> {
    let list = |name: &str| -> String {
        email
            .get(name)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        let address = x.get("email")?.as_str()?;
                        if address.contains(['\r', '\n', '<', '>', ',']) {
                            return None;
                        }
                        Some(
                            match x
                                .get("name")
                                .and_then(Value::as_str)
                                .filter(|n| !n.is_empty())
                            {
                                Some(n) if n.is_ascii() => format!(
                                    "\"{}\" <{address}>",
                                    n.replace(['"', '\\', '\r', '\n'], "")
                                ),
                                Some(n) => format!(
                                    "{} <{address}>",
                                    encoded(&n.replace(['"', '\\', '\r', '\n'], ""))
                                ),
                                None => format!("<{address}>"),
                            },
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    let clean = |s: &str| s.replace(['\r', '\n'], " ");
    let subject = clean(
        email
            .get("subject")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let body_of = |kind: &str| -> Option<String> {
        let part = email.get(kind)?.as_array()?.first()?;
        let part_id = part.get("partId")?.as_str()?;
        Some(
            email
                .get("bodyValues")?
                .get(part_id)?
                .get("value")?
                .as_str()?
                .to_string(),
        )
    };
    let text = body_of("textBody");
    let html = body_of("htmlBody");
    if text.is_none() && html.is_none() && email.get("bodyValues").is_some() {
        return Err(json!({"type": "invalidProperties", "properties": ["textBody"]}));
    }
    let from = match list("from") {
        f if f.is_empty() => format!("<{}>", ctx.user.email),
        f => f,
    };
    let domain = ctx
        .user
        .email
        .rsplit_once('@')
        .map_or("ortus.local", |(_, d)| d);
    let mut head = format!(
        "From: {from}\r\nDate: {}\r\nMessage-ID: <{}@{domain}>\r\nSubject: {}\r\nMIME-Version: 1.0\r\n",
        rfc2822(ortus_core::program::now_ms()),
        ortus_core::auth::random_hex(16),
        encoded(&subject)
    );
    for (header, name) in [
        ("To", "to"),
        ("Cc", "cc"),
        ("Bcc", "bcc"),
        ("Reply-To", "replyTo"),
    ] {
        let value = list(name);
        if !value.is_empty() {
            head.push_str(&format!("{header}: {value}\r\n"));
        }
    }
    for (header, name) in [("In-Reply-To", "inReplyTo"), ("References", "references")] {
        if let Some(ids) = email.get(name).and_then(Value::as_array) {
            let ids: Vec<String> = ids
                .iter()
                .filter_map(Value::as_str)
                .filter(|i| !i.contains(['\r', '\n', '<', '>']))
                .map(|i| format!("<{i}>"))
                .collect();
            if !ids.is_empty() {
                head.push_str(&format!("{header}: {}\r\n", ids.join(" ")));
            }
        }
    }
    if let Some(label) = email
        .get("header:X-Ortus-Label:asText")
        .and_then(Value::as_str)
    {
        head.push_str(&format!("X-Ortus-Label: {}\r\n", clean(label)));
    }
    let crlf = |t: &str| t.replace("\r\n", "\n").replace('\n', "\r\n");
    let body = match (text, html) {
        (Some(t), None) => format!("Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{}\r\n", crlf(&t)),
        (None, Some(h)) => format!("Content-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{}\r\n", crlf(&h)),
        (Some(t), Some(h)) => {
            let b = format!("alt{}", ortus_core::auth::random_hex(8));
            format!("Content-Type: multipart/alternative; boundary=\"{b}\"\r\n\r\n--{b}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{}\r\n--{b}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{}\r\n--{b}--\r\n", crlf(&t), crlf(&h))
        }
        (None, None) => "Content-Type: text/plain; charset=utf-8\r\n\r\n".into(),
    };
    let files = email
        .get("attachments")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if files.is_empty() {
        return Ok(format!("{head}{body}").into_bytes());
    }
    use base64::Engine;
    // A part taken from a labelled message keeps its label.
    let mut inherited: Vec<String> = Vec::new();
    for file in &files {
        let blob = file
            .get("blobId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !blob.starts_with("u_") {
            let message = blob.split('.').next().unwrap_or_default();
            if let Ok(Some(raw)) = store::raw(ctx.instance, &ctx.user.id, message).await {
                for label in send::labels_in(&raw) {
                    if !inherited.contains(&label) {
                        inherited.push(label);
                    }
                }
            }
        }
    }
    for label in &inherited {
        head.push_str(&format!("X-Ortus-Label: {label}\r\n"));
    }
    let b = format!("mix{}", ortus_core::auth::random_hex(8));
    let mut out =
        format!("{head}Content-Type: multipart/mixed; boundary=\"{b}\"\r\n\r\n--{b}\r\n{body}");
    for file in &files {
        let blob = file
            .get("blobId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let data = blob_bytes(ctx, blob)
            .await
            .ok_or_else(|| json!({"type": "blobNotFound", "notFound": [blob]}))?;
        let kind = file
            .get("type")
            .and_then(Value::as_str)
            .filter(|t| {
                t.len() < 100
                    && t.split('/').count() == 2
                    && t.chars()
                        .all(|c| c.is_ascii_alphanumeric() || "/.+-".contains(c))
            })
            .unwrap_or("application/octet-stream");
        let name: String = file
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("attachment")
            .chars()
            .filter(|c| !c.is_control() && !matches!(c, '"' | '\\' | '/'))
            .take(200)
            .collect();
        // A name that is not plain ASCII goes as RFC 2231.
        let name_param = if name.is_ascii() {
            format!("filename=\"{name}\"")
        } else {
            let escaped: String = name
                .bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b".-_".contains(&b) {
                        (b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect();
            format!("filename*=UTF-8''{escaped}")
        };
        let encoded_data = base64::engine::general_purpose::STANDARD.encode(&data);
        let lines: Vec<&str> = encoded_data
            .as_bytes()
            .chunks(76)
            .map(|c| std::str::from_utf8(c).unwrap_or_default())
            .collect();
        out.push_str(&format!("\r\n--{b}\r\nContent-Type: {kind}\r\nContent-Disposition: attachment; {name_param}\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n", lines.join("\r\n")));
    }
    out.push_str(&format!("--{b}--\r\n"));
    Ok(out.into_bytes())
}

fn rfc2822(ms: i64) -> String {
    let d = mail_parser::DateTime::from_timestamp(ms.div_euclid(1000));
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let day = DAYS[(ms.div_euclid(86_400_000)).rem_euclid(7) as usize];
    format!(
        "{day}, {:02} {} {:04} {:02}:{:02}:{:02} +0000",
        d.day,
        MONTHS[usize::from(d.month.clamp(1, 12)) - 1],
        d.year,
        d.hour,
        d.minute,
        d.second
    )
}

/// Stores a new message in the given folders; answers its Email id.
async fn store_new(
    ctx: &Ctx<'_>,
    raw: &[u8],
    mailbox_ids: &[String],
    keywords: &[String],
) -> Result<(String, String), Value> {
    let fail = |_| json!({"type": "serverFail"});
    if raw.len() > crate::smtp::max_size() {
        return Err(json!({"type": "tooLarge"}));
    }
    if !store::fits(ctx.store(), &ctx.user.id, raw.len())
        .await
        .map_err(fail)?
    {
        return Err(json!({"type": "overQuota"}));
    }
    let flags: Vec<String> = keywords.iter().map(|k| flag_of(k)).collect();
    let flag_refs: Vec<&str> = flags.iter().map(String::as_str).collect();
    let delivered = store::deliver(
        ctx.instance,
        &ctx.user.id,
        &mailbox_ids[0],
        raw,
        &flag_refs,
        None,
        0,
    )
    .await
    .map_err(fail)?;
    for m in &mailbox_ids[1..] {
        store::copy(ctx.store(), &mailbox_ids[0], delivered.uid, m)
            .await
            .map_err(fail)?;
    }
    Ok((delivered.message, delivered.thread))
}

fn creation_mailboxes(
    ctx_boxes: &[store::Mailbox],
    value: &Value,
    created_ids: &HashMap<String, String>,
) -> Result<Vec<String>, Value> {
    let ids: Vec<String> = value
        .get("mailboxIds")
        .and_then(Value::as_object)
        .map(|m| {
            m.keys()
                .map(|k| {
                    k.strip_prefix('#')
                        .and_then(|c| created_ids.get(c))
                        .cloned()
                        .unwrap_or_else(|| k.clone())
                })
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() || ids.iter().any(|id| !ctx_boxes.iter().any(|b| &b.id == id)) {
        return Err(json!({"type": "invalidProperties", "properties": ["mailboxIds"]}));
    }
    Ok(ids)
}

fn creation_keywords(value: &Value) -> Result<Vec<String>, Value> {
    let keywords: Vec<String> = value
        .get("keywords")
        .and_then(Value::as_object)
        .map(|k| k.keys().map(|k| k.to_ascii_lowercase()).collect())
        .unwrap_or_default();
    if keywords.iter().any(|k| !valid_keyword(k)) {
        return Err(json!({"type": "invalidProperties", "properties": ["keywords"]}));
    }
    Ok(keywords)
}

async fn email_set(
    ctx: &Ctx<'_>,
    args: &Value,
    created_ids: &mut HashMap<String, String>,
) -> Answer {
    let old_state = state(ctx.instance, &ctx.user.id).await?;
    check_if_in_state(args, &old_state)?;
    let boxes = store::mailboxes(ctx.store(), &ctx.user.id).await?;
    let mut created = Map::new();
    let mut not_created = Map::new();
    for (key, value) in args
        .get("create")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let made = async {
            let mailboxes = creation_mailboxes(&boxes, value, created_ids)?;
            let keywords = creation_keywords(value)?;
            let raw = compose(ctx, value).await?;
            let (id, thread) = store_new(ctx, &raw, &mailboxes, &keywords).await?;
            Ok::<_, Value>(json!({"id": id, "blobId": id, "threadId": thread, "size": raw.len()}))
        }
        .await;
        match made {
            Ok(v) => {
                created_ids.insert(
                    key.clone(),
                    v["id"].as_str().unwrap_or_default().to_string(),
                );
                created.insert(key.clone(), v);
            }
            Err(e) => {
                not_created.insert(key.clone(), e);
            }
        }
    }
    let mut updated = Map::new();
    let mut not_updated = Map::new();
    for (id, patch) in args
        .get("update")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let id = id
            .strip_prefix('#')
            .and_then(|c| created_ids.get(c))
            .cloned()
            .unwrap_or_else(|| id.clone());
        let Some(s) = stored(ctx, std::slice::from_ref(&id))
            .await?
            .into_iter()
            .next()
        else {
            not_updated.insert(id, json!({"type": "notFound"}));
            continue;
        };
        match email_update(
            ctx,
            &s,
            patch.as_object().unwrap_or(&Map::new()),
            created_ids,
        )
        .await
        {
            Ok(()) => {
                updated.insert(id, Value::Null);
            }
            Err(e) => {
                not_updated.insert(id, e);
            }
        }
    }
    let mut destroyed = Vec::new();
    let mut not_destroyed = Map::new();
    for id in ids_arg(args, "destroy")?.unwrap_or_default() {
        match stored(ctx, std::slice::from_ref(&id))
            .await?
            .into_iter()
            .next()
        {
            Some(s) => {
                destroy_email(ctx, &s).await?;
                destroyed.push(id);
            }
            None => {
                not_destroyed.insert(id, json!({"type": "notFound"}));
            }
        }
    }
    Ok(
        json!({"accountId": ctx.user.id, "oldState": old_state, "newState": state(ctx.instance, &ctx.user.id).await?,
        "created": created, "notCreated": not_created, "updated": updated, "notUpdated": not_updated, "destroyed": destroyed, "notDestroyed": not_destroyed}),
    )
}

async fn email_import(
    ctx: &Ctx<'_>,
    args: &Value,
    created_ids: &mut HashMap<String, String>,
) -> Answer {
    let old_state = state(ctx.instance, &ctx.user.id).await?;
    check_if_in_state(args, &old_state)?;
    let boxes = store::mailboxes(ctx.store(), &ctx.user.id).await?;
    let mut created = Map::new();
    let mut not_created = Map::new();
    for (key, value) in args
        .get("emails")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let made = async {
            let mailboxes = creation_mailboxes(&boxes, value, created_ids)?;
            let keywords = creation_keywords(value)?;
            let blob = value
                .get("blobId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let raw = blob_bytes(ctx, blob)
                .await
                .ok_or_else(|| json!({"type": "blobNotFound", "notFound": [blob]}))?;
            if MessageParser::default().parse(&raw).is_none() {
                return Err(json!({"type": "invalidEmail"}));
            }
            let (id, thread) = store_new(ctx, &raw, &mailboxes, &keywords).await?;
            if let Some(path) = upload_path(&ctx.user.id, blob) {
                let _ = store::drop_upload(ctx.instance, &path).await;
            }
            Ok::<_, Value>(json!({"id": id, "blobId": id, "threadId": thread, "size": raw.len()}))
        }
        .await;
        match made {
            Ok(v) => {
                created_ids.insert(
                    key.clone(),
                    v["id"].as_str().unwrap_or_default().to_string(),
                );
                created.insert(key.clone(), v);
            }
            Err(e) => {
                not_created.insert(key.clone(), e);
            }
        }
    }
    Ok(
        json!({"accountId": ctx.user.id, "oldState": old_state, "newState": state(ctx.instance, &ctx.user.id).await?, "created": created, "notCreated": not_created}),
    )
}

async fn thread_get(ctx: &Ctx<'_>, args: &Value) -> Answer {
    let ids = ids_arg(args, "ids")?.ok_or_else(|| MethodError::invalid("Thread/get needs ids"))?;
    let mut list = Vec::new();
    let mut not_found = Vec::new();
    for id in ids {
        let emails: Vec<String> = ctx
            .store()
            .query(
                "SELECT m.id FROM mail_messages m WHERE m.user_id = $1 AND m.thread_id = $2 AND EXISTS (SELECT 1 FROM mail_entries e WHERE e.message = m.id) ORDER BY m.received_at, m.id",
                &[ctx.user.id.as_str().into(), id.as_str().into()],
            )
            .await?
            .iter()
            .filter_map(|r| r.text(0).map(str::to_string))
            .collect();
        if emails.is_empty() {
            not_found.push(id);
        } else {
            list.push(json!({"id": id, "emailIds": emails}));
        }
    }
    Ok(
        json!({"accountId": ctx.user.id, "state": state(ctx.instance, &ctx.user.id).await?, "list": list, "notFound": not_found}),
    )
}

async fn identities(ctx: &Ctx<'_>) -> Result<Vec<String>, MethodError> {
    let mut addresses: Vec<String> = ctx
        .store()
        .query(
            "SELECT address FROM mail_addresses WHERE user_id = $1 ORDER BY address",
            &[ctx.user.id.as_str().into()],
        )
        .await?
        .iter()
        .filter_map(|r| r.text(0).map(str::to_string))
        .collect();
    let domain = ctx
        .user
        .email
        .rsplit_once('@')
        .map(|(_, d)| d)
        .unwrap_or_default();
    if !addresses.contains(&ctx.user.email) && store::is_our_domain(ctx.store(), domain).await? {
        addresses.insert(0, ctx.user.email.clone());
    }
    Ok(addresses)
}

async fn identity_get(ctx: &Ctx<'_>, _args: &Value) -> Answer {
    let mut list = Vec::new();
    for a in identities(ctx).await? {
        let domain = a.rsplit_once('@').map(|(_, d)| d).unwrap_or_default();
        // Signed when the domain has its DKIM key.
        let signed = !ctx
            .store()
            .query(
                "SELECT domain FROM mail_domains WHERE domain = $1 AND dkim_key IS NOT NULL",
                &[domain.into()],
            )
            .await?
            .is_empty();
        list.push(json!({"id": a, "name": ctx.user.name, "email": a, "replyTo": null, "bcc": null, "textSignature": "", "htmlSignature": "", "mayDelete": false, "ortusSigned": signed}));
    }
    Ok(json!({"accountId": ctx.user.id, "state": "1", "list": list, "notFound": []}))
}

async fn submission_set(
    ctx: &Ctx<'_>,
    args: &Value,
    created_ids: &mut HashMap<String, String>,
    follow_up: &mut Vec<(String, Value)>,
) -> Answer {
    let old_state = state(ctx.instance, &ctx.user.id).await?;
    let mine = identities(ctx).await?;
    let mut created = Map::new();
    let mut not_created = Map::new();
    let mut sent_drafts: Vec<(String, String)> = Vec::new();
    for (key, value) in args
        .get("create")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let made = async {
            let identity = value.get("identityId").and_then(Value::as_str).unwrap_or_default();
            if !mine.iter().any(|m| m == identity) {
                return Err(json!({"type": "forbiddenFrom"}));
            }
            let email_id = value.get("emailId").and_then(Value::as_str).unwrap_or_default();
            let email_id = email_id.strip_prefix('#').and_then(|c| created_ids.get(c)).cloned().unwrap_or_else(|| email_id.to_string());
            let raw = store::raw(ctx.instance, &ctx.user.id, &email_id).await.map_err(|_| json!({"type": "serverFail"}))?.ok_or_else(|| json!({"type": "notFound", "properties": ["emailId"]}))?;
            let recipients: Vec<String> = match value.get("envelope").and_then(|e| e.get("rcptTo")).and_then(Value::as_array) {
                Some(list) => list.iter().filter_map(|r| r.get("email").and_then(Value::as_str)).map(str::to_string).collect(),
                None => {
                    let message = MessageParser::default().parse(&raw).ok_or_else(|| json!({"type": "invalidEmail"}))?;
                    [message.to(), message.cc(), message.bcc()].into_iter().flatten().flat_map(|l| l.mailboxes().filter_map(|m| m.address().map(str::to_string)).collect::<Vec<_>>()).collect()
                }
            };
            if recipients.is_empty() {
                return Err(json!({"type": "noRecipients"}));
            }
            // Bcc never leaves with the message.
            let outgoing = send::strip_bcc(&raw);
            let label = MessageParser::default().parse(&raw).and_then(|m| m.headers().value("X-Ortus-Label").and_then(|h| h.as_text().map(str::to_string)).map(|t| t.trim().to_string()));
            let reason = value.get("ortusReason").and_then(Value::as_str);
            let out = send::Outgoing { user: &ctx.user.id, from: identity, to: &recipients, raw: &outgoing, label: label.as_deref(), reason };
            // Later (or with a few seconds to undo): a scheduled submission.
            let send_at = match value.get("ortusSendAt") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_str().and_then(parse_utc).ok_or_else(|| json!({"type": "invalidProperties", "properties": ["ortusSendAt"]}))?),
            };
            if let Some(send_at) = send_at.filter(|t| *t > ortus_core::program::now_ms()) {
                let id = crate::later::schedule(ctx.instance, &out, &email_id, send_at).await.map_err(refusal_json)?;
                return Ok::<_, Value>((json!({"id": id, "undoStatus": "pending", "sendAt": utc(send_at), "emailId": email_id}), None));
            }
            let sent = send::send(ctx.instance, &out).await.map_err(refusal_json)?;
            let id = format!("s_{}", ortus_core::auth::random_hex(8));
            Ok::<_, Value>((json!({"id": id, "undoStatus": "final", "sentEmailId": sent.message}), Some(email_id)))
        }
        .await;
        match made {
            Ok((v, sent_draft)) => {
                created_ids.insert(
                    key.clone(),
                    v["id"].as_str().unwrap_or_default().to_string(),
                );
                if let Some(email_id) = sent_draft {
                    sent_drafts.push((key.clone(), email_id));
                }
                created.insert(key.clone(), v);
            }
            Err(e) => {
                not_created.insert(key.clone(), e);
            }
        }
    }
    // The server already filed a copy in Sent, so a draft that the app
    // asked to move or change after sending goes away instead.
    let wants_follow = |name: &str, key: &str| -> bool {
        match args.get(name) {
            Some(Value::Object(m)) => m.contains_key(&format!("#{key}")),
            Some(Value::Array(a)) => a.iter().any(|x| x.as_str() == Some(&format!("#{key}"))),
            _ => false,
        }
    };
    let drafts: Vec<String> = sent_drafts
        .iter()
        .filter(|(key, _)| {
            wants_follow("onSuccessUpdateEmail", key) || wants_follow("onSuccessDestroyEmail", key)
        })
        .map(|(_, id)| id.clone())
        .collect();
    if !drafts.is_empty() {
        follow_up.push((
            "Email/set".into(),
            json!({"accountId": ctx.user.id, "destroy": drafts}),
        ));
    }
    // Calling back a scheduled message: {"undoStatus": "canceled"}.
    let mut updated = Map::new();
    let mut not_updated = Map::new();
    for (id, patch) in args
        .get("update")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        if patch.get("undoStatus").and_then(Value::as_str) != Some("canceled") {
            not_updated.insert(
                id.clone(),
                json!({"type": "invalidProperties", "properties": ["undoStatus"]}),
            );
        } else if crate::later::cancel(ctx.store(), &ctx.user.id, id).await? {
            updated.insert(id.clone(), Value::Null);
        } else {
            not_updated.insert(id.clone(), json!({"type": "cannotUnsend"}));
        }
    }
    Ok(
        json!({"accountId": ctx.user.id, "oldState": old_state, "newState": state(ctx.instance, &ctx.user.id).await?, "created": created, "notCreated": not_created, "updated": updated, "notUpdated": not_updated, "destroyed": []}),
    )
}

fn refusal_json(refused: send::Refused) -> Value {
    match refused {
        send::Refused::NotYourAddress => json!({"type": "forbiddenFrom"}),
        send::Refused::BadRecipient { address } => {
            json!({"type": "invalidRecipients", "invalidRecipients": [address]})
        }
        send::Refused::Policy { address, decision } => {
            json!({"type": "forbiddenToSend", "description": format!("The label does not let it go to {address}."), "ortusDecision": decision, "ortusAddress": address})
        }
        send::Refused::NeedsReason { address, label } => {
            json!({"type": "forbiddenToSend", "description": format!("Sending {label} mail to {address} needs a reason (ortusReason)."), "ortusNeedsReason": true, "ortusAddress": address, "ortusLabel": label})
        }
        send::Refused::Failed => json!({"type": "serverFail"}),
    }
}

fn submission_json(s: &crate::later::Scheduled) -> Value {
    let undo = match s.state.as_str() {
        "pending" => "pending",
        "canceled" => "canceled",
        _ => "final",
    };
    json!({"id": s.id, "identityId": s.identity, "emailId": s.email_id, "sendAt": utc(s.send_at), "undoStatus": undo, "ortusState": s.state, "ortusError": s.error})
}

/// The person's submissions that are waiting, or that could not go.
async fn submission_get(ctx: &Ctx<'_>, args: &Value) -> Answer {
    let mut list = Vec::new();
    let mut not_found = Vec::new();
    match ids_arg(args, "ids")? {
        None => {
            list = crate::later::list(ctx.store(), &ctx.user.id)
                .await?
                .iter()
                .map(submission_json)
                .collect()
        }
        Some(ids) => {
            for id in ids {
                match crate::later::get(ctx.store(), &ctx.user.id, &id).await? {
                    Some(s) => list.push(submission_json(&s)),
                    None => not_found.push(id),
                }
            }
        }
    }
    Ok(
        json!({"accountId": ctx.user.id, "state": state(ctx.instance, &ctx.user.id).await?, "list": list, "notFound": not_found}),
    )
}

async fn submission_query(ctx: &Ctx<'_>, args: &Value) -> Answer {
    let wanted = args
        .get("filter")
        .and_then(|f| f.get("undoStatus"))
        .and_then(Value::as_str);
    let ids: Vec<String> = crate::later::list(ctx.store(), &ctx.user.id)
        .await?
        .iter()
        .filter(|s| wanted.is_none_or(|w| submission_json(s)["undoStatus"] == w))
        .map(|s| s.id.clone())
        .collect();
    Ok(
        json!({"accountId": ctx.user.id, "queryState": state(ctx.instance, &ctx.user.id).await?, "canCalculateChanges": false, "position": 0, "ids": ids, "total": ids.len()}),
    )
}

async fn changes(ctx: &Ctx<'_>, args: &Value, kind: &str) -> Answer {
    let since = args
        .get("sinceState")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let now = state(ctx.instance, &ctx.user.id).await?;
    if since != now {
        return Err(MethodError("cannotCalculateChanges", None));
    }
    let mut out = json!({"accountId": ctx.user.id, "oldState": since, "newState": now, "hasMoreChanges": false, "created": [], "updated": [], "destroyed": []});
    if kind == "Mailbox" {
        out["updatedProperties"] = Value::Null;
    }
    Ok(out)
}

// --- Requests ---

fn ids_arg(args: &Value, name: &str) -> Result<Option<Vec<String>>, MethodError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => Ok(Some(
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
        )),
        Some(_) => Err(MethodError::invalid(format!(
            "{name} must be a list of ids"
        ))),
    }
}

fn check_if_in_state(args: &Value, now: &str) -> Result<(), MethodError> {
    match args.get("ifInState").and_then(Value::as_str) {
        Some(s) if s != now => Err(MethodError("stateMismatch", None)),
        _ => Ok(()),
    }
}

fn only_properties(list: Vec<Value>, args: &Value) -> Vec<Value> {
    let Some(Value::Array(props)) = args.get("properties") else {
        return list;
    };
    let props: HashSet<&str> = props.iter().filter_map(Value::as_str).collect();
    list.into_iter()
        .map(|v| match v {
            Value::Object(m) => Value::Object(
                m.into_iter()
                    .filter(|(k, _)| k == "id" || props.contains(k.as_str()))
                    .collect(),
            ),
            other => other,
        })
        .collect()
}

/// Follows a JSON pointer (RFC 6901) with JMAP's `*` over lists.
fn pointer(value: &Value, path: &str) -> Option<Value> {
    let mut parts = path.trim_start_matches('/').splitn(2, '/');
    let Some(head) = parts.next().filter(|h| !h.is_empty() || !path.is_empty()) else {
        return Some(value.clone());
    };
    if path.is_empty() {
        return Some(value.clone());
    }
    let rest = parts.next().map(|r| format!("/{r}")).unwrap_or_default();
    let head = head.replace("~1", "/").replace("~0", "~");
    match value {
        Value::Array(items) if head == "*" => {
            let mut out = Vec::new();
            for item in items {
                match pointer(item, &rest)? {
                    Value::Array(inner) => out.extend(inner),
                    other => out.push(other),
                }
            }
            Some(Value::Array(out))
        }
        Value::Array(items) => pointer(items.get(head.parse::<usize>().ok()?)?, &rest),
        Value::Object(map) => pointer(map.get(&head)?, &rest),
        _ => None,
    }
}

/// Replaces `#name` arguments by the results they refer to.
fn resolve_references(
    args: &Value,
    responses: &[(String, Value, String)],
) -> Result<Value, MethodError> {
    let Value::Object(map) = args else {
        return Ok(args.clone());
    };
    let mut out = Map::new();
    for (key, value) in map {
        if let Some(name) = key.strip_prefix('#') {
            if map.contains_key(name) {
                return Err(MethodError::invalid(format!("both {name} and #{name}")));
            }
            let (of, call, path) = (
                value.get("resultOf").and_then(Value::as_str),
                value.get("name").and_then(Value::as_str),
                value.get("path").and_then(Value::as_str),
            );
            let (Some(of), Some(call), Some(path)) = (of, call, path) else {
                return Err(MethodError("invalidResultReference", None));
            };
            let found = responses
                .iter()
                .find(|(n, _, id)| id == of && n == call)
                .ok_or(MethodError("invalidResultReference", None))?;
            out.insert(
                name.to_string(),
                pointer(&found.1, path).ok_or(MethodError("invalidResultReference", None))?,
            );
        } else {
            out.insert(key.clone(), value.clone());
        }
    }
    Ok(Value::Object(out))
}

async fn call(
    ctx: &Ctx<'_>,
    name: &str,
    args: &Value,
    created_ids: &mut HashMap<String, String>,
    follow_up: &mut Vec<(String, Value)>,
) -> Answer {
    if let Some(account) = args.get("accountId").and_then(Value::as_str) {
        if account != ctx.user.id {
            return Err(MethodError("accountNotFound", None));
        }
    }
    match name {
        "Core/echo" => Ok(args.clone()),
        "Mailbox/get" => mailbox_get(ctx, args).await,
        "Mailbox/query" => mailbox_query(ctx, args).await,
        "Mailbox/changes" => changes(ctx, args, "Mailbox").await,
        "Mailbox/set" => mailbox_set(ctx, args, created_ids).await,
        "Email/get" => email_get(ctx, args).await,
        "Email/query" => email_query(ctx, args).await,
        "Email/changes" => changes(ctx, args, "Email").await,
        "Email/set" => email_set(ctx, args, created_ids).await,
        "Email/import" => email_import(ctx, args, created_ids).await,
        "Thread/get" => thread_get(ctx, args).await,
        "Thread/changes" => changes(ctx, args, "Thread").await,
        "Identity/get" => identity_get(ctx, args).await,
        "EmailSubmission/set" => submission_set(ctx, args, created_ids, follow_up).await,
        "EmailSubmission/get" => submission_get(ctx, args).await,
        "EmailSubmission/query" => submission_query(ctx, args).await,
        _ => Err(MethodError("unknownMethod", None)),
    }
}

/// Runs a JMAP request for a signed-in person.
pub async fn run(
    instance: &Arc<Instance>,
    user: &User,
    request: &Value,
) -> Result<Value, ApiError> {
    let problem = |kind: &str, detail: &str| {
        ApiError { status: 400, code: "jmap", message: json!({"type": format!("urn:ietf:params:jmap:error:{kind}"), "status": 400, "detail": detail}).to_string() }
    };
    let using: Vec<&str> = request
        .get("using")
        .and_then(Value::as_array)
        .map(|u| u.iter().filter_map(Value::as_str).collect())
        .ok_or_else(|| problem("notRequest", "using is missing"))?;
    if let Some(unknown) = using.iter().find(|u| ![CORE, MAIL, SUBMISSION].contains(u)) {
        return Err(problem("unknownCapability", unknown));
    }
    let calls = request
        .get("methodCalls")
        .and_then(Value::as_array)
        .ok_or_else(|| problem("notRequest", "methodCalls is missing"))?;
    if calls.len() > MAX_CALLS {
        return Err(problem("limit", "maxCallsInRequest"));
    }
    let ctx = Ctx { instance, user };
    let mut created_ids: HashMap<String, String> = request
        .get("createdIds")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let mut responses: Vec<(String, Value, String)> = Vec::new();
    for c in calls {
        let (Some(name), Some(args), Some(id)) = (
            c.get(0).and_then(Value::as_str),
            c.get(1),
            c.get(2).and_then(Value::as_str),
        ) else {
            return Err(problem(
                "notRequest",
                "a method call is [name, arguments, id]",
            ));
        };
        let mut follow_up = Vec::new();
        let result = match resolve_references(args, &responses) {
            Ok(args) => call(&ctx, name, &args, &mut created_ids, &mut follow_up).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(v) => responses.push((name.to_string(), v, id.to_string())),
            Err(e) => responses.push(("error".into(), e.json(), id.to_string())),
        }
        for (name, args) in follow_up {
            let mut none = Vec::new();
            match call(&ctx, &name, &args, &mut created_ids, &mut none).await {
                Ok(v) => responses.push((name, v, id.to_string())),
                Err(e) => responses.push(("error".into(), e.json(), id.to_string())),
            }
        }
    }
    let mut out = json!({
        "methodResponses": responses.into_iter().map(|(n, v, id)| json!([n, v, id])).collect::<Vec<_>>(),
        "sessionState": session_state(),
    });
    if request.get("createdIds").is_some() {
        out["createdIds"] = json!(created_ids.into_iter().collect::<BTreeMap<_, _>>());
    }
    Ok(out)
}

fn session_state() -> &'static str {
    "1"
}

fn session(instance: &Instance, user: &User) -> Value {
    let base = format!(
        "{}{}",
        instance.config.public_url.trim_end_matches('/'),
        ortus_core::api::PREFIX
    );
    json!({
        "capabilities": {
            CORE: {"maxSizeUpload": MAX_UPLOAD, "maxConcurrentUpload": 4, "maxSizeRequest": 10_000_000, "maxConcurrentRequests": 4,
                "maxCallsInRequest": MAX_CALLS, "maxObjectsInGet": MAX_OBJECTS, "maxObjectsInSet": MAX_OBJECTS, "collationAlgorithms": ["i;ascii-casemap"]},
            MAIL: {},
            SUBMISSION: {},
        },
        "accounts": {
            &user.id: {"name": user.email, "isPersonal": true, "isReadOnly": false, "accountCapabilities": {
                MAIL: {"maxMailboxesPerEmail": null, "maxMailboxDepth": 1, "maxSizeMailboxName": 200, "maxSizeAttachmentsPerEmail": 25 * 1024 * 1024,
                    "emailQuerySortOptions": ["receivedAt", "size", "subject"], "mayCreateTopLevelMailbox": true},
                SUBMISSION: {"maxDelayedSend": 0, "submissionExtensions": {}},
            }}
        },
        "primaryAccounts": {MAIL: &user.id, SUBMISSION: &user.id},
        "username": user.email,
        "apiUrl": format!("{base}/mail/jmap/api"),
        "downloadUrl": format!("{base}/mail/jmap/download/{{accountId}}/{{blobId}}/{{name}}?accept={{type}}"),
        "uploadUrl": format!("{base}/mail/jmap/upload/{{accountId}}"),
        "eventSourceUrl": format!("{base}/mail/jmap/eventsource?types={{types}}&closeafter={{closeafter}}&ping={{ping}}"),
        "state": session_state(),
    })
}

// --- Blobs ---

fn upload_path(user_id: &str, blob: &str) -> Option<String> {
    let id = blob.strip_prefix("u_")?;
    (id.len() == 24 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| format!("mail/{user_id}/uploads/{blob}"))
}

/// A blob of the person's: an upload, a whole message (its Email id) or
/// one part of one (`<email id>.<part>`), decoded.
async fn blob_bytes(ctx: &Ctx<'_>, blob: &str) -> Option<Vec<u8>> {
    if let Some(path) = upload_path(&ctx.user.id, blob) {
        return ctx.instance.files.get(&path).await.ok().flatten();
    }
    let (message, part) = match blob.split_once('.') {
        Some((m, p)) => (m, Some(p.parse::<u32>().ok()?)),
        None => (blob, None),
    };
    let raw = store::raw(ctx.instance, &ctx.user.id, message)
        .await
        .ok()
        .flatten()?;
    match part {
        None => Some(raw),
        Some(index) => {
            let parsed = MessageParser::default().parse(&raw)?;
            let part = parsed.part(index)?;
            (!part.is_multipart()).then(|| part.decoded().into_owned())
        }
    }
}

async fn signed(
    request: &Request,
    connection: &ConnectionInfo,
    instance: &Arc<Instance>,
) -> Result<User, ApiError> {
    let client =
        ortus_core::auth::client_address(request, connection, &instance.config.trusted_proxies);
    ortus_core::auth::by_header(request, instance, &client)
        .await
        .ok_or_else(ApiError::sign_in)
}

fn h_session(
    request: Request,
    _: PathParams,
    connection: ConnectionInfo,
    instance: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move {
        let result = async {
            let user = signed(&request, &connection, &instance).await?;
            Ok(ok(&session(&instance, &user)))
        }
        .await;
        answer(&request, result)
    })
}

fn parse_request(request: &Request) -> Result<Value, ApiError> {
    serde_json::from_slice(&request.body[..]).map_err(|_| ApiError {
        status: 400,
        code: "jmap",
        message: json!({"type": "urn:ietf:params:jmap:error:notJSON", "status": 400}).to_string(),
    })
}

fn h_api(
    request: Request,
    _: PathParams,
    connection: ConnectionInfo,
    instance: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move {
        let result = async {
            let user = signed(&request, &connection, &instance).await?;
            let body = parse_request(&request)?;
            Ok(ok(&run(&instance, &user, &body).await?))
        }
        .await;
        answer(&request, result)
    })
}

/// The same methods for the web client, signed in by its session.
fn h_web(
    request: Request,
    _: PathParams,
    _: ConnectionInfo,
    instance: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move {
        let result = async {
            let me = ortus_core::auth::current(&request, &instance)
                .await
                .ok_or_else(ApiError::sign_in)?;
            let body = parse_request(&request)?;
            Ok(ok(&run(&instance, &me.user, &body).await?))
        }
        .await;
        answer(&request, result)
    })
}

fn h_upload(
    request: Request,
    params: PathParams,
    connection: ConnectionInfo,
    instance: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move {
        let result = async {
            let user = signed(&request, &connection, &instance).await?;
            if params.get("account").unwrap_or_default() != user.id {
                return Err(ApiError::not_found("There is no such account."));
            }
            let data = &request.body[..];
            if data.len() > MAX_UPLOAD {
                return Err(ApiError {
                    status: 413,
                    code: "too_big",
                    message: "That is more than an upload may hold.".into(),
                });
            }
            let blob = format!("u_{}", ortus_core::auth::random_hex(12));
            let path = upload_path(&user.id, &blob).unwrap_or_default();
            instance.files.put(&path, data).await.map_err(|e| {
                eprintln!("ortus: mail: jmap upload: {e}");
                ApiError::internal()
            })?;
            store::track_upload(instance.store.as_ref(), &user.id, &path).await?;
            let kind = request
                .header("content-type")
                .map(|h| h.value.to_string())
                .unwrap_or_else(|| "application/octet-stream".into());
            Ok(ok(
                &json!({"accountId": user.id, "blobId": blob, "type": kind, "size": data.len()}),
            ))
        }
        .await;
        answer(&request, result)
    })
}

async fn download(
    instance: &Arc<Instance>,
    user: &User,
    blob: &str,
    name: &str,
) -> Result<Response, ApiError> {
    let ctx = Ctx { instance, user };
    let data = blob_bytes(&ctx, blob)
        .await
        .ok_or_else(|| ApiError::not_found("There is no such blob."))?;
    let name: String = name
        .chars()
        .filter(|c| c.is_alphanumeric() || "._- ".contains(*c))
        .collect();
    // Always a download, never shown as a page of this site.
    let header = |name: &str, value: String| rust_web_server::header::Header {
        name: name.to_string().into(),
        value: value.into(),
    };
    let headers = vec![
        header(
            "Content-Disposition",
            format!("attachment; filename=\"{name}\""),
        ),
        header("X-Content-Type-Options", "nosniff".into()),
        header(
            "Content-Security-Policy",
            "sandbox; default-src 'none'".into(),
        ),
        header("Cache-Control", "private, no-store".into()),
    ];
    let body = rust_web_server::range::Range::get_content_range(
        data,
        "application/octet-stream".to_string(),
    );
    Ok(Response::get_response(
        STATUS_CODE_REASON_PHRASE.n200_ok,
        Some(headers),
        Some(vec![body]),
    ))
}

fn h_download(
    request: Request,
    params: PathParams,
    connection: ConnectionInfo,
    instance: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move {
        let result = async {
            let user = signed(&request, &connection, &instance).await?;
            if params.get("account").unwrap_or_default() != user.id {
                return Err(ApiError::not_found("There is no such account."));
            }
            let blob = params.get("blob").unwrap_or_default();
            download(
                &instance,
                &user,
                blob,
                params.get("name").unwrap_or("download"),
            )
            .await
        }
        .await;
        answer(&request, result)
    })
}

/// Attachments for the web client, with its session.
fn h_web_download(
    request: Request,
    params: PathParams,
    _: ConnectionInfo,
    instance: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move {
        let result = async {
            let me = ortus_core::auth::current(&request, &instance)
                .await
                .ok_or_else(ApiError::sign_in)?;
            let blob = params.get("blob").unwrap_or_default();
            download(
                &instance,
                &me.user,
                blob,
                params.get("name").unwrap_or("download"),
            )
            .await
        }
        .await;
        answer(&request, result)
    })
}

/// Push (RFC 8620, 7.3): a StateChange event whenever the person's mail
/// changes, and a ping as asked.
fn h_events(
    request: Request,
    _: PathParams,
    connection: ConnectionInfo,
    instance: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move {
        match signed(&request, &connection, &instance).await {
            Ok(user) => events(&request, instance, user),
            Err(e) => answer(&request, Err::<Response, _>(e)),
        }
    })
}

/// Push for the web client, with its session.
fn h_web_events(
    request: Request,
    _: PathParams,
    _: ConnectionInfo,
    instance: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move {
        match ortus_core::auth::current(&request, &instance).await {
            Some(me) => events(&request, instance, me.user),
            None => answer(&request, Err::<Response, _>(ApiError::sign_in())),
        }
    })
}

fn events(request: &Request, instance: Arc<Instance>, user: User) -> Response {
    {
        let query = request
            .request_uri
            .split_once('?')
            .map(|(_, q)| q.to_string())
            .unwrap_or_default();
        let get = |name: &str| {
            query
                .split('&')
                .find_map(|p| p.strip_prefix(&format!("{name}=")))
                .unwrap_or_default()
                .to_string()
        };
        let close_after_state = get("closeafter") == "state";
        let ping = get("ping").parse::<u64>().unwrap_or(0).clamp(0, 300);
        let (events, response) = rust_web_server::sse::SseStream::new()
            .keep_alive(Duration::from_secs(if ping > 0 { ping } else { 15 }))
            .start();
        ortus_core::runtime::io().spawn(async move {
            let mut last = state(&instance, &user.id).await.unwrap_or_default();
            let event = |s: &str| {
                let change = json!({"@type": "StateChange", "changed": {&user.id: {"Email": s, "Mailbox": s, "Thread": s, "EmailDelivery": s}}});
                rust_web_server::sse::SseEvent::data(change.to_string()).event_type("state")
            };
            if events.try_send(event(&last)).is_err() {
                return;
            }
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let Ok(now) = state(&instance, &user.id).await else { continue };
                if now != last {
                    last = now;
                    match events.try_send(event(&last)) {
                        Err(_) => return,
                        Ok(_) if close_after_state => return,
                        Ok(_) => {}
                    }
                } else if events.comment("").is_err() {
                    return;
                }
            }
        });
        response
    }
}

fn any_object() -> Value {
    json!({"type": "object"})
}

pub fn routes() -> Vec<Route> {
    let route = |method, path, id, summary, access, handler| Route {
        method,
        path,
        id,
        tag: "mail",
        summary,
        access,
        response: any_object,
        handler,
    };
    vec![
        route(
            Method::Get,
            "/mail/jmap/session",
            "jmapSession",
            "The JMAP session (RFC 8620): capabilities, the account, and where to call.",
            Access::Header,
            h_session as ortus_core::api::Handler,
        ),
        route(
            Method::Post,
            "/mail/jmap/api",
            "jmapApi",
            "JMAP method calls (Core, Mail, Submission).",
            Access::Header,
            h_api,
        ),
        route(
            Method::Post,
            "/mail/jmap/upload/:account",
            "jmapUpload",
            "Upload a blob (a message to import).",
            Access::Header,
            h_upload,
        ),
        route(
            Method::Get,
            "/mail/jmap/download/:account/:blob/:name",
            "jmapDownload",
            "Download a message or one of its parts.",
            Access::Header,
            h_download,
        ),
        route(
            Method::Get,
            "/mail/jmap/eventsource",
            "jmapEvents",
            "Push: state changes as server-sent events.",
            Access::Header,
            h_events,
        ),
        route(
            Method::Post,
            "/mail/web/jmap",
            "jmapWeb",
            "JMAP method calls from the web client, with its session.",
            Access::Signed,
            h_web,
        ),
        route(
            Method::Get,
            "/mail/web/download/:blob/:name",
            "mailWebDownload",
            "Download a message or one of its parts, from the web client.",
            Access::Signed,
            h_web_download,
        ),
        route(
            Method::Get,
            "/mail/web/events",
            "mailWebEvents",
            "Push for the web client: state changes as server-sent events.",
            Access::Signed,
            h_web_events,
        ),
    ]
}
