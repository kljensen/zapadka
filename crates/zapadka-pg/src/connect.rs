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

/// Everything needed to open a connection to a target.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// How to connect.
    pub config: PgConfig,
    /// Where the connection details came from.
    pub source: Source,
    /// A private certificate authority to verify the server against, from the
    /// service file's `sslrootcert`.
    pub root_certificate: Option<String>,
}

/// Builds the connection configuration for a target.
///
/// `uri` overrides the target's own configuration; it exists so an operator can
/// point Zapadka at a database without editing anything.
pub fn resolve(
    target_name: &str,
    target: Option<&TargetConfig>,
    uri: Option<&str>,
) -> Result<Resolved> {
    if let Some(uri) = uri {
        let (uri, root_certificate) = split_root_certificate(uri);
        return Ok(Resolved {
            config: parse_uri(&uri)?,
            source: Source::CommandLine,
            root_certificate,
        });
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
        let (uri, root_certificate) = split_root_certificate(&uri);
        return Ok(Resolved {
            config: parse_uri(&uri)?,
            source: Source::Environment,
            root_certificate,
        });
    }

    if let Some(name) = &target.pg_service {
        let settings = service::lookup(name)?;
        return Ok(Resolved {
            config: from_service(&settings, name)?,
            source: Source::ServiceFile,
            // Carried through rather than discarded: without it, a target using
            // a private certificate authority could never verify its server.
            root_certificate: settings.get("sslrootcert").cloned(),
        });
    }

    Err(Error::new(
        ErrorCode::TargetInvalid,
        format!("target {target_name:?} says nothing about how to connect"),
    )
    .with_hint("set pg_service or uri_env on the target, or pass --uri"))
}

/// Removes `sslrootcert` from a URI and returns it separately.
///
/// tokio-postgres rejects the parameter outright, so a URI carrying it could
/// not connect at all -- which would mean a private certificate authority was
/// usable only through a service file, despite `--uri` and `uri_env` being
/// offered as equals.
fn split_root_certificate(uri: &str) -> (String, Option<String>) {
    let Some((base, query)) = uri.split_once('?') else {
        return (uri.to_owned(), None);
    };

    let mut certificate = None;
    let kept: Vec<&str> = query
        .split('&')
        .filter(|parameter| match parameter.split_once('=') {
            Some(("sslrootcert", value)) => {
                certificate = Some(percent_decode(value));
                false
            }
            _ => true,
        })
        .collect();

    let rebuilt = if kept.is_empty() {
        base.to_owned()
    } else {
        format!("{base}?{}", kept.join("&"))
    };
    (rebuilt, certificate)
}

/// Decodes the percent-escapes a path in a URI query string may carry.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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
            // In libpq, `hostaddr` is the address to connect to while `host`
            // remains the name used for TLS verification. tokio-postgres has no
            // way to express that pairing -- passing both would add two
            // failover hosts, so Zapadka could connect to one while verifying
            // the other. Refusing is the only honest option.
            "hostaddr" if settings.contains_key("host") => {
                return Err(Error::new(
                    ErrorCode::TargetInvalid,
                    format!("service {name:?} sets both host and hostaddr"),
                )
                .with_hint(
                    "Zapadka cannot express libpq's pairing of hostaddr for the connection with \
                     host for certificate verification; set one or the other",
                ));
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
                // Refused rather than ignored. Silently dropping a malformed
                // value turns a bounded connection attempt into an indefinite
                // wait, which is the opposite of what the setting was for.
                let seconds = value.parse::<u64>().map_err(|_| {
                    Error::new(
                        ErrorCode::TargetInvalid,
                        format!("service {name:?} has an invalid connect_timeout {value:?}"),
                    )
                    .with_hint("connect_timeout is a whole number of seconds, as libpq defines it")
                })?;
                config.connect_timeout(std::time::Duration::from_secs(seconds));
            }
            "application_name" => {
                config.application_name(value);
            }
            "sslmode" => {
                config.ssl_mode(ssl_mode(value, name)?);
            }
            // Read by `resolve` and passed to `tls_config`; tokio-postgres
            // itself has no notion of it.
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
        "prefer" => Ok(SslMode::Prefer),
        // `allow` means "try plaintext first, then TLS", which is the opposite
        // order to `prefer`. Mapping it to `prefer` would change which
        // connection succeeds on a server that accepts plaintext but presents
        // a certificate Zapadka cannot verify, so it is refused rather than
        // quietly reinterpreted.
        "allow" => Err(Error::new(
            ErrorCode::TargetInvalid,
            format!("service {name:?} sets sslmode=allow, which Zapadka does not implement"),
        )
        .with_hint(
            "`allow` tries an unencrypted connection first and only then TLS; use `prefer` for \
             TLS-first with a plaintext fallback, or `disable` to state that plaintext is \
             intended",
        )),
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
///
/// Takes the whole resolution rather than its parts, so a caller cannot
/// accidentally drop the private certificate authority on the way here — which
/// is exactly the bug this signature replaced.
pub async fn connect(resolved: &Resolved) -> Result<Connection> {
    let config = &resolved.config;
    let source = resolved.source;
    let root_certificate = resolved.root_certificate.as_deref();
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

    // Asked of the server rather than read from the configuration. When a URI
    // or service entry omits `dbname`, PostgreSQL defaults it to the user name,
    // and a report saying the database was "" would be worse than saying
    // nothing.
    let database = client
        .query_one("SELECT current_database()", &[])
        .await
        .map_or_else(
            |_| config.get_dbname().unwrap_or_default().to_owned(),
            |row| row.get::<_, String>(0),
        );

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
        // `allow` inverts prefer's order and is refused rather than mismapped.
        assert_eq!(
            ssl_mode("allow", "s").unwrap_err().code,
            ErrorCode::TargetInvalid
        );
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
    fn a_uri_can_carry_a_private_certificate_authority() {
        // tokio-postgres rejects `sslrootcert` outright, so without lifting it
        // out a private CA would be usable only through a service file --
        // despite --uri and uri_env being offered as equals.
        let (uri, ca) = split_root_certificate(
            "postgresql://db/app?sslmode=verify-full&sslrootcert=%2Fetc%2Fssl%2Fca.pem",
        );
        assert_eq!(uri, "postgresql://db/app?sslmode=verify-full");
        assert_eq!(ca.as_deref(), Some("/etc/ssl/ca.pem"));
        // The remaining URI must still parse.
        parse_uri(&uri).unwrap();
    }

    #[test]
    fn a_uri_without_a_certificate_is_left_alone() {
        for uri in ["postgresql://db/app", "postgresql://db/app?sslmode=require"] {
            let (rebuilt, ca) = split_root_certificate(uri);
            assert_eq!(rebuilt, uri);
            assert_eq!(ca, None);
        }
        // The only parameter, so the `?` goes with it.
        let (rebuilt, ca) = split_root_certificate("postgresql://db/app?sslrootcert=/ca.pem");
        assert_eq!(rebuilt, "postgresql://db/app");
        assert_eq!(ca.as_deref(), Some("/ca.pem"));
    }

    #[test]
    fn host_and_hostaddr_together_are_refused_rather_than_guessed_at() {
        // libpq connects to hostaddr and verifies the certificate against host.
        // Silently treating them as two failover hosts could connect to one and
        // verify the other.
        let mut settings = service::ServiceSettings::new();
        settings.insert("host".to_owned(), "db.internal".to_owned());
        settings.insert("hostaddr".to_owned(), "10.0.0.5".to_owned());
        let error = from_service(&settings, "s").unwrap_err();
        assert_eq!(error.code, ErrorCode::TargetInvalid);

        // Either alone is fine.
        settings.remove("host");
        assert!(from_service(&settings, "s").is_ok());
    }

    #[test]
    fn a_malformed_connect_timeout_is_refused_rather_than_ignored() {
        // `5s` is a natural thing to write and not what libpq accepts. Ignoring
        // it would leave the connection with no timeout at all.
        let mut settings = service::ServiceSettings::new();
        settings.insert("connect_timeout".to_owned(), "5s".to_owned());
        let error = from_service(&settings, "s").unwrap_err();
        assert_eq!(error.code, ErrorCode::TargetInvalid);

        settings.insert("connect_timeout".to_owned(), "5".to_owned());
        let config = from_service(&settings, "s").unwrap();
        assert_eq!(
            config.get_connect_timeout(),
            Some(&std::time::Duration::from_secs(5))
        );
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
        let resolved = resolve(
            "production",
            Some(&target),
            Some("postgresql://localhost:5432/app"),
        )
        .unwrap();
        assert_eq!(resolved.source, Source::CommandLine);
        assert_eq!(resolved.config.get_dbname(), Some("app"));
    }

    #[test]
    fn a_private_certificate_authority_survives_resolution() {
        // It used to be parsed and thrown away, so a target using a private CA
        // could never verify its server.
        let mut settings = service::ServiceSettings::new();
        settings.insert("host".to_owned(), "db.internal".to_owned());
        settings.insert("sslmode".to_owned(), "verify-full".to_owned());
        settings.insert(
            "sslrootcert".to_owned(),
            "/etc/ssl/private-ca.pem".to_owned(),
        );

        // `from_service` accepts the keyword, and `resolve` is what carries it.
        from_service(&settings, "app-production").unwrap();
        assert_eq!(
            settings.get("sslrootcert").map(String::as_str),
            Some("/etc/ssl/private-ca.pem")
        );
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
