//! IMAP: no password in the clear, and a real client (Python's imaplib)
//! over TLS doing what mail apps do, on both installation shapes.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::Arc;

use ortus_core::auth::accounts::Role;
use ortus_core::instance::testing_auth::signed_in;
use ortus_core::runtime::block_on;
use ortus_mail::{imap, store};

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

const FLOOD: &str = "From: Maria Peters <maria@mail.example>\r\nTo: olena@ministry.example\r\nSubject: Flood damage report\r\nDate: Thu, 19 Nov 2026 08:05:00 +0000\r\nMessage-ID: <a@mail.example>\r\n\r\nMy car was damaged.\r\n";
const BUDGET: &str = "From: Sam Kowalski <sam@ministry.example>\r\nTo: olena@ministry.example\r\nSubject: The budget\r\nDate: Thu, 19 Nov 2026 09:00:00 +0000\r\nMessage-ID: <b@ministry.example>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"x\"\r\n\r\n--x\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nNumbers attached.\r\n--x\r\nContent-Type: text/csv; name=\"budget.csv\"\r\nContent-Disposition: attachment; filename=\"budget.csv\"\r\n\r\nline,amount\r\nroads,120\r\n--x--\r\n";

fn listen(
    instance: &Arc<ortus_core::instance::Instance>,
    implicit_tls: bool,
) -> (u16, tokio::sync::watch::Sender<bool>) {
    let listener = block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await }).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop_tx, mut stop) = tokio::sync::watch::channel(false);
    let server = Arc::clone(instance);
    ortus_core::runtime::io().spawn(async move {
        imap::serve(server, listener, &mut stop, implicit_tls)
            .await
            .unwrap()
    });
    (port, stop_tx)
}

#[test]
fn mail_apps_read_and_file_mail_over_imap() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = common::dir();
    let certified =
        rcgen::generate_simple_self_signed(vec!["mail.ministry.example".into()]).unwrap();
    let cafile = dir.0.join("cert.pem");
    std::fs::write(&cafile, certified.cert.pem()).unwrap();
    std::fs::write(dir.0.join("key.pem"), certified.signing_key.serialize_pem()).unwrap();
    unsafe {
        std::env::set_var("ORTUS_MAIL_TLS_CERT", &cafile);
        std::env::set_var("ORTUS_MAIL_TLS_KEY", dir.0.join("key.pem"));
    }
    for (name, instance, _keep) in common::installations() {
        let token = signed_in(&instance, "olena@ministry.example", Role::Member);
        let olena = block_on(ortus_core::auth::accounts::by_email(
            instance.store.as_ref(),
            "olena@ministry.example",
        ))
        .unwrap()
        .unwrap()
        .id;
        let inbox = block_on(store::by_role(instance.store.as_ref(), &olena, "inbox")).unwrap();
        for raw in [FLOOD, BUDGET] {
            block_on(store::deliver(
                &instance,
                &olena,
                &inbox.id,
                raw.as_bytes(),
                &[],
                None,
                0,
            ))
            .unwrap();
        }
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

        // Port 143 in the clear: STARTTLS offered, passwords refused.
        let (plain_port, _stop_plain) = listen(&instance, false);
        let stream = TcpStream::connect(("127.0.0.1", plain_port)).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        let mut line = || {
            let mut l = String::new();
            reader.read_line(&mut l).unwrap();
            l
        };
        let greeting = line();
        assert!(
            greeting.contains("STARTTLS")
                && greeting.contains("LOGINDISABLED")
                && !greeting.contains("AUTH=PLAIN"),
            "{name}: {greeting}"
        );
        writer
            .write_all(format!("a1 LOGIN olena@ministry.example {password}\r\n").as_bytes())
            .unwrap();
        assert!(
            line().starts_with("a1 NO [PRIVACYREQUIRED]"),
            "{name}: no passwords in the clear"
        );
        writer.write_all(b"a2 SELECT INBOX\r\n").unwrap();
        assert!(line().starts_with("a2 NO"), "{name}");
        writer.write_all(b"a3 LOGOUT\r\n").unwrap();
        assert!(line().starts_with("* BYE"));

        // Port 993: a real client.
        let (port, _stop) = listen(&instance, true);
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/clients/imap_client.py");
        let out = std::process::Command::new("python3")
            .args([
                "-I",
                script,
                &port.to_string(),
                cafile.to_str().unwrap(),
                "olena@ministry.example",
                &password,
            ])
            .output()
            .expect("python3");
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.status.success() && said.contains("all steps passed"),
            "{name}: {said}"
        );

        // What the client did is what the store holds.
        let boxes = block_on(store::mailboxes(instance.store.as_ref(), &olena)).unwrap();
        let count = |role: &str| {
            let m = boxes
                .iter()
                .find(|m| m.role.as_deref() == Some(role))
                .unwrap();
            block_on(store::entries(instance.store.as_ref(), &m.id, 0, 100))
                .unwrap()
                .len()
        };
        assert_eq!(
            (1, 1, 1),
            (count("inbox"), count("archive"), count("trash")),
            "{name}"
        );
        assert!(
            !boxes.iter().any(|m| m.name.starts_with("Projects")),
            "{name}"
        );
    }
}

#[test]
fn one_address_cannot_take_every_imap_connection() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_name, instance, _keep) = common::installations().remove(0);
    let (port, _stop) = listen(&instance, false);
    let greeted = |s: &TcpStream| {
        s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(s)
            .read_line(&mut line)
            .map(|_| line)
            .unwrap_or_default()
    };
    // Up to the cap, each connection is greeted.
    let mut held = Vec::new();
    for _ in 0..imap::PER_ADDRESS {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        assert!(greeted(&s).starts_with("* OK"));
        held.push(s);
    }
    // The next one from the same address is closed unanswered.
    let extra = TcpStream::connect(("127.0.0.1", port)).unwrap();
    assert_eq!("", greeted(&extra));
    // One goes, and another may come.
    drop(held.pop());
    std::thread::sleep(std::time::Duration::from_millis(300));
    let again = TcpStream::connect(("127.0.0.1", port)).unwrap();
    assert!(greeted(&again).starts_with("* OK"));
}
