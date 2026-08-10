//! The full static-analysis battery.
//!
//! One command so that "is this ready to merge" has a single answer, and so
//! that CI and a laptop run exactly the same checks. Each check reports
//! separately and the battery keeps going after a failure: fixing five findings
//! from one run is faster than rediscovering them one run at a time.

use std::process::Command;

use anyhow::{Result, bail};

/// One check in the battery.
struct Check {
    name: &'static str,
    /// What this check protects against, shown when it fails.
    rationale: &'static str,
    program: &'static str,
    args: &'static [&'static str],
    /// Whether the check needs a tool that may not be installed.
    optional_tool: bool,
    /// Whether to skip this check under `--fast`.
    slow: bool,
}

const CHECKS: &[Check] = &[
    Check {
        name: "formatting",
        rationale: "unformatted code makes every later diff noisier than it needs to be",
        program: "cargo",
        args: &["fmt", "--all", "--check"],
        optional_tool: false,
        slow: false,
    },
    Check {
        name: "clippy",
        rationale: "the lint set includes pedantic, strict numeric casts, and missing docs",
        program: "cargo",
        args: &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
        optional_tool: false,
        slow: true,
    },
    Check {
        name: "unit tests",
        rationale: "the behaviour contracts that do not need a database",
        program: "cargo",
        args: &["test", "--workspace", "--locked"],
        optional_tool: false,
        slow: true,
    },
    Check {
        name: "documentation",
        rationale: "a broken intra-doc link is a reference that silently stopped resolving",
        program: "cargo",
        args: &[
            "doc",
            "--workspace",
            "--no-deps",
            "--document-private-items",
        ],
        optional_tool: false,
        slow: true,
    },
    Check {
        name: "unused dependencies",
        rationale: "an unused dependency is build time and attack surface bought for nothing",
        // Invoked directly rather than as `cargo machete`: the subcommand form
        // passes its own name through as a path to scan.
        program: "cargo-machete",
        args: &["."],
        optional_tool: true,
        slow: false,
    },
    Check {
        name: "advisories, licences, and sources",
        rationale: "a known-vulnerable or unexpectedly licensed dependency is a release blocker",
        program: "cargo",
        args: &["deny", "--all-features", "check"],
        optional_tool: true,
        slow: false,
    },
];

/// Runs the battery.
pub fn run(args: &[String]) -> Result<()> {
    let fast = args.iter().any(|arg| arg == "--fast");
    let root = crate::repo_root()?;

    let mut failed: Vec<&str> = Vec::new();
    let mut skipped: Vec<&str> = Vec::new();

    for check in CHECKS {
        if fast && check.slow {
            skipped.push(check.name);
            continue;
        }

        println!("== {} ==", check.name);
        let mut command = Command::new(check.program);
        command.args(check.args).current_dir(&root);
        if check.name.starts_with("advisories") {
            // cargo-deny clones the advisory database over https. A developer
            // whose git config rewrites github https URLs to ssh would fail
            // here for reasons that have nothing to do with the dependencies.
            command.env("GIT_CONFIG_GLOBAL", "/dev/null");
        }
        let outcome = command.status();

        match outcome {
            Ok(status) if status.success() => println!("   ok\n"),
            Ok(_) => {
                println!("   FAILED — {}\n", check.rationale);
                failed.push(check.name);
            }
            Err(error) if check.optional_tool => {
                // A missing optional tool is reported but does not fail a local
                // run. CI installs every tool, so nothing is silently skipped
                // where it matters.
                println!("   skipped: {} is not installed ({error})\n", check.program);
                skipped.push(check.name);
            }
            Err(error) => {
                println!("   FAILED to run: {error}\n");
                failed.push(check.name);
            }
        }
    }

    println!("== complexity budgets ==");
    match crate::metrics::collect() {
        Ok(report) => {
            let violations = report.violations();
            if violations.is_empty() {
                println!(
                    "   ok — {} functions, none over budget\n",
                    report.functions.len()
                );
            } else {
                for (function, budget, limit) in &violations {
                    println!(
                        "   {}:{} {} exceeds {budget} (limit {limit})",
                        function.file, function.line, function.name
                    );
                }
                println!();
                failed.push("complexity budgets");
            }
        }
        Err(error) => {
            println!("   skipped: {error}\n");
            skipped.push("complexity budgets");
        }
    }

    println!("== fixture provenance ==");
    match crate::provenance::verify() {
        Ok(()) => println!("   ok\n"),
        Err(error) => {
            println!("   FAILED: {error}\n");
            failed.push("fixture provenance");
        }
    }

    if !skipped.is_empty() {
        println!("Skipped: {}", skipped.join(", "));
    }
    if failed.is_empty() {
        println!("All checks passed.");
        return Ok(());
    }
    bail!("{} check(s) failed: {}", failed.len(), failed.join(", "));
}
