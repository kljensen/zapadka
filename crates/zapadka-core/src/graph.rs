//! The migration dependency graph.
//!
//! Migrations form a directed acyclic graph, not a list. A required ordering
//! must be expressed as a dependency edge — filesystem order, creation time,
//! and file naming carry no ordering authority. That is what lets two branches
//! add migrations independently and converge without renumbering anything.
//!
//! # Deterministic order
//!
//! Deployment order is a topological sort. When several migrations are ready at
//! the same time, the tie is broken by UUIDv7 order, which is creation order.
//! The result is that the same graph always produces the same plan, on every
//! machine and in every run — a property `--dry-run` would be worthless
//! without.

use std::collections::{BTreeMap, BTreeSet};

use uuid::Uuid;

use crate::error::{Error, ErrorCode, Result};
use crate::manifest::MANIFEST_FILE_NAME;
use crate::migration::{Migration, short_id};
use crate::report::Location;

/// A validated migration graph.
///
/// Constructing one proves the graph is well formed: every dependency exists
/// and there are no cycles.
#[derive(Debug, Clone)]
pub struct Graph {
    migrations: BTreeMap<Uuid, Migration>,
    /// For each migration, the migrations that depend on it.
    dependents: BTreeMap<Uuid, BTreeSet<Uuid>>,
}

impl Graph {
    /// Validates `migrations` and builds the graph.
    pub fn build(migrations: Vec<Migration>) -> Result<Self> {
        let migrations: BTreeMap<Uuid, Migration> = migrations
            .into_iter()
            .map(|migration| (migration.id, migration))
            .collect();

        let mut dependents: BTreeMap<Uuid, BTreeSet<Uuid>> =
            migrations.keys().map(|id| (*id, BTreeSet::new())).collect();

        for migration in migrations.values() {
            for dependency in migration.depends() {
                let Some(dependents) = dependents.get_mut(dependency) else {
                    return Err(Error::new(
                        ErrorCode::MigrationUnknownDependency,
                        format!(
                            "{} depends on {dependency}, which is not in this project",
                            migration.relative_dir
                        ),
                    )
                    .at(Location::file(format!(
                        "{}/{MANIFEST_FILE_NAME}",
                        migration.relative_dir
                    )))
                    .with_hint(
                        "the dependency was deleted or was never committed; restore it or remove \
                         the edge",
                    ));
                };
                dependents.insert(migration.id);
            }
        }

        let graph = Self {
            migrations,
            dependents,
        };
        graph.check_for_cycles()?;
        Ok(graph)
    }

    /// The number of migrations in the graph.
    pub fn len(&self) -> usize {
        self.migrations.len()
    }

    /// Whether the project has no migrations.
    pub fn is_empty(&self) -> bool {
        self.migrations.is_empty()
    }

    /// Looks up a migration by id.
    pub fn get(&self, id: Uuid) -> Option<&Migration> {
        self.migrations.get(&id)
    }

    /// All migrations, in id order.
    pub fn migrations(&self) -> impl Iterator<Item = &Migration> {
        self.migrations.values()
    }

    /// The migrations nothing else depends on: the current tips of the graph.
    ///
    /// `zapadka new` depends on all of these by default, which is what makes
    /// the common linear workflow produce a linear graph, and what makes a new
    /// migration after a branch merge converge the branches explicitly.
    pub fn heads(&self) -> Vec<Uuid> {
        let mut heads: Vec<Uuid> = self
            .dependents
            .iter()
            .filter(|(_, dependents)| dependents.is_empty())
            .map(|(id, _)| *id)
            .collect();
        heads.sort();
        heads
    }

    /// The migrations that depend directly on `id`.
    pub fn dependents_of(&self, id: Uuid) -> impl Iterator<Item = &Migration> {
        self.dependents
            .get(&id)
            .into_iter()
            .flatten()
            .filter_map(|id| self.migrations.get(id))
    }

    /// Every migration in the deterministic order they must be deployed in.
    pub fn deployment_order(&self) -> Vec<&Migration> {
        self.topological_sort()
            .into_iter()
            .filter_map(|id| self.migrations.get(&id))
            .collect()
    }

    /// `id` together with everything it transitively depends on, in deployment
    /// order.
    ///
    /// This is what `--to <id>` and `baseline --to <id>` select: a dependency
    /// closure, never "everything created before this".
    pub fn closure_of(&self, id: Uuid) -> Result<Vec<&Migration>> {
        let target = self.migrations.get(&id).ok_or_else(|| {
            Error::new(
                ErrorCode::MigrationUnknownDependency,
                format!("no migration {id} in this project"),
            )
        })?;

        let mut included = BTreeSet::new();
        let mut stack = vec![target.id];
        while let Some(current) = stack.pop() {
            if !included.insert(current) {
                continue;
            }
            if let Some(migration) = self.migrations.get(&current) {
                stack.extend(migration.depends().iter().copied());
            }
        }

        Ok(self
            .deployment_order()
            .into_iter()
            .filter(|migration| included.contains(&migration.id))
            .collect())
    }

    /// Kahn's algorithm with a deterministic tie-break.
    ///
    /// Returns every id when the graph is acyclic. When it is not, the returned
    /// vector is short, which [`Graph::check_for_cycles`] detects.
    fn topological_sort(&self) -> Vec<Uuid> {
        let mut remaining: BTreeMap<Uuid, usize> = self
            .migrations
            .iter()
            .map(|(id, migration)| (*id, migration.depends().len()))
            .collect();

        // A sorted set, not a queue: among migrations that are all ready, the
        // smallest UUIDv7 — the one created first — always goes next.
        let mut ready: BTreeSet<Uuid> = remaining
            .iter()
            .filter(|(_, count)| **count == 0)
            .map(|(id, _)| *id)
            .collect();

        let mut order = Vec::with_capacity(self.migrations.len());
        while let Some(&next) = ready.iter().next() {
            ready.remove(&next);
            remaining.remove(&next);
            order.push(next);

            for dependent in self.dependents.get(&next).into_iter().flatten() {
                if let Some(count) = remaining.get_mut(dependent) {
                    *count -= 1;
                    if *count == 0 {
                        ready.insert(*dependent);
                    }
                }
            }
        }
        order
    }

    /// Fails when the graph contains a cycle, naming the migrations in it.
    fn check_for_cycles(&self) -> Result<()> {
        let sorted = self.topological_sort();
        if sorted.len() == self.migrations.len() {
            return Ok(());
        }

        let placed: BTreeSet<Uuid> = sorted.into_iter().collect();
        let stuck: BTreeSet<Uuid> = self
            .migrations
            .keys()
            .copied()
            .filter(|id| !placed.contains(id))
            .collect();

        let cycle = self
            .find_cycle(&stuck)
            .unwrap_or_else(|| stuck.iter().copied().collect());
        let described = cycle
            .iter()
            .map(|id| match self.migrations.get(id) {
                Some(migration) => migration.label(),
                None => short_id(*id),
            })
            .collect::<Vec<_>>()
            .join(" -> ");

        let location = cycle
            .first()
            .and_then(|id| self.migrations.get(id))
            .map(|migration| {
                Location::file(format!("{}/{MANIFEST_FILE_NAME}", migration.relative_dir))
            });

        let mut error = Error::new(
            ErrorCode::GraphCycle,
            format!("dependency cycle: {described} -> {}",
                cycle.first().and_then(|id| self.migrations.get(id)).map_or_else(String::new, Migration::label)),
        )
        .with_hint("remove one of these dependency edges; migrations cannot depend on each other in a loop");
        if let Some(location) = location {
            error = error.at(location);
        }
        Err(error)
    }

    /// Walks the cyclic remainder to recover one concrete cycle to show.
    ///
    /// Naming the actual loop is far more actionable than listing every
    /// migration that could not be ordered.
    fn find_cycle(&self, stuck: &BTreeSet<Uuid>) -> Option<Vec<Uuid>> {
        let start = *stuck.iter().next()?;
        let mut path = Vec::new();
        let mut on_path = BTreeSet::new();
        let mut current = start;

        loop {
            if !on_path.insert(current) {
                // Trim the approach to the loop, leaving only the loop itself.
                let entry = path.iter().position(|id| *id == current)?;
                return Some(path[entry..].to_vec());
            }
            path.push(current);
            let next = self
                .migrations
                .get(&current)?
                .depends()
                .iter()
                .find(|dependency| stuck.contains(dependency))?;
            current = *next;
        }
    }
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;
    use crate::manifest::Manifest;
    use crate::migration::Script;
    use crate::report::ScriptRole;
    use camino::Utf8PathBuf;

    /// Builds an id whose UUIDv7 ordering matches `n`, so tests can reason
    /// about tie-breaks by number.
    fn id(n: u8) -> Uuid {
        Uuid::parse_str(&format!("0198f5c0-0000-7000-8000-0000000000{n:02x}")).unwrap()
    }

    fn migration(n: u8, depends: &[u8]) -> Migration {
        let own_id = id(n);
        let depends_list = depends
            .iter()
            .map(|d| format!("\"{}\"", id(*d)))
            .collect::<Vec<_>>()
            .join(", ");
        let manifest = Manifest::parse(
            &format!("format_version = 1\nid = \"{own_id}\"\ndepends = [{depends_list}]\n"),
            "migration.toml",
        )
        .unwrap();
        let slug = format!("m{n}");
        let relative_dir = format!("migrations/{own_id}-{slug}");
        Migration {
            id: own_id,
            deploy: Script {
                role: ScriptRole::Deploy,
                path: Utf8PathBuf::from("deploy.sql"),
                relative_path: format!("{relative_dir}/deploy.sql"),
                sql: "SELECT 1".to_owned(),
                sha256: "0".repeat(64),
            },
            revert: None,
            verify: None,
            definition_sha256: "0".repeat(64),
            dir: Utf8PathBuf::from(&relative_dir),
            relative_dir,
            slug,
            manifest,
        }
    }

    fn order(graph: &Graph) -> Vec<String> {
        graph
            .deployment_order()
            .into_iter()
            .map(|migration| migration.slug.clone())
            .collect()
    }

    #[test]
    fn orders_a_linear_chain() {
        let graph = Graph::build(vec![
            migration(3, &[2]),
            migration(1, &[]),
            migration(2, &[1]),
        ])
        .unwrap();
        assert_eq!(order(&graph), ["m1", "m2", "m3"]);
    }

    #[test]
    fn respects_dependencies_over_creation_order() {
        // m1 was created first but depends on m3, so it must run last.
        let graph = Graph::build(vec![migration(1, &[3]), migration(3, &[])]).unwrap();
        assert_eq!(order(&graph), ["m3", "m1"]);
    }

    #[test]
    fn breaks_ties_by_creation_order_not_filesystem_order() {
        // m2 and m3 are both ready once m1 lands; the older id goes first.
        let graph = Graph::build(vec![
            migration(3, &[1]),
            migration(2, &[1]),
            migration(1, &[]),
        ])
        .unwrap();
        assert_eq!(order(&graph), ["m1", "m2", "m3"]);
    }

    #[test]
    fn the_same_graph_always_produces_the_same_plan() {
        // Determinism is what makes `--dry-run` a promise rather than a guess.
        let build = |migrations: Vec<Migration>| order(&Graph::build(migrations).unwrap());
        let expected = build(vec![
            migration(1, &[]),
            migration(2, &[1]),
            migration(3, &[1]),
            migration(4, &[2, 3]),
        ]);
        for permutation in [
            vec![
                migration(4, &[2, 3]),
                migration(3, &[1]),
                migration(2, &[1]),
                migration(1, &[]),
            ],
            vec![
                migration(2, &[1]),
                migration(4, &[2, 3]),
                migration(1, &[]),
                migration(3, &[1]),
            ],
            vec![
                migration(3, &[1]),
                migration(1, &[]),
                migration(4, &[2, 3]),
                migration(2, &[1]),
            ],
        ] {
            assert_eq!(build(permutation), expected);
        }
        assert_eq!(expected, ["m1", "m2", "m3", "m4"]);
    }

    #[test]
    fn heads_are_the_migrations_nothing_depends_on() {
        // Two branches from a common base: both tips are heads.
        let graph = Graph::build(vec![
            migration(1, &[]),
            migration(2, &[1]),
            migration(3, &[1]),
        ])
        .unwrap();
        assert_eq!(graph.heads(), vec![id(2), id(3)]);

        // A convergence migration leaves exactly one head again.
        let graph = Graph::build(vec![
            migration(1, &[]),
            migration(2, &[1]),
            migration(3, &[1]),
            migration(4, &[2, 3]),
        ])
        .unwrap();
        assert_eq!(graph.heads(), vec![id(4)]);
    }

    #[test]
    fn an_empty_project_has_no_heads_and_no_plan() {
        let graph = Graph::build(Vec::new()).unwrap();
        assert!(graph.is_empty());
        assert!(graph.heads().is_empty());
        assert!(graph.deployment_order().is_empty());
    }

    #[test]
    fn reports_a_missing_dependency_with_the_migration_that_wants_it() {
        let error = Graph::build(vec![migration(2, &[1])]).unwrap_err();
        assert_eq!(error.code, ErrorCode::MigrationUnknownDependency);
        assert!(
            error.message.contains(&id(1).to_string()),
            "{}",
            error.message
        );
    }

    #[test]
    fn reports_a_cycle_by_naming_the_migrations_in_it() {
        let error = Graph::build(vec![
            migration(1, &[3]),
            migration(2, &[1]),
            migration(3, &[2]),
        ])
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::GraphCycle);
        for slug in ["m1", "m2", "m3"] {
            assert!(error.message.contains(slug), "{}", error.message);
        }
    }

    #[test]
    fn a_cycle_is_reported_without_naming_unrelated_migrations() {
        // m1 is fine; only m2 and m3 are in the loop.
        let error = Graph::build(vec![
            migration(1, &[]),
            migration(2, &[1, 3]),
            migration(3, &[2]),
        ])
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::GraphCycle);
        assert!(!error.message.contains("m1"), "{}", error.message);
    }

    #[test]
    fn a_closure_selects_dependencies_not_everything_older() {
        // m2 is older than m3 but is not a dependency of it, so it is excluded.
        let graph = Graph::build(vec![
            migration(1, &[]),
            migration(2, &[1]),
            migration(3, &[1]),
        ])
        .unwrap();
        let closure: Vec<String> = graph
            .closure_of(id(3))
            .unwrap()
            .into_iter()
            .map(|migration| migration.slug.clone())
            .collect();
        assert_eq!(closure, ["m1", "m3"]);
    }

    #[test]
    fn a_closure_is_returned_in_deployment_order() {
        let graph = Graph::build(vec![
            migration(1, &[]),
            migration(2, &[1]),
            migration(3, &[2]),
        ])
        .unwrap();
        let closure: Vec<String> = graph
            .closure_of(id(3))
            .unwrap()
            .into_iter()
            .map(|migration| migration.slug.clone())
            .collect();
        assert_eq!(closure, ["m1", "m2", "m3"]);
    }
}
