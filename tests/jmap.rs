//! JMAP on both installation shapes: an app signed in with an app password
//! lists, reads, files, drafts and sends mail, uploads and downloads, and
//! hears of changes; a browser cookie opens nothing; another person's mail
//! is not found.

mod common;

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use common::dns::Dns;
use ortus_core::auth::accounts::Role;
use ortus_core::instance::testing_auth::signed_in;
use ortus_core::runtime::block_on;
use ortus_mail::store;
use rust_web_server::test_client::TestClient;
use serde_json::{json, Value};

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

const FLOOD: &str = "From: Maria Peters <maria@mail.example>\r\nTo: olena@ministry.example\r\nSubject: Flood damage report\r\nDate: Thu, 19 Nov 2026 08:05:00 +0000\r\nMessage-ID: <a@mail.example>\r\n\r\nMy car was damaged on Mill Street.\r\n";
const BUDGET: &str = "From: Sam Kowalski <sam@ministry.example>\r\nTo: olena@ministry.example\r\nSubject: The budget\r\nDate: Thu, 19 Nov 2026 09:00:00 +0000\r\nMessage-ID: <b@ministry.example>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"x\"\r\n\r\n--x\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nNumbers attached.\r\n--x\r\nContent-Type: text/csv; name=\"budget.csv\"\r\nContent-Disposition: attachment; filename=\"budget.csv\"\r\n\r\nline,amount\r\nroads,120\r\n--x--\r\n";

const USING: [&str; 3] = [
    "urn:ietf:params:jmap:core",
    "urn:ietf:params:jmap:mail",
    "urn:ietf:params:jmap:submission",
];

#[test]
fn apps_use_mail_over_jmap() {
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
        let sams_inbox = block_on(store::by_role(instance.store.as_ref(), &sam, "inbox")).unwrap();
        let private = block_on(store::deliver(
            &instance,
            &sam,
            &sams_inbox.id,
            FLOOD.as_bytes(),
            &[],
            None,
            0,
        ))
        .unwrap();

        let client = TestClient::new(ortus_core::app(Arc::clone(&instance)));
        let made: Value = serde_json::from_str(
            client
                .post("/api/v1/me/app-passwords")
                .header("Authorization", &format!("Bearer {token}"))
                .header("X-Requested-With", "ortus")
                .body_text(r#"{"name":"Mail app"}"#)
                .send()
                .body_text(),
        )
        .unwrap();
        let basic = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!(
                "olena@ministry.example:{}",
                made["password"].as_str().unwrap()
            ))
        );
        let jmap = |calls: Value| -> Vec<Value> {
            let response = client
                .post("/api/v1/mail/jmap/api")
                .header("Authorization", &basic)
                .body_text(&json!({"using": USING, "methodCalls": calls}).to_string())
                .send();
            assert_eq!(200, response.status(), "{name}: {}", response.body_text());
            let body: Value = serde_json::from_str(response.body_text()).unwrap();
            body["methodResponses"].as_array().unwrap().clone()
        };

        // The session.
        let session: Value = serde_json::from_str(
            client
                .get("/api/v1/mail/jmap/session")
                .header("Authorization", &basic)
                .send()
                .body_text(),
        )
        .unwrap();
        assert_eq!(
            olena, session["primaryAccounts"]["urn:ietf:params:jmap:mail"],
            "{name}: {session}"
        );
        assert!(session["apiUrl"]
            .as_str()
            .unwrap()
            .ends_with("/api/v1/mail/jmap/api"));

        // A cookie alone opens nothing; the web client uses its own door.
        let cookie = client
            .post("/api/v1/mail/jmap/api")
            .header("Cookie", &format!("ortus={token}"))
            .body_text(
                &json!({"using": USING, "methodCalls": [["Core/echo", {}, "0"]]}).to_string(),
            )
            .send();
        assert_eq!(401, cookie.status(), "{name}");
        let web = client
            .post("/api/v1/mail/web/jmap")
            .header("Authorization", &format!("Bearer {token}"))
            .header("X-Requested-With", "ortus")
            .body_text(
                &json!({"using": USING, "methodCalls": [["Core/echo", {"hi": 1}, "0"]]})
                    .to_string(),
            )
            .send();
        assert!(
            web.status() == 200 && web.body_text().contains("\"hi\":1"),
            "{name}: {}",
            web.body_text()
        );

        // Folders, the newest mail with a back-reference, and its text.
        let r = jmap(json!([
            ["Mailbox/get", {"accountId": olena}, "0"],
            ["Email/query", {"accountId": olena, "filter": {"inMailbox": inbox.id}, "sort": [{"property": "receivedAt", "isAscending": false}]}, "1"],
            ["Email/get", {"accountId": olena, "#ids": {"resultOf": "1", "name": "Email/query", "path": "/ids"}, "properties": ["subject", "from", "keywords", "mailboxIds", "textBody", "attachments", "bodyValues", "threadId"], "fetchTextBodyValues": true}, "2"],
        ]));
        let boxes = r[0][1]["list"].as_array().unwrap();
        let inbox_json = boxes.iter().find(|b| b["role"] == "inbox").unwrap();
        assert_eq!(
            (2, 2),
            (
                inbox_json["totalEmails"].as_i64().unwrap(),
                inbox_json["unreadEmails"].as_i64().unwrap()
            ),
            "{name}: {inbox_json}"
        );
        let ids = r[1][1]["ids"].as_array().unwrap().clone();
        assert_eq!(2, ids.len(), "{name}: {}", r[1]);
        let emails = r[2][1]["list"].as_array().unwrap();
        let budget = &emails[0];
        assert_eq!("The budget", budget["subject"], "{name}: newest first");
        assert_eq!("sam@ministry.example", budget["from"][0]["email"]);
        let text_part = budget["textBody"][0]["partId"].as_str().unwrap();
        assert!(
            budget["bodyValues"][text_part]["value"]
                .as_str()
                .unwrap()
                .contains("Numbers attached"),
            "{name}: {budget}"
        );
        let attachment = &budget["attachments"][0];
        assert_eq!("budget.csv", attachment["name"], "{name}: {budget}");

        // Downloading the attachment.
        let blob = attachment["blobId"].as_str().unwrap();
        let download = client
            .get(&format!(
                "/api/v1/mail/jmap/download/{olena}/{blob}/budget.csv"
            ))
            .header("Authorization", &basic)
            .send();
        assert!(
            download.status() == 200 && download.body_text().contains("roads,120"),
            "{name}: {}",
            download.body_text()
        );
        assert!(download
            .header("content-disposition")
            .unwrap_or_default()
            .starts_with("attachment"));

        // Searching, reading (IMAP sees \Seen), filing into Archive.
        let r = jmap(json!([["Email/query", {"filter": {"text": "mill street"}}, "0"]]));
        assert_eq!(
            1,
            r[0][1]["ids"].as_array().unwrap().len(),
            "{name}: {}",
            r[0]
        );
        let flood = r[0][1]["ids"][0].as_str().unwrap().to_string();
        let archive = boxes.iter().find(|b| b["role"] == "archive").unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let r = jmap(
            json!([["Email/set", {"update": {&flood: {"keywords/$seen": true, "keywords/$flagged": true, format!("mailboxIds/{}", inbox.id): null, format!("mailboxIds/{archive}"): true}}}, "0"]]),
        );
        assert!(r[0][1]["updated"].get(&flood).is_some(), "{name}: {}", r[0]);
        let archived = block_on(store::entries(instance.store.as_ref(), &archive, 0, 10)).unwrap();
        assert!(
            archived.len() == 1
                && archived[0].flags.contains(&"\\Seen".to_string())
                && archived[0].flags.contains(&"\\Flagged".to_string()),
            "{name}: {archived:?}"
        );
        assert_eq!(
            1,
            block_on(store::entries(instance.store.as_ref(), &inbox.id, 0, 10))
                .unwrap()
                .len()
        );

        // A thread.
        let thread = budget["threadId"].as_str().unwrap();
        let r = jmap(json!([["Thread/get", {"ids": [thread]}, "0"]]));
        assert_eq!(
            1,
            r[0][1]["list"][0]["emailIds"].as_array().unwrap().len(),
            "{name}: {}",
            r[0]
        );

        // A draft, sent; the draft goes, Sent keeps a copy, Sam receives it.
        let drafts = boxes.iter().find(|b| b["role"] == "drafts").unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let r = jmap(json!([
            ["Identity/get", {}, "i"],
            ["Email/set", {"create": {"d1": {"mailboxIds": {&drafts: true}, "keywords": {"$draft": true}, "from": [{"name": "Olena", "email": "olena@ministry.example"}], "to": [{"name": "Sam", "email": "sam@ministry.example"}], "subject": "Road repairs", "textBody": [{"partId": "t", "type": "text/plain"}], "bodyValues": {"t": {"value": "Can we start on Monday?"}}}}}, "0"],
            ["EmailSubmission/set", {"create": {"s1": {"identityId": "olena@ministry.example", "emailId": "#d1"}}, "onSuccessDestroyEmail": ["#s1"]}, "1"],
        ]));
        assert_eq!(
            "olena@ministry.example", r[0][1]["list"][0]["email"],
            "{name}: {}",
            r[0]
        );
        assert!(
            r[1][1]["created"]["d1"]["id"].is_string(),
            "{name}: {}",
            r[1]
        );
        assert!(
            r[2][1]["created"]["s1"]["id"].is_string(),
            "{name}: {}",
            r[2]
        );
        assert_eq!(
            "Email/set", r[3][0],
            "{name}: the draft is destroyed after sending: {r:?}"
        );
        assert_eq!(
            0,
            block_on(store::entries(instance.store.as_ref(), &drafts, 0, 10))
                .unwrap()
                .len(),
            "{name}"
        );
        let sams = block_on(store::entries(
            instance.store.as_ref(),
            &sams_inbox.id,
            0,
            10,
        ))
        .unwrap();
        assert!(
            sams.iter().any(|e| e.subject == "Road repairs"),
            "{name}: {sams:?}"
        );
        let sent = block_on(store::by_role(instance.store.as_ref(), &olena, "sent")).unwrap();
        assert_eq!(
            1,
            block_on(store::entries(instance.store.as_ref(), &sent.id, 0, 10))
                .unwrap()
                .len(),
            "{name}"
        );

        // Sending as someone else is refused.
        let r = jmap(
            json!([["EmailSubmission/set", {"create": {"s2": {"identityId": "sam@ministry.example", "emailId": flood}}}, "0"]]),
        );
        assert_eq!(
            "forbiddenFrom", r[0][1]["notCreated"]["s2"]["type"],
            "{name}: {}",
            r[0]
        );

        // Upload and import.
        let upload = client.post(&format!("/api/v1/mail/jmap/upload/{olena}")).header("Authorization", &basic).header("Content-Type", "message/rfc822").body_text("From: x@old.example\r\nTo: olena@ministry.example\r\nSubject: From the old server\r\nMessage-ID: <old@old.example>\r\n\r\nKept.\r\n").send();
        let uploaded: Value = serde_json::from_str(upload.body_text()).unwrap();
        let r = jmap(
            json!([["Email/import", {"emails": {"m1": {"blobId": uploaded["blobId"], "mailboxIds": {&archive: true}, "keywords": {"$seen": true}}}}, "0"]]),
        );
        assert!(
            r[0][1]["created"]["m1"]["id"].is_string(),
            "{name}: {}",
            r[0]
        );
        assert_eq!(
            2,
            block_on(store::entries(instance.store.as_ref(), &archive, 0, 10))
                .unwrap()
                .len()
        );

        // Folders of one's own.
        let r = jmap(
            json!([["Mailbox/set", {"create": {"p": {"name": "Projects"}}}, "0"], ["Mailbox/set", {"destroy": ["#p"]}, "1"]]),
        );
        let made = r[0][1]["created"]["p"]["id"].as_str().unwrap().to_string();
        let r = jmap(json!([["Mailbox/set", {"destroy": [made, inbox.id]}, "0"]]));
        assert_eq!(
            1,
            r[0][1]["destroyed"].as_array().unwrap().len(),
            "{name}: {}",
            r[0]
        );
        assert_eq!("forbidden", r[0][1]["notDestroyed"][&inbox.id]["type"]);

        // Someone else's mail and account.
        let r = jmap(
            json!([["Email/get", {"ids": [private.message]}, "0"], ["Mailbox/get", {"accountId": sam}, "1"]]),
        );
        assert_eq!(json!([private.message]), r[0][1]["notFound"], "{name}");
        assert_eq!("accountNotFound", r[1][1]["type"], "{name}");
        let other = client
            .get(&format!(
                "/api/v1/mail/jmap/download/{olena}/{}/x.eml",
                private.message
            ))
            .header("Authorization", &basic)
            .send();
        assert_eq!(404, other.status(), "{name}");

        // Changes: unchanged state, then a change; push says so.
        let r = jmap(json!([["Email/get", {"ids": []}, "0"]]));
        let before = r[0][1]["state"].as_str().unwrap().to_string();
        let r = jmap(json!([["Email/changes", {"sinceState": before}, "0"]]));
        assert_eq!(before, r[0][1]["newState"], "{name}");
        let events = client
            .get("/api/v1/mail/jmap/eventsource?types=*&closeafter=no&ping=0")
            .header("Authorization", &basic)
            .send()
            .sse();
        let mut events = events;
        let first = events
            .next_event(Duration::from_secs(5))
            .expect("a first state");
        assert!(
            first.data.contains("StateChange") && first.data.contains(&before),
            "{name}: {}",
            first.data
        );
        jmap(json!([["Email/set", {"update": {&flood: {"keywords/$seen": null}}}, "0"]]));
        let second = events
            .next_event(Duration::from_secs(10))
            .expect("a change");
        assert!(
            second.data.contains("StateChange") && !second.data.contains(&before),
            "{name}: {}",
            second.data
        );
        let r = jmap(json!([["Email/changes", {"sinceState": before}, "0"]]));
        assert_eq!("cannotCalculateChanges", r[0][1]["type"], "{name}");
    }
}
