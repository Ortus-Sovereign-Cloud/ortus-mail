//! Outbound limits, on each shape: at most 100 recipients in one message;
//! at most ORTUS_MAIL_SEND_PER_DAY recipients outside the installation in
//! 24 hours per person (here 3), refused and audited past that; mail
//! inside the installation does not count.

mod common;

use std::sync::Arc;

use ortus_core::auth::accounts::Role;
use ortus_core::instance::{testing_auth::signed_in, Instance};
use ortus_core::program::now_ms;
use ortus_core::runtime::block_on;
use ortus_mail::send::{self, Outgoing, Refused};

fn user(instance: &Arc<Instance>, email: &str) -> String {
    signed_in(instance, email, Role::Member);
    block_on(ortus_core::auth::accounts::by_email(
        instance.store.as_ref(),
        email,
    ))
    .unwrap()
    .unwrap()
    .id
}

fn message(subject: &str) -> Vec<u8> {
    format!("From: Olena Hrytsenko <olena@ministry.example>\r\nTo: someone\r\nSubject: {subject}\r\nDate: Fri, 20 Nov 2026 09:00:00 +0000\r\nMessage-ID: <{subject}-{}@ministry.example>\r\n\r\nHello.\r\n", now_ms()).into_bytes()
}

#[test]
fn outbound_limits() {
    unsafe { std::env::set_var("ORTUS_MAIL_SEND_PER_DAY", "3") };
    for (name, a, _dir) in common::installations() {
        let olena = user(&a, "olena@ministry.example");
        user(&a, "sam@ministry.example");
        block_on(ortus_mail::domains::add(&a, "ministry.example")).unwrap();
        let send = |to: Vec<String>, subject: &str| {
            let raw = message(subject);
            block_on(send::send(
                &a,
                &Outgoing {
                    user: &olena,
                    from: "olena@ministry.example",
                    to: &to,
                    raw: &raw,
                    label: None,
                    reason: None,
                },
            ))
        };
        // 101 recipients in one message: refused before anything goes.
        let many: Vec<String> = (0..101).map(|i| format!("p{i}@outside.example")).collect();
        assert!(
            matches!(
                send(many, "many"),
                Err(Refused::TooMany {
                    limit: "per_message"
                })
            ),
            "{name}"
        );
        // Inside the installation: not counted.
        for i in 0..5 {
            assert!(
                send(vec!["sam@ministry.example".into()], &format!("inside-{i}")).is_ok(),
                "{name}"
            );
        }
        // Outside: three today, then refused and audited.
        assert!(
            send(
                vec!["a@outside.example".into(), "b@outside.example".into()],
                "two"
            )
            .is_ok(),
            "{name}"
        );
        assert!(
            send(vec!["c@outside.example".into()], "three").is_ok(),
            "{name}"
        );
        assert!(
            matches!(
                send(vec!["d@outside.example".into()], "four"),
                Err(Refused::TooMany { limit: "per_day" })
            ),
            "{name}"
        );
        let audited = block_on(a.store.query(
            "SELECT CAST(COUNT(*) AS BIGINT) FROM core_audit WHERE action = 'mail.limit'",
            &[],
        ))
        .unwrap()[0]
            .int(0)
            .unwrap();
        assert_eq!(1, audited, "{name}");
        // A delivery already sent is not sent again (a job tried twice).
        let rows = block_on(a.store.query("SELECT id, payload, max_attempts FROM ortus_jobs WHERE kind LIKE 'mail.%' AND payload LIKE '%outside.example%' LIMIT 1", &[])).unwrap();
        let job = ortus_core::jobs::Job {
            id: rows[0].text(0).unwrap().into(),
            kind: "mail.deliver".into(),
            payload: rows[0].text(1).unwrap().into(),
            attempt: 1,
            max_attempts: rows[0].int(2).unwrap(),
        };
        let message = serde_json::from_str::<serde_json::Value>(&job.payload).unwrap()["message"]
            .as_str()
            .unwrap()
            .to_string();
        block_on(a.store.execute(
            "UPDATE mail_outbound SET state = 'sent' WHERE message = $1",
            &[message.as_str().into()],
        ))
        .unwrap();
        assert_eq!(
            Ok(()),
            block_on(send::deliver(Arc::clone(&a), &job)),
            "{name}: no network touched"
        );
        let states = block_on(a.store.query(
            "SELECT state FROM mail_outbound WHERE message = $1",
            &[message.as_str().into()],
        ))
        .unwrap();
        assert!(states.iter().all(|r| r.text(0) == Some("sent")), "{name}");
        // A day later the count starts again.
        block_on(
            a.store
                .execute("UPDATE mail_outbound SET created_at = 0", &[]),
        )
        .unwrap();
        assert!(
            send(vec!["d@outside.example".into()], "next-day").is_ok(),
            "{name}"
        );
    }
}
