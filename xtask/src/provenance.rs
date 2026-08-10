//! Verifying that vendored third-party files are exactly what upstream shipped.
//!
//! Zapadka compiles a copy of PostgreSQL's parser into its binary and will
//! eventually ship a copy of pgTAP. Both are safety-relevant: the parser
//! decides whether a migration is allowed to run, and pgTAP decides whether a
//! test passed. A local edit to either — deliberate or accidental — changes
//! what Zapadka promises while still looking like upstream code in review.
//!
//! So every vendored file's hash is recorded, and this check re-derives them.
//! It fails on a modified file, a missing file, and equally on an *unrecorded*
//! file, because a file nobody recorded is a file nobody reviewed.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The manifest each vendored tree carries.
const MANIFEST: &str = "PROVENANCE.toml";

/// The per-file checksum list each vendored tree carries.
const CHECKSUMS: &str = "SHA256SUMS";

/// Files that describe the vendored tree rather than belonging to it.
const METADATA: [&str; 3] = [MANIFEST, CHECKSUMS, "NOTICE.md"];

#[derive(Debug, Deserialize)]
struct Provenance {
    format_version: u32,
    source: Source,
}

#[derive(Debug, Deserialize)]
struct Source {
    id: String,
    upstream: String,
    release: String,
    revision: String,
    classification: String,
    license: String,
    files_sha256: String,
    file_count: usize,
    #[serde(default)]
    archive_sha256: Option<String>,
}

/// Verifies every vendored tree under `third_party/`.
pub fn verify() -> Result<()> {
    let root = crate::repo_root()?;
    let third_party = root.join("third_party");
    if !third_party.is_dir() {
        // Nothing vendored yet is a valid state, not a failure.
        return Ok(());
    }

    let mut checked = 0usize;
    for entry in std::fs::read_dir(&third_party)? {
        let path = Utf8PathBuf::from_path_buf(entry?.path())
            .map_err(|path| anyhow::anyhow!("{} is not a UTF-8 path", path.display()))?;
        if path.is_dir() {
            verify_tree(&path)?;
            checked += 1;
        }
    }

    if checked == 0 {
        bail!("third_party/ exists but contains no vendored trees");
    }

    verify_scenarios(&root)?;
    Ok(())
}

/// The scenarios file: Zapadka-owned tests adapted from other tools.
#[derive(Debug, Deserialize)]
struct Scenarios {
    format_version: u32,
    #[serde(default)]
    scenario: Vec<Scenario>,
}

/// One adapted scenario and where its idea came from.
#[derive(Debug, Deserialize)]
struct Scenario {
    name: String,
    /// `path/to/file.rs::test_name`.
    test: String,
    inspired_by: String,
    source: String,
    classification: String,
}

/// Checks that every adapted scenario names a test that actually exists.
///
/// The scenarios file records where an idea came from. Its value depends
/// entirely on the reference still pointing at something, and a renamed test
/// would otherwise turn it into folklore.
fn verify_scenarios(root: &Utf8Path) -> Result<()> {
    let path = root.join("tests/fixtures/provenance.toml");
    if !path.is_file() {
        return Ok(());
    }

    let text = std::fs::read_to_string(&path)?;
    let scenarios: Scenarios = toml::from_str(&text)
        .with_context(|| format!("{path} is not a valid scenario manifest"))?;
    if scenarios.format_version != 1 {
        bail!("{path} declares an unsupported format_version");
    }

    let mut missing = Vec::new();
    for scenario in &scenarios.scenario {
        for (field, value) in [
            ("name", &scenario.name),
            ("inspired_by", &scenario.inspired_by),
            ("source", &scenario.source),
        ] {
            if value.trim().is_empty() {
                bail!("{path}: scenario {:?} leaves {field} empty", scenario.name);
            }
        }
        if scenario.classification != "adapted" {
            bail!(
                "{path}: scenario {:?} is classified {:?}; scenarios are always \"adapted\", \
                 because a literal copy belongs in third_party/ instead",
                scenario.name,
                scenario.classification
            );
        }

        let Some((source_path, test_name)) = scenario.test.split_once("::") else {
            bail!(
                "{path}: scenario {:?} has test {:?}, expected '<file>::<test name>'",
                scenario.name,
                scenario.test
            );
        };
        let found = std::fs::read_to_string(root.join(source_path))
            .is_ok_and(|contents| contents.contains(&format!("fn {test_name}(")));
        if !found {
            missing.push(format!("  {} -> {}", scenario.name, scenario.test));
        }
    }

    if !missing.is_empty() {
        bail!(
            "{path} references tests that no longer exist:\n{}\n\n\
             Rename the reference or remove the scenario; a provenance record that points at \
             nothing is worse than none.",
            missing.join("\n")
        );
    }
    Ok(())
}

/// Verifies one vendored tree.
fn verify_tree(tree: &Utf8Path) -> Result<()> {
    let manifest_path = tree.join(MANIFEST);
    let text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("{tree} has no {MANIFEST}; every vendored tree needs one"))?;
    let provenance: Provenance = toml::from_str(&text)
        .with_context(|| format!("{manifest_path} is not a valid provenance manifest"))?;

    if provenance.format_version != 1 {
        bail!(
            "{manifest_path} declares format_version {}, which this xtask does not understand",
            provenance.format_version
        );
    }
    require_nonempty(&provenance.source, &manifest_path)?;

    // The checksum file is the record of what was reviewed; its own hash is in
    // the manifest so neither can be edited alone.
    let checksums_path = tree.join(CHECKSUMS);
    let checksums =
        std::fs::read(&checksums_path).with_context(|| format!("{tree} has no {CHECKSUMS}"))?;
    let recorded = hex(&Sha256::digest(&checksums));
    if recorded != provenance.source.files_sha256 {
        bail!(
            "{checksums_path} has changed: {MANIFEST} records {}, the file hashes to {recorded}",
            provenance.source.files_sha256
        );
    }

    let expected = parse_checksums(&String::from_utf8_lossy(&checksums), &checksums_path)?;
    if expected.len() != provenance.source.file_count {
        bail!(
            "{manifest_path} says {} files, {CHECKSUMS} lists {}",
            provenance.source.file_count,
            expected.len()
        );
    }

    let actual = hash_tree(tree)?;

    let mut problems = Vec::new();
    for (path, want) in &expected {
        match actual.get(path) {
            Some(got) if got == want => {}
            Some(got) => problems.push(format!(
                "  modified: {path}\n    recorded {want}\n    found    {got}"
            )),
            None => problems.push(format!("  missing:  {path}")),
        }
    }
    for path in actual.keys() {
        if !expected.contains_key(path) {
            problems.push(format!("  unrecorded: {path}"));
        }
    }

    if !problems.is_empty() {
        bail!(
            "{} ({} {}) does not match its recorded provenance:\n{}\n\n\
             Vendored files are never edited in place. Re-vendor from upstream, or if this change \
             is intended, update {CHECKSUMS} and {MANIFEST} in a commit that explains why.",
            tree,
            provenance.source.id,
            provenance.source.release,
            problems.join("\n")
        );
    }
    Ok(())
}

/// Fails when a field that must identify the source is blank.
fn require_nonempty(source: &Source, path: &Utf8Path) -> Result<()> {
    let fields = [
        ("id", &source.id),
        ("upstream", &source.upstream),
        ("release", &source.release),
        ("revision", &source.revision),
        ("license", &source.license),
        ("classification", &source.classification),
    ];
    for (name, value) in fields {
        if value.trim().is_empty() {
            bail!("{path} leaves {name} empty; provenance must identify what was vendored");
        }
    }
    if !matches!(source.classification.as_str(), "exact" | "adapted") {
        bail!(
            "{path} classifies the source as {:?}; it must be \"exact\" or \"adapted\"",
            source.classification
        );
    }
    // An `exact` tree came from a published archive, so the archive's own hash
    // is what ties it to the release.
    if source.classification == "exact" && source.archive_sha256.is_none() {
        bail!("{path} is classified exact but records no archive_sha256");
    }
    Ok(())
}

/// Parses `SHA256SUMS` into a path-to-hash map.
fn parse_checksums(text: &str, path: &Utf8Path) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let (hash, file) = line
            .split_once("  ")
            .with_context(|| format!("{path}:{}: expected '<sha256>  <path>'", index + 1))?;
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("{path}:{}: {hash:?} is not a SHA-256 digest", index + 1);
        }
        if map.insert(file.to_owned(), hash.to_lowercase()).is_some() {
            bail!("{path}:{}: {file} is listed twice", index + 1);
        }
    }
    Ok(map)
}

/// Hashes every file in a vendored tree, excluding its own metadata.
fn hash_tree(tree: &Utf8Path) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for entry in walkdir::WalkDir::new(tree)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = Utf8Path::from_path(entry.path())
            .ok_or_else(|| anyhow::anyhow!("{} is not a UTF-8 path", entry.path().display()))?;
        let relative = path
            .strip_prefix(tree)
            .unwrap_or(path)
            .as_str()
            .replace('\\', "/");
        if METADATA.contains(&relative.as_str()) {
            continue;
        }
        map.insert(relative, hex(&Sha256::digest(std::fs::read(path)?)));
    }
    Ok(map)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}
