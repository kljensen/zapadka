//! Zapadka: a static PostgreSQL migration and database-test tool.
//!
//! This crate is a binary, so it has no public API: every `pub` item is
//! reachable only from within it, and the `pub`/`pub(crate)` distinction that
//! `unreachable_pub` enforces carries no information here.
#![allow(unreachable_pub)]

mod cli;
mod commands;
mod human;
mod session;
#[cfg(test)]
mod testing;

use std::io::{IsTerminal, Write};

use camino::Utf8PathBuf;
use clap::Parser;
use cli::{Cli, Command, OutputFormat};
use session::Session;
use zapadka_core::error::{Error, ErrorCode, ExitCode, Result};
use zapadka_core::report::ReportV1;

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let report = run(&cli);
    emit(&report, cli.output, cli.quiet);
    // The exit code always comes from the report, so a nonzero exit and a
    // `"outcome": "failure"` can never disagree. Every code Zapadka defines
    // fits in a u8; anything else would be a bug, and reporting it as an
    // internal error is better than wrapping it into an unrelated code.
    let code = u8::try_from(report.exit_code).unwrap_or(ExitCode::Internal.code_u8());
    std::process::ExitCode::from(code)
}

/// Runs the requested command and returns its report.
fn run(cli: &Cli) -> ReportV1 {
    let mut session = Session::new(cli.command.name());
    let result = dispatch(cli, &mut session);
    session.finish(result.as_ref().err())
}

/// Routes to the command implementation.
fn dispatch(cli: &Cli, session: &mut Session) -> Result<()> {
    let directory = working_directory(cli)?;

    match &cli.command {
        // `init` is the one command that runs without an existing project.
        Command::Init(args) => commands::init::run(&directory, args, session),

        Command::New(args) => {
            let (config, graph) = commands::load_project(&directory)?;
            commands::new::run(&config.root, &graph, args, session)
        }

        Command::Lint => {
            let (config, graph) = commands::load_project(&directory)?;
            commands::lint::run(&graph, &config.config.policy, session)
        }

        // Commands that talk to a database. One current-thread runtime per
        // run: Zapadka opens a single connection and does one thing at a time,
        // so a work-stealing pool would buy nothing and cost startup time.
        Command::Status(args) => {
            let (config, graph) = commands::load_project(&directory)?;
            block_on(commands::status::run(&config, &graph, args, session))
        }
        Command::Deploy(args) => {
            let (config, graph) = commands::load_project(&directory)?;
            block_on(commands::deploy::run(&config, &graph, args, session))
        }
        Command::Verify(args) => {
            let (config, graph) = commands::load_project(&directory)?;
            block_on(commands::verify::run(&config, &graph, args, session))
        }
        Command::Revert(args) => {
            let (config, graph) = commands::load_project(&directory)?;
            block_on(commands::revert::run(&config, &graph, args, session))
        }
        Command::Baseline(args) => {
            let (config, graph) = commands::load_project(&directory)?;
            block_on(commands::baseline::run(&config, &graph, args, session))
        }
    }
}

/// Runs an asynchronous command to completion.
fn block_on<T>(future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            Error::new(
                ErrorCode::Internal,
                format!("cannot start the async runtime: {error}"),
            )
        })?
        .block_on(future)
}

/// Resolves the directory the command operates in.
fn working_directory(cli: &Cli) -> Result<Utf8PathBuf> {
    if let Some(directory) = &cli.directory {
        if !directory.is_dir() {
            return Err(Error::new(
                ErrorCode::Io,
                format!("no such directory: {directory}"),
            ));
        }
        Ok(directory.clone())
    } else {
        let current = std::env::current_dir().map_err(|e| {
            Error::new(
                ErrorCode::Io,
                format!("cannot determine the working directory: {e}"),
            )
        })?;
        Utf8PathBuf::from_path_buf(current).map_err(|path| {
            Error::new(
                ErrorCode::Io,
                format!(
                    "the working directory {} is not valid UTF-8",
                    path.display()
                ),
            )
        })
    }
}

/// Writes the report.
///
/// In JSON mode stdout carries exactly one document and nothing else, so the
/// output can be piped straight into a parser. In human mode stdout carries the
/// summary. Either way, progress and warnings that are not part of the result
/// go to stderr.
fn emit(report: &ReportV1, format: OutputFormat, quiet: bool) {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();

    match format {
        OutputFormat::Json => {
            let _ = stdout.write_all(report.to_json().as_bytes());
        }
        OutputFormat::Human if !quiet => {
            let _ = human::render(report, &mut stdout);
        }
        OutputFormat::Human => {
            // Even when quiet, a failure must say something: a silent nonzero
            // exit is the hardest kind of failure to diagnose.
            if let Some(error) = &report.error {
                let _ = writeln!(
                    std::io::stderr(),
                    "error: {} [{}]",
                    error.message,
                    error.code
                );
            }
        }
    }
    let _ = stdout.flush();
}

/// Whether stderr is a terminal.
///
/// Used only to decide whether to draw progress, never to decide the shape of
/// the result: piping Zapadka's output must not change what it reports.
#[allow(dead_code)]
fn stderr_is_terminal() -> bool {
    std::io::stderr().is_terminal()
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;
    use testing::{temp_dir, temp_project, write_migration};

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("zapadka").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn init_then_new_then_lint_is_a_working_first_session() {
        let dir = temp_dir();
        let path = dir.path().as_str();

        let init = run(&cli(&["-C", path, "init"]));
        assert_eq!(init.exit_code, 0, "{:?}", init.error);

        let new = run(&cli(&["-C", path, "new", "add-orders"]));
        assert_eq!(new.exit_code, 0, "{:?}", new.error);

        let lint = run(&cli(&["-C", path, "lint"]));
        assert_eq!(lint.exit_code, 0, "{:?}", lint.error);
    }

    #[test]
    fn every_command_produces_exactly_one_report() {
        let project = temp_project();
        let path = project.path().as_str();
        for args in [
            vec!["-C", path, "lint"],
            vec!["-C", path, "new", "add-orders"],
            vec!["-C", path, "init"],
        ] {
            let report = run(&cli(&args));
            let json = report.to_json();
            // Exactly one document: a parser must consume the whole string.
            serde_json::from_str::<serde_json::Value>(&json).unwrap();
            assert_eq!(report.report_version, 1);
        }
    }

    #[test]
    fn a_failure_reports_a_nonzero_exit_code_matching_the_error() {
        let dir = temp_dir();
        // No project here, so loading one must fail rather than panic.
        let report = run(&cli(&["-C", dir.path().as_str(), "lint"]));
        assert_ne!(report.exit_code, 0);
        assert_eq!(report.error.as_ref().unwrap().code, "config.not_found");
        assert_eq!(
            report.exit_code,
            zapadka_core::error::ExitCode::Project.code()
        );
    }

    #[test]
    fn invalid_sql_fails_lint_with_a_validation_exit_code() {
        let project = temp_project();
        write_migration(project.path(), "bad", &[], "COMMIT;");
        let report = run(&cli(&["-C", project.path().as_str(), "lint"]));
        assert_eq!(
            report.exit_code,
            zapadka_core::error::ExitCode::Validation.code()
        );
        assert_eq!(
            report.error.as_ref().unwrap().code,
            "script.transaction_control"
        );
    }

    #[test]
    fn a_missing_directory_is_reported_rather_than_panicking() {
        let report = run(&cli(&["-C", "/definitely/not/a/directory", "lint"]));
        assert_ne!(report.exit_code, 0);
        assert_eq!(report.error.as_ref().unwrap().code, "io.error");
    }

    #[test]
    fn the_json_report_never_contains_an_absolute_path() {
        // Reports are compared across machines and pasted into issues; an
        // absolute path leaks the runner's layout and defeats comparison.
        let project = temp_project();
        write_migration(project.path(), "bad", &[], "COMMIT;");
        let report = run(&cli(&["-C", project.path().as_str(), "lint"]));
        let json = report.to_json();
        assert!(
            !json.contains(project.path().as_str()),
            "report leaked the project path:\n{json}"
        );
        assert!(json.contains("migrations/"), "{json}");
    }

    #[test]
    fn a_warning_does_not_change_the_exit_code() {
        let project = temp_project();
        write_migration(project.path(), "risky", &[], "DROP TABLE legacy;");
        let report = run(&cli(&["-C", project.path().as_str(), "lint"]));
        assert_eq!(report.exit_code, 0);
        assert_eq!(report.diagnostics.len(), 1);
    }
}
