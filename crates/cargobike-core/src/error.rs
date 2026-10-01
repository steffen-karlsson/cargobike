//! Release error codes and the typed library error style .
//!
//! Error codes are stable string contract values; the API surfaces them as
//! the `code` extension member of RFC 9457 problem details . Define
//! them once here — coded magic strings are a constant violation.

/// `status.error.code`: a step body failed.
pub const STEP_FAILED: &str = "StepFailed";
/// `status.error.code`: a `wait: approval` step timed out (`on_timeout: fail`).
pub const APPROVAL_TIMEOUT: &str = "ApprovalTimeout";
/// `status.error.code`: a `wait: merge` step timed out (`on_timeout: fail`).
pub const MERGE_TIMEOUT: &str = "MergeTimeout";
/// `status.error.code`: the CR was closed without merging . Terminal.
pub const APPROVAL_REJECTED: &str = "ApprovalRejected";
/// `status.error.code`: the version did not verify against tag/tok SHA .
pub const VERSION_NOT_VERIFIED: &str = "VersionNotVerified";
/// `status.error.code`: the concurrency policy refused or blocked .
pub const CONCURRENCY_REJECTED: &str = "ConcurrencyRejected";
/// `status.error.code`: CR head SHA changed under `on_modified: fail` .
pub const CHANGE_REQUEST_MODIFIED: &str = "ChangeRequestModified";

/// Structured error of a failed release .
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReleaseError {
    /// One of the code constants in this module.
    pub code: String,
    /// Human-readable failure detail.
    pub message: String,
}

impl ReleaseError {
    /// An error with a code constant from this module and a message.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }
}

/// Typed internal error used by this crate (`thiserror`).
/// Message style: `failed to <action>: <cause>`.
#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    /// A required value was absent where it is structurally required.
    #[error("failed to build release: missing {0}")]
    Malformed(&'static str),
    /// No provider is registered under the referenced name .
    #[error("failed to resolve provider: no provider named `{0}`")]
    UnknownProvider(String),
    /// Secret-name resolution failed .
    #[error("failed to resolve secret: no secret named `{0}`")]
    UnknownSecret(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_error_codes_match_the_f8_vocabulary() {
        // spells the vocabulary; adds MergeTimeout for wait timeouts.
        assert_eq!(STEP_FAILED, "StepFailed");
        assert_eq!(APPROVAL_TIMEOUT, "ApprovalTimeout");
        assert_eq!(MERGE_TIMEOUT, "MergeTimeout");
        assert_eq!(APPROVAL_REJECTED, "ApprovalRejected");
        assert_eq!(VERSION_NOT_VERIFIED, "VersionNotVerified");
        assert_eq!(CONCURRENCY_REJECTED, "ConcurrencyRejected");
        assert_eq!(CHANGE_REQUEST_MODIFIED, "ChangeRequestModified");
    }

    #[test]
    fn test_release_error_carries_code_and_message() {
        let error = ReleaseError::new(STEP_FAILED, "commit rejected");
        assert_eq!(error.code, "StepFailed");
        assert_eq!(error.message, "commit rejected");
    }
}
