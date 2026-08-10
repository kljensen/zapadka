//! `zapadka status` — compare the project with what a target has applied.
//!
//! Read-only, and deliberately so: it takes no lock, creates nothing, and
//! upgrades nothing. Someone checking on a database during an incident should
//! not be able to change it by accident, and `status` should work for a role
//! that has only read access.
//!
//! It does still run the history integrity check, because "these migrations are
//! applied" is not a useful answer if the source of those migrations has since
//! been edited.

use zapadka_core::config::LoadedConfig;
use zapadka_core::error::Result;
use zapadka_core::graph::Graph;
use zapadka_core::report::{
    Action, Diagnostic, MigrationResult, Severity, Status, TransactionMode,
};
use zapadka_pg::history;

use crate::cli::TargetArgs;
use crate::commands::{deploy::result_of, target};
use crate::session::Session;

/// Runs `zapadka status`.
pub async fn run(
    config: &LoadedConfig,
    graph: &Graph,
    args: &TargetArgs,
    session: &mut Session,
) -> Result<()> {
    let opened = target::open(config, args, session).await?;

    // Reported, not refused. `status` is how someone finds out a target is
    // blocked, so failing here would hide the answer behind the problem. The
    // commands that would *act* on this target are the ones that stop.
    for attempt in opened.state.unresolved.values() {
        session.diagnose(Diagnostic {
            severity: Severity::Warning,
            code: "target.blocked".to_owned(),
            message: format!(
                "{} {} was started at {} and its outcome was never recorded",
                zapadka_core::migration::short_id(attempt.id),
                attempt.slug,
                attempt.started_at
            ),
            migration_id: Some(attempt.id),
            location: None,
            hint: Some(format!(
                "deploys to this target are blocked. Look at the database, then record what you \
                 found with `zapadka resolve {} --applied` or `--not-applied`.",
                zapadka_core::migration::short_id(attempt.id)
            )),
        });
    }

    // An uninitialized registry is a normal state to report, not a failure:
    // "nothing is applied yet" is exactly what someone is asking about.
    let plan = history::plan(graph, &opened.state.applied)?;

    for id in &plan.applied {
        if let Some(migration) = graph.get(*id) {
            session
                .migrations
                .push(result_of(migration, Action::Plan, Status::Applied));
        }
    }
    for id in &plan.pending {
        // An unresolved migration is absent from `applied_migrations`, so the
        // plan classifies it as pending. It is not: pending means "not applied,
        // and a deploy would apply it", and neither half is true here. It is
        // listed as blocked below instead.
        if opened.state.unresolved.contains_key(id) {
            continue;
        }
        if let Some(migration) = graph.get(*id) {
            session
                .migrations
                .push(result_of(migration, Action::Plan, Status::Pending));
        }
    }

    // Built from the attempt rather than the graph, so a migration deleted from
    // the checkout since the interrupted run is still reported. That case is
    // not hypothetical: someone whose deploy died may well `git checkout` back
    // to the previous revision before working out what happened.
    for attempt in opened.state.unresolved.values() {
        session.migrations.push(MigrationResult {
            id: attempt.id,
            slug: attempt.slug.clone(),
            action: Action::Plan,
            status: Status::Blocked,
            transaction: TransactionMode::Forbidden,
            definition_sha256: attempt.definition_sha256.clone(),
            scripts: Vec::new(),
            duration_ms: None,
            error: None,
        });
    }

    Ok(())
}
