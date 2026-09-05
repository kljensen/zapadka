use super::*;
use crate::history::tests::{migration, record_of};
use std::collections::BTreeMap;

fn state(migrations: &[Migration]) -> RegistryState {
    RegistryState {
        format_version: Some(2),
        project_id: Some(Uuid::nil()),
        unresolved: BTreeMap::new(),
        applied: migrations
            .iter()
            .map(|migration| (migration.id, record_of(migration)))
            .collect(),
    }
}

#[test]
fn batch_distinguishes_verified_accepted_and_pending_without_mutation() {
    let first = migration(1, &[], "SELECT 1;");
    let second = migration(2, &[], "SELECT 2;");
    let state = state(&[first.clone(), second.clone()]);
    let graph = Graph::build(vec![
        first,
        migration(2, &[], "SELECT 3;"),
        migration(3, &[], "SELECT 4;"),
    ])
    .unwrap();
    let preview = plan(&graph, &state, false).unwrap();
    assert_eq!(preview.entries.len(), 2);
    assert_eq!(preview.entries[0].disposition, Disposition::Verified);
    assert_eq!(preview.entries[1].disposition, Disposition::SourceMismatch);
    assert!(preview.validate().is_err());
    let accepted = plan(&graph, &state, true).unwrap();
    assert!(accepted.validate().is_ok());
    assert_eq!(accepted.entries[1].disposition, Disposition::Accepted);
    assert_eq!(
        accepted.entries[1].original_deploy_sha256,
        second.deploy.sha256
    );
    assert_ne!(
        accepted.entries[1].current_deploy_sha256.as_ref(),
        Some(&second.deploy.sha256)
    );
    assert_eq!(state.format_version, Some(2));
    assert_eq!(
        state.applied[&second.id].definition_algorithm,
        registry::RAW_V1
    );
}

#[test]
fn structural_rows_are_idempotent_and_cannot_be_reaccepted() {
    let original = migration(1, &[], "SELECT 1;");
    let mut state = state(std::slice::from_ref(&original));
    let row = state.applied.get_mut(&original.id).unwrap();
    row.definition_algorithm = registry::STRUCTURAL_V1.to_owned();
    row.definition_sha256 = original.structural_definition_sha256().unwrap();
    let graph = Graph::build(vec![migration(1, &[], "-- cosmetic\nSELECT 1 ;")]).unwrap();
    let repeat = plan(&graph, &state, true).unwrap();
    assert!(repeat.validate().is_ok());
    assert!(repeat.is_empty());
    assert_eq!(repeat.entries[0].disposition, Disposition::AlreadyCurrent);
    let graph = Graph::build(vec![migration(1, &[], "SELECT 2;")]).unwrap();
    let changed = plan(&graph, &state, true).unwrap();
    assert_eq!(changed.entries[0].disposition, Disposition::Blocked);
    assert!(changed.validate().is_err());
}

#[test]
fn acceptance_cannot_bypass_metadata_parse_or_execution_boundaries() {
    let original = migration(1, &[], "SELECT 1;");
    let state = state(std::slice::from_ref(&original));
    for sql in ["SELECT (", "BEGIN; SELECT 1; COMMIT;"] {
        let graph = Graph::build(vec![migration(1, &[], sql)]).unwrap();
        let preview = plan(&graph, &state, true).unwrap();
        assert_eq!(preview.entries[0].disposition, Disposition::Blocked);
        assert!(preview.validate().is_err());
    }
    let mut changed = original.clone();
    changed.manifest.transaction = Transaction::Forbidden;
    let graph = Graph::build(vec![changed]).unwrap();
    assert!(plan(&graph, &state, true).unwrap().validate().is_err());
    let graph = Graph::build(Vec::new()).unwrap();
    assert_eq!(
        plan(&graph, &state, true)
            .unwrap()
            .validate()
            .unwrap_err()
            .code,
        ErrorCode::HistoryMigrationMissing
    );
}

#[test]
fn legacy_empty_deploy_transitions_under_existing_noop_policy() {
    let original = migration(1, &[], "-- documentation only\n");
    let state = state(std::slice::from_ref(&original));
    let graph = Graph::build(vec![original]).unwrap();
    let preview = plan(&graph, &state, false).unwrap();
    assert!(preview.validate().is_ok());
    assert_eq!(preview.entries[0].disposition, Disposition::Verified);
}

#[test]
fn uninitialized_and_unresolved_targets_are_rejected() {
    let original = migration(1, &[], "SELECT 1;");
    let graph = Graph::build(vec![original.clone()]).unwrap();
    let mut state = state(std::slice::from_ref(&original));
    state.format_version = None;
    assert_eq!(
        plan(&graph, &state, true).unwrap_err().code,
        ErrorCode::RegistryNotInitialized
    );
    state.format_version = Some(2);
    state.unresolved.insert(
        original.id,
        registry::UnresolvedAttempt {
            id: original.id,
            slug: original.slug,
            definition_algorithm: registry::RAW_V1.to_owned(),
            definition_sha256: original.definition_sha256,
            deploy_sha256: original.deploy.sha256,
            depends: Vec::new(),
            started_at: "2026-01-01".to_owned(),
            run_id: Uuid::nil(),
            session_user_name: "tester".to_owned(),
        },
    );
    assert_eq!(
        plan(&graph, &state, true).unwrap_err().code,
        ErrorCode::RegistryBlocked
    );
}
