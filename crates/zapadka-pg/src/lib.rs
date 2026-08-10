//! Zapadka's PostgreSQL 18 adapter.
//!
//! Everything that talks to a database lives here: connecting, the registry,
//! the deployment lock, and runner-owned execution. The domain crate knows what
//! a migration graph means; this crate knows what a server does about it.
//!
//! Only `deploy.sql`, `revert.sql`, `verify.sql`, and database test files are
//! ever sent as raw SQL. Every query Zapadka issues about its own state uses
//! bound parameters and the extended protocol.

pub mod connect;
pub mod error;
pub mod execute;
pub mod history;
pub mod lock;
pub mod pgtap;
pub mod registry;
pub mod service;
pub mod testrun;

pub use connect::{Connection, Source, connect, resolve};
pub use execute::{Runner, ScriptOutcome, Timeouts};
pub use lock::DeploymentLock;
pub use registry::{AppliedMigration, RegistryState, ServerFacts};
pub use tokio_postgres::Client;
