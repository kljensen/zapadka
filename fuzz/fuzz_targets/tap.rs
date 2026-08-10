//! Fuzzes the TAP parser.
//!
//! TAP arrives from a database, as whatever a test file happened to print. The
//! property being fuzzed is not that any particular input parses, but that
//! every input terminates with a classified answer rather than a panic: a
//! migration tool must not be brought down by a test file printing something
//! strange.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(document) = zapadka_core::tap::parse(text) {
            // A document that parsed must satisfy the invariants everything
            // downstream relies on: consecutive numbering, and a plan count
            // that agrees with the results.
            for (index, assertion) in document.assertions.iter().enumerate() {
                assert_eq!(assertion.number, index as u64 + 1);
            }
            if let zapadka_core::tap::Plan::Count(planned) = document.plan {
                assert_eq!(planned, document.assertions.len() as u64);
            }
        }
    }
});
