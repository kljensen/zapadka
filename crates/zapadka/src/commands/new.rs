//! `zapadka new` — create a migration package.
//!
//! By default the new migration depends on every current graph head. That is
//! what makes the ordinary case behave the way people expect: work sequentially
//! and you get a linear graph; merge a branch that added migrations and the
//! next `new` explicitly converges them. Neither case requires anyone to think
//! about ordering, and neither renumbers anything.
//!
//! `--depends` exists for the deliberate exception: work that genuinely does
//! not depend on the current tip and should be able to deploy independently.

use camino::Utf8Path;
use uuid::Uuid;
use zapadka_core::error::{Error, ErrorCode, Result, io_error};
use zapadka_core::graph::Graph;
use zapadka_core::manifest::{MANIFEST_FILE_NAME, Manifest, Reversibility};
use zapadka_core::migration::{MIGRATIONS_DIR, normalize_slug, short_id};
use zapadka_core::report::{Diagnostic, Location, Severity};

use crate::cli::NewArgs;
use crate::session::Session;

/// The starting content of a new `deploy.sql`.
const DEPLOY_TEMPLATE: &str = "\
-- Deployed inside a transaction Zapadka opens and commits.
-- Do not write BEGIN, COMMIT, ROLLBACK, or SAVEPOINT here.
";

/// The starting content of a new `revert.sql`.
const REVERT_TEMPLATE: &str = "\
-- Undoes deploy.sql. Runs inside a transaction Zapadka opens and commits.
";

/// Runs `zapadka new`.
pub fn run(root: &Utf8Path, graph: &Graph, args: &NewArgs, session: &mut Session) -> Result<()> {
    let slug = normalize_slug(&args.slug)?;
    let id = Uuid::now_v7();

    let depends = if args.depends.is_empty() {
        graph.heads()
    } else {
        resolve_explicit_dependencies(graph, &args.depends, session)?
    };

    let reversibility = match &args.irreversible {
        Some(reason) if reason.trim().is_empty() => {
            // Checked before the directory is created. Writing the package and
            // then having the next command reject it leaves a project to be
            // repaired by hand.
            return Err(Error::new(
                ErrorCode::MigrationReversibilityInvalid,
                "--irreversible needs a reason",
            )
            .with_hint(
                "explain what makes this migration impossible to undo, such as dropping a column \
                 whose data is not recoverable",
            ));
        }
        Some(_) => Reversibility::Irreversible,
        None => Reversibility::Reversible,
    };

    let dir = root.join(MIGRATIONS_DIR).join(format!("{id}-{slug}"));
    if dir.exists() {
        return Err(Error::new(
            ErrorCode::AlreadyExists,
            format!("{dir} already exists"),
        ));
    }
    std::fs::create_dir_all(&dir).map_err(|e| io_error(&dir, "create", e))?;

    let mut manifest = Manifest::scaffold(id, &depends, reversibility);
    if let Some(reason) = &args.irreversible {
        // Serialized as TOML rather than pasted in with the quotes swapped. A
        // reason containing a newline or a backslash would otherwise produce a
        // manifest that `zapadka new` reports as created and the next `lint`
        // rejects.
        manifest = manifest.replace(
            "\"TODO: explain what makes this impossible to undo\"",
            &toml_string(reason),
        );
    }

    write(&dir.join(MANIFEST_FILE_NAME), &manifest)?;
    write(&dir.join("deploy.sql"), DEPLOY_TEMPLATE)?;
    if reversibility.is_reversible() {
        write(&dir.join("revert.sql"), REVERT_TEMPLATE)?;
    }
    // `verify.sql` is deliberately not created. Verification is opt-in per
    // migration, and an empty file would make every migration look verified.

    let created = dir.strip_prefix(root).unwrap_or(&dir);
    session.diagnose(Diagnostic {
        severity: Severity::Note,
        code: "new.created".to_owned(),
        // The path is carried by `location`; repeating it here would print it
        // twice in human output.
        message: format!("created migration {} {slug}", short_id(id)),
        migration_id: Some(id),
        location: Some(Location::file(created.as_str())),
        hint: Some(match depends.len() {
            0 => "this is the first migration in the project".to_owned(),
            1 => format!("depends on {}", short_id(depends[0])),
            n => format!(
                "depends on {n} graph heads: {}",
                depends
                    .iter()
                    .map(|id| short_id(*id))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }),
    });

    Ok(())
}

/// Resolves `--depends` values, which may be full ids or unambiguous prefixes.
fn resolve_explicit_dependencies(
    graph: &Graph,
    requested: &[String],
    session: &mut Session,
) -> Result<Vec<Uuid>> {
    let mut resolved: Vec<Uuid> = Vec::with_capacity(requested.len());
    for text in requested {
        let id = resolve_one(graph, text)?;
        // The same migration can be named twice -- once by slug and once by id.
        // Keeping both would write a manifest with duplicate edges, which the
        // next command rejects, leaving a project that has to be repaired by
        // hand.
        if !resolved.contains(&id) {
            resolved.push(id);
        }
    }

    // Depending on something that is not a head is legal but unusual: the new
    // migration will not be ordered after the current tip.
    let heads = graph.heads();
    let non_heads: Vec<String> = resolved
        .iter()
        .filter(|id| !heads.contains(id))
        .map(|id| short_id(*id))
        .collect();
    if !non_heads.is_empty() && !heads.is_empty() {
        session.diagnose(Diagnostic {
            severity: Severity::Warning,
            code: "new.depends_below_head".to_owned(),
            message: format!(
                "depends on {} which {} not a current graph head",
                non_heads.join(", "),
                if non_heads.len() == 1 { "is" } else { "are" }
            ),
            migration_id: None,
            location: None,
            hint: Some(
                "this migration will not be ordered after the project's current tip, creating a \
                 second head that a later migration must converge"
                    .to_owned(),
            ),
        });
    }

    Ok(resolved)
}

/// Resolves one id or id prefix to a migration in the graph.
fn resolve_one(graph: &Graph, text: &str) -> Result<Uuid> {
    if let Ok(id) = Uuid::parse_str(text) {
        return match graph.get(id) {
            Some(_) => Ok(id),
            None => Err(Error::new(
                ErrorCode::MigrationUnknownDependency,
                format!("no migration {id} in this project"),
            )),
        };
    }

    let matches: Vec<&zapadka_core::migration::Migration> = graph
        .migrations()
        .filter(|migration| migration.id.to_string().starts_with(text) || migration.slug == text)
        .collect();

    match matches.as_slice() {
        [migration] => Ok(migration.id),
        [] => Err(Error::new(
            ErrorCode::MigrationUnknownDependency,
            format!("no migration matches {text:?}"),
        )
        .with_hint("pass a migration id, an unambiguous id prefix, or a slug")),
        many => Err(Error::new(
            ErrorCode::MigrationUnknownDependency,
            format!("{text:?} matches {} migrations", many.len()),
        )
        .with_hint(format!(
            "use a longer prefix; candidates are {}",
            many.iter()
                .map(|m| m.label())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Renders a value as a TOML basic string.
///
/// Small enough to do by hand, and doing it by hand keeps `new` from depending
/// on a serializer for one field.
fn toml_string(value: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn write(path: &Utf8Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents).map_err(|e| io_error(path, "write", e))
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;
    use crate::commands::load_project;
    use crate::testing::temp_project;

    /// Creates a migration and returns its id.
    fn new_migration(root: &Utf8Path, slug: &str, depends: &[&str]) -> Result<Uuid> {
        let (_, graph) = load_project(root)?;
        let mut session = Session::new("new");
        run(
            root,
            &graph,
            &NewArgs {
                slug: slug.to_owned(),
                depends: depends.iter().map(|d| (*d).to_owned()).collect(),
                irreversible: None,
            },
            &mut session,
        )?;
        // Located by code rather than by position: a warning such as
        // `new.depends_below_head` can precede the creation note.
        Ok(session
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == "new.created")
            .and_then(|diagnostic| diagnostic.migration_id)
            .expect("a successful `new` records the migration it created"))
    }

    #[test]
    fn the_first_migration_depends_on_nothing() {
        let project = temp_project();
        let id = new_migration(project.path(), "add-orders", &[]).unwrap();

        let (_, graph) = load_project(project.path()).unwrap();
        let migration = graph.get(id).unwrap();
        assert!(migration.depends().is_empty());
        assert_eq!(migration.slug, "add-orders");
        assert_eq!(graph.heads(), vec![id]);
    }

    #[test]
    fn what_it_writes_is_a_valid_migration_that_lints_clean() {
        let project = temp_project();
        new_migration(project.path(), "add-orders", &[]).unwrap();

        let (config, graph) = load_project(project.path()).unwrap();
        let findings = zapadka_core::lint::check(
            &graph.migrations().cloned().collect::<Vec<_>>(),
            &config.config.policy,
            crate::commands::CAPABILITIES,
        );
        assert!(!findings.has_errors(), "{:?}", findings.errors);
    }

    #[test]
    fn successive_migrations_form_a_chain_without_anyone_ordering_them() {
        let project = temp_project();
        let first = new_migration(project.path(), "one", &[]).unwrap();
        let second = new_migration(project.path(), "two", &[]).unwrap();
        let third = new_migration(project.path(), "three", &[]).unwrap();

        let (_, graph) = load_project(project.path()).unwrap();
        assert_eq!(graph.get(second).unwrap().depends(), [first]);
        assert_eq!(graph.get(third).unwrap().depends(), [second]);
        assert_eq!(graph.heads(), vec![third]);

        let order: Vec<Uuid> = graph.deployment_order().iter().map(|m| m.id).collect();
        assert_eq!(order, [first, second, third]);
    }

    #[test]
    fn a_new_migration_converges_every_head_a_branch_merge_left_behind() {
        let project = temp_project();
        let base = new_migration(project.path(), "base", &[]).unwrap();
        // Two branches, each adding a migration on top of the same base.
        let left = new_migration(project.path(), "left", &[&base.to_string()]).unwrap();
        let right = new_migration(project.path(), "right", &[&base.to_string()]).unwrap();

        let (_, graph) = load_project(project.path()).unwrap();
        assert_eq!(graph.heads(), vec![left, right]);

        // The next ordinary `new` converges them with no special ceremony.
        let merge = new_migration(project.path(), "merge", &[]).unwrap();
        let (_, graph) = load_project(project.path()).unwrap();
        assert_eq!(graph.get(merge).unwrap().depends(), [left, right]);
        assert_eq!(graph.heads(), vec![merge]);
    }

    #[test]
    fn dependencies_can_be_named_by_prefix_or_slug() {
        let project = temp_project();
        let base = new_migration(project.path(), "base", &[]).unwrap();

        let by_prefix = new_migration(project.path(), "a", &[&base.to_string()[..8]]).unwrap();
        let by_slug = new_migration(project.path(), "b", &["base"]).unwrap();

        let (_, graph) = load_project(project.path()).unwrap();
        assert_eq!(graph.get(by_prefix).unwrap().depends(), [base]);
        assert_eq!(graph.get(by_slug).unwrap().depends(), [base]);
    }

    #[test]
    fn an_unknown_dependency_is_refused_before_anything_is_written() {
        let project = temp_project();
        let error = new_migration(project.path(), "x", &["does-not-exist"]).unwrap_err();
        assert_eq!(error.code, ErrorCode::MigrationUnknownDependency);

        let (_, graph) = load_project(project.path()).unwrap();
        assert!(graph.is_empty(), "nothing should have been created");
    }

    #[test]
    fn depending_below_the_current_head_is_allowed_but_warned_about() {
        let project = temp_project();
        let base = new_migration(project.path(), "base", &[]).unwrap();
        new_migration(project.path(), "tip", &[]).unwrap();

        let (_, graph) = load_project(project.path()).unwrap();
        let mut session = Session::new("new");
        run(
            project.path(),
            &graph,
            &NewArgs {
                slug: "independent".to_owned(),
                depends: vec![base.to_string()],
                irreversible: None,
            },
            &mut session,
        )
        .unwrap();

        assert!(
            session
                .diagnostics
                .iter()
                .any(|d| d.code == "new.depends_below_head" && d.severity == Severity::Warning)
        );
    }

    #[test]
    fn an_irreversible_migration_records_the_reason_and_gets_no_revert_script() {
        let project = temp_project();
        let (_, graph) = load_project(project.path()).unwrap();
        let mut session = Session::new("new");
        run(
            project.path(),
            &graph,
            &NewArgs {
                slug: "drop-legacy".to_owned(),
                depends: Vec::new(),
                irreversible: Some("archived rows are deleted and not recoverable".to_owned()),
            },
            &mut session,
        )
        .unwrap();

        let (_, graph) = load_project(project.path()).unwrap();
        let migration = graph.migrations().next().unwrap();
        assert!(!migration.is_reversible());
        assert!(migration.revert.is_none());
        assert_eq!(
            migration.manifest.irreversible_reason.as_deref(),
            Some("archived rows are deleted and not recoverable")
        );
    }

    #[test]
    fn an_irreversible_reason_containing_awkward_characters_still_parses() {
        // A reason with a newline or a quote used to produce a manifest that
        // `new` reported as created and the next command rejected.
        for reason in [
            "line one\nline two",
            "contains \"quotes\" and a \\ backslash",
            "a tab\there",
        ] {
            let project = temp_project();
            let (_, graph) = load_project(project.path()).unwrap();
            let mut session = Session::new("new");
            run(
                project.path(),
                &graph,
                &NewArgs {
                    slug: "drop-legacy".to_owned(),
                    depends: Vec::new(),
                    irreversible: Some(reason.to_owned()),
                },
                &mut session,
            )
            .unwrap();

            // The written package must load, which is what proves the manifest
            // is well formed rather than merely written.
            let (_, graph) = load_project(project.path()).unwrap_or_else(|error| {
                panic!("{reason:?} produced an unloadable project: {error}")
            });
            let migration = graph.migrations().next().unwrap();
            assert_eq!(
                migration.manifest.irreversible_reason.as_deref(),
                Some(reason)
            );
        }
    }

    #[test]
    fn naming_the_same_dependency_twice_does_not_write_a_broken_manifest() {
        // Once by slug and once by id resolves to the same migration; keeping
        // both would write duplicate edges that the next command rejects.
        let project = temp_project();
        let base = new_migration(project.path(), "base", &[]).unwrap();

        let (_, graph) = load_project(project.path()).unwrap();
        let mut session = Session::new("new");
        run(
            project.path(),
            &graph,
            &NewArgs {
                slug: "next".to_owned(),
                depends: vec!["base".to_owned(), base.to_string()],
                irreversible: None,
            },
            &mut session,
        )
        .unwrap();

        let (_, graph) = load_project(project.path()).expect("the project must still load");
        let created = graph.migrations().find(|m| m.slug == "next").unwrap();
        assert_eq!(created.depends(), [base]);
    }

    #[test]
    fn verification_stays_opt_in() {
        // An empty verify.sql would make every migration look verified.
        let project = temp_project();
        new_migration(project.path(), "add-orders", &[]).unwrap();
        let (_, graph) = load_project(project.path()).unwrap();
        assert!(graph.migrations().next().unwrap().verify.is_none());
    }

    #[test]
    fn slugs_are_normalized_into_the_directory_name() {
        let project = temp_project();
        let id = new_migration(project.path(), "Add Orders Table!", &[]).unwrap();
        assert!(
            project
                .path()
                .join("migrations")
                .join(format!("{id}-add-orders-table"))
                .is_dir()
        );
    }
}
