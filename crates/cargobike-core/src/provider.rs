//! The provider abstraction.
//!
//! Async, decoupled from DBOS: an in-process trait object resolved by the
//! provider registry ([`crate::registry`]). The neutral term "change
//! request" is used throughout — GitHub PRs, GitLab MRs, and any
//! future provider's concept land under one name.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::RepoRef;

/// Result type of provider operations .
pub type ProviderResult<T> = Result<T, ProviderError>;

/// Errors a provider reports upward.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// The provider does not implement a capability-gated operation .
    #[error("provider does not support this operation")]
    Unsupported,
    /// The provider's backend call failed.
    #[error("failed to call the provider: {0}")]
    Request(String),
    /// The object the caller asked about does not exist.
    #[error("failed to find {0}: not found")]
    NotFound(&'static str),
}

/// Errors of the SSRF-guarded HTTP seam ([`crate::step::HttpService`]).
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// The egress policy refused the host address (the deny-list).
    #[error("blocked by egress policy: {0}")]
    Forbidden(String),
    /// The request exceeded its bound.
    #[error("request timed out")]
    Timeout,
    /// Connection-level failure (the connect, TLS, proxy).
    #[error("connection failed: {0}")]
    Connect(String),
    /// Anything else the client reports.
    #[error("client error: {0}")]
    Other(String),
}

/// Capabilities a provider advertises ; optional operations
/// (`enable_auto_merge`, `create_tag`, `create_release`, webhook
/// verification and parsing) are gated on these.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProviderCaps {
    /// `enable_auto_merge` for opened CRs (the v1.1 `auto_merge` param).
    pub auto_merge: bool,
    /// Tag creation (`builtin/tag@1`, v1.1).
    pub tags: bool,
    /// Release creation (`builtin/release@1`, v1.1).
    pub releases: bool,
    /// Branch-protection checks .
    pub branch_protection: bool,
    /// Tag-protection checks for `require_tag_protection` .
    pub tag_protection: bool,
    /// Branch delete (the cleanup path).
    pub branch_delete: bool,
    /// Signature verification of webhook deliveries .
    pub webhook_verification: bool,
    /// Parsing of webhook deliveries .
    pub webhook_parsing: bool,
}

/// Document format of an edit ( `Edit`); the registry may omit it,
/// in which case the engine infers it from the file extension .
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EditFormat {
    /// YAML document (the format-preserving edits, see).
    #[default]
    Yaml,
    /// JSON document (the pointer-style edits).
    Json,
    /// TOML document (the key paths).
    Toml,
}

/// Why an edit application refused .
#[derive(Debug, thiserror::Error)]
pub enum EditError {
    /// The base document did not parse in the edit's format.
    #[error("the document did not parse: {0}")]
    InvalidDocument(String),
    /// The dot-notation path does not exist in the document.
    #[error("the edit path is invalid: {0}")]
    InvalidPath(String),
}

/// A structured file edit : a dot-notation path into a YAML/JSON
/// document or a key path into TOML — never text substitution.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    /// Repository-relative file; normalised, `..` rejected .
    pub file: String,
    /// Document format, inferred from the extension when omitted .
    pub format: Option<EditFormat>,
    /// Dot-notation (the or key-path) of the field to update (`image.tag`).
    pub field: String,
    /// Value to write; the release version by default .
    pub value: Option<Value>,
}

/// What a successful commit produced (the raw form before CR creation).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitResult {
    /// New commit SHA on the branch.
    pub sha: String,
    /// Branch that was committed to.
    pub branch: String,
}

/// A change request, provider-neutrally .
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRequest {
    /// Provider CR number.
    pub number: u64,
    /// Provider web URL for humans.
    pub url: String,
    /// Head SHA the CR currently carries .
    pub head_sha: String,
    /// State of the CR.
    pub state: crate::model::CrState,
}

/// Branch protection state of a base branch .
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BranchProtection {
    /// Whether the branch requires reviews (a merge is only a review if so).
    pub requires_reviews: bool,
    /// Minimum number of approving reviews, when the provider reports it.
    pub required_review_count: Option<u16>,
}

/// Commit status to set ( `set_commit_status`).
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

/// The provider trait: including webhook verification and
/// parsing in the contract , exposed behind [`ProviderCaps`].
///
/// Sidecars (the v1.0) and WASM modules (the v1.1) implement the same operations
/// over their own transports; the adapter into this trait
/// lives in `cargobike-server`, not here.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Capability discovery .
    fn capabilities(&self) -> ProviderCaps;

    /// Fetches the repository path (`my-org/my-repo`) for an opaque
    /// [`RepoRef`] .
    async fn get_repo_path(&self, repo: &RepoRef) -> ProviderResult<String>;

    /// Creates `branch` from `from_sha`; "already exists at expected SHA"
    /// is success .
    async fn create_branch(
        &self,
        repo: &RepoRef,
        branch: &str,
        from_sha: &str,
    ) -> ProviderResult<()>;

    /// Applies structured edits and commits in one operation (,
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

    /// Opens a change request . Existing open CRs are found by head
    /// branch first ( `change-request` idempotency).
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

    /// Finds an open CR by head branch .
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

    /// Lists open CRs (the reconciliation batching uses provider paging;).
    async fn list_open_change_requests(&self, repo: &RepoRef)
    -> ProviderResult<Vec<ChangeRequest>>;

    /// Reads a file at `ref` (the merge verification reads base files;).
    async fn read_file(&self, repo: &RepoRef, path: &str, git_ref: &str)
    -> ProviderResult<Vec<u8>>;

    /// Comments on a CR (the supersede notices;).
    async fn comment(&self, repo: &RepoRef, cr_number: u64, body: &str) -> ProviderResult<()>;

    /// Adds labels to a CR (`builtin/set-labels@1`;).
    async fn add_labels(
        &self,
        repo: &RepoRef,
        cr_number: u64,
        labels: &[String],
    ) -> ProviderResult<()>;

    /// Merges the base branch into `branch` ( `update_branch`).
    async fn update_branch(&self, repo: &RepoRef, branch: &str) -> ProviderResult<()>;

    /// Deletes a release branch (the cleanup), gated on
    /// [`ProviderCaps::branch_delete`].
    async fn delete_branch(&self, repo: &RepoRef, branch: &str) -> ProviderResult<()> {
        let _ = (repo, branch);
        Err(ProviderError::Unsupported)
    }

    /// The repository's default branch (the branch creation base;).
    async fn default_branch(&self, repo: &RepoRef) -> ProviderResult<String>;

    /// The SHA of a branch tip ( "already exists at expected SHA").
    async fn branch_sha(&self, repo: &RepoRef, branch: &str) -> ProviderResult<String>;

    /// Sets a commit status (`builtin/wait-for-check` draft surface).
    async fn set_commit_status(
        &self,
        repo: &RepoRef,
        sha: &str,
        status: CommitStatus,
    ) -> ProviderResult<()>;

    /// Branch protection check, gated on [`ProviderCaps::branch_protection`]
    /// (; the step fails closed when the capability is absent).
    async fn check_branch_protection(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> ProviderResult<BranchProtection> {
        let _ = (repo, branch);
        Err(ProviderError::Unsupported)
    }

    /// Tag protection check for `require_tag_protection` , gated on
    /// [`ProviderCaps::tag_protection`].
    async fn check_tag_protection(
        &self,
        repo: &RepoRef,
        tag_pattern: &str,
    ) -> ProviderResult<bool> {
        let _ = (repo, tag_pattern);
        Err(ProviderError::Unsupported)
    }

    /// Enables auto merge on a CR (the v1.1 `auto_merge` param), gated on
    /// [`ProviderCaps::auto_merge`].
    async fn enable_auto_merge(&self, repo: &RepoRef, number: u64) -> ProviderResult<()> {
        let _ = (repo, number);
        Err(ProviderError::Unsupported)
    }

    /// Creates a tag (the v1.1 `builtin/tag@1`), gated on [`ProviderCaps::tags`].
    async fn create_tag(&self, repo: &RepoRef, tag: &str, sha: &str) -> ProviderResult<()> {
        let _ = (repo, tag, sha);
        Err(ProviderError::Unsupported)
    }

    /// Creates a release (the v1.1 `builtin/release@1`), gated on
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

    /// Verifies a webhook delivery and normalises the event , gated
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
