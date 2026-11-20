//! Mail with an outside server (Postfix, from ci/outside-mail.sh), both
//! ways, on both installation shapes: mail from bob@outside.example reaches
//! Olena's inbox with SPF checked; her reply is queued, delivered by the
//! job to Postfix's MX (found through the test DNS), signed with DKIM.
//! Runs when ORTUS_TEST_OUTSIDE_MAILDIR is set; CI requires it.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::dns::Dns;
use ortus_core::auth::accounts::Role;
use ortus_core::instance::testing_auth::signed_in;
use ortus_core::jobs::Job;
use ortus_core::runtime::block_on;
use ortus_mail::{send, smtp, store};

fn talk(reader: &mut BufReader<TcpStream>, writer: &mut TcpStream, line: &str) -> String {
    writer.write_all(format!("{line}\r\n").as_bytes()).unwrap();
    loop {
        let mut l = String::new();
        reader.read_line(&mut l).unwrap();
        if l.len() < 4 || l.as_bytes()[3] == b' ' {
            return l;
        }
    }
}

fn wait_for<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(found) = check() {
            return found;
        }
        assert!(Instant::now() < deadline, "waited a minute for {what}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
fn mail_goes_both_ways_with_an_outside_server() {
    let Ok(maildir) = std::env::var("ORTUS_TEST_OUTSIDE_MAILDIR") else {
        assert!(std::env::var("ORTUS_REQUIRE_OUTSIDE").is_err(), "ORTUS_REQUIRE_OUTSIDE is set but ORTUS_TEST_OUTSIDE_MAILDIR is not: run ci/outside-mail.sh");
        eprintln!("skipped: no outside mail server (ci/outside-mail.sh)");
        return;
    };
    let dns = Dns::start();
    dns.mx("outside.example", 10, "mail.outside.example");
    dns.a("mail.outside.example", "127.0.0.1".parse().unwrap());
    dns.txt("outside.example", &["v=spf1 ip4:127.0.0.1 -all"]);
    unsafe {
        std::env::set_var("ORTUS_DNS_SERVERS", dns.addr.to_string());
        std::env::set_var("ORTUS_MAIL_HOSTNAME", "mail.ministry.example");
        std::env::set_var("ORTUS_SMTP_OUTBOUND_PORT", "2525");
        // The outside server is on this machine.
        std::env::set_var("ORTUS_SMTP_ALLOW_PRIVATE", "1");
    }
    for (name, instance, _keep) in common::installations() {
        signed_in(&instance, "olena@ministry.example", Role::Member);
        let olena = block_on(ortus_core::auth::accounts::by_email(
            instance.store.as_ref(),
            "olena@ministry.example",
        ))
        .unwrap()
        .unwrap()
        .id;
        block_on(ortus_mail::domains::add(&instance, "ministry.example")).unwrap();
        // Postfix hands mail for ministry.example to 127.0.0.1:2526.
        let listener = block_on(async { tokio::net::TcpListener::bind("127.0.0.1:2526").await })
            .expect("port 2526 for Postfix to deliver to");
        let (stop_tx, mut stop) = tokio::sync::watch::channel(false);
        let server = Arc::clone(&instance);
        let serving = ortus_core::runtime::io()
            .spawn(async move { smtp::serve(server, listener, &mut stop).await.unwrap() });

        // In: Bob writes from outside; Postfix relays to us.
        let subject = format!("Flood barriers {name} {}", ortus_core::program::now_ms());
        let stream = TcpStream::connect(("127.0.0.1", 2525)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        let mut greeting = String::new();
        reader.read_line(&mut greeting).unwrap();
        assert!(talk(&mut reader, &mut writer, "EHLO client.outside.example").starts_with("250"));
        assert!(
            talk(&mut reader, &mut writer, "MAIL FROM:<bob@outside.example>").starts_with("250")
        );
        assert!(
            talk(&mut reader, &mut writer, "RCPT TO:<olena@ministry.example>").starts_with("250")
        );
        assert!(talk(&mut reader, &mut writer, "DATA").starts_with("354"));
        let message = format!("From: Bob Smith <bob@outside.example>\r\nTo: olena@ministry.example\r\nSubject: {subject}\r\nMessage-ID: <{}@outside.example>\r\nDate: Fri, 20 Nov 2026 10:00:00 +0000\r\n\r\nHow many barriers do you need?\r\n", ortus_core::program::now_ms());
        writer.write_all(message.as_bytes()).unwrap();
        assert!(talk(&mut reader, &mut writer, ".").starts_with("250"));
        talk(&mut reader, &mut writer, "QUIT");
        let inbox = block_on(store::by_role(instance.store.as_ref(), &olena, "inbox")).unwrap();
        let got = wait_for("Bob's mail via Postfix", || {
            block_on(store::entries(instance.store.as_ref(), &inbox.id, 0, 100))
                .unwrap()
                .into_iter()
                .find(|e| e.subject == subject)
        });
        let raw = String::from_utf8(
            block_on(store::raw(&instance, &olena, &got.message))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(
            raw.contains("mail.outside.example") && raw.contains("spf=pass"),
            "{name}: received through Postfix, SPF checked: {raw}"
        );
        let _ = stop_tx.send(true);
        let _ = block_on(serving);

        // Out: Olena answers; the queued delivery goes to Postfix.
        let reply = format!("From: Olena <olena@ministry.example>\r\nTo: bob@outside.example\r\nSubject: Re: {subject}\r\nMessage-ID: <{}@ministry.example>\r\nDate: Fri, 20 Nov 2026 10:05:00 +0000\r\n\r\nForty, please.\r\n", ortus_core::program::now_ms());
        let to = vec!["bob@outside.example".to_string()];
        block_on(send::send(
            &instance,
            &send::Outgoing {
                user: &olena,
                from: "olena@ministry.example",
                to: &to,
                raw: reply.as_bytes(),
                label: None,
                reason: None,
            },
        ))
        .unwrap();
        let jobs = block_on(instance.store.query(
            "SELECT id, payload, max_attempts FROM ortus_jobs WHERE kind = $1 AND state = 'queued'",
            &[send::DELIVER_JOB.into()],
        ))
        .unwrap();
        assert_eq!(1, jobs.len(), "{name}");
        let job = Job {
            id: jobs[0].text(0).unwrap().into(),
            kind: send::DELIVER_JOB.into(),
            payload: jobs[0].text(1).unwrap().into(),
            attempt: 1,
            max_attempts: jobs[0].int(2).unwrap(),
        };
        block_on(send::deliver(Arc::clone(&instance), &job)).unwrap();
        block_on(instance.store.execute(
            "UPDATE ortus_jobs SET state = 'done' WHERE id = $1",
            &[job.id.as_str().into()],
        ))
        .unwrap();
        let delivered = wait_for("the reply in Bob's Maildir", || {
            let dir = std::path::Path::new(&maildir).join("new");
            std::fs::read_dir(dir).ok()?.flatten().find_map(|f| {
                let text = std::fs::read_to_string(f.path()).ok()?;
                text.contains(&format!("Subject: Re: {subject}"))
                    .then_some(text)
            })
        });
        assert!(
            delivered.contains("DKIM-Signature:") && delivered.contains("d=ministry.example"),
            "{name}: signed: {delivered}"
        );
        let state = block_on(
            instance
                .store
                .query("SELECT state, mx FROM mail_outbound", &[]),
        )
        .unwrap();
        assert_eq!(Some("sent"), state[0].text(0), "{name}");
        assert_eq!(Some("mail.outside.example"), state[0].text(1));
    }
}
