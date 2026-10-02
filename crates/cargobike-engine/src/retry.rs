//! The retry workflow (F-23, US-7): a failed release's fork.
//!
//! The flow: the release is `Failed`; its attempt's workflow is forked
//! with `ForkFrom::LastFailure`, so the fork inherits every recorded
//! step below the failure and re-runs from there — earlier environments
//! are not re-deployed. The release keeps its id; this workflow appends
//! the attempt record with `fork_from` set to the source attempt.
//!
//! The deterministic workflow id (`cargobike/retry/<release_id>/<source
//! attempt id>` — the source names itself in the id; a real unique per
//! source attempt) makes a retry of the SAME failed attempt an
//! acknowledged no-op: the DBOS start dedupes into the recorded run.
//! Once the retried run fails again, the doc's last attempt is a
//! different workflow id, and a new retry forks again.

use std::sync::Arc;

use cargobike_core::error::STEP_FAILED;
use uuid::Uuid;

use crate::signals::InterpreterError;
use crate::status::ReleaseStatusStore;

/// The registered retry workflow name.
pub const RETRY_WORKFLOW: &str = "cargobike.retry.v1";

/// The retry workflow's single durable argument.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RetryArgs {
    /// The failed release (its id; the fork carries the SAME release).
    pub release_id: String,
}

/// The retry workflow's services (the store read + the instance's fork).
pub struct RetryServices {
    /// The release's status store (the doc read + the attempt append).
    pub statuses: Arc<dyn ReleaseStatusStore>,
    /// The DBOS instance: the fork rides it (the call is a checkpointed
    /// step inside this workflow, so a replay never writes a second
    /// fork).
    pub instance: dbos::DBOS,
}

/// Registers the retry workflow BEFORE launch (the registry snapshot).
pub fn register_retry(
    instance: &dbos::DBOS,
    services: Arc<RetryServices>,
) -> dbos::Result<dbos::WorkflowRef<RetryArgs, String, InterpreterError>> {
    instance.register_workflow(RETRY_WORKFLOW, move |args: RetryArgs| {
        let services = Arc::clone(&services);
        async move { fork_failure(args, services).await }
    })
}

/// The workflow-id convention for a retry (one failed attempt at a
/// time; a retry of a later attempt addresses a different source).
pub fn retry_workflow_id(release_id: &str, source_workflow_id: &str) -> String {
    format!("cargobike/retry/{release_id}/{source_workflow_id}")
}

/// One fork: read the failed release, fork its attempt from the last
/// failure, append the attempt record.
async fn fork_failure(
    args: RetryArgs,
    services: Arc<RetryServices>,
) -> dbos::Result<String, InterpreterError> {
    let release_id = Uuid::parse_str(&args.release_id)
        .map_err(|_| dbos::Error::Application(refused("the release id is not a UUID")))?;
    let document = services
        .statuses
        .load_document(release_id)
        .await
        .map_err(|failure| {
            dbos::Error::Application(InterpreterError::Step(
                cargobike_core::step::StepError::Failed {
                    code: STEP_FAILED.to_owned(),
                    message: format!("the release's document could not be read: {failure}"),
                },
            ))
        })?;
    let phase = document
        .pointer("/status/phase")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if phase != "Failed" {
        return Err(dbos::Error::Application(refused(&format!(
            "only a failed release can fork in place (this release is `{phase}`)"
        ))));
    }
    // The source attempt: the doc's last entry (a failed release's
    // attempts' tail names the workflow that failed).
    let source = document
        .pointer("/status/attempts")
        .and_then(serde_json::Value::as_array)
        .and_then(|attempts| attempts.last())
        .and_then(|attempt| attempt.get("workflow_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            dbos::Error::Application(refused("the release carries no attempt to fork"))
        })?;

    // The row re-opens first: the fork's own bookkeeping must be able
    // to write again (the release's phase follows the attempt). The
    // reopen ride a durable step; a replay reads its no-op.
    let reopened = dbos::step_with::<Result<bool, ()>, InterpreterError, _, _>(
        "status/reopen",
        dbos::StepOptions::default(),
        {
            let store = Arc::clone(&services.statuses);
            move || {
                let store = Arc::clone(&store);
                async move {
                    match store.release_reopened(release_id).await {
                        Ok(()) => dbos::Result::Ok(Ok(true)),
                        Err(failure) => {
                            // A refused reopen (a gone row): the
                            // re-delivery's problem later; no fork
                            // meanwhile.
                            tracing::warn!(release = %release_id, %failure, "the retry's reopen refused");
                            dbos::Result::Ok(Ok(false))
                        }
                    }
                }
            }
        },
    )
    .await;
    match reopened {
        Ok(Ok(_)) => {}
        Ok(Err(_)) | Err(_) => {
            return Err(dbos::Error::Application(refused(
                "the retry's reopen failed; try again",
            )));
        }
    }

    // The fork: inherit everything recorded below the failing step and
    // re-run from there. Inside this workflow it is a checkpointed
    // step, so a replay hands back the id the first execution formed.
    let source_of_fork = source.clone();
    let handle = services
        .instance
        .fork_with::<crate::InterpretResult, InterpreterError>(
            source_of_fork.as_str(),
            dbos::ForkFrom::LastFailure,
            dbos::ForkOptions::default(),
        )
        .await
        .map_err(|failure| {
            // The fork's management step carries the engine-only
            // envelope; the driver's text stays transient for the step
            // retry.
            InterpreterError::Step(cargobike_core::step::StepError::Transient {
                reason: failure.to_string(),
            })
        })?;
    let fork_id = handle.workflow_id().to_owned();

    // The attempt record (its own durable step; `fork_from` names the
    // attempt this run inherits).
    let fork_from = source;
    let fork_id_for_record = fork_id.clone();
    crate::interpreter::status_write(
        "status/attempt-forked".to_owned(),
        release_id,
        &services.statuses,
        move |store, release_id| {
            let fork_id = fork_id_for_record.clone();
            let fork_from = fork_from.clone();
            async move {
                store
                    .attempt_started(
                        release_id,
                        &fork_id,
                        time::OffsetDateTime::now_utc(),
                        Some(&fork_from),
                    )
                    .await
            }
        },
    )
    .await?;
    Ok(fork_id)
}

/// The retry's refusal as the workflow-level failure (the API maps it).
fn refused(message: &str) -> InterpreterError {
    InterpreterError::Step(cargobike_core::step::StepError::Failed {
        code: STEP_FAILED.to_owned(),
        message: message.to_owned(),
    })
}
