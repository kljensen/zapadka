//! `zapadka resolve` — record what happened to an interrupted statement.
//!
//! # Why a command exists for this at all
//!
//! Everything else Zapadka records, it observed. This records what a person
//! says, and it is the only place that happens.
//!
//! The need comes from a fact about PostgreSQL rather than a choice Zapadka
//! made. A `CREATE INDEX CONCURRENTLY` cannot run inside a transaction, so its
//! SQL and the record that it ran cannot commit together. If the connection
//! dies while it is running, the statement may still complete on the server
//! afterwards. Nothing the client can do afterwards distinguishes "it finished"
//! from "it did not" in general — the index may exist and be valid, exist and
//! be invalid, or not exist at all, and which of those is *correct* depends on
//! what the migration was trying to do.
//!
//! So Zapadka stops, and asks. The alternative designs are worse:
//!
//! - **Retry automatically.** Re-running the statement can fail on a name that
//!   already exists, and cleaning up the leftover first means dropping an
//!   object without knowing whether something else now depends on it.
//! - **Assume it failed.** The next deploy runs against a schema that already
//!   has the change, and the error surfaces somewhere unrelated.
//! - **Assume it worked.** Zapadka records a migration as applied that never
//!   ran, which is the one lie the whole tool exists to prevent.
//!
//! # Why the assertion is audited
//!
//! The operator's claim is written to the append-only history with their role,
//! the server, and the exact hashes involved — the same evidence a deploy
//! leaves. A later reader can tell the difference between a migration Zapadka
//! watched succeed and one a person vouched for, which is precisely the
//! distinction that matters when the schema turns out to be wrong.

use zapadka_core::config::LoadedConfig;
use zapadka_core::error::{Error, ErrorCode, Result};
use zapadka_core::graph::Graph;
use zapadka_core::migration::short_id;
use zapadka_core::report::{Action, MigrationResult, Status};
use zapadka_pg::registry::UnresolvedAttempt;
use zapadka_pg::{execute::Runner, lock};

use crate::cli::ResolveArgs;
use crate::commands::target;
use crate::session::Session;

/// Runs `zapadka resolve`.
pub async fn run(
    config: &LoadedConfig,
    graph: &Graph,
    args: &ResolveArgs,
    session: &mut Session,
) -> Result<()> {
    // Neither flag means the operator has not actually made a claim. Defaulting
    // to either one would be Zapadka guessing, which is the entire thing this
    // command exists to avoid.
    if !args.applied && !args.not_applied {
        return Err(Error::new(
            ErrorCode::NothingToResolve,
            "resolve needs to be told what happened",
        )
        .with_hint(
            "look at the database first, then say what you found: --applied if the statement took \
             effect, --not-applied if it did not. There is no default, because a wrong guess here \
             is recorded as fact.",
        ));
    }

    let opened = target::open(config, &args.target, session).await?;
    let name = opened.name.clone();
    let schema = opened.schema.clone();
    let wait = args
        .wait
        .unwrap_or(config.config.policy.advisory_lock_timeout);

    let client = opened.connection.client;
    let held = lock::acquire(&client, config.config.project.id, wait).await?;

    let (client, result) = resolve_under_lock(
        config,
        graph,
        args,
        session,
        client,
        &name,
        &schema,
        opened.timeouts,
        opened.facts,
    )
    .await;

    let released = held.release(&client).await;
    result.and(released)
}

/// The body of a resolve, with the deployment lock held.
#[allow(clippy::too_many_arguments)]
async fn resolve_under_lock(
    config: &LoadedConfig,
    graph: &Graph,
    args: &ResolveArgs,
    session: &mut Session,
    client: zapadka_pg::Client,
    name: &str,
    schema: &str,
    timeouts: zapadka_pg::Timeouts,
    facts: zapadka_pg::ServerFacts,
) -> (zapadka_pg::Client, Result<()>) {
    // Read under the lock. A resolve races a deploy by definition -- the deploy
    // that is blocked is often running in the next terminal window.
    let state = match target::refresh_state(&client, config, schema).await {
        Ok(state) => state,
        Err(error) => return (client, Err(error)),
    };
    if let Err(error) = target::require_initialized(&state, name) {
        return (client, Err(error));
    }

    let attempt = match select(&state.unresolved, &args.migration, graph) {
        Ok(attempt) => attempt.clone(),
        Err(error) => return (client, Err(error)),
    };

    let mut runner = Runner::new(
        client,
        schema.to_owned(),
        session.run_id,
        facts,
        crate::session::VERSION.to_owned(),
        timeouts,
    );

    let outcome = runner.resolve(&attempt, args.applied).await;
    if outcome.is_ok() {
        session.migrations.push(MigrationResult {
            status: if args.applied {
                Status::Succeeded
            } else {
                // Nothing was applied and nothing was undone. The migration is
                // pending again, which is what `Skipped` means everywhere else
                // in a report: selected, not applied.
                Status::Skipped
            },
            ..result_of(&attempt)
        });
        session.diagnose(zapadka_core::report::Diagnostic {
            severity: zapadka_core::report::Severity::Warning,
            code: "resolve.asserted_by_operator".to_owned(),
            message: format!(
                "{} {} was recorded as {} because an operator said so, not because Zapadka \
                 observed it",
                short_id(attempt.id),
                attempt.slug,
                if args.applied {
                    "applied"
                } else {
                    "not applied"
                }
            ),
            migration_id: Some(attempt.id),
            location: None,
            hint: Some(
                "the assertion is in the append-only history with the role that made it. If the \
                 schema later disagrees with the record, this event is where to start."
                    .to_owned(),
            ),
        });
    }

    (runner.into_client(), outcome)
}

/// Finds the unresolved attempt a selector names.
///
/// Deliberately searches the *attempts*, not the project. The registry knows
/// what was actually started, and a checkout that has moved on since is exactly
/// the situation where someone needs this command.
fn select<'a>(
    unresolved: &'a std::collections::BTreeMap<uuid::Uuid, UnresolvedAttempt>,
    selector: &str,
    graph: &Graph,
) -> Result<&'a UnresolvedAttempt> {
    let matches: Vec<&UnresolvedAttempt> = unresolved
        .values()
        .filter(|attempt| attempt.id.to_string().starts_with(selector) || attempt.slug == selector)
        .collect();

    match matches.as_slice() {
        [attempt] => Ok(attempt),
        [] => Err(nothing_to_resolve(unresolved, selector, graph)),
        many => Err(Error::new(
            ErrorCode::NothingToResolve,
            format!("{selector} matches {} unresolved migrations", many.len()),
        )
        .with_context(
            "matches",
            many.iter()
                .map(|attempt| short_id(attempt.id))
                .collect::<Vec<_>>()
                .join(", "),
        )
        .with_hint("name one of them by its full id")),
    }
}

/// The error for a selector that matches no unresolved attempt.
fn nothing_to_resolve(
    unresolved: &std::collections::BTreeMap<uuid::Uuid, UnresolvedAttempt>,
    selector: &str,
    graph: &Graph,
) -> Error {
    let error = Error::new(
        ErrorCode::NothingToResolve,
        if unresolved.is_empty() {
            "this target has no unresolved nontransactional migrations".to_owned()
        } else {
            format!("{selector} does not name an unresolved nontransactional migration")
        },
    );

    if unresolved.is_empty() {
        // The common confusion: a failed deploy that the server refused is not
        // an unresolved one, and there is nothing here to fix.
        return error.with_hint(
            "nothing is blocked, so there is nothing to resolve. A deploy that failed with an \
             error from the server needs no resolution: the statement did not take effect.",
        );
    }

    let waiting: Vec<String> = unresolved
        .values()
        .map(|attempt| format!("{} {}", short_id(attempt.id), attempt.slug))
        .collect();
    let known = graph.migrations().any(|migration| {
        migration.id.to_string().starts_with(selector) || migration.slug == selector
    });
    error
        .with_context("unresolved", waiting.join(", "))
        .with_hint(if known {
            "that migration exists in the project but is not the one that was interrupted; the \
             unresolved migrations are listed above"
        } else {
            "name one of the unresolved migrations listed above"
        })
}

/// The report entry for a resolved attempt.
fn result_of(attempt: &UnresolvedAttempt) -> MigrationResult {
    MigrationResult {
        id: attempt.id,
        slug: attempt.slug.clone(),
        action: Action::Resolve,
        status: Status::Succeeded,
        transaction: zapadka_core::report::TransactionMode::Forbidden,
        definition_sha256: attempt.definition_sha256.clone(),
        scripts: Vec::new(),
        duration_ms: None,
        error: None,
    }
}
