//! The deployment lock.
//!
//! Every mutating command holds one session-scoped PostgreSQL advisory lock,
//! from preflight until after the last verification. Session-scoped, not
//! transaction-scoped: a deploy commits each migration separately and still
//! must not interleave with another deployer between those commits.
//!
//! The lock serializes Zapadka runs against each other and nothing else. It
//! does not block application traffic, and it is not a substitute for the row
//! and table locks the migrations themselves take.

use std::time::Duration;

use tokio_postgres::Client;
use uuid::Uuid;
use zapadka_core::duration::Timeout;
use zapadka_core::error::{Error, ErrorCode, Result};

use crate::error::registry_failed;

/// How often to retry while waiting for the lock.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// A held advisory lock.
///
/// Releasing is explicit rather than done on drop: releasing needs to talk to
/// the database, and a `Drop` implementation cannot await. The lock is
/// session-scoped, so if a process dies without releasing, PostgreSQL drops it
/// when the connection closes — which is exactly the behaviour a crashed
/// deployer needs.
#[allow(missing_debug_implementations)] // holds a tokio_postgres::Client, which is not Debug
pub struct DeploymentLock {
    key: i64,
}

impl DeploymentLock {
    /// The advisory lock key this project uses.
    pub fn key(&self) -> i64 {
        self.key
    }

    /// Releases the lock.
    pub async fn release(self, client: &Client) -> Result<()> {
        client
            .execute("SELECT pg_advisory_unlock($1)", &[&self.key])
            .await
            .map_err(|error| registry_failed(error, "release the deployment lock"))?;
        Ok(())
    }
}

/// Derives the advisory lock key for a project.
///
/// Computed in Zapadka from the project id rather than by a PostgreSQL hash
/// function, so the key is the same across server versions and does not depend
/// on any server setting. Two different projects colliding would merely make
/// them wait for each other, never corrupt anything.
pub fn key_for(project_id: Uuid) -> i64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("zapadka.deployment.v1:{project_id}").as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    i64::from_be_bytes(bytes)
}

/// The advisory lock key that serializes *ownership claims*.
///
/// A constant, deliberately: the deployment lock is derived from the project
/// id, so two projects first deploying to the same empty database would take
/// different locks, both find no existing registry, and both create one. The
/// claim has to be serialized on something neither project chooses.
const OWNERSHIP_KEY: i64 = -0x7a70_6164_6b61_0001;

/// Takes the database-global ownership lock.
///
/// Held only across the ownership check and registry creation, which is a
/// handful of statements. Every Zapadka project on a server contends for this
/// one lock, so it is deliberately not held for the duration of a deploy.
pub async fn acquire_ownership(client: &Client, wait: Timeout) -> Result<DeploymentLock> {
    acquire_key(client, OWNERSHIP_KEY, wait).await
}

/// Acquires the deployment lock, waiting up to `wait`.
///
/// A zero `wait` waits indefinitely, matching PostgreSQL's convention for
/// timeouts. Waiting forever must be asked for; the default is short, because a
/// deploy that cannot get the lock promptly is usually racing another deploy,
/// and failing fast tells a pipeline something useful.
pub async fn acquire(client: &Client, project_id: Uuid, wait: Timeout) -> Result<DeploymentLock> {
    acquire_key(client, key_for(project_id), wait).await
}

/// Acquires a specific advisory lock, waiting up to `wait`.
async fn acquire_key(client: &Client, key: i64, wait: Timeout) -> Result<DeploymentLock> {
    if try_lock(client, key).await? {
        return Ok(DeploymentLock { key });
    }

    if wait.is_zero() {
        // Indefinite: hand the waiting to PostgreSQL rather than polling.
        client
            .execute("SELECT pg_advisory_lock($1)", &[&key])
            .await
            .map_err(|error| registry_failed(error, "wait for the deployment lock"))?;
        return Ok(DeploymentLock { key });
    }

    // A syntactically valid but enormous `--wait` would overflow the deadline
    // and panic, abandoning the run without the report it promised. Treated as
    // "wait as long as this machine can represent" instead.
    let deadline = std::time::Instant::now()
        .checked_add(wait.as_std())
        .unwrap_or_else(|| std::time::Instant::now() + Duration::from_secs(365 * 24 * 60 * 60));
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(POLL_INTERVAL.min(wait.as_std())).await;
        if try_lock(client, key).await? {
            return Ok(DeploymentLock { key });
        }
    }

    Err(contention_error(client, key, wait).await)
}

/// Attempts to take the lock without waiting.
async fn try_lock(client: &Client, key: i64) -> Result<bool> {
    let row = client
        .query_one("SELECT pg_try_advisory_lock($1)", &[&key])
        .await
        .map_err(|error| registry_failed(error, "acquire the deployment lock"))?;
    Ok(row.get(0))
}

/// Builds the error for a lock Zapadka could not get, describing who holds it.
///
/// The holder's identity is what makes this actionable: an operator needs to
/// know whether to wait for a colleague's deploy or to investigate a stuck
/// process.
async fn contention_error(client: &Client, key: i64, wait: Timeout) -> Error {
    let mut error = Error::new(
        ErrorCode::LockUnavailable,
        format!("another Zapadka run holds the deployment lock (waited {wait})"),
    )
    .with_context("lock_key", key)
    .with_hint("wait for the other run to finish, or pass --wait to wait longer");

    // Advisory lock keys are split across two catalog columns.
    let classid = (key >> 32) as i32;
    let objid = (key & 0xffff_ffff) as i32;

    let holders = client
        .query(
            "SELECT activity.pid, \
                    coalesce(activity.application_name, ''), \
                    coalesce(activity.client_addr::text, ''), \
                    coalesce(activity.state, ''), \
                    coalesce(to_char(activity.query_start, 'YYYY-MM-DD\"T\"HH24:MI:SSOF'), '') \
             FROM pg_locks lock \
             JOIN pg_stat_activity activity ON activity.pid = lock.pid \
             WHERE lock.locktype = 'advisory' \
               AND lock.classid = $1::bigint::int \
               AND lock.objid = $2::bigint::int \
               AND lock.granted",
            &[&i64::from(classid), &i64::from(objid)],
        )
        .await;

    // Reading pg_locks needs privileges the deploying role may not have. Losing
    // the diagnostic must not change the failure: the lock is still unavailable.
    if let Ok(rows) = holders
        && let Some(row) = rows.first()
    {
        error = error.with_context("holder_pid", row.get::<_, i32>(0));
        for (index, name) in [
            (1, "holder_application"),
            (2, "holder_client"),
            (3, "holder_state"),
            (4, "holder_query_start"),
        ] {
            let value: String = row.get(index);
            if !value.is_empty() {
                error = error.with_context(name, value);
            }
        }
    }

    error
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;

    #[test]
    fn the_key_is_stable_for_a_project() {
        let id = Uuid::parse_str("0198f5c0-0000-7000-8000-00000000000a").unwrap();
        assert_eq!(key_for(id), key_for(id));
    }

    #[test]
    fn different_projects_get_different_keys() {
        let a = Uuid::parse_str("0198f5c0-0000-7000-8000-00000000000a").unwrap();
        let b = Uuid::parse_str("0198f5c0-0000-7000-8000-00000000000b").unwrap();
        assert_ne!(key_for(a), key_for(b));
    }

    #[test]
    fn the_key_derivation_is_pinned() {
        // Changing this would let an old and a new binary deploy concurrently
        // to the same database, each believing it held the lock.
        let id = Uuid::parse_str("0198f5c0-0000-7000-8000-00000000000a").unwrap();
        let expected = {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(
                b"zapadka.deployment.v1:0198f5c0-0000-7000-8000-00000000000a".as_slice(),
            );
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&digest[..8]);
            i64::from_be_bytes(bytes)
        };
        assert_eq!(key_for(id), expected);
    }

    #[test]
    fn keys_split_into_the_two_catalog_columns_without_loss() {
        for id in [Uuid::nil(), Uuid::max(), Uuid::now_v7()] {
            let key = key_for(id);
            let classid = (key >> 32) as i32;
            let objid = (key & 0xffff_ffff) as i32;
            let recombined = (i64::from(classid) << 32) | (i64::from(objid) & 0xffff_ffff);
            assert_eq!(recombined, key, "{id}");
        }
    }
}
