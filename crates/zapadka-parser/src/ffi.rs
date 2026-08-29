//! Raw bindings to the vendored `libpg_query` C library.
//!
//! Only [`parse_to_json`] is exposed to the rest of the crate; every raw
//! pointer is freed before it returns.
//!
//! This is the only module in Zapadka permitted to use `unsafe`. The workspace
//! denies it everywhere else, so this opt-in is the complete list of places
//! where memory safety rests on review rather than on the compiler.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_int};

use crate::{FormatOptions, ParseError};

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

#[repr(C)]
#[derive(Clone, Copy)]
struct PgQueryProtobuf {
    len: usize,
    data: *mut c_char,
}

#[repr(C)]
struct PgQueryProtobufParseResult {
    parse_tree: PgQueryProtobuf,
    stderr_buffer: *mut c_char,
    error: *mut PgQueryError,
}

#[repr(C)]
struct PgQueryDeparseResult {
    query: *mut c_char,
    error: *mut PgQueryError,
}

#[repr(C)]
struct PostgresDeparseComment {
    match_location: c_int,
    newlines_before_comment: c_int,
    newlines_after_comment: c_int,
    str_: *mut c_char,
}

#[repr(C)]
struct PgQueryDeparseCommentsResult {
    comments: *mut *mut PostgresDeparseComment,
    comment_count: usize,
    error: *mut PgQueryError,
}

#[repr(C)]
struct PostgresDeparseOpts {
    comments: *mut *mut PostgresDeparseComment,
    comment_count: usize,
    pretty_print: bool,
    indent_size: c_int,
    max_line_length: c_int,
    trailing_newline: bool,
    commas_start_of_line: bool,
}

unsafe extern "C" {
    fn pg_query_parse(input: *const c_char) -> PgQueryParseResult;
    fn pg_query_free_parse_result(result: PgQueryParseResult);
    fn pg_query_parse_protobuf(input: *const c_char) -> PgQueryProtobufParseResult;
    fn pg_query_deparse_protobuf_opts(
        parse_tree: PgQueryProtobuf,
        options: PostgresDeparseOpts,
    ) -> PgQueryDeparseResult;
    fn pg_query_deparse_comments_for_query(input: *const c_char) -> PgQueryDeparseCommentsResult;
    fn pg_query_free_protobuf_parse_result(result: PgQueryProtobufParseResult);
    fn pg_query_free_deparse_result(result: PgQueryDeparseResult);
    fn pg_query_free_deparse_comments_result(result: PgQueryDeparseCommentsResult);
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
        // `cursorpos` is a 1-based *character* position, as every PostgreSQL
        // error position is; 0 means "no position". Treating it as a byte
        // offset reports the wrong column as soon as the script contains a
        // multibyte character, and slices mid-codepoint when the arithmetic
        // lands inside one.
        //
        // `lineno` refers to the parser's own C source and is ignored.
        let offset = usize::try_from(error.cursorpos)
            .ok()
            .and_then(|position| position.checked_sub(1))
            .map(|characters| byte_offset_of_character(sql, characters));
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

/// Formats `sql` through libpg_query's protobuf deparser.
///
/// The protobuf parse tree and comment mapping are deliberately kept in the C
/// boundary: neither is part of Zapadka's public parser vocabulary, and each
/// must be freed by the matching upstream function.
pub(crate) fn format(sql: &str, options: FormatOptions) -> Result<String, ParseError> {
    let input = c_input(sql)?;

    // SAFETY: `input` is valid for all C calls below. Every returned allocation
    // is copied into Rust before its matching upstream free function is called.
    let parsed = unsafe { pg_query_parse_protobuf(input.as_ptr()) };
    if !parsed.error.is_null() {
        let error = unsafe { parse_error(&*parsed.error, sql) };
        // SAFETY: `parsed` came from pg_query_parse_protobuf and is not reused.
        unsafe { pg_query_free_protobuf_parse_result(parsed) };
        return Err(error);
    }

    // Comments are not present in PostgreSQL parse trees. Ask libpg_query for
    // its source-to-node mapping and pass it straight back to the deparser.
    let comments = unsafe { pg_query_deparse_comments_for_query(input.as_ptr()) };
    if !comments.error.is_null() {
        let error = unsafe { parse_error(&*comments.error, sql) };
        // SAFETY: both values came from their respective libpg_query calls.
        unsafe {
            pg_query_free_deparse_comments_result(comments);
            pg_query_free_protobuf_parse_result(parsed);
        }
        return Err(error);
    }

    let deparsed = unsafe {
        pg_query_deparse_protobuf_opts(
            parsed.parse_tree,
            PostgresDeparseOpts {
                comments: comments.comments,
                comment_count: comments.comment_count,
                pretty_print: options.pretty_print,
                indent_size: c_int::from(options.indent_size),
                max_line_length: c_int::from(options.max_line_length),
                trailing_newline: options.trailing_newline,
                commas_start_of_line: options.commas_start_of_line,
            },
        )
    };

    let outcome = if deparsed.error.is_null() {
        // SAFETY: successful deparse results contain a valid NUL-terminated query.
        Ok(unsafe { CStr::from_ptr(deparsed.query) }
            .to_string_lossy()
            .into_owned())
    } else {
        // SAFETY: a non-null error is initialized and owned by `deparsed`.
        Err(unsafe { parse_error(&*deparsed.error, sql) })
    };

    // SAFETY: each value is freed exactly once after all Rust copies are made.
    unsafe {
        pg_query_free_deparse_result(deparsed);
        pg_query_free_deparse_comments_result(comments);
        pg_query_free_protobuf_parse_result(parsed);
    }
    outcome
}

fn c_input(sql: &str) -> Result<CString, ParseError> {
    CString::new(sql).map_err(|error| {
        let offset = error.nul_position();
        let (line, column) = line_and_column(sql, offset);
        ParseError {
            message: "script contains a NUL byte".to_owned(),
            line,
            column,
            offset: Some(offset),
        }
    })
}

/// Copies an upstream error before its owning result is freed.
unsafe fn parse_error(error: &PgQueryError, sql: &str) -> ParseError {
    let message = if error.message.is_null() {
        "syntax error".to_owned()
    } else {
        // SAFETY: non-null error messages are valid C strings by libpg_query's API.
        unsafe { CStr::from_ptr(error.message) }
            .to_string_lossy()
            .into_owned()
    };
    let offset = usize::try_from(error.cursorpos)
        .ok()
        .and_then(|position| position.checked_sub(1))
        .map(|characters| byte_offset_of_character(sql, characters));
    let (line, column) = match offset {
        Some(offset) => line_and_column(sql, offset),
        None => (1, 1),
    };
    ParseError {
        message,
        line,
        column,
        offset,
    }
}

/// Converts a 0-based character index into a byte offset.
///
/// An index past the end clamps to the end of the string, so a position the
/// parser reports for a truncated script still produces a usable location.
fn byte_offset_of_character(text: &str, characters: usize) -> usize {
    text.char_indices()
        .nth(characters)
        .map_or(text.len(), |(offset, _)| offset)
}

/// Converts a byte offset into 1-based line and character column.
///
/// Columns count characters rather than bytes so that multi-byte identifiers
/// and string literals produce a caret position a human can match to their
/// editor.
fn line_and_column(text: &str, offset: usize) -> (usize, usize) {
    // Clamped to a character boundary as well as to the length. Slicing a &str
    // mid-codepoint panics, and a diagnostic must never be able to take down a
    // migration run.
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let before = &text[..offset];
    let line = before.bytes().filter(|&b| b == b'\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let column = text[line_start..offset].chars().count() + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

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
    fn multibyte_characters_before_an_error_do_not_move_the_column() {
        // PostgreSQL reports error positions in characters. Reading one as a
        // byte offset reports the wrong column, and lands mid-codepoint often
        // enough to crash: `SELECT 'ééééé' , SELECT FROM;` used to panic.
        let sql = "SELECT 'ééééé' , SELECT FROM;";
        let error = crate::parse(sql).unwrap_err();
        assert_eq!(error.line, 1);
        // The offending token is the second `SELECT`, which starts at
        // character 18 -- not byte 22, which is where it would land if the
        // position were read as bytes.
        assert_eq!(error.column, 18, "{error:?}");
        assert_eq!(error.offset, Some(22), "the byte offset is 22");
    }

    #[test]
    fn a_position_inside_a_multibyte_character_never_panics() {
        // Defence in depth: whatever the parser reports, slicing must be safe.
        let text = "SELECT 'é'";
        for offset in 0..=text.len() + 4 {
            let (line, column) = line_and_column(text, offset);
            assert!(line >= 1 && column >= 1);
        }
    }

    #[test]
    fn character_indices_map_to_byte_offsets() {
        assert_eq!(byte_offset_of_character("abc", 0), 0);
        assert_eq!(byte_offset_of_character("abc", 2), 2);
        assert_eq!(byte_offset_of_character("éb", 1), 2, "é is two bytes");
        // Past the end clamps rather than panicking.
        assert_eq!(byte_offset_of_character("abc", 99), 3);
    }

    #[test]
    fn nul_bytes_are_reported_rather_than_truncating_the_script() {
        let error = parse_to_json("SELECT 1;\0DROP TABLE t;").unwrap_err();
        assert_eq!(error.offset, Some(9));
        assert_eq!(error.line, 1);
    }
}
