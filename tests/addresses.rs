//! More addresses for a person, through the API on every installation
//! shape: an owner who signs in with a personal address gets one in the
//! installation's domain; mail to it is theirs and they may send from it;
//! nobody else's address can be taken, and only administrators give them.

mod common;

use std::sync::Arc;

use ortus_core::auth::accounts::Role;
use ortus_core::instance::testing_auth::signed_in;
use ortus_core::runtime::block_on;
use rust_web_server::test_client::TestClient;
use serde_json::{json, Value};

#[test]
fn a_person_gets_another_address() {
    for (name, instance, _dir) in common::installations() {
        let domain = format!("{}.example", name.replace('+', "-"));
        block_on(ortus_mail::domains::add(&instance, &domain)).unwrap();
        let client = TestClient::new(ortus_core::app(Arc::clone(&instance)));
        // The owner signs in with a personal address, not one of ours.
        let owner = signed_in(
            &instance,
            &format!("bohdan-{name}@gmail.example"),
            Role::Owner,
        );
        let admin = signed_in(&instance, &format!("marek@{domain}"), Role::Admin);
        let member = signed_in(&instance, &format!("olena@{domain}"), Role::Member);
        let owner_id = block_on(ortus_core::auth::accounts::by_email(
            instance.store.as_ref(),
            &format!("bohdan-{name}@gmail.example"),
        ))
        .unwrap()
        .unwrap()
        .id;
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
        let mine = format!("bohdan@{domain}");
        let give = |token: &str, address: &str| {
            send(
                "POST",
                "/api/v1/mail/admin/addresses",
                token,
                json!({ "address": address, "user_id": owner_id }),
            )
        };

        // Only an administrator gives addresses.
        assert_eq!(403, give(&member, &mine).status(), "{name}");
        // Not one of our domains; someone else's account address.
        let elsewhere = give(&admin, "bohdan@elsewhere.example");
        assert_eq!(400, elsewhere.status(), "{name}");
        assert!(elsewhere.body_text().contains("not_our_domain"), "{name}");
        let olenas = give(&admin, &format!("olena@{domain}"));
        assert_eq!(409, olenas.status(), "{name}");
        assert!(
            olenas.body_text().contains("account address"),
            "{name}: {}",
            olenas.body_text()
        );
        assert_eq!(
            400,
            give(&admin, &format!("bad address@{domain}")).status(),
            "{name}"
        );

        // The address goes to the owner.
        let given = give(&admin, &format!(" Bohdan@{domain} "));
        assert_eq!(200, given.status(), "{name}: {}", given.body_text());
        assert_eq!(
            Some(owner_id.clone()),
            block_on(ortus_mail::store::recipient(instance.store.as_ref(), &mine)).unwrap(),
            "{name}: mail to it is the owner's"
        );
        // Taken now.
        assert_eq!(409, give(&admin, &mine).status(), "{name}");
        let listed: Value = serde_json::from_str(
            send("GET", "/api/v1/mail/admin/addresses", &admin, json!({})).body_text(),
        )
        .unwrap();
        assert_eq!(mine, listed["addresses"][0]["address"], "{name}");
        assert_eq!(
            format!("bohdan-{name}@gmail.example"),
            listed["addresses"][0]["email"],
            "{name}"
        );

        // The owner may send from it: it is among their addresses.
        let identities = send(
            "POST",
            "/api/v1/mail/web/jmap",
            &owner,
            json!({ "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail", "urn:ietf:params:jmap:submission"], "methodCalls": [["Identity/get", {}, "i"]] }),
        );
        assert!(
            identities.body_text().contains(&mine),
            "{name}: {}",
            identities.body_text()
        );

        // Taken away: no longer theirs.
        assert_eq!(
            200,
            send(
                "DELETE",
                // As the page sends it: the "@" escaped.
                &format!("/api/v1/mail/admin/addresses/{}", mine.replace('@', "%40")),
                &admin,
                json!({})
            )
            .status(),
            "{name}"
        );
        assert_eq!(
            None,
            block_on(ortus_mail::store::recipient(instance.store.as_ref(), &mine)).unwrap(),
            "{name}"
        );
        assert_eq!(
            404,
            send(
                "DELETE",
                &format!("/api/v1/mail/admin/addresses/{mine}"),
                &admin,
                json!({})
            )
            .status(),
            "{name}"
        );
    }
}
