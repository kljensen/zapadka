//! Integration-test harness: a real PostgreSQL 18 and a real Zapadka binary.
//!
//! # What these tests exercise
//!
//! They run the built `zapadka` executable as a subprocess, exactly as a user
//! or a CI pipeline would. That is deliberate: exit codes, the separation of
//! stdout from stderr, and the "exactly one JSON document" promise are part of
//! Zapadka's contract, and none of them can be tested by calling a function.
//!
//! # Docker is required, never optional
//!
//! If Docker is unavailable these tests fail loudly. A database test that
//! quietly skips is worse than no test at all: it turns a broken environment
//! into a green build, which is precisely the situation where someone ships a
//! migration bug.

// Each test binary uses a different subset of the harness.
#![allow(dead_code)]
// A harness that cannot reach its database has nothing useful to return; the
// failure message is the point.
#![allow(clippy::panic)]
// A test harness module is reachable only from its own test binary.
#![allow(unreachable_pub)]
// The advisory-lock key is deliberately narrowed to the two signed 32-bit
// catalog columns, mirroring zapadka_pg::lock.
#![allow(clippy::cast_possible_truncation)]

use std::process::Command;
use std::sync::OnceLock;

use camino::{Utf8Path, Utf8PathBuf};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};
use uuid::Uuid;

/// PostgreSQL 18, pinned by immutable digest.
///
/// A tag can be repointed at a different build; a digest cannot. Zapadka's
/// safety analysis is tied to a specific PostgreSQL version, so the version
/// under test has to be a fact rather than a hope.
///
/// `testcontainers` composes the image reference as `name:tag`, so the digest
/// is split across the two halves to produce `postgres@sha256:<digest>`.
const IMAGE_NAME: &str = "postgres@sha256";
const IMAGE_DIGEST: &str = "9a8afca54e7861fd90fab5fdf4c42477a6b1cb7d293595148e674e0a3181de15";

/// The PostgreSQL release the digest above corresponds to, asserted at run time
/// so that a digest bump to a different major version cannot pass unnoticed.
const EXPECTED_MAJOR: &str = "18";

const SUPERUSER: &str = "postgres";
const PASSWORD: &str = "zapadka-test";

/// The shared container, started once per test binary.
static POSTGRES: OnceLock<Postgres> = OnceLock::new();

/// A running PostgreSQL container.
struct Postgres {
    /// Held so the container lives as long as the process, and read for its id.
    container: Container<GenericImage>,
    host: String,
    port: u16,
}

/// Returns the shared container, starting it on first use.
fn postgres() -> &'static Postgres {
    POSTGRES.get_or_init(|| {
        let container = GenericImage::new(IMAGE_NAME, IMAGE_DIGEST)
            .with_exposed_port(5432.tcp())
            // The image logs this line twice: once after its bootstrap pass and
            // again when it is genuinely accepting connections.
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_PASSWORD", PASSWORD)
            .with_env_var("POSTGRES_DB", "postgres")
            .start()
            .unwrap_or_else(|error| {
                panic!(
                    "cannot start PostgreSQL {EXPECTED_MAJOR}: {error}\n\n\
                     These tests require Docker. They fail rather than skip, because a database \
                     test that silently skips turns a broken environment into a green build."
                )
            });

        let host = container
            .get_host()
            .expect("container has a host")
            .to_string();
        let port = container
            .get_host_port_ipv4(5432.tcp())
            .expect("container exposes 5432");

        Postgres {
            container,
            host,
            port,
        }
    })
}

/// A disposable database, dropped when the test finishes.
pub struct Database {
    name: String,
}

impl Database {
    /// The connection URI for this database.
    pub fn uri(&self) -> String {
        let postgres = postgres();
        format!(
            "postgresql://{SUPERUSER}:{PASSWORD}@{}:{}/{}",
            postgres.host, postgres.port, self.name
        )
    }

    /// The database's name, useful in a failure message.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Runs a query and returns its rows as strings.
    ///
    /// Deliberately stringly typed: these are assertions about what the
    /// database contains, and a test reads better asserting on `"applied"` than
    /// on a decoded enum.
    pub fn query(&self, sql: &str) -> Vec<Vec<String>> {
        let output = psql(&self.uri(), sql);
        output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.split('|').map(str::to_owned).collect())
            .collect()
    }

    /// Runs a query expected to return exactly one value.
    pub fn scalar(&self, sql: &str) -> String {
        let rows = self.query(sql);
        assert_eq!(rows.len(), 1, "expected one row from {sql:?}, got {rows:?}");
        rows[0].join("|")
    }

    /// Whether a relation exists.
    pub fn has_relation(&self, qualified_name: &str) -> bool {
        self.scalar(&format!(
            "SELECT to_regclass('{qualified_name}') IS NOT NULL"
        )) == "t"
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        // Best effort: a leaked database costs nothing once the container dies,
        // and panicking here would mask the real test failure.
        let admin = admin_uri();
        let _ = try_psql(
            &admin,
            &format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", self.name),
        );
    }
}

/// Creates a fresh database with a name no other test can collide with.
pub fn database() -> Database {
    let postgres = postgres();
    let name = format!("zapadka_{}", Uuid::now_v7().simple());

    psql(&admin_uri(), &format!("CREATE DATABASE \"{name}\""));

    // Verifying the version here rather than trusting the digest means a digest
    // bump to the wrong major version fails on the first test, with a clear
    // message, instead of producing confusing downstream failures.
    let database = Database { name };
    let version = database.scalar("SELECT current_setting('server_version_num')");
    assert!(
        version.starts_with(EXPECTED_MAJOR),
        "the pinned image is PostgreSQL {version}, expected {EXPECTED_MAJOR}.x — \
         update IMAGE_DIGEST and EXPECTED_MAJOR together"
    );
    let _ = postgres;
    database
}

fn admin_uri() -> String {
    let postgres = postgres();
    format!(
        "postgresql://{SUPERUSER}:{PASSWORD}@{}:{}/postgres",
        postgres.host, postgres.port
    )
}

/// Runs SQL through the container's own `psql`, panicking on failure.
fn psql(uri: &str, sql: &str) -> String {
    try_psql(uri, sql).unwrap_or_else(|error| panic!("psql failed for {sql:?}: {error}"))
}

/// Runs SQL through the container's own `psql`.
///
/// Uses the container's client rather than a Rust one so that the harness has
/// no opinion about connection handling; whatever it observes is what an
/// ordinary client would see.
fn try_psql(uri: &str, sql: &str) -> Result<String, String> {
    let container_uri = uri.replace(
        &format!("@{}:{}", postgres().host, postgres().port),
        "@127.0.0.1:5432",
    );
    let output = Command::new("docker")
        .args(["exec", "-i", &container_id(), "psql"])
        .args([
            "-v",
            "ON_ERROR_STOP=1",
            "-tAF|",
            "-d",
            &container_uri,
            "-c",
            sql,
        ])
        .output()
        .map_err(|error| format!("cannot run docker exec: {error}"))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The container's id, looked up once.
fn container_id() -> String {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| postgres().container.id().to_owned())
        .clone()
}

/// A Zapadka project in a temporary directory.
pub struct Project {
    root: Utf8PathBuf,
}

impl Project {
    /// The project root.
    pub fn root(&self) -> &Utf8Path {
        &self.root
    }

    /// Runs `zapadka` and returns what happened.
    pub fn run(&self, args: &[&str]) -> Run {
        let output = Command::new(env!("CARGO_BIN_EXE_zapadka"))
            .arg("-C")
            .arg(self.root.as_str())
            .args(args)
            .output()
            .expect("cannot run the zapadka binary");

        Run {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// Runs `zapadka --output json` and parses the report.
    pub fn report(&self, args: &[&str]) -> Report {
        let mut all = vec!["--output", "json"];
        all.extend_from_slice(args);
        let run = self.run(&all);
        Report {
            json: serde_json::from_str(&run.stdout).unwrap_or_else(|error| {
                panic!(
                    "stdout was not one JSON document ({error})\nstdout:\n{}\nstderr:\n{}",
                    run.stdout, run.stderr
                )
            }),
            run,
        }
    }

    /// Writes a migration package and returns its id.
    pub fn migration(&self, slug: &str, depends: &[Uuid], deploy: &str) -> Uuid {
        self.migration_with(slug, depends, deploy, None, None)
    }

    /// Writes a migration package with optional revert and verify scripts.
    pub fn migration_with(
        &self,
        slug: &str,
        depends: &[Uuid],
        deploy: &str,
        revert: Option<&str>,
        verify: Option<&str>,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let dir = self.root.join("migrations").join(format!("{id}-{slug}"));
        std::fs::create_dir_all(&dir).expect("cannot create a migration directory");

        let depends_list = depends
            .iter()
            .map(|id| format!("\"{id}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let reversibility = if revert.is_some() {
            "reversibility = \"reversible\"".to_owned()
        } else {
            "reversibility = \"irreversible\"\nirreversible_reason = \"test fixture\"".to_owned()
        };

        std::fs::write(
            dir.join("migration.toml"),
            format!(
                "format_version = 1\nid = \"{id}\"\ndepends = [{depends_list}]\n\
                 transaction = \"required\"\n{reversibility}\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("deploy.sql"), deploy).unwrap();
        if let Some(revert) = revert {
            std::fs::write(dir.join("revert.sql"), revert).unwrap();
        }
        if let Some(verify) = verify {
            std::fs::write(dir.join("verify.sql"), verify).unwrap();
        }
        id
    }

    /// Writes a database test file under `tests/db`.
    pub fn test_file(&self, relative_path: &str, sql: &str) {
        let path = self.root.join("tests/db").join(relative_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("cannot create a test directory");
        }
        std::fs::write(path, sql).expect("cannot write a test file");
    }

    /// Rewrites a migration's `revert.sql` or `verify.sql`.
    ///
    /// These are mutable by design, so a script can acquire a problem long
    /// after the migration that owns it was reviewed and deployed. That is the
    /// case worth testing.
    pub fn rewrite_script(&self, id: Uuid, file_name: &str, sql: &str) {
        std::fs::write(self.migration_dir(id).join(file_name), sql).unwrap();
    }

    /// Rewrites a migration's `deploy.sql`, as an ill-advised edit would.
    pub fn rewrite_deploy(&self, id: Uuid, sql: &str) {
        let dir = self.migration_dir(id);
        std::fs::write(dir.join("deploy.sql"), sql).unwrap();
    }

    /// Deletes a migration package entirely.
    pub fn delete_migration(&self, id: Uuid) {
        std::fs::remove_dir_all(self.migration_dir(id)).unwrap();
    }

    /// The directory holding a migration.
    pub fn migration_dir(&self, id: Uuid) -> Utf8PathBuf {
        let migrations = self.root.join("migrations");
        std::fs::read_dir(&migrations)
            .expect("project has a migrations directory")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .find(|name| name.starts_with(&id.to_string()))
            .map_or_else(
                || panic!("no migration {id} in {migrations}"),
                |name| migrations.join(name),
            )
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Creates an initialized project in a temporary directory.
pub fn project() -> Project {
    let root = Utf8PathBuf::from_path_buf(
        std::env::temp_dir().join(format!("zapadka-it-{}", Uuid::now_v7().simple())),
    )
    .expect("the temporary directory is valid UTF-8");
    std::fs::create_dir_all(&root).unwrap();

    let project = Project { root };
    let run = project.run(&["init"]);
    assert_eq!(run.code, 0, "init failed:\n{}", run.stderr);
    project
}

/// The result of running the binary.
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// A run together with its parsed JSON report.
pub struct Report {
    pub run: Run,
    pub json: serde_json::Value,
}

impl Report {
    /// The process exit code.
    pub fn code(&self) -> i32 {
        self.run.code
    }

    /// The report's `outcome` field.
    pub fn outcome(&self) -> &str {
        self.json["outcome"].as_str().unwrap_or_default()
    }

    /// The top-level error code, or an empty string when the run succeeded.
    pub fn error_code(&self) -> &str {
        self.json["error"]["code"].as_str().unwrap_or_default()
    }

    /// The top-level error's `SQLSTATE`, when it came from the server.
    pub fn sqlstate(&self) -> &str {
        self.json["error"]["sqlstate"].as_str().unwrap_or_default()
    }

    /// One structured fact attached to the error.
    pub fn error_context(&self, key: &str) -> &str {
        self.json["error"]["context"][key]
            .as_str()
            .unwrap_or_default()
    }

    /// The migration entries, in report order.
    pub fn migrations(&self) -> &[serde_json::Value] {
        self.json["migrations"]
            .as_array()
            .map_or(&[], Vec::as_slice)
    }

    /// The slugs of migrations with a given status, in report order.
    pub fn slugs_with_status(&self, status: &str) -> Vec<&str> {
        self.migrations()
            .iter()
            .filter(|migration| migration["status"] == status)
            .filter_map(|migration| migration["slug"].as_str())
            .collect()
    }

    /// The diagnostic codes the run reported.
    pub fn diagnostic_codes(&self) -> Vec<&str> {
        self.json["diagnostics"]
            .as_array()
            .map_or(Vec::new(), |diagnostics| {
                diagnostics
                    .iter()
                    .filter_map(|diagnostic| diagnostic["code"].as_str())
                    .collect()
            })
    }

    /// Asserts the run succeeded, printing the report if it did not.
    pub fn assert_success(&self) -> &Self {
        assert_eq!(
            self.code(),
            0,
            "expected success, got exit {} and error {:?}\n{}",
            self.code(),
            self.error_code(),
            serde_json::to_string_pretty(&self.json).unwrap_or_default()
        );
        assert_eq!(self.outcome(), "success");
        self
    }

    /// Asserts the run failed with a specific error code and exit code.
    pub fn assert_failed(&self, error_code: &str, exit_code: i32) -> &Self {
        assert_eq!(
            self.error_code(),
            error_code,
            "expected {error_code}, got:\n{}",
            serde_json::to_string_pretty(&self.json).unwrap_or_default()
        );
        assert_eq!(self.code(), exit_code, "exit code for {error_code}");
        assert_eq!(self.outcome(), "failure");
        self
    }

    /// Replaces values that change between runs, so a report can be compared.
    ///
    /// Run ids, timestamps, durations, and the temporary database name are all
    /// different on every run and say nothing about behaviour. Everything else
    /// is left alone, including hashes: a hash changing *is* a behaviour change.
    pub fn redacted(&self) -> serde_json::Value {
        let mut json = self.json.clone();
        redact(&mut json);
        json
    }
}

/// Runs SQL against a test database, returning the server's error on failure.
///
/// Used to prove that a constraint is enforced by PostgreSQL rather than merely
/// respected by Zapadka.
pub fn try_sql(database: &Database, sql: &str) -> Result<String, String> {
    try_psql(&database.uri(), sql)
}

/// A session holding a project's deployment lock.
pub struct LockHolder {
    child: std::process::Child,
    uri: String,
    backend_pid: String,
    /// Reused on teardown to confirm the lock is genuinely gone.
    holder_query: String,
}

impl Drop for LockHolder {
    fn drop(&mut self) {
        // Killing the local `docker exec` process does not kill the process it
        // started inside the container, so the backend is terminated explicitly.
        // Ending the session is what releases a session-scoped advisory lock,
        // and it is the same path a crashed deployer takes.
        let _ = try_psql(
            &self.uri,
            &format!("SELECT pg_terminate_backend({})", self.backend_pid),
        );
        let _ = self.child.kill();
        let _ = self.child.wait();

        // Terminating a backend is asynchronous. Waiting for the lock to
        // actually be gone means the next command in a test is not racing this
        // teardown.
        for _ in 0..100 {
            let still_held = try_psql(&self.uri, &self.holder_query)
                .map(|rows| !rows.trim().is_empty())
                .unwrap_or(false);
            if !still_held {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

/// Takes a project's deployment lock from an unrelated session.
///
/// Derives the key exactly as Zapadka does, so this reproduces real contention
/// rather than contention on some other key that merely looks similar.
pub fn hold_deployment_lock(database: &Database, project_root: &Utf8Path) -> LockHolder {
    let config = std::fs::read_to_string(project_root.join("zapadka.toml"))
        .expect("the project has a configuration file");
    let project_id = config
        .lines()
        .find_map(|line| line.trim().strip_prefix("id = "))
        .map(|value| value.trim().trim_matches('"').to_owned())
        .expect("the configuration declares a project id");

    let key = deployment_lock_key(&project_id);
    let uri = database.uri();
    let container_uri = uri.replace(
        &format!("@{}:{}", postgres().host, postgres().port),
        "@127.0.0.1:5432",
    );

    let mut child = Command::new("docker")
        .args([
            "exec",
            "-i",
            &container_id(),
            "psql",
            "-tAq",
            "-d",
            &container_uri,
            "-c",
        ])
        .arg(format!(
            "SELECT pg_advisory_lock({key}); SELECT pg_sleep(120);"
        ))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("cannot start a lock-holding session");

    // Wait until the lock is genuinely held before returning, so the test that
    // follows exercises contention rather than racing this setup.
    //
    // The two catalog columns are signed 32-bit, so the halves are narrowed in
    // Rust exactly as `zapadka_pg::lock` does. Interpolating the raw halves
    // would overflow `int` whenever the low word exceeds i32::MAX.
    let holder_query = format!(
        "SELECT pid FROM pg_locks WHERE locktype = 'advisory' \
         AND classid = {}::bigint::int AND objid = {}::bigint::int AND granted",
        (key >> 32) as i32,
        (key & 0xffff_ffff) as i32
    );
    for _ in 0..100 {
        if let Ok(output) = try_psql(&uri, &holder_query) {
            let pid = output.trim();
            if !pid.is_empty() {
                return LockHolder {
                    child,
                    uri,
                    backend_pid: pid.to_owned(),
                    holder_query,
                };
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let _ = child.kill();
    let _ = child.wait();
    panic!("the lock-holding session never acquired the lock");
}

/// Derives a project's advisory lock key, mirroring `zapadka_pg::lock::key_for`.
fn deployment_lock_key(project_id: &str) -> i64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("zapadka.deployment.v1:{project_id}").as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    i64::from_be_bytes(bytes)
}

/// Recursively replaces unstable values with placeholders.
fn redact(value: &mut serde_json::Value) {
    const UNSTABLE: [&str; 7] = [
        "id",
        "started_at",
        "finished_at",
        "duration_ms",
        "database",
        "server_version",
        "applied_at",
    ];

    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if UNSTABLE.contains(&key.as_str()) {
                    *child = serde_json::Value::String(format!("[{key}]"));
                } else {
                    redact(child);
                }
            }
            // Migration ids are UUIDv7 values generated per run, but they are
            // referenced from several places, so they are replaced by a stable
            // label rather than removed.
            if let Some(slug) = map.get("slug").and_then(serde_json::Value::as_str) {
                let slug = slug.to_owned();
                map.insert(
                    "id".to_owned(),
                    serde_json::Value::String(format!("[id of {slug}]")),
                );
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact(item);
            }
        }
        _ => {}
    }
}
