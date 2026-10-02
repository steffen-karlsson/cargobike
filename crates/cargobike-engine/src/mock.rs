//! The harness's mock provider (the feature crash-hooks only): state
//! lives in ONE JSON file that the sequential harness processes share,
//! so the side-effect counts are observable across the kill.
//!
//! The mock behaves like a small git host: files are text keyed
//! `{branch}/{path}`, commits apply structured edits through the crate's
//! edit applier (the real provider's path too), and change requests are
//! stateful (`open`/`merged`/`closed`): a merged CR lands the head
//! branch's files on the base branch, so merge verification runs
//! end-to-end.
//!
//! (Test-only support code: it panics on impossible states and reports
//! on stderr, so those lints stay off.)

#![allow(clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;

use cargobike_core::error::LibraryError;
use cargobike_core::model::CrState;
use cargobike_core::model::RepoRef;
use cargobike_core::provider::ChangeRequest as CoreChangeRequest;
use cargobike_core::provider::CommitResult;
use cargobike_core::provider::CommitStatus;
use cargobike_core::provider::Edit;
use cargobike_core::provider::Provider;
use cargobike_core::provider::ProviderCaps;
use cargobike_core::provider::ProviderError;
use cargobike_core::provider::ProviderResult;
use cargobike_core::step::HttpRequest;
use cargobike_core::step::HttpResponse;
use cargobike_core::step::HttpService as HttpServiceTrait;
use secrecy::SecretString;

/// The base branch every repository has; merges land here.
pub const BASE_BRANCH: &str = "main";
/// The file path the seed manifest lives at (the release edit targets it).
pub const THE_SEED_PATH: &str = "apps/stage/manifest.yaml";
/// The seed manifest the mock's base branch starts with.
pub const DEFAULT_SEED_MANIFEST: &str = "image:\n  tag: 0.0.0\n";

/// What the state file declares (the convergence facts).
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MockState {
    /// Branch creations (the one per release).
    pub branch_creates: u64,
    /// Fused edit+commit runs that CHANGED content (the recovery
    /// replay must be a no-op, so the steady state holds one).
    pub commit_runs: u64,
    /// The change requests, keyed by head branch.
    pub change_requests: BTreeMap<String, CrSummary>,
    /// Branch SHAs per name.
    pub branches: BTreeMap<String, String>,
    /// File texts: `{branch}/{path}` -> serialized document.
    pub files: BTreeMap<String, String>,
}

/// A change request compact enough for the state file.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CrSummary {
    pub number: u64,
    pub url: String,
    pub head_sha: String,
    pub state: String,
}

/// The `state` text to the core value (the mock's single mapping).
fn cr_state_of(text: &str) -> CrState {
    match text {
        "merged" => CrState::Merged,
        "closed" => CrState::Closed,
        _ => CrState::Open,
    }
}

/// The core shape from a stored summary (state honoured, or the
/// merged/close flows could never finish).
fn core_request_of_summary(summary: &CrSummary) -> CoreChangeRequest {
    CoreChangeRequest {
        number: summary.number,
        url: summary.url.clone(),
        head_sha: summary.head_sha.clone(),
        state: cr_state_of(&summary.state),
    }
}

/// The state-map key for a file on the base branch.
pub fn base_key(path: &str) -> String {
    format!("{BASE_BRANCH}/{path}")
}

/// The state-map key for a file on a branch.
fn branch_key(branch: &str, path: &str) -> String {
    format!("{branch}/{path}")
}

impl MockState {
    /// Loads the state file, defaulting to empty.
    pub fn load(path: &PathBuf) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Writes the state file atomically (temp + rename): a killed
    /// process mid-write must never leave a torn file that reads back
    /// as empty defaults.
    pub fn save(&self, path: &PathBuf) {
        let contents = serde_json::to_string_pretty(self).expect("state serialises");
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, contents).expect("state temp write");
        std::fs::rename(&temporary, path).expect("state rename");
    }

    /// The state file path for a scratch dir.
    pub fn state_path(scratch: &str) -> PathBuf {
        PathBuf::from(scratch).join("state.json")
    }
}

/// A provider reading/writing the state file per call.
pub struct MockProvider {
    scratch: PathBuf,
}

impl MockProvider {
    /// A provider over one scratch dir; the base branch pre-exists with
    /// the seed manifest that releases edit.
    pub fn at(scratch: &str) -> Self {
        std::fs::create_dir_all(scratch).expect("scratch mkdir");
        let scratch_path = PathBuf::from(scratch);
        let state_file = MockState::state_path(scratch);
        if !state_file.exists() {
            let mut state = MockState::default();
            state
                .branches
                .insert(BASE_BRANCH.to_owned(), "main-seed-sha".to_owned());
            state
                .files
                .insert(base_key(THE_SEED_PATH), DEFAULT_SEED_MANIFEST.to_owned());
            state.save(&state_file);
        }
        Self {
            scratch: scratch_path,
        }
    }

    /// The state file path of this provider's scratch dir.
    pub fn state_file(&self) -> PathBuf {
        MockState::state_path(self.scratch.to_string_lossy().as_ref())
    }

    /// Marks a change request merged: the head branch's files land on
    /// the base branch — what the engine's merge verification reads.
    pub fn mark_merged(&self, number: u64) -> Result<(), ProviderError> {
        let path = self.state_file();
        let mut state = MockState::load(&path);
        let Some((head, summary)) = state
            .change_requests
            .iter_mut()
            .find(|(_, summary)| summary.number == number)
        else {
            return Err(ProviderError::NotFound("change request"));
        };
        summary.state = "merged".to_owned();
        let head = head.clone();
        let copies: Vec<(String, String)> = state
            .files
            .iter()
            .filter(|(key, _)| key.starts_with(&format!("{head}/")))
            .map(|(key, value)| {
                let rest = key
                    .strip_prefix(&format!("{head}/"))
                    .unwrap_or_default()
                    .to_owned();
                (base_key(&rest), value.clone())
            })
            .collect();
        for (landed_key, text) in copies {
            state.files.insert(landed_key, text);
        }
        state.save(&path);
        Ok(())
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn capabilities(&self) -> ProviderCaps {
        ProviderCaps {
            branch_protection: false,
            tag_protection: false,
            auto_merge: false,
            releases: false,
            tags: false,
            webhook_parsing: false,
            webhook_verification: false,
            branch_delete: false,
        }
    }

    async fn get_repo_path(&self, repo: &RepoRef) -> ProviderResult<String> {
        Ok(format!("mock/{}", repo.id))
    }

    async fn create_branch(
        &self,
        _repo: &RepoRef,
        branch: &str,
        from_sha: &str,
    ) -> ProviderResult<()> {
        let path = self.state_file();
        let mut state = MockState::load(&path);
        state.branch_creates += 1;
        state
            .branches
            .insert(branch.to_owned(), from_sha.to_owned());
        // A branch starts where its source stands: the base branch's
        // files belong to it (what `git branch` means for the read a
        // release's commit does next).
        let base_prefix = format!("{BASE_BRANCH}/");
        let copies: Vec<(String, String)> = state
            .files
            .iter()
            .filter(|(key, _)| key.starts_with(&base_prefix))
            .map(|(key, value)| {
                (
                    branch_key(branch, key.strip_prefix(&base_prefix).unwrap_or_default()),
                    value.clone(),
                )
            })
            .collect();
        for (branch_file, text) in copies {
            state.files.insert(branch_file, text);
        }
        state.save(&path);
        Ok(())
    }

    async fn branch_sha(&self, _repo: &RepoRef, branch: &str) -> ProviderResult<String> {
        let path = self.state_file();
        let state = MockState::load(&path);
        state
            .branches
            .get(branch)
            .cloned()
            .ok_or(ProviderError::NotFound("branch"))
    }

    async fn commit_files(
        &self,
        _repo: &RepoRef,
        branch: &str,
        edits: &[Edit],
        _message: &str,
        _expected_parent: Option<&str>,
    ) -> ProviderResult<CommitResult> {
        let path = self.state_file();
        let mut state = MockState::load(&path);
        // Group by file: one simulated commit per file (the same
        // grouping the GitHub provider does).
        let mut grouped: BTreeMap<String, Vec<Edit>> = BTreeMap::new();
        for edit in edits {
            grouped
                .entry(edit.file.clone())
                .or_default()
                .push(edit.clone());
        }
        let mut changed = false;
        let mut tip = state
            .branches
            .get(branch)
            .cloned()
            .ok_or(ProviderError::NotFound("branch"))?;
        for (file, edits_of_file) in &grouped {
            let current = state
                .files
                .get(&branch_key(branch, file))
                .cloned()
                .unwrap_or_default();
            let applied = cargobike_core::edits::apply_to_document(
                current.as_bytes(),
                edits_of_file,
                cargobike_core::edits::format_for_path(file),
                &serde_json::Value::Null,
            )
            .map_err(|failure| ProviderError::Request(failure.to_string()))?;
            let text = String::from_utf8(applied).map_err(|_| {
                ProviderError::Request(format!("the edited `{file}` is not valid UTF-8"))
            })?;
            if text != current {
                changed = true;
                tip = format!("mock-sha-{}", state.commit_runs + 1);
                state.files.insert(branch_key(branch, file), text);
            }
        }
        if changed {
            state.commit_runs += 1;
            state.branches.insert(branch.to_owned(), tip.clone());
            state.save(&path);
        }
        Ok(CommitResult {
            sha: tip,
            branch: branch.to_owned(),
        })
    }

    async fn create_change_request(
        &self,
        _repo: &RepoRef,
        head: &str,
        _base: &str,
        _title: &str,
        _body: &str,
        _labels: &[String],
    ) -> ProviderResult<CoreChangeRequest> {
        let path = self.state_file();
        let mut state = MockState::load(&path);
        if let Some(existing) = state.change_requests.get(head) {
            return Ok(core_request_of_summary(existing));
        }
        let number = state.change_requests.len() as u64 + 1;
        let head_sha = state
            .branches
            .get(head)
            .cloned()
            .unwrap_or_else(|| "mock-sha".to_owned());
        let summary = CrSummary {
            number,
            url: format!("mock://crs/{number}"),
            head_sha: head_sha.clone(),
            state: "open".to_owned(),
        };
        state
            .change_requests
            .insert(head.to_owned(), summary.clone());
        state.save(&path);
        Ok(core_request_of_summary(&summary))
    }

    async fn get_change_request(
        &self,
        _repo: &RepoRef,
        number: u64,
    ) -> ProviderResult<CoreChangeRequest> {
        let path = self.state_file();
        let state = MockState::load(&path);
        state
            .change_requests
            .values()
            .find(|summary| summary.number == number)
            .map(core_request_of_summary)
            .ok_or(ProviderError::NotFound("change request"))
    }

    async fn find_change_request_by_head(
        &self,
        _repo: &RepoRef,
        head: &str,
    ) -> ProviderResult<Option<CoreChangeRequest>> {
        let path = self.state_file();
        let state = MockState::load(&path);
        Ok(state.change_requests.get(head).map(core_request_of_summary))
    }

    async fn close_change_request(
        &self,
        _repo: &RepoRef,
        number: u64,
        _comment: &str,
    ) -> ProviderResult<()> {
        let path = self.state_file();
        let mut state = MockState::load(&path);
        for summary in state.change_requests.values_mut() {
            if summary.number == number {
                summary.state = "closed".to_owned();
            }
        }
        state.save(&path);
        Ok(())
    }

    async fn list_open_change_requests(
        &self,
        _repo: &RepoRef,
    ) -> ProviderResult<Vec<CoreChangeRequest>> {
        let path = self.state_file();
        let state = MockState::load(&path);
        Ok(state
            .change_requests
            .values()
            .filter(|summary| summary.state == "open")
            .map(core_request_of_summary)
            .collect())
    }

    async fn read_file(
        &self,
        _repo: &RepoRef,
        file: &str,
        git_ref: &str,
    ) -> ProviderResult<Vec<u8>> {
        let path = self.state_file();
        let state = MockState::load(&path);
        state
            .files
            .get(&branch_key(git_ref, file))
            .cloned()
            .map(String::into_bytes)
            .ok_or(ProviderError::NotFound("file"))
    }

    async fn comment(&self, _repo: &RepoRef, _cr_number: u64, _body: &str) -> ProviderResult<()> {
        Ok(())
    }

    async fn add_labels(
        &self,
        _repo: &RepoRef,
        _cr_number: u64,
        _labels: &[String],
    ) -> ProviderResult<()> {
        Ok(())
    }

    async fn update_branch(&self, _repo: &RepoRef, _branch: &str) -> ProviderResult<()> {
        Ok(())
    }

    async fn default_branch(&self, _repo: &RepoRef) -> ProviderResult<String> {
        Ok(BASE_BRANCH.to_owned())
    }

    async fn set_commit_status(
        &self,
        _repo: &RepoRef,
        _sha: &str,
        _status: CommitStatus,
    ) -> ProviderResult<()> {
        Ok(())
    }
}

/// The harness's no-network HTTP service.
pub struct StubHttpService;

#[async_trait]
impl HttpServiceTrait for StubHttpService {
    async fn send(
        &self,
        _request: HttpRequest,
    ) -> Result<HttpResponse, cargobike_core::provider::HttpError> {
        Ok(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: Default::default(),
        })
    }
}

/// The harness's no-secrets credential store.
pub struct StubCredentials;

impl cargobike_core::registry::CredentialStore for StubCredentials {
    fn resolve(&self, name: &str) -> Result<SecretString, LibraryError> {
        Err(LibraryError::UnknownSecret(format!(
            "the harness holds no secret ({} requested)",
            name
        )))
    }
}

/// A credential store over a literal map — the scan tests resolve named
/// secrets from it.
pub struct MapCredentials(pub BTreeMap<String, SecretString>);

impl cargobike_core::registry::CredentialStore for MapCredentials {
    fn resolve(&self, name: &str) -> Result<SecretString, LibraryError> {
        self.0
            .get(name)
            .cloned()
            .ok_or(LibraryError::UnknownSecret(format!(
                "failed to resolve secret: no secret named `{name}`"
            )))
    }
}
