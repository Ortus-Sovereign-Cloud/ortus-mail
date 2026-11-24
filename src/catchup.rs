//! The catch-up card (Main board): what came since the person was last
//! here (their previous visit, after an hour or more away; else the last
//! day): mail from people that waits for a reply, invitations not yet
//! answered, and alerts from the notification centre.

use std::sync::Arc;

use ortus_core::api::{answer, ok, ApiError, Method, Route};
use ortus_core::auth::Access;
use ortus_core::instance::Instance;
use ortus_core::storage::BoxFuture;
use rust_web_server::prelude::*;
use serde_json::{json, Value};

/// A visit after this long away starts a new "since".
const AWAY_MS: i64 = 3_600_000;

/// When the person was last here before now (and this visit noted).
async fn since(instance: &Instance, user: &str, now: i64) -> Result<i64, ApiError> {
    let s = instance.store.as_ref();
    let row = s
        .query(
            "SELECT last_at, previous_at FROM mail_visits WHERE user_id = $1",
            &[user.into()],
        )
        .await?;
    let (previous, last) = match row.first() {
        None => (now - 86_400_000, now),
        Some(r) => {
            let (last, previous) = (r.int(0).unwrap_or(now), r.int(1).unwrap_or(now));
            if now - last > AWAY_MS {
                (last, now)
            } else {
                (previous, now)
            }
        }
    };
    let mut tx = s.begin().await?;
    tx.execute("DELETE FROM mail_visits WHERE user_id = $1", &[user.into()])
        .await?;
    tx.execute(
        "INSERT INTO mail_visits (user_id, last_at, previous_at) VALUES ($1, $2, $3)",
        &[user.into(), last.into(), previous.into()],
    )
    .await?;
    tx.commit().await?;
    Ok(previous)
}

/// Mail sent by machines, not people (nothing to reply to).
fn automated(address: &str) -> bool {
    let local = address
        .split('@')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    [
        "noreply",
        "no-reply",
        "donotreply",
        "do-not-reply",
        "mailer-daemon",
        "postmaster",
        "notifications",
        "notification",
        "alerts",
        "alert",
        "bounce",
        "newsletter",
    ]
    .iter()
    .any(|m| {
        local == *m || local.starts_with(&format!("{m}+")) || local.starts_with(&format!("{m}-"))
    })
}

/// The files named, sixteen at a time; `None` where none is wanted or
/// it could not be read.
async fn read_all(instance: &Arc<Instance>, blobs: &[Option<String>]) -> Vec<Option<Vec<u8>>> {
    const AT_ONCE: usize = 16;
    let mut out: Vec<Option<Vec<u8>>> = vec![None; blobs.len()];
    let mut reading = tokio::task::JoinSet::new();
    for (index, blob) in blobs.iter().enumerate() {
        let Some(blob) = blob.clone().filter(|b| !b.is_empty()) else {
            continue;
        };
        if reading.len() >= AT_ONCE {
            if let Some(Ok((i, raw))) = reading.join_next().await {
                out[i] = raw;
            }
        }
        let files = Arc::clone(&instance.files);
        reading.spawn(async move { (index, files.get(&blob).await.ok().flatten()) });
    }
    while let Some(done) = reading.join_next().await {
        if let Ok((i, raw)) = done {
            out[i] = raw;
        }
    }
    out
}

async fn catch_up(request: &Request, instance: &Arc<Instance>) -> Result<Response, ApiError> {
    let me = ortus_core::auth::current(request, instance)
        .await
        .ok_or_else(ApiError::sign_in)?
        .user;
    let now = ortus_core::program::now_ms();
    let since = since(instance, &me.id, now).await?;
    let s = instance.store.as_ref();
    // The person's addresses: their mail addresses and their account's.
    let mut mine: Vec<String> = s
        .query(
            "SELECT address FROM mail_addresses WHERE user_id = $1",
            &[me.id.as_str().into()],
        )
        .await?
        .iter()
        .filter_map(|r| r.text(0).map(str::to_lowercase))
        .collect();
    mine.push(me.email.to_lowercase());
    let rows = s
        .query(
            "SELECT m.id, m.from_addr, m.from_name, m.subject, m.recipients, e.flags, m.received_at, m.blob FROM mail_entries e JOIN mail_messages m ON m.id = e.message JOIN mail_mailboxes b ON b.id = e.mailbox_id WHERE b.user_id = $1 AND b.role = 'inbox' AND m.received_at > $2 AND m.spam = 0 ORDER BY m.received_at DESC LIMIT 60",
            &[me.id.as_str().into(), since.into()],
        )
        .await?;
    // Each message from someone else is looked at for an invitation: their
    // files read several at a time, not one after another.
    let wanted: Vec<Option<String>> = rows
        .iter()
        .map(|r| {
            let (from, flags) = (
                r.text(1).unwrap_or_default().to_lowercase(),
                r.text(5).unwrap_or_default(),
            );
            let skip =
                mine.contains(&from) || flags.contains("\\Answered") || flags.contains("\\Deleted");
            (!skip).then(|| r.text(7).unwrap_or_default().to_string())
        })
        .collect();
    let mut raws = read_all(instance, &wanted).await;
    let mut reply = Vec::new();
    let mut invites = Vec::new();
    for (index, r) in rows.iter().enumerate() {
        let (id, from, name, subject, recipients, flags) = (
            r.text(0).unwrap_or_default(),
            r.text(1).unwrap_or_default().to_lowercase(),
            r.text(2).unwrap_or_default(),
            r.text(3).unwrap_or_default(),
            r.text(4).unwrap_or("[]"),
            r.text(5).unwrap_or_default(),
        );
        if mine.contains(&from) || flags.contains("\\Answered") || flags.contains("\\Deleted") {
            continue;
        }
        // An invitation still waiting for an answer.
        if let Some(raw) = raws[index].take() {
            if let Some(inv) = crate::invites::shown(instance, &me.id, &raw).await {
                let waiting = inv["method"] != "REPLY"
                    && inv["method"] != "CANCEL"
                    && inv["cancelled"] != true
                    && inv["inCalendar"] == true
                    && inv["answer"] == "NEEDS-ACTION";
                if waiting {
                    invites.push(json!({"id": id, "summary": inv["summary"], "start": inv["start"], "from": if name.is_empty() { from.clone() } else { name.to_string() }}));
                }
                continue;
            }
        }
        let to_me = serde_json::from_str::<Vec<String>>(recipients)
            .unwrap_or_default()
            .iter()
            .any(|a| mine.contains(&a.to_lowercase()));
        if to_me && !automated(&from) {
            reply.push(json!({"id": id, "from": if name.is_empty() { from.clone() } else { name.to_string() }, "subject": subject}));
        }
    }
    let alerts: Vec<Value> =
        ortus_core::notify::recent(s, &me.id, ortus_core::notify::Kind::Alert, since, 5)
            .await?
            .into_iter()
            .map(|n| json!({"title": n.title, "path": n.path, "at": n.at}))
            .collect();
    Ok(ok(
        &json!({"since": since, "reply": reply, "invites": invites, "alerts": alerts}),
    ))
}

fn h_catch_up(
    request: Request,
    _: PathParams,
    _: ConnectionInfo,
    instance: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move { answer(&request, catch_up(&request, &instance).await) })
}

pub fn routes() -> Vec<Route> {
    vec![Route {
        method: Method::Get,
        path: "/mail/catch-up",
        id: "mailCatchUp",
        tag: "mail",
        summary: "What came since one's previous visit: mail waiting for a reply, invitations not answered, alerts.",
        access: Access::Signed,
        response: || json!({"type": "object"}),
        handler: h_catch_up,
    }]
}
