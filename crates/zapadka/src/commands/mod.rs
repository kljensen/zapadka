//! Command implementations.
//!
//! Each command records what it did into the [`crate::session::Session`] and
//! returns `Ok(())` or one [`zapadka_core::error::Error`]. None of them print
//! anything or choose an exit code; that belongs to the report.

pub mod deploy;
pub mod init;
pub mod lint;
pub mod new;
pub mod status;
pub mod target;
pub mod verify;

use camino::Utf8Path;
use zapadka_core::config::{LoadedConfig, load_from};
use zapadka_core::error::Result;
use zapadka_core::graph::Graph;
use zapadka_core::lint::Capabilities;
use zapadka_core::migration::discover;

/// What this build can execute.
///
/// Alpha ships the transactional slice, so a project that declares
/// `transaction = "forbidden"` fails validation rather than failing partway
/// through a deploy.
pub const CAPABILITIES: Capabilities = Capabilities::TRANSACTIONAL_ONLY;

/// Loads the project and builds its validated migration graph.
///
/// Every command that reads migrations goes through here, so no command can
/// operate on a graph with a cycle, a missing dependency, or a duplicate id.
pub fn load_project(directory: &Utf8Path) -> Result<(LoadedConfig, Graph)> {
    let config = load_from(directory)?;
    let migrations = discover(&config.root)?;
    let graph = Graph::build(migrations)?;
    Ok((config, graph))
}
