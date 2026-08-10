//! Raw bindings to the vendored `libpg_query` C library.
//!
//! Only [`parse_to_json`] is exposed to the rest of the crate; every raw
//! pointer is freed before it returns.

use std::ffi::{CStr, CString, c_char, c_int};

use crate::ParseError;

#[repr(C)]
struct PgQueryError {
    message: *mut c_char,
    funcname: *mut c_char,
    filename: *mut c_char,
    lineno: c_int,
    /// 1-based character position within the input, or 0 when unknown.
    cursorpos: c_int,
    context: *mut c_char,
}

#[repr(C)]
struct PgQueryParseResult {
    parse_tree: *mut c_char,
    stderr_buffer: *mut c_char,
    error: *mut PgQueryError,
}

unsafe extern "C" {
    fn pg_query_parse(input: *const c_char) -> PgQueryParseResult;
    fn pg_query_free_parse_result(result: PgQueryParseResult);
}

/// Parses `sql` and returns the parse tree as JSON.
///
/// An interior NUL byte cannot reach the C parser, so it is reported as a
/// syntax error at its own offset rather than silently truncating the script.
pub(crate) fn parse_to_json(sql: &str) -> Result<String, ParseError> {
    let input = CString::new(sql).map_err(|e| {
        let offset = e.nul_position();
        let (line, column) = line_and_column(sql, offset);
        ParseError {
            message: "script contains a NUL byte".to_owned(),
            line,
            column,
            offset: Some(offset),
        }
    })?;

    // SAFETY: `input` is a valid NUL-terminated C string that outlives the call.
    // The returned result owns its buffers and is freed on every path below.
    let result = unsafe { pg_query_parse(input.as_ptr()) };

    let outcome = if result.error.is_null() {
        // SAFETY: upstream guarantees `parse_tree` is a valid C string whenever
        // `error` is null.
        Ok(unsafe { CStr::from_ptr(result.parse_tree) }
            .to_string_lossy()
            .into_owned())
    } else {
        // SAFETY: `error` is non-null, so it points at a fully initialized
        // `PgQueryError` owned by the result.
        let error = unsafe { &*result.error };
        let message = if error.message.is_null() {
            "syntax error".to_owned()
        } else {
            // SAFETY: non-null message pointers are valid C strings.
            unsafe { CStr::from_ptr(error.message) }
                .to_string_lossy()
                .into_owned()
        };
        // `cursorpos` is 1-based and 0 means "no position"; `lineno` refers to
        // the parser's own C source and is deliberately ignored.
        let offset = (error.cursorpos > 0).then(|| error.cursorpos as usize - 1);
        let (line, column) = match offset {
            Some(offset) => line_and_column(sql, offset),
            None => (1, 1),
        };
        Err(ParseError {
            message,
            line,
            column,
            offset,
        })
    };

    // SAFETY: `result` was returned by `pg_query_parse` and has not been freed.
    // Every borrow above has been copied into owned Rust values.
    unsafe { pg_query_free_parse_result(result) };

    outcome
}

/// Converts a byte offset into 1-based line and character column.
///
/// Columns count characters rather than bytes so that multi-byte identifiers
/// and string literals produce a caret position a human can match to their
/// editor.
fn line_and_column(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let line = before.bytes().filter(|&b| b == b'\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let column = text[line_start..offset].chars().count() + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_and_column_are_one_based() {
        assert_eq!(line_and_column("abc", 0), (1, 1));
        assert_eq!(line_and_column("abc", 2), (1, 3));
        assert_eq!(line_and_column("ab\ncd", 3), (2, 1));
        assert_eq!(line_and_column("ab\ncd", 4), (2, 2));
    }

    #[test]
    fn columns_count_characters_not_bytes() {
        // "héllo" — the offset is past a two-byte character.
        assert_eq!(line_and_column("héllo", 3), (1, 3));
    }

    #[test]
    fn nul_bytes_are_reported_rather_than_truncating_the_script() {
        let error = parse_to_json("SELECT 1;\0DROP TABLE t;").unwrap_err();
        assert_eq!(error.offset, Some(9));
        assert_eq!(error.line, 1);
    }
}
