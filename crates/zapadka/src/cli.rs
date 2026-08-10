//! The command-line surface.

use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand, ValueEnum};
use zapadka_core::duration::Timeout;

/// A static PostgreSQL migration and database-test tool.
#[derive(Debug, Parser)]
#[command(name = "zapadka", version, about, long_about = None)]
#[command(propagate_version = true)]
pub struct Cli {
    /// Run as if started in this directory.
    #[arg(short = 'C', long, global = true, value_name = "DIR")]
    pub directory: Option<Utf8PathBuf>,

    /// How to report the result.
    ///
    /// The default never changes based on whether output is a terminal: a
    /// pipeline and an interactive shell must see the same thing.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Human)]
    pub output: OutputFormat,

    /// Suppress progress output on stderr. Does not affect the result.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// How to report the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// A summary for a person, on stdout.
    Human,
    /// Exactly one ReportV1 JSON document, on stdout.
    Json,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a Zapadka project in this directory.
    Init(InitArgs),

    /// Create a migration that depends on every current graph head.
    New(NewArgs),

    /// Validate migrations without connecting to a database.
    Lint,

    /// Compare the checked-out graph with what the target has applied.
    Status(TargetArgs),

    /// Apply every pending migration in dependency order.
    Deploy(DeployArgs),

    /// Run verification for migrations already applied to the target.
    Verify(VerifyArgs),

    /// Undo one applied migration that nothing else depends on.
    Revert(RevertArgs),

    /// Record migrations as applied without running them.
    Baseline(BaselineArgs),
}

impl Command {
    /// The command name recorded in the report.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Init(_) => "init",
            Self::New(_) => "new",
            Self::Lint => "lint",
            Self::Status(_) => "status",
            Self::Deploy(_) => "deploy",
            Self::Verify(_) => "verify",
            Self::Revert(_) => "revert",
            Self::Baseline(_) => "baseline",
        }
    }
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Overwrite nothing, but do not fail if the project already exists.
    #[arg(long)]
    pub allow_existing: bool,
}

#[derive(Debug, Args)]
pub struct NewArgs {
    /// A short name for the migration, such as add-orders-table.
    pub slug: String,

    /// Depend on these migrations instead of the current graph heads.
    ///
    /// Use this only to create deliberately independent work; the default is
    /// what keeps ordinary sequential development linear.
    #[arg(long, value_name = "ID", num_args = 1..)]
    pub depends: Vec<String>,

    /// Create a migration that cannot be reverted, with a stated reason.
    #[arg(long, value_name = "REASON")]
    pub irreversible: Option<String>,
}

/// Options for selecting and reaching a database.
#[derive(Debug, Args, Clone)]
pub struct TargetArgs {
    /// The target to act on, as named in zapadka.toml.
    #[arg(long, value_name = "NAME")]
    pub target: Option<String>,

    /// Connect to this URI instead of resolving the target's configuration.
    ///
    /// Prefer a target with `pg_service` or `uri_env`: a URI on the command
    /// line is visible in the process list to every user on the machine.
    #[arg(long, value_name = "URI")]
    pub uri: Option<String>,
}

#[derive(Debug, Args)]
pub struct DeployArgs {
    #[command(flatten)]
    pub target: TargetArgs,

    /// Validate and report the exact plan without running any user SQL.
    ///
    /// This is a plan preview, not a rehearsal: it does not predict how long
    /// migrations take, which locks they will take, or what they will do to
    /// your data.
    #[arg(long)]
    pub dry_run: bool,

    /// Do not run verification after each migration commits.
    #[arg(long, conflicts_with = "verify")]
    pub no_verify: bool,

    /// Run verification after each migration commits. This is the default.
    #[arg(long)]
    pub verify: bool,

    /// How long to wait for the deployment lock.
    ///
    /// Overrides `policy.advisory_lock_timeout`. `0` waits indefinitely.
    #[arg(long, value_name = "DURATION", value_parser = parse_timeout)]
    pub wait: Option<Timeout>,
}

impl DeployArgs {
    /// Whether to verify each migration after it commits.
    ///
    /// Verification is on unless it is explicitly turned off. `--verify` is
    /// accepted so that a cautious pipeline can state the default rather than
    /// depend on it.
    pub fn should_verify(&self) -> bool {
        !self.no_verify
    }
}

#[derive(Debug, Args)]
pub struct VerifyArgs {
    #[command(flatten)]
    pub target: TargetArgs,

    /// Verify only these migrations. Defaults to every applied migration.
    #[arg(value_name = "ID")]
    pub migrations: Vec<String>,

    /// How long to wait for the deployment lock.
    #[arg(long, value_name = "DURATION", value_parser = parse_timeout)]
    pub wait: Option<Timeout>,
}

#[derive(Debug, Args)]
pub struct RevertArgs {
    #[command(flatten)]
    pub target: TargetArgs,

    /// The migration to revert, as an id, an id prefix, or a slug.
    ///
    /// It must be applied, reversible, and a leaf: nothing else applied may
    /// depend on it. Reverting several migrations means running this several
    /// times, in an order you choose.
    pub migration: String,

    /// How long to wait for the deployment lock.
    #[arg(long, value_name = "DURATION", value_parser = parse_timeout)]
    pub wait: Option<Timeout>,
}

#[derive(Debug, Args)]
pub struct BaselineArgs {
    #[command(flatten)]
    pub target: TargetArgs,

    /// Record this migration and everything it depends on as applied.
    #[arg(long, value_name = "ID")]
    pub to: String,

    /// State that the schema these migrations describe is already present.
    ///
    /// Required. Zapadka cannot check that a database matches the migrations
    /// being recorded, so the claim has to be made by a person.
    #[arg(long)]
    pub acknowledge_existing_schema: bool,

    /// How long to wait for the deployment lock.
    #[arg(long, value_name = "DURATION", value_parser = parse_timeout)]
    pub wait: Option<Timeout>,
}

/// Parses a `--wait` value, reporting the accepted spellings on failure.
fn parse_timeout(text: &str) -> Result<Timeout, String> {
    Timeout::parse(text).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_definition_is_valid() {
        Cli::command().debug_assert();
    }

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("zapadka").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn human_output_is_the_default_regardless_of_context() {
        assert_eq!(parse(&["lint"]).output, OutputFormat::Human);
        assert_eq!(
            parse(&["--output", "json", "lint"]).output,
            OutputFormat::Json
        );
    }

    #[test]
    fn verification_is_on_unless_explicitly_turned_off() {
        let deploy = |args: &[&str]| match parse(args).command {
            Command::Deploy(deploy) => deploy,
            _ => unreachable!(),
        };
        assert!(deploy(&["deploy"]).should_verify());
        assert!(deploy(&["deploy", "--verify"]).should_verify());
        assert!(!deploy(&["deploy", "--no-verify"]).should_verify());
    }

    #[test]
    fn asking_for_verification_and_refusing_it_is_a_usage_error() {
        assert!(
            Cli::try_parse_from(["zapadka", "deploy", "--verify", "--no-verify"]).is_err(),
            "contradictory flags must not silently pick one"
        );
    }

    #[test]
    fn wait_accepts_durations_and_rejects_nonsense() {
        let wait = |args: &[&str]| match parse(args).command {
            Command::Deploy(deploy) => deploy.wait,
            _ => unreachable!(),
        };
        assert_eq!(wait(&["deploy"]), None);
        assert_eq!(
            wait(&["deploy", "--wait", "30s"]),
            Some(Timeout::from_secs(30))
        );
        assert_eq!(wait(&["deploy", "--wait", "0"]), Some(Timeout::ZERO));
        assert!(Cli::try_parse_from(["zapadka", "deploy", "--wait", "soon"]).is_err());
    }

    #[test]
    fn global_options_work_before_or_after_the_subcommand() {
        assert_eq!(
            parse(&["--output", "json", "lint"]).output,
            OutputFormat::Json
        );
        assert_eq!(
            parse(&["lint", "--output", "json"]).output,
            OutputFormat::Json
        );
    }

    #[test]
    fn command_names_match_what_the_report_records() {
        assert_eq!(parse(&["init"]).command.name(), "init");
        assert_eq!(parse(&["new", "add-orders"]).command.name(), "new");
        assert_eq!(parse(&["deploy"]).command.name(), "deploy");
    }

    #[test]
    fn new_requires_a_slug_and_accepts_explicit_dependencies() {
        assert!(Cli::try_parse_from(["zapadka", "new"]).is_err());
        let Command::New(args) = parse(&["new", "add-orders", "--depends", "a", "b"]).command
        else {
            unreachable!()
        };
        assert_eq!(args.slug, "add-orders");
        assert_eq!(args.depends, ["a", "b"]);
    }
}
