//! The lifecycle every command shares.
//!
//! A command does not decide how it is reported. It records what it did into a
//! [`Session`] and returns either success or one [`Error`]; the session turns
//! that into exactly one [`ReportV1`], whether the command succeeded, failed
//! partway, or never started. That is what makes "every command produces one
//! report" true rather than aspirational — there is no path that skips it.

use std::time::Instant;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;
use zapadka_core::error::{Error, ExitCode};
#[cfg(test)]
use zapadka_core::report::Severity;
use zapadka_core::report::{
    Diagnostic, MigrationResult, Outcome, REPORT_VERSION, ReportV1, Run, Target, TestFile, Tool,
};

/// The Zapadka release, taken from the crate version at build time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Accumulates what a command did, then renders the run report.
#[derive(Debug)]
pub struct Session {
    /// Identifies this run in both the report and every registry event it
    /// writes, so a report can be joined to database history after the fact.
    pub run_id: Uuid,
    command: String,
    started_at: OffsetDateTime,
    monotonic_start: Instant,
    /// The target, once a command has connected and observed it.
    pub target: Option<Target>,
    /// Migrations planned or acted on, in execution order.
    pub migrations: Vec<MigrationResult>,
    /// Test files, in execution order.
    pub tests: Vec<TestFile>,
    /// Warnings and notes.
    pub diagnostics: Vec<Diagnostic>,
}

impl Session {
    /// Starts a run of `command`.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            // UUIDv7 so that runs sort by start time in the events table.
            run_id: Uuid::now_v7(),
            command: command.into(),
            started_at: OffsetDateTime::now_utc(),
            monotonic_start: Instant::now(),
            target: None,
            migrations: Vec::new(),
            tests: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    /// Records a diagnostic.
    pub fn diagnose(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }

    /// Records several diagnostics.
    pub fn diagnose_all(&mut self, diagnostics: impl IntoIterator<Item = Diagnostic>) {
        self.diagnostics.extend(diagnostics);
    }

    /// How many warnings have been recorded.
    ///
    /// Only tests need this: production code reads the finished report, which
    /// is the single source of truth for what a run found.
    #[cfg(test)]
    pub fn warning_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == Severity::Warning)
            .count()
    }

    /// Milliseconds since the run started.
    pub fn elapsed_ms(&self) -> u64 {
        // Saturating rather than truncating: a wrapped duration in a report
        // used to investigate an incident would be actively misleading.
        u64::try_from(self.monotonic_start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Renders the report for a finished run.
    ///
    /// Duration is measured monotonically rather than by subtracting
    /// timestamps, so a clock adjustment mid-run cannot produce a negative or
    /// wildly wrong duration in a report someone is using to investigate an
    /// incident.
    pub fn finish(self, error: Option<&Error>) -> ReportV1 {
        // Measured once, so the duration in the report and the interval between
        // its two timestamps always agree.
        let duration_ms = self.elapsed_ms();
        let finished_at = self.started_at
            + time::Duration::milliseconds(i64::try_from(duration_ms).unwrap_or(i64::MAX));
        let mut report = ReportV1 {
            report_version: REPORT_VERSION,
            tool: Tool {
                name: "zapadka".to_owned(),
                version: VERSION.to_owned(),
                parser_version: zapadka_parser::parser_version(),
            },
            run: Run {
                id: self.run_id,
                command: self.command,
                started_at: format_time(self.started_at),
                finished_at: format_time(finished_at),
                duration_ms,
            },
            outcome: Outcome::Success,
            exit_code: ExitCode::Success.code(),
            target: self.target,
            migrations: self.migrations,
            tests: self.tests,
            diagnostics: self.diagnostics,
            error: None,
        };
        if let Some(error) = error {
            report.fail(error);
        }
        report
    }
}

/// Formats a timestamp as RFC 3339 in UTC.
fn format_time(time: OffsetDateTime) -> String {
    time.format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;
    use zapadka_core::error::ErrorCode;

    #[test]
    fn a_successful_run_reports_success_and_exits_zero() {
        let report = Session::new("init").finish(None);
        assert_eq!(report.outcome, Outcome::Success);
        assert_eq!(report.exit_code, 0);
        assert!(report.error.is_none());
        assert_eq!(report.run.command, "init");
        assert_eq!(report.report_version, REPORT_VERSION);
    }

    #[test]
    fn a_failed_run_reports_the_error_and_a_matching_exit_code() {
        let error = Error::new(ErrorCode::GraphCycle, "dependency cycle");
        let report = Session::new("deploy").finish(Some(&error));
        assert_eq!(report.outcome, Outcome::Failure);
        assert_eq!(report.exit_code, ExitCode::Validation.code());
        assert_eq!(report.error.unwrap().code, "graph.cycle");
    }

    #[test]
    fn work_recorded_before_a_failure_is_still_reported() {
        // A deploy that fails on its third migration must still show that the
        // first two were applied; that is the difference between a report and
        // an error message.
        let mut session = Session::new("deploy");
        session.diagnose(Diagnostic {
            severity: Severity::Warning,
            code: "lint.destructive".to_owned(),
            message: "drops a table".to_owned(),
            migration_id: None,
            location: None,
            hint: None,
        });
        let report = session.finish(Some(&Error::new(ErrorCode::DeployFailed, "boom")));
        assert_eq!(report.outcome, Outcome::Failure);
        assert_eq!(report.diagnostics.len(), 1);
    }

    #[test]
    fn timestamps_are_rfc_3339_in_utc() {
        let report = Session::new("status").finish(None);
        assert!(
            report.run.started_at.ends_with('Z'),
            "{}",
            report.run.started_at
        );
        OffsetDateTime::parse(&report.run.started_at, &Rfc3339).unwrap();
        OffsetDateTime::parse(&report.run.finished_at, &Rfc3339).unwrap();
    }

    #[test]
    fn each_run_has_its_own_sortable_identity() {
        let first = Session::new("deploy").run_id;
        let second = Session::new("deploy").run_id;
        assert_ne!(first, second);
        assert_eq!(first.get_version_num(), 7);
        // UUIDv7 is time-ordered, so events sort by run start.
        assert!(first < second);
    }

    #[test]
    fn the_report_records_which_parser_made_the_safety_decisions() {
        let report = Session::new("lint").finish(None);
        assert_eq!(report.tool.parser_version / 10000, 18);
        assert_eq!(report.tool.name, "zapadka");
    }
}
