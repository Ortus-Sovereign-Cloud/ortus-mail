//! Mailbox import over IMAP, on both installation shapes: an older
//! installation plays the old server (IMAP with TLS); folders map to ours,
//! flags come along, a wrong password fails and is fixed and retried, and
//! a second run copies only what is new.

mod common;

use std::sync::Arc;

use ortus_core::auth::accounts::Role;
use ortus_core::instance::testing_auth::signed_in;
use ortus_core::jobs::Job;
use ortus_core::runtime::block_on;
use ortus_mail::{imap, import, store};
use rust_web_server::test_client::TestClient;
use serde_json::{json, Value};

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn message(subject: &str) -> Vec<u8> {
    format!("From: Anna Kowalska <anna@regional.example>\r\nTo: olena@old.example\r\nSubject: {subject}\r\nMessage-ID: <{subject}@regional.example>\r\nDate: Thu, 19 Nov 2026 08:05:00 +0000\r\n\r\nAbout {subject}.\r\n").into_bytes()
}

fn app_password(instance: &Arc<ortus_core::instance::Instance>, token: &str) -> String {
    let client = TestClient::new(ortus_core::app(Arc::clone(instance)));
    let made: Value = serde_json::from_str(
        client
            .post("/api/v1/me/app-passwords")
            .header("Authorization", &format!("Bearer {token}"))
            .header("X-Requested-With", "ortus")
            .body_text(r#"{"name":"Migration"}"#)
            .send()
            .body_text(),
    )
    .unwrap();
    made["password"].as_str().unwrap().to_string()
}

fn run_queued(instance: &Arc<ortus_core::instance::Instance>) {
    let jobs = block_on(instance.store.query(
        "SELECT id, payload FROM ortus_jobs WHERE kind = $1 AND state = 'queued'",
        &[import::IMPORT_JOB.into()],
    ))
    .unwrap();
    for j in jobs {
        let job = Job {
            id: j.text(0).unwrap().into(),
            kind: import::IMPORT_JOB.into(),
            payload: j.text(1).unwrap().into(),
            attempt: 1,
            max_attempts: 1,
        };
        block_on(import::run(instance, &job)).unwrap();
        block_on(instance.store.execute(
            "UPDATE ortus_jobs SET state = 'done' WHERE id = $1",
            &[job.id.as_str().into()],
        ))
        .unwrap();
    }
    // The copying goes on in the background: wait until it settles.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        let busy = block_on(instance.store.query(
            "SELECT id FROM mail_import_accounts WHERE state IN ('queued', 'running')",
            &[],
        ))
        .unwrap();
        if busy.is_empty() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("the import did not finish");
}

#[test]
fn mailboxes_come_over_from_the_old_server() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = common::dir();
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let ca = dir.0.join("cert.pem");
    std::fs::write(&ca, certified.cert.pem()).unwrap();
    std::fs::write(dir.0.join("key.pem"), certified.signing_key.serialize_pem()).unwrap();
    unsafe {
        std::env::set_var("ORTUS_MAIL_TLS_CERT", &ca);
        std::env::set_var("ORTUS_MAIL_TLS_KEY", dir.0.join("key.pem"));
        std::env::set_var("ORTUS_IMPORT_CA", &ca);
    }
    for (name, instance, _keep) in common::installations() {
        // The old server: another installation, with Olena's and Marek's mail.
        let old = ortus_core::instance::testing::instance(ortus_mail::apps());
        let olena_old = signed_in(&old, "olena@old.example", Role::Member);
        let marek_old = signed_in(&old, "marek@old.example", Role::Member);
        let id_old = |email: &str| {
            block_on(ortus_core::auth::accounts::by_email(
                old.store.as_ref(),
                email,
            ))
            .unwrap()
            .unwrap()
            .id
        };
        let (olena_id, marek_id) = (id_old("olena@old.example"), id_old("marek@old.example"));
        let boxes = |user: &str| block_on(store::mailboxes(old.store.as_ref(), user)).unwrap();
        let role = |user: &str, r: &str| {
            boxes(user)
                .into_iter()
                .find(|m| m.role.as_deref() == Some(r))
                .unwrap()
                .id
        };
        block_on(store::deliver(
            &old,
            &olena_id,
            &role(&olena_id, "inbox"),
            &message("Gauge report"),
            &["\\Seen", "flood"],
            None,
            0,
        ))
        .unwrap();
        block_on(store::deliver(
            &old,
            &olena_id,
            &role(&olena_id, "inbox"),
            &message("Shelter agreement"),
            &[],
            None,
            0,
        ))
        .unwrap();
        block_on(store::deliver(
            &old,
            &olena_id,
            &role(&olena_id, "sent"),
            &message("Re: Gauge report"),
            &["\\Seen"],
            None,
            0,
        ))
        .unwrap();
        let projects = block_on(store::create_mailbox(
            old.store.as_ref(),
            &olena_id,
            "Projects/2026",
        ))
        .unwrap()
        .unwrap();
        block_on(store::deliver(
            &old,
            &olena_id,
            &projects.id,
            &message("Bus plan"),
            &["\\Flagged"],
            None,
            0,
        ))
        .unwrap();
        block_on(store::deliver(
            &old,
            &marek_id,
            &role(&marek_id, "inbox"),
            &message("Escort schedule"),
            &[],
            None,
            0,
        ))
        .unwrap();
        let (olena_pw, marek_pw) = (
            app_password(&old, &olena_old),
            app_password(&old, &marek_old),
        );
        let listener =
            block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await }).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
        let server = Arc::clone(&old);
        ortus_core::runtime::io().spawn(async move {
            imap::serve(server, listener, &mut stop, true)
                .await
                .unwrap()
        });

        // The new installation.
        let admin = signed_in(&instance, "taras@ministry.example", Role::Owner);
        let olena_token = signed_in(&instance, "olena@ministry.example", Role::Member);
        let marek_token = signed_in(&instance, "marek@ministry.example", Role::Member);
        let id_new = |email: &str| {
            block_on(ortus_core::auth::accounts::by_email(
                instance.store.as_ref(),
                email,
            ))
            .unwrap()
            .unwrap()
            .id
        };
        let (olena, marek) = (
            id_new("olena@ministry.example"),
            id_new("marek@ministry.example"),
        );
        let client = TestClient::new(ortus_core::app(Arc::clone(&instance)));
        let post = |path: &str, body: Value| {
            client
                .post(path)
                .header("Authorization", &format!("Bearer {admin}"))
                .header("X-Requested-With", "ortus")
                .body_text(&body.to_string())
                .send()
        };
        let refused = post(
            "/api/v1/mail/admin/imports",
            json!({"host": "localhost", "port": port, "security": "none", "accounts": [{"login": "x", "password": "y", "to": "olena@ministry.example"}]}),
        );
        assert_eq!(400, refused.status(), "{name}: no passwords in the clear");
        let started = post(
            "/api/v1/mail/admin/imports",
            json!({"host": "localhost", "port": port, "security": "tls", "accounts": [
                {"login": "olena@old.example", "password": olena_pw, "to": "olena@ministry.example"},
                {"login": "marek@old.example", "password": "aaaa-bbbb-cccc-dddd", "to": "marek@ministry.example"},
            ]}),
        );
        assert_eq!(200, started.status(), "{name}: {}", started.body_text());
        let stored = block_on(
            instance
                .store
                .query("SELECT password FROM mail_import_accounts", &[]),
        )
        .unwrap();
        assert!(
            stored
                .iter()
                .all(|r| r.text(0).unwrap().starts_with("enc:v1:")),
            "{name}: passwords are sealed"
        );
        run_queued(&instance);

        let count = |user: &str, mailbox: &str| {
            block_on(store::entries(instance.store.as_ref(), mailbox, 0, 100))
                .unwrap()
                .into_iter()
                .filter(|_| !user.is_empty())
                .collect::<Vec<_>>()
        };
        let mine = |user: &str| block_on(store::mailboxes(instance.store.as_ref(), user)).unwrap();
        let by_role = |user: &str, r: &str| {
            mine(user)
                .into_iter()
                .find(|m| m.role.as_deref() == Some(r))
                .unwrap()
                .id
        };
        let inbox = count(&olena, &by_role(&olena, "inbox"));
        assert_eq!(2, inbox.len(), "{name}: {inbox:?}");
        let gauge = inbox.iter().find(|e| e.subject == "Gauge report").unwrap();
        assert!(
            gauge.flags.contains(&"\\Seen".to_string())
                && gauge.flags.contains(&"flood".to_string()),
            "{name}: flags come along: {:?}",
            gauge.flags
        );
        assert_eq!(
            1,
            count(&olena, &by_role(&olena, "sent")).len(),
            "{name}: Sent maps to Sent"
        );
        let own = mine(&olena)
            .into_iter()
            .find(|m| m.name == "Projects/2026")
            .expect("own folder");
        let bus = count(&olena, &own.id);
        assert!(
            bus.len() == 1 && bus[0].flags.contains(&"\\Flagged".to_string()),
            "{name}: {bus:?}"
        );

        let listed: Value = serde_json::from_str(
            client
                .get("/api/v1/mail/admin/imports")
                .header("Authorization", &format!("Bearer {admin}"))
                .send()
                .body_text(),
        )
        .unwrap();
        let accounts = listed["imports"][0]["accounts"].as_array().unwrap().clone();
        let marek_row = accounts
            .iter()
            .find(|a| a["login"] == "marek@old.example")
            .unwrap();
        assert_eq!("failed", marek_row["state"], "{name}: {accounts:?}");
        assert_eq!("login_failed", marek_row["error"]);
        let olena_row = accounts
            .iter()
            .find(|a| a["login"] == "olena@old.example")
            .unwrap();
        assert!(
            olena_row["state"] == "done" && olena_row["copied"] == 4,
            "{name}: {olena_row}"
        );
        assert!(
            block_on(instance.store.query(
                "SELECT password FROM mail_import_accounts WHERE login = 'olena@old.example'",
                &[]
            ))
            .unwrap()[0]
                .text(0)
                .is_none(),
            "{name}: the password is forgotten once done"
        );

        // The administrator asks Marek; he gives the old password himself.
        let marek_account = marek_row["id"].as_str().unwrap();
        assert_eq!(
            409,
            post(
                &format!(
                    "/api/v1/mail/admin/imports/accounts/{}/retry",
                    olena_row["id"].as_str().unwrap()
                ),
                json!({})
            )
            .status()
        );
        let asked: Value = serde_json::from_str(
            post(
                "/api/v1/mail/admin/imports/ask-owners",
                json!({"accounts": [marek_account]}),
            )
            .body_text(),
        )
        .unwrap();
        assert_eq!(1, asked["asked"], "{name}");
        let note = count(&marek, &by_role(&marek, "inbox"));
        assert!(
            note.len() == 1 && note[0].subject == "Your old mailbox needs your password",
            "{name}: {note:?}"
        );
        let raw = String::from_utf8(
            block_on(store::raw(&instance, &marek, &note[0].message))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(
            raw.contains(&format!("/mail/reconnect/{marek_account}")),
            "{name}: {raw}"
        );
        let own = |path: &str, body: Value, token: &str| {
            client
                .post(path)
                .header("Authorization", &format!("Bearer {token}"))
                .header("X-Requested-With", "ortus")
                .body_text(&body.to_string())
                .send()
        };
        let mine: Value = serde_json::from_str(
            client
                .get("/api/v1/mail/imports/mine")
                .header("Authorization", &format!("Bearer {marek_token}"))
                .send()
                .body_text(),
        )
        .unwrap();
        assert_eq!(
            "login_failed", mine["accounts"][0]["error"],
            "{name}: {mine}"
        );
        assert_eq!(
            404,
            own(
                &format!("/api/v1/mail/imports/mine/{marek_account}/retry"),
                json!({"password": "x"}),
                &olena_token
            )
            .status(),
            "{name}: only the owner"
        );
        assert_eq!(
            200,
            own(
                &format!("/api/v1/mail/imports/mine/{marek_account}/retry"),
                json!({"password": marek_pw}),
                &marek_token
            )
            .status()
        );
        run_queued(&instance);
        assert_eq!(
            2,
            count(&marek, &by_role(&marek, "inbox")).len(),
            "{name}: Marek's mail came after the retry"
        );

        // A later run copies only what is new.
        block_on(store::deliver(
            &old,
            &olena_id,
            &role(&olena_id, "inbox"),
            &message("Town hall"),
            &[],
            None,
            0,
        ))
        .unwrap();
        block_on(instance.store.execute("UPDATE mail_import_accounts SET state = 'failed', password = $1 WHERE login = 'olena@old.example'", &[instance.keys.seal_text(&olena_pw).into()])).unwrap();
        let olena_account = olena_row["id"].as_str().unwrap();
        assert_eq!(
            200,
            post(
                &format!("/api/v1/mail/admin/imports/accounts/{olena_account}/retry"),
                json!({})
            )
            .status()
        );
        run_queued(&instance);
        assert_eq!(
            3,
            count(&olena, &by_role(&olena, "inbox")).len(),
            "{name}: no duplicates"
        );
    }
}
