//! A basic spam filter: a score from the authentication verdicts and a
//! few rules about the message, adjusted by what each person taught it
//! (marking a sender's mail as junk or not). Five or more goes to Junk.
//! Every reason is kept, so a person can see why something was filed.

use mail_parser::MessageParser;
use ortus_core::storage::{AsyncStore, StoreResult};
use serde::Serialize;

/// From this score on, mail goes to Junk.
pub const JUNK: f32 = 5.0;

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Verdict {
    pub score: f32,
    pub reasons: Vec<&'static str>,
}

impl Verdict {
    fn add(&mut self, points: f32, reason: &'static str) {
        self.score += points;
        self.reasons.push(reason);
    }

    pub fn junk(&self) -> bool {
        self.score >= JUNK
    }

    /// The header kept with the message.
    pub fn header(&self) -> String {
        format!(
            "X-Ortus-Spam: score={:.1}; reasons={}\r\n",
            self.score,
            if self.reasons.is_empty() {
                "none".to_string()
            } else {
                self.reasons.join(",")
            }
        )
    }
}

/// What the receiver learnt about authentication.
pub struct Auth<'a> {
    pub spf: &'a str,
    pub dkim: &'a [String],
    /// pass, fail, none
    pub dmarc: &'a str,
}

const EXECUTABLE: [&str; 12] = [
    ".exe", ".scr", ".js", ".jse", ".vbs", ".vbe", ".bat", ".cmd", ".com", ".pif", ".msi", ".hta",
];

fn host_of_url(url: &str) -> Option<String> {
    let rest = url.trim().split_once("://")?.1;
    Some(
        rest.split(['/', '?', '#', ':'])
            .next()?
            .trim_start_matches("www.")
            .to_ascii_lowercase(),
    )
}

/// Anchors whose text names one site while their link goes to another.
fn deceptive_links(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    let mut rest = lower.as_str();
    while let Some(start) = rest.find("<a ") {
        rest = &rest[start..];
        let Some(href_at) = rest.find("href=") else {
            break;
        };
        let after = &rest[href_at + 5..];
        let quote = after.chars().next().unwrap_or('"');
        let href = after
            .trim_start_matches(['"', '\''])
            .split(quote)
            .next()
            .unwrap_or_default();
        let Some(close) = rest.find('>') else { break };
        let Some(end) = rest[close..].find("</a>") else {
            break;
        };
        let text = &rest[close + 1..close + end];
        if let (Some(shown), Some(target)) = (
            host_of_url(text).or_else(|| {
                text.trim().starts_with("www.").then(|| {
                    text.trim()
                        .trim_start_matches("www.")
                        .split('/')
                        .next()
                        .unwrap_or_default()
                        .to_string()
                })
            }),
            host_of_url(href),
        ) {
            if !shown.is_empty() && shown != target && !target.ends_with(&format!(".{shown}")) {
                return true;
            }
        }
        rest = &rest[close + end..];
    }
    false
}

/// Scores a received message.
pub fn score(raw: &[u8], auth: &Auth<'_>) -> Verdict {
    let mut verdict = Verdict::default();
    match auth.spf {
        "fail" => verdict.add(3.0, "spf_fail"),
        "softfail" => verdict.add(1.0, "spf_softfail"),
        "none" | "neutral" => verdict.add(0.5, "spf_none"),
        _ => {}
    }
    if auth.dkim.iter().any(|d| d == "fail") && !auth.dkim.iter().any(|d| d == "pass") {
        verdict.add(2.0, "dkim_fail");
    } else if auth.dkim.is_empty() || auth.dkim.iter().all(|d| d == "none") {
        verdict.add(0.5, "dkim_none");
    }
    if auth.dmarc == "fail" {
        verdict.add(3.0, "dmarc_fail");
    }
    let Some(message) = MessageParser::default().parse(raw) else {
        verdict.add(2.0, "unparsable");
        return verdict;
    };
    if message.date().is_none() {
        verdict.add(1.0, "no_date");
    }
    if message.message_id().is_none() {
        verdict.add(1.0, "no_message_id");
    }
    // A display name carrying another address ("PayPal service@paypal.com").
    if let Some(from) = message.from().and_then(|l| l.first()) {
        let address_domain = from
            .address()
            .and_then(|a| a.rsplit_once('@').map(|(_, d)| d.to_ascii_lowercase()))
            .unwrap_or_default();
        if let Some(name) = from.name() {
            if let Some((_, named_domain)) = name.split_whitespace().find_map(|w| {
                w.trim_matches(|c: char| "<>()\"',;".contains(c))
                    .rsplit_once('@')
            }) {
                if !named_domain.eq_ignore_ascii_case(&address_domain) {
                    verdict.add(3.0, "display_name_spoof");
                }
            }
        }
    }
    let subject = message.subject().unwrap_or_default();
    let letters: Vec<char> = subject.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.len() >= 10 && letters.iter().all(|c| c.is_uppercase()) {
        verdict.add(1.0, "shouting_subject");
    }
    if subject.contains("!!!") || subject.contains("$$$") {
        verdict.add(1.0, "excited_subject");
    }
    let recipients: usize = [message.to(), message.cc()]
        .into_iter()
        .flatten()
        .map(|l| l.mailboxes().count())
        .sum();
    if recipients > 50 {
        verdict.add(1.0, "many_recipients");
    }
    // HTML with no plain text alternative (the parser converts HTML to
    // text bodies itself, so the declared types decide).
    let declared = String::from_utf8_lossy(raw).to_ascii_lowercase();
    if declared.contains("text/html") && !declared.contains("text/plain") {
        verdict.add(0.5, "html_only");
    }
    if (0..message.html_body().len())
        .any(|i| message.body_html(i).is_some_and(|h| deceptive_links(&h)))
    {
        verdict.add(3.0, "deceptive_link");
    }
    if message.attachments().any(|a| {
        a.attachment_name().is_some_and(|n| {
            EXECUTABLE
                .iter()
                .any(|e| n.to_ascii_lowercase().ends_with(e))
        })
    }) {
        verdict.add(6.0, "executable_attachment");
    }
    verdict
}

/// What a person taught the filter about a sender: (marked junk, marked
/// not junk).
pub async fn taught(store: &dyn AsyncStore, user: &str, sender: &str) -> StoreResult<(i64, i64)> {
    let rows = store
        .query(
            "SELECT junk, ham FROM mail_senders WHERE user_id = $1 AND address = $2",
            &[user.into(), sender.to_ascii_lowercase().into()],
        )
        .await?;
    Ok(rows
        .first()
        .map(|r| (r.int(0).unwrap_or(0), r.int(1).unwrap_or(0)))
        .unwrap_or((0, 0)))
}

/// A person's verdict for a sender, on top of the message's own.
pub async fn personal(
    store: &dyn AsyncStore,
    user: &str,
    sender: Option<&str>,
    verdict: &Verdict,
) -> StoreResult<Verdict> {
    let mut mine = Verdict {
        score: verdict.score,
        reasons: verdict.reasons.clone(),
    };
    if let Some(sender) = sender {
        let (junk, ham) = taught(store, user, sender).await?;
        if ham > junk {
            mine.add(-6.0, "sender_trusted");
        } else if junk > ham {
            mine.add(6.0, "sender_marked_junk");
        }
    }
    Ok(mine)
}

/// Remembers a correction: `junk` true for "this is junk".
pub async fn learn(
    store: &dyn AsyncStore,
    user: &str,
    sender: &str,
    junk: bool,
) -> StoreResult<()> {
    let sender = sender.to_ascii_lowercase();
    let (j, h) = if junk { (1i64, 0i64) } else { (0, 1) };
    store
        .execute(
            "INSERT INTO mail_senders (user_id, address, junk, ham) VALUES ($1, $2, $3, $4)
             ON CONFLICT (user_id, address) DO UPDATE SET junk = mail_senders.junk + excluded.junk, ham = mail_senders.ham + excluded.ham",
            &[user.into(), sender.as_str().into(), j.into(), h.into()],
        )
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: Auth = Auth {
        spf: "pass",
        dkim: &[],
        dmarc: "pass",
    };

    fn msg(headers: &str, body: &str) -> Vec<u8> {
        format!("{headers}\r\n\r\n{body}").into_bytes()
    }

    #[test]
    fn honest_mail_is_not_junk() {
        let dkim = ["pass".to_string()];
        let raw = msg("From: Maria Peters <maria@mail.example>\r\nTo: olena@ministry.example\r\nSubject: Flood damage report\r\nDate: Thu, 19 Nov 2026 08:05:00 +0000\r\nMessage-ID: <a@mail.example>", "My car was damaged.");
        let verdict = score(
            &raw,
            &Auth {
                spf: "pass",
                dkim: &dkim,
                dmarc: "pass",
            },
        );
        assert_eq!(0.0, verdict.score, "{verdict:?}");
        assert!(!verdict.junk());
    }

    #[test]
    fn phishing_adds_up() {
        let raw = msg(
            "From: \"service@bank.example\" <alerts@cheap.example>\r\nTo: olena@ministry.example\r\nSubject: VERIFY YOUR ACCOUNT NOW\r\nDate: Thu, 19 Nov 2026 08:05:00 +0000\r\nMessage-ID: <b@cheap.example>\r\nContent-Type: text/html",
            "<p>Sign in at <a href=\"https://login.cheap.example/x\">https://www.bank.example/login</a></p>",
        );
        let verdict = score(
            &raw,
            &Auth {
                spf: "softfail",
                dkim: &[],
                dmarc: "none",
            },
        );
        assert!(verdict.junk(), "{verdict:?}");
        for reason in [
            "display_name_spoof",
            "shouting_subject",
            "html_only",
            "deceptive_link",
        ] {
            assert!(verdict.reasons.contains(&reason), "{reason}: {verdict:?}");
        }
        assert!(verdict.header().starts_with("X-Ortus-Spam: score="));
    }

    #[test]
    fn executables_are_junk_by_themselves() {
        let raw = msg(
            "From: a@b.example\r\nTo: olena@ministry.example\r\nSubject: invoice\r\nDate: Thu, 19 Nov 2026 08:05:00 +0000\r\nMessage-ID: <c@b.example>\r\nContent-Type: multipart/mixed; boundary=x",
            "--x\r\nContent-Type: text/plain\r\n\r\nsee attached\r\n--x\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"invoice.pdf.exe\"\r\n\r\nMZ\r\n--x--\r\n",
        );
        let verdict = score(&raw, &GOOD);
        assert!(
            verdict.junk() && verdict.reasons.contains(&"executable_attachment"),
            "{verdict:?}"
        );
    }

    #[test]
    fn honest_links_are_not_deceptive() {
        assert!(!deceptive_links(
            "<a href=\"https://www.bank.example/login\">https://bank.example/login</a>"
        ));
        assert!(!deceptive_links("<a href=\"https://mail.bank.example/x\">bank.example</a> <a href=\"https://x.example\">click here</a>"));
        assert!(deceptive_links(
            "<a href=\"https://evil.example\">www.bank.example</a>"
        ));
    }
}
