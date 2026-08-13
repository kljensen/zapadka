//! Running database test files.
//!
//! Each file runs on a **fresh connection** inside a runner-owned transaction
//! that is **always rolled back**. That combination is what makes a suite
//! order-independent: no file can see another's data, its temporary objects, or
//! any session state it left behind. A test that only passes when it runs after
//! another one is a test that is lying about what it checks.
//!
//! Files run **serially**. Running them concurrently against one database would
//! mean sharing a schema between tests that each assume they own it, which
//! trades a real guarantee for a small amount of wall-clock time.
//!
//! # Sequences, and what isolation does not cover
//!
//! Rollback is not quite enough on its own. PostgreSQL deliberately does not
//! roll back `nextval()`, so a test that inserts one row into a table with a
//! generated key advances that sequence permanently — and a later file
//! asserting on a generated id depends on which files ran before it.
//!
//! Zapadka **reports** this rather than undoing it. Restoring a sequence means
//! calling `setval` backwards, and Zapadka's advisory lock serializes other
//! Zapadka runs but not application connections. If anything else drew from
//! that sequence between the snapshot and the restore, rewinding it would hand
//! out a key that has already been issued. Trading a test-ordering problem for
//! a duplicate-key problem in a live database is not a trade worth making.
//!
//! So a run that advances a sequence says so, naming it, and the fix belongs
//! in the test: assert on what a row contains rather than on the id it was
//! given.

use std::time::Instant;

use tokio_postgres::Client;
use zapadka_core::error::{Error, ErrorCode, Result};
use zapadka_core::report::Location;
use zapadka_core::testresult::TestDocument;
use zapadka_core::testsuite::TestFile;

use crate::error::registry_failed;
use crate::execute::Timeouts;
use crate::testlib;

/// What one test file did.
#[derive(Debug)]
pub struct TestOutcome {
    /// What the file recorded, when it ran to completion.
    pub document: Option<TestDocument>,
    /// Why the file failed, when it did.
    pub error: Option<Error>,
    /// Sequences the file advanced, which rollback does not undo.
    pub advanced_sequences: Vec<AdvancedSequence>,
    /// How long the file took, in milliseconds.
    pub duration_ms: u64,
}

impl TestOutcome {
    /// Whether the file passed.
    pub fn passed(&self) -> bool {
        self.error.is_none() && self.document.as_ref().is_some_and(TestDocument::passed)
    }
}

/// Runs one test file.
///
/// `client` is used for this file and nothing else; the caller supplies a fresh
/// one per file.
pub async fn run_file(
    client: &mut Client,
    file: &TestFile,
    application_schemas: &[String],
    timeouts: Timeouts,
) -> TestOutcome {
    let started = Instant::now();
    let mut advanced = Vec::new();
    let result = run_inner(client, file, application_schemas, timeouts, &mut advanced).await;
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    match result {
        Ok(document) => TestOutcome {
            document: Some(document),
            error: None,
            advanced_sequences: advanced,
            duration_ms,
        },
        Err(error) => TestOutcome {
            document: None,
            error: Some(error),
            advanced_sequences: advanced,
            duration_ms,
        },
    }
}

/// Executes a test file and parses what it emitted.
async fn run_inner(
    client: &mut Client,
    file: &TestFile,
    application_schemas: &[String],
    timeouts: Timeouts,
    advanced: &mut Vec<AdvancedSequence>,
) -> Result<TestDocument> {
    // A test file that commits would escape the rollback: the whole file is
    // sent as one simple query, so statements after a `COMMIT` run outside the
    // transaction and survive it. That would silently break the isolation the
    // whole suite depends on.
    zapadka_core::lint::ensure_runner_owns_transaction(&file.sql, &file.relative_path)?;

    // Captured before the file runs, because `nextval()` survives the rollback
    // that undoes everything else.
    let sequences = snapshot_sequences(client).await?;

    let transaction = client
        .transaction()
        .await
        .map_err(|error| registry_failed(error, "begin the test transaction"))?;

    // The target's configured limits apply here too. A test blocked on a lock,
    // or looping in a query, should be cut off by the same `lock_timeout` and
    // `statement_timeout` a migration would be -- otherwise a suite can hang
    // indefinitely on a target that explicitly asked it not to.
    crate::execute::apply_timeouts(&transaction, timeouts).await?;

    transaction
        .batch_execute(&format!(
            "SET LOCAL search_path = {};",
            testlib::test_search_path(application_schemas)
        ))
        .await
        .map_err(|error| registry_failed(error, "set the test search path"))?;

    // Prepared before the file runs, so the assertions have somewhere to
    // record themselves.
    crate::capture::begin(&transaction).await?;

    // `batch_execute` rather than `simple_query`: the file's result sets are no
    // longer the test protocol, so whatever it returns is its own business. A
    // test may now contain an ordinary two-column SELECT, which the TAP-era
    // runner rejected outright.
    let outcome = transaction.batch_execute(&file.sql).await;

    // Read before the rollback, because the capture tables are ON COMMIT DROP
    // and share this transaction. Only when the file succeeded: a SQL error
    // leaves the transaction aborted, and every query against it would fail
    // with the abort rather than the original problem.
    let captured = match &outcome {
        Ok(()) => Some(crate::capture::read(&transaction).await),
        Err(_) => None,
    };

    // Rolled back on every path, whether the file passed, failed, or errored. A
    // test that could leave data behind could make the next one pass.
    transaction
        .rollback()
        .await
        .map_err(|error| registry_failed(error, "roll back the test transaction"))?;

    advanced.extend(sequences_advanced(client, &sequences).await?);

    outcome.map_err(|error| sql_error(&error, file))?;
    let document = captured.expect("a successful run always reads its results")?;

    // The columns cannot misreport an outcome, but a gap between what the
    // library recorded and what arrived here would mean results were lost --
    // and a lost failure reads as success.
    document.validate().map_err(|problem| {
        Error::new(
            ErrorCode::VerifyFailed,
            format!("{}: {problem}", file.relative_path),
        )
        .at(Location::file(&file.relative_path))
    })?;

    Ok(document)
}

/// One sequence's position.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SequenceState {
    /// Fully qualified and quoted, ready to interpolate.
    name: String,
    last_value: i64,
    is_called: bool,
}

/// A sequence a test file advanced.
#[derive(Debug, Clone)]
pub struct AdvancedSequence {
    /// The sequence's qualified name.
    pub name: String,
    /// Where it was before the file ran.
    pub was: i64,
    /// Where it is now.
    pub now: i64,
}

/// Reads the position of every sequence a test could move.
///
/// System catalogs and Zapadka's test-library schema are excluded.
///
/// The registry schema is deliberately *not* excluded. It is configurable, and
/// setting it to an application schema such as `public` is supported -- so
/// skipping the whole schema would skip the application's own sequences, which
/// are exactly the ones a test can advance. Zapadka's registry tables define no
/// sequences of their own, so there is nothing there to skip.
async fn snapshot_sequences(client: &Client) -> Result<Vec<SequenceState>> {
    // Restricted to sequences the role can actually advance. A database can
    // contain sequences belonging to other applications that this role cannot
    // touch; requiring SELECT on those would fail the suite before a single
    // test ran, for sequences no test could move anyway.
    let rows = client
        .query(
            "SELECT format('%I.%I', schemaname, sequencename) \
             FROM pg_sequences \
             WHERE schemaname NOT IN ('pg_catalog', 'information_schema', $1) \
               AND has_sequence_privilege( \
                     format('%I.%I', schemaname, sequencename), 'USAGE,UPDATE') \
             ORDER BY 1",
            &[&testlib::TEST_SCHEMA],
        )
        .await
        .map_err(|error| registry_failed(error, "list sequences"))?;

    let mut states = Vec::with_capacity(rows.len());
    for row in rows {
        let name: String = row.get(0);
        // A role can hold USAGE on a sequence -- enough to call `nextval()` --
        // without SELECT, which is what reading its position needs. Skipping it
        // would leave a sequence a test can advance and Zapadka cannot report.
        let state = client
            .query_one(&format!("SELECT last_value, is_called FROM {name}"), &[])
            .await
            .map_err(|error| {
                registry_failed(error, &format!("read sequence {name}")).with_hint(
                    "the test role needs SELECT on every sequence it can advance, so that Zapadka \
                     can tell you when a test file moved one",
                )
            })?;
        states.push(SequenceState {
            name,
            last_value: state.get(0),
            is_called: state.get(1),
        });
    }
    Ok(states)
}

/// Reports which sequences moved while the file ran.
///
/// Deliberately does not put them back. See the module documentation.
async fn sequences_advanced(
    client: &Client,
    before: &[SequenceState],
) -> Result<Vec<AdvancedSequence>> {
    let mut advanced = Vec::new();
    for state in before {
        let row = client
            .query_one(
                &format!("SELECT last_value, is_called FROM {}", state.name),
                &[],
            )
            .await
            .map_err(|error| registry_failed(error, &format!("read sequence {}", state.name)))?;

        let now = SequenceState {
            name: state.name.clone(),
            last_value: row.get(0),
            is_called: row.get(1),
        };
        if &now != state {
            advanced.push(AdvancedSequence {
                name: state.name.clone(),
                was: state.last_value,
                now: now.last_value,
            });
        }
    }
    Ok(advanced)
}

/// Turns a server error during a test file into a Zapadka error./// Turns a server error during a test file into a Zapadka error.
fn sql_error(error: &tokio_postgres::Error, file: &TestFile) -> Error {
    let database = error.as_db_error();
    let message = database.map_or_else(|| error.to_string(), |db| db.message().to_owned());

    let mut zapadka = Error::new(
        ErrorCode::VerifyFailed,
        format!("{}: {message}", file.relative_path),
    )
    .at(Location::file(&file.relative_path));

    if let Some(database) = database {
        zapadka = zapadka.with_sqlstate(database.code().code());
        if let Some(detail) = database.detail() {
            zapadka = zapadka.with_detail(detail.to_owned());
        }
        if let Some(hint) = database.hint() {
            zapadka = zapadka.with_hint(hint.to_owned());
        }
    }
    zapadka
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;
    use zapadka_core::testresult::{Assertion, Directive};

    fn assertion(number: u32, passed: bool, directive: Option<Directive>) -> Assertion {
        Assertion {
            number,
            kind: "is".to_owned(),
            passed,
            description: None,
            directive,
            detail: None,
        }
    }

    fn outcome(document: Option<TestDocument>, error: Option<Error>) -> TestOutcome {
        TestOutcome {
            document,
            error,
            advanced_sequences: Vec::new(),
            duration_ms: 1,
        }
    }

    #[test]
    fn an_outcome_with_a_failure_did_not_pass() {
        let document = TestDocument {
            assertions: vec![assertion(1, true, None), assertion(2, false, None)],
            ..TestDocument::default()
        };
        assert!(!outcome(Some(document), None).passed());
    }

    #[test]
    fn an_outcome_with_only_todo_failures_passed() {
        let document = TestDocument {
            assertions: vec![assertion(1, false, Some(Directive::Todo("later".into())))],
            ..TestDocument::default()
        };
        assert!(outcome(Some(document), None).passed());
    }

    #[test]
    fn a_file_that_errored_did_not_pass_even_with_no_results() {
        let failed = outcome(None, Some(Error::new(ErrorCode::VerifyFailed, "boom")));
        assert!(!failed.passed());
    }

    #[test]
    fn a_file_that_recorded_nothing_passed() {
        // Zero assertions is a real answer, not a failure: a file may set up
        // fixtures and assert nothing yet. The old parser could not say this
        // without a plan line to read.
        assert!(outcome(Some(TestDocument::default()), None).passed());
    }
}
