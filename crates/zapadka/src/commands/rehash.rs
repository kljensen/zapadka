//! Change applied comparison identities without executing user SQL.

use zapadka_core::config::LoadedConfig;
use zapadka_core::error::{Error, ErrorCode, Result};
use zapadka_core::graph::Graph;
use zapadka_core::manifest::STRUCTURAL_DEFINITION_ALGORITHM;
use zapadka_core::report::{Action, MigrationResult, RehashResult, Status};
use zapadka_pg::execute::Runner;
use zapadka_pg::lock;
use zapadka_pg::rehash::{self, Disposition, RehashPlan};

use crate::cli::RehashArgs;
use crate::commands::target;
use crate::session::Session;

/// Runs a read-only preview or a locked, atomic transition.
pub async fn run(
    config: &LoadedConfig,
    graph: &Graph,
    args: &RehashArgs,
    session: &mut Session,
) -> Result<()> {
    if args.accept_current
        && args
            .reason
            .as_deref()
            .is_none_or(|reason| reason.trim().is_empty())
    {
        return Err(Error::new(
            ErrorCode::ConfigInvalid,
            "--accept-current requires a nonblank --reason",
        ));
    }
    let opened = target::open(config, &args.target, session).await?;
    if args.dry_run {
        let plan = rehash::plan(graph, &opened.state, args.accept_current)?;
        record(&plan, true, false, args.reason.as_deref(), session);
        return plan.validate();
    }
    let wait = args
        .wait
        .unwrap_or(config.config.policy.advisory_lock_timeout);
    let mut client = opened.connection.client;
    let held = lock::acquire(&client, config.config.project.id, wait).await?;
    // Reread after locking. The Graph holds one immutable capture of source
    // bytes for planning, writing comparison state, and audit evidence.
    let refreshed = target::refresh_state(&mut client, config, &opened.schema).await;
    let mut runner = Runner::new(
        client,
        opened.schema,
        session.run_id,
        opened.facts,
        crate::session::VERSION.to_owned(),
        opened.timeouts,
        opened.connection.server_messages,
    );
    let outcome = async {
        let state = refreshed?;
        // Another converter may have upgraded while this run waited for the
        // lock. Even a resulting no-op must report that refreshed version.
        if let Some(target) = &mut session.target {
            target.registry_format_version = state
                .format_version
                .and_then(|version| u32::try_from(version).ok());
        }
        let plan = rehash::plan(graph, &state, args.accept_current)?;
        if let Err(error) = plan.validate() {
            record(&plan, false, false, args.reason.as_deref(), session);
            return Err(error);
        }
        match runner
            .rehash(graph, &state, args.accept_current, args.reason.as_deref())
            .await
        {
            Ok(committed) => {
                // A successful conversion also committed the registry upgrade.
                // Empty legacy targets are no-ops and keep their old version.
                if !committed.is_empty()
                    && let Some(target) = &mut session.target
                {
                    target.registry_format_version =
                        u32::try_from(zapadka_pg::registry::REGISTRY_FORMAT_VERSION).ok();
                }
                record(&committed, false, true, args.reason.as_deref(), session);
                Ok(())
            }
            Err(error) => {
                record(&plan, false, false, args.reason.as_deref(), session);
                Err(error)
            }
        }
    }
    .await;
    let client = runner.into_client();
    let released = held.release(&client).await;
    outcome.and(released)
}

fn record(
    plan: &RehashPlan,
    dry_run: bool,
    committed: bool,
    reason: Option<&str>,
    session: &mut Session,
) {
    for entry in &plan.entries {
        session
            .migrations
            .push(entry_result(entry, dry_run, committed, reason));
    }
}

fn entry_result(
    entry: &zapadka_pg::rehash::RehashEntry,
    dry_run: bool,
    committed: bool,
    reason: Option<&str>,
) -> MigrationResult {
    let disposition = match entry.disposition {
        Disposition::Verified => "verified",
        Disposition::Accepted => "accepted",
        Disposition::AlreadyCurrent => "already_current",
        Disposition::SourceMismatch => "source_mismatch",
        Disposition::Blocked => "blocked",
    };
    let changed = committed
        && matches!(
            entry.disposition,
            Disposition::Verified | Disposition::Accepted
        );
    let status = if entry.error.is_some() {
        Status::Blocked
    } else if entry.disposition == Disposition::AlreadyCurrent {
        Status::Applied
    } else if changed {
        Status::Succeeded
    } else {
        Status::Skipped
    };
    MigrationResult {
        id: entry.id,
        slug: entry.slug.clone(),
        action: Action::Rehash,
        status,
        transaction: entry.transaction.to_report(),
        definition_sha256: if changed {
            entry
                .new_definition_sha256
                .clone()
                .unwrap_or_else(|| entry.old_definition_sha256.clone())
        } else {
            entry.old_definition_sha256.clone()
        },
        definition_algorithm: Some(if changed {
            STRUCTURAL_DEFINITION_ALGORITHM.to_owned()
        } else {
            entry.old_definition_algorithm.clone()
        }),
        rehash: Some(RehashResult {
            original_deploy_sha256: entry.original_deploy_sha256.clone(),
            current_deploy_sha256: entry.current_deploy_sha256.clone(),
            reason: if entry.disposition == Disposition::Accepted {
                reason.map(str::to_owned)
            } else {
                None
            },
            committed: changed,
            disposition: disposition.to_owned(),
            old_definition_sha256: entry.old_definition_sha256.clone(),
            old_definition_algorithm: entry.old_definition_algorithm.clone(),
            new_definition_sha256: entry.new_definition_sha256.clone(),
            new_definition_algorithm: STRUCTURAL_DEFINITION_ALGORITHM.to_owned(),
            dry_run,
        }),
        scripts: Vec::new(),
        duration_ms: None,
        error: entry.error.as_ref().map(Into::into),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]
    use super::*;
    use zapadka_core::manifest::{LEGACY_DEFINITION_ALGORITHM, Transaction};
    use zapadka_pg::rehash::RehashEntry;

    fn plan(disposition: Disposition) -> RehashPlan {
        RehashPlan {
            entries: vec![RehashEntry {
                id: uuid::Uuid::now_v7(),
                slug: "orders".to_owned(),
                transaction: Transaction::Required,
                disposition,
                old_definition_sha256: "a".repeat(64),
                old_definition_algorithm: LEGACY_DEFINITION_ALGORITHM.to_owned(),
                new_definition_sha256: Some("b".repeat(64)),
                original_deploy_sha256: "c".repeat(64),
                current_deploy_sha256: Some("d".repeat(64)),
                error: None,
            }],
        }
    }

    #[test]
    fn preview_keeps_recorded_identity_and_commit_reports_new_identity() {
        let plan = plan(Disposition::Verified);
        let mut preview = Session::new("rehash");
        record(&plan, true, false, None, &mut preview);
        assert_eq!(preview.migrations[0].definition_sha256, "a".repeat(64));
        assert_eq!(preview.migrations[0].status, Status::Skipped);
        let mut committed = Session::new("rehash");
        record(&plan, false, true, None, &mut committed);
        assert_eq!(committed.migrations[0].definition_sha256, "b".repeat(64));
        assert_eq!(committed.migrations[0].status, Status::Succeeded);
        assert!(committed.migrations[0].scripts.is_empty());
    }

    #[test]
    fn acceptance_is_reported_as_an_assertion() {
        let mut session = Session::new("rehash");
        record(
            &plan(Disposition::Accepted),
            true,
            false,
            Some("reviewed source"),
            &mut session,
        );
        let report = session.finish(None);
        let mut output = Vec::new();
        crate::human::render(&report, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Operator assertion"), "{output}");
        assert!(output.contains("has not been adopted"), "{output}");
        let entry = report.migrations[0].rehash.as_ref().unwrap();
        assert_eq!(entry.reason.as_deref(), Some("reviewed source"));
        assert!(!entry.committed);
        assert_eq!(entry.original_deploy_sha256, "c".repeat(64));
        assert_eq!(
            entry.current_deploy_sha256.as_deref(),
            Some("d".repeat(64).as_str())
        );
        assert!(output.contains("1 accepted"), "{output}");
        assert!(report.to_json().contains("\"disposition\": \"accepted\""));
    }
}
