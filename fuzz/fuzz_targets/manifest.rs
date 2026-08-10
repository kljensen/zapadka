//! Fuzzes `migration.toml` parsing and canonicalization.
//!
//! The canonical form feeds the hash that history integrity is checked against,
//! so it must be total and deterministic: every manifest that parses must
//! canonicalize, and canonicalizing twice must give the same answer.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(manifest) = zapadka_core::manifest::Manifest::parse(text, "migration.toml") {
            assert_eq!(manifest.id.get_version_num(), 7);

            let canonical = manifest.canonical_form();
            assert_eq!(canonical, manifest.canonical_form(), "canonicalization is stable");

            let hash = manifest.definition_sha256(b"SELECT 1");
            assert_eq!(hash.len(), 64);
            assert_eq!(hash, manifest.definition_sha256(b"SELECT 1"));
        }
    }
});
