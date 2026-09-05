//! Planning a one-way transition of applied migration comparison identities.

#[cfg(test)]
#[path = "rehash_tests.rs"]
mod tests;

use uuid::Uuid;
use zapadka_core::error::{Error, ErrorCode, Result};
use zapadka_core::graph::Graph;
use zapadka_core::manifest::Transaction;
use zapadka_core::migration::Migration;

use crate::history;
use crate::registry::{self, AppliedMigration, RegistryState};

/// How an applied migration can participate in the transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// The original exact-byte definition was verified.
    Verified,
    /// The operator accepts the checked-out source without proof of equivalence.
    Accepted,
    /// This structural definition already matches.
    AlreadyCurrent,
    /// Legacy source differs and needs explicit operator acceptance.
    SourceMismatch,
    /// A structural or execution-history invariant prevents transition.
    Blocked,
}

/// The decision and captured comparison evidence for one applied migration.
#[derive(Debug, Clone)]
pub struct RehashEntry {
    /// Applied migration identity.
    pub id: Uuid,
    /// The recorded deployment slug.
    pub slug: String,
    /// The recorded execution mode.
    pub transaction: Transaction,
    /// Classification of this migration.
    pub disposition: Disposition,
    /// Its existing active comparison hash.
    pub old_definition_sha256: String,
    /// Its existing comparison policy.
    pub old_definition_algorithm: String,
    /// The structural identity, if source was available and parseable.
    pub new_definition_sha256: Option<String>,
    /// Exact bytes originally deployed, retained without modification.
    pub original_deploy_sha256: String,
    /// Exact checked-out source bytes used to compute the new definition.
    pub current_deploy_sha256: Option<String>,
    /// Why this entry cannot proceed.
    pub error: Option<Error>,
}

/// A complete batch, computed without registry mutations or migration SQL.
#[derive(Debug, Clone)]
pub struct RehashPlan {
    /// Every applied row, in deterministic UUID order.
    pub entries: Vec<RehashEntry>,
}

impl RehashPlan {
    /// Refuses the entire batch if any row cannot transition.
    pub fn validate(&self) -> Result<()> {
        for entry in &self.entries {
            if let Some(error) = &entry.error {
                return Err(error.clone());
            }
        }
        Ok(())
    }

    /// True when no comparison-state updates are needed.
    pub fn is_empty(&self) -> bool {
        !self.entries.iter().any(|entry| {
            matches!(
                entry.disposition,
                Disposition::Verified | Disposition::Accepted
            )
        })
    }
}

/// Plans conversion of applied history; pending migrations are untouched.
pub fn plan(graph: &Graph, state: &RegistryState, accept_current: bool) -> Result<RehashPlan> {
    if !state.is_initialized() {
        return Err(Error::new(
            ErrorCode::RegistryNotInitialized,
            "rehash requires an initialized target registry",
        )
        .with_hint("deploy or baseline the project before rehashing its applied history"));
    }
    if !state.unresolved.is_empty() {
        return Err(Error::new(
            ErrorCode::RegistryBlocked,
            "unresolved nontransactional attempts prevent rehashing",
        )
        .with_hint("resolve the interrupted deployment before rehashing"));
    }
    let entries = state
        .applied
        .values()
        .map(|record| {
            let mut entry = RehashEntry {
                id: record.id,
                slug: record.slug.clone(),
                transaction: registry::parse_transaction_mode(&record.transaction_mode),
                disposition: Disposition::Blocked,
                old_definition_sha256: record.definition_sha256.clone(),
                old_definition_algorithm: record.definition_algorithm.clone(),
                new_definition_sha256: None,
                original_deploy_sha256: record.deploy_sha256.clone(),
                current_deploy_sha256: None,
                error: None,
            };
            if let Err(error) = classify(&mut entry, graph, state, record, accept_current) {
                entry.error = Some(error);
            }
            entry
        })
        .collect();
    Ok(RehashPlan { entries })
}

fn classify(
    entry: &mut RehashEntry,
    graph: &Graph,
    state: &RegistryState,
    record: &AppliedMigration,
    accept_current: bool,
) -> Result<()> {
    registry::check_definition_algorithm(&record.definition_algorithm)?;
    let migration = graph
        .get(record.id)
        .ok_or_else(|| history::missing(record))?;
    history::check_dependencies_applied(migration, &state.applied)?;
    check_execution_metadata(migration, record)?;
    check_deploy_source(migration)?;
    entry.current_deploy_sha256 = Some(migration.deploy.sha256.clone());
    entry.new_definition_sha256 = Some(migration.structural_definition_sha256()?);
    if record.definition_algorithm == registry::STRUCTURAL_V1 {
        history::check_unchanged(migration, record)?;
        entry.disposition = Disposition::AlreadyCurrent;
    } else if migration
        .manifest
        .definition_sha256(migration.deploy.sql.as_bytes())
        == record.definition_sha256
    {
        entry.disposition = Disposition::Verified;
    } else if accept_current {
        entry.disposition = Disposition::Accepted;
    } else {
        entry.disposition = Disposition::SourceMismatch;
        return Err(Error::new(ErrorCode::HistoryDefinitionChanged, format!("legacy source for {} does not match its recorded definition", migration.label()))
            .with_context("migration_id", migration.id)
            .with_hint("restore the original source, or use rehash --accept-current --reason <reason> to explicitly adopt current SQL without proof that the changes are cosmetic"));
    }
    Ok(())
}

fn check_execution_metadata(migration: &Migration, record: &AppliedMigration) -> Result<()> {
    let mut current = migration.depends().to_vec();
    current.sort();
    let mut recorded = record.depends.clone();
    recorded.sort();
    if current != recorded {
        return Err(Error::new(
            ErrorCode::HistoryDependenciesChanged,
            format!(
                "dependencies of applied migration {} have changed",
                migration.label()
            ),
        ));
    }
    if migration.manifest.transaction.as_str() != record.transaction_mode {
        return Err(Error::new(
            ErrorCode::HistoryDefinitionChanged,
            format!(
                "transaction mode of applied migration {} has changed",
                migration.label()
            ),
        ));
    }
    Ok(())
}

fn check_deploy_source(migration: &Migration) -> Result<()> {
    use zapadka_core::lint::{Capabilities, Findings, check_migration};
    // Mutable verification/revert files do not participate in this transition.
    let mut definition = migration.clone();
    definition.verify = None;
    definition.revert = None;
    let mut findings = Findings::default();
    check_migration(
        &definition,
        &zapadka_core::config::Policy::default(),
        Capabilities::ALL,
        &mut findings,
    );
    match findings.errors.into_iter().next() {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Identity of the operator and run performing an atomic transition.
pub(crate) struct AuditContext<'a> {
    pub schema: &'a str,
    pub run_id: Uuid,
    pub facts: &'a registry::ServerFacts,
    pub zapadka_version: &'a str,
    pub reason: Option<&'a str>,
}

pub(crate) async fn apply(
    transaction: &tokio_postgres::Transaction<'_>,
    plan: &RehashPlan,
    context: &AuditContext<'_>,
) -> Result<()> {
    for entry in &plan.entries {
        if matches!(
            entry.disposition,
            Disposition::Verified | Disposition::Accepted
        ) {
            apply_entry(transaction, entry, context).await?;
        }
    }
    Ok(())
}

async fn apply_entry(
    transaction: &tokio_postgres::Transaction<'_>,
    entry: &RehashEntry,
    context: &AuditContext<'_>,
) -> Result<()> {
    let schema = registry::quote_identifier(context.schema);
    let provenance = if entry.disposition == Disposition::Accepted {
        "operator-accepted"
    } else {
        "verified"
    };
    let reason = if entry.disposition == Disposition::Accepted {
        context.reason
    } else {
        None
    };
    let changed = transaction.execute(
        &format!("UPDATE {schema}.applied_migrations SET definition_sha256 = $1, definition_algorithm = $2 WHERE migration_id = $3 AND definition_sha256 = $4 AND definition_algorithm = $5"),
        &[&entry.new_definition_sha256, &registry::STRUCTURAL_V1, &entry.id, &entry.old_definition_sha256, &entry.old_definition_algorithm],
    ).await.map_err(|error| crate::error::registry_failed(error, "update the migration comparison identity"))?;
    if changed != 1 {
        return Err(Error::new(
            ErrorCode::HistoryDefinitionChanged,
            "registry changed while rehashing; retry under the deployment lock",
        ));
    }
    transaction.execute(
        &format!("INSERT INTO {schema}.rehash_events (run_id, migration_id, old_algorithm, old_definition_sha256, new_algorithm, new_definition_sha256, original_deploy_sha256, current_deploy_sha256, provenance, reason, session_user_name, current_user_name, server_version, zapadka_version) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)"),
        &[&context.run_id, &entry.id, &entry.old_definition_algorithm, &entry.old_definition_sha256, &registry::STRUCTURAL_V1, &entry.new_definition_sha256, &entry.original_deploy_sha256, &entry.current_deploy_sha256, &provenance, &reason, &context.facts.session_user, &context.facts.current_user, &context.facts.server_version, &context.zapadka_version],
    ).await.map_err(|error| crate::error::registry_failed(error, "record the rehash audit evidence"))?;
    Ok(())
}
