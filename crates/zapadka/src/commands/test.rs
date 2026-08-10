//! `zapadka test` — run database tests against a prepared test target.
//!
//! # Why the target must be named
//!
//! This command installs a test framework and runs arbitrary SQL. Both are
//! fine on a database that exists for testing and catastrophic on one that does
//! not. So there is no default target, no "the only target you declared", and
//! no fallback to whatever `deploy` last used. The operator names the target,
//! every time.
//!
//! # Why the migration state must already match
//!
//! Zapadka does not deploy, reset, or provision the test target. If it did, a
//! failing test could be "fixed" by a tool silently changing the database
//! underneath it, and nobody would know which schema the suite actually ran
//! against. Preparing the target is a separate, visible step — usually
//! `zapadka deploy --target test` in the line above this one in a CI script.
//!
//! # Why the whole suite holds the deployment lock
//!
//! A suite is not read-only: it installs pgTAP, and it restores sequence
//! positions after each file. Two suites overlapping on one target would race
//! on both — and each one's sequence restoration would undo the other's. A
//! suite overlapping a deploy would run against a schema changing underneath
//! it. So the lock is held from before the pgTAP install until after the last
//! file, and every per-file connection runs inside it.

use zapadka_core::config::LoadedConfig;
use zapadka_core::error::{Error, ErrorCode, Result};
use zapadka_core::graph::Graph;
use zapadka_core::report::{Assertion, AssertionStatus, Status, TestFile as TestFileReport};
use zapadka_core::tap::{Outcome, Plan};
use zapadka_core::testsuite;
use zapadka_pg::{history, lock, pgtap, testrun};

use crate::cli::TestArgs;
use crate::commands::target;
use crate::session::Session;

/// Runs `zapadka test`.
pub async fn run(
    config: &LoadedConfig,
    graph: &Graph,
    args: &TestArgs,
    session: &mut Session,
) -> Result<()> {
    if args.target.target.is_none() && args.target.uri.is_none() {
        return Err(Error::new(
            ErrorCode::TargetUnknown,
            "test requires the target to be named explicitly",
        )
        .with_hint(
            "this command installs a test framework and runs arbitrary SQL, so it never guesses \
             which database to use; pass --target or --uri",
        ));
    }

    let files = testsuite::discover(&config.root)?;
    let selected = testsuite::select(&files, &args.files)?;
    if selected.is_empty() {
        // Nothing to run is a real answer, not a failure: a project may not
        // have written tests yet.
        return Ok(());
    }

    let opened = target::open(config, &args.target, session).await?;
    let application_schemas = config
        .config
        .targets
        .get(&opened.name)
        .map(|target| target.application_schemas.clone())
        .unwrap_or_default();

    // Checked once here so an obviously unprepared target fails before waiting
    // for a lock; checked again under the lock, which is the check that counts.
    target::require_initialized(&opened.state, &opened.name)?;

    let (name, server_version) = (opened.name.clone(), opened.facts.server_version.clone());
    let schema = opened.schema.clone();
    let timeouts = opened.timeouts;
    let mut client = opened.connection.client;

    // Taken before the pgTAP install and held until the last file has run.
    let held = lock::acquire(
        &client,
        config.config.project.id,
        config.config.policy.advisory_lock_timeout,
    )
    .await?;

    let outcome = run_suite(
        config,
        graph,
        args,
        session,
        &mut client,
        &name,
        &schema,
        &server_version,
        &selected,
        &application_schemas,
        timeouts,
    )
    .await;

    let released = held.release(&client).await;
    outcome.and(released)
}

/// Runs every selected file, with the deployment lock held.
#[allow(clippy::too_many_arguments)]
async fn run_suite(
    config: &LoadedConfig,
    graph: &Graph,
    args: &TestArgs,
    session: &mut Session,
    client: &mut zapadka_pg::Client,
    name: &str,
    schema: &str,
    server_version: &str,
    selected: &[testsuite::TestFile],
    application_schemas: &[String],
    timeouts: zapadka_pg::Timeouts,
) -> Result<()> {
    // Read again now the lock is held: a deploy or revert from another checkout
    // could have finished while this command was waiting, and the suite would
    // otherwise run against a schema nobody validated.
    let state = target::refresh_state(client, config, schema).await?;
    target::require_initialized(&state, name)?;

    let plan = history::plan(graph, &state.applied)?;
    if !plan.pending.is_empty() {
        return Err(Error::new(
            ErrorCode::RegistryNotInitialized,
            format!(
                "target {name} has {} migration(s) that are not applied",
                plan.pending.len()
            ),
        )
        .with_context("pending", plan.pending.len())
        .with_hint(
            "Zapadka does not deploy to a test target implicitly; run `zapadka deploy` against it \
             first, so the schema the suite runs against is one you chose",
        ));
    }

    ensure_pgtap(client, server_version, session).await?;

    let mut failures = 0usize;
    for file in selected {
        // A fresh connection per file, so no session state can leak between
        // them: a temporary table, a `SET`, a prepared statement. A suite whose
        // result depends on file order is not a suite.
        let resolved = zapadka_pg::resolve(
            name,
            config.config.targets.get(name),
            args.target.uri.as_deref(),
        )?;
        let mut connection = zapadka_pg::connect(&resolved).await?;

        let outcome = testrun::run_file(
            &mut connection.client,
            file,
            application_schemas,
            schema,
            timeouts,
        )
        .await;
        if !outcome.passed() {
            failures += 1;
        }
        // Rollback does not undo `nextval()`, and Zapadka deliberately does not
        // rewind it -- see the runner's module documentation. Reporting it is
        // what keeps the isolation guarantee honest about its own edge.
        for sequence in &outcome.advanced_sequences {
            session.diagnose(zapadka_core::report::Diagnostic {
                severity: zapadka_core::report::Severity::Warning,
                code: "test.sequence_advanced".to_owned(),
                message: format!(
                    "{} advanced sequence {} from {} to {}",
                    file.relative_path, sequence.name, sequence.was, sequence.now
                ),
                migration_id: None,
                location: Some(zapadka_core::report::Location::file(&file.relative_path)),
                hint: Some(
                    "PostgreSQL does not roll back nextval(), and Zapadka will not rewind a \
                     sequence in case another session has already been issued a value from it. \
                     A later file that asserts on a generated id will depend on run order; \
                     assert on what the row contains instead."
                        .to_owned(),
                ),
            });
        }
        session.tests.push(to_report(file, &outcome));
    }

    if failures == 0 {
        return Ok(());
    }
    Err(Error::new(
        ErrorCode::VerifyFailed,
        format!("{failures} of {} test file(s) failed", selected.len()),
    ))
}

/// Installs pgTAP when it is absent or stale.
async fn ensure_pgtap(
    client: &mut zapadka_pg::Client,
    server_version: &str,
    session: &mut Session,
) -> Result<()> {
    let installed = pgtap::installed(client).await?;
    let reason = match &installed {
        pgtap::Installation::Current => return Ok(()),
        pgtap::Installation::Absent => "it was not installed".to_owned(),
        pgtap::Installation::Stale {
            installed_version, ..
        } => format!("the installed artifact is pgTAP {installed_version}"),
    };

    pgtap::install(client, server_version).await?;
    session.diagnose(zapadka_core::report::Diagnostic {
        severity: zapadka_core::report::Severity::Note,
        code: "test.pgtap_installed".to_owned(),
        message: format!(
            "installed pgTAP {} into {} because {reason}",
            pgtap::PGTAP_VERSION,
            pgtap::TEST_SCHEMA
        ),
        migration_id: None,
        location: None,
        hint: Some(format!(
            "{} holds no application data and is safe to drop",
            pgtap::TEST_SCHEMA
        )),
    });
    Ok(())
}

/// Converts a file's outcome into its report entry.
fn to_report(file: &testsuite::TestFile, outcome: &testrun::TestOutcome) -> TestFileReport {
    let assertions = outcome
        .document
        .as_ref()
        .map(|document| {
            document
                .assertions
                .iter()
                .map(|assertion| Assertion {
                    number: assertion.number,
                    description: assertion.description.clone(),
                    status: match assertion.outcome {
                        Outcome::Passed => AssertionStatus::Passed,
                        Outcome::Failed => AssertionStatus::Failed,
                        Outcome::TodoFailed => AssertionStatus::TodoFailed,
                        Outcome::TodoPassed => AssertionStatus::TodoPassed,
                        Outcome::Skipped => AssertionStatus::Skipped,
                    },
                    directive_reason: assertion.directive_reason.clone(),
                    diagnostics: assertion.diagnostics.clone(),
                })
                .collect()
        })
        .unwrap_or_default();

    let planned = outcome
        .document
        .as_ref()
        .and_then(|document| match document.plan {
            Plan::Count(count) => Some(count),
            Plan::SkipAll(_) => None,
        });

    TestFileReport {
        path: file.relative_path.clone(),
        sha256: file.sha256.clone(),
        status: if outcome.passed() {
            Status::Succeeded
        } else {
            Status::Failed
        },
        assertions,
        planned,
        duration_ms: Some(outcome.duration_ms),
        error: outcome.error.as_ref().map(Into::into),
    }
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;
    use zapadka_core::tap;

    fn file() -> testsuite::TestFile {
        testsuite::TestFile {
            path: camino::Utf8PathBuf::from("tests/db/orders.sql"),
            relative_path: "tests/db/orders.sql".to_owned(),
            sql: String::new(),
            sha256: "a".repeat(64),
        }
    }

    fn outcome(text: &str) -> testrun::TestOutcome {
        testrun::TestOutcome {
            document: Some(tap::parse(text).unwrap()),
            error: None,
            advanced_sequences: Vec::new(),
            duration_ms: 7,
        }
    }

    #[test]
    fn a_passing_file_reports_every_assertion() {
        let report = to_report(&file(), &outcome("1..2\nok 1 - one\nok 2 - two\n"));
        assert_eq!(report.status, Status::Succeeded);
        assert_eq!(report.planned, Some(2));
        assert_eq!(report.assertions.len(), 2);
        assert_eq!(report.assertions[0].status, AssertionStatus::Passed);
        assert_eq!(report.assertions[0].description.as_deref(), Some("one"));
    }

    #[test]
    fn a_failing_file_carries_the_failing_assertions_diagnostics() {
        let report = to_report(
            &file(),
            &outcome("1..1\nnot ok 1 - totals match\n---\nhave: 41\nwant: 42\n...\n"),
        );
        assert_eq!(report.status, Status::Failed);
        assert_eq!(report.assertions[0].status, AssertionStatus::Failed);
        assert_eq!(report.assertions[0].diagnostics["have"], "41");
    }

    #[test]
    fn a_file_that_skipped_everything_passes_and_reports_no_plan_count() {
        let report = to_report(&file(), &outcome("1..0 # SKIP not applicable here\n"));
        assert_eq!(report.status, Status::Succeeded);
        assert_eq!(report.planned, None);
        assert!(report.assertions.is_empty());
    }

    #[test]
    fn a_file_whose_sql_errored_reports_the_error_and_no_assertions() {
        let report = to_report(
            &file(),
            &testrun::TestOutcome {
                document: None,
                error: Some(
                    Error::new(ErrorCode::VerifyFailed, "relation does not exist")
                        .with_sqlstate("42P01"),
                ),
                advanced_sequences: Vec::new(),
                duration_ms: 3,
            },
        );
        assert_eq!(report.status, Status::Failed);
        assert!(report.assertions.is_empty());
        let error = report.error.unwrap();
        assert_eq!(error.sqlstate.as_deref(), Some("42P01"));
    }

    #[test]
    fn todo_and_skip_directives_survive_into_the_report() {
        let report = to_report(
            &file(),
            &outcome("1..2\nnot ok 1 - later # TODO next sprint\nok 2 - x # SKIP no data\n"),
        );
        assert_eq!(report.status, Status::Succeeded, "neither fails the file");
        assert_eq!(report.assertions[0].status, AssertionStatus::TodoFailed);
        assert_eq!(
            report.assertions[0].directive_reason.as_deref(),
            Some("next sprint")
        );
        assert_eq!(report.assertions[1].status, AssertionStatus::Skipped);
    }
}
