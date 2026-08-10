//! Validation and risk analysis for migration SQL.
//!
//! Zapadka draws a hard line between two kinds of finding.
//!
//! **Errors** are provable invalidity. The script does not parse, it takes the
//! transaction boundary away from the runner, or PostgreSQL will certainly
//! refuse to run it in the mode the manifest declared. These always fail the
//! command; there is no way to accept them, because accepting them would just
//! move the failure to production.
//!
//! **Warnings** are intentional operational risks: dropping data, rewriting a
//! table, taking a lock that blocks writes. Zapadka cannot know whether a
//! warning matters — dropping a column is reckless on a hot table and routine
//! on an empty one — so it reports rather than refuses. A project promotes the
//! ones it cares about with `policy.deny`, and a migration accepts a specific
//! one with an `[[allow]]` entry that states a reason.
//!
//! Warnings are deliberately conservative. The parser sees syntax, not the
//! catalog: it cannot know whether a table has rows, whether a default is
//! volatile, or whether an index already exists. A rule that guessed would
//! train people to ignore it.

use zapadka_parser::{
    AlterTableAction, ConstraintKind, ParsedScript, StatementKind, TransactionOperation,
};

use crate::config::Policy;
use crate::error::{Error, ErrorCode, Result};
use crate::manifest::Transaction;
use crate::migration::{Migration, Script};
use crate::report::{Diagnostic, Location, ScriptRole, Severity};

/// Lint rule identifiers.
///
/// These are public: they appear in reports, in `policy.deny`, and in
/// `[[allow]]` entries, so they are stable once released.
pub mod codes {
    /// Removes data or an object that holds data.
    pub const DESTRUCTIVE: &str = "lint.destructive";
    /// Rewrites an existing table, holding an exclusive lock for its duration.
    pub const TABLE_REWRITE: &str = "lint.table_rewrite";
    /// Builds an index without `CONCURRENTLY`, blocking writes meanwhile.
    pub const INDEX_WITHOUT_CONCURRENTLY: &str = "lint.index_without_concurrently";
    /// Adds a constraint that scans every existing row before it is accepted.
    pub const CONSTRAINT_SCANS_TABLE: &str = "lint.constraint_scans_table";
    /// Renames an object that running application code may still reference.
    pub const COMPATIBILITY_WINDOW: &str = "lint.compatibility_window";
    /// A script that contains no SQL.
    pub const EMPTY_SCRIPT: &str = "lint.empty_script";

    /// Every rule, for documentation and for validating `policy.deny`.
    pub const ALL: [&str; 6] = [
        DESTRUCTIVE,
        TABLE_REWRITE,
        INDEX_WITHOUT_CONCURRENTLY,
        CONSTRAINT_SCANS_TABLE,
        COMPATIBILITY_WINDOW,
        EMPTY_SCRIPT,
    ];
}

/// What this build of Zapadka is able to execute.
///
/// Alpha ships a transactional slice only. A migration that declares
/// `transaction = "forbidden"` is rejected during validation — before Zapadka
/// connects to anything — so an unsupported project fails at lint time rather
/// than partway through a deploy.
#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    /// Whether `transaction = "forbidden"` can be executed.
    pub nontransactional: bool,
}

impl Capabilities {
    /// The alpha capability set: transactional migrations only.
    pub const TRANSACTIONAL_ONLY: Self = Self {
        nontransactional: false,
    };

    /// The full capability set.
    pub const ALL: Self = Self {
        nontransactional: true,
    };
}

/// The result of linting a project.
#[derive(Debug, Clone, Default)]
pub struct Findings {
    /// Provable invalidity. Any of these fails the command.
    pub errors: Vec<Error>,
    /// Operational risks and notes.
    pub diagnostics: Vec<Diagnostic>,
}

impl Findings {
    /// Whether anything blocks execution.
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// The first error, which is the one a command reports as its failure.
    pub fn first_error(&self) -> Option<&Error> {
        self.errors.first()
    }

    /// The number of warnings, for the summary line.
    pub fn warning_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == Severity::Warning)
            .count()
    }
}

/// Rejects a script that would take the transaction boundary away from the
/// runner.
///
/// `lint` and `deploy` already check this, but they check the project as a
/// whole before connecting. Standalone `verify`, `revert`, and `test` execute a
/// script without going through that path, and `verify.sql`, `revert.sql`, and
/// test files are all *mutable* — one can acquire a `COMMIT` after the migration
/// that owns it was deployed and reviewed.
///
/// So this is called immediately before any script is executed, whatever asked
/// for it. It is the guarantee ADR-0002 actually rests on; everything earlier is
/// there to fail sooner and explain better.
pub fn ensure_runner_owns_transaction(sql: &str, path: &str) -> Result<()> {
    let parsed = zapadka_parser::parse(sql).map_err(|error| {
        Error::new(
            ErrorCode::ScriptParseError,
            format!("{path}: {}", error.message),
        )
        .at(Location::at(path, error.line, error.column))
        .with_hint("PostgreSQL could not parse this script, so Zapadka will not send it")
    })?;

    let Some(statement) = parsed.transaction_control().next() else {
        return Ok(());
    };
    let StatementKind::TransactionControl(operation) = statement.kind else {
        return Ok(());
    };

    Err(Error::new(
        ErrorCode::ScriptTransactionControl,
        format!("{path} uses {} at the top level", operation.keyword()),
    )
    .at(Location::at(path, statement.line, 1))
    .with_hint(transaction_control_hint(operation)))
}

/// Lints every migration in `migrations`.
pub fn check(migrations: &[Migration], policy: &Policy, capabilities: Capabilities) -> Findings {
    let mut findings = Findings::default();
    for migration in migrations {
        check_migration(migration, policy, capabilities, &mut findings);
    }
    findings
}

/// Lints one migration's manifest and scripts.
pub fn check_migration(
    migration: &Migration,
    policy: &Policy,
    capabilities: Capabilities,
    findings: &mut Findings,
) {
    let mode = migration.manifest.transaction;

    if mode == Transaction::Forbidden && !capabilities.nontransactional {
        findings.errors.push(
            Error::new(
                ErrorCode::ExecutionModeUnsupported,
                format!(
                    "{} declares transaction = \"forbidden\", which this build of Zapadka cannot execute",
                    migration.relative_dir
                ),
            )
            .at(Location::file(format!(
                "{}/migration.toml",
                migration.relative_dir
            )))
            .with_hint(
                "this alpha deploys transactional migrations only; nontransactional execution and \
                 its recovery workflow ship in a later release",
            ),
        );
    }

    // Every script goes through the parser, including the mutable ones: a
    // `verify.sql` that opens a transaction is just as capable of escaping the
    // runner's boundary as a `deploy.sql`.
    check_script(migration, &migration.deploy, mode, policy, findings);
    for script in [migration.revert.as_ref(), migration.verify.as_ref()]
        .into_iter()
        .flatten()
    {
        // Revert and verify always run inside a runner-owned transaction, even
        // when the deploy itself does not.
        check_script(migration, script, Transaction::Required, policy, findings);
    }
}

/// Lints one script.
fn check_script(
    migration: &Migration,
    script: &Script,
    mode: Transaction,
    policy: &Policy,
    findings: &mut Findings,
) {
    if script.runs_nothing() {
        // A `verify.sql` that runs nothing is a different matter from an empty
        // deploy script. Its presence is what makes Zapadka verify a migration
        // at all, so a no-op one is reported as a successful verification --
        // the report and the registry both claim a check happened when none
        // did. That is a false green, and false greens are the one thing a
        // verification mechanism must not produce.
        if script.role == ScriptRole::Verify {
            findings.errors.push(
                Error::new(
                    ErrorCode::ScriptEmpty,
                    format!("{} runs no statements", script.relative_path),
                )
                .at(Location::file(&script.relative_path))
                .with_hint(
                    "a verification script that does nothing would be recorded as a successful \
                     verification; write the check, or delete the file to make this migration \
                     unverified",
                ),
            );
            return;
        }

        // An empty `deploy.sql` may be a placeholder a later commit fills in,
        // so it is only a warning.
        findings.diagnostics.push(warn(
            migration,
            script,
            codes::EMPTY_SCRIPT,
            format!("{} contains no SQL", script.relative_path),
            "delete the file, or write the statements it should run",
            policy,
            None,
        ));
        return;
    }

    let parsed = match zapadka_parser::parse(&script.sql) {
        Ok(parsed) => parsed,
        Err(error) => {
            findings.errors.push(
                Error::new(
                    ErrorCode::ScriptParseError,
                    format!("{}: {}", script.relative_path, error.message),
                )
                .at(Location::at(
                    &script.relative_path,
                    error.line,
                    error.column,
                ))
                .with_hint("PostgreSQL could not parse this script, so Zapadka will not send it"),
            );
            return;
        }
    };

    check_transaction_control(script, &parsed, findings);
    check_execution_mode(migration, script, mode, &parsed, findings);
    collect_warnings(migration, script, &parsed, policy, findings);
}

/// Rejects scripts that manage transactions themselves.
fn check_transaction_control(script: &Script, parsed: &ParsedScript, findings: &mut Findings) {
    for statement in parsed.transaction_control() {
        let StatementKind::TransactionControl(operation) = statement.kind else {
            continue;
        };
        findings.errors.push(
            Error::new(
                ErrorCode::ScriptTransactionControl,
                format!(
                    "{} uses {} at the top level",
                    script.relative_path,
                    operation.keyword()
                ),
            )
            .at(Location::at(&script.relative_path, statement.line, 1))
            .with_hint(transaction_control_hint(operation)),
        );
    }
}

/// Explains why a particular transaction-control statement is not allowed, in
/// terms of what the author was probably trying to achieve.
fn transaction_control_hint(operation: TransactionOperation) -> &'static str {
    match operation {
        TransactionOperation::Begin => {
            "Zapadka already runs this script inside a transaction; delete the BEGIN"
        }
        TransactionOperation::Commit => {
            "Zapadka commits the migration when the script succeeds; delete the COMMIT"
        }
        TransactionOperation::Rollback => {
            "Zapadka rolls back when the script fails; raise an exception instead of rolling back \
             by hand"
        }
        TransactionOperation::Savepoint
        | TransactionOperation::ReleaseSavepoint
        | TransactionOperation::RollbackToSavepoint => {
            "partial rollback inside a migration would leave Zapadka unable to describe what was \
             applied; split the work into separate migrations"
        }
        TransactionOperation::SetTransaction | TransactionOperation::SetSessionCharacteristics => {
            "the transaction's properties belong to the runner; set them on the target instead"
        }
        _ => "Zapadka owns the transaction boundary for every script it runs",
    }
}

/// Checks the script against the execution mode its manifest declared.
fn check_execution_mode(
    migration: &Migration,
    script: &Script,
    mode: Transaction,
    parsed: &ParsedScript,
    findings: &mut Findings,
) {
    match mode {
        Transaction::Required => {
            for statement in &parsed.statements {
                let Some(construct) = statement.kind.forbidden_in_transaction() else {
                    continue;
                };
                findings.errors.push(
                    Error::new(
                        ErrorCode::ScriptStatementCount,
                        format!(
                            "{} runs {construct}, which PostgreSQL cannot run inside a transaction",
                            script.relative_path
                        ),
                    )
                    .at(Location::at(&script.relative_path, statement.line, 1))
                    .with_hint(format!(
                        "move {construct} into its own migration with transaction = \"forbidden\""
                    )),
                );
            }
        }
        Transaction::Forbidden => {
            // Exactly one statement, so that a failure has exactly one possible
            // meaning. With two, a crash between them would leave Zapadka
            // unable to say which had run.
            if parsed.statements.len() != 1 {
                findings.errors.push(
                    Error::new(
                        ErrorCode::ScriptStatementCount,
                        format!(
                            "{} declares transaction = \"forbidden\" but contains {} statements",
                            migration.relative_dir,
                            parsed.statements.len()
                        ),
                    )
                    .at(Location::file(&script.relative_path))
                    .with_hint(
                        "a nontransactional migration runs exactly one statement, so that an \
                         interrupted run has only one possible outcome to resolve",
                    ),
                );
            }
        }
    }
}

/// Collects operational-risk warnings.
fn collect_warnings(
    migration: &Migration,
    script: &Script,
    parsed: &ParsedScript,
    policy: &Policy,
    findings: &mut Findings,
) {
    // Only `deploy.sql` is warned about. A `revert.sql` is expected to drop what
    // its deploy created, and a `verify.sql` is read-only by construction;
    // warning about them would be noise that teaches people to ignore warnings.
    if script.role != ScriptRole::Deploy {
        return;
    }

    for statement in &parsed.statements {
        let line = Some(statement.line);
        match &statement.kind {
            StatementKind::Drop {
                object, cascade, ..
            } if object.is_data_bearing() => {
                let cascade_note = if *cascade {
                    ", and CASCADE extends that to every dependent object"
                } else {
                    ""
                };
                findings.diagnostics.push(warn(
                    migration,
                    script,
                    codes::DESTRUCTIVE,
                    format!("drops a {}{cascade_note}", object.label()),
                    "data removed here cannot be restored by reverting; confirm it is unused, and \
                     consider an expand/contract change that stops writing to it first",
                    policy,
                    line,
                ));
            }
            StatementKind::Truncate { .. } => {
                findings.diagnostics.push(warn(
                    migration,
                    script,
                    codes::DESTRUCTIVE,
                    "truncates a table".to_owned(),
                    "TRUNCATE removes every row and cannot be undone by reverting",
                    policy,
                    line,
                ));
            }
            StatementKind::CreateIndex {
                concurrent: false,
                relation,
                ..
            } => {
                let on = relation
                    .as_ref()
                    .map_or_else(String::new, |name| format!(" on {name}"));
                findings.diagnostics.push(warn(
                    migration,
                    script,
                    codes::INDEX_WITHOUT_CONCURRENTLY,
                    format!("builds an index{on} without CONCURRENTLY"),
                    "this blocks writes to the table until the index is built; on a table with \
                     existing rows, build it CONCURRENTLY in its own nontransactional migration",
                    policy,
                    line,
                ));
            }
            StatementKind::Rename { object, to } => {
                findings.diagnostics.push(warn(
                    migration,
                    script,
                    codes::COMPATIBILITY_WINDOW,
                    format!("renames a {} to {to}", object.label()),
                    "application code still using the old name breaks the moment this commits; \
                     add the new name, migrate readers and writers, then remove the old one",
                    policy,
                    line,
                ));
            }
            StatementKind::AlterTable { actions, .. } => {
                for action in actions {
                    warn_alter_table(migration, script, action, policy, line, findings);
                }
            }
            _ => {}
        }
    }
}

/// Warns about one `ALTER TABLE` action.
fn warn_alter_table(
    migration: &Migration,
    script: &Script,
    action: &AlterTableAction,
    policy: &Policy,
    line: Option<usize>,
    findings: &mut Findings,
) {
    let (code, message, hint) = match action {
        AlterTableAction::DropColumn { name, .. } => (
            codes::DESTRUCTIVE,
            format!("drops column {name}"),
            "the column's data is gone once this commits; stop reading and writing it in a \
             previous release first",
        ),
        AlterTableAction::AlterColumnType { name } => (
            codes::TABLE_REWRITE,
            format!("changes the type of column {name}"),
            "most type changes rewrite the whole table under an exclusive lock; add a new column \
             and backfill it in batches instead",
        ),
        AlterTableAction::AddColumn {
            name,
            default_calls_function: true,
            ..
        } => (
            codes::TABLE_REWRITE,
            format!("adds column {name} with a function call as its default"),
            "a volatile default rewrites every existing row; a constant default does not, so \
             prefer one, or add the column without a default and backfill it",
        ),
        AlterTableAction::SetNotNull { name } => (
            codes::CONSTRAINT_SCANS_TABLE,
            format!("sets NOT NULL on column {name}"),
            "this scans the table under an exclusive lock; add a NOT VALID check constraint, \
             validate it separately, then set NOT NULL",
        ),
        AlterTableAction::AddConstraint {
            kind,
            not_valid: false,
        } if kind.scans_existing_rows() => (
            codes::CONSTRAINT_SCANS_TABLE,
            format!(
                "adds a {} constraint that validates existing rows",
                kind.label()
            ),
            match kind {
                ConstraintKind::PrimaryKey | ConstraintKind::Unique => {
                    "build the index CONCURRENTLY first, then add the constraint USING that index"
                }
                _ => {
                    "add it NOT VALID, then VALIDATE CONSTRAINT in a separate migration, which \
                      takes a weaker lock"
                }
            },
        ),
        _ => return,
    };

    findings
        .diagnostics
        .push(warn(migration, script, code, message, hint, policy, line));
}

/// Builds one diagnostic, applying project policy and migration suppressions.
///
/// A suppression downgrades to a note rather than removing the finding, so that
/// a report still shows every risk the migration takes and the reason given for
/// it. Policy promotion beats nothing here: a suppression is migration-local and
/// reasoned, so it wins over a blanket `deny`.
fn warn(
    migration: &Migration,
    script: &Script,
    code: &str,
    message: String,
    hint: &str,
    policy: &Policy,
    line: Option<usize>,
) -> Diagnostic {
    let location = Location {
        path: script.relative_path.clone(),
        line,
        column: None,
    };

    match migration.manifest.suppression(code) {
        Some(allow) => Diagnostic {
            severity: Severity::Note,
            code: code.to_owned(),
            message: format!("{message} (accepted: {})", allow.reason),
            migration_id: Some(migration.id),
            location: Some(location),
            hint: None,
        },
        None => Diagnostic {
            severity: if policy.deny.iter().any(|denied| denied == code) {
                Severity::Error
            } else {
                Severity::Warning
            },
            code: code.to_owned(),
            message,
            migration_id: Some(migration.id),
            location: Some(location),
            hint: Some(hint.to_owned()),
        },
    }
}

/// Promotes denied warnings into errors that fail the command.
///
/// Kept separate from diagnostic construction so that a report shows the
/// finding in its natural place *and* the command fails for a reason that names
/// the project policy responsible.
pub fn apply_policy(findings: &mut Findings) {
    let promoted: Vec<Error> = findings
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Error)
        .map(|diagnostic| {
            let mut error = Error::new(ErrorCode::LintFailed, diagnostic.message.clone())
                .with_context("lint", &diagnostic.code)
                .with_hint(format!(
                    "{} is denied by policy in zapadka.toml; fix it, or accept it in this \
                     migration with an [[allow]] entry that states a reason",
                    diagnostic.code
                ));
            if let Some(location) = &diagnostic.location {
                error = error.at(location.clone());
            }
            error
        })
        .collect();
    findings.errors.extend(promoted);
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;
    use crate::manifest::Manifest;
    use camino::Utf8PathBuf;
    use uuid::Uuid;

    const ID: &str = "0198f5c0-0000-7000-8000-00000000000a";

    fn build(manifest_extra: &str, deploy: &str) -> Migration {
        let manifest = Manifest::parse(
            &format!("format_version = 1\nid = \"{ID}\"\n{manifest_extra}"),
            "migration.toml",
        )
        .unwrap();
        let relative_dir = format!("migrations/{ID}-test");
        Migration {
            id: Uuid::parse_str(ID).unwrap(),
            slug: "test".to_owned(),
            dir: Utf8PathBuf::from(&relative_dir),
            deploy: Script {
                role: ScriptRole::Deploy,
                path: Utf8PathBuf::from("deploy.sql"),
                relative_path: format!("{relative_dir}/deploy.sql"),
                sql: deploy.to_owned(),
                sha256: "0".repeat(64),
            },
            revert: None,
            verify: None,
            definition_sha256: "0".repeat(64),
            relative_dir,
            manifest,
        }
    }

    fn lint(deploy: &str) -> Findings {
        lint_with(
            "reversibility = \"irreversible\"\nirreversible_reason = \"test\"\n",
            deploy,
            &Policy::default(),
        )
    }

    fn lint_with(manifest_extra: &str, deploy: &str, policy: &Policy) -> Findings {
        let migration = build(manifest_extra, deploy);
        let mut findings = Findings::default();
        check_migration(
            &migration,
            policy,
            Capabilities::TRANSACTIONAL_ONLY,
            &mut findings,
        );
        apply_policy(&mut findings);
        findings
    }

    fn codes_of(findings: &Findings) -> Vec<String> {
        findings
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code.clone())
            .collect()
    }

    #[test]
    fn ordinary_ddl_produces_no_findings() {
        let findings = lint("CREATE TABLE app.orders (id bigint PRIMARY KEY, total numeric);");
        assert!(!findings.has_errors(), "{:?}", findings.errors);
        assert!(
            findings.diagnostics.is_empty(),
            "{:?}",
            findings.diagnostics
        );
    }

    #[test]
    fn transaction_control_is_an_error_with_an_actionable_hint() {
        let findings = lint("CREATE TABLE t(i int);\nCOMMIT;");
        let error = findings.first_error().unwrap();
        assert_eq!(error.code, ErrorCode::ScriptTransactionControl);
        assert_eq!(error.location().unwrap().line, Some(2));
        assert!(error.hint().unwrap().contains("commits"));
    }

    #[test]
    fn a_syntax_error_is_reported_at_its_position_and_stops_analysis() {
        let findings = lint("CREATE TABLE t(i int);\nSELECT 1 FROM;");
        let error = findings.first_error().unwrap();
        assert_eq!(error.code, ErrorCode::ScriptParseError);
        assert_eq!(error.location().unwrap().line, Some(2));
        // Nothing else is claimed about a script that does not parse.
        assert_eq!(findings.errors.len(), 1);
    }

    #[test]
    fn concurrent_index_creation_in_a_transactional_migration_is_an_error() {
        // PostgreSQL would refuse this at run time; saying so at lint time is
        // the whole point of embedding the parser.
        let findings = lint("CREATE INDEX CONCURRENTLY i ON t (c);");
        let error = findings.first_error().unwrap();
        assert_eq!(error.code, ErrorCode::ScriptStatementCount);
        assert!(error.hint().unwrap().contains("forbidden"));
    }

    #[test]
    fn alpha_rejects_nontransactional_migrations_before_connecting() {
        let findings = lint_with(
            "transaction = \"forbidden\"\nreversibility = \"irreversible\"\nirreversible_reason = \"t\"\n",
            "CREATE INDEX CONCURRENTLY i ON t (c);",
            &Policy::default(),
        );
        assert_eq!(
            findings.first_error().unwrap().code,
            ErrorCode::ExecutionModeUnsupported
        );
    }

    #[test]
    fn a_nontransactional_migration_must_hold_exactly_one_statement() {
        let mut findings = Findings::default();
        let migration = build(
            "transaction = \"forbidden\"\nreversibility = \"irreversible\"\nirreversible_reason = \"t\"\n",
            "CREATE INDEX CONCURRENTLY a ON t (c);\nCREATE INDEX CONCURRENTLY b ON t (d);",
        );
        check_migration(
            &migration,
            &Policy::default(),
            Capabilities::ALL,
            &mut findings,
        );
        let error = findings.first_error().unwrap();
        assert_eq!(error.code, ErrorCode::ScriptStatementCount);
        assert!(error.message.contains('2'), "{}", error.message);
    }

    #[test]
    fn warns_about_destructive_and_locking_changes() {
        for (sql, expected) in [
            ("DROP TABLE t;", codes::DESTRUCTIVE),
            ("TRUNCATE t;", codes::DESTRUCTIVE),
            ("ALTER TABLE t DROP COLUMN c;", codes::DESTRUCTIVE),
            (
                "CREATE INDEX i ON t (c);",
                codes::INDEX_WITHOUT_CONCURRENTLY,
            ),
            (
                "ALTER TABLE t ALTER COLUMN c TYPE bigint;",
                codes::TABLE_REWRITE,
            ),
            (
                "ALTER TABLE t ADD COLUMN c timestamptz DEFAULT now();",
                codes::TABLE_REWRITE,
            ),
            (
                "ALTER TABLE t ALTER COLUMN c SET NOT NULL;",
                codes::CONSTRAINT_SCANS_TABLE,
            ),
            (
                "ALTER TABLE t ADD CONSTRAINT fk FOREIGN KEY (c) REFERENCES u(id);",
                codes::CONSTRAINT_SCANS_TABLE,
            ),
            (
                "ALTER TABLE t RENAME COLUMN a TO b;",
                codes::COMPATIBILITY_WINDOW,
            ),
        ] {
            let findings = lint(sql);
            assert!(!findings.has_errors(), "{sql} should warn, not fail");
            assert_eq!(codes_of(&findings), [expected], "{sql}");
        }
    }

    #[test]
    fn the_safe_forms_of_risky_changes_do_not_warn() {
        // These are exactly the patterns the warnings steer people toward, so
        // warning about them would make the rules self-defeating.
        for sql in [
            "ALTER TABLE t ADD CONSTRAINT ck CHECK (c > 0) NOT VALID;",
            "ALTER TABLE t VALIDATE CONSTRAINT ck;",
            "ALTER TABLE t ADD COLUMN c int NOT NULL DEFAULT 0;",
            "ALTER TABLE t ADD COLUMN c int;",
            "CREATE TABLE t (i int);",
            "DROP INDEX i;",
        ] {
            let findings = lint(sql);
            assert!(!findings.has_errors(), "{sql}: {:?}", findings.errors);
            assert!(
                findings.diagnostics.is_empty(),
                "{sql}: {:?}",
                findings.diagnostics
            );
        }
    }

    #[test]
    fn a_script_of_nothing_but_comments_is_reported_as_empty() {
        // Far more likely to be mistaken for real work than a blank file, and
        // just as much a no-op.
        for sql in ["", "   \n", "-- TODO: write this\n", "/* later */"] {
            let findings = lint(sql);
            assert_eq!(codes_of(&findings), [codes::EMPTY_SCRIPT], "{sql:?}");
        }
    }

    #[test]
    fn policy_can_promote_a_warning_into_a_failure() {
        let policy = Policy {
            deny: vec![codes::DESTRUCTIVE.to_owned()],
            ..Policy::default()
        };
        let findings = lint_with(
            "reversibility = \"irreversible\"\nirreversible_reason = \"t\"\n",
            "DROP TABLE t;",
            &policy,
        );
        assert!(findings.has_errors());
        let error = findings.first_error().unwrap();
        assert_eq!(error.code, ErrorCode::LintFailed);
        assert_eq!(
            error.context().get("lint").map(String::as_str),
            Some(codes::DESTRUCTIVE)
        );
    }

    #[test]
    fn a_reasoned_suppression_beats_a_blanket_policy_denial() {
        // The migration author knows something the policy cannot: this table is
        // already unused. The reason stays visible in the report.
        let policy = Policy {
            deny: vec![codes::DESTRUCTIVE.to_owned()],
            ..Policy::default()
        };
        let findings = lint_with(
            "reversibility = \"irreversible\"\nirreversible_reason = \"t\"\n\
             [[allow]]\nlint = \"lint.destructive\"\nreason = \"the table has been unused since v4\"\n",
            "DROP TABLE t;",
            &policy,
        );
        assert!(!findings.has_errors(), "{:?}", findings.errors);
        let diagnostic = &findings.diagnostics[0];
        assert_eq!(diagnostic.severity, Severity::Note);
        assert!(
            diagnostic.message.contains("unused since v4"),
            "{}",
            diagnostic.message
        );
    }

    #[test]
    fn verify_and_revert_scripts_are_parsed_but_not_warned_about() {
        let mut migration = build(
            "reversibility = \"reversible\"\n",
            "CREATE TABLE t (i int);",
        );
        migration.revert = Some(Script {
            role: ScriptRole::Revert,
            path: Utf8PathBuf::from("revert.sql"),
            relative_path: "migrations/x/revert.sql".to_owned(),
            // Dropping the table this migration created is the correct revert,
            // and must not be reported as a destructive risk.
            sql: "DROP TABLE t;".to_owned(),
            sha256: "0".repeat(64),
        });
        migration.verify = Some(Script {
            role: ScriptRole::Verify,
            path: Utf8PathBuf::from("verify.sql"),
            relative_path: "migrations/x/verify.sql".to_owned(),
            sql: "COMMIT;".to_owned(),
            sha256: "0".repeat(64),
        });

        let mut findings = Findings::default();
        check_migration(
            &migration,
            &Policy::default(),
            Capabilities::ALL,
            &mut findings,
        );

        assert!(
            findings.diagnostics.is_empty(),
            "{:?}",
            findings.diagnostics
        );
        // But transaction control in verify.sql is still rejected.
        assert_eq!(
            findings.first_error().unwrap().code,
            ErrorCode::ScriptTransactionControl
        );
    }

    #[test]
    fn every_documented_rule_code_is_one_a_rule_actually_emits() {
        // Guards against `policy.deny` silently accepting a code nothing raises.
        for code in codes::ALL {
            assert!(code.starts_with("lint."), "{code}");
        }
        let emitted = [
            lint("DROP TABLE t;"),
            lint("ALTER TABLE t ALTER COLUMN c TYPE bigint;"),
            lint("CREATE INDEX i ON t (c);"),
            lint("ALTER TABLE t ALTER COLUMN c SET NOT NULL;"),
            lint("ALTER TABLE t RENAME TO u;"),
            lint("   "),
        ]
        .iter()
        .flat_map(codes_of)
        .collect::<std::collections::BTreeSet<_>>();
        for code in codes::ALL {
            assert!(emitted.contains(code), "no rule emits {code}");
        }
    }
}
