//! The provider abstraction (PRD §7.1, F-43..F-48).
//!
//! Async, decoupled from DBOS: an in-process trait object resolved by the
//! provider registry ([`crate::registry`]). The neutral term "change
//! request" is used throughout (F-48) — GitHub PRs, GitLab MRs, and any
//! future provider's concept land under one name.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::RepoRef;

/// Result type of provider operations (§10.1).
pub type ProviderResult<T> = Result<T, ProviderError>;

/// Errors a provider reports upward.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// The provider does not implement a capability-gated operation (F-47).
    #[error("provider does not support this operation")]
    Unsupported,
    /// The provider's backend call failed.
    #[error("failed to call the provider: {0}")]
    Request(String),
    /// The object the caller asked about does not exist.
    #[error("failed to find {0}: not found")]
    NotFound(&'static str),
}

/// Errors of the SSRF-guarded HTTP seam ([`crate::step::HttpService`], F-117).
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// The egress policy refused the host address (F-117 deny-list).
    #[error("blocked by egress policy: {0}")]
    Forbidden(String),
    /// The request exceeded its bound.
    #[error("request timed out")]
    Timeout,
    /// Connection-level failure (connect, TLS, proxy).
    #[error("connection failed: {0}")]
    Connect(String),
    /// Anything else the client reports.
    #[error("client error: {0}")]
    Other(String),
}

/// Capabilities a provider advertises (F-47); optional operations
/// (`enable_auto_merge`, `create_tag`, `create_release`, webhook
/// verification and parsing) are gated on these.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProviderCaps {
    /// `enable_auto_merge` for opened CRs (v1.1 `auto_merge` param).
    pub auto_merge: bool,
    /// Tag creation (`builtin/tag@1`, v1.1).
    pub tags: bool,
    /// Release creation (`builtin/release@1`, v1.1).
    pub releases: bool,
    /// Branch-protection checks (F-65).
    pub branch_protection: bool,
    /// Tag-protection checks for `require_tag_protection` (F-82).
    pub tag_protection: bool,
    /// Branch delete (F-75's cleanup path).
    pub branch_delete: bool,
    /// Signature verification of webhook deliveries (F-51).
    pub webhook_verification: bool,
    /// Parsing of webhook deliveries (F-46).
    pub webhook_parsing: bool,
}

/// Document format of an edit (F-44's `Edit`); the registry may omit it,
/// in which case the engine infers it from the file extension (F-41).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EditFormat {
    /// YAML document (format-preserving edits, see A.8).
    #[default]
    Yaml,
    /// JSON document (pointer-style edits).
    Json,
    /// TOML document (key paths).
    Toml,
}

/// A structured file edit (F-41): a dot-notation path into a YAML/JSON
/// document or a key path into TOML — never text substitution.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    /// Repository-relative file; normalised, `..` rejected (F-41).
    pub file: String,
    /// Document format, inferred from the extension when omitted (F-41).
    pub format: Option<EditFormat>,
    /// Dot-notation (or key-path) of the field to update (`image.tag`).
    pub field: String,
    /// Value to write; the release version by default (F-41, F-147).
    pub value: Option<Value>,
}

/// What a successful commit produced (F-52's raw form before CR creation).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitResult {
    /// New commit SHA on the branch.
    pub sha: String,
    /// Branch that was committed to.
    pub branch: String,
}

/// A change request, provider-neutrally (F-48).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRequest {
    /// Provider CR number.
    pub number: u64,
    /// Provider web URL for humans.
    pub url: String,
    /// Head SHA the CR currently carries (F-62).
    pub head_sha: String,
    /// State of the CR.
    pub state: crate::model::CrState,
}

/// Branch protection state of a base branch (F-65).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BranchProtection {
    /// Whether the branch requires reviews (a merge is only a review if so).
    pub requires_reviews: bool,
    /// Minimum number of approving reviews, when the provider reports it.
    pub required_review_count: Option<u16>,
}

/// Commit status to set (F-44's `set_commit_status`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CommitStatus {
    /// Post a status that maps to CI pending.
    Pending,
    /// Post a status that maps to CI success.
    Success,
    /// Post a status that maps to CI failure.
    Failure,
}

/// The provider trait (PRD §7.1): including webhook verification and
/// parsing in the contract (F-46), exposed behind [`ProviderCaps`].
///
/// Sidecars (v1.0) and WASM modules (v1.1) implement the same operations
/// over their own transports (PRD §7.3, §7.4); the adapter into this trait
/// lives in `cargobike-server`, not here.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Capability discovery (F-47).
    fn capabilities(&self) -> ProviderCaps;

    /// Fetches the repository path (`my-org/my-repo`) for an opaque
    /// [`RepoRef`] (F-10).
    async fn get_repo_path(&self, repo: &RepoRef) -> ProviderResult<String>;

    /// Creates `branch` from `from_sha`; "already exists at expected SHA"
    /// is success (A1).
    async fn create_branch(
        &self,
        repo: &RepoRef,
        branch: &str,
        from_sha: &str,
    ) -> ProviderResult<()>;

    /// Applies structured edits and commits in one operation (F-41, A1
    /// fused edit+commit). `expected_parent` is the pre-commit SHA: a
    /// mismatch is a conflict rather than a silent overwrite.
    async fn commit_files(
        &self,
        repo: &RepoRef,
        branch: &str,
        edits: &[Edit],
        message: &str,
        expected_parent: Option<&str>,
    ) -> ProviderResult<CommitResult>;

    /// Opens a change request (F-48). Existing open CRs are found by head
    /// branch first (A1 `change-request` idempotency).
    async fn create_change_request(
        &self,
        repo: &RepoRef,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> ProviderResult<ChangeRequest>;

    /// Gets a CR by number.
    async fn get_change_request(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> ProviderResult<ChangeRequest>;

    /// Finds an open CR by head branch (A1).
    async fn find_change_request_by_head(
        &self,
        repo: &RepoRef,
        head: &str,
    ) -> ProviderResult<Option<ChangeRequest>>;

    /// Closes a CR, posting `comment`.
    async fn close_change_request(
        &self,
        repo: &RepoRef,
        number: u64,
        comment: &str,
    ) -> ProviderResult<()>;

    /// Lists open CRs (reconciliation batching uses provider paging; F-69).
    async fn list_open_change_requests(&self, repo: &RepoRef)
    -> ProviderResult<Vec<ChangeRequest>>;

    /// Reads a file at `ref` (merge verification reads base files; F-62).
    async fn read_file(&self, repo: &RepoRef, path: &str, git_ref: &str)
    -> ProviderResult<Vec<u8>>;

    /// Comments on a CR (supersede notices; F-73).
    async fn comment(&self, repo: &RepoRef, cr_number: u64, body: &str) -> ProviderResult<()>;

    /// Adds labels to a CR (`builtin/set-labels@1`; F-35).
    async fn add_labels(
        &self,
        repo: &RepoRef,
        cr_number: u64,
        labels: &[String],
    ) -> ProviderResult<()>;

    /// Merges the base branch into `branch` (F-46 `update_branch`).
    async fn update_branch(&self, repo: &RepoRef, branch: &str) -> ProviderResult<()>;

    /// Deletes a release branch (F-75's cleanup), gated on
    /// [`ProviderCaps::branch_delete`].
    async fn delete_branch(&self, repo: &RepoRef, branch: &str) -> ProviderResult<()> {
        let _ = (repo, branch);
        Err(ProviderError::Unsupported)
    }

    /// The repository's default branch (branch creation base; A1).
    async fn default_branch(&self, repo: &RepoRef) -> ProviderResult<String>;

    /// The SHA of a branch tip (A1 "already exists at expected SHA").
    async fn branch_sha(&self, repo: &RepoRef, branch: &str) -> ProviderResult<String>;

    /// Sets a commit status (`builtin/wait-for-check` draft surface).
    async fn set_commit_status(
        &self,
        repo: &RepoRef,
        sha: &str,
        status: CommitStatus,
    ) -> ProviderResult<()>;

    /// Branch protection check, gated on [`ProviderCaps::branch_protection`]
    /// (F-65; the step fails closed when the capability is absent).
    async fn check_branch_protection(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> ProviderResult<BranchProtection> {
        let _ = (repo, branch);
        Err(ProviderError::Unsupported)
    }

    /// Tag protection check for `require_tag_protection` (F-82), gated on
    /// [`ProviderCaps::tag_protection`].
    async fn check_tag_protection(
        &self,
        repo: &RepoRef,
        tag_pattern: &str,
    ) -> ProviderResult<bool> {
        let _ = (repo, tag_pattern);
        Err(ProviderError::Unsupported)
    }

    /// Enables auto merge on a CR (v1.1 `auto_merge` param), gated on
    /// [`ProviderCaps::auto_merge`].
    async fn enable_auto_merge(&self, repo: &RepoRef, number: u64) -> ProviderResult<()> {
        let _ = (repo, number);
        Err(ProviderError::Unsupported)
    }

    /// Creates a tag (v1.1 `builtin/tag@1`), gated on [`ProviderCaps::tags`].
    async fn create_tag(&self, repo: &RepoRef, tag: &str, sha: &str) -> ProviderResult<()> {
        let _ = (repo, tag, sha);
        Err(ProviderError::Unsupported)
    }

    /// Creates a release (v1.1 `builtin/release@1`), gated on
    /// [`ProviderCaps::releases`].
    async fn create_release(
        &self,
        repo: &RepoRef,
        tag: &str,
        name: &str,
        body: &str,
    ) -> ProviderResult<()> {
        let _ = (repo, tag, name, body);
        Err(ProviderError::Unsupported)
    }

    /// Verifies a webhook delivery and normalises the event (F-51), gated
    /// on [`ProviderCaps::webhook_verification`].
    async fn verify_webhook(
        &self,
        headers: &[(&str, &str)],
        body: &[u8],
        secrets: &[secrecy::SecretString],
    ) -> ProviderResult<crate::webhook::NormalisedEvent> {
        let _ = (headers, body, secrets);
        Err(ProviderError::Unsupported)
    }
}
