//! The release model (PRD §5): the only root-level properties are
//! `metadata`, `spec`, and `status` (F-1). Repositories are opaque
//! [`RepoRef`]s (F-10); phases roll up on the server (F-6, F-7).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// A release: metadata, spec, status — the only root-level properties (F-1).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Release {
    /// Bookkeeping: IDs, timestamps, optimistic-concurrency version.
    pub metadata: ReleaseMetadata,
    /// What to release: application, version, resolved source and template.
    pub spec: ReleaseSpec,
    /// How it is going: phase rollup, per-environment status, error.
    pub status: ReleaseStatus,
}

/// Release bookkeeping (PRD §5.1).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleaseMetadata {
    /// UUIDv7 — time-ordered and sortable (F-2).
    pub id: Uuid,
    /// When the release was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When the release row was last updated.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// Optimistic concurrency, honoured via `If-Match` (F-9).
    pub resource_version: u64,
    /// ID of the release this one was retried from, if any.
    pub retried_from: Option<Uuid>,
    /// Free-form labels for filtering (F-142).
    pub labels: BTreeMap<String, String>,
    /// Untrusted caller-supplied values (F-142).
    pub annotations: BTreeMap<String, String>,
}

/// One execution attempt of a release workflow (PRD §5.1).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attempt {
    /// DBOS workflow ID of the attempt.
    pub workflow_id: String,
    /// When the attempt started.
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    /// Step ID the attempt forked from (`"last_failure"` or a step ID).
    pub fork_from: Option<String>,
}

/// What to release (PRD §5.2). The registry, not the caller, resolves
/// source and template (F-3, F-5, F-11).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleaseSpec {
    /// Application name in the registry.
    pub application: String,
    /// Opaque version string, validated by the application's
    /// [`crate::version::VersionScheme`] (F-4).
    pub version: String,
    /// Where the version came from, resolved from the registry (F-11).
    pub source: RepoRef,
    /// Registry-only template reference, `name@version` (F-5).
    pub template: String,
}

/// How the release is going (PRD §5.3).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleaseStatus {
    /// Physical release phase (F-6).
    pub phase: Phase,
    /// Who started the release (F-83).
    pub actor: Actor,
    /// Generic CI context, `Some` when started from CI (F-83).
    pub ci: Option<CiContext>,
    /// Interpreter workflow identity and pinned versions (F-37).
    pub workflow: WorkflowInfo,
    /// Error detail when the release has failed (F-8).
    pub error: Option<crate::error::ReleaseError>,
    /// Per-environment status array (F-7).
    pub environments: Vec<EnvironmentStatus>,
    /// Attempt records with workflow IDs and fork points (F-9).
    pub attempts: Vec<Attempt>,
}

/// Release phase (F-6).
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::Display,
    strum::EnumIter,
)]
#[strum(serialize_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum Phase {
    /// Initial state before the first environment starts.
    Pending,
    /// At least one environment is running.
    Running,
    /// At least one environment is `PendingApproval` and none failed.
    PendingApproval,
    /// Any environment failed.
    Failed,
    /// Cancellation was requested and honoured.
    Canceled,
    /// A newer release took over.
    Superseded,
    /// Every environment is `Completed` or `Skipped`.
    Completed,
}

impl Phase {
    /// Terminal phases allow `release delete` (US-6, F-74).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Phase::Failed | Phase::Canceled | Phase::Superseded | Phase::Completed
        )
    }
}

/// Who started the release: `(issuer, subject)` plus a display name (F-83).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Actor {
    /// OIDC issuer the auth token came from.
    pub issuer: String,
    /// Subject from the token or the API key name.
    pub subject: String,
    /// Presentable name (GitHub Actions: repository). Token lacks email by design.
    pub display_name: String,
}

/// Generic CI context (PRD §5.3) — verified values only. A generic type,
/// never GitHub-specific; both are token claims (F-83).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CiContext {
    /// provider of the CI system.
    pub provider: String,
    /// repository the run executed in.
    pub repository: String,
    /// repository ID (immutable) the run executed in.
    pub repository_id: String,
    /// workflow ref of the run.
    pub workflow_ref: String,
    /// run ID supplied by the CI system.
    pub run_id: String,
    /// URL to the run.
    pub run_url: String,
}

/// Interpreter workflow identity and pinned step-type versions (F-16, F-37).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowInfo {
    /// Interpreter workflow name, always `"cargobike.interpret.v1"`.
    pub id: String,
    /// Template the interpreter reads (registry-only, F-5).
    pub template_name: String,
    /// Template version, snapshotted (F-25).
    pub template_version: String,
    /// Content hash over template + inputs (F-16).
    pub template_hash: String,
    /// Built-in and sidecar step type versions pinned in the snapshot.
    pub step_type_versions: BTreeMap<String, String>,
}

/// Environment phase (F-7).
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::Display,
    strum::EnumIter,
)]
#[strum(serialize_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum EnvironmentPhase {
    /// Environment has not started.
    Pending,
    /// Steps are running.
    Running,
    /// A `wait: approval` step blocks.
    PendingApproval,
    /// Gate false, or no changes needed.
    Skipped,
    /// Held by the concurrency policy.
    Waiting,
    /// Steps completed.
    Completed,
    /// Steps failed.
    Failed,
    /// Cancellation reached this environment.
    Canceled,
    /// A newer release superseded this one here.
    Superseded,
}

impl EnvironmentPhase {
    /// Whether the environment has concluded (per F-7).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            EnvironmentPhase::Skipped
                | EnvironmentPhase::Completed
                | EnvironmentPhase::Failed
                | EnvironmentPhase::Canceled
                | EnvironmentPhase::Superseded
        )
    }
}

/// Status of one environment (F-7).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvironmentStatus {
    /// Environment name (registry key; lowercase shown here).
    pub name: String,
    /// Environment phase.
    pub phase: EnvironmentPhase,
    /// Change request opened, if any.
    pub change_request: Option<ChangeRequestRef>,
    /// When the first step started.
    pub started_at: Option<OffsetDateTime>,
    /// When the environment became terminal.
    pub completed_at: Option<OffsetDateTime>,
}

/// A change request opened for a release (PRD §5.3), kept for verification
/// and webhook correlation (F-63).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangeRequestRef {
    /// Provider CR number.
    pub number: u64,
    /// Provider web URL.
    pub url: String,
    /// Target repo (opaque) used for verification and correlation.
    pub target_repo: crate::model::RepoRef,
    /// Head SHA Cargobike committed (F-62).
    pub head_sha: String,
    /// Current CR state.
    pub state: CrState,
}

/// State of a change request (PRD §5.3).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, strum::EnumString, strum::Display,
)]
#[strum(serialize_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum CrState {
    /// Open, awaiting merge.
    Open,
    /// Closed without merging (⇒ `ApprovalRejected`, F-62).
    Closed,
    /// Merged into the base branch (F-62).
    Merged,
}

/// An opaque repository reference (F-10).
///
/// `id` is the provider's immutable ID, a string used for authorization.
/// `path` may be declared as a verified label: the server checks it at
/// startup, marks the application unavailable on mismatch, and retries in
/// the background; the strict fail-on-error check is
/// `cargobike validate --resolve`. Otherwise `path` is fetched at runtime
/// via `get_repo_path`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRef {
    /// Provider name, resolved against the provider registry (F-39).
    pub provider: String,
    /// Immutable provider ID (GitHub repository ID) as a string (R6).
    pub id: String,
    /// Optional verified label, otherwise fetched at runtime (F-10).
    pub path: Option<String>,
}

impl RepoRef {
    /// A reference without a declared path.
    pub fn new(provider: &str, id: &str) -> Self {
        Self {
            provider: provider.to_owned(),
            id: id.to_owned(),
            path: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;
    use std::cmp::Ordering;

    #[rstest]
    #[case::pending(Phase::Pending, false)]
    #[case::running(Phase::Running, false)]
    #[case::pending_approval(Phase::PendingApproval, false)]
    #[case::failed(Phase::Failed, true)]
    #[case::canceled(Phase::Canceled, true)]
    #[case::superseded(Phase::Superseded, true)]
    #[case::completed(Phase::Completed, true)]
    fn test_phase_terminality_matches_f74(#[case] phase: Phase, #[case] terminal: bool) {
        assert_eq!(phase.is_terminal(), terminal);
    }

    #[rstest]
    #[case::pending(EnvironmentPhase::Pending, false)]
    #[case::running(EnvironmentPhase::Running, false)]
    #[case::pending_approval(EnvironmentPhase::PendingApproval, false)]
    #[case::waiting(EnvironmentPhase::Waiting, false)]
    #[case::skipped(EnvironmentPhase::Skipped, true)]
    #[case::completed(EnvironmentPhase::Completed, true)]
    #[case::failed(EnvironmentPhase::Failed, true)]
    #[case::canceled(EnvironmentPhase::Canceled, true)]
    #[case::superseded(EnvironmentPhase::Superseded, true)]
    fn test_environment_phase_terminality_matches_f7(
        #[case] phase: EnvironmentPhase,
        #[case] terminal: bool,
    ) {
        assert_eq!(phase.is_terminal(), terminal);
    }

    #[test]
    fn test_release_roundtrips_through_serde_json() {
        let release = sample_release();
        let json = serde_json::to_vec(&release).expect("release must serialise");
        let back: Release = serde_json::from_slice(&json).expect("release must deserialise");
        assert_eq!(back.metadata.id, release.metadata.id);
        assert_eq!(back.spec.application, release.spec.application);
        assert_eq!(back.spec.source, release.spec.source);
        assert_eq!(back.status.phase, release.status.phase);
    }

    #[test]
    fn test_uuidv7_ids_sort_by_creation_time() {
        let earlier = Uuid::now_v7();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let later = Uuid::now_v7();
        assert_eq!(earlier.cmp(&later), Ordering::Less);
    }

    #[test]
    fn test_reporef_ids_are_strings_and_path_optional() {
        let repo = RepoRef::new("github", "789012");
        assert!(repo.path.is_none());
        let json = serde_json::to_value(&repo).expect("reporef must serialise");
        assert_eq!(json["provider"], "github");
        assert_eq!(json["id"], "789012");
        assert_eq!(json["path"], serde_json::Value::Null);
        let back: RepoRef = serde_json::from_value(json).expect("reporef must deserialise");
        assert_eq!(back, repo);
    }

    fn sample_release() -> Release {
        let id = Uuid::now_v7();
        let now = OffsetDateTime::now_utc();
        Release {
            metadata: ReleaseMetadata {
                id,
                created_at: now,
                updated_at: now,
                resource_version: 1,
                retried_from: None,
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
            },
            spec: ReleaseSpec {
                application: "my-service".to_owned(),
                version: "1.2.3".to_owned(),
                source: RepoRef::new("github", "123456"),
                template: "service@1".to_owned(),
            },
            status: ReleaseStatus {
                phase: Phase::Pending,
                actor: Actor {
                    issuer: "https://token.actions.githubusercontent.com".to_owned(),
                    subject: "repo:my-org/my-service".to_owned(),
                    display_name: "my-org/my-service".to_owned(),
                },
                ci: None,
                workflow: WorkflowInfo {
                    id: "cargobike.interpret.v1".to_owned(),
                    template_name: "service".to_owned(),
                    template_version: "1".to_owned(),
                    template_hash: "sha256:abc".to_owned(),
                    step_type_versions: BTreeMap::new(),
                },
                error: None,
                environments: Vec::new(),
                attempts: Vec::new(),
            },
        }
    }
}
