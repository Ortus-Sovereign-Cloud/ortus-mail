//! More addresses for a person (admin console → Addresses): an address in
//! one of this installation's domains that delivers to them and that they
//! may send from, besides their account's own. So an owner who signs in
//! with an address elsewhere (a personal one) can still have mail here,
//! and anyone can have a role address (info@, press@).

use std::sync::Arc;

use ortus_core::api::{answer, body, ok, ApiError, Method, Route};
use ortus_core::auth::accounts;
use ortus_core::auth::Access;
use ortus_core::instance::Instance;
use ortus_core::storage::BoxFuture;
use rust_web_server::prelude::*;
use serde::Deserialize;
use serde_json::{json, Value};

/// The most addresses one person may have besides their own.
pub const MAX_PER_PERSON: i64 = 20;

fn refused(status: u16, code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError {
        status,
        code,
        message: message.into(),
    }
}

/// Every extra address, with its person.
async fn list(_: &Request, ortus: &Arc<Instance>) -> Result<Response, ApiError> {
    let store = ortus.store.as_ref();
    let rows = store
        .query(
            "SELECT address, user_id, created_at FROM mail_addresses WHERE kind = 'extra' ORDER BY address",
            &[],
        )
        .await?;
    let mut out = Vec::new();
    for r in &rows {
        let user_id = r.text(1).unwrap_or_default();
        let person = accounts::by_id(store, user_id).await?;
        out.push(json!({
            "address": r.text(0),
            "user_id": user_id,
            "name": person.as_ref().map(|p| p.name.clone()),
            "email": person.as_ref().map(|p| p.email.clone()),
            "created_at": r.int(2),
        }));
    }
    let domains: Vec<String> = store
        .query("SELECT domain FROM mail_domains ORDER BY domain", &[])
        .await?
        .iter()
        .filter_map(|r| r.text(0).map(str::to_string))
        .collect();
    Ok(ok(&json!({ "addresses": out, "domains": domains })))
}

#[derive(Deserialize)]
struct NewAddress {
    address: String,
    user_id: String,
}

/// Gives a person another address: in one of our domains, nobody else's
/// (another person's account address or extra address).
async fn add(request: &Request, ortus: &Arc<Instance>) -> Result<Response, ApiError> {
    let me = ortus_core::auth::current(request, ortus)
        .await
        .ok_or_else(ApiError::sign_in)?;
    let input: NewAddress = body(request)?;
    let store = ortus.store.as_ref();
    let address = input.address.trim().to_ascii_lowercase();
    let Some((local, domain)) = address.rsplit_once('@') else {
        return Err(refused(400, "bad_address", "That is not an email address."));
    };
    let plain = |c: char| c.is_ascii_alphanumeric() || "._+-".contains(c);
    if local.is_empty()
        || local.len() > 64
        || !local.chars().all(plain)
        || local.starts_with('.')
        || local.ends_with('.')
    {
        return Err(refused(400, "bad_address", "That is not an email address."));
    }
    let ours = !store
        .query(
            "SELECT domain FROM mail_domains WHERE domain = $1",
            &[domain.into()],
        )
        .await?
        .is_empty();
    if !ours {
        return Err(refused(
            400,
            "not_our_domain",
            format!("{domain} is not one of this installation's mail domains. Add it under Domains first."),
        ));
    }
    let person = accounts::by_id(store, &input.user_id)
        .await?
        .ok_or_else(|| ApiError::not_found("There is no such person."))?;
    if person.email.eq_ignore_ascii_case(&address) {
        return Err(refused(
            409,
            "taken",
            format!("{address} is already {}'s own address.", person.name),
        ));
    }
    if let Some(owner) = accounts::by_email(store, &address).await? {
        return Err(refused(
            409,
            "taken",
            format!("{address} is {}'s account address. Remove that account first, or choose another address.", owner.name),
        ));
    }
    if let Some(row) = store
        .query(
            "SELECT user_id FROM mail_addresses WHERE address = $1",
            &[address.as_str().into()],
        )
        .await?
        .first()
    {
        let holder = accounts::by_id(store, row.text(0).unwrap_or_default()).await?;
        return Err(refused(
            409,
            "taken",
            format!(
                "{address} already belongs to {}.",
                holder.map(|h| h.name).unwrap_or_default()
            ),
        ));
    }
    let count = store
        .query(
            "SELECT CAST(COUNT(*) AS BIGINT) FROM mail_addresses WHERE user_id = $1",
            &[person.id.as_str().into()],
        )
        .await?
        .first()
        .and_then(|r| r.int(0))
        .unwrap_or(0);
    if count >= MAX_PER_PERSON {
        return Err(refused(
            409,
            "too_many",
            format!("A person has at most {MAX_PER_PERSON} more addresses."),
        ));
    }
    store
        .execute(
            "INSERT INTO mail_addresses (address, user_id, kind, created_at) VALUES ($1, $2, 'extra', $3)",
            &[address.as_str().into(), person.id.as_str().into(), ortus_core::program::now_ms().into()],
        )
        .await?;
    ortus_core::auth::audit(
        ortus,
        ortus_core::audit::Event::new("mail.address_added", ortus_core::audit::Outcome::Done)
            .by(&me.user.id)
            .on(&person.id)
            .with(json!({ "address": address })),
    )
    .await;
    Ok(ok(&json!({ "address": address, "user_id": person.id })))
}

/// Takes an extra address away (mail to it is no longer delivered here).
async fn remove(
    request: &Request,
    params: &PathParams,
    ortus: &Arc<Instance>,
) -> Result<Response, ApiError> {
    let me = ortus_core::auth::current(request, ortus)
        .await
        .ok_or_else(ApiError::sign_in)?;
    let address = decoded(params.get("address").unwrap_or_default())
        .trim()
        .to_ascii_lowercase();
    let store = ortus.store.as_ref();
    let rows = store
        .query(
            "SELECT user_id FROM mail_addresses WHERE address = $1 AND kind = 'extra'",
            &[address.as_str().into()],
        )
        .await?;
    let user = rows
        .first()
        .and_then(|r| r.text(0))
        .ok_or_else(|| ApiError::not_found("There is no such address."))?
        .to_string();
    store
        .execute(
            "DELETE FROM mail_addresses WHERE address = $1 AND kind = 'extra'",
            &[address.as_str().into()],
        )
        .await?;
    ortus_core::auth::audit(
        ortus,
        ortus_core::audit::Event::new("mail.address_removed", ortus_core::audit::Outcome::Done)
            .by(&me.user.id)
            .on(&user)
            .with(json!({ "address": address })),
    )
    .await;
    Ok(ok(&json!({ "removed": address })))
}

/// A path segment with its %XX escapes undone (the page encodes the "@").
fn decoded(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(segment.get(i + 1..i + 3).unwrap_or(""), 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn h_list(
    request: Request,
    _: PathParams,
    _: ConnectionInfo,
    ortus: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move { answer(&request, list(&request, &ortus).await) })
}

fn h_add(
    request: Request,
    _: PathParams,
    _: ConnectionInfo,
    ortus: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move { answer(&request, add(&request, &ortus).await) })
}

fn h_remove(
    request: Request,
    params: PathParams,
    _: ConnectionInfo,
    ortus: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move { answer(&request, remove(&request, &params, &ortus).await) })
}

fn any_object() -> Value {
    json!({"type": "object"})
}

pub fn routes() -> Vec<Route> {
    let r = |method, path, id, summary, access, handler| Route {
        method,
        path,
        id,
        tag: "mail",
        summary,
        access,
        response: any_object,
        handler,
    };
    vec![
        r(Method::Get, "/mail/admin/addresses", "listMailAddresses", "Every address people have besides their own (address, person), and the mail domains they can be in.", Access::Admin, h_list as ortus_core::api::Handler),
        r(Method::Post, "/mail/admin/addresses", "addMailAddress", "Give a person another address: {address, user_id}. It must be in one of the mail domains and nobody else's; mail to it is theirs, and they may send from it.", Access::Admin, h_add),
        r(Method::Delete, "/mail/admin/addresses/:address", "removeMailAddress", "Take an address away from its person.", Access::Admin, h_remove),
    ]
}
