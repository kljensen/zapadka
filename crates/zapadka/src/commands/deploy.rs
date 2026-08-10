//! `zapadka deploy` — apply pending migrations.
//!
//! The order of operations is the design:
//!
//! 1. Validate everything locally. A project that cannot deploy should find out
//!    before it connects to production.
//! 2. Connect, check the server version, take the deployment lock.
//! 3. Compare deployed history with the checked-out project. Any disagreement
//!    stops the run before a single migration is applied.
//! 4. Apply each pending migration in deterministic order, verifying each one
//!    after it commits.
//! 5. Release the lock on every path.
//!
//! A failure at step 4 stops the run. Migrations already committed stay
//! committed and stay recorded, because they did in fact happen. Zapadka never
//! reverts automatically; see ADR-0002.

use zapadka_core::config::LoadedConfig;
use zapadka_core::error::{Error, Result};
use zapadka_core::graph::Graph;
use zapadka_core::migration::Migration;
use zapadka_core::report::{Action, MigrationResult, Script, Status};
use zapadka_pg::execute::Runner;
use zapadka_pg::history;
use zapadka_pg::{ScriptOutcome, lock, registry};

use crate::cli::DeployArgs;
use crate::commands::target;
use crate::session::Session;

/// Runs `zapadka deploy`.
pub async fn run(
    config: &LoadedConfig,
    graph: &Graph,
    args: &DeployArgs,
    session: &mut Session,
) -> Result<()> {
    // Local validation first, so an invalid project never reaches a database.
    let findings =
        crate::commands::lint::analyze(graph, &config.config.policy, crate::commands::CAPABILITIES);
    session.diagnose_all(findings.diagnostics.clone());
    if let Some(error) = findings.first_error() {
        return Err(error.clone());
    }

    let opened = target::open(config, &args.target, session).await?;
    let project_id = config.config.project.id;
    let wait = args
        .wait
        .unwrap_or(config.config.policy.advisory_lock_timeout);

    let client = opened.connection.client;
    let held = lock::acquire(&client, project_id, wait).await?;

    // Everything from here runs under the lock. The client comes back on every
    // path, including failure, so the lock is always released on the same
    // session that took it.
    let (client, result) = deploy_under_lock(
        config,
        graph,
        args,
        session,
        client,
        config,
        &opened.schema,
        opened.timeouts,
        opened.facts,
    )
    .await;

    let released = held.release(&client).await;
    result.and(released)
}

/// The body of a deploy, with the lock held.
///
/// Returns the connection alongside the outcome so the caller can release the
/// lock whatever happened.
#[allow(clippy::too_many_arguments)]
async fn deploy_under_lock(
    config: &LoadedConfig,
    graph: &Graph,
    args: &DeployArgs,
    session: &mut Session,
    mut client: zapadka_pg::Client,
    loaded: &LoadedConfig,
    schema: &str,
    timeouts: zapadka_pg::Timeouts,
    facts: zapadka_pg::ServerFacts,
) -> (zapadka_pg::Client, Result<()>) {
    // Read again now the lock is held. The state gathered while connecting is a
    // snapshot of a database another run may have been changing.
    let state = match target::refresh_state(&client, loaded, schema).await {
        Ok(state) => state,
        Err(error) => return (client, Err(error)),
    };

    // The registry is created or upgraded under the lock, so two binaries can
    // never race to upgrade it.
    if !args.dry_run
        && let Err(error) = registry::upgrade(
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

    let plan = match history::plan(graph, &state.applied) {
        Ok(plan) => plan,
        Err(error) => return (client, Err(error)),
    };

    if args.dry_run {
        // A plan preview. It has connected, validated, checked history, and
        // computed the exact order — but it runs no user SQL, so it says
        // nothing about how long the migrations take, what locks they need, or
        // what they do to the data.
        for id in &plan.pending {
            if let Some(migration) = graph.get(*id) {
                session.migrations.push(planned(migration));
            }
        }
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

    let outcome = apply_all(&plan, graph, args, session, &mut runner).await;
    (runner.into_client(), outcome)
}

/// Applies every pending migration in order.
async fn apply_all(
    plan: &history::Plan,
    graph: &Graph,
    args: &DeployArgs,
    session: &mut Session,
    runner: &mut Runner,
) -> Result<()> {
    let mut failure: Option<Error> = None;

    for id in &plan.pending {
        let Some(migration) = graph.get(*id) else {
            continue;
        };

        // Once anything has failed, the rest are reported as skipped rather
        // than silently omitted: a report must account for every migration the
        // run selected.
        if failure.is_some() {
            session.migrations.push(MigrationResult {
                status: Status::Skipped,
                ..planned(migration)
            });
            continue;
        }

        match runner.deploy(migration).await {
            Ok(deployed) => {
                let mut result = result_of(migration, Action::Deploy, Status::Succeeded);
                result.duration_ms = Some(deployed.duration_ms);
                result.scripts.push(script_of(&deployed, Status::Succeeded));

                if args.should_verify() {
                    // Verification runs after the commit, so it observes
                    // exactly what a later reader would see.
                    match runner.verify(migration).await {
                        Ok(Some(verified)) => {
                            result.scripts.push(script_of(&verified, Status::Succeeded));
                        }
                        Ok(None) => {}
                        Err(error) => {
                            // The migration stays applied. It committed, and
                            // pretending otherwise would make the report lie.
                            result.status = Status::Succeeded;
                            result.error = Some((&error).into());
                            session.migrations.push(result);
                            failure = Some(error);
                            continue;
                        }
                    }
                }

                session.migrations.push(result);
            }
            Err(error) => {
                let mut result = result_of(migration, Action::Deploy, Status::Failed);
                result.error = Some((&error).into());
                result.scripts.push(Script {
                    role: zapadka_core::report::ScriptRole::Deploy,
                    path: migration.deploy.relative_path.clone(),
                    sha256: migration.deploy.sha256.clone(),
                    status: Status::Failed,
                    duration_ms: None,
                    error: Some((&error).into()),
                });
                session.migrations.push(result);
                failure = Some(error);
            }
        }
    }

    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// The report entry for a migration a dry run would apply.
fn planned(migration: &Migration) -> MigrationResult {
    result_of(migration, Action::Plan, Status::Pending)
}

/// Builds a report entry for a migration.
pub fn result_of(migration: &Migration, action: Action, status: Status) -> MigrationResult {
    MigrationResult {
        id: migration.id,
        slug: migration.slug.clone(),
        action,
        status,
        transaction: migration.manifest.transaction.to_report(),
        definition_sha256: migration.definition_sha256.clone(),
        scripts: Vec::new(),
        duration_ms: None,
        error: None,
    }
}

/// Builds a report entry for an executed script.
pub fn script_of(outcome: &ScriptOutcome, status: Status) -> Script {
    Script {
        role: outcome.role,
        path: outcome.path.clone(),
        sha256: outcome.sha256.clone(),
        status,
        duration_ms: Some(outcome.duration_ms),
        error: None,
    }
}
