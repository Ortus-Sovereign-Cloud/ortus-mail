//! What Compose asks while a person writes: the labels to choose from,
//! people to send to, whether the policy lets it go (nothing recorded),
//! and attachments uploaded from the browser.

use std::sync::Arc;

use ortus_core::api::{answer, body, ok, ApiError, Method, Route};
use ortus_core::auth::Access;
use ortus_core::instance::Instance;
use ortus_core::storage::BoxFuture;
use rust_web_server::prelude::*;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::send::{self, Outgoing, Refused};

const MAX_UPLOAD: usize = 25 * 1024 * 1024;

async fn labels(request: &Request, ortus: &Arc<Instance>) -> Result<Response, ApiError> {
    ortus_core::auth::current(request, ortus)
        .await
        .ok_or_else(ApiError::sign_in)?;
    let labels: Vec<Value> = ortus_core::policy::labels(ortus.store.as_ref())
        .await?
        .into_iter()
        .map(|l| json!({"id": l.id, "name": l.name, "rank": l.rank}))
        .collect();
    Ok(ok(&json!({ "labels": labels })))
}

/// People to suggest for `q`: colleagues here, then people one has
/// written with. Each says whether they are here.
async fn people(request: &Request, ortus: &Arc<Instance>) -> Result<Response, ApiError> {
    let me = ortus_core::auth::current(request, ortus)
        .await
        .ok_or_else(ApiError::sign_in)?;
    let query = request
        .request_uri
        .split_once('?')
        .map(|(_, q)| q)
        .unwrap_or_default();
    let q = query
        .split('&')
        .find_map(|p| p.strip_prefix("q="))
        .map(|v| percent_decode(v).to_lowercase())
        .unwrap_or_default();
    let q = q.trim();
    if q.is_empty() || q.len() > 100 {
        return Ok(ok(&json!({ "people": [] })));
    }
    let like = format!("%{}%", q.replace(['%', '_', '\\'], ""));
    let store = ortus.store.as_ref();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for row in store
        .query(
            "SELECT name, email FROM core_users WHERE state = 'active' AND (LOWER(name) LIKE $1 OR LOWER(email) LIKE $1) ORDER BY name LIMIT 8",
            &[like.as_str().into()],
        )
        .await?
    {
        let email = row.text(1).unwrap_or_default().to_string();
        if seen.insert(email.clone()) {
            out.push(json!({"name": row.text(0), "email": email, "here": true}));
        }
    }
    for row in store
        .query(
            "SELECT MAX(from_name), from_addr FROM mail_messages WHERE user_id = $1 AND from_addr IS NOT NULL AND (LOWER(from_addr) LIKE $2 OR LOWER(COALESCE(from_name, '')) LIKE $2) GROUP BY from_addr ORDER BY MAX(received_at) DESC LIMIT 8",
            &[me.user.id.as_str().into(), like.as_str().into()],
        )
        .await?
    {
        let email = row.text(1).unwrap_or_default().to_string();
        if out.len() < 8 && seen.insert(email.clone()) {
            let here = crate::store::recipient(store, &email).await?.is_some();
            out.push(json!({"name": row.text(0), "email": email, "here": here}));
        }
    }
    Ok(ok(&json!({ "people": out })))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(
                    std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"),
                    16,
                ) {
                    Ok(b) => {
                        out.push(b);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Deserialize)]
struct Check {
    from: String,
    to: Vec<String>,
    label: Option<String>,
    reason: Option<String>,
}

/// Whether the message may go to everyone as it is; nothing is recorded.
async fn check(request: &Request, ortus: &Arc<Instance>) -> Result<Response, ApiError> {
    let me = ortus_core::auth::current(request, ortus)
        .await
        .ok_or_else(ApiError::sign_in)?;
    let input: Check = body(request)?;
    let out = Outgoing {
        user: &me.user.id,
        from: &input.from,
        to: &input.to,
        raw: b"",
        label: input.label.as_deref().filter(|l| !l.is_empty()),
        reason: input.reason.as_deref().filter(|r| !r.trim().is_empty()),
    };
    let answer = match send::check(ortus, &out).await {
        Ok(()) => json!({"ok": true}),
        Err(Refused::NotYourAddress) => json!({"ok": false, "problem": "not_your_address"}),
        Err(Refused::BadRecipient { address }) => {
            json!({"ok": false, "problem": "bad_recipient", "address": address})
        }
        Err(Refused::Policy { address, .. }) => {
            json!({"ok": false, "problem": "policy", "address": address})
        }
        Err(Refused::NeedsReason { address, label }) => {
            json!({"ok": false, "problem": "needs_reason", "address": address, "label": label})
        }
        Err(Refused::Failed) => return Err(ApiError::internal()),
    };
    Ok(ok(&answer))
}

/// An attachment from the browser: kept as an upload blob for Email/set.
async fn upload(request: &Request, ortus: &Arc<Instance>) -> Result<Response, ApiError> {
    let me = ortus_core::auth::current(request, ortus)
        .await
        .ok_or_else(ApiError::sign_in)?;
    let data = &request.body[..];
    if data.len() > MAX_UPLOAD {
        return Err(ApiError {
            status: 413,
            code: "too_big",
            message: "Attachments are up to 25 MB.".into(),
        });
    }
    let blob = format!("u_{}", ortus_core::auth::random_hex(12));
    let path = format!("mail/{}/uploads/{blob}", me.user.id);
    ortus.files.put(&path, data).await.map_err(|e| {
        eprintln!("ortus: mail: upload: {e}");
        ApiError::internal()
    })?;
    crate::store::track_upload(ortus.store.as_ref(), &me.user.id, &path).await?;
    Ok(ok(&json!({"blobId": blob, "size": data.len()})))
}

macro_rules! handler {
    ($name:ident, $f:ident) => {
        fn $name(
            request: Request,
            _: PathParams,
            _: ConnectionInfo,
            ortus: Arc<Instance>,
        ) -> BoxFuture<'static, Response> {
            Box::pin(async move { answer(&request, $f(&request, &ortus).await) })
        }
    };
}

handler!(h_labels, labels);
handler!(h_people, people);
handler!(h_check, check);
handler!(h_upload, upload);

fn any_object() -> Value {
    json!({"type": "object"})
}

pub fn routes() -> Vec<Route> {
    let route = |method, path, id, summary, handler| Route {
        method,
        path,
        id,
        tag: "mail",
        summary,
        access: Access::Signed,
        response: any_object,
        handler,
    };
    vec![
        route(
            Method::Get,
            "/mail/labels",
            "mailLabels",
            "The classification labels a person can give a message.",
            h_labels as ortus_core::api::Handler,
        ),
        route(
            Method::Get,
            "/mail/people",
            "mailPeople",
            "People to send to, for ?q=: colleagues, then people one has written with.",
            h_people,
        ),
        route(
            Method::Post,
            "/mail/check",
            "mailCheck",
            "Whether a message may go as it is: {from, to, label, reason}. Records nothing.",
            h_check,
        ),
        route(
            Method::Post,
            "/mail/web/upload",
            "mailWebUpload",
            "Upload an attachment (the body is the file); answers its blobId.",
            h_upload,
        ),
    ]
}

#[cfg(test)]
mod tests {
    #[test]
    fn queries_are_decoded() {
        assert_eq!("anna k", super::percent_decode("anna+k"));
        assert_eq!("ä@x", super::percent_decode("%C3%A4%40x"));
        assert_eq!("100%", super::percent_decode("100%"));
    }
}
