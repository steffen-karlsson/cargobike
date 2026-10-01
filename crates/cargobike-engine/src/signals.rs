//! Durable signal system.
//!
//! Topics so different waits cannot receive each other's messages;
//! idempotency keys so redelivered webhooks or a retried reconciler send
//! no duplicate signal; `Forks::Skip` chosen and documented (the
//! spike report docs/spike-dbos.md ).
//!
//! Envelope + errors: the interpreter's durable error type implements
//! the crate's `DurableError` (a blanket impl covers serde + Error types).

/// Topic prefixes (the vocabulary).
pub const MERGE_TOPIC: &str = "merge";
pub const APPROVAL_TOPIC: &str = "approval";
pub const LEASE_TOPIC: &str = "lease";

/// The merge-verification wait topic: `merge/{environment}/{step_id}`.
pub fn merge_topic(environment: &str, step_id: &str) -> String {
    format!("{MERGE_TOPIC}/{environment}/{step_id}")
}

/// The approval-wait topic: `approval/{environment}/{step_id}`.
pub fn approval_topic(environment: &str, step_id: &str) -> String {
    format!("{APPROVAL_TOPIC}/{environment}/{step_id}")
}

/// The queue hand-off topic: `lease/{environment}` .
pub fn lease_topic(environment: &str) -> String {
    format!("{LEASE_TOPIC}/{environment}")
}

/// The idempotency key of a webhook-borne merge signal: one signal per
/// delivery, no matter how many times the workflow's verifier runs .
pub fn merge_signal_key(provider: &str, delivery_id: &str) -> String {
    format!("{provider}/delivery/{delivery_id}")
}

/// The idempotency key of an approval submission (the one submission per
/// `(the release, environment, step, principal)`; a replayed request no-ops).
pub fn approval_signal_key(
    release_id: &str,
    environment: &str,
    step_id: &str,
    principal: &str,
) -> String {
    format!("{release_id}/{environment}/{step_id}/approve/{principal}")
}

/// The idempotency key of a lease hand-off (the queue's wake).
pub fn lease_release_key(application: &str, environment: &str) -> String {
    format!("lease/{application}/{environment}/release")
}

/// The signal payload each wait receives (the typed envelope).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case", tag = "signal")]
pub enum Signal {
    /// The merge signal after webhook correlation or reconciler output.
    MergeComplete {
        /// Whether the CR merged (the false ⇒ closed-without-merge ⇒ failure).
        merged: bool,
        /// The delivery that carries it (the audit trail).
        by_way_of: String,
        /// The CR number observed (the re-verify consults it).
        number: u64,
        /// The CR's head SHA at observation (the on_modified detector,
        /// ); the provider re-verify refreshes it when absent.
        head_sha: Option<String>,
    },
    /// An approval `approve`/`reject` .
    ApprovalSubmitted {
        /// Whether the reviewer approved.
        approved: bool,
        /// The `(the issuer, subject)` pair; distinct-count uses it .
        principal: String,
        /// Display name .
        approver: String,
        /// Optional rejection comment.
        comment: Option<String>,
    },
    /// The lease became free — a queued environment may proceed .
    LeaseReleased {
        /// The application+environment the lease covered.
        resource: String,
    },
}

/// Typed errors — never logged secrets; the vocabulary rides in
/// `Error.code` fields and the API lifts it into problem documents.
#[derive(Debug, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InterpreterError {
    /// A step failed non-transiently ( `StepFailed`).
    #[error("step failed: {0:?}")]
    Step(cargobike_core::step::StepError),
    /// The `wait: merge` consumer timed out (`MergeTimeout`).
    #[error("failed to observe the merge in time")]
    MergeTimeout,
    /// The `wait: approval` waiter timed out (`ApprovalTimeout`).
    #[error("failed to receive an approval in time")]
    ApprovalTimeout,
    /// CR closed without merge (`ApprovalRejected`, terminal; ).
    #[error("the change request was closed without merging")]
    ApprovalRejected,
    /// CR head changed under `on_modified: fail` (`ChangeRequestModified`).
    #[error("the change request was modified beyond its head declaration")]
    ChangeRequestModified,
    /// Concurrency refused or lower version blocked .
    #[error("concurrency denied")]
    ConcurrencyRejected,
    /// The version failed verification .
    #[error("the version failed verification")]
    VersionNotVerified,
    /// A cancel signal reached the interpreter (the semantics; the
    /// environment/release become `Canceled` — terminal in the model).
    #[error("cancellation propagated")]
    Cancelled,
}

impl InterpreterError {
    /// The code of each failure (the vocabulary family).
    pub const fn code(&self) -> &'static str {
        match self {
            InterpreterError::Step(_) => cargobike_core::error::STEP_FAILED,
            InterpreterError::MergeTimeout => cargobike_core::error::MERGE_TIMEOUT,
            InterpreterError::ApprovalTimeout => cargobike_core::error::APPROVAL_TIMEOUT,
            InterpreterError::ApprovalRejected => cargobike_core::error::APPROVAL_REJECTED,
            InterpreterError::ChangeRequestModified => {
                cargobike_core::error::CHANGE_REQUEST_MODIFIED
            }
            InterpreterError::ConcurrencyRejected => cargobike_core::error::CONCURRENCY_REJECTED,
            InterpreterError::VersionNotVerified => cargobike_core::error::VERSION_NOT_VERIFIED,
            InterpreterError::Cancelled => cargobike_core::error::STEP_FAILED,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_topics_are_scoped_so_waits_cannot_confuse_sends_f20a() {
        assert_eq!(
            merge_topic("production", "merge-0"),
            "merge/production/merge-0"
        );
        assert_eq!(
            approval_topic("production", "approval-0"),
            "approval/production/approval-0"
        );
        assert_eq!(lease_topic("preview"), "lease/preview");
        // wait for merge in the preview never gets production's messages.
        assert_ne!(merge_topic("preview", "x"), merge_topic("production", "x"));
    }

    #[test]
    fn test_signal_keys_derive_per_source_event_f20a() {
        assert_eq!(
            merge_signal_key("github", "delivery-1"),
            "github/delivery/delivery-1"
        );
        assert_eq!(
            approval_signal_key("rel-1", "production", "approval-0", "iss/sub"),
            "rel-1/production/approval-0/approve/iss/sub"
        );
        assert_eq!(
            lease_release_key("my-service", "preview"),
            "lease/my-service/preview/release"
        );
    }

    #[test]
    fn test_signal_envelope_roundtrips() {
        let signal = Signal::ApprovalSubmitted {
            approved: true,
            principal: "iss/sub".to_owned(),
            approver: "Alice".to_owned(),
            comment: Some("ok".to_owned()),
        };
        let json = serde_json::to_value(&signal).expect("serialises");
        assert_eq!(json["signal"], "approval-submitted");
        let back: Signal = serde_json::from_value(json).expect("deserialises");
        assert_eq!(back, signal);
    }

    #[test]
    fn test_interpreter_errors_carry_the_f8_vocabulary() {
        let failure = InterpreterError::Step(cargobike_core::step::StepError::Failed {
            code: cargobike_core::error::STEP_FAILED.to_owned(),
            message: "x".to_owned(),
        });
        assert_eq!(failure.code(), "StepFailed");
        assert_eq!(InterpreterError::MergeTimeout.code(), "MergeTimeout");
        assert_eq!(InterpreterError::ApprovalTimeout.code(), "ApprovalTimeout");
        assert_eq!(
            InterpreterError::ApprovalRejected.code(),
            "ApprovalRejected"
        );
        assert_eq!(
            InterpreterError::ChangeRequestModified.code(),
            "ChangeRequestModified"
        );
        assert_eq!(
            InterpreterError::ConcurrencyRejected.code(),
            "ConcurrencyRejected"
        );
        assert_eq!(
            InterpreterError::VersionNotVerified.code(),
            "VersionNotVerified"
        );
    }
}
