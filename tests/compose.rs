//! What Compose needs, on both installation shapes: labels and people to
//! choose from, the policy asked without recording, attachments with
//! names in any language, send later and undo, and snooze.

mod common;

use std::sync::Arc;

use common::dns::Dns;
use ortus_core::auth::accounts::Role;
use ortus_core::instance::testing_auth::signed_in;
use ortus_core::jobs::Job;
use ortus_core::runtime::block_on;
use ortus_mail::{later, store};
use rust_web_server::test_client::TestClient;
use serde_json::{json, Value};

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

const USING: [&str; 3] = [
    "urn:ietf:params:jmap:core",
    "urn:ietf:params:jmap:mail",
    "urn:ietf:params:jmap:submission",
];

fn in_seconds(s: i64) -> String {
    let t = ortus_core::program::now_ms() / 1000 + s;
    let d = mail_parser::DateTime::from_timestamp(t);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        d.year, d.month, d.day, d.hour, d.minute, d.second
    )
}

#[test]
fn compose_sends_later_undoes_attaches_and_snoozes() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dns = Dns::start();
    unsafe {
        std::env::set_var("ORTUS_DNS_SERVERS", dns.addr.to_string());
        std::env::set_var("ORTUS_MAIL_HOSTNAME", "mail.ministry.example");
    }
    for (name, instance, _keep) in common::installations() {
        let token = signed_in(&instance, "olena@ministry.example", Role::Member);
        signed_in(&instance, "sam@ministry.example", Role::Member);
        let id_of = |email: &str| {
            block_on(ortus_core::auth::accounts::by_email(
                instance.store.as_ref(),
                email,
            ))
            .unwrap()
            .unwrap()
            .id
        };
        let (olena, sam) = (
            id_of("olena@ministry.example"),
            id_of("sam@ministry.example"),
        );
        block_on(ortus_mail::domains::add(&instance, "ministry.example")).unwrap();
        let inbox = block_on(store::by_role(instance.store.as_ref(), &olena, "inbox")).unwrap();
        let maria = block_on(store::deliver(&instance, &olena, &inbox.id, b"From: Maria Peters <maria@mail.example>\r\nTo: olena@ministry.example\r\nSubject: Car\r\nMessage-ID: <m@mail.example>\r\n\r\nDamaged.\r\n", &[], None, 0)).unwrap();
        let client = TestClient::new(ortus_core::app(Arc::clone(&instance)));
        let get = |path: &str| -> Value {
            serde_json::from_str(
                client
                    .get(path)
                    .header("Authorization", &format!("Bearer {token}"))
                    .send()
                    .body_text(),
            )
            .unwrap()
        };
        let post = |path: &str, body: &str| {
            client
                .post(path)
                .header("Authorization", &format!("Bearer {token}"))
                .header("X-Requested-With", "ortus")
                .body_text(body)
                .send()
        };
        let jmap = |calls: Value| -> Vec<Value> {
            let r = post(
                "/api/v1/mail/web/jmap",
                &json!({"using": USING, "methodCalls": calls}).to_string(),
            );
            assert_eq!(200, r.status(), "{name}: {}", r.body_text());
            serde_json::from_str::<Value>(r.body_text()).unwrap()["methodResponses"]
                .as_array()
                .unwrap()
                .clone()
        };

        // Labels and people.
        let labels = get("/api/v1/mail/labels");
        assert!(
            labels["labels"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l["id"] == "internal"),
            "{name}: {labels}"
        );
        let people = get("/api/v1/mail/people?q=sam");
        assert_eq!(
            json!([{"name": "sam", "email": "sam@ministry.example", "here": true}])
                .to_string()
                .contains("sam@ministry.example"),
            people.to_string().contains("sam@ministry.example"),
            "{name}: {people}"
        );
        assert_eq!(true, people["people"][0]["here"], "{name}: {people}");
        // Someone invited (not yet joined) is suggested: new colleagues can
        // be written to before their first day; someone suspended is not.
        signed_in(&instance, "iryna@ministry.example", Role::Member);
        signed_in(&instance, "petro@ministry.example", Role::Member);
        block_on(instance.store.execute(
            "UPDATE core_users SET state = 'invited' WHERE email = 'iryna@ministry.example'",
            &[],
        ))
        .unwrap();
        block_on(instance.store.execute(
            "UPDATE core_users SET state = 'suspended' WHERE email = 'petro@ministry.example'",
            &[],
        ))
        .unwrap();
        assert!(
            get("/api/v1/mail/people?q=iryna")
                .to_string()
                .contains("iryna@ministry.example"),
            "{name}"
        );
        assert!(
            !get("/api/v1/mail/people?q=petro")
                .to_string()
                .contains("petro@ministry.example"),
            "{name}"
        );
        let people = get("/api/v1/mail/people?q=maria");
        assert_eq!(
            "maria@mail.example", people["people"][0]["email"],
            "{name}: {people}"
        );
        assert_eq!(false, people["people"][0]["here"]);

        // The policy, asked while writing: nothing is recorded.
        let audit_before = block_on(
            instance
                .store
                .query("SELECT CAST(COUNT(*) AS BIGINT) FROM core_audit", &[]),
        )
        .unwrap()[0]
            .int(0)
            .unwrap();
        let check = |body: Value| -> Value {
            serde_json::from_str(post("/api/v1/mail/check", &body.to_string()).body_text()).unwrap()
        };
        assert_eq!(
            json!({"ok": true}),
            check(json!({"from": "olena@ministry.example", "to": ["maria@mail.example"]})),
            "{name}: unlabelled mail goes"
        );
        let needs = check(
            json!({"from": "olena@ministry.example", "to": ["maria@mail.example"], "label": "internal"}),
        );
        assert_eq!("needs_reason", needs["problem"], "{name}: {needs}");
        assert_eq!(
            json!({"ok": true}),
            check(
                json!({"from": "olena@ministry.example", "to": ["maria@mail.example"], "label": "internal", "reason": "She reported the damage"})
            )
        );
        assert_eq!(
            json!({"ok": true}),
            check(
                json!({"from": "olena@ministry.example", "to": ["sam@ministry.example"], "label": "internal"})
            ),
            "{name}: colleagues always"
        );
        assert_eq!(
            "not_your_address",
            check(json!({"from": "sam@ministry.example", "to": ["maria@mail.example"]}))["problem"]
        );
        let audit_after = block_on(
            instance
                .store
                .query("SELECT CAST(COUNT(*) AS BIGINT) FROM core_audit", &[]),
        )
        .unwrap()[0]
            .int(0)
            .unwrap();
        assert_eq!(
            audit_before, audit_after,
            "{name}: checking records nothing"
        );

        // An attachment with a name in Ukrainian, a subject in Polish.
        let upload: Value = serde_json::from_str(
            post("/api/v1/mail/web/upload", "line,amount\nroads,120\n").body_text(),
        )
        .unwrap();
        let blob = upload["blobId"].as_str().unwrap();
        let drafts = block_on(store::by_role(instance.store.as_ref(), &olena, "drafts"))
            .unwrap()
            .id;
        let r = jmap(json!([
            ["Email/set", {"create": {"d": {"mailboxIds": {&drafts: true}, "keywords": {"$draft": true}, "from": [{"name": "Olena Hrytsenko", "email": "olena@ministry.example"}], "to": [{"name": "Sam", "email": "sam@ministry.example"}], "subject": "Zażółć gęślą jaźń", "textBody": [{"partId": "1", "type": "text/plain"}], "bodyValues": {"1": {"value": "Numbers attached."}}, "attachments": [{"blobId": blob, "type": "text/csv", "name": "бюджет.csv"}]}}}, "0"],
        ]));
        let draft = r[0][1]["created"]["d"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: {:?}", r[0]))
            .to_string();
        let r = jmap(
            json!([["Email/get", {"ids": [&draft], "properties": ["subject", "attachments", "ortusText"]}, "1"]]),
        );
        let got = &r[0][1]["list"][0];
        assert_eq!("Zażółć gęślą jaźń", got["subject"], "{name}: {got}");
        assert_eq!("бюджет.csv", got["attachments"][0]["name"], "{name}: {got}");
        assert_eq!("text/csv", got["attachments"][0]["type"]);

        // Sent with ten seconds to undo, then undone: nothing goes, the draft stays.
        let r = jmap(
            json!([["Identity/get", {}, "i"], ["EmailSubmission/set", {"create": {"s": {"identityId": "olena@ministry.example", "emailId": draft, "ortusSendAt": in_seconds(10)}}}, "0"]]),
        );
        assert_eq!(
            true, r[0][1]["list"][0]["ortusSigned"],
            "{name}: {:?}",
            r[0]
        );
        let pending = r[1][1]["created"]["s"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: {:?}", r[1]))
            .to_string();
        assert_eq!("pending", r[1][1]["created"]["s"]["undoStatus"]);
        let r =
            jmap(json!([["EmailSubmission/query", {"filter": {"undoStatus": "pending"}}, "0"]]));
        assert_eq!(json!([pending]), r[0][1]["ids"], "{name}");
        let r = jmap(
            json!([["EmailSubmission/set", {"update": {&pending: {"undoStatus": "canceled"}}}, "0"]]),
        );
        assert!(
            r[0][1]["updated"].get(&pending).is_some(),
            "{name}: {:?}",
            r[0]
        );
        let job = |id: &str| Job {
            id: format!("t-{id}"),
            kind: later::SEND_LATER_JOB.into(),
            payload: json!({"id": id}).to_string(),
            attempt: 1,
            max_attempts: 8,
        };
        block_on(later::run(&instance, &job(&pending))).unwrap();
        let sams = block_on(store::by_role(instance.store.as_ref(), &sam, "inbox"))
            .unwrap()
            .id;
        assert_eq!(
            0,
            block_on(store::entries(instance.store.as_ref(), &sams, 0, 10))
                .unwrap()
                .len(),
            "{name}: undone means not sent"
        );
        assert_eq!(
            1,
            block_on(store::entries(instance.store.as_ref(), &drafts, 0, 10))
                .unwrap()
                .len(),
            "{name}: the draft stays"
        );

        // Sent later: waits, then goes when its job runs; the draft goes.
        let r = jmap(
            json!([["EmailSubmission/set", {"create": {"s": {"identityId": "olena@ministry.example", "emailId": draft, "ortusSendAt": in_seconds(3600)}}}, "0"]]),
        );
        let later_id = r[0][1]["created"]["s"]["id"].as_str().unwrap().to_string();
        let r = jmap(json!([["EmailSubmission/get", {"ids": null}, "0"]]));
        assert_eq!(later_id, r[0][1]["list"][0]["id"], "{name}: {:?}", r[0]);
        block_on(later::run(&instance, &job(&later_id))).unwrap();
        let received = block_on(store::entries(instance.store.as_ref(), &sams, 0, 10)).unwrap();
        assert!(
            received.len() == 1 && received[0].subject == "Zażółć gęślą jaźń",
            "{name}: {received:?}"
        );
        assert_eq!(
            0,
            block_on(store::entries(instance.store.as_ref(), &drafts, 0, 10))
                .unwrap()
                .len(),
            "{name}: the draft is gone"
        );
        let r = jmap(
            json!([["EmailSubmission/set", {"update": {&later_id: {"undoStatus": "canceled"}}}, "0"]]),
        );
        assert_eq!(
            "cannotUnsend", r[0][1]["notUpdated"][&later_id]["type"],
            "{name}"
        );
        // A refusal when the time comes is kept for the person to see.
        let r = jmap(
            json!([["Email/set", {"create": {"d": {"mailboxIds": {&drafts: true}, "header:X-Ortus-Label:asText": "internal", "to": [{"email": "maria@mail.example"}], "subject": "Inside only", "textBody": [{"partId": "1", "type": "text/plain"}], "bodyValues": {"1": {"value": "x"}}}}}, "0"]]),
        );
        let inside = r[0][1]["created"]["d"]["id"].as_str().unwrap().to_string();
        let r = jmap(
            json!([["EmailSubmission/set", {"create": {"s": {"identityId": "olena@ministry.example", "emailId": inside, "ortusSendAt": in_seconds(3600)}}}, "0"]]),
        );
        assert_eq!(
            true, r[0][1]["notCreated"]["s"]["ortusNeedsReason"],
            "{name}: refused before scheduling: {:?}",
            r[0]
        );

        // Snooze until later; it waits in Snoozed, then comes back unread.
        let r = jmap(
            json!([["Email/set", {"update": {&maria.message: {"keywords/$seen": true, "ortusSnoozedUntil": in_seconds(3600)}}}, "0"], ["Email/get", {"ids": [&maria.message], "properties": ["mailboxIds", "ortusSnoozedUntil", "keywords"]}, "1"]]),
        );
        assert!(
            r[0][1]["updated"].get(&maria.message).is_some(),
            "{name}: {:?}",
            r[0]
        );
        let snoozed = block_on(store::by_role(instance.store.as_ref(), &olena, "snoozed"))
            .unwrap()
            .id;
        assert_eq!(
            json!({&snoozed: true}),
            r[1][1]["list"][0]["mailboxIds"],
            "{name}: {:?}",
            r[1]
        );
        assert!(r[1][1]["list"][0]["ortusSnoozedUntil"].is_string());
        block_on(store::wake_due(&instance)).unwrap();
        assert_eq!(
            0,
            block_on(store::entries(instance.store.as_ref(), &inbox.id, 0, 10))
                .unwrap()
                .len(),
            "{name}: not yet"
        );
        block_on(
            instance
                .store
                .execute("UPDATE mail_snoozed SET until = 0", &[]),
        )
        .unwrap();
        block_on(store::wake_due(&instance)).unwrap();
        let back = block_on(store::entries(instance.store.as_ref(), &inbox.id, 0, 10)).unwrap();
        assert!(
            back.len() == 1 && !back[0].flags.contains(&"\\Seen".to_string()),
            "{name}: back and unread: {back:?}"
        );
        assert_eq!(
            0,
            block_on(store::entries(instance.store.as_ref(), &snoozed, 0, 10))
                .unwrap()
                .len()
        );
    }
}
