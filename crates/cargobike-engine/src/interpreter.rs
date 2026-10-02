//! The durable interpreter: one registered
//! DBOS workflow per instance (`cargobike.interpret.v1`), the control
//! steps (the wait rules), and the merge verification .
//!
//! The body is a pure function of the snapshot and the recorded outputs
//! . Wait steps are interpreter-native: `recv` over the signal
//! topics and a provider re-verification for merges. One
//! argument only — the snapshot (the spike's confirmed shape); the
//! closure must not capture the instance (the spike doc).

use std::sync::Arc;
use std::time::Duration;

use cargobike_core::error::ReleaseError;
use cargobike_core::model::{ChangeRequestRef, CrState, EnvironmentPhase};
use cargobike_core::provider::Provider;
use cargobike_core::step::{HttpService, StepContext, StepOutput};
use cargobike_core::template::{OnModified, OnTimeout};
use tracing::Span;

use crate::concurrency::LeaseDecision;
use crate::correlation::CorrelationRow;
use crate::expr::{ExprContext, eval_gate, interpolate_params};
use crate::leases::LeaseRepository;
use crate::status::{ReleaseStatusStore, StatusError};

use crate::signals::{InterpreterError, Signal, approval_topic, merge_topic};
use crate::snapshot::ReleaseSnapshot;
use crate::steps::StepRegistry;
use crate::template::{ResolvedEnvironment, ResolvedStep, StepBody};

/// The registered interpreter workflow name .
pub const INTERPRETER_WORKFLOW: &str = "cargobike.interpret.v2";

/// The PREVIOUS interpreter's name (registered alongside until the
/// in-flight rows drain: a v1 workflow under recovery replays through
/// its own name; its pre-change step rows may refuse the new decode
/// and surface as a step failure, never a silent re-run).
pub const INTERPRETER_WORKFLOW_LEGACY: &str = "cargobike.interpret.v1";

/// The interpreter's single durable argument.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InterpretArgs {
    /// The snapshot: template, inputs, step-type versions, hash.
    pub snapshot: ReleaseSnapshot,
}

/// The interpreter's result (the environments' phases).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InterpretResult {
    /// Per-environment outcomes in declaration order.
    pub environments: Vec<EnvironmentOutcome>,
}

/// One environment's outcome (the rollup input).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnvironmentOutcome {
    /// The environment's name.
    pub name: String,
    /// The environment's terminal phase.
    pub phase: EnvironmentPhase,
    /// Error detail when failed .
    pub error: Option<ReleaseError>,
}

/// Everything the interpreter runs against, part of the registered
/// closure (the executor snapshots the registry at launch).
pub struct InterpreterServices {
    /// Step types installed (the registry).
    pub steps: Arc<StepRegistry>,
    /// Providers resolved by `RepoRef.provider` name .
    pub providers: Arc<cargobike_core::registry::ProviderRegistry>,
    /// Named secrets .
    pub credentials: Arc<dyn cargobike_core::registry::CredentialStore>,
    /// The SSRF-guarded client (theirs step paths).
    pub http: Arc<dyn HttpService>,
    /// Lease rows .
    pub leases: Arc<LeaseRepository>,
    /// The release's status writes (the bookkeeping).
    pub statuses: Arc<dyn ReleaseStatusStore>,
    /// The CR-correlation rows the change-request step stamps (the
    /// webhook receiver and the reconciler sweep read them).
    pub correlations: Arc<crate::correlation::CorrelationRepository>,
    /// The DBOS instance (the supersede path's cancel reads it; the
    /// cloned handle is the same connection surface).
    pub instance: dbos::DBOS,
    /// The cleanup workflow's registration, set after both workflows
    /// register (the supersede's starter needs the child handle; a
    /// snapshot-pinned start with a deterministic child id).
    pub cleanup_ref: Arc<
        std::sync::OnceLock<dbos::WorkflowRef<crate::cleanup::CleanupArgs, (), InterpreterError>>,
    >,
}

/// The status store's failure to the interpreter's envelope.
pub(crate) fn status_to_interpreter_error(failure: crate::status::StatusError) -> InterpreterError {
    use crate::status::StatusError as StoreError;
    match failure {
        StoreError::Terminal | StoreError::NotFound(_) => {
            InterpreterError::Step(cargobike_core::step::StepError::Transient {
                reason: failure.to_string(),
            })
        }
        StoreError::Internal(text) => {
            InterpreterError::Step(cargobike_core::step::StepError::Failed {
                code: cargobike_core::error::STEP_FAILED.to_owned(),
                message: text,
            })
        }
    }
}

/// The interpreter's workflow id for a release (the convention the
/// create's start, the cancel and the retry address, and the one the
/// supersede path cancels by).
pub fn interpret_workflow_id(release_id: impl std::fmt::Display) -> String {
    format!("cargobike/interpret/{release_id}")
}

/// The cleanup attempt's workflow id for a release (the supersede's
/// started child: deterministic ids converge; an existing cleanup joins
/// instead of double-running).
pub fn cleanup_workflow_id(release_id: impl std::fmt::Display) -> String {
    format!("cargobike/cleanup/{release_id}")
}

impl InterpreterServices {
    /// A `StepContext` for a concrete step run .
    pub fn step_context(&self, release_id: &str, environment: &str, step_id: &str) -> StepContext {
        StepContext {
            providers: (*self.providers).clone(),
            http: Arc::clone(&self.http),
            credentials: Arc::clone(&self.credentials),
            // `release_id/environment/step_id` is the idempotency input.
            idempotency_key: format!("{release_id}/{environment}/{step_id}"),
            cancel_token: cargobike_core::step::CancelToken::new(),
            log: Span::current(),
        }
    }
}

/// Registers the interpreter; registration only works BEFORE `launch`
/// (; the spike's registry snapshot applies).
pub fn register_interpreter(
    instance: &dbos::DBOS,
    services: Arc<InterpreterServices>,
) -> dbos::Result<dbos::WorkflowRef<InterpretArgs, InterpretResult, InterpreterError>> {
    instance.register_workflow(INTERPRETER_WORKFLOW, {
        let services = Arc::clone(&services);
        move |args: InterpretArgs| {
            let services = Arc::clone(&services);
            async move { run(args, services).await }
        }
    })?;
    // The legacy registration (§6.8's drain window): a v1 workflow
    // under recovery replays through its own name. Its pre-change step
    // rows decode through the same body; anything it decodes fails on
    // is recorded as the step's failure text rather than silently
    // re-run.
    instance.register_workflow(INTERPRETER_WORKFLOW_LEGACY, move |args: InterpretArgs| {
        let services = Arc::clone(&services);
        async move { run(args, services).await }
    })
}

/// The workflow body: environments × steps of the snapshot (
/// pure-function rule). The attempt lands first (the attempt record's
/// bookkeeping), then the environments in declaration order; a failed
/// environment stops the run (the later environments do not start).
async fn run(
    args: InterpretArgs,
    services: Arc<InterpreterServices>,
) -> dbos::Result<InterpretResult, InterpreterError> {
    let release_id = parse_release_id(&args.snapshot)?;
    let attempt = dbos::workflow_id().unwrap_or_else(|| "unknown".to_owned());
    status_write(
        "status/attempt".to_owned(),
        release_id,
        &services.statuses,
        move |store, release_id| {
            let attempt = attempt.clone();
            async move {
                store
                    .attempt_started(release_id, &attempt, time::OffsetDateTime::now_utc(), None)
                    .await
            }
        },
    )
    .await?;
    // Every provisioned environment's record seeds now: the rollup
    // never reads a doc env list smaller than the template's (the
    // retry's docs keep theirs; the fork's replays skip this step).
    let provisioned_names = args
        .snapshot
        .template
        .environments
        .iter()
        .map(|env| env.name.clone())
        .collect::<Vec<_>>();
    status_write(
        "status/envs-seeded".to_owned(),
        release_id,
        &services.statuses,
        move |store, release_id| {
            let provisioned_names = provisioned_names.clone();
            async move {
                store
                    .environments_posted(release_id, &provisioned_names)
                    .await
            }
        },
    )
    .await?;
    let mut outcomes = Vec::with_capacity(args.snapshot.template.environments.len());
    for environment in &args.snapshot.template.environments {
        let outcome = run_environment(&args.snapshot, environment, &services)
            .await
            .unwrap_or_else(|failure| error_to_outcome(environment, interpreter_error_of(failure)));
        let stopped = matches!(
            outcome.phase,
            EnvironmentPhase::Failed | EnvironmentPhase::Canceled | EnvironmentPhase::Superseded
        );
        outcomes.push(outcome);
        if stopped {
            break;
        }
    }
    dbos::Result::Ok(InterpretResult {
        environments: outcomes,
    })
}

/// The release id off the snapshot (a shared parse; the error is the
/// interpreter's step-failed envelope).
fn parse_release_id(snapshot: &ReleaseSnapshot) -> dbos::Result<UuidAlias, InterpreterError> {
    let failure = |message: String| {
        InterpreterError::Step(cargobike_core::step::StepError::Failed {
            code: cargobike_core::error::STEP_FAILED.to_owned(),
            message,
        })
    };
    let parsed = match uuid::Uuid::parse_str(snapshot.release.id.as_str()) {
        Ok(parsed) => parsed,
        Err(parse_error) => {
            return dbos::Result::Err(dbos::Error::Application(failure(format!(
                "failed to parse the snapshot's release id: {parse_error}"
            ))));
        }
    };
    dbos::Result::Ok(parsed)
}

/// The release id's alias (the crate's uuid re-use).
type UuidAlias = uuid::Uuid;

/// The engine's error envelope to the interpreter's own error: an
/// application error is held as itself; anything engine-level becomes
/// a transient step error (a machinery refusal is not the workflow's
/// semantics).
pub(crate) fn interpreter_error_of(failure: dbos::Error<InterpreterError>) -> InterpreterError {
    match failure {
        dbos::Error::Application(failure) => failure,
        other => InterpreterError::Step(cargobike_core::step::StepError::Transient {
            reason: other.to_string(),
        }),
    }
}

/// One status write as its own durable step (`weight: avoid the replay
/// re-writing a recorded fact). The write closure takes its store and
/// release id arguments so re-runs re-evaluate only the same statement.
pub(crate) async fn status_write<W, Fut>(
    label: String,
    release_id: UuidAlias,
    store: &Arc<dyn ReleaseStatusStore>,
    write: W,
) -> dbos::Result<(), InterpreterError>
where
    W: Fn(Arc<dyn ReleaseStatusStore>, UuidAlias) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<(), StatusError>> + Send,
{
    let write = Arc::new(write);
    let outcome = dbos::step_with::<Result<bool, ()>, InterpreterError, _, _>(
        label.as_str(),
        dbos::StepOptions::default(),
        move || {
            let store = Arc::clone(store);
            let write = Arc::clone(&write);
            async move {
                match write(store, release_id).await {
                    Ok(()) => dbos::Result::Ok(Ok(true)),
                    // Terminal refusals and not-found rows are recorded
                    // as no-ops (a replay must not re-decide against a
                    // stale release).
                    Err(StatusError::Terminal | StatusError::NotFound(_)) => {
                        dbos::Result::Ok(Ok(false))
                    }
                    Err(StatusError::Internal(failure)) => {
                        tracing::error!(%failure, "a status write failed");
                        dbos::Result::Ok(Ok(false))
                    }
                }
            }
        },
    )
    .await;
    outcome.map(|_| ())
}

/// One environment: the gate decides a skip; steps run in order. The
/// status writes ride the boundaries (running, waiting, a change
/// request's opening, the terminal phase) — durable steps whose
/// replays re-read the recorded fact rather than re-write it.
async fn run_environment(
    snapshot: &ReleaseSnapshot,
    environment: &ResolvedEnvironment,
    services: &InterpreterServices,
) -> dbos::Result<EnvironmentOutcome, InterpreterError> {
    let release_id = parse_release_id(snapshot)?;
    let mut context = snapshot.context(&environment.name);
    if let Some(when) = &environment.when {
        match eval_gate(when, &context) {
            Ok(false) => {
                // The gate said no: the environment skipped before any
                // side effect, and no lease belonged to it.
                status_write(
                    format!("status/{}/skipped", environment.name),
                    release_id,
                    &services.statuses,
                    |store, release_id| {
                        let environment = environment.name.clone();
                        async move {
                            store
                                .environment_terminal(
                                    release_id,
                                    &environment,
                                    EnvironmentPhase::Skipped,
                                    None,
                                    time::OffsetDateTime::now_utc(),
                                )
                                .await
                        }
                    },
                )
                .await?;
                return dbos::Result::Ok(finished(environment, EnvironmentPhase::Skipped, None));
            }
            Err(error) => {
                return dbos::Result::Ok(error_to_outcome(environment, expr_failure(error)));
            }
            Ok(true) => {}
        }
    }
    // the lease gates the environment's work.
    match crate::concurrency::enter(services, snapshot, environment).await {
        Ok(LeaseDecision::Proceed) => {}
        Ok(LeaseDecision::WaitForLease) => {
            status_write(
                format!("status/{}/waiting", environment.name),
                release_id,
                &services.statuses,
                |store, release_id| {
                    let environment = environment.name.clone();
                    async move { store.environment_waiting(release_id, &environment).await }
                },
            )
            .await?;
            // queue: the lease's release signal wakes it.
            loop {
                let taken = dbos::recv::<Signal, InterpreterError>(
                    Some(crate::signals::lease_topic(&environment.name).as_str()),
                    crate::concurrency::QUEUE_WAKE_TIMEOUT,
                )
                .await;
                let Some(Signal::LeaseReleased { .. }) = taken.ok().flatten() else {
                    continue;
                };
                match crate::concurrency::enter(services, snapshot, environment).await {
                    Ok(LeaseDecision::Proceed) => break,
                    Ok(LeaseDecision::WaitForLease) => continue,
                    Err(error) => return dbos::Result::Ok(error_to_outcome(environment, error)),
                }
            }
        }
        Err(concurrency_error) => {
            return dbos::Result::Ok(error_to_outcome(environment, concurrency_error));
        }
    }
    status_write(
        format!("status/{}/running", environment.name),
        release_id,
        &services.statuses,
        |store, release_id| {
            let environment = environment.name.clone();
            async move {
                store
                    .environment_running(release_id, &environment, time::OffsetDateTime::now_utc())
                    .await
            }
        },
    )
    .await?;
    for (index, step) in environment.steps.iter().enumerate() {
        match run_step(snapshot, environment, index, step, &mut context, services).await {
            StepFlow::Continue => {}
            StepFlow::Skip => {
                // the lease releases on skip, waking the queue.
                if let Err(release_failure) =
                    crate::concurrency::release_lease(services, snapshot, environment).await
                {
                    tracing::warn!(
                        %release_failure,
                        release = %snapshot.release.id,
                        environment = %environment.name,
                        "queue release failed on skip"
                    );
                }
                status_write(
                    format!("status/{}/skipped", environment.name),
                    release_id,
                    &services.statuses,
                    |store, release_id| {
                        let environment = environment.name.clone();
                        async move {
                            store
                                .environment_terminal(
                                    release_id,
                                    &environment,
                                    EnvironmentPhase::Skipped,
                                    None,
                                    time::OffsetDateTime::now_utc(),
                                )
                                .await
                        }
                    },
                )
                .await?;
                return dbos::Result::Ok(finished(environment, EnvironmentPhase::Skipped, None));
            }
            StepFlow::Error(failure) => {
                // a failed environment KEEPS the lease for the fork
                // resume; the supersede chain releases it server-side.
                let terminal_phase = environment_phase_of(&failure);
                let release_error = ReleaseError::new(failure.code(), failure.to_string());
                status_write(
                    format!("status/{}/terminal", environment.name),
                    release_id,
                    &services.statuses,
                    |store, release_id| {
                        let environment = environment.name.clone();
                        let release_error = release_error.clone();
                        async move {
                            store
                                .environment_terminal(
                                    release_id,
                                    &environment,
                                    terminal_phase,
                                    Some(release_error),
                                    time::OffsetDateTime::now_utc(),
                                )
                                .await
                        }
                    },
                )
                .await?;
                return dbos::Result::Ok(error_to_outcome(environment, failure));
            }
        }
    }
    // the lease releases on complete, waking the queue.
    if let Err(release_failure) =
        crate::concurrency::release_lease(services, snapshot, environment).await
    {
        tracing::warn!(
            %release_failure,
            release = %snapshot.release.id,
            environment = %environment.name,
            "queue release failed on complete"
        );
    }
    status_write(
        format!("status/{}/completed", environment.name),
        release_id,
        &services.statuses,
        |store, release_id| {
            let environment = environment.name.clone();
            async move {
                store
                    .environment_terminal(
                        release_id,
                        &environment,
                        EnvironmentPhase::Completed,
                        None,
                        time::OffsetDateTime::now_utc(),
                    )
                    .await
            }
        },
    )
    .await?;
    dbos::Result::Ok(finished(environment, EnvironmentPhase::Completed, None))
}

/// The release phase each failure maps to.
pub const fn environment_phase_of(failure: &InterpreterError) -> EnvironmentPhase {
    match failure {
        InterpreterError::Cancelled => EnvironmentPhase::Canceled,
        _ => EnvironmentPhase::Failed,
    }
}

/// An interpreter failure becomes the environment's outcome.
fn error_to_outcome(
    environment: &ResolvedEnvironment,
    failure: InterpreterError,
) -> EnvironmentOutcome {
    EnvironmentOutcome {
        name: environment.name.clone(),
        phase: environment_phase_of(&failure),
        error: Some(ReleaseError::new(failure.code(), failure.to_string())),
    }
}

fn finished(
    environment: &ResolvedEnvironment,
    phase: EnvironmentPhase,
    error: Option<ReleaseError>,
) -> EnvironmentOutcome {
    EnvironmentOutcome {
        name: environment.name.clone(),
        phase,
        error,
    }
}

/// Maps an expression failure to the interpreter's envelope (
/// `StepFailed` family: the step never evaluated).
fn expr_failure(error: crate::expr::ExprError) -> InterpreterError {
    InterpreterError::Step(cargobike_core::step::StepError::Failed {
        code: cargobike_core::error::STEP_FAILED.to_owned(),
        message: error.to_string(),
    })
}

/// What one step told the environment.
enum StepFlow {
    /// Keep going.
    Continue,
    /// The environment skips (the skip path).
    Skip,
    /// A failure the environment takes (the error).
    Error(InterpreterError),
}

async fn run_step(
    snapshot: &ReleaseSnapshot,
    environment: &ResolvedEnvironment,
    index: usize,
    step: &ResolvedStep,
    context: &mut ExprContext,
    services: &InterpreterServices,
) -> StepFlow {
    let step_id: String = step.id.clone();
    match &step.body {
        StepBody::WaitMerge {
            timeout,
            on_timeout,
            on_modified,
        } => {
            wait_merge(
                snapshot,
                environment,
                step_id.as_str(),
                context,
                services,
                *timeout,
                *on_timeout,
                *on_modified,
            )
            .await
        }
        StepBody::WaitApproval {
            timeout,
            on_timeout,
            ..
        } => {
            // The wait's entry is the environment's PendingApproval (the
            // approval endpoint's 409 rule keys on this state, and the
            // reconciler's sweep finds the release through it).
            if let Ok(release_id) = uuid::Uuid::parse_str(&snapshot.release.id) {
                if let Err(engine) = status_write(
                    format!("status/{}/pending-approval", environment.name),
                    release_id,
                    &services.statuses,
                    |store, release_id| {
                        let environment = environment.name.clone();
                        async move {
                            store
                                .environment_pending_approval(release_id, &environment)
                                .await
                        }
                    },
                )
                .await
                {
                    return StepFlow::Error(interpreter_error_of(engine));
                }
            }
            wait_approval(environment, step_id.as_str(), *timeout, *on_timeout).await
        }
        StepBody::WaitSleep { duration } => {
            let _slept = dbos::sleep::<InterpreterError>(*duration).await;
            StepFlow::Continue
        }
        StepBody::Action { .. } => {
            let flow = dispatch_action(snapshot, environment, step, context, services).await;
            if let StepFlow::Continue = &flow {
                if uses_change_request(step) {
                    if let Err(engine) = after_change_request_continue(
                        snapshot,
                        environment,
                        index,
                        context,
                        services,
                    )
                    .await
                    {
                        return StepFlow::Error(interpreter_error_of(engine));
                    }
                }
            }
            flow
        }
    }
}

/// The verify decision, computed inside a durable step (a replay
/// re-reads the recorded decision without re-querying the provider, so
/// the wait's loop branch sequence stays replay-deterministic).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
enum MergeVerify {
    /// CR merged, head SHA unchanged, content verified.
    Verified,
    /// The provider still reports the CR open (a raced signal).
    NotYet,
    /// The CR reached the closed state without merging.
    CrClosed,
    /// The CR's head SHA moved (a fix-up commit or an update_branch).
    Modified,
}

/// The wall clock in epoch millis (the wait's anchor input; the value
/// itself is durably recorded on first evaluation).
fn now_unix_millis() -> u64 {
    use std::time::UNIX_EPOCH;
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or_default()
}

/// `wait: merge`'s enclosure : durable anchor -> recv -> recorded
/// provider re-verify; open CRs keep waiting under the recorded
/// deadline; closed-without-merge is terminal; `on_modified` decides
/// the head-SHA move.
#[allow(clippy::too_many_arguments)]
async fn wait_merge(
    snapshot: &ReleaseSnapshot,
    environment: &ResolvedEnvironment,
    step_id: &str,
    context: &crate::expr::ExprContext,
    services: &InterpreterServices,
    timeout: Duration,
    on_timeout: OnTimeout,
    on_modified: OnModified,
) -> StepFlow {
    let topic = merge_topic(&environment.name, step_id);
    let anchor_span = format!("merge/anchor/{step_id}");
    let anchor = match dbos::step_with::<Result<u64, ()>, InterpreterError, _, _>(
        anchor_span.as_str(),
        dbos::StepOptions::default(),
        || async { dbos::Result::Ok(Ok(now_unix_millis())) },
    )
    .await
    {
        // The recorded anchor (the workflow's original start).
        Ok(Ok(anchor)) => anchor,
        Ok(Err(_)) => {
            return StepFlow::Error(InterpreterError::Step(
                cargobike_core::step::StepError::Transient {
                    reason: "the anchor step failed".to_owned(),
                },
            ));
        }
        Err(engine_failure) => {
            return StepFlow::Error(InterpreterError::Step(
                cargobike_core::step::StepError::Transient {
                    reason: engine_failure.to_string(),
                },
            ));
        }
    };
    let total = timeout.as_millis() as u64;
    // The deadline's verdict, wanted per iteration as the wall clock
    // moves.
    loop {
        let elapsed = now_unix_millis().saturating_sub(anchor);
        if elapsed >= total {
            return match on_timeout {
                OnTimeout::Fail => StepFlow::Error(InterpreterError::MergeTimeout),
                OnTimeout::Cancel => StepFlow::Error(InterpreterError::Cancelled),
            };
        }
        let taken = dbos::recv::<Signal, InterpreterError>(
            Some(topic.as_str()),
            Duration::from_millis(total - elapsed),
        )
        .await;
        let received = match taken {
            Ok(received) => received,
            // An engine-level failure is transient: the next retry
            // redoes the wait with the same recorded facts .
            Err(_) => {
                return StepFlow::Error(InterpreterError::Step(
                    cargobike_core::step::StepError::Transient {
                        reason: "the durable receive failed".to_owned(),
                    },
                ));
            }
        };
        let Some(Signal::MergeComplete {
            merged,
            number,
            head_sha,
            ..
        }) = received
        else {
            return match received {
                Some(Signal::MergeComplete { merged: false, .. }) => {
                    StepFlow::Error(InterpreterError::ApprovalRejected)
                }
                _ => wrong_signal(),
            };
        };
        if !merged {
            // Closed without merge is terminal (`ApprovalRejected`).
            return StepFlow::Error(InterpreterError::ApprovalRejected);
        }

        // The CR's head SHA as the change-request step recorded it (the
        // replay rebuilds this context identically: the dispatch of the
        // CR step precedes the wait).
        let cr_step_head = environment
            .steps
            .iter()
            .find(|step| uses_change_request(step))
            .and_then(|step| context_peek(context, step.id.as_str()));

        // The verify runs as its own durable step: replay re-reads the
        // recorded decision (the no re-query), so the loop's branch shapes
        // stay deterministic.
        let verify_name = format!("merge/verify/{step_id}");
        let providers = services.providers.clone();
        let repo = snapshot.repo_of(&environment.name);
        let edits = snapshot.edits_of(&environment.name);
        let version = snapshot.release.version.clone();
        let outcome = dbos::step_with::<Result<MergeVerify, ()>, InterpreterError, _, _>(
            verify_name.as_str(),
            dbos::StepOptions::default(),
            move || {
                let providers = providers.clone();
                let repo = repo.clone();
                let edits = edits.clone();
                let version = version.clone();
                let observed_head = head_sha.clone();
                let original_head = cr_step_head.clone();
                async move {
                    let decision = verify_merged_facts(
                        &providers,
                        repo.as_ref(),
                        &edits,
                        &version,
                        number,
                        observed_head,
                        original_head,
                    )
                    .await;
                    dbos::Result::Ok(Ok(decision))
                }
            },
        )
        .await;
        let decision = match outcome {
            Ok(Ok(decision)) => decision,
            Ok(Err(())) | Err(_) => {
                // The provider refused during the verify: transient.
                return StepFlow::Error(InterpreterError::Step(
                    cargobike_core::step::StepError::Transient {
                        reason: "the merge verification could not read the provider".to_owned(),
                    },
                ));
            }
        };
        match decision {
            MergeVerify::Verified => return StepFlow::Continue,
            MergeVerify::NotYet => continue,
            MergeVerify::CrClosed => return StepFlow::Error(InterpreterError::ApprovalRejected),
            MergeVerify::Modified => match on_modified {
                OnModified::Accept => return StepFlow::Continue,
                OnModified::Fail => {
                    return StepFlow::Error(InterpreterError::ChangeRequestModified);
                }
            },
        }
    }
}

/// Whether a resolved step is the env's change-request step (the
/// head-SHA reference on_modified compares against).
fn uses_change_request(step: &ResolvedStep) -> bool {
    matches!(&step.body, crate::template::StepBody::Action { uses, .. } if uses.starts_with("builtin/change-request"))
}

/// A change request just opened: the correlation row lands (the webhook
/// receiver and the reconciler sweep join on it) and the environment's
/// status carries the reference. The stamp rides a durable step, so a
/// replay re-reads the recorded no-op rather than re-stamping.
async fn after_change_request_continue(
    snapshot: &ReleaseSnapshot,
    environment: &ResolvedEnvironment,
    index: usize,
    context: &ExprContext,
    services: &InterpreterServices,
) -> dbos::Result<(), InterpreterError> {
    let release_id = parse_release_id(snapshot)?;
    let cr_step_id = environment.steps[index].id.as_str();
    let record = context
        .steps
        .get(cr_step_id)
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let number = record
        .pointer("/outputs/number")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            InterpreterError::Step(cargobike_core::step::StepError::Failed {
                code: cargobike_core::error::STEP_FAILED.to_owned(),
                message: format!("the change-request step `{cr_step_id}` carried no number"),
            })
        })?;
    let url = record
        .pointer("/outputs/url")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let head_sha = record
        .pointer("/outputs/head_sha")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let environment_name = environment.name.clone();
    let repo_of_env = snapshot
        .repo_of(&environment.name)
        .unwrap_or_else(|| cargobike_core::model::RepoRef::new("github", "0"));

    // The correlation row: only when this CR awaits a merge (the sweep
    // addresses the signal to the merge-wait step's topic).
    if let Some(wait_step) = environment.steps[index + 1..]
        .iter()
        .find(|step| matches!(step.body, crate::template::StepBody::WaitMerge { .. }))
    {
        let row = CorrelationRow {
            provider: repo_of_env.provider.clone(),
            repo_id: repo_of_env.id.clone(),
            cr_number: number as i64,
            release_id,
            environment: environment_name.clone(),
            step_id: wait_step.id.clone(),
            workflow_id: dbos::workflow_id().unwrap_or_else(|| "unknown".to_owned()),
        };
        let correlations = Arc::clone(&services.correlations);
        if let Err(engine_failure) = dbos::step_with::<Result<bool, ()>, InterpreterError, _, _>(
            format!("status/{}/correlation", environment_name).as_str(),
            dbos::StepOptions::default(),
            move || {
                let correlations = Arc::clone(&correlations);
                let row = row.clone();
                async move {
                    match correlations.stamp(&row).await {
                        Ok(()) => dbos::Result::Ok(Ok(true)),
                        Err(cause) => {
                            // The stamp is recoverable: the sweep's
                            // next pass or a retry re-stamps.
                            tracing::warn!(%cause, "the correlation stamp failed");
                            dbos::Result::Ok(Ok(false))
                        }
                    }
                }
            },
        )
        .await
        {
            return dbos::Result::Err(dbos::Error::Application(InterpreterError::Step(
                cargobike_core::step::StepError::Transient {
                    reason: format!("the correlation stamp's step failed: {engine_failure:.100}"),
                },
            )));
        }
    }

    status_write(
        format!("status/{}/change-request", environment_name),
        release_id,
        &services.statuses,
        |store, release_id| {
            let environment = environment_name.clone();
            let reference = ChangeRequestRef {
                number,
                url: url.clone(),
                target_repo: repo_of_env.clone(),
                head_sha: head_sha.clone(),
                state: CrState::Open,
            };
            async move {
                store
                    .environment_change_request(release_id, &environment, reference)
                    .await
            }
        },
    )
    .await
}

/// The recorded outputs an earlier step left in the environment's
/// expression context (`steps.<id>.outputs.<field>`).
fn context_peek(context: &crate::expr::ExprContext, step_id: &str) -> Option<String> {
    context
        .steps
        .get(step_id)
        .and_then(|recorded| recorded.pointer("/outputs/head_sha"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// The facts game: provider state -> head-SHA policy -> content
/// presence, one decision per invocation.
async fn verify_merged_facts(
    providers: &cargobike_core::registry::ProviderRegistry,
    repo: Option<&cargobike_core::model::RepoRef>,
    edits: &[cargobike_core::provider::Edit],
    version: &str,
    number: u64,
    head_sha_at_signal: Option<String>,
    original_head_sha: Option<String>,
) -> MergeVerify {
    use cargobike_core::model::{CrState, RepoRef};

    let fallback = RepoRef::new("github", "0");
    let repo = repo.unwrap_or(&fallback);
    let Ok(provider) = providers.resolve(repo) else {
        return MergeVerify::NotYet; // unavailable: wait again 
    };
    let Ok(change_request) = Provider::get_change_request(&*provider, repo, number).await else {
        return MergeVerify::NotYet; // unavailable this cycle: wait again
    };
    match change_request.state {
        CrState::Open => MergeVerify::NotYet,
        CrState::Closed => MergeVerify::CrClosed,
        CrState::Merged => {
            // The head-SHA move: the ORIGINAL head (the recorded at the CR's
            // opening) vs the merge's head (the re-verify's own read).
            if let (Some(original), Some(current)) =
                (&original_head_sha, Some(&change_request.head_sha))
            {
                if original != current {
                    return MergeVerify::Modified;
                }
            }
            // Consulted at signal time too: a webhook's head_sha higher
            // than the recorded original says the fix-up happened
            // before merge.
            if let (Some(original), Some(observed)) = (&original_head_sha, &head_sha_at_signal) {
                if original != observed {
                    return MergeVerify::Modified;
                }
            }
            match verify_content(provider.as_ref(), repo, edits, version).await {
                Ok(()) => MergeVerify::Verified,
                Err(_) => MergeVerify::Modified,
            }
        }
    }
}
async fn wait_approval(
    environment: &ResolvedEnvironment,
    step_id: &str,
    timeout: Duration,
    on_timeout: OnTimeout,
) -> StepFlow {
    let topic = approval_topic(&environment.name, step_id);
    let taken = dbos::recv::<Signal, InterpreterError>(Some(topic.as_str()), timeout).await;
    let received = match taken {
        Ok(value) => value,
        Err(_engine_failure) => return StepFlow::Error(InterpreterError::ApprovalTimeout),
    };
    match received {
        Some(Signal::ApprovalSubmitted { approved: true, .. }) => StepFlow::Continue,
        Some(Signal::ApprovalSubmitted {
            approved: false, ..
        }) => StepFlow::Error(InterpreterError::ApprovalRejected),
        Some(_) => wrong_signal(),
        None => match on_timeout {
            OnTimeout::Fail => StepFlow::Error(InterpreterError::ApprovalTimeout),
            OnTimeout::Cancel => StepFlow::Error(InterpreterError::Cancelled),
        },
    }
}

/// A signal arrived on a topic that does not belong to it — a bug, and
/// never a successful advance (the topics are addressed per wait).
fn wrong_signal() -> StepFlow {
    StepFlow::Error(InterpreterError::Step(
        cargobike_core::step::StepError::Failed {
            code: cargobike_core::error::STEP_FAILED.to_owned(),
            message: "a signal arrived on the wrong topic; never gated".to_owned(),
        },
    ))
}

/// Content verification (the merged case): the CR's target files on
/// the base branch carry the intended values at the intended pointers.
/// A mismatch becomes `ChangeRequestModified` (the fail).
async fn verify_content(
    provider: &(dyn cargobike_core::provider::Provider + 'static),
    repo: &cargobike_core::model::RepoRef,
    edits: &[cargobike_core::provider::Edit],
    version: &str,
) -> Result<(), cargobike_core::provider::ProviderError> {
    use cargobike_core::provider::ProviderError;
    let base = provider.default_branch(repo).await?;
    for edit in edits {
        let contents = provider
            .read_file(repo, edit.file.as_str(), base.as_str())
            .await?;
        // format inference by extension when the edit didn't
        // declare; the parse is a CONTRACT ERROR (the never honoured as
        // empty — a malformed base document cannot decide).
        let format = edit
            .format
            .unwrap_or_else(|| cargobike_core::edits::format_for_path(edit.file.as_str()));
        let document =
            cargobike_core::edits::parse_document(&contents, format).map_err(|failure| {
                ProviderError::Request(format!(
                    "the merged base file `{}` did not parse: {failure}",
                    edit.file
                ))
            })?;
        // absent or explicit-null values mean the release version;
        // shared with the apply path so both ends agree.
        let desired = cargobike_core::edits::desired_value(
            edit,
            &serde_json::Value::String(version.to_owned()),
        );
        match get_by_dot(&document, edit.field.as_str()) {
            Some(found) if found == &desired => {}
            _ => {
                return Err(ProviderError::Request(format!(
                    "the merged file `{}` does not carry the intended `{}`",
                    edit.file, edit.field
                )));
            }
        }
    }
    Ok(())
}

/// Reads a dot-notation path into the document tree (the field walk);
/// arrays take integer indices.
pub fn get_by_dot<'tree>(
    document: &'tree serde_json::Value,
    field: &str,
) -> Option<&'tree serde_json::Value> {
    let mut current = document;
    for segment in field.split('.') {
        match current {
            serde_json::Value::Object(entries) => {
                current = entries.get(segment)?;
            }
            serde_json::Value::Array(items) => {
                current = items.get(segment.parse::<usize>().ok()?)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

/// The provisioned view (the name/repo/edits/commit_message) from the
/// snapshot's per-environment inputs.
fn environment_env_ref(
    snapshot: &ReleaseSnapshot,
    environment: &ResolvedEnvironment,
) -> cargobike_core::step::EnvRef {
    // provision stamp: an edit without an explicit (the or
    // non-null) value writes the release version; after this point the
    // edits carry values (the providers stay version-agnostic).
    let version_value = snapshot.release.version.clone();
    let edits = snapshot
        .edits_of(&environment.name)
        .into_iter()
        .map(|mut edit| {
            let stamps_default = edit.value.as_ref().is_none_or(serde_json::Value::is_null);
            if stamps_default {
                edit.value = Some(serde_json::Value::String(version_value.clone()));
            }
            edit
        })
        .collect();
    cargobike_core::step::EnvRef {
        name: environment.name.clone(),
        repo: snapshot.repo_of(&environment.name),
        edits,
        commit_message: environment
            .env_inputs
            .get("commit_message")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        branch_format: None,
        release_version: snapshot.release.version.clone(),
    }
}

async fn dispatch_action(
    snapshot: &ReleaseSnapshot,
    environment: &ResolvedEnvironment,
    step: &ResolvedStep,
    context: &mut ExprContext,
    services: &InterpreterServices,
) -> StepFlow {
    let uses = match &step.body {
        StepBody::Action { uses, .. } => uses.clone(),
        _ => return StepFlow::Continue,
    };
    let action = match services.steps.resolve(&uses) {
        Ok(found_step) => found_step,
        Err(registry_error) => {
            return StepFlow::Error(InterpreterError::Step(
                cargobike_core::step::StepError::Failed {
                    code: cargobike_core::error::STEP_FAILED.to_owned(),
                    message: format!("the step `{uses}` is not installed: {registry_error}"),
                },
            ));
        }
    };
    let release_view = snapshot.read_release();
    // provisioned view: repo/edits/commit_message from env inputs.
    let environment_view = environment_env_ref(snapshot, environment);
    // `with`: parameters evaluate against the documented context;
    // the release version is a step-edit's default .
    let with = match interpolate_params(&step.params(), context) {
        Ok(with) => with,
        Err(error) => {
            return StepFlow::Error(InterpreterError::Step(
                cargobike_core::step::StepError::Failed {
                    code: cargobike_core::error::STEP_FAILED.to_owned(),
                    message: format!("the parameters failed to evaluate: {error}"),
                },
            ));
        }
    };
    // action steps run inside `dbos::step` — the checkpoint makes
    // the run exactly-once per attempt (the replays skip the body) and the
    // template's retry policy maps onto `StepOptions` .
    let options = step_options(step);
    let ran = dbos::step_with::<StepOutput, InterpreterError, _, _>(
        format!("builtin/{}/{}", environment.name, step.id).as_str(),
        options,
        move || {
            // kill-point: the harness exits exactly here, at the
            // boundary, before the body runs (the feature-gated hook).
            crate::crash::milestone_maybe(environment.name.as_str(), step.id.as_str());
            // Each attempt builds its own context; the async block owns it.
            let owned_context =
                services.step_context(&snapshot.release.id, &environment.name, step.id.as_str());
            let owned_release = release_view.clone();
            let owned_environment = environment_view.clone();
            let owned_params = with.clone();
            let owned_action = Arc::clone(&action);
            async move {
                // A step's own failure rides the DBOS error column (the
                // F-23 fork's point-resolution reads `error IS NOT
                // NULL`; a failure double-wrapped in the value would be
                // invisible to `ForkFrom::LastFailure`). The deadline's
                // failures land the same way.
                match crate::builtin::execute_step_run(
                    &owned_action,
                    &owned_context,
                    &owned_release,
                    &owned_environment,
                    &owned_params,
                )
                .await
                {
                    Ok(output) => dbos::Result::Ok(output),
                    Err(step_failure) => dbos::Result::Err(dbos::Error::Application(
                        InterpreterError::Step(step_failure),
                    )),
                }
            }
        },
    )
    .await;
    match ran {
        Ok(execution_result) => match execution_result {
            StepOutput::Continue(outputs) => {
                // the recorded outputs (`steps.<id>.outputs.*`) are
                // durable WITH the step's checkpoint — the context only
                // gains them after the run.
                context
                    .steps
                    .insert(step.id.clone(), serde_json::json!({ "outputs": outputs }));
                StepFlow::Continue
            }
            StepOutput::SkipEnvironment => StepFlow::Skip,
            StepOutput::Stop(reason) => StepFlow::Error(InterpreterError::Step(
                cargobike_core::step::StepError::Failed {
                    code: cargobike_core::error::STEP_FAILED.to_owned(),
                    message: reason,
                },
            )),
        },
        Err(dbos::Error::Application(step_failure)) => match step_failure {
            InterpreterError::Step(inner) => StepFlow::Error(InterpreterError::Step(inner)),
            other => StepFlow::Error(other),
        },
        Err(engine_failure) => {
            tracing::error!(%engine_failure, "the durable step engine refused the run");
            StepFlow::Error(InterpreterError::Step(
                cargobike_core::step::StepError::Transient {
                    reason: "the durable step engine refused the run".to_owned(),
                },
            ))
        }
    }
}

/// Maps the template's retry policy to `StepOptions` (; total
/// attempts is `max_attempts`).
fn step_options(step: &ResolvedStep) -> dbos::StepOptions<InterpreterError> {
    let attempt_bound = step.action_kind_timeout();
    let Some(policy) = &step.body_retry_policy() else {
        return dbos::StepOptions {
            timeout: attempt_bound,
            ..dbos::StepOptions::default()
        };
    };
    let attempts = policy.attempts.max(1);
    let interval = policy
        .initial_delay
        .as_deref()
        .and_then(|text| humantime::parse_duration(text).ok())
        .unwrap_or(Duration::from_secs(1));
    let default = dbos::StepOptions::<InterpreterError>::default();
    // backoff: `fixed` stays at the interval; `exponential` grows by
    // the default rate (2.0) within `max_delay`.
    let (backoff_rate, max_interval) = match policy.backoff {
        cargobike_core::template::Backoff::Fixed => (1.0, default.max_interval),
        cargobike_core::template::Backoff::Exponential => (
            default.backoff_rate,
            policy
                .max_delay
                .as_deref()
                .and_then(|text| humantime::parse_duration(text).ok())
                .unwrap_or(default.max_interval),
        ),
    };
    dbos::StepOptions {
        max_attempts: attempts,
        interval,
        backoff_rate,
        max_interval,
        timeout: attempt_bound,
        // The Transient contract: machinery wobble retries, permanent
        // template/step failures do not .
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
