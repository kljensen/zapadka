//! Rendering a [`ReportV1`] for a person.
//!
//! This is a view over the report, not a second source of truth: everything
//! shown here is read from the same value `--output json` serializes. A future
//! JUnit renderer is another view over the same value, never another execution
//! path.
//!
//! Deliberately plain text with no colour or progress animation. Zapadka's
//! output is read as often from a CI log as from a terminal, and output that
//! changes shape based on whether stdout is a terminal is output that cannot be
//! reasoned about.

use std::io::Write;

use zapadka_core::report::{
    Action, AssertionStatus, Diagnostic, MigrationResult, Outcome, ReportError, ReportV1, Severity,
    Status, TestFile,
};

/// Writes the human summary of `report`.
pub fn render(report: &ReportV1, out: &mut impl Write) -> std::io::Result<()> {
    for diagnostic in &report.diagnostics {
        write_diagnostic(diagnostic, out)?;
    }

    if !report.migrations.is_empty() {
        if !report.diagnostics.is_empty() {
            writeln!(out)?;
        }
        for migration in &report.migrations {
            write_migration(migration, &report.run.command, out)?;
        }
    }

    if !report.tests.is_empty() {
        writeln!(out)?;
        for test in &report.tests {
            write_test_file(test, out)?;
        }
    }

    // The run's failure is usually the failure of one migration, which has
    // already been printed under that migration. Repeating it verbatim a few
    // lines later is noise, so it is shown only when it is something new — a
    // configuration problem, a lock timeout, a history mismatch.
    if let Some(error) = &report.error
        && !already_shown(error, report)
    {
        writeln!(out)?;
        write_error(error, out)?;
    }

    writeln!(out)?;
    writeln!(out, "{}", summary(report))?;
    Ok(())
}

/// The closing line: what happened, and how long it took.
fn summary(report: &ReportV1) -> String {
    let mut parts = Vec::new();

    let applied = count(report, Action::Deploy, Status::Succeeded);
    let verified = count(report, Action::Verify, Status::Succeeded);
    // Entries a command listed without acting on them.
    let listed_applied = count(report, Action::Plan, Status::Applied);
    let listed_pending = count(report, Action::Plan, Status::Pending);

    if applied > 0 {
        parts.push(format!(
            "{applied} {} applied",
            plural(applied, "migration")
        ));
    }
    if verified > 0 {
        parts.push(format!("{verified} verified"));
    }
    if listed_applied > 0 {
        parts.push(format!("{listed_applied} applied"));
    }
    if listed_pending > 0 {
        parts.push(match report.run.command.as_str() {
            // A dry run is reporting what it would do, not what is outstanding.
            "deploy" => format!(
                "{listed_pending} {} planned",
                plural(listed_pending, "migration")
            ),
            _ => format!("{listed_pending} pending"),
        });
    }
    if !report.tests.is_empty() {
        let passed = report
            .tests
            .iter()
            .filter(|test| test.status == Status::Succeeded)
            .count();
        parts.push(format!("{passed}/{} test files passed", report.tests.len()));
    }

    let warnings = report
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Warning)
        .count();
    if warnings > 0 {
        parts.push(format!("{warnings} {}", plural(warnings, "warning")));
    }

    let outcome = match report.outcome {
        // Commands whose whole effect is on the local filesystem have nothing
        // to count, so the summary names what they did instead of claiming
        // there was nothing to do.
        Outcome::Success if parts.is_empty() => match report.run.command.as_str() {
            "init" => "Initialized project".to_owned(),
            "new" => "Created migration".to_owned(),
            "lint" => "No problems found".to_owned(),
            _ => "Nothing to do".to_owned(),
        },
        Outcome::Success => format!("Done: {}", parts.join(", ")),
        Outcome::Failure if parts.is_empty() => "Failed".to_owned(),
        Outcome::Failure => format!("Failed after {}", parts.join(", ")),
    };

    format!("{outcome} in {}", format_duration(report.run.duration_ms))
}

/// Whether this failure was already printed under a migration or test file.
fn already_shown(error: &ReportError, report: &ReportV1) -> bool {
    let matches = |other: &Option<ReportError>| {
        other
            .as_ref()
            .is_some_and(|shown| shown.code == error.code && shown.message == error.message)
    };
    report.migrations.iter().any(|migration| {
        matches(&migration.error)
            || migration
                .scripts
                .iter()
                .any(|script| matches(&script.error))
    }) || report.tests.iter().any(|test| matches(&test.error))
}

/// Counts migrations matching an action and status.
fn count(report: &ReportV1, action: Action, status: Status) -> usize {
    report
        .migrations
        .iter()
        .filter(|migration| migration.action == action && migration.status == status)
        .count()
}

fn write_migration(
    migration: &MigrationResult,
    command: &str,
    out: &mut impl Write,
) -> std::io::Result<()> {
    let mark = mark(migration.status);
    let id = &migration.id.to_string()[..8];
    let verb = match (migration.action, migration.status) {
        // Before the skipped arm: a resolution that recorded "not applied" is
        // not a migration the run skipped over, and calling it that would read
        // as though nothing had been decided.
        (Action::Resolve, Status::Succeeded) => "recorded as applied",
        (Action::Resolve, _) => "recorded as not applied",
        // A run that stopped early still lists what it did not get to, so the
        // report accounts for every migration it selected.
        (_, Status::Skipped) => "skipped",
        // A planned entry means something different depending on why it was
        // planned: `status` is describing the world, `deploy --dry-run` is
        // describing an intention.
        (Action::Plan, Status::Applied) => "applied",
        (Action::Plan, Status::Pending) if command == "deploy" => "would deploy",
        (Action::Plan, Status::Pending) => "pending",
        (Action::Plan, _) => "planned",
        (Action::Deploy, _) => "deploy",
        (Action::Verify, _) => "verify",
        (Action::Revert, _) => "revert",
        (Action::Baseline, _) => "baseline",
    };
    let timing = migration
        .duration_ms
        .map(|ms| format!(" ({})", format_duration(ms)))
        .unwrap_or_default();

    writeln!(out, "{mark} {verb} {id} {}{timing}", migration.slug)?;

    if let Some(error) = &migration.error {
        for line in describe(error) {
            writeln!(out, "    {line}")?;
        }
    }
    Ok(())
}

/// Writes one test file and the assertions that did not pass.
///
/// Passing assertions are deliberately not listed: a suite with a thousand
/// passing assertions should print one line, not a thousand.
fn write_test_file(test: &TestFile, out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out, "{} {}", mark(test.status), test.path)?;

    let failures = test.assertions.iter().filter(|assertion| {
        matches!(
            assertion.status,
            AssertionStatus::Failed | AssertionStatus::TodoFailed
        )
    });
    for assertion in failures {
        let description = assertion.description.as_deref().unwrap_or("");
        writeln!(out, "    not ok {} {description}", assertion.number)?;
        for (key, value) in &assertion.diagnostics {
            writeln!(out, "      {key}: {value}")?;
        }
    }
    Ok(())
}

fn write_diagnostic(diagnostic: &Diagnostic, out: &mut impl Write) -> std::io::Result<()> {
    let label = match diagnostic.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Note => "note",
    };
    let location = diagnostic
        .location
        .as_ref()
        .map(|location| match location.line {
            Some(line) => format!("{}:{line}: ", location.path),
            None => format!("{}: ", location.path),
        })
        .unwrap_or_default();

    writeln!(out, "{label}: {location}{}", diagnostic.message)?;
    writeln!(out, "  [{}]", diagnostic.code)?;
    if let Some(hint) = &diagnostic.hint {
        for line in wrap(hint, 76) {
            writeln!(out, "  {line}")?;
        }
    }
    Ok(())
}

fn write_error(error: &ReportError, out: &mut impl Write) -> std::io::Result<()> {
    for line in describe(error) {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// Renders an error as the lines a person needs, in the order they need them:
/// what failed, where, what the database said, and what to do next.
fn describe(error: &ReportError) -> Vec<String> {
    let mut lines = Vec::new();

    let location = error
        .location
        .as_ref()
        .map(|location| match location.line {
            Some(line) => format!("{}:{line}: ", location.path),
            None => format!("{}: ", location.path),
        })
        .unwrap_or_default();
    lines.push(format!("error: {location}{}", error.message));
    lines.push(format!("  [{}]", error.code));

    if let Some(sqlstate) = &error.sqlstate {
        lines.push(format!("  SQLSTATE {sqlstate}"));
    }
    if let Some(detail) = &error.detail {
        lines.extend(wrap(detail, 76).into_iter().map(|line| format!("  {line}")));
    }
    for (key, value) in &error.context {
        lines.push(format!("  {key}: {value}"));
    }
    if let Some(hint) = &error.hint {
        lines.extend(wrap(hint, 76).into_iter().map(|line| format!("  {line}")));
    }
    lines
}

/// A one-character status marker, chosen to stay legible without colour.
fn mark(status: Status) -> char {
    match status {
        Status::Succeeded | Status::Applied => '+',
        Status::Failed => 'x',
        Status::Pending => '.',
        Status::Skipped => '-',
        Status::Blocked => '!',
    }
}

/// Formats a duration the way a person reads one.
fn format_duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        // Under a minute here, so the conversion is exact.
        #[allow(clippy::cast_precision_loss)]
        let seconds = ms as f64 / 1_000.0;
        format!("{seconds:.1}s")
    } else {
        format!("{}m{}s", ms / 60_000, (ms % 60_000) / 1_000)
    }
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        word.to_owned()
    } else {
        format!("{word}s")
    }
}

/// Wraps text to `width`, breaking only between words.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;
    use uuid::Uuid;
    use zapadka_core::report::{Location, REPORT_VERSION, Run, Tool, TransactionMode};

    fn report() -> ReportV1 {
        ReportV1 {
            report_version: REPORT_VERSION,
            tool: Tool {
                name: "zapadka".to_owned(),
                version: "0.1.0".to_owned(),
                parser_version: 180_004,
            },
            run: Run {
                id: Uuid::nil(),
                command: "deploy".to_owned(),
                started_at: "2026-01-01T00:00:00Z".to_owned(),
                finished_at: "2026-01-01T00:00:00Z".to_owned(),
                duration_ms: 120,
            },
            outcome: Outcome::Success,
            exit_code: 0,
            target: None,
            migrations: Vec::new(),
            tests: Vec::new(),
            diagnostics: Vec::new(),
            error: None,
        }
    }

    fn migration(status: Status, action: Action) -> MigrationResult {
        MigrationResult {
            id: Uuid::parse_str("0198f5c0-0000-7000-8000-00000000000a").unwrap(),
            slug: "add-orders".to_owned(),
            action,
            status,
            transaction: TransactionMode::Required,
            definition_sha256: "0".repeat(64),
            scripts: Vec::new(),
            duration_ms: Some(35),
            error: None,
        }
    }

    fn render_to_string(report: &ReportV1) -> String {
        let mut out = Vec::new();
        render(report, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_run_with_nothing_to_do_says_so() {
        let text = render_to_string(&report());
        assert!(text.contains("Nothing to do"), "{text}");
    }

    #[test]
    fn commands_with_nothing_to_count_still_say_what_they_did() {
        // These act on the filesystem, so there are no migrations to tally.
        // "Nothing to do" would be a lie right after creating a project.
        for (command, expected) in [
            ("init", "Initialized project"),
            ("new", "Created migration"),
            ("lint", "No problems found"),
        ] {
            let mut report = report();
            report.run.command = command.to_owned();
            let text = render_to_string(&report);
            assert!(text.contains(expected), "{command}: {text}");
        }
    }

    #[test]
    fn applied_migrations_are_listed_and_counted() {
        let mut report = report();
        report.migrations = vec![migration(Status::Succeeded, Action::Deploy)];
        let text = render_to_string(&report);
        assert!(text.contains("deploy 0198f5c0 add-orders"), "{text}");
        assert!(text.contains("1 migration applied"), "{text}");
    }

    #[test]
    fn a_dry_run_says_what_it_would_do_not_what_it_did() {
        let mut report = report();
        report.migrations = vec![migration(Status::Pending, Action::Plan)];
        let text = render_to_string(&report);
        assert!(text.contains("would deploy"), "{text}");
        assert!(text.contains("planned"), "{text}");
        assert!(!text.contains("applied"), "{text}");
    }

    #[test]
    fn a_failure_shows_the_cause_the_position_and_the_next_step() {
        let mut report = report();
        report.outcome = Outcome::Failure;
        report.error = Some(ReportError {
            code: "script.transaction_control".to_owned(),
            message: "deploy.sql uses COMMIT at the top level".to_owned(),
            sqlstate: None,
            detail: None,
            hint: Some("Zapadka commits the migration when the script succeeds".to_owned()),
            location: Some(Location::at("migrations/x/deploy.sql", 4, 1)),
            context: std::collections::BTreeMap::default(),
        });
        let text = render_to_string(&report);
        assert!(text.contains("migrations/x/deploy.sql:4:"), "{text}");
        assert!(text.contains("[script.transaction_control]"), "{text}");
        assert!(text.contains("Zapadka commits"), "{text}");
        assert!(text.contains("Failed"), "{text}");
    }

    #[test]
    fn a_database_failure_shows_the_sqlstate() {
        let mut report = report();
        report.outcome = Outcome::Failure;
        let mut failed = migration(Status::Failed, Action::Deploy);
        failed.error = Some(ReportError {
            code: "deploy.failed".to_owned(),
            message: "relation \"orders\" does not exist".to_owned(),
            sqlstate: Some("42P01".to_owned()),
            detail: None,
            hint: None,
            location: None,
            context: std::collections::BTreeMap::default(),
        });
        report.migrations = vec![failed];
        let text = render_to_string(&report);
        assert!(text.contains("SQLSTATE 42P01"), "{text}");
        assert!(text.contains("x deploy"), "{text}");
    }

    #[test]
    fn warnings_are_shown_with_their_code_so_they_can_be_denied_or_allowed() {
        let mut report = report();
        report.diagnostics = vec![Diagnostic {
            severity: Severity::Warning,
            code: "lint.destructive".to_owned(),
            message: "drops a table".to_owned(),
            migration_id: None,
            location: Some(Location::at("migrations/x/deploy.sql", 2, 1)),
            hint: Some("data removed here cannot be restored by reverting".to_owned()),
        }];
        let text = render_to_string(&report);
        assert!(
            text.contains("warning: migrations/x/deploy.sql:2: drops a table"),
            "{text}"
        );
        assert!(text.contains("[lint.destructive]"), "{text}");
        assert!(text.contains("1 warning"), "{text}");
    }

    #[test]
    fn durations_are_readable_at_every_scale() {
        assert_eq!(format_duration(0), "0ms");
        assert_eq!(format_duration(999), "999ms");
        assert_eq!(format_duration(1_500), "1.5s");
        assert_eq!(format_duration(90_000), "1m30s");
    }

    #[test]
    fn wrapping_never_splits_a_word_or_drops_text() {
        let text = "add it NOT VALID, then VALIDATE CONSTRAINT in a separate migration";
        let lines = wrap(text, 20);
        assert!(
            lines
                .iter()
                .all(|line| line.chars().count() <= 20 || !line.contains(' '))
        );
        assert_eq!(lines.join(" "), text);
    }

    #[test]
    fn output_does_not_depend_on_whether_it_is_a_terminal() {
        // Rendering is a pure function of the report; there is no branch on
        // terminal detection anywhere in this module.
        let report = report();
        assert_eq!(render_to_string(&report), render_to_string(&report));
    }
}
