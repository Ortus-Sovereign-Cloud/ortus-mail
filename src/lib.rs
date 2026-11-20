//! Mail: store, receiving and sending, IMAP and JMAP, the inbox and compose.
//!
//! An Ortus app crate: it depends on `ortus-core` only and hands its apps
//! to a program through [`apps`].

pub mod admin;
pub mod domains;
pub mod send;
pub mod smtp;
pub mod store;

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
        admin::routes()
    }

    fn admin_sections(&self) -> Vec<AdminSection> {
        vec![AdminSection {
            path: "/admin/domains",
            element: "ortus-mail-domains",
            label: "domains",
        }]
    }

    fn role_tasks(&self) -> Vec<RoleTask> {
        vec![RoleTask {
            role: ortus_core::config::Role::Mail,
            name: "smtp",
            run: smtp::run,
        }]
    }

    fn periodic(&self) -> Vec<(&'static str, i64)> {
        vec![(domains::CHECK_JOB, 10 * 60 * 1000)]
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
                handler: Box::new(move |job, _tx| {
                    let instance = Arc::clone(&instance);
                    let payload = job.payload.clone();
                    Box::pin(async move {
                        let blobs: Vec<String> =
                            serde_json::from_str(&payload).map_err(|e| e.to_string())?;
                        for blob in blobs {
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
        let instance = Arc::clone(instance);
        vec![OutsideJobKind {
            kind: send::DELIVER_JOB.into(),
            handler: Box::new(move |job| {
                let instance = Arc::clone(&instance);
                Box::pin(async move { send::deliver(instance, job).await })
            }),
        }]
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
}

/// The apps of this crate, for a program to run.
pub fn apps() -> Vec<Arc<dyn App>> {
    vec![Arc::new(Mail)]
}
