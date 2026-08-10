//! Generating the published `ReportV1` JSON Schema.
//!
//! The schema is checked in and shipped with each release so that a consumer
//! can validate Zapadka's output without running Zapadka. Because it is
//! generated, it cannot drift from the Rust model by hand — but it *can* drift
//! by someone changing the model and forgetting to regenerate, which is what
//! `--check` exists to catch in CI.

use anyhow::{Result, bail};

/// Where the generated schema lives.
const SCHEMA_PATH: &str = "docs/report-v1.schema.json";

/// Runs the schema command.
pub fn run(args: &[String]) -> Result<()> {
    let root = crate::repo_root()?;
    let path = root.join(SCHEMA_PATH);

    let mut json = serde_json::to_string_pretty(&zapadka_core::report::ReportV1::json_schema())?;
    json.push('\n');

    if args.iter().any(|arg| arg == "--check") {
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        if current != json {
            bail!(
                "{SCHEMA_PATH} is out of date with the ReportV1 model; run `cargo xtask schema` \
                 and commit the result"
            );
        }
        println!("{SCHEMA_PATH} is up to date");
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, json)?;
    println!("wrote {SCHEMA_PATH}");
    Ok(())
}
