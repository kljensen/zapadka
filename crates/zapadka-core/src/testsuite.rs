//! Finding and selecting database test files.
//!
//! Test files live under `tests/db` and are discovered recursively. Order is by
//! normalized relative path, so a run is reproducible and a failure is easy to
//! locate — not by filesystem order, which differs between machines.

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{Error, ErrorCode, Result, io_error};
use crate::manifest::sha256_hex;

/// The project directory holding database tests.
pub const TESTS_DIR: &str = "tests/db";

/// One test file on disk.
#[derive(Debug, Clone)]
pub struct TestFile {
    /// Absolute path.
    pub path: Utf8PathBuf,
    /// Project-relative path with forward slashes, used in reports.
    pub relative_path: String,
    /// The file's contents.
    pub sql: String,
    /// SHA-256 of the exact bytes on disk.
    pub sha256: String,
}

/// Finds every test file under `root/tests/db`.
///
/// A project with no tests directory is valid and yields nothing.
pub fn discover(root: &Utf8Path) -> Result<Vec<TestFile>> {
    let dir = root.join(TESTS_DIR);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(&dir).sort_by_file_name() {
        let entry = entry
            .map_err(|error| Error::new(ErrorCode::Io, format!("cannot read {dir}: {error}")))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = Utf8Path::from_path(entry.path())
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::Io,
                    format!("test path {} is not valid UTF-8", entry.path().display()),
                )
            })?
            .to_path_buf();
        if path.extension() != Some("sql") {
            // Fixtures, README files, and editor droppings share the directory.
            continue;
        }
        files.push(read(root, &path)?);
    }

    // Sorted by the path a report will show, so the order a reader sees is the
    // order things ran in.
    files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(files)
}

/// Reads one test file.
fn read(root: &Utf8Path, path: &Utf8Path) -> Result<TestFile> {
    let relative_path = path
        .strip_prefix(root)
        .unwrap_or(path)
        .as_str()
        .replace('\\', "/");
    let bytes = std::fs::read(path).map_err(|e| io_error(&relative_path, "read", e))?;
    let sql = String::from_utf8(bytes.clone())
        .map_err(|_| Error::new(ErrorCode::Io, format!("{relative_path} is not valid UTF-8")))?;
    Ok(TestFile {
        sha256: sha256_hex(&bytes),
        path: path.to_path_buf(),
        relative_path,
        sql,
    })
}

/// Narrows `files` to those matching `selectors`.
///
/// A selector matches a file whose relative path equals it, is inside it when
/// it names a directory, or matches it as a `*`/`**` glob. Selectors form a
/// union, and each one must match something: a selector that matched nothing
/// would otherwise let a CI script quietly test less than it used to.
pub fn select(files: &[TestFile], selectors: &[String]) -> Result<Vec<TestFile>> {
    // Sorting here rather than relying on the caller means selection does not
    // depend on an unstated precondition about how `files` was built.
    let ordered = |mut chosen: Vec<TestFile>| {
        chosen.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        chosen
    };

    if selectors.is_empty() {
        return Ok(ordered(files.to_vec()));
    }

    let mut chosen: Vec<TestFile> = Vec::new();
    for selector in selectors {
        let normalized = selector.trim_start_matches("./").trim_end_matches('/');
        let matched: Vec<&TestFile> = files
            .iter()
            .filter(|file| matches(&file.relative_path, normalized))
            .collect();

        if matched.is_empty() {
            return Err(Error::new(
                ErrorCode::SelectorMatchedNothing,
                format!("no test file matches {selector:?}"),
            )
            .with_hint(format!(
                "tests are discovered under {TESTS_DIR}; this project has {} test file(s)",
                files.len()
            )));
        }
        for file in matched {
            if !chosen.iter().any(|existing| existing.path == file.path) {
                chosen.push(file.clone());
            }
        }
    }

    // Returned in discovery order regardless of the order they were named.
    Ok(ordered(chosen))
}

/// Whether a relative path matches a selector.
fn matches(path: &str, selector: &str) -> bool {
    if path == selector {
        return true;
    }
    // A directory selects everything beneath it.
    if path.starts_with(&format!("{selector}/")) {
        return true;
    }
    // Selectors are often written relative to the tests directory rather than
    // to the project root, because that is where the author is looking.
    let within = path.strip_prefix(&format!("{TESTS_DIR}/")).unwrap_or(path);
    if within == selector || within.starts_with(&format!("{selector}/")) {
        return true;
    }
    glob_matches(path, selector) || glob_matches(within, selector)
}

/// Matches a path against a `*`/`**` glob.
///
/// Delegated to `globset` rather than hand-rolled. The rules look simple until
/// you write them down — whether `*` crosses a separator, whether `**/` matches
/// zero directories, how character classes interact with `/` — and getting one
/// wrong means a CI run quietly testing a different set of files than its
/// author intended.
fn glob_matches(path: &str, pattern: &str) -> bool {
    if !pattern.contains(['*', '?', '[']) {
        return false;
    }
    // `literal_separator` is what makes `*` stop at a `/` while `**` crosses
    // one. Without it `tests/db/*.sql` would also match `tests/db/orders/x.sql`,
    // which is not what anyone means when they type it.
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .is_ok_and(|glob| glob.compile_matcher().is_match(path))
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;

    fn file(relative_path: &str) -> TestFile {
        TestFile {
            path: Utf8PathBuf::from(relative_path),
            relative_path: relative_path.to_owned(),
            sql: "SELECT plan(0); SELECT finish();".to_owned(),
            sha256: "0".repeat(64),
        }
    }

    fn files() -> Vec<TestFile> {
        vec![
            file("tests/db/orders/totals.sql"),
            file("tests/db/orders/status.sql"),
            file("tests/db/schema.sql"),
        ]
    }

    fn selected(selectors: &[&str]) -> Vec<String> {
        let owned: Vec<String> = selectors.iter().map(|s| (*s).to_owned()).collect();
        select(&files(), &owned)
            .unwrap()
            .into_iter()
            .map(|file| file.relative_path)
            .collect()
    }

    #[test]
    fn no_selectors_means_every_file_in_path_order() {
        assert_eq!(
            selected(&[]),
            [
                "tests/db/orders/status.sql",
                "tests/db/orders/totals.sql",
                "tests/db/schema.sql"
            ]
        );
    }

    #[test]
    fn a_file_can_be_named_exactly() {
        assert_eq!(selected(&["tests/db/schema.sql"]), ["tests/db/schema.sql"]);
    }

    #[test]
    fn a_directory_selects_everything_beneath_it() {
        assert_eq!(
            selected(&["tests/db/orders"]),
            ["tests/db/orders/status.sql", "tests/db/orders/totals.sql"]
        );
    }

    #[test]
    fn selectors_may_be_written_relative_to_the_tests_directory() {
        // Which is where the author is looking when they type it.
        assert_eq!(selected(&["schema.sql"]), ["tests/db/schema.sql"]);
        assert_eq!(
            selected(&["orders"]),
            ["tests/db/orders/status.sql", "tests/db/orders/totals.sql"]
        );
    }

    #[test]
    fn globs_are_supported_and_star_does_not_cross_a_separator() {
        assert_eq!(selected(&["tests/db/*.sql"]), ["tests/db/schema.sql"]);
        assert_eq!(
            selected(&["tests/db/**/*.sql"]),
            [
                "tests/db/orders/status.sql",
                "tests/db/orders/totals.sql",
                "tests/db/schema.sql"
            ]
        );
        assert_eq!(
            selected(&["orders/*.sql"]),
            ["tests/db/orders/status.sql", "tests/db/orders/totals.sql"]
        );
    }

    #[test]
    fn selectors_form_a_union_without_duplicates() {
        assert_eq!(
            selected(&["tests/db/orders", "tests/db/orders/totals.sql"]),
            ["tests/db/orders/status.sql", "tests/db/orders/totals.sql"]
        );
    }

    #[test]
    fn results_come_back_in_discovery_order_not_selector_order() {
        assert_eq!(
            selected(&["tests/db/schema.sql", "tests/db/orders/totals.sql"]),
            ["tests/db/orders/totals.sql", "tests/db/schema.sql"]
        );
    }

    #[test]
    fn a_selector_matching_nothing_is_an_error() {
        // Otherwise a CI script whose selector stopped matching would quietly
        // test less than it used to and still pass.
        let error = select(&files(), &["tests/db/nope.sql".to_owned()]).unwrap_err();
        assert_eq!(error.code, ErrorCode::SelectorMatchedNothing);

        // Even when other selectors did match.
        let error = select(
            &files(),
            &["tests/db/schema.sql".to_owned(), "nope".to_owned()],
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::SelectorMatchedNothing);
    }

    #[test]
    fn glob_matching_follows_the_documented_rules() {
        assert!(glob_matches("a/b.sql", "a/*.sql"));
        assert!(!glob_matches("a/b/c.sql", "a/*.sql"));
        assert!(glob_matches("a/b/c.sql", "a/**/c.sql"));
        // `**/` also matches zero directories.
        assert!(glob_matches("a/c.sql", "a/**/c.sql"));
        assert!(glob_matches("anything", "*"));
        assert!(
            !glob_matches("plain", "plain"),
            "a pattern with no star is not a glob"
        );
    }
}
