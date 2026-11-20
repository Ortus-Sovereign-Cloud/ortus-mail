//! Deliverability (board AdminDeliverability): what happened to mail we
//! sent (delivered, deferred, bounced, by receiving provider, the queue),
//! and DMARC aggregate reports, which say who sends as our domains.
//!
//! Reports arrive at `dmarc-reports@<domain>` (the address our DMARC record
//! names) as XML, gzip or zip; they go to these tables, not to a mailbox.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::net::IpAddr;
use std::sync::Arc;

use mail_parser::MessageParser;
use ortus_core::api::{answer, ok, ApiError, Method, Route};
use ortus_core::auth::Access;
use ortus_core::instance::Instance;
use ortus_core::storage::{BoxFuture, StoreResult};
use rust_web_server::prelude::*;
use serde::Deserialize;
use serde_json::{json, Value};

/// Local parts that take reports instead of people's mail.
pub const REPORT_ADDRESSES: [&str; 2] = ["dmarc-reports", "tls-reports"];

/// Nothing decompresses past this: a report is small, a bomb is not.
const MAX_XML: u64 = 10 * 1024 * 1024;

const DAY: i64 = 86_400_000;

fn now() -> i64 {
    ortus_core::program::now_ms()
}

#[derive(Debug, Deserialize)]
struct Feedback {
    report_metadata: Metadata,
    policy_published: Published,
    #[serde(default)]
    record: Vec<Record>,
}

#[derive(Debug, Deserialize)]
struct Metadata {
    report_id: String,
    date_range: Range,
}

#[derive(Debug, Deserialize)]
struct Range {
    begin: i64,
    end: i64,
}

#[derive(Debug, Deserialize)]
struct Published {
    domain: String,
}

#[derive(Debug, Deserialize)]
struct Record {
    row: Row,
    identifiers: Identifiers,
}

#[derive(Debug, Deserialize)]
struct Row {
    source_ip: String,
    count: i64,
    policy_evaluated: Evaluated,
}

#[derive(Debug, Deserialize)]
struct Evaluated {
    #[serde(default)]
    disposition: String,
    #[serde(default)]
    dkim: String,
    #[serde(default)]
    spf: String,
}

#[derive(Debug, Deserialize)]
struct Identifiers {
    header_from: String,
}

/// Inflates a gzip file, up to the limit.
fn gunzip(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data)
        .take(MAX_XML)
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}

/// The first file of a zip archive (stored or deflated), up to the limit.
fn unzip(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 30 || &data[..4] != b"PK\x03\x04" {
        return None;
    }
    let u16_at = |i: usize| u16::from_le_bytes([data[i], data[i + 1]]) as usize;
    let u32_at =
        |i: usize| u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
    let method = u16_at(8);
    let flags = u16_at(6);
    let name_len = u16_at(26);
    let extra_len = u16_at(28);
    let start = 30 + name_len + extra_len;
    // Sizes may only be in the trailing descriptor (bit 3): read to the end.
    let size = if flags & 0x08 == 0 {
        u32_at(18)
    } else {
        data.len().saturating_sub(start)
    };
    let body = data.get(start..start.checked_add(size)?.min(data.len()))?;
    match method {
        0 => (body.len() as u64 <= MAX_XML).then(|| body.to_vec()),
        8 => {
            let mut out = Vec::new();
            flate2::read::DeflateDecoder::new(body)
                .take(MAX_XML)
                .read_to_end(&mut out)
                .ok()?;
            Some(out)
        }
        _ => None,
    }
}

/// The XML of every report in a message: attachments or the body itself.
fn report_xml(raw: &[u8]) -> Vec<Vec<u8>> {
    let Some(message) = MessageParser::default().parse(raw) else {
        return Vec::new();
    };
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut total = 0u64;
    for part in message.parts().take(20) {
        if part.is_multipart() {
            continue;
        }
        // All of one message's reports together stay under the limit.
        if total >= MAX_XML {
            break;
        }
        let kind = part
            .content_type()
            .map(|c| {
                format!("{}/{}", c.ctype(), c.subtype().unwrap_or_default()).to_ascii_lowercase()
            })
            .unwrap_or_default();
        let name = part
            .attachment_name()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let data = part.decoded();
        let xml = if kind.contains("gzip") || name.ends_with(".gz") {
            gunzip(&data)
        } else if kind.contains("zip") || name.ends_with(".zip") {
            unzip(&data)
        } else if kind.ends_with("/xml") || name.ends_with(".xml") {
            (data.len() as u64 <= MAX_XML).then(|| data.to_vec())
        } else {
            None
        };
        if let Some(xml) = xml {
            total += xml.len() as u64;
            if total <= MAX_XML {
                out.push(xml);
            }
        }
    }
    out
}

/// The name of whoever sends from `ip`: its reverse DNS, by domain.
async fn source_name(resolver: &hickory_resolver::TokioResolver, ip: &str) -> Option<String> {
    let ip: IpAddr = ip.parse().ok()?;
    let found = resolver.reverse_lookup(ip).await.ok()?;
    let name = found.answers().iter().find_map(|r| match &r.data {
        hickory_resolver::proto::rr::RData::PTR(ptr) => Some(ptr.0.to_string()),
        _ => None,
    })?;
    let labels: Vec<&str> = name.trim_end_matches('.').split('.').collect();
    (labels.len() >= 2).then(|| labels[labels.len() - 2..].join("."))
}

/// Takes the reports in a received message from `reporter` (the domain
/// that DMARC confirmed sent it; reports are kept by it and their id).
/// Answers how many were kept; reports about domains that are not ours,
/// or seen before, are dropped.
pub async fn take(instance: &Instance, raw: &[u8], reporter: &str) -> Result<usize, String> {
    let store = instance.store.as_ref();
    let resolver = crate::domains::resolver().ok();
    let mut kept = 0;
    for xml in report_xml(raw) {
        let Ok(feedback) = quick_xml::de::from_reader::<_, Feedback>(&xml[..]) else {
            continue;
        };
        let domain = feedback
            .policy_published
            .domain
            .trim()
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if !crate::store::is_our_domain(store, &domain)
            .await
            .map_err(|e| e.to_string())?
        {
            continue;
        }
        let meta = &feedback.report_metadata;
        let id = format!("dr_{}", ortus_core::auth::random_hex(12));
        let added = store
            .execute(
                "INSERT INTO mail_dmarc_reports (id, domain, org, report_id, begin_at, end_at, received_at) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (org, report_id) DO NOTHING",
                &[id.as_str().into(), domain.as_str().into(), reporter.to_ascii_lowercase().chars().take(200).collect::<String>().into(), meta.report_id.chars().take(200).collect::<String>().into(), meta.date_range.begin.clamp(0, i64::MAX / 1000).saturating_mul(1000).into(), meta.date_range.end.clamp(0, i64::MAX / 1000).saturating_mul(1000).into(), now().into()],
            )
            .await
            .map_err(|e| e.to_string())?;
        if added == 0 {
            continue;
        }
        let mut names: HashMap<String, Option<String>> = HashMap::new();
        for record in feedback.record.iter().take(10_000) {
            let ip = record.row.source_ip.trim().to_string();
            if !names.contains_key(&ip) {
                let name = match &resolver {
                    // A few hundred lookups at most, each one quick.
                    Some(r) if names.len() < 200 => {
                        tokio::time::timeout(std::time::Duration::from_secs(3), source_name(r, &ip))
                            .await
                            .ok()
                            .flatten()
                    }
                    Some(_) => None,
                    None => None,
                };
                names.insert(ip.clone(), name);
            }
            let e = &record.row.policy_evaluated;
            store
                .execute(
                    "INSERT INTO mail_dmarc_rows (report, source_ip, source_name, count, disposition, dkim, spf, header_from) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                    &[id.as_str().into(), ip.as_str().into(), names[&ip].clone().into(), record.row.count.max(0).into(), e.disposition.to_ascii_lowercase().into(), e.dkim.to_ascii_lowercase().into(), e.spf.to_ascii_lowercase().into(), record.identifiers.header_from.to_ascii_lowercase().into()],
                )
                .await
                .map_err(|e| e.to_string())?;
        }
        kept += 1;
    }
    if kept > 0 {
        if let Ok(servers) = std::env::var("ORTUS_DNS_SERVERS") {
            for server in servers.split(',').filter(|s| !s.trim().is_empty()) {
                ortus_core::outbound::note(store, "dns", server.trim()).await;
            }
        }
    }
    Ok(kept)
}

/// The addresses this installation sends from: the mail host's own.
async fn our_ips(instance: &Instance) -> Vec<String> {
    let mut ips: Vec<String> = std::env::var("ORTUS_MAIL_OUTBOUND_IPS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if let Ok(resolver) = crate::domains::resolver() {
        if let Ok(found) = resolver
            .lookup_ip(format!("{}.", crate::domains::Hosts::of(instance).mail))
            .await
        {
            ips.extend(found.iter().map(|ip| ip.to_string()));
        }
    }
    ips
}

/// Which provider runs a receiving server.
fn provider(mx: &str) -> &'static str {
    let mx = mx.to_ascii_lowercase();
    if mx.ends_with("google.com") || mx.ends_with("googlemail.com") {
        "gmail"
    } else if mx.contains("outlook.com") || mx.ends_with("microsoft.com") || mx.contains("hotmail")
    {
        "microsoft"
    } else if mx.contains("yahoodns.net") || mx.contains("yahoo.com") || mx.contains("aol.com") {
        "yahoo"
    } else {
        "corporate"
    }
}

fn percent(part: i64, whole: i64) -> Option<f64> {
    (whole > 0).then(|| (part as f64 * 1000.0 / whole as f64).round() / 10.0)
}

/// The numbers of the board for the last `days` days.
pub async fn summary(instance: &Instance, days: i64) -> StoreResult<Value> {
    let store = instance.store.as_ref();
    let now = now();
    let since = now - days * DAY;
    let rows = store
        .query(
            "SELECT state, created_at, mx, attempts FROM mail_outbound WHERE created_at >= $1",
            &[(since - days * DAY).into()],
        )
        .await?;
    let (mut sent, mut delivered, mut bounced, mut before_sent, mut before_delivered) =
        (0i64, 0i64, 0i64, 0i64, 0i64);
    let bucket = if days <= 1 { 3_600_000 } else { DAY };
    let buckets = if days <= 1 { 24 } else { days };
    let start = now - buckets * bucket;
    let mut per: BTreeMap<i64, [i64; 3]> = (0..buckets).map(|i| (i, [0, 0, 0])).collect();
    let mut providers: BTreeMap<&'static str, (i64, i64)> = BTreeMap::new();
    for r in &rows {
        let state = r.text(0).unwrap_or_default();
        let at = r.int(1).unwrap_or(0);
        if matches!(state, "cancelled") {
            continue;
        }
        let done = matches!(state, "sent" | "bounced");
        if at < since {
            if done {
                before_sent += 1;
                before_delivered += i64::from(state == "sent");
            }
            continue;
        }
        sent += 1;
        delivered += i64::from(state == "sent");
        bounced += i64::from(state == "bounced");
        if at >= start {
            let slot = per.entry((at - start) / bucket).or_insert([0, 0, 0]);
            match state {
                "sent" => slot[0] += 1,
                "bounced" => slot[2] += 1,
                _ => slot[1] += 1,
            }
        }
        if let Some(mx) = r.text(2).filter(|_| done) {
            let p = providers.entry(provider(mx)).or_insert((0, 0));
            p.0 += i64::from(state == "sent");
            p.1 += 1;
        }
    }
    let queue = store
        .query("SELECT CAST(COUNT(*) AS BIGINT), MIN(created_at), CAST(SUM(CASE WHEN attempts > 0 THEN 1 ELSE 0 END) AS BIGINT) FROM mail_outbound WHERE state IN ('queued', 'deferred')", &[])
        .await?;
    let q = queue.first();
    let finished = delivered + bounced;
    let per_day: Vec<Value> = per
        .iter()
        .map(|(i, [d, f, b])| json!({"at": start + i * bucket, "delivered": d, "deferred": f, "bounced": b}))
        .collect();
    let providers: Vec<Value> = ["gmail", "microsoft", "yahoo", "corporate"]
        .iter()
        .filter_map(|name| {
            providers.get(name).map(
                |(ok, all)| json!({"name": name, "delivered": percent(*ok, *all), "total": all}),
            )
        })
        .collect();
    Ok(json!({
        "days": days,
        "sent": sent,
        "per_day_average": if days > 0 { sent / days } else { sent },
        "delivered": percent(delivered, finished),
        "delivered_before": percent(before_delivered, before_sent),
        "bounced": percent(bounced, finished),
        "hard_bounces": bounced,
        "queue": {
            "count": q.and_then(|r| r.int(0)).unwrap_or(0),
            "oldest": q.and_then(|r| r.int(1)),
            "retrying": q.and_then(|r| r.int(2)).unwrap_or(0),
        },
        "per_day": per_day,
        "bucket": if days <= 1 { "hour" } else { "day" },
        "providers": providers,
        "senders": senders(instance, since).await?,
    }))
}

/// Who sends as each of our domains, from the DMARC reports.
async fn senders(instance: &Instance, since: i64) -> StoreResult<Value> {
    let store = instance.store.as_ref();
    let ours = our_ips(instance).await;
    let rows = store
        .query(
            "SELECT r.domain, w.source_ip, w.source_name, w.count, w.dkim, w.spf FROM mail_dmarc_rows w JOIN mail_dmarc_reports r ON r.id = w.report WHERE r.end_at >= $1",
            &[since.into()],
        )
        .await?;
    // domain -> (ours, name, result) -> messages. A result per row: DKIM
    // passed; only SPF passed (DKIM fails); neither (someone spoofing).
    let mut out: BTreeMap<String, BTreeMap<(bool, String, &'static str), i64>> = BTreeMap::new();
    for r in &rows {
        let domain = r.text(0).unwrap_or_default().to_string();
        let ip = r.text(1).unwrap_or_default();
        let is_ours = ours.iter().any(|o| o == ip);
        let name = if is_ours {
            String::new()
        } else {
            r.text(2).unwrap_or_default().to_string()
        };
        let (dkim, spf) = (
            r.text(4).unwrap_or_default() == "pass",
            r.text(5).unwrap_or_default() == "pass",
        );
        let status = if dkim {
            "pass"
        } else if spf {
            "dkim_fail"
        } else {
            "spoofing"
        };
        *out.entry(domain)
            .or_default()
            .entry((is_ours, name, status))
            .or_insert(0) += r.int(3).unwrap_or(0);
    }
    Ok(Value::Array(
        out.into_iter()
            .map(|(domain, groups)| {
                let mut groups: Vec<Value> = groups
                    .into_iter()
                    .map(|((ours, name, status), count)| json!({"ours": ours, "name": name, "count": count, "status": status}))
                    .collect();
                groups.sort_by_key(|g| (!g["ours"].as_bool().unwrap_or(false), -g["count"].as_i64().unwrap_or(0)));
                json!({"domain": domain, "groups": groups})
            })
            .collect(),
    ))
}

fn h_summary(
    request: Request,
    _: PathParams,
    _: ConnectionInfo,
    instance: Arc<Instance>,
) -> BoxFuture<'static, Response> {
    Box::pin(async move {
        let query = request
            .request_uri
            .split_once('?')
            .map(|(_, q)| q.to_string())
            .unwrap_or_default();
        let days = query
            .split('&')
            .find_map(|p| p.strip_prefix("days="))
            .and_then(|d| d.parse::<i64>().ok())
            .filter(|d| [1, 14, 90].contains(d))
            .unwrap_or(14);
        let result: Result<Response, ApiError> =
            async { Ok(ok(&summary(&instance, days).await?)) }.await;
        answer(&request, result)
    })
}

fn any_object() -> Value {
    json!({"type": "object"})
}

pub fn routes() -> Vec<Route> {
    vec![Route {
        method: Method::Get,
        path: "/mail/admin/deliverability",
        id: "mailDeliverability",
        tag: "mail",
        summary: "Deliverability for the last ?days= (1, 14 or 90): delivered, sent, bounced, the queue, per day, by receiving provider, and who sends as our domains (DMARC reports).",
        access: Access::Admin,
        response: any_object,
        handler: h_summary,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const XML: &str = r#"<?xml version="1.0"?><feedback><report_metadata><org_name>google.com</org_name><email>noreply-dmarc-support@google.com</email><report_id>123</report_id><date_range><begin>1763251200</begin><end>1763337599</end></date_range></report_metadata><policy_published><domain>ministry.example</domain><p>none</p></policy_published><record><row><source_ip>192.0.2.10</source_ip><count>40</count><policy_evaluated><disposition>none</disposition><dkim>pass</dkim><spf>pass</spf></policy_evaluated></row><identifiers><header_from>ministry.example</header_from></identifiers></record><record><row><source_ip>203.0.113.7</source_ip><count>3</count><policy_evaluated><disposition>none</disposition><dkim>fail</dkim><spf>fail</spf></policy_evaluated></row><identifiers><header_from>ministry.example</header_from></identifiers></record></feedback>"#;

    #[test]
    fn reports_parse_from_xml_gzip_and_zip() {
        let feedback: Feedback = quick_xml::de::from_str(XML).unwrap();
        assert_eq!(2, feedback.record.len());
        assert_eq!("ministry.example", feedback.policy_published.domain);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(XML.as_bytes()).unwrap();
        assert_eq!(XML.as_bytes(), gunzip(&gz.finish().unwrap()).unwrap());
        // A one-file zip, deflated.
        let mut deflated =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        deflated.write_all(XML.as_bytes()).unwrap();
        let body = deflated.finish().unwrap();
        let name = b"report.xml";
        let mut zip =
            b"PK\x03\x04\x14\x00\x00\x00\x08\x00\x00\x00\x00\x00\x00\x00\x00\x00".to_vec();
        zip.extend((body.len() as u32).to_le_bytes());
        zip.extend((XML.len() as u32).to_le_bytes());
        zip.extend((name.len() as u16).to_le_bytes());
        zip.extend(0u16.to_le_bytes());
        zip.extend(name);
        zip.extend(&body);
        assert_eq!(XML.as_bytes(), unzip(&zip).unwrap());
        assert_eq!(None, unzip(b"not a zip at all, sorry"));
    }

    #[test]
    fn bombs_stop_at_the_limit() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        let zeros = vec![0u8; 1024 * 1024];
        for _ in 0..20 {
            gz.write_all(&zeros).unwrap();
        }
        assert_eq!(
            MAX_XML as usize,
            gunzip(&gz.finish().unwrap()).unwrap().len()
        );
    }

    #[test]
    fn providers_by_server() {
        assert_eq!("gmail", provider("alt1.gmail-smtp-in.l.google.com"));
        assert_eq!(
            "microsoft",
            provider("ministry-example.mail.protection.outlook.com")
        );
        assert_eq!("yahoo", provider("mta5.am0.yahoodns.net"));
        assert_eq!("corporate", provider("mx.regional.example"));
    }
}
