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

use base64::Engine;
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
/// Stands for the report addresses among the recipients (no user id has it).
const REPORTS: &str = "@reports";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
const HANDSHAKE: Duration = Duration::from_secs(30);
/// Connections at once on one port.
const MAX_CONNECTIONS: usize = 1000;

pub fn max_size() -> usize {
    std::env::var("ORTUS_MAIL_MAX_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(25 * 1024 * 1024)
}

/// The TLS configuration for STARTTLS, when certificate and key are set.
pub fn tls_config() -> Option<Arc<tokio_rustls::rustls::ServerConfig>> {
    tls()
}

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

/// The submission listeners for signed-in apps: 587 (STARTTLS), and 465
/// (TLS from the start) when a certificate is set.
pub fn run_submission(
    instance: Arc<Instance>,
    mut stop: ortus_core::app::Stop,
) -> ortus_core::storage::BoxFuture<'static, Result<(), String>> {
    Box::pin(async move {
        let starttls =
            std::env::var("ORTUS_SUBMISSION_LISTEN").unwrap_or_else(|_| "0.0.0.0:587".into());
        let listener = TcpListener::bind(&starttls)
            .await
            .map_err(|e| format!("submission on {starttls}: {e}"))?;
        println!("ortus: mail: submission on {starttls}");
        if tls().is_some() {
            let implicit =
                std::env::var("ORTUS_SUBMISSIONS_LISTEN").unwrap_or_else(|_| "0.0.0.0:465".into());
            let tls_listener = TcpListener::bind(&implicit)
                .await
                .map_err(|e| format!("submission on {implicit}: {e}"))?;
            println!("ortus: mail: submission with TLS on {implicit}");
            let (instance2, mut stop2) = (Arc::clone(&instance), stop.clone());
            tokio::spawn(async move {
                if let Err(e) = serve_as(
                    instance2,
                    tls_listener,
                    &mut stop2,
                    Mode::Submission { implicit_tls: true },
                )
                .await
                {
                    eprintln!("ortus: mail: submission with TLS: {e}");
                }
            });
        }
        serve_as(
            instance,
            listener,
            &mut stop,
            Mode::Submission {
                implicit_tls: false,
            },
        )
        .await
    })
}

/// Accepts connections on `listener` until `stop` turns true.
pub async fn serve(
    instance: Arc<Instance>,
    listener: TcpListener,
    stop: &mut ortus_core::app::Stop,
) -> Result<(), String> {
    serve_as(instance, listener, stop, Mode::Receive).await
}

/// Accepts connections for `mode` on `listener` until `stop` turns true.
pub async fn serve_as(
    instance: Arc<Instance>,
    listener: TcpListener,
    stop: &mut ortus_core::app::Stop,
    mode: Mode,
) -> Result<(), String> {
    let (config, options) = crate::domains::resolver_config()?;
    let authenticator =
        Arc::new(MessageAuthenticator::new(config, options).map_err(|e| e.to_string())?);
    let tls = tls();
    let open = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((socket, peer)) = accepted else {
                    // Out of file descriptors, say: wait instead of spinning.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                };
                let Ok(permit) = Arc::clone(&open).try_acquire_owned() else { continue };
                let (instance, authenticator, tls) = (Arc::clone(&instance), Arc::clone(&authenticator), tls.clone());
                tokio::spawn(async move {
                    let _permit = permit;
                    let session = Session::new(instance, authenticator, peer, tls.is_some(), mode);
                    if let Err(e) = session.run(socket, tls).await {
                        eprintln!("ortus: mail: SMTP from {peer}: {e}");
                    }
                });
            }
            _ = stop.changed() => return Ok(()),
        }
    }
}

/// What a listener is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Mail from other servers for our domains (port 25).
    Receive,
    /// Mail from signed-in apps, to anyone (587 with STARTTLS, 465 with TLS).
    Submission { implicit_tls: bool },
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
    mode: Mode,
    /// Who signed in (submission).
    user: Option<String>,
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
        mode: Mode,
    ) -> Self {
        let host = crate::domains::Hosts::of(&instance).mail;
        Self {
            instance,
            authenticator,
            peer,
            host,
            tls_offered,
            secure: false,
            mode,
            user: None,
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
        let stream: Box<dyn Stream> = match (&self.mode, &tls) {
            // Port 465: TLS from the first byte.
            (Mode::Submission { implicit_tls: true }, Some(config)) => {
                let accepted = tokio::time::timeout(
                    HANDSHAKE,
                    tokio_rustls::TlsAcceptor::from(Arc::clone(config)).accept(socket),
                )
                .await
                .map_err(|_| "the client went away in the TLS handshake".to_string())?
                .map_err(|e| e.to_string())?;
                self.secure = true;
                Box::new(accepted)
            }
            (Mode::Submission { implicit_tls: true }, None) => {
                return Err("port 465 needs a TLS certificate".into())
            }
            _ => Box::new(socket),
        };
        // One reader for the whole session, so pipelined commands are kept.
        let mut reader = BufReader::new(stream);
        write(
            reader.get_mut(),
            &format!("220 {} ESMTP Ortus\r\n", self.host),
        )
        .await?;
        loop {
            let line = match tokio::time::timeout(COMMAND_TIMEOUT, read_line(&mut reader)).await {
                Ok(Ok(Some(line))) => line,
                Ok(Ok(None)) => return Ok(()),
                Ok(Err(e)) => {
                    let _ = write(reader.get_mut(), "500 5.5.2 Line too long\r\n").await;
                    return Err(e);
                }
                Err(_) => {
                    let _ = write(reader.get_mut(), "421 4.4.2 Too slow, closing\r\n").await;
                    return Ok(());
                }
            };
            let (verb, argument) = line
                .split_once(' ')
                .map_or((line.as_str(), ""), |(v, a)| (v, a.trim()));
            let verb = verb.to_ascii_uppercase();
            let reply = match verb.as_str() {
                "EHLO" | "HELO" => {
                    self.helo = Some(argument.to_string());
                    self.reset();
                    if verb == "EHLO" {
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
                        // Passwords only over TLS.
                        if matches!(self.mode, Mode::Submission { .. }) && self.secure {
                            lines.push("250-AUTH PLAIN LOGIN".into());
                        }
                        lines.push("250 SMTPUTF8".into());
                        lines.join("\r\n") + "\r\n"
                    } else {
                        format!("250 {}\r\n", self.host)
                    }
                }
                "STARTTLS" if self.tls_offered && !self.secure => {
                    write(reader.get_mut(), "220 2.0.0 Ready to start TLS\r\n").await?;
                    // Anything sent before the handshake is dropped (RFC 3207).
                    let plain = reader.into_inner();
                    let config = tls.clone().ok_or("no TLS")?;
                    let accepted = tokio::time::timeout(
                        HANDSHAKE,
                        tokio_rustls::TlsAcceptor::from(config).accept(plain),
                    )
                    .await
                    .map_err(|_| "the client went away in the TLS handshake".to_string())?
                    .map_err(|e| e.to_string())?;
                    reader = BufReader::new(Box::new(accepted));
                    self.secure = true;
                    self.helo = None;
                    self.user = None;
                    self.reset();
                    continue;
                }
                "AUTH" => self.auth(argument, &mut reader).await?,
                "MAIL" => self.mail(argument),
                "RCPT" => self.rcpt(argument).await,
                "DATA" if self.to.is_empty() => "503 5.5.1 RCPT first\r\n".into(),
                "DATA" => {
                    write(reader.get_mut(), "354 End data with <CR><LF>.<CR><LF>\r\n").await?;
                    let reply = match read_data(&mut reader, max_size()).await? {
                        Data::Message(message) if matches!(self.mode, Mode::Submission { .. }) => {
                            self.submit(message).await
                        }
                        Data::Message(message) => self.receive(message).await,
                        Data::TooBig => "552 5.3.4 Message too big\r\n".to_string(),
                        Data::BareLf => {
                            "554 5.6.0 Lines must end in CRLF (bare LF refused)\r\n".to_string()
                        }
                    };
                    self.reset();
                    reply
                }
                "RSET" => {
                    self.reset();
                    "250 2.0.0 OK\r\n".into()
                }
                "NOOP" => "250 2.0.0 OK\r\n".into(),
                "VRFY" => "252 2.1.5 Send some mail and see\r\n".into(),
                "QUIT" => {
                    write(reader.get_mut(), "221 2.0.0 Bye\r\n").await?;
                    return Ok(());
                }
                _ => "502 5.5.1 Not here\r\n".into(),
            };
            write(reader.get_mut(), &reply).await?;
        }
    }

    /// AUTH PLAIN or LOGIN with an app password, over TLS only.
    async fn auth(
        &mut self,
        argument: &str,
        reader: &mut BufReader<Box<dyn Stream>>,
    ) -> Result<String, String> {
        if !matches!(self.mode, Mode::Submission { .. }) {
            return Ok("502 5.5.1 Not here\r\n".into());
        }
        if !self.secure {
            return Ok("538 5.7.11 Encryption required: STARTTLS first\r\n".into());
        }
        if self.user.is_some() {
            return Ok("503 5.5.1 Already signed in\r\n".into());
        }
        let b64 = base64::engine::general_purpose::STANDARD;
        let (mechanism, initial) = argument
            .split_once(' ')
            .map_or((argument, None), |(m, i)| (m, Some(i.trim().to_string())));
        let (user, password) = match mechanism.to_ascii_uppercase().as_str() {
            "PLAIN" => {
                let response = match initial {
                    Some(i) => i,
                    None => {
                        write(reader.get_mut(), "334 \r\n").await?;
                        read_line(reader).await?.unwrap_or_default()
                    }
                };
                let decoded = b64.decode(response.trim()).unwrap_or_default();
                let mut parts = decoded.split(|b| *b == 0);
                let (_authz, user, password) = (parts.next(), parts.next(), parts.next());
                (
                    String::from_utf8_lossy(user.unwrap_or_default()).into_owned(),
                    String::from_utf8_lossy(password.unwrap_or_default()).into_owned(),
                )
            }
            "LOGIN" => {
                write(reader.get_mut(), "334 VXNlcm5hbWU6\r\n").await?;
                let user = read_line(reader).await?.unwrap_or_default();
                write(reader.get_mut(), "334 UGFzc3dvcmQ6\r\n").await?;
                let password = read_line(reader).await?.unwrap_or_default();
                (
                    String::from_utf8_lossy(&b64.decode(user.trim()).unwrap_or_default())
                        .into_owned(),
                    String::from_utf8_lossy(&b64.decode(password.trim()).unwrap_or_default())
                        .into_owned(),
                )
            }
            _ => return Ok("504 5.5.4 PLAIN or LOGIN\r\n".into()),
        };
        match ortus_core::auth::app_passwords::authenticate(
            &self.instance,
            &user,
            &password,
            &self.peer.ip().to_string(),
        )
        .await
        {
            Ok(user) => {
                self.user = Some(user.id);
                Ok("235 2.7.0 Signed in\r\n".into())
            }
            Err(ortus_core::auth::app_passwords::Refused::Blocked(_)) => {
                Ok("454 4.7.0 Too many tries; wait a while\r\n".into())
            }
            Err(ortus_core::auth::app_passwords::Refused::Wrong) => {
                Ok("535 5.7.8 That address and app password do not match\r\n".into())
            }
        }
    }

    /// A message from a signed-in app: sent as the web would send it.
    async fn submit(&mut self, message: Vec<u8>) -> String {
        let (Some(user), Some(from)) = (self.user.clone(), self.from.clone()) else {
            return "530 5.7.0 Sign in first\r\n".into();
        };
        let to: Vec<String> = self.to.iter().map(|(address, _)| address.clone()).collect();
        // Bcc never leaves with the message.
        let message = crate::send::strip_bcc(&message);
        let out = crate::send::Outgoing {
            user: &user,
            from: &from,
            to: &to,
            raw: &message,
            label: None,
            reason: None,
        };
        match crate::send::send(&self.instance, &out).await {
            Ok(_) => "250 2.0.0 OK sent\r\n".into(),
            Err(crate::send::Refused::NotYourAddress) => {
                "553 5.7.1 That is not your address\r\n".into()
            }
            Err(crate::send::Refused::BadRecipient { address }) => {
                format!("553 5.1.3 Bad recipient {address}\r\n")
            }
            Err(crate::send::Refused::Policy { address, .. }) => {
                format!("550 5.7.1 Its label does not let it go to {address}\r\n")
            }
            Err(crate::send::Refused::NeedsReason { address, .. }) => format!(
                "550 5.7.1 To send it to {address}, give a reason: send it from the web\r\n"
            ),
            Err(crate::send::Refused::Failed) => "451 4.3.0 Try again later\r\n".into(),
        }
    }

    fn mail(&mut self, argument: &str) -> String {
        if self.helo.is_none() {
            return "503 5.5.1 EHLO first\r\n".into();
        }
        if matches!(self.mode, Mode::Submission { .. }) && self.user.is_none() {
            return "530 5.7.0 Sign in first (AUTH)\r\n".into();
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
        if matches!(self.mode, Mode::Submission { .. }) {
            // A signed-in app may write to anyone; sending decides the rest.
            if !self.to.iter().any(|(a, _)| *a == address) {
                self.to.push((address, String::new()));
            }
            return "250 2.1.5 OK\r\n".into();
        }
        let store = self.instance.store.as_ref();
        let (local, domain) = address.rsplit_once('@').unwrap_or_default();
        if crate::reports::REPORT_ADDRESSES.contains(&local)
            && store::is_our_domain(store, domain).await.unwrap_or(false)
        {
            if !self.to.iter().any(|(_, u)| u == REPORTS) {
                self.to.push((address, REPORTS.into()));
            }
            return "250 2.1.5 OK\r\n".into();
        }
        match store::recipient(store, &address).await {
            Ok(Some(user))
                if store::quota_left(store, &user)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|left| left <= 0) =>
            {
                "452 4.2.2 That mailbox is full\r\n".into()
            }
            Ok(Some(user)) => {
                // One copy per person, however many of their addresses it names.
                if !self.to.iter().any(|(_, u)| *u == user) {
                    self.to.push((address, user));
                }
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
        for (_, user) in self.to.iter().filter(|(_, u)| u != REPORTS) {
            if !store::fits(store, user, message.len())
                .await
                .unwrap_or(true)
            {
                return "552 5.2.2 A mailbox it is for is full\r\n".into();
            }
        }
        // Everyone gets it, or no one does: a 451 makes the sender try
        // again, and a partial delivery would then come twice.
        let mut stored_for: Vec<(String, String)> = Vec::new();
        let mut not_stored = false;
        let sender = (!header_from.is_empty()).then(|| header_from.clone());
        for (_, user) in &self.to {
            if user == REPORTS {
                // DMARC and TLS reports are read, not filed, and only when
                // they really come from who they say (DMARC passed); after
                // the reply, so the sender does not wait for it.
                if dmarc_word == "pass" {
                    let reporter = header_from
                        .rsplit_once('@')
                        .map(|(_, d)| d.to_ascii_lowercase())
                        .unwrap_or_default();
                    let (instance, message) = (Arc::clone(&self.instance), message.clone());
                    tokio::spawn(async move {
                        if let Err(e) = crate::reports::take(&instance, &message, &reporter).await {
                            eprintln!("ortus: mail: a report: {e}");
                        }
                    });
                }
                continue;
            }
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
                not_stored = true;
                break;
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
                Ok(delivered) => stored_for.push((user.clone(), delivered.message)),
                Err(e) => {
                    eprintln!("ortus: mail: delivering to {user}: {e}");
                    not_stored = true;
                    break;
                }
            }
        }
        if not_stored {
            for (user, message) in stored_for {
                let _ = store::remove_message(&self.instance, &user, &message).await;
            }
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

/// How DATA ended.
enum Data {
    Message(Vec<u8>),
    TooBig,
    /// A line ended in a bare LF: refused, so no second message can hide
    /// in this one (SMTP smuggling).
    BareLf,
}

/// The longest line DATA takes (RFC 5321 allows 1000; mail in the wild
/// goes longer, but not this long).
const MAX_DATA_LINE: usize = 64 * 1024;

/// The message after DATA, dots unstuffed. Only CRLF.CRLF ends it; a bare
/// LF anywhere refuses the message. The rest of a message that grew past
/// `limit` is read and dropped, so the session can go on.
async fn read_data<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    limit: usize,
) -> Result<Data, String> {
    let mut message = Vec::new();
    let mut too_big = false;
    let mut bare_lf = false;
    // Only CRLF . CRLF ends it: the line before must have ended in CRLF.
    let mut after_crlf = true;
    loop {
        let mut line = Vec::new();
        // One line, at most MAX_DATA_LINE; a longer one is dropped in parts.
        let read = tokio::time::timeout(
            COMMAND_TIMEOUT,
            tokio::io::AsyncReadExt::take(&mut *reader, MAX_DATA_LINE as u64)
                .read_until(b'\n', &mut line),
        )
        .await
        .map_err(|_| "timed out in DATA")?
        .map_err(|e| e.to_string())?;
        if read == 0 {
            return Err("the client went away in DATA".into());
        }
        if !line.ends_with(b"\n") {
            too_big = true;
            message.clear();
            after_crlf = false;
            continue;
        }
        if !line.ends_with(b"\r\n") {
            bare_lf = true;
            after_crlf = false;
            continue;
        }
        let ends = after_crlf && line == b".\r\n";
        after_crlf = true;
        if ends {
            return Ok(if bare_lf {
                Data::BareLf
            } else if too_big {
                Data::TooBig
            } else {
                Data::Message(message)
            });
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
        if !too_big && !bare_lf {
            message.extend_from_slice(line);
        }
    }
}
