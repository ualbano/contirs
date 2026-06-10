use anyhow::{bail, Context, Result};
use lettre::message::header::ContentType;
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters, TlsParametersBuilder};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

const DEFAULT_PORT: u16 = 587;

/// Outcome of a single container update attempt, used to build the summary email.
pub enum NotificationEvent {
    Updated {
        container: String,
        old_image: String,
        new_image: String,
    },
    Failed {
        container: String,
        error: String,
    },
}

#[derive(PartialEq)]
enum Encryption {
    None,
    StartTls,
    Tls,
}

#[derive(PartialEq)]
enum NotifyOn {
    All,
    ErrorsOnly,
}

pub struct SmtpConfig {
    host: String,
    port: u16,
    username: Option<String>,
    password: Option<String>,
    from: Mailbox,
    to: Vec<Mailbox>,
    encryption: Encryption,
    notify_on: NotifyOn,
    accept_invalid_certs: bool,
}

impl SmtpConfig {
    /// Reads SMTP settings from the environment. Returns `Ok(None)` if `SMTP_HOST`
    /// is not set, meaning email notifications are disabled entirely.
    pub fn from_env() -> Result<Option<Self>> {
        let host = match std::env::var("SMTP_HOST") {
            Ok(host) if !host.is_empty() => host,
            _ => return Ok(None),
        };

        let port = match std::env::var("SMTP_PORT") {
            Ok(port) => port.parse().context("SMTP_PORT must be a valid port number")?,
            Err(_) => DEFAULT_PORT,
        };

        let username = std::env::var("SMTP_USERNAME").ok().filter(|s| !s.is_empty());
        let password = std::env::var("SMTP_PASSWORD").ok().filter(|s| !s.is_empty());

        let from = std::env::var("SMTP_FROM")
            .context("SMTP_FROM must be set when SMTP_HOST is configured")?
            .parse()
            .context("SMTP_FROM is not a valid email address")?;

        let to_raw = std::env::var("SMTP_TO")
            .context("SMTP_TO must be set when SMTP_HOST is configured")?;
        let to = to_raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.parse().with_context(|| format!("SMTP_TO contains an invalid address: {}", s)))
            .collect::<Result<Vec<Mailbox>>>()?;
        if to.is_empty() {
            bail!("SMTP_TO must contain at least one email address");
        }

        let encryption = match std::env::var("SMTP_ENCRYPTION").as_deref() {
            Ok("none") => Encryption::None,
            Ok("tls") => Encryption::Tls,
            Ok("starttls") | Err(_) => Encryption::StartTls,
            Ok(other) => bail!("Invalid SMTP_ENCRYPTION '{}' (expected starttls, tls or none)", other),
        };

        let notify_on = match std::env::var("SMTP_NOTIFY").as_deref() {
            Ok("errors") => NotifyOn::ErrorsOnly,
            Ok("all") | Err(_) => NotifyOn::All,
            Ok(other) => bail!("Invalid SMTP_NOTIFY '{}' (expected all or errors)", other),
        };

        let accept_invalid_certs = match std::env::var("SMTP_ACCEPT_INVALID_CERTS").as_deref() {
            Ok("true") => true,
            Ok("false") | Err(_) => false,
            Ok(other) => bail!(
                "Invalid SMTP_ACCEPT_INVALID_CERTS '{}' (expected true or false)",
                other
            ),
        };

        Ok(Some(SmtpConfig {
            host,
            port,
            username,
            password,
            from,
            to,
            encryption,
            notify_on,
            accept_invalid_certs,
        }))
    }

    fn tls_parameters(&self) -> Result<TlsParameters> {
        if self.accept_invalid_certs {
            Ok(TlsParametersBuilder::new(self.host.clone())
                .dangerous_accept_invalid_certs(true)
                .dangerous_accept_invalid_hostnames(true)
                .build()?)
        } else {
            Ok(TlsParameters::new(self.host.clone())?)
        }
    }

    fn build_transport(&self) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
        let tls = match self.encryption {
            Encryption::None => Tls::None,
            Encryption::StartTls => Tls::Required(self.tls_parameters()?),
            Encryption::Tls => Tls::Wrapper(self.tls_parameters()?),
        };

        let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&self.host)
            .port(self.port)
            .tls(tls);

        if let (Some(username), Some(password)) = (&self.username, &self.password) {
            builder = builder.credentials(Credentials::new(username.clone(), password.clone()));
        }

        Ok(builder.build())
    }
}

/// Sends a single summary email covering all updates and failures from this run.
/// Does nothing if there is nothing to report, or if `SMTP_NOTIFY=errors` and no
/// update failed.
pub async fn send_summary(config: &SmtpConfig, events: &[NotificationEvent]) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }

    let updated: Vec<&NotificationEvent> = events
        .iter()
        .filter(|e| matches!(e, NotificationEvent::Updated { .. }))
        .collect();
    let failed: Vec<&NotificationEvent> = events
        .iter()
        .filter(|e| matches!(e, NotificationEvent::Failed { .. }))
        .collect();

    if config.notify_on == NotifyOn::ErrorsOnly && failed.is_empty() {
        return Ok(());
    }

    let subject = match (updated.len(), failed.len()) {
        (0, f) => format!("conti: {} update(s) failed", f),
        (u, 0) => format!("conti: {} container(s) updated", u),
        (u, f) => format!("conti: {} updated, {} failed", u, f),
    };

    let mut body = String::new();

    if !updated.is_empty() {
        body.push_str("Updated:\n");
        for event in &updated {
            if let NotificationEvent::Updated { container, old_image, new_image } = event {
                body.push_str(&format!("  - {}: {} -> {}\n", container, old_image, new_image));
            }
        }
        body.push('\n');
    }

    if !failed.is_empty() {
        body.push_str("Failed:\n");
        for event in &failed {
            if let NotificationEvent::Failed { container, error } = event {
                body.push_str(&format!("  - {}: {}\n", container, error));
            }
        }
        body.push('\n');
    }

    let mut message_builder = Message::builder().from(config.from.clone()).subject(subject);
    for to in &config.to {
        message_builder = message_builder.to(to.clone());
    }

    let email = message_builder
        .header(ContentType::TEXT_PLAIN)
        .body(body)
        .context("Failed to build notification email")?;

    let transport = config.build_transport().context("Failed to configure SMTP transport")?;

    transport
        .send(email)
        .await
        .context("Failed to send notification email")?;

    Ok(())
}
