//! Lease rows: work per
//! `(the application, environment)` serialises through one DB row whose
//! holder names a release. Every operation is conditional, so two
//! concurrent acquirers cannot both win (the database is the arbiter).
//!
//! Split by policy in the interpreter: `supersede` transfers the lease
//! atomically here , `queue` waits for a `LeaseReleased` signal
//! , `reject` fails immediately .

use sqlx::PgPool;
use uuid::Uuid;

/// SQL access for lease rows.
pub struct LeaseRepository {
    pool: PgPool,
}

/// What an acquire attempt found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseAttempt {
    /// This release now holds the lease.
    Held,
    /// This release already held the lease (the an idempotent no-op).
    HeldAlready,
    /// Another release holds it, with its version for compare.
    HeldBy {
        /// The holder's release ID.
        other_release_id: Uuid,
        /// The holder's version, when recorded.
        other_version: Option<String>,
    },
}

/// What a transfer attempt found (the atomic supersede).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseTransfer {
    /// The lease moved: the old holder lost it in the same statement.
    Transferred,
    /// Stale hand-off: something else holds it or the old holder has
    /// already released; the caller (the supersede path) refuses rather than
    /// fighting.
    RestState {
        /// Who actually holds it now, when held.
        holder: Option<Uuid>,
    },
}

impl LeaseRepository {
    /// A lease repository over the shared pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Conditional INSERT : no row ⇒ insert this holder; a row with
    /// this holder ⇒ no-op success; any other holder ⇒ visible loss.
    pub async fn acquire(
        &self,
        application: &str,
        environment: &str,
        release_id: Uuid,
        holder_version: Option<&str>,
    ) -> Result<LeaseAttempt, sqlx::Error> {
        let inserted = sqlx::query(
            "INSERT INTO leases (application, environment, holder_release_id, holder_version) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (application, environment) DO NOTHING",
        )
        .bind(application)
        .bind(environment)
        .bind(release_id)
        .bind(holder_version)
        .execute(&self.pool)
        .await?;
        if inserted.rows_affected() > 0 {
            return Ok(LeaseAttempt::Held);
        }
        let state: Option<(Uuid, Option<String>)> = sqlx::query_as(
            "SELECT holder_release_id, holder_version FROM leases \
             WHERE application = $1 AND environment = $2",
        )
        .bind(application)
        .bind(environment)
        .fetch_optional(&self.pool)
        .await?;
        match state {
            Some((other, _other_version)) if other == release_id => {
                // A retry of the same release updates the recorded version.
                sqlx::query(
                    "UPDATE leases SET holder_version = $3 \
                     WHERE application = $1 AND environment = $2",
                )
                .bind(application)
                .bind(environment)
                .bind(holder_version)
                .execute(&self.pool)
                .await?;
                Ok(LeaseAttempt::HeldAlready)
            }
            Some((other, other_version)) => Ok(LeaseAttempt::HeldBy {
                other_release_id: other,
                other_version,
            }),
            None => Ok(LeaseAttempt::HeldBy {
                other_release_id: Uuid::nil(),
                other_version: None,
            }),
        }
    }

    /// atomic supersede transfer: one statement moves holder from
    /// `from` to `to` (the stamping the new holder's version); a third
    /// release cannot slip in between.
    pub async fn transfer(
        &self,
        application: &str,
        environment: &str,
        from: Uuid,
        to: Uuid,
        to_version: Option<&str>,
    ) -> Result<LeaseTransfer, sqlx::Error> {
        let moved = sqlx::query(
            "UPDATE leases SET holder_release_id = $3, holder_version = $5 \
             WHERE application = $1 AND environment = $2 AND holder_release_id = $4",
        )
        .bind(application)
        .bind(environment)
        .bind(to)
        .bind(from)
        .bind(to_version)
        .execute(&self.pool)
        .await?;
        if moved.rows_affected() > 0 {
            return Ok(LeaseTransfer::Transferred);
        }
        let holder: Option<Uuid> = sqlx::query_scalar(
            "SELECT holder_release_id FROM leases WHERE application = $1 AND environment = $2",
        )
        .bind(application)
        .bind(environment)
        .fetch_optional(&self.pool)
        .await?;
        Ok(LeaseTransfer::RestState { holder })
    }

    /// Releases the lease only when held by `holder` (the per-environment
    /// release; `cargobike.cleanup.v1` on cancel uses the same form). The
    /// row goes with it — the table is the live state only.
    pub async fn release(
        &self,
        application: &str,
        environment: &str,
        holder: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let released = sqlx::query(
            "DELETE FROM leases \
             WHERE application = $1 AND environment = $2 AND holder_release_id = $3",
        )
        .bind(application)
        .bind(environment)
        .bind(holder)
        .execute(&self.pool)
        .await?;
        Ok(released.rows_affected() > 0)
    }

    /// Reads who holds it (the reconciler wakes queued waiters).
    pub async fn holder(
        &self,
        application: &str,
        environment: &str,
    ) -> Result<Option<Uuid>, sqlx::Error> {
        let holder: Option<Uuid> = sqlx::query_scalar(
            "SELECT holder_release_id FROM leases WHERE application = $1 AND environment = $2",
        )
        .bind(application)
        .bind(environment)
        .fetch_optional(&self.pool)
        .await?;
        Ok(holder)
    }

    /// The holder's version (the compare's input).
    pub async fn version_of(
        &self,
        application: &str,
        environment: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        let version: Option<Option<String>> = sqlx::query_scalar(
            "SELECT holder_version FROM leases WHERE application = $1 AND environment = $2",
        )
        .bind(application)
        .bind(environment)
        .fetch_optional(&self.pool)
        .await?;
        Ok(version.flatten())
    }
}
