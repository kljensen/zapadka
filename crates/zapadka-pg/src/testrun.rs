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

use tokio_postgres::{Client, SimpleQueryMessage};
use zapadka_core::error::{Error, ErrorCode, Result};
use zapadka_core::report::Location;
use zapadka_core::tap::{self, TapDocument};
use zapadka_core::testsuite::TestFile;

use crate::error::registry_failed;
use crate::execute::Timeouts;
use crate::pgtap;

/// What one test file did.
#[derive(Debug)]
pub struct TestOutcome {
    /// The parsed TAP, when the file produced any Zapadka could read.
    pub document: Option<TapDocument>,
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
        self.error.is_none() && self.document.as_ref().is_some_and(TapDocument::passed)
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
) -> Result<TapDocument> {
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
            pgtap::test_search_path(application_schemas)
        ))
        .await
        .map_err(|error| registry_failed(error, "set the test search path"))?;

    // The whole file goes to the server as one simple query, exactly as a
    // migration does, so pgTAP's results come back as a sequence of result
    // sets in the order the file produced them.
    let outcome = transaction.simple_query(&file.sql).await;

    // Rolled back on every path, whether the file passed, failed, or errored. A
    // test that could leave data behind could make the next one pass.
    transaction
        .rollback()
        .await
        .map_err(|error| registry_failed(error, "roll back the test transaction"))?;

    advanced.extend(sequences_advanced(client, &sequences).await?);

    let messages = outcome.map_err(|error| sql_error(&error, file))?;
    let tap = collect_tap(&messages, file)?;

    tap::parse(&tap).map_err(|error| {
        Error::new(
            ErrorCode::VerifyFailed,
            format!("{}: {error}", file.relative_path),
        )
        .at(Location::file(&file.relative_path))
        .with_hint(
            "a test file must call plan(...) or no_plan() and finish(), and must emit nothing \
             but pgTAP results",
        )
    })
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
/// System catalogs and the pgTAP schema are excluded.
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
            &[&pgtap::TEST_SCHEMA],
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

/// Extracts the TAP stream from a file's result sets.
///
/// pgTAP emits TAP as rows of a single text column. Zapadka accepts exactly
/// that shape and refuses anything else: a result set with two columns, or one
/// that is not text, means the file did something other than call pgTAP, and
/// guessing which column was meant to be the output would be how a broken test
/// gets reported as passing.
///
/// Statements that produce no rows at all — `SET`, `CREATE TABLE`, an `INSERT`
/// used to set up a fixture — are ignored, because a test file legitimately
/// contains them.
fn collect_tap(messages: &[SimpleQueryMessage], file: &TestFile) -> Result<String> {
    let mut lines = Vec::new();
    let mut columns: Option<usize> = None;

    for message in messages {
        match message {
            SimpleQueryMessage::RowDescription(description) => {
                columns = Some(description.len());
            }
            SimpleQueryMessage::Row(row) => {
                let width = columns.unwrap_or_else(|| row.columns().len());
                if width != 1 {
                    return Err(protocol_error(
                        file,
                        &format!("a result with {width} columns"),
                    ));
                }
                match row.get(0) {
                    Some(value) => lines.push(value.to_owned()),
                    // A NULL cannot be a TAP line, and treating it as a blank
                    // one would silently drop whatever the file meant to say.
                    None => return Err(protocol_error(file, "a NULL result")),
                }
            }
            // A command that returned no rows.
            _ => {}
        }
    }

    Ok(lines.join("\n"))
}

/// The error for output Zapadka cannot interpret as TAP.
fn protocol_error(file: &TestFile, what: &str) -> Error {
    Error::new(
        ErrorCode::VerifyFailed,
        format!(
            "{} produced {what}, which is not pgTAP output",
            file.relative_path
        ),
    )
    .at(Location::file(&file.relative_path))
    .with_hint(
        "a test file should emit only pgTAP results; wrap other queries so they return no rows, \
         or move them into a fixture",
    )
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;
    use zapadka_core::tap::Outcome;

    fn file() -> TestFile {
        TestFile {
            path: camino::Utf8PathBuf::from("tests/db/orders.sql"),
            relative_path: "tests/db/orders.sql".to_owned(),
            sql: String::new(),
            sha256: "0".repeat(64),
        }
    }

    #[test]
    fn an_outcome_with_a_failure_did_not_pass() {
        let document = tap::parse("1..2\nok 1 - fine\nnot ok 2 - broken\n").unwrap();
        let outcome = TestOutcome {
            document: Some(document),
            error: None,
            advanced_sequences: Vec::new(),
            duration_ms: 1,
        };
        assert!(!outcome.passed());
    }

    #[test]
    fn an_outcome_with_only_todo_failures_passed() {
        let document = tap::parse("1..1\nnot ok 1 - later # TODO\n").unwrap();
        let outcome = TestOutcome {
            document: Some(document),
            error: None,
            advanced_sequences: Vec::new(),
            duration_ms: 1,
        };
        assert!(outcome.passed());
        assert_eq!(
            outcome.document.unwrap().assertions[0].outcome,
            Outcome::TodoFailed
        );
    }

    #[test]
    fn a_file_that_errored_did_not_pass_even_with_no_tap() {
        let outcome = TestOutcome {
            document: None,
            error: Some(Error::new(ErrorCode::VerifyFailed, "boom")),
            advanced_sequences: Vec::new(),
            duration_ms: 1,
        };
        assert!(!outcome.passed());
    }

    #[test]
    fn unreadable_output_names_the_file_and_says_what_to_do() {
        let error = protocol_error(&file(), "a result with 3 columns");
        assert_eq!(error.code, ErrorCode::VerifyFailed);
        assert_eq!(error.location().unwrap().path, "tests/db/orders.sql");
        assert!(error.hint().unwrap().contains("only pgTAP results"));
    }
}
