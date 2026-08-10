//! Zapadka's domain logic: configuration, migration packages, the dependency
//! graph, integrity hashing, linting, and the public report model.
//!
//! Nothing here touches a database or a terminal. That separation is what lets
//! the rules a deploy enforces be tested without either.

pub mod config;
pub mod duration;
pub mod error;
pub mod graph;
pub mod lint;
pub mod manifest;
pub mod migration;
pub mod report;
