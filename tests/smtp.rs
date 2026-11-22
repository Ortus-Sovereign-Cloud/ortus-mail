//! Receiving over SMTP, end to end: a real TCP session against the
//! receiver, test DNS for SPF, DKIM and DMARC, delivery into the store.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::Arc;

use base64::Engine;
use common::dns::Dns;
use mail_auth::crypto::Ed25519Key;
use mail_auth::dkim::DkimSigner;
use mail_auth::headers::HeaderWriter;
use ortus_core::auth::accounts::Role;
use ortus_core::instance::{testing_auth::signed_in, Instance};
use ortus_core::runtime::block_on;
use ortus_mail::store;

/// The tests here set process-wide settings (DNS servers, TLS files): one
/// at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Client {
    fn connect(port: u16) -> Self {
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(20)))
            .unwrap();
        let mut client = Self {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
        };
        assert!(client.reply().starts_with("220 "));
        client
    }

    /// The last line of the reply.
    fn reply(&mut self) -> String {
        self.reply_lines().last().cloned().unwrap_or_default()
    }

    /// Every line of the reply.
    fn reply_lines(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).unwrap();
            let done = line.len() < 4 || line.as_bytes()[3] == b' ';
            lines.push(line.trim_end().to_string());
            if done {
                return lines;
            }
        }
    }

    fn send(&mut self, line: &str) -> String {
        self.writer
            .write_all(format!("{line}\r\n").as_bytes())
            .unwrap();
        self.reply()
    }

    fn data(&mut self, message: &str) -> String {
        assert!(self.send("DATA").starts_with("354"));
        let stuffed: String = message
            .split("\r\n")
            .map(|l| {
                if l.starts_with('.') {
                    format!(".{l}\r\n")
                } else {
                    format!("{l}\r\n")
                }
            })
            .collect();
        self.writer.write_all(stuffed.as_bytes()).unwrap();
        self.send(".")
    }
}

fn message(from: &str, subject: &str) -> String {
    format!("From: Maria Peters <{from}>\r\nTo: olena@ministry.example\r\nSubject: {subject}\r\nMessage-ID: <{subject}-{}@mail.example>\r\nDate: Thu, 19 Nov 2026 08:05:00 +0000\r\n\r\nMy car was damaged on Mill Lane.\r\n.hidden line starting with a dot\r\n", ortus_core::program::now_ms())
}

fn last_raw(instance: &Arc<Instance>, user: &str, role: &str) -> Option<String> {
    let mailbox = block_on(store::by_role(instance.store.as_ref(), user, role)).unwrap();
    let entries = block_on(store::entries(instance.store.as_ref(), &mailbox.id, 0, 100)).unwrap();
    let last = entries.last()?;
    Some(
        String::from_utf8(
            block_on(store::raw(instance, user, &last.message))
                .unwrap()
                .unwrap(),
        )
        .unwrap(),
    )
}

#[test]
fn mail_is_received_checked_and_delivered() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dns = Dns::start();
    unsafe {
        std::env::set_var("ORTUS_DNS_SERVERS", dns.addr.to_string());
        std::env::set_var("ORTUS_MAIL_HOSTNAME", "mail.ministry.example");
    }
    // mail.example signs with DKIM and allows 127.0.0.1; it asks receivers
    // to reject what fails. spoof.example only asks for quarantine.
    let key = Ed25519Key::generate_pkcs8().unwrap();
    let signer_key = Ed25519Key::from_pkcs8_der(&key).unwrap();
    let public = base64::engine::general_purpose::STANDARD.encode(signer_key.public_key());
    dns.txt("mail.example", &["v=spf1 ip4:127.0.0.1 -all"]);
    dns.txt(
        "s1._domainkey.mail.example",
        &[&format!("v=DKIM1; k=ed25519; p={public}")],
    );
    dns.txt("_dmarc.mail.example", &["v=DMARC1; p=reject"]);
    dns.txt("spoof.example", &["v=spf1 ip4:192.0.2.1 -all"]);
    dns.txt("_dmarc.spoof.example", &["v=DMARC1; p=quarantine"]);

    for (name, instance, _dir) in common::installations() {
        let olena_token = signed_in(&instance, "olena@ministry.example", Role::Member);
        let _ = olena_token;
        let olena = block_on(ortus_core::auth::accounts::by_email(
            instance.store.as_ref(),
            "olena@ministry.example",
        ))
        .unwrap()
        .unwrap()
        .id;
        block_on(ortus_mail::domains::add(&instance, "ministry.example")).unwrap();

        let listener =
            block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await }).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop_tx, mut stop) = tokio::sync::watch::channel(false);
        let server = Arc::clone(&instance);
        ortus_core::runtime::io().spawn(async move {
            ortus_mail::smtp::serve(server, listener, &mut stop)
                .await
                .unwrap();
        });

        // A signed, aligned message: accepted, verdicts recorded.
        let mut c = Client::connect(port);
        assert!(c.send("EHLO mail.example").starts_with("250 "));
        assert!(
            c.send("RCPT TO:<olena@ministry.example>")
                .starts_with("503"),
            "MAIL first"
        );
        assert!(c
            .send("MAIL FROM:<maria@mail.example> SIZE=900")
            .starts_with("250"));
        assert!(
            c.send("RCPT TO:<nobody@ministry.example>")
                .starts_with("550 5.1.1"),
            "{name}"
        );
        assert!(
            c.send("RCPT TO:<someone@other.example>")
                .starts_with("550 5.7.1"),
            "no relaying"
        );
        assert!(c
            .send("RCPT TO:<Olena@Ministry.Example>")
            .starts_with("250"));
        let body = message("maria@mail.example", "signed");
        let signature = DkimSigner::from_key(Ed25519Key::from_pkcs8_der(&key).unwrap())
            .domain("mail.example")
            .selector("s1")
            .headers(["From", "To", "Subject", "Message-ID", "Date"])
            .sign(body.as_bytes())
            .unwrap();
        let reply = c.data(&format!("{}{body}", signature.to_header()));
        assert!(reply.starts_with("250"), "{name}: {reply}");
        let raw = last_raw(&instance, &olena, "inbox").unwrap();
        assert!(
            raw.starts_with("X-Ortus-Spam: score=0.0; reasons=none\r\nReceived: from mail.example (127.0.0.1) by mail.ministry.example"),
            "{raw}"
        );
        assert!(
            raw.contains("spf=pass") && raw.contains("dkim=pass") && raw.contains("dmarc=pass"),
            "{name}: {raw}"
        );
        assert!(raw.contains("\r\n.hidden line"), "dots unstuffed: {raw}");
        // New Important mail in the Inbox is a notification (phones hear of it).
        let notices = |olena: &str| {
            block_on(instance.store.query(
                "SELECT title, detail, path FROM core_notifications WHERE user_id = $1 AND app = 'mail'",
                &[olena.into()],
            ))
            .unwrap()
            .iter()
            .map(|r| format!("{} | {} | {}", r.text(0).unwrap_or(""), r.text(1).unwrap_or(""), r.text(2).unwrap_or("")))
            .collect::<Vec<_>>()
        };
        let told = notices(&olena);
        assert_eq!(1, told.len(), "{name}: {told:?}");
        assert!(told[0].contains("/mail?thread="), "{told:?}");

        // Pretending to be mail.example from elsewhere, unsigned: refused.
        assert!(c.send("MAIL FROM:<boss@spoof.example>").starts_with("250"));
        assert!(c
            .send("RCPT TO:<olena@ministry.example>")
            .starts_with("250"));
        let reply = c.data(&message("ceo@mail.example", "spoofed"));
        assert!(reply.starts_with("550 5.7.1"), "{name}: {reply}");

        // spoof.example asks for quarantine: delivered, to Junk.
        assert!(c.send("MAIL FROM:<news@spoof.example>").starts_with("250"));
        assert!(c
            .send("RCPT TO:<olena@ministry.example>")
            .starts_with("250"));
        assert!(c
            .data(&message("news@spoof.example", "quarantined"))
            .starts_with("250"));
        assert!(last_raw(&instance, &olena, "junk")
            .unwrap()
            .contains("Subject: quarantined"));
        // Junk is no notification.
        assert_eq!(1, notices(&olena).len(), "{name}");

        // Too big by its own word; a session that ends cleanly.
        assert!(c
            .send("MAIL FROM:<maria@mail.example> SIZE=999999999")
            .starts_with("552"));
        assert!(c.send("QUIT").starts_with("221"));
        stop_tx.send(true).unwrap();
    }
}

#[test]
fn starttls_is_offered_and_used() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dns = Dns::start();
    let dir = common::dir();
    let certified =
        rcgen::generate_simple_self_signed(vec!["mail.ministry.example".into()]).unwrap();
    std::fs::write(dir.0.join("cert.pem"), certified.cert.pem()).unwrap();
    std::fs::write(dir.0.join("key.pem"), certified.signing_key.serialize_pem()).unwrap();
    unsafe {
        std::env::set_var("ORTUS_DNS_SERVERS", dns.addr.to_string());
        std::env::set_var("ORTUS_MAIL_TLS_CERT", dir.0.join("cert.pem"));
        std::env::set_var("ORTUS_MAIL_TLS_KEY", dir.0.join("key.pem"));
    }
    let (_, instance, _keep) = common::installations().remove(0);
    signed_in(&instance, "olena@ministry.example", Role::Member);
    block_on(ortus_mail::domains::add(&instance, "ministry.example")).unwrap();
    let listener = block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await }).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
    let server = Arc::clone(&instance);
    ortus_core::runtime::io().spawn(async move {
        ortus_mail::smtp::serve(server, listener, &mut stop)
            .await
            .unwrap();
    });

    let mut c = Client::connect(port);
    c.writer.write_all(b"EHLO sender.example\r\n").unwrap();
    assert!(c.reply_lines().iter().any(|l| l == "250-STARTTLS"));
    assert!(c.send("STARTTLS").starts_with("220"));
    // The same socket, now through TLS, trusting the test certificate.
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.add(certified.cert.der().clone()).unwrap();
    let config = tokio_rustls::rustls::ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connection = tokio_rustls::rustls::ClientConnection::new(
        Arc::new(config),
        "mail.ministry.example".try_into().unwrap(),
    )
    .unwrap();
    let mut tls = tokio_rustls::rustls::StreamOwned::new(connection, c.writer.try_clone().unwrap());
    let mut talk = |line: &str| -> String {
        tls.write_all(format!("{line}\r\n").as_bytes()).unwrap();
        let mut reply = String::new();
        loop {
            let mut buf = [0u8; 1];
            let mut line = Vec::new();
            loop {
                std::io::Read::read_exact(&mut tls, &mut buf).unwrap();
                line.push(buf[0]);
                if buf[0] == b'\n' {
                    break;
                }
            }
            let text = String::from_utf8_lossy(&line).to_string();
            reply.push_str(&text);
            if text.len() < 4 || text.as_bytes()[3] == b' ' {
                return reply;
            }
        }
    };
    let ehlo = talk("EHLO sender.example");
    assert!(
        ehlo.starts_with("250") && !ehlo.contains("STARTTLS"),
        "{ehlo}"
    );
    assert!(talk("MAIL FROM:<x@sender.example>").starts_with("250"));
    assert!(talk("RCPT TO:<olena@ministry.example>").starts_with("250"));
    assert!(talk("DATA").starts_with("354"));
    assert!(
        talk("From: x@sender.example\r\nSubject: over TLS\r\n\r\nhello\r\n.").starts_with("250")
    );
    let olena = block_on(ortus_core::auth::accounts::by_email(
        instance.store.as_ref(),
        "olena@ministry.example",
    ))
    .unwrap()
    .unwrap()
    .id;
    assert!(last_raw(&instance, &olena, "inbox")
        .unwrap()
        .contains("with ESMTPS"));
}

#[test]
fn the_spam_filter_files_junk_and_learns_from_corrections() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dns = Dns::start();
    unsafe {
        std::env::set_var("ORTUS_DNS_SERVERS", dns.addr.to_string());
        std::env::set_var("ORTUS_MAIL_HOSTNAME", "mail.ministry.example");
    }
    dns.txt("deals.example", &["v=spf1 ip4:192.0.2.9 ~all"]);
    let (_, instance, _keep) = common::installations().remove(0);
    let token = signed_in(&instance, "olena@ministry.example", Role::Member);
    let olena = block_on(ortus_core::auth::accounts::by_email(
        instance.store.as_ref(),
        "olena@ministry.example",
    ))
    .unwrap()
    .unwrap()
    .id;
    block_on(ortus_mail::domains::add(&instance, "ministry.example")).unwrap();
    let listener = block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await }).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
    let server = Arc::clone(&instance);
    ortus_core::runtime::io().spawn(async move {
        ortus_mail::smtp::serve(server, listener, &mut stop)
            .await
            .unwrap()
    });

    let shouty = |n: u32| {
        format!("From: \"support@bank.example\" <offers@deals.example>\r\nTo: olena@ministry.example\r\nSubject: CLAIM YOUR REWARD TODAY {n}\r\nDate: Thu, 19 Nov 2026 08:05:00 +0000\r\nMessage-ID: <{n}@deals.example>\r\n\r\nOnly today.\r\n")
    };
    let mut c = Client::connect(port);
    assert!(c.send("EHLO deals.example").starts_with("250"));
    for n in 1..=2 {
        assert!(c
            .send("MAIL FROM:<offers@deals.example>")
            .starts_with("250"));
        assert!(c
            .send("RCPT TO:<olena@ministry.example>")
            .starts_with("250"));
        assert!(c.data(&shouty(n)).starts_with("250"));
        if n == 1 {
            // Filed as junk, with its reasons.
            let junk = last_raw(&instance, &olena, "junk").expect("in Junk");
            assert!(
                junk.starts_with("X-Ortus-Spam: score=") && junk.contains("display_name_spoof"),
                "{junk}"
            );
            // Olena says it is not junk: it moves, and the sender is trusted.
            let mailbox =
                block_on(store::by_role(instance.store.as_ref(), &olena, "junk")).unwrap();
            let message = block_on(store::entries(instance.store.as_ref(), &mailbox.id, 0, 10))
                .unwrap()[0]
                .message
                .clone();
            let client = rust_web_server::test_client::TestClient::new(ortus_core::app(
                Arc::clone(&instance),
            ));
            let moved = client
                .post(&format!("/api/v1/mail/messages/{message}/junk"))
                .header("Authorization", &format!("Bearer {token}"))
                .header("X-Requested-With", "ortus")
                .body_text(r#"{"junk": false}"#)
                .send();
            assert_eq!(200, moved.status(), "{}", moved.body_text());
            assert!(last_raw(&instance, &olena, "inbox")
                .unwrap()
                .contains("REWARD TODAY 1"));
            assert!(last_raw(&instance, &olena, "junk").is_none());
        }
    }
    // The next one from that sender goes to the inbox.
    assert!(
        last_raw(&instance, &olena, "inbox")
            .unwrap()
            .contains("REWARD TODAY 2"),
        "trusted now"
    );
}

#[test]
fn a_trusted_relay_in_front_names_the_real_sender() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dns = Dns::start();
    unsafe {
        std::env::set_var("ORTUS_DNS_SERVERS", dns.addr.to_string());
        std::env::set_var("ORTUS_MAIL_HOSTNAME", "mail.ministry.example");
        std::env::remove_var("ORTUS_SMTP_TRUSTED_RELAYS");
    }
    // relayed.example sends from 203.0.113.5 only, unsigned, and asks
    // receivers to reject what fails.
    dns.txt("relayed.example", &["v=spf1 ip4:203.0.113.5 -all"]);
    dns.txt("_dmarc.relayed.example", &["v=DMARC1; p=reject"]);
    let (_, instance, _keep) = common::installations().remove(0);
    signed_in(&instance, "olena@ministry.example", Role::Member);
    let olena = block_on(ortus_core::auth::accounts::by_email(
        instance.store.as_ref(),
        "olena@ministry.example",
    ))
    .unwrap()
    .unwrap()
    .id;
    block_on(ortus_mail::domains::add(&instance, "ministry.example")).unwrap();
    let listener = block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await }).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
    let server = Arc::clone(&instance);
    ortus_core::runtime::io().spawn(async move {
        ortus_mail::smtp::serve(server, listener, &mut stop)
            .await
            .unwrap();
    });
    let send = |c: &mut Client, subject: &str| {
        assert!(c
            .send("MAIL FROM:<anna@relayed.example>")
            .starts_with("250"));
        assert!(c
            .send("RCPT TO:<olena@ministry.example>")
            .starts_with("250"));
        c.data(&message("anna@relayed.example", subject))
    };

    // Not trusted: no XFORWARD, and 127.0.0.1 fails SPF; rejected.
    let mut c = Client::connect(port);
    c.writer.write_all(b"EHLO relay.local\r\n").unwrap();
    assert!(!c.reply_lines().iter().any(|l| l.contains("XFORWARD")));
    assert!(c.send("XFORWARD ADDR=203.0.113.5").starts_with("502"));
    assert!(send(&mut c, "unrelayed").starts_with("550 5.7.1"));
    assert!(c.send("QUIT").starts_with("221"));

    // Trusted: the relay names the sender, which SPF allows.
    unsafe { std::env::set_var("ORTUS_SMTP_TRUSTED_RELAYS", "192.0.2.9, 127.0.0.1") };
    let mut c = Client::connect(port);
    c.writer.write_all(b"EHLO relay.local\r\n").unwrap();
    assert!(c
        .reply_lines()
        .iter()
        .any(|l| l == "250-XFORWARD NAME ADDR PROTO HELO"));
    assert!(c
        .send("XFORWARD NAME=mx.relayed.example ADDR=203.0.113.5")
        .starts_with("250"));
    assert!(c
        .send("XFORWARD PROTO=ESMTP HELO=mx.relayed.example")
        .starts_with("250"));
    let reply = send(&mut c, "relayed");
    assert!(reply.starts_with("250"), "{reply}");
    let raw = last_raw(&instance, &olena, "inbox").unwrap();
    assert!(
        raw.contains("Received: from mx.relayed.example (203.0.113.5)"),
        "{raw}"
    );
    assert!(
        raw.contains("spf=pass") && raw.contains("dmarc=pass"),
        "{raw}"
    );

    // Forgotten after each message: the next one is the relay's own again.
    assert!(send(&mut c, "after").starts_with("550 5.7.1"));
    assert!(c.send("XFORWARD ADDR=not-an-address").starts_with("501"));
    assert!(c.send("QUIT").starts_with("221"));
    unsafe { std::env::remove_var("ORTUS_SMTP_TRUSTED_RELAYS") };
}
