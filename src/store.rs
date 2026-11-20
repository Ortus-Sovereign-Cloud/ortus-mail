//! The mail store: domains and addresses, each person's mailboxes, and
//! their messages. A message's raw form lives in the file store (local or
//! S3) under `mail/<user>/<id>.eml`; what lists, threads and protocols need
//! lives in the database. Every mailbox keeps IMAP's numbers: a UID
//! validity, the next UID, and a modification sequence bumped by every
//! change, so IMAP and JMAP clients can sync from where they were.

use std::collections::HashSet;

use mail_parser::MessageParser;
use ortus_core::instance::Instance;
use ortus_core::storage::{AsyncStore, AsyncTx, Migration, StoreError, StoreResult};
use serde::Serialize;
use serde_json::json;

pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "store",
    sql: "CREATE TABLE mail_domains (
        domain TEXT PRIMARY KEY,
        added_at BIGINT NOT NULL,
        state TEXT NOT NULL,
        dkim_selector TEXT,
        dkim_key TEXT,
        dkim_public TEXT,
        checks TEXT,
        checked_at BIGINT
    );
    CREATE TABLE mail_addresses (
        address TEXT PRIMARY KEY,
        user_id TEXT NOT NULL,
        kind TEXT NOT NULL,
        created_at BIGINT NOT NULL
    );
    CREATE INDEX mail_addresses_user ON mail_addresses (user_id);
    CREATE TABLE mail_mailboxes (
        id TEXT PRIMARY KEY,
        user_id TEXT NOT NULL,
        name TEXT NOT NULL,
        role TEXT,
        uid_validity BIGINT NOT NULL,
        uid_next BIGINT NOT NULL,
        modseq BIGINT NOT NULL,
        created_at BIGINT NOT NULL,
        UNIQUE (user_id, name)
    );
    CREATE TABLE mail_messages (
        id TEXT PRIMARY KEY,
        user_id TEXT NOT NULL,
        blob TEXT NOT NULL,
        size BIGINT NOT NULL,
        received_at BIGINT NOT NULL,
        sent_at BIGINT,
        message_id TEXT,
        thread_id TEXT NOT NULL,
        subject TEXT NOT NULL,
        from_addr TEXT,
        from_name TEXT,
        recipients TEXT NOT NULL,
        preview TEXT NOT NULL,
        attachments BIGINT NOT NULL,
        auth TEXT,
        spam BIGINT NOT NULL
    );
    CREATE INDEX mail_messages_user ON mail_messages (user_id, received_at);
    CREATE INDEX mail_messages_mid ON mail_messages (user_id, message_id);
    CREATE TABLE mail_entries (
        mailbox_id TEXT NOT NULL REFERENCES mail_mailboxes (id),
        uid BIGINT NOT NULL,
        message TEXT NOT NULL REFERENCES mail_messages (id),
        flags TEXT NOT NULL,
        modseq BIGINT NOT NULL,
        PRIMARY KEY (mailbox_id, uid)
    );
    CREATE INDEX mail_entries_message ON mail_entries (message)",
}];

/// The mailboxes everyone has, by role, with their IMAP names.
pub const DEFAULT_MAILBOXES: [(&str, &str); 6] = [
    ("inbox", "INBOX"),
    ("drafts", "Drafts"),
    ("sent", "Sent"),
    ("junk", "Junk"),
    ("trash", "Trash"),
    ("archive", "Archive"),
];

fn new_id(prefix: &str) -> String {
    format!("{prefix}{}", ortus_core::auth::random_hex(12))
}

fn now() -> i64 {
    ortus_core::program::now_ms()
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Mailbox {
    pub id: String,
    pub name: String,
    pub role: Option<String>,
    pub uid_validity: i64,
    pub uid_next: i64,
    pub modseq: i64,
}

/// Every person's mailboxes, the default ones made the first time.
pub async fn mailboxes(store: &dyn AsyncStore, user_id: &str) -> StoreResult<Vec<Mailbox>> {
    let read = |rows: Vec<ortus_core::storage::Row>| -> Vec<Mailbox> {
        rows.iter()
            .map(|r| Mailbox {
                id: r.text(0).unwrap_or_default().into(),
                name: r.text(1).unwrap_or_default().into(),
                role: r.text(2).map(str::to_string),
                uid_validity: r.int(3).unwrap_or_default(),
                uid_next: r.int(4).unwrap_or_default(),
                modseq: r.int(5).unwrap_or_default(),
            })
            .collect()
    };
    const SELECT: &str = "SELECT id, name, role, uid_validity, uid_next, modseq FROM mail_mailboxes WHERE user_id = $1 ORDER BY created_at, name";
    let found = read(store.query(SELECT, &[user_id.into()]).await?);
    if !found.is_empty() {
        return Ok(found);
    }
    let mut tx = store.begin().await?;
    // Two first visits at once make them once.
    if tx
        .query(
            "SELECT id FROM mail_mailboxes WHERE user_id = $1",
            &[user_id.into()],
        )
        .await?
        .is_empty()
    {
        let validity = (now() / 1000) & 0x7fff_ffff;
        for (i, (role, name)) in DEFAULT_MAILBOXES.iter().enumerate() {
            tx.execute(
                "INSERT INTO mail_mailboxes (id, user_id, name, role, uid_validity, uid_next, modseq, created_at) VALUES ($1, $2, $3, $4, $5, 1, 1, $6)",
                &[new_id("mb_").into(), user_id.into(), (*name).into(), (*role).into(), validity.into(), (now() + i as i64).into()],
            )
            .await?;
        }
    }
    tx.commit().await?;
    Ok(read(store.query(SELECT, &[user_id.into()]).await?))
}

/// The person's mailbox with this role.
pub async fn by_role(store: &dyn AsyncStore, user_id: &str, role: &str) -> StoreResult<Mailbox> {
    mailboxes(store, user_id)
        .await?
        .into_iter()
        .find(|m| m.role.as_deref() == Some(role))
        .ok_or_else(|| StoreError::Database(format!("no {role} mailbox")))
}

/// What the lists and protocols need about a message, read once from its
/// raw form.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub message_id: Option<String>,
    pub parents: Vec<String>,
    pub subject: String,
    pub from_addr: Option<String>,
    pub from_name: Option<String>,
    pub recipients: Vec<String>,
    pub sent_at: Option<i64>,
    pub preview: String,
    pub attachments: i64,
}

/// Reads a raw message; one that does not parse still gets stored, with
/// an empty summary.
pub fn summarise(raw: &[u8]) -> Summary {
    let Some(message) = MessageParser::default().parse(raw) else {
        return Summary::default();
    };
    let attachments = message.attachments().len() as i64;
    let from = message.from().and_then(|list| list.first());
    let mut recipients = Vec::new();
    for list in [message.to(), message.cc()].into_iter().flatten() {
        recipients.extend(
            list.mailboxes()
                .filter_map(|m| m.address().map(str::to_lowercase)),
        );
    }
    let mut parents: Vec<String> = Vec::new();
    for list in [message.in_reply_to(), message.references()]
        .into_iter()
        .flatten()
    {
        parents.extend(list.iter().map(str::to_string));
    }
    Summary {
        message_id: message.message_id().map(str::to_string),
        parents,
        subject: message
            .subject()
            .unwrap_or_default()
            .chars()
            .take(500)
            .collect(),
        from_addr: from
            .as_ref()
            .and_then(|m| m.address())
            .map(str::to_lowercase),
        from_name: from.as_ref().and_then(|m| m.name()).map(str::to_string),
        recipients,
        sent_at: message.date().map(|d| d.to_timestamp() * 1000),
        preview: message
            .body_preview(200)
            .map(|p| p.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default(),
        attachments,
    }
}

/// Puts a message into a mailbox: the next UID, a new modification
/// sequence, the same thread as the message it answers.
async fn add_entry(
    tx: &mut dyn AsyncTx,
    mailbox_id: &str,
    message: &str,
    flags: &str,
) -> StoreResult<(i64, i64)> {
    let bumped = tx
        .execute(
            "UPDATE mail_mailboxes SET uid_next = uid_next + 1, modseq = modseq + 1 WHERE id = $1",
            &[mailbox_id.into()],
        )
        .await?;
    if bumped != 1 {
        return Err(StoreError::Database("no such mailbox".into()));
    }
    let row = tx
        .query(
            "SELECT uid_next - 1, modseq FROM mail_mailboxes WHERE id = $1",
            &[mailbox_id.into()],
        )
        .await?;
    let (uid, modseq) = row
        .first()
        .map(|r| (r.int(0).unwrap_or_default(), r.int(1).unwrap_or_default()))
        .unwrap_or_default();
    tx.execute(
        "INSERT INTO mail_entries (mailbox_id, uid, message, flags, modseq) VALUES ($1, $2, $3, $4, $5)",
        &[mailbox_id.into(), uid.into(), message.into(), flags.into(), modseq.into()],
    )
    .await?;
    Ok((uid, modseq))
}

/// Flags, in IMAP's spelling, as a set; stored space-separated and sorted.
pub fn normal_flags(flags: &[&str]) -> String {
    let set: std::collections::BTreeSet<String> = flags
        .iter()
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty() && !f.contains(char::is_whitespace))
        .collect();
    set.into_iter().collect::<Vec<_>>().join(" ")
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Delivered {
    pub message: String,
    pub thread: String,
    pub uid: i64,
}

/// Stores `raw` for `user_id` in `mailbox_id`. The file goes first; if the
/// database refuses, the file is removed again.
pub async fn deliver(
    instance: &Instance,
    user_id: &str,
    mailbox_id: &str,
    raw: &[u8],
    flags: &[&str],
    auth: Option<&serde_json::Value>,
    spam: i64,
) -> StoreResult<Delivered> {
    let summary = summarise(raw);
    let id = new_id("m_");
    let blob = format!("mail/{user_id}/{id}.eml");
    instance
        .files
        .put(&blob, raw)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;
    let result = async {
        let store = instance.store.as_ref();
        let mut tx = store.begin().await?;
        // The thread of the first earlier message it answers, else its own.
        let mut thread = None;
        for parent in &summary.parents {
            let rows = tx
                .query("SELECT thread_id FROM mail_messages WHERE user_id = $1 AND message_id = $2", &[user_id.into(), parent.as_str().into()])
                .await?;
            if let Some(found) = rows.first().and_then(|r| r.text(0)) {
                thread = Some(found.to_string());
                break;
            }
        }
        let thread = thread.unwrap_or_else(|| new_id("t_"));
        tx.execute(
            "INSERT INTO mail_messages (id, user_id, blob, size, received_at, sent_at, message_id, thread_id, subject, from_addr, from_name, recipients, preview, attachments, auth, spam)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
            &[
                id.as_str().into(),
                user_id.into(),
                blob.as_str().into(),
                (raw.len() as i64).into(),
                now().into(),
                summary.sent_at.into(),
                summary.message_id.clone().into(),
                thread.as_str().into(),
                summary.subject.as_str().into(),
                summary.from_addr.clone().into(),
                summary.from_name.clone().into(),
                json!(summary.recipients).to_string().into(),
                summary.preview.as_str().into(),
                summary.attachments.into(),
                auth.map(|a| a.to_string()).into(),
                spam.into(),
            ],
        )
        .await?;
        let (uid, _) = add_entry(&mut *tx, mailbox_id, &id, &normal_flags(flags)).await?;
        tx.commit().await?;
        Ok(Delivered { message: id.clone(), thread, uid })
    }
    .await;
    if result.is_err() {
        let _ = instance.files.delete(&blob).await;
    }
    result
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Entry {
    pub uid: i64,
    pub message: String,
    pub flags: Vec<String>,
    pub modseq: i64,
    pub thread: String,
    pub subject: String,
    pub from_addr: Option<String>,
    pub from_name: Option<String>,
    pub preview: String,
    pub received_at: i64,
    pub size: i64,
    pub attachments: i64,
}

const ENTRY: &str = "SELECT e.uid, e.message, e.flags, e.modseq, m.thread_id, m.subject, m.from_addr, m.from_name, m.preview, m.received_at, m.size, m.attachments
    FROM mail_entries e JOIN mail_messages m ON m.id = e.message";

fn entry(r: &ortus_core::storage::Row) -> Entry {
    Entry {
        uid: r.int(0).unwrap_or_default(),
        message: r.text(1).unwrap_or_default().into(),
        flags: r
            .text(2)
            .unwrap_or_default()
            .split(' ')
            .filter(|f| !f.is_empty())
            .map(str::to_string)
            .collect(),
        modseq: r.int(3).unwrap_or_default(),
        thread: r.text(4).unwrap_or_default().into(),
        subject: r.text(5).unwrap_or_default().into(),
        from_addr: r.text(6).map(str::to_string),
        from_name: r.text(7).map(str::to_string),
        preview: r.text(8).unwrap_or_default().into(),
        received_at: r.int(9).unwrap_or_default(),
        size: r.int(10).unwrap_or_default(),
        attachments: r.int(11).unwrap_or_default(),
    }
}

/// A mailbox's messages by UID, from `after_uid` on.
pub async fn entries(
    store: &dyn AsyncStore,
    mailbox_id: &str,
    after_uid: i64,
    limit: i64,
) -> StoreResult<Vec<Entry>> {
    Ok(store
        .query(
            &format!("{ENTRY} WHERE e.mailbox_id = $1 AND e.uid > $2 ORDER BY e.uid LIMIT $3"),
            &[mailbox_id.into(), after_uid.into(), limit.into()],
        )
        .await?
        .iter()
        .map(entry)
        .collect())
}

/// What changed in a mailbox since `modseq` (for IMAP CONDSTORE and JMAP).
pub async fn changed_since(
    store: &dyn AsyncStore,
    mailbox_id: &str,
    modseq: i64,
) -> StoreResult<Vec<Entry>> {
    Ok(store
        .query(
            &format!("{ENTRY} WHERE e.mailbox_id = $1 AND e.modseq > $2 ORDER BY e.uid"),
            &[mailbox_id.into(), modseq.into()],
        )
        .await?
        .iter()
        .map(entry)
        .collect())
}

/// Sets a message's flags in a mailbox, bumping the mailbox's sequence.
pub async fn set_flags(
    store: &dyn AsyncStore,
    mailbox_id: &str,
    uid: i64,
    flags: &[&str],
) -> StoreResult<bool> {
    let mut tx = store.begin().await?;
    tx.execute(
        "UPDATE mail_mailboxes SET modseq = modseq + 1 WHERE id = $1",
        &[mailbox_id.into()],
    )
    .await?;
    let changed = tx
        .execute(
            "UPDATE mail_entries SET flags = $1, modseq = (SELECT modseq FROM mail_mailboxes WHERE id = $2) WHERE mailbox_id = $2 AND uid = $3",
            &[normal_flags(flags).into(), mailbox_id.into(), uid.into()],
        )
        .await?;
    if changed == 1 {
        tx.commit().await?;
    } else {
        tx.rollback().await?;
    }
    Ok(changed == 1)
}

/// Copies a message into another of the same person's mailboxes; answers
/// its UID there.
pub async fn copy(
    store: &dyn AsyncStore,
    from: &str,
    uid: i64,
    to: &str,
) -> StoreResult<Option<i64>> {
    let mut tx = store.begin().await?;
    let rows = tx
        .query(
            "SELECT e.message, e.flags FROM mail_entries e JOIN mail_mailboxes a ON a.id = e.mailbox_id JOIN mail_mailboxes b ON b.id = $3 AND b.user_id = a.user_id WHERE e.mailbox_id = $1 AND e.uid = $2",
            &[from.into(), uid.into(), to.into()],
        )
        .await?;
    let Some(row) = rows.first() else {
        tx.rollback().await?;
        return Ok(None);
    };
    let (message, flags) = (
        row.text(0).unwrap_or_default().to_string(),
        row.text(1).unwrap_or_default().to_string(),
    );
    let (new_uid, _) = add_entry(&mut *tx, to, &message, &flags).await?;
    tx.commit().await?;
    Ok(Some(new_uid))
}

/// Removes a message from a mailbox inside `tx`; a message in no mailbox
/// any more is deleted, and its file is answered for the caller to remove
/// (after the commit, or by job). `None` when it was not there.
pub async fn remove_in(
    tx: &mut dyn AsyncTx,
    mailbox_id: &str,
    uid: i64,
) -> StoreResult<Option<Option<String>>> {
    let rows = tx
        .query(
            "SELECT message FROM mail_entries WHERE mailbox_id = $1 AND uid = $2",
            &[mailbox_id.into(), uid.into()],
        )
        .await?;
    let Some(message) = rows.first().and_then(|r| r.text(0)).map(str::to_string) else {
        return Ok(None);
    };
    tx.execute(
        "DELETE FROM mail_entries WHERE mailbox_id = $1 AND uid = $2",
        &[mailbox_id.into(), uid.into()],
    )
    .await?;
    tx.execute(
        "UPDATE mail_mailboxes SET modseq = modseq + 1 WHERE id = $1",
        &[mailbox_id.into()],
    )
    .await?;
    let orphan = tx
        .query(
            "SELECT COUNT(*) FROM mail_entries WHERE message = $1",
            &[message.as_str().into()],
        )
        .await?
        .first()
        .and_then(|r| r.int(0))
        == Some(0);
    if !orphan {
        return Ok(Some(None));
    }
    let blob = tx
        .query(
            "SELECT blob FROM mail_messages WHERE id = $1",
            &[message.as_str().into()],
        )
        .await?
        .first()
        .and_then(|r| r.text(0))
        .map(str::to_string);
    tx.execute(
        "DELETE FROM mail_messages WHERE id = $1",
        &[message.as_str().into()],
    )
    .await?;
    Ok(Some(blob))
}

/// Removes a message from a mailbox, and its file once no mailbox holds
/// it. Answers whether it was there.
pub async fn expunge(instance: &Instance, mailbox_id: &str, uid: i64) -> StoreResult<bool> {
    let mut tx = instance.store.begin().await?;
    let Some(blob) = remove_in(&mut *tx, mailbox_id, uid).await? else {
        tx.rollback().await?;
        return Ok(false);
    };
    tx.commit().await?;
    if let Some(blob) = blob {
        // A file left behind is only space; the database no longer points at it.
        let _ = instance.files.delete(&blob).await;
    }
    Ok(true)
}

/// The job that removes files once their messages are gone.
pub const DELETE_FILES_JOB: &str = "mail.delete-files";

/// Queues the removal of `blobs` in `tx` (they go once it commits).
pub async fn delete_files_later(tx: &mut dyn AsyncTx, blobs: &[String]) -> StoreResult<()> {
    if blobs.is_empty() {
        return Ok(());
    }
    let payload = json!(blobs).to_string();
    let id = format!("mail-delete-{}", ortus_core::auth::random_hex(12));
    ortus_core::jobs::enqueue_in(
        tx,
        &ortus_core::jobs::NewJob {
            id: &id,
            kind: DELETE_FILES_JOB,
            payload: &payload,
            run_at: now(),
            max_attempts: 10,
        },
        now(),
    )
    .await?;
    Ok(())
}

/// A message's raw form, if it is the person's.
pub async fn raw(
    instance: &Instance,
    user_id: &str,
    message: &str,
) -> StoreResult<Option<Vec<u8>>> {
    let rows = instance
        .store
        .query(
            "SELECT blob FROM mail_messages WHERE id = $1 AND user_id = $2",
            &[message.into(), user_id.into()],
        )
        .await?;
    let Some(blob) = rows.first().and_then(|r| r.text(0)).map(str::to_string) else {
        return Ok(None);
    };
    instance
        .files
        .get(&blob)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))
}

/// How many bytes of mail the person keeps.
pub async fn usage(store: &dyn AsyncStore, user_id: &str) -> StoreResult<i64> {
    Ok(store
        .query(
            "SELECT CAST(COALESCE(SUM(size), 0) AS BIGINT) FROM mail_messages WHERE user_id = $1",
            &[user_id.into()],
        )
        .await?
        .first()
        .and_then(|r| r.int(0))
        .unwrap_or(0))
}

/// Removes everything a deleted account kept, in the deletion's
/// transaction; the files go by job. (Mail has no channels to hold.)
pub async fn forget(tx: &mut dyn AsyncTx, user_id: &str) -> StoreResult<Vec<String>> {
    let blobs: Vec<String> = tx
        .query(
            "SELECT blob FROM mail_messages WHERE user_id = $1",
            &[user_id.into()],
        )
        .await?
        .iter()
        .filter_map(|r| r.text(0).map(str::to_string))
        .collect();
    for sql in [
        "DELETE FROM mail_entries WHERE mailbox_id IN (SELECT id FROM mail_mailboxes WHERE user_id = $1)",
        "DELETE FROM mail_messages WHERE user_id = $1",
        "DELETE FROM mail_mailboxes WHERE user_id = $1",
        "DELETE FROM mail_addresses WHERE user_id = $1",
    ] {
        tx.execute(sql, &[user_id.into()]).await?;
    }
    delete_files_later(tx, &blobs).await?;
    Ok(blobs)
}

/// Removes what is past retention ("mail", and "deleted" for the trash)
/// and on no hold through its owner, inside the sweep's `tx`; files go by
/// job. Answers how many messages went.
pub async fn purge(tx: &mut dyn AsyncTx, rules: &ortus_core::retention::Rules) -> StoreResult<u64> {
    let rows = tx
        .query(
            "SELECT b.user_id, e.mailbox_id, e.uid, m.received_at, b.role FROM mail_entries e JOIN mail_messages m ON m.id = e.message JOIN mail_mailboxes b ON b.id = e.mailbox_id",
            &[],
        )
        .await?;
    let mut doomed = Vec::new();
    let mut seen = HashSet::new();
    for r in rows {
        let (user, mailbox, uid, at) = (
            r.text(0).unwrap_or_default(),
            r.text(1).unwrap_or_default(),
            r.int(2).unwrap_or_default(),
            r.int(3).unwrap_or_default(),
        );
        let kind = if r.text(4) == Some("trash") {
            "deleted"
        } else {
            "mail"
        };
        if rules.may_purge(kind, at, &[user], None) && seen.insert((mailbox.to_string(), uid)) {
            doomed.push((mailbox.to_string(), uid));
        }
    }
    let mut blobs = Vec::new();
    for (mailbox, uid) in &doomed {
        if let Some(Some(blob)) = remove_in(tx, mailbox, *uid).await? {
            blobs.push(blob);
        }
    }
    delete_files_later(tx, &blobs).await?;
    Ok(doomed.len() as u64)
}

/// The person an address delivers to here: an address or alias of theirs,
/// or their account address in one of our domains. Unverified sign-ups
/// receive nothing.
pub async fn recipient(store: &dyn AsyncStore, address: &str) -> StoreResult<Option<String>> {
    let address = address.trim().to_ascii_lowercase();
    if let Some(user) = store
        .query(
            "SELECT user_id FROM mail_addresses WHERE address = $1",
            &[address.as_str().into()],
        )
        .await?
        .first()
        .and_then(|r| r.text(0))
    {
        return Ok(Some(user.to_string()));
    }
    let Some((_, domain)) = address.rsplit_once('@') else {
        return Ok(None);
    };
    let ours = !store
        .query(
            "SELECT domain FROM mail_domains WHERE domain = $1",
            &[domain.into()],
        )
        .await?
        .is_empty();
    if !ours {
        return Ok(None);
    }
    Ok(store
        .query(
            "SELECT id FROM core_users WHERE email = $1 AND state <> 'unverified'",
            &[address.as_str().into()],
        )
        .await?
        .first()
        .and_then(|r| r.text(0))
        .map(str::to_string))
}

/// Whether a domain is one of ours.
pub async fn is_our_domain(store: &dyn AsyncStore, domain: &str) -> StoreResult<bool> {
    Ok(!store
        .query(
            "SELECT domain FROM mail_domains WHERE domain = $1",
            &[domain.to_ascii_lowercase().into()],
        )
        .await?
        .is_empty())
}
