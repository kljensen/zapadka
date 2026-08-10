//! Complexity metrics and the budgets they must stay inside.
//!
//! # Why cognitive complexity, and not cyclomatic
//!
//! Both are reported, but only cognitive complexity is enforced.
//!
//! Cyclomatic complexity counts branches, so it charges the same amount for a
//! flat forty-arm `match` mapping error codes to strings as for four levels of
//! nested conditionals. The first is trivially readable and the second is not.
//! `ErrorCode::as_str` in this repository scores 39 cyclomatic and 0 cognitive,
//! which is exactly right: there is nothing to hold in your head.
//!
//! Cognitive complexity charges for nesting and for breaks in linear flow,
//! which is much closer to "can a reviewer follow this". That makes it the
//! metric worth failing a build over.
//!
//! Metrics come from Mozilla's `rust-code-analysis`, deliberately a different
//! implementation from clippy's own `cognitive_complexity` lint, so the two
//! have to agree before a function is considered simple.

use std::collections::BTreeMap;
use std::process::Command;

use anyhow::{Context, Result, bail};
use camino::Utf8PathBuf;
use serde::Deserialize;

/// The most cognitive complexity any single function may carry.
///
/// Matches `cognitive-complexity-threshold` in `clippy.toml`. Raising it is a
/// decision to make deliberately, in a commit that says why.
const MAX_COGNITIVE: u64 = 15;

/// The most source lines any single function may span.
///
/// A function longer than this is usually several functions that have not been
/// separated yet.
const MAX_FUNCTION_SLOC: u64 = 120;

/// Directories analysed. Tests are excluded: a test's job is to be obvious and
/// repetitive, and holding them to a production complexity budget would push
/// people toward clever tests.
const ANALYSED: [&str; 4] = [
    "crates/zapadka-core/src",
    "crates/zapadka-parser/src",
    "crates/zapadka-pg/src",
    "crates/zapadka/src",
];

/// One function's measurements.
#[derive(Debug, Clone)]
pub struct FunctionMetrics {
    pub file: String,
    pub name: String,
    pub line: u64,
    pub cognitive: u64,
    pub cyclomatic: u64,
    pub sloc: u64,
}

/// One file's measurements.
#[derive(Debug, Clone)]
pub struct FileMetrics {
    /// Repository-relative path.
    pub file: String,
    /// Source lines, excluding blanks and comments.
    pub sloc: u64,
    /// How many functions the file defines.
    pub functions: u64,
}

/// Everything measured in one run.
#[derive(Debug, Default)]
pub struct Report {
    pub functions: Vec<FunctionMetrics>,
    pub files: Vec<FileMetrics>,
}

impl Report {
    /// Functions that exceed a budget.
    pub fn violations(&self) -> Vec<(&FunctionMetrics, &'static str, u64)> {
        let mut found = Vec::new();
        for function in &self.functions {
            if function.cognitive > MAX_COGNITIVE {
                found.push((function, "cognitive complexity", MAX_COGNITIVE));
            }
            if function.sloc > MAX_FUNCTION_SLOC {
                found.push((function, "length in lines", MAX_FUNCTION_SLOC));
            }
        }
        found
    }

    /// The nth percentile of cognitive complexity, for the summary.
    ///
    /// Computed with integer arithmetic: a floating-point index into a sorted
    /// list buys nothing and invites rounding questions.
    fn percentile(&self, percent: usize) -> u64 {
        if self.functions.is_empty() {
            return 0;
        }
        let mut values: Vec<u64> = self.functions.iter().map(|f| f.cognitive).collect();
        values.sort_unstable();
        values[(values.len() - 1) * percent / 100]
    }
}

/// Runs the metrics command.
pub fn run(args: &[String]) -> Result<()> {
    let report = collect()?;

    if args.iter().any(|arg| arg == "--json") {
        print_json(&report);
    } else {
        print_table(&report);
    }

    let violations = report.violations();
    if violations.is_empty() {
        return Ok(());
    }

    eprintln!();
    for (function, budget, limit) in &violations {
        eprintln!(
            "over budget: {}:{} {} has {} {}, limit {limit}",
            function.file,
            function.line,
            function.name,
            if *budget == "cognitive complexity" {
                function.cognitive
            } else {
                function.sloc
            },
            budget,
        );
    }
    bail!(
        "{} function(s) exceed a complexity budget; split them, or raise the budget in \
         xtask/src/metrics.rs with a note explaining why",
        violations.len()
    );
}

/// Measures the workspace.
pub fn collect() -> Result<Report> {
    let root = crate::repo_root()?;
    let output = std::env::temp_dir().join(format!("zapadka-metrics-{}", std::process::id()));
    let output = Utf8PathBuf::from_path_buf(output)
        .map_err(|path| anyhow::anyhow!("temporary path {} is not UTF-8", path.display()))?;
    let _ = std::fs::remove_dir_all(&output);
    std::fs::create_dir_all(&output)?;

    for directory in ANALYSED {
        let status = Command::new("rust-code-analysis-cli")
            .args(["--metrics", "--output-format", "json"])
            .arg("--output")
            .arg(output.as_str())
            .arg("--paths")
            .arg(root.join(directory).as_str())
            .status()
            .context(
                "cannot run rust-code-analysis-cli; install it with \
                 `cargo install rust-code-analysis-cli --locked`",
            )?;
        if !status.success() {
            bail!("rust-code-analysis-cli failed on {directory}");
        }
    }

    let mut report = Report::default();
    collect_from(&output, &root, &mut report)?;
    let _ = std::fs::remove_dir_all(&output);

    report
        .functions
        .sort_by(|a, b| b.cognitive.cmp(&a.cognitive).then(a.file.cmp(&b.file)));
    report.files.sort_by(|a, b| b.sloc.cmp(&a.sloc));
    Ok(report)
}

/// The subset of `rust-code-analysis` output Zapadka reads.
#[derive(Debug, Deserialize)]
struct Space {
    name: Option<String>,
    kind: Option<String>,
    start_line: Option<u64>,
    metrics: Option<Metrics>,
    #[serde(default)]
    spaces: Vec<Space>,
}

#[derive(Debug, Deserialize)]
struct Metrics {
    cognitive: Option<Sum>,
    cyclomatic: Option<Sum>,
    loc: Option<Loc>,
    nom: Option<Nom>,
}

#[derive(Debug, Deserialize)]
struct Sum {
    sum: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct Loc {
    sloc: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct Nom {
    total: Option<f64>,
}

/// Rounds a metric, which the tool emits as a float.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn whole(value: Option<f64>) -> u64 {
    value.unwrap_or_default().max(0.0).round() as u64
}

fn collect_from(output: &Utf8PathBuf, root: &Utf8PathBuf, report: &mut Report) -> Result<()> {
    for entry in walkdir::WalkDir::new(output)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let text = std::fs::read_to_string(entry.path())?;
        let Ok(space) = serde_json::from_str::<Space>(&text) else {
            continue;
        };
        let absolute = space.name.clone().unwrap_or_default();
        let file = absolute.strip_prefix(root.as_str()).map_or_else(
            || absolute.clone(),
            |path| path.trim_start_matches('/').to_owned(),
        );

        if let Some(metrics) = &space.metrics {
            report.files.push(FileMetrics {
                file: file.clone(),
                sloc: whole(metrics.loc.as_ref().and_then(|loc| loc.sloc)),
                functions: whole(metrics.nom.as_ref().and_then(|nom| nom.total)),
            });
        }
        walk(&space, &file, report);
    }
    Ok(())
}

fn walk(space: &Space, file: &str, report: &mut Report) {
    if space.kind.as_deref() == Some("function")
        && let Some(metrics) = &space.metrics
    {
        report.functions.push(FunctionMetrics {
            file: file.to_owned(),
            name: space
                .name
                .clone()
                .unwrap_or_else(|| "<anonymous>".to_owned()),
            line: space.start_line.unwrap_or_default(),
            cognitive: whole(metrics.cognitive.as_ref().and_then(|m| m.sum)),
            cyclomatic: whole(metrics.cyclomatic.as_ref().and_then(|m| m.sum)),
            sloc: whole(metrics.loc.as_ref().and_then(|loc| loc.sloc)),
        });
    }
    for child in &space.spaces {
        walk(child, file, report);
    }
}

fn print_table(report: &Report) {
    println!(
        "Complexity budgets: cognitive <= {MAX_COGNITIVE}, function length <= {MAX_FUNCTION_SLOC} lines\n"
    );

    println!("Most complex functions:");
    println!("  {:>4} {:>4} {:>5}  location", "COG", "CYC", "SLOC");
    for function in report.functions.iter().take(10) {
        println!(
            "  {:>4} {:>4} {:>5}  {}:{} {}",
            function.cognitive,
            function.cyclomatic,
            function.sloc,
            function.file,
            function.line,
            function.name
        );
    }

    let total = report.functions.len();
    println!("\nSummary:");
    println!("  functions analysed   {total}");
    if total > 0 {
        println!(
            "  cognitive complexity max {}, p90 {}, median {}",
            report.functions.first().map_or(0, |f| f.cognitive),
            report.percentile(90),
            report.percentile(50),
        );
    }
    println!(
        "  source lines         {} across {} files",
        report.files.iter().map(|f| f.sloc).sum::<u64>(),
        report.files.len()
    );
    println!(
        "  largest file         {}",
        report.files.first().map_or_else(
            || "none".to_owned(),
            |f| format!("{} ({} lines, {} functions)", f.file, f.sloc, f.functions)
        )
    );

    let over: Vec<&FunctionMetrics> = report
        .functions
        .iter()
        .filter(|f| f.cognitive > MAX_COGNITIVE || f.sloc > MAX_FUNCTION_SLOC)
        .collect();
    println!("  over budget          {}", over.len());
}

fn print_json(report: &Report) {
    let functions: Vec<BTreeMap<&str, serde_json::Value>> = report
        .functions
        .iter()
        .map(|function| {
            BTreeMap::from([
                ("file", function.file.clone().into()),
                ("name", function.name.clone().into()),
                ("line", function.line.into()),
                ("cognitive", function.cognitive.into()),
                ("cyclomatic", function.cyclomatic.into()),
                ("sloc", function.sloc.into()),
            ])
        })
        .collect();
    let document = serde_json::json!({
        "budgets": {
            "max_cognitive": MAX_COGNITIVE,
            "max_function_sloc": MAX_FUNCTION_SLOC,
        },
        "functions": functions,
        "over_budget": report.violations().len(),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&document).unwrap_or_default()
    );
}
