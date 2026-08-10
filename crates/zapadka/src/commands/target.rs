//! Opening a target: connecting, checking the server, and reading the registry.
//!
//! Shared by every command that touches a database so that they all observe the
//! same preconditions in the same order. A command that skipped one of these
//! checks would be the one that produced a misleading report.

use zapadka_core::config::LoadedConfig;
use zapadka_core::error::{Error, ErrorCode, Result};
use zapadka_core::report::{Diagnostic, Severity, Target};
use zapadka_pg::execute::Timeouts;
use zapadka_pg::registry::{self, RegistryState, ServerFacts};

use crate::cli::TargetArgs;
use crate::session::Session;

/// A connected, inspected target.
pub struct OpenTarget {
    pub connection: zapadka_pg::Connection,
    pub facts: ServerFacts,
    pub state: RegistryState,
    pub name: String,
    pub schema: String,
    pub timeouts: Timeouts,
}

/// Connects to the selected target and reads its registry.
pub async fn open(
    config: &LoadedConfig,
    args: &TargetArgs,
    session: &mut Session,
) -> Result<OpenTarget> {
    let name = select(config, args)?;
    let target = config.config.targets.get(&name);

    let resolved = zapadka_pg::resolve(&name, target, args.uri.as_deref())?;
    let connection = zapadka_pg::connect(&resolved).await?;

    // Version first: every later check assumes a PostgreSQL 18 catalog.
    let facts = registry::server_facts(&connection.client).await?;
    let schema = config.config.project.registry_schema.clone();
    let state = registry::read(&connection.client, &schema).await?;

    // Before any command uses this state. An initialized registry belonging to
    // a different project would otherwise be read as if it were ours, and a
    // mutating command would act on it.
    registry::check_project(&state, config.config.project.id)?;
    // And again database-wide, because `registry_schema` is configurable: two
    // projects with different schema names would each see only their own
    // registry and both conclude the database was theirs.
    registry::check_database_ownership(&connection.client, &schema, config.config.project.id)
        .await?;

    if !connection.encrypted && !connection.encryption_opted_out {
        session.diagnose(Diagnostic {
            severity: Severity::Warning,
            code: "target.unencrypted".to_owned(),
            message: format!("the connection to {name} is not encrypted"),
            migration_id: None,
            location: None,
            hint: Some(
                "the server did not offer TLS; set sslmode=disable on the target to state that \
                 this is intended, or enable TLS on the server"
                    .to_owned(),
            ),
        });
    }

    let timeouts = Timeouts {
        lock_timeout: target.and_then(|target| target.lock_timeout),
        statement_timeout: target.and_then(|target| target.statement_timeout),
    };

    session.target = Some(Target {
        name: name.clone(),
        database: Some(connection.database.clone()),
        server_version: Some(facts.server_version.clone()),
        session_user: Some(facts.session_user.clone()),
        current_user: Some(facts.current_user.clone()),
        registry_schema: Some(schema.clone()),
        // Registry versions are small positive integers; a negative one
        // would mean the registry is not a Zapadka registry.
        registry_format_version: state
            .format_version
            .and_then(|version| u32::try_from(version).ok()),
        lock_timeout: timeouts.lock_timeout.map(|value| value.to_string()),
        statement_timeout: timeouts.statement_timeout.map(|value| value.to_string()),
    });

    Ok(OpenTarget {
        connection,
        facts,
        state,
        name,
        schema,
        timeouts,
    })
}

/// Chooses which target to act on.
///
/// Naming a target explicitly is always allowed. When the project declares
/// exactly one, that one is used, because asking someone to name their only
/// target is pointless ceremony. When it declares several, Zapadka refuses to
/// guess: picking the wrong one means deploying to the wrong database.
fn select(config: &LoadedConfig, args: &TargetArgs) -> Result<String> {
    if let Some(name) = &args.target {
        // Validated even when `--uri` overrides the connection, so a typo is
        // still caught.
        config.target(name)?;
        return Ok(name.clone());
    }

    let names: Vec<&String> = config.config.targets.keys().collect();
    match names.as_slice() {
        [only] => Ok((*only).clone()),
        [] if args.uri.is_some() => Ok("uri".to_owned()),
        [] => Err(
            Error::new(ErrorCode::TargetUnknown, "this project declares no targets")
                .with_hint("add a [targets.<name>] section to zapadka.toml, or pass --uri"),
        ),
        many => Err(Error::new(
            ErrorCode::TargetUnknown,
            "this project declares several targets, so one must be named",
        )
        .with_hint(format!(
            "pass --target with one of: {}",
            many.iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Re-reads the registry now that the deployment lock is held.
///
/// Everything read before the lock was acquired is a snapshot of a database
/// another Zapadka run may have been in the middle of changing. Acting on it
/// is a time-of-check-to-time-of-use bug with real consequences: a migration
/// that was a graph leaf when it was checked may have gained a dependent by
/// the time it is reverted.
///
/// So mutating commands read once to report the target and check the server,
/// then read again under the lock and decide from that.
pub async fn refresh_state(
    client: &zapadka_pg::Client,
    config: &LoadedConfig,
    schema: &str,
) -> Result<RegistryState> {
    let state = registry::read(client, schema).await?;
    registry::check_project(&state, config.config.project.id)?;
    registry::check_database_ownership(client, schema, config.config.project.id).await?;
    Ok(state)
}

/// Requires that the registry exists, for commands that cannot create it.
pub fn require_initialized(state: &RegistryState, name: &str) -> Result<()> {
    if state.is_initialized() {
        return Ok(());
    }
    Err(Error::new(
        ErrorCode::RegistryNotInitialized,
        format!("target {name} has no Zapadka registry"),
    )
    .with_hint("run `zapadka deploy` to create the registry and apply migrations"))
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;
    use camino::Utf8PathBuf;
    use zapadka_core::config::Config;

    fn project(targets: &str) -> LoadedConfig {
        let text = format!(
            "format_version = 1\n[project]\nid = \"0198f5c0-0000-7000-8000-00000000000a\"\n{targets}"
        );
        LoadedConfig {
            config: Config::parse(&text).unwrap(),
            root: Utf8PathBuf::from("/project"),
        }
    }

    fn args(target: Option<&str>, uri: Option<&str>) -> TargetArgs {
        TargetArgs {
            target: target.map(str::to_owned),
            uri: uri.map(str::to_owned),
        }
    }

    #[test]
    fn a_single_target_needs_no_naming() {
        let config = project("[targets.production]\npg_service = \"p\"\n");
        assert_eq!(select(&config, &args(None, None)).unwrap(), "production");
    }

    #[test]
    fn several_targets_must_be_disambiguated() {
        // Guessing here would mean deploying to the wrong database.
        let config = project(
            "[targets.production]\npg_service = \"p\"\n[targets.staging]\npg_service = \"s\"\n",
        );
        let error = select(&config, &args(None, None)).unwrap_err();
        assert_eq!(error.code, ErrorCode::TargetUnknown);
        let hint = error.hint().unwrap();
        assert!(
            hint.contains("production") && hint.contains("staging"),
            "{hint}"
        );
    }

    #[test]
    fn an_explicitly_named_target_is_used_however_many_exist() {
        let config = project(
            "[targets.production]\npg_service = \"p\"\n[targets.staging]\npg_service = \"s\"\n",
        );
        assert_eq!(
            select(&config, &args(Some("staging"), None)).unwrap(),
            "staging"
        );
    }

    #[test]
    fn a_misspelled_target_is_caught_even_when_uri_overrides_the_connection() {
        let config = project("[targets.production]\npg_service = \"p\"\n");
        let error = select(
            &config,
            &args(Some("prodcution"), Some("postgresql://localhost/app")),
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::TargetUnknown);
    }

    #[test]
    fn a_uri_alone_works_without_any_declared_target() {
        let config = project("");
        assert_eq!(
            select(&config, &args(None, Some("postgresql://localhost/app"))).unwrap(),
            "uri"
        );
    }

    #[test]
    fn a_project_with_no_targets_and_no_uri_says_what_to_do() {
        let config = project("");
        let error = select(&config, &args(None, None)).unwrap_err();
        assert!(error.hint().unwrap().contains("--uri"));
    }

    #[test]
    fn commands_that_cannot_create_a_registry_say_which_one_can() {
        let state = RegistryState {
            format_version: None,
            project_id: None,
            applied: std::collections::BTreeMap::default(),
        };
        let error = require_initialized(&state, "production").unwrap_err();
        assert_eq!(error.code, ErrorCode::RegistryNotInitialized);
        assert!(error.hint().unwrap().contains("deploy"));
    }
}
