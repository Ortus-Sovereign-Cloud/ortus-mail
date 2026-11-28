//! `Email/query` by date reads the mailbox a chunk at a time and stops once
//! its page is full; it answers exactly what reading the whole mailbox
//! does. Each query is asked twice: as the clients ask it, and with a
//! `{"text": ""}` condition added (it matches every message, and a text
//! filter is still answered by reading everything).

mod common;

use ortus_core::runtime::block_on;
use ortus_mail::{jmap, store};
use serde_json::{json, Value};

const USING: [&str; 2] = ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"];

#[test]
fn a_query_by_date_answers_what_reading_everything_does() {
    for (name, instance, _keep) in common::installations() {
        ortus_core::instance::testing_auth::signed_in(
            &instance,
            "olena@ministry.example",
            ortus_core::auth::accounts::Role::Member,
        );
        let s = instance.store.as_ref();
        let olena = block_on(ortus_core::auth::accounts::by_email(
            s,
            "olena@ministry.example",
        ))
        .unwrap()
        .unwrap();
        let inbox = block_on(store::by_role(s, &olena.id, "inbox")).unwrap();
        let archive = block_on(store::by_role(s, &olena.id, "archive")).unwrap();
        // 600 messages (more than two chunks): threads of three, some read,
        // some in Archive, some copied to both folders with other flags.
        let mut delivered = Vec::new();
        for i in 0..600 {
            let thread = i / 3;
            let reply = if i % 3 == 0 {
                String::new()
            } else {
                format!("In-Reply-To: <t{thread}.0@partner.example>\r\n")
            };
            let raw = format!("From: Colleague {c} <c{c}@partner.example>\r\nTo: olena@ministry.example\r\nSubject: Bridge {thread}\r\nMessage-ID: <t{thread}.{n}@partner.example>\r\n{reply}\r\nNumber {i}.\r\n", c = i % 7, n = i % 3);
            let to = if i % 5 == 0 { &archive.id } else { &inbox.id };
            let flags: &[&str] = if i % 4 == 0 { &["\\Seen"] } else { &[] };
            let d = block_on(store::deliver(
                &instance,
                &olena.id,
                to,
                raw.as_bytes(),
                flags,
                None,
                0,
            ))
            .unwrap();
            if i % 11 == 0 && to == &inbox.id {
                // A copy in Archive, read there.
                let uid = block_on(store::copy(s, &inbox.id, d.uid, &archive.id))
                    .unwrap()
                    .unwrap();
                block_on(s.execute(
                    "UPDATE mail_entries SET flags = '\\Seen' WHERE mailbox_id = $1 AND uid = $2",
                    &[archive.id.as_str().into(), uid.into()],
                ))
                .unwrap();
            }
            delivered.push(d.message);
        }
        // Ties on the time, and categories.
        for (i, id) in delivered.iter().enumerate() {
            let at = 1_795_000_000_000i64 + (i as i64 / 4) * 1000;
            let category = ["important", "updates", "important", "promotions"][i % 4];
            block_on(s.execute(
                "UPDATE mail_messages SET received_at = $1, category = $2 WHERE id = $3",
                &[at.into(), category.into(), id.as_str().into()],
            ))
            .unwrap();
        }
        let run = |args: Value| -> Value {
            let request = json!({"using": USING, "methodCalls": [["Email/query", args, "q"]]});
            let answer = block_on(jmap::run(&instance, &olena, &request))
                .unwrap_or_else(|e| panic!("{name}: {}", e.message));
            answer["methodResponses"][0].clone()
        };
        let newest = json!([{"property": "receivedAt", "isAscending": false}]);
        let oldest = json!([{"property": "receivedAt", "isAscending": true}]);
        let unread_in = |box_id: &str| json!({"operator": "AND", "conditions": [{"inMailbox": box_id}, {"notKeyword": "$seen"}]});
        let a_reply = delivered[4].clone();
        let cases = vec![
            json!({"filter": {"notKeyword": "$deleted"}, "limit": 500}),
            json!({"filter": {"inMailbox": inbox.id}, "sort": newest, "collapseThreads": true, "limit": 100}),
            json!({"filter": {"inMailbox": archive.id}, "sort": newest, "collapseThreads": true, "limit": 100, "position": 30}),
            json!({"filter": unread_in(&inbox.id), "sort": newest, "limit": 4}),
            json!({"filter": unread_in(&archive.id), "sort": newest, "limit": 50}),
            json!({"filter": {"operator": "AND", "conditions": [{"inMailbox": inbox.id}, {"ortusCategory": "updates"}]}, "sort": newest, "collapseThreads": true, "limit": 100}),
            json!({"filter": {"operator": "AND", "conditions": [{"inMailbox": inbox.id}, {"ortusCategory": "important"}, {"notKeyword": "$seen"}]}, "calculateTotal": true, "limit": 0}),
            json!({"filter": {"operator": "AND", "conditions": [{"inMailbox": inbox.id}, {"ortusCategory": "important"}, {"notKeyword": "$seen"}]}, "limit": 20}),
            json!({"filter": {"inMailbox": inbox.id}, "sort": oldest, "collapseThreads": true, "limit": 70, "position": 200}),
            json!({"filter": {"inMailbox": inbox.id}, "sort": newest, "limit": 10, "position": 5000}),
            json!({"filter": {"inMailbox": inbox.id}, "sort": newest, "limit": 10, "position": -15}),
            json!({"filter": {"inMailbox": inbox.id}, "sort": newest, "limit": 10, "anchor": a_reply, "anchorOffset": -3}),
            json!({"filter": {"operator": "OR", "conditions": [{"inMailbox": archive.id}, {"hasKeyword": "$seen"}]}, "sort": newest, "collapseThreads": true, "limit": 300, "calculateTotal": true}),
            json!({"filter": {"from": "c3@partner"}, "sort": newest, "limit": 30}),
            json!({"sort": newest, "limit": 500, "calculateTotal": true}),
        ];
        for args in cases {
            let quick = run(args.clone());
            let mut everything = args.clone();
            let filter = args.get("filter").cloned().unwrap_or(json!({}));
            everything["filter"] = json!({"operator": "AND", "conditions": [filter, {"text": ""}]});
            let slow = run(everything);
            assert_eq!("Email/query", quick[0], "{name}: {args}: {quick}");
            for key in ["ids", "position"] {
                assert_eq!(slow[1][key], quick[1][key], "{name}: {key} of {args}");
            }
            if args.get("calculateTotal") == Some(&json!(true)) {
                assert_eq!(
                    slow[1]["total"], quick[1]["total"],
                    "{name}: total of {args}"
                );
            }
        }
    }
}
