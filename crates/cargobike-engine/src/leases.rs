//! Lease rows (PRD 3.12, A2, F-70..F-73): work per
//! `(application, environment)` serialises through one DB row whose
//! holder names a release. Every operation is conditional, so two
//! concurrent acquirers cannot both win (the database is the arbiter).
//!
//! Split by policy in the interpreter: `supersede` transfers the lease
//! atomically here (F-73), `queue` waits for a `LeaseReleased` signal
//! (F-71), `reject` fails immediately (F-74).

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
    /// This release already held the lease (A1's idempotent no-op).
    HeldAlready,
    /// Another release holds it.
    HeldBy {
        /// The holder's release ID.
        other_release_id: Uuid,
    },
}

/// What a transfer attempt found (F-73's atomic supersede).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseTransfer {
    /// The lease moved: the old holder lost it in the same statement.
    Transferred,
    /// Stale hand-off: something else holds it or the old holder has
    /// already released; the caller (supersede path) refuses rather than
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

    /// Conditional INSERT (A2): no row ⇒ insert this holder; a row with
    /// this holder ⇒ no-op success; any other holder ⇒ visible loss.
    pub async fn acquire(
        &self,
        application: &str,
        environment: &str,
        release_id: Uuid,
    ) -> Result<LeaseAttempt, sqlx::Error> {
        let inserted = sqlx::query(
            "INSERT INTO leases (application, environment, holder_release_id) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (application, environment) DO NOTHING",
        )
        .bind(application)
        .bind(environment)
        .bind(release_id)
        .execute(&self.pool)
        .await?;
        if inserted.rows_affected() > 0 {
            return Ok(LeaseAttempt::Held);
        }
        let holder: Option<Uuid> = sqlx::query_scalar(
            "SELECT holder_release_id FROM leases WHERE application = $1 AND environment = $2",
        )
        .bind(application)
        .bind(environment)
        .fetch_optional(&self.pool)
        .await?;
        match holder {
            Some(holder) if holder == release_id => Ok(LeaseAttempt::HeldAlready),
            Some(holder) => Ok(LeaseAttempt::HeldBy {
                other_release_id: holder,
            }),
            None => Ok(LeaseAttempt::HeldBy {
                other_release_id: Uuid::nil(),
            }),
        }
    }

    /// F-73's atomic supersede transfer: one statement moves holder from
    /// `from` to `to`; a third release cannot slip in between.
    pub async fn transfer(
        &self,
        application: &str,
        environment: &str,
        from: Uuid,
        to: Uuid,
    ) -> Result<LeaseTransfer, sqlx::Error> {
        let moved = sqlx::query(
            "UPDATE leases SET holder_release_id = $3 \
             WHERE application = $1 AND environment = $2 AND holder_release_id = $4",
        )
        .bind(application)
        .bind(environment)
        .bind(to)
        .bind(from)
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

    /// Releases the lease only when held by `holder` (A2's per-environment
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
}
