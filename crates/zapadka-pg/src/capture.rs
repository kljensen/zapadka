//! Reading a test file's recorded results out of the database.
//!
//! The assertion library records into temporary tables the runner created, so
//! this is three ordinary queries against typed columns. There is no parsing
//! step and therefore no class of bug where prose is misread as an outcome.
//!
//! Everything here must run *before* the rollback that disposes of the test's
//! work, because the capture tables are `ON COMMIT DROP` and live in the same
//! transaction.

use tokio_postgres::Transaction;
use zapadka_core::error::{Error, ErrorCode, Result};
use zapadka_core::testresult::{Assertion, Directive, Note, PlanDeclaration, TestDocument};

use crate::error::registry_failed;

/// The capture-table layout this binary understands.
///
/// Compared against what the installed library reports. A mismatch means the
/// schema was installed by a different Zapadka, and guessing at columns that
/// may have changed meaning is exactly what a versioned protocol exists to
/// prevent.
pub const PROTOCOL_VERSION: i32 = 1;

/// Prepares the capture tables for one file.
///
/// The library's location is not a parameter: it is always the reserved test
/// schema, and threading a schema through here only creates the opportunity to
/// pass the registry's by mistake.
pub async fn begin(transaction: &Transaction<'_>) -> Result<()> {
    let quoted = crate::registry::quote_identifier(crate::testlib::TEST_SCHEMA);
    transaction
        .batch_execute(&format!("SELECT {quoted}._begin_run()"))
        .await
        .map_err(|error| registry_failed(error, "prepare the test capture tables"))
}

/// Reads what the file recorded.
pub async fn read(transaction: &Transaction<'_>) -> Result<TestDocument> {
    let run = transaction
        .query_opt(
            "SELECT protocol_version, plan_mode, declared_plan, finished \
             FROM pg_temp.__zapadka_run",
            &[],
        )
        .await
        .map_err(|error| registry_failed(error, "read the test run state"))?
        .ok_or_else(|| {
            // The runner created this row itself, so its absence means the test
            // file removed it. That is not something to paper over: every
            // assertion count below would be unverifiable.
            Error::new(
                ErrorCode::VerifyFailed,
                "the test file destroyed Zapadka's capture state",
            )
            .with_hint(
                "something in the file dropped or truncated the temporary tables in pg_temp that \
                 record assertions; Zapadka cannot report on a run whose evidence is gone",
            )
        })?;

    let protocol: i32 = run.get(0);
    if protocol != PROTOCOL_VERSION {
        return Err(Error::new(
            ErrorCode::VerifyFailed,
            format!(
                "the installed test library records protocol {protocol}, but this Zapadka reads \
                 {PROTOCOL_VERSION}"
            ),
        )
        .with_context("installed_protocol", protocol)
        .with_context("supported_protocol", PROTOCOL_VERSION)
        .with_hint(format!(
            "drop the {} schema so the matching library is installed, or use the Zapadka that \
             installed it",
            crate::testlib::TEST_SCHEMA
        )));
    }

    let plan_mode: Option<String> = run.get(1);
    let declared: Option<i32> = run.get(2);
    let plan = match plan_mode.as_deref() {
        Some("count") => declared
            .and_then(|count| u32::try_from(count).ok())
            .map(PlanDeclaration::Count),
        Some("no_plan") => Some(PlanDeclaration::NoPlan),
        _ => None,
    };

    let assertions = transaction
        .query(
            "SELECT number, kind, passed, description, directive, directive_reason, detail \
             FROM pg_temp.__zapadka_assertion ORDER BY number",
            &[],
        )
        .await
        .map_err(|error| registry_failed(error, "read the recorded assertions"))?
        .into_iter()
        .map(|row| {
            let directive: Option<String> = row.get(4);
            let reason: Option<String> = row.get(5);
            Assertion {
                number: u32::try_from(row.get::<_, i32>(0)).unwrap_or(0),
                kind: row.get(1),
                passed: row.get(2),
                description: row.get(3),
                directive: match directive.as_deref() {
                    Some("todo") => Some(Directive::Todo(reason.unwrap_or_default())),
                    Some("skip") => Some(Directive::Skip(reason.unwrap_or_default())),
                    _ => None,
                },
                detail: row.get(6),
            }
        })
        .collect();

    let notes = transaction
        .query(
            "SELECT after_assertion, message FROM pg_temp.__zapadka_note ORDER BY ordinal",
            &[],
        )
        .await
        .map_err(|error| registry_failed(error, "read the recorded notes"))?
        .into_iter()
        .map(|row| Note {
            after_assertion: row
                .get::<_, Option<i32>>(0)
                .and_then(|number| u32::try_from(number).ok()),
            message: row.get(1),
        })
        .collect();

    Ok(TestDocument {
        plan,
        finished: run.get(3),
        assertions,
        notes,
    })
}
