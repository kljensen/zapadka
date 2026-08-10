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
use zapadka_core::report::{Action, Status};
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
        if let Some(migration) = graph.get(*id) {
            session
                .migrations
                .push(result_of(migration, Action::Plan, Status::Pending));
        }
    }

    Ok(())
}
