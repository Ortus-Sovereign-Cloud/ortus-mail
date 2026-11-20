//! Sending, end to end: installation A (each shape) sends to installation
//! B over SMTP; B checks A's DKIM, SPF and DMARC through the test DNS.
//! Recipients at A get it directly; an unknown recipient at B bounces;
//! a SECRET message never leaves.

mod common;

use std::sync::Arc;

use common::dns::Dns;
use ortus_core::auth::accounts::Role;
use ortus_core::instance::{testing_auth::signed_in, Instance};
use ortus_core::program::{now_ms, AppWorker};
use ortus_core::runtime::block_on;
use ortus_mail::send::{self, Outgoing, Refused};
use ortus_mail::store;

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

fn inbox(instance: &Arc<Instance>, user: &str, role: &str) -> Vec<String> {
    let mailbox = block_on(store::by_role(instance.store.as_ref(), user, role)).unwrap();
    block_on(store::entries(instance.store.as_ref(), &mailbox.id, 0, 100))
        .unwrap()
        .iter()
        .map(|e| {
            String::from_utf8(
                block_on(store::raw(instance, user, &e.message))
                    .unwrap()
                    .unwrap(),
            )
            .unwrap()
        })
        .collect()
}

fn message(to: &str, subject: &str) -> Vec<u8> {
    format!("From: Olena Hrytsenko <olena@ministry.example>\r\nTo: {to}\r\nSubject: {subject}\r\nDate: Fri, 20 Nov 2026 09:00:00 +0000\r\nMessage-ID: <{subject}-{}@ministry.example>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nThe shelter agreement, as discussed.\r\n", now_ms()).into_bytes()
}

fn run_jobs(instance: &Arc<Instance>) {
    let worker = AppWorker::new(Arc::clone(instance), "test");
    for _ in 0..50 {
        if !block_on(worker.tick(now_ms())).unwrap() {
            break;
        }
    }
}

#[test]
fn mail_goes_out_signed_and_comes_back_when_it_cannot() {
    let dns = Dns::start();
    unsafe {
        std::env::set_var("ORTUS_DNS_SERVERS", dns.addr.to_string());
        std::env::set_var("ORTUS_MAIL_HOSTNAME", "mail.ministry.example");
    }
    // B: the outside server, for regional.example.
    let (_, b, _keep_b) = common::installations().remove(0);
    let anna = user(&b, "anna@regional.example");
    block_on(ortus_mail::domains::add(&b, "regional.example")).unwrap();
    let listener = block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await }).unwrap();
    unsafe {
        std::env::set_var(
            "ORTUS_SMTP_OUTBOUND_PORT",
            listener.local_addr().unwrap().port().to_string(),
        );
        // The other installation is on this machine.
        std::env::set_var("ORTUS_SMTP_ALLOW_PRIVATE", "1");
    };
    let (_stop, mut stop) = tokio::sync::watch::channel(false);
    let server = Arc::clone(&b);
    ortus_core::runtime::io().spawn(async move {
        ortus_mail::smtp::serve(server, listener, &mut stop)
            .await
            .unwrap()
    });
    dns.mx("regional.example", 10, "mx.regional.example.");
    dns.a("mx.regional.example", std::net::Ipv4Addr::LOCALHOST);

    for (name, a, _dir) in common::installations() {
        let olena = user(&a, "olena@ministry.example");
        let sam = user(&a, "sam@ministry.example");
        block_on(ortus_mail::domains::add(&a, "ministry.example")).unwrap();
        // A's records, as its owner published them.
        let shown = block_on(ortus_mail::domains::show(&a, "ministry.example")).unwrap();
        let dkim = shown["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["purpose"] == "dkim_rsa")
            .unwrap();
        dns.txt(
            &format!("{}.ministry.example", dkim["name"].as_str().unwrap()),
            &[dkim["value"].as_str().unwrap()],
        );
        dns.txt("ministry.example", &["v=spf1 ip4:127.0.0.1 -all"]);
        dns.txt("_dmarc.ministry.example", &["v=DMARC1; p=reject"]);

        // Not her address: refused.
        let to = vec!["anna@regional.example".to_string()];
        let raw = message("anna@regional.example", "agreement");
        let spoof = Outgoing {
            user: &olena,
            from: "director@ministry.example",
            to: &to,
            raw: &raw,
            label: None,
            reason: None,
        };
        assert_eq!(
            Err(Refused::NotYourAddress),
            block_on(send::send(&a, &spoof))
        );

        // Labelled INTERNAL, to outside, it needs a reason; then it goes.
        let to = vec![
            "anna@regional.example".to_string(),
            "sam@ministry.example".to_string(),
        ];
        let out = Outgoing {
            user: &olena,
            from: "olena@ministry.example",
            to: &to,
            raw: &raw,
            label: Some("internal"),
            reason: None,
        };
        assert!(
            matches!(
                block_on(send::send(&a, &out)),
                Err(Refused::NeedsReason { .. })
            ),
            "{name}"
        );
        let out = Outgoing {
            reason: Some("agreed with the regional office"),
            ..out
        };
        let sent = block_on(send::send(&a, &out)).unwrap();
        assert_eq!((1, 1), (sent.delivered_here, sent.queued), "{name}");
        assert!(
            inbox(&a, &sam, "inbox")
                .last()
                .unwrap()
                .contains("Subject: agreement"),
            "{name}: Sam got it at once"
        );
        let copy = inbox(&a, &olena, "sent").pop().unwrap();
        assert!(
            copy.starts_with("DKIM-Signature: v=1; a=rsa-sha256; s=ortus"),
            "{name}: signed: {copy}"
        );

        // The queue delivers it to B, where it checks out.
        run_jobs(&a);
        let received = inbox(&b, &anna, "inbox").pop().unwrap_or_default();
        assert!(
            received.contains("Subject: agreement"),
            "{name}: {received}"
        );
        assert!(
            received.contains("dkim=pass")
                && received.contains("spf=pass")
                && received.contains("dmarc=pass"),
            "{name}: {received}"
        );
        let state = block_on(a.store.query(
            "SELECT state FROM mail_outbound WHERE recipient = 'anna@regional.example'",
            &[],
        ))
        .unwrap();
        assert_eq!(Some("sent"), state[0].text(0));

        // Nobody by that name at B: a bounce in Olena's inbox.
        let to = vec!["nobody@regional.example".to_string()];
        let raw = message("nobody@regional.example", "lost");
        block_on(send::send(
            &a,
            &Outgoing {
                user: &olena,
                from: "olena@ministry.example",
                to: &to,
                raw: &raw,
                // Unlabelled mail goes as mail always has.
                label: None,
                reason: None,
            },
        ))
        .unwrap();
        run_jobs(&a);
        let bounce = inbox(&a, &olena, "inbox").pop().unwrap_or_default();
        assert!(
            bounce.contains("Subject: Undelivered: lost")
                && bounce.contains("nobody@regional.example")
                && bounce.contains("550"),
            "{name}: {bounce}"
        );

        // SECRET never leaves, and nothing is kept as sent; a mail app's
        // label header counts the same.
        let before = inbox(&a, &olena, "sent").len();
        let mut labelled = b"X-Ortus-Label: SECRET\r\n".to_vec();
        labelled.extend(message("anna@regional.example", "from-an-app"));
        let to_anna = vec!["anna@regional.example".to_string()];
        let by_app = block_on(send::send(
            &a,
            &Outgoing {
                user: &olena,
                from: "olena@ministry.example",
                to: &to_anna,
                raw: &labelled,
                label: None,
                reason: None,
            },
        ));
        assert!(
            matches!(by_app, Err(Refused::Policy { .. })),
            "{name}: {by_app:?}"
        );
        let raw = message("anna@regional.example", "secret");
        let to = vec!["anna@regional.example".to_string()];
        let refused = block_on(send::send(
            &a,
            &Outgoing {
                user: &olena,
                from: "olena@ministry.example",
                to: &to,
                raw: &raw,
                label: Some("secret"),
                reason: Some("urgent"),
            },
        ));
        assert!(
            matches!(refused, Err(Refused::Policy { .. })),
            "{name}: {refused:?}"
        );
        assert_eq!(before, inbox(&a, &olena, "sent").len());
    }
}
