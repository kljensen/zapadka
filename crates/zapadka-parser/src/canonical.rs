//! Stable full-tree SQL representation for structural-v1 (ADR-0006).

use serde_json::Value;

use crate::{ParseError, tree};

/// Canonicalizes the complete PostgreSQL parse tree, excluding source positions.
///
/// Object keys are lexicographically sorted, arrays retain their order, and
/// literal contents (including procedural bodies) remain significant. This is
/// structural equivalence under the pinned parser, not semantic equivalence.
pub fn canonicalize(sql: &str) -> Result<String, ParseError> {
    let mut statements = Value::Array(tree::parse(sql)?.statements);
    strip_positions(&mut statements);
    // Explicitly sort even if another dependency enables serde_json's
    // preserve_order feature. Parser build version is outside the payload.
    statements.sort_all_objects();
    serde_json::to_string(&statements).map_err(tree::error)
}

fn strip_positions(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.retain(|key, value| !is_position(key, value));
            for child in object.values_mut() {
                strip_positions(child);
            }
        }
        Value::Array(array) => {
            for child in array {
                strip_positions(child);
            }
        }
        _ => {}
    }
}

fn is_position(key: &str, value: &Value) -> bool {
    // Exhaustive ParseLoc names in the vendored parsenodes.h/primnodes.h and
    // generated pg_query_outfuncs_defs.c. In particular CreateTableSpaceStmt's
    // STRING location is database data and must survive.
    value.is_i64()
        && matches!(
            key,
            "location"
                | "name_location"
                | "stmt_location"
                | "stmt_len"
                | "list_start"
                | "list_end"
                | "rexpr_list_start"
                | "rexpr_list_end"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn representation_is_fixed() {
        assert_eq!(
            canonicalize("SELECT 1").unwrap(),
            r#"[{"stmt":{"SelectStmt":{"limitOption":"LIMIT_OPTION_DEFAULT","op":"SETOP_NONE","targetList":[{"ResTarget":{"val":{"A_Const":{"ival":{"ival":1}}}}}]}}}]"#
        );
        assert_eq!(
            canonicalize(" -- nothing\n /* nested /* yes */ */ ").unwrap(),
            "[]"
        );
    }

    #[test]
    fn cosmetics_preserve_structure() {
        for (a, b) in [
            (
                "SELECT 1; SELECT 'é'",
                "\r\n/* outer /* inner */ */select 1 ; -- hi\r\nSELECT 'é';",
            ),
            ("SELECT 'ab'", "SELECT 'a'\n'b'"),
            ("SELECT E'a\\nb'", "SELECT 'a\nb'"),
            ("SELECT $$hello$$", "SELECT $tag$hello$tag$"),
            (
                "SELECT 1 IN (1,2), ARRAY[1,2]",
                "SELECT 1 IN ( 1 , 2 ), ARRAY[ 1 , 2 ]",
            ),
            (
                "SELECT json_object('a': 1)",
                "SELECT json_object( 'a' : 1 )",
            ),
        ] {
            assert_eq!(
                canonicalize(a).unwrap(),
                canonicalize(b).unwrap(),
                "{a} / {b}"
            );
        }
    }

    #[test]
    fn substantive_changes_survive() {
        for (a, b) in [
            ("SELECT 1", "SELECT 2"),
            ("SELECT 1+2", "SELECT 1-2"),
            ("SELECT 1,2", "SELECT 2,1"),
            ("SELECT 1; SELECT 2", "SELECT 2; SELECT 1"),
            ("SELECT \"Foo\"", "SELECT \"foo\""),
            (
                "CREATE TABLE t(a int, b text)",
                "CREATE TABLE t(b text, a int)",
            ),
            ("CREATE TABLE t(a int)", "CREATE TABLE t(a int NOT NULL)"),
            ("DO $$BEGIN NULL; END$$", "DO $$BEGIN  NULL; END$$"),
            (
                "DO $$BEGIN NULL; END$$",
                "DO $$BEGIN /*comment*/ NULL; END$$",
            ),
            ("COMMENT ON TABLE t IS 'old'", "COMMENT ON TABLE t IS 'new'"),
            (
                "CREATE TABLESPACE t LOCATION '/a'",
                "CREATE TABLESPACE t LOCATION '/b'",
            ),
            ("SELECT 1 LIMIT 2 OFFSET 3", "SELECT 1 LIMIT 2 OFFSET 4"),
        ] {
            assert_ne!(
                canonicalize(a).unwrap(),
                canonicalize(b).unwrap(),
                "{a} / {b}"
            );
        }
        assert!(canonicalize("SELECT (").is_err());
        assert!(canonicalize("SELECT 1\0; SELECT 2").is_err());
    }

    #[test]
    fn formatter_corpus_preserves_structure() {
        for sql in [
            include_str!("../tests/fixtures/pgformatter-comments.sql"),
            include_str!("../tests/fixtures/pgformatter-create-type.sql"),
        ] {
            let formatted = crate::format(sql, crate::FormatOptions::default()).unwrap();
            assert_eq!(
                canonicalize(sql).unwrap(),
                canonicalize(&formatted).unwrap()
            );
        }
    }
}
