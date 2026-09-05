//! `zapadka format` — canonicalize SQL without connecting to a database.
//!
//! A migration's `deploy.sql` is part of its immutable deployed definition.
//! Rewrites must preserve the complete SQL structure. Legacy targets require
//! explicit rehashing before formatting; this command never connects to them.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use camino::{Utf8Path, Utf8PathBuf};
use zapadka_core::config::LoadedConfig;
use zapadka_core::error::{Error, ErrorCode, Result, io_error};
use zapadka_core::graph::Graph;
use zapadka_core::report::{Diagnostic, Location, Severity};

use crate::cli::FormatArgs;
use crate::session::Session;

/// Runs `zapadka format --check` or `zapadka format --write`.
pub fn run(
    config: &LoadedConfig,
    graph: &Graph,
    args: &FormatArgs,
    session: &mut Session,
) -> Result<()> {
    let paths = select_paths(config, graph, args)?;
    // Format every input before touching the filesystem. A syntax error in a
    // later file therefore cannot leave earlier files rewritten.
    let formatted: Vec<_> = paths.iter().map(format_file).collect::<Result<_>>()?;
    finish_format(&formatted, args, session)
}

fn finish_format(
    formatted: &[FormattedFile],
    args: &FormatArgs,
    session: &mut Session,
) -> Result<()> {
    let changed: Vec<_> = formatted
        .iter()
        .filter(|file| file.original != file.formatted)
        .collect();

    // Complete preflight before any writes, including when the deprecated
    // --allow-deploy-rewrite compatibility flag is supplied.
    for file in &changed {
        ensure_equivalent(file)?;
    }
    if changed
        .iter()
        .any(|file| is_deploy_script(&file.path.absolute))
    {
        session.diagnose(Diagnostic {
            severity: Severity::Note,
            code: "format.legacy_targets".to_owned(),
            message: "deploy.sql formatting preserves structural-v1; targets using raw-v1 must be rehashed before accepting these byte changes".to_owned(),
            migration_id: None,
            location: None,
            hint: Some("run zapadka rehash --dry-run and zapadka rehash for each target before formatting; format does not connect to databases".to_owned()),
        });
    }

    if args.check {
        for file in &changed {
            session.diagnose(needs_formatting_diagnostic(&file.path.relative));
        }
        if let Some(first) = changed.first() {
            return Err(Error::new(
                ErrorCode::FormatCheckFailed,
                format!(
                    "{} SQL {} need formatting",
                    changed.len(),
                    plural(changed.len(), "file")
                ),
            )
            .at(Location::file(&first.path.relative))
            .with_context(
                "files",
                changed
                    .iter()
                    .map(|file| file.path.relative.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            )
            .with_hint("run zapadka format --write on the listed files"));
        }
        return Ok(());
    }

    for file in changed {
        atomic_write(&file.path.absolute, &file.formatted)?;
        session.diagnose(Diagnostic {
            severity: Severity::Note,
            code: "format.written".to_owned(),
            message: "formatted".to_owned(),
            migration_id: None,
            location: Some(Location::file(&file.path.relative)),
            hint: None,
        });
    }
    Ok(())
}

fn ensure_equivalent(file: &FormattedFile) -> Result<()> {
    let canonicalize = |sql: &str| {
        zapadka_parser::canonicalize(sql).map_err(|error| {
            Error::new(ErrorCode::ScriptParseError, error.to_string())
                .at(Location::file(&file.path.relative))
        })
    };
    if canonicalize(&file.original)? != canonicalize(&file.formatted)? {
        return Err(Error::new(
            ErrorCode::FormatDeployRewriteDenied,
            "formatter output changes SQL structure; no files were rewritten",
        ).at(Location::file(&file.path.relative))
         .with_hint("report this formatter incompatibility; literal and procedural-body changes are substantive"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SelectedPath {
    absolute: Utf8PathBuf,
    relative: String,
}

#[derive(Debug)]
struct FormattedFile {
    path: SelectedPath,
    original: String,
    formatted: String,
}

fn select_paths(
    config: &LoadedConfig,
    graph: &Graph,
    args: &FormatArgs,
) -> Result<Vec<SelectedPath>> {
    let paths = if args.paths.is_empty() {
        default_paths(config, graph)?
    } else {
        args.paths
            .iter()
            .map(|path| explicit_path(config, path))
            .collect::<Result<Vec<_>>>()?
    };
    let mut paths = paths;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn explicit_path(config: &LoadedConfig, supplied: &Utf8Path) -> Result<SelectedPath> {
    let absolute = if supplied.is_absolute() {
        supplied.to_path_buf()
    } else {
        config.root.join(supplied)
    };
    if !absolute.is_file() {
        return Err(Error::new(
            ErrorCode::Io,
            format!("no such SQL file: {supplied}"),
        ));
    }
    selected(config, absolute)
}

fn default_paths(config: &LoadedConfig, graph: &Graph) -> Result<Vec<SelectedPath>> {
    let mut paths = Vec::new();
    for migration in graph.migrations() {
        paths.push(selected(config, migration.deploy.path.clone())?);
        if let Some(script) = &migration.revert {
            paths.push(selected(config, script.path.clone())?);
        }
        if let Some(script) = &migration.verify {
            paths.push(selected(config, script.path.clone())?);
        }
    }
    collect_sql_files(config, &config.tests_dir(), &mut paths)?;
    Ok(paths)
}

fn collect_sql_files(
    config: &LoadedConfig,
    directory: &Utf8Path,
    paths: &mut Vec<SelectedPath>,
) -> Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(directory).map_err(|error| io_error(directory, "read", error))? {
        let entry = entry.map_err(|error| io_error(directory, "read", error))?;
        let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
            Error::new(
                ErrorCode::Io,
                format!("test path {} is not valid UTF-8", path.display()),
            )
        })?;
        if path.is_dir() {
            collect_sql_files(config, &path, paths)?;
        } else if path.extension() == Some("sql") {
            paths.push(selected(config, path)?);
        }
    }
    Ok(())
}

fn selected(config: &LoadedConfig, absolute: Utf8PathBuf) -> Result<SelectedPath> {
    if absolute.extension() != Some("sql") {
        return Err(Error::new(
            ErrorCode::ConfigInvalid,
            format!("format accepts SQL files only: {absolute}"),
        ));
    }
    let relative = absolute
        .strip_prefix(&config.root)
        .map_or_else(|_| absolute.to_string(), ToString::to_string);
    Ok(SelectedPath { absolute, relative })
}

fn format_file(path: &SelectedPath) -> Result<FormattedFile> {
    let original = fs::read_to_string(&path.absolute)
        .map_err(|error| io_error(&path.relative, "read", error))?;
    let formatted = zapadka_parser::format(&original, zapadka_parser::FormatOptions::default())
        .map_err(|error| {
            Error::new(
                ErrorCode::ScriptParseError,
                format!("cannot format {}: {}", path.relative, error.message),
            )
            .at(Location::at(&path.relative, error.line, error.column))
            .with_context("line", error.line)
            .with_context("column", error.column)
        })?;
    Ok(FormattedFile {
        path: path.clone(),
        original,
        formatted,
    })
}

fn needs_formatting_diagnostic(path: &str) -> Diagnostic {
    Diagnostic {
        severity: Severity::Note,
        code: "format.required".to_owned(),
        message: "needs formatting".to_owned(),
        migration_id: None,
        location: Some(Location::file(path)),
        hint: Some("run zapadka format --write on this file".to_owned()),
    }
}

fn is_deploy_script(path: &Utf8Path) -> bool {
    path.file_name() == Some("deploy.sql")
}

fn atomic_write(path: &Utf8Path, contents: &str) -> Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().ok_or_else(|| {
        Error::new(
            ErrorCode::Io,
            format!("cannot determine parent directory for {path}"),
        )
    })?;
    let temporary = parent.join(format!(
        ".zapadka-format-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| io_error(&temporary, "create", error))?;
        file.write_all(contents.as_bytes())
            .map_err(|error| io_error(&temporary, "write", error))?;
        file.sync_all()
            .map_err(|error| io_error(&temporary, "sync", error))?;
        drop(file);
        fs::rename(&temporary, path).map_err(|error| io_error(path, "replace atomically", error))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        noun.to_owned()
    } else {
        format!("{noun}s")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use super::*;
    use crate::commands::load_project;
    use crate::testing::{temp_project, write_migration};

    fn args(
        paths: Vec<Utf8PathBuf>,
        check: bool,
        write: bool,
        allow_deploy_rewrite: bool,
    ) -> FormatArgs {
        FormatArgs {
            paths,
            check,
            write,
            allow_deploy_rewrite,
        }
    }

    #[test]
    fn check_reports_every_unformatted_explicit_file_without_writing() {
        let project = temp_project();
        let first = project.path().join("first.sql");
        let second = project.path().join("second.sql");
        fs::write(&first, "select  1;").unwrap();
        fs::write(&second, "select  2;").unwrap();
        let (config, graph) = load_project(project.path()).unwrap();
        let mut session = Session::new("format");

        let error = run(
            &config,
            &graph,
            &args(
                vec![
                    Utf8PathBuf::from("first.sql"),
                    Utf8PathBuf::from("second.sql"),
                ],
                true,
                false,
                false,
            ),
            &mut session,
        )
        .unwrap_err();

        assert_eq!(error.code, ErrorCode::FormatCheckFailed);
        assert_eq!(
            error.context().get("files").unwrap(),
            "first.sql,second.sql"
        );
        assert_eq!(session.diagnostics.len(), 2);
        let report = session.finish(Some(&error));
        assert_eq!(report.error.as_ref().unwrap().code, "format.check_failed");
        assert_eq!(
            report.error.as_ref().unwrap().context["files"],
            "first.sql,second.sql"
        );
        assert_eq!(report.diagnostics.len(), 2);
        assert_eq!(fs::read_to_string(&first).unwrap(), "select  1;");
        assert_eq!(fs::read_to_string(&second).unwrap(), "select  2;");
    }

    #[test]
    fn check_without_paths_discovers_database_tests() {
        let project = temp_project();
        let tests = project.path().join("tests/db/nested");
        fs::create_dir_all(&tests).unwrap();
        fs::write(tests.join("query.sql"), "select  1;").unwrap();
        let (config, graph) = load_project(project.path()).unwrap();
        let mut session = Session::new("format");

        let error = run(
            &config,
            &graph,
            &args(Vec::new(), true, false, false),
            &mut session,
        )
        .unwrap_err();

        assert_eq!(error.code, ErrorCode::FormatCheckFailed);
        assert_eq!(
            error.context().get("files").unwrap(),
            "tests/db/nested/query.sql"
        );
    }

    #[test]
    fn write_rewrites_only_selected_files() {
        let project = temp_project();
        let selected = project.path().join("selected.sql");
        let untouched = project.path().join("untouched.sql");
        fs::write(&selected, "select  1;").unwrap();
        fs::write(&untouched, "select  2;").unwrap();
        let (config, graph) = load_project(project.path()).unwrap();
        let mut session = Session::new("format");

        run(
            &config,
            &graph,
            &args(vec![Utf8PathBuf::from("selected.sql")], false, true, false),
            &mut session,
        )
        .unwrap();

        assert_eq!(fs::read_to_string(&selected).unwrap(), "SELECT 1\n");
        assert_eq!(fs::read_to_string(&untouched).unwrap(), "select  2;");
        assert_eq!(session.diagnostics[0].code, "format.written");
    }

    #[test]
    fn write_preserves_structural_definition_without_acknowledgement() {
        let project = temp_project();
        let id = write_migration(
            project.path(),
            "unformatted",
            &[],
            "create table things(id int);",
        );
        let deploy = project
            .path()
            .join(format!("migrations/{id}-unformatted/deploy.sql"));
        let (config, graph) = load_project(project.path()).unwrap();
        let mut session = Session::new("format");

        run(
            &config,
            &graph,
            &args(vec![deploy.clone()], false, true, false),
            &mut session,
        )
        .unwrap();
        assert_eq!(session.diagnostics[0].code, "format.legacy_targets");
        assert_eq!(
            zapadka_parser::canonicalize(&fs::read_to_string(&deploy).unwrap()).unwrap(),
            zapadka_parser::canonicalize("create table things(id int);").unwrap()
        );

        run(
            &config,
            &graph,
            &args(vec![deploy.clone()], false, true, true),
            &mut session,
        )
        .unwrap();
        assert_ne!(
            fs::read_to_string(&deploy).unwrap(),
            "create table things(id int);"
        );
    }

    #[test]
    fn structural_failure_preflights_every_file_even_with_legacy_override() {
        let project = temp_project();
        let mut files = Vec::new();
        for (name, original, formatted) in [
            ("first.sql", "select  1;", "SELECT 1"),
            (
                "deploy.sql",
                "DO $$BEGIN NULL; END$$",
                "DO $$BEGIN  NULL; END$$",
            ),
        ] {
            let absolute = project.path().join(name);
            fs::write(&absolute, original).unwrap();
            files.push(FormattedFile {
                path: SelectedPath {
                    absolute,
                    relative: name.to_owned(),
                },
                original: original.to_owned(),
                formatted: formatted.to_owned(),
            });
        }
        let error = finish_format(
            &files,
            &args(Vec::new(), false, true, true),
            &mut Session::new("format"),
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::FormatDeployRewriteDenied);
        for file in files {
            assert_eq!(
                fs::read_to_string(file.path.absolute).unwrap(),
                file.original
            );
        }
    }

    #[test]
    fn a_format_error_leaves_every_selected_file_unchanged() {
        let project = temp_project();
        let valid = project.path().join("valid.sql");
        let invalid = project.path().join("invalid.sql");
        fs::write(&valid, "select  1;").unwrap();
        fs::write(&invalid, "select from;").unwrap();
        let (config, graph) = load_project(project.path()).unwrap();
        let mut session = Session::new("format");

        let error = run(
            &config,
            &graph,
            &args(vec![valid.clone(), invalid], false, true, false),
            &mut session,
        )
        .unwrap_err();

        assert_eq!(error.code, ErrorCode::ScriptParseError);
        assert_eq!(fs::read_to_string(&valid).unwrap(), "select  1;");
    }
}
