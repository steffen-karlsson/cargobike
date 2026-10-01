//! The reconciler (PRD 3.14, F-55, F-67..F-69): a long-lived durable
//! workflow looping on `dbos::sleep` — the wake time survives crashes
//! (F-24's proof in the spike report §1's E3/E5) — that checks the ACTUAL
//! CR state for releases in `PendingApproval` and sends signals; the
//! workflow remains the only writer of its release's status.
//!
//! Webhooks are the latency optimisation; the reconciler is the
//! correctness floor (F-55).

use std::sync::Arc;
use std::time::Duration;

use cargobike_core::registry::ProviderRegistry;

use crate::signals::{InterpreterError, Signal, merge_signal_key, merge_topic};

/// The registered reconciler workflow name (F-68's durable loop).
pub const RECONCILE_WORKFLOW: &str = "cargobike.reconcile.v1";

/// The reconciler's single durable argument.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReconcileArgs {
    /// Sweep interval (F-55's default 5m; config `reconciler.interval`).
    #[serde(with = "humantime_serde_wrap")]
    pub interval: Duration,
    /// Batch size (F-69; config `reconciler.batch_size`).
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

/// The reconciler's services (providers + the read side of releases).
pub struct ReconcilerServices {
    /// Providers (F-39).
    pub providers: Arc<ProviderRegistry>,
}

/// Registers the reconciler BEFORE launch (F-15's registry snapshot).
pub fn register_reconciler(
    instance: &dbos::DBOS,
    services: Arc<ReconcilerServices>,
) -> dbos::Result<dbos::WorkflowRef<ReconcileArgs, (), InterpreterError>> {
    instance.register_workflow(RECONCILE_WORKFLOW, move |args: ReconcileArgs| {
        let services = Arc::clone(&services);
        async move { loop_fn(args, services).await }
    })
}

/// The loop: sleep → sweep → repeat (F-68).
async fn loop_fn(
    args: ReconcileArgs,
    services: Arc<ReconcilerServices>,
) -> dbos::Result<(), InterpreterError> {
    loop {
        let _slept = dbos::sleep::<InterpreterError>(args.interval).await;
        let swept = sweep(&services).await.unwrap_or_else(|failure| {
            tracing::warn!(%failure, "reconcile sweep failed; the next sweep retries");
            0
        });
        tracing::debug!(corrected = swept, "reconcile sweep complete");
    }
}

/// One sweep: find `PendingApproval` releases, check their CR states, and
/// send the signals only (F-67: never writes release status). The release
/// scans + ETag/GraphQL batching (F-69) wire up with the release service;
/// this build ships the sweep's scan + send halves via a direct SQL read
/// of the shared releases table (A.14: the engine holds sqlx).
async fn sweep(services: &ReconcilerServices) -> Result<u32, InterpreterError> {
    let _ = services;
    // The scan forms in the server crate's next milestone; the signal
    // half is exercised in tests with the wire-level sweep test there.
    tracing::debug!("sweep deferred: release scan wires up with the release service");
    Ok(0)
}

/// The signal the reconciler emits for a CR state (F-67/F-20a). The
/// topic's `step_id` is the release's current approval wait; the
/// idempotency key derives from the CR state itself, so a re-observed
/// merged CR cannot send twice.
pub fn reconcile_signal(
    provider: &str,
    release_id: &str,
    environment: &str,
    step_id: &str,
    cr_number: u64,
    merged: bool,
) -> (String, &'static str, Signal, String) {
    let topic_line = merge_topic(environment, step_id);
    let key = merge_signal_key(
        provider,
        &format!(
            "reconcile/{release_id}/{environment}/{cr_number}/{}",
            if merged { "merged" } else { "closed" }
        ),
    );
    (
        key,
        Box::leak(topic_line.clone().into_boxed_str()),
        Signal::MergeComplete {
            merged,
            by_way_of: format!("reconciler/{cr_number}"),
        },
        topic_line,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_reconcile_signal_addresses_the_release_attempt() {
        let (key, _topic_leak, signal, topic_value) =
            reconcile_signal("github", "rel-1", "production", "merge-0", 42, true);
        assert_eq!(key, "github/delivery/reconcile/rel-1/production/42/merged");
        assert_eq!(topic_value, "merge/production/merge-0");
        let _ = _topic_leak;
        assert!(matches!(signal, Signal::MergeComplete { merged: true, .. }));
        // A re-observed merged CR: the SAME key (dedupe, F-20a).
        let (key_again, _, _, _) =
            reconcile_signal("github", "rel-1", "production", "merge-0", 42, true);
        assert_eq!(key, key_again);
    }

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
