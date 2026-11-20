//! Mail's API for each person (under /api/v1/mail/): their mailboxes and
//! messages. The web client and JMAP build on the same store.

use std::sync::Arc;

use ortus_core::api::{answer, body, ok, ApiError, Method, Route};
use ortus_core::auth::Access;
use ortus_core::instance::Instance;
use ortus_core::storage::BoxFuture;
use rust_web_server::prelude::*;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{spam, store};

#[derive(Deserialize)]
struct Junk {
    junk: bool,
}

/// "This is junk" / "Not junk": moves the message and teaches the filter
/// about its sender, for this person only.
async fn junk(
    request: &Request,
    params: &PathParams,
    ortus: &Arc<Instance>,
) -> Result<Response, ApiError> {
    let me = ortus_core::auth::current(request, ortus)
        .await
        .ok_or_else(ApiError::sign_in)?;
    let message = params.get("message").unwrap_or_default();
    let input: Junk = body(request)?;
    let store = ortus.store.as_ref();
    let sender = store::sender_of(store, &me.user.id, message)
        .await?
        .ok_or_else(|| ApiError::not_found("There is no such message."))?;
    let role = if input.junk { "junk" } else { "inbox" };
    let uid = store::move_to_role(store, &me.user.id, message, role)
        .await?
        .ok_or_else(|| ApiError::not_found("There is no such message."))?;
    spam::learn(store, &me.user.id, &sender, input.junk).await?;
    Ok(ok(&json!({ "mailbox": role, "uid": uid })))
}

fn h_junk(
    request: Request,
    params: PathParams,
    _: ConnectionInfo,
    ortus: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move { answer(&request, junk(&request, &params, &ortus).await) })
}

fn any_object() -> Value {
    json!({"type": "object"})
}

pub fn routes() -> Vec<Route> {
    vec![Route {
        method: Method::Post,
        path: "/mail/messages/:message/junk",
        id: "markJunk",
        tag: "mail",
        summary: "Mark one's message as junk ({junk: true}) or not ({junk: false}): it moves, and the filter learns about its sender for this person.",
        access: Access::Signed,
        response: any_object,
        handler: h_junk,
    }]
}
