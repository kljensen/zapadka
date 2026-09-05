//! Canonicalization must finish before runner SQL or registry writes.

use super::harness::{database, project};
use zapadka_core::error::ErrorCode;
use zapadka_pg::{Runner, Timeouts};

#[test]
fn direct_runner_rejects_uncanonicalizable_sql_before_any_effects() {
    let db = database();
    let project = project();
    project
        .report(&["deploy", "--uri", &db.uri()])
        .assert_success();
    db.query(
        "CREATE SEQUENCE public.canonical_counter; SELECT nextval('public.canonical_counter')",
    );
    let before = db.scalar("SELECT last_value FROM public.canonical_counter");
    let sql = format!(
        "SELECT nextval('public.canonical_counter'); SELECT {}",
        vec!["1"; 100].join(" + ")
    );
    let id = project.migration("deep", &[], &sql);
    let migration =
        zapadka_core::migration::read(project.root(), &project.migration_dir(id)).unwrap();

    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let resolved = zapadka_pg::resolve("test", None, Some(&db.uri())).unwrap();
        let connection = zapadka_pg::connect(&resolved).await.unwrap();
        let facts = zapadka_pg::registry::server_facts(&connection.client)
            .await
            .unwrap();
        let mut runner = Runner::new(
            connection.client,
            "zapadka".to_owned(),
            uuid::Uuid::now_v7(),
            facts,
            "test".to_owned(),
            Timeouts::default(),
            connection.server_messages,
        );
        assert_eq!(
            runner.deploy(&migration).await.unwrap_err().code,
            ErrorCode::ScriptParseError
        );
        assert_eq!(
            runner
                .deploy_nontransactional(&migration)
                .await
                .unwrap_err()
                .code,
            ErrorCode::ScriptParseError
        );
        assert_eq!(
            runner.baseline(&[&migration]).await.unwrap_err().code,
            ErrorCode::ScriptParseError
        );
    });

    assert_eq!(
        db.scalar("SELECT last_value FROM public.canonical_counter"),
        before
    );
    assert_eq!(
        db.scalar("SELECT count(*) FROM zapadka.applied_migrations"),
        "0"
    );
    assert_eq!(
        db.scalar("SELECT count(*) FROM zapadka.nontransactional_attempts"),
        "0"
    );
    assert_eq!(
        db.scalar("SELECT count(*) FROM zapadka.events WHERE migration_id IS NOT NULL"),
        "0"
    );
}
