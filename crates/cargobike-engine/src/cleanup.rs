//! The cleanup workflow: a separate durable
//! workflow started from the cancel and supersede paths — the cancelled
//! interpreter cannot run its own compensation.
//!
//! Work per environment: close the CR (the comment links to the replacing CR
//! under supersede), delete the release branch, then settle the lease —
//! release, or transfer to the new holder in one statement .
//! Every action idempotent ; a replayed cleanup converges.

use std::sync::Arc;

use cargobike_core::model::RepoRef as CoreRepoRef;
use cargobike_core::provider::Provider;
use cargobike_core::registry::ProviderRegistry;

use crate::leases::{LeaseRepository, LeaseTransfer};
use crate::signals::InterpreterError;

/// The registered cleanup workflow name .
pub const CLEANUP_WORKFLOW: &str = "cargobike.cleanup.v1";

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

/// One environment's compensation, idempotent .
async fn clean_target(release_id: &str, target: &CleanupTarget, services: &CleanupServices) {
    // 1: close the CR.
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
            let outcome = Provider::close_change_request(&*provider, &repo, number, &comment).await;
            log_outcome(
                release_id,
                &target.environment,
                "close-change-request",
                outcome,
            );
        }
    }
    // 2: delete the release branch (the when cut).
    if let Some(branch) = &target.branch {
        if let Some(repo) = target.change_request.as_ref().map(|summary| &summary.repo) {
            if let Ok(provider) = services.providers.resolve(repo) {
                // The provider's contract covers branch deletes via the
                // CR close + branch cleanup; the branch delete is the repo
                // content operation the provider exposes .
                let outcome = Provider::delete_branch(&*provider, repo, branch).await;
                log_outcome(release_id, &target.environment, "delete-branch", outcome);
            }
        }
    }
    // 3: settle the lease.
    let lease_outcome = match &target.superseded_by {
        Some(superseded_by) => {
            // : the atomic hand-off to the replacing release.
            match uuid::Uuid::try_parse(superseded_by) {
                Ok(new_holder) => match uuid::Uuid::try_parse(release_id) {
                    Ok(old_holder) => services
                        .leases
                        .transfer(
                            &target.application,
                            &target.environment,
                            old_holder,
                            new_holder,
                            target.to_version.as_deref(),
                        )
                        .await
                        .map(|transfer| match transfer {
                            LeaseTransfer::Transferred => "transferred".to_owned(),
                            LeaseTransfer::RestState { holder: now_holder } => {
                                format!("stale (holder now {now_holder:?})")
                            }
                        }),
                    Err(_) => Ok(format!("the release id `{release_id}` never parses")),
                },
                Err(_) => Ok(format!("the supersede id `{superseded_by}` never parses")),
            }
        }
        None => match uuid::Uuid::try_parse(release_id) {
            Ok(holder) => services
                .leases
                .release(&target.application, &target.environment, holder)
                .await
                .map(|released| {
                    if released {
                        "released".to_owned()
                    } else {
                        "was not held".to_owned()
                    }
                }),
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

/// A displayed line per action.
fn log_outcome(release_id: &str, environment: &str, action: &str, outcome: Result<(), _Failure>) {
    match outcome {
        Ok(()) => {
            tracing::info!(release = %release_id, environment = %environment, "cleanup {action}: ok")
        }
        Err(failure) => {
            tracing::warn!(release = %release_id, environment = %environment, "cleanup {action}: {failure}")
        }
    }
}

type _Failure = cargobike_core::provider::ProviderError;
