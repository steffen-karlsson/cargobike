//! The webhook workflow (Phase 5.1/5.2): the server's receiver
//! fast-acks, this workflow does the durable follow-through —
//! tag-pushed release creation (the registry scan, the protection
//! check, the shared create core — the server's seam) and the
//! `pull_request.closed` correlation signal.
//!
//! The delivery id keys the workflow (`cargobike/webhook/<provider>/
//! <delivery>`), so GitHub's redeliveries deduplicate into the recorded
//! run. The workflow is the reason "verify before parse + fast-ack" is
//! safe: every acting side effect re-runs idempotently here.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use cargobike_core::webhook::NormalisedEvent;

use crate::correlation::CorrelationRepository;
use crate::signals::{InterpreterError, Signal, merge_signal_key, merge_topic};

/// The registered webhook workflow name.
pub const WEBHOOK_WORKFLOW: &str = "cargobike.webhook.v1";

/// The webhook workflow's single durable argument.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WebhookArgs {
    /// The provider the delivery rode in on.
    pub provider: String,
    /// The delivery's id (the dedupe's key half; the workflow's id
    /// carries it, this records it for the audit trail).
    pub delivery_id: String,
    /// The normalised event.
    pub event: NormalisedEvent,
}

/// The tag-push create seam: the registry scan + the tag-format
/// extraction + the tag-protection check + the create core live on the
/// server (the engine consumes their result). The outcome records what
/// the push did for the audit surface.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TagPushOutcome {
    /// Releases created by this push.
    pub created: Vec<Uuid>,
    /// Releases that already existed and were left alone.
    pub duplicates: Vec<Uuid>,
    /// Skips with reasons (protection refused, no trigger, ...).
    pub skipped: Vec<String>,
}

#[async_trait]
pub trait TagPushCreator: Send + Sync {
    /// Creates (or finds) the release(s) one tag push stands for.
    async fn create_from_tag(
        &self,
        provider: &str,
        repository_id: &str,
        tag: &str,
        sha: &str,
        sender: &str,
    ) -> Result<TagPushOutcome, String>;
}

/// The webhook workflow's services (the correlation read + the server's
/// create seam).
pub struct WebhookServices {
    /// CR correlation rows (the closed-PR path's input).
    pub correlations: Arc<CorrelationRepository>,
    /// The tag-push create seam (the server's impl).
    pub creator: Arc<dyn TagPushCreator>,
}

/// Registers the webhook workflow BEFORE launch (the registry snapshot).
pub fn register_webhook(
    instance: &dbos::DBOS,
    services: Arc<WebhookServices>,
) -> dbos::Result<dbos::WorkflowRef<WebhookArgs, (), InterpreterError>> {
    instance.register_workflow(WEBHOOK_WORKFLOW, move |args: WebhookArgs| {
        let services = Arc::clone(&services);
        async move { deliver(args, services).await }
    })
}

/// One delivery's follow-through (the workflow's body).
async fn deliver(
    args: WebhookArgs,
    services: Arc<WebhookServices>,
) -> dbos::Result<(), InterpreterError> {
    match &args.event {
        NormalisedEvent::TagPush(tag) => {
            // The create rides one durable step: a replay of the
            // delivery re-reads the recorded outcome instead of
            // re-acting (the create core's own dedupe is the second
            // net).
            let outcome = dbos::step_with::<Result<TagPushOutcome, ()>, InterpreterError, _, _>(
                "webhook/tag-push",
                dbos::StepOptions::default(),
                {
                    let creator = Arc::clone(&services.creator);
                    let provider = args.provider.clone();
                    let repository_id = tag.repository_id.clone();
                    let tag_name = tag.tag.clone();
                    let sha = tag.sha.clone();
                    let sender = tag.sender.clone();
                    move || {
                        let creator = Arc::clone(&creator);
                        let provider = provider.clone();
                        let repository_id = repository_id.clone();
                        let tag_name = tag_name.clone();
                        let sha = sha.clone();
                        let sender = sender.clone();
                        async move {
                            match creator
                                .create_from_tag(
                                    &provider, &repository_id, &tag_name, &sha, &sender,
                                )
                                .await
                            {
                                Ok(outcome) => dbos::Result::Ok(Ok(outcome)),
                                Err(failure) => {
                                    tracing::warn!(%failure, "the tag-push create failed; the skip is recorded as the delivery's outcome");
                                    dbos::Result::Ok(Ok(TagPushOutcome {
                                        created: vec![],
                                        duplicates: vec![],
                                        skipped: vec![failure],
                                    }))
                                }
                            }
                        }
                    }
                },
            )
            .await?;
            let outcome = outcome.unwrap_or_else(|()| {
                // The step's () layer only surfaces for a cancelled run;
                // record an empty outcome for the audit trail.
                TagPushOutcome {
                    created: vec![],
                    duplicates: vec![],
                    skipped: vec![],
                }
            });
            tracing::info!(
                created = outcome.created.len(),
                duplicates = outcome.duplicates.len(),
                skipped = %outcome.skipped.len(),
                "the tag push followed through"
            );
            Ok(())
        }
        NormalisedEvent::ChangeRequestClosed {
            number,
            merged,
            repository_id,
            sender: _,
        } => {
            let Some(row) = services
                .correlations
                .find(&args.provider, repository_id, *number as i64)
                .await
                .map_err(|failure| {
                    tracing::warn!(%failure, "the correlation read failed; the hangover is the reconciler's sweep");
                    InterpreterError::Step(cargobike_core::step::StepError::Failed {
                        code: cargobike_core::error::STEP_FAILED.to_owned(),
                        message: format!("the correlation read failed: {failure}"),
                    })
                })?
            else {
                tracing::debug!(
                    delivery = %args.delivery_id,
                    provider = %args.provider,
                    %repository_id,
                    %number,
                    "no correlation for the closed CR; the delivery is acknowledged and ignored"
                );
                return Ok(());
            };
            let (key, topic, message) =
                webhook_send_form(&row, *number, *merged, &args.provider, &args.delivery_id);
            let options = dbos::SendOptions {
                topic: Some(topic.as_ref() as &str),
                idempotency_key: Some(key.as_str()),
                ..dbos::SendOptions::default()
            };
            let sent: dbos::Result<(), dbos::EngineOnly> =
                dbos::send_with(&row.workflow_id, &message, options).await;
            if let Err(failure) = sent {
                // The correlation points at a workflow this instance
                // cannot address (a foreign-key refusal on the
                // nonexistent destination; DBOS's contract). The signal
                // is not lost: the attempt's own wake timeout and the
                // reconciler's sweep still look at the CR's actual
                // state.
                tracing::warn!(
                    workflow = %row.workflow_id,
                    release = %row.release_id,
                    %failure,
                    "the webhook's signal could not be addressed; the sweep remains the floor"
                );
            }
            Ok(())
        }
        NormalisedEvent::Unrecognised { provider_event } => {
            tracing::debug!(
                delivery = %args.delivery_id,
                %provider_event,
                "unrecognised event: acknowledged and ignored"
            );
            Ok(())
        }
    }
}

/// The closed-CR signal form (the webhook's observation substitutes the
/// reconciler's sweep; the wait re-verifies against the provider no
/// matter who spoke).
fn webhook_send_form(
    row: &crate::correlation::CorrelationRow,
    number: u64,
    merged: bool,
    provider: &str,
    delivery_id: &str,
) -> (String, Arc<str>, Signal) {
    let topic: Arc<str> = Arc::from(merge_topic(&row.environment, &row.step_id).as_str());
    let key = merge_signal_key(
        provider,
        &format!("webhook/{delivery_id}/{}/cr-{number}", row.release_id),
    );
    (
        key,
        topic,
        Signal::MergeComplete {
            merged,
            by_way_of: format!("webhook/cr-{number}"),
            number,
            head_sha: None,
        },
    )
}
