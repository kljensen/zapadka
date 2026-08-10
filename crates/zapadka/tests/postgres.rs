//! Integration tests against a real PostgreSQL 18.
//!
//! Every test here asserts something Zapadka promises that cannot be checked
//! without a server: what commits, what rolls back, what the registry records,
//! and what happens when two runs collide.
//!
//! A test binary has no public API; `pub` here means "visible to this file".
#![allow(unreachable_pub)]

mod harness;

use harness::{database, project};

/// Exit codes, restated here so a change to the mapping fails a test rather
/// than silently changing a scripting contract.
mod exit {
    pub const VALIDATION: i32 = 4;
    pub const HISTORY: i32 = 5;
    pub const LOCK: i32 = 6;
    pub const REGISTRY: i32 = 8;
    pub const EXECUTION: i32 = 9;
}

#[test]
fn deploys_an_empty_project_without_creating_anything_unexpected() {
    let db = database();
    let project = project();

    let report = project.report(&["deploy", "--uri", &db.uri()]);
    report.assert_success();
    assert!(report.migrations().is_empty());

    // The registry is created even with nothing to apply, so a later deploy
    // has somewhere to record itself.
    assert_eq!(
        db.scalar("SELECT count(*) FROM zapadka.applied_migrations"),
        "0"
    );
    assert_eq!(
        db.scalar("SELECT registry_format_version FROM zapadka.meta"),
        "1"
    );
}

#[test]
fn deploys_dependent_migrations_in_graph_order() {
    let db = database();
    let project = project();
    let first = project.migration("create-schema", &[], "CREATE SCHEMA app;");
    let second = project.migration(
        "create-orders",
        &[first],
        "CREATE TABLE app.orders (id bigint PRIMARY KEY);",
    );
    let third = project.migration(
        "add-status",
        &[second],
        "ALTER TABLE app.orders ADD COLUMN status text;",
    );

    let report = project.report(&["deploy", "--uri", &db.uri()]);
    report.assert_success();

    assert_eq!(
        report.slugs_with_status("succeeded"),
        ["create-schema", "create-orders", "add-status"]
    );
    assert!(db.has_relation("app.orders"));
    assert_eq!(
        db.scalar(
            "SELECT string_agg(slug, ',' ORDER BY applied_at) FROM zapadka.applied_migrations"
        ),
        "create-schema,create-orders,add-status"
    );
    let _ = third;
}

#[test]
fn converges_two_graph_heads_deterministically() {
    let db = database();
    let project = project();
    let base = project.migration("base", &[], "CREATE SCHEMA app;");
    // Two independent branches on the same base, then a migration that
    // converges them. Neither branch orders the other.
    let left = project.migration("left", &[base], "CREATE TABLE app.left (i int);");
    let right = project.migration("right", &[base], "CREATE TABLE app.right (i int);");
    project.migration(
        "merge",
        &[left, right],
        "CREATE VIEW app.both AS SELECT i FROM app.left UNION ALL SELECT i FROM app.right;",
    );

    let report = project.report(&["deploy", "--uri", &db.uri()]);
    report.assert_success();

    let order = report.slugs_with_status("succeeded");
    assert_eq!(order[0], "base", "the base must be first");
    assert_eq!(order[3], "merge", "the convergence must be last");
    // The two branches are ordered by creation time, which is the documented
    // tie-break between simultaneously ready migrations.
    assert_eq!(&order[1..3], ["left", "right"]);
}

#[test]
fn a_failing_migration_rolls_back_its_own_sql_and_its_applied_state() {
    let db = database();
    let project = project();
    project.migration("good", &[], "CREATE SCHEMA app;");
    project.migration(
        "bad",
        &[],
        // The table is created and then the statement after it fails, so a
        // rollback is the only thing that can leave the schema clean.
        "CREATE TABLE public.half_built (i int);\nINSERT INTO public.missing VALUES (1);",
    );

    let report = project.report(&["deploy", "--uri", &db.uri()]);
    report.assert_failed("deploy.failed", exit::EXECUTION);
    assert_eq!(report.sqlstate(), "42P01", "undefined_table");

    // Neither the user's SQL nor its applied-state row survived.
    assert!(!db.has_relation("public.half_built"));
    assert_eq!(
        db.scalar("SELECT count(*) FROM zapadka.applied_migrations WHERE slug = 'bad'"),
        "0"
    );
    // The failure is still recorded, with the diagnostics needed to explain it.
    assert_eq!(
        db.scalar(
            "SELECT sqlstate FROM zapadka.events WHERE outcome = 'failed' AND action = 'deploy'"
        ),
        "42P01"
    );
}

#[test]
fn a_failure_stops_later_migrations_but_keeps_earlier_ones() {
    let db = database();
    let project = project();
    let first = project.migration("first", &[], "CREATE SCHEMA app;");
    let second = project.migration("second", &[first], "INSERT INTO app.missing VALUES (1);");
    project.migration("third", &[second], "CREATE TABLE app.third (i int);");

    let report = project.report(&["deploy", "--uri", &db.uri()]);
    report.assert_failed("deploy.failed", exit::EXECUTION);

    assert_eq!(report.slugs_with_status("succeeded"), ["first"]);
    assert_eq!(report.slugs_with_status("failed"), ["second"]);
    // The migration that never ran is reported, not omitted: a report has to
    // account for everything the run selected.
    assert_eq!(report.slugs_with_status("skipped"), ["third"]);
    assert!(!db.has_relation("app.third"));
}

#[test]
fn verification_runs_after_commit_and_always_rolls_back() {
    let db = database();
    let project = project();
    project.migration_with(
        "create-orders",
        &[],
        "CREATE TABLE public.orders (id bigint PRIMARY KEY);",
        None,
        // Verification observes the committed table, and its own write is
        // discarded whatever the outcome.
        Some(
            "CREATE TABLE public.verification_side_effect (i int);\n\
             SELECT 1 FROM public.orders;",
        ),
    );

    let report = project.report(&["deploy", "--uri", &db.uri()]);
    report.assert_success();

    assert!(db.has_relation("public.orders"), "the migration committed");
    assert!(
        !db.has_relation("public.verification_side_effect"),
        "verification must not be able to leave anything behind"
    );
    assert_eq!(
        db.scalar(
            "SELECT count(*) FROM zapadka.events WHERE action = 'verify' AND outcome = 'succeeded'"
        ),
        "1"
    );
}

#[test]
fn a_failed_verification_leaves_the_migration_applied_and_never_reverts() {
    let db = database();
    let project = project();
    project.migration_with(
        "create-orders",
        &[],
        "CREATE TABLE public.orders (id bigint PRIMARY KEY);",
        None,
        Some("SELECT 1 / 0;"),
    );

    let report = project.report(&["deploy", "--uri", &db.uri()]);
    report.assert_failed("verify.failed", exit::EXECUTION);
    assert_eq!(report.sqlstate(), "22012", "division_by_zero");

    // This is the behaviour ADR-0002 chose deliberately: the migration
    // committed, so it stays committed and stays recorded. Reverting it
    // automatically would run unproven SQL against an unexpected schema while
    // nobody is watching.
    assert!(db.has_relation("public.orders"));
    assert_eq!(
        db.scalar("SELECT count(*) FROM zapadka.applied_migrations WHERE slug = 'create-orders'"),
        "1"
    );
    assert_eq!(
        db.scalar("SELECT outcome FROM zapadka.events WHERE action = 'verify'"),
        "failed"
    );
}

#[test]
fn no_verify_skips_verification_entirely() {
    let db = database();
    let project = project();
    project.migration_with(
        "create-orders",
        &[],
        "CREATE TABLE public.orders (id bigint PRIMARY KEY);",
        None,
        Some("SELECT 1 / 0;"),
    );

    project
        .report(&["deploy", "--uri", &db.uri(), "--no-verify"])
        .assert_success();

    assert!(db.has_relation("public.orders"));
    assert_eq!(
        db.scalar("SELECT count(*) FROM zapadka.events WHERE action = 'verify'"),
        "0"
    );
}

#[test]
fn a_migration_without_verification_is_not_reported_as_verified() {
    let db = database();
    let project = project();
    project.migration(
        "create-orders",
        &[],
        "CREATE TABLE public.orders (id bigint);",
    );
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    // Standalone verify reports it skipped, not passed: a migration with no
    // verification script has not been verified.
    let report = project.report(&["verify", "--uri", &db.uri()]);
    report.assert_success();
    assert_eq!(report.slugs_with_status("skipped"), ["create-orders"]);
}

#[test]
fn editing_a_deployed_migration_is_a_hard_history_failure() {
    let db = database();
    let project = project();
    let id = project.migration(
        "create-orders",
        &[],
        "CREATE TABLE public.orders (id bigint);",
    );
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    project.rewrite_deploy(id, "CREATE TABLE public.orders (id bigint, extra text);");

    for command in ["status", "deploy"] {
        let report = project.report(&[command, "--uri", &db.uri()]);
        report.assert_failed("history.definition_changed", exit::HISTORY);
        // The report names both hashes so the discrepancy can be investigated
        // without guessing.
        assert_ne!(report.error_context("deployed_deploy_sha256"), "");
        assert_ne!(report.error_context("current_deploy_sha256"), "");
    }
}

#[test]
fn deleting_a_deployed_migration_is_a_hard_history_failure() {
    let db = database();
    let project = project();
    let id = project.migration(
        "create-orders",
        &[],
        "CREATE TABLE public.orders (id bigint);",
    );
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    project.delete_migration(id);

    project
        .report(&["status", "--uri", &db.uri()])
        .assert_failed("history.migration_missing", exit::HISTORY);
}

#[test]
fn changing_a_deployed_migrations_dependencies_is_reported_specifically() {
    let db = database();
    let project = project();
    let first = project.migration("first", &[], "CREATE SCHEMA app;");
    let second = project.migration("second", &[], "CREATE SCHEMA other;");
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    // Rewire the second migration to depend on the first, after both deployed.
    let manifest = project.migration_dir(second).join("migration.toml");
    let text = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(
        &manifest,
        text.replace("depends = []", &format!("depends = [\"{first}\"]")),
    )
    .unwrap();

    let report = project.report(&["status", "--uri", &db.uri()]);
    report.assert_failed("history.dependencies_changed", exit::HISTORY);
    assert_eq!(report.error_context("deployed_dependencies"), "none");
}

#[test]
fn a_dry_run_reports_the_plan_and_changes_nothing() {
    let db = database();
    let project = project();
    let first = project.migration("first", &[], "CREATE SCHEMA app;");
    project.migration("second", &[first], "CREATE TABLE app.orders (id bigint);");

    let report = project.report(&["deploy", "--uri", &db.uri(), "--dry-run"]);
    report.assert_success();
    assert_eq!(report.slugs_with_status("pending"), ["first", "second"]);

    // No user SQL and no registry rows. The registry schema itself is not
    // created either, because a dry run must be safe on a database nobody has
    // deployed to yet.
    assert!(!db.has_relation("app.orders"));
    assert_eq!(
        db.scalar("SELECT count(*) FROM pg_namespace WHERE nspname = 'zapadka'"),
        "0"
    );
}

#[test]
fn status_reports_applied_and_pending_without_modifying_anything() {
    let db = database();
    let project = project();
    let first = project.migration("first", &[], "CREATE SCHEMA app;");
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    project.migration("second", &[first], "CREATE TABLE app.orders (id bigint);");

    let before = db.scalar("SELECT count(*) FROM zapadka.events");
    let report = project.report(&["status", "--uri", &db.uri()]);
    report.assert_success();

    assert_eq!(report.slugs_with_status("applied"), ["first"]);
    assert_eq!(report.slugs_with_status("pending"), ["second"]);
    assert_eq!(
        db.scalar("SELECT count(*) FROM zapadka.events"),
        before,
        "status is read-only and must not write events"
    );
}

#[test]
fn status_refuses_to_create_a_registry() {
    let db = database();
    let project = project();
    project.migration("first", &[], "CREATE SCHEMA app;");

    let report = project.report(&["status", "--uri", &db.uri()]);
    report.assert_success();
    assert_eq!(report.slugs_with_status("pending"), ["first"]);
    assert_eq!(
        db.scalar("SELECT count(*) FROM pg_namespace WHERE nspname = 'zapadka'"),
        "0"
    );
}

#[test]
fn verify_requires_a_registry_rather_than_creating_one() {
    let db = database();
    let project = project();
    project.migration("first", &[], "CREATE SCHEMA app;");

    project
        .report(&["verify", "--uri", &db.uri()])
        .assert_failed("registry.not_initialized", exit::REGISTRY);
}

#[test]
fn deploying_twice_is_a_no_op() {
    let db = database();
    let project = project();
    project.migration("first", &[], "CREATE SCHEMA app;");

    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();
    let second = project.report(&["deploy", "--uri", &db.uri()]);
    second.assert_success();

    assert!(second.migrations().is_empty(), "nothing left to do");
    assert_eq!(
        db.scalar("SELECT count(*) FROM zapadka.applied_migrations"),
        "1"
    );
}

#[test]
fn a_second_deployer_cannot_run_while_the_lock_is_held() {
    let db = database();
    let project = project();
    project.migration("first", &[], "CREATE SCHEMA app;");
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    // Hold the project's advisory lock from an unrelated session, exactly as a
    // concurrent deployer would.
    let holder = harness::hold_deployment_lock(&db, project.root());

    let report = project.report(&["deploy", "--uri", &db.uri(), "--wait", "1s"]);
    report.assert_failed("lock.unavailable", exit::LOCK);
    // The diagnostic names who is holding it, which is what makes it possible
    // to decide between waiting and investigating.
    assert_ne!(report.error_context("holder_pid"), "");

    drop(holder);

    // Once released, the same command succeeds.
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();
}

#[test]
fn a_project_cannot_deploy_to_another_projects_database() {
    let db = database();
    let first = project();
    first.migration("first", &[], "CREATE SCHEMA app;");
    first
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    // A different project, with its own identity, pointed at the same database.
    let second = project();
    second.migration("other", &[], "CREATE SCHEMA other;");

    second
        .report(&["deploy", "--uri", &db.uri()])
        .assert_failed("registry.project_mismatch", exit::REGISTRY);
}

#[test]
fn invalid_sql_is_rejected_before_zapadka_connects() {
    let project = project();
    project.migration("bad", &[], "CREATE TABLE t (i int);\nCOMMIT;");

    // A URI that cannot possibly connect: validation has to happen first, so
    // this must fail on the transaction control rather than on the connection.
    let report = project.report(&["deploy", "--uri", "postgresql://nobody@127.0.0.1:1/nothing"]);
    report.assert_failed("script.transaction_control", exit::VALIDATION);
}

#[test]
fn the_registry_history_cannot_be_rewritten() {
    let db = database();
    let project = project();
    project.migration("first", &[], "CREATE SCHEMA app;");
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    // The append-only guarantee is enforced by the database, not merely by
    // Zapadka declining to issue the statement.
    for statement in [
        "UPDATE zapadka.events SET outcome = 'succeeded'",
        "DELETE FROM zapadka.events",
    ] {
        let result = harness::try_sql(&db, statement);
        assert!(
            result.is_err(),
            "{statement} should have been refused by the registry"
        );
    }
}

#[test]
fn the_json_report_is_stable_enough_to_compare_between_runs() {
    let db = database();
    let project = project();
    project.migration_with(
        "create-orders",
        &[],
        "CREATE TABLE public.orders (id bigint PRIMARY KEY);",
        None,
        Some("SELECT 1 FROM public.orders;"),
    );

    let report = project.report(&["deploy", "--uri", &db.uri()]);
    report.assert_success();

    let redacted = report.redacted();
    // The parts a consumer depends on are present and named as documented.
    assert_eq!(redacted["report_version"], 1);
    assert_eq!(redacted["tool"]["name"], "zapadka");
    assert_eq!(redacted["outcome"], "success");
    assert_eq!(redacted["exit_code"], 0);

    let migration = &redacted["migrations"][0];
    assert_eq!(migration["action"], "deploy");
    assert_eq!(migration["status"], "succeeded");
    assert_eq!(migration["transaction"], "required");
    assert_eq!(migration["scripts"][0]["role"], "deploy");
    assert_eq!(migration["scripts"][1]["role"], "verify");
    // Hashes are deliberately not redacted: a hash changing is a behaviour
    // change, not noise.
    assert_eq!(
        migration["definition_sha256"]
            .as_str()
            .unwrap_or_default()
            .len(),
        64
    );

    assert!(
        !serde_json::to_string(&redacted)
            .unwrap()
            .contains(db.name()),
        "the report must not carry the temporary database name after redaction"
    );
}

#[test]
fn an_unencrypted_connection_is_reported() {
    let db = database();
    let project = project();

    let report = project.report(&["status", "--uri", &db.uri()]);
    report.assert_success();
    assert!(
        report.diagnostic_codes().contains(&"target.unencrypted"),
        "a connection with no TLS should say so: {:?}",
        report.diagnostic_codes()
    );
}

#[test]
fn reverts_a_leaf_and_removes_it_from_applied_state() {
    let db = database();
    let project = project();
    project.migration_with(
        "create-orders",
        &[],
        "CREATE TABLE public.orders (id bigint PRIMARY KEY);",
        Some("DROP TABLE public.orders;"),
        None,
    );
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();
    assert!(db.has_relation("public.orders"));

    let report = project.report(&["revert", "--uri", &db.uri(), "create-orders"]);
    report.assert_success();

    // The revert SQL and the removal of the applied-state row commit together.
    assert!(!db.has_relation("public.orders"));
    assert_eq!(
        db.scalar("SELECT count(*) FROM zapadka.applied_migrations"),
        "0"
    );
    // The history keeps both facts: it was applied, and it was reverted.
    assert_eq!(
        db.scalar(
            "SELECT count(*) FROM zapadka.events WHERE action = 'revert' AND outcome = 'succeeded'"
        ),
        "1"
    );

    // Having been reverted, it is pending again.
    let status = project.report(&["status", "--uri", &db.uri()]);
    assert_eq!(status.slugs_with_status("pending"), ["create-orders"]);
}

#[test]
fn refuses_to_revert_a_migration_something_applied_depends_on() {
    let db = database();
    let project = project();
    let base = project.migration_with(
        "base",
        &[],
        "CREATE TABLE public.orders (id bigint PRIMARY KEY);",
        Some("DROP TABLE public.orders;"),
        None,
    );
    project.migration_with(
        "dependent",
        &[base],
        "ALTER TABLE public.orders ADD COLUMN status text;",
        Some("ALTER TABLE public.orders DROP COLUMN status;"),
        None,
    );
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    // Reverting the base would strand the dependent on a schema its own
    // migration no longer describes.
    let report = project.report(&["revert", "--uri", &db.uri(), "base"]);
    report.assert_failed("migration.reversibility_invalid", exit::VALIDATION);
    assert!(report.error_context("dependents").contains("dependent"));
    assert!(db.has_relation("public.orders"), "nothing was reverted");

    // Reverting the leaf first makes the base revertable, one at a time.
    project
        .report(&["revert", "--uri", &db.uri(), "dependent"])
        .assert_success();
    project
        .report(&["revert", "--uri", &db.uri(), "base"])
        .assert_success();
    assert!(!db.has_relation("public.orders"));
}

#[test]
fn refuses_to_revert_a_migration_declared_irreversible() {
    let db = database();
    let project = project();
    // `migration` without a revert script is declared irreversible.
    project.migration("drop-legacy", &[], "CREATE TABLE public.t (i int);");
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    let report = project.report(&["revert", "--uri", &db.uri(), "drop-legacy"]);
    report.assert_failed("migration.reversibility_invalid", exit::VALIDATION);
    assert!(!report.error_context("reason").is_empty());
}

#[test]
fn a_failing_revert_leaves_the_migration_applied() {
    let db = database();
    let project = project();
    project.migration_with(
        "create-orders",
        &[],
        "CREATE TABLE public.orders (id bigint PRIMARY KEY);",
        Some("DROP TABLE public.does_not_exist;"),
        None,
    );
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    let report = project.report(&["revert", "--uri", &db.uri(), "create-orders"]);
    report.assert_failed("revert.failed", exit::EXECUTION);

    // The revert transaction rolled back, so the migration is still applied and
    // its table is still there. Zapadka does not half-revert.
    assert!(db.has_relation("public.orders"));
    assert_eq!(
        db.scalar("SELECT count(*) FROM zapadka.applied_migrations"),
        "1"
    );
}

#[test]
fn baseline_records_a_closure_without_running_any_sql() {
    let db = database();
    let project = project();
    let base = project.migration("base", &[], "CREATE TABLE public.will_not_run (i int);");
    let tip = project.migration(
        "tip",
        &[base],
        "CREATE TABLE public.also_will_not_run (i int);",
    );
    // On another branch, so not part of the tip's closure.
    project.migration(
        "unrelated",
        &[base],
        "CREATE TABLE public.unrelated (i int);",
    );

    let report = project.report(&[
        "baseline",
        "--uri",
        &db.uri(),
        "--to",
        "tip",
        "--acknowledge-existing-schema",
    ]);
    report.assert_success();

    // Recorded as applied...
    assert_eq!(
        db.scalar("SELECT string_agg(slug, ',' ORDER BY slug) FROM zapadka.applied_migrations"),
        "base,tip"
    );
    // ...but none of their SQL ran.
    assert!(!db.has_relation("public.will_not_run"));
    assert!(!db.has_relation("public.also_will_not_run"));
    // The unrelated branch is not part of the closure and stays pending.
    let status = project.report(&["status", "--uri", &db.uri()]);
    assert_eq!(status.slugs_with_status("pending"), ["unrelated"]);
    let _ = tip;
}

#[test]
fn baseline_requires_the_operator_to_state_the_claim() {
    let db = database();
    let project = project();
    project.migration("base", &[], "CREATE TABLE public.t (i int);");

    // Zapadka cannot verify that a schema matches, so the claim has to be made
    // explicitly rather than implied by running the command.
    let report = project.report(&["baseline", "--uri", &db.uri(), "--to", "base"]);
    report.assert_failed("config.invalid", 3);
    assert_eq!(
        db.scalar("SELECT count(*) FROM pg_namespace WHERE nspname = 'zapadka'"),
        "0"
    );
}

#[test]
fn a_baselined_project_deploys_only_what_follows_it() {
    let db = database();
    let project = project();
    let base = project.migration("base", &[], "CREATE TABLE public.pre_existing (i int);");
    project.migration(
        "next",
        &[base],
        "CREATE TABLE public.genuinely_new (i int);",
    );

    // Pretend the base schema is already there, as it would be for a project
    // adopting Zapadka.
    db.query("CREATE TABLE public.pre_existing (i int)");
    project
        .report(&[
            "baseline",
            "--uri",
            &db.uri(),
            "--to",
            "base",
            "--acknowledge-existing-schema",
        ])
        .assert_success();

    let report = project.report(&["deploy", "--uri", &db.uri()]);
    report.assert_success();
    assert_eq!(report.slugs_with_status("succeeded"), ["next"]);
    assert!(db.has_relation("public.genuinely_new"));
}
