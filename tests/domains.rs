//! Mail domains through the API on every installation shape: adding one
//! makes its records and DKIM keys, the checks read what a (test) DNS
//! server says, and a fixed record turns green.

mod common;

use std::sync::Arc;

use common::dns::Dns;
use hickory_proto::rr::RecordType;
use ortus_core::auth::accounts::Role;
use ortus_core::instance::testing_auth::signed_in;
use rust_web_server::test_client::TestClient;
use serde_json::{json, Value};

fn status_of(shown: &Value, purpose: &str) -> String {
    shown["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["purpose"] == purpose)
        .unwrap()["status"]
        .as_str()
        .unwrap()
        .to_string()
}

fn value_of(shown: &Value, purpose: &str) -> String {
    shown["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["purpose"] == purpose)
        .unwrap()["value"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn domains_are_added_checked_and_fixed() {
    let dns = Dns::start();
    // SAFETY of the test: one test in this binary sets these variables.
    unsafe {
        std::env::set_var("ORTUS_DNS_SERVERS", dns.addr.to_string());
        std::env::set_var("ORTUS_MAIL_HOSTNAME", "mail.ministry.example");
    }
    for (name, instance, _dir) in common::installations() {
        // Each shape its own domain: the test DNS zone is shared.
        let domain = format!("{}.example", name.replace('+', "-"));
        let d = domain.as_str();
        let client = TestClient::new(ortus_core::app(Arc::clone(&instance)));
        let owner = signed_in(
            &instance,
            &format!("taras-{name}@ministry.example"),
            Role::Owner,
        );
        let admin = signed_in(
            &instance,
            &format!("marek-{name}@ministry.example"),
            Role::Admin,
        );
        let send = |method: &str, path: &str, token: &str, body: Value| {
            let request = match method {
                "GET" => client.get(path),
                "DELETE" => client.delete(path),
                _ => client.post(path),
            };
            request
                .header("Authorization", &format!("Bearer {token}"))
                .header("X-Requested-With", "ortus")
                .body_text(&body.to_string())
                .send()
        };
        let read = |method: &str, path: &str, token: &str, body: Value| -> Value {
            let response = send(method, path, token, body);
            assert_eq!(
                200,
                response.status(),
                "{name}: {method} {path}: {}",
                response.body_text()
            );
            serde_json::from_str(response.body_text()).unwrap()
        };

        assert_eq!(
            403,
            send(
                "POST",
                "/api/v1/mail/admin/domains",
                &admin,
                json!({"domain": d})
            )
            .status(),
            "{name}: owners add domains"
        );
        assert_eq!(
            400,
            send(
                "POST",
                "/api/v1/mail/admin/domains",
                &owner,
                json!({"domain": "not a domain"})
            )
            .status()
        );
        let added = read(
            "POST",
            "/api/v1/mail/admin/domains",
            &owner,
            json!({"domain": format!("{}.", d.to_uppercase())}),
        );
        assert_eq!(d, added["domain"]);
        assert_eq!(
            409,
            send(
                "POST",
                "/api/v1/mail/admin/domains",
                &owner,
                json!({"domain": d})
            )
            .status()
        );
        assert_eq!("10 mail.ministry.example.", value_of(&added, "receive"));
        let dkim = value_of(&added, "dkim_rsa");
        assert!(dkim.starts_with("v=DKIM1; k=rsa; p=MII"), "{dkim}");
        // The private keys are sealed at rest.
        let stored = ortus_core::runtime::block_on(
            instance
                .store
                .query("SELECT dkim_key FROM mail_domains", &[]),
        )
        .unwrap();
        assert!(stored[0].text(0).unwrap().contains("enc:v1:"), "{name}");

        // Nothing in DNS yet: waiting.
        let checked = read(
            "POST",
            &format!("/api/v1/mail/admin/domains/{d}/check"),
            &admin,
            Value::Null,
        );
        assert_eq!("waiting", checked["state"]);
        assert_eq!("missing", status_of(&checked, "receive"));

        // MX, SPF and DKIM right, DMARC at p=none: verified, DMARC lenient.
        dns.mx(d, 10, "mail.ministry.example.");
        dns.txt(d, &["google-site-verification=abc", "v=spf1 mx -all"]);
        let selector = added["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["purpose"] == "dkim_rsa")
            .unwrap()["name"]
            .as_str()
            .unwrap()
            .to_string();
        dns.txt(&format!("{selector}.{d}"), &[&dkim]);
        dns.txt(
            &format!("_dmarc.{d}"),
            &["v=DMARC1; p=none; rua=mailto:dmarc-reports@ministry.example"],
        );
        dns.cname(&format!("autoconfig.{d}"), "other.example.");
        let checked = read(
            "POST",
            &format!("/api/v1/mail/admin/domains/{d}/check"),
            &admin,
            Value::Null,
        );
        assert_eq!("needs_fix", checked["state"], "{name}: {checked}");
        assert_eq!(
            ("ok", "ok", "ok", "lenient", "different"),
            (
                status_of(&checked, "receive").as_str(),
                status_of(&checked, "spf").as_str(),
                status_of(&checked, "dkim_rsa").as_str(),
                status_of(&checked, "dmarc").as_str(),
                status_of(&checked, "autoconfig").as_str()
            )
        );
        let listed = read("GET", "/api/v1/mail/admin/domains", &admin, Value::Null);
        assert_eq!(
            1, listed["domains"][0]["fixes"],
            "{name}: one fix, the DMARC policy"
        );

        // DMARC to quarantine: verified.
        dns.txt(
            &format!("_dmarc.{d}"),
            &["v=DMARC1; p=quarantine; rua=mailto:dmarc-reports@ministry.example"],
        );
        let checked = read(
            "POST",
            &format!("/api/v1/mail/admin/domains/{d}/check"),
            &admin,
            Value::Null,
        );
        assert_eq!("verified", checked["state"], "{name}");

        // The zone file to paste elsewhere.
        let zone = send(
            "GET",
            &format!("/api/v1/mail/admin/domains/{d}/zone"),
            &admin,
            Value::Null,
        );
        assert!(zone
            .header("content-disposition")
            .unwrap_or_default()
            .contains(&format!("{d}.zone")));
        assert!(
            zone.body_text()
                .contains("@\t3600\tIN\tMX\t10 mail.ministry.example."),
            "{}",
            zone.body_text()
        );

        // The periodic check runs for every domain.
        dns.clear(d, RecordType::MX);
        let worker = ortus_core::program::AppWorker::new(Arc::clone(&instance), "test");
        while ortus_core::runtime::block_on(worker.tick(ortus_core::program::now_ms())).unwrap() {}
        // The check runs on its own once the job has started it.
        let mut state = String::new();
        for _ in 0..100 {
            state = read(
                "GET",
                &format!("/api/v1/mail/admin/domains/{d}"),
                &admin,
                Value::Null,
            )["state"]
                .as_str()
                .unwrap()
                .to_string();
            if state == "needs_fix" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!("needs_fix", state, "{name}: the MX went away");
        dns.mx(d, 10, "mail.ministry.example.");

        assert_eq!(
            200,
            send(
                "DELETE",
                &format!("/api/v1/mail/admin/domains/{d}"),
                &owner,
                Value::Null
            )
            .status()
        );
    }
}

#[test]
fn the_admin_console_learns_which_addresses_are_mailboxes_here() {
    for (name, instance, _dir) in common::installations() {
        let domain = format!("ours-{}.example", name.replace('+', "-"));
        ortus_core::runtime::block_on(ortus_mail::domains::add(&instance, &domain)).unwrap();
        let ours = ortus_core::runtime::block_on(
            instance.service::<dyn ortus_core::interfaces::MailDomains>(),
        )
        .expect("the mail app provides MailDomains");
        assert!(
            ortus_core::runtime::block_on(ours.is_ours(&domain)),
            "{name}"
        );
        assert!(
            !ortus_core::runtime::block_on(ours.is_ours("elsewhere.example")),
            "{name}"
        );
    }
}
