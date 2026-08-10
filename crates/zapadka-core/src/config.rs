//! The project configuration file, `zapadka.toml`.
//!
//! The configuration is checked in, so it never holds a credential. A target
//! names *where* to find its connection information — a PostgreSQL service
//! entry or an environment variable — and Zapadka resolves it at run time. This
//! is a deliberate product boundary, not an omission: Zapadka is not a secret
//! manager, and a repository is the wrong place for a database password.

use std::collections::BTreeMap;

use camino::{Utf8Path, Utf8PathBuf};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::duration::Timeout;
use crate::error::{Error, ErrorCode, Result, io_error};
use crate::report::Location;

/// The configuration file name, searched for from the working directory upward.
pub const CONFIG_FILE_NAME: &str = "zapadka.toml";

/// The `format_version` this binary writes and understands.
pub const CONFIG_FORMAT_VERSION: u32 = 1;

/// The default schema holding Zapadka's registry.
pub const DEFAULT_REGISTRY_SCHEMA: &str = "zapadka";

/// The default time a mutating command waits for the deployment lock.
///
/// Short by design: a deploy that cannot get the lock promptly is usually
/// racing another deploy, and failing fast is more useful than blocking a
/// pipeline. Longer waits must be asked for explicitly.
pub const DEFAULT_LOCK_WAIT: Timeout = Timeout::from_secs(5);

/// A parsed `zapadka.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The configuration schema version. Must be [`CONFIG_FORMAT_VERSION`].
    pub format_version: u32,
    /// Identity and registry placement for this project.
    pub project: Project,
    /// Named databases this project can act on.
    #[serde(default)]
    pub targets: BTreeMap<String, TargetConfig>,
    /// Project-wide operational policy.
    #[serde(default)]
    pub policy: Policy,
}

/// Project identity.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Project {
    /// A UUIDv7 identifying this project.
    ///
    /// Written into the registry so a target cannot be shared by two projects
    /// by accident, which would interleave two unrelated histories.
    pub id: Uuid,
    /// The schema holding Zapadka's registry tables.
    #[serde(default = "default_registry_schema")]
    pub registry_schema: String,
}

fn default_registry_schema() -> String {
    DEFAULT_REGISTRY_SCHEMA.to_owned()
}

/// One named database.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    /// A PostgreSQL service name, resolved from `pg_service.conf` the way
    /// `libpq` would resolve it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pg_service: Option<String>,
    /// The environment variable holding this target's connection URI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri_env: Option<String>,
    /// Schemas belonging to the application, used to build the search path for
    /// `zapadka test`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub application_schemas: Vec<String>,
    /// `lock_timeout` to apply while running this target's SQL. Absent means
    /// Zapadka sets nothing and PostgreSQL's own default applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_timeout: Option<Timeout>,
    /// `statement_timeout` to apply while running this target's SQL. Absent
    /// means Zapadka sets nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statement_timeout: Option<Timeout>,
}

impl TargetConfig {
    /// Whether the target says where to find its connection information.
    ///
    /// A target with neither is still valid: `--uri` on the command line can
    /// supply it. It only fails when a command actually needs to connect.
    pub fn has_connection_source(&self) -> bool {
        self.pg_service.is_some() || self.uri_env.is_some()
    }
}

/// Project-wide operational policy.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// How long a mutating command waits for the deployment advisory lock
    /// before failing. `--wait` overrides it per run.
    #[serde(default = "default_lock_wait")]
    pub advisory_lock_timeout: Timeout,
    /// Lint codes promoted from warning to error for this project.
    ///
    /// This is how a team makes a risk non-negotiable — for example requiring
    /// that every index on an existing table be built concurrently.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

fn default_lock_wait() -> Timeout {
    DEFAULT_LOCK_WAIT
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            advisory_lock_timeout: DEFAULT_LOCK_WAIT,
            deny: Vec::new(),
        }
    }
}

/// A configuration file together with the directory it governs.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// The parsed configuration.
    pub config: Config,
    /// The directory containing `zapadka.toml`. All project paths are relative
    /// to it, so Zapadka behaves the same from any subdirectory.
    pub root: Utf8PathBuf,
}

impl LoadedConfig {
    /// The path of the configuration file itself.
    pub fn path(&self) -> Utf8PathBuf {
        self.root.join(CONFIG_FILE_NAME)
    }

    /// The directory holding migration packages.
    pub fn migrations_dir(&self) -> Utf8PathBuf {
        self.root.join("migrations")
    }

    /// The directory holding database test files.
    pub fn tests_dir(&self) -> Utf8PathBuf {
        self.root.join("tests").join("db")
    }

    /// Looks up a target by name.
    pub fn target(&self, name: &str) -> Result<&TargetConfig> {
        self.config.targets.get(name).ok_or_else(|| {
            let known = self
                .config
                .targets
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            let hint = if known.is_empty() {
                "declare a target in zapadka.toml, for example [targets.production]".to_owned()
            } else {
                format!("known targets: {known}")
            };
            Error::new(ErrorCode::TargetUnknown, format!("unknown target {name:?}"))
                .at(Location::file(CONFIG_FILE_NAME))
                .hint(hint)
        })
    }
}

impl Config {
    /// Parses and validates configuration text.
    pub fn parse(text: &str) -> Result<Self> {
        let config: Config = toml::from_str(text).map_err(|error| {
            let mut zapadka = Error::new(
                ErrorCode::ConfigInvalid,
                format!("{CONFIG_FILE_NAME} is not valid: {}", first_line(&error.to_string())),
            );
            if let Some(span) = error.span() {
                let (line, column) = line_and_column(text, span.start);
                zapadka = zapadka.at(Location::at(CONFIG_FILE_NAME, line, column));
            } else {
                zapadka = zapadka.at(Location::file(CONFIG_FILE_NAME));
            }
            zapadka.hint(credential_hint(&error.to_string()))
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Checks invariants `serde` cannot express.
    fn validate(&self) -> Result<()> {
        if self.format_version != CONFIG_FORMAT_VERSION {
            return Err(Error::new(
                ErrorCode::ConfigUnsupportedFormatVersion,
                format!(
                    "{CONFIG_FILE_NAME} declares format_version {}, but this Zapadka understands {CONFIG_FORMAT_VERSION}",
                    self.format_version
                ),
            )
            .at(Location::file(CONFIG_FILE_NAME))
            .hint(if self.format_version > CONFIG_FORMAT_VERSION {
                "this project was written by a newer Zapadka; upgrade the binary"
            } else {
                "migrate the file to the current format"
            }));
        }

        if self.project.registry_schema.trim().is_empty() {
            return Err(Error::new(
                ErrorCode::ConfigInvalid,
                "project.registry_schema must not be empty",
            )
            .at(Location::file(CONFIG_FILE_NAME)));
        }

        for (name, target) in &self.targets {
            if target.pg_service.is_some() && target.uri_env.is_some() {
                return Err(Error::new(
                    ErrorCode::TargetInvalid,
                    format!(
                        "target {name:?} sets both pg_service and uri_env, so its connection is ambiguous"
                    ),
                )
                .at(Location::file(CONFIG_FILE_NAME))
                .hint("keep exactly one connection source per target"));
            }
        }

        Ok(())
    }

    /// Renders the configuration a fresh `zapadka init` writes.
    ///
    /// Written by hand rather than serialized so that it can carry the comments
    /// a new user needs, including why no URL appears here.
    pub fn scaffold(project_id: Uuid) -> String {
        format!(
            "\
# Zapadka project configuration.
#
# This file is checked in, so it holds no credentials. A target names where to
# find its connection information; Zapadka resolves it when it connects.

format_version = {CONFIG_FORMAT_VERSION}

[project]
# This project's permanent identity. The registry records it so one database is
# never shared by two Zapadka projects by accident.
id = \"{project_id}\"
registry_schema = \"{DEFAULT_REGISTRY_SCHEMA}\"

# Declare one entry per database you deploy to. Use `pg_service` to name an
# entry in your PostgreSQL service file, or `uri_env` to name an environment
# variable holding the connection URI. You can also pass `--uri` on the command
# line without declaring anything here.
#
# [targets.production]
# pg_service = \"app-production\"
#
# [targets.test]
# uri_env = \"TEST_DATABASE_URL\"
# application_schemas = [\"app\"]

[policy]
# How long a deploy waits for the deployment lock before giving up.
advisory_lock_timeout = \"{DEFAULT_LOCK_WAIT}\"

# Lint codes to treat as errors rather than warnings, for example:
# deny = [\"lint.index_without_concurrently\"]
"
        )
    }
}

/// Finds and loads the configuration governing `start`.
///
/// Searches `start` and each parent directory, so Zapadka works from anywhere
/// inside a project the way `git` does.
pub fn load_from(start: &Utf8Path) -> Result<LoadedConfig> {
    let root = find_root(start).ok_or_else(|| {
        Error::new(
            ErrorCode::ConfigNotFound,
            format!("no {CONFIG_FILE_NAME} found in {start} or any parent directory"),
        )
        .hint("run `zapadka init` to create a project here")
    })?;
    let path = root.join(CONFIG_FILE_NAME);
    let text = std::fs::read_to_string(&path).map_err(|e| io_error(&path, "read", e))?;
    let config = Config::parse(&text)?;
    Ok(LoadedConfig { config, root })
}

/// Returns the nearest ancestor of `start` containing a `zapadka.toml`.
pub fn find_root(start: &Utf8Path) -> Option<Utf8PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(CONFIG_FILE_NAME).is_file())
        .map(Utf8Path::to_path_buf)
}

/// Adds guidance when a rejected key looks like an attempt to store a
/// credential, which is the mistake most worth explaining rather than merely
/// reporting.
fn credential_hint(message: &str) -> String {
    const CREDENTIAL_KEYS: [&str; 6] = ["uri", "url", "dsn", "password", "user", "host"];
    let looks_like_credential = CREDENTIAL_KEYS
        .iter()
        .any(|key| message.contains(&format!("`{key}`")));
    if looks_like_credential {
        "zapadka.toml is checked in and holds no connection details; use `pg_service`, \
         `uri_env`, or the `--uri` option instead"
            .to_owned()
    } else {
        format!("see the {CONFIG_FILE_NAME} reference for the supported keys")
    }
}

/// TOML errors are multi-line; reports want one sentence.
fn first_line(message: &str) -> String {
    message.lines().next().unwrap_or(message).trim().to_owned()
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
    use super::*;

    const PROJECT_ID: &str = "0198f5c0-0000-7000-8000-000000000001";

    fn minimal() -> String {
        format!(
            "format_version = 1\n[project]\nid = \"{PROJECT_ID}\"\n"
        )
    }

    #[test]
    fn parses_a_minimal_project() {
        let config = Config::parse(&minimal()).unwrap();
        assert_eq!(config.format_version, 1);
        assert_eq!(config.project.registry_schema, DEFAULT_REGISTRY_SCHEMA);
        assert_eq!(config.policy.advisory_lock_timeout, DEFAULT_LOCK_WAIT);
        assert!(config.targets.is_empty());
    }

    #[test]
    fn parses_targets_and_policy() {
        let config = Config::parse(
            &format!(
                "{}\n\
                 [targets.production]\npg_service = \"app-production\"\nlock_timeout = \"3s\"\n\n\
                 [targets.test]\nuri_env = \"TEST_DATABASE_URL\"\napplication_schemas = [\"app\"]\n\n\
                 [policy]\nadvisory_lock_timeout = \"30s\"\ndeny = [\"lint.destructive_drop\"]\n",
                minimal()
            ),
        )
        .unwrap();

        let production = &config.targets["production"];
        assert_eq!(production.pg_service.as_deref(), Some("app-production"));
        assert_eq!(production.lock_timeout, Some(Timeout::from_secs(3)));
        assert_eq!(config.targets["test"].application_schemas, ["app"]);
        assert_eq!(config.policy.advisory_lock_timeout, Timeout::from_secs(30));
        assert_eq!(config.policy.deny, ["lint.destructive_drop"]);
    }

    #[test]
    fn refuses_a_configuration_that_would_store_a_credential() {
        let error = Config::parse(&format!(
            "{}\n[targets.production]\nuri = \"postgresql://user:secret@host/db\"\n",
            minimal()
        ))
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::ConfigInvalid);
        let hint = error.hint.unwrap();
        assert!(hint.contains("pg_service"), "{hint}");
        assert!(hint.contains("uri_env"), "{hint}");
    }

    #[test]
    fn refuses_a_target_with_two_connection_sources() {
        let error = Config::parse(&format!(
            "{}\n[targets.production]\npg_service = \"s\"\nuri_env = \"E\"\n",
            minimal()
        ))
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::TargetInvalid);
    }

    #[test]
    fn refuses_a_newer_format_version_and_says_to_upgrade() {
        let error =
            Config::parse(&format!("format_version = 2\n[project]\nid = \"{PROJECT_ID}\"\n"))
                .unwrap_err();
        assert_eq!(error.code, ErrorCode::ConfigUnsupportedFormatVersion);
        assert!(error.hint.unwrap().contains("upgrade"));
    }

    #[test]
    fn reports_the_line_of_a_syntax_error() {
        let error = Config::parse("format_version = 1\n[project\nid = \"x\"\n").unwrap_err();
        assert_eq!(error.code, ErrorCode::ConfigInvalid);
        let location = error.location.expect("syntax errors carry a location");
        assert_eq!(location.path, CONFIG_FILE_NAME);
        assert_eq!(location.line, Some(2));
    }

    #[test]
    fn unknown_keys_are_rejected_rather_than_silently_ignored() {
        // A typo in a safety-relevant key must not look like it took effect.
        let error = Config::parse(&format!("{}\n[policy]\nadvisory_lock_timout = \"5s\"\n", minimal()))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ConfigInvalid);
    }

    #[test]
    fn the_scaffold_it_writes_is_a_configuration_it_accepts() {
        let id = Uuid::parse_str(PROJECT_ID).unwrap();
        let text = Config::scaffold(id);
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.project.id, id);
        assert_eq!(config.project.registry_schema, DEFAULT_REGISTRY_SCHEMA);
    }

    #[test]
    fn unknown_targets_list_the_known_ones() {
        let loaded = LoadedConfig {
            config: Config::parse(&format!(
                "{}\n[targets.production]\npg_service = \"s\"\n",
                minimal()
            ))
            .unwrap(),
            root: Utf8PathBuf::from("/project"),
        };
        let error = loaded.target("staging").unwrap_err();
        assert_eq!(error.code, ErrorCode::TargetUnknown);
        assert!(error.hint.unwrap().contains("production"));
    }
}
