//! `zapadka revert` — undo one applied migration.
//!
//! Reverting is deliberately the most restricted thing Zapadka does.
//!
//! It acts on **one** migration, and only one that is a *leaf* of the applied
//! graph — nothing else applied depends on it. It does not cascade, and it does
//! not work out an order for you. Cascading reverts are how a routine
//! correction turns into an outage: the tool picks an order nobody reviewed and
//! runs several revert scripts, each of which was written assuming the schema
//! looked the way it did when its migration was authored.
//!
//! Reverting several migrations means running `revert` several times, in an
//! order the operator chose. That is more typing and considerably more control.

use zapadka_core::config::LoadedConfig;
use zapadka_core::error::{Error, ErrorCode, Result};
use zapadka_core::graph::Graph;
use zapadka_core::migration::Migration;
use zapadka_core::report::{Action, Status};
use zapadka_pg::execute::Runner;
use zapadka_pg::{RegistryState, history, lock};

use crate::cli::RevertArgs;
use crate::commands::{deploy::result_of, deploy::script_of, target};
use crate::session::Session;

/// Runs `zapadka revert`.
pub async fn run(
    config: &LoadedConfig,
    graph: &Graph,
    args: &RevertArgs,
    session: &mut Session,
) -> Result<()> {
    let opened = target::open(config, &args.target, session).await?;
    target::require_initialized(&opened.state, &opened.name)?;

    // History integrity first: reverting a migration whose source has been
    // edited would run a revert script that does not match what was deployed.
    history::plan(graph, &opened.state.applied)?;

    let migration = select(graph, &opened.state, &args.migration)?;
    check_revertible(graph, &opened.state, migration)?;

    let wait = args
        .wait
        .unwrap_or(config.config.policy.advisory_lock_timeout);
    let client = opened.connection.client;
    let held = lock::acquire(&client, config.config.project.id, wait).await?;

    let mut runner = Runner::new(
        client,
        opened.schema.clone(),
        session.run_id,
        opened.facts,
        crate::session::VERSION.to_owned(),
        opened.timeouts,
    );

    let outcome = revert_one(migration, session, &mut runner).await;

    let client = runner.into_client();
    let released = held.release(&client).await;
    outcome.and(released)
}

/// Reverts one migration and records the result.
async fn revert_one(
    migration: &Migration,
    session: &mut Session,
    runner: &mut Runner,
) -> Result<()> {
    match runner.revert(migration).await {
        Ok(reverted) => {
            let mut result = result_of(migration, Action::Revert, Status::Succeeded);
            result.duration_ms = Some(reverted.duration_ms);
            result.scripts.push(script_of(&reverted, Status::Succeeded));
            session.migrations.push(result);
            Ok(())
        }
        Err(error) => {
            let mut result = result_of(migration, Action::Revert, Status::Failed);
            result.error = Some((&error).into());
            session.migrations.push(result);
            Err(error)
        }
    }
}

/// Resolves the selector to exactly one applied migration.
fn select<'a>(graph: &'a Graph, state: &RegistryState, selector: &str) -> Result<&'a Migration> {
    let matches: Vec<&Migration> = graph
        .migrations()
        .filter(|migration| state.applied.contains_key(&migration.id))
        .filter(|migration| {
            migration.id.to_string().starts_with(selector) || migration.slug == selector
        })
        .collect();

    match matches.as_slice() {
        [migration] => Ok(migration),
        [] => Err(Error::new(
            ErrorCode::SelectorMatchedNothing,
            format!("no applied migration matches {selector:?}"),
        )
        .with_hint(
            "revert acts on a migration already applied to the target; `zapadka status` lists them",
        )),
        many => Err(Error::new(
            ErrorCode::SelectorMatchedNothing,
            format!("{selector:?} matches {} applied migrations", many.len()),
        )
        .with_hint(format!(
            "use a longer prefix; candidates are {}",
            many.iter()
                .map(|m| m.label())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Fails unless the migration is a reversible leaf of the applied graph.
fn check_revertible(graph: &Graph, state: &RegistryState, migration: &Migration) -> Result<()> {
    // Anything still applied that depends on this one would be left standing on
    // a schema its own migration no longer describes.
    let dependents: Vec<String> = graph
        .dependents_of(migration.id)
        .filter(|dependent| state.applied.contains_key(&dependent.id))
        .map(Migration::label)
        .collect();

    if !dependents.is_empty() {
        return Err(Error::new(
            ErrorCode::MigrationReversibilityInvalid,
            format!(
                "{} is depended on by {} applied migration(s)",
                migration.label(),
                dependents.len()
            ),
        )
        .with_context("dependents", dependents.join(", "))
        .with_hint(
            "revert acts only on a leaf of the applied graph; revert the migrations that depend \
             on it first, one at a time, in an order you have chosen",
        ));
    }

    if !migration.is_reversible() {
        let reason = migration
            .manifest
            .irreversible_reason
            .as_deref()
            .unwrap_or("no reason recorded");
        return Err(Error::new(
            ErrorCode::MigrationReversibilityInvalid,
            format!("{} is declared irreversible", migration.label()),
        )
        .with_context("reason", reason)
        .with_hint(
            "this migration states that it cannot be undone; correcting it means writing a new \
             migration",
        ));
    }

    let Some(revert) = &migration.revert else {
        return Err(Error::new(
            ErrorCode::MigrationMissingScript,
            format!("{} has no revert.sql", migration.relative_dir),
        ));
    };
    if revert.runs_nothing() {
        return Err(Error::new(
            ErrorCode::ScriptEmpty,
            format!("{} contains no SQL", revert.relative_path),
        )
        .with_hint("an empty revert script would report success while undoing nothing"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;
    use crate::commands::load_project;
    use crate::testing::{temp_project, write_migration, write_migration_with};
    use std::collections::BTreeMap;
    use zapadka_pg::AppliedMigration;

    /// A registry state in which `applied` are the applied migrations.
    fn state(applied: &[&Migration]) -> RegistryState {
        let mut map = BTreeMap::new();
        for migration in applied {
            map.insert(
                migration.id,
                AppliedMigration {
                    id: migration.id,
                    slug: migration.slug.clone(),
                    definition_sha256: migration.definition_sha256.clone(),
                    deploy_sha256: migration.deploy.sha256.clone(),
                    depends: migration.depends().to_vec(),
                    transaction_mode: "required".to_owned(),
                    applied_at: "2026-01-01T00:00:00Z".to_owned(),
                },
            );
        }
        RegistryState {
            format_version: Some(1),
            project_id: None,
            applied: map,
        }
    }

    #[test]
    fn a_reversible_leaf_can_be_reverted() {
        let project = temp_project();
        write_migration_with(
            project.path(),
            "only",
            &[],
            "CREATE TABLE t (i int);",
            Some("DROP TABLE t;"),
            None,
        );
        let (_, graph) = load_project(project.path()).unwrap();
        let migration = graph.migrations().next().unwrap();

        check_revertible(&graph, &state(&[migration]), migration).unwrap();
    }

    #[test]
    fn a_migration_with_applied_dependents_cannot_be_reverted() {
        // Reverting the base would leave the dependent standing on a schema its
        // own migration no longer describes.
        let project = temp_project();
        let base = write_migration_with(
            project.path(),
            "base",
            &[],
            "CREATE TABLE t (i int);",
            Some("DROP TABLE t;"),
            None,
        );
        write_migration_with(
            project.path(),
            "dependent",
            &[base],
            "ALTER TABLE t ADD COLUMN j int;",
            Some("ALTER TABLE t DROP COLUMN j;"),
            None,
        );
        let (_, graph) = load_project(project.path()).unwrap();
        let base_migration = graph.get(base).unwrap();
        let dependent = graph.migrations().find(|m| m.slug == "dependent").unwrap();

        let error = check_revertible(&graph, &state(&[base_migration, dependent]), base_migration)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::MigrationReversibilityInvalid);
        assert!(error.context()["dependents"].contains("dependent"));

        // Once the dependent is reverted, the base becomes a leaf.
        check_revertible(&graph, &state(&[base_migration]), base_migration).unwrap();
    }

    #[test]
    fn an_unapplied_dependent_does_not_block_a_revert() {
        // The dependent exists in the project but was never deployed, so
        // reverting the base cannot strand it.
        let project = temp_project();
        let base = write_migration_with(
            project.path(),
            "base",
            &[],
            "CREATE TABLE t (i int);",
            Some("DROP TABLE t;"),
            None,
        );
        write_migration_with(
            project.path(),
            "dependent",
            &[base],
            "ALTER TABLE t ADD COLUMN j int;",
            Some("ALTER TABLE t DROP COLUMN j;"),
            None,
        );
        let (_, graph) = load_project(project.path()).unwrap();
        let base_migration = graph.get(base).unwrap();

        check_revertible(&graph, &state(&[base_migration]), base_migration).unwrap();
    }

    #[test]
    fn an_irreversible_migration_is_refused_with_its_stated_reason() {
        let project = temp_project();
        write_migration(project.path(), "drop-legacy", &[], "DROP TABLE legacy;");
        let (_, graph) = load_project(project.path()).unwrap();
        let migration = graph.migrations().next().unwrap();

        let error = check_revertible(&graph, &state(&[migration]), migration).unwrap_err();
        assert_eq!(error.code, ErrorCode::MigrationReversibilityInvalid);
        assert!(!error.context()["reason"].is_empty());
    }

    #[test]
    fn an_empty_revert_script_is_refused_rather_than_reported_as_success() {
        let project = temp_project();
        write_migration_with(
            project.path(),
            "only",
            &[],
            "CREATE TABLE t (i int);",
            Some("-- nothing here\n"),
            None,
        );
        let (_, graph) = load_project(project.path()).unwrap();
        let migration = graph.migrations().next().unwrap();

        let error = check_revertible(&graph, &state(&[migration]), migration).unwrap_err();
        assert_eq!(error.code, ErrorCode::ScriptEmpty);
    }

    #[test]
    fn only_applied_migrations_can_be_selected() {
        let project = temp_project();
        let applied = write_migration_with(
            project.path(),
            "applied",
            &[],
            "CREATE TABLE t (i int);",
            Some("DROP TABLE t;"),
            None,
        );
        write_migration_with(
            project.path(),
            "pending",
            &[applied],
            "CREATE TABLE u (i int);",
            Some("DROP TABLE u;"),
            None,
        );
        let (_, graph) = load_project(project.path()).unwrap();
        let state = state(&[graph.get(applied).unwrap()]);

        assert_eq!(select(&graph, &state, "applied").unwrap().id, applied);
        assert_eq!(
            select(&graph, &state, "pending").unwrap_err().code,
            ErrorCode::SelectorMatchedNothing
        );
    }
}
