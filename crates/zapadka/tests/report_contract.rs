//! Validate real command output using the schema shipped to report consumers.
#![allow(clippy::panic)]
#![allow(unreachable_pub)]

use std::sync::OnceLock;

fn validator() -> &'static jsonschema::Validator {
    static VALIDATOR: OnceLock<jsonschema::Validator> = OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../../docs/report-v1.schema.json"))
                .expect("published schema is valid JSON");
        jsonschema::validator_for(&schema).expect("published schema is a valid JSON Schema")
    })
}

/// Checks the complete JSON document, including nested transition evidence.
pub fn validate(report: &serde_json::Value) {
    let errors: Vec<String> = validator()
        .iter_errors(report)
        .map(|error| error.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "report does not satisfy published schema: {errors:?}\n{report}"
    );
}

#[test]
fn published_schema_rejects_wrong_nested_rehash_evidence_types() {
    let mut report = serde_json::json!({
        "report_version": 1,
        "tool": { "name": "zapadka", "version": "0.6.0", "parser_version": 180_004 },
        "run": { "id": "00000000-0000-0000-0000-000000000000", "command": "rehash",
            "started_at": "2026-01-01T00:00:00Z", "finished_at": "2026-01-01T00:00:00Z", "duration_ms": 0 },
        "outcome": "success", "exit_code": 0,
        "migrations": [{ "id": "00000000-0000-0000-0000-000000000000", "slug": "orders",
            "action": "rehash", "status": "succeeded", "transaction": "required",
            "definition_sha256": "new", "definition_algorithm": "structural-v1",
            "rehash": { "original_deploy_sha256": "original", "current_deploy_sha256": "current",
                "committed": true, "disposition": "accepted", "reason": "reviewed source",
                "old_definition_sha256": "old", "old_definition_algorithm": "raw-v1",
                "new_definition_sha256": "new", "new_definition_algorithm": "structural-v1", "dry_run": false }
        }]
    });
    validate(&report);
    report["migrations"][0]["rehash"]["committed"] = serde_json::json!("yes");
    assert!(!validator().is_valid(&report));
}
