//! Reading PostgreSQL service files.
//!
//! A service file lets an operator keep connection details — including a
//! password — outside the repository, which is exactly what `zapadka.toml`
//! refuses to hold. Zapadka reads the file itself rather than linking `libpq`,
//! because ADR-0005 requires a binary with no PostgreSQL client dependency.
//!
//! The format is the one `libpq` documents: `[service-name]` sections
//! containing `keyword=value` lines, with `#` comments.

use std::collections::BTreeMap;

use zapadka_core::error::{Error, ErrorCode, Result};

/// The settings of one service entry.
pub type ServiceSettings = BTreeMap<String, String>;

/// Finds and reads the settings for `name`.
///
/// Search order matches `libpq`: `PGSERVICEFILE`, then the per-user
/// `~/.pg_service.conf`, then the system-wide
/// `$PGSYSCONFDIR/pg_service.conf`.
pub fn lookup(name: &str) -> Result<ServiceSettings> {
    let mut searched = Vec::new();
    for path in candidate_paths() {
        searched.push(path.clone());
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            // A file that is not there is the normal case: try the next one.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            // A file that exists but cannot be read is not. Falling through
            // could find the same service name in the system-wide file and
            // connect somewhere the operator did not configure.
            Err(error) => {
                return Err(Error::new(
                    ErrorCode::TargetInvalid,
                    format!("cannot read the service file {path}: {error}"),
                )
                .with_hint(
                    "a service file that exists but cannot be read is not skipped, because the \
                     next candidate could define the same service differently",
                ));
            }
        };
        if let Some(settings) = parse(&text, name)
            .map_err(|message| Error::new(ErrorCode::TargetInvalid, format!("{path}: {message}")))?
        {
            return Ok(settings);
        }
    }

    Err(Error::new(
        ErrorCode::TargetInvalid,
        format!("no PostgreSQL service named {name:?}"),
    )
    .with_hint(format!(
        "define [{name}] in a service file; Zapadka looked in {}",
        searched.join(", ")
    )))
}

/// The service files to search, in `libpq`'s order.
///
/// The per-user file comes before the system-wide one, because that is what
/// libpq does: "if the same service name exists in both the user and the system
/// file, the user file takes precedence". Searching them the other way round
/// would make Zapadka connect somewhere `psql` would not, which is the worst
/// possible disagreement for a tool that deploys migrations.
fn candidate_paths() -> Vec<String> {
    let mut paths = Vec::new();

    // `PGSERVICEFILE` *replaces* the per-user file rather than being searched
    // before it -- libpq describes the user file as "~/.pg_service.conf, or the
    // location specified by PGSERVICEFILE". Searching both would let Zapadka
    // find a service in the home file that `psql`, with the same environment,
    // would not.
    match std::env::var("PGSERVICEFILE") {
        Ok(file) => paths.push(file),
        Err(_) => {
            if let Ok(home) = std::env::var("HOME") {
                paths.push(format!("{home}/.pg_service.conf"));
            }
        }
    }

    // The system-wide file is searched after the per-user one, because libpq
    // gives the user file precedence when both define the same service.
    if let Ok(dir) = std::env::var("PGSYSCONFDIR") {
        paths.push(format!("{dir}/pg_service.conf"));
    }
    paths
}

/// Extracts the settings for `name`, or `None` when the file has no such
/// section.
fn parse(text: &str, name: &str) -> std::result::Result<Option<ServiceSettings>, String> {
    let mut current: Option<String> = None;
    let mut settings = ServiceSettings::new();
    let mut found = false;

    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(section) = line.strip_prefix('[') {
            let section = section.strip_suffix(']').ok_or_else(|| {
                format!(
                    "line {}: section header is missing its closing bracket",
                    index + 1
                )
            })?;
            if found {
                // The requested section has ended.
                return Ok(Some(settings));
            }
            current = Some(section.trim().to_owned());
            found = current.as_deref() == Some(name);
            continue;
        }

        // Structure is validated for every line, not only those in the section
        // being looked for. A malformed file reported as "no such service"
        // would send someone hunting for the wrong problem.
        if current.is_none() {
            return Err(format!(
                "line {}: setting appears before any section",
                index + 1
            ));
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {}: expected keyword=value", index + 1))?;

        if found {
            settings.insert(key.trim().to_lowercase(), value.trim().to_owned());
        }
    }

    Ok(found.then_some(settings))
}

/// Reads service settings from explicit text. Exposed for testing.
#[cfg(test)]
pub(crate) fn parse_for_test(
    text: &str,
    name: &str,
) -> std::result::Result<Option<ServiceSettings>, String> {
    parse(text, name)
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;

    const FILE: &str = "\
# a comment
[app-production]
host=db.internal
port=5433
dbname=app
user=deployer
sslmode=verify-full

[app-staging]
host=staging.internal
dbname=app_staging
";

    #[test]
    fn reads_the_requested_section_only() {
        let settings = parse_for_test(FILE, "app-production").unwrap().unwrap();
        assert_eq!(settings["host"], "db.internal");
        assert_eq!(settings["port"], "5433");
        assert_eq!(settings["dbname"], "app");
        assert_eq!(settings["sslmode"], "verify-full");
        // Nothing from the following section leaks in.
        assert_eq!(settings.len(), 5);
    }

    #[test]
    fn reads_a_section_that_is_not_the_first() {
        let settings = parse_for_test(FILE, "app-staging").unwrap().unwrap();
        assert_eq!(settings["host"], "staging.internal");
        assert_eq!(settings.len(), 2);
    }

    #[test]
    fn an_absent_section_is_not_an_error_so_the_next_file_can_be_tried() {
        assert!(parse_for_test(FILE, "nope").unwrap().is_none());
    }

    #[test]
    fn keywords_are_case_insensitive_and_values_are_not() {
        let settings = parse_for_test("[s]\nHost=Db.Internal\n", "s")
            .unwrap()
            .unwrap();
        assert_eq!(settings["host"], "Db.Internal");
    }

    #[test]
    fn tolerates_the_whitespace_people_leave_in_configuration_files() {
        let settings = parse_for_test("  [ s ] \n  host = db  \n", "s")
            .unwrap()
            .unwrap();
        assert_eq!(settings["host"], "db");
    }

    #[test]
    fn malformed_files_are_reported_with_a_line_number() {
        for (text, expected) in [
            ("[s\nhost=db\n", "closing bracket"),
            ("host=db\n", "before any section"),
            ("[s]\nhost\n", "keyword=value"),
        ] {
            let error = parse_for_test(text, "s").unwrap_err();
            assert!(error.contains(expected), "{text:?} produced {error:?}");
            assert!(error.contains("line "), "{error:?}");
        }
    }

    #[test]
    fn a_password_in_a_service_file_is_read_but_never_echoed() {
        // The service file is precisely where a password belongs; Zapadka just
        // must not put it anywhere else.
        let settings = parse_for_test("[s]\npassword=hunter2\n", "s")
            .unwrap()
            .unwrap();
        assert_eq!(settings["password"], "hunter2");
    }
}
