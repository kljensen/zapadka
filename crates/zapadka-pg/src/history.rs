//! Comparing deployed history with the checked-out project.
//!
//! A migration that has been applied is a statement about what was run against
//! a database. If the source of that migration later changes, the statement is
//! no longer true, and nothing Zapadka reports about that database can be
//! trusted — `status` would describe a project that no longer exists.
//!
//! So Zapadka refuses. It does not re-run the changed migration, because the
//! old one already ran. It does not update the recorded hash, because that
//! would erase the evidence. Corrective work is a new migration, which leaves
//! both facts in the history: what was originally deployed, and what fixed it.
//!
//! This is deliberately stricter than "the schema looks right". Zapadka checks
//! that the *source* matches, not that the *result* does, because it cannot
//! know what else depended on the original text.

use std::collections::BTreeMap;

use uuid::Uuid;
use zapadka_core::error::{Error, ErrorCode, Result};
use zapadka_core::graph::Graph;
use zapadka_core::migration::{Migration, short_id};
use zapadka_core::report::Location;

use crate::registry::AppliedMigration;

/// What a run intends to do, having compared the project with the registry.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Migrations to apply, in deterministic deployment order.
    pub pending: Vec<Uuid>,
    /// Migrations already applied, in deployment order.
    pub applied: Vec<Uuid>,
}

impl Plan {
    /// Whether there is nothing to do.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// Checks that every applied migration still matches its source, then computes
/// what remains to be applied.
///
/// The integrity check runs first and covers every applied migration, not only
/// the ones a deploy would touch. A tampered migration deep in the history is
/// exactly the case worth catching, and it would never be touched by a normal
/// deploy.
pub fn plan(graph: &Graph, applied: &BTreeMap<Uuid, AppliedMigration>) -> Result<Plan> {
    for (id, record) in applied {
        let migration = graph.get(*id).ok_or_else(|| missing(record))?;
        check_unchanged(migration, record)?;
    }

    let ordered = graph.deployment_order();
    let pending = ordered
        .iter()
        .filter(|migration| !applied.contains_key(&migration.id))
        .map(|migration| migration.id)
        .collect();
    let already = ordered
        .iter()
        .filter(|migration| applied.contains_key(&migration.id))
        .map(|migration| migration.id)
        .collect();

    Ok(Plan {
        pending,
        applied: already,
    })
}

/// The error for a migration the database has but the project does not.
fn missing(record: &AppliedMigration) -> Error {
    Error::new(
        ErrorCode::HistoryMigrationMissing,
        format!(
            "migration {} {} is applied to the target but is not in this project",
            short_id(record.id),
            record.slug
        ),
    )
    .with_context("migration_id", record.id)
    .with_context("applied_at", &record.applied_at)
    .with_hint(
        "restore the migration, or check out the revision that was deployed; Zapadka cannot \
         describe a database whose history it does not have",
    )
}

/// Fails when a deployed migration's immutable definition has changed.
fn check_unchanged(migration: &Migration, record: &AppliedMigration) -> Result<()> {
    if migration.definition_sha256 == record.definition_sha256 {
        return Ok(());
    }

    let manifest_path = format!("{}/migration.toml", migration.relative_dir);

    // Dependency edges are part of the definition, so a changed edge also
    // changes the hash. Reporting the edge specifically is far more useful than
    // reporting that two hashes differ.
    let mut current = migration.depends().to_vec();
    current.sort();
    let mut recorded = record.depends.clone();
    recorded.sort();
    if current != recorded {
        return Err(Error::new(
            ErrorCode::HistoryDependenciesChanged,
            format!(
                "the dependencies of applied migration {} {} have changed",
                short_id(migration.id),
                migration.slug
            ),
        )
        .at(Location::file(manifest_path))
        .with_context("deployed_dependencies", describe(&recorded))
        .with_context("current_dependencies", describe(&current))
        .with_context("migration_id", migration.id)
        .with_hint(
            "the graph a database was built from cannot be rewritten after the fact; restore the \
             original edges and express the new ordering in a new migration",
        ));
    }

    let mode = migration.manifest.transaction.as_str();
    if mode != record.transaction_mode {
        return Err(Error::new(
            ErrorCode::HistoryDefinitionChanged,
            format!(
                "the transaction mode of applied migration {} {} changed from {} to {mode}",
                short_id(migration.id),
                migration.slug,
                record.transaction_mode
            ),
        )
        .at(Location::file(manifest_path))
        .with_context("migration_id", migration.id)
        .with_hint("how a migration was executed is a fact about the past and cannot be edited"));
    }

    // Everything else in the definition is the deploy script itself.
    let changed_script = migration.deploy.sha256 != record.deploy_sha256;
    let mut error = Error::new(
        ErrorCode::HistoryDefinitionChanged,
        format!(
            "applied migration {} {} has changed since it was deployed",
            short_id(migration.id),
            migration.slug
        ),
    )
    .with_context("migration_id", migration.id)
    .with_context("deployed_definition_sha256", &record.definition_sha256)
    .with_context("current_definition_sha256", &migration.definition_sha256)
    .with_context("applied_at", &record.applied_at);

    if changed_script {
        error = error
            .at(Location::file(&migration.deploy.relative_path))
            .with_context("deployed_deploy_sha256", &record.deploy_sha256)
            .with_context("current_deploy_sha256", &migration.deploy.sha256)
            .with_hint(
                "deploy.sql has been edited since it ran; restore it and write a new migration \
                 with the correction, so the history keeps both facts",
            );
    } else {
        error = error.at(Location::file(manifest_path)).with_hint(
            "the migration's manifest has been edited since it ran; restore it and express the \
             change in a new migration",
        );
    }

    Err(error)
}

/// Renders a dependency list for a diagnostic.
fn describe(ids: &[Uuid]) -> String {
    if ids.is_empty() {
        return "none".to_owned();
    }
    ids.iter()
        .map(|id| short_id(*id))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;
    use camino::Utf8PathBuf;
    use zapadka_core::manifest::Manifest;
    use zapadka_core::migration::Script;
    use zapadka_core::report::ScriptRole;

    fn id(n: u8) -> Uuid {
        Uuid::parse_str(&format!("0198f5c0-0000-7000-8000-0000000000{n:02x}")).unwrap()
    }

    fn migration(n: u8, depends: &[u8], deploy: &str) -> Migration {
        let own_id = id(n);
        let list = depends
            .iter()
            .map(|d| format!("\"{}\"", id(*d)))
            .collect::<Vec<_>>()
            .join(", ");
        let manifest = Manifest::parse(
            &format!(
                "format_version = 1\nid = \"{own_id}\"\ndepends = [{list}]\n\
                 reversibility = \"irreversible\"\nirreversible_reason = \"test\"\n"
            ),
            "migration.toml",
        )
        .unwrap();
        let slug = format!("m{n}");
        let relative_dir = format!("migrations/{own_id}-{slug}");
        let definition_sha256 = manifest.definition_sha256(deploy.as_bytes());
        Migration {
            id: own_id,
            deploy: Script {
                role: ScriptRole::Deploy,
                path: Utf8PathBuf::from("deploy.sql"),
                relative_path: format!("{relative_dir}/deploy.sql"),
                sha256: zapadka_core::manifest::sha256_hex(deploy.as_bytes()),
                sql: deploy.to_owned(),
            },
            revert: None,
            verify: None,
            definition_sha256,
            dir: Utf8PathBuf::from(&relative_dir),
            relative_dir,
            slug,
            manifest,
        }
    }

    /// The registry row a successful deploy of `migration` would have written.
    fn record_of(migration: &Migration) -> AppliedMigration {
        let mut depends = migration.depends().to_vec();
        depends.sort();
        AppliedMigration {
            id: migration.id,
            slug: migration.slug.clone(),
            definition_sha256: migration.definition_sha256.clone(),
            deploy_sha256: migration.deploy.sha256.clone(),
            depends,
            transaction_mode: migration.manifest.transaction.as_str().to_owned(),
            applied_at: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    fn applied(records: Vec<AppliedMigration>) -> BTreeMap<Uuid, AppliedMigration> {
        records
            .into_iter()
            .map(|record| (record.id, record))
            .collect()
    }

    #[test]
    fn an_untouched_history_produces_a_plan_of_what_remains() {
        let first = migration(1, &[], "CREATE TABLE a();");
        let second = migration(2, &[1], "CREATE TABLE b();");
        let graph = Graph::build(vec![first.clone(), second.clone()]).unwrap();

        let plan = plan(&graph, &applied(vec![record_of(&first)])).unwrap();
        assert_eq!(plan.applied, [first.id]);
        assert_eq!(plan.pending, [second.id]);
        assert!(!plan.is_empty());
    }

    #[test]
    fn a_fully_deployed_project_has_nothing_pending() {
        let first = migration(1, &[], "CREATE TABLE a();");
        let graph = Graph::build(vec![first.clone()]).unwrap();
        let plan = plan(&graph, &applied(vec![record_of(&first)])).unwrap();
        assert!(plan.is_empty());
    }

    #[test]
    fn pending_migrations_come_back_in_deployment_order() {
        let first = migration(1, &[], "a");
        let second = migration(2, &[1], "b");
        let third = migration(3, &[2], "c");
        // Built out of order to prove the plan does not inherit input order.
        let graph = Graph::build(vec![third.clone(), first.clone(), second.clone()]).unwrap();
        let plan = plan(&graph, &BTreeMap::new()).unwrap();
        assert_eq!(plan.pending, [first.id, second.id, third.id]);
    }

    #[test]
    fn an_edited_deploy_script_is_a_hard_failure_naming_the_file() {
        let deployed = migration(1, &[], "CREATE TABLE a();");
        let edited = migration(1, &[], "CREATE TABLE a(); -- fixed");
        let graph = Graph::build(vec![edited.clone()]).unwrap();

        let error = plan(&graph, &applied(vec![record_of(&deployed)])).unwrap_err();
        assert_eq!(error.code, ErrorCode::HistoryDefinitionChanged);
        assert_eq!(
            error.location().unwrap().path,
            edited.deploy.relative_path,
            "the report should point at the file to restore"
        );
        assert_eq!(
            error.context().get("deployed_deploy_sha256"),
            Some(&deployed.deploy.sha256)
        );
        assert!(error.hint().unwrap().contains("new migration"));
    }

    #[test]
    fn a_changed_dependency_edge_is_reported_specifically() {
        // The hash alone would say "something changed"; the edge is what the
        // author actually needs to put back.
        let base = migration(1, &[], "a");
        let deployed = migration(2, &[], "b");
        let rewired = migration(2, &[1], "b");
        let graph = Graph::build(vec![base, rewired]).unwrap();

        let error = plan(&graph, &applied(vec![record_of(&deployed)])).unwrap_err();
        assert_eq!(error.code, ErrorCode::HistoryDependenciesChanged);
        assert_eq!(
            error
                .context()
                .get("deployed_dependencies")
                .map(String::as_str),
            Some("none")
        );
        assert_eq!(
            error
                .context()
                .get("current_dependencies")
                .map(String::as_str),
            Some("0198f5c0")
        );
    }

    #[test]
    fn reordering_a_dependency_list_is_not_a_history_change() {
        // The edges are a set. Rewriting the list in a different order is a
        // cosmetic change and must not look like tampering.
        let mut deployed = record_of(&migration(3, &[1, 2], "c"));
        deployed.depends = vec![id(2), id(1)];
        let current = migration(3, &[2, 1], "c");
        let graph = Graph::build(vec![
            migration(1, &[], "a"),
            migration(2, &[], "b"),
            current,
        ])
        .unwrap();

        assert!(plan(&graph, &applied(vec![deployed])).is_ok());
    }

    #[test]
    fn a_deleted_migration_that_is_still_applied_is_a_hard_failure() {
        let deployed = migration(1, &[], "a");
        let graph = Graph::build(Vec::new()).unwrap();

        let error = plan(&graph, &applied(vec![record_of(&deployed)])).unwrap_err();
        assert_eq!(error.code, ErrorCode::HistoryMigrationMissing);
        assert!(error.message.contains("m1"), "{}", error.message);
    }

    #[test]
    fn tampering_deep_in_the_history_is_caught_even_though_a_deploy_would_not_touch_it() {
        let deployed_old = migration(1, &[], "CREATE TABLE a();");
        let edited_old = migration(1, &[], "CREATE TABLE a(id int);");
        let recent = migration(2, &[1], "CREATE TABLE b();");
        let graph = Graph::build(vec![edited_old, recent.clone()]).unwrap();

        // Only migration 2 is pending, but migration 1 is what changed.
        let error = plan(&graph, &applied(vec![record_of(&deployed_old)])).unwrap_err();
        assert_eq!(error.code, ErrorCode::HistoryDefinitionChanged);
        assert!(error.message.contains("m1"), "{}", error.message);
    }

    #[test]
    fn changing_how_a_migration_was_executed_is_reported_as_such() {
        let mut deployed = record_of(&migration(1, &[], "a"));
        deployed.transaction_mode = "forbidden".to_owned();
        // Keep the definition hash mismatched, as it would be in reality.
        deployed.definition_sha256 = "0".repeat(64);
        let graph = Graph::build(vec![migration(1, &[], "a")]).unwrap();

        let error = plan(&graph, &applied(vec![deployed])).unwrap_err();
        assert_eq!(error.code, ErrorCode::HistoryDefinitionChanged);
        assert!(
            error.message.contains("transaction mode"),
            "{}",
            error.message
        );
    }

    #[test]
    fn editing_a_mutable_script_is_not_a_history_change() {
        // verify.sql and revert.sql are deliberately mutable; only the
        // deployment definition is frozen.
        let mut current = migration(1, &[], "CREATE TABLE a();");
        let record = record_of(&current);
        current.verify = Some(Script {
            role: ScriptRole::Verify,
            path: Utf8PathBuf::from("verify.sql"),
            relative_path: "migrations/x/verify.sql".to_owned(),
            sql: "SELECT 1".to_owned(),
            sha256: "1".repeat(64),
        });
        let graph = Graph::build(vec![current]).unwrap();

        assert!(plan(&graph, &applied(vec![record])).is_ok());
    }
}
