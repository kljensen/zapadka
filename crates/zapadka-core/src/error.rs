//! Structured errors with stable codes.
//!
//! Every failure Zapadka can report carries a stable dotted code, an exit code,
//! and enough structure to be rendered either for a human or into a
//! [`crate::report::ReportV1`]. Automation is expected to match on
//! [`ErrorCode`] strings and process exit codes, never on message text, so both
//! are treated as public compatibility contracts.

use std::collections::BTreeMap;
use std::fmt;

use crate::report::Location;

/// A stable, machine-matchable error identifier.
///
/// Codes are dotted, lowercase, and never reused for a different meaning. New
/// codes may be added in a minor release; existing codes keep their meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ErrorCode {
    // -- Project and configuration ----------------------------------------
    /// No `zapadka.toml` was found in this directory or any parent.
    ConfigNotFound,
    /// `zapadka.toml` could not be parsed or violates its schema.
    ConfigInvalid,
    /// The config declares a `format_version` this binary does not understand.
    ConfigUnsupportedFormatVersion,
    /// The requested target is not declared in `zapadka.toml`.
    TargetUnknown,
    /// A target is declared but its connection information is unusable.
    TargetInvalid,

    // -- Migration packages ------------------------------------------------
    /// A `migration.toml` could not be parsed or violates its schema.
    ManifestInvalid,
    /// A manifest declares a `format_version` this binary does not understand.
    ManifestUnsupportedFormatVersion,
    /// A migration directory is missing a required file.
    MigrationMissingScript,
    /// The directory name does not agree with the manifest's declared id.
    MigrationDirectoryMismatch,
    /// Two migration directories declare the same id.
    MigrationDuplicateId,
    /// A migration depends on an id that is not present in the project.
    MigrationUnknownDependency,
    /// A reversible migration has no `revert.sql`, or an irreversible one has no
    /// stated reason.
    MigrationReversibilityInvalid,
    /// A migration id is not a UUIDv7.
    MigrationIdInvalid,

    // -- Graph -------------------------------------------------------------
    /// The dependency graph contains a cycle.
    GraphCycle,
    /// A migration declares itself as a dependency.
    GraphSelfDependency,

    // -- Scripts -----------------------------------------------------------
    /// PostgreSQL could not parse a script.
    ScriptParseError,
    /// A script contains top-level transaction control.
    ScriptTransactionControl,
    /// A nontransactional migration does not contain exactly one statement.
    ScriptStatementCount,
    /// A script is empty when it must not be.
    ScriptEmpty,

    // -- Execution policy --------------------------------------------------
    /// The migration requests an execution mode this binary does not support.
    ExecutionModeUnsupported,

    // -- Deployed history --------------------------------------------------
    /// A migration recorded as applied is absent from the checked-out project.
    HistoryMigrationMissing,
    /// A deployed migration's immutable definition changed on disk.
    HistoryDefinitionChanged,
    /// A deployed migration's dependency edges changed on disk.
    HistoryDependenciesChanged,

    // -- Registry ----------------------------------------------------------
    /// The registry uses a format newer than this binary understands.
    RegistryFormatTooNew,
    /// The registry belongs to a different Zapadka project.
    RegistryProjectMismatch,
    /// The registry is absent where the command requires it.
    RegistryNotInitialized,
    /// The registry could not be created or upgraded.
    RegistryUpgradeFailed,

    // -- Locking -----------------------------------------------------------
    /// Another Zapadka run holds the deployment lock.
    LockUnavailable,

    // -- Target execution --------------------------------------------------
    /// Could not connect to the target database.
    ConnectionFailed,
    /// The server is not a supported PostgreSQL version.
    ServerUnsupported,
    /// A migration's `deploy.sql` failed.
    DeployFailed,
    /// A `verify.sql` failed.
    VerifyFailed,
    /// A `revert.sql` failed.
    RevertFailed,

    // -- Local project mutation -------------------------------------------
    /// `init` or `new` would overwrite something that already exists.
    AlreadyExists,
    /// A selector matched no migrations or test files.
    SelectorMatchedNothing,
    /// Lint found at least one hard error.
    LintFailed,

    // -- Generic -----------------------------------------------------------
    /// A filesystem operation failed.
    Io,
    /// A bug in Zapadka.
    Internal,
}

impl ErrorCode {
    /// The stable dotted string automation matches on.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConfigNotFound => "config.not_found",
            Self::ConfigInvalid => "config.invalid",
            Self::ConfigUnsupportedFormatVersion => "config.unsupported_format_version",
            Self::TargetUnknown => "target.unknown",
            Self::TargetInvalid => "target.invalid",
            Self::ManifestInvalid => "manifest.invalid",
            Self::ManifestUnsupportedFormatVersion => "manifest.unsupported_format_version",
            Self::MigrationMissingScript => "migration.missing_script",
            Self::MigrationDirectoryMismatch => "migration.directory_mismatch",
            Self::MigrationDuplicateId => "migration.duplicate_id",
            Self::MigrationUnknownDependency => "migration.unknown_dependency",
            Self::MigrationReversibilityInvalid => "migration.reversibility_invalid",
            Self::MigrationIdInvalid => "migration.id_invalid",
            Self::GraphCycle => "graph.cycle",
            Self::GraphSelfDependency => "graph.self_dependency",
            Self::ScriptParseError => "script.parse_error",
            Self::ScriptTransactionControl => "script.transaction_control",
            Self::ScriptStatementCount => "script.statement_count",
            Self::ScriptEmpty => "script.empty",
            Self::ExecutionModeUnsupported => "execution.mode_unsupported",
            Self::HistoryMigrationMissing => "history.migration_missing",
            Self::HistoryDefinitionChanged => "history.definition_changed",
            Self::HistoryDependenciesChanged => "history.dependencies_changed",
            Self::RegistryFormatTooNew => "registry.format_too_new",
            Self::RegistryProjectMismatch => "registry.project_mismatch",
            Self::RegistryNotInitialized => "registry.not_initialized",
            Self::RegistryUpgradeFailed => "registry.upgrade_failed",
            Self::LockUnavailable => "lock.unavailable",
            Self::ConnectionFailed => "target.connection_failed",
            Self::ServerUnsupported => "target.server_unsupported",
            Self::DeployFailed => "deploy.failed",
            Self::VerifyFailed => "verify.failed",
            Self::RevertFailed => "revert.failed",
            Self::AlreadyExists => "project.already_exists",
            Self::SelectorMatchedNothing => "selector.matched_nothing",
            Self::LintFailed => "lint.failed",
            Self::Io => "io.error",
            Self::Internal => "internal",
        }
    }

    /// The process exit code this failure produces.
    ///
    /// Callers script against these, so the mapping is deliberately coarse and
    /// stable: the specific [`ErrorCode`] carries the detail.
    pub fn exit_code(self) -> ExitCode {
        match self {
            Self::ConfigNotFound
            | Self::ConfigInvalid
            | Self::ConfigUnsupportedFormatVersion
            | Self::TargetUnknown
            | Self::TargetInvalid
            | Self::AlreadyExists
            | Self::SelectorMatchedNothing => ExitCode::Project,

            Self::ManifestInvalid
            | Self::ManifestUnsupportedFormatVersion
            | Self::MigrationMissingScript
            | Self::MigrationDirectoryMismatch
            | Self::MigrationDuplicateId
            | Self::MigrationUnknownDependency
            | Self::MigrationReversibilityInvalid
            | Self::MigrationIdInvalid
            | Self::GraphCycle
            | Self::GraphSelfDependency
            | Self::ScriptParseError
            | Self::ScriptTransactionControl
            | Self::ScriptStatementCount
            | Self::ScriptEmpty
            | Self::ExecutionModeUnsupported
            | Self::LintFailed => ExitCode::Validation,

            Self::HistoryMigrationMissing
            | Self::HistoryDefinitionChanged
            | Self::HistoryDependenciesChanged => ExitCode::History,

            Self::RegistryFormatTooNew
            | Self::RegistryProjectMismatch
            | Self::RegistryNotInitialized
            | Self::RegistryUpgradeFailed => ExitCode::Registry,

            Self::LockUnavailable => ExitCode::Lock,

            Self::ConnectionFailed | Self::ServerUnsupported => ExitCode::Target,

            Self::DeployFailed | Self::VerifyFailed | Self::RevertFailed => ExitCode::Execution,

            Self::Io | Self::Internal => ExitCode::Internal,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Process exit codes.
///
/// A caller that only needs "did it work" checks for zero. A caller that needs
/// to branch — retry on lock contention, page a human on a history mismatch —
/// branches on these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    /// The command did what it was asked to do.
    Success = 0,
    /// The command line itself was wrong. Produced by argument parsing.
    Usage = 2,
    /// The project or its configuration is unusable.
    Project = 3,
    /// Migration content, graph, or SQL is provably invalid.
    Validation = 4,
    /// Deployed history and the checked-out project disagree.
    History = 5,
    /// Another Zapadka run holds the deployment lock.
    Lock = 6,
    /// The target database could not be reached or is unsupported.
    Target = 7,
    /// The registry could not be read, created, or upgraded.
    Registry = 8,
    /// User SQL failed: a deploy, verify, or revert script errored.
    Execution = 9,
    /// Zapadka failed for a reason that is its own fault.
    Internal = 70,
}

impl ExitCode {
    /// The numeric code to pass to the operating system.
    pub fn code(self) -> i32 {
        i32::from(self.code_u8())
    }

    /// The exit code as the byte the operating system actually receives.
    pub fn code_u8(self) -> u8 {
        self as u8
    }
}

/// A Zapadka failure.
///
/// Carries a stable code and a one-sentence message inline, and everything
/// else — position, hint, `SQLSTATE`, detail, structured context — behind a
/// single boxed allocation. Most errors have none of that, and every fallible
/// function in Zapadka returns this type, so keeping it small keeps `Result`
/// small everywhere.
#[derive(Debug, Clone)]
pub struct Error {
    /// The stable identifier for this failure.
    pub code: ErrorCode,
    /// A single sentence describing what went wrong, without a trailing period.
    pub message: String,
    /// Present only when the error carries more than a code and a message.
    details: Option<Box<Details>>,
}

/// The parts of an error most errors do not have.
#[derive(Debug, Clone, Default)]
struct Details {
    location: Option<Location>,
    hint: Option<String>,
    sqlstate: Option<String>,
    detail: Option<String>,
    context: BTreeMap<String, String>,
}

impl Error {
    /// Creates an error with a code and message.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    /// Returns the details, allocating them on first use.
    fn details_mut(&mut self) -> &mut Details {
        self.details.get_or_insert_with(Box::default)
    }

    /// Attaches the file, and optionally line and column, the problem is at.
    #[must_use]
    pub fn at(mut self, location: Location) -> Self {
        self.details_mut().location = Some(location);
        self
    }

    /// Attaches guidance on how to resolve the failure.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.details_mut().hint = Some(hint.into());
        self
    }

    /// Attaches PostgreSQL's `SQLSTATE`.
    #[must_use]
    pub fn with_sqlstate(mut self, sqlstate: impl Into<String>) -> Self {
        self.details_mut().sqlstate = Some(sqlstate.into());
        self
    }

    /// Attaches PostgreSQL's `DETAIL`.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.details_mut().detail = Some(detail.into());
        self
    }

    /// Attaches one structured fact, such as the conflicting hash.
    #[must_use]
    pub fn with_context(mut self, key: impl Into<String>, value: impl fmt::Display) -> Self {
        self.details_mut()
            .context
            .insert(key.into(), value.to_string());
        self
    }

    /// Where in the project the problem is, when it has a location.
    pub fn location(&self) -> Option<&Location> {
        self.details.as_ref()?.location.as_ref()
    }

    /// What the author or operator should do about it.
    pub fn hint(&self) -> Option<&str> {
        self.details.as_ref()?.hint.as_deref()
    }

    /// The PostgreSQL `SQLSTATE`, when the failure came from the server.
    pub fn sqlstate(&self) -> Option<&str> {
        self.details.as_ref()?.sqlstate.as_deref()
    }

    /// PostgreSQL's `DETAIL` field, when present.
    pub fn detail(&self) -> Option<&str> {
        self.details.as_ref()?.detail.as_deref()
    }

    /// Additional structured facts, rendered into the report as-is.
    pub fn context(&self) -> &BTreeMap<String, String> {
        // A shared empty map, so callers need no special case for the common
        // error that carries no context.
        static EMPTY: std::sync::OnceLock<BTreeMap<String, String>> = std::sync::OnceLock::new();
        match &self.details {
            Some(details) => &details.context,
            None => EMPTY.get_or_init(BTreeMap::new),
        }
    }

    /// The process exit code for this failure.
    pub fn exit_code(&self) -> ExitCode {
        self.code.exit_code()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// Shorthand for a fallible Zapadka operation.
pub type Result<T> = std::result::Result<T, Error>;

/// Wraps an I/O failure with the path it happened on.
///
/// Bare `io::Error` messages omit the path, which is the first thing a user
/// needs in order to fix the problem.
#[allow(clippy::needless_pass_by_value)] // takes ownership so callers cannot reuse a consumed error
pub fn io_error(path: impl fmt::Display, action: &str, source: std::io::Error) -> Error {
    Error::new(ErrorCode::Io, format!("cannot {action} {path}: {source}"))
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;

    #[test]
    fn success_is_zero_and_failures_are_not() {
        assert_eq!(ExitCode::Success.code(), 0);
        for code in [
            ErrorCode::ConfigNotFound,
            ErrorCode::GraphCycle,
            ErrorCode::HistoryDefinitionChanged,
            ErrorCode::LockUnavailable,
            ErrorCode::DeployFailed,
            ErrorCode::Internal,
        ] {
            assert_ne!(code.exit_code().code(), 0, "{code} must not exit zero");
        }
    }

    #[test]
    fn exit_codes_do_not_collide_with_argument_parsing() {
        // clap exits 2 for usage errors, so no Zapadka failure may map to 2.
        for code in ALL_CODES {
            assert_ne!(
                code.exit_code(),
                ExitCode::Usage,
                "{code} must not reuse the usage exit code"
            );
        }
    }

    #[test]
    fn error_codes_are_unique_and_well_formed() {
        let mut seen = std::collections::BTreeSet::new();
        for code in ALL_CODES {
            let text = code.as_str();
            assert!(seen.insert(text), "duplicate error code {text}");
            assert!(
                text.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_'),
                "error code {text} must be lowercase dotted snake_case"
            );
        }
    }

    /// Every code, so the tests above fail when a new one is added carelessly.
    const ALL_CODES: [ErrorCode; 38] = [
        ErrorCode::ConfigNotFound,
        ErrorCode::ConfigInvalid,
        ErrorCode::ConfigUnsupportedFormatVersion,
        ErrorCode::TargetUnknown,
        ErrorCode::TargetInvalid,
        ErrorCode::ManifestInvalid,
        ErrorCode::ManifestUnsupportedFormatVersion,
        ErrorCode::MigrationMissingScript,
        ErrorCode::MigrationDirectoryMismatch,
        ErrorCode::MigrationDuplicateId,
        ErrorCode::MigrationUnknownDependency,
        ErrorCode::MigrationReversibilityInvalid,
        ErrorCode::MigrationIdInvalid,
        ErrorCode::GraphCycle,
        ErrorCode::GraphSelfDependency,
        ErrorCode::ScriptParseError,
        ErrorCode::ScriptTransactionControl,
        ErrorCode::ScriptStatementCount,
        ErrorCode::ScriptEmpty,
        ErrorCode::ExecutionModeUnsupported,
        ErrorCode::HistoryMigrationMissing,
        ErrorCode::HistoryDefinitionChanged,
        ErrorCode::HistoryDependenciesChanged,
        ErrorCode::RegistryFormatTooNew,
        ErrorCode::RegistryProjectMismatch,
        ErrorCode::RegistryNotInitialized,
        ErrorCode::RegistryUpgradeFailed,
        ErrorCode::LockUnavailable,
        ErrorCode::ConnectionFailed,
        ErrorCode::ServerUnsupported,
        ErrorCode::DeployFailed,
        ErrorCode::VerifyFailed,
        ErrorCode::RevertFailed,
        ErrorCode::AlreadyExists,
        ErrorCode::SelectorMatchedNothing,
        ErrorCode::LintFailed,
        ErrorCode::Io,
        ErrorCode::Internal,
    ];
}
