//! Mail in the search index, on both shapes: found by subject, sender and
//! text, with from: and has:attachment; only one's own; gone when the
//! message is.

mod common;

use ortus_core::auth::accounts::Role;
use ortus_core::instance::testing_auth::signed_in;
use ortus_core::runtime::block_on;
use ortus_core::search::{self, Query};
use ortus_mail::store;

#[test]
fn mail_is_found() {
    for (name, instance, _keep) in common::installations() {
        signed_in(&instance, "olena@ministry.example", Role::Member);
        signed_in(&instance, "marek@ministry.example", Role::Member);
        let id = |e: &str| {
            block_on(ortus_core::auth::accounts::by_email(
                instance.store.as_ref(),
                e,
            ))
            .unwrap()
            .unwrap()
            .id
        };
        let (olena, marek) = (id("olena@ministry.example"), id("marek@ministry.example"));
        let inbox = block_on(store::by_role(instance.store.as_ref(), &olena, "inbox")).unwrap();
        let gauge = b"From: Anna Kowalska <anna@regional.example>\r\nTo: olena@ministry.example\r\nSubject: Gauge report and updated shelter agreement\r\nMessage-ID: <g1@regional.example>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nLegal should look at clause 4.\r\n--b\r\nContent-Type: application/pdf; name=\"gauge.pdf\"\r\nContent-Disposition: attachment; filename=\"gauge.pdf\"\r\n\r\nJVBER\r\n--b--\r\n";
        let escort = b"From: Jonas Weber <jonas@police.example>\r\nTo: olena@ministry.example\r\nSubject: Police escort schedule\r\nMessage-ID: <e1@police.example>\r\n\r\nThe escort leaves at 07:00 past the gauge.\r\n";
        let g = block_on(store::deliver(
            &instance,
            &olena,
            &inbox.id,
            gauge,
            &[],
            None,
            0,
        ))
        .unwrap();
        block_on(store::deliver(
            &instance,
            &olena,
            &inbox.id,
            escort,
            &[],
            None,
            0,
        ))
        .unwrap();
        let find = |user: &str, q: &str| -> Vec<String> {
            let mut t: Vec<String> =
                block_on(search::search(&instance, user, &Query::parse(q), 20))
                    .unwrap()
                    .into_iter()
                    .map(|h| h.title)
                    .collect();
            t.sort();
            t
        };
        assert_eq!(
            vec![
                "Gauge report and updated shelter agreement",
                "Police escort schedule"
            ],
            find(&olena, "gauge"),
            "{name}"
        );
        assert_eq!(
            vec!["Gauge report and updated shelter agreement"],
            find(&olena, "clause"),
            "{name}: by text"
        );
        assert_eq!(
            vec!["Gauge report and updated shelter agreement"],
            find(&olena, "gauge from:anna"),
            "{name}"
        );
        assert_eq!(
            vec!["Gauge report and updated shelter agreement"],
            find(&olena, "gauge has:attachment"),
            "{name}"
        );
        assert_eq!(
            vec!["Police escort schedule"],
            find(&olena, "kowalska")
                .into_iter()
                .chain(find(&olena, "weber"))
                .filter(|t| t.starts_with("Police"))
                .collect::<Vec<_>>(),
            "{name}: by sender"
        );
        assert!(
            find(&marek, "gauge").is_empty(),
            "{name}: not someone else's mail"
        );
        let hit = block_on(search::search(
            &instance,
            &olena,
            &Query::parse("clause"),
            5,
        ))
        .unwrap()
        .remove(0);
        assert!(
            hit.meta["path"]
                .as_str()
                .unwrap()
                .starts_with("/mail?thread="),
            "{name}"
        );
        // The message goes: so does its entry.
        assert!(block_on(store::expunge(&instance, &inbox.id, g.uid)).unwrap());
        assert_eq!(
            vec!["Police escort schedule"],
            find(&olena, "gauge"),
            "{name}"
        );
    }
}
