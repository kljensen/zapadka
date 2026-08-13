//! Compiles the vendored, pinned PostgreSQL 18 `libpg_query` sources into a
//! static archive linked directly into the Zapadka binary.
//!
//! The file list mirrors the upstream `Makefile` `SRC_FILES` variable exactly so
//! that no vendored file needs local modification. See
//! `third_party/libpg_query/PROVENANCE.toml` for the pinned release.
// Panicking is how a build script reports a problem: there is no report to
// write and no run to abandon.
#![allow(clippy::panic)]

use std::path::{Path, PathBuf};

/// Repository-root-relative location of the vendored upstream sources.
const VENDOR_REL: &str = "../../third_party/libpg_query";

fn main() {
    // Keep Cargo's ordinary absolute path. On Windows, canonicalize() adds a
    // `\\?\` prefix that MSVC's cl.exe does not accept for C source paths.
    let vendor = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(VENDOR_REL);
    assert!(
        vendor.is_dir(),
        "vendored libpg_query missing at {}",
        vendor.display()
    );

    let mut build = cc::Build::new();
    build
        .include(&vendor)
        .include(vendor.join("vendor"))
        .include(vendor.join("src/include"))
        .include(vendor.join("src/postgres/include"))
        .warnings(false)
        // Upstream requires these; PostgreSQL relies on defined overflow and
        // does not tolerate strict-aliasing optimizations.
        .flag_if_supported("-fno-strict-aliasing")
        .flag_if_supported("-fwrapv")
        .opt_level(2);

    if build.get_compiler().is_like_msvc() {
        build.include(vendor.join("src/postgres/include/port/win32"));
    }

    for file in source_files(&vendor) {
        build.file(file);
    }

    build.compile("pg_query");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", vendor.display());
}

/// Returns the upstream `SRC_FILES` list: `src/*.c`, `src/postgres/*.c`, the two
/// vendored support libraries, and the generated protobuf C bindings.
fn source_files(vendor: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in ["src", "src/postgres"] {
        files.extend(c_files_in(&vendor.join(dir)));
    }
    files.push(vendor.join("vendor/protobuf-c/protobuf-c.c"));
    files.push(vendor.join("vendor/xxhash/xxhash.c"));
    files.push(vendor.join("protobuf/pg_query.pb-c.c"));
    files
}

/// Collects `*.c` files in `dir`, sorted so the compile order is deterministic.
fn c_files_in(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "c"))
        .collect();
    files.sort();
    files
}
