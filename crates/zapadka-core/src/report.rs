//! The public, versioned run report.
//!
//! Every command produces exactly one [`ReportV1`], whatever it did and however
//! it failed. `--output json` writes it to stdout; human rendering is a
//! separate view over the same value. A future JUnit or TAP renderer is another
//! view, never a second execution path.
//!
//! This is a compatibility contract. Within report version 1:
//!
//! - field names are stable snake_case and are not renamed or removed;
//! - new optional fields may be added;
//! - enum variants may be added, so consumers must tolerate unknown values;
//! - timestamps are RFC 3339 in UTC.
//!
//! A breaking change means a new `ReportV2` and a new `report_version`.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, ExitCode};

/// The report version this binary emits.
pub const REPORT_VERSION: u32 = 1;

/// One command run.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReportV1 {
    /// Always `1` for this model. Consumers should refuse versions they do not
    /// know rather than guessing.
    pub report_version: u32,
    /// What produced this report.
    pub tool: Tool,
    /// Identity and timing of this run.
    pub run: Run,
    /// Whether the command achieved what it was asked to do.
    pub outcome: Outcome,
    /// The process exit code that accompanies this report.
    pub exit_code: i32,
    /// The target database, when the command connected to one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<Target>,
    /// The migrations this command planned or acted on, in execution order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub migrations: Vec<MigrationResult>,
    /// Database test files, in execution order. Empty outside `zapadka test`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<TestFile>,
    /// Warnings and notes that did not by themselves fail the run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
    /// Why the run failed. Present if and only if `outcome` is `failure`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ReportError>,
}

/// What produced the report.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Tool {
    /// Always `zapadka`.
    pub name: String,
    /// The Zapadka release, e.g. `0.1.0`.
    pub version: String,
    /// The embedded PostgreSQL parser's `PG_VERSION_NUM`, e.g. `180004`.
    ///
    /// Recorded because a parser decision — accepting or rejecting a script — is
    /// only reproducible against a specific parser build.
    pub parser_version: u32,
}

/// Identity and timing of a single run.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Run {
    /// A UUIDv7 identifying this run. The same id appears on every registry
    /// event the run wrote, so a report can be joined to database history.
    pub id: Uuid,
    /// The command that ran, e.g. `deploy`.
    pub command: String,
    /// When the run started, RFC 3339 in UTC.
    pub started_at: String,
    /// When the run finished, RFC 3339 in UTC.
    pub finished_at: String,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

/// Whether a run or step achieved its goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The command did what it was asked to do.
    Success,
    /// The command did not. `error` explains why.
    Failure,
}

/// Facts observed about the target database.
///
/// Zapadka records only what it observed or was explicitly given. It never
/// guesses an operator's identity, hostname, or Git revision.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Target {
    /// The target name from `zapadka.toml`, e.g. `production`.
    pub name: String,
    /// The database that was connected to. Never includes credentials.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// `server_version` as reported by PostgreSQL, e.g. `18.4`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
    /// `session_user` at connection time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_user: Option<String>,
    /// `current_user` at connection time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_user: Option<String>,
    /// The schema holding Zapadka's registry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_schema: Option<String>,
    /// The registry format version found, before any upgrade this run applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_format_version: Option<u32>,
    /// The effective `lock_timeout` Zapadka applied, when the target set one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_timeout: Option<String>,
    /// The effective `statement_timeout` Zapadka applied, when the target set
    /// one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statement_timeout: Option<String>,
}

/// What happened to one migration during this run.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MigrationResult {
    /// The migration's permanent UUIDv7 identity.
    pub id: Uuid,
    /// The human-readable slug from its directory name.
    pub slug: String,
    /// What the run did, or intended to do, with this migration.
    pub action: Action,
    /// The result of that action.
    pub status: Status,
    /// The execution mode the manifest declared.
    pub transaction: TransactionMode,
    /// SHA-256 of the immutable deployment definition: the canonical manifest
    /// plus `deploy.sql`. This is what history integrity is checked against.
    pub definition_sha256: String,
    /// The scripts this action ran, in execution order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scripts: Vec<Script>,
    /// Wall-clock duration in milliseconds. Absent for planned-only entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Why this migration failed, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ReportError>,
}

/// The operation a run performed on a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Selected for execution but not executed, as in `--dry-run` and `status`.
    Plan,
    /// Applied the migration's `deploy.sql`.
    Deploy,
    /// Ran the migration's `verify.sql` against committed state.
    Verify,
    /// Applied the migration's `revert.sql`.
    Revert,
    /// Recorded the migration as applied without running its SQL.
    Baseline,
    /// Recorded an operator's account of an interrupted nontransactional
    /// statement, rather than something Zapadka observed.
    Resolve,
}

/// The outcome of one migration's action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Already applied; the run had nothing to do.
    Applied,
    /// Not applied, and this run did not apply it.
    Pending,
    /// This run applied or verified it successfully.
    Succeeded,
    /// This run attempted it and it failed.
    Failed,
    /// Selected but deliberately not run, e.g. after an earlier failure.
    Skipped,
    /// Started outside a transaction and never resolved: whether it took
    /// effect is unknown, and an operator must say which before deployment can
    /// continue. Deliberately neither `Applied` nor `Pending`, because it is
    /// the absence of that answer that the status exists to report.
    Blocked,
}

/// How a migration's SQL is executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransactionMode {
    /// Runs inside a transaction Zapadka opens and closes.
    Required,
    /// Runs outside any transaction, for statements PostgreSQL forbids in one.
    Forbidden,
}

/// One script execution.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Script {
    /// Which script this was.
    pub role: ScriptRole,
    /// Project-relative path, e.g. `migrations/019.../deploy.sql`.
    pub path: String,
    /// SHA-256 of the exact bytes executed. Recorded even for mutable scripts
    /// so a past run can be reproduced.
    pub sha256: String,
    /// The result of running it.
    pub status: Status,
    /// Wall-clock duration in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Why it failed, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ReportError>,
}

/// Which of a migration's scripts ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScriptRole {
    /// `deploy.sql`, the immutable script that applies the migration.
    Deploy,
    /// `revert.sql`, the mutable script that undoes it.
    Revert,
    /// `verify.sql`, the mutable script that checks it, always rolled back.
    Verify,
}

impl ScriptRole {
    /// The file name this role is stored under.
    pub fn file_name(self) -> &'static str {
        match self {
            Self::Deploy => "deploy.sql",
            Self::Revert => "revert.sql",
            Self::Verify => "verify.sql",
        }
    }
}

/// One database test file's result.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestFile {
    /// Project-relative path, e.g. `tests/db/orders.sql`.
    pub path: String,
    /// SHA-256 of the file as executed.
    pub sha256: String,
    /// Whether every assertion in the file passed.
    pub status: Status,
    /// Assertions in the order the file emitted them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assertions: Vec<Assertion>,
    /// The plan the file declared, when it declared a count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planned: Option<u64>,
    /// Wall-clock duration in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Why the file failed as a whole, e.g. a SQL error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ReportError>,
    /// Notes the file wrote with `diag()`, in order.
    ///
    /// Kept at file level rather than folded into assertions because a note
    /// written before the first assertion belongs to no assertion, and
    /// attaching it to one anyway would misreport where it came from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<TestNote>,
}

/// A note a test file wrote with `diag()`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestNote {
    /// The assertion this note followed, when it followed one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_assertion: Option<u64>,
    /// What the file said.
    pub message: String,
}

/// One assertion within a test file.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Assertion {
    /// 1-based position within the file.
    pub number: u64,
    /// The assertion's description, when it gave one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether it passed.
    pub status: AssertionStatus,
    /// The reason attached to a `TODO` or `SKIP` directive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directive_reason: Option<String>,
    /// Diagnostic fields the assertion attached, such as `have` and `want`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub diagnostics: BTreeMap<String, String>,
}

/// The result of a single assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AssertionStatus {
    /// The assertion held.
    Passed,
    /// The assertion did not hold, and it was not marked `TODO`.
    Failed,
    /// Failed, but marked `TODO`, so it does not fail the file.
    TodoFailed,
    /// Passed while marked `TODO`, which usually means the `TODO` is stale.
    TodoPassed,
    /// Not run, because it was marked `SKIP`.
    Skipped,
}

/// A warning or note that did not by itself fail the run.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Diagnostic {
    /// How seriously to take it.
    pub severity: Severity,
    /// A stable dotted identifier, e.g. `lint.destructive_drop`. Suppressions
    /// and policy promotion both key on this.
    pub code: String,
    /// One sentence describing the concern.
    pub message: String,
    /// The migration it concerns, when it concerns one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migration_id: Option<Uuid>,
    /// Where in the project it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    /// What to do about it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// How seriously to take a diagnostic.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Provably invalid. Always fails the command.
    Error,
    /// An intentional operational risk. Fails only when policy promotes it.
    Warning,
    /// Informational.
    Note,
}

/// A position in a project file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Location {
    /// Project-relative path.
    pub path: String,
    /// 1-based line, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// 1-based column in characters, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
}

impl Location {
    /// A location naming a whole file.
    pub fn file(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            line: None,
            column: None,
        }
    }

    /// A location naming a specific line and column.
    pub fn at(path: impl Into<String>, line: usize, column: usize) -> Self {
        Self {
            path: path.into(),
            line: Some(line),
            column: Some(column),
        }
    }
}

/// A failure, as it appears in a report.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReportError {
    /// The stable dotted error code, e.g. `history.definition_changed`.
    pub code: String,
    /// One sentence describing what went wrong.
    pub message: String,
    /// PostgreSQL's `SQLSTATE`, when the failure came from the server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sqlstate: Option<String>,
    /// PostgreSQL's `DETAIL`, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// What to do about it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Where in the project the problem is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    /// Additional structured facts, such as expected and actual hashes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub context: BTreeMap<String, String>,
}

impl From<&Error> for ReportError {
    fn from(error: &Error) -> Self {
        Self {
            code: error.code.as_str().to_owned(),
            message: error.message.clone(),
            sqlstate: error.sqlstate().map(str::to_owned),
            detail: error.detail().map(str::to_owned),
            hint: error.hint().map(str::to_owned),
            location: error.location().cloned(),
            context: error.context().clone(),
        }
    }
}

impl From<Error> for ReportError {
    fn from(error: Error) -> Self {
        Self::from(&error)
    }
}

impl ReportV1 {
    /// Marks the report failed and attaches the cause.
    ///
    /// The exit code always comes from the error, so a caller cannot report a
    /// failure while exiting zero.
    pub fn fail(&mut self, error: &Error) {
        self.outcome = Outcome::Failure;
        self.exit_code = error.exit_code().code();
        self.error = Some(ReportError::from(error));
    }

    /// The exit code this report implies.
    ///
    /// Derived from the recorded numeric code, not from the outcome alone: a
    /// history mismatch is not an internal error, and a helper that said so
    /// would contradict the very field it is reading.
    pub fn exit(&self) -> ExitCode {
        ExitCode::from_code(self.exit_code)
    }

    /// Serializes the report as the single JSON document written to stdout.
    ///
    /// Pretty-printed with a trailing newline: reports are read by humans in
    /// CI logs at least as often as by programs, and both tolerate whitespace.
    pub fn to_json(&self) -> String {
        let mut json = serde_json::to_string_pretty(self)
            .expect("ReportV1 is a plain data model and cannot fail to serialize");
        json.push('\n');
        json
    }

    /// Generates the JSON Schema published alongside each release.
    pub fn json_schema() -> serde_json::Value {
        let schema = schemars::schema_for!(ReportV1);
        serde_json::to_value(schema).expect("a generated schema is always valid JSON")
    }
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;

    fn sample() -> ReportV1 {
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
                finished_at: "2026-01-01T00:00:01Z".to_owned(),
                duration_ms: 1000,
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

    #[test]
    fn round_trips_through_json() {
        let report = sample();
        let json = report.to_json();
        let parsed: ReportV1 = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.report_version, REPORT_VERSION);
        assert_eq!(parsed.run.command, "deploy");
        assert_eq!(parsed.outcome, Outcome::Success);
    }

    #[test]
    fn emits_exactly_one_json_document_ending_in_a_newline() {
        let json = sample().to_json();
        assert!(json.ends_with("}\n"));
        // A single document: parsing the whole string must consume all of it.
        serde_json::from_str::<serde_json::Value>(&json).unwrap();
    }

    #[test]
    fn absent_optional_fields_are_omitted_rather_than_null() {
        // Consumers distinguish "not applicable" from "explicitly null"; Zapadka
        // never emits the latter.
        let json = sample().to_json();
        assert!(!json.contains("null"), "{json}");
        assert!(!json.contains("\"target\""), "{json}");
        assert!(!json.contains("\"error\""), "{json}");
    }

    #[test]
    fn failing_a_report_sets_a_nonzero_exit_code_and_the_cause() {
        use crate::error::{Error, ErrorCode};
        let mut report = sample();
        report.fail(
            &Error::new(ErrorCode::GraphCycle, "dependency cycle").with_hint("break the cycle"),
        );
        assert_eq!(report.outcome, Outcome::Failure);
        assert_ne!(report.exit_code, 0);
        let error = report.error.unwrap();
        assert_eq!(error.code, "graph.cycle");
        assert_eq!(error.hint.as_deref(), Some("break the cycle"));
    }

    #[test]
    fn enum_values_serialize_as_stable_snake_case() {
        assert_eq!(
            serde_json::to_string(&Outcome::Failure).unwrap(),
            "\"failure\""
        );
        assert_eq!(
            serde_json::to_string(&Action::Deploy).unwrap(),
            "\"deploy\""
        );
        assert_eq!(
            serde_json::to_string(&Status::Succeeded).unwrap(),
            "\"succeeded\""
        );
        assert_eq!(
            serde_json::to_string(&TransactionMode::Required).unwrap(),
            "\"required\""
        );
        assert_eq!(
            serde_json::to_string(&AssertionStatus::TodoFailed).unwrap(),
            "\"todo_failed\""
        );
        assert_eq!(
            serde_json::to_string(&Severity::Warning).unwrap(),
            "\"warning\""
        );
    }

    #[test]
    fn schema_generation_describes_the_model() {
        let schema = ReportV1::json_schema();
        let properties = schema.get("properties").expect("schema has properties");
        for required in ["report_version", "tool", "run", "outcome", "exit_code"] {
            assert!(properties.get(required).is_some(), "missing {required}");
        }
    }
}
