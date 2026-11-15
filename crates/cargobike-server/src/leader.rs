//! The leader-election fence (F-131): Postgres advisory locks on a
//! dedicated, held-for-life connection — never a pooled one, whose
//! recycling would silently drop the session (and the lock with it).
//!
//! The lock's lifetime IS the connection's: the guard holds one
//! `PgConnection` for the process's whole leadership, pings it on an
//! interval, and reports `Lost` the moment it dies. What happens then
//! is the fence: two active executors are the one unrecoverable state
//! (idempotent steps make a *lost* leader safe; a *split* one is not),
//! so the boot reacts to `Lost` by stopping the executor and exiting
//! the process — the PRD's "simplest reliable option".
//!
//! A standby keeps its connection retrying: when the leader's session
//! closes (its process died; the socket's death is what frees the
//! lock), the standby's next attempt wins and it promotes.
//!
//! The dedicated URL (`leader_election.database_url`, defaulting to
//! `database.url`) is the PgBouncer escape: session advisory locks do
//! not survive transaction pooling, so the election must ride a
//! direct connection (F-131/F-136).

use std::sync::Arc;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use sqlx::Connection as _;
use tokio::sync::watch;

/// The Cargobike leader-election lock key ('cargobk').
pub const LOCK_ID: i64 = 0x0063_6172_676f_626b;

/// How often a leader pings its lock connection (the lost-connection
/// detection window; F-131's fence waits at most this long).
pub const PING_INTERVAL: Duration = Duration::from_secs(5);

/// How often a standby retries the lock (the promotion window after a
/// leader's death).
pub const RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// The guard's transitions, on a watch the boot reacts to.
///
/// `Electing` is the channel's initial value only — the guard replaces
/// it with `Leader` or `Standby` after its first attempt (immediately,
/// when election is disabled), and a boot should wait for that first
/// real verdict before deciding whether to activate the executor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// No attempt has landed yet (the channel's initial value).
    Electing,
    /// The lock is held on a live connection: this replica may run
    /// the executor.
    Leader,
    /// Another replica holds the lock: serve, but do not launch.
    Standby,
    /// The lock connection was lost after winning: fence time (the
    /// boot stops the executor and exits; the guard never re-elects
    /// itself).
    Lost,
}

/// The running guard: the role watch plus the leader-side connection's
/// backend PID (a test's terminate target — `pg_terminate_backend`
/// on it is a REAL lock loss, not a simulated one).
#[derive(Clone)]
pub struct LeaderGuard {
    roles: watch::Receiver<Role>,
    backend_pid: std::sync::Arc<std::sync::Mutex<Option<i32>>>,
}

impl LeaderGuard {
    /// The role watch (every transition after the first lands here;
    /// the first is what [`Role::Electing`] hands the boot).
    pub fn roles(&self) -> watch::Receiver<Role> {
        self.roles.clone()
    }

    /// The lock connection's backend PID once the lock is held (the
    /// leader's; a standby's between-attempts connection is not
    /// exposed — it holds no lock).
    pub fn backend_pid(&self) -> Option<i32> {
        *self
            .backend_pid
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Starts the guard: one task owning the dedicated connection and the
/// role watch. Election disabled = an uncontested win at once (the
/// single-replica dev mode; the boot still routes its activation
/// through the watch, so both paths are the same code).
pub fn spawn(database_url: SecretString, enabled: bool) -> LeaderGuard {
    spawn_with_intervals(database_url, enabled, PING_INTERVAL, RETRY_INTERVAL)
}

/// [`spawn`] with the intervals the tests compress (the fence and the
/// promotion land in seconds, not production's window).
pub fn spawn_with_intervals(
    database_url: SecretString,
    enabled: bool,
    ping: Duration,
    retry: Duration,
) -> LeaderGuard {
    let (role_tx, role_rx) = watch::channel(Role::Electing);
    let backend_pid = std::sync::Arc::new(std::sync::Mutex::new(None));
    let pid_slot = Arc::clone(&backend_pid);
    tokio::spawn(async move {
        if !enabled {
            let _ = role_tx.send(Role::Leader);
            return;
        }
        let url = database_url.expose_secret().to_owned();
        loop {
            // One dedicated connection per attempt; a failure here is
            // a standby's to retry (the leader never gets this far
            // twice — its fence ends the loop).
            let mut connection = match sqlx::PgConnection::connect(&url).await {
                Ok(connection) => connection,
                Err(failure) => {
                    tracing::warn!(%failure, "the leader-election connection refused; retrying");
                    tokio::time::sleep(retry).await;
                    continue;
                }
            };
            let held: bool = match sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                .bind::<i64>(LOCK_ID)
                .fetch_one(&mut connection)
                .await
            {
                Ok(held) => held,
                Err(failure) => {
                    tracing::warn!(%failure, "the advisory-lock attempt failed; retrying");
                    drop(connection);
                    tokio::time::sleep(retry).await;
                    continue;
                }
            };
            if !held {
                let became_standby = role_tx.send_if_modified(|role| {
                    if *role == Role::Standby {
                        false
                    } else {
                        *role = Role::Standby;
                        true
                    }
                });
                if became_standby {
                    tracing::info!("another replica holds the election lock: standing by");
                }
                // The connection closes here: a standby holds nothing
                // (the lock is session-scoped, and holding a session
                // per attempt would block the leader's own retries).
                drop(connection);
                tokio::time::sleep(retry).await;
                continue;
            }
            let pid: i32 = match sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut connection)
                .await
            {
                Ok(pid) => pid,
                Err(failure) => {
                    // The win was real; the pid probe rides the same
                    // connection, so its death is a fence, not a retry.
                    tracing::error!(%failure, "the lock connection died at the win");
                    let _ = role_tx.send(Role::Lost);
                    return;
                }
            };
            *pid_slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(pid);
            tracing::info!(backend_pid = pid, "the election lock is held: leading");
            let _ = role_tx.send(Role::Leader);
            // The fence's whole job: notice the connection's death.
            // `Connection::ping` is the cheap probe (postgres traffic
            // on the held session; a dead socket surfaces here).
            loop {
                tokio::time::sleep(ping).await;
                if let Err(failure) =
                    <sqlx::PgConnection as sqlx::Connection>::ping(&mut connection).await
                {
                    tracing::error!(%failure, "the election lock connection was lost: fencing");
                    let _ = role_tx.send(Role::Lost);
                    return;
                }
            }
        }
    });
    LeaderGuard {
        roles: role_rx,
        backend_pid,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    /// The fixture database URL (the testsupport container/URL; these
    /// tests ride the same self-sufficiency as every DB-backed suite).
    async fn database_url() -> Option<String> {
        cargobike_testsupport::database_url().await
    }

    /// A second guard loses to a first (the contention half), and wins
    /// once the first's connection dies (the failover half).
    #[tokio::test]
    async fn the_second_guard_stands_by_then_wins_on_release() {
        let Some(url) = database_url().await else {
            return;
        };
        let first = spawn_with_intervals(
            secrecy::SecretString::from(url.clone()),
            true,
            Duration::from_millis(200),
            Duration::from_millis(200),
        );
        let mut first_roles = first.roles();
        first_roles
            .wait_for(|role| !matches!(role, Role::Electing))
            .await
            .expect("the first guard elects");
        assert_eq!(*first_roles.borrow(), Role::Leader);
        let pid = first.backend_pid().expect("the leader's backend pid");

        let second = spawn_with_intervals(
            secrecy::SecretString::from(url.clone()),
            true,
            Duration::from_millis(200),
            Duration::from_millis(200),
        );
        let mut second_roles = second.roles();
        second_roles
            .wait_for(|role| !matches!(role, Role::Electing))
            .await
            .expect("the second guard elects");
        assert_eq!(
            *second_roles.borrow(),
            Role::Standby,
            "the lock is contended"
        );

        // The REAL lock loss: terminating the leader's backend kills
        // its session (postgres releases the advisory lock with it).
        let pool = sqlx::PgPool::connect(&url)
            .await
            .expect("the terminate pool");
        sqlx::query("SELECT pg_terminate_backend($1)")
            .bind(pid)
            .execute(&pool)
            .await
            .expect("the terminate");

        // The first guard fences (its connection died)…
        first_roles
            .wait_for(|role| matches!(role, Role::Lost))
            .await
            .expect("the leader fences on lock loss");
        // …and the second promotes on its next attempt.
        second_roles
            .wait_for(|role| matches!(role, Role::Leader))
            .await
            .expect("the standby promotes after the release");
    }

    /// Election disabled elects at once (the single-replica dev mode).
    #[tokio::test]
    async fn disabled_elects_immediately() {
        let guard = spawn(secrecy::SecretString::from("postgres://unused"), false);
        let mut roles = guard.roles();
        roles
            .wait_for(|role| !matches!(role, Role::Electing))
            .await
            .expect("the disabled guard answers");
        assert_eq!(*roles.borrow(), Role::Leader);
    }
}
