//! Sending later: a draft with a time to go, which can be called back
//! until then. Undo send is the same, ten seconds ahead. The policy is
//! asked when it is scheduled (nothing recorded) and again when it goes.

use ortus_core::instance::Instance;
use ortus_core::jobs::Job;
use ortus_core::storage::{AsyncStore, StoreResult};
use serde::Serialize;
use serde_json::json;

use crate::send::{self, Outgoing, Refused};
use crate::store;

/// The job that sends a scheduled message when its time comes.
pub const SEND_LATER_JOB: &str = "mail.send-later";

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Scheduled {
    pub id: String,
    pub email_id: String,
    pub identity: String,
    pub send_at: i64,
    /// pending, sending, sent, failed, canceled
    pub state: String,
    pub error: Option<String>,
}

fn now() -> i64 {
    ortus_core::program::now_ms()
}

/// What went wrong, as a short code the client translates.
pub fn refusal_code(refused: &Refused) -> &'static str {
    match refused {
        Refused::NotYourAddress => "not_your_address",
        Refused::BadRecipient { .. } => "bad_recipient",
        Refused::Policy { .. } => "policy",
        Refused::NeedsReason { .. } => "needs_reason",
        Refused::Failed => "failed",
    }
}

/// Schedules a draft to go at `send_at` (Unix ms) to `recipients`.
pub async fn schedule(
    instance: &Instance,
    out: &Outgoing<'_>,
    email_id: &str,
    send_at: i64,
) -> Result<String, Refused> {
    send::check(instance, out).await?;
    let id = format!("s_{}", ortus_core::auth::random_hex(12));
    let result: StoreResult<()> = async {
        let mut tx = instance.store.begin().await?;
        tx.execute(
            "INSERT INTO mail_scheduled (id, user_id, email_id, identity, recipients, label, reason, send_at, state, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'pending', $9)",
            &[
                id.as_str().into(),
                out.user.into(),
                email_id.into(),
                out.from.into(),
                json!(out.to).to_string().into(),
                out.label.map(str::to_string).into(),
                out.reason.map(str::to_string).into(),
                send_at.into(),
                now().into(),
            ],
        )
        .await?;
        let payload = json!({ "id": id }).to_string();
        ortus_core::jobs::enqueue_in(
            &mut *tx,
            &ortus_core::jobs::NewJob { id: &format!("mail-later-{id}"), kind: SEND_LATER_JOB, payload: &payload, run_at: send_at, max_attempts: 8 },
            now(),
        )
        .await?;
        tx.commit().await
    }
    .await;
    result.map_err(|_| Refused::Failed)?;
    Ok(id)
}

/// Calls a scheduled message back, while it has not gone yet.
pub async fn cancel(store: &dyn AsyncStore, user_id: &str, id: &str) -> StoreResult<bool> {
    Ok(store
        .execute("UPDATE mail_scheduled SET state = 'canceled' WHERE id = $1 AND user_id = $2 AND state = 'pending'", &[id.into(), user_id.into()])
        .await?
        == 1)
}

/// A person's scheduled messages: waiting ones and ones that could not go.
pub async fn list(store: &dyn AsyncStore, user_id: &str) -> StoreResult<Vec<Scheduled>> {
    Ok(store
        .query(
            "SELECT id, email_id, identity, send_at, state, error FROM mail_scheduled WHERE user_id = $1 AND state IN ('pending', 'sending', 'failed') ORDER BY send_at",
            &[user_id.into()],
        )
        .await?
        .iter()
        .map(|r| Scheduled {
            id: r.text(0).unwrap_or_default().into(),
            email_id: r.text(1).unwrap_or_default().into(),
            identity: r.text(2).unwrap_or_default().into(),
            send_at: r.int(3).unwrap_or(0),
            state: r.text(4).unwrap_or_default().into(),
            error: r.text(5).map(str::to_string),
        })
        .collect())
}

/// One of a person's scheduled messages, in any state.
pub async fn get(
    store: &dyn AsyncStore,
    user_id: &str,
    id: &str,
) -> StoreResult<Option<Scheduled>> {
    Ok(store
        .query("SELECT id, email_id, identity, send_at, state, error FROM mail_scheduled WHERE id = $1 AND user_id = $2", &[id.into(), user_id.into()])
        .await?
        .first()
        .map(|r| Scheduled {
            id: r.text(0).unwrap_or_default().into(),
            email_id: r.text(1).unwrap_or_default().into(),
            identity: r.text(2).unwrap_or_default().into(),
            send_at: r.int(3).unwrap_or(0),
            state: r.text(4).unwrap_or_default().into(),
            error: r.text(5).map(str::to_string),
        }))
}

/// Sends a scheduled message (the job). It goes once: the row turns from
/// pending to sending first, so a cancel and the job cannot both win.
pub async fn run(instance: &Instance, job: &Job) -> Result<(), String> {
    let payload: serde_json::Value =
        serde_json::from_str(&job.payload).map_err(|e| e.to_string())?;
    let id = payload["id"].as_str().unwrap_or_default();
    let store = instance.store.as_ref();
    let rows = store
        .query("SELECT user_id, email_id, identity, recipients, label, reason, state FROM mail_scheduled WHERE id = $1", &[id.into()])
        .await
        .map_err(|e| e.to_string())?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    let state = row.text(6).unwrap_or_default();
    if state != "pending" && state != "sending" {
        return Ok(());
    }
    if state == "pending"
        && store
            .execute(
                "UPDATE mail_scheduled SET state = 'sending' WHERE id = $1 AND state = 'pending'",
                &[id.into()],
            )
            .await
            .map_err(|e| e.to_string())?
            != 1
    {
        return Ok(());
    }
    let user = row.text(0).unwrap_or_default().to_string();
    let email_id = row.text(1).unwrap_or_default().to_string();
    let identity = row.text(2).unwrap_or_default().to_string();
    let recipients: Vec<String> =
        serde_json::from_str(row.text(3).unwrap_or("[]")).unwrap_or_default();
    let (label, reason) = (
        row.text(4).map(str::to_string),
        row.text(5).map(str::to_string),
    );
    let finish = |state: &'static str, error: Option<String>| async move {
        store
            .execute(
                "UPDATE mail_scheduled SET state = $1, error = $2 WHERE id = $3",
                &[state.into(), error.into(), id.into()],
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    };
    let Some(raw) = store::raw(instance, &user, &email_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return finish("failed", Some("draft_gone".into())).await;
    };
    let outgoing = send::strip_bcc(&raw);
    let result = send::send(
        instance,
        &Outgoing {
            user: &user,
            from: &identity,
            to: &recipients,
            raw: &outgoing,
            label: label.as_deref(),
            reason: reason.as_deref(),
        },
    )
    .await;
    match result {
        Ok(_) => {
            // The draft goes: a copy is in Sent now.
            for entry in store
                .query("SELECT e.mailbox_id, e.uid FROM mail_entries e JOIN mail_mailboxes b ON b.id = e.mailbox_id WHERE e.message = $1 AND b.user_id = $2", &[email_id.as_str().into(), user.as_str().into()])
                .await
                .map_err(|e| e.to_string())?
            {
                store::expunge(instance, entry.text(0).unwrap_or_default(), entry.int(1).unwrap_or(0)).await.map_err(|e| e.to_string())?;
            }
            finish("sent", None).await
        }
        // Storing or queueing failed: the job tries again.
        Err(Refused::Failed) => Err("could not send yet".into()),
        Err(refused) => finish("failed", Some(refusal_code(&refused).into())).await,
    }
}
