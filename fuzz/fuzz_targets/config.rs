//! Fuzzes `zapadka.toml` parsing.
//!
//! A configuration file comes from a repository, which on a shared runner is
//! not necessarily trusted input. Parsing it must never panic, and anything it
//! accepts must satisfy the invariants the rest of Zapadka assumes.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(config) = zapadka_core::config::Config::parse(text) {
            assert_eq!(config.format_version, 1);
            assert!(!config.project.registry_schema.trim().is_empty());
            // A target never carries both connection sources, because that
            // would make which one is used ambiguous.
            for target in config.targets.values() {
                assert!(!(target.pg_service.is_some() && target.uri_env.is_some()));
            }
        }
    }
});
