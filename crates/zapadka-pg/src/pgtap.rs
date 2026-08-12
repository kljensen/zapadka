//! Installing the embedded pgTAP assertion library.
//!
//! pgTAP ships inside the Zapadka binary and is installed into a reserved
//! schema on a **test target only**. It is deliberately not installed as a
//! PostgreSQL extension:
//!
//! - An extension needs files on the server's filesystem and usually a package
//!   install. Zapadka is one binary and can assume neither.
//! - `CREATE EXTENSION` is a privileged, database-wide act. Creating a schema
//!   is not, and it is trivially reversible with a single `DROP SCHEMA`.
//! - This release of pgTAP contains no `LANGUAGE C` functions, so there is
//!   nothing an extension would provide that a schema does not.
//!
//! Deploy targets never see any of this. That is the whole point of ADR-0004:
//! production databases should not have to carry a test framework in order for
//! a team to test their migrations.

use sha2::{Digest, Sha256};
use tokio_postgres::Client;
use zapadka_core::error::{Error, ErrorCode, Result};

use crate::error::registry_failed;
use crate::registry::quote_identifier;

/// The schema pgTAP is installed into.
///
/// Reserved, and separate from the registry schema: dropping the test framework
/// must never risk the migration history.
pub const TEST_SCHEMA: &str = "zapadka_test";

/// The pgTAP release embedded in this binary.
pub const PGTAP_VERSION: &str = "1.3.4";

/// The version of Zapadka's own assertion library.
///
/// Reported to operators and recorded in the installed schema. Distinct from
/// the capture protocol: the protocol changes when the *tables* change, this
/// changes when the assertions do.
pub const TEST_LIBRARY_VERSION: &str = "1";

/// The `major.minor` value `pgtap_version()` returns.
const PGTAP_NUMERIC_VERSION: &str = "1.3";

/// The vendored source, exactly as upstream ships it.
///
/// See `third_party/pgtap/PROVENANCE.toml`. `cargo xtask verify-fixtures`
/// proves this file has not been edited.
const PGTAP_SOURCE: &str = include_str!("../../../third_party/pgtap/sql/pgtap.sql.in");

/// Zapadka's own assertion library.
///
/// Installed in place of pgTAP. See
/// `docs/adr/0006-own-a-native-sql-assertion-library.md`.
/// The library, in installation order: the core defines `_record`, which every
/// assertion in the later files calls.
const NATIVE_SOURCE: &[(&str, &str)] = &[
    ("core", include_str!("../sql/core.sql")),
    ("objects", include_str!("../sql/objects.sql")),
];

/// What is installed in a target's test schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installation {
    /// The schema does not exist.
    Absent,
    /// Installed, and its artifact matches this binary's.
    Current,
    /// Installed by a different Zapadka build.
    Stale {
        /// The artifact hash recorded on the target.
        installed_sha256: String,
        /// The pgTAP release recorded on the target.
        installed_version: String,
    },
}

/// Renders the SQL to install pgTAP.
///
/// Upstream's Makefile builds this by applying compatibility patches and
/// substituting placeholders with `sed`. Every patch is gated on PostgreSQL
/// 9.x, and Zapadka supports 18 only, so none apply; that leaves two
/// substitutions, which are done here.
///
/// `os_name` comes from the *server*, not from the machine Zapadka is running
/// on. Upstream uses the build host's `uname`, which would be wrong here:
/// Zapadka commonly runs on one operating system and installs into a database
/// on another, and `os_name()` is meant to describe where the tests run.
pub fn artifact(os_name: &str) -> String {
    PGTAP_SOURCE
        .replace("__VERSION__", PGTAP_NUMERIC_VERSION)
        .replace("__OS__", &escape_sql_literal(os_name))
}

/// The hash identifying this binary's pgTAP artifact.
///
/// Covers the vendored source and the substituted version, but not the
/// server-dependent OS name — otherwise the same Zapadka build would appear to
/// carry different artifacts on different servers.
pub fn artifact_sha256() -> String {
    use std::fmt::Write as _;

    let mut hasher = Sha256::new();
    hasher.update(b"zapadka.testlib.v1\n");
    hasher.update(crate::capture::PROTOCOL_VERSION.to_string().as_bytes());
    hasher.update(b"\n");
    for (part, sql) in NATIVE_SOURCE {
        hasher.update(part.as_bytes());
        hasher.update(b"\n");
        hasher.update(sql.as_bytes());
    }
    let digest = hasher.finalize();
    digest
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Escapes a value being embedded in a single-quoted SQL literal.
fn escape_sql_literal(value: &str) -> String {
    value.replace('\'', "''")
}

/// Derives the server's operating system from its own `version()` string.
///
/// PostgreSQL reports its build triple, for example
/// `PostgreSQL 18.4 on aarch64-unknown-linux-musl, compiled by ...`. The third
/// component of the triple is the OS.
pub fn os_from_version(version: &str) -> String {
    version
        .split(" on ")
        .nth(1)
        .and_then(|rest| rest.split([',', ' ']).next())
        .and_then(|triple| triple.split('-').nth(2))
        .unwrap_or("unknown")
        .to_owned()
}

/// Reads what is installed in the test schema.
pub async fn installed(client: &Client) -> Result<Installation> {
    let exists: bool = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = $1)",
            &[&TEST_SCHEMA],
        )
        .await
        .map_err(|error| registry_failed(error, "look for the test schema"))?
        .get(0);

    if !exists {
        return Ok(Installation::Absent);
    }

    let quoted = quote_identifier(TEST_SCHEMA);
    let marker = match client
        .query_opt(
            &format!("SELECT artifact_sha256, pgtap_version FROM {quoted}.zapadka_pgtap"),
            &[],
        )
        .await
    {
        Ok(marker) => marker,
        // Only a missing table means "Zapadka did not create this schema".
        // Treating a permission error or a dropped connection the same way
        // would tell someone to rename their schema when the real problem was
        // that Zapadka could not read it.
        Err(error)
            if error
                .as_db_error()
                .map(tokio_postgres::error::DbError::code)
                == Some(&tokio_postgres::error::SqlState::UNDEFINED_TABLE) =>
        {
            None
        }
        Err(error) => {
            return Err(registry_failed(error, "read the pgTAP installation marker"));
        }
    };

    match marker {
        Some(row) => {
            let installed_sha256: String = row.get(0);
            if installed_sha256 == artifact_sha256() {
                Ok(Installation::Current)
            } else {
                Ok(Installation::Stale {
                    installed_sha256,
                    installed_version: row.get(1),
                })
            }
        }
        // A schema by that name that Zapadka did not create. Refusing is the
        // only safe answer: dropping and recreating it could destroy something
        // that has nothing to do with Zapadka.
        None => Err(Error::new(
            ErrorCode::RegistryUpgradeFailed,
            format!("schema {TEST_SCHEMA} exists but was not created by Zapadka"),
        )
        .with_hint(format!(
            "Zapadka reserves {TEST_SCHEMA} for its test framework; rename or drop the existing \
             schema if it is not needed"
        ))),
    }
}

/// Installs pgTAP into the test schema, replacing any previous installation.
///
/// The whole install runs in one transaction: a half-installed assertion
/// library would produce test failures that look like application bugs.
pub async fn install(client: &mut Client, _server_version: &str) -> Result<String> {
    let quoted = quote_identifier(TEST_SCHEMA);
    let sha256 = artifact_sha256();

    let transaction = client
        .transaction()
        .await
        .map_err(|error| registry_failed(error, "begin the pgTAP install"))?;

    // Dropped and recreated rather than patched: pgTAP has no in-place upgrade
    // path between arbitrary versions, and a test schema holds nothing worth
    // preserving.
    transaction
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {quoted} CASCADE;\n\
             CREATE SCHEMA {quoted};\n\
             COMMENT ON SCHEMA {quoted} IS \
             'Zapadka test assertions {TEST_LIBRARY_VERSION}, installed by Zapadka. Safe to \
              drop; contains no application data.';"
        ))
        .await
        .map_err(|error| registry_failed(error, "create the test schema"))?;

    // pgTAP's SQL creates unqualified objects, so the schema is chosen by the
    // search path rather than by editing the vendored file.
    transaction
        .batch_execute(&format!("SET LOCAL search_path = {quoted}, pg_catalog;"))
        .await
        .map_err(|error| registry_failed(error, "set the install search path"))?;

    for (part, sql) in NATIVE_SOURCE {
        transaction.batch_execute(sql).await.map_err(|error| {
            registry_failed(
                error,
                &format!("install the test assertion library ({part})"),
            )
        })?;
    }

    transaction
        .batch_execute(&format!(
            "CREATE TABLE {quoted}.zapadka_pgtap (\n\
                 singleton        boolean     PRIMARY KEY DEFAULT true CHECK (singleton),\n\
                 pgtap_version    text        NOT NULL,\n\
                 artifact_sha256  text        NOT NULL,\n\
                 zapadka_version  text        NOT NULL,\n\
                 installed_at     timestamptz NOT NULL DEFAULT now()\n\
             );"
        ))
        .await
        .map_err(|error| registry_failed(error, "record the pgTAP installation"))?;

    transaction
        .execute(
            &format!(
                "INSERT INTO {quoted}.zapadka_pgtap \
                    (pgtap_version, artifact_sha256, zapadka_version) VALUES ($1, $2, $3)"
            ),
            &[&TEST_LIBRARY_VERSION, &sha256, &env!("CARGO_PKG_VERSION")],
        )
        .await
        .map_err(|error| registry_failed(error, "record the pgTAP installation"))?;

    transaction
        .commit()
        .await
        .map_err(|error| registry_failed(error, "commit the pgTAP install"))?;

    Ok(sha256)
}

/// The `search_path` a test file runs under.
///
/// Ordered deliberately:
///
/// - `pg_temp` first, so a test can create temporary objects that shadow real
///   ones without touching them.
/// - the test schema, so assertions resolve.
/// - the application's own schemas, so a test reads like application code.
/// - `pg_catalog` last, explicitly, so a test cannot accidentally depend on it
///   being searched before the application's schemas.
pub fn test_search_path(application_schemas: &[String]) -> String {
    let mut parts = vec!["pg_temp".to_owned(), quote_identifier(TEST_SCHEMA)];
    parts.extend(
        application_schemas
            .iter()
            .map(|schema| quote_identifier(schema)),
    );
    parts.push("pg_catalog".to_owned());
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;

    #[test]
    fn the_artifact_substitutes_both_placeholders() {
        let sql = artifact("linux");
        assert!(!sql.contains("__VERSION__"), "version placeholder remained");
        assert!(!sql.contains("__OS__"), "os placeholder remained");
        assert!(
            sql.contains("SELECT 1.3;"),
            "pgtap_version() should return 1.3"
        );
        assert!(
            sql.contains("''linux''::text"),
            "os_name() should return the OS"
        );
    }

    #[test]
    fn the_vendored_source_needs_no_extension_or_shared_library() {
        // This is what lets Zapadka install pgTAP into a plain schema. If a
        // future release adds either, the install strategy has to change.
        assert!(!PGTAP_SOURCE.contains("@extschema@"));
        assert!(!PGTAP_SOURCE.contains("CREATE EXTENSION"));
        assert!(!PGTAP_SOURCE.contains("LANGUAGE C"));
        assert!(!PGTAP_SOURCE.contains("MODULE_PATHNAME"));
    }

    #[test]
    fn the_artifact_hash_is_stable_and_independent_of_the_server() {
        // Two servers running different operating systems must not appear to
        // have different Zapadka builds installed.
        assert_eq!(artifact_sha256(), artifact_sha256());
        assert_eq!(artifact_sha256().len(), 64);
        assert_ne!(artifact("linux"), artifact("darwin"));
    }

    #[test]
    fn the_operating_system_comes_from_the_servers_own_version_string() {
        for (version, expected) in [
            (
                "PostgreSQL 18.4 on aarch64-unknown-linux-musl, compiled by gcc",
                "linux",
            ),
            (
                "PostgreSQL 18.4 on x86_64-pc-linux-gnu, compiled by gcc 13.2",
                "linux",
            ),
            (
                "PostgreSQL 18.4 on aarch64-apple-darwin23.0.0, compiled by clang",
                "darwin23.0.0",
            ),
        ] {
            assert_eq!(os_from_version(version), expected, "{version}");
        }
        // An unrecognizable version string is reported as unknown rather than
        // guessed at.
        assert_eq!(os_from_version("something else entirely"), "unknown");
    }

    #[test]
    fn a_quote_in_the_os_name_cannot_escape_its_literal() {
        let sql = artifact("weird'; DROP TABLE x; --");
        assert!(sql.contains("weird''; DROP TABLE x; --"));
    }

    #[test]
    fn the_search_path_puts_temporary_objects_first_and_the_catalog_last() {
        let path = test_search_path(&["app".to_owned(), "billing".to_owned()]);
        assert_eq!(
            path,
            "pg_temp, \"zapadka_test\", \"app\", \"billing\", pg_catalog"
        );
    }

    #[test]
    fn application_schema_names_are_quoted() {
        let path = test_search_path(&["Mixed Case".to_owned()]);
        assert!(path.contains("\"Mixed Case\""));
    }

    #[test]
    fn the_test_schema_is_separate_from_the_registry_schema() {
        // Dropping the test framework must never risk the migration history.
        assert_ne!(TEST_SCHEMA, zapadka_core::config::DEFAULT_REGISTRY_SCHEMA);
    }
}
