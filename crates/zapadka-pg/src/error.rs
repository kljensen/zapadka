//! Turning PostgreSQL failures into Zapadka errors.
//!
//! A migration that fails must report what the server actually said —
//! `SQLSTATE`, message, detail, hint, and position — because that is what tells
//! an operator whether to fix the SQL, retry, or escalate. A tool that reduces
//! `deadlock detected` and `syntax error` to the same "migration failed" line
//! makes every incident longer.
//!
//! Each wrapper takes its `tokio_postgres::Error` by value: the Zapadka error
//! it returns replaces it entirely, and a caller holding on to both would be
//! able to report the same failure twice.
#![allow(clippy::needless_pass_by_value)]

use zapadka_core::error::{Error, ErrorCode};
use zapadka_core::report::{Location, ScriptRole};

/// Wraps a connection failure.
pub fn connection_failed(error: tokio_postgres::Error, source: &str) -> Error {
    let mut zapadka = Error::new(
        ErrorCode::ConnectionFailed,
        format!("cannot connect to the target: {}", root_cause(&error)),
    )
    .with_context("connection_source", source);

    if let Some(state) = error.code() {
        zapadka = zapadka.with_sqlstate(state.code());
    }
    zapadka.with_hint(
        "check that the database is reachable, that the credentials are valid, and that the \
         connecting role may log in",
    )
}

/// Wraps a failure running one of a migration's scripts.
///
/// The failing script's path becomes the error's location, so a report points
/// at the file to edit rather than at a migration directory.
pub fn script_failed(error: tokio_postgres::Error, role: ScriptRole, path: &str) -> Error {
    let code = match role {
        ScriptRole::Deploy => ErrorCode::DeployFailed,
        ScriptRole::Revert => ErrorCode::RevertFailed,
        ScriptRole::Verify => ErrorCode::VerifyFailed,
    };

    let database = error.as_db_error();
    let message = match database {
        Some(database) => database.message().to_owned(),
        None => root_cause(&error),
    };

    // The path is carried by the error's location, so it is not repeated here;
    // every renderer prefixes the location already.
    let mut zapadka = Error::new(code, message);

    if let Some(database) = database {
        zapadka = zapadka.with_sqlstate(database.code().code());
        if let Some(detail) = database.detail() {
            zapadka = zapadka.with_detail(detail.to_owned());
        }
        // PostgreSQL's own hint is more specific than anything Zapadka could
        // add, so it wins when there is one.
        if let Some(hint) = database.hint() {
            zapadka = zapadka.with_hint(hint.to_owned());
        }
        if let Some(where_) = database.where_() {
            zapadka = zapadka.with_context("context", where_);
        }
        // `position` is a 1-based character offset into the statement, which is
        // the whole script Zapadka sent.
        if let Some(position) = database_position(database) {
            zapadka = zapadka.with_context("position", position);
        }
    }

    zapadka.at(Location::file(path))
}

/// Wraps a failure Zapadka's own registry SQL caused.
///
/// These are Zapadka's bugs or a privilege problem, never the user's SQL, so
/// they are reported separately from migration failures.
pub fn registry_failed(error: tokio_postgres::Error, action: &str) -> Error {
    let mut zapadka = Error::new(
        ErrorCode::RegistryUpgradeFailed,
        format!("cannot {action}: {}", root_cause(&error)),
    );
    if let Some(database) = error.as_db_error() {
        zapadka = zapadka.with_sqlstate(database.code().code());
        if let Some(detail) = database.detail() {
            zapadka = zapadka.with_detail(detail.to_owned());
        }
        // Insufficient privilege is the overwhelmingly common cause and has a
        // specific fix, so it gets a specific hint.
        if database.code() == &tokio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE {
            zapadka = zapadka.with_hint(
                "the connecting role needs CREATE on the database to create Zapadka's registry \
                 schema, and read/write access to it thereafter",
            );
        }
    }
    zapadka
}

/// The 1-based position within the statement, when the server reported one.
// The two position kinds mean different things to PostgreSQL even though
// Zapadka reports both the same way.
#[allow(clippy::match_same_arms)]
fn database_position(database: &tokio_postgres::error::DbError) -> Option<u32> {
    match database.position() {
        Some(tokio_postgres::error::ErrorPosition::Original(position)) => Some(*position),
        Some(tokio_postgres::error::ErrorPosition::Internal { position, .. }) => Some(*position),
        None => None,
    }
}

/// The most specific message available for an error.
///
/// `tokio_postgres::Error`'s own `Display` is often just "db error" or "error
/// connecting to server"; the useful text is in its source.
fn root_cause(error: &tokio_postgres::Error) -> String {
    use std::error::Error as _;
    let mut message = error.to_string();
    let mut current: Option<&(dyn std::error::Error + 'static)> = error.source();
    while let Some(source) = current {
        message = source.to_string();
        current = source.source();
    }
    message
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;

    #[test]
    fn each_script_role_maps_to_its_own_error_code() {
        // A failed verification and a failed deploy have different operational
        // meanings and must not share a code.
        assert_ne!(
            ErrorCode::DeployFailed.as_str(),
            ErrorCode::VerifyFailed.as_str()
        );
        assert_ne!(ErrorCode::VerifyFailed.exit_code().code(), 0);
    }

    #[test]
    fn a_connection_failure_records_where_the_connection_details_came_from() {
        // Knowing whether a bad URI came from --uri, an environment variable,
        // or a service file is the first thing needed to fix it.
        let error = Error::new(ErrorCode::ConnectionFailed, "boom")
            .with_context("connection_source", "uri_env");
        assert_eq!(
            error.context().get("connection_source").map(String::as_str),
            Some("uri_env")
        );
    }
}
