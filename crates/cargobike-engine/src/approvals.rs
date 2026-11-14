//! The approval workflow (Phase 5.5, F-59): the approvals endpoint
//! authorises and records the decision, this workflow does the durable
//! follow-through — the `Signal::ApprovalSubmitted` send into the
//! parked `wait: approval`'s durable recv, plus the decision's event
//! on the release's hash-chained log. Both sides are idempotent (the
//! send keys on the decision, the event id is deterministic), so a
//! replayed submission converges instead of double-signalling.
//!
//! The counting policy (F-96's `required` distinct principals, the
//! reject-fast rule) lives on the SERVER — the workflow receives the
//! decision that already crossed the threshold (or the reject).

use std::sync::Arc;

use uuid::Uuid;

use crate::signals::{Signal, approval_signal_key, approval_topic};

/// The registered approval workflow name.
pub const APPROVAL_WORKFLOW: &str = "cargobike.approval.v1";

/// The approval workflow's single durable argument.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApprovalArgs {
    /// The release the decision addresses.
    pub release_id: Uuid,
    /// The environment whose wait parks.
    pub environment: String,
    /// The parked `wait: approval` step (F-20b's binding).
    pub step_id: String,
    /// The decision — `approve` or `reject`.
    pub decision: String,
    /// The distinct principal (`issuer/subject`; api keys are
    /// `api-key/<name>`) — F-96's distinct-count unit and the event's
    /// actor.
    pub principal: String,
    /// Presentable name for the event's reason text.
    pub approver: String,
    /// The caller's display facts as the event's actor (the verified
    /// identity, not body-supplied).
    pub actor: serde_json::Value,
    /// Optional comment.
    pub comment: Option<String>,
}

/// The approval workflow's services: the event sink the decision's
/// audit row rides on.
pub struct ApprovalServices {
    /// The release event log.
    pub events: Arc<dyn crate::eventlog::EventSink>,
}

/// Registers the approval workflow BEFORE launch (the registry
/// snapshot).
pub fn register_approval(
    instance: &dbos::DBOS,
    services: Arc<ApprovalServices>,
) -> dbos::Result<dbos::WorkflowRef<ApprovalArgs, (), crate::InterpreterError>> {
    instance.register_workflow(APPROVAL_WORKFLOW, move |args: ApprovalArgs| {
        let services = Arc::clone(&services);
        async move { deliver(args, services).await }
    })
}

/// One decision's follow-through (the workflow's body).
#[allow(clippy::too_many_lines)]
async fn deliver(
    args: ApprovalArgs,
    services: Arc<ApprovalServices>,
) -> dbos::Result<(), crate::InterpreterError> {
    let approved = args.decision == "approve";
    // One durable step: the signal send and the audit row ride
    // together — a replay re-runs both, and both converge (the send's
    // idempotency key, the event's deterministic id).
    let topic = approval_topic(&args.environment, &args.step_id);
    let key = approval_signal_key(
        &args.release_id.to_string(),
        &args.environment,
        &args.step_id,
        &args.decision,
        &args.principal,
    );
    let workflow_id = crate::interpreter::interpret_workflow_id(args.release_id);
    let signal = Signal::ApprovalSubmitted {
        approved,
        principal: args.principal.clone(),
        approver: args.approver.clone(),
        comment: args.comment.clone(),
    };
    let _ = dbos::step_with::<Result<(), String>, crate::InterpreterError, _, _>(
        "approval/submit",
        dbos::StepOptions::default(),
        move || {
            let topic = topic.clone();
            let key = key.clone();
            let workflow_id = workflow_id.clone();
            let signal = signal.clone();
            let events = Arc::clone(&services.events);
            let args = args.clone();
            async move {
                let options = dbos::SendOptions {
                    topic: Some(topic.as_ref()),
                    idempotency_key: Some(key.as_str()),
                    ..dbos::SendOptions::default()
                };
                let send = match dbos::send_with::<Signal, dbos::EngineOnly>(
                    &workflow_id,
                    &signal,
                    options,
                )
                .await
                {
                    dbos::Result::Ok(()) => Ok(()),
                    dbos::Result::Err(failure) => {
                        Err(format!("the approval signal send failed: {failure}"))
                    }
                };
                // The audit row: the event id is the decision's
                // identity (a replay converges on the first append).
                let event_id = format!(
                    "{}/{}/approval-{}/{}",
                    args.release_id, args.environment, args.step_id, args.principal
                );
                let reason = match (&args.comment, approved) {
                    (Some(comment), _) => format!(
                        "{} by {} — {comment}",
                        if approved { "approved" } else { "rejected" },
                        args.approver
                    ),
                    (None, true) => format!("approved by {}", args.approver),
                    (None, false) => format!("rejected by {}", args.approver),
                };
                let noted = crate::eventlog::EventSink::append(
                    events.as_ref(),
                    args.release_id,
                    &event_id,
                    if approved {
                        "approval-granted"
                    } else {
                        "approval-rejected"
                    },
                    &reason,
                    serde_json::json!({
                        "environment": args.environment,
                        "step": args.step_id,
                        "decision": args.decision,
                    }),
                    Some(args.actor.clone()),
                )
                .await;
                if let Err(failure) = noted {
                    // The signal is the liveness half; the audit row is
                    // the record. A lost row is logged, not fatal (the
                    // status seam's trade-off).
                    tracing::warn!(
                        %failure,
                        release = %args.release_id,
                        "the approval's event append failed"
                    );
                }
                // A failed send fails the step: the workflow retries
                // (the idempotency key makes the retry a no-op when the
                // first send actually landed).
                dbos::Result::Ok(send)
            }
        },
    )
    .await?;
    Ok(())
}
