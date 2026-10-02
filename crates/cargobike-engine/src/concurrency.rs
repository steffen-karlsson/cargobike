//! Concurrency: per `(application, environment)` the interpreter holds
//! the lease before stepping. The decision:
//!
//! - no row / already held ⇒ proceed (a no-op);
//! - held by another: `supersede` transfers the lease atomically — but
//!   never a higher version (the guard orders by the scheme, with the
//!   creation-time fallback for `opaque`);
//! - `queue` blocks on the `lease/{environment}` signal;
//! - `reject` fails with `ConcurrencyRejected`.
//!
//! The lease is RELEASED when the environment completes or skips
//! (the per-environment rule); failed environments keep it so a retry
//! resumes; cancels settle it via `cargobike.cleanup.v1` .

use std::time::Duration;

use cargobike_core::version::VersionScheme;

use crate::interpreter::InterpreterServices;
use crate::leases::{LeaseAttempt, LeaseTransfer};
use crate::signals::{InterpreterError, Signal};
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
    services: &InterpreterServices,
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
    let leases = services.leases.as_ref();
    let held = leases
        .acquire(
            application,
            &environment.name,
            release_id,
            Some(&snapshot.release.version),
        )
        .await
        .map_err(sqlx_failure)?;
    match held {
        LeaseAttempt::Held | LeaseAttempt::HeldAlready => Ok(LeaseDecision::Proceed),
        LeaseAttempt::HeldBy {
            other_release_id, ..
        } => decide_policy(services, snapshot, environment, scheme, other_release_id).await,
    }
}

/// The policy branch for a held lease (the three values).
async fn decide_policy(
    services: &InterpreterServices,
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
            supersede(services, snapshot, environment, scheme, other_release_id).await
        }
        Some(Policy::Queue) => Ok(LeaseDecision::WaitForLease),
    }
}

/// The supersede guard: ordering via the scheme — a lower version never
/// supersedes a higher one . `opaque` falls back to creation time:
/// the lease's holder wins a release already newer-in-time, so arrival
/// order decides (the arriving release is later ⇒ it's blocked).
async fn supersede(
    services: &InterpreterServices,
    snapshot: &ReleaseSnapshot,
    environment: &crate::template::ResolvedEnvironment,
    scheme: &VersionScheme,
    other_release_id: uuid::Uuid,
) -> Result<LeaseDecision, InterpreterError> {
    let to_release = uuid::Uuid::try_parse(&snapshot.release.id).map_err(step_failure)?;
    let leases = services.leases.as_ref();
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
    let leases = services.leases.as_ref();
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
        LeaseTransfer::Transferred => {
            supersede_chain(services, snapshot, environment, other_release_id).await?;
            Ok(LeaseDecision::Proceed)
        }
        LeaseTransfer::RestState { .. } => {
            // The hand-off raced: treat as a refusal (the next attempt
            // re-decides with fresh facts).
            Err(InterpreterError::ConcurrencyRejected)
        }
    }
}

/// The supersede chain on a won lease: the old release's attempt is
/// cancelled (a recorded workflow op), its environment's status marks
/// Superseded (the rollup goes terminal), and the cleanup child starts
/// with the hand-over (close the CR with the replacing-release
/// comment, delete the branch, the lease is already transferred).
async fn supersede_chain(
    services: &InterpreterServices,
    snapshot: &ReleaseSnapshot,
    environment: &crate::template::ResolvedEnvironment,
    old_release_id: uuid::Uuid,
) -> Result<(), InterpreterError> {
    use cargobike_core::model::EnvironmentPhase;
    let old_workflow = crate::interpreter::interpret_workflow_id(old_release_id);
    if let Err(failure) = services.instance.cancel(&old_workflow).await {
        tracing::warn!(%failure, release = %old_release_id, "the superseded attempt's cancel failed");
    }
    // The old environment's status: Superseded (the rollup terminal).
    if let Some(record) = services
        .statuses
        .load_environment(old_release_id, &environment.name)
        .await
        .map_err(crate::interpreter::status_to_interpreter_error)?
    {
        // The env's CR record to the cleanup's summary shape.
        let change_request = record
            .change_request
            .map(|reference| crate::cleanup::CrSummary {
                number: reference.number,
                repo: reference.target_repo,
                head_sha: reference.head_sha,
            });
        if let Err(failure) = services
            .statuses
            .environment_terminal(
                old_release_id,
                &environment.name,
                EnvironmentPhase::Superseded,
                None,
                time::OffsetDateTime::now_utc(),
            )
            .await
        {
            tracing::warn!(%failure, release = %old_release_id, "the supersede's status write refused");
        }
        // The cleanup child: the deterministic id converges a replayed
        // start (the same child joins rather than double-runs).
        let targets = vec![crate::cleanup::CleanupTarget {
            application: snapshot.release.application.clone(),
            environment: environment.name.clone(),
            branch: {
                // The old attempt's branch name: the same deterministic
                // format the environment configured (the application's
                // branch format is shared by both releases).
                let placeholders = std::collections::BTreeMap::from([
                    (
                        "application".to_owned(),
                        snapshot.release.application.clone(),
                    ),
                    ("environment".to_owned(), environment.name.clone()),
                    ("release_id".to_owned(), old_release_id.to_string()),
                ]);
                Some(crate::names::expand(
                    branch_format_of(snapshot, environment).as_str(),
                    &placeholders,
                ))
            },
            change_request,
            superseded_by: Some(snapshot.release.id.clone()),
            to_version: Some(snapshot.release.version.clone()),
        }];
        if let Some(cleanup) = services.cleanup_ref.get() {
            let release_id_text = old_release_id.to_string();
            let child_id = crate::interpreter::cleanup_workflow_id(&release_id_text);
            let started = cleanup
                .start_with(
                    crate::cleanup::CleanupArgs {
                        release_id: release_id_text,
                        targets,
                    },
                    dbos::StartOptions {
                        workflow_id: Some(child_id.as_str()),
                        ..dbos::StartOptions::default()
                    },
                )
                .await;
            if let Err(failure) = started {
                tracing::warn!(release = %old_release_id, ?failure, "the supersede's cleanup start failed");
            }
        }
    }
    Ok(())
}

/// The branch format the arriving release's snapshot carries (the env's
/// staged input wins; the default format is the fallback).
fn branch_format_of(
    snapshot: &ReleaseSnapshot,
    environment: &crate::template::ResolvedEnvironment,
) -> String {
    let _ = environment;
    snapshot
        .template
        .environments
        .iter()
        .find(|env_snapshot| env_snapshot.name == environment.name)
        .and_then(|env_snapshot| env_snapshot.env_inputs.get("branch_format"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| crate::names::DEFAULT_BRANCH_FORMAT.to_owned())
}

/// Steps the interpreter calls when the environment is terminal-ok
/// (the releases the lease on complete/skip). The wake goes to every
/// waiting release's current attempt directly: the queue's membership
/// reads the release status store, the send rides the lease topic with
/// the release event's idempotency key (F-20a's shape: one send per
/// lease-release event).
pub async fn release_lease(
    services: &InterpreterServices,
    snapshot: &ReleaseSnapshot,
    environment: &crate::template::ResolvedEnvironment,
) -> Result<(), InterpreterError> {
    let release_id = uuid::Uuid::try_parse(&snapshot.release.id).map_err(step_failure)?;
    services
        .leases
        .release(&snapshot.release.application, &environment.name, release_id)
        .await
        .map_err(sqlx_failure)?;
    // The waiting releases for this lease: the F-71 wake.
    let waiting = services
        .statuses
        .waiting_releases(&snapshot.release.application, &environment.name)
        .await
        .map_err(sqlx_status_failure)?;
    if waiting.is_empty() {
        return Ok(());
    }
    let resource = format!("{}/{}", snapshot.release.application, environment.name);
    let key = crate::signals::lease_release_key(&snapshot.release.application, &environment.name);
    let topic = crate::signals::lease_topic(&environment.name);
    for (waiting_release, workflow_id) in waiting {
        let workflow_id_owned = workflow_id;
        let message = Signal::LeaseReleased {
            resource: resource.clone(),
        };
        let options = dbos::SendOptions {
            topic: Some(topic.as_str()),
            idempotency_key: Some(key.as_str()),
            ..dbos::SendOptions::default()
        };
        let sent: dbos::Result<(), dbos::EngineOnly> =
            dbos::send_with(&workflow_id_owned, &message, options).await;
        if let Err(failure) = sent {
            // A missed wake is soft: the waiting attempt's recv re-tries
            // at its own wake timeout.
            tracing::warn!(
                release = %waiting_release,
                %failure,
                "the queue's wake send failed"
            );
        }
    }
    Ok(())
}

/// The status store's failure mapped like the rest.
fn sqlx_status_failure(failure: crate::status::StatusError) -> InterpreterError {
    InterpreterError::Step(cargobike_core::step::StepError::Failed {
        code: cargobike_core::error::STEP_FAILED.to_owned(),
        message: format!("failed to read the queue's waiters: {failure}"),
    })
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
