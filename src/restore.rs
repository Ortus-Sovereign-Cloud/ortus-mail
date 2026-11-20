//! Bringing back a mail folder as it was at a restore point: a new folder
//! beside the person's own, with each message as it was (its flags and the
//! time it came); nothing of theirs is changed.

use std::collections::BTreeMap;
use std::sync::Arc;

use ortus_core::instance::Instance;
use ortus_core::points::{int, text, Snapshot};

/// The person's folders at the point: (id, name).
pub async fn choices(point: &Snapshot, user: &str) -> Result<Vec<(String, String)>, String> {
    let mut list: Vec<(String, String)> = point
        .rows("mail_mailboxes", |r| text(r, "user_id") == user)
        .await?
        .iter()
        .map(|r| (text(r, "id").to_string(), text(r, "name").to_string()))
        .collect();
    list.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(list)
}

/// Restores folder `id` of `user` into a new folder `into`; gives its path.
pub async fn folder(
    instance: &Arc<Instance>,
    point: &Snapshot,
    user: &str,
    id: &str,
    into: &str,
) -> Result<String, String> {
    let entries = point
        .rows("mail_entries", |r| text(r, "mailbox_id") == id)
        .await?;
    let flags: BTreeMap<String, String> = entries
        .iter()
        .map(|r| (text(r, "message").to_string(), text(r, "flags").to_string()))
        .collect();
    let messages = point
        .rows("mail_messages", |r| {
            text(r, "user_id") == user && flags.contains_key(text(r, "id"))
        })
        .await?;
    let s = instance.store.as_ref();
    let made = crate::store::create_mailbox(s, user, into)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("a folder of that name is there already")?;
    let mut ordered: Vec<_> = messages.iter().collect();
    ordered.sort_by_key(|r| int(r, "received_at").unwrap_or(0));
    for m in ordered {
        let Some(raw) = point.file(text(m, "blob")).await? else {
            continue;
        };
        let these: Vec<&str> = flags
            .get(text(m, "id"))
            .map(|f| f.split_whitespace().collect())
            .unwrap_or_default();
        let done = crate::store::deliver(instance, user, &made.id, &raw, &these, None, 0)
            .await
            .map_err(|e| e.to_string())?;
        // When it came, as it was.
        if let Some(at) = int(m, "received_at") {
            s.execute(
                "UPDATE mail_messages SET received_at = $1 WHERE id = $2",
                &[at.into(), done.message.as_str().into()],
            )
            .await
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(format!("/mail?box={}", made.id))
}
