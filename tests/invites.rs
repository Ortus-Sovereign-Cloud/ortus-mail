//! Invitations through mail, on both installation shapes: the calendar
//! sends them (Mailer) past the label policy; one from a colleague's mail
//! app reaches the calendar with its verified sender; the inbox shows it,
//! and an answer goes back through the calendar. A stand-in calendar
//! records what it is handed.

mod common;

use std::sync::{Arc, Mutex};

use common::dns::Dns;
use ortus_core::app::{App, AppInfo, Group};
use ortus_core::auth::accounts::Role;
use ortus_core::instance::{testing_auth::signed_in, Instance};
use ortus_core::interfaces::{AgendaItem, Calendar, Invitation, ItemRef, Mailer};
use ortus_core::runtime::block_on;
use ortus_core::storage::BoxFuture;
use ortus_mail::{invites, store};
use serde_json::{json, Value};

#[derive(Default)]
struct Book(Mutex<Vec<String>>);

fn inv(uid: &str) -> Invitation {
    Invitation {
        uid: uid.into(),
        method: "REQUEST".into(),
        summary: "Review".into(),
        start_ms: Some(1),
        end_ms: Some(2),
        organizer: Some("olena@ministry.example".into()),
        answer: Some("ACCEPTED".into()),
        cancelled: false,
        item: None,
    }
}

impl Calendar for Book {
    fn receive<'a>(
        &'a self,
        user: &'a str,
        sender: &'a str,
        ics: &'a str,
    ) -> BoxFuture<'a, Result<Invitation, String>> {
        self.0.lock().unwrap().push(format!(
            "receive {user} {sender} {}",
            ics.contains("UID:review")
        ));
        Box::pin(async { Ok(inv("review")) })
    }
    fn respond<'a>(
        &'a self,
        user: &'a str,
        uid: &'a str,
        answer: &'a str,
    ) -> BoxFuture<'a, Result<Invitation, String>> {
        self.0
            .lock()
            .unwrap()
            .push(format!("respond {user} {uid} {answer}"));
        Box::pin(async { Ok(inv("review")) })
    }
    fn agenda<'a>(
        &'a self,
        _: &'a str,
        _: i64,
        _: i64,
    ) -> BoxFuture<'a, Result<Vec<AgendaItem>, String>> {
        Box::pin(async {
            Ok(vec![AgendaItem {
                uid: "review@ministry.example".into(),
                summary: "Review".into(),
                start_ms: 1_795_672_800_000,
                end_ms: 1_795_674_600_000,
                all_day: false,
                answer: Some("TENTATIVE".into()),
                item: ItemRef {
                    app: "calendar".into(),
                    id: "e".into(),
                    path: "/calendar".into(),
                },
            }])
        })
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

struct FakeCalendar(Arc<Book>);

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
        let calendar: Arc<dyn Calendar> = self.0.clone();
        services.provide("calendar", calendar);
    }
}

const ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:review@ministry.example\r\nDTSTAMP:20261120T090000Z\r\nDTSTART:20261126T140000Z\r\nDTEND:20261126T143000Z\r\nSUMMARY:Shelter agreement review\r\nORGANIZER:mailto:olena@ministry.example\r\nATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:marek@ministry.example\r\nATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:anna@regional.example\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

#[test]
fn invitations_go_out_come_in_and_are_answered() {
    let dns = Dns::start();
    unsafe {
        std::env::set_var("ORTUS_DNS_SERVERS", dns.addr.to_string());
        std::env::set_var("ORTUS_MAIL_HOSTNAME", "mail.ministry.example");
    }
    for (name, base, _keep) in common::installations() {
        let book = Arc::new(Book::default());
        let mut apps = ortus_mail::apps();
        apps.push(Arc::new(FakeCalendar(Arc::clone(&book))));
        let instance = ortus_core::instance::testing_store::instance(Arc::clone(&base.store), apps);
        for app in instance.apps.clone() {
            app.provide(&instance, &instance.services);
        }
        let s = instance.store.as_ref();
        let olena_token = signed_in(&instance, "olena@ministry.example", Role::Member);
        let marek_token = signed_in(&instance, "marek@ministry.example", Role::Member);
        let id = |email: &str| {
            block_on(ortus_core::auth::accounts::by_email(s, email))
                .unwrap()
                .unwrap()
                .id
        };
        let (olena, marek) = (id("olena@ministry.example"), id("marek@ministry.example"));
        block_on(ortus_mail::domains::add(&instance, "ministry.example")).unwrap();

        // The calendar sends through mail: outside gets a queued REQUEST.
        let mailer = block_on(instance.services.get::<dyn Mailer>(&instance.switches))
            .expect("mail provides Mailer");
        block_on(mailer.send_invitation(
            &olena,
            &["anna@regional.example".to_string()],
            "Invitation: Shelter agreement review",
            "You are invited.",
            ICS,
        ))
        .unwrap();
        let sent = block_on(store::by_role(s, &olena, "sent")).unwrap();
        let entries = block_on(store::entries(s, &sent.id, 0, 10)).unwrap();
        let raw = block_on(store::raw(&instance, &olena, &entries[0].message))
            .unwrap()
            .unwrap();
        let (method, text) = invites::calendar_part(&raw).expect("a calendar part");
        assert_eq!("REQUEST", method, "{name}");
        assert!(text.contains("UID:review@ministry.example"));
        assert_eq!(
            1,
            block_on(s.query(
                "SELECT id FROM mail_outbound WHERE recipient = 'anna@regional.example'",
                &[]
            ))
            .unwrap()
            .len(),
            "{name}: queued"
        );

        // From a colleague's mail app: delivered, and handed to the calendar.
        block_on(mailer.send_invitation(
            &olena,
            &["marek@ministry.example".to_string()],
            "Invitation",
            "Invited.",
            ICS,
        ))
        .unwrap();
        let calls = book.0.lock().unwrap().clone();
        assert!(
            calls
                .iter()
                .any(|c| c == &format!("receive {marek} olena@ministry.example true")),
            "{name}: {calls:?}"
        );

        // The inbox shows it with Marek's answer, and he answers from there.
        let inbox = block_on(store::by_role(s, &marek, "inbox")).unwrap();
        let message = block_on(store::entries(s, &inbox.id, 0, 10)).unwrap()[0]
            .message
            .clone();
        let user = block_on(ortus_core::auth::accounts::by_id(s, &marek))
            .unwrap()
            .unwrap();
        let r = block_on(ortus_mail::jmap::run(&instance, &user, &json!({"using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"], "methodCalls": [["Email/get", {"ids": [message], "properties": ["ortusInvite"]}, "0"]]}))).unwrap();
        let invite = &r["methodResponses"][0][1]["list"][0]["ortusInvite"];
        assert!(
            invite["summary"] == "Shelter agreement review"
                && invite["method"] == "REQUEST"
                && invite["answer"] == "TENTATIVE",
            "{name}: {invite}"
        );
        let client =
            rust_web_server::test_client::TestClient::new(ortus_core::app(Arc::clone(&instance)));
        let reply = client
            .post(&format!("/api/v1/mail/messages/{message}/rsvp"))
            .header("Authorization", &format!("Bearer {marek_token}"))
            .header("X-Requested-With", "ortus")
            .body_text(r#"{"answer":"accepted"}"#)
            .send();
        assert_eq!(200, reply.status(), "{name}: {}", reply.body_text());
        assert!(
            book.0
                .lock()
                .unwrap()
                .iter()
                .any(|c| c == &format!("respond {marek} review@ministry.example accepted")),
            "{name}"
        );
        // Not someone else's message.
        let other = client
            .post(&format!("/api/v1/mail/messages/{message}/rsvp"))
            .header("Authorization", &format!("Bearer {olena_token}"))
            .header("X-Requested-With", "ortus")
            .body_text(r#"{"answer":"accepted"}"#)
            .send();
        assert_eq!(404, other.status());
        let _: Value = json!(null);
    }
}
