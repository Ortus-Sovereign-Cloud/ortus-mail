//! Mail's part of the admin console: Domains (and later Deliverability and
//! Migrations), as API routes under /api/v1/mail/admin/.

use std::sync::Arc;

use ortus_core::api::{answer, body, ok, ApiError, Method, Route};
use ortus_core::auth::Access;
use ortus_core::instance::Instance;
use ortus_core::storage::BoxFuture;
use rust_web_server::prelude::*;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::domains;

fn failed(code: &str) -> ApiError {
    match code {
        "not_found" => ApiError::not_found("There is no such domain."),
        "bad_domain" => ApiError::bad_request("That is not a domain name."),
        "exists" => ApiError {
            status: 409,
            code: "exists",
            message: "That domain is already here.".into(),
        },
        _ => {
            eprintln!("ortus: mail domains: {code}");
            ApiError::internal()
        }
    }
}

async fn list(_: &Request, ortus: &Arc<Instance>) -> Result<Response, ApiError> {
    Ok(ok(
        &json!({ "domains": domains::list(ortus.store.as_ref()).await?, "hosts": { "mail": domains::Hosts::of(ortus).mail, "web": domains::Hosts::of(ortus).web } }),
    ))
}

#[derive(Deserialize)]
struct NewDomain {
    domain: String,
}

async fn add(request: &Request, ortus: &Arc<Instance>) -> Result<Response, ApiError> {
    let me = ortus_core::auth::current(request, ortus)
        .await
        .ok_or_else(ApiError::sign_in)?;
    let input: NewDomain = body(request)?;
    domains::add(ortus, &input.domain)
        .await
        .map_err(|e| failed(&e))?;
    let domain = input
        .domain
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    ortus_core::auth::audit(
        ortus,
        ortus_core::audit::Event::new("mail.domain_added", ortus_core::audit::Outcome::Done)
            .by(&me.user.id)
            .with(json!({ "domain": domain })),
    )
    .await;
    Ok(ok(&domains::show(ortus, &domain)
        .await
        .map_err(|e| failed(&e))?))
}

async fn show(
    _: &Request,
    params: &PathParams,
    ortus: &Arc<Instance>,
) -> Result<Response, ApiError> {
    let domain = params.get("domain").unwrap_or_default();
    Ok(ok(&domains::show(ortus, domain)
        .await
        .map_err(|e| failed(&e))?))
}

async fn check(
    _: &Request,
    params: &PathParams,
    ortus: &Arc<Instance>,
) -> Result<Response, ApiError> {
    let domain = params.get("domain").unwrap_or_default();
    domains::check(ortus, domain)
        .await
        .map_err(|e| failed(&e))?;
    Ok(ok(&domains::show(ortus, domain)
        .await
        .map_err(|e| failed(&e))?))
}

async fn zone(
    request: &Request,
    params: &PathParams,
    ortus: &Arc<Instance>,
) -> Result<Response, ApiError> {
    let _ = request;
    let domain = params.get("domain").unwrap_or_default();
    let shown = domains::show(ortus, domain).await.map_err(|e| failed(&e))?;
    let text = domains::zone(domain, &shown["records"]);
    let response = Response::text(STATUS_CODE_REASON_PHRASE.n200_ok, &text);
    Ok(ortus_core::api::with_headers(
        response,
        vec![rust_web_server::header::Header {
            name: "Content-Disposition".to_string().into(),
            value: format!("attachment; filename=\"{domain}.zone\"").into(),
        }],
    ))
}

async fn remove(
    request: &Request,
    params: &PathParams,
    ortus: &Arc<Instance>,
) -> Result<Response, ApiError> {
    let me = ortus_core::auth::current(request, ortus)
        .await
        .ok_or_else(ApiError::sign_in)?;
    let domain = params.get("domain").unwrap_or_default();
    let removed = ortus
        .store
        .execute(
            "DELETE FROM mail_domains WHERE domain = $1",
            &[domain.into()],
        )
        .await?;
    if removed == 0 {
        return Err(failed("not_found"));
    }
    ortus_core::auth::audit(
        ortus,
        ortus_core::audit::Event::new("mail.domain_removed", ortus_core::audit::Outcome::Done)
            .by(&me.user.id)
            .with(json!({ "domain": domain })),
    )
    .await;
    list(request, ortus).await
}

macro_rules! h {
    ($name:ident, $inner:ident) => {
        fn $name(
            request: Request,
            _: PathParams,
            _: ConnectionInfo,
            ortus: Arc<Instance>,
        ) -> BoxFuture<'static, Response> {
            Box::pin(async move { answer(&request, $inner(&request, &ortus).await) })
        }
    };
    ($name:ident, $inner:ident, params) => {
        fn $name(
            request: Request,
            params: PathParams,
            _: ConnectionInfo,
            ortus: Arc<Instance>,
        ) -> BoxFuture<'static, Response> {
            Box::pin(async move { answer(&request, $inner(&request, &params, &ortus).await) })
        }
    };
}

h!(h_list, list);
h!(h_add, add);
h!(h_show, show, params);
h!(h_check, check, params);
h!(h_zone, zone, params);
h!(h_remove, remove, params);

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
        r(Method::Get, "/mail/admin/domains", "listMailDomains", "The mail domains, each with its state (verified, needs_fix, waiting) and last check.", Access::Admin, h_list as ortus_core::api::Handler),
        r(Method::Post, "/mail/admin/domains", "addMailDomain", "Add a mail domain: {domain}. New DKIM keys are made; the answer lists the records to publish.", Access::Owner, h_add),
        r(Method::Get, "/mail/admin/domains/:domain", "getMailDomain", "A domain's records, as last checked: purpose, type, name, value, status (ok, missing, different, lenient), what DNS had.", Access::Admin, h_show),
        r(Method::Post, "/mail/admin/domains/:domain/check", "checkMailDomain", "Check a domain's records in DNS now.", Access::Admin, h_check),
        r(Method::Get, "/mail/admin/domains/:domain/zone", "mailDomainZone", "The domain's records as a zone file.", Access::Admin, h_zone),
        r(Method::Delete, "/mail/admin/domains/:domain", "removeMailDomain", "Remove a mail domain.", Access::Owner, h_remove),
    ]
}

/// Checks every domain (the periodic job).
pub async fn check_all(instance: &Instance) -> Result<(), String> {
    let rows = instance
        .store
        .query("SELECT domain FROM mail_domains", &[])
        .await
        .map_err(|e| e.to_string())?;
    for row in rows {
        let domain = row.text(0).unwrap_or_default().to_string();
        if let Err(e) = domains::check(instance, &domain).await {
            eprintln!("ortus: mail: checking {domain}: {e}");
        }
    }
    Ok(())
}
