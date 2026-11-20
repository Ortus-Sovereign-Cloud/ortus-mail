//! What the slice 3 review found, kept fixed, on both installation shapes:
//! no sending as someone else, no SMTP smuggling, labels found anywhere in
//! a message, no mail to private addresses, quotas, safe IMAP SEARCH,
//! LIST and MOVE, CONDSTORE STORE, snoozes that respect a later move, and
//! folders with mail kept unless the app says otherwise.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::dns::Dns;
use ortus_core::auth::accounts::Role;
use ortus_core::instance::{testing_auth::signed_in, Instance};
use ortus_core::runtime::block_on;
use ortus_mail::send::{self, Outgoing, Refused};
use ortus_mail::{imap, smtp, store};
use tokio_rustls::rustls;

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn user(instance: &Arc<Instance>, email: &str) -> (String, String) {
    let token = signed_in(instance, email, Role::Member);
    let id = block_on(ortus_core::auth::accounts::by_email(
        instance.store.as_ref(),
        email,
    ))
    .unwrap()
    .unwrap()
    .id;
    (id, token)
}

struct Line {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Line {
    fn open(port: u16) -> Self {
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut line = Self {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
        };
        line.read();
        line
    }
    fn read(&mut self) -> String {
        let mut all = String::new();
        loop {
            let mut l = String::new();
            self.reader.read_line(&mut l).unwrap();
            all.push_str(&l);
            if l.len() < 4
                || l.as_bytes()[3] == b' '
                || l.starts_with("* OK")
                || l.starts_with("+ ")
            {
                return all;
            }
        }
    }
    fn say(&mut self, text: &str) -> String {
        self.writer
            .write_all(format!("{text}\r\n").as_bytes())
            .unwrap();
        self.read()
    }
}

fn mail(from: &str, to: &str, subject: &str) -> String {
    format!("From: <{from}>\r\nTo: <{to}>\r\nSubject: {subject}\r\nMessage-ID: <{subject}-{}@x.example>\r\nDate: Fri, 20 Nov 2026 10:00:00 +0000\r\n\r\nBody.\r\n", ortus_core::program::now_ms())
}

#[test]
fn review_findings_stay_fixed() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dns = Dns::start();
    // A domain whose mail servers are on a private network.
    dns.mx("inside.example", 10, "mx.inside.example");
    dns.a("mx.inside.example", "10.1.2.3".parse().unwrap());
    unsafe {
        std::env::set_var("ORTUS_DNS_SERVERS", dns.addr.to_string());
        std::env::set_var("ORTUS_MAIL_HOSTNAME", "mail.ministry.example");
        std::env::remove_var("ORTUS_SMTP_ALLOW_PRIVATE");
    }
    for (name, instance, _keep) in common::installations() {
        let (olena, olena_token) = user(&instance, "olena@ministry.example");
        let (taras, _) = user(&instance, "taras@ministry.example");
        block_on(ortus_mail::domains::add(&instance, "ministry.example")).unwrap();
        let to = |a: &str| vec![a.to_string()];

        // 1. The From header must be the sender's own.
        let spoof = mail("taras@ministry.example", "anna@regional.example", "spoof");
        let r = block_on(send::send(
            &instance,
            &Outgoing {
                user: &olena,
                from: "olena@ministry.example",
                to: &to("anna@regional.example"),
                raw: spoof.as_bytes(),
                label: None,
                reason: None,
            },
        ));
        assert!(matches!(r, Err(Refused::NotYourAddress)), "{name}: {r:?}");

        // 12. A label inside a forwarded message is judged too.
        let forwarded = String::from(
            "From: <olena@ministry.example>\r\nTo: <anna@regional.example>\r\nSubject: Fwd\r\nMessage-ID: <f@x>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nSee below.\r\n--b\r\nContent-Type: message/rfc822\r\n\r\nFrom: <taras@ministry.example>\r\nSubject: Inside\r\nX-Ortus-Label:\r\n internal\r\n\r\nInternal only.\r\n--b--\r\n"
        );
        let r = block_on(send::send(
            &instance,
            &Outgoing {
                user: &olena,
                from: "olena@ministry.example",
                to: &to("anna@regional.example"),
                raw: forwarded.as_bytes(),
                label: None,
                reason: None,
            },
        ));
        assert!(
            matches!(r, Err(Refused::NeedsReason { .. })),
            "{name}: a folded label in a forwarded message: {r:?}"
        );

        // 13. No mail to servers on a private network.
        let r = block_on(send::send(
            &instance,
            &Outgoing {
                user: &olena,
                from: "olena@ministry.example",
                to: &to("x@inside.example"),
                raw: mail("olena@ministry.example", "x@inside.example", "inside").as_bytes(),
                label: None,
                reason: None,
            },
        ))
        .unwrap();
        assert_eq!(1, r.queued);
        let job = block_on(instance.store.query(
            "SELECT id, payload FROM ortus_jobs WHERE kind = $1 AND state = 'queued'",
            &[send::DELIVER_JOB.into()],
        ))
        .unwrap();
        let job = ortus_core::jobs::Job {
            id: job[0].text(0).unwrap().into(),
            kind: send::DELIVER_JOB.into(),
            payload: job[0].text(1).unwrap().into(),
            attempt: 16,
            max_attempts: 16,
        };
        block_on(send::deliver(Arc::clone(&instance), &job)).unwrap();
        let state = block_on(instance.store.query(
            "SELECT state, last_error FROM mail_outbound WHERE recipient = 'x@inside.example'",
            &[],
        ))
        .unwrap();
        assert_eq!(Some("bounced"), state[0].text(0), "{name}");
        assert!(
            state[0].text(1).unwrap().contains("private network"),
            "{name}: {:?}",
            state[0].text(1)
        );
        block_on(instance.store.execute(
            "UPDATE ortus_jobs SET state = 'done' WHERE id = $1",
            &[job.id.as_str().into()],
        ))
        .unwrap();

        // 2 and 10. SMTP: a bare LF cannot smuggle a second message; one copy per person.
        let listener =
            block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await }).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop_tx, mut stop) = tokio::sync::watch::channel(false);
        let server = Arc::clone(&instance);
        ortus_core::runtime::io()
            .spawn(async move { smtp::serve(server, listener, &mut stop).await.unwrap() });
        let mut c = Line::open(port);
        c.say("EHLO relay.example");
        c.say("MAIL FROM:<a@relay.example>");
        assert!(c.say("RCPT TO:<olena@ministry.example>").starts_with("250"));
        assert!(c.say("RCPT TO:<olena@ministry.example>").starts_with("250"));
        assert!(c.say("DATA").starts_with("354"));
        c.writer.write_all(b"From: <a@relay.example>\r\nSubject: smuggle\r\n\r\nhello\n.\r\nMAIL FROM:<ceo@ministry.example>\r\nRCPT TO:<olena@ministry.example>\r\nDATA\r\nSubject: spoofed\r\n\r\nx\r\n.\r\n").unwrap();
        let reply = c.read();
        assert!(reply.starts_with("554"), "{name}: bare LF refused: {reply}");
        let r1 = c.say("RSET");
        let r2 = c.say("MAIL FROM:<a@relay.example>");
        let r3 = c.say("RCPT TO:<olena@ministry.example>");
        let r4 = c.say("RCPT TO:<olena@ministry.example>");
        let r5 = c.say("DATA");
        assert!(r5.starts_with("354"), "{name}: {r1}{r2}{r3}{r4}{r5}");
        c.writer
            .write_all(mail("a@relay.example", "olena@ministry.example", "twice").as_bytes())
            .unwrap();
        assert!(c.say(".").starts_with("250"));
        let inbox = block_on(store::by_role(instance.store.as_ref(), &olena, "inbox")).unwrap();
        let entries = block_on(store::entries(instance.store.as_ref(), &inbox.id, 0, 100)).unwrap();
        assert_eq!(
            1,
            entries.iter().filter(|e| e.subject == "twice").count(),
            "{name}: one copy"
        );
        assert!(
            !entries
                .iter()
                .any(|e| e.subject == "spoofed" || e.subject == "smuggle"),
            "{name}: {entries:?}"
        );

        // 17. A full mailbox takes no more.
        block_on(instance.store.execute(
            "UPDATE core_users SET quota_bytes = 1 WHERE id = $1",
            &[taras.as_str().into()],
        ))
        .unwrap();
        block_on(store::deliver(
            &instance,
            &taras,
            &block_on(store::by_role(instance.store.as_ref(), &taras, "inbox"))
                .unwrap()
                .id,
            b"Subject: fill\r\n\r\nx\r\n",
            &[],
            None,
            0,
        ))
        .unwrap();
        c.say("MAIL FROM:<a@relay.example>");
        assert!(
            c.say("RCPT TO:<taras@ministry.example>").starts_with("452"),
            "{name}: full"
        );
        c.say("QUIT");
        let _ = stop_tx.send(true);

        // 14. A snoozed message moved on is left where it was put.
        let m = block_on(store::deliver(
            &instance,
            &olena,
            &inbox.id,
            mail("x@y.example", "olena@ministry.example", "snoozed").as_bytes(),
            &[],
            None,
            0,
        ))
        .unwrap();
        block_on(store::snooze(
            &instance,
            &olena,
            &m.message,
            ortus_core::program::now_ms() - 1,
        ))
        .unwrap();
        let archive = block_on(store::by_role(instance.store.as_ref(), &olena, "archive")).unwrap();
        block_on(store::move_message(
            &instance,
            &olena,
            &m.message,
            &archive.id,
        ))
        .unwrap();
        block_on(store::wake_due(&instance)).unwrap();
        let archived =
            block_on(store::entries(instance.store.as_ref(), &archive.id, 0, 100)).unwrap();
        assert!(
            archived.iter().any(|e| e.message == m.message),
            "{name}: stays in Archive"
        );

        // 4, 8, 9, 16. IMAP: pathological LIST and SEARCH, MOVE when read-only, UNCHANGEDSINCE.
        let dir = common::dir();
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        std::fs::write(dir.0.join("c.pem"), certified.cert.pem()).unwrap();
        std::fs::write(dir.0.join("k.pem"), certified.signing_key.serialize_pem()).unwrap();
        unsafe {
            std::env::set_var("ORTUS_MAIL_TLS_CERT", dir.0.join("c.pem"));
            std::env::set_var("ORTUS_MAIL_TLS_KEY", dir.0.join("k.pem"));
        }
        let made: serde_json::Value = serde_json::from_str(
            rust_web_server::test_client::TestClient::new(ortus_core::app(Arc::clone(&instance)))
                .post("/api/v1/me/app-passwords")
                .header("Authorization", &format!("Bearer {olena_token}"))
                .header("X-Requested-With", "ortus")
                .body_text(r#"{"name":"Review"}"#)
                .send()
                .body_text(),
        )
        .unwrap();
        let password = made["password"].as_str().unwrap().to_string();
        let listener =
            block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await }).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop_tx, mut stop) = tokio::sync::watch::channel(false);
        let server = Arc::clone(&instance);
        ortus_core::runtime::io().spawn(async move {
            imap::serve(server, listener, &mut stop, true)
                .await
                .unwrap()
        });
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certified.cert.der().clone()).unwrap();
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let connection =
            rustls::ClientConnection::new(Arc::new(config), "localhost".try_into().unwrap())
                .unwrap();
        let mut tls = BufReader::new(rustls::StreamOwned::new(connection, tcp));
        let mut greeting = String::new();
        tls.read_line(&mut greeting).unwrap();
        let mut imap = |tag: &str, text: &str| -> String {
            tls.get_mut()
                .write_all(format!("{tag} {text}\r\n").as_bytes())
                .unwrap();
            let mut all = String::new();
            loop {
                let mut l = String::new();
                if tls.read_line(&mut l).unwrap_or(0) == 0 {
                    return all;
                }
                all.push_str(&l);
                if l.starts_with(&format!("{tag} ")) {
                    return all;
                }
            }
        };
        assert!(
            imap("a", &format!("LOGIN olena@ministry.example {password}")).contains("a OK"),
            "{name}"
        );
        let started = Instant::now();
        let stars = "*".repeat(3000);
        assert!(
            imap("b", &format!("LIST \"\" \"{stars}z\"")).contains("b OK"),
            "{name}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{name}: LIST took {:?}",
            started.elapsed()
        );
        assert!(imap("c", "SELECT INBOX").contains("c OK"));
        let deep = format!("{}ALL", "NOT ".repeat(1000));
        assert!(
            imap("d", &format!("SEARCH {deep}")).contains("d OK"),
            "{name}: deep SEARCH answered"
        );
        // UNCHANGEDSINCE older than the message: left alone, flags intact.
        let reply = imap("e", "STORE 1 (UNCHANGEDSINCE 0) +FLAGS (\\Flagged)");
        assert!(reply.contains("e OK"), "{name}: {reply}");
        let reply = imap("f", "STORE 1 (UNCHANGEDSINCE 999999999) +FLAGS (\\Flagged)");
        assert!(
            reply.contains("\\Flagged") && !reply.contains("+FLAGS ("),
            "{name}: {reply}"
        );
        let flags = block_on(store::entries(instance.store.as_ref(), &inbox.id, 0, 100)).unwrap();
        assert!(
            flags
                .iter()
                .all(|e| !e.flags.iter().any(|f| f.contains("+FLAGS"))),
            "{name}: {flags:?}"
        );
        // MOVE is a change: refused when the mailbox was opened read-only.
        assert!(imap("g", "EXAMINE INBOX").contains("g OK"));
        assert!(
            imap("h", "UID MOVE 1:* Archive").contains("h NO"),
            "{name}: read-only"
        );
        imap("z", "LOGOUT");
        let _ = stop_tx.send(true);

        // 15. A folder with mail is not deleted unless the app says so.
        let own = block_on(store::create_mailbox(
            instance.store.as_ref(),
            &olena,
            "Keep",
        ))
        .unwrap()
        .unwrap();
        block_on(store::deliver(
            &instance,
            &olena,
            &own.id,
            b"Subject: kept\r\n\r\nx\r\n",
            &[],
            None,
            0,
        ))
        .unwrap();
        let r = block_on(ortus_mail::jmap::run(
            &instance,
            &block_on(ortus_core::auth::accounts::by_id(instance.store.as_ref(), &olena)).unwrap().unwrap(),
            &serde_json::json!({"using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"], "methodCalls": [["Mailbox/set", {"destroy": [own.id]}, "0"]]}),
        ))
        .unwrap();
        assert_eq!(
            "mailboxHasEmail", r["methodResponses"][0][1]["notDestroyed"][&own.id]["type"],
            "{name}: {r}"
        );
    }
}
