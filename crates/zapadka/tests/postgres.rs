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
        "CREATE TABLE public.orders (id bigint PRIMARY KEY);
         INSERT INTO public.orders VALUES (1);",
        None,
        // Verification observes the committed table and the row the migration
        // inserted, which it could not see if it ran before the commit.
        Some("SELECT 1 / (SELECT count(*)::int FROM public.orders);"),
    );

    let report = project.report(&["deploy", "--uri", &db.uri()]);
    report.assert_success();

    assert!(db.has_relation("public.orders"), "the migration committed");
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

#[test]
fn runs_database_tests_against_a_prepared_target() {
    let db = database();
    let project = project();
    project.migration(
        "create-orders",
        &[],
        "CREATE SCHEMA app; CREATE TABLE app.orders (id bigint PRIMARY KEY, total numeric NOT NULL);",
    );
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    project.test_file(
        "orders.sql",
        "SELECT plan(2);
         SELECT has_table('app', 'orders', 'the orders table exists');
         SELECT col_is_pk('app', 'orders', 'id', 'id is the primary key');
         SELECT finish();",
    );

    let report = project.report(&["test", "--uri", &db.uri()]);
    report.assert_success();

    let tests = report.json["tests"].as_array().expect("tests are reported");
    assert_eq!(tests.len(), 1);
    assert_eq!(tests[0]["path"], "tests/db/orders.sql");
    assert_eq!(tests[0]["status"], "succeeded");
    assert_eq!(tests[0]["planned"], 2);
    assert_eq!(tests[0]["assertions"].as_array().unwrap().len(), 2);
    assert_eq!(tests[0]["assertions"][0]["status"], "passed");

    // pgTAP went into its own schema, not into the application's and not as an
    // extension.
    assert_eq!(
        db.scalar("SELECT count(*) FROM pg_extension WHERE extname = 'pgtap'"),
        "0"
    );
    assert_eq!(db.scalar("SELECT zapadka_test.pgtap_version()"), "1.3");
}

#[test]
fn a_failing_assertion_fails_the_run_and_reports_its_diagnostics() {
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

    project.test_file(
        "orders.sql",
        "SELECT plan(2);
         SELECT has_table('public', 'orders', 'the orders table exists');
         SELECT is(1, 2, 'one equals two');
         SELECT finish();",
    );

    let report = project.report(&["test", "--uri", &db.uri()]);
    report.assert_failed("verify.failed", exit::EXECUTION);

    let tests = &report.json["tests"];
    assert_eq!(tests[0]["status"], "failed");
    assert_eq!(tests[0]["assertions"][0]["status"], "passed");
    assert_eq!(tests[0]["assertions"][1]["status"], "failed");
    // pgTAP's YAML diagnostics survive into the report.
    assert_eq!(tests[0]["assertions"][1]["diagnostics"]["have"], "1");
    assert_eq!(tests[0]["assertions"][1]["diagnostics"]["want"], "2");
}

#[test]
fn a_test_file_cannot_leave_anything_behind() {
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

    // The first file writes; the second asserts the write is not visible. Both
    // run in the same suite, in order.
    project.test_file(
        "a-writes.sql",
        "SELECT plan(1);
         INSERT INTO public.orders VALUES (1);
         SELECT ok(true, 'inserted a row');
         SELECT finish();",
    );
    project.test_file(
        "b-cannot-see-it.sql",
        "SELECT plan(1);
         SELECT is((SELECT count(*)::int FROM public.orders), 0, 'the other file left nothing');
         SELECT finish();",
    );

    project
        .report(&["test", "--uri", &db.uri()])
        .assert_success();
    assert_eq!(db.scalar("SELECT count(*) FROM public.orders"), "0");
}

#[test]
fn a_test_file_with_no_plan_is_a_failure_not_a_pass() {
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

    // Emits a passing assertion and then stops. Without the plan check this
    // would look like a pass, which is the failure mode that matters most.
    project.test_file("orders.sql", "SELECT ok(true, 'looks fine');");

    // pgTAP itself refuses to run an assertion before a plan, so the failure
    // arrives as a SQL error rather than as unparseable TAP. Either way the run
    // fails, which is the property that matters.
    let report = project.report(&["test", "--uri", &db.uri()]);
    report.assert_failed("verify.failed", exit::EXECUTION);
    let message = report.json["tests"][0]["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    assert!(message.contains("plan"), "{message}");
}

#[test]
fn test_refuses_to_guess_which_database_to_use() {
    let project = project();
    project.test_file("orders.sql", "SELECT plan(0); SELECT finish();");

    // No --target and no --uri. This command installs a framework and runs
    // arbitrary SQL, so it never picks a database on its own.
    let report = project.report(&["test"]);
    report.assert_failed("target.unknown", 3);
}

#[test]
fn test_refuses_a_target_whose_migrations_are_not_applied() {
    let db = database();
    let project = project();
    let first = project.migration("first", &[], "CREATE TABLE public.a (i int);");
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();
    // Added after the deploy, so the target no longer matches the project.
    project.migration("second", &[first], "CREATE TABLE public.b (i int);");
    project.test_file("orders.sql", "SELECT plan(0); SELECT finish();");

    let report = project.report(&["test", "--uri", &db.uri()]);
    report.assert_failed("registry.not_initialized", exit::REGISTRY);
    assert_eq!(report.error_context("pending"), "1");
}

#[test]
fn a_selector_matching_no_test_file_is_an_error() {
    let db = database();
    let project = project();
    project.test_file("orders.sql", "SELECT plan(0); SELECT finish();");

    project
        .report(&["test", "--uri", &db.uri(), "does-not-exist.sql"])
        .assert_failed("selector.matched_nothing", 3);
}

// --- Regressions for issues a code review found -------------------------------
//
// Each of these passed review as "obviously correct" and was not. They are kept
// as integration tests rather than unit tests because every one of them is
// about what the database ends up containing.

#[test]
fn no_command_will_act_on_another_projects_registry() {
    let db = database();
    let owner = project();
    owner.migration("first", &[], "CREATE TABLE public.a (i int);");
    owner
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    // A different project — different project.id — pointed at the same database.
    let intruder = project();
    intruder.migration_with(
        "other",
        &[],
        "CREATE TABLE public.b (i int);",
        Some("DROP TABLE public.b;"),
        None,
    );
    // `test` returns early when a project has no test files, so it needs one
    // before it reaches the point where it would open the target.
    intruder.test_file("any.sql", "SELECT plan(0); SELECT finish();");

    // Every command that opens a target must refuse, not just the ones that
    // upgrade the registry. `revert` in particular would otherwise run one
    // project's revert script against another project's schema.
    let uri = db.uri();
    for command in [
        vec!["status"],
        vec!["verify"],
        vec!["deploy"],
        vec!["test"],
        vec!["revert", "other"],
        vec!["baseline", "--to", "other", "--acknowledge-existing-schema"],
    ] {
        let mut args = command.clone();
        args.extend(["--uri", uri.as_str()]);
        let report = intruder.report(&args);
        assert_eq!(
            report.error_code(),
            "registry.project_mismatch",
            "`{}` acted on another project's registry",
            command.join(" ")
        );
    }
}

#[test]
fn a_verify_script_cannot_commit_its_way_out_of_the_rollback() {
    let db = database();
    let project = project();
    let id = project.migration_with(
        "create-orders",
        &[],
        "CREATE TABLE public.orders (id bigint PRIMARY KEY);",
        None,
        Some("SELECT 1 FROM public.orders;"),
    );
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    // `verify.sql` is mutable, so it can acquire a COMMIT long after the
    // migration that owns it was reviewed and deployed. Everything after that
    // commit would run outside the transaction and survive the rollback.
    project.rewrite_script(
        id,
        "verify.sql",
        "COMMIT;\nCREATE TABLE public.escaped (i int);",
    );

    let report = project.report(&["verify", "--uri", &db.uri()]);
    report.assert_failed("script.transaction_control", exit::VALIDATION);
    assert!(!db.has_relation("public.escaped"));
}

#[test]
fn a_revert_script_cannot_commit_its_way_out_of_the_rollback() {
    let db = database();
    let project = project();
    let id = project.migration_with(
        "create-orders",
        &[],
        "CREATE TABLE public.orders (id bigint PRIMARY KEY);",
        Some("DROP TABLE public.orders;"),
        None,
    );
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    // Acquired after the deploy, as a mutable script can. Nothing validated it
    // on the way in, so the runner is what has to catch it.
    project.rewrite_script(
        id,
        "revert.sql",
        "COMMIT;\nCREATE TABLE public.escaped (i int);",
    );

    let report = project.report(&["revert", "--uri", &db.uri(), "create-orders"]);
    report.assert_failed("script.transaction_control", exit::VALIDATION);
    assert!(!db.has_relation("public.escaped"));
    // The migration is untouched, because nothing ran.
    assert_eq!(
        db.scalar("SELECT count(*) FROM zapadka.applied_migrations"),
        "1"
    );
}

#[test]
fn a_test_file_cannot_commit_its_way_out_of_the_rollback() {
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

    // The whole file is sent as one simple query, so statements after a COMMIT
    // would run outside the transaction and the later rollback would not undo
    // them — silently breaking the isolation the whole suite depends on.
    project.test_file(
        "escapes.sql",
        "SELECT plan(1);
         COMMIT;
         CREATE TABLE public.escaped (i int);
         SELECT ok(true, 'still here');
         SELECT finish();",
    );

    let report = project.report(&["test", "--uri", &db.uri()]);
    assert_ne!(report.code(), 0, "a test file that commits must not pass");
    assert!(!db.has_relation("public.escaped"));
}

#[test]
fn verification_cannot_write_even_without_transaction_control() {
    let db = database();
    let project = project();
    project.migration_with(
        "create-orders",
        &[],
        "CREATE TABLE public.orders (id bigint PRIMARY KEY);",
        None,
        // No COMMIT, so the guard does not fire. Rollback would undo this, but
        // the read-only transaction refuses it at the point it is attempted,
        // which is the difference between a claim and an enforced property.
        Some("INSERT INTO public.orders VALUES (1);"),
    );

    let report = project.report(&["deploy", "--uri", &db.uri()]);
    report.assert_failed("verify.failed", exit::EXECUTION);
    // 25006: read_only_sql_transaction.
    assert_eq!(report.sqlstate(), "25006");
    assert_eq!(db.scalar("SELECT count(*) FROM public.orders"), "0");
}

#[test]
fn verification_can_build_an_expected_set_without_writing() {
    // A read-only transaction refuses every CREATE, including CREATE TEMP
    // TABLE, so a verification script that wants a set to compare against
    // builds it with a CTE or a VALUES list. This is the pattern to reach for,
    // and it is why the restriction is liveable.
    let db = database();
    let project = project();
    project.migration_with(
        "create-orders",
        &[],
        "CREATE TABLE public.orders (id bigint PRIMARY KEY);
         INSERT INTO public.orders VALUES (1), (2);",
        None,
        Some(
            "WITH expected(id) AS (VALUES (1::bigint), (2::bigint))
             SELECT 1 / (CASE WHEN (SELECT count(*) FROM public.orders)
                              = (SELECT count(*) FROM expected) THEN 1 ELSE 0 END);",
        ),
    );

    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();
    assert_eq!(db.scalar("SELECT count(*) FROM public.orders"), "2");
}

#[test]
fn verification_cannot_advance_a_sequence() {
    // Rollback does not undo `nextval()` -- a sequence advanced inside a
    // rolled-back transaction stays advanced. Rollback alone therefore cannot
    // deliver "verification leaves nothing behind"; the read-only transaction
    // is what makes it true, by refusing the call.
    let db = database();
    let project = project();
    project.migration_with(
        "create-counter",
        &[],
        "CREATE SEQUENCE public.counter;",
        None,
        Some("SELECT nextval('public.counter');"),
    );

    let report = project.report(&["deploy", "--uri", &db.uri()]);
    report.assert_failed("verify.failed", exit::EXECUTION);
    assert_eq!(report.sqlstate(), "25006", "read_only_sql_transaction");
    assert_eq!(
        db.scalar("SELECT is_called FROM public.counter"),
        "f",
        "the sequence was never advanced"
    );
}

#[test]
fn a_successful_deploy_records_its_event_atomically() {
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

    // The applied row and its success event commit together, so history can
    // never say a deploy failed while state says it is applied.
    assert_eq!(
        db.scalar(
            "SELECT count(*) FROM zapadka.events \
             WHERE action = 'deploy' AND outcome = 'succeeded'"
        ),
        "1"
    );
    assert_eq!(
        db.scalar("SELECT count(*) FROM zapadka.applied_migrations"),
        "1"
    );
}

#[test]
fn a_test_file_that_advances_a_sequence_is_reported() {
    // PostgreSQL does not roll back `nextval()`. Zapadka deliberately does not
    // rewind it either: its lock serializes other Zapadka runs but not
    // application connections, so rewinding could hand out a key already
    // issued to someone else. Trading a test-ordering problem for a
    // duplicate-key problem in a live database is not a trade worth making.
    //
    // So the property is reported rather than silently broken or unsafely
    // repaired.
    let db = database();
    let project = project();
    project.migration(
        "create-orders",
        &[],
        "CREATE TABLE public.orders (id bigserial PRIMARY KEY, note text);",
    );
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    project.test_file(
        "inserts.sql",
        "SELECT plan(1);
         INSERT INTO public.orders (note) VALUES ('first');
         SELECT ok(true, 'inserted a row');
         SELECT finish();",
    );

    let report = project.report(&["test", "--uri", &db.uri()]);
    report.assert_success();

    assert!(
        report
            .diagnostic_codes()
            .contains(&"test.sequence_advanced"),
        "advancing a sequence should be reported: {:?}",
        report.diagnostic_codes()
    );
    // The row itself is gone, which is what rollback does cover.
    assert_eq!(db.scalar("SELECT count(*) FROM public.orders"), "0");
}

#[test]
fn the_events_table_cannot_be_truncated() {
    let db = database();
    let project = project();
    project.migration("first", &[], "CREATE TABLE public.a (i int);");
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    // TRUNCATE is neither an UPDATE nor a DELETE, so the row-level trigger
    // never sees it. Without a statement-level trigger the table's owner --
    // normally the deploying role -- could erase the whole history.
    assert!(
        harness::try_sql(&db, "TRUNCATE zapadka.events").is_err(),
        "TRUNCATE should have been refused by the registry"
    );
    assert_ne!(db.scalar("SELECT count(*) FROM zapadka.events"), "0");
}

#[test]
fn a_statement_postgresql_forbids_in_a_transaction_is_caught_before_connecting() {
    let project = project();
    // CREATE DATABASE cannot run inside a transaction block at all. Without
    // classifying it, this would pass lint, connect, and only then fail.
    project.migration("make-a-database", &[], "CREATE DATABASE other;");

    let report = project.report(&["deploy", "--uri", "postgresql://nobody@127.0.0.1:1/nothing"]);
    report.assert_failed("script.statement_count", exit::VALIDATION);
    assert!(
        report.json["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("CREATE DATABASE"),
        "{}",
        report.json["error"]["message"]
    );
}

#[test]
fn a_successful_revert_records_exactly_one_event_atomically() {
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
    project
        .report(&["revert", "--uri", &db.uri(), "create-orders"])
        .assert_success();

    // Exactly one: the event commits inside the transaction that performed the
    // revert, so it is neither missing nor duplicated.
    assert_eq!(
        db.scalar(
            "SELECT count(*) FROM zapadka.events \
             WHERE action = 'revert' AND outcome = 'succeeded'"
        ),
        "1"
    );
    assert_eq!(
        db.scalar("SELECT count(*) FROM zapadka.applied_migrations"),
        "0"
    );
}

#[test]
fn the_report_names_the_database_the_server_says_it_is_connected_to() {
    // A URI can omit the database name, in which case PostgreSQL defaults it to
    // the user name. Reading it back from the configuration would report an
    // empty string for a perfectly ordinary connection.
    let db = database();
    let project = project();

    let report = project.report(&["status", "--uri", &db.uri()]);
    report.assert_success();
    assert_eq!(
        report.json["target"]["database"]
            .as_str()
            .unwrap_or_default(),
        db.name(),
        "the report should name the database actually connected to"
    );
}

#[test]
fn baseline_refuses_a_migration_a_later_deploy_could_not_accept() {
    // Recording a migration that fails lint would wedge the project: it is
    // applied, so it cannot be edited without a history mismatch, and every
    // later deploy fails on it.
    let db = database();
    let project = project();
    project.migration("bad", &[], "CREATE TABLE t (i int);\nCOMMIT;");

    let report = project.report(&[
        "baseline",
        "--uri",
        &db.uri(),
        "--to",
        "bad",
        "--acknowledge-existing-schema",
    ]);
    report.assert_failed("script.transaction_control", exit::VALIDATION);
    assert_eq!(
        db.scalar("SELECT count(*) FROM pg_namespace WHERE nspname = 'zapadka'"),
        "0",
        "nothing should have been recorded"
    );
}

#[test]
fn deploy_reports_a_policy_denial_that_names_no_rule() {
    // A typo in policy.deny means the safeguard someone added is not running.
    // That matters most on the deploy path, which is where it was silent.
    let db = database();
    let project = project();
    let config = project.root().join("zapadka.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        text.replace("[policy]", "[policy]\ndeny = [\"lint.destrutive\"]"),
    )
    .unwrap();
    project.migration("first", &[], "CREATE TABLE public.a (i int);");

    let report = project.report(&["deploy", "--uri", &db.uri()]);
    report.assert_success();
    assert!(
        report.diagnostic_codes().contains(&"policy.unknown_lint"),
        "{:?}",
        report.diagnostic_codes()
    );
}

#[test]
fn a_failed_post_deploy_verification_names_the_script_that_failed() {
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

    // `verify.sql` is mutable, so the report has to say which bytes ran.
    let scripts = report.migrations()[0]["scripts"].as_array().unwrap();
    let verify = scripts
        .iter()
        .find(|script| script["role"] == "verify")
        .expect("the failed verification script should be in the report");
    assert_eq!(verify["status"], "failed");
    assert_eq!(verify["sha256"].as_str().unwrap_or_default().len(), 64);
    assert!(
        verify["path"]
            .as_str()
            .unwrap_or_default()
            .ends_with("verify.sql")
    );
}

#[test]
fn renaming_the_registry_schema_does_not_make_room_for_a_second_project() {
    // `registry_schema` is configurable, so two projects pointed at one
    // database with different schema names would each see only their own
    // registry, both conclude the database was theirs, take different
    // project-derived advisory locks, and deploy over each other.
    let db = database();
    let owner = project();
    owner.migration("first", &[], "CREATE TABLE public.a (i int);");
    owner
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    let intruder = project();
    let config = intruder.root().join("zapadka.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        text.replace(
            "registry_schema = \"zapadka\"",
            "registry_schema = \"zapadka_other\"",
        ),
    )
    .unwrap();
    intruder.migration("other", &[], "CREATE TABLE public.b (i int);");

    let report = intruder.report(&["deploy", "--uri", &db.uri()]);
    report.assert_failed("registry.project_mismatch", exit::REGISTRY);
    assert_eq!(
        db.scalar("SELECT count(*) FROM pg_namespace WHERE nspname = 'zapadka_other'"),
        "0",
        "the second project must not have created its own registry"
    );
}
