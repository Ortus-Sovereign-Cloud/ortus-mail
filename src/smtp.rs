//! Receiving mail: SMTP on the mail role (port 25), for our own domains
//! only. Every message is checked (SPF, DKIM, DMARC), carries the verdicts
//! in an Authentication-Results header, and is delivered to each
//! recipient's inbox; DMARC "reject" from a domain that fails is refused,
//! "quarantine" goes to Junk.
//!
//! Settings (environment): `ORTUS_SMTP_LISTEN` (default `0.0.0.0:25`),
//! `ORTUS_MAIL_TLS_CERT` and `ORTUS_MAIL_TLS_KEY` (PEM files; STARTTLS is
//! offered when both are set), `ORTUS_MAIL_MAX_SIZE` (bytes, default 25 MB).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use mail_auth::dmarc::verify::DmarcParameters;
use mail_auth::dmarc::Policy;
use mail_auth::headers::HeaderWriter;
use mail_auth::spf::verify::SpfParameters;
use mail_auth::{AuthenticatedMessage, AuthenticationResults, DmarcResult, MessageAuthenticator};
use ortus_core::instance::Instance;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use crate::store;

/// One line may be this long (RFC 5321 allows 1000; some senders exceed).
const MAX_LINE: usize = 8_192;
const MAX_RECIPIENTS: usize = 100;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(300);

fn max_size() -> usize {
    std::env::var("ORTUS_MAIL_MAX_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(25 * 1024 * 1024)
}

/// The TLS configuration for STARTTLS, when certificate and key are set.
fn tls() -> Option<Arc<tokio_rustls::rustls::ServerConfig>> {
    let cert = std::env::var("ORTUS_MAIL_TLS_CERT").ok()?;
    let key = std::env::var("ORTUS_MAIL_TLS_KEY").ok()?;
    use tokio_rustls::rustls::pki_types::pem::PemObject;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&cert)
        .ok()?
        .filter_map(Result::ok)
        .collect();
    let key = PrivateKeyDer::from_pem_file(&key).ok()?;
    let config = tokio_rustls::rustls::ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .ok()?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(|e| eprintln!("ortus: mail: the TLS certificate: {e}"))
    .ok()?;
    Some(Arc::new(config))
}

/// The role task: listens until told to stop.
pub fn run(
    instance: Arc<Instance>,
    mut stop: ortus_core::app::Stop,
) -> ortus_core::storage::BoxFuture<'static, Result<(), String>> {
    Box::pin(async move {
        let address = std::env::var("ORTUS_SMTP_LISTEN").unwrap_or_else(|_| "0.0.0.0:25".into());
        let listener = TcpListener::bind(&address)
            .await
            .map_err(|e| format!("SMTP on {address}: {e}"))?;
        println!("ortus: mail: SMTP on {address}");
        serve(instance, listener, &mut stop).await
    })
}

/// Accepts connections on `listener` until `stop` turns true.
pub async fn serve(
    instance: Arc<Instance>,
    listener: TcpListener,
    stop: &mut ortus_core::app::Stop,
) -> Result<(), String> {
    let (config, options) = crate::domains::resolver_config()?;
    let authenticator =
        Arc::new(MessageAuthenticator::new(config, options).map_err(|e| e.to_string())?);
    let tls = tls();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((socket, peer)) = accepted else { continue };
                let (instance, authenticator, tls) = (Arc::clone(&instance), Arc::clone(&authenticator), tls.clone());
                tokio::spawn(async move {
                    let session = Session::new(instance, authenticator, peer, tls.is_some());
                    if let Err(e) = session.run(socket, tls).await {
                        eprintln!("ortus: mail: SMTP from {peer}: {e}");
                    }
                });
            }
            _ = stop.changed() => return Ok(()),
        }
    }
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

struct Session {
    instance: Arc<Instance>,
    authenticator: Arc<MessageAuthenticator>,
    peer: SocketAddr,
    host: String,
    tls_offered: bool,
    secure: bool,
    helo: Option<String>,
    from: Option<String>,
    /// (address, user id)
    to: Vec<(String, String)>,
}

/// The address inside `<...>` of a MAIL or RCPT argument, and the rest.
fn path(argument: &str) -> Option<(String, &str)> {
    let start = argument.find('<')?;
    let end = argument[start..].find('>')? + start;
    Some((
        argument[start + 1..end].trim().to_string(),
        argument[end + 1..].trim(),
    ))
}

fn valid_address(address: &str) -> bool {
    let Some((local, domain)) = address.rsplit_once('@') else {
        return false;
    };
    !local.is_empty()
        && local.len() <= 64
        && crate::domains::valid_domain(domain)
        && !address.chars().any(|c| c.is_whitespace() || c.is_control())
}

impl Session {
    fn new(
        instance: Arc<Instance>,
        authenticator: Arc<MessageAuthenticator>,
        peer: SocketAddr,
        tls_offered: bool,
    ) -> Self {
        let host = crate::domains::Hosts::of(&instance).mail;
        Self {
            instance,
            authenticator,
            peer,
            host,
            tls_offered,
            secure: false,
            helo: None,
            from: None,
            to: Vec::new(),
        }
    }

    fn reset(&mut self) {
        self.from = None;
        self.to.clear();
    }

    async fn run(
        mut self,
        socket: tokio::net::TcpStream,
        tls: Option<Arc<tokio_rustls::rustls::ServerConfig>>,
    ) -> Result<(), String> {
        let mut stream: Box<dyn Stream> = Box::new(socket);
        write(&mut stream, &format!("220 {} ESMTP Ortus\r\n", self.host)).await?;
        loop {
            let mut reader = BufReader::new(stream);
            let line = match tokio::time::timeout(COMMAND_TIMEOUT, read_line(&mut reader)).await {
                Ok(Ok(Some(line))) => line,
                Ok(Ok(None)) => return Ok(()),
                Ok(Err(e)) => {
                    let mut stream = reader.into_inner();
                    let _ = write(&mut stream, "500 5.5.2 Line too long\r\n").await;
                    return Err(e);
                }
                Err(_) => {
                    let mut stream = reader.into_inner();
                    let _ = write(&mut stream, "421 4.4.2 Too slow, closing\r\n").await;
                    return Ok(());
                }
            };
            let (verb, argument) = line
                .split_once(' ')
                .map_or((line.as_str(), ""), |(v, a)| (v, a.trim()));
            let verb = verb.to_ascii_uppercase();
            match verb.as_str() {
                "EHLO" | "HELO" => {
                    self.helo = Some(argument.to_string());
                    self.reset();
                    let reply = if verb == "EHLO" {
                        let mut lines = vec![
                            format!("250-{} greets {}", self.host, argument),
                            format!("250-SIZE {}", max_size()),
                            "250-8BITMIME".into(),
                            "250-ENHANCEDSTATUSCODES".into(),
                            "250-PIPELINING".into(),
                        ];
                        if self.tls_offered && !self.secure {
                            lines.push("250-STARTTLS".into());
                        }
                        lines.push("250 SMTPUTF8".into());
                        lines.join("\r\n") + "\r\n"
                    } else {
                        format!("250 {}\r\n", self.host)
                    };
                    stream = reader.into_inner();
                    write(&mut stream, &reply).await?;
                }
                "STARTTLS" if self.tls_offered && !self.secure => {
                    stream = reader.into_inner();
                    write(&mut stream, "220 2.0.0 Ready to start TLS\r\n").await?;
                    let config = tls.clone().ok_or("no TLS")?;
                    let accepted = tokio_rustls::TlsAcceptor::from(config)
                        .accept(stream)
                        .await
                        .map_err(|e| e.to_string())?;
                    stream = Box::new(accepted);
                    self.secure = true;
                    self.helo = None;
                    self.reset();
                }
                "MAIL" => {
                    stream = reader.into_inner();
                    let reply = self.mail(argument);
                    write(&mut stream, &reply).await?;
                }
                "RCPT" => {
                    stream = reader.into_inner();
                    let reply = self.rcpt(argument).await;
                    write(&mut stream, &reply).await?;
                }
                "DATA" => {
                    if self.to.is_empty() {
                        stream = reader.into_inner();
                        write(&mut stream, "503 5.5.1 RCPT first\r\n").await?;
                        continue;
                    }
                    {
                        let inner = reader.get_mut();
                        write(inner, "354 End data with <CR><LF>.<CR><LF>\r\n").await?;
                    }
                    let data = read_data(&mut reader, max_size()).await;
                    stream = reader.into_inner();
                    let reply = match data {
                        Ok(Some(message)) => self.receive(message).await,
                        Ok(None) => "552 5.3.4 Message too big\r\n".to_string(),
                        Err(e) => return Err(e),
                    };
                    self.reset();
                    write(&mut stream, &reply).await?;
                }
                "RSET" => {
                    self.reset();
                    stream = reader.into_inner();
                    write(&mut stream, "250 2.0.0 OK\r\n").await?;
                }
                "NOOP" => {
                    stream = reader.into_inner();
                    write(&mut stream, "250 2.0.0 OK\r\n").await?;
                }
                "VRFY" => {
                    stream = reader.into_inner();
                    write(&mut stream, "252 2.1.5 Send some mail and see\r\n").await?;
                }
                "QUIT" => {
                    stream = reader.into_inner();
                    write(&mut stream, "221 2.0.0 Bye\r\n").await?;
                    return Ok(());
                }
                _ => {
                    stream = reader.into_inner();
                    write(&mut stream, "502 5.5.1 Not here\r\n").await?;
                }
            }
        }
    }

    fn mail(&mut self, argument: &str) -> String {
        if self.helo.is_none() {
            return "503 5.5.1 EHLO first\r\n".into();
        }
        if self.from.is_some() {
            return "503 5.5.1 One MAIL per message\r\n".into();
        }
        let Some(argument) = argument.strip_prefix_ci("FROM:") else {
            return "501 5.5.4 MAIL FROM:<address>\r\n".into();
        };
        let Some((address, parameters)) = path(argument) else {
            return "501 5.1.7 MAIL FROM:<address>\r\n".into();
        };
        if !address.is_empty() && !valid_address(&address) {
            return "501 5.1.7 Bad sender address\r\n".into();
        }
        for parameter in parameters.split_whitespace() {
            if let Some(size) = parameter.strip_prefix_ci("SIZE=") {
                if size.parse::<usize>().unwrap_or(0) > max_size() {
                    return "552 5.3.4 Message too big\r\n".into();
                }
            }
        }
        self.from = Some(address.to_ascii_lowercase());
        "250 2.1.0 OK\r\n".into()
    }

    async fn rcpt(&mut self, argument: &str) -> String {
        if self.from.is_none() {
            return "503 5.5.1 MAIL first\r\n".into();
        }
        let Some(argument) = argument.strip_prefix_ci("TO:") else {
            return "501 5.5.4 RCPT TO:<address>\r\n".into();
        };
        let Some((address, _)) = path(argument) else {
            return "501 5.1.3 RCPT TO:<address>\r\n".into();
        };
        let address = address.to_ascii_lowercase();
        if !valid_address(&address) {
            return "501 5.1.3 Bad recipient address\r\n".into();
        }
        if self.to.len() >= MAX_RECIPIENTS {
            return "452 4.5.3 Too many recipients\r\n".into();
        }
        let store = self.instance.store.as_ref();
        match store::recipient(store, &address).await {
            Ok(Some(user)) => {
                self.to.push((address, user));
                "250 2.1.5 OK\r\n".into()
            }
            Ok(None) => {
                let domain = address.rsplit_once('@').map(|(_, d)| d).unwrap_or_default();
                if store::is_our_domain(store, domain).await.unwrap_or(false) {
                    "550 5.1.1 No such person here\r\n".into()
                } else {
                    "550 5.7.1 Not a domain of ours; no relaying\r\n".into()
                }
            }
            Err(_) => "451 4.3.0 Try again later\r\n".into(),
        }
    }

    /// Checks and delivers one message; answers the reply line.
    async fn receive(&mut self, message: Vec<u8>) -> String {
        let ip: IpAddr = self.peer.ip();
        let from = self.from.clone().unwrap_or_default();
        let helo = self.helo.clone().unwrap_or_default();
        let Some(parsed) = AuthenticatedMessage::parse(&message) else {
            return "554 5.6.0 That is not a message\r\n".into();
        };
        let spf_params = if from.is_empty() {
            SpfParameters::helo(ip, &helo, &self.host)
        } else {
            SpfParameters::mail_from(ip, &helo, &self.host, &from)
        };
        let spf = self.authenticator.verify_spf(spf_params).await;
        let dkim = self.authenticator.verify_dkim(&parsed).await;
        let header_from = parsed.first_from_address().to_ascii_lowercase();
        let mail_from_domain = from
            .rsplit_once('@')
            .map(|(_, d)| d.to_string())
            .unwrap_or_else(|| helo.clone());
        let dmarc = self
            .authenticator
            .verify_dmarc(DmarcParameters::new(
                &parsed,
                &dkim,
                &mail_from_domain,
                &spf,
            ))
            .await;
        let failed = !matches!(dmarc.dkim_result(), DmarcResult::Pass)
            && !matches!(dmarc.spf_result(), DmarcResult::Pass);
        let policy = dmarc.policy();
        if failed && policy == Policy::Reject {
            return "550 5.7.1 DMARC: the sending domain says to reject this\r\n".into();
        }
        let junk = failed && policy == Policy::Quarantine;
        let results = AuthenticationResults::new(&self.host)
            .with_dkim_results(&dkim, &header_from)
            .with_spf_result(&spf, &spf_params)
            .with_dmarc_result(&dmarc);
        let received = format!(
            "Received: from {} ({}) by {} with {}; {}\r\n",
            helo.chars().filter(|c| !c.is_control()).collect::<String>(),
            ip,
            self.host,
            if self.secure { "ESMTPS" } else { "ESMTP" },
            mail_parser::DateTime::from_timestamp(ortus_core::program::now_ms() / 1000).to_rfc822()
        );
        let spf_word = format!("{:?}", spf.result()).to_lowercase();
        let dkim_words: Vec<String> = dkim
            .iter()
            .map(|d| {
                format!("{:?}", d.result())
                    .to_lowercase()
                    .split('(')
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        let dmarc_word = if !failed {
            "pass"
        } else if matches!(dmarc.dkim_result(), DmarcResult::None) {
            "none"
        } else {
            "fail"
        };
        let verdict = crate::spam::score(
            &message,
            &crate::spam::Auth {
                spf: &spf_word,
                dkim: &dkim_words,
                dmarc: dmarc_word,
            },
        );
        let mut stored = format!("{received}{}", results.to_header()).into_bytes();
        stored.extend_from_slice(&message);
        let verdicts = json!({
            "spf": format!("{:?}", spf.result()).to_lowercase(),
            "dkim": dkim.iter().map(|d| format!("{:?}", d.result()).to_lowercase()).collect::<Vec<_>>(),
            "dmarc": format!("{:?}", dmarc.dkim_result()).to_lowercase(),
            "from": header_from,
            "ip": ip.to_string(),
            "tls": self.secure,
        });
        let store = self.instance.store.as_ref();
        let mut delivered = 0;
        let sender = (!header_from.is_empty()).then(|| header_from.clone());
        for (_, user) in &self.to {
            // Each person's own corrections count too.
            let mine = match crate::spam::personal(store, user, sender.as_deref(), &verdict).await {
                Ok(mine) => mine,
                Err(_) => crate::spam::Verdict {
                    score: verdict.score,
                    reasons: verdict.reasons.clone(),
                },
            };
            let role = if junk || mine.junk() { "junk" } else { "inbox" };
            let Ok(mailbox) = store::by_role(store, user, role).await else {
                continue;
            };
            let mut copy = mine.header().into_bytes();
            copy.extend_from_slice(&stored);
            match store::deliver(
                &self.instance,
                user,
                &mailbox.id,
                &copy,
                &[],
                Some(&verdicts),
                (mine.score * 10.0) as i64,
            )
            .await
            {
                Ok(_) => delivered += 1,
                Err(e) => eprintln!("ortus: mail: delivering to {user}: {e}"),
            }
        }
        if delivered == 0 {
            return "451 4.3.0 Could not store the message; try again later\r\n".into();
        }
        "250 2.0.0 OK delivered\r\n".into()
    }
}

trait StripPrefixCi {
    fn strip_prefix_ci<'a>(&'a self, prefix: &str) -> Option<&'a str>;
}

impl StripPrefixCi for str {
    fn strip_prefix_ci<'a>(&'a self, prefix: &str) -> Option<&'a str> {
        (self.len() >= prefix.len() && self[..prefix.len()].eq_ignore_ascii_case(prefix))
            .then(|| self[prefix.len()..].trim_start())
    }
}

async fn write(stream: &mut Box<dyn Stream>, text: &str) -> Result<(), String> {
    stream
        .write_all(text.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stream.flush().await.map_err(|e| e.to_string())
}

/// One command line without its CRLF; `None` when the client went away.
async fn read_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<String>, String> {
    let mut line = Vec::new();
    let read = tokio::io::AsyncReadExt::take(&mut *reader, MAX_LINE as u64 + 2)
        .read_until(b'\n', &mut line)
        .await
        .map_err(|e| e.to_string())?;
    if read == 0 {
        return Ok(None);
    }
    if !line.ends_with(b"\n") {
        return Err("line too long".into());
    }
    while line.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
        line.pop();
    }
    Ok(Some(String::from_utf8_lossy(&line).into_owned()))
}

/// The message after DATA, dots unstuffed; `None` when it grew past
/// `limit` (the rest is read and dropped, so the session can go on).
async fn read_data<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    limit: usize,
) -> Result<Option<Vec<u8>>, String> {
    let mut message = Vec::new();
    let mut too_big = false;
    loop {
        let mut line = Vec::new();
        let read = tokio::time::timeout(COMMAND_TIMEOUT, reader.read_until(b'\n', &mut line))
            .await
            .map_err(|_| "timed out in DATA")?
            .map_err(|e| e.to_string())?;
        if read == 0 {
            return Err("the client went away in DATA".into());
        }
        if line == b".\r\n" || line == b".\n" {
            return Ok((!too_big).then_some(message));
        }
        let line = if line.starts_with(b"..") {
            &line[1..]
        } else {
            &line[..]
        };
        if message.len() + line.len() > limit {
            too_big = true;
            message.clear();
        }
        if !too_big {
            message.extend_from_slice(line);
        }
    }
}
