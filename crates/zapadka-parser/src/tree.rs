//! One fail-closed decoder for classification and structural hashing.

use serde_json::Value;

use crate::{ParseError, ffi};

pub(crate) struct Tree {
    pub version: u32,
    pub statements: Vec<Value>,
}

pub(crate) fn parse(sql: &str) -> Result<Tree, ParseError> {
    decode(&ffi::parse_to_json(sql)?)
}

fn decode(json: &str) -> Result<Tree, ParseError> {
    let mut root: Value = serde_json::from_str(json).map_err(error)?;
    let version = root
        .get("version")
        .and_then(Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .filter(|version| *version != 0)
        .ok_or_else(|| error("missing or invalid parser version"))?;
    let statements = root
        .get_mut("stmts")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| error("missing or invalid statement array"))?;
    for raw in statements.iter() {
        let node = raw
            .get("stmt")
            .and_then(Value::as_object)
            .ok_or_else(|| error("missing or invalid statement node"))?;
        if node.len() != 1 || !node.values().all(Value::is_object) {
            return Err(error("invalid statement node shape"));
        }
    }
    Ok(Tree {
        version,
        statements: std::mem::take(statements),
    })
}

pub(crate) fn error(error: impl std::fmt::Display) -> ParseError {
    ParseError {
        message: format!("cannot decode PostgreSQL parse tree: {error}"),
        line: 1,
        column: 1,
        offset: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_trees_never_become_empty_scripts() {
        for json in [
            "null",
            "{}",
            "{",
            r#"{"version":180004,"stmts":null}"#,
            r#"{"version":180004,"stmts":[{}]}"#,
            r#"{"version":180004,"stmts":[{"stmt":{}}]}"#,
            r#"{"version":180004,"stmts":[{"stmt":{"SelectStmt":null}}]}"#,
        ] {
            assert!(decode(json).is_err(), "{json}");
        }
    }

    #[test]
    fn canonical_and_safety_classification_reject_oversized_trees_together() {
        let sql = format!("SELECT {}; COMMIT", vec!["1"; 100].join(" + "));
        // PostgreSQL accepts the SQL, but its JSON exceeds the Rust decoder's
        // recursion limit. It must never masquerade as zero safe statements.
        assert!(ffi::parse_to_json(&sql).is_ok());
        assert!(crate::parse(&sql).is_err());
        assert!(crate::canonicalize(&sql).is_err());
    }
}
