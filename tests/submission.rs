//! Submission for signed-in apps: no password before TLS, AUTH with an
//! app password, sending as the web would; and pipelined commands on the
//! receiving port are all answered.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use base64::Engine;
use common::dns::Dns;
use ortus_core::auth::accounts::Role;
use ortus_core::instance::testing_auth::signed_in;
use ortus_core::runtime::block_on;
use ortus_mail::smtp::{self, Mode};
use ortus_mail::store;
use tokio_rustls::rustls;

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn reply_from(read: &mut dyn FnMut() -> String) -> String {
    let mut all = String::new();
    loop {
        let line = read();
        all.push_str(&line);
        if line.len() < 4 || line.as_bytes()[3] == b' ' {
            return all;
        }
    }
}

#[test]
fn apps_sign_in_with_an_app_password_and_send() {
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
        std::env::set_var("ORTUS_MAIL_HOSTNAME", "mail.ministry.example");
    }
    for (name, instance, _keep) in common::installations() {
        let token = signed_in(&instance, "olena@ministry.example", Role::Member);
        signed_in(&instance, "sam@ministry.example", Role::Member);
        let sam = block_on(ortus_core::auth::accounts::by_email(
            instance.store.as_ref(),
            "sam@ministry.example",
        ))
        .unwrap()
        .unwrap()
        .id;
        block_on(ortus_mail::domains::add(&instance, "ministry.example")).unwrap();
        let client =
            rust_web_server::test_client::TestClient::new(ortus_core::app(Arc::clone(&instance)));
        let made: serde_json::Value = serde_json::from_str(
            client
                .post("/api/v1/me/app-passwords")
                .header("Authorization", &format!("Bearer {token}"))
                .header("X-Requested-With", "ortus")
                .body_text(r#"{"name":"Thunderbird"}"#)
                .send()
                .body_text(),
        )
        .unwrap();
        let password = made["password"].as_str().unwrap().to_string();

        let listener =
            block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await }).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
        let server = Arc::clone(&instance);
        ortus_core::runtime::io().spawn(async move {
            smtp::serve_as(
                server,
                listener,
                &mut stop,
                Mode::Submission {
                    implicit_tls: false,
                },
            )
            .await
            .unwrap()
        });

        // In the clear: no AUTH offered, and none taken.
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut plain = BufReader::new(stream.try_clone().unwrap());
        let mut raw = stream.try_clone().unwrap();
        let mut line = || {
            let mut l = String::new();
            plain.read_line(&mut l).unwrap();
            l
        };
        assert!(line().starts_with("220"));
        raw.write_all(b"EHLO laptop\r\n").unwrap();
        let ehlo = reply_from(&mut line);
        assert!(
            ehlo.contains("STARTTLS") && !ehlo.contains("AUTH"),
            "{ehlo}"
        );
        raw.write_all(b"AUTH PLAIN AGEAYg==\r\n").unwrap();
        assert!(
            reply_from(&mut line).starts_with("538"),
            "{name}: no passwords in the clear"
        );
        raw.write_all(b"MAIL FROM:<olena@ministry.example>\r\n")
            .unwrap();
        assert!(reply_from(&mut line).starts_with("530"));
        raw.write_all(b"STARTTLS\r\n").unwrap();
        assert!(reply_from(&mut line).starts_with("220"));

        let mut roots = rustls::RootCertStore::empty();
        roots.add(certified.cert.der().clone()).unwrap();
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let connection = rustls::ClientConnection::new(
            Arc::new(config),
            "mail.ministry.example".try_into().unwrap(),
        )
        .unwrap();
        let mut tls = rustls::StreamOwned::new(connection, stream);
        let mut talk = |text: &str| -> String {
            tls.write_all(text.as_bytes()).unwrap();
            let mut reply = String::new();
            loop {
                let mut line = Vec::new();
                let mut byte = [0u8; 1];
                loop {
                    tls.read_exact(&mut byte).unwrap();
                    line.push(byte[0]);
                    if byte[0] == b'\n' {
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
        assert!(talk("EHLO laptop\r\n").contains("AUTH PLAIN LOGIN"));
        let b64 = base64::engine::general_purpose::STANDARD;
        let wrong = b64.encode("\0olena@ministry.example\0aaaa-bbbb-cccc-dddd");
        assert!(
            talk(&format!("AUTH PLAIN {wrong}\r\n")).starts_with("535"),
            "{name}"
        );
        // AUTH LOGIN, step by step.
        assert!(talk("AUTH LOGIN\r\n").starts_with("334"));
        assert!(talk(&format!("{}\r\n", b64.encode("olena@ministry.example"))).starts_with("334"));
        assert!(
            talk(&format!("{}\r\n", b64.encode(&password))).starts_with("235"),
            "{name}"
        );

        // Not her address: refused when it would be sent.
        assert!(talk("MAIL FROM:<director@ministry.example>\r\n").starts_with("250"));
        assert!(talk("RCPT TO:<sam@ministry.example>\r\n").starts_with("250"));
        assert!(talk("DATA\r\n").starts_with("354"));
        assert!(
            talk("From: director@ministry.example\r\nSubject: x\r\n\r\nx\r\n.\r\n")
                .starts_with("553"),
            "{name}"
        );
        // Hers: sent, Sam has it.
        assert!(talk("MAIL FROM:<olena@ministry.example>\r\n").starts_with("250"));
        assert!(talk("RCPT TO:<sam@ministry.example>\r\n").starts_with("250"));
        assert!(
            talk("RCPT TO:<anna@regional.example>\r\n").starts_with("250"),
            "anyone, once signed in"
        );
        assert!(talk("DATA\r\n").starts_with("354"));
        let reply = talk("From: Olena <olena@ministry.example>\r\nTo: sam@ministry.example\r\nSubject: from Thunderbird\r\nMessage-ID: <t1@ministry.example>\r\nDate: Fri, 20 Nov 2026 10:00:00 +0000\r\n\r\nSent from an app.\r\n.\r\n");
        assert!(reply.starts_with("250"), "{name}: {reply}");
        let inbox = block_on(store::by_role(instance.store.as_ref(), &sam, "inbox")).unwrap();
        let entries = block_on(store::entries(instance.store.as_ref(), &inbox.id, 0, 10)).unwrap();
        assert_eq!("from Thunderbird", entries.last().unwrap().subject);
        let queued = block_on(
            instance
                .store
                .query("SELECT recipient FROM mail_outbound", &[]),
        )
        .unwrap();
        assert_eq!(Some("anna@regional.example"), queued[0].text(0));
        talk("QUIT\r\n");
    }
}

#[test]
fn pipelined_commands_are_all_answered() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dns = Dns::start();
    unsafe { std::env::set_var("ORTUS_DNS_SERVERS", dns.addr.to_string()) };
    let (_, instance, _keep) = common::installations().remove(0);
    signed_in(&instance, "olena@ministry.example", Role::Member);
    block_on(ortus_mail::domains::add(&instance, "ministry.example")).unwrap();
    let listener = block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await }).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
    let server = Arc::clone(&instance);
    ortus_core::runtime::io()
        .spawn(async move { smtp::serve(server, listener, &mut stop).await.unwrap() });
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = || {
        let mut l = String::new();
        reader.read_line(&mut l).unwrap();
        l
    };
    assert!(line().starts_with("220"));
    // Everything in one write, as PIPELINING allows.
    (&stream).write_all(b"EHLO sender.example\r\nMAIL FROM:<a@sender.example>\r\nRCPT TO:<olena@ministry.example>\r\nRCPT TO:<nobody@ministry.example>\r\nDATA\r\n").unwrap();
    let replies = [
        reply_from(&mut line),
        reply_from(&mut line),
        reply_from(&mut line),
        reply_from(&mut line),
        reply_from(&mut line),
    ];
    assert!(
        replies[0].starts_with("250")
            && replies[1].starts_with("250")
            && replies[2].starts_with("250"),
        "{replies:?}"
    );
    assert!(
        replies[3].starts_with("550") && replies[4].starts_with("354"),
        "{replies:?}"
    );
}
