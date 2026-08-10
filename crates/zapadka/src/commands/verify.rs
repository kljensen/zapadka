//! `zapadka verify` — re-run verification for applied migrations.
//!
//! Same execution contract as verification during a deploy: a fresh
//! runner-owned transaction per migration, always rolled back. The difference
//! is only *when* it runs, which makes it useful for confirming that a database
//! still holds after something else changed it — a manual fix, a restore, a
//! failover.
//!
//! Selecting nothing verifies every applied migration in deployment order. A
//! selector that matches nothing is an error rather than a quiet success,
//! because a typo in a CI script that verifies zero migrations would otherwise
//! pass.

use uuid::Uuid;
use zapadka_core::config::LoadedConfig;
use zapadka_core::error::{Error, ErrorCode, Result};
use zapadka_core::graph::Graph;
use zapadka_core::migration::short_id;
use zapadka_core::report::{Action, Status};
use zapadka_pg::execute::Runner;
use zapadka_pg::{history, lock};

use crate::cli::VerifyArgs;
use crate::commands::{deploy::result_of, deploy::script_of, target};
use crate::session::Session;

/// Runs `zapadka verify`.
pub async fn run(
    config: &LoadedConfig,
    graph: &Graph,
    args: &VerifyArgs,
    session: &mut Session,
) -> Result<()> {
    let opened = target::open(config, &args.target, session).await?;
    target::require_initialized(&opened.state, &opened.name)?;

    let plan = history::plan(graph, &opened.state.applied)?;
    let selected = select(graph, &plan.applied, &args.migrations)?;

    let wait = args
        .wait
        .unwrap_or(config.config.policy.advisory_lock_timeout);
    let client = opened.connection.client;

    // Verification writes events, and an event written while a deploy is midway
    // through would sit in the middle of that deploy's history. The lock keeps
    // the record coherent.
    let held = lock::acquire(&client, config.config.project.id, wait).await?;

    let mut runner = Runner::new(
        client,
        opened.schema.clone(),
        session.run_id,
        opened.facts,
        crate::session::VERSION.to_owned(),
        opened.timeouts,
    );

    let outcome = verify_all(&selected, graph, session, &mut runner).await;

    let client = runner.into_client();
    let released = held.release(&client).await;
    outcome.and(released)
}

/// Verifies each selected migration, stopping at the first failure.
async fn verify_all(
    selected: &[Uuid],
    graph: &Graph,
    session: &mut Session,
    runner: &mut Runner,
) -> Result<()> {
    for id in selected {
        let Some(migration) = graph.get(*id) else {
            continue;
        };

        match runner.verify(migration).await {
            Ok(Some(verified)) => {
                let mut result = result_of(migration, Action::Verify, Status::Succeeded);
                result.duration_ms = Some(verified.duration_ms);
                result.scripts.push(script_of(&verified, Status::Succeeded));
                session.migrations.push(result);
            }
            Ok(None) => {
                // No verify.sql. Reported as skipped rather than passed: a
                // migration with no verification has not been verified.
                session
                    .migrations
                    .push(result_of(migration, Action::Verify, Status::Skipped));
            }
            Err(error) => {
                let mut result = result_of(migration, Action::Verify, Status::Failed);
                result.error = Some((&error).into());
                session.migrations.push(result);
                return Err(error);
            }
        }
    }
    Ok(())
}

/// Resolves the requested selectors against the applied migrations.
fn select(graph: &Graph, applied: &[Uuid], selectors: &[String]) -> Result<Vec<Uuid>> {
    if selectors.is_empty() {
        return Ok(applied.to_vec());
    }

    let mut chosen = Vec::new();
    for selector in selectors {
        let matches: Vec<Uuid> = applied
            .iter()
            .filter(|id| {
                let Some(migration) = graph.get(**id) else {
                    return false;
                };
                migration.id.to_string().starts_with(selector.as_str())
                    || migration.slug == *selector
            })
            .copied()
            .collect();

        match matches.as_slice() {
            [id] => {
                if !chosen.contains(id) {
                    chosen.push(*id);
                }
            }
            [] => {
                return Err(Error::new(
                    ErrorCode::SelectorMatchedNothing,
                    format!("no applied migration matches {selector:?}"),
                )
                .with_hint(
                    "verify acts on migrations already applied to the target; check `zapadka \
                     status` for what is applied",
                ));
            }
            many => {
                return Err(Error::new(
                    ErrorCode::SelectorMatchedNothing,
                    format!("{selector:?} matches {} applied migrations", many.len()),
                )
                .with_hint(format!(
                    "use a longer prefix; candidates are {}",
                    many.iter()
                        .map(|id| short_id(*id))
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }
    }

    // Returned in deployment order regardless of the order they were named, so
    // verification observes the same ordering a deploy would.
    Ok(applied
        .iter()
        .filter(|id| chosen.contains(id))
        .copied()
        .collect())
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;
    use crate::commands::load_project;
    use crate::testing::{temp_project, write_migration};

    #[test]
    fn no_selectors_means_every_applied_migration() {
        let project = temp_project();
        let first = write_migration(project.path(), "one", &[], "SELECT 1;");
        let second = write_migration(project.path(), "two", &[first], "SELECT 1;");
        let (_, graph) = load_project(project.path()).unwrap();

        let applied = vec![first, second];
        assert_eq!(select(&graph, &applied, &[]).unwrap(), applied);
    }

    #[test]
    fn selectors_are_returned_in_deployment_order_not_the_order_given() {
        let project = temp_project();
        let first = write_migration(project.path(), "one", &[], "SELECT 1;");
        let second = write_migration(project.path(), "two", &[first], "SELECT 1;");
        let (_, graph) = load_project(project.path()).unwrap();

        let chosen = select(
            &graph,
            &[first, second],
            &["two".to_owned(), "one".to_owned()],
        )
        .unwrap();
        assert_eq!(chosen, [first, second]);
    }

    #[test]
    fn a_selector_matching_nothing_is_an_error() {
        // A CI script whose selector stopped matching must fail, not silently
        // verify nothing.
        let project = temp_project();
        let first = write_migration(project.path(), "one", &[], "SELECT 1;");
        let (_, graph) = load_project(project.path()).unwrap();

        let error = select(&graph, &[first], &["nope".to_owned()]).unwrap_err();
        assert_eq!(error.code, ErrorCode::SelectorMatchedNothing);
    }

    #[test]
    fn a_migration_that_is_not_applied_cannot_be_selected() {
        let project = temp_project();
        let first = write_migration(project.path(), "one", &[], "SELECT 1;");
        let second = write_migration(project.path(), "two", &[first], "SELECT 1;");
        let (_, graph) = load_project(project.path()).unwrap();

        // Only the first is applied.
        let error = select(&graph, &[first], &["two".to_owned()]).unwrap_err();
        assert_eq!(error.code, ErrorCode::SelectorMatchedNothing);
        assert!(error.message.contains("two"), "{}", error.message);
        let _ = second;
    }

    #[test]
    fn naming_the_same_migration_twice_verifies_it_once() {
        let project = temp_project();
        let first = write_migration(project.path(), "one", &[], "SELECT 1;");
        let (_, graph) = load_project(project.path()).unwrap();

        let chosen = select(&graph, &[first], &["one".to_owned(), first.to_string()]).unwrap();
        assert_eq!(chosen, [first]);
    }
}
