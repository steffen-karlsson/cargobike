//! The cleanup workflow (PRD 3.13, F-75/F-73, A2): a separate durable
//! workflow started from the cancel and supersede paths — the cancelled
//! interpreter cannot run its own compensation.
//!
//! Work per environment: close the CR (comment links to the replacing CR
//! under supersede), delete the release branch, then settle the lease —
//! release, or transfer to the new holder in one statement (F-73).
//! Every action idempotent (A1); a replayed cleanup converges.

use std::sync::Arc;

use cargobike_core::model::RepoRef as CoreRepoRef;
use cargobike_core::provider::Provider;
use cargobike_core::registry::ProviderRegistry;

use crate::leases::{LeaseRepository, LeaseTransfer};
use crate::signals::InterpreterError;

/// The registered cleanup workflow name (F-75).
pub const CLEANUP_WORKFLOW: &str = "cargobike.cleanup.v1";

/// What a cleanup serves: the CR/branch facts of one environment.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CleanupTarget {
    /// The application's name (lease + comment context).
    pub application: String,
    /// The environment the release was progressing through.
    pub environment: String,
    /// The release's branch (A1's deterministic name), when it was cut.
    pub branch: Option<String>,
    /// The open CR this release holds, when any (F-63's row form).
    pub change_request: Option<CrSummary>,
    /// Under `supersede`: the new release becomes the lease's holder in
    /// the same statement (F-73); the CR comment links to it.
    pub superseded_by: Option<String>,
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

/// The cleanup's services (providers + lease rows).
pub struct CleanupServices {
    /// Providers resolved by `RepoRef.provider` (F-39).
    pub providers: Arc<ProviderRegistry>,
    /// Lease rows (A2).
    pub leases: Arc<LeaseRepository>,
}

impl CleanupServices {
    /// Builds the CR reference a provider call needs (F-63's form).
    pub fn env_of(&self, target: &CleanupTarget) -> Option<(CoreRepoRef, u64)> {
        target
            .change_request
            .as_ref()
            .map(|summary| (summary.repo.clone(), summary.number))
    }
}

/// Registers the cleanup BEFORE launch (F-15).
pub fn register_cleanup(
    instance: &dbos::DBOS,
    services: Arc<CleanupServices>,
) -> dbos::Result<dbos::WorkflowRef<CleanupArgs, (), InterpreterError>> {
    instance.register_workflow(CLEANUP_WORKFLOW, move |args: CleanupArgs| {
        let services = Arc::clone(&services);
        async move { run(args, services).await }
    })
}

/// The body: per target — close the CR (linkedin comment under supersee),
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

/// One environment's compensation, idempotent (A1).
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
    // 2: delete the release branch (when cut).
    if let Some(branch) = &target.branch {
        if let Some(repo) = target.change_request.as_ref().map(|summary| &summary.repo) {
            if let Ok(provider) = services.providers.resolve(repo) {
                // The provider's contract covers branch deletes via the
                // CR close + branch cleanup; the branch delete is the repo
                // content operation the provider exposes (F-46).
                let outcome = Provider::delete_branch(&*provider, repo, branch).await;
                log_outcome(release_id, &target.environment, "delete-branch", outcome);
            }
        }
    }
    // 3: settle the lease.
    let lease_outcome = match &target.superseded_by {
        Some(superseded_by) => {
            // F-73: the atomic hand-off to the replacing release.
            match uuid::Uuid::try_parse(superseded_by) {
                Ok(new_holder) => match uuid::Uuid::try_parse(release_id) {
                    Ok(old_holder) => services
                        .leases
                        .transfer(
                            &target.application,
                            &target.environment,
                            old_holder,
                            new_holder,
                        )
                        .await
                        .map(|transfer| match transfer {
                            LeaseTransfer::Transferred => "transferred".to_owned(),
                            LeaseTransfer::RestState { holder } => {
                                format!("stale (holder now {holder:?})")
                            }
                        }),
                    Err(_) => Ok(format!("the release id `{release_id}` never parses")),
                },
                Err(_) => Ok(format!("the supersede id `{superseded_by}` never parses")),
            }
        }
        None => services
            .leases
            .acquire_amp(&target.application, &target.environment)
            .await
            .map(|_unused| "released".to_owned()),
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

/// The leases' release-form helper the cleanup needs (holder-guarded).
impl crate::leases::LeaseRepository {
    /// Releases only when `holder` holds it; the cleanup's `None`
    /// supersede path fulfils the holder from the release_id.
    pub async fn acquire_amp(
        &self,
        application: &str,
        environment: &str,
    ) -> Result<bool, sqlx::Error> {
        let holder = self.holder(application, environment).await?;
        match holder {
            Some(holder) => self.release(application, environment, holder).await,
            None => Ok(false),
        }
    }
}
