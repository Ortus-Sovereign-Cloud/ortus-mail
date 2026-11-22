//! Mail: store, receiving and sending, IMAP and JMAP, the inbox and compose.
//!
//! An Ortus app crate: it depends on `ortus-core` only and hands its apps
//! to a program through [`apps`].

pub mod addresses;
pub mod admin;
pub mod catchup;
pub mod compose;
pub mod domains;
pub mod imap;
pub mod import;
pub mod invites;
pub mod jmap;
pub mod later;
pub mod reports;
pub mod restore;
pub mod send;
pub mod smtp;
pub mod spam;
pub mod store;
pub mod user;

use std::borrow::Cow;
use std::sync::Arc;

use ortus_core::api::Route;
use ortus_core::app::{AdminSection, App, AppInfo, Group, JobKind, OutsideJobKind, RoleTask};
use ortus_core::instance::Instance;
use ortus_core::retention::Rules;
use ortus_core::storage::{AsyncTx, BoxFuture, Migration};

/// The app's built web client (web/dist).
#[derive(rust_embed::RustEmbed)]
#[folder = "web/dist"]
struct Web;

/// The mail app.
pub struct Mail;

impl App for Mail {
    fn info(&self) -> AppInfo {
        AppInfo {
            id: "mail",
            group: Group::Communicate,
            path: "/mail",
            order: 1,
        }
    }

    fn web_file(&self, path: &str) -> Option<Cow<'static, [u8]>> {
        Web::get(path).map(|file| file.data)
    }

    fn migrations(&self) -> &'static [Migration] {
        store::MIGRATIONS
    }

    fn routes(&self) -> Vec<Route> {
        let mut all = admin::routes();
        all.extend(user::routes());
        all.extend(jmap::routes());
        all.extend(compose::routes());
        all.extend(reports::routes());
        all.extend(import::routes());
        all.extend(invites::routes());
        all.extend(catchup::routes());
        all.extend(addresses::routes());
        all
    }

    fn admin_sections(&self) -> Vec<AdminSection> {
        vec![
            AdminSection {
                path: "/admin/domains",
                element: "ortus-mail-domains",
                label: "domains",
            },
            AdminSection {
                path: "/admin/addresses",
                element: "ortus-mail-addresses",
                label: "addresses",
            },
            AdminSection {
                path: "/admin/deliverability",
                element: "ortus-mail-deliverability",
                label: "deliverability",
            },
            AdminSection {
                path: "/admin/migrations",
                element: "ortus-mail-migrations",
                label: "migrations",
            },
        ]
    }

    fn role_tasks(&self) -> Vec<RoleTask> {
        vec![
            RoleTask {
                role: ortus_core::config::Role::Mail,
                name: "smtp",
                run: smtp::run,
            },
            RoleTask {
                role: ortus_core::config::Role::Mail,
                name: "submission",
                run: smtp::run_submission,
            },
            RoleTask {
                role: ortus_core::config::Role::Mail,
                name: "imap",
                run: imap::run,
            },
        ]
    }

    fn provide(&self, instance: &Arc<Instance>, services: &ortus_core::services::Services) {
        let mailer: Arc<dyn ortus_core::interfaces::Mailer> = Arc::new(invites::Service {
            instance: Arc::downgrade(instance),
        });
        services.provide("mail", mailer);
        // The admin console asks whether an invited address is a mailbox here.
        let domains: Arc<dyn ortus_core::interfaces::MailDomains> = Arc::new(OurDomains {
            instance: Arc::downgrade(instance),
        });
        services.provide("mail", domains);
    }

    fn sealed(&self) -> Vec<ortus_core::secrets::Place> {
        vec![
            ortus_core::secrets::Place {
                table: "mail_domains",
                id: "domain",
                column: "dkim_key",
                fields: &["rsa", "ed25519"],
            },
            ortus_core::secrets::Place {
                table: "mail_import_accounts",
                id: "id",
                column: "password",
                fields: &[],
            },
        ]
    }

    fn periodic(&self) -> Vec<(&'static str, i64)> {
        vec![
            (domains::CHECK_JOB, 10 * 60 * 1000),
            (store::WAKE_JOB, 60 * 1000),
            (import::IMPORT_WATCH_JOB, 5 * 60 * 1000),
            (store::UPLOADS_JOB, 60 * 60 * 1000),
            (store::INDEX_JOB, 5 * 60 * 1000),
        ]
    }

    fn jobs(&self, instance: &Arc<Instance>) -> Vec<JobKind> {
        let checks = Arc::clone(instance);
        let instance = Arc::clone(instance);
        vec![
            JobKind {
                kind: domains::CHECK_JOB.into(),
                handler: Box::new(move |_, _tx| {
                    // DNS lookups take seconds: they run outside the job's
                    // transaction (which holds SQLite's write lock), on
                    // their own; the next period tries again if one fails.
                    let instance = Arc::clone(&checks);
                    ortus_core::runtime::io().spawn(async move {
                        let _ = admin::check_all(&instance).await;
                    });
                    Box::pin(async { Ok(()) })
                }),
            },
            JobKind {
                kind: store::DELETE_FILES_JOB.into(),
                handler: Box::new(move |job, tx| {
                    let instance = Arc::clone(&instance);
                    let payload = job.payload.clone();
                    Box::pin(async move {
                        let blobs: Vec<String> =
                            serde_json::from_str(&payload).map_err(|e| e.to_string())?;
                        for blob in blobs {
                            if let Some(message) = store::message_of_blob(&blob) {
                                ortus_core::search::remove_in(
                                    &mut *tx,
                                    instance.store.engine(),
                                    "mail",
                                    message,
                                )
                                .await
                                .map_err(|e| e.to_string())?;
                            }
                            instance
                                .files
                                .delete(&blob)
                                .await
                                .map_err(|e| e.to_string())?;
                        }
                        Ok(())
                    })
                }),
            },
        ]
    }

    fn outside_jobs(&self, instance: &Arc<Instance>) -> Vec<OutsideJobKind> {
        let (deliver, later, wake, import) = (
            Arc::clone(instance),
            Arc::clone(instance),
            Arc::clone(instance),
            Arc::clone(instance),
        );
        vec![
            OutsideJobKind {
                kind: send::DELIVER_JOB.into(),
                handler: Box::new(move |job| {
                    let instance = Arc::clone(&deliver);
                    Box::pin(async move { send::deliver(instance, job).await })
                }),
            },
            OutsideJobKind {
                kind: later::SEND_LATER_JOB.into(),
                handler: Box::new(move |job| {
                    let instance = Arc::clone(&later);
                    Box::pin(async move { later::run(&instance, job).await })
                }),
            },
            OutsideJobKind {
                kind: import::IMPORT_JOB.into(),
                handler: Box::new(move |job| {
                    let instance = Arc::clone(&import);
                    Box::pin(async move { import::run(&instance, job).await })
                }),
            },
            OutsideJobKind {
                kind: store::INDEX_JOB.into(),
                handler: Box::new({
                    let instance = Arc::clone(instance);
                    move |_| {
                        let instance = Arc::clone(&instance);
                        Box::pin(async move { store::index_backlog(&instance).await })
                    }
                }),
            },
            OutsideJobKind {
                kind: store::UPLOADS_JOB.into(),
                handler: Box::new({
                    let instance = Arc::clone(instance);
                    move |_| {
                        let instance = Arc::clone(&instance);
                        Box::pin(async move { store::sweep_uploads(&instance).await })
                    }
                }),
            },
            OutsideJobKind {
                kind: import::IMPORT_WATCH_JOB.into(),
                handler: Box::new({
                    let instance = Arc::clone(instance);
                    move |_| {
                        let instance = Arc::clone(&instance);
                        Box::pin(async move { import::watch(&instance).await })
                    }
                }),
            },
            OutsideJobKind {
                kind: store::WAKE_JOB.into(),
                handler: Box::new(move |_| {
                    let instance = Arc::clone(&wake);
                    Box::pin(async move { store::wake_due(&instance).await })
                }),
            },
        ]
    }

    fn purge<'a>(
        &'a self,
        tx: &'a mut dyn AsyncTx,
        rules: &'a Rules,
    ) -> BoxFuture<'a, Result<u64, String>> {
        Box::pin(async move { store::purge(tx, rules).await.map_err(|e| e.to_string()) })
    }

    fn forget_user<'a>(
        &'a self,
        tx: &'a mut dyn AsyncTx,
        user_id: &'a str,
        _rules: &'a Rules,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            store::forget(tx, user_id)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }

    fn restorable(&self) -> &'static [&'static str] {
        &["folder"]
    }

    fn restore_choices<'a>(
        &'a self,
        point: &'a ortus_core::points::Snapshot,
        user: &'a str,
        _what: &'a str,
    ) -> BoxFuture<'a, Result<Vec<(String, String)>, String>> {
        Box::pin(restore::choices(point, user))
    }

    fn restore<'a>(
        &'a self,
        instance: &'a Arc<Instance>,
        point: &'a ortus_core::points::Snapshot,
        user: &'a str,
        _what: &'a str,
        id: &'a str,
        into: &'a str,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(restore::folder(instance, point, user, id, into))
    }
}

/// The apps of this crate, for a program to run.
pub fn apps() -> Vec<Arc<dyn App>> {
    vec![Arc::new(Mail)]
}

/// This installation's mail domains, for the admin console's invitations.
struct OurDomains {
    instance: std::sync::Weak<Instance>,
}

impl ortus_core::interfaces::MailDomains for OurDomains {
    fn is_ours<'a>(&'a self, domain: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let Some(instance) = self.instance.upgrade() else {
                return false;
            };
            store::is_our_domain(instance.store.as_ref(), domain)
                .await
                .unwrap_or(false)
        })
    }
}
