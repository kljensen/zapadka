//! `zapadka baseline` — adopt a database that already has the schema.
//!
//! Baselining records a dependency closure as applied **without running any of
//! its SQL**. It is how a project that predates Zapadka, or a database restored
//! from a snapshot, starts being managed.
//!
//! # What it does not do
//!
//! It does not look at the schema, and it does not check that the database
//! actually matches the migrations being recorded. It cannot: two schemas that
//! look alike can differ in ways no query would reveal, and a tool that claimed
//! otherwise would be trusted exactly when it should not be.
//!
//! So baselining is an **assertion by the operator**, not a discovery by
//! Zapadka. That is why it requires `--acknowledge-existing-schema`: the flag
//! exists to make the operator state the claim, so that a later reader of the
//! registry knows a human asserted it rather than a tool inferred it.

use zapadka_core::config::LoadedConfig;
use zapadka_core::error::{Error, ErrorCode, Result};
use zapadka_core::graph::Graph;
use zapadka_core::migration::Migration;
use zapadka_core::report::{Action, Status};
use zapadka_pg::execute::Runner;
use zapadka_pg::{history, lock, registry};

use crate::cli::BaselineArgs;
use crate::commands::{deploy::result_of, target};
use crate::session::Session;

/// Runs `zapadka baseline`.
pub async fn run(
    config: &LoadedConfig,
    graph: &Graph,
    args: &BaselineArgs,
    session: &mut Session,
) -> Result<()> {
    if !args.acknowledge_existing_schema {
        return Err(Error::new(
            ErrorCode::ConfigInvalid,
            "baseline requires --acknowledge-existing-schema",
        )
        .with_hint(
            "baseline records migrations as applied without running them. Zapadka cannot check \
             that the database matches, so the claim has to be yours: pass \
             --acknowledge-existing-schema to state that the schema is already present",
        ));
    }

    let target_migration = resolve(graph, &args.to)?;
    let opened = target::open(config, &args.target, session).await?;

    let wait = args
        .wait
        .unwrap_or(config.config.policy.advisory_lock_timeout);
    let client = opened.connection.client;
    let held = lock::acquire(&client, config.config.project.id, wait).await?;

    // The client comes back on every path, so the lock is always released on
    // the session that took it.
    let (client, outcome) = baseline_under_lock(
        config,
        graph,
        target_migration,
        session,
        client,
        &opened.schema,
        opened.timeouts,
        opened.facts,
    )
    .await;

    let released = held.release(&client).await;
    outcome.and(released)
}

/// The body of a baseline, with the lock held.
///
/// Returns the connection alongside the outcome so the caller can release the
/// lock whatever happened.
#[allow(clippy::too_many_arguments)]
async fn baseline_under_lock(
    config: &LoadedConfig,
    graph: &Graph,
    target_migration: &Migration,
    session: &mut Session,
    mut client: zapadka_pg::Client,
    schema: &str,
    timeouts: zapadka_pg::Timeouts,
    facts: zapadka_pg::ServerFacts,
) -> (zapadka_pg::Client, Result<()>) {
    // Read again now the lock is held. Otherwise a concurrent revert could make
    // a migration pending between the read and the decision, and this run would
    // compute an empty closure and report success while recording nothing.
    let state = match target::refresh_state(&client, config, schema).await {
        Ok(state) => state,
        Err(error) => return (client, Err(error)),
    };

    if let Err(error) = registry::upgrade(
        &mut client,
        schema,
        config.config.project.id,
        crate::session::VERSION,
        &state,
    )
    .await
    {
        return (client, Err(error));
    }

    // Whatever is already applied still has to match its source. Baselining is
    // not a way to paper over a history mismatch.
    if let Err(error) = history::plan(graph, &state.applied) {
        return (client, Err(error));
    }

    // A closure, never "everything created before this". A migration created
    // earlier but on an unrelated branch is not part of what this one needs.
    let closure = match graph.closure_of(target_migration.id) {
        Ok(closure) => closure,
        Err(error) => return (client, Err(error)),
    };
    let pending: Vec<&Migration> = closure
        .into_iter()
        .filter(|migration| !state.applied.contains_key(&migration.id))
        .collect();

    if pending.is_empty() {
        return (client, Ok(()));
    }

    let mut runner = Runner::new(
        client,
        schema.to_owned(),
        session.run_id,
        facts,
        crate::session::VERSION.to_owned(),
        timeouts,
    );

    let result = runner.baseline(&pending).await;
    if result.is_ok() {
        for migration in pending {
            session
                .migrations
                .push(result_of(migration, Action::Baseline, Status::Succeeded));
        }
    }
    (runner.into_client(), result)
}

/// Resolves `--to` to one migration.
fn resolve<'a>(graph: &'a Graph, selector: &str) -> Result<&'a Migration> {
    let matches: Vec<&Migration> = graph
        .migrations()
        .filter(|migration| {
            migration.id.to_string().starts_with(selector) || migration.slug == selector
        })
        .collect();

    match matches.as_slice() {
        [migration] => Ok(migration),
        [] => Err(Error::new(
            ErrorCode::SelectorMatchedNothing,
            format!("no migration matches {selector:?}"),
        )),
        many => Err(Error::new(
            ErrorCode::SelectorMatchedNothing,
            format!("{selector:?} matches {} migrations", many.len()),
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

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;
    use crate::commands::load_project;
    use crate::testing::{temp_project, write_migration};

    #[test]
    fn resolves_a_migration_by_slug_or_prefix() {
        let project = temp_project();
        let id = write_migration(
            project.path(),
            "create-orders",
            &[],
            "CREATE TABLE t (i int);",
        );
        let (_, graph) = load_project(project.path()).unwrap();

        assert_eq!(resolve(&graph, "create-orders").unwrap().id, id);
        assert_eq!(resolve(&graph, &id.to_string()[..8]).unwrap().id, id);
        assert_eq!(
            resolve(&graph, "nope").unwrap_err().code,
            ErrorCode::SelectorMatchedNothing
        );
    }

    #[test]
    fn a_closure_excludes_migrations_that_are_merely_older() {
        // `unrelated` was created before `tip` but is on another branch, so it
        // is not part of what `tip` requires and must not be baselined with it.
        let project = temp_project();
        let base = write_migration(project.path(), "base", &[], "CREATE SCHEMA app;");
        write_migration(
            project.path(),
            "unrelated",
            &[base],
            "CREATE TABLE a (i int);",
        );
        let tip = write_migration(project.path(), "tip", &[base], "CREATE TABLE b (i int);");
        let (_, graph) = load_project(project.path()).unwrap();

        let closure: Vec<&str> = graph
            .closure_of(tip)
            .unwrap()
            .into_iter()
            .map(|migration| migration.slug.as_str())
            .collect();
        assert_eq!(closure, ["base", "tip"]);
    }
}
