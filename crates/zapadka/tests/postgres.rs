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
fn a_failed_deploy_can_still_leave_the_database_changed() {
    let db = database();
    let project = project();
    project.migration(
        "create-counter",
        &[],
        "CREATE SEQUENCE public.order_id;\nSELECT nextval('public.order_id');",
    );
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();
    // Primed, so `last_value` moves rather than only `is_called` flipping.
    let before = db.scalar("SELECT last_value FROM public.order_id");

    // A migration that advances a sequence and then fails. PostgreSQL rolls
    // back the registry row and every transactional effect -- but not
    // nextval(), which is non-transactional by design.
    project.migration(
        "use-counter",
        &[],
        "SELECT nextval('public.order_id');\nSELECT 1 / 0;",
    );
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_failed("deploy.failed", exit::EXECUTION);

    let after = db.scalar("SELECT last_value FROM public.order_id");
    let applied = db.scalar("SELECT count(*) FROM zapadka.applied_migrations");

    assert_eq!(
        applied, "1",
        "the failed migration is not recorded as applied"
    );
    assert_ne!(
        before, after,
        "the sequence advanced despite the rollback: {before} -> {after}"
    );
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
        "2"
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

/// Zapadka runs verification read-only so it cannot change committed state.
/// That guarantee is about the database, and a role that can reach past the
/// database is outside it — no transaction rolls back a `COPY ... TO PROGRAM`.
/// Only Zapadka is positioned to notice, so it says so rather than letting the
/// promise read as broader than it is.
#[test]
fn a_role_that_can_act_outside_the_database_is_reported() {
    let db = database();
    let project = project();

    // The harness connects as the superuser, which is exactly such a role.
    let report = project.report(&["status", "--uri", &db.uri()]);
    report.assert_success();
    assert!(
        report
            .diagnostic_codes()
            .contains(&"target.privileged_role"),
        "connecting as a superuser should say so: {:?}",
        report.diagnostic_codes()
    );
}

#[test]
fn an_ordinary_role_draws_no_privilege_note() {
    let db = database();
    let project = project();
    // Roles are cluster-wide while the database is disposable, so the role is
    // named after the database to keep concurrent tests from colliding.
    let role = format!("{}_deployer", db.name());
    db.query(&format!(
        "CREATE ROLE {role} LOGIN PASSWORD 'deployer'; \
         GRANT CONNECT ON DATABASE {} TO {role}",
        db.name()
    ));

    let report = project.report(&["status", "--uri", &db.uri_as(&role, "deployer")]);
    report.assert_success();
    assert!(
        !report
            .diagnostic_codes()
            .contains(&"target.privileged_role"),
        "a role with no privileges beyond the database should draw no note: {:?}",
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
    assert_eq!(db.scalar("SELECT zapadka_test.zapadka_test_version()"), "1");
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
fn exception_privilege_and_type_assertions_work_and_report_structurally() {
    let db = database();
    let project = project();
    project.migration(
        "create-orders",
        &[],
        "CREATE TABLE public.orders (id bigint PRIMARY KEY, status text);\n\
         INSERT INTO public.orders VALUES (1,'paid');\n\
         CREATE TYPE public.mood AS ENUM ('sad','ok','happy');",
    );
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    // Everything here runs inside the runner's transaction and is rolled back,
    // including the role.
    project.test_file(
        "checks.sql",
        "CREATE ROLE reader;\n\
         GRANT SELECT ON public.orders TO reader;\n\
         SELECT throws_ok($$INSERT INTO public.orders VALUES (1,'dup')$$, '23505', \n\
        \x20              NULL::text, 'a duplicate key is rejected');\n\
         SELECT throws_like($$SELECT * FROM public.nope$$, '%does not exist%', \n\
        \x20              'a missing relation is named');\n\
         SELECT lives_ok($$SELECT 1$$, 'a trivial query lives');\n\
         SELECT table_privs_are('public','orders','reader', ARRAY['SELECT'], \n\
        \x20              'reader may only select');\n\
         SELECT enum_has_labels('public','mood', ARRAY['sad','ok','happy'], \n\
        \x20              'mood labels in order');\n\
         SELECT cast_context_is('integer','bigint','implicit', 'int widens implicitly');\n\
         SELECT has_domain('nonexistent_domain', 'this one should fail');",
    );

    let report = project.report(&["test", "--uri", &db.uri()]);
    report.assert_failed("verify.failed", exit::EXECUTION);

    let assertions = report.json["tests"][0]["assertions"].as_array().unwrap();
    assert_eq!(assertions.len(), 7);
    let statuses: Vec<&str> = assertions
        .iter()
        .map(|a| a["status"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        statuses,
        vec![
            "passed", "passed", "passed", "passed", "passed", "passed", "failed"
        ],
        "only the last assertion should fail: {statuses:?}"
    );
}

/// Every public assertion, exercised so that all of them pass.
///
/// This is coverage of *signatures* rather than of semantics. A typo, a missing
/// overload, or a helper that does not resolve shows up here as an aborted file
/// -- which is how the missing `hasnt_view(name, name)` would have been caught
/// before review found it.
#[test]
fn every_public_assertion_resolves_and_passes() {
    let db = database();
    let project = project();
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();
    project.test_file("every.sql", EVERY_ASSERTION);

    let report = project.report(&["test", "--uri", &db.uri()]);
    report.assert_success();

    let assertions = report.json["tests"][0]["assertions"].as_array().unwrap();
    assert_eq!(assertions.len(), 66, "the plan and the file must agree");

    // The only non-passing entries should be the deliberate TODO and SKIP.
    let unusual: Vec<&str> = assertions
        .iter()
        .map(|a| a["status"].as_str().unwrap_or_default())
        .filter(|status| *status != "passed")
        .collect();
    assert_eq!(unusual, vec!["todo_failed", "skipped"], "{unusual:?}");

    assert_eq!(
        report.json["tests"][0]["notes"].as_array().unwrap().len(),
        2
    );
}

const EVERY_ASSERTION: &str = r"-- Every public assertion, exercised so that all of them pass.
--
-- The point is coverage of *signatures*, not of semantics: a typo, a missing
-- overload, or a helper that does not resolve shows up here as an aborted file
-- rather than as a mystery in someone's project months later.
CREATE SCHEMA fixture;
CREATE TABLE fixture.orders (id bigint PRIMARY KEY, status text NOT NULL DEFAULT 'new');
INSERT INTO fixture.orders (id, status) VALUES (1, 'paid'), (2, 'pending');
CREATE VIEW fixture.paid AS SELECT * FROM fixture.orders WHERE status = 'paid';
CREATE SEQUENCE fixture.counter;
CREATE TYPE fixture.mood AS ENUM ('sad', 'ok', 'happy');
CREATE DOMAIN fixture.positive AS integer CHECK (VALUE > 0);
CREATE ROLE fixture_reader;
GRANT USAGE ON SCHEMA fixture TO fixture_reader;
GRANT SELECT ON fixture.orders TO fixture_reader;

SELECT plan(66);

-- scalar
SELECT ok(true, 'ok');
SELECT pass('pass');
SELECT is(1, 1, 'is');
SELECT isnt(1, 2, 'isnt');
SELECT matches('hello'::text, '^hel', 'matches');
SELECT imatches('HELLO'::text, '^hel', 'imatches');
SELECT doesnt_match('hello'::text, '^zzz', 'doesnt_match');
SELECT cmp_ok(2, '>', 1, 'cmp_ok');
SELECT isa_ok(1::bigint, 'bigint'::regtype, 'isa_ok');

-- objects
SELECT has_schema('fixture', 'has_schema');
SELECT hasnt_schema('nope_schema', 'hasnt_schema');
SELECT has_table('fixture', 'orders', 'has_table qualified');
-- Two bare literals are both `unknown`, and PostgreSQL prefers `text` in that
-- category, so this resolves to has_table(table, description) rather than
-- has_table(schema, table). pgTAP behaves identically. Reaching the
-- schema-qualified two-argument form needs explicit casts.
SELECT has_table_in('fixture', 'orders');
SELECT hasnt_table_in('fixture', 'nope');
SELECT has_view_in('fixture', 'paid');
SELECT has_column_in('fixture', 'orders', 'status');
SELECT col_is_pk_in('fixture', 'orders', 'id');
SELECT has_pk_in('fixture', 'orders');
SELECT throws_sqlstate($$SELECT 1/0$$, '22012', 'throws_sqlstate');
SELECT hasnt_table('fixture', 'nope', 'hasnt_table qualified');
SELECT hasnt_table('fixture', 'nope');
SELECT has_view('fixture', 'paid', 'has_view qualified');
SELECT hasnt_view('fixture', 'nope', 'hasnt_view qualified');
SELECT hasnt_view('fixture', 'nope');
SELECT has_sequence('fixture', 'counter', 'has_sequence qualified');
SELECT has_sequence('fixture'::name, 'counter'::name);
SELECT hasnt_sequence('nope_seq', 'hasnt_sequence');
SELECT hasnt_sequence('fixture'::name, 'nope_seq'::name);
SELECT performs_within($$SELECT 1$$, 0, 60000);
SELECT has_column('fixture', 'orders', 'status', 'has_column qualified');
SELECT hasnt_column('fixture', 'orders', 'nope', 'hasnt_column qualified');
SELECT has_pk('fixture', 'orders', 'has_pk');
SELECT col_is_pk('fixture', 'orders', 'id', 'col_is_pk');
SELECT col_is_pk('fixture'::name, 'orders'::name, 'id'::name);
SELECT col_is_pk('fixture'::name, 'orders'::name, ARRAY['id']::name[]);
-- Queries that cannot be compared are certainly not the same set, and this
-- must record an assertion rather than abort the file.
SELECT set_ne('SELECT id, status FROM fixture.orders', 'SELECT id FROM fixture.orders',
              'set_ne on incomparable shapes');

-- relations
SELECT set_eq('SELECT status FROM fixture.orders', ARRAY['paid','pending'], 'set_eq array');
SELECT set_eq('SELECT id FROM fixture.orders', 'SELECT unnest(ARRAY[1::bigint,2::bigint])', 'set_eq sql');
SELECT set_ne('SELECT id FROM fixture.orders', 'SELECT 99::bigint', 'set_ne');
SELECT set_has('SELECT id FROM fixture.orders', 'SELECT 1::bigint', 'set_has');
SELECT bag_eq('SELECT id FROM fixture.orders', 'SELECT unnest(ARRAY[1::bigint,2::bigint])', 'bag_eq');
SELECT bag_has('SELECT id FROM fixture.orders', 'SELECT 1::bigint', 'bag_has');
SELECT results_eq('SELECT id FROM fixture.orders ORDER BY id',
                  'SELECT unnest(ARRAY[1::bigint,2::bigint])', 'results_eq');
SELECT is_empty('SELECT 1 WHERE false', 'is_empty');
SELECT isnt_empty('SELECT id FROM fixture.orders', 'isnt_empty');

-- behaviour
SELECT throws_ok($$SELECT 1/0$$, '22012', NULL::text, 'throws_ok');
SELECT throws_like($$SELECT * FROM fixture.nope$$, '%does not exist%', 'throws_like');
SELECT throws_ilike($$SELECT * FROM fixture.nope$$, '%DOES NOT EXIST%', 'throws_ilike');
SELECT throws_matching($$SELECT 1/0$$, 'division', 'throws_matching');
SELECT lives_ok($$SELECT 1$$, 'lives_ok');
SELECT performs_ok($$SELECT 1$$, 60000, 'performs_ok');
SELECT performs_within($$SELECT 1$$, 0, 60000, 3, 'performs_within');

-- catalog
SELECT table_privs_are('fixture', 'orders', 'fixture_reader', ARRAY['SELECT'], 'table_privs_are');
SELECT schema_privs_are('fixture', 'fixture_reader', ARRAY['USAGE'], 'schema_privs_are');
SELECT table_owner_is('fixture', 'orders', current_user::name, 'table_owner_is');
SELECT view_owner_is('fixture', 'paid', current_user::name, 'view_owner_is');
SELECT has_enum('fixture', 'mood', 'has_enum');
SELECT enum_has_labels('fixture', 'mood', ARRAY['sad','ok','happy'], 'enum_has_labels');
SELECT has_domain('fixture', 'positive', 'has_domain');
SELECT domain_type_is('fixture', 'positive', 'integer', 'domain_type_is');
SELECT has_cast('integer', 'bigint', 'has_cast');
SELECT cast_context_is('integer', 'bigint', 'implicit', 'cast_context_is');
SELECT has_operator('integer', '+'::name, 'integer', 'integer', 'has_operator');
SELECT has_leftop('-'::name, 'integer', 'integer', 'has_leftop');

-- directives and notes
SELECT diag('a note');
SELECT note('another note');
SELECT todo_start('not done');
SELECT ok(false, 'a todo failure');
SELECT todo_end();
SELECT skip('skipped on purpose', 1);

SELECT finish();
";

#[test]
fn a_schema_left_by_the_pgtap_era_is_replaced_rather_than_refused() {
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

    // Exactly what a target tested by v0.2.0 carries: the reserved schema with
    // the old marker table. Without recognising it, Zapadka would report a
    // schema it had created itself as one it did not, and tell the operator to
    // drop it -- an upgrade that blocks on a lie.
    db.query(
        "DROP SCHEMA IF EXISTS zapadka_test CASCADE;          CREATE SCHEMA zapadka_test;          CREATE TABLE zapadka_test.zapadka_pgtap (              singleton boolean PRIMARY KEY DEFAULT true,              pgtap_version text NOT NULL,              artifact_sha256 text NOT NULL,              zapadka_version text NOT NULL);          INSERT INTO zapadka_test.zapadka_pgtap (pgtap_version, artifact_sha256, zapadka_version)          VALUES ('1.3.4', repeat('a', 64), '0.2.0')",
    );

    project.test_file("orders.sql", "SELECT ok(true, 'runs after the upgrade');");
    let report = project.report(&["test", "--uri", &db.uri()]);
    report.assert_success();
    assert!(
        report
            .diagnostic_codes()
            .contains(&"test.library_installed"),
        "the stale installation should be replaced: {:?}",
        report.diagnostic_codes()
    );
    assert_eq!(
        db.scalar("SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace                    WHERE n.nspname = 'zapadka_test' AND c.relname = 'zapadka_pgtap'"),
        "0",
        "the old marker should be gone"
    );
}

#[test]
fn a_schema_that_merely_shares_a_name_is_not_dropped() {
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

    // Somebody else's schema that happens to hold a relation named
    // `zapadka_pgtap`. Classifying it as a previous installation leads to
    // DROP SCHEMA ... CASCADE, so the shape has to be checked and not just the
    // name -- otherwise recognising the upgrade path becomes a way to destroy
    // data.
    db.query(
        "DROP SCHEMA IF EXISTS zapadka_test CASCADE;          CREATE SCHEMA zapadka_test;          CREATE TABLE zapadka_test.zapadka_pgtap (whatever text);          INSERT INTO zapadka_test.zapadka_pgtap VALUES ('precious');",
    );

    project.test_file("orders.sql", "SELECT ok(true, 'x');");
    let report = project.report(&["test", "--uri", &db.uri()]);
    report.assert_failed("registry.upgrade_failed", exit::REGISTRY);

    assert_eq!(
        db.scalar("SELECT whatever FROM zapadka_test.zapadka_pgtap"),
        "precious",
        "an unrelated schema must survive"
    );
}

#[test]
fn a_sqlstate_where_a_description_belongs_is_refused() {
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

    // Arguments in pgTAP's order, where the third was the expected message.
    // Reinterpreting it silently would make the file assert something its
    // author never wrote, so it is refused instead -- the same reasoning as
    // blocking on an unknown nontransactional outcome.
    project.test_file(
        "checks.sql",
        "SELECT throws_ok($$SELECT 1/0$$, 'division by zero', '22012');",
    );

    let report = project.report(&["test", "--uri", &db.uri()]);
    report.assert_failed("verify.failed", exit::EXECUTION);
    let message = report.json["tests"][0]["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        message.contains("looks like a SQLSTATE"),
        "the refusal should name the problem: {message}"
    );
    // PostgreSQL's HINT travels in its own field rather than in the message.
    let hint = report.json["tests"][0]["error"]["hint"]
        .as_str()
        .unwrap_or_default();
    assert!(
        hint.contains("throws_sqlstate"),
        "and point at the unambiguous spelling: {hint}"
    );
}

#[test]
fn throws_ok_reads_a_five_byte_argument_as_a_sqlstate() {
    let db = database();
    let project = project();
    project.migration(
        "create-orders",
        &[],
        "CREATE TABLE public.orders (id bigint PRIMARY KEY);\n\
         INSERT INTO public.orders VALUES (1);",
    );
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    // The second argument still auto-detects, because `throws_ok(sql, '23505')`
    // and `throws_ok(sql, 'division by zero')` are both natural. What changed
    // from pgTAP is that the *third* argument no longer shifts meaning: it is
    // the description, always.
    project.test_file(
        "checks.sql",
        "SELECT throws_ok($$INSERT INTO public.orders VALUES (1)$$, '23505');\n\
         SELECT throws_ok($$SELECT 1/0$$, 'division by zero');\n\
         SELECT throws_ok($$SELECT 1/0$$, 'not the message it raises');",
    );

    let report = project.report(&["test", "--uri", &db.uri()]);
    report.assert_failed("verify.failed", exit::EXECUTION);
    let statuses: Vec<&str> = report.json["tests"][0]["assertions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["status"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        statuses,
        vec!["passed", "passed", "failed"],
        "five bytes is a sqlstate; anything else is a message: {statuses:?}"
    );
}

#[test]
fn a_test_files_notes_reach_the_report() {
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
        "checks.sql",
        "SELECT diag('setup context, before any assertion');\n\
         SELECT ok(false, 'this fails');\n\
         SELECT diag('why it failed');",
    );

    let report = project.report(&["test", "--uri", &db.uri()]);
    report.assert_failed("verify.failed", exit::EXECUTION);
    let notes = report.json["tests"][0]["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 2);
    // The first belongs to no assertion, and saying otherwise would misreport
    // where it came from.
    assert!(notes[0].get("after_assertion").is_none());
    assert_eq!(notes[1]["after_assertion"], 1);
}

#[test]
fn a_wrong_sqlstate_expectation_reports_both_codes() {
    let db = database();
    let project = project();
    project.migration(
        "create-orders",
        &[],
        "CREATE TABLE public.orders (id bigint PRIMARY KEY);\n\
         INSERT INTO public.orders VALUES (1);",
    );
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    // pgTAP would tell you the error text did not match. The point of keeping
    // SQLSTATE as its own field is being told 23505 arrived where 23503 was
    // expected.
    project.test_file(
        "checks.sql",
        "SELECT throws_ok($$INSERT INTO public.orders VALUES (1)$$, '23503', \n\
        \x20              'expects a foreign-key violation');",
    );

    let report = project.report(&["test", "--uri", &db.uri()]);
    report.assert_failed("verify.failed", exit::EXECUTION);
    let diagnostics = &report.json["tests"][0]["assertions"][0]["diagnostics"];
    let caught = diagnostics["caught"].as_str().unwrap_or_default();
    let expected = diagnostics["expected"].as_str().unwrap_or_default();
    assert!(caught.contains("23505"), "the real sqlstate: {caught}");
    assert!(
        expected.contains("23503"),
        "the expected sqlstate: {expected}"
    );
}

#[test]
fn a_result_set_failure_reports_the_rows_and_their_types() {
    let db = database();
    let project = project();
    project.migration(
        "create-orders",
        &[],
        "CREATE TABLE public.orders (id bigint, status text);\n\
         INSERT INTO public.orders VALUES (1,'paid'),(2,'pending');",
    );
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    project.test_file(
        "orders.sql",
        "SELECT set_eq(\n\
        \x20   'SELECT id, status FROM public.orders',\n\
        \x20   $$VALUES (1::bigint,'paid'),(2::bigint,'shipped')$$,\n\
        \x20   'orders have the expected statuses');",
    );

    let report = project.report(&["test", "--uri", &db.uri()]);
    report.assert_failed("verify.failed", exit::EXECUTION);

    // pgTAP would render both sides with `record::text` and leave the reader to
    // spot the difference. The structured detail names the rows and the types.
    let diagnostics = &report.json["tests"][0]["assertions"][0]["diagnostics"];
    assert_eq!(diagnostics["kind"], "set");
    assert_eq!(diagnostics["missing_count"], "1");
    assert_eq!(diagnostics["extra_count"], "1");
    assert!(
        diagnostics["missing"]
            .as_str()
            .unwrap_or_default()
            .contains("shipped"),
        "the missing row should be named: {diagnostics:?}"
    );
    assert!(
        diagnostics["columns"]
            .as_str()
            .unwrap_or_default()
            .contains("bigint"),
        "column types should be reported: {diagnostics:?}"
    );
}

#[test]
fn a_test_file_may_return_whatever_it_likes() {
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

    // Under TAP every result row had to be exactly one text column, so this
    // file could not run at all. The runner now ignores output entirely.
    project.test_file(
        "orders.sql",
        "SELECT 1 AS a, 2 AS b, 3 AS c;\n\
         CREATE TEMP TABLE scratch AS SELECT generate_series(1,3) AS n;\n\
         SELECT n, n * 2 FROM scratch;\n\
         SELECT is((SELECT count(*) FROM scratch), 3::bigint, 'scratch has three rows');",
    );

    let report = project.report(&["test", "--uri", &db.uri()]);
    report.assert_success();
    assert_eq!(
        report.json["tests"][0]["assertions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn a_plan_is_optional_but_a_wrong_one_fails() {
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

    // A file with no plan at all. Under TAP this had to fail: `1..N` was the
    // only way a text consumer could tell a finished stream from a truncated
    // one. Reading a table, the runner knows the transaction completed, so the
    // ceremony buys nothing and the file is simply valid.
    project.test_file("orders.sql", "SELECT ok(true, 'looks fine');");
    let report = project.report(&["test", "--uri", &db.uri()]);
    report.assert_success();
    assert_eq!(
        report.json["tests"][0]["assertions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // A plan that disagrees with what ran is still a failure, and now it is one
    // the runner enforces. pgTAP could only ever report this as a diagnostic.
    project.test_file(
        "mismatch.sql",
        "SELECT plan(3); SELECT ok(true, 'only one'); SELECT finish();",
    );
    let report = project.report(&["test", "mismatch.sql", "--uri", &db.uri()]);
    report.assert_failed("verify.failed", exit::EXECUTION);
    let message = report.json["tests"][0]["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    assert!(message.contains("planned 3"), "{message}");
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

#[test]
fn the_registry_schema_cannot_be_renamed_out_from_under_a_deployed_project() {
    // Pointing the same project at a different schema would create a second,
    // empty registry and re-run every migration against a database that
    // already has them.
    let db = database();
    let project = project();
    project.migration("first", &[], "CREATE TABLE public.a (i int);");
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    let config = project.root().join("zapadka.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        text.replace(
            "registry_schema = \"zapadka\"",
            "registry_schema = \"zapadka_renamed\"",
        ),
    )
    .unwrap();

    let report = project.report(&["deploy", "--uri", &db.uri()]);
    report.assert_failed("registry.project_mismatch", exit::REGISTRY);
    assert!(
        report.json["error"]["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("run them all again"),
        "the hint should say what would happen"
    );
    assert_eq!(
        db.scalar("SELECT count(*) FROM pg_namespace WHERE nspname = 'zapadka_renamed'"),
        "0"
    );
}

#[test]
fn the_registry_can_be_created_in_a_schema_that_already_exists() {
    // `registry_schema` may name a schema that exists for other reasons --
    // `public` being the obvious case -- and a first deploy into it must work.
    let db = database();
    let project = project();
    let config = project.root().join("zapadka.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        text.replace(
            "registry_schema = \"zapadka\"",
            "registry_schema = \"public\"",
        ),
    )
    .unwrap();
    project.migration("first", &[], "CREATE TABLE public.a (i int);");

    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();
    assert_eq!(
        db.scalar("SELECT count(*) FROM public.applied_migrations"),
        "1"
    );
}

#[test]
fn an_empty_verification_script_is_refused_rather_than_reported_as_verified() {
    // A `verify.sql` that runs nothing would be recorded as a successful
    // verification -- the report and the registry would both claim a check
    // happened when none did.
    let db = database();
    let project = project();
    project.migration_with(
        "create-orders",
        &[],
        "CREATE TABLE public.orders (id bigint PRIMARY KEY);",
        None,
        Some("-- TODO: write the check\n"),
    );

    let report = project.report(&["deploy", "--uri", &db.uri()]);
    report.assert_failed("script.empty", exit::VALIDATION);
    assert_eq!(
        db.scalar("SELECT count(*) FROM pg_namespace WHERE nspname = 'zapadka'"),
        "0"
    );
}

#[test]
fn standalone_verify_refuses_a_script_that_became_empty() {
    // `verify.sql` is mutable and standalone `verify` never runs lint, so the
    // check has to live at the execution boundary. Running a no-op would record
    // a successful verification for a check that did not happen.
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

    project.rewrite_script(id, "verify.sql", "-- the check was removed\n");

    let report = project.report(&["verify", "--uri", &db.uri()]);
    report.assert_failed("script.empty", exit::VALIDATION);
    assert_eq!(
        db.scalar(
            "SELECT count(*) FROM zapadka.events \
             WHERE action = 'verify' AND outcome = 'succeeded'"
        ),
        "1",
        "only the original deploy-time verification should be recorded"
    );
}

// -- Nontransactional migrations ------------------------------------------
//
// The mode exists for statements PostgreSQL refuses to run in a transaction.
// Everything below is about the consequence: without a transaction, the SQL and
// the record that it ran cannot commit together, so the recovery path is the
// feature rather than an afterthought.

/// Stages exactly what a run killed mid-statement leaves behind.
///
/// The attempt row is committed before the statement runs, so a process that
/// dies during the statement leaves this and nothing else.
fn insert_attempt(db: &harness::Database, id: uuid::Uuid, slug: &str) {
    db.query(&format!(
        "INSERT INTO zapadka.nontransactional_attempts \
           (migration_id, slug, definition_sha256, deploy_sha256, depends, run_id, \
            session_user_name, server_version, zapadka_version) \
         VALUES ('{id}', '{slug}', repeat('a', 64), repeat('b', 64), '{{}}'::uuid[], \
                 gen_random_uuid(), 'postgres', '18', '0.0.0')"
    ));
}

#[test]
fn a_nontransactional_migration_deploys_and_records_its_attempt_first() {
    let db = database();
    let project = project();
    // The index needs a table, and the migration must be a single statement, so
    // the table is created outside Zapadka.
    db.query("CREATE TABLE public.orders (id bigint, total numeric)");
    project.nontransactional_migration(
        "add-orders-index",
        &[],
        "CREATE INDEX CONCURRENTLY orders_total_idx ON public.orders (total);",
    );

    let report = project.report(&["deploy", "--uri", &db.uri()]);
    report.assert_success();
    assert_eq!(report.slugs_with_status("succeeded"), ["add-orders-index"]);
    assert!(
        db.has_relation("public.orders_total_idx"),
        "the index should exist"
    );

    // The attempt is recorded before the statement runs and cleared after, so
    // the history shows both and the table is empty.
    let actions = db.query(
        "SELECT action, outcome FROM zapadka.events WHERE migration_id IS NOT NULL ORDER BY sequence",
    );
    assert_eq!(
        actions,
        vec![
            vec!["deploy".to_owned(), "attempted".to_owned()],
            vec!["deploy".to_owned(), "succeeded".to_owned()],
        ],
        "the attempt must be recorded before the outcome"
    );
    assert_eq!(
        db.scalar("SELECT count(*) FROM zapadka.nontransactional_attempts"),
        "0",
        "a resolved attempt leaves no row behind"
    );
}

#[test]
fn a_nontransactional_statement_the_server_refuses_still_blocks_the_target() {
    let db = database();
    let project = project();
    project.nontransactional_migration(
        "bad-index",
        &[],
        "CREATE INDEX CONCURRENTLY bad_idx ON public.does_not_exist (id);",
    );

    let report = project.report(&["deploy", "--uri", &db.uri()]);
    report.assert_failed("deploy.failed", 9);

    // An error from the server is not proof that nothing happened. A failed
    // CREATE INDEX CONCURRENTLY leaves an invalid index behind, and an
    // automatic retry would then fail on a name that already exists -- after
    // the operator had been told the target was clean. So the attempt survives
    // and a person decides.
    assert_eq!(
        db.scalar("SELECT count(*) FROM zapadka.nontransactional_attempts"),
        "1"
    );
    let status = project.report(&["status", "--uri", &db.uri()]);
    status.assert_success();
    assert!(status.diagnostic_codes().contains(&"target.blocked"));

    // And it is recoverable without ceremony once they have looked.
    project
        .report(&["resolve", "bad-index", "--not-applied", "--uri", &db.uri()])
        .assert_success();
    assert_eq!(
        db.scalar("SELECT count(*) FROM zapadka.nontransactional_attempts"),
        "0"
    );
}

#[test]
fn a_nontransactional_migration_that_would_release_the_lock_is_rejected() {
    let project = project();
    // DISCARD ALL is implemented partly as pg_advisory_unlock_all(), and the
    // deployment lock is session-scoped: running it would hand the lock back
    // mid-deploy and let a second run start alongside this one.
    project.nontransactional_migration("discard", &[], "DISCARD ALL;");

    let report = project.report(&["lint"]);
    report.assert_failed("execution.mode_unsupported", 4);
}

#[test]
fn an_unresolved_attempt_blocks_deploys_until_it_is_resolved() {
    let db = database();
    let project = project();
    // A first deploy creates the registry, which the simulated crash needs.
    project.migration(
        "orders",
        &[],
        "CREATE TABLE public.orders (id bigint, total numeric);",
    );
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    let blocked = project.nontransactional_migration(
        "add-index",
        &[],
        "CREATE INDEX CONCURRENTLY idx ON public.orders (total);",
    );
    let later = project.migration("later", &[], "CREATE TABLE public.later (id bigint);");

    // Simulate a run that died mid-statement: the attempt row is exactly what
    // such a run leaves behind, because it is committed before the statement.
    insert_attempt(&db, blocked, "add-index");

    let report = project.report(&["deploy", "--uri", &db.uri()]);
    report.assert_failed("registry.blocked", 8);
    assert!(
        !db.has_relation("public.later"),
        "nothing after the block may be deployed"
    );
    assert_eq!(report.error_context("migration_id"), blocked.to_string());

    // status reports the block rather than refusing: it is how someone finds out.
    let status = project.report(&["status", "--uri", &db.uri()]);
    status.assert_success();
    assert!(status.diagnostic_codes().contains(&"target.blocked"));
    // And says so in the structured report, not only in a warning. It is
    // neither applied nor pending, and calling it either would be a lie
    // automation would act on.
    assert_eq!(status.slugs_with_status("blocked"), ["add-index"]);
    assert!(
        !status.slugs_with_status("pending").contains(&"add-index"),
        "a blocked migration must not also be listed as pending: {:?}",
        status.slugs_with_status("pending")
    );

    // The operator looks, decides the index is not there, and says so.
    let resolved = project.report(&[
        "resolve",
        &blocked.to_string(),
        "--not-applied",
        "--uri",
        &db.uri(),
    ]);
    resolved.assert_success();
    assert!(
        resolved
            .diagnostic_codes()
            .contains(&"resolve.asserted_by_operator"),
        "an asserted outcome must be marked as asserted: {:?}",
        resolved.diagnostic_codes()
    );

    // Unblocked, and the migration is pending again rather than applied.
    let after = project.report(&["deploy", "--uri", &db.uri()]);
    after.assert_success();
    assert!(db.has_relation("public.later"));
    assert!(db.has_relation("public.idx"));
    let _ = later;
}

#[test]
fn resolving_as_applied_records_it_without_running_any_sql() {
    let db = database();
    let project = project();
    project.migration(
        "orders",
        &[],
        "CREATE TABLE public.orders (id bigint, total numeric);",
    );
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    let blocked = project.nontransactional_migration(
        "add-index",
        &[],
        "CREATE INDEX CONCURRENTLY idx ON public.orders (total);",
    );
    insert_attempt(&db, blocked, "add-index");

    let report = project.report(&[
        "resolve",
        &blocked.to_string(),
        "--applied",
        "--uri",
        &db.uri(),
    ]);
    report.assert_success();

    // Recorded as applied, and no index was created: resolve runs no user SQL.
    assert_eq!(
        db.scalar(&format!(
            "SELECT count(*) FROM zapadka.applied_migrations WHERE migration_id = '{blocked}'"
        )),
        "1"
    );
    assert!(
        !db.has_relation("public.idx"),
        "resolve records a claim; it does not run the migration"
    );
    // The history says a person asserted this, not that Zapadka watched it.
    assert_eq!(
        db.scalar("SELECT outcome FROM zapadka.events WHERE action = 'resolve'"),
        "asserted_applied"
    );
}

#[test]
fn every_command_that_acts_on_a_blocked_target_refuses() {
    let db = database();
    let project = project();
    let id = project.migration(
        "orders",
        &[],
        "CREATE TABLE public.orders (id bigint, total numeric);",
    );
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();

    // `test` returns early when a project has no test files, so it needs one
    // here to reach the guard at all.
    project.test_file(
        "tests/db/orders.sql",
        "SELECT zapadka_test.plan(1);\nSELECT zapadka_test.ok(true, 'x');\nSELECT zapadka_test.finish();\n",
    );

    let blocked = project.nontransactional_migration(
        "add-index",
        &[],
        "CREATE INDEX CONCURRENTLY idx ON public.orders (total);",
    );
    insert_attempt(&db, blocked, "add-index");

    // Anything that would act on the schema refuses; only reporting continues.
    let uri = db.uri();
    let target = id.to_string();
    for args in [
        vec!["deploy"],
        vec!["verify"],
        vec!["revert", &target],
        vec!["test"],
    ] {
        let mut argv = args.clone();
        argv.extend_from_slice(&["--uri", &uri]);
        let report = project.report(&argv);
        assert_eq!(
            report.error_code(),
            "registry.blocked",
            "`zapadka {}` must refuse a blocked target",
            args.join(" ")
        );
    }

    project
        .report(&["status", "--uri", &db.uri()])
        .assert_success();
}

#[test]
fn resolve_refuses_without_being_told_what_happened() {
    let db = database();
    let project = project();
    let id = project.nontransactional_migration(
        "add-index",
        &[],
        "CREATE INDEX CONCURRENTLY idx ON public.orders (total);",
    );

    let report = project.report(&["resolve", &id.to_string(), "--uri", &db.uri()]);
    report.assert_failed("resolve.nothing_to_resolve", 8);
}

#[test]
fn a_nontransactional_migration_whose_statement_needs_no_such_mode_is_rejected() {
    let project = project();
    // A `CALL` is the case that matters: a procedure can COMMIT some work and
    // then raise, so the server error the runner treats as "nothing happened"
    // would be a lie, and a retry would duplicate the committed part.
    project.nontransactional_migration("call-a-procedure", &[], "CALL do_the_thing();");

    let report = project.report(&["lint"]);
    report.assert_failed("execution.mode_unsupported", 4);
}

#[test]
fn a_nontransactional_migration_with_two_statements_is_rejected_before_connecting() {
    let project = project();
    project.nontransactional_migration(
        "two-statements",
        &[],
        "CREATE INDEX CONCURRENTLY a_idx ON public.t (x); CREATE INDEX CONCURRENTLY b_idx ON public.t (y);",
    );

    let report = project.report(&["lint"]);
    report.assert_failed("script.statement_count", 4);
}
