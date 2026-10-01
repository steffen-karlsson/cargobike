//! The durable interpreter (PRD 3.9/3.4, F-15..F-24): one registered
//! DBOS workflow per instance (`cargobike.interpret.v1`), the control
//! steps (F-34's wait rules), and the merge verification (F-62/F-64).
//!
//! The body is a pure function of the snapshot and the recorded outputs
//! (F-15). Wait steps are interpreter-native: `recv` over the signal
//! topics (F-20a) and a provider re-verification for merges. One
//! argument only — the snapshot (the spike's confirmed shape); the
//! closure must not capture the instance (spike doc §2.1).

use std::sync::Arc;
use std::time::Duration;

use cargobike_core::error::ReleaseError;
use cargobike_core::model::EnvironmentPhase;
use cargobike_core::provider::Provider;
use cargobike_core::step::{HttpService, StepContext, StepOutput};
use cargobike_core::template::{OnModified, OnTimeout};
use tracing::Span;

use crate::expr::{ExprContext, eval_gate};
use crate::leases::LeaseRepository;

use crate::signals::{InterpreterError, Signal, approval_topic, merge_topic};
use crate::snapshot::ReleaseSnapshot;
use crate::steps::StepRegistry;
use crate::template::{ResolvedEnvironment, ResolvedStep, StepBody};

/// The registered interpreter workflow name (F-15).
pub const INTERPRETER_WORKFLOW: &str = "cargobike.interpret.v1";

/// The interpreter's single durable argument.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InterpretArgs {
    /// The F-16 snapshot: template, inputs, step-type versions, hash.
    pub snapshot: ReleaseSnapshot,
}

/// The interpreter's result (the environments' phases).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InterpretResult {
    /// Per-environment outcomes in declaration order.
    pub environments: Vec<EnvironmentOutcome>,
}

/// One environment's outcome (F-7's rollup input).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnvironmentOutcome {
    /// The environment's name.
    pub name: String,
    /// The environment's terminal phase.
    pub phase: EnvironmentPhase,
    /// Error detail when failed (F-8).
    pub error: Option<ReleaseError>,
}

/// Everything the interpreter runs against, part of the registered
/// closure (the executor snapshots the registry at launch, F-15).
pub struct InterpreterServices {
    /// Step types installed (3.3's registry).
    pub steps: Arc<StepRegistry>,
    /// Providers resolved by `RepoRef.provider` name (F-39).
    pub providers: Arc<cargobike_core::registry::ProviderRegistry>,
    /// Named secrets (F-146).
    pub credentials: Arc<dyn cargobike_core::registry::CredentialStore>,
    /// The SSRF-guarded client (§6.7's step paths).
    pub http: Arc<dyn HttpService>,
    /// Lease rows (A2/F-71).
    pub leases: Arc<LeaseRepository>,
}

impl InterpreterServices {
    /// A `StepContext` for a concrete step run (F-39).
    pub fn step_context(&self, release_id: &str, environment: &str, step_id: &str) -> StepContext {
        StepContext {
            providers: (*self.providers).clone(),
            http: Arc::clone(&self.http),
            credentials: Arc::clone(&self.credentials),
            // A1: `release_id/environment/step_id` is the idempotency input.
            idempotency_key: format!("{release_id}/{environment}/{step_id}"),
            cancel_token: cargobike_core::step::CancelToken::new(),
            log: Span::current(),
        }
    }
}

/// Registers the interpreter; registration only works BEFORE `launch()`
/// (F-15; the spike's registry snapshot applies).
pub fn register_interpreter(
    instance: &dbos::DBOS,
    services: Arc<InterpreterServices>,
) -> dbos::Result<dbos::WorkflowRef<InterpretArgs, InterpretResult, InterpreterError>> {
    instance.register_workflow(INTERPRETER_WORKFLOW, move |args: InterpretArgs| {
        let services = Arc::clone(&services);
        async move { run(args, services).await }
    })
}

/// The workflow body: environments × steps of the snapshot (F-15's
/// pure-function rule).
async fn run(
    args: InterpretArgs,
    services: Arc<InterpreterServices>,
) -> dbos::Result<InterpretResult, InterpreterError> {
    let mut outcomes = Vec::with_capacity(args.snapshot.template.environments.len());
    for environment in &args.snapshot.template.environments {
        let outcome = run_environment(&args.snapshot, environment, &services).await;
        outcomes.push(outcome);
    }
    dbos::Result::Ok(InterpretResult {
        environments: outcomes,
    })
}

/// One environment: the gate decides a skip; steps run in order.
async fn run_environment(
    snapshot: &ReleaseSnapshot,
    environment: &ResolvedEnvironment,
    services: &InterpreterServices,
) -> EnvironmentOutcome {
    let context = snapshot.context(&environment.name);
    if let Some(when) = &environment.when {
        match eval_gate(when, &context) {
            Ok(false) => return finished(environment, EnvironmentPhase::Skipped, None),
            Err(error) => return error_to_outcome(environment, expr_failure(error)),
            Ok(true) => {}
        }
    }
    for step in &environment.steps {
        match run_step(snapshot, environment, step, &context, services).await {
            StepFlow::Continue => {}
            StepFlow::Skip => return finished(environment, EnvironmentPhase::Skipped, None),
            StepFlow::Error(failure) => return error_to_outcome(environment, failure),
        }
    }
    finished(environment, EnvironmentPhase::Completed, None)
}

/// Maps an expression failure to the interpreter's envelope (F-8's
/// `StepFailed` family: the step never evaluated).
fn expr_failure(error: crate::expr::ExprError) -> InterpreterError {
    InterpreterError::Step(cargobike_core::step::StepError::Failed {
        code: cargobike_core::error::STEP_FAILED.to_owned(),
        message: error.to_string(),
    })
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

/// An interpreter failure becomes the environment's F-8 outcome.
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

/// The F-7 phase each failure maps to.
pub const fn environment_phase_of(failure: &InterpreterError) -> EnvironmentPhase {
    match failure {
        InterpreterError::Cancelled => EnvironmentPhase::Canceled,
        _ => EnvironmentPhase::Failed,
    }
}

/// What one step told the environment.
enum StepFlow {
    /// Keep going.
    Continue,
    /// The environment skips (F-7's skip path).
    Skip,
    /// A failure the environment takes (F-8's error).
    Error(InterpreterError),
}

/// One step: action steps run their registered type; control steps walk
/// the wait logic (F-33's two kinds).
async fn run_step(
    snapshot: &ReleaseSnapshot,
    environment: &ResolvedEnvironment,
    step: &ResolvedStep,
    context: &ExprContext,
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
        } => wait_approval(environment, step_id.as_str(), *timeout, *on_timeout).await,
        StepBody::WaitSleep { duration } => {
            let _slept = dbos::sleep::<InterpreterError>(*duration).await;
            StepFlow::Continue
        }
        StepBody::Action { uses, .. } => {
            dispatch_action(
                snapshot,
                environment,
                step_id.as_str(),
                uses,
                context,
                services,
            )
            .await
        }
    }
}

/// `wait: merge`'s enclosure (F-62): recv → merged → verified.
#[allow(clippy::too_many_arguments)]
async fn wait_merge(
    snapshot: &ReleaseSnapshot,
    environment: &ResolvedEnvironment,
    step_id: &str,
    services: &InterpreterServices,
    timeout: Duration,
    on_timeout: OnTimeout,
    _on_modified: OnModified,
) -> StepFlow {
    let topic = merge_topic(&environment.name, step_id);
    let taken = dbos::recv::<Signal, InterpreterError>(Some(topic.as_str()), timeout).await;
    let received = match taken {
        Ok(value) => value,
        Err(_engine_failure) => return StepFlow::Error(InterpreterError::MergeTimeout),
    };
    let Some(received) = received else {
        // F-34's deadline semantics.
        return match on_timeout {
            OnTimeout::Fail => StepFlow::Error(InterpreterError::MergeTimeout),
            OnTimeout::Cancel => StepFlow::Error(InterpreterError::Cancelled),
        };
    };
    match received {
        Signal::MergeComplete { merged: false, .. } => {
            // Closed without merge is terminal (`ApprovalRejected`; F-62).
            StepFlow::Error(InterpreterError::ApprovalRejected)
        }
        Signal::MergeComplete { merged: true, .. } => {
            match verify_merged(snapshot, environment, services).await {
                Ok(()) => StepFlow::Continue,
                Err(failure) => StepFlow::Error(failure),
            }
        }
        // The merge topic carries merges only; anything else is a bug.
        Signal::ApprovalSubmitted { .. } | Signal::LeaseReleased { .. } => wrong_signal(),
    }
}

/// `wait: approval` (F-59/F-96): an approval submission advances; a
/// rejection fails immediately; nothing by the deadline ⇒ the F-34
/// outcomes.
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
/// never a successful advance (F-20a's topics are addressed per wait).
fn wrong_signal() -> StepFlow {
    StepFlow::Error(InterpreterError::Step(
        cargobike_core::step::StepError::Failed {
            code: cargobike_core::error::STEP_FAILED.to_owned(),
            message: "a signal arrived on the wrong topic; never gated".to_owned(),
        },
    ))
}

/// Content verification (F-62's merged case): the CR's target files on
/// the base branch carry the intended values at the intended pointers.
/// A mismatch becomes `ChangeRequestModified` (F-62's fail).
async fn verify_merged(
    snapshot: &ReleaseSnapshot,
    environment: &ResolvedEnvironment,
    services: &InterpreterServices,
) -> Result<(), InterpreterError> {
    let Some(repo) = snapshot.repo_of(&environment.name) else {
        return Err(InterpreterError::Step(
            cargobike_core::step::StepError::Failed {
                code: cargobike_core::error::STEP_FAILED.to_owned(),
                message: "the environment's edits have no target repo (F-32a)".to_owned(),
            },
        ));
    };
    let provider = services
        .providers
        .resolve(&repo)
        .map_err(failure_from_library)?;
    let base = Provider::default_branch(&*provider, &repo)
        .await
        .map_err(failure_from_provider)?;
    let edits = snapshot.edits_of(&environment.name);
    let version = snapshot_release_version(snapshot);
    for edit in &edits {
        let contents = Provider::read_file(&*provider, &repo, edit.file.as_str(), base.as_str())
            .await
            .map_err(failure_from_provider)?;
        let text =
            String::from_utf8(contents).map_err(|_| InterpreterError::ChangeRequestModified)?;
        let document: serde_json::Value = match edit
            .format
            .unwrap_or(cargobike_core::provider::EditFormat::Yaml)
        {
            cargobike_core::provider::EditFormat::Json => {
                serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
            }
            _ => serde_yaml_ng::from_str(&text).unwrap_or(serde_json::Value::Null),
        };
        let desired = edit
            .value
            .clone()
            .unwrap_or(serde_json::Value::String(version.clone()));
        match get_by_dot(&document, edit.field.as_str()) {
            Some(field) if field == &desired => {}
            _ => return Err(InterpreterError::ChangeRequestModified),
        }
    }
    Ok(())
}

/// A provider error via a wrapper into the interpreter's envelope.
fn failure_from_provider(failure: cargobike_core::provider::ProviderError) -> InterpreterError {
    InterpreterError::Step(cargobike_core::step::StepError::Failed {
        code: cargobike_core::error::STEP_FAILED.to_owned(),
        message: failure.to_string(),
    })
}

fn failure_from_library(failure: cargobike_core::error::LibraryError) -> InterpreterError {
    InterpreterError::Step(cargobike_core::step::StepError::Failed {
        code: cargobike_core::error::STEP_FAILED.to_owned(),
        message: failure.to_string(),
    })
}

/// The version string in an environment (F-8's own vocabulary).
fn snapshot_release_version(snapshot: &ReleaseSnapshot) -> String {
    let _ = snapshot;
    String::new()
}

/// Reads a dot-notation path into the document tree (F-41's field walk);
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

/// Action steps dispatch into their registered type (F-35; PRD §6.7's
/// isolation rules: the client is constructed inside the step).
async fn dispatch_action(
    snapshot: &ReleaseSnapshot,
    environment: &ResolvedEnvironment,
    step_id: &str,
    uses: &str,
    _context: &ExprContext,
    services: &InterpreterServices,
) -> StepFlow {
    let Some(action) = services.steps.resolve(uses).ok() else {
        return StepFlow::Error(InterpreterError::Step(
            cargobike_core::step::StepError::Failed {
                code: cargobike_core::error::STEP_FAILED.to_owned(),
                message: format!("the step `{uses}` is not installed (F-35)"),
            },
        ));
    };
    let step_context = services.step_context(&snapshot.release.id, &environment.name, step_id);
    let release_view = snapshot.read_release();
    let environment_spec = snapshot.env_spec(&environment.name);
    let output = action
        .execute(
            &step_context,
            &release_view,
            &environment_spec,
            &serde_json::Value::Null,
        )
        .await;
    match output {
        Ok(StepOutput::Continue(_)) => StepFlow::Continue,
        Ok(StepOutput::SkipEnvironment) => StepFlow::Skip,
        Ok(StepOutput::Stop(reason)) => StepFlow::Error(InterpreterError::Step(
            cargobike_core::step::StepError::Failed {
                code: cargobike_core::error::STEP_FAILED.to_owned(),
                message: reason,
            },
        )),
        Err(error) => StepFlow::Error(InterpreterError::Step(error)),
    }
}
