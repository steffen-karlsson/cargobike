//! GitHub provider implementation for Cargobike (PRD §7.2, Phase 4).
//!
//! The octocrab-backed implementation of the provider contract: REST for
//! repositories/PRs/commits, the App's JWT for installations (or a PAT),
//! and the webhook's signature + normalisation at the transport edge.
//! Every shape here is pinned against octocrab 0.54's source (get_ref,
//! create_ref, create_tree with UTF-8 content, create_commit, update_ref
//! force=false, pulls list/get/update, issues comments + labels,
//! repos_by_id).

use async_trait::async_trait;
use cargobike_core::edits::apply_to_document;
use cargobike_core::model::{CrState, RepoRef as CoreRepoRef};
use cargobike_core::provider::{
    ChangeRequest, CommitResult, CommitStatus, Edit, EditFormat, Provider, ProviderCaps,
    ProviderError, ProviderResult,
};
use octocrab::params::repos::Reference;
use secrecy::ExposeSecret as _;
use secrecy::SecretString;

mod signature;
mod webhook;

pub use webhook::verify_and_normalise;

/// Either a GitHub App (§7.2's default; app-level JWT resolved to an
/// installation token at construction) or a personal token.
pub enum GithubAuth {
    /// A GitHub App identity; the installation is required (one
    /// installation of the App owns the repositories Cargobike drives).
    App {
        app_id: u64,
        installation_id: u64,
        private_key_pem: SecretString,
    },
    /// A fine-grained or classic PAT.
    Token(SecretString),
}

impl std::fmt::Debug for GithubAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::App {
                app_id,
                installation_id,
                ..
            } => formatter.write_fmt(format_args!(
                "App(app_id: {app_id}, installation_id: {installation_id})"
            )),
            Self::Token(_) => formatter.write_str("Token(<redacted>)"),
        }
    }
}

/// The octocrab-backed provider; the client is built once per instance
/// (§6.7's isolation: clients live inside the step context's lifetime).
#[derive(Clone)]
pub struct GithubProvider {
    github: octocrab::Octocrab,
}

impl GithubProvider {
    /// Builds the client with the auth's token lifecycle absorbed at
    /// construction (octocrab caches installation tokens internally).
    pub fn new(auth: &GithubAuth) -> Result<Self, ProviderError> {
        let mut builder = octocrab::Octocrab::builder();
        builder = match auth {
            GithubAuth::Token(token) => builder.personal_token(token.expose_secret().to_owned()),
            GithubAuth::App {
                app_id,
                private_key_pem,
                ..
            } => {
                let encoding_key = jsonwebtoken::EncodingKey::from_rsa_pem(
                    private_key_pem.expose_secret().as_bytes(),
                )
                .map_err(|failure| {
                    ProviderError::Request(format!(
                        "the app's private key is unreadable: {failure}"
                    ))
                })?;
                builder.app(octocrab::models::AppId::from(*app_id), encoding_key)
            }
        };
        let client = builder.build().map_err(|failure| {
            ProviderError::Request(format!("the GitHub client refused to build: {failure}"))
        })?;
        let github = match auth {
            GithubAuth::App {
                installation_id, ..
            } => client
                .installation(octocrab::models::InstallationId::from(*installation_id))
                .map_err(|failure| {
                    ProviderError::Request(format!(
                        "the installation refused to resolve: {failure}"
                    ))
                })?,
            _ => client,
        };
        Ok(Self { github })
    }

    /// The immutable-ID repo reference for octocrab's handlers.
    fn repo_by_id(
        &self,
        repo: &CoreRepoRef,
    ) -> Result<octocrab::repos::RepoHandler<'_>, ProviderError> {
        Ok(self.github.repos_by_id(reference_id(repo)?))
    }

    /// The git handler by the immutable ID.
    fn git_by_id(
        &self,
        repo: &CoreRepoRef,
    ) -> Result<octocrab::git::GitHandler<'_>, ProviderError> {
        Ok(self.github.git_by_id(reference_id(repo)?))
    }
}

/// The octocrab failure mapped to the provider vocabulary (F-48: a
/// missing object is `NotFound`; upable-and-retried failures are
/// `Request`).
fn request_failure(failure: octocrab::Error) -> ProviderError {
    match &failure {
        octocrab::Error::GitHub { source, .. }
            if source.status_code == http::StatusCode::NOT_FOUND =>
        {
            ProviderError::NotFound("object")
        }
        _ => ProviderError::Request(failure.to_string()),
    }
}

/// The sha a Git reference points at (the object kinds vary).
fn reference_sha(found: octocrab::models::repos::Ref) -> String {
    use octocrab::models::repos::Object;
    match found.object {
        Object::Commit { sha, .. } | Object::Tag { sha, .. } => sha,
        _ => String::new(),
    }
}

impl std::ops::Deref for GithubProvider {
    type Target = octocrab::Octocrab;

    fn deref(&self) -> &Self::Target {
        &self.github
    }
}

#[async_trait]
impl Provider for GithubProvider {
    fn capabilities(&self) -> ProviderCaps {
        ProviderCaps {
            branch_protection: true,
            tag_protection: true,
            webhook_parsing: true,
            webhook_verification: true,
            auto_merge: true,
            releases: true,
            tags: true,
            branch_delete: true,
        }
    }

    async fn get_repo_path(&self, repo: &CoreRepoRef) -> ProviderResult<String> {
        if let Some(path) = &repo.path {
            return Ok(path.clone());
        }
        let repository = self
            .repo_by_id(repo)?
            .get()
            .await
            .map_err(request_failure)?;
        repository
            .full_name
            .ok_or(ProviderError::NotFound("repository"))
    }

    async fn branch_sha(&self, repo: &CoreRepoRef, branch: &str) -> ProviderResult<String> {
        let handler = self.git_by_id(repo)?;

        let found = handler
            .get_ref(&Reference::Branch(branch.to_owned()))
            .await
            .map_err(request_failure)?;
        Ok(reference_sha(found))
    }

    async fn create_branch(
        &self,
        repo: &CoreRepoRef,
        branch: &str,
        from_sha: &str,
    ) -> ProviderResult<()> {
        let handler = self.git_by_id(repo)?;
        handler
            .create_ref(&Reference::Branch(branch.to_owned()), from_sha)
            .await
            .map_err(request_failure)?;
        Ok(())
    }

    async fn commit_files(
        &self,
        repo: &CoreRepoRef,
        branch: &str,
        edits: &[Edit],
        message: &str,
        expected_parent: Option<&str>,
    ) -> ProviderResult<CommitResult> {
        if edits.is_empty() {
            return Err(ProviderError::NotFound("edits"));
        }
        let handler = self.git_by_id(repo)?;
        // The branch must exist and carry our parent (A1's fused branch+
        // commit: the provider creates it when absent — the caller's
        // `branch_sha` find-then-create already covers the general case;
        // here a missing branch means we are mid-recovery of one).
        let base_sha = match handler.get_ref(&Reference::Branch(branch.to_owned())).await {
            Ok(found) => reference_sha(found),
            Err(_) => {
                let handler = self.git_by_id(repo)?;
                let base = reference_sha(
                    handler
                        .get_ref(&Reference::Branch("main".to_owned()))
                        .await
                        .map_err(request_failure)?,
                );
                handler
                    .create_ref(&Reference::Branch(branch.to_owned()), &base)
                    .await
                    .map_err(request_failure)?;
                base
            }
        };
        let parent = expected_parent.unwrap_or(&base_sha);
        if let Some(at) = expected_parent {
            if at != base_sha.as_str() {
                // F-52: the branch moved under us; the caller's A1 retry
                // path decides (a new commit would silently betray the
                // expected base).
                return Err(ProviderError::Request(format!(
                    "the branch moved: expected parent {at}, the ref is at {base_sha}"
                )));
            }
        }
        // Full-file replacements into the tree (the edits' files are
        // always readable as text; the tree entry carries content).
        let mut entries = Vec::with_capacity(edits.len());
        for edit in edits {
            let base_document = self.read_file(repo, edit.file.as_str(), branch).await?;
            let format = edit.format.unwrap_or(EditFormat::Yaml);
            let desired = edit.value.clone().unwrap_or(serde_json::Value::Null);
            let body =
                apply_to_document(&base_document, std::slice::from_ref(edit), format, &desired)
                    .map_err(|failure| ProviderError::Request(failure.to_string()))?;
            let text = String::from_utf8(body).map_err(|_| {
                ProviderError::Request(format!("the edited `{}` is not valid UTF-8", edit.file))
            })?;
            entries.push(octocrab::models::git::CreateTreeEntry {
                path: edit.file.clone(),
                mode: "100644".to_owned(),
                r#type: "blob".to_owned(),
                sha: None,
                content: Some(text),
            });
        }
        let tree = handler
            .create_tree(entries)
            .base_tree(parent.to_owned())
            .send()
            .await
            .map_err(request_failure)?;
        let commit = handler
            .create_commit(message.to_owned(), tree.sha.clone())
            .parents(vec![parent.to_owned()])
            .send()
            .await
            .map_err(request_failure)?;
        handler
            .update_ref(&Reference::Branch(branch.to_owned()), commit.sha.clone())
            .force(false) // a moved ref betrays the expected parent (F-52)
            .send()
            .await
            .map_err(request_failure)?;
        Ok(CommitResult {
            sha: commit.sha,
            branch: branch.to_owned(),
        })
    }

    async fn get_change_request(
        &self,
        repo: &CoreRepoRef,
        number: u64,
    ) -> ProviderResult<ChangeRequest> {
        let (owner, name) = owner_name(self, repo).await?;
        let pull = self
            .github
            .pulls(owner.clone(), name)
            .get(number)
            .await
            .map_err(request_failure)?;
        Ok(change_request_shape(&pull))
    }

    async fn create_change_request(
        &self,
        repo: &CoreRepoRef,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> ProviderResult<ChangeRequest> {
        let (owner, name) = owner_name(self, repo).await?;
        let pull = self
            .github
            .pulls(owner.clone(), name.clone())
            .create(title, head, base)
            .body(body)
            .send()
            .await
            .map_err(request_failure)?;
        // F-49's labels apply separately (the create's belt-and-bed check).
        if !labels.is_empty() {
            self.github
                .issues(owner, name)
                .add_labels(pull.number, labels)
                .await
                .map_err(request_failure)?;
        }
        Ok(change_request_shape(&pull))
    }

    async fn find_change_request_by_head(
        &self,
        repo: &CoreRepoRef,
        head: &str,
    ) -> ProviderResult<Option<ChangeRequest>> {
        let (owner, name) = owner_name(self, repo).await?;
        let head_reference = format!("{owner}:{head}");
        let page = self
            .github
            .pulls(owner, name)
            .list()
            .state(octocrab::params::State::Open)
            .head(head_reference)
            .send()
            .await
            .map_err(request_failure)?;
        Ok(page
            .items
            .into_iter()
            .next()
            .map(|pull| change_request_shape(&pull)))
    }

    async fn default_branch(&self, repo: &CoreRepoRef) -> ProviderResult<String> {
        let repository = self
            .repo_by_id(repo)?
            .get()
            .await
            .map_err(request_failure)?;
        Ok(repository
            .default_branch
            .unwrap_or_else(|| "main".to_owned()))
    }

    async fn read_file(
        &self,
        repo: &CoreRepoRef,
        file: &str,
        git_ref: &str,
    ) -> ProviderResult<Vec<u8>> {
        let mut content = self
            .repo_by_id(repo)?
            .get_content()
            .path(file)
            .r#ref(git_ref)
            .send()
            .await
            .map_err(request_failure)?;
        let Some(first) = content.take_items().first().cloned() else {
            return Err(ProviderError::NotFound("file"));
        };
        first
            .decoded_content()
            .map(String::into_bytes)
            .ok_or(ProviderError::Request(format!(
                "the file `{file}` payload did not decode"
            )))
    }

    async fn comment(&self, repo: &CoreRepoRef, cr_number: u64, body: &str) -> ProviderResult<()> {
        let (owner, name) = owner_name(self, repo).await?;
        self.github
            .issues(owner, name)
            .create_comment(cr_number, body)
            .await
            .map_err(request_failure)?;
        Ok(())
    }

    async fn add_labels(
        &self,
        repo: &CoreRepoRef,
        cr_number: u64,
        labels: &[String],
    ) -> ProviderResult<()> {
        let (owner, name) = owner_name(self, repo).await?;
        self.github
            .issues(owner, name)
            .add_labels(cr_number, labels)
            .await
            .map_err(request_failure)?;
        Ok(())
    }

    async fn update_branch(&self, _repo: &CoreRepoRef, _branch: &str) -> ProviderResult<()> {
        // F-47's `builtin/update-branch@1` wires this in v1.1; the
        // contract's shape stays (the mock's implementation notes the
        // same boundary).
        Err(ProviderError::Unsupported)
    }

    async fn set_commit_status(
        &self,
        repo: &CoreRepoRef,
        sha: &str,
        status: CommitStatus,
    ) -> ProviderResult<()> {
        let state = match status {
            CommitStatus::Pending => octocrab::models::StatusState::Pending,
            CommitStatus::Success => octocrab::models::StatusState::Success,
            CommitStatus::Failure => octocrab::models::StatusState::Failure,
        };
        self.repo_by_id(repo)?
            .create_status(sha.to_owned(), state)
            .send()
            .await
            .map_err(request_failure)?;
        Ok(())
    }

    async fn close_change_request(
        &self,
        repo: &CoreRepoRef,
        number: u64,
        comment: &str,
    ) -> ProviderResult<()> {
        let (owner, name) = owner_name(self, repo).await?;
        self.github
            .pulls(owner, name)
            .update(number)
            .state(octocrab::params::pulls::State::Closed)
            .send()
            .await
            .map_err(request_failure)?;
        self.comment(repo, number, comment).await
    }

    async fn list_open_change_requests(
        &self,
        repo: &CoreRepoRef,
    ) -> ProviderResult<Vec<ChangeRequest>> {
        let (owner, name) = owner_name(self, repo).await?;
        let mut page = self
            .github
            .pulls(owner.clone(), name.clone())
            .list()
            .state(octocrab::params::State::Open)
            .send()
            .await
            .map_err(request_failure)?;
        let mut requests = page
            .take_items()
            .into_iter()
            .map(|pull| change_request_shape(&pull))
            .collect::<Vec<_>>();
        // The `next` link's `page=N` query parameter (no extra parsing
        // dependency; the link lives on the Page).
        while let Some(next_number) =
            page.next
                .as_ref()
                .and_then(|uri| uri.query())
                .and_then(|query| {
                    query
                        .split('&')
                        .find(|parameter| parameter.starts_with("page="))
                        .and_then(|parameter| parameter[5..].parse::<u32>().ok())
                })
        {
            page = self
                .github
                .pulls(owner.clone(), name.clone())
                .list()
                .state(octocrab::params::State::Open)
                .page(next_number)
                .send()
                .await
                .map_err(request_failure)?;
            requests.extend(
                page.take_items()
                    .into_iter()
                    .map(|pull| change_request_shape(&pull)),
            );
        }
        Ok(requests)
    }
}

/// REST owner/name-pair handlers need the full name; the immutable ID
/// resolves when the verified label is absent (F-10).
/// The octocrab repository ID out of the immutable string id (F-10; R6).
fn reference_id(repo: &CoreRepoRef) -> Result<octocrab::models::RepositoryId, ProviderError> {
    let parsed: u64 = repo.id.parse().map_err(|_| {
        ProviderError::Request(format!("the repo id `{}` is not a number", repo.id))
    })?;
    Ok(parsed.into())
}

/// REST owner/name-pair handlers need the full name; the immutable ID
/// resolves when the verified label is absent (F-10).
async fn owner_name(
    provider: &GithubProvider,
    repo: &CoreRepoRef,
) -> ProviderResult<(String, String)> {
    let full_name = provider.get_repo_path(repo).await?;
    full_name
        .split_once('/')
        .map(|(owner, name)| (owner.to_owned(), name.to_owned()))
        .ok_or_else(|| {
            ProviderError::Request(format!("the repo path `{full_name}` is not owner/name"))
        })
}

fn change_request_shape(pull: &octocrab::models::pulls::PullRequest) -> ChangeRequest {
    ChangeRequest {
        number: pull.number,
        url: pull
            .html_url
            .as_ref()
            .map(|url| url.to_string())
            .unwrap_or_default(),
        head_sha: pull.head.sha.clone(),
        state: if pull.merged == Some(true) {
            CrState::Merged
        } else if pull.state == Some(octocrab::models::IssueState::Closed) {
            CrState::Closed
        } else {
            CrState::Open
        },
    }
}
