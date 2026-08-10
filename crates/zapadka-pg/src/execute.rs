//! Runner-owned execution.
//!
//! Zapadka opens and closes every transaction its scripts run in. A script
//! never sees a transaction it can commit, roll back, or checkpoint, which is
//! what makes the applied state Zapadka records the same as the state the
//! database is actually in.
//!
//! # Why verification always rolls back
//!
//! `verify.sql` runs after its migration has committed, in a fresh **read-only**
//! transaction that is rolled back whatever happens. It therefore observes
//! exactly the committed state a later reader would see, while being unable to
//! leave anything behind. A verification that could write would be able to make
//! itself pass.
//!
//! Read-only as well as rolled back, because rollback alone is not enough:
//! `nextval()` is not rolled back, and neither is anything a function does
//! outside the database. The read-only transaction refuses those attempts
//! rather than discovering afterwards that they persisted.
//!
//! # Why a failed verification does not revert
//!
//! The migration committed. Reverting it automatically would run revert SQL
//! that has not been proven correct, against a schema in a state nobody
//! anticipated, while an operator is not watching. Zapadka stops, records the
//! truth, and leaves the decision to a person. See ADR-0002.

use std::time::Instant;

use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, Transaction as PgTransaction};
use uuid::Uuid;
use zapadka_core::duration::Timeout;
use zapadka_core::error::{Error, Result};
use zapadka_core::migration::Migration;
use zapadka_core::report::ScriptRole;

use crate::error::{registry_failed, script_failed};
use crate::registry::{self, ServerFacts, quote_identifier};

/// Timeouts applied to the SQL Zapadka runs.
///
/// Absent means Zapadka sets nothing and PostgreSQL's own configuration
/// applies. Zapadka deliberately has no default of its own: a hidden
/// `statement_timeout` would abort long migrations that were working correctly.
#[derive(Debug, Clone, Copy, Default)]
pub struct Timeouts {
    /// How long a statement waits for a lock before giving up.
    pub lock_timeout: Option<Timeout>,
    /// How long a statement may run before being cancelled.
    pub statement_timeout: Option<Timeout>,
}

/// What a script did.
#[derive(Debug, Clone)]
pub struct ScriptOutcome {
    /// Which of the migration's scripts ran.
    pub role: ScriptRole,
    /// Project-relative path of the script.
    pub path: String,
    /// SHA-256 of the exact bytes executed.
    pub sha256: String,
    /// How long the script took, in milliseconds.
    pub duration_ms: u64,
}

/// Executes migrations against one connection.
#[allow(missing_debug_implementations)] // holds a tokio_postgres::Client, which is not Debug
pub struct Runner {
    client: Client,
    schema: String,
    run_id: Uuid,
    facts: ServerFacts,
    zapadka_version: String,
    timeouts: Timeouts,
    /// Orders events within this run. Events are append-only, so the sequence
    /// is what reconstructs the order things happened in.
    sequence: i32,
}

impl Runner {
    /// Creates a runner.
    pub fn new(
        client: Client,
        schema: String,
        run_id: Uuid,
        facts: ServerFacts,
        zapadka_version: String,
        timeouts: Timeouts,
    ) -> Self {
        Self {
            client,
            schema,
            run_id,
            facts,
            zapadka_version,
            timeouts,
            sequence: 0,
        }
    }

    /// Borrows the underlying client, for reads that need no transaction.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The server facts observed at connection time.
    pub fn facts(&self) -> &ServerFacts {
        &self.facts
    }

    /// Applies one migration inside a transaction Zapadka owns.
    ///
    /// The migration's SQL and the row recording it as applied commit together.
    /// Neither can exist without the other, so a crash at any instant leaves
    /// either both or nothing — never a schema change Zapadka has forgotten
    /// about, or a record of work that was rolled back.
    pub async fn deploy(&mut self, migration: &Migration) -> Result<ScriptOutcome> {
        let started = Instant::now();
        let path = migration.deploy.relative_path.clone();

        let result = self.deploy_inner(migration, started).await;
        // A script running for longer than 584 million years is not a case worth
        // modelling; saturating keeps the report honest without a panic.
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        match result {
            Ok(()) => {
                // The success event was written inside the deploy transaction,
                // so there is nothing to record here. Writing it separately
                // would mean a failed insert could report an applied migration
                // as failed and skip everything after it.
                Ok(ScriptOutcome {
                    role: ScriptRole::Deploy,
                    path,
                    sha256: migration.deploy.sha256.clone(),
                    duration_ms,
                })
            }
            Err(error) => {
                // The deploy transaction has already rolled back, so this event
                // is written by a separate statement. Recording the failure
                // must not be able to hide it: if the event cannot be written,
                // the original failure is still what gets reported.
                let _ = self
                    .record(Event {
                        migration_id: Some(migration.id),
                        action: "deploy",
                        outcome: "failed",
                        transaction_mode: Some(migration.manifest.transaction.as_str()),
                        definition_sha256: Some(&migration.definition_sha256),
                        script_role: Some("deploy"),
                        script_sha256: Some(&migration.deploy.sha256),
                        duration_ms: Some(duration_ms),
                        error: Some(&error),
                    })
                    .await;
                Err(error)
            }
        }
    }

    /// The transactional body of a deploy.
    ///
    /// The user's SQL, the applied-migration row, and the success event all
    /// commit together. Recording the event afterwards would open a window
    /// where the database says a migration is applied and the history says the
    /// deploy failed.
    async fn deploy_inner(&mut self, migration: &Migration, started: Instant) -> Result<()> {
        let transaction = self
            .client
            .transaction()
            .await
            .map_err(|error| registry_failed(error, "begin the migration transaction"))?;

        apply_timeouts(&transaction, self.timeouts).await?;

        // Checked again here, immediately before execution. `deploy` validated
        // the whole project before connecting; this is the check that actually
        // guards the boundary, and it guards every path into the runner.
        zapadka_core::lint::ensure_runner_owns_transaction(
            &migration.deploy.sql,
            &migration.deploy.relative_path,
        )?;

        // The whole script goes to the server as one simple query. Zapadka does
        // not split it: any splitting rule it invented would eventually
        // disagree with PostgreSQL about where a statement ends.
        transaction
            .batch_execute(&migration.deploy.sql)
            .await
            .map_err(|error| {
                script_failed(error, ScriptRole::Deploy, &migration.deploy.relative_path)
            })?;

        registry::record_applied(&transaction, &self.schema, self.run_id, migration).await?;

        self.sequence += 1;
        record_event(
            &transaction,
            &self.schema,
            self.sequence,
            self.run_id,
            &self.facts,
            &self.zapadka_version,
            Event {
                migration_id: Some(migration.id),
                action: "deploy",
                outcome: "succeeded",
                transaction_mode: Some(migration.manifest.transaction.as_str()),
                definition_sha256: Some(&migration.definition_sha256),
                script_role: Some("deploy"),
                script_sha256: Some(&migration.deploy.sha256),
                // Measured before the commit, which is the only point at which
                // it can be recorded inside the transaction it describes.
                duration_ms: Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
                error: None,
            },
        )
        .await?;

        transaction
            .commit()
            .await
            .map_err(|error| registry_failed(error, "commit the migration"))?;
        Ok(())
    }

    /// Runs a migration's `verify.sql` and rolls it back.
    ///
    /// Returns `Ok(None)` when the migration has no verification script, which
    /// is not a failure: verification is opt-in per migration.
    pub async fn verify(&mut self, migration: &Migration) -> Result<Option<ScriptOutcome>> {
        let Some(script) = &migration.verify else {
            return Ok(None);
        };

        let started = Instant::now();
        let result = self.verify_inner(&script.sql, &script.relative_path).await;
        // A script running for longer than 584 million years is not a case worth
        // modelling; saturating keeps the report honest without a panic.
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        let outcome = if result.is_ok() {
            "succeeded"
        } else {
            "failed"
        };
        let error = result.as_ref().err();
        self.record(Event {
            migration_id: Some(migration.id),
            action: "verify",
            outcome,
            transaction_mode: Some("required"),
            definition_sha256: Some(&migration.definition_sha256),
            script_role: Some("verify"),
            // The exact bytes verified, recorded because `verify.sql` is
            // mutable: a past run's result only means something alongside the
            // script that produced it.
            script_sha256: Some(&script.sha256),
            duration_ms: Some(duration_ms),
            error,
        })
        .await
        .ok();

        result?;
        Ok(Some(ScriptOutcome {
            role: ScriptRole::Verify,
            path: script.relative_path.clone(),
            sha256: script.sha256.clone(),
            duration_ms,
        }))
    }

    /// Runs verification SQL in a transaction that is always rolled back.
    async fn verify_inner(&mut self, sql: &str, path: &str) -> Result<()> {
        // `verify.sql` is mutable, so it can acquire a `COMMIT` long after the
        // migration that owns it was reviewed. Without this, the statements
        // after that commit would run outside the transaction and survive the
        // rollback.
        zapadka_core::lint::ensure_runner_owns_transaction(sql, path)?;

        let transaction = self
            .client
            .transaction()
            .await
            .map_err(|error| registry_failed(error, "begin the verification transaction"))?;

        // Read-only, not merely rolled back. Rollback undoes table changes, but
        // it does not undo everything PostgreSQL can do: a `nextval()` is not
        // rolled back, and neither is anything a function writes outside the
        // database. A read-only transaction refuses those at the point they are
        // attempted, which turns "verification leaves nothing behind" from a
        // claim about rollback into something the server enforces.
        //
        // Temporary tables remain writable, so a verification script can still
        // build scratch data to check against.
        transaction
            .batch_execute("SET TRANSACTION READ ONLY")
            .await
            .map_err(|error| {
                registry_failed(error, "make the verification transaction read-only")
            })?;

        apply_timeouts(&transaction, self.timeouts).await?;

        let result = transaction
            .batch_execute(sql)
            .await
            .map_err(|error| script_failed(error, ScriptRole::Verify, path));

        // Rolled back on both paths. A verification that succeeded must leave
        // no more behind than one that failed.
        transaction
            .rollback()
            .await
            .map_err(|error| registry_failed(error, "roll back the verification transaction"))?;

        result
    }

    /// Reverts one migration inside a transaction Zapadka owns.
    ///
    /// The revert SQL and the removal of the applied-state row commit together,
    /// exactly as a deploy does, so a crash cannot leave a migration whose SQL
    /// was undone but which the registry still calls applied.
    pub async fn revert(&mut self, migration: &Migration) -> Result<ScriptOutcome> {
        let script = migration.revert.as_ref().ok_or_else(|| {
            Error::new(
                zapadka_core::error::ErrorCode::MigrationMissingScript,
                format!("{} has no revert.sql", migration.relative_dir),
            )
        })?;

        let started = Instant::now();
        let result = self
            .revert_inner(migration, &script.sql, &script.relative_path)
            .await;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        let outcome = if result.is_ok() {
            "succeeded"
        } else {
            "failed"
        };
        let error = result.as_ref().err();
        let _ = self
            .record(Event {
                migration_id: Some(migration.id),
                action: "revert",
                outcome,
                transaction_mode: Some("required"),
                definition_sha256: Some(&migration.definition_sha256),
                script_role: Some("revert"),
                // The exact bytes reverted. `revert.sql` is mutable, so a past
                // revert only means something alongside the script that ran.
                script_sha256: Some(&script.sha256),
                duration_ms: Some(duration_ms),
                error,
            })
            .await;

        result?;
        Ok(ScriptOutcome {
            role: ScriptRole::Revert,
            path: script.relative_path.clone(),
            sha256: script.sha256.clone(),
            duration_ms,
        })
    }

    /// The transactional body of a revert.
    async fn revert_inner(&mut self, migration: &Migration, sql: &str, path: &str) -> Result<()> {
        // `revert.sql` is mutable for the same reason `verify.sql` is: it can
        // acquire a `COMMIT` long after the migration that owns it was reviewed
        // and deployed. A commit here would end the runner's transaction, so
        // the statements after it would run outside it and the applied-state
        // row would not be removed with them.
        zapadka_core::lint::ensure_runner_owns_transaction(sql, path)?;

        let transaction = self
            .client
            .transaction()
            .await
            .map_err(|error| registry_failed(error, "begin the revert transaction"))?;

        apply_timeouts(&transaction, self.timeouts).await?;

        transaction
            .batch_execute(sql)
            .await
            .map_err(|error| script_failed(error, ScriptRole::Revert, path))?;

        registry::remove_applied(&transaction, &self.schema, migration.id).await?;

        transaction
            .commit()
            .await
            .map_err(|error| registry_failed(error, "commit the revert"))?;
        Ok(())
    }

    /// Records migrations as applied without running any of their SQL.
    ///
    /// All of them commit together: a partially baselined closure would be a
    /// history nobody asserted and nobody can explain.
    pub async fn baseline(&mut self, migrations: &[&Migration]) -> Result<()> {
        let transaction = self
            .client
            .transaction()
            .await
            .map_err(|error| registry_failed(error, "begin the baseline transaction"))?;

        for migration in migrations {
            registry::record_applied(&transaction, &self.schema, self.run_id, migration).await?;
        }

        transaction
            .commit()
            .await
            .map_err(|error| registry_failed(error, "commit the baseline"))?;

        // Recorded after the commit, so an event never claims a baseline that
        // did not happen.
        for migration in migrations {
            let _ = self
                .record(Event {
                    migration_id: Some(migration.id),
                    action: "baseline",
                    outcome: "succeeded",
                    transaction_mode: Some(migration.manifest.transaction.as_str()),
                    definition_sha256: Some(&migration.definition_sha256),
                    script_role: None,
                    script_sha256: None,
                    duration_ms: None,
                    error: None,
                })
                .await;
        }
        Ok(())
    }

    /// Records an event for something that happened outside a migration, such
    /// as creating the registry.
    pub async fn record_run_event(&mut self, action: &str, outcome: &str) -> Result<()> {
        self.record(Event {
            migration_id: None,
            action,
            outcome,
            transaction_mode: None,
            definition_sha256: None,
            script_role: None,
            script_sha256: None,
            duration_ms: None,
            error: None,
        })
        .await
    }

    /// Appends one event on the runner's own connection.
    ///
    /// Used for events that describe something already committed or rolled
    /// back, where there is no transaction left to write into.
    async fn record(&mut self, event: Event<'_>) -> Result<()> {
        self.sequence += 1;
        let (sql, values) = event_insert(&self.schema, &event);
        let bound = bind(
            self.run_id,
            self.sequence,
            &event,
            &values,
            &self.facts,
            &self.zapadka_version,
        );

        self.client
            .execute(&sql, &bound.params())
            .await
            .map_err(|error| registry_failed(error, "record an event"))?;
        Ok(())
    }

    /// Consumes the runner, returning the connection so the lock can be
    /// released on it.
    pub fn into_client(self) -> Client {
        self.client
    }
}

/// Appends one event inside an open transaction.
///
/// This is what lets a deploy's success event commit atomically with the
/// migration it describes.
#[allow(clippy::too_many_arguments)]
async fn record_event(
    transaction: &PgTransaction<'_>,
    schema: &str,
    sequence: i32,
    run_id: Uuid,
    facts: &ServerFacts,
    zapadka_version: &str,
    event: Event<'_>,
) -> Result<()> {
    let (sql, values) = event_insert(schema, &event);
    let bound = bind(run_id, sequence, &event, &values, facts, zapadka_version);

    transaction
        .execute(&sql, &bound.params())
        .await
        .map_err(|error| registry_failed(error, "record an event"))?;
    Ok(())
}

/// The owned values an event insert needs, kept alive while the query runs.
struct EventValues {
    sqlstate: Option<String>,
    message: Option<String>,
    detail: Option<String>,
    duration: Option<i64>,
}

/// The bound parameters of an event insert.
struct BoundEvent<'a> {
    run_id: Uuid,
    sequence: i32,
    event: &'a Event<'a>,
    values: &'a EventValues,
    facts: &'a ServerFacts,
    zapadka_version: &'a str,
}

impl BoundEvent<'_> {
    fn params(&self) -> [&(dyn ToSql + Sync); 17] {
        [
            &self.run_id,
            &self.sequence,
            &self.event.migration_id,
            &self.event.action,
            &self.event.outcome,
            &self.event.transaction_mode,
            &self.event.definition_sha256,
            &self.event.script_role,
            &self.event.script_sha256,
            &self.values.duration,
            &self.values.sqlstate,
            &self.values.message,
            &self.values.detail,
            &self.facts.session_user,
            &self.facts.current_user,
            &self.facts.server_version,
            &self.zapadka_version,
        ]
    }
}

/// Extracts the owned values an event insert needs.
fn event_insert(schema: &str, event: &Event<'_>) -> (String, EventValues) {
    let quoted = quote_identifier(schema);
    let (sqlstate, message, detail) = match event.error {
        Some(error) => (
            error.sqlstate().map(str::to_owned),
            Some(error.message.clone()),
            error.detail().map(str::to_owned),
        ),
        None => (None, None, None),
    };
    let values = EventValues {
        sqlstate,
        message,
        detail,
        // PostgreSQL has no unsigned integer type, so durations are stored
        // signed; saturating keeps an implausible value from becoming negative.
        duration: event
            .duration_ms
            .map(|ms| i64::try_from(ms).unwrap_or(i64::MAX)),
    };
    let sql = format!(
        "INSERT INTO {quoted}.events \
            (run_id, sequence, migration_id, action, outcome, transaction_mode, \
             definition_sha256, script_role, script_sha256, duration_ms, sqlstate, \
             message, detail, session_user_name, current_user_name, server_version, \
             zapadka_version) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)"
    );
    (sql, values)
}

/// Ties an event and its owned values together for binding.
///
/// The values have to outlive the query, and `ToSql` borrows them, so they are
/// held in one place rather than as a dozen locals at each call site.
fn bind<'a>(
    run_id: Uuid,
    sequence: i32,
    event: &'a Event<'a>,
    values: &'a EventValues,
    facts: &'a ServerFacts,
    zapadka_version: &'a str,
) -> BoundEvent<'a> {
    BoundEvent {
        run_id,
        sequence,
        event,
        values,
        facts,
        zapadka_version,
    }
}

/// One row of history.
struct Event<'a> {
    migration_id: Option<Uuid>,
    action: &'a str,
    outcome: &'a str,
    transaction_mode: Option<&'a str>,
    definition_sha256: Option<&'a String>,
    script_role: Option<&'a str>,
    script_sha256: Option<&'a String>,
    duration_ms: Option<u64>,
    error: Option<&'a Error>,
}

/// Applies the target's configured timeouts to the current transaction.
///
/// `SET LOCAL` so they last exactly as long as the transaction Zapadka opened
/// and cannot leak into the next one.
async fn apply_timeouts(transaction: &PgTransaction<'_>, timeouts: Timeouts) -> Result<()> {
    for (setting, value) in [
        ("lock_timeout", timeouts.lock_timeout),
        ("statement_timeout", timeouts.statement_timeout),
    ] {
        let Some(value) = value else {
            continue;
        };
        // The value is a number Zapadka produced from a parsed duration, so
        // there is nothing here a configuration file could inject.
        transaction
            .batch_execute(&format!(
                "SET LOCAL {setting} = '{}'",
                value.as_postgres_setting()
            ))
            .await
            .map_err(|error| registry_failed(error, &format!("apply {setting}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;

    #[test]
    fn timeout_settings_are_rendered_as_plain_integers() {
        // Nothing from configuration reaches the SQL string except digits.
        for text in ["5s", "500ms", "0", "2min"] {
            let value = Timeout::parse(text).unwrap().as_postgres_setting();
            assert!(
                value.chars().all(|c| c.is_ascii_digit()),
                "{text} rendered as {value}"
            );
        }
    }

    #[test]
    fn absent_timeouts_mean_zapadka_sets_nothing() {
        // A hidden default statement_timeout would abort long migrations that
        // were working correctly.
        let timeouts = Timeouts::default();
        assert!(timeouts.lock_timeout.is_none());
        assert!(timeouts.statement_timeout.is_none());
    }
}
