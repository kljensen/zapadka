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
    check_ownership(&connection.client, &schema, config.config.project.id).await?;

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

    if facts.reaches_outside_database {
        session.diagnose(Diagnostic {
            severity: Severity::Note,
            code: "target.privileged_role".to_owned(),
            message: format!(
                "{} connects as {}, which can act outside the database",
                name, facts.current_user
            ),
            migration_id: None,
            location: None,
            hint: Some(
                "the role is a superuser or a member of pg_execute_server_program or \
                 pg_write_server_files. Zapadka runs verification read-only so it cannot change \
                 committed state, but nothing rolls back a COPY ... TO PROGRAM or a file an \
                 untrusted-language function writes. Deploying as a role that owns the schema and \
                 no more keeps that guarantee whole."
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
    check_ownership(client, schema, config.config.project.id).await?;
    Ok(state)
}

/// Checks database-wide ownership.
///
/// A read-only check, used by every command that opens a target so none of them
/// operates on a database another project owns. It is *not* sufficient on its
/// own for a command that creates a registry -- see [`claim_and_upgrade`].
async fn check_ownership(
    client: &zapadka_pg::Client,
    schema: &str,
    project_id: uuid::Uuid,
) -> Result<()> {
    registry::check_database_ownership(client, schema, project_id).await
}

/// Claims the database and creates or upgrades the registry, atomically.
///
/// The check and the creation happen under one database-global advisory lock.
/// Checking under a lock and then releasing it before creating would leave the
/// race it was meant to close: two projects first deploying to the same empty
/// database would each take the lock in turn, each see no owner, and each go on
/// to create a registry in its own schema.
///
/// The lock is global rather than project-derived, because the projects
/// contending for it have by definition not agreed on anything else. It is held
/// only across the claim, so unrelated databases are not serialized for the
/// length of a deploy.
pub async fn claim_and_upgrade(
    client: &mut zapadka_pg::Client,
    config: &LoadedConfig,
    schema: &str,
    state: &RegistryState,
    wait: zapadka_core::duration::Timeout,
) -> Result<()> {
    let project_id = config.config.project.id;
    let held = zapadka_pg::lock::acquire_ownership(client, wait).await?;

    let outcome = match registry::check_database_ownership(client, schema, project_id).await {
        Ok(()) => registry::upgrade(client, schema, project_id, crate::session::VERSION, state)
            .await
            .map(|_| ()),
        Err(error) => Err(error),
    };

    let released = held.release(client).await;
    outcome.and(released)
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

/// Fails when a nontransactional statement's outcome was never resolved.
///
/// Every command that changes a target calls this. The reasoning is the same
/// each time: the database is in a state Zapadka cannot describe. It does not
/// know whether an index exists, so it does not know what the next migration
/// would be running against, and a plan computed from applied state would be
/// built on a gap. Blocking is the only answer that cannot make it worse.
pub fn require_not_blocked(state: &RegistryState, name: &str) -> Result<()> {
    let mut blocked: Vec<&zapadka_pg::registry::UnresolvedAttempt> =
        state.unresolved.values().collect();
    blocked.sort_by(|a, b| a.started_at.cmp(&b.started_at));
    let Some(attempt) = blocked.first() else {
        return Ok(());
    };

    Err(Error::new(
        ErrorCode::RegistryBlocked,
        format!(
            "target {name} has an unresolved nontransactional migration: {} {}",
            zapadka_core::migration::short_id(attempt.id),
            attempt.slug
        ),
    )
    .with_context("migration_id", attempt.id)
    .with_context("started_at", &attempt.started_at)
    .with_context("started_by", &attempt.session_user_name)
    .with_context("unresolved", blocked.len())
    .with_hint(
        "a statement that cannot be rolled back was started and its outcome was never observed. \
         Look at the database and decide what actually happened, then say so with \
         `zapadka resolve <id> --applied` or `--not-applied`. Zapadka will not guess, because \
         both wrong answers are expensive.",
    ))
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;
    use camino::Utf8PathBuf;
    use std::collections::BTreeMap;
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
            unresolved: BTreeMap::new(),
        };
        let error = require_initialized(&state, "production").unwrap_err();
        assert_eq!(error.code, ErrorCode::RegistryNotInitialized);
        assert!(error.hint().unwrap().contains("deploy"));
    }
}
