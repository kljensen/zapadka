//! Command implementations.
//!
//! Each command records what it did into the [`crate::session::Session`] and
//! returns `Ok(())` or one [`zapadka_core::error::Error`]. None of them print
//! anything or choose an exit code; that belongs to the report.

pub mod baseline;
pub mod deploy;
pub mod init;
pub mod lint;
pub mod new;
pub mod resolve;
pub mod revert;
pub mod status;
pub mod target;
pub mod test;
pub mod verify;

use camino::Utf8Path;
use zapadka_core::config::{LoadedConfig, load_from};
use zapadka_core::error::Result;
use zapadka_core::graph::Graph;
use zapadka_core::lint::Capabilities;
use zapadka_core::migration::discover;

/// What this build can execute.
///
/// Nontransactional migrations became executable once the recovery path they
/// need existed: an attempt recorded before the statement runs, a target that
/// blocks while an outcome is unknown, and `zapadka resolve` for the operator's
/// account of what happened. The capability and that machinery ship together,
/// because the execution mode without the recovery is the dangerous half.
pub const CAPABILITIES: Capabilities = Capabilities::ALL;

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
