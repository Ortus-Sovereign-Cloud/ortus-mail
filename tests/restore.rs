//! Bringing a mail folder back as it was, on both shapes: the folder's
//! messages (with their flags and the time they came) go into a new folder
//! beside the person's own; another person's folders are not offered.

mod common;

use ortus_core::auth::accounts::Role;
use ortus_core::files::Files;
use ortus_core::instance::testing_auth::signed_in;
use ortus_core::points::{self, Snapshot};
use ortus_core::runtime::block_on;
use ortus_mail::store;

fn mail(subject: &str) -> Vec<u8> {
    format!("From: Anna <anna@regional.example>\r\nTo: olena@ministry.example\r\nSubject: {subject}\r\nMessage-ID: <{}@x>\r\n\r\nClause 4.\r\n", subject.replace(' ', "")).into_bytes()
}

#[test]
fn a_mail_folder_comes_back_into_a_new_folder() {
    for (name, instance, _keep) in common::installations() {
        let s = instance.store.as_ref();
        signed_in(&instance, "olena@ministry.example", Role::Member);
        signed_in(&instance, "marek@ministry.example", Role::Member);
        let id = |e: &str| {
            block_on(ortus_core::auth::accounts::by_email(s, e))
                .unwrap()
                .unwrap()
                .id
        };
        let (olena, marek) = (id("olena@ministry.example"), id("marek@ministry.example"));
        block_on(store::mailboxes(s, &marek)).unwrap();
        let folder = block_on(store::create_mailbox(s, &olena, "Agreements"))
            .unwrap()
            .unwrap();
        for (subject, flags) in [
            ("Shelter agreement", vec!["\\Seen"]),
            ("Bus contract", vec![]),
        ] {
            block_on(store::deliver(
                &instance,
                &olena,
                &folder.id,
                &mail(subject),
                &flags,
                None,
                0,
            ))
            .unwrap();
        }
        let dir = common::dir();
        let dest = Files::local(&dir.0.join("backups")).unwrap();
        let at = ortus_core::program::now_ms();
        block_on(points::take(&instance, &dest, at, true)).unwrap();
        // Since then she deleted the folder's messages.
        block_on(s.execute(
            "DELETE FROM mail_entries WHERE mailbox_id = $1",
            &[folder.id.as_str().into()],
        ))
        .unwrap();

        let point = block_on(Snapshot::open(dest, &points::name_at(at))).unwrap();
        let mail_app = ortus_mail::apps().remove(0);
        let hers = block_on(mail_app.restore_choices(&point, &olena, "folder")).unwrap();
        assert!(
            hers.iter()
                .any(|(i, n)| *i == folder.id && n == "Agreements"),
            "{name}: {hers:?}"
        );
        let his = block_on(mail_app.restore_choices(&point, &marek, "folder")).unwrap();
        assert!(!his.iter().any(|(i, _)| *i == folder.id), "{name}");

        let path = block_on(mail_app.restore(
            &instance,
            &point,
            &olena,
            "folder",
            &folder.id,
            "Agreements (2026-11-17 18:00)",
        ))
        .unwrap();
        let made = block_on(store::mailboxes(s, &olena))
            .unwrap()
            .into_iter()
            .find(|m| m.name == "Agreements (2026-11-17 18:00)")
            .expect("the new folder");
        assert_eq!(format!("/mail?box={}", made.id), path, "{name}");
        let rows = block_on(s.query(
            "SELECT m.subject, e.flags FROM mail_entries e JOIN mail_messages m ON m.id = e.message WHERE e.mailbox_id = $1 ORDER BY m.subject",
            &[made.id.as_str().into()],
        ))
        .unwrap();
        let got: Vec<(String, String)> = rows
            .iter()
            .map(|r| {
                (
                    r.text(0).unwrap().to_string(),
                    r.text(1).unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            vec![
                ("Bus contract".to_string(), String::new()),
                ("Shelter agreement".to_string(), "\\Seen".to_string())
            ],
            got,
            "{name}"
        );
        // Restoring again under the same name is refused, not merged.
        assert!(block_on(mail_app.restore(
            &instance,
            &point,
            &olena,
            "folder",
            &folder.id,
            "Agreements (2026-11-17 18:00)"
        ))
        .is_err());
    }
}
