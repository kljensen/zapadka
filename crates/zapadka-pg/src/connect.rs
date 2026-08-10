//! Resolving a target into a live PostgreSQL connection.
//!
//! # TLS policy
//!
//! Zapadka uses `rustls` and always verifies the server's identity when it
//! negotiates TLS. There is no mode that encrypts without checking who is on
//! the other end, because that combination protects against nothing an attacker
//! who can reach the connection cannot already do.
//!
//! The consequence is that Zapadka's `require` is stricter than `libpq`'s: a
//! server presenting a certificate Zapadka cannot verify is refused rather than
//! trusted. A private certificate authority is supplied with `sslrootcert`.
//!
//! Running unencrypted is supported — it is the normal case for a container on
//! a private network — but Zapadka says so in the report unless the target
//! asked for it with `sslmode=disable`.

use rustls::ClientConfig;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use tokio_postgres::config::SslMode;
use tokio_postgres::{Client, Config as PgConfig, NoTls};
use zapadka_core::config::TargetConfig;
use zapadka_core::error::{Error, ErrorCode, Result, io_error};

use crate::error::connection_failed;
use crate::service;

/// Where a target's connection information came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A `--uri` argument.
    CommandLine,
    /// The environment variable named by `uri_env`.
    Environment,
    /// A PostgreSQL service file entry named by `pg_service`.
    ServiceFile,
}

impl Source {
    fn describe(self) -> &'static str {
        match self {
            Self::CommandLine => "--uri",
            Self::Environment => "uri_env",
            Self::ServiceFile => "pg_service",
        }
    }
}

/// A resolved, connectable target.
#[allow(missing_debug_implementations)] // holds a tokio_postgres::Client, which is not Debug
pub struct Connection {
    /// The live client.
    pub client: Client,
    /// The database that was connected to.
    pub database: String,
    /// Whether the connection is encrypted.
    pub encrypted: bool,
    /// Whether the target explicitly asked for an unencrypted connection.
    pub encryption_opted_out: bool,
}

/// Builds the connection configuration for a target.
///
/// `uri` overrides the target's own configuration; it exists so an operator can
/// point Zapadka at a database without editing anything.
pub fn resolve(
    target_name: &str,
    target: Option<&TargetConfig>,
    uri: Option<&str>,
) -> Result<(PgConfig, Source)> {
    if let Some(uri) = uri {
        let config = parse_uri(uri)?;
        return Ok((config, Source::CommandLine));
    }

    let target = target.ok_or_else(|| {
        Error::new(
            ErrorCode::TargetUnknown,
            format!("no connection information for target {target_name:?}"),
        )
        .with_hint("declare pg_service or uri_env for the target, or pass --uri")
    })?;

    if let Some(variable) = &target.uri_env {
        let uri = std::env::var(variable).map_err(|_| {
            Error::new(
                ErrorCode::TargetInvalid,
                format!("environment variable {variable} is not set"),
            )
            .with_hint(format!(
                "target {target_name:?} takes its connection URI from {variable}"
            ))
        })?;
        return Ok((parse_uri(&uri)?, Source::Environment));
    }

    if let Some(name) = &target.pg_service {
        let settings = service::lookup(name)?;
        return Ok((from_service(&settings, name)?, Source::ServiceFile));
    }

    Err(Error::new(
        ErrorCode::TargetInvalid,
        format!("target {target_name:?} says nothing about how to connect"),
    )
    .with_hint("set pg_service or uri_env on the target, or pass --uri"))
}

/// Parses a `postgresql://` URI.
fn parse_uri(uri: &str) -> Result<PgConfig> {
    uri.parse::<PgConfig>().map_err(|error| {
        // The message can echo the URI, which may carry a password.
        Error::new(
            ErrorCode::TargetInvalid,
            format!(
                "the connection URI is not valid: {}",
                redact(&error.to_string())
            ),
        )
        .with_hint("expected a URI such as postgresql://host:5432/database")
    })
}

/// Builds a connection configuration from service-file settings.
///
/// Unknown keywords are refused rather than ignored. A misspelled `sslmode` that
/// silently did nothing would be a security failure disguised as a typo.
// Keywords are listed individually, including those handled identically, so
// this reads as the complete list of what Zapadka accepts.
#[allow(clippy::match_same_arms)]
fn from_service(settings: &service::ServiceSettings, name: &str) -> Result<PgConfig> {
    let mut config = PgConfig::new();
    for (key, value) in settings {
        match key.as_str() {
            "host" => {
                config.host(value);
            }
            "hostaddr" => {
                config.host(value);
            }
            "port" => {
                let port = value.parse::<u16>().map_err(|_| {
                    Error::new(
                        ErrorCode::TargetInvalid,
                        format!("service {name:?} has an invalid port {value:?}"),
                    )
                })?;
                config.port(port);
            }
            "dbname" => {
                config.dbname(value);
            }
            "user" => {
                config.user(value);
            }
            "password" => {
                config.password(value);
            }
            "connect_timeout" => {
                if let Ok(seconds) = value.parse::<u64>() {
                    config.connect_timeout(std::time::Duration::from_secs(seconds));
                }
            }
            "application_name" => {
                config.application_name(value);
            }
            "sslmode" => {
                config.ssl_mode(ssl_mode(value, name)?);
            }
            // Read by `tls_config`, not by tokio-postgres.
            "sslrootcert" => {}
            "service" => {}
            other => {
                return Err(Error::new(
                    ErrorCode::TargetInvalid,
                    format!("service {name:?} sets {other:?}, which Zapadka does not support"),
                )
                .with_hint(
                    "Zapadka supports host, hostaddr, port, dbname, user, password, sslmode, \
                     sslrootcert, connect_timeout, and application_name",
                ));
            }
        }
    }
    Ok(config)
}

/// Maps an `sslmode` keyword onto what Zapadka will actually do.
fn ssl_mode(value: &str, name: &str) -> Result<SslMode> {
    match value.to_lowercase().as_str() {
        "disable" => Ok(SslMode::Disable),
        "allow" | "prefer" => Ok(SslMode::Prefer),
        // Zapadka verifies whenever it encrypts, so these collapse to one mode.
        "require" | "verify-ca" | "verify-full" => Ok(SslMode::Require),
        other => Err(Error::new(
            ErrorCode::TargetInvalid,
            format!("service {name:?} sets sslmode={other:?}, which Zapadka does not understand"),
        )
        .with_hint(
            "supported values are disable, allow, prefer, require, verify-ca, and verify-full; \
             Zapadka verifies the server's identity whenever it uses TLS",
        )),
    }
}

/// Opens a connection to the target.
pub async fn connect(
    config: &PgConfig,
    source: Source,
    root_certificate: Option<&str>,
) -> Result<Connection> {
    let opted_out = config.get_ssl_mode() == SslMode::Disable;

    let client = if opted_out {
        let (client, connection) = config
            .connect(NoTls)
            .await
            .map_err(|error| connection_failed(error, source.describe()))?;
        // The task owns the socket and lives as long as the client does.
        tokio::spawn(drive(connection));
        client
    } else {
        install_crypto_provider();
        let connector =
            tokio_postgres_rustls::MakeRustlsConnect::new(tls_config(root_certificate)?);
        let (client, connection) = config
            .connect(connector)
            .await
            .map_err(|error| connection_failed(error, source.describe()))?;
        tokio::spawn(drive(connection));
        client
    };

    // Ask the server what actually happened rather than inferring it from the
    // requested mode. With `prefer`, whether TLS was negotiated depends on the
    // server, and reporting a guess would be worse than reporting nothing.
    let encrypted = if opted_out {
        false
    } else {
        client
            .query_one(
                "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
                &[],
            )
            .await
            .map(|row| row.get::<_, bool>(0))
            .unwrap_or(false)
    };

    let database = config.get_dbname().unwrap_or_default().to_owned();

    Ok(Connection {
        client,
        database,
        encrypted,
        encryption_opted_out: opted_out,
    })
}

/// Installs the process-wide `rustls` cryptography provider.
///
/// `rustls` requires exactly one, and installing it twice is an error rather
/// than a no-op, so this is idempotent by construction.
fn install_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Fails only if something else already installed a provider, which is
        // an acceptable outcome: there is one either way.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Drives a connection to completion in the background.
async fn drive<S, T>(connection: tokio_postgres::Connection<S, T>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // A connection error surfaces on the next query as a "connection closed"
    // failure, which carries far more context than logging it here would.
    let _ = connection.await;
}

/// Builds the TLS configuration.
fn tls_config(root_certificate: Option<&str>) -> Result<ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();

    match root_certificate {
        Some(path) => {
            // An explicit root replaces the public ones entirely: an operator
            // who names a private CA means that CA, not "that one as well as
            // every public authority".
            let pem = std::fs::read(path).map_err(|e| io_error(path, "read", e))?;
            let certificates = parse_pem_certificates(&pem).map_err(|message| {
                Error::new(ErrorCode::TargetInvalid, format!("{path}: {message}"))
            })?;
            for certificate in certificates {
                roots.add(certificate).map_err(|error| {
                    Error::new(
                        ErrorCode::TargetInvalid,
                        format!("{path} is not a usable certificate: {error}"),
                    )
                })?;
            }
        }
        None => {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
    }

    Ok(ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}

/// Extracts DER certificates from PEM text.
///
/// Delegated to `rustls-pki-types` rather than hand-rolled: PEM framing and
/// base64 are exactly the kind of parsing where a subtle mistake produces a
/// certificate that is accepted but wrong.
fn parse_pem_certificates(pem: &[u8]) -> std::result::Result<Vec<CertificateDer<'static>>, String> {
    let certificates: std::result::Result<Vec<_>, _> =
        CertificateDer::pem_slice_iter(pem).collect();
    let certificates =
        certificates.map_err(|error| format!("cannot read certificates: {error}"))?;
    if certificates.is_empty() {
        return Err("contains no certificates".to_owned());
    }
    Ok(certificates)
}

/// Removes anything that looks like a password from a message.
///
/// Connection errors sometimes echo the URI they failed to parse, and that URI
/// may contain a credential the user does not want in a CI log.
fn redact(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(start) = rest.find("://") {
        let (before, after) = rest.split_at(start + 3);
        out.push_str(before);
        match after.find(['@', ' ']) {
            Some(at) if after.as_bytes()[at] == b'@' => {
                let (credentials, remainder) = after.split_at(at);
                match credentials.split_once(':') {
                    Some((user, _)) => {
                        out.push_str(user);
                        out.push_str(":***");
                    }
                    None => out.push_str(credentials),
                }
                rest = remainder;
            }
            _ => {
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;

    #[test]
    fn every_encrypting_mode_verifies_the_server() {
        // There is deliberately no mode that encrypts without verifying.
        for mode in ["require", "verify-ca", "verify-full"] {
            assert_eq!(ssl_mode(mode, "s").unwrap(), SslMode::Require, "{mode}");
        }
        assert_eq!(ssl_mode("disable", "s").unwrap(), SslMode::Disable);
        assert_eq!(ssl_mode("prefer", "s").unwrap(), SslMode::Prefer);
    }

    #[test]
    fn an_unrecognized_sslmode_is_refused_rather_than_defaulted() {
        // Silently falling back would turn a typo into an unencrypted
        // connection the operator believed was protected.
        let error = ssl_mode("verify_full", "s").unwrap_err();
        assert_eq!(error.code, ErrorCode::TargetInvalid);
        assert!(error.hint().unwrap().contains("verify-full"));
    }

    #[test]
    fn unsupported_service_keywords_are_refused() {
        let mut settings = service::ServiceSettings::new();
        settings.insert("sslcompression".to_owned(), "1".to_owned());
        let error = from_service(&settings, "s").unwrap_err();
        assert_eq!(error.code, ErrorCode::TargetInvalid);
        assert!(
            error.message.contains("sslcompression"),
            "{}",
            error.message
        );
    }

    #[test]
    fn service_settings_become_a_connection_configuration() {
        let mut settings = service::ServiceSettings::new();
        for (key, value) in [
            ("host", "db.internal"),
            ("port", "5433"),
            ("dbname", "app"),
            ("user", "deployer"),
            ("sslmode", "verify-full"),
        ] {
            settings.insert(key.to_owned(), value.to_owned());
        }
        let config = from_service(&settings, "app-production").unwrap();
        assert_eq!(config.get_ports(), [5433]);
        assert_eq!(config.get_dbname(), Some("app"));
        assert_eq!(config.get_user(), Some("deployer"));
        assert_eq!(config.get_ssl_mode(), SslMode::Require);
    }

    #[test]
    fn passwords_are_redacted_out_of_error_messages() {
        assert_eq!(
            redact("invalid uri postgresql://deployer:hunter2@db/app"),
            "invalid uri postgresql://deployer:***@db/app"
        );
        // Nothing to redact is left alone.
        assert_eq!(
            redact("invalid uri postgresql://db/app"),
            "invalid uri postgresql://db/app"
        );
        assert_eq!(redact("plain message"), "plain message");
    }

    #[test]
    fn a_bad_uri_is_reported_without_echoing_its_password() {
        let error = parse_uri("postgresql://user:hunter2@host:notaport/db").unwrap_err();
        assert!(!error.message.contains("hunter2"), "{}", error.message);
    }

    #[test]
    fn a_command_line_uri_overrides_the_targets_own_configuration() {
        let target = TargetConfig {
            pg_service: Some("should-not-be-used".to_owned()),
            ..TargetConfig::default()
        };
        let (config, source) = resolve(
            "production",
            Some(&target),
            Some("postgresql://localhost:5432/app"),
        )
        .unwrap();
        assert_eq!(source, Source::CommandLine);
        assert_eq!(config.get_dbname(), Some("app"));
    }

    #[test]
    fn a_target_with_no_connection_information_says_what_to_do() {
        let error = resolve("production", Some(&TargetConfig::default()), None).unwrap_err();
        assert_eq!(error.code, ErrorCode::TargetInvalid);
        assert!(error.hint().unwrap().contains("--uri"));
    }

    #[test]
    fn a_missing_environment_variable_names_the_variable() {
        let target = TargetConfig {
            uri_env: Some("ZAPADKA_TEST_URI_THAT_IS_NOT_SET".to_owned()),
            ..TargetConfig::default()
        };
        let error = resolve("test", Some(&target), None).unwrap_err();
        assert!(
            error.message.contains("ZAPADKA_TEST_URI_THAT_IS_NOT_SET"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_certificate_file_with_no_certificate_is_reported_clearly() {
        let error = parse_pem_certificates(b"not a certificate").unwrap_err();
        assert!(error.contains("no certificates"), "{error}");
    }

    #[test]
    fn a_real_pem_certificate_is_accepted() {
        // Proves the reader is wired up: a syntactically valid PEM block
        // produces one DER certificate rather than an empty list.
        let pem = b"-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n";
        let certificates = parse_pem_certificates(pem).unwrap();
        assert_eq!(certificates.len(), 1);
        assert_eq!(certificates[0].as_ref(), &[1, 2, 3]);
    }
}
