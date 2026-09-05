//! Cross-version transitions using frozen v0.5.4 registry DDL and hashing.

use super::{exit, harness};
use harness::{Database, Project, database, project};
use sha2::{Digest, Sha256};
use uuid::Uuid;

trait CheckedReport {
    fn checked_report(&self, args: &[&str]) -> harness::Report;
}

impl CheckedReport for Project {
    fn checked_report(&self, args: &[&str]) -> harness::Report {
        let report = self.report(args);
        super::report_contract::validate(&report.json);
        report
    }
}

fn digest(bytes: &[u8]) -> String {
    use std::fmt::Write;
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut text, byte| {
            write!(&mut text, "{byte:02x}").unwrap();
            text
        })
}

// Kept independent of the current Manifest implementation: this is the exact
// v0.5.4 wire format, so accidentally changing both producer and consumer
// cannot make a compatibility regression pass.
pub(super) fn legacy_identity(project: &Project, id: Uuid) -> (String, String, String, String) {
    let dir = project.migration_dir(id);
    let manifest = zapadka_core::manifest::Manifest::parse(
        &std::fs::read_to_string(dir.join("migration.toml")).unwrap(),
        "migration.toml",
    )
    .unwrap();
    let sql = std::fs::read(dir.join("deploy.sql")).unwrap();
    let mut depends: Vec<_> = manifest.depends.iter().map(Uuid::to_string).collect();
    depends.sort_unstable();
    let mode = manifest.transaction.as_str();
    let bytes = digest(&sql);
    let canonical = format!(
        "zapadka.migration.v1\nid={id}\ntransaction={mode}\ndepends={}\ndeploy_sha256={bytes}\n",
        depends.join(",")
    );
    (
        digest(canonical.as_bytes()),
        bytes,
        format!("{{{}}}", depends.join(",")),
        mode.to_owned(),
    )
}

pub(super) fn seed_legacy(db: &Database, project: &Project, version: i32, ids: &[Uuid]) {
    harness::try_sql(db, include_str!("../fixtures/registry-v1.sql")).unwrap();
    if version == 2 {
        harness::try_sql(db, include_str!("../fixtures/registry-v2.sql")).unwrap();
    }
    let config = std::fs::read_to_string(project.root().join("zapadka.toml")).unwrap();
    let project_id = config
        .lines()
        .find_map(|line| line.strip_prefix("id = "))
        .unwrap()
        .trim_matches('"');
    harness::try_sql(db, &format!("INSERT INTO zapadka.meta(project_id,registry_format_version,created_by) VALUES ('{project_id}',{version},'0.5.4');")).unwrap();
    for (index, id) in ids.iter().enumerate() {
        let dir = project.migration_dir(*id);
        let sql = std::fs::read_to_string(dir.join("deploy.sql")).unwrap();
        harness::try_sql(db, &sql).unwrap();
        let slug = dir.file_name().unwrap().split_at(37).1;
        let (definition, bytes, depends, mode) = legacy_identity(project, *id);
        harness::try_sql(db, &format!("INSERT INTO zapadka.applied_migrations(migration_id,slug,definition_sha256,deploy_sha256,depends,transaction_mode,applied_at,run_id) VALUES ('{id}','{slug}','{definition}','{bytes}','{depends}','{mode}','2026-01-01T00:00:00Z','00000000-0000-0000-0000-000000000001'); INSERT INTO zapadka.events(run_id,sequence,migration_id,action,outcome,transaction_mode,definition_sha256,script_role,script_sha256,session_user_name,current_user_name,server_version,zapadka_version) VALUES ('00000000-0000-0000-0000-000000000001',{index},'{id}','deploy','succeeded','{mode}','{definition}','deploy','{bytes}','legacy-actor','legacy-actor','18.4','0.5.4');")).unwrap();
    }
}

fn deployment_evidence(db: &Database) -> String {
    db.scalar("SELECT coalesce(jsonb_agg(to_jsonb(m) - 'definition_sha256' - 'definition_algorithm' ORDER BY migration_id)::text,'[]') FROM zapadka.applied_migrations m")
}

fn old_events(db: &Database) -> String {
    db.scalar("SELECT coalesce(jsonb_agg(to_jsonb(e) - 'definition_algorithm' ORDER BY sequence)::text,'[]') FROM zapadka.events e WHERE zapadka_version = '0.5.4'")
}

#[test]
fn frozen_v1_and_v2_transition_preserves_evidence_pending_and_idempotency() {
    for version in [1, 2] {
        let db = database();
        let project = project();
        let applied = project.migration(
            "counter",
            &[],
            "CREATE SEQUENCE public.rehash_counter; SELECT nextval('public.rehash_counter');",
        );
        seed_legacy(&db, &project, version, &[applied]);
        project.migration(
            "pending",
            &[applied],
            "CREATE TABLE public.rehash_pending(n integer);",
        );
        let facts = deployment_evidence(&db);
        let events = old_events(&db);
        let original = legacy_identity(&project, applied);
        let preview = project.checked_report(&["rehash", "--uri", &db.uri(), "--dry-run"]);
        assert_eq!(preview.json["target"]["registry_format_version"], version);
        super::report_contract::validate(&preview.json);
        preview.assert_success();
        assert_eq!(preview.migrations()[0]["rehash"]["disposition"], "verified");
        assert_eq!(preview.migrations()[0]["definition_sha256"], original.0);
        assert_eq!(
            db.scalar("SELECT registry_format_version FROM zapadka.meta"),
            version.to_string()
        );
        assert!(!db.has_relation("zapadka.rehash_events"));
        let converted = project.checked_report(&["rehash", "--uri", &db.uri()]);
        converted.assert_success();
        assert_eq!(converted.json["target"]["registry_format_version"], 3);
        assert_eq!(deployment_evidence(&db), facts);
        assert_eq!(old_events(&db), events);
        assert_eq!(
            db.scalar("SELECT last_value FROM public.rehash_counter"),
            "1",
            "rehash must execute no user SQL"
        );
        assert!(!db.has_relation("public.rehash_pending"));
        assert_eq!(
            db.scalar(
                "SELECT provenance || '|' || original_deploy_sha256 FROM zapadka.rehash_events"
            ),
            format!("verified|{}", original.1)
        );
        project
            .checked_report(&["rehash", "--uri", &db.uri()])
            .assert_success();
        assert_eq!(db.scalar("SELECT count(*) FROM zapadka.rehash_events"), "1");
        project.rewrite_deploy(applied, "-- cosmetic\nCREATE SEQUENCE public.rehash_counter;\nSELECT nextval('public.rehash_counter') ;\n");
        project
            .checked_report(&["status", "--uri", &db.uri()])
            .assert_success();
        for sql in [
            "UPDATE zapadka.rehash_events SET reason = 'erased'",
            "DELETE FROM zapadka.rehash_events",
            "TRUNCATE zapadka.rehash_events",
        ] {
            assert!(harness::try_sql(&db, sql).is_err());
        }
    }
}

#[test]
fn explicit_acceptance_retains_assertion_provenance_and_never_resets_structural_history() {
    let db = database();
    let project = project();
    let first = project.migration(
        "first",
        &[],
        "CREATE TABLE public.accept_first(n integer DEFAULT 1);",
    );
    let second = project.migration(
        "second",
        &[],
        "CREATE TABLE public.accept_second(n integer);",
    );
    seed_legacy(&db, &project, 2, &[first, second]);
    let evidence = deployment_evidence(&db);
    project.rewrite_deploy(
        first,
        "CREATE TABLE public.accept_first(n integer DEFAULT 2);",
    );
    project
        .checked_report(&["rehash", "--uri", &db.uri()])
        .assert_failed("history.definition_changed", exit::HISTORY);
    assert_eq!(
        db.scalar("SELECT registry_format_version FROM zapadka.meta"),
        "2"
    );
    assert_ne!(
        project
            .run(&["rehash", "--uri", &db.uri(), "--accept-current"])
            .code,
        0
    );
    assert_ne!(
        project
            .run(&[
                "rehash",
                "--uri",
                &db.uri(),
                "--accept-current",
                "--reason",
                "  "
            ])
            .code,
        0
    );
    let accepted = project.checked_report(&[
        "rehash",
        "--uri",
        &db.uri(),
        "--accept-current",
        "--reason",
        "Reviewed current source",
    ]);
    accepted.assert_success();
    assert_eq!(
        accepted.migrations()[0]["rehash"]["disposition"],
        "accepted"
    );
    assert_eq!(
        accepted.migrations()[0]["rehash"]["reason"],
        "Reviewed current source"
    );
    assert_eq!(
        db.scalar(
            "SELECT string_agg(provenance, ',' ORDER BY migration_id) FROM zapadka.rehash_events"
        ),
        "operator-accepted,verified"
    );
    assert_eq!(deployment_evidence(&db), evidence);
    assert_eq!(db.scalar("SELECT column_default FROM information_schema.columns WHERE table_name='accept_first' AND column_name='n'"), "1", "acceptance does not change the database");
    project.rewrite_deploy(
        first,
        "CREATE TABLE public.accept_first(n integer DEFAULT 3);",
    );
    project
        .checked_report(&[
            "rehash",
            "--uri",
            &db.uri(),
            "--accept-current",
            "--reason",
            "try again",
        ])
        .assert_failed("history.definition_changed", exit::HISTORY);
    assert_eq!(db.scalar("SELECT count(*) FROM zapadka.rehash_events"), "2");
}

#[test]
fn mixed_targets_and_fresh_structural_rows_validate_independently() {
    let staging = database();
    let production = database();
    let project = project();
    let first = project.migration("base", &[], "CREATE TABLE public.mixed_base(n integer);");
    seed_legacy(&staging, &project, 1, &[first]);
    seed_legacy(&production, &project, 2, &[first]);
    let second = project.migration("new", &[first], "CREATE TABLE public.mixed_new(n integer);");
    project
        .checked_report(&["deploy", "--uri", &staging.uri()])
        .assert_success();
    assert_eq!(staging.scalar("SELECT string_agg(definition_algorithm, ',' ORDER BY migration_id) FROM zapadka.applied_migrations"), "raw-v1,structural-v1");
    project
        .checked_report(&["rehash", "--uri", &staging.uri()])
        .assert_success();
    project.rewrite_deploy(
        first,
        "-- reformatted\n CREATE TABLE public.mixed_base ( n integer );\n",
    );
    project
        .checked_report(&["status", "--uri", &staging.uri()])
        .assert_success();
    project
        .checked_report(&["status", "--uri", &production.uri()])
        .assert_failed("history.definition_changed", exit::HISTORY);
    project
        .checked_report(&[
            "rehash",
            "--uri",
            &production.uri(),
            "--accept-current",
            "--reason",
            "Adopt formatting",
        ])
        .assert_success();
    assert!(!production.has_relation("public.mixed_new"));
    project.rewrite_deploy(second, "CREATE TABLE public.mixed_new(n bigint);");
    project
        .checked_report(&["status", "--uri", &staging.uri()])
        .assert_failed("history.definition_changed", exit::HISTORY);
}

#[test]
fn read_only_roles_can_preview_legacy_and_structural_targets() {
    let db = database();
    let project = project();
    let id = project.migration("readonly", &[], "SELECT 1;");
    seed_legacy(&db, &project, 2, &[id]);
    let role = format!("reader_{}", Uuid::now_v7().simple());
    harness::try_sql(&db, &format!("CREATE ROLE {role} LOGIN PASSWORD 'readonly'; GRANT USAGE ON SCHEMA zapadka TO {role}; GRANT SELECT ON ALL TABLES IN SCHEMA zapadka TO {role}; ALTER ROLE {role} SET default_transaction_read_only=on;")).unwrap();
    let uri = db.uri_as(&role, "readonly");
    project
        .checked_report(&["status", "--uri", &uri])
        .assert_success();
    project
        .checked_report(&["rehash", "--uri", &uri, "--dry-run"])
        .assert_success();
    assert_eq!(
        db.scalar("SELECT registry_format_version FROM zapadka.meta"),
        "2"
    );
    project
        .checked_report(&["rehash", "--uri", &db.uri()])
        .assert_success();
    project
        .checked_report(&["status", "--uri", &uri])
        .assert_success();
    project
        .checked_report(&["rehash", "--uri", &uri, "--dry-run"])
        .assert_success();
}

#[test]
fn audit_failure_rolls_back_all_comparison_updates_and_retry_is_clean() {
    let db = database();
    let project = project();
    let first = project.migration("one", &[], "SELECT 1;");
    let second = project.migration("two", &[], "SELECT 2;");
    seed_legacy(&db, &project, 2, &[first, second]);
    // A normal mutating command upgrades schema while keeping applied rows raw.
    project
        .checked_report(&["deploy", "--uri", &db.uri()])
        .assert_success();
    let evidence = deployment_evidence(&db);
    harness::try_sql(&db, &format!("CREATE FUNCTION public.fail_second_rehash() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.migration_id = '{second}' THEN RAISE EXCEPTION 'injected audit failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER injected BEFORE INSERT ON zapadka.rehash_events FOR EACH ROW EXECUTE FUNCTION public.fail_second_rehash();")).unwrap();
    project
        .checked_report(&["rehash", "--uri", &db.uri()])
        .assert_failed("registry.upgrade_failed", exit::REGISTRY);
    assert_eq!(
        db.scalar(
            "SELECT count(*) FROM zapadka.applied_migrations WHERE definition_algorithm='raw-v1'"
        ),
        "2"
    );
    assert_eq!(db.scalar("SELECT count(*) FROM zapadka.rehash_events"), "0");
    assert_eq!(deployment_evidence(&db), evidence);
    harness::try_sql(&db, "DROP TRIGGER injected ON zapadka.rehash_events").unwrap();
    project
        .checked_report(&["rehash", "--uri", &db.uri()])
        .assert_success();
    assert_eq!(db.scalar("SELECT count(*) FROM zapadka.rehash_events"), "2");
}

#[test]
fn a_failed_upgrade_and_rehash_leave_a_legacy_registry_intact() {
    let db = database();
    let project = project();
    let id = project.migration("legacy", &[], "SELECT 1;");
    seed_legacy(&db, &project, 1, &[id]);
    let evidence = deployment_evidence(&db);
    // Conflict late in v3 DDL, after v2 and algorithm columns were created.
    harness::try_sql(&db, "CREATE TABLE zapadka.rehash_events(blocker integer)").unwrap();
    let rolled_back = project.checked_report(&["rehash", "--uri", &db.uri()]);
    rolled_back.assert_failed("registry.upgrade_failed", exit::REGISTRY);
    assert_eq!(rolled_back.json["target"]["registry_format_version"], 1);
    assert_eq!(
        db.scalar("SELECT registry_format_version FROM zapadka.meta"),
        "1"
    );
    assert!(!db.has_relation("zapadka.nontransactional_attempts"));
    assert_eq!(deployment_evidence(&db), evidence);
    assert_eq!(db.scalar("SELECT count(*) FROM information_schema.columns WHERE table_schema='zapadka' AND table_name='applied_migrations' AND column_name='definition_algorithm'"), "0");
    harness::try_sql(&db, "DROP TABLE zapadka.rehash_events").unwrap();
    project
        .checked_report(&["rehash", "--uri", &db.uri()])
        .assert_success();
}

#[test]
fn rehash_contends_on_the_same_lock_as_deploy_and_revert() {
    let db = database();
    let project = project();
    let id = project.migration_with(
        "locked",
        &[],
        "CREATE TABLE public.locked(n int);",
        Some("DROP TABLE public.locked;"),
        None,
    );
    seed_legacy(&db, &project, 2, &[id]);
    let held = harness::hold_deployment_lock(&db, project.root());
    for command in [vec!["rehash"], vec!["deploy"], vec!["revert", "locked"]] {
        let mut args = command;
        let uri = db.uri();
        args.extend(["--uri", &uri, "--wait", "100ms"]);
        let run = project.run(&args);
        assert_eq!(run.code, exit::LOCK, "{}", run.stderr);
    }
    project
        .checked_report(&["rehash", "--uri", &db.uri(), "--dry-run"])
        .assert_success();
    assert_eq!(
        db.scalar("SELECT registry_format_version FROM zapadka.meta"),
        "2"
    );
    drop(held);
    project
        .checked_report(&["rehash", "--uri", &db.uri()])
        .assert_success();
}

#[test]
fn acceptance_never_bypasses_missing_source_metadata_or_invalid_sql() {
    for edit in ["missing", "dependencies", "mode", "parse", "transaction"] {
        let db = database();
        let project = project();
        let first = project.migration("base", &[], "SELECT 1;");
        let second = project.migration("edited", &[], "SELECT 2;");
        seed_legacy(&db, &project, 2, &[first, second]);
        let evidence = deployment_evidence(&db);
        let events = old_events(&db);
        match edit {
            "missing" => project.delete_migration(second),
            "dependencies" | "mode" => {
                let path = project.migration_dir(second).join("migration.toml");
                let source = std::fs::read_to_string(&path).unwrap();
                let edited = if edit == "dependencies" {
                    source.replace("depends = []", &format!("depends = [\"{first}\"]"))
                } else {
                    source.replace("transaction = \"required\"", "transaction = \"forbidden\"")
                };
                std::fs::write(path, edited).unwrap();
            }
            "parse" => project.rewrite_deploy(second, "SELECT ("),
            _ => project.rewrite_deploy(second, "BEGIN; SELECT 2; COMMIT;"),
        }
        for dry_run in [true, false] {
            let uri = db.uri();
            let mut args = vec![
                "rehash",
                "--uri",
                &uri,
                "--accept-current",
                "--reason",
                "Cannot override invariants",
            ];
            if dry_run {
                args.push("--dry-run");
            }
            let report = project.checked_report(&args);
            assert_ne!(report.code(), 0, "{edit} unexpectedly accepted");
            assert_eq!(
                report.migrations().last().unwrap()["rehash"]["disposition"],
                "blocked"
            );
        }
        assert_eq!(
            db.scalar("SELECT registry_format_version FROM zapadka.meta"),
            "2"
        );
        assert_eq!(deployment_evidence(&db), evidence);
        assert_eq!(old_events(&db), events);
        assert!(!db.has_relation("zapadka.rehash_events"));
    }
}

#[test]
fn unknown_algorithms_and_newer_registries_fail_without_changes() {
    let db = database();
    let project = project();
    project.migration("new", &[], "SELECT 1;");
    project
        .checked_report(&["deploy", "--uri", &db.uri()])
        .assert_success();
    harness::try_sql(
        &db,
        "UPDATE zapadka.applied_migrations SET definition_algorithm='future-v9'",
    )
    .unwrap();
    for command in ["status", "rehash"] {
        project
            .checked_report(&[command, "--uri", &db.uri()])
            .assert_failed("registry.format_too_new", exit::REGISTRY);
    }
    assert_eq!(
        db.scalar("SELECT definition_algorithm FROM zapadka.applied_migrations"),
        "future-v9"
    );
    harness::try_sql(&db, "UPDATE zapadka.applied_migrations SET definition_algorithm='structural-v1'; UPDATE zapadka.meta SET registry_format_version=99").unwrap();
    project
        .checked_report(&["status", "--uri", &db.uri()])
        .assert_failed("registry.format_too_new", exit::REGISTRY);
    project
        .checked_report(&["rehash", "--uri", &db.uri(), "--dry-run"])
        .assert_failed("registry.format_too_new", exit::REGISTRY);
    assert_eq!(
        db.scalar("SELECT registry_format_version FROM zapadka.meta"),
        "99"
    );
    assert_eq!(db.scalar("SELECT count(*) FROM zapadka.rehash_events"), "0");
}

#[test]
fn concurrent_rehashes_reread_after_lock_and_emit_one_transition_each() {
    let db = database();
    let project = project();
    let id = project.migration("once", &[], "SELECT 1;");
    seed_legacy(&db, &project, 2, &[id]);
    let held = harness::hold_deployment_lock(&db, project.root());
    std::thread::scope(|scope| {
        // Own the guard inside the scope callback so even an unexpected panic
        // releases it before thread::scope joins clients waiting indefinitely.
        let held = held;
        let first =
            scope.spawn(|| project.checked_report(&["rehash", "--uri", &db.uri(), "--wait", "0s"]));
        let second =
            scope.spawn(|| project.checked_report(&["rehash", "--uri", &db.uri(), "--wait", "0s"]));
        // Observe both processes genuinely queued on the deployment lock.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let queued = loop {
            let observed = harness::try_sql(
                &db,
                "SELECT count(*) FROM pg_locks l JOIN pg_stat_activity a ON a.pid=l.pid WHERE a.datname=current_database() AND l.locktype='advisory' AND NOT l.granted",
            );
            match observed {
                Ok(count) if count.trim() == "2" => break Ok(true),
                Err(error) => break Err(error),
                Ok(_) if std::time::Instant::now() >= deadline => break Ok(false),
                Ok(_) => {}
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        drop(held);
        assert!(
            queued.as_ref().is_ok_and(|queued| *queued),
            "rehash clients never queued: {queued:?}"
        );
        let first = first.join().unwrap();
        let second = second.join().unwrap();
        first.assert_success();
        second.assert_success();
        assert_eq!(first.json["target"]["registry_format_version"], 3);
        assert_eq!(second.json["target"]["registry_format_version"], 3);
        let mut dispositions = vec![
            first.migrations()[0]["rehash"]["disposition"]
                .as_str()
                .unwrap(),
            second.migrations()[0]["rehash"]["disposition"]
                .as_str()
                .unwrap(),
        ];
        dispositions.sort_unstable();
        assert_eq!(dispositions, ["already_current", "verified"]);
    });
    assert_eq!(db.scalar("SELECT count(*) FROM zapadka.rehash_events"), "1");
}

#[test]
fn formatting_before_and_after_transition_preserves_literal_boundaries() {
    let db = database();
    let project = project();
    let id = project.migration(
        "function",
        &[],
        "create function public.constant_value() returns integer language sql as $$ SELECT 1; $$;",
    );
    seed_legacy(&db, &project, 2, &[id]);
    let before = std::fs::read(project.migration_dir(id).join("deploy.sql")).unwrap();
    assert_eq!(project.run(&["format", "--write"]).code, 0);
    assert_ne!(
        std::fs::read(project.migration_dir(id).join("deploy.sql")).unwrap(),
        before
    );
    project
        .checked_report(&["status", "--uri", &db.uri()])
        .assert_failed("history.definition_changed", exit::HISTORY);
    project
        .checked_report(&[
            "rehash",
            "--uri",
            &db.uri(),
            "--accept-current",
            "--reason",
            "Adopt formatter output",
        ])
        .assert_success();
    let formatted = std::fs::read_to_string(project.migration_dir(id).join("deploy.sql")).unwrap();
    project.rewrite_deploy(id, &format!("-- outer comment\n{formatted}"));
    assert_eq!(project.run(&["format", "--write"]).code, 0);
    project
        .checked_report(&["status", "--uri", &db.uri()])
        .assert_success();
    project.rewrite_deploy(id, &formatted.replace("SELECT 1;", "SELECT 2;"));
    for command in ["status", "deploy", "verify"] {
        project
            .checked_report(&[command, "--uri", &db.uri()])
            .assert_failed("history.definition_changed", exit::HISTORY);
    }
    assert_eq!(db.scalar("SELECT public.constant_value()"), "1");
}

#[test]
fn empty_legacy_registry_rehash_reports_the_unchanged_version() {
    let db = database();
    let project = project();
    seed_legacy(&db, &project, 1, &[]);
    let report = project.checked_report(&["rehash", "--uri", &db.uri()]);
    report.assert_success();
    assert_eq!(report.json["target"]["registry_format_version"], 1);
    assert_eq!(
        db.scalar("SELECT registry_format_version FROM zapadka.meta"),
        "1"
    );
    assert!(!db.has_relation("zapadka.rehash_events"));
    for version in [0, -1] {
        harness::try_sql(
            &db,
            &format!("UPDATE zapadka.meta SET registry_format_version={version}"),
        )
        .unwrap();
        project
            .checked_report(&["status", "--uri", &db.uri()])
            .assert_failed("registry.upgrade_failed", exit::REGISTRY);
    }
}

#[test]
fn registry_read_uses_one_snapshot_when_v1_upgrades_and_new_attempt_commits() {
    let db = database();
    let project = project();
    let original = project.migration("original", &[], "SELECT 1;");
    seed_legacy(&db, &project, 1, &[original]);
    let newly_applied = project.migration("new-applied", &[], "SELECT 2;");
    let attempt = project.nontransactional_migration("new-attempt", &[], "SELECT 3;");
    let applied_identity = legacy_identity(&project, newly_applied);
    let attempt_identity = legacy_identity(&project, attempt);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let resolved = zapadka_pg::resolve("snapshot-test", None, Some(&db.uri())).unwrap();
        let mut writer = zapadka_pg::connect(&resolved).await.unwrap().client;
        let mut reader = zapadka_pg::connect(&resolved).await.unwrap().client;
        let state = zapadka_pg::registry::read(&mut writer, "zapadka").await.unwrap();
        let reader_pid: i32 = reader.query_one("SELECT pg_backend_pid()", &[]).await.unwrap().get(0);
        let transaction = writer.transaction().await.unwrap();
        transaction.batch_execute("LOCK TABLE zapadka.applied_migrations IN ACCESS EXCLUSIVE MODE").await.unwrap();
        let reading = tokio::spawn(async move {
            let state = zapadka_pg::registry::read(&mut reader, "zapadka").await;
            (reader, state)
        });
        // The writer owns the relation lock, so it can apply DDL while the
        // reader is paused after reading metadata and before reading state.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let waiting: bool = transaction.query_one("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE pid=$1 AND relation='zapadka.applied_migrations'::regclass AND NOT granted)", &[&reader_pid]).await.unwrap().get(0);
            if waiting { break; }
            assert!(std::time::Instant::now() < deadline, "reader did not wait on applied state");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        zapadka_pg::registry::upgrade_in_transaction(&transaction, "zapadka", state.project_id.unwrap(), "test", &state).await.unwrap();
        transaction.execute("INSERT INTO zapadka.applied_migrations(migration_id,slug,definition_sha256,deploy_sha256,depends,transaction_mode,run_id) VALUES($1,'new-applied',$2,$3,'{}','required',$4)", &[&newly_applied, &applied_identity.0, &applied_identity.1, &Uuid::nil()]).await.unwrap();
        transaction.execute("INSERT INTO zapadka.nontransactional_attempts(migration_id,slug,definition_sha256,deploy_sha256,depends,run_id,session_user_name,server_version,zapadka_version) VALUES($1,'new-attempt',$2,$3,'{}',$4,'test','18','test')", &[&attempt, &attempt_identity.0, &attempt_identity.1, &Uuid::nil()]).await.unwrap();
        transaction.commit().await.unwrap();
        let (mut reader, snapshot) = reading.await.unwrap();
        let snapshot = snapshot.unwrap();
        assert_eq!(snapshot.format_version, Some(1));
        assert_eq!(snapshot.applied.len(), 1, "must not combine v1 metadata with later applied rows");
        assert!(snapshot.unresolved.is_empty());
        let latest = zapadka_pg::registry::read(&mut reader, "zapadka").await.unwrap();
        assert_eq!(latest.format_version, Some(3));
        assert_eq!(latest.applied.len(), 2);
        assert!(latest.unresolved.contains_key(&attempt));
    });
}
