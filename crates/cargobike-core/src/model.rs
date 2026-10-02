//! The release model: the only root-level properties are
//! `metadata`, `spec`, and `status` . Repositories are opaque
//! [`RepoRef`]s ; phases roll up on the server .

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// A release: metadata, spec, status — the only root-level properties .
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Release {
    /// Bookkeeping: IDs, timestamps, optimistic-concurrency version.
    pub metadata: ReleaseMetadata,
    /// What to release: application, version, resolved source and template.
    pub spec: ReleaseSpec,
    /// How it is going: phase rollup, per-environment status, error.
    pub status: ReleaseStatus,
}

/// Release bookkeeping.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleaseMetadata {
    /// UUIDv7 — time-ordered and sortable .
    pub id: Uuid,
    /// When the release was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When the release row was last updated.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// Optimistic concurrency, honoured via `If-Match` .
    pub resource_version: u64,
    /// ID of the release this one was retried from, if any.
    pub retried_from: Option<Uuid>,
    /// Free-form labels for filtering .
    pub labels: BTreeMap<String, String>,
    /// Untrusted caller-supplied values .
    pub annotations: BTreeMap<String, String>,
}

/// One execution attempt of a release workflow.
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

/// What to release. The registry, not the caller, resolves
/// source and template .
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleaseSpec {
    /// Application name in the registry.
    pub application: String,
    /// Opaque version string, validated by the application's
    /// [`crate::version::VersionScheme`] .
    pub version: String,
    /// Where the version came from, resolved from the registry .
    pub source: RepoRef,
    /// Registry-only template reference, `name@version` .
    pub template: String,
}

/// How the release is going.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleaseStatus {
    /// Physical release phase .
    pub phase: Phase,
    /// Who started the release .
    pub actor: Actor,
    /// Generic CI context, `Some` when started from CI .
    pub ci: Option<CiContext>,
    /// Interpreter workflow identity and pinned versions .
    pub workflow: WorkflowInfo,
    /// Error detail when the release has failed .
    pub error: Option<crate::error::ReleaseError>,
    /// Per-environment status array .
    pub environments: Vec<EnvironmentStatus>,
    /// Attempt records with workflow IDs and fork points .
    pub attempts: Vec<Attempt>,
}

/// Release phase .
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
    /// Terminal phases allow `release delete` .
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Phase::Failed | Phase::Canceled | Phase::Superseded | Phase::Completed
        )
    }
}

/// Who started the release: `(the issuer, subject)` plus a display name .
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Actor {
    /// OIDC issuer the auth token came from.
    pub issuer: String,
    /// Subject from the token or the API key name.
    pub subject: String,
    /// Presentable name (GitHub Actions: repository). Token lacks email by design.
    pub display_name: String,
}

/// Generic CI context — verified values only. A generic type,
/// never GitHub-specific; both are token claims .
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CiContext {
    /// provider of the CI system.
    pub provider: String,
    /// repository the run executed in.
    pub repository: String,
    /// repository ID (the immutable) the run executed in.
    pub repository_id: String,
    /// workflow ref of the run.
    pub workflow_ref: String,
    /// run ID supplied by the CI system.
    pub run_id: String,
    /// URL to the run.
    pub run_url: String,
}

/// Interpreter workflow identity and pinned step-type versions .
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowInfo {
    /// Interpreter workflow name, always `"cargobike.interpret.v1"`.
    pub id: String,
    /// Template the interpreter reads (the registry-only).
    pub template_name: String,
    /// Template version, snapshotted .
    pub template_version: String,
    /// Content hash over template + inputs .
    pub template_hash: String,
    /// Built-in and sidecar step type versions pinned in the snapshot.
    pub step_type_versions: BTreeMap<String, String>,
}

/// Environment phase .
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
    /// Whether the environment has concluded (the per).
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

/// Status of one environment .
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvironmentStatus {
    /// Environment name (the registry key; lowercase shown here).
    pub name: String,
    /// Environment phase.
    pub phase: EnvironmentPhase,
    /// Change request opened, if any.
    pub change_request: Option<ChangeRequestRef>,
    /// When the first step started.
    #[serde(
        with = "time::serde::rfc3339::option",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub started_at: Option<OffsetDateTime>,
    /// When the environment became terminal.
    #[serde(
        with = "time::serde::rfc3339::option",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub completed_at: Option<OffsetDateTime>,
}

/// A change request opened for a release, kept for verification
/// and webhook correlation .
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangeRequestRef {
    /// Provider CR number.
    pub number: u64,
    /// Provider web URL.
    pub url: String,
    /// Target repo (the opaque) used for verification and correlation.
    pub target_repo: crate::model::RepoRef,
    /// Head SHA Cargobike committed .
    pub head_sha: String,
    /// Current CR state.
    pub state: CrState,
}

/// State of a change request.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, strum::EnumString, strum::Display,
)]
#[strum(serialize_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum CrState {
    /// Open, awaiting merge.
    Open,
    /// Closed without merging (⇒ `ApprovalRejected`).
    Closed,
    /// Merged into the base branch .
    Merged,
}

/// An opaque repository reference .
///
/// `id` is the provider's immutable ID, a string used for authorization.
/// `path` may be declared as a verified label: the server checks it at
/// startup, marks the application unavailable on mismatch, and retries in
/// the background; the strict fail-on-error check is
/// `cargobike validate --resolve`. Otherwise `path` is fetched at runtime
/// via `get_repo_path`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRef {
    /// Provider name, resolved against the provider registry .
    pub provider: String,
    /// Immutable provider ID (GitHub repository ID) as a string .
    pub id: String,
    /// Optional verified label, otherwise fetched at runtime .
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
    fn test_phase_terminality_matches(#[case] phase: Phase, #[case] terminal: bool) {
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
    fn test_environment_phase_terminality_matches(
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
