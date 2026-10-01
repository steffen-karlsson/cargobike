//! The T1 harness's mock provider (feature t1-crash-hooks only): state
//! lives in ONE JSON file that the sequential harness processes share, so
//! the side-effect counts are observable across the kill.

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

/// What the state file declares (T1's convergence facts).
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MockState {
    /// Branch creations (one per release).
    pub branch_creates: u64,
    /// Fused edit+commit runs (one per release).
    pub commit_runs: u64,
    /// The CRs, keyed by head branch.
    pub change_requests: BTreeMap<String, CrSummary>,
    /// Branch SHAs per name.
    pub branches: BTreeMap<String, String>,
    /// The files (branch/field -> last committed content).
    pub files: BTreeMap<String, String>,
}

/// A CR compact enough for the state file.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CrSummary {
    pub number: u64,
    pub url: String,
    pub head_sha: String,
    pub state: String,
}

impl MockState {
    /// Loads the state file, defaulting to empty.
    pub fn load(path: &PathBuf) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Writes the state file.
    pub fn save(&self, path: &PathBuf) {
        let contents = serde_json::to_string_pretty(self).expect("state serialises");
        std::fs::write(path, contents).expect("state write");
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
    /// A provider over one scratch dir; the base branch pre-exists (a
    /// repository always has `main`).
    pub fn at(scratch: &str) -> Self {
        std::fs::create_dir_all(scratch).expect("scratch mkdir");
        let scratch_path = PathBuf::from(scratch);
        let state_file = MockState::state_path(scratch);
        if !state_file.exists() {
            let mut state = MockState::default();
            state
                .branches
                .insert("main".to_owned(), "main-seed-sha".to_owned());
            state.save(&state_file);
        }
        Self {
            scratch: scratch_path,
        }
    }
}

/// The state path of this provider's scratch dir.
fn state_of(provider: &MockProvider) -> PathBuf {
    MockState::state_path(provider.scratch.to_string_lossy().as_ref())
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
        let path = state_of(self);
        let mut state = MockState::load(&path);
        state.branch_creates += 1;
        state
            .branches
            .insert(branch.to_owned(), from_sha.to_owned());
        state.save(&path);
        Ok(())
    }

    async fn branch_sha(&self, _repo: &RepoRef, branch: &str) -> ProviderResult<String> {
        let path = state_of(self);
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
        let path = state_of(self);
        let mut state = MockState::load(&path);
        state.commit_runs += 1;
        let mut sha = "mock-sha".to_owned();
        for edit in edits {
            sha = format!("mock-sha-{}", edit.file);
            let value = edit.value.clone().unwrap_or(serde_json::Value::Null);
            state
                .files
                .insert(format!("{}/{}", branch, edit.field), value.to_string());
        }
        state.branches.insert(branch.to_owned(), sha.clone());
        state.save(&path);
        Ok(CommitResult {
            sha,
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
        let path = state_of(self);
        let mut state = MockState::load(&path);
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
        state.change_requests.insert(head.to_owned(), summary);
        state.save(&path);
        Ok(CoreChangeRequest {
            number,
            url: format!("mock://crs/{number}"),
            head_sha,
            state: CrState::Open,
        })
    }

    async fn get_change_request(
        &self,
        _repo: &RepoRef,
        number: u64,
    ) -> ProviderResult<CoreChangeRequest> {
        let path = state_of(self);
        let state = MockState::load(&path);
        state
            .change_requests
            .values()
            .find(|summary| summary.number == number)
            .map(|found| CoreChangeRequest {
                number: found.number,
                url: found.url.clone(),
                head_sha: found.head_sha.clone(),
                state: CrState::Open,
            })
            .ok_or(ProviderError::NotFound("change request"))
    }

    async fn find_change_request_by_head(
        &self,
        _repo: &RepoRef,
        head: &str,
    ) -> ProviderResult<Option<CoreChangeRequest>> {
        let path = state_of(self);
        let state = MockState::load(&path);
        Ok(state
            .change_requests
            .get(head)
            .map(|found| CoreChangeRequest {
                number: found.number,
                url: found.url.clone(),
                head_sha: found.head_sha.clone(),
                state: CrState::Open,
            }))
    }

    async fn close_change_request(
        &self,
        _repo: &RepoRef,
        _number: u64,
        _comment: &str,
    ) -> ProviderResult<()> {
        Ok(())
    }

    async fn list_open_change_requests(
        &self,
        _repo: &RepoRef,
    ) -> ProviderResult<Vec<CoreChangeRequest>> {
        Ok(Vec::new())
    }

    async fn read_file(
        &self,
        _repo: &RepoRef,
        _file: &str,
        _git_ref: &str,
    ) -> ProviderResult<Vec<u8>> {
        Err(ProviderError::NotFound("file"))
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
        Ok("main".to_owned())
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
