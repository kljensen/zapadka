//! The only module in Zapadka that understands PostgreSQL parse trees.
//!
//! Zapadka's safety decisions — rejecting top-level transaction control,
//! counting statements for nontransactional migrations, and classifying risky
//! DDL for `lint` — must agree with the PostgreSQL version Zapadka supports.
//! This crate wraps a pinned PostgreSQL 18 `libpg_query` build and translates
//! its parse tree into the small vocabulary the rest of Zapadka uses. Parse-tree
//! shapes never escape this crate: callers see [`Statement`] and
//! [`StatementKind`], never JSON or upstream node types.
//!
//! Zapadka deliberately does not implement its own SQL splitter or use a
//! permissive multi-dialect parser, because either would let a script escape the
//! runner's transaction boundary. See ADR-0002.

mod classify;
mod ffi;

pub use classify::{
    AlterTableAction, ConstraintKind, DropObject, QualifiedName, Statement, StatementKind,
    TransactionOperation,
};

use std::fmt;

/// A successfully parsed SQL script.
#[derive(Debug, Clone)]
pub struct ParsedScript {
    /// The `PG_VERSION_NUM` of the parser that produced this tree, e.g.
    /// `180004`. Recorded in reports so a parse decision can be attributed to a
    /// specific parser build.
    pub parser_version: u32,
    /// Top-level statements in source order. Empty for a script that contains
    /// only whitespace and comments.
    pub statements: Vec<Statement>,
}

impl ParsedScript {
    /// Returns every statement that would take Zapadka's transaction boundary
    /// away from it.
    ///
    /// Zapadka owns transaction boundaries, so a migration, verification, or
    /// test script may not begin, end, or checkpoint a transaction itself.
    pub fn transaction_control(&self) -> impl Iterator<Item = &Statement> {
        self.statements
            .iter()
            .filter(|statement| statement.kind.is_transaction_control())
    }
}

/// A syntax error reported by the PostgreSQL parser.
///
/// This is a hard error: Zapadka refuses to send a script it could not parse,
/// because it cannot prove the script respects the runner's boundaries.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message} (line {line}, column {column})")]
pub struct ParseError {
    /// The PostgreSQL parser's message, e.g. `syntax error at end of input`.
    pub message: String,
    /// 1-based line within the script.
    pub line: usize,
    /// 1-based column within the line, counted in characters.
    pub column: usize,
    /// 0-based byte offset within the script, or `None` when PostgreSQL did not
    /// report a position.
    pub offset: Option<usize>,
}

/// Parses a SQL script with the pinned PostgreSQL 18 parser.
///
/// Syntax and structure only. This does not check that referenced objects
/// exist, that types are compatible, or that the script is safe to run under
/// production load — PostgreSQL execution remains authoritative.
pub fn parse(sql: &str) -> Result<ParsedScript, ParseError> {
    let tree = ffi::parse_to_json(sql)?;
    Ok(classify::classify(&tree, sql))
}

/// Returns the `PG_VERSION_NUM` of the embedded parser without parsing a script.
pub fn parser_version() -> u32 {
    // A trivial statement is cheaper than exposing another FFI entry point, and
    // it proves the parser is actually linked and initializable.
    parse("SELECT 1")
        .expect("the embedded parser must parse a trivial statement")
        .parser_version
}

/// Renders a `PG_VERSION_NUM` as a human-readable version, e.g. `18.4`.
#[derive(Debug, Clone, Copy)]
pub struct ParserVersion(pub u32);

impl fmt::Display for ParserVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.0 / 10000, self.0 % 10000)
    }
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;

    #[test]
    fn parses_postgresql_18() {
        let script = parse("SELECT 1").unwrap();
        assert_eq!(
            script.parser_version / 10000,
            18,
            "Zapadka must be built against a PostgreSQL 18 parser, got {}",
            ParserVersion(script.parser_version)
        );
    }

    #[test]
    fn reports_position_of_syntax_errors_in_script_coordinates() {
        // The upstream error carries a byte offset; `lineno` refers to the C
        // source of the parser, so Zapadka derives line and column itself.
        let error = parse("CREATE TABLE t(i int);\nSELECT 1 FROM;").unwrap_err();
        assert_eq!(error.line, 2, "{error:?}");
        assert!(error.message.contains("syntax error"), "{error:?}");
    }

    #[test]
    fn empty_and_comment_only_scripts_have_no_statements() {
        assert!(parse("").unwrap().statements.is_empty());
        assert!(
            parse("-- nothing here\n/* or here */\n")
                .unwrap()
                .statements
                .is_empty()
        );
    }
}
