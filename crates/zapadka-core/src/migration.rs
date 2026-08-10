//! Discovering migration packages on disk.
//!
//! A migration is a directory named `<uuidv7>-<slug>` containing
//! `migration.toml`, `deploy.sql`, and optionally `revert.sql` and
//! `verify.sql`. The directory name carries the slug so that a listing of
//! `migrations/` is readable, but the manifest's `id` is authoritative — a
//! directory whose name disagrees with its manifest is an error, not a rename
//! Zapadka guesses at.

use camino::{Utf8Path, Utf8PathBuf};
use uuid::Uuid;

use crate::error::{Error, ErrorCode, Result, io_error};
use crate::manifest::{MANIFEST_FILE_NAME, Manifest, Reversibility, sha256_hex};
use crate::report::{Location, ScriptRole};

/// The project directory holding migration packages.
pub const MIGRATIONS_DIR: &str = "migrations";

/// One migration on disk, with its scripts read and hashed.
#[derive(Debug, Clone)]
pub struct Migration {
    /// The permanent identity from the manifest.
    pub id: Uuid,
    /// The human-readable part of the directory name.
    pub slug: String,
    /// Absolute path to the migration directory.
    pub dir: Utf8PathBuf,
    /// Project-relative directory path, used in reports and diagnostics.
    pub relative_dir: String,
    /// The parsed manifest.
    pub manifest: Manifest,
    /// `deploy.sql`. Always present.
    pub deploy: Script,
    /// `revert.sql`, when the migration has one.
    pub revert: Option<Script>,
    /// `verify.sql`, when the migration has one.
    pub verify: Option<Script>,
    /// SHA-256 of the immutable deployment definition.
    pub definition_sha256: String,
}

impl Migration {
    /// The migrations this one depends on.
    pub fn depends(&self) -> &[Uuid] {
        &self.manifest.depends
    }

    /// Whether this migration declares a revert path.
    pub fn is_reversible(&self) -> bool {
        self.manifest.reversibility.is_reversible()
    }

    /// The script for `role`, when the migration has one.
    pub fn script(&self, role: ScriptRole) -> Option<&Script> {
        match role {
            ScriptRole::Deploy => Some(&self.deploy),
            ScriptRole::Revert => self.revert.as_ref(),
            ScriptRole::Verify => self.verify.as_ref(),
        }
    }

    /// How this migration is named in human output: a short id and its slug.
    pub fn label(&self) -> String {
        format!("{} {}", short_id(self.id), self.slug)
    }
}

/// One SQL file belonging to a migration.
#[derive(Debug, Clone)]
pub struct Script {
    /// Which script this is.
    pub role: ScriptRole,
    /// Absolute path.
    pub path: Utf8PathBuf,
    /// Project-relative path, used in reports and diagnostics.
    pub relative_path: String,
    /// The file's contents.
    pub sql: String,
    /// SHA-256 of the exact bytes on disk.
    pub sha256: String,
}

impl Script {
    /// Whether the script has no SQL in it at all.
    pub fn is_effectively_empty(&self) -> bool {
        self.sql.trim().is_empty()
    }
}

/// The first eight characters of an id, for human output.
///
/// UUIDv7 ids are time-ordered, so the leading characters are the part that
/// distinguishes migrations created at different times.
pub fn short_id(id: Uuid) -> String {
    id.to_string()[..8].to_owned()
}

/// Reads every migration package under `root/migrations`.
///
/// A project with no `migrations` directory is valid and yields nothing, so a
/// freshly initialized project works without a special case.
pub fn discover(root: &Utf8Path) -> Result<Vec<Migration>> {
    let dir = root.join(MIGRATIONS_DIR);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let entries = std::fs::read_dir(&dir).map_err(|e| io_error(&dir, "read", e))?;
    let mut directories = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_error(&dir, "read", e))?;
        let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
            Error::new(
                ErrorCode::Io,
                format!("migration path {} is not valid UTF-8", path.display()),
            )
        })?;
        if !path.is_dir() {
            // Stray files such as README.md or .DS_Store are ignored: the
            // directory is a package container, not a manifest.
            continue;
        }
        directories.push(path);
    }
    // Sorted so discovery order — and therefore the order of any error a user
    // sees — does not depend on the filesystem.
    directories.sort();

    let mut migrations = Vec::with_capacity(directories.len());
    for directory in directories {
        migrations.push(read(root, &directory)?);
    }

    check_for_duplicate_ids(&migrations)?;
    Ok(migrations)
}

/// Reads one migration package.
pub fn read(root: &Utf8Path, dir: &Utf8Path) -> Result<Migration> {
    let relative_dir = relative(root, dir);
    let directory_name = dir.file_name().unwrap_or_default();

    let manifest_path = dir.join(MANIFEST_FILE_NAME);
    let relative_manifest = format!("{relative_dir}/{MANIFEST_FILE_NAME}");
    if !manifest_path.is_file() {
        return Err(Error::new(
            ErrorCode::MigrationMissingScript,
            format!("{relative_dir} has no {MANIFEST_FILE_NAME}"),
        )
        .at(Location::file(&relative_dir))
        .with_hint("every directory under migrations/ must be a migration package"));
    }
    let manifest_text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| io_error(&relative_manifest, "read", e))?;
    let manifest = Manifest::parse(&manifest_text, &relative_manifest)?;

    let (name_id, slug) = split_directory_name(directory_name);
    match name_id {
        Some(name_id) if name_id == manifest.id => {}
        _ => {
            return Err(Error::new(
                ErrorCode::MigrationDirectoryMismatch,
                format!(
                    "directory {relative_dir} does not match its manifest id {}",
                    manifest.id
                ),
            )
            .at(Location::file(&relative_manifest))
            .with_hint(format!(
                "rename the directory to {}-{}, or correct the id in {MANIFEST_FILE_NAME}",
                manifest.id,
                if slug.is_empty() { "slug" } else { &slug }
            )));
        }
    }

    let deploy = read_script(root, dir, ScriptRole::Deploy)?.ok_or_else(|| {
        Error::new(
            ErrorCode::MigrationMissingScript,
            format!("{relative_dir} has no deploy.sql"),
        )
        .at(Location::file(&relative_dir))
    })?;
    let revert = read_script(root, dir, ScriptRole::Revert)?;
    let verify = read_script(root, dir, ScriptRole::Verify)?;

    // Reversibility is a promise about `revert.sql`, so the two must agree.
    // Zapadka checks this at discovery rather than at revert time: finding out
    // a migration cannot be undone during an incident is too late.
    match (manifest.reversibility, revert.is_some()) {
        (Reversibility::Reversible, false) => {
            return Err(Error::new(
                ErrorCode::MigrationReversibilityInvalid,
                format!("{relative_dir} is declared reversible but has no revert.sql"),
            )
            .at(Location::file(&relative_manifest))
            .with_hint(
                "write revert.sql, or declare reversibility = \"irreversible\" with a reason",
            ));
        }
        (Reversibility::Irreversible, true) => {
            return Err(Error::new(
                ErrorCode::MigrationReversibilityInvalid,
                format!("{relative_dir} is declared irreversible but has a revert.sql"),
            )
            .at(Location::file(&relative_manifest))
            .with_hint("delete revert.sql, or declare reversibility = \"reversible\""));
        }
        _ => {}
    }

    let definition_sha256 = manifest.definition_sha256(deploy.sql.as_bytes());

    Ok(Migration {
        id: manifest.id,
        slug,
        dir: dir.to_path_buf(),
        relative_dir,
        manifest,
        deploy,
        revert,
        verify,
        definition_sha256,
    })
}

/// Reads one optional script file.
fn read_script(root: &Utf8Path, dir: &Utf8Path, role: ScriptRole) -> Result<Option<Script>> {
    let path = dir.join(role.file_name());
    if !path.is_file() {
        return Ok(None);
    }
    let relative_path = relative(root, &path);
    let bytes = std::fs::read(&path).map_err(|e| io_error(&relative_path, "read", e))?;
    let sql = String::from_utf8(bytes.clone()).map_err(|_| {
        Error::new(ErrorCode::Io, format!("{relative_path} is not valid UTF-8"))
            .at(Location::file(&relative_path))
            .with_hint("Zapadka sends SQL to PostgreSQL as UTF-8 text")
    })?;
    Ok(Some(Script {
        role,
        // Hash the bytes on disk, not the decoded string: what was executed is
        // what the file contained, including its line endings.
        sha256: sha256_hex(&bytes),
        path,
        relative_path,
        sql,
    }))
}

/// Splits `<uuid>-<slug>` into its parts.
///
/// Returns `None` for the id when the directory name does not begin with a
/// UUID, which the caller reports as a mismatch.
fn split_directory_name(name: &str) -> (Option<Uuid>, String) {
    // A hyphenated UUID is 36 characters and itself contains hyphens, so the
    // slug separator is the hyphen at exactly that offset.
    const UUID_LEN: usize = 36;
    if name.len() > UUID_LEN
        && name.is_char_boundary(UUID_LEN)
        && let Ok(id) = Uuid::parse_str(&name[..UUID_LEN])
    {
        let rest = &name[UUID_LEN..];
        return (Some(id), rest.strip_prefix('-').unwrap_or(rest).to_owned());
    }
    match Uuid::parse_str(name) {
        Ok(id) => (Some(id), String::new()),
        Err(_) => (None, String::new()),
    }
}

/// Fails when two directories claim the same identity.
fn check_for_duplicate_ids(migrations: &[Migration]) -> Result<()> {
    let mut by_id: std::collections::BTreeMap<Uuid, &Migration> = std::collections::BTreeMap::new();
    for migration in migrations {
        if let Some(existing) = by_id.insert(migration.id, migration) {
            return Err(Error::new(
                ErrorCode::MigrationDuplicateId,
                format!(
                    "migrations {} and {} declare the same id {}",
                    existing.relative_dir, migration.relative_dir, migration.id
                ),
            )
            .at(Location::file(format!(
                "{}/{MANIFEST_FILE_NAME}",
                migration.relative_dir
            )))
            .with_hint(
                "a migration's id is permanent and unique; generate a new one with `zapadka new` \
                 rather than copying a directory",
            ));
        }
    }
    Ok(())
}

/// Renders `path` relative to the project root, with forward slashes.
///
/// Reports must be comparable across machines, so they never contain an
/// absolute path.
fn relative(root: &Utf8Path, path: &Utf8Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .as_str()
        .replace('\\', "/")
}

/// Normalizes a user-supplied slug into the form used in directory names.
///
/// Lowercase ASCII alphanumerics and single hyphens. This is a naming
/// convention, not a security boundary — but it does keep a slug from escaping
/// the migrations directory or colliding on case-insensitive filesystems.
pub fn normalize_slug(input: &str) -> Result<String> {
    /// Leaves room for the UUID prefix, the separator, and a filename inside.
    const MAX_SLUG: usize = 80;

    let mut slug = String::with_capacity(input.len());
    let mut pending_separator = false;
    for character in input.chars() {
        if character.is_ascii_alphanumeric() {
            if pending_separator && !slug.is_empty() {
                slug.push('-');
            }
            pending_separator = false;
            slug.push(character.to_ascii_lowercase());
        } else {
            pending_separator = true;
        }
    }

    if slug.is_empty() {
        return Err(Error::new(
            ErrorCode::ManifestInvalid,
            format!("slug {input:?} contains no letters or digits"),
        )
        .with_hint("use a short name such as add-orders-table"));
    }

    slug.truncate(MAX_SLUG);
    Ok(slug.trim_end_matches('-').to_owned())
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;

    #[test]
    fn normalizes_slugs_people_type() {
        assert_eq!(
            normalize_slug("add-orders-table").unwrap(),
            "add-orders-table"
        );
        assert_eq!(
            normalize_slug("Add Orders Table").unwrap(),
            "add-orders-table"
        );
        assert_eq!(
            normalize_slug("add_orders__table").unwrap(),
            "add-orders-table"
        );
        assert_eq!(normalize_slug("  spaced  out  ").unwrap(), "spaced-out");
        assert_eq!(normalize_slug("v2.1/orders").unwrap(), "v2-1-orders");
    }

    #[test]
    fn slugs_cannot_escape_the_migrations_directory() {
        // Path separators and traversal are stripped rather than rejected, so a
        // slug can never name a directory outside migrations/.
        assert_eq!(normalize_slug("../../etc/passwd").unwrap(), "etc-passwd");
        assert_eq!(normalize_slug("a/../b").unwrap(), "a-b");
        assert!(normalize_slug("../..").is_err());
    }

    #[test]
    fn rejects_a_slug_with_nothing_in_it() {
        for empty in ["", "   ", "---", "!!!"] {
            assert!(normalize_slug(empty).is_err(), "{empty:?}");
        }
    }

    #[test]
    fn splits_directory_names_into_id_and_slug() {
        let id = "0198f5c0-0000-7000-8000-00000000000a";
        let (parsed, slug) = split_directory_name(&format!("{id}-add-orders"));
        assert_eq!(parsed.unwrap().to_string(), id);
        assert_eq!(slug, "add-orders");

        // A bare id with no slug is valid.
        let (parsed, slug) = split_directory_name(id);
        assert_eq!(parsed.unwrap().to_string(), id);
        assert_eq!(slug, "");

        // Anything that is not a UUID has no identity.
        assert_eq!(split_directory_name("0001-add-orders").0, None);
        assert_eq!(split_directory_name("").0, None);
    }

    #[test]
    fn short_ids_are_stable_and_readable() {
        let id = Uuid::parse_str("0198f5c0-0000-7000-8000-00000000000a").unwrap();
        assert_eq!(short_id(id), "0198f5c0");
    }
}
