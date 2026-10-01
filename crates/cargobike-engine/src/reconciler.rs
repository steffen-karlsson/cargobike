//! The reconciler: a long-lived durable
//! workflow looping on `dbos::sleep` — the wake time survives crashes
//! (the spike report's kill-and-recover proof) — that checks the ACTUAL
//! CR state for releases in `PendingApproval` and sends signals; the
//! workflow remains the only writer of its release's status.
//!
//! Webhooks are the latency optimisation; the reconciler is the
//! correctness floor .

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use cargobike_core::provider::Provider as _ProviderContract;
use cargobike_core::registry::ProviderRegistry;
use uuid::Uuid;

use crate::signals::{InterpreterError, Signal, merge_topic};

/// The registered reconciler workflow name (the durable loop).
pub const RECONCILE_WORKFLOW: &str = "cargobike.reconcile.v1";

/// The reconciler's single durable argument.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReconcileArgs {
    /// Sweep interval (the default: 5m; config `reconciler.interval`).
    #[serde(with = "humantime_serde_wrap")]
    pub interval: Duration,
    /// Batch size (; config `reconciler.batch_size`).
    pub batch_size: u32,
}

/// humantime's serde for durations (the config's `${*}` untouched form).
pub mod humantime_serde_wrap {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    // humantime text ⇄ Duration.
    pub fn serialize<S: Serializer>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        String::serialize(
            &humantime::format_duration(*duration).to_string(),
            serializer,
        )
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        humantime::parse_duration(&text).map_err(serde::de::Error::custom)
    }
}

/// release scan seam: the sweep joins only releases that are
/// *currently* `PendingApproval` — a signal to a finished release finds
/// no listener, but the provider calls would be waste (the call budget).
/// The server's release repository backs this seam in production; tests
/// mount the in-memory fixture.
#[async_trait]
pub trait PendingReleaseSource: Send + Sync {
    /// The release IDs in the PendingApproval phase (the cursor:
    /// newest first, IDs smaller than the cursor).
    async fn pending_approval_ids(
        &self,
        batch_size: u32,
        cursor: Option<Uuid>,
    ) -> Result<Vec<Uuid>, String>;
}

/// The test fixture for the scan seam (the release ids in a set).
pub struct InMemoryPendingReleaseSource {
    existing_id_set: std::sync::RwLock<HashSet<Uuid>>,
}

impl InMemoryPendingReleaseSource {
    /// The seam over the named pending set.
    pub fn fixture(ids: &[Uuid]) -> Arc<dyn PendingReleaseSource + Send + Sync> {
        Arc::new(Self {
            existing_id_set: std::sync::RwLock::new(ids.iter().copied().collect()),
        })
    }
}

#[async_trait]
impl PendingReleaseSource for InMemoryPendingReleaseSource {
    async fn pending_approval_ids(
        &self,
        batch_size: u32,
        cursor: Option<Uuid>,
    ) -> Result<Vec<Uuid>, String> {
        let read = self.existing_id_set.read();
        let Ok(set) = read else {
            return Err("the fixture set is poisoned".to_owned());
        };
        // Newest-first with the cursor skip (the live seam's ordering;
        // the fixture simply must terminate its pagination).
        let mut ids: Vec<Uuid> = set.iter().copied().collect();
        ids.sort();
        ids.reverse();
        let start = cursor
            .map(|latest| {
                ids.iter()
                    .take_while(|candidate| *candidate >= &latest)
                    .count()
            })
            .unwrap_or(0);
        Ok(ids
            .into_iter()
            .skip(start)
            .take(batch_size as usize)
            .collect())
    }
}

/// The reconciler's services (the providers, the correlation read side, the
/// release scan).
pub struct ReconcilerServices {
    /// Providers .
    pub providers: Arc<ProviderRegistry>,
    /// CR correlation rows (the sweep's input).
    pub correlations: Arc<crate::correlation::CorrelationRepository>,
    /// The PendingApproval scan (the join through this seam).
    pub releases: Arc<dyn PendingReleaseSource + Send + Sync>,
}

/// Registers the reconciler BEFORE launch (the registry snapshot).
pub fn register_reconciler(
    instance: &dbos::DBOS,
    services: Arc<ReconcilerServices>,
) -> dbos::Result<dbos::WorkflowRef<ReconcileArgs, (), InterpreterError>> {
    instance.register_workflow(RECONCILE_WORKFLOW, move |args: ReconcileArgs| {
        let services = Arc::clone(&services);
        async move { loop_fn(args, services).await }
    })
}

/// The loop: sleep → sweep → repeat .
async fn loop_fn(
    args: ReconcileArgs,
    services: Arc<ReconcilerServices>,
) -> dbos::Result<(), InterpreterError> {
    loop {
        let _slept = dbos::sleep::<InterpreterError>(args.interval).await;
        let swept = sweep(&services, args.batch_size)
            .await
            .unwrap_or_else(|failure| {
                tracing::warn!(%failure, "reconcile sweep failed; the next sweep retries");
                0
            });
        tracing::debug!(corrected = swept, "reconcile sweep complete");
    }
}

/// One sweep: join the PendingApproval releases, then check their CR
/// states and send the signals only (the never writes release status).
async fn sweep(services: &ReconcilerServices, batch_size: u32) -> Result<u32, InterpreterError> {
    const SWEEP_PAGE: u32 = 200;
    // scan: consulted once per sweep; the join is in-memory.
    let mut pending: HashSet<Uuid> = HashSet::new();
    let mut release_cursor: Option<Uuid> = None;
    loop {
        let batch = services
            .releases
            .pending_approval_ids(batch_size.max(1), release_cursor)
            .await
            .map_err(|sqlx_failure| {
                InterpreterError::Step(cargobike_core::step::StepError::Failed {
                    code: cargobike_core::error::STEP_FAILED.to_owned(),
                    message: format!("the release scan failed to read: {sqlx_failure}"),
                })
            })?;
        let exhausted = batch.len() < batch_size.max(1) as usize;
        release_cursor = batch.last().copied();
        pending.extend(batch);
        if exhausted {
            break;
        }
    }
    if pending.is_empty() {
        return Ok(0);
    }
    let mut sent = 0_u32;
    let mut cursor: Option<(String, String, i64)> = None;
    loop {
        let rows = services
            .correlations
            .page(SWEEP_PAGE, cursor.clone())
            .await
            .map_err(|sqlx_failure| {
                InterpreterError::Step(cargobike_core::step::StepError::Failed {
                    code: cargobike_core::error::STEP_FAILED.to_owned(),
                    message: format!("the correlation sweep failed to read: {sqlx_failure}"),
                })
            })?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            // The join: only live releases' correlations speak .
            if !pending.contains(&row.release_id) {
                continue;
            }
            let repo = cargobike_core::model::RepoRef::new(&row.provider, &row.repo_id);
            let Ok(provider) = services.providers.resolve(&repo) else {
                continue; // the provider is gone; the correlation is stale
            };
            let Ok(change_request) =
                _ProviderContract::get_change_request(&*provider, &repo, row.cr_number as u64)
                    .await
            else {
                continue; // unavailable this cycle: the next sweep retries 
            };
            let merged = matches!(change_request.state, cargobike_core::model::CrState::Merged);
            let closed = matches!(change_request.state, cargobike_core::model::CrState::Closed);
            if !(merged || closed) {
                continue; // still open: nothing to send
            }
            let (key, topic, message) = reconciler_send_form(row, merged);
            let options = dbos::SendOptions {
                topic: Some(topic.as_ref() as &str),
                idempotency_key: Some(key.as_str()),
                ..dbos::SendOptions::default()
            };
            let _result: dbos::Result<(), dbos::EngineOnly> =
                dbos::send_with(&row.workflow_id, &message, options).await;
            sent += 1;
        }
        if let Some(last) = rows.last().cloned() {
            cursor = Some((last.provider, last.repo_id, last.cr_number));
        }
        if rows.len() < SWEEP_PAGE as usize {
            break;
        }
    }
    Ok(sent)
}

/// The signal half of one row: topic + idempotency key + envelope (the owned;
/// the caller builds the borrow-bearing `SendOptions` across its await).
fn reconciler_send_form(
    row: &crate::correlation::CorrelationRow,
    merged: bool,
) -> (String, Arc<str>, Signal) {
    let topic: Arc<str> = Arc::from(merge_topic(&row.environment, &row.step_id).as_str());
    let key = crate::signals::merge_signal_key(
        &row.provider,
        &format!(
            "reconcile/{}/{}/{}/{}",
            row.release_id,
            row.environment,
            row.cr_number,
            if merged { "merged" } else { "closed" }
        ),
    );
    (
        key,
        topic,
        Signal::MergeComplete {
            merged,
            by_way_of: format!("reconciler/cr-{}", row.cr_number),
            // re-verify needs the identity; the sweep's
            // observation provides the number (the head SHA arrives
            // via the provider re-verify).
            number: row.cr_number as u64,
            head_sha: None,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_humantime_serde_roundtrips_the_config_forms() {
        #[derive(serde::Deserialize)]
        struct Form {
            #[serde(with = "super::humantime_serde_wrap")]
            value: Duration,
        }
        let form: Form = serde_yaml_ng::from_str("value: 5m").expect("duration");
        assert_eq!(form.value.as_secs(), 300);
    }
}
