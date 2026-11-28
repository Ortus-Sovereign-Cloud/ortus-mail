//! What the web inbox costs in the database: the mail's state is read once
//! for the whole JMAP request, however many methods answer with it, and
//! read again after a write in the same request.

mod common;

use ortus_core::runtime::block_on;
use ortus_mail::{jmap, store};
use serde_json::{json, Value};

const USING: [&str; 3] = [
    "urn:ietf:params:jmap:core",
    "urn:ietf:params:jmap:mail",
    "urn:ietf:params:jmap:submission",
];

#[test]
fn the_inbox_reads_the_state_once_a_request() {
    for (name, instance, _keep) in common::installations() {
        ortus_core::instance::testing_auth::signed_in(
            &instance,
            "olena@ministry.example",
            ortus_core::auth::accounts::Role::Member,
        );
        let olena = block_on(ortus_core::auth::accounts::by_email(
            instance.store.as_ref(),
            "olena@ministry.example",
        ))
        .unwrap()
        .unwrap();
        let inbox = block_on(store::by_role(instance.store.as_ref(), &olena.id, "inbox")).unwrap();
        for n in 0..3 {
            let raw = format!("From: Maria Peters <maria@mail.example>\r\nTo: olena@ministry.example\r\nSubject: Report {n}\r\nMessage-ID: <r{n}@mail.example>\r\n\r\nNumber {n}.\r\n");
            block_on(store::deliver(
                &instance,
                &olena.id,
                &inbox.id,
                raw.as_bytes(),
                &[],
                None,
                0,
            ))
            .unwrap();
        }
        // The answers, and the database statements they took.
        let run = |calls: Value| -> (Vec<Value>, u32) {
            let request = json!({"using": USING, "methodCalls": calls});
            let (answer, cost) = block_on(ortus_core::request::serve(
                || "jmap".into(),
                jmap::run(&instance, &olena, &request),
            ));
            let answer = answer.unwrap_or_else(|e| panic!("{name}: {}", e.message));
            (
                answer["methodResponses"].as_array().unwrap().clone(),
                cost.statements,
            )
        };
        let boxes = json!(["Mailbox/get", {}, "b"]);
        let (one, alone) = run(json!([boxes]));
        // The inbox's first request: methods that each answer with the state.
        let (all, five) = run(json!([
            boxes,
            ["Mailbox/query", {}, "mq"],
            ["Email/query", {"filter": {"notKeyword": "$deleted"}, "limit": 500}, "k"],
            ["Email/get", {"#ids": {"resultOf": "k", "name": "Email/query", "path": "/ids"}, "properties": ["keywords"]}, "kw"],
            ["Thread/get", {"ids": []}, "t"],
        ]));
        // The state alone was four statements a method (16 more for these four).
        assert!(
            five <= alone + 8,
            "{name}: {five} statements for five methods, {alone} for one"
        );
        let state = one[0][1]["state"].as_str().unwrap().to_string();
        for r in &all {
            let s = r[1].get("state").or_else(|| r[1].get("queryState"));
            assert_eq!(
                Some(state.as_str()),
                s.and_then(Value::as_str),
                "{name}: {}",
                r[0]
            );
        }
        // A write in the request: what comes after it sees the new state.
        let id = all[3][1]["list"][0]["id"].as_str().unwrap().to_string();
        let (after, _) = run(json!([
            boxes,
            ["Email/set", {"update": {id: {"keywords/$seen": true}}}, "s"],
            ["Mailbox/get", {}, "b2"],
        ]));
        let (before, set, later) = (&after[0][1]["state"], &after[1][1], &after[2][1]);
        assert_eq!(before, &set["oldState"], "{name}");
        assert_eq!(later["state"], set["newState"], "{name}");
        assert_ne!(before, &later["state"], "{name}: reading changes the state");
        let unread = later["list"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "inbox")
            .unwrap()["unreadEmails"]
            .as_i64();
        assert_eq!(Some(2), unread, "{name}");
    }
}
