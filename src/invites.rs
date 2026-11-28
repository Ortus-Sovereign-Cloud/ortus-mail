//! Invitations by mail (iMIP, RFC 6047): the calendar sends them through
//! here ([`Service`] provides `interfaces::Mailer`), and invitations,
//! answers and cancellations that arrive are handed to the calendar, only
//! when the sender is verified. A message's invitation and the person's
//! answer show in the inbox; the answer goes back through the calendar.

use std::sync::{Arc, Weak};

use mail_parser::MessageParser;
use ortus_core::api::{answer, body, ok, ApiError, Method, Route};
use ortus_core::auth::Access;
use ortus_core::instance::Instance;
use ortus_core::interfaces::{Calendar, Invitation};
use ortus_core::storage::BoxFuture;
use rust_web_server::prelude::*;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{send, store};

/// The calendar part of a message: its METHOD and text.
pub fn calendar_part(raw: &[u8]) -> Option<(String, String)> {
    let message = MessageParser::default().parse(raw)?;
    for part in message.parts() {
        let Some(ct) = part.content_type() else {
            continue;
        };
        let kind =
            format!("{}/{}", ct.ctype(), ct.subtype().unwrap_or_default()).to_ascii_lowercase();
        let named = part
            .attachment_name()
            .is_some_and(|n| n.to_ascii_lowercase().ends_with(".ics"));
        if kind == "text/calendar" || (kind == "application/ics" || named) {
            let text = part
                .text()
                .map(|t| t.into_owned())
                .or_else(|| String::from_utf8(part.decoded().into_owned()).ok())?;
            if text.len() > 1024 * 1024 {
                return None;
            }
            let method = ct
                .attribute("method")
                .map(str::to_string)
                .or_else(|| {
                    text.lines()
                        .find_map(|l| l.trim().strip_prefix("METHOD:").map(str::to_string))
                })
                .unwrap_or_else(|| "PUBLISH".into())
                .to_ascii_uppercase();
            return Some((method, text));
        }
    }
    None
}

async fn calendar(instance: &Instance) -> Option<Arc<dyn Calendar>> {
    instance
        .services
        .get::<dyn Calendar>(&instance.switches)
        .await
        .ok()
}

/// Hands a delivered message's invitation, if any, to the calendar of
/// `user`, from `sender` (an address mail verified). Never fails delivery.
pub async fn process(instance: &Instance, user: &str, sender: &str, raw: &[u8]) {
    let Some((_, text)) = calendar_part(raw) else {
        return;
    };
    let Some(calendar) = calendar(instance).await else {
        return;
    };
    if let Err(e) = calendar.receive(user, sender, &text).await {
        // Not the organiser, not an attendee, not a calendar: just mail.
        eprintln!("ortus: mail: an invitation for {user} from {sender}: {e}");
    }
}

/// `interfaces::Mailer` for the calendar.
pub struct Service {
    pub instance: Weak<Instance>,
}

fn encoded(text: &str) -> String {
    use base64::Engine;
    if text.is_ascii() {
        text.replace(['\r', '\n'], " ")
    } else {
        format!(
            "=?UTF-8?B?{}?=",
            base64::engine::general_purpose::STANDARD.encode(text.replace(['\r', '\n'], " "))
        )
    }
}

/// Sends an invitation (or answer, or cancellation) from the person.
pub async fn send_invitation(
    instance: &Instance,
    user: &str,
    to: &[String],
    subject: &str,
    text: &str,
    ics: &str,
) -> Result<(), String> {
    let account = ortus_core::auth::accounts::by_id(instance.store.as_ref(), user)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("no_account")?;
    let from = account.email.to_ascii_lowercase();
    let method = ics
        .lines()
        .find_map(|l| l.trim().strip_prefix("METHOD:").map(str::to_string))
        .unwrap_or_else(|| "REQUEST".into())
        .to_ascii_uppercase();
    if ![
        "REQUEST", "REPLY", "CANCEL", "PUBLISH", "COUNTER", "REFRESH",
    ]
    .contains(&method.as_str())
    {
        return Err("bad_method".into());
    }
    let domain = from.rsplit_once('@').map_or("ortus.local", |(_, d)| d);
    let boundary = format!("cal{}", ortus_core::auth::random_hex(8));
    let ics = ics.replace("\r\n", "\n").replace('\n', "\r\n");
    let raw = format!(
        "From: {} <{from}>\r\nTo: {}\r\nSubject: {}\r\nDate: {}\r\nMessage-ID: <{}@{domain}>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=\"{boundary}\"\r\n\r\n--{boundary}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{}\r\n--{boundary}\r\nContent-Type: text/calendar; charset=utf-8; method={method}\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{ics}\r\n--{boundary}--\r\n",
        encoded(&account.name),
        to.iter().map(|t| format!("<{t}>")).collect::<Vec<_>>().join(", "),
        encoded(subject),
        mail_parser::DateTime::from_timestamp(ortus_core::program::now_ms() / 1000).to_rfc822(),
        ortus_core::auth::random_hex(16),
        text.replace("\r\n", "\n").replace('\n', "\r\n"),
    );
    send::send(
        instance,
        &send::Outgoing {
            user,
            from: &from,
            to,
            raw: raw.as_bytes(),
            label: None,
            reason: None,
        },
    )
    .await
    .map(|_| ())
    .map_err(|refused| crate::later::refusal_code(&refused).to_string())
}

impl ortus_core::interfaces::Mailer for Service {
    fn send_invitation<'a>(
        &'a self,
        user: &'a str,
        to: &'a [String],
        subject: &'a str,
        text: &'a str,
        ics: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let instance = self.instance.upgrade().ok_or("stopping")?;
            send_invitation(&instance, user, to, subject, text, ics).await
        })
    }
}

/// The UID of a calendar text, its lines unfolded first (long UIDs are
/// folded over lines).
pub fn uid_of(text: &str) -> Option<String> {
    text.replace("\r\n ", "")
        .replace("\r\n\t", "")
        .replace("\n ", "")
        .replace("\n\t", "")
        .lines()
        .find_map(|l| {
            let (head, value) = l.split_once(':')?;
            head.split(';')
                .next()?
                .trim()
                .eq_ignore_ascii_case("UID")
                .then(|| value.trim().to_string())
        })
}

/// What the inbox shows of a message's invitation, with the person's answer.
pub async fn shown(instance: &Instance, user: &str, raw: &[u8]) -> Option<Value> {
    let (method, text) = calendar_part(raw)?;
    let field = |name: &str| -> Option<String> {
        text.replace("\r\n ", "")
            .replace("\n ", "")
            .lines()
            .find_map(|l| {
                let (head, value) = l.split_once(':')?;
                (head.split(';').next()?.eq_ignore_ascii_case(name))
                    .then(|| value.trim().to_string())
            })
    };
    let uid = field("UID")?;
    // The sender's own answer, in a REPLY.
    let replied = text
        .replace("\r\n ", "")
        .replace("\n ", "")
        .lines()
        .find_map(|l| {
            let upper = l.to_ascii_uppercase();
            if !upper.starts_with("ATTENDEE") {
                return None;
            }
            upper
                .split([';', ':'])
                .find_map(|p| p.strip_prefix("PARTSTAT=").map(str::to_string))
        });
    let mut answer = None;
    let mut start = None;
    let mut end = None;
    let mut summary = field("SUMMARY")
        .unwrap_or_default()
        .replace("\\,", ",")
        .replace("\\;", ";");
    let mut cancelled = method == "CANCEL";
    let mut in_calendar = false;
    if let Some(calendar) = calendar(instance).await {
        // The person's copy says when, and how they answered.
        if let Ok(Some(copy)) = calendar.invitation(user, &uid).await {
            in_calendar = true;
            answer = copy.answer;
            start = copy.start_ms;
            end = copy.end_ms;
            cancelled |= copy.cancelled;
            if !copy.summary.is_empty() {
                summary = copy.summary;
            }
        }
    }
    Some(json!({
        "uid": uid,
        "method": method,
        "summary": summary,
        "inCalendar": in_calendar,
        "replied": if method == "REPLY" { replied } else { None },
        "organizer": field("ORGANIZER").map(|o| o.trim_start_matches("mailto:").trim_start_matches("MAILTO:").to_ascii_lowercase()),
        "start": start,
        "end": end,
        "answer": answer,
        "cancelled": cancelled,
    }))
}

fn invitation_json(i: &Invitation) -> Value {
    json!({"uid": i.uid, "method": i.method, "summary": i.summary, "start": i.start_ms, "end": i.end_ms, "organizer": i.organizer, "answer": i.answer, "cancelled": i.cancelled})
}

#[derive(Deserialize)]
struct Rsvp {
    /// accepted, tentative or declined
    answer: String,
}

/// Answers the invitation in one of the person's messages.
async fn rsvp(
    request: &Request,
    params: &PathParams,
    instance: &Arc<Instance>,
) -> Result<Response, ApiError> {
    let me = ortus_core::auth::current(request, instance)
        .await
        .ok_or_else(ApiError::sign_in)?;
    let input: Rsvp = body(request)?;
    let message = params.get("message").unwrap_or_default();
    let raw = store::raw(instance, &me.user.id, message)
        .await?
        .ok_or_else(|| ApiError::not_found("There is no such message."))?;
    let Some((_, text)) = calendar_part(&raw) else {
        return Err(ApiError::not_found("That message has no invitation."));
    };
    let uid = uid_of(&text).ok_or_else(|| ApiError::bad_request("The invitation has no UID."))?;
    let calendar = calendar(instance)
        .await
        .ok_or_else(|| ApiError::not_installed("calendar"))?;
    let done = calendar
        .respond(&me.user.id, &uid, &input.answer)
        .await
        .map_err(|e| match e.as_str() {
            "bad_answer" => ApiError::bad_request("The answer is accepted, tentative or declined."),
            "not_found" | "not_invited" => {
                ApiError::not_found("That invitation is not in your calendar.")
            }
            _ => ApiError {
                status: 502,
                code: "rsvp",
                message: e,
            },
        })?;
    Ok(ok(&invitation_json(&done)))
}

fn h_rsvp(
    request: Request,
    params: PathParams,
    _: ConnectionInfo,
    instance: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move { answer(&request, rsvp(&request, &params, &instance).await) })
}

#[derive(Deserialize)]
struct SaveInput {
    #[serde(rename = "blobId")]
    blob_id: String,
    name: String,
    #[serde(default, rename = "type")]
    mime: String,
}

/// Saves an attachment of one's mail into one's drive ("From email").
async fn save_to_drive(request: &Request, instance: &Arc<Instance>) -> Result<Response, ApiError> {
    let me = ortus_core::auth::current(request, instance)
        .await
        .ok_or_else(ApiError::sign_in)?;
    let input: SaveInput = body(request)?;
    let drive = instance
        .services
        .get::<dyn ortus_core::interfaces::Drive>(&instance.switches)
        .await
        .map_err(|_| ApiError::not_installed("drive"))?;
    let ctx = crate::jmap::Ctx::new(instance, &me.user);
    let bytes = crate::jmap::blob_bytes(&ctx, &input.blob_id)
        .await
        .ok_or_else(|| ApiError::not_found("There is no such attachment."))?;
    // Who sent it, from the message the attachment is in.
    let message = input.blob_id.split('.').next().unwrap_or_default();
    let sender = instance
        .store
        .query(
            "SELECT from_name, from_addr FROM mail_messages WHERE id = $1 AND user_id = $2",
            &[message.into(), me.user.id.as_str().into()],
        )
        .await?
        .first()
        .and_then(|r| {
            r.text(0)
                .filter(|n| !n.is_empty())
                .or(r.text(1))
                .map(str::to_string)
        });
    let note = sender.map(|s| format!("Attached by {s}"));
    let mime = if input.mime.is_empty() {
        "application/octet-stream"
    } else {
        input.mime.as_str()
    };
    let saved = drive
        .save(
            &me.user.id,
            &input.name,
            mime,
            &bytes,
            "mail",
            note.as_deref(),
        )
        .await
        .map_err(|e| ApiError {
            status: 409,
            code: "drive",
            message: e,
        })?;
    Ok(ok(&json!({"id": saved.id, "path": saved.path})))
}

fn h_save(
    request: Request,
    _: PathParams,
    _: ConnectionInfo,
    instance: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move { answer(&request, save_to_drive(&request, &instance).await) })
}

pub fn routes() -> Vec<Route> {
    vec![Route {
        method: Method::Post,
        path: "/mail/web/save-to-drive",
        id: "saveAttachmentToDrive",
        tag: "mail",
        summary: "Save an attachment of one's mail into one's drive: {blobId, name, type}.",
        access: Access::Signed,
        response: || json!({"type": "object"}),
        handler: h_save,
    },
    Route {
        method: Method::Post,
        path: "/mail/messages/:message/rsvp",
        id: "answerInvitation",
        tag: "mail",
        summary: "Answer the invitation in one's message: {answer: accepted, tentative or declined}. It goes into the calendar, and to the organiser.",
        access: Access::Signed,
        response: || json!({"type": "object"}),
        handler: h_rsvp,
    }]
}

#[cfg(test)]
mod tests {
    #[test]
    fn calendar_parts_are_found() {
        let raw = b"From: a@b\r\nSubject: x\r\nMIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nInvited.\r\n--b\r\nContent-Type: text/calendar; charset=utf-8; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:x\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n--b--\r\n";
        let (method, text) = super::calendar_part(raw).unwrap();
        assert_eq!("REQUEST", method);
        assert!(text.contains("UID:x"));
        assert!(super::calendar_part(b"Subject: x\r\n\r\nplain").is_none());
        let folded = "BEGIN:VEVENT\r\nUID:040000008200E00074C5B7101A82E0080000000\r\n 0B0A1B2C3D4E5F\r\nEND:VEVENT\r\n";
        assert_eq!(
            Some("040000008200E00074C5B7101A82E00800000000B0A1B2C3D4E5F".to_string()),
            super::uid_of(folded)
        );
    }
}
