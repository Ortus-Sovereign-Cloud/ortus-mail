//! Mail domains: the records a domain needs (MX, SPF, DKIM, DMARC,
//! MTA-STS, TLS reports, and what mail apps use to find us), the DKIM keys
//! behind them, and checks of what DNS really says, every ten minutes and
//! on demand.
//!
//! The resolver is the system's, or `ORTUS_DNS_SERVERS` (`ip:port`,
//! comma separated), which tests point at their own DNS server.

use std::net::SocketAddr;

use aws_lc_rs::encoding::{AsDer, Pkcs8V1Der, PublicKeyX509Der};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hickory_resolver::config::{ConnectionConfig, NameServerConfig, ResolverConfig, ResolverOpts};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::proto::rr::{RData, RecordType};
use hickory_resolver::TokioResolver;
use ortus_core::instance::Instance;
use ortus_core::storage::{AsyncStore, StoreError, StoreResult};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The periodic check of every domain.
pub const CHECK_JOB: &str = "mail.check-domains";

fn now() -> i64 {
    ortus_core::program::now_ms()
}

/// The host names a domain's records point at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hosts {
    /// Where mail is received and submitted (MX, IMAP, submission).
    pub mail: String,
    /// The web client (autoconfig, MTA-STS policy, JMAP).
    pub web: String,
}

impl Hosts {
    pub fn of(instance: &Instance) -> Self {
        let web = ortus_core::outbound::host_of(&instance.config.public_url);
        let web = web
            .rsplit_once(':')
            .map_or(web.clone(), |(host, _)| host.to_string());
        let mail = std::env::var("ORTUS_MAIL_HOSTNAME")
            .ok()
            .filter(|h| !h.trim().is_empty())
            .unwrap_or_else(|| web.clone());
        Self {
            mail: mail.trim_end_matches('.').to_ascii_lowercase(),
            web,
        }
    }
}

/// A domain name we accept: letters, digits and hyphens, at least two labels.
pub fn valid_domain(domain: &str) -> bool {
    let labels: Vec<&str> = domain.split('.').collect();
    labels.len() >= 2
        && domain.len() <= 253
        && labels.iter().all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                && !l.starts_with('-')
                && !l.ends_with('-')
        })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DkimKeys {
    pub rsa_selector: String,
    pub ed_selector: String,
    /// Base64 SubjectPublicKeyInfo (RSA) and raw key (Ed25519), as DNS has them.
    pub rsa_public: String,
    pub ed_public: String,
}

/// New DKIM keys: RSA-2048 (what every receiver checks) and Ed25519.
/// Answers the public half and the private keys (PKCS#8) to seal.
pub fn new_dkim_keys(month: &str) -> Result<(DkimKeys, Vec<u8>, Vec<u8>), String> {
    let rsa = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048)
        .map_err(|_| "could not make an RSA key")?;
    let rsa_private: Pkcs8V1Der = rsa.as_der().map_err(|_| "could not encode the RSA key")?;
    let rsa_public: PublicKeyX509Der = aws_lc_rs::signature::KeyPair::public_key(&rsa)
        .as_der()
        .map_err(|_| "could not encode the RSA public key")?;
    let ed_private = mail_auth::crypto::Ed25519Key::generate_pkcs8().map_err(|e| e.to_string())?;
    let ed =
        mail_auth::crypto::Ed25519Key::from_pkcs8_der(&ed_private).map_err(|e| e.to_string())?;
    Ok((
        DkimKeys {
            rsa_selector: format!("ortus{month}r"),
            ed_selector: format!("ortus{month}e"),
            rsa_public: STANDARD.encode(rsa_public.as_ref()),
            ed_public: STANDARD.encode(ed.public_key()),
        },
        rsa_private.as_ref().to_vec(),
        ed_private,
    ))
}

/// One record a domain should have.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Expected {
    pub purpose: &'static str,
    pub kind: &'static str,
    /// Relative to the domain (`@` is the domain itself).
    pub name: String,
    pub value: String,
    /// Mail does not flow without it.
    pub required: bool,
}

/// The records for `domain`.
pub fn expected(
    domain: &str,
    hosts: &Hosts,
    keys: &DkimKeys,
    dmarc: &str,
    sts_id: &str,
) -> Vec<Expected> {
    let r = |purpose, kind, name: &str, value: String, required| Expected {
        purpose,
        kind,
        name: name.into(),
        value,
        required,
    };
    vec![
        r("receive", "MX", "@", format!("10 {}.", hosts.mail), true),
        r("spf", "TXT", "@", "v=spf1 mx -all".into(), true),
        r(
            "dkim_rsa",
            "TXT",
            &format!("{}._domainkey", keys.rsa_selector),
            format!("v=DKIM1; k=rsa; p={}", keys.rsa_public),
            true,
        ),
        r(
            "dkim_ed25519",
            "TXT",
            &format!("{}._domainkey", keys.ed_selector),
            format!("v=DKIM1; k=ed25519; p={}", keys.ed_public),
            false,
        ),
        r(
            "dmarc",
            "TXT",
            "_dmarc",
            format!("v=DMARC1; p={dmarc}; rua=mailto:dmarc-reports@{domain}"),
            true,
        ),
        r(
            "mta_sts",
            "TXT",
            "_mta-sts",
            format!("v=STSv1; id={sts_id}"),
            false,
        ),
        r(
            "mta_sts_host",
            "CNAME",
            "mta-sts",
            format!("{}.", hosts.web),
            false,
        ),
        r(
            "tls_reports",
            "TXT",
            "_smtp._tls",
            format!("v=TLSRPTv1; rua=mailto:tls-reports@{domain}"),
            false,
        ),
        r(
            "imap",
            "SRV",
            "_imaps._tcp",
            format!("0 1 993 {}.", hosts.mail),
            false,
        ),
        r(
            "submission",
            "SRV",
            "_submissions._tcp",
            format!("0 1 465 {}.", hosts.mail),
            false,
        ),
        r(
            "jmap",
            "SRV",
            "_jmap._tcp",
            format!("0 1 443 {}.", hosts.web),
            false,
        ),
        r(
            "autoconfig",
            "CNAME",
            "autoconfig",
            format!("{}.", hosts.web),
            false,
        ),
    ]
}

/// What a check found for one record.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Missing,
    /// Something else is there.
    Different,
    /// DMARC is there, but asks receivers to do nothing (p=none).
    Lenient,
}

/// The DNS servers to ask: `ORTUS_DNS_SERVERS` (`ip:port`, comma
/// separated), else the system's.
pub fn resolver_config() -> Result<(ResolverConfig, ResolverOpts), String> {
    let servers = std::env::var("ORTUS_DNS_SERVERS").unwrap_or_default();
    let (config, mut options) = if servers.trim().is_empty() {
        hickory_resolver::system_conf::read_system_conf().map_err(|e| e.to_string())?
    } else {
        let mut list = Vec::new();
        for server in servers.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let address: SocketAddr = server
                .parse()
                .map_err(|_| format!("ORTUS_DNS_SERVERS: {server} is not ip:port"))?;
            let mut udp = ConnectionConfig::udp();
            udp.port = address.port();
            let mut tcp = ConnectionConfig::tcp();
            tcp.port = address.port();
            let mut config = NameServerConfig::udp_and_tcp(address.ip());
            config.connections = vec![udp, tcp];
            list.push(config);
        }
        (
            ResolverConfig::from_name_servers(list),
            ResolverOpts::default(),
        )
    };
    options.timeout = std::time::Duration::from_secs(5);
    options.attempts = 2;
    Ok((config, options))
}

/// A resolver for the checks: no cache, the configured servers.
pub fn resolver() -> Result<TokioResolver, String> {
    let (config, mut options) = resolver_config()?;
    options.cache_size = 0;
    TokioResolver::builder_with_config(config, TokioRuntimeProvider::default())
        .with_options(options)
        .build()
        .map_err(|e| e.to_string())
}

/// What DNS says for a name and type, as text (TXT strings joined).
async fn found(resolver: &TokioResolver, name: &str, kind: RecordType) -> Vec<String> {
    let Ok(lookup) = resolver.lookup(name, kind).await else {
        return Vec::new();
    };
    lookup
        .answers()
        .iter()
        .filter(|record| record.record_type() == kind)
        .map(|record| match &record.data {
            RData::TXT(txt) => txt
                .txt_data
                .iter()
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect::<String>(),
            other => other.to_string(),
        })
        .collect()
}

fn same(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .trim_end_matches('.')
            .to_ascii_lowercase()
    };
    norm(a) == norm(b)
}

/// Judges one record against what DNS has.
pub fn judge(expected: &Expected, found: &[String]) -> Status {
    let tag = |prefix: &str| {
        found
            .iter()
            .find(|f| f.to_ascii_lowercase().starts_with(prefix))
            .cloned()
    };
    match expected.purpose {
        // SPF: one record that lets our MX send (mx, or our host).
        "spf" => match tag("v=spf1") {
            None => Status::Missing,
            Some(spf) if spf.split_whitespace().any(|t| t == "mx" || t == "+mx") => Status::Ok,
            Some(_) => Status::Different,
        },
        // DKIM: our public key, whatever the spacing.
        "dkim_rsa" | "dkim_ed25519" => {
            let key = expected.value.rsplit("p=").next().unwrap_or_default();
            match tag("v=dkim1").or_else(|| found.first().cloned()) {
                None => Status::Missing,
                Some(record)
                    if record
                        .replace([' ', '\t'], "")
                        .contains(&format!("p={key}")) =>
                {
                    Status::Ok
                }
                Some(_) => Status::Different,
            }
        }
        // DMARC: any policy counts; p=none works but protects nobody.
        "dmarc" => match tag("v=dmarc1") {
            None => Status::Missing,
            Some(d) if d.replace(' ', "").to_ascii_lowercase().contains("p=none") => {
                Status::Lenient
            }
            Some(_) => Status::Ok,
        },
        "mta_sts" => tag("v=stsv1").map_or(Status::Missing, |_| Status::Ok),
        "tls_reports" => tag("v=tlsrptv1").map_or(Status::Missing, |_| Status::Ok),
        // MX and SRV: ours among them; CNAME: exactly ours.
        _ if found.is_empty() => Status::Missing,
        _ if found.iter().any(|f| {
            same(f, &expected.value)
                || same(
                    f.split_whitespace().last().unwrap_or_default(),
                    expected.value.split_whitespace().last().unwrap_or_default(),
                )
        }) =>
        {
            Status::Ok
        }
        _ => Status::Different,
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Checked {
    #[serde(flatten)]
    pub expected: Expected,
    pub status: Status,
    pub found: Vec<String>,
}

/// The domain's state from its checks: verified (every required record
/// right, DMARC enforcing), needs_fix (some there, not all right), or
/// waiting (nothing yet); and how many fixes are left.
pub fn state(checks: &[Checked]) -> (&'static str, usize) {
    let wrong = checks
        .iter()
        .filter(|c| c.expected.required && !matches!(c.status, Status::Ok | Status::Lenient))
        .count();
    let lenient = checks
        .iter()
        .filter(|c| c.status == Status::Lenient)
        .count();
    let anything = checks.iter().any(|c| c.status != Status::Missing);
    match (wrong + lenient, anything) {
        (0, _) => ("verified", 0),
        (fixes, false) => ("waiting", fixes),
        (fixes, true) => ("needs_fix", fixes),
    }
}

struct Stored {
    keys: DkimKeys,
    sts_id: String,
}

async fn stored(store: &dyn AsyncStore, domain: &str) -> StoreResult<Option<Stored>> {
    let rows = store
        .query(
            "SELECT dkim_public, checks FROM mail_domains WHERE domain = $1",
            &[domain.into()],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let keys: DkimKeys = serde_json::from_str(row.text(0).unwrap_or_default())
        .map_err(|e| StoreError::Database(e.to_string()))?;
    let meta: Value = row
        .text(1)
        .and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or_default();
    Ok(Some(Stored {
        keys,
        sts_id: meta["sts_id"].as_str().unwrap_or_default().to_string(),
    }))
}

/// Adds a domain with new DKIM keys (sealed at rest).
pub async fn add(instance: &Instance, domain: &str) -> Result<(), String> {
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    if !valid_domain(&domain) {
        return Err("bad_domain".into());
    }
    let month = chrono_month(now());
    let (keys, rsa, ed) = new_dkim_keys(&month)?;
    let private =
        json!({ "rsa": instance.keys.seal(&rsa), "ed25519": instance.keys.seal(&ed) }).to_string();
    let meta = json!({ "dmarc": "none", "sts_id": format!("{month}01"), "checks": [] }).to_string();
    let added = instance
        .store
        .execute(
            "INSERT INTO mail_domains (domain, added_at, state, dkim_selector, dkim_key, dkim_public, checks) VALUES ($1, $2, 'waiting', $3, $4, $5, $6) ON CONFLICT (domain) DO NOTHING",
            &[domain.as_str().into(), now().into(), keys.rsa_selector.as_str().into(), private.into(), serde_json::to_string(&keys).unwrap_or_default().into(), meta.into()],
        )
        .await
        .map_err(|e| e.to_string())?;
    if added == 0 {
        return Err("exists".into());
    }
    Ok(())
}

/// `YYYYMM` of a time, for key selectors.
fn chrono_month(ms: i64) -> String {
    // Days since 1970 to a civil date (Howard Hinnant's algorithm).
    let days = ms.div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}")
}

/// Checks a domain's records now and keeps the result.
pub async fn check(instance: &Instance, domain: &str) -> Result<Vec<Checked>, String> {
    let store = instance.store.as_ref();
    let stored = stored(store, domain)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("not_found")?;
    let hosts = Hosts::of(instance);
    let resolver = resolver()?;
    // The resolver asked, not each name looked up.
    let servers = std::env::var("ORTUS_DNS_SERVERS")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "system resolver".into());
    for server in servers.split(',') {
        ortus_core::outbound::note(store, "dns", server.trim()).await;
    }
    let mut checks = Vec::new();
    for expected in expected(domain, &hosts, &stored.keys, "quarantine", &stored.sts_id) {
        let name = if expected.name == "@" {
            format!("{domain}.")
        } else {
            format!("{}.{domain}.", expected.name)
        };
        let kind = match expected.kind {
            "MX" => RecordType::MX,
            "CNAME" => RecordType::CNAME,
            "SRV" => RecordType::SRV,
            _ => RecordType::TXT,
        };
        let found = found(&resolver, &name, kind).await;
        let status = judge(&expected, &found);
        checks.push(Checked {
            expected,
            status,
            found,
        });
    }
    let (state, _) = state(&checks);
    let dmarc = checks
        .iter()
        .find(|c| c.expected.purpose == "dmarc")
        .map(|c| {
            if c.status == Status::Lenient {
                "none"
            } else {
                "quarantine"
            }
        })
        .unwrap_or("none");
    let meta = json!({ "dmarc": dmarc, "sts_id": stored.sts_id, "checks": checks });
    store
        .execute(
            "UPDATE mail_domains SET state = $1, checks = $2, checked_at = $3 WHERE domain = $4",
            &[
                state.into(),
                meta.to_string().into(),
                now().into(),
                domain.into(),
            ],
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(checks)
}

/// Every domain, with its state and when it was last checked.
pub async fn list(store: &dyn AsyncStore) -> StoreResult<Vec<Value>> {
    Ok(store
        .query("SELECT domain, state, checked_at, checks FROM mail_domains ORDER BY added_at, domain", &[])
        .await?
        .iter()
        .map(|r| {
            let meta: Value = r.text(3).and_then(|t| serde_json::from_str(t).ok()).unwrap_or_default();
            let fixes = meta["checks"].as_array().map_or(0, |c| c.iter().filter(|c| c["required"] == true && c["status"] != "ok" && c["status"] != "lenient").count()
                + c.iter().filter(|c| c["status"] == "lenient").count());
            json!({ "domain": r.text(0), "state": r.text(1), "checked_at": r.int(2), "fixes": fixes })
        })
        .collect())
}

/// One domain: the records it should have, as last checked (or not yet).
pub async fn show(instance: &Instance, domain: &str) -> Result<Value, String> {
    let store = instance.store.as_ref();
    let rows = store
        .query(
            "SELECT state, checked_at, checks FROM mail_domains WHERE domain = $1",
            &[domain.into()],
        )
        .await
        .map_err(|e| e.to_string())?;
    let row = rows.first().ok_or("not_found")?;
    let meta: Value = row
        .text(2)
        .and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or_default();
    let stored = stored(store, domain)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("not_found")?;
    let checks: Value = if meta["checks"].as_array().is_some_and(|c| !c.is_empty()) {
        meta["checks"].clone()
    } else {
        let hosts = Hosts::of(instance);
        json!(
            expected(domain, &hosts, &stored.keys, "quarantine", &stored.sts_id)
                .into_iter()
                .map(|e| Checked {
                    expected: e,
                    status: Status::Missing,
                    found: Vec::new()
                })
                .collect::<Vec<_>>()
        )
    };
    Ok(
        json!({ "domain": domain, "state": row.text(0), "checked_at": row.int(1), "records": checks, "hosts": { "mail": Hosts::of(instance).mail, "web": Hosts::of(instance).web } }),
    )
}

/// The records as a zone file to paste into a DNS provider.
pub fn zone(domain: &str, records: &Value) -> String {
    let mut out = format!("; Records for {domain}, from Ortus\n$ORIGIN {domain}.\n");
    for r in records.as_array().into_iter().flatten() {
        let name = r["name"].as_str().unwrap_or("@");
        let kind = r["kind"].as_str().unwrap_or("TXT");
        let value = r["value"].as_str().unwrap_or_default();
        let value = if kind == "TXT" {
            // TXT strings are at most 255 bytes each.
            value
                .as_bytes()
                .chunks(255)
                .map(|c| format!("\"{}\"", String::from_utf8_lossy(c)))
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            value.to_string()
        };
        out.push_str(&format!("{name}\t3600\tIN\t{kind}\t{value}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exp(purpose: &'static str, value: &str) -> Expected {
        Expected {
            purpose,
            kind: "TXT",
            name: "@".into(),
            value: value.into(),
            required: true,
        }
    }

    #[test]
    fn records_are_judged_by_meaning() {
        assert_eq!(
            Status::Ok,
            judge(&exp("spf", ""), &["v=spf1 ip4:1.2.3.4 mx -all".into()])
        );
        assert_eq!(
            Status::Different,
            judge(
                &exp("spf", ""),
                &["v=spf1 include:_spf.google.com ~all".into()]
            )
        );
        assert_eq!(
            Status::Missing,
            judge(&exp("spf", ""), &["google-site-verification=x".into()])
        );
        assert_eq!(
            Status::Lenient,
            judge(
                &exp("dmarc", ""),
                &["v=DMARC1; p=none; rua=mailto:x@y".into()]
            )
        );
        assert_eq!(
            Status::Ok,
            judge(&exp("dmarc", ""), &["v=DMARC1; p=reject".into()])
        );
        assert_eq!(
            Status::Ok,
            judge(
                &exp("dkim_rsa", "v=DKIM1; k=rsa; p=ABC"),
                &["v=DKIM1; k=rsa; p=A BC".into()]
            )
        );
        assert_eq!(
            Status::Different,
            judge(
                &exp("dkim_rsa", "v=DKIM1; k=rsa; p=ABC"),
                &["v=DKIM1; p=XYZ".into()]
            )
        );
        let mx = Expected {
            purpose: "receive",
            kind: "MX",
            name: "@".into(),
            value: "10 mail.ministry.example.".into(),
            required: true,
        };
        assert_eq!(
            Status::Ok,
            judge(
                &mx,
                &[
                    "20 MAIL.ministry.example.".into(),
                    "10 backup.example.".into()
                ]
            )
        );
        assert_eq!(
            Status::Different,
            judge(&mx, &["10 mx.other.example.".into()])
        );
    }

    #[test]
    fn months_are_civil() {
        assert_eq!("202611", chrono_month(1_795_158_041_299));
        assert_eq!("197001", chrono_month(0));
        assert_eq!("202402", chrono_month(1_709_164_800_000));
    }

    #[test]
    fn zones_split_long_text() {
        let records = json!([{ "name": "x._domainkey", "kind": "TXT", "value": "p=".to_string() + &"A".repeat(300) }]);
        let zone = zone("ministry.example", &records);
        assert!(zone.contains("\" \""), "{zone}");
    }
}
