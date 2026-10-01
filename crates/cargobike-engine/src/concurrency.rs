//! Concurrency: per `(application, environment)` the interpreter holds
//! the lease before stepping. The decision:
//!
//! - no row / already held ⇒ proceed (a no-op);
//! - held by another: `supersede` transfers the lease atomically — but
//! never a higher version (the guard orders by the scheme, with the
//! creation-time fallback for `opaque`);
//! - `queue` blocks on the `lease/{environment}` signal;
//! - `reject` fails with `ConcurrencyRejected`.
//!
//! The lease is RELEASED when the environment completes or skips
//! (the per-environment rule); failed environments keep it so a retry
//! resumes; cancels settle it via `cargobike.cleanup.v1` .

use std::time::Duration;

use cargobike_core::version::VersionScheme;

use crate::leases::{LeaseAttempt, LeaseRepository, LeaseTransfer};
use crate::signals::InterpreterError;
use crate::snapshot::ReleaseSnapshot;

/// The decision the interpreter asks the environment to take.
#[derive(Clone, Debug)]
pub enum LeaseDecision {
    /// Hold the lease and run the environment.
    Proceed,
    /// The policy's queue: block for the lease's signal .
    WaitForLease,
}

/// Acquires the lease with the policy's semantics; on a `supersede`,
/// version guard refuses higher holders. Returns the decision.
pub async fn enter(
    leases: &LeaseRepository,
    snapshot: &ReleaseSnapshot,
    environment: &crate::template::ResolvedEnvironment,
) -> Result<LeaseDecision, InterpreterError> {
    let application = snapshot.release.application.as_str();
    let release_id = uuid::Uuid::try_parse(&snapshot.release.id).map_err(|_| {
        InterpreterError::Step(cargobike_core::step::StepError::Failed {
            code: cargobike_core::error::STEP_FAILED.to_owned(),
            message: "the snapshot's release id is not a UUID (probe)".to_owned(),
        })
    })?;
    let scheme = &snapshot.release.version_scheme;
    match leases
        .acquire(
            application,
            &environment.name,
            release_id,
            Some(&snapshot.release.version),
        )
        .await
        .map_err(sqlx_failure)?
    {
        LeaseAttempt::Held | LeaseAttempt::HeldAlready => Ok(LeaseDecision::Proceed),
        LeaseAttempt::HeldBy {
            other_release_id, ..
        } => decide_policy(leases, snapshot, environment, scheme, other_release_id).await,
    }
}

/// The policy branch for a held lease (the three values).
async fn decide_policy(
    leases: &LeaseRepository,
    snapshot: &ReleaseSnapshot,
    environment: &crate::template::ResolvedEnvironment,
    scheme: &VersionScheme,
    other_release_id: uuid::Uuid,
) -> Result<LeaseDecision, InterpreterError> {
    use cargobike_core::template::ConcurrencyPolicy as Policy;
    let policy = environment.concurrency;
    match policy {
        Some(Policy::Reject) => Err(InterpreterError::ConcurrencyRejected),
        Some(Policy::Supersede) | None => {
            supersede(leases, snapshot, environment, scheme, other_release_id).await
        }
        Some(Policy::Queue) => Ok(LeaseDecision::WaitForLease),
    }
}

/// The supersede guard: ordering via the scheme — a lower version never
/// supersedes a higher one . `opaque` falls back to creation time:
/// the lease's holder wins a release already newer-in-time, so arrival
/// order decides (the arriving release is later ⇒ it's blocked).
async fn supersede(
    leases: &LeaseRepository,
    snapshot: &ReleaseSnapshot,
    environment: &crate::template::ResolvedEnvironment,
    scheme: &VersionScheme,
    other_release_id: uuid::Uuid,
) -> Result<LeaseDecision, InterpreterError> {
    let to_release = uuid::Uuid::try_parse(&snapshot.release.id).map_err(step_failure)?;
    // The version-scheme guard is fail-CLOSED: without a scheme,
    // without a readable holder version, or with an ordering failure
    // that is not a clear Losing, the arriving release is rejected and
    // a human resolves.
    let other_version = leases
        .version_of(&snapshot.release.application, &environment.name)
        .await
        .map_err(sqlx_failure)?;
    let Some(other_version) = other_version else {
        // The holder has no version (a legacy row): reject.
        return Err(InterpreterError::ConcurrencyRejected);
    };
    match scheme.order(&other_version, &snapshot.release.version) {
        Ok(std::cmp::Ordering::Greater) => {
            return Err(InterpreterError::ConcurrencyRejected);
        }
        Ok(_) => {}
        Err(failure) => {
            return Err(InterpreterError::Step(
                cargobike_core::step::StepError::Failed {
                    code: cargobike_core::error::CONCURRENCY_REJECTED.to_owned(),
                    message: format!("the version-scheme guard refused: {failure}"),
                },
            ));
        }
    }
    // atomic transfer.
    let transfer = leases
        .transfer(
            &snapshot.release.application,
            &environment.name,
            other_release_id,
            to_release,
            Some(&snapshot.release.version),
        )
        .await
        .map_err(sqlx_failure)?;
    match transfer {
        LeaseTransfer::Transferred => Ok(LeaseDecision::Proceed),
        LeaseTransfer::RestState { .. } => {
            // The hand-off raced: treat as a refusal (the next attempt
            // re-decides with fresh facts).
            Err(InterpreterError::ConcurrencyRejected)
        }
    }
}

/// Steps the interpreter calls when the environment is terminal-ok
/// (the releases the lease on complete/skip). The queue's WAKE send
/// needs queued-environment identities ( "the previous holder or
/// the reconciler" — the membership lives in the release store, so the
/// wake path lands with the executor/reconciler wiring); until then the
/// queued release's `QUEUE_WAKE_TIMEOUT` retry is the wake mechanism.
pub async fn release_lease(
    leases: &LeaseRepository,
    snapshot: &ReleaseSnapshot,
    environment: &crate::template::ResolvedEnvironment,
) -> Result<(), InterpreterError> {
    let release_id = uuid::Uuid::try_parse(&snapshot.release.id).map_err(step_failure)?;
    leases
        .release(&snapshot.release.application, &environment.name, release_id)
        .await
        .map_err(sqlx_failure)?;
    Ok(())
}

fn sqlx_failure(failure: sqlx::Error) -> InterpreterError {
    InterpreterError::Step(cargobike_core::step::StepError::Failed {
        code: cargobike_core::error::STEP_FAILED.to_owned(),
        message: format!("the lease work failed: {failure}"),
    })
}

fn step_failure(_prob: uuid::Error) -> InterpreterError {
    InterpreterError::Step(cargobike_core::step::StepError::Failed {
        code: cargobike_core::error::STEP_FAILED.to_owned(),
        message: "the snapshot's release id is not a UUID".to_owned(),
    })
}

/// The queue's unbounded wait: on `LeaseReleased` the caller retries the
/// acquire. Cancels reach the workflow through the cancel machinery .
pub const QUEUE_WAKE_TIMEOUT: Duration = Duration::from_secs(3600);
