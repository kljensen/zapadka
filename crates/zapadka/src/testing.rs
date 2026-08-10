//! Test helpers for building throwaway projects on disk.
//!
//! Compiled only under `cfg(test)`. Zapadka avoids a temporary-directory
//! dependency here because what it needs is small and because a test that
//! leaves a directory behind on failure is often easier to debug than one that
//! cleans up perfectly.

use std::sync::atomic::{AtomicU64, Ordering};

use camino::{Utf8Path, Utf8PathBuf};
use uuid::Uuid;
use zapadka_core::manifest::{MANIFEST_FILE_NAME, Manifest, Reversibility};

/// A directory removed when it goes out of scope.
pub struct TempDir {
    path: Utf8PathBuf,
}

impl TempDir {
    /// The directory's path.
    pub fn path(&self) -> &Utf8Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Creates an empty temporary directory.
pub fn temp_dir() -> TempDir {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = format!(
        "zapadka-test-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let path = Utf8PathBuf::from_path_buf(std::env::temp_dir().join(unique))
        .expect("the system temporary directory must be valid UTF-8");
    std::fs::create_dir_all(&path).expect("cannot create a temporary directory");
    TempDir { path }
}

/// Creates a temporary directory holding an initialized Zapadka project.
pub fn temp_project() -> TempDir {
    let dir = temp_dir();
    let mut session = crate::session::Session::new("init");
    crate::commands::init::run(
        dir.path(),
        &crate::cli::InitArgs {
            allow_existing: false,
        },
        &mut session,
    )
    .expect("cannot initialize a test project");
    dir
}

/// Writes a migration package and returns its id.
///
/// `depends` names the ids this migration depends on.
pub fn write_migration(root: &Utf8Path, slug: &str, depends: &[Uuid], deploy: &str) -> Uuid {
    write_migration_with(root, slug, depends, deploy, None, None)
}

/// Writes a migration package, optionally with revert and verify scripts.
pub fn write_migration_with(
    root: &Utf8Path,
    slug: &str,
    depends: &[Uuid],
    deploy: &str,
    revert: Option<&str>,
    verify: Option<&str>,
) -> Uuid {
    let id = Uuid::now_v7();
    let dir = root.join("migrations").join(format!("{id}-{slug}"));
    std::fs::create_dir_all(&dir).expect("cannot create a migration directory");

    let reversibility = match revert {
        Some(_) => Reversibility::Reversible,
        None => Reversibility::Irreversible,
    };
    let mut manifest = Manifest::scaffold(id, depends, reversibility);
    if reversibility == Reversibility::Irreversible {
        manifest = manifest.replace(
            "TODO: explain what makes this impossible to undo",
            "test migration with no revert script",
        );
    }

    std::fs::write(dir.join(MANIFEST_FILE_NAME), manifest).unwrap();
    std::fs::write(dir.join("deploy.sql"), deploy).unwrap();
    if let Some(revert) = revert {
        std::fs::write(dir.join("revert.sql"), revert).unwrap();
    }
    if let Some(verify) = verify {
        std::fs::write(dir.join("verify.sql"), verify).unwrap();
    }
    id
}
