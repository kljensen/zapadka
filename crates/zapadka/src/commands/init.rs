//! `zapadka init` — create a project.
//!
//! Creates only files that belong in version control: the configuration and the
//! directories migrations and tests live in. It creates no database, no
//! registry, no credentials, and no local state directory. Everything Zapadka
//! knows about a project is either checked in or in the target database, and
//! `init` is where that promise starts.

use camino::Utf8Path;
use uuid::Uuid;
use zapadka_core::config::{CONFIG_FILE_NAME, Config, find_root};
use zapadka_core::error::{Error, ErrorCode, Result, io_error};
use zapadka_core::migration::MIGRATIONS_DIR;
use zapadka_core::report::{Diagnostic, Location, Severity};

use crate::cli::InitArgs;
use crate::session::Session;

/// Runs `zapadka init`.
pub fn run(directory: &Utf8Path, args: &InitArgs, session: &mut Session) -> Result<()> {
    let config_path = directory.join(CONFIG_FILE_NAME);

    if config_path.exists() {
        if !args.allow_existing {
            return Err(Error::new(
                ErrorCode::AlreadyExists,
                format!("{CONFIG_FILE_NAME} already exists in {directory}"),
            )
            .at(Location::file(CONFIG_FILE_NAME))
            .with_hint("this project is already initialized; pass --allow-existing to create only the missing directories"));
        }
        session.diagnose(Diagnostic {
            severity: Severity::Note,
            code: "init.already_initialized".to_owned(),
            message: format!("{CONFIG_FILE_NAME} already exists and was left unchanged"),
            migration_id: None,
            location: Some(Location::file(CONFIG_FILE_NAME)),
            hint: None,
        });
    }

    // Initializing inside an existing project almost always means the user is
    // in the wrong directory. Warn rather than refuse: nesting is unusual, not
    // impossible.
    if let Some(existing) = find_root(directory)
        && existing != directory
    {
        session.diagnose(Diagnostic {
            severity: Severity::Warning,
            code: "init.nested_project".to_owned(),
            message: format!("this directory is already inside the Zapadka project at {existing}"),
            migration_id: None,
            location: None,
            hint: Some(
                "a nested project has its own separate history; if you meant to add a migration \
                 to the existing project, run `zapadka new` instead"
                    .to_owned(),
            ),
        });
    }

    if !config_path.exists() {
        // A UUIDv7 so that a project's identity, like a migration's, carries
        // its creation time.
        let text = Config::scaffold(Uuid::now_v7());
        std::fs::create_dir_all(directory).map_err(|e| io_error(directory, "create", e))?;
        std::fs::write(&config_path, text).map_err(|e| io_error(&config_path, "write", e))?;
    }

    for relative in [MIGRATIONS_DIR, "tests/db"] {
        let path = directory.join(relative);
        if !path.exists() {
            std::fs::create_dir_all(&path).map_err(|e| io_error(&path, "create", e))?;
            // Git does not track empty directories, so a scaffolded project
            // would arrive at a teammate's checkout without them.
            let keep = path.join(".gitkeep");
            std::fs::write(&keep, "").map_err(|e| io_error(&keep, "write", e))?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;
    use crate::testing::temp_dir;

    fn init(directory: &Utf8Path, allow_existing: bool) -> Result<Session> {
        let mut session = Session::new("init");
        run(directory, &InitArgs { allow_existing }, &mut session)?;
        Ok(session)
    }

    #[test]
    fn creates_a_project_that_zapadka_can_then_load() {
        let dir = temp_dir();
        init(dir.path(), false).unwrap();

        let config = zapadka_core::config::load_from(dir.path()).unwrap();
        assert_eq!(config.root, dir.path());
        assert!(dir.path().join("migrations").is_dir());
        assert!(dir.path().join("tests/db").is_dir());
    }

    #[test]
    fn creates_no_credentials_database_or_local_state() {
        let dir = temp_dir();
        init(dir.path(), false).unwrap();

        let created: Vec<String> = walkdir::WalkDir::new(dir.path())
            .into_iter()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                entry
                    .path()
                    .strip_prefix(dir.path())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();

        // Exactly the checked-in scaffolding, and nothing else.
        let mut sorted = created.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            ["migrations/.gitkeep", "tests/db/.gitkeep", "zapadka.toml"]
        );

        let text = std::fs::read_to_string(dir.path().join("zapadka.toml")).unwrap();
        for secret_ish in ["password", "postgresql://", "@localhost"] {
            assert!(
                !text.contains(secret_ish),
                "scaffold must not contain {secret_ish}"
            );
        }
    }

    #[test]
    fn each_project_gets_its_own_identity() {
        let (first, second) = (temp_dir(), temp_dir());
        init(first.path(), false).unwrap();
        init(second.path(), false).unwrap();

        let id = |dir: &Utf8Path| {
            zapadka_core::config::load_from(dir)
                .unwrap()
                .config
                .project
                .id
        };
        assert_ne!(id(first.path()), id(second.path()));
        assert_eq!(id(first.path()).get_version_num(), 7);
    }

    #[test]
    fn refuses_to_reinitialize_an_existing_project() {
        let dir = temp_dir();
        init(dir.path(), false).unwrap();
        let error = init(dir.path(), false).unwrap_err();
        assert_eq!(error.code, ErrorCode::AlreadyExists);
    }

    #[test]
    fn never_overwrites_an_existing_configuration() {
        let dir = temp_dir();
        init(dir.path(), false).unwrap();
        let original = std::fs::read_to_string(dir.path().join(CONFIG_FILE_NAME)).unwrap();

        std::fs::remove_dir_all(dir.path().join("migrations")).unwrap();
        let session = init(dir.path(), true).unwrap();

        // The missing directory is restored; the configuration is untouched.
        assert!(dir.path().join("migrations").is_dir());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(CONFIG_FILE_NAME)).unwrap(),
            original
        );
        assert!(
            session
                .diagnostics
                .iter()
                .any(|d| d.code == "init.already_initialized")
        );
    }

    #[test]
    fn warns_when_initializing_inside_an_existing_project() {
        let outer = temp_dir();
        init(outer.path(), false).unwrap();

        let inner = outer.path().join("services/billing");
        std::fs::create_dir_all(&inner).unwrap();
        let session = init(&inner, false).unwrap();

        let warning = session
            .diagnostics
            .iter()
            .find(|d| d.code == "init.nested_project")
            .expect("nesting should be warned about");
        assert_eq!(warning.severity, Severity::Warning);
    }
}
