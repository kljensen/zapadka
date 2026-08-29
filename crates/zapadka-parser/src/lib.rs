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

/// Options for PostgreSQL's canonical SQL deparser.
///
/// The defaults intentionally define Zapadka's single project style: four
/// spaces, 80 columns, trailing newline, and conventional trailing commas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatOptions {
    /// Whether to emit multiline, indented SQL.
    pub pretty_print: bool,
    /// Spaces per indentation level.
    pub indent_size: u16,
    /// Preferred maximum rendered line width.
    pub max_line_length: u16,
    /// Whether to end the result with one newline.
    pub trailing_newline: bool,
    /// Whether commas begin rather than end list lines.
    pub commas_start_of_line: bool,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            pretty_print: true,
            indent_size: 4,
            max_line_length: 80,
            trailing_newline: true,
            commas_start_of_line: false,
        }
    }
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

/// Formats PostgreSQL SQL with the pinned libpg_query deparser.
///
/// Zapadka parses to libpg_query's protobuf tree, obtains its accompanying
/// comment mapping, and deparses both together. This preserves comments in
/// their intended statement context while normalizing syntactic spelling.
pub fn format(sql: &str, options: FormatOptions) -> Result<String, ParseError> {
    // libpg_query attaches source comments to the newly deparsed tree. On a
    // few complex comment layouts that mapping changes whitespace once (for
    // example an inline comment following a type option), so a single pass is
    // not necessarily a canonical form. Iterate to the fixed point instead:
    // callers of a formatter must be able to run it twice without a diff.
    let mut formatted = ffi::format(sql, options)?;
    for _ in 0..3 {
        let next = ffi::format(&formatted, options)?;
        if next == formatted {
            return Ok(formatted);
        }
        formatted = next;
    }
    Ok(formatted)
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

    #[test]
    fn preserves_comments_from_pgformatter_regression_fixture() {
        // Verbatim pgFormatter regression fixture. Its local provenance file
        // records the upstream path and PostgreSQL-license terms.
        let source = include_str!("../tests/fixtures/pgformatter-comments.sql");
        let formatted = format(source, FormatOptions::default()).unwrap();
        for comment in [
            "-- COMMENTS",
            "-- trailing single line",
            "/* embedded single line */",
            "/* both embedded and trailing single line */",
            "This is an example of SQL which should not execute",
            "SELECT 'trailing' as x1; -- inside block comment",
            "SELECT 'deepest nest' as n3;",
        ] {
            assert!(
                formatted.contains(comment),
                "missing {comment:?} in {formatted}"
            );
        }
    }

    #[test]
    fn pgformatter_create_type_fixture_is_idempotent_and_reparseable() {
        // Verbatim pgFormatter PostgreSQL regression fixture. It covers type
        // options, shell and composite types, internal-language functions,
        // comments, casts, and a long multi-statement script.
        let source = include_str!("../tests/fixtures/pgformatter-create-type.sql");
        assert_formatter_invariants(source);
    }

    #[test]
    fn pganalyze_pretty_print_fixture_has_its_upstream_rendering() {
        // Verbatim query and expected rendering from pganalyze/pg_query's
        // deparse_pretty_print_spec.rb. This is deliberately an exact output
        // test, complementing the larger invariant-only pgFormatter corpus.
        let source = "SELECT a AS b\nFROM x\nWHERE\n    y = 5\n    AND z = y";
        let options = FormatOptions {
            trailing_newline: false,
            ..FormatOptions::default()
        };
        assert_eq!(format(source, options).unwrap(), source);
    }

    #[test]
    fn formatting_is_idempotent_and_preserves_statement_classification() {
        // The CREATE FUNCTION shape is adapted from pgFormatter's upstream
        // PostgreSQL regression corpus (create_function_3.sql). Dollar quoted
        // bodies must remain literals to the outer SQL formatter.
        let source = r"
CREATE TABLE public.orders(id bigint primary key, state text not null);
CREATE FUNCTION public.order_count() RETURNS integer LANGUAGE sql AS $$
  SELECT count(*)::integer FROM public.orders;
$$;
ALTER TABLE public.orders ADD COLUMN created_at timestamptz DEFAULT now();
";
        let once = format(source, FormatOptions::default()).unwrap();
        let twice = format(&once, FormatOptions::default()).unwrap();
        assert_eq!(once, twice);

        let original_kinds: Vec<_> = parse(source)
            .unwrap()
            .statements
            .into_iter()
            .map(|statement| statement.kind)
            .collect();
        let formatted_kinds: Vec<_> = parse(&once)
            .unwrap()
            .statements
            .into_iter()
            .map(|statement| statement.kind)
            .collect();
        assert_eq!(original_kinds, formatted_kinds);
    }

    #[test]
    fn upstream_style_corpus_always_reparses_after_formatting() {
        // Small, representative cases adapted from pgFormatter's PostgreSQL
        // regression inputs. Keeping this as a corpus test complements the
        // exact comment case above and protects the C deparser boundary from
        // crashes on varied, valid PostgreSQL syntax.
        let corpus = [
            "SELECT a, count(*) FROM accounts WHERE active GROUP BY a ORDER BY a;",
            "CREATE INDEX CONCURRENTLY accounts_email_idx ON accounts (lower(email));",
            "ALTER TABLE accounts ADD CONSTRAINT accounts_email_key UNIQUE (email);",
            "WITH changed AS (UPDATE accounts SET active = true RETURNING id) SELECT * FROM changed;",
            "COMMENT ON TABLE accounts IS 'customer accounts';",
        ];
        for source in corpus {
            assert_formatter_invariants(source);
        }
    }

    fn assert_formatter_invariants(source: &str) {
        let once = format(source, FormatOptions::default()).unwrap();
        let twice = format(&once, FormatOptions::default()).unwrap();
        assert_eq!(once, twice, "formatter must be idempotent");
        parse(&once).unwrap();
    }
}
