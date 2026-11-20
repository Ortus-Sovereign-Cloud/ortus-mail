//! The catch-up card on both shapes: since the previous visit, mail from
//! people to the person that waits for a reply (not machines', not
//! answered, not to others), invitations not yet answered (the calendar
//! says), and alerts from the notification centre.

mod common;

use std::sync::Arc;

use ortus_core::app::{App, AppInfo, Group};
use ortus_core::auth::accounts::Role;
use ortus_core::instance::{testing_auth::signed_in, Instance};
use ortus_core::interfaces::{AgendaItem, Calendar, Invitation};
use ortus_core::runtime::block_on;
use ortus_core::storage::BoxFuture;
use ortus_mail::store;
use rust_web_server::test_client::TestClient;
use serde_json::{json, Value};

struct Book;

impl Calendar for Book {
    fn receive<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Invitation, String>> {
        Box::pin(async { Err("not here".into()) })
    }
    fn invitation<'a>(
        &'a self,
        _: &'a str,
        uid: &'a str,
    ) -> BoxFuture<'a, Result<Option<Invitation>, String>> {
        let found = (uid == "brief@regional.example").then(|| Invitation {
            uid: uid.into(),
            method: "REQUEST".into(),
            summary: "Hydrology briefing".into(),
            start_ms: Some(1_795_784_400_000),
            end_ms: Some(1_795_786_200_000),
            organizer: Some("daniel@regional.example".into()),
            answer: Some("NEEDS-ACTION".into()),
            cancelled: false,
            item: None,
        });
        Box::pin(async move { Ok(found) })
    }
    fn respond<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Invitation, String>> {
        Box::pin(async { Err("not here".into()) })
    }
    fn agenda<'a>(
        &'a self,
        _: &'a str,
        _: i64,
        _: i64,
    ) -> BoxFuture<'a, Result<Vec<AgendaItem>, String>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn free_times<'a>(
        &'a self,
        _: &'a str,
        _: &'a [String],
        _: i64,
        _: i64,
        _: i64,
        _: usize,
    ) -> BoxFuture<'a, Result<Vec<i64>, String>> {
        Box::pin(async { Ok(vec![]) })
    }
}

struct FakeCalendar;

impl App for FakeCalendar {
    fn info(&self) -> AppInfo {
        AppInfo {
            id: "calendar",
            group: Group::Organise,
            path: "/calendar",
            order: 1,
        }
    }
    fn provide(&self, _: &Arc<Instance>, services: &ortus_core::services::Services) {
        let calendar: Arc<dyn Calendar> = Arc::new(Book);
        services.provide("calendar", calendar);
    }
}

fn mail(from: &str, to: &str, subject: &str) -> Vec<u8> {
    format!("From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nDate: Fri, 20 Nov 2026 10:00:00 +0000\r\nMessage-ID: <{}@x>\r\n\r\nHello.\r\n", subject.replace(' ', "")).into_bytes()
}

const ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:brief@regional.example\r\nDTSTART:20261126T140000Z\r\nDTEND:20261126T143000Z\r\nSUMMARY:Hydrology briefing\r\nORGANIZER:mailto:daniel@regional.example\r\nATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:olena@ministry.example\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

#[test]
fn the_catch_up_card_counts_what_waits() {
    for (name, base, _keep) in common::installations() {
        let mut apps = ortus_mail::apps();
        apps.push(Arc::new(FakeCalendar));
        let instance = ortus_core::instance::testing_store::instance(Arc::clone(&base.store), apps);
        for app in instance.apps.clone() {
            app.provide(&instance, &instance.services);
        }
        let s = instance.store.as_ref();
        let token = signed_in(&instance, "olena@ministry.example", Role::Member);
        block_on(ortus_mail::domains::add(&instance, "ministry.example")).unwrap();
        let olena = block_on(ortus_core::auth::accounts::by_email(
            s,
            "olena@ministry.example",
        ))
        .unwrap()
        .unwrap()
        .id;
        let inbox = block_on(store::by_role(s, &olena, "inbox")).unwrap();
        let deliver = |raw: Vec<u8>, flags: &[&str]| {
            block_on(store::deliver(
                &instance, &olena, &inbox.id, &raw, flags, None, 0,
            ))
            .unwrap();
        };
        deliver(
            mail(
                "Anna Kowalska <anna@regional.example>",
                "olena@ministry.example",
                "Gauge report",
            ),
            &[],
        );
        deliver(
            mail(
                "River Authority <alerts@river.example>",
                "olena@ministry.example",
                "Gauge at the bridge",
            ),
            &[],
        );
        deliver(
            mail(
                "Jonas Weber <jonas@police.example>",
                "olena@ministry.example",
                "Escort plan",
            ),
            &["\\Answered"],
        );
        deliver(
            mail(
                "Ana Ferreira <ana@x.example>",
                "team@x.example",
                "Town hall",
            ),
            &[],
        );
        let invite = format!(
            "From: Daniel Price <daniel@regional.example>\r\nTo: olena@ministry.example\r\nSubject: Invitation: Hydrology briefing\r\nMIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nInvited.\r\n--b\r\nContent-Type: text/calendar; charset=utf-8; method=REQUEST\r\n\r\n{ICS}--b--\r\n"
        );
        deliver(invite.into_bytes(), &[]);
        block_on(ortus_core::notify::add(
            s,
            &olena,
            &ortus_core::notify::Notice {
                app: "chat",
                kind: ortus_core::notify::Kind::Alert,
                title: "Gauge at the bridge passed 4.2 m",
                detail: "",
                path: "/chat?room=r",
                key: None,
                args: json!({}),
            },
        ))
        .unwrap();

        let client = TestClient::new(ortus_core::app(Arc::clone(&instance)));
        let get = || -> Value {
            serde_json::from_slice(
                client
                    .get("/api/v1/mail/catch-up")
                    .header("Authorization", &format!("Bearer {token}"))
                    .send()
                    .body_bytes(),
            )
            .unwrap()
        };
        let card = get();
        let names: Vec<&str> = card["reply"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["from"].as_str().unwrap())
            .collect();
        assert_eq!(vec!["Anna Kowalska"], names, "{name}: {card}");
        assert_eq!(
            "Hydrology briefing", card["invites"][0]["summary"],
            "{name}: {card}"
        );
        assert_eq!(1, card["invites"].as_array().unwrap().len(), "{name}");
        assert_eq!(
            "Gauge at the bridge passed 4.2 m", card["alerts"][0]["title"],
            "{name}"
        );
        // Again soon after: the same "since" (the previous visit is kept).
        assert_eq!(card["since"], get()["since"], "{name}");
    }
}
