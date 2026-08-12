//! Property tests for the boundaries where inputs are adversarial or infinite.
//!
//! Used sparingly, and only where an example-based test cannot cover the shape
//! of the input space:
//!
//! - The **TAP parser** reads text produced by a database, which may be
//!   truncated, interleaved, or malformed in ways nobody thought to write down.
//!   The property that matters is that it always terminates with a classified
//!   answer, never a panic.
//! - The **dependency graph** decides the order migrations run in. Its
//!   invariants — dependencies first, and the same plan every time — must hold
//!   for every graph, not for the four in the unit tests.
//!
//! Everything else in Zapadka is tested by example, because the interesting
//! cases are specific rather than general and a generator would only obscure
//! them.

use proptest::prelude::*;
use uuid::Uuid;
use zapadka_core::duration::Timeout;
use zapadka_core::graph::Graph;
use zapadka_core::manifest::Manifest;
use zapadka_core::migration::{Migration, Script};
use zapadka_core::report::ScriptRole;

/// Builds an id whose UUIDv7 ordering matches `n`.
fn id(n: u8) -> Uuid {
    Uuid::parse_str(&format!("0198f5c0-0000-7000-8000-0000000000{n:02x}")).unwrap()
}

/// Builds a migration depending on the given indices.
fn migration(n: u8, depends: &[u8]) -> Migration {
    let own_id = id(n);
    let list = depends
        .iter()
        .map(|d| format!("\"{}\"", id(*d)))
        .collect::<Vec<_>>()
        .join(", ");
    let manifest = Manifest::parse(
        &format!(
            "format_version = 1\nid = \"{own_id}\"\ndepends = [{list}]\n\
             reversibility = \"irreversible\"\nirreversible_reason = \"property test\"\n"
        ),
        "migration.toml",
    )
    .unwrap();
    let slug = format!("m{n}");
    let relative_dir = format!("migrations/{own_id}-{slug}");
    Migration {
        id: own_id,
        deploy: Script {
            role: ScriptRole::Deploy,
            path: camino::Utf8PathBuf::from("deploy.sql"),
            relative_path: format!("{relative_dir}/deploy.sql"),
            sha256: "0".repeat(64),
            sql: "SELECT 1".to_owned(),
        },
        revert: None,
        verify: None,
        definition_sha256: "0".repeat(64),
        dir: camino::Utf8PathBuf::from(&relative_dir),
        relative_dir,
        slug,
        manifest,
    }
}

/// Generates an acyclic graph by only ever depending on lower indices.
///
/// This is a total ordering of the *possible* edges, not of the resulting
/// graph: a migration may depend on any subset of its predecessors, including
/// none, so branches and convergences arise naturally.
fn acyclic_graph() -> impl Strategy<Value = Vec<(u8, Vec<u8>)>> {
    (1usize..12).prop_flat_map(|count| {
        let nodes: Vec<BoxedStrategy<(u8, Vec<u8>)>> = (0..count)
            .map(|index| {
                let index = u8::try_from(index).unwrap_or(u8::MAX);
                proptest::collection::vec(0u8..index.max(1), 0..=usize::from(index).min(3))
                    .prop_map(move |mut depends| {
                        depends.retain(|d| *d < index);
                        depends.sort_unstable();
                        depends.dedup();
                        (index, depends)
                    })
                    .boxed()
            })
            .collect();
        nodes
    })
}

proptest! {
    /// Deployment order always places a migration after everything it depends
    /// on. This is the whole promise of the graph.
    #[test]
    fn deployment_order_always_respects_dependencies(spec in acyclic_graph()) {
        let migrations: Vec<Migration> = spec
            .iter()
            .map(|(index, depends)| migration(*index, depends))
            .collect();
        let graph = Graph::build(migrations).unwrap();

        let order: Vec<Uuid> = graph.deployment_order().iter().map(|m| m.id).collect();
        prop_assert_eq!(order.len(), spec.len(), "every migration is in the plan");

        let position = |id: Uuid| order.iter().position(|other| *other == id).unwrap();
        for (index, depends) in &spec {
            for dependency in depends {
                prop_assert!(
                    position(id(*dependency)) < position(id(*index)),
                    "m{} must come before m{}",
                    dependency,
                    index
                );
            }
        }
    }

    /// The same graph always produces the same plan, whatever order the
    /// migrations were discovered in. `--dry-run` is worthless without this.
    #[test]
    fn deployment_order_does_not_depend_on_discovery_order(
        spec in acyclic_graph(),
        seed in 0usize..1000,
    ) {
        let build = |migrations: Vec<Migration>| -> Vec<Uuid> {
            Graph::build(migrations)
                .unwrap()
                .deployment_order()
                .iter()
                .map(|m| m.id)
                .collect()
        };

        let migrations: Vec<Migration> = spec
            .iter()
            .map(|(index, depends)| migration(*index, depends))
            .collect();
        let expected = build(migrations.clone());

        // A deterministic shuffle, so a failure is reproducible from the seed.
        let mut shuffled = migrations;
        let len = shuffled.len();
        for index in 0..len {
            let swap = seed.wrapping_mul(index + 7).wrapping_add(index) % len;
            shuffled.swap(index, swap);
        }

        prop_assert_eq!(build(shuffled), expected);
    }

    /// Every migration is reachable from the graph, and heads are exactly the
    /// migrations nothing depends on.
    #[test]
    fn heads_are_exactly_the_migrations_nothing_depends_on(spec in acyclic_graph()) {
        let migrations: Vec<Migration> = spec
            .iter()
            .map(|(index, depends)| migration(*index, depends))
            .collect();
        let graph = Graph::build(migrations).unwrap();

        let depended_on: std::collections::BTreeSet<Uuid> = spec
            .iter()
            .flat_map(|(_, depends)| depends.iter().map(|d| id(*d)))
            .collect();
        let expected: std::collections::BTreeSet<Uuid> = spec
            .iter()
            .map(|(index, _)| id(*index))
            .filter(|id| !depended_on.contains(id))
            .collect();

        let heads: std::collections::BTreeSet<Uuid> = graph.heads().into_iter().collect();
        prop_assert_eq!(heads, expected);
    }

    /// A dependency closure contains every transitive dependency and nothing
    /// that is not one. `baseline --to` records exactly this set.
    #[test]
    fn a_closure_is_closed_under_dependencies(spec in acyclic_graph()) {
        let migrations: Vec<Migration> = spec
            .iter()
            .map(|(index, depends)| migration(*index, depends))
            .collect();
        let graph = Graph::build(migrations).unwrap();

        for (index, _) in &spec {
            let closure: std::collections::BTreeSet<Uuid> = graph
                .closure_of(id(*index))
                .unwrap()
                .iter()
                .map(|m| m.id)
                .collect();

            prop_assert!(closure.contains(&id(*index)), "a closure contains its target");
            for member in &closure {
                let migration = graph.get(*member).unwrap();
                for dependency in migration.depends() {
                    prop_assert!(
                        closure.contains(dependency),
                        "a closure must contain its members' dependencies"
                    );
                }
            }
        }
    }

    /// Timeouts survive a round trip through their written form, so a value
    /// read from configuration and echoed into a report means the same thing.
    #[test]
    fn timeouts_round_trip_through_their_written_form(milliseconds in 0u64..1_000_000_000) {
        let timeout = Timeout::from_millis(milliseconds);
        let parsed = Timeout::parse(&timeout.to_string()).unwrap();
        prop_assert_eq!(parsed.as_millis(), milliseconds);
    }

    /// Parsing a timeout never panics, whatever the configuration file says.
    #[test]
    fn timeout_parsing_never_panics(text in ".*") {
        let _ = Timeout::parse(&text);
    }
}
