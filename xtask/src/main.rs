//! Repository automation.
//!
//! Checks that cannot be expressed as a compiler lint: third-party fixture
//! provenance, the published JSON Schema, and code-quality budgets.
//!
//! Run `cargo xtask <command>`. Every command is also a CI step, so a green
//! local run means a green pipeline.
//!
//! This crate is a binary, so `pub` items are reachable only from within it.
#![allow(unreachable_pub)]

mod metrics;
mod provenance;
mod quality;
mod schema;

use std::process::ExitCode;

use anyhow::{Result, bail};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    let rest: Vec<String> = args.collect();

    let result = match command.as_str() {
        "quality" => quality::run(&rest),
        "metrics" => metrics::run(&rest),
        "verify-fixtures" => provenance::verify(),
        "schema" => schema::run(&rest),
        "help" | "--help" | "-h" | "" => {
            print_help();
            Ok(())
        }
        other => Err(anyhow::anyhow!(
            "unknown command {other:?}\n\nRun `cargo xtask help` for the list."
        )),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xtask: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    println!(
        "\
Repository automation for Zapadka.

Usage: cargo xtask <command>

Commands:
  quality           Run the full static-analysis and code-quality battery
  metrics           Report complexity metrics and enforce the budgets
  verify-fixtures   Check that vendored third-party files are unmodified
  schema            Generate the ReportV1 JSON Schema

Options:
  metrics --json    Emit metrics as JSON instead of a table
  quality --fast    Skip checks that need a network or a full rebuild
  schema --check    Fail if the checked-in schema is out of date
"
    );
}

/// The repository root, derived from this crate's location.
pub fn repo_root() -> Result<camino::Utf8PathBuf> {
    let manifest = camino::Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest
        .parent()
        .ok_or_else(|| anyhow::anyhow!("cannot find the repository root from {manifest}"))?;
    if !root.join("Cargo.toml").is_file() {
        bail!("{root} does not look like the repository root");
    }
    Ok(root.to_path_buf())
}
