//! `zapadka lint` — validate a project without touching a database.
//!
//! Everything `lint` checks is checked again by `deploy` before it connects.
//! `lint` exists so that the check runs in review, on a laptop, with no
//! credentials — the point in the process where a mistake is cheapest to fix.

use zapadka_core::config::Policy;
use zapadka_core::error::Result;
use zapadka_core::graph::Graph;
use zapadka_core::lint::{Capabilities, Findings, apply_policy, check};
use zapadka_core::report::{Diagnostic, Location, Severity};

use crate::session::Session;

/// Runs `zapadka lint`.
pub fn run(graph: &Graph, policy: &Policy, session: &mut Session) -> Result<()> {
    let findings = analyze(graph, policy, crate::commands::CAPABILITIES);
    session.diagnose_all(findings.diagnostics.clone());
    report_unknown_denials(policy, session);

    match findings.first_error() {
        Some(error) => Err(error.clone()),
        None => Ok(()),
    }
}

/// Lints the graph and applies project policy.
///
/// Shared with `deploy`, which must reach exactly the same verdict: a project
/// that lints clean and then fails validation at deploy time would make `lint`
/// worthless.
pub fn analyze(graph: &Graph, policy: &Policy, capabilities: Capabilities) -> Findings {
    let migrations: Vec<_> = graph.deployment_order().into_iter().cloned().collect();
    let mut findings = check(&migrations, policy, capabilities);
    apply_policy(&mut findings);
    findings
}

/// Warns about `policy.deny` entries that name no real rule.
///
/// A misspelled code silently denies nothing, which is the worst possible
/// outcome for a setting whose entire purpose is to make a risk non-negotiable.
fn report_unknown_denials(policy: &Policy, session: &mut Session) {
    for denied in &policy.deny {
        if zapadka_core::lint::codes::ALL.contains(&denied.as_str()) {
            continue;
        }
        session.diagnose(Diagnostic {
            severity: Severity::Warning,
            code: "policy.unknown_lint".to_owned(),
            message: format!("policy.deny lists {denied:?}, which is not a lint rule"),
            migration_id: None,
            location: Some(Location::file(zapadka_core::config::CONFIG_FILE_NAME)),
            hint: Some(format!(
                "this denial has no effect; known rules are {}",
                zapadka_core::lint::codes::ALL.join(", ")
            )),
        });
    }
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;
    use crate::commands::load_project;
    use crate::testing::{temp_project, write_migration};
    use zapadka_core::error::ErrorCode;

    fn lint(project: &camino::Utf8Path) -> (Result<()>, Session) {
        let (config, graph) = load_project(project).unwrap();
        let mut session = Session::new("lint");
        let result = run(&graph, &config.config.policy, &mut session);
        (result, session)
    }

    #[test]
    fn an_empty_project_lints_clean() {
        let project = temp_project();
        let (result, session) = lint(project.path());
        assert!(result.is_ok());
        assert!(session.diagnostics.is_empty());
    }

    #[test]
    fn a_well_formed_migration_lints_clean() {
        let project = temp_project();
        write_migration(
            project.path(),
            "add-orders",
            &[],
            "CREATE TABLE app.orders (id bigint PRIMARY KEY);",
        );
        let (result, session) = lint(project.path());
        assert!(result.is_ok(), "{:?}", result.unwrap_err());
        assert_eq!(session.warning_count(), 0);
    }

    #[test]
    fn transaction_control_fails_the_command() {
        let project = temp_project();
        write_migration(
            project.path(),
            "bad",
            &[],
            "CREATE TABLE t (i int);\nCOMMIT;",
        );
        let (result, _) = lint(project.path());
        assert_eq!(
            result.unwrap_err().code,
            ErrorCode::ScriptTransactionControl
        );
    }

    #[test]
    fn a_risky_but_valid_migration_warns_without_failing() {
        let project = temp_project();
        write_migration(project.path(), "drop-legacy", &[], "DROP TABLE legacy;");
        let (result, session) = lint(project.path());
        assert!(result.is_ok(), "a warning must not fail lint");
        assert_eq!(session.warning_count(), 1);
        assert_eq!(
            session.diagnostics[0].code,
            zapadka_core::lint::codes::DESTRUCTIVE
        );
    }

    #[test]
    fn warnings_are_reported_even_when_another_migration_fails() {
        // A report that hid the warnings behind the first error would make the
        // fix-and-rerun loop longer than it needs to be.
        let project = temp_project();
        write_migration(project.path(), "risky", &[], "DROP TABLE legacy;");
        write_migration(project.path(), "broken", &[], "COMMIT;");
        let (result, session) = lint(project.path());
        assert!(result.is_err());
        assert_eq!(session.warning_count(), 1);
    }

    #[test]
    fn a_misspelled_policy_denial_is_reported_rather_than_silently_ignored() {
        let project = temp_project();
        let config = project.path().join("zapadka.toml");
        let text = std::fs::read_to_string(&config).unwrap();
        std::fs::write(
            &config,
            text.replace("[policy]", "[policy]\ndeny = [\"lint.destrutive\"]"),
        )
        .unwrap();

        let (result, session) = lint(project.path());
        assert!(result.is_ok());
        let warning = session
            .diagnostics
            .iter()
            .find(|d| d.code == "policy.unknown_lint")
            .expect("a denial that matches no rule must be reported");
        assert_eq!(warning.severity, Severity::Warning);
    }

    #[test]
    fn lint_and_deploy_reach_the_same_verdict() {
        // `analyze` is the single implementation both use; this pins that the
        // command does not add checks of its own.
        let project = temp_project();
        write_migration(project.path(), "bad", &[], "SAVEPOINT s;");
        let (config, graph) = load_project(project.path()).unwrap();

        let direct = analyze(&graph, &config.config.policy, crate::commands::CAPABILITIES);
        let (via_command, _) = lint(project.path());

        assert_eq!(
            direct.first_error().unwrap().code,
            via_command.unwrap_err().code
        );
    }
}
