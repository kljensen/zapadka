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

use std::time::Instant;

use tokio_postgres::{Client, SimpleQueryMessage};
use zapadka_core::error::{Error, ErrorCode, Result};
use zapadka_core::report::Location;
use zapadka_core::tap::{self, TapDocument};
use zapadka_core::testsuite::TestFile;

use crate::error::registry_failed;
use crate::pgtap;

/// What one test file did.
#[derive(Debug)]
pub struct TestOutcome {
    /// The parsed TAP, when the file produced any Zapadka could read.
    pub document: Option<TapDocument>,
    /// Why the file failed, when it did.
    pub error: Option<Error>,
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
) -> TestOutcome {
    let started = Instant::now();
    let result = run_inner(client, file, application_schemas).await;
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    match result {
        Ok(document) => TestOutcome {
            document: Some(document),
            error: None,
            duration_ms,
        },
        Err(error) => TestOutcome {
            document: None,
            error: Some(error),
            duration_ms,
        },
    }
}

/// Executes a test file and parses what it emitted.
async fn run_inner(
    client: &mut Client,
    file: &TestFile,
    application_schemas: &[String],
) -> Result<TapDocument> {
    let transaction = client
        .transaction()
        .await
        .map_err(|error| registry_failed(error, "begin the test transaction"))?;

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

/// Turns a server error during a test file into a Zapadka error.
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
