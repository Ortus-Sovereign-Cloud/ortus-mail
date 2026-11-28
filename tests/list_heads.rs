//! An inbox page does not read each message's file: From, To and whether
//! there is an invitation are kept at delivery. What it shows is what the
//! file says (the same page with nothing kept reads the files), an
//! invitation still shows, and messages kept from before still read theirs.

mod common;

use ortus_core::runtime::block_on;
use ortus_mail::{jmap, store};
use serde_json::{json, Value};

const USING: [&str; 2] = ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"];
const LIST: [&str; 12] = [
    "threadId",
    "subject",
    "from",
    "to",
    "receivedAt",
    "preview",
    "keywords",
    "mailboxIds",
    "hasAttachment",
    "ortusCategory",
    "ortusInvite",
    "ortusDelivery",
];

#[test]
fn an_inbox_page_shows_what_the_files_say_without_reading_them() {
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
        let letters = [
            "From: Марія Петренко <maria@mail.example>\r\nTo: olena@ministry.example, \"Sam K.\" <sam@ministry.example>\r\nSubject: Звіт\r\nMessage-ID: <a@mail.example>\r\n\r\nДобрий день.\r\n".to_string(),
            "From: noreply@city.example\r\nSubject: No To at all\r\nMessage-ID: <b@city.example>\r\n\r\nNews.\r\n".to_string(),
            "Subject: No From either\r\nMessage-ID: <c@x.example>\r\n\r\nOdd.\r\n".to_string(),
            "From: Taras <taras@partner.example>\r\nTo: olena@ministry.example\r\nSubject: Meeting\r\nMessage-ID: <d@partner.example>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nInvited.\r\n--b\r\nContent-Type: text/calendar; charset=utf-8; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:meeting-1@partner.example\r\nSUMMARY:Bridge works\r\nDTSTART:20261201T090000Z\r\nDTEND:20261201T100000Z\r\nORGANIZER:mailto:taras@partner.example\r\nATTENDEE:mailto:olena@ministry.example\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n--b--\r\n".to_string(),
        ];
        let mut ids = Vec::new();
        for raw in &letters {
            ids.push(
                block_on(store::deliver(
                    &instance,
                    &olena.id,
                    &inbox.id,
                    raw.as_bytes(),
                    &[],
                    None,
                    0,
                ))
                .unwrap()
                .message,
            );
        }
        let page = || -> Value {
            let request = json!({"using": USING, "methodCalls": [["Email/get", {"ids": ids, "properties": LIST}, "l"]]});
            let answer = block_on(jmap::run(&instance, &olena, &request))
                .unwrap_or_else(|e| panic!("{name}: {}", e.message));
            answer["methodResponses"][0][1]["list"].clone()
        };
        let kept = page();
        assert_eq!(
            json!([{"name": "Марія Петренко", "email": "maria@mail.example"}]),
            kept[0]["from"],
            "{name}"
        );
        assert_eq!(2, kept[0]["to"].as_array().unwrap().len(), "{name}");
        assert!(
            kept[3]["ortusInvite"].is_object(),
            "{name}: the invitation shows: {}",
            kept[3]
        );
        // Nothing kept (as for mail from before): the files are read, and say the same.
        for id in &ids {
            block_on(s.execute(
                "UPDATE mail_messages SET heads = NULL WHERE id = $1",
                &[id.as_str().into()],
            ))
            .unwrap();
        }
        assert_eq!(kept, page(), "{name}: what the files say");
        // Kept again, and the files gone: the page needs none but the invitation's.
        for (id, raw) in ids.iter().zip(&letters) {
            let heads = store::summarise(raw.as_bytes()).heads;
            block_on(s.execute(
                "UPDATE mail_messages SET heads = $1 WHERE id = $2",
                &[heads.into(), id.as_str().into()],
            ))
            .unwrap();
        }
        for id in &ids[..3] {
            block_on(
                instance
                    .files
                    .delete(&format!("mail/{}/{id}.eml", olena.id)),
            )
            .unwrap();
        }
        assert_eq!(kept, page(), "{name}: without the files");
    }
}
