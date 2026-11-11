//! The cleanup workflow: a separate durable
//! workflow started from the cancel and supersede paths — the cancelled
//! interpreter cannot run its own compensation.
//!
//! Work per environment: close the CR (the comment links to the replacing CR
//! under supersede), delete the release branch, then settle the lease —
//! release, or transfer to the new holder in one statement .
//! Every action idempotent ; a replayed cleanup converges.

use std::sync::Arc;
use std::time::Duration;

use cargobike_core::model::RepoRef as CoreRepoRef;
use cargobike_core::provider::Provider;
use cargobike_core::registry::ProviderRegistry;

use crate::leases::{LeaseRepository, LeaseTransfer};
use crate::signals::InterpreterError;

/// The registered cleanup workflow name .
pub const CLEANUP_WORKFLOW: &str = "cargobike.cleanup.v1";

/// The cleanup's compensations are best-effort, but they still run as
/// durable steps : each has a checkpoint (a crash mid-cleanup
/// resumes at the same compensation, not from the first target) and a
/// bounded retry policy (transient provider wobble retries three
/// times; a permanent failure is recorded and the cleanup moves on).
fn cleanup_options() -> dbos::StepOptions<InterpreterError> {
    dbos::StepOptions {
        max_attempts: 3,
        interval: Duration::from_secs(1),
        backoff_rate: 2.0,
        max_interval: Duration::from_secs(30),
        should_retry: Some(std::sync::Arc::new(
            |failure: &dbos::Error<InterpreterError>| {
                matches!(
                    failure,
                    dbos::Error::StepTimeout { .. }
                        | dbos::Error::Application(InterpreterError::Step(
                            cargobike_core::step::StepError::Transient { reason: _ }
                        ))
                )
            },
        )),
        ..dbos::StepOptions::default()
    }
}

/// A provider failure as the step's error: request wobble is transient
/// (the retry policy applies); the rest are permanent (recorded, the
/// cleanup continues with the next compensation).
fn provider_step_failure(failure: cargobike_core::provider::ProviderError) -> InterpreterError {
    match failure {
        cargobike_core::provider::ProviderError::Request(cause) => {
            InterpreterError::Step(cargobike_core::step::StepError::Transient { reason: cause })
        }
        other => InterpreterError::Step(cargobike_core::step::StepError::Failed {
            code: cargobike_core::error::STEP_FAILED.to_owned(),
            message: other.to_string(),
        }),
    }
}

/// What a cleanup serves: the CR/branch facts of one environment.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CleanupTarget {
    /// The application's name (the lease + comment context).
    pub application: String,
    /// The environment the release was progressing through.
    pub environment: String,
    /// The release's branch (the deterministic name), when it was cut.
    pub branch: Option<String>,
    /// The open CR this release holds, when any (the row's form).
    pub change_request: Option<CrSummary>,
    /// Under `supersede`: the new release becomes the lease's holder in
    /// the same statement ; the CR comment links to it.
    pub superseded_by: Option<String>,
    /// The replacing release's version (the transfer stamps it so the
    /// guard stays honest for the next arrival; the supersede
    /// flow's starter supplies it).
    pub to_version: Option<String>,
}

/// A CR compact enough for the cleanup's argument.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CrSummary {
    /// CR number.
    pub number: u64,
    /// Target repo.
    pub repo: CoreRepoRef,
    /// Head SHA the interpreter had committed.
    pub head_sha: String,
}

/// The cleanup's single durable argument.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CleanupArgs {
    /// The release being cleaned.
    pub release_id: String,
    /// The environments' work.
    pub targets: Vec<CleanupTarget>,
}

/// The cleanup's services (the providers + lease rows).
pub struct CleanupServices {
    /// Providers resolved by `RepoRef.provider` .
    pub providers: Arc<ProviderRegistry>,
    /// Lease rows .
    pub leases: Arc<LeaseRepository>,
}

impl CleanupServices {
    /// Builds the CR reference a provider call needs (the row's form).
    pub fn env_of(&self, target: &CleanupTarget) -> Option<(CoreRepoRef, u64)> {
        target
            .change_request
            .as_ref()
            .map(|summary| (summary.repo.clone(), summary.number))
    }
}

/// Registers the cleanup BEFORE launch .
pub fn register_cleanup(
    instance: &dbos::DBOS,
    services: Arc<CleanupServices>,
) -> dbos::Result<dbos::WorkflowRef<CleanupArgs, (), InterpreterError>> {
    instance.register_workflow(CLEANUP_WORKFLOW, move |args: CleanupArgs| {
        let services = Arc::clone(&services);
        async move { run(args, services).await }
    })
}

/// The body: per target — close the CR (the linkedin comment under supersee),
/// delete the branch, settle the lease. Failures log and continue (the
/// cleanup is best-effort compensation; the release record is already
/// terminal either way).
async fn run(
    args: CleanupArgs,
    services: Arc<CleanupServices>,
) -> dbos::Result<(), InterpreterError> {
    for target in &args.targets {
        clean_target(&args.release_id, target, &services).await;
    }
    dbos::Result::Ok(())
}

/// One environment's compensation, idempotent . Each provider
/// call and the lease settle run inside their own durable step
/// (the deviation the audit called out): the checkpoint makes a
/// crash mid-cleanup resume at the same compensation instead of
/// re-running provider work, and the bounded retry policy turns
/// transient provider wobble into three attempts instead of one.
async fn clean_target(release_id: &str, target: &CleanupTarget, services: &CleanupServices) {
    // 1: close the CR. An already-closed CR is converged, so the
    // provider's "not found" answer is success here.
    if let Some((repo, number)) = services.env_of(target) {
        let comment = target
            .superseded_by
            .as_ref()
            .map(|superseded_by| {
                format!(
                    "Superseded by release {superseded_by} for this environment; closing this change request."
                )
            })
            .unwrap_or_else(|| "Release cancelled; closing this change request.".to_owned());
        if let Ok(provider) = services.providers.resolve(&repo) {
            let outcome = dbos::step_with::<bool, InterpreterError, _, _>(
                format!("cleanup/{}/close-cr", target.environment).as_str(),
                cleanup_options(),
                move || {
                    let repo = repo.clone();
                    let provider = Arc::clone(&provider);
                    let comment = comment.clone();
                    async move {
                        match Provider::close_change_request(&*provider, &repo, number, &comment)
                            .await
                        {
                            Ok(()) => dbos::Result::Ok(true),
                            // The CR is gone: the compensation converged
                            // before this attempt (a replay, a human).
                            Err(cargobike_core::provider::ProviderError::NotFound(_)) => {
                                dbos::Result::Ok(false)
                            }
                            Err(failure) => dbos::Result::Err(dbos::Error::Application(
                                provider_step_failure(failure),
                            )),
                        }
                    }
                },
            )
            .await;
            log_step_outcome(
                release_id,
                &target.environment,
                "close-change-request",
                outcome,
            );
        }
    }
    // 2: delete the release branch (the when cut); a missing branch is
    // converged.
    if let Some(branch) = &target.branch {
        if let Some(repo) = target.change_request.as_ref().map(|summary| &summary.repo) {
            if let Ok(provider) = services.providers.resolve(repo) {
                let outcome = dbos::step_with::<bool, InterpreterError, _, _>(
                    format!("cleanup/{}/delete-branch", target.environment).as_str(),
                    cleanup_options(),
                    move || {
                        let repo = repo.clone();
                        let provider = Arc::clone(&provider);
                        let branch = branch.clone();
                        async move {
                            match Provider::delete_branch(&*provider, &repo, &branch).await {
                                Ok(()) => dbos::Result::Ok(true),
                                // The branch is gone already: converged.
                                Err(cargobike_core::provider::ProviderError::NotFound(_)) => {
                                    dbos::Result::Ok(false)
                                }
                                Err(failure) => dbos::Result::Err(dbos::Error::Application(
                                    provider_step_failure(failure),
                                )),
                            }
                        }
                    },
                )
                .await;
                log_step_outcome(release_id, &target.environment, "delete-branch", outcome);
            }
        }
    }
    // 3: settle the lease — the write rides a durable step too, so a
    // crash between the CR close and the lease settle resumes at the
    // settle instead of re-running the provider work.
    let lease_outcome = match &target.superseded_by {
        Some(superseded_by) => {
            // the atomic hand-off to the replacing release.
            match uuid::Uuid::try_parse(superseded_by) {
                Ok(new_holder) => match uuid::Uuid::try_parse(release_id) {
                    Ok(old_holder) => {
                        let settled = dbos::step_with::<LeaseTransfer, InterpreterError, _, _>(
                            format!("cleanup/{}/transfer-lease", target.environment).as_str(),
                            cleanup_options(),
                            move || {
                                let leases = Arc::clone(&services.leases);
                                let application = target.application.clone();
                                let environment = target.environment.clone();
                                let to_version = target.to_version.clone();
                                async move {
                                    match leases
                                        .transfer(
                                            &application,
                                            &environment,
                                            old_holder,
                                            new_holder,
                                            to_version.as_deref(),
                                        )
                                        .await
                                    {
                                        Ok(transfer) => dbos::Result::Ok(transfer),
                                        Err(sqlx_failure) => dbos::Result::Err(
                                            dbos::Error::Application(InterpreterError::Step(
                                                cargobike_core::step::StepError::Transient {
                                                    reason: sqlx_failure.to_string(),
                                                },
                                            )),
                                        ),
                                    }
                                }
                            },
                        )
                        .await;
                        match settled {
                            Ok(LeaseTransfer::Transferred) => Ok("transferred".to_owned()),
                            Ok(LeaseTransfer::RestState { holder: now_holder }) => {
                                Ok(format!("stale (holder now {now_holder:?})"))
                            }
                            Err(failure) => Err(failure.to_string()),
                        }
                    }
                    Err(_) => Ok(format!("the release id `{release_id}` never parses")),
                },
                Err(_) => Ok(format!("the supersede id `{superseded_by}` never parses")),
            }
        }
        None => match uuid::Uuid::try_parse(release_id) {
            Ok(holder) => {
                let settled = dbos::step_with::<bool, InterpreterError, _, _>(
                    format!("cleanup/{}/release-lease", target.environment).as_str(),
                    cleanup_options(),
                    move || {
                        let leases = Arc::clone(&services.leases);
                        let application = target.application.clone();
                        let environment = target.environment.clone();
                        async move {
                            match leases.release(&application, &environment, holder).await {
                                Ok(released) => dbos::Result::Ok(released),
                                Err(sqlx_failure) => dbos::Result::Err(dbos::Error::Application(
                                    InterpreterError::Step(
                                        cargobike_core::step::StepError::Transient {
                                            reason: sqlx_failure.to_string(),
                                        },
                                    ),
                                )),
                            }
                        }
                    },
                )
                .await;
                match settled {
                    Ok(true) => Ok("released".to_owned()),
                    Ok(false) => Ok("was not held".to_owned()),
                    Err(failure) => Err(failure.to_string()),
                }
            }
            Err(_) => Ok(format!("the release id `{release_id}` never parses")),
        },
    };
    match lease_outcome {
        Ok(detail) => tracing::info!(
            release = %release_id,
            environment = %target.environment,
            "cleanup lease: {detail}"
        ),
        Err(error) => tracing::warn!(%error, release = %release_id, "cleanup lease failed"),
    }
}

/// A displayed line per compensation's durable step outcome.
fn log_step_outcome(
    release_id: &str,
    environment: &str,
    action: &str,
    outcome: dbos::Result<bool, InterpreterError>,
) {
    match outcome {
        Ok(true) => tracing::info!(
            release = %release_id,
            environment = %environment,
            "cleanup {action}: ok"
        ),
        Ok(false) => tracing::info!(
            release = %release_id,
            environment = %environment,
            "cleanup {action}: already settled (converged)"
        ),
        Err(dbos::Error::Application(failure)) => tracing::warn!(
            release = %release_id,
            environment = %environment,
            "cleanup {action}: {failure}"
        ),
        Err(other) => tracing::warn!(
            release = %release_id,
            environment = %environment,
            "cleanup {action}: {other}"
        ),
    }
}
