//! An attachment saved from mail into the drive (a stand-in drive records
//! it), on both shapes: the attachment's bytes, its sender in the note;
//! only one's own mail.

mod common;

use std::sync::{Arc, Mutex};

use ortus_core::app::{App, AppInfo, Group};
use ortus_core::auth::accounts::Role;
use ortus_core::instance::{testing_auth::signed_in, Instance};
use ortus_core::interfaces::{Drive, ItemRef};
use ortus_core::runtime::block_on;
use ortus_core::storage::BoxFuture;
use ortus_mail::store;

/// User, name, contents, source, note.
type Save = (String, String, Vec<u8>, String, Option<String>);

#[derive(Default)]
struct Saved(Mutex<Vec<Save>>);

impl Drive for Saved {
    fn save<'a>(
        &'a self,
        user: &'a str,
        name: &'a str,
        _mime: &'a str,
        contents: &'a [u8],
        source: &'a str,
        note: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ItemRef, String>> {
        self.0.lock().unwrap().push((
            user.into(),
            name.into(),
            contents.to_vec(),
            source.into(),
            note.map(str::to_string),
        ));
        Box::pin(async {
            Ok(ItemRef {
                app: "drive".into(),
                id: "d1".into(),
                path: "/drive?file=d1".into(),
            })
        })
    }
    fn create<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<ortus_core::interfaces::ItemRef, String>> {
        Box::pin(async { Err("not here".into()) })
    }
    fn describe<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Option<ortus_core::interfaces::DriveItem>, String>> {
        Box::pin(async { Ok(None) })
    }
    fn rename<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Err("not here".into()) })
    }
}

struct FakeDrive(Arc<Saved>);

impl App for FakeDrive {
    fn info(&self) -> AppInfo {
        AppInfo {
            id: "drive",
            group: Group::Create,
            path: "/drive",
            order: 1,
        }
    }
    fn provide(&self, _: &Arc<Instance>, services: &ortus_core::services::Services) {
        let drive: Arc<dyn Drive> = self.0.clone();
        services.provide("drive", drive);
    }
}

#[test]
fn attachments_go_to_the_drive() {
    for (name, base, _keep) in common::installations() {
        let saved = Arc::new(Saved::default());
        let mut apps = ortus_mail::apps();
        apps.push(Arc::new(FakeDrive(Arc::clone(&saved))));
        let instance = ortus_core::instance::testing_store::instance(Arc::clone(&base.store), apps);
        for app in instance.apps.clone() {
            app.provide(&instance, &instance.services);
        }
        let olena_token = signed_in(&instance, "olena@ministry.example", Role::Member);
        let marek_token = signed_in(&instance, "marek@ministry.example", Role::Member);
        let olena = block_on(ortus_core::auth::accounts::by_email(
            instance.store.as_ref(),
            "olena@ministry.example",
        ))
        .unwrap()
        .unwrap()
        .id;
        let inbox = block_on(store::by_role(instance.store.as_ref(), &olena, "inbox")).unwrap();
        let raw = b"From: Anna Kowalska <anna@regional.example>\r\nTo: olena@ministry.example\r\nSubject: Report\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nAttached.\r\n--b\r\nContent-Type: application/pdf; name=\"gauge.pdf\"\r\nContent-Disposition: attachment; filename=\"gauge.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERi0xLjQ=\r\n--b--\r\n";
        let m = block_on(store::deliver(
            &instance,
            &olena,
            &inbox.id,
            raw,
            &[],
            None,
            0,
        ))
        .unwrap();
        let client =
            rust_web_server::test_client::TestClient::new(ortus_core::app(Arc::clone(&instance)));
        let post = |token: &str, blob: &str| {
            client.post("/api/v1/mail/web/save-to-drive").header("Authorization", &format!("Bearer {token}")).header("X-Requested-With", "ortus").header("Content-Type", "application/json").body_text(&serde_json::json!({"blobId": blob, "name": "gauge.pdf", "type": "application/pdf"}).to_string()).send().status()
        };
        assert_eq!(
            200,
            post(&olena_token, &format!("{}.2", m.message)),
            "{name}"
        );
        let got = saved.0.lock().unwrap().clone();
        assert_eq!(
            (
                olena.clone(),
                "gauge.pdf".to_string(),
                b"%PDF-1.4".to_vec(),
                "mail".to_string(),
                Some("Attached by Anna Kowalska".to_string())
            ),
            got[0].clone(),
            "{name}"
        );
        assert_eq!(
            404,
            post(&marek_token, &format!("{}.2", m.message)),
            "{name}: not his mail"
        );
    }
}
