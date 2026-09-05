//! Command and event identities across old and current comparison policies.

use super::harness::{Database, Project, Report, database, project};
use super::transition::{legacy_identity, seed_legacy};

fn report(project: &Project, args: &[&str]) -> Report {
    let report = project.report(args);
    super::report_contract::validate(&report.json);
    report
}

fn assert_identity(report: &Report, definition: &str, algorithm: &str) {
    assert_eq!(report.migrations().len(), 1, "{}", report.json);
    assert_eq!(report.migrations()[0]["definition_sha256"], definition);
    assert_eq!(report.migrations()[0]["definition_algorithm"], algorithm);
}

fn assert_event(
    db: &Database,
    report: &Report,
    action: &str,
    outcome: &str,
    definition: &str,
    algorithm: &str,
) {
    let run_id = report.json["run"]["id"].as_str().unwrap();
    assert_eq!(db.scalar(&format!(
        "SELECT action || '|' || outcome || '|' || definition_sha256 || '|' || definition_algorithm FROM zapadka.events WHERE run_id = '{run_id}' AND action = '{action}' AND outcome = '{outcome}'"
    )), format!("{action}|{outcome}|{definition}|{algorithm}"));
    if let Some(scripts) = report.migrations()[0]["scripts"].as_array()
        && let Some(script) = scripts.iter().find(|script| script["role"] == action)
    {
        assert_eq!(db.scalar(&format!(
            "SELECT script_sha256 FROM zapadka.events WHERE run_id = '{run_id}' AND action = '{action}' AND outcome = '{outcome}'"
        )), script["sha256"].as_str().unwrap());
    }
}

#[test]
fn frozen_verify_and_revert_upgrade_and_keep_raw_identities_on_success_and_failure() {
    for version in [1, 2] {
        for action in ["verify", "revert"] {
            let db = database();
            let project = project();
            let id = project.migration_with(
                "orders",
                &[],
                "CREATE TABLE public.orders(id integer);",
                Some("DROP TABLE public.orders;"),
                Some("SELECT count(*) FROM public.orders;"),
            );
            seed_legacy(&db, &project, version, &[id]);
            let (definition, _, _, _) = legacy_identity(&project, id);
            let path = format!("{action}.sql");
            project.rewrite_script(id, &path, "SELECT 1 / 0;");
            let uri = db.uri();
            let args = if action == "verify" {
                vec![action, "--uri", &uri]
            } else {
                vec![action, "orders", "--uri", &uri]
            };
            let failed = report(&project, &args);
            failed.assert_failed(&format!("{action}.failed"), super::exit::EXECUTION);
            assert_identity(&failed, &definition, "raw-v1");
            assert_event(&db, &failed, action, "failed", &definition, "raw-v1");
            assert_eq!(
                db.scalar("SELECT registry_format_version FROM zapadka.meta"),
                "3"
            );
            assert_eq!(
                db.scalar("SELECT definition_algorithm FROM zapadka.applied_migrations"),
                "raw-v1"
            );
            assert!(db.has_relation("public.orders"));
            project.rewrite_script(
                id,
                &path,
                if action == "verify" {
                    "SELECT count(*) FROM public.orders;"
                } else {
                    "DROP TABLE public.orders;"
                },
            );
            let succeeded = report(&project, &args);
            succeeded.assert_success();
            assert_identity(&succeeded, &definition, "raw-v1");
            assert_event(&db, &succeeded, action, "succeeded", &definition, "raw-v1");
            assert_eq!(
                db.scalar(
                    "SELECT definition_sha256 FROM zapadka.events WHERE zapadka_version = '0.5.4'"
                ),
                definition
            );
            assert_eq!(db.has_relation("public.orders"), action == "verify");
        }
    }
}

#[test]
fn a_new_baseline_records_structural_identity_without_executing_sql() {
    let db = database();
    let project = project();
    let id = project.migration(
        "unexecuted",
        &[],
        "CREATE TABLE public.baseline_was_executed(id integer);",
    );
    let (_, bytes, _, _) = legacy_identity(&project, id);
    let baseline = report(
        &project,
        &[
            "baseline",
            "--to",
            "unexecuted",
            "--acknowledge-existing-schema",
            "--uri",
            &db.uri(),
        ],
    );
    baseline.assert_success();
    let definition = db.scalar("SELECT definition_sha256 FROM zapadka.applied_migrations");
    assert_identity(&baseline, &definition, "structural-v1");
    assert_event(
        &db,
        &baseline,
        "baseline",
        "succeeded",
        &definition,
        "structural-v1",
    );
    assert!(!db.has_relation("public.baseline_was_executed"));
    assert_eq!(
        db.scalar("SELECT deploy_sha256 FROM zapadka.applied_migrations"),
        bytes
    );
    assert!(baseline.migrations()[0].get("scripts").is_none());
    project.rewrite_deploy(
        id,
        "-- accepted cosmetic change\nCREATE TABLE public.baseline_was_executed ( id integer );",
    );
    let status = report(&project, &["status", "--uri", &db.uri()]);
    status.assert_success();
    assert_identity(&status, &definition, "structural-v1");
}

#[test]
fn resolving_frozen_v2_attempts_preserves_the_identity_recorded_before_interruption() {
    for applied in [true, false] {
        let db = database();
        let project = project();
        let id = project.nontransactional_migration(
            "interrupted",
            &[],
            "CREATE INDEX CONCURRENTLY unresolved_idx ON public.never_created(id);",
        );
        seed_legacy(&db, &project, 2, &[]);
        let (definition, bytes, depends, _) = legacy_identity(&project, id);
        db.query(&format!("INSERT INTO zapadka.nontransactional_attempts(migration_id,slug,definition_sha256,deploy_sha256,depends,run_id,session_user_name,server_version,zapadka_version) VALUES ('{id}','interrupted','{definition}','{bytes}','{depends}','00000000-0000-0000-0000-000000000099','legacy-actor','18.4','0.5.4')"));
        let blocked = report(&project, &["status", "--uri", &db.uri()]);
        blocked.assert_success();
        assert_identity(&blocked, &definition, "raw-v1");
        assert_eq!(blocked.migrations()[0]["status"], "blocked");
        report(&project, &["rehash", "--uri", &db.uri()])
            .assert_failed("registry.blocked", super::exit::REGISTRY);
        assert_eq!(
            db.scalar("SELECT registry_format_version FROM zapadka.meta"),
            "2"
        );
        let resolved = report(
            &project,
            &[
                "resolve",
                "interrupted",
                if applied {
                    "--applied"
                } else {
                    "--not-applied"
                },
                "--uri",
                &db.uri(),
            ],
        );
        resolved.assert_success();
        assert_identity(&resolved, &definition, "raw-v1");
        assert_event(
            &db,
            &resolved,
            "resolve",
            if applied {
                "asserted_applied"
            } else {
                "asserted_not_applied"
            },
            &definition,
            "raw-v1",
        );
        assert_eq!(
            db.scalar("SELECT count(*) FROM zapadka.nontransactional_attempts"),
            "0"
        );
        assert!(!db.has_relation("public.unresolved_idx"));
        let status = report(&project, &["status", "--uri", &db.uri()]);
        status.assert_success();
        if applied {
            assert_identity(&status, &definition, "raw-v1");
            assert_eq!(
                db.scalar("SELECT deploy_sha256 FROM zapadka.applied_migrations"),
                bytes
            );
        } else {
            assert_eq!(status.migrations()[0]["status"], "pending");
            assert_eq!(
                status.migrations()[0]["definition_algorithm"],
                "structural-v1"
            );
        }
        let converted = report(&project, &["rehash", "--uri", &db.uri()]);
        converted.assert_success();
        assert_eq!(converted.migrations().len(), usize::from(applied));
        if applied {
            assert_eq!(
                converted.migrations()[0]["rehash"]["disposition"],
                "verified"
            );
        }
    }
}

#[test]
fn new_nontransactional_deployment_and_verification_report_structural_event_identities() {
    let db = database();
    let project = project();
    db.query("CREATE TABLE public.index_target(id integer)");
    let id = project.nontransactional_migration(
        "index",
        &[],
        "CREATE INDEX CONCURRENTLY current_idx ON public.index_target(id);",
    );
    project.rewrite_script(id, "verify.sql", "SELECT 1 FROM public.index_target;");
    let deployed = report(&project, &["deploy", "--uri", &db.uri()]);
    deployed.assert_success();
    let definition = db.scalar("SELECT definition_sha256 FROM zapadka.applied_migrations");
    assert_identity(&deployed, &definition, "structural-v1");
    assert_event(
        &db,
        &deployed,
        "deploy",
        "succeeded",
        &definition,
        "structural-v1",
    );
    assert_event(
        &db,
        &deployed,
        "deploy",
        "attempted",
        &definition,
        "structural-v1",
    );
    assert_event(
        &db,
        &deployed,
        "verify",
        "succeeded",
        &definition,
        "structural-v1",
    );
    assert_eq!(
        db.scalar("SELECT count(*) FROM zapadka.nontransactional_attempts"),
        "0"
    );
    project.rewrite_deploy(
        id,
        "-- formatting\nCREATE INDEX CONCURRENTLY current_idx ON public.index_target (id);\n",
    );
    let verified = report(&project, &["verify", "--uri", &db.uri()]);
    verified.assert_success();
    assert_identity(&verified, &definition, "structural-v1");
    assert_event(
        &db,
        &verified,
        "verify",
        "succeeded",
        &definition,
        "structural-v1",
    );
}
