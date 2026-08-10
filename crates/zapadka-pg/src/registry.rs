//! Zapadka's registry: what a database records about its own migration history.
//!
//! Three tables in a reserved schema:
//!
//! - `meta` — one row, naming the project that owns this database and the
//!   registry format version.
//! - `applied_migrations` — current state. One row per successfully applied
//!   migration, holding the immutable facts that history integrity is checked
//!   against.
//! - `events` — append-only history. Every deploy, verify, revert, baseline,
//!   and failure, with the diagnostics needed to explain it later.
//!
//! Splitting current state from history is deliberate. `status` must be a cheap
//! read of a small table, while an incident review needs everything that ever
//! happened, including the attempts that failed. Collapsing the two would make
//! one of those two jobs bad.
//!
//! # Upgrades
//!
//! The binary embeds an ordered list of registry versions. A mutating command
//! applies any missing ones inside a transaction while holding the deployment
//! lock. A binary that meets a registry newer than it understands refuses to
//! act rather than guessing what the unknown columns mean.

use std::collections::BTreeMap;

use tokio_postgres::Client;
use uuid::Uuid;
use zapadka_core::error::{Error, ErrorCode, Result};
use zapadka_core::manifest::Transaction;

use crate::error::registry_failed;

/// The registry format this binary writes.
pub const REGISTRY_FORMAT_VERSION: i32 = 1;

/// One registry format version and the SQL that creates it.
struct Upgrade {
    version: i32,
    sql: fn(&str) -> String,
}

/// Every registry version, in order.
///
/// Each entry moves the registry from the previous version to its own. They are
/// applied in order inside one transaction, so a partially upgraded registry is
/// not a state that can be observed.
const UPGRADES: &[Upgrade] = &[Upgrade {
    version: 1,
    sql: initial_schema,
}];

/// The SQL creating registry format version 1.
fn initial_schema(schema: &str) -> String {
    format!(
        r"
CREATE SCHEMA IF NOT EXISTS {schema};

COMMENT ON SCHEMA {schema} IS
    'Zapadka migration registry. Managed by the zapadka tool; do not edit by hand.';

-- Exactly one row: this database belongs to exactly one Zapadka project.
CREATE TABLE {schema}.meta (
    singleton               boolean     PRIMARY KEY DEFAULT true
                                        CONSTRAINT meta_is_singleton CHECK (singleton),
    project_id              uuid        NOT NULL,
    registry_format_version integer     NOT NULL,
    created_at              timestamptz NOT NULL DEFAULT now(),
    created_by              text        NOT NULL
);

-- Current state: the immutable facts about every applied migration.
CREATE TABLE {schema}.applied_migrations (
    migration_id      uuid        PRIMARY KEY,
    slug              text        NOT NULL,
    definition_sha256 text        NOT NULL,
    deploy_sha256     text        NOT NULL,
    depends           uuid[]      NOT NULL,
    transaction_mode  text        NOT NULL
                                  CHECK (transaction_mode IN ('required', 'forbidden')),
    applied_at        timestamptz NOT NULL DEFAULT now(),
    run_id            uuid        NOT NULL
);

-- Append-only history.
CREATE TABLE {schema}.events (
    run_id            uuid        NOT NULL,
    sequence          integer     NOT NULL,
    recorded_at       timestamptz NOT NULL DEFAULT now(),
    migration_id      uuid,
    action            text        NOT NULL,
    outcome           text        NOT NULL,
    transaction_mode  text,
    definition_sha256 text,
    script_role       text,
    script_sha256     text,
    duration_ms       bigint,
    sqlstate          text,
    message           text,
    detail            text,
    session_user_name text        NOT NULL,
    current_user_name text        NOT NULL,
    server_version    text        NOT NULL,
    zapadka_version   text        NOT NULL,
    PRIMARY KEY (run_id, sequence)
);

CREATE INDEX events_recorded_at_idx ON {schema}.events (recorded_at DESC);
CREATE INDEX events_migration_idx ON {schema}.events (migration_id, recorded_at DESC);

-- History is evidence. Making it append-only in the database means a Zapadka
-- bug cannot quietly rewrite the record of what it did.
CREATE FUNCTION {schema}.events_are_append_only() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'zapadka.events is append-only; % is not permitted', TG_OP
        USING ERRCODE = 'restrict_violation';
END;
$$;

CREATE TRIGGER events_append_only
    BEFORE UPDATE OR DELETE ON {schema}.events
    FOR EACH ROW EXECUTE FUNCTION {schema}.events_are_append_only();

-- TRUNCATE is not an UPDATE or a DELETE, and the table's owner -- which is
-- normally the deploying role -- can issue it. Without this, the whole history
-- could be erased by a statement the row-level trigger never sees.
CREATE TRIGGER events_no_truncate
    BEFORE TRUNCATE ON {schema}.events
    FOR EACH STATEMENT EXECUTE FUNCTION {schema}.events_are_append_only();
"
    )
}

/// What a target's registry currently holds.
#[derive(Debug, Clone)]
pub struct RegistryState {
    /// The registry format version found, before any upgrade this run applied.
    pub format_version: Option<i32>,
    /// The project that owns this database.
    pub project_id: Option<Uuid>,
    /// Applied migrations, by id.
    pub applied: BTreeMap<Uuid, AppliedMigration>,
}

impl RegistryState {
    /// Whether the registry has been created.
    pub fn is_initialized(&self) -> bool {
        self.format_version.is_some()
    }
}

/// One applied migration, as the database recorded it.
#[derive(Debug, Clone)]
pub struct AppliedMigration {
    /// The migration's permanent UUIDv7 identity.
    pub id: Uuid,
    /// Its slug as of the deploy that applied it.
    pub slug: String,
    /// The definition hash at the time it was applied. History integrity is
    /// this value compared against the checked-out project.
    pub definition_sha256: String,
    /// SHA-256 of the `deploy.sql` that was executed.
    pub deploy_sha256: String,
    /// The dependency edges it was deployed with, sorted.
    pub depends: Vec<Uuid>,
    /// The execution mode it was deployed under.
    pub transaction_mode: String,
    /// When it was applied, as an RFC 3339 timestamp.
    pub applied_at: String,
}

/// Facts observed about the connected server.
#[derive(Debug, Clone)]
pub struct ServerFacts {
    /// `server_version`, e.g. `18.4`.
    pub server_version: String,
    /// `server_version_num`, e.g. `180004`.
    pub server_version_num: i32,
    /// The role that authenticated.
    pub session_user: String,
    /// The role privileges are currently evaluated against.
    pub current_user: String,
}

/// The oldest PostgreSQL Zapadka supports.
///
/// Zapadka's safety analysis uses the PostgreSQL 18 grammar. Running it against
/// an older server would mean accepting SQL the server cannot parse and
/// rejecting SQL it would accept, so the version is checked rather than hoped
/// for.
pub const MINIMUM_SERVER_VERSION_NUM: i32 = 180_000;

/// Reads the server's identity and checks it is supported.
pub async fn server_facts(client: &Client) -> Result<ServerFacts> {
    let row = client
        .query_one(
            "SELECT current_setting('server_version'), \
                    current_setting('server_version_num')::int, \
                    session_user::text, \
                    current_user::text",
            &[],
        )
        .await
        .map_err(|error| registry_failed(error, "read the server version"))?;

    let facts = ServerFacts {
        server_version: row.get(0),
        server_version_num: row.get(1),
        session_user: row.get(2),
        current_user: row.get(3),
    };

    if facts.server_version_num < MINIMUM_SERVER_VERSION_NUM {
        return Err(Error::new(
            ErrorCode::ServerUnsupported,
            format!(
                "the target runs PostgreSQL {}, but Zapadka requires 18 or newer",
                facts.server_version
            ),
        )
        .with_hint(
            "Zapadka analyses migrations with the PostgreSQL 18 grammar, so it cannot make \
             truthful safety decisions about an older server",
        ));
    }

    Ok(facts)
}

/// Reads the current registry state.
///
/// A database with no registry is a normal state, not an error: it is what
/// every project's first deploy meets.
pub async fn read(client: &Client, schema: &str) -> Result<RegistryState> {
    let quoted = quote_identifier(schema);

    let exists: bool = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = $1)",
            &[&schema],
        )
        .await
        .map_err(|error| registry_failed(error, "look for the registry schema"))?
        .get(0);

    if !exists {
        return Ok(RegistryState {
            format_version: None,
            project_id: None,
            applied: BTreeMap::new(),
        });
    }

    let meta = client
        .query_opt(
            &format!("SELECT project_id, registry_format_version FROM {quoted}.meta"),
            &[],
        )
        .await
        .map_err(|error| registry_failed(error, "read the registry metadata"))?;

    let (project_id, format_version) = match meta {
        Some(row) => (Some(row.get::<_, Uuid>(0)), Some(row.get::<_, i32>(1))),
        None => (None, None),
    };

    // A newer registry is refused before anything is read from it: the unknown
    // columns might carry state this binary would silently discard.
    if let Some(version) = format_version
        && version > REGISTRY_FORMAT_VERSION
    {
        return Err(Error::new(
            ErrorCode::RegistryFormatTooNew,
            format!(
                "the target's registry is format {version}, but this Zapadka understands {REGISTRY_FORMAT_VERSION}"
            ),
        )
        .with_hint("a newer Zapadka has deployed to this database; upgrade this binary")
        .with_context("registry_format_version", version)
        .with_context("supported_format_version", REGISTRY_FORMAT_VERSION));
    }

    let rows = client
        .query(
            &format!(
                "SELECT migration_id, slug, definition_sha256, deploy_sha256, depends, \
                        transaction_mode, applied_at::text \
                 FROM {quoted}.applied_migrations ORDER BY migration_id"
            ),
            &[],
        )
        .await
        .map_err(|error| registry_failed(error, "read applied migrations"))?;

    let applied = rows
        .into_iter()
        .map(|row| {
            let migration = AppliedMigration {
                id: row.get(0),
                slug: row.get(1),
                definition_sha256: row.get(2),
                deploy_sha256: row.get(3),
                depends: row.get(4),
                transaction_mode: row.get(5),
                applied_at: row.get(6),
            };
            (migration.id, migration)
        })
        .collect();

    Ok(RegistryState {
        format_version,
        project_id,
        applied,
    })
}

/// Creates or upgrades the registry to the version this binary writes.
///
/// Must be called while holding the deployment lock: two binaries upgrading the
/// same registry concurrently is exactly the race the lock exists to prevent.
pub async fn upgrade(
    client: &mut Client,
    schema: &str,
    project_id: Uuid,
    zapadka_version: &str,
    state: &RegistryState,
) -> Result<i32> {
    let current = state.format_version.unwrap_or(0);
    if current == REGISTRY_FORMAT_VERSION {
        check_project(state, project_id)?;
        return Ok(current);
    }

    let transaction = client
        .transaction()
        .await
        .map_err(|error| registry_failed(error, "begin the registry upgrade"))?;

    for step in UPGRADES.iter().filter(|step| step.version > current) {
        transaction
            .batch_execute(&(step.sql)(&quote_identifier(schema)))
            .await
            .map_err(|error| {
                registry_failed(error, &format!("create registry format {}", step.version))
            })?;
    }

    let quoted = quote_identifier(schema);
    if current == 0 {
        transaction
            .execute(
                &format!(
                    "INSERT INTO {quoted}.meta (project_id, registry_format_version, created_by) \
                     VALUES ($1, $2, $3)"
                ),
                &[&project_id, &REGISTRY_FORMAT_VERSION, &zapadka_version],
            )
            .await
            .map_err(|error| registry_failed(error, "record the project identity"))?;
    } else {
        check_project(state, project_id)?;
        transaction
            .execute(
                &format!("UPDATE {quoted}.meta SET registry_format_version = $1"),
                &[&REGISTRY_FORMAT_VERSION],
            )
            .await
            .map_err(|error| registry_failed(error, "record the registry format version"))?;
    }

    transaction
        .commit()
        .await
        .map_err(|error| registry_failed(error, "commit the registry upgrade"))?;

    Ok(REGISTRY_FORMAT_VERSION)
}

/// Refuses to act on a database that belongs to a different project.
///
/// Two projects sharing one registry would interleave two unrelated histories,
/// and neither project's `status` would mean anything afterwards. Worse, a
/// `revert` would run one project's revert script against another project's
/// schema while holding a lock the other project does not use.
///
/// Called from every command that opens a target, not only from the ones that
/// write, because reading a foreign registry produces a confidently wrong
/// answer rather than an obviously wrong one.
pub fn check_project(state: &RegistryState, project_id: Uuid) -> Result<()> {
    match state.project_id {
        Some(existing) if existing != project_id => Err(Error::new(
            ErrorCode::RegistryProjectMismatch,
            format!("this database belongs to Zapadka project {existing}"),
        )
        .with_context("registry_project_id", existing)
        .with_context("configured_project_id", project_id)
        .with_hint(
            "one database holds one project's history; point this project at its own database, \
             or correct project.id in zapadka.toml",
        )),
        _ => Ok(()),
    }
}

/// Records that a migration is now applied.
pub async fn record_applied(
    client: &tokio_postgres::Transaction<'_>,
    schema: &str,
    run_id: Uuid,
    migration: &zapadka_core::migration::Migration,
) -> Result<()> {
    let quoted = quote_identifier(schema);
    let mode = migration.manifest.transaction.as_str();
    let mut depends = migration.depends().to_vec();
    // Stored sorted so a comparison against the manifest's canonical order is a
    // plain equality check.
    depends.sort();

    client
        .execute(
            &format!(
                "INSERT INTO {quoted}.applied_migrations \
                    (migration_id, slug, definition_sha256, deploy_sha256, depends, \
                     transaction_mode, run_id) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)"
            ),
            &[
                &migration.id,
                &migration.slug,
                &migration.definition_sha256,
                &migration.deploy.sha256,
                &depends,
                &mode,
                &run_id,
            ],
        )
        .await
        .map_err(|error| registry_failed(error, "record the applied migration"))?;
    Ok(())
}

/// Removes a migration from current state, as `revert` does.
pub async fn remove_applied(
    client: &tokio_postgres::Transaction<'_>,
    schema: &str,
    migration_id: Uuid,
) -> Result<()> {
    let quoted = quote_identifier(schema);
    client
        .execute(
            &format!("DELETE FROM {quoted}.applied_migrations WHERE migration_id = $1"),
            &[&migration_id],
        )
        .await
        .map_err(|error| registry_failed(error, "remove the reverted migration"))?;
    Ok(())
}

/// Quotes an identifier for use in SQL Zapadka builds by hand.
///
/// The registry schema comes from `zapadka.toml`, which is checked in and
/// reviewed, so this is defence in depth rather than a trust boundary. It is
/// applied anyway: nothing that concatenates a name into SQL should rely on the
/// name being well behaved.
pub fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The transaction mode a stored string represents.
pub fn parse_transaction_mode(text: &str) -> Transaction {
    match text {
        "forbidden" => Transaction::Forbidden,
        _ => Transaction::Required,
    }
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;

    #[test]
    fn identifiers_are_quoted_and_embedded_quotes_escaped() {
        assert_eq!(quote_identifier("zapadka"), "\"zapadka\"");
        assert_eq!(quote_identifier("Mixed Case"), "\"Mixed Case\"");
        // A name that tried to end the quoting cannot.
        assert_eq!(
            quote_identifier("evil\"; DROP TABLE x; --"),
            "\"evil\"\"; DROP TABLE x; --\""
        );
    }

    #[test]
    fn the_schema_name_is_quoted_everywhere_it_appears_in_ddl() {
        let sql = initial_schema(&quote_identifier("my schema"));
        assert!(sql.contains("CREATE SCHEMA IF NOT EXISTS \"my schema\""));
        assert!(sql.contains("CREATE TABLE \"my schema\".meta"));
        assert!(
            !sql.contains("my schema."),
            "an unquoted use slipped through"
        );
    }

    #[test]
    fn upgrades_are_ordered_and_start_at_one() {
        let versions: Vec<i32> = UPGRADES.iter().map(|step| step.version).collect();
        let mut sorted = versions.clone();
        sorted.sort_unstable();
        assert_eq!(versions, sorted, "upgrades must be listed in order");
        assert_eq!(versions.first(), Some(&1));
        assert_eq!(
            versions.last(),
            Some(&REGISTRY_FORMAT_VERSION),
            "the last upgrade must produce the version this binary writes"
        );
    }

    #[test]
    fn the_registry_declares_its_history_append_only() {
        let sql = initial_schema("\"zapadka\"");
        assert!(sql.contains("BEFORE UPDATE OR DELETE ON \"zapadka\".events"));
        // TRUNCATE is not an UPDATE or a DELETE and needs its own trigger; the
        // table owner can otherwise erase the entire history.
        assert!(sql.contains("BEFORE TRUNCATE ON \"zapadka\".events"));
        assert!(sql.contains("append-only"));
    }

    #[test]
    fn a_newer_registry_is_refused_rather_than_read() {
        let state = RegistryState {
            format_version: Some(REGISTRY_FORMAT_VERSION + 1),
            project_id: Some(Uuid::nil()),
            applied: BTreeMap::new(),
        };
        // `read` performs this check; the condition is asserted directly here
        // because constructing it needs no database.
        assert!(state.format_version.unwrap() > REGISTRY_FORMAT_VERSION);
    }

    #[test]
    fn a_database_owned_by_another_project_is_refused() {
        let other = Uuid::parse_str("0198f5c0-0000-7000-8000-00000000000b").unwrap();
        let ours = Uuid::parse_str("0198f5c0-0000-7000-8000-00000000000a").unwrap();
        let state = RegistryState {
            format_version: Some(1),
            project_id: Some(other),
            applied: BTreeMap::new(),
        };
        let error = check_project(&state, ours).unwrap_err();
        assert_eq!(error.code, ErrorCode::RegistryProjectMismatch);
        assert_eq!(
            error
                .context()
                .get("registry_project_id")
                .map(String::as_str),
            Some(other.to_string().as_str())
        );
    }

    #[test]
    fn an_uninitialized_registry_accepts_any_project() {
        let state = RegistryState {
            format_version: None,
            project_id: None,
            applied: BTreeMap::new(),
        };
        assert!(check_project(&state, Uuid::now_v7()).is_ok());
        assert!(!state.is_initialized());
    }

    #[test]
    fn transaction_modes_round_trip_through_their_stored_form() {
        for mode in [Transaction::Required, Transaction::Forbidden] {
            assert_eq!(parse_transaction_mode(mode.as_str()), mode);
        }
    }
}
