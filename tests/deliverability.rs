//! Deliverability on both installation shapes: a DMARC aggregate report
//! (gzip) arrives by mail at dmarc-reports@ and is read, not filed; the
//! summary counts sent, delivered, bounced and the queue, by provider and
//! by day, and says who sends as our domain.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::Arc;

use base64::Engine;
use common::dns::Dns;
use ortus_core::auth::accounts::Role;
use ortus_core::instance::testing_auth::signed_in;
use ortus_core::runtime::block_on;
use ortus_mail::{reports, smtp, store};
use serde_json::Value;

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

const XML: &str = r#"<?xml version="1.0"?><feedback><report_metadata><org_name>google.com</org_name><report_id>rep-1</report_id><date_range><begin>BEGIN</begin><end>END</end></date_range></report_metadata><policy_published><domain>ministry.example</domain><p>none</p></policy_published><record><row><source_ip>192.0.2.10</source_ip><count>40</count><policy_evaluated><disposition>none</disposition><dkim>pass</dkim><spf>pass</spf></policy_evaluated></row><identifiers><header_from>ministry.example</header_from></identifiers></record><record><row><source_ip>198.51.100.4</source_ip><count>5</count><policy_evaluated><disposition>none</disposition><dkim>fail</dkim><spf>pass</spf></policy_evaluated></row><identifiers><header_from>ministry.example</header_from></identifiers></record><record><row><source_ip>203.0.113.7</source_ip><count>3</count><policy_evaluated><disposition>none</disposition><dkim>fail</dkim><spf>fail</spf></policy_evaluated></row><identifiers><header_from>ministry.example</header_from></identifiers></record></feedback>"#;

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

#[test]
fn reports_and_outbound_numbers() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dns = Dns::start();
    dns.a("mail.ministry.example", "192.0.2.10".parse().unwrap());
    // Reports are read only from who they say they are from.
    dns.txt("google.com", &["v=spf1 ip4:127.0.0.1 -all"]);
    dns.txt("_dmarc.google.com", &["v=DMARC1; p=reject"]);
    unsafe {
        std::env::set_var("ORTUS_DNS_SERVERS", dns.addr.to_string());
        std::env::set_var("ORTUS_MAIL_HOSTNAME", "mail.ministry.example");
    }
    let now = ortus_core::program::now_ms();
    let xml = XML
        .replace("BEGIN", &(now / 1000 - 86_400).to_string())
        .replace("END", &(now / 1000).to_string());
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(xml.as_bytes()).unwrap();
    let attachment = base64::engine::general_purpose::STANDARD.encode(gz.finish().unwrap());
    let report = format!(
        "From: noreply-dmarc-support@google.com\r\nTo: dmarc-reports@ministry.example\r\nSubject: Report domain: ministry.example\r\nMessage-ID: <r1@google.com>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nThis is an aggregate report.\r\n--b\r\nContent-Type: application/gzip; name=\"google.com!ministry.example!1!2.xml.gz\"\r\nContent-Disposition: attachment\r\nContent-Transfer-Encoding: base64\r\n\r\n{attachment}\r\n--b--\r\n"
    );
    for (name, instance, _keep) in common::installations() {
        let admin = signed_in(&instance, "taras@ministry.example", Role::Owner);
        let member = signed_in(&instance, "olena@ministry.example", Role::Member);
        block_on(ortus_mail::domains::add(&instance, "ministry.example")).unwrap();

        let listener =
            block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await }).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
        let server = Arc::clone(&instance);
        ortus_core::runtime::io()
            .spawn(async move { smtp::serve(server, listener, &mut stop).await.unwrap() });
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        let mut greeting = String::new();
        reader.read_line(&mut greeting).unwrap();
        assert!(talk(&mut reader, &mut writer, "EHLO google.com").starts_with("250"));
        assert!(talk(
            &mut reader,
            &mut writer,
            "MAIL FROM:<noreply-dmarc-support@google.com>"
        )
        .starts_with("250"));
        assert!(
            talk(
                &mut reader,
                &mut writer,
                "RCPT TO:<dmarc-reports@ministry.example>"
            )
            .starts_with("250"),
            "{name}: the report address takes mail"
        );
        assert!(talk(&mut reader, &mut writer, "DATA").starts_with("354"));
        writer.write_all(report.as_bytes()).unwrap();
        let reply = talk(&mut reader, &mut writer, ".");
        assert!(reply.starts_with("250"), "{name}: {reply}");
        // It is read after the reply.
        let count = || {
            block_on(instance.store.query(
                "SELECT CAST(COUNT(*) AS BIGINT) FROM mail_dmarc_reports",
                &[],
            ))
            .unwrap()[0]
                .int(0)
                .unwrap()
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while count() == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // The same report again is not counted twice.
        assert_eq!(
            0,
            block_on(reports::take(&instance, report.as_bytes(), "google.com")).unwrap()
        );
        assert_eq!(
            1,
            block_on(instance.store.query(
                "SELECT CAST(COUNT(*) AS BIGINT) FROM mail_dmarc_reports",
                &[]
            ))
            .unwrap()[0]
                .int(0)
                .unwrap()
        );
        assert_eq!(
            0,
            block_on(
                instance
                    .store
                    .query("SELECT CAST(COUNT(*) AS BIGINT) FROM mail_messages", &[])
            )
            .unwrap()[0]
                .int(0)
                .unwrap(),
            "{name}: not filed as mail"
        );

        // Outbound: three delivered (two at Gmail), one bounced at a
        // company, one waiting; one from before the period.
        let rows: [(&str, Option<&str>, i64, i64); 6] = [
            (
                "sent",
                Some("alt1.gmail-smtp-in.l.google.com"),
                now - 3_600_000,
                1,
            ),
            (
                "sent",
                Some("alt2.gmail-smtp-in.l.google.com"),
                now - 2 * 86_400_000,
                1,
            ),
            ("sent", Some("mx.regional.example"), now - 86_400_000, 1),
            ("bounced", Some("mx.regional.example"), now - 86_400_000, 1),
            ("deferred", None, now - 600_000, 2),
            (
                "sent",
                Some("mx.regional.example"),
                now - 20 * 86_400_000,
                1,
            ),
        ];
        for (i, (state, mx, at, attempts)) in rows.iter().enumerate() {
            block_on(instance.store.execute(
                "INSERT INTO mail_outbound (id, user_id, message, recipient, domain, state, attempts, created_at, updated_at, mx) VALUES ($1, 'u', 'm', 'r', 'd', $2, $3, $4, $4, $5)",
                &[format!("o{i}").into(), (*state).into(), (*attempts).into(), (*at).into(), mx.map(str::to_string).into()],
            ))
            .unwrap();
        }
        let client =
            rust_web_server::test_client::TestClient::new(ortus_core::app(Arc::clone(&instance)));
        assert_eq!(
            403,
            client
                .get("/api/v1/mail/admin/deliverability")
                .header("Authorization", &format!("Bearer {member}"))
                .send()
                .status(),
            "{name}: admins only"
        );
        let summary: Value = serde_json::from_str(
            client
                .get("/api/v1/mail/admin/deliverability?days=14")
                .header("Authorization", &format!("Bearer {admin}"))
                .send()
                .body_text(),
        )
        .unwrap();
        assert_eq!(5, summary["sent"], "{name}: {summary}");
        assert_eq!(75.0, summary["delivered"], "{name}: three of four finished");
        assert_eq!(25.0, summary["bounced"]);
        assert_eq!(1, summary["queue"]["count"]);
        assert_eq!(1, summary["queue"]["retrying"]);
        assert_eq!(14, summary["per_day"].as_array().unwrap().len());
        let gmail = summary["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "gmail")
            .unwrap();
        assert_eq!(100.0, gmail["delivered"], "{name}: {summary}");
        let corporate = summary["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "corporate")
            .unwrap();
        assert_eq!(50.0, corporate["delivered"]);
        let groups = summary["senders"][0]["groups"].as_array().unwrap();
        assert_eq!("ministry.example", summary["senders"][0]["domain"]);
        assert!(
            groups[0]["ours"] == true && groups[0]["count"] == 40 && groups[0]["status"] == "pass",
            "{name}: {groups:?}"
        );
        assert!(
            groups
                .iter()
                .any(|g| g["count"] == 5 && g["status"] == "dkim_fail"),
            "{name}: {groups:?}"
        );
        assert!(
            groups
                .iter()
                .any(|g| g["count"] == 3 && g["status"] == "spoofing"),
            "{name}: {groups:?}"
        );
        let day: Value = serde_json::from_str(
            client
                .get("/api/v1/mail/admin/deliverability?days=1")
                .header("Authorization", &format!("Bearer {admin}"))
                .send()
                .body_text(),
        )
        .unwrap();
        assert_eq!(24, day["per_day"].as_array().unwrap().len());
        assert_eq!(2, day["sent"], "{name}: the last 24 hours: {day}");
        let _ = store::DELETE_FILES_JOB;
    }
}
