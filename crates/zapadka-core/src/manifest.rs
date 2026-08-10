//! The per-migration manifest, `migration.toml`, and its canonical form.
//!
//! # What is immutable
//!
//! A migration's *deployment definition* is the pair (canonical manifest,
//! `deploy.sql`). Once a migration is applied, that pair is frozen: changing it
//! means the database no longer matches the source that produced it, which
//! Zapadka reports as a history error rather than silently re-running or
//! ignoring. Corrective work is a new migration.
//!
//! The definition covers only what determines *how `deploy.sql` executes*: the
//! migration's identity, its dependency edges, and its transaction mode.
//!
//! `reversibility` and `irreversible_reason` are deliberately excluded. They
//! govern `revert.sql`, which ADR-0001 makes mutable — a team must be able to
//! write a revert script for an already-deployed migration. Excluding them also
//! keeps the definition honest: nothing in it changes what the deploy did.

use std::collections::BTreeSet;
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{Error, ErrorCode, Result};
use crate::report::{Location, TransactionMode};

/// The manifest file name inside a migration directory.
pub const MANIFEST_FILE_NAME: &str = "migration.toml";

/// The `format_version` this binary writes and understands.
pub const MANIFEST_FORMAT_VERSION: u32 = 1;

/// The prefix of the canonical manifest, which also versions the hashing
/// algorithm. Changing how definitions are canonicalized requires changing this
/// string, because every recorded hash in every registry depends on it.
const CANONICAL_HEADER: &str = "zapadka.migration.v1";

/// A parsed `migration.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The manifest schema version. Must be [`MANIFEST_FORMAT_VERSION`].
    pub format_version: u32,
    /// This migration's permanent UUIDv7 identity.
    ///
    /// Distinct from its content hash: the identity survives edits to mutable
    /// artifacts, while the hash is what proves the deployed definition has not
    /// changed.
    pub id: Uuid,
    /// The migrations that must be applied before this one.
    #[serde(default)]
    pub depends: Vec<Uuid>,
    /// How this migration's SQL is executed.
    #[serde(default)]
    pub transaction: Transaction,
    /// Whether this migration can be reverted.
    #[serde(default)]
    pub reversibility: Reversibility,
    /// Why this migration cannot be reverted. Required when, and permitted only
    /// when, `reversibility` is `irreversible`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub irreversible_reason: Option<String>,
    /// Lint warnings this migration accepts, each with a stated reason.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<Allow>,
}

/// How a migration's SQL is executed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Transaction {
    /// Zapadka opens a transaction, runs the whole script, and commits it.
    #[default]
    Required,
    /// Zapadka runs a single statement outside any transaction, for operations
    /// PostgreSQL forbids inside one, such as `CREATE INDEX CONCURRENTLY`.
    Forbidden,
}

impl Transaction {
    /// The report vocabulary for this mode.
    pub fn to_report(self) -> TransactionMode {
        match self {
            Self::Required => TransactionMode::Required,
            Self::Forbidden => TransactionMode::Forbidden,
        }
    }

    /// The spelling used in the canonical manifest and in `migration.toml`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Forbidden => "forbidden",
        }
    }
}

impl fmt::Display for Transaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a migration can be reverted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Reversibility {
    /// A `revert.sql` exists and undoes this migration.
    #[default]
    Reversible,
    /// This migration cannot be undone, and says why.
    Irreversible,
}

impl Reversibility {
    /// Whether this migration declares a revert path.
    pub fn is_reversible(self) -> bool {
        matches!(self, Self::Reversible)
    }
}

/// A migration-local acceptance of one lint warning.
///
/// A reason is required. A suppression without one is how a codebase quietly
/// loses the ability to explain why a risk was taken.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Allow {
    /// The lint code being accepted, e.g. `lint.destructive_drop`.
    pub lint: String,
    /// Why this migration accepts the risk.
    pub reason: String,
}

impl Manifest {
    /// Parses and validates manifest text found at `path`.
    pub fn parse(text: &str, path: &str) -> Result<Self> {
        let manifest: Manifest = toml::from_str(text).map_err(|error| {
            let mut zapadka = Error::new(
                ErrorCode::ManifestInvalid,
                format!(
                    "{MANIFEST_FILE_NAME} is not valid: {}",
                    error.to_string().lines().next().unwrap_or_default().trim()
                ),
            );
            zapadka = match error.span() {
                Some(span) => {
                    let (line, column) = line_and_column(text, span.start);
                    zapadka.at(Location::at(path, line, column))
                }
                None => zapadka.at(Location::file(path)),
            };
            zapadka
        })?;
        manifest.validate(path)?;
        Ok(manifest)
    }

    /// Checks invariants `serde` cannot express.
    fn validate(&self, path: &str) -> Result<()> {
        if self.format_version != MANIFEST_FORMAT_VERSION {
            return Err(Error::new(
                ErrorCode::ManifestUnsupportedFormatVersion,
                format!(
                    "{MANIFEST_FILE_NAME} declares format_version {}, but this Zapadka understands {MANIFEST_FORMAT_VERSION}",
                    self.format_version
                ),
            )
            .at(Location::file(path)));
        }

        if self.id.get_version_num() != 7 {
            return Err(Error::new(
                ErrorCode::MigrationIdInvalid,
                format!("migration id {} is not a UUIDv7", self.id),
            )
            .at(Location::file(path))
            .with_hint(
                "migration ids are UUIDv7 so that ties in the dependency graph break in creation \
                 order; let `zapadka new` generate them",
            ));
        }

        if self.depends.contains(&self.id) {
            return Err(Error::new(
                ErrorCode::GraphSelfDependency,
                format!("migration {} depends on itself", self.id),
            )
            .at(Location::file(path)));
        }

        let mut seen = BTreeSet::new();
        for dependency in &self.depends {
            if !seen.insert(dependency) {
                return Err(Error::new(
                    ErrorCode::ManifestInvalid,
                    format!("dependency {dependency} is listed more than once"),
                )
                .at(Location::file(path)));
            }
        }

        match (self.reversibility, self.irreversible_reason.as_deref()) {
            (Reversibility::Irreversible, None | Some("")) => {
                return Err(Error::new(
                    ErrorCode::MigrationReversibilityInvalid,
                    "an irreversible migration must state irreversible_reason",
                )
                .at(Location::file(path))
                .with_hint("explain what makes this migration impossible to undo, such as dropping a column whose data is not recoverable"));
            }
            (Reversibility::Reversible, Some(_)) => {
                return Err(Error::new(
                    ErrorCode::MigrationReversibilityInvalid,
                    "irreversible_reason is set on a reversible migration",
                )
                .at(Location::file(path))
                .with_hint("remove irreversible_reason, or set reversibility = \"irreversible\""));
            }
            _ => {}
        }

        for allow in &self.allow {
            if allow.reason.trim().is_empty() {
                return Err(Error::new(
                    ErrorCode::ManifestInvalid,
                    format!("suppression of {} must state a reason", allow.lint),
                )
                .at(Location::file(path)));
            }
        }

        Ok(())
    }

    /// Whether this migration suppresses `code`, and why.
    pub fn suppression(&self, code: &str) -> Option<&Allow> {
        self.allow.iter().find(|allow| allow.lint == code)
    }

    /// The dependency ids in canonical order.
    fn canonical_depends(&self) -> Vec<String> {
        // Sorted so that reordering `depends` in the file is not a history
        // change: the set of edges is what matters, not how it was written.
        let mut ids: Vec<String> = self.depends.iter().map(Uuid::to_string).collect();
        ids.sort();
        ids
    }

    /// Renders the canonical, execution-defining form of this manifest.
    ///
    /// This exact text is hashed into the deployment definition, so its format
    /// is a permanent compatibility contract. Any change to it invalidates
    /// every hash recorded in every existing registry and requires a new
    /// canonical header version and a registry migration.
    pub fn canonical_form(&self) -> String {
        format!(
            "{CANONICAL_HEADER}\nid={}\ntransaction={}\ndepends={}\n",
            self.id,
            self.transaction,
            self.canonical_depends().join(",")
        )
    }

    /// Computes the immutable deployment definition hash.
    ///
    /// Binds the canonical manifest to the exact bytes of `deploy.sql`. The
    /// script is included by its own hash rather than concatenated, so no
    /// script content can be mistaken for manifest structure.
    pub fn definition_sha256(&self, deploy_sql: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.canonical_form().as_bytes());
        hasher.update(format!("deploy_sha256={}\n", sha256_hex(deploy_sql)).as_bytes());
        hex(&hasher.finalize())
    }

    /// Renders the `migration.toml` that `zapadka new` writes.
    pub fn scaffold(id: Uuid, depends: &[Uuid], reversibility: Reversibility) -> String {
        let mut depends_list: Vec<String> = depends.iter().map(|id| format!("\"{id}\"")).collect();
        depends_list.sort();
        let depends_line = if depends_list.is_empty() {
            "depends = []".to_owned()
        } else if depends_list.len() == 1 {
            format!("depends = [{}]", depends_list[0])
        } else {
            format!("depends = [\n  {},\n]", depends_list.join(",\n  "))
        };

        let reversibility_block = match reversibility {
            Reversibility::Reversible => "reversibility = \"reversible\"\n".to_owned(),
            Reversibility::Irreversible => "reversibility = \"irreversible\"\n\
                 irreversible_reason = \"TODO: explain what makes this impossible to undo\"\n"
                .to_owned(),
        };

        format!(
            "\
format_version = {MANIFEST_FORMAT_VERSION}

# This migration's permanent identity. Never change it.
id = \"{id}\"

# The migrations that must be applied before this one. Order within the list
# does not matter; these are graph edges, not a sequence.
{depends_line}

# \"required\" runs deploy.sql inside a transaction Zapadka owns.
transaction = \"required\"

{reversibility_block}"
        )
    }
}

/// Hex-encodes a byte slice.
fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Returns the lowercase hex SHA-256 of `bytes`.
///
/// Exact bytes are hashed with no normalization: line endings and trailing
/// whitespace are part of what was deployed.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Converts a byte offset into 1-based line and column.
fn line_and_column(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let line = before.bytes().filter(|&b| b == b'\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    (line, text[line_start..offset].chars().count() + 1)
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;

    const A: &str = "0198f5c0-0000-7000-8000-00000000000a";
    const B: &str = "0198f5c0-0000-7000-8000-00000000000b";
    const C: &str = "0198f5c0-0000-7000-8000-00000000000c";

    fn parse(body: &str) -> Result<Manifest> {
        Manifest::parse(body, "migrations/x/migration.toml")
    }

    fn manifest(depends: &str) -> Manifest {
        parse(&format!(
            "format_version = 1\nid = \"{A}\"\ndepends = [{depends}]\n"
        ))
        .unwrap()
    }

    #[test]
    fn parses_a_minimal_manifest_with_conservative_defaults() {
        let manifest = parse(&format!("format_version = 1\nid = \"{A}\"\n")).unwrap();
        assert_eq!(manifest.transaction, Transaction::Required);
        assert_eq!(manifest.reversibility, Reversibility::Reversible);
        assert!(manifest.depends.is_empty());
    }

    #[test]
    fn requires_uuidv7_identities() {
        // A v4 id would break the deterministic tie-break between ready
        // migrations, which relies on UUIDv7 being time-ordered.
        let error = parse("format_version = 1\nid = \"f47ac10b-58cc-4372-a567-0e02b2c3d479\"\n")
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::MigrationIdInvalid);
    }

    #[test]
    fn rejects_self_dependency_and_duplicate_edges() {
        assert_eq!(
            parse(&format!(
                "format_version = 1\nid = \"{A}\"\ndepends = [\"{A}\"]\n"
            ))
            .unwrap_err()
            .code,
            ErrorCode::GraphSelfDependency
        );
        assert_eq!(
            parse(&format!(
                "format_version = 1\nid = \"{A}\"\ndepends = [\"{B}\", \"{B}\"]\n"
            ))
            .unwrap_err()
            .code,
            ErrorCode::ManifestInvalid
        );
    }

    #[test]
    fn irreversible_migrations_must_say_why() {
        let error = parse(&format!(
            "format_version = 1\nid = \"{A}\"\nreversibility = \"irreversible\"\n"
        ))
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::MigrationReversibilityInvalid);

        parse(&format!(
            "format_version = 1\nid = \"{A}\"\nreversibility = \"irreversible\"\n\
             irreversible_reason = \"drops archived rows that are not recoverable\"\n"
        ))
        .unwrap();
    }

    #[test]
    fn a_reversible_migration_may_not_claim_a_reason_it_does_not_need() {
        let error = parse(&format!(
            "format_version = 1\nid = \"{A}\"\nirreversible_reason = \"oops\"\n"
        ))
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::MigrationReversibilityInvalid);
    }

    #[test]
    fn suppressions_require_a_reason() {
        let error = parse(&format!(
            "format_version = 1\nid = \"{A}\"\n[[allow]]\nlint = \"lint.destructive_drop\"\nreason = \"  \"\n"
        ))
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::ManifestInvalid);

        let manifest = parse(&format!(
            "format_version = 1\nid = \"{A}\"\n[[allow]]\nlint = \"lint.destructive_drop\"\n\
             reason = \"the table was emptied by an earlier migration\"\n"
        ))
        .unwrap();
        assert!(manifest.suppression("lint.destructive_drop").is_some());
        assert!(manifest.suppression("lint.table_rewrite").is_none());
    }

    #[test]
    fn dependency_order_in_the_file_does_not_change_the_definition() {
        // `depends` is a set of edges. Rewriting the list in a different order
        // must not look like tampering with deployed history.
        let forward = manifest(&format!("\"{B}\", \"{C}\""));
        let reverse = manifest(&format!("\"{C}\", \"{B}\""));
        assert_eq!(forward.canonical_form(), reverse.canonical_form());
        assert_eq!(
            forward.definition_sha256(b"SELECT 1"),
            reverse.definition_sha256(b"SELECT 1")
        );
    }

    #[test]
    fn adding_or_removing_an_edge_does_change_the_definition() {
        assert_ne!(
            manifest(&format!("\"{B}\"")).definition_sha256(b"SELECT 1"),
            manifest(&format!("\"{B}\", \"{C}\"")).definition_sha256(b"SELECT 1")
        );
    }

    #[test]
    fn changing_the_transaction_mode_changes_the_definition() {
        let required = parse(&format!("format_version = 1\nid = \"{A}\"\n")).unwrap();
        let forbidden = parse(&format!(
            "format_version = 1\nid = \"{A}\"\ntransaction = \"forbidden\"\n"
        ))
        .unwrap();
        assert_ne!(
            required.definition_sha256(b"SELECT 1"),
            forbidden.definition_sha256(b"SELECT 1")
        );
    }

    #[test]
    fn changing_deploy_sql_by_one_byte_changes_the_definition() {
        let manifest = manifest("");
        assert_ne!(
            manifest.definition_sha256(b"SELECT 1"),
            manifest.definition_sha256(b"SELECT 1 ")
        );
    }

    #[test]
    fn reversibility_is_not_part_of_the_immutable_definition() {
        // A team must be able to write a revert script for a migration that is
        // already deployed; that is not a change to what the deploy did.
        let reversible = parse(&format!("format_version = 1\nid = \"{A}\"\n")).unwrap();
        let irreversible = parse(&format!(
            "format_version = 1\nid = \"{A}\"\nreversibility = \"irreversible\"\n\
             irreversible_reason = \"not recoverable\"\n"
        ))
        .unwrap();
        assert_eq!(
            reversible.definition_sha256(b"SELECT 1"),
            irreversible.definition_sha256(b"SELECT 1")
        );
    }

    #[test]
    fn the_canonical_form_is_the_documented_text() {
        // This literal is a compatibility contract; if it must change, the
        // header version and every recorded hash change with it.
        assert_eq!(
            manifest(&format!("\"{C}\", \"{B}\"")).canonical_form(),
            format!("zapadka.migration.v1\nid={A}\ntransaction=required\ndepends={B},{C}\n")
        );
    }

    #[test]
    fn the_scaffold_it_writes_is_a_manifest_it_accepts() {
        let id = Uuid::parse_str(A).unwrap();
        let depends = [Uuid::parse_str(C).unwrap(), Uuid::parse_str(B).unwrap()];
        let manifest = parse(&Manifest::scaffold(id, &depends, Reversibility::Reversible)).unwrap();
        assert_eq!(manifest.id, id);
        assert_eq!(manifest.depends.len(), 2);
        assert_eq!(manifest.transaction, Transaction::Required);
    }

    #[test]
    fn sha256_matches_the_well_known_digest_of_the_empty_input() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
