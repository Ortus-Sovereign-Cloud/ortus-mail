//! The mail store on every installation shape: default mailboxes, delivery
//! with threads, flags and modification sequences, copy, expunge (files
//! too), retention with holds, and forgetting an account.

mod common;

use std::sync::Arc;

use ortus_core::instance::Instance;
use ortus_core::program::{now_ms, AppWorker};
use ortus_core::runtime::block_on;
use ortus_mail::store;

fn message(id: &str, subject: &str, reply_to: Option<&str>) -> Vec<u8> {
    let parent = reply_to
        .map(|p| format!("In-Reply-To: {p}\r\nReferences: {p}\r\n"))
        .unwrap_or_default();
    format!(
        "From: \"Maria Peters\" <Maria.Peters@mail.example>\r\nTo: olena@ministry.example, \"Sam\" <sam@ministry.example>\r\nSubject: {subject}\r\nMessage-ID: {id}\r\n{parent}Date: Thu, 19 Nov 2026 08:05:00 +0000\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nMy car was damaged by a hole in the road on Mill Lane\r\nafter the water went down.\r\n"
    )
    .into_bytes()
}

fn run_jobs(instance: &Arc<Instance>) {
    let worker = AppWorker::new(Arc::clone(instance), "test");
    while block_on(worker.tick(now_ms())).unwrap() {}
}

fn store_works(name: &str, instance: Arc<Instance>) {
    let s = instance.store.as_ref();
    let olena = "u_olena";

    // The default mailboxes, made once.
    let boxes = block_on(store::mailboxes(s, olena)).unwrap();
    let names: Vec<&str> = boxes.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        vec!["INBOX", "Drafts", "Sent", "Junk", "Trash", "Archive"],
        names,
        "{name}"
    );
    assert_eq!(6, block_on(store::mailboxes(s, olena)).unwrap().len());
    let inbox = block_on(store::by_role(s, olena, "inbox")).unwrap();
    let archive = block_on(store::by_role(s, olena, "archive")).unwrap();

    // Delivery: the summary, the file, the first UID; a reply joins the thread.
    let first = block_on(store::deliver(
        &instance,
        olena,
        &inbox.id,
        &message("<a1@mail.example>", "Flood damage report", None),
        &[],
        None,
        0,
    ))
    .unwrap();
    assert_eq!(1, first.uid);
    let reply = block_on(store::deliver(
        &instance,
        olena,
        &inbox.id,
        &message(
            "<a2@mail.example>",
            "Re: Flood damage report",
            Some("<a1@mail.example>"),
        ),
        &["\\Seen"],
        None,
        0,
    ))
    .unwrap();
    assert_eq!(
        (2, first.thread.clone()),
        (reply.uid, reply.thread.clone()),
        "{name}: the reply is in the same thread"
    );
    let other = block_on(store::deliver(
        &instance,
        olena,
        &inbox.id,
        &message("<b1@mail.example>", "Sandbags", None),
        &[],
        None,
        0,
    ))
    .unwrap();
    assert_ne!(first.thread, other.thread);

    let listed = block_on(store::entries(s, &inbox.id, 0, 10)).unwrap();
    assert_eq!(3, listed.len());
    assert_eq!("Flood damage report", listed[0].subject);
    assert_eq!(
        Some("maria.peters@mail.example"),
        listed[0].from_addr.as_deref()
    );
    assert_eq!(Some("Maria Peters"), listed[0].from_name.as_deref());
    assert!(
        listed[0]
            .preview
            .starts_with("My car was damaged by a hole in the road on Mill Lane after"),
        "{}",
        listed[0].preview
    );
    assert_eq!(vec!["\\Seen"], listed[1].flags);
    let raw = block_on(store::raw(&instance, olena, &first.message))
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&raw).contains("Mill Lane"));
    assert_eq!(
        None,
        block_on(store::raw(&instance, "u_someone_else", &first.message)).unwrap(),
        "only the owner reads it"
    );

    // Flags bump the sequence; a client syncing from before sees the change.
    let before = block_on(store::by_role(s, olena, "inbox")).unwrap().modseq;
    assert!(block_on(store::set_flags(
        s,
        &inbox.id,
        1,
        &["\\Flagged", "\\Seen", "\\Seen"]
    ))
    .unwrap());
    assert!(!block_on(store::set_flags(s, &inbox.id, 99, &["\\Seen"])).unwrap());
    let changed = block_on(store::changed_since(s, &inbox.id, before)).unwrap();
    assert_eq!(vec![1], changed.iter().map(|e| e.uid).collect::<Vec<_>>());
    assert_eq!(vec!["\\Flagged", "\\Seen"], changed[0].flags);

    // Copy into the archive; another person's mailbox is refused.
    assert_eq!(
        Some(1),
        block_on(store::copy(s, &inbox.id, 1, &archive.id)).unwrap()
    );
    let sams = block_on(store::by_role(s, "u_sam", "inbox")).unwrap();
    assert_eq!(
        None,
        block_on(store::copy(s, &inbox.id, 1, &sams.id)).unwrap()
    );

    // Expunged from the inbox it stays (archived); from both, its file goes.
    let size = block_on(store::usage(s, olena)).unwrap();
    assert!(size > 0);
    assert!(block_on(store::expunge(&instance, &inbox.id, 1)).unwrap());
    assert!(block_on(store::raw(&instance, olena, &first.message))
        .unwrap()
        .is_some());
    assert!(block_on(store::expunge(&instance, &archive.id, 1)).unwrap());
    assert_eq!(
        None,
        block_on(store::raw(&instance, olena, &first.message)).unwrap()
    );
    assert!(
        !block_on(
            instance
                .files
                .exists(&format!("mail/{olena}/{}.eml", first.message))
        )
        .unwrap(),
        "{name}: the file is gone"
    );
    assert!(block_on(store::usage(s, olena)).unwrap() < size);

    // Retention: mail older than the period goes, unless its owner is held.
    block_on(s.execute(
        "UPDATE core_retention SET days = 30 WHERE kind = 'mail'",
        &[],
    ))
    .unwrap();
    block_on(s.execute(
        "UPDATE mail_messages SET received_at = $1",
        &[(now_ms() - 40 * 24 * 3_600_000).into()],
    ))
    .unwrap();
    block_on(s.execute("INSERT INTO core_holds (id, case_ref, note, created_by, created_at) VALUES ('h1', '2026-114', '', 'u_owner', 1)", &[])).unwrap();
    block_on(s.execute(
        "INSERT INTO core_hold_targets (hold_id, scope, target) VALUES ('h1', 'person', $1)",
        &[olena.into()],
    ))
    .unwrap();
    run_jobs(&instance);
    assert_eq!(
        2,
        block_on(store::entries(s, &inbox.id, 0, 10)).unwrap().len(),
        "{name}: held, kept"
    );
    block_on(s.execute("UPDATE core_holds SET released_at = 1", &[])).unwrap();
    block_on(s.execute("DELETE FROM ortus_jobs", &[])).unwrap();
    run_jobs(&instance);
    assert_eq!(
        0,
        block_on(store::entries(s, &inbox.id, 0, 10)).unwrap().len(),
        "{name}: released, purged"
    );
    assert!(
        !block_on(
            instance
                .files
                .exists(&format!("mail/{olena}/{}.eml", reply.message))
        )
        .unwrap(),
        "{name}: files go by job"
    );

    // Forgetting an account removes its mail, files included.
    let sam_mail = block_on(store::deliver(
        &instance,
        "u_sam",
        &sams.id,
        &message("<c1@mail.example>", "Hello", None),
        &[],
        None,
        0,
    ))
    .unwrap();
    let mut tx = block_on(s.begin()).unwrap();
    block_on(store::forget(&mut *tx, "u_sam")).unwrap();
    block_on(tx.commit()).unwrap();
    run_jobs(&instance);
    assert!(!block_on(
        instance
            .files
            .exists(&format!("mail/u_sam/{}.eml", sam_mail.message))
    )
    .unwrap());
    assert_eq!(
        0,
        block_on(s.query(
            "SELECT COUNT(*) FROM mail_mailboxes WHERE user_id = 'u_sam'",
            &[]
        ))
        .unwrap()[0]
            .int(0)
            .unwrap()
    );
}

#[test]
fn the_store_works_on_every_shape() {
    for (name, instance, _dir) in common::installations() {
        store_works(name, instance);
    }
}

#[test]
fn a_message_that_does_not_parse_still_has_a_summary() {
    let summary = store::summarise(b"\xff\xfe not mail at all");
    assert_eq!("", summary.subject);
    assert_eq!(store::summarise(b""), store::Summary::default());
}
