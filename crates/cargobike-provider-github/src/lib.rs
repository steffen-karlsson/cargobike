//! GitHub provider implementation for Cargobike (Phase 4).
//!
//! The octocrab-backed implementation of the provider contract: REST for
//! repositories/PRs/commits, the App's JWT for installations (the or a PAT),
//! and the webhook's signature + normalisation at the transport edge.
//! Every shape here is pinned against octocrab 0.54's source (get_ref,
//! create_ref, create_tree with UTF-8 content, create_commit, update_ref
//! force=false, pulls list/get/update, issues comments + labels,
//! repos_by_id).

use async_trait::async_trait;
use cargobike_core::edits::apply_to_document;
use cargobike_core::model::{CrState, RepoRef as CoreRepoRef};
use cargobike_core::provider::{
    ChangeRequest, CommitResult, CommitStatus, CrLookup, Edit, EditFormat, Provider, ProviderCaps,
    ProviderError, ProviderResult,
};
use octocrab::params::repos::Reference;
use secrecy::ExposeSecret as _;
use secrecy::SecretString;

mod signature;
mod webhook;

pub use webhook::verify_and_normalise;

/// Either a GitHub App (the installation may be pinned statically or
/// resolved per repository, F-122) or a personal token.
pub enum GithubAuth {
    /// A GitHub App identity. `installation_id: Some` pins the one
    /// installation (the small-scale shape: one App installed once,
    /// octocrab caches its token). `installation_id: None` is the
    /// F-122 shape: each repository call resolves its own installation
    /// and mints a token scoped to that single repository.
    App {
        app_id: u64,
        installation_id: Option<u64>,
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
                "App(app_id: {app_id}, installation_id: {installation_id:?})"
            )),
            Self::Token(_) => formatter.write_str("Token(<redacted>)"),
        }
    }
}

/// How the provider authenticates its API calls.
#[derive(Clone)]
enum ClientMode {
    /// One prebuilt client: the PAT and the pinned-installation App
    /// shapes (today's behaviour — octocrab caches the installation
    /// token internally).
    Static(octocrab::Octocrab),
    /// F-122: an App-level client that resolves each repository's
    /// installation and mints a token scoped to that single
    /// repository; the minted clients cache per repo id until their
    /// token's expiry (minus a refresh buffer).
    PerRepo {
        app: octocrab::Octocrab,
        /// The api base the minted clients must inherit (GHES's
        /// `api_url`; a fresh builder would default to github.com and
        /// the scoped client would call the wrong host).
        base_url: Option<String>,
        cache: std::sync::Arc<tokio::sync::Mutex<std::collections::BTreeMap<u64, CachedScoped>>>,
    },
}

/// One minted repository-scoped client.
#[derive(Clone)]
struct CachedScoped {
    client: octocrab::Octocrab,
    /// The token's expiry; the cache refreshes a minute early.
    expires_at: time::OffsetDateTime,
}

/// The token's permission subset (F-122 "minimal permissions"): the
/// README's documented permission table — a token mint request can
/// only NARROW the installation's grants, so this pins what the
/// provider actually uses and nothing else.
fn minimal_permissions() -> std::collections::HashMap<String, String> {
    [
        ("metadata", "read"),
        ("contents", "write"),
        ("pull requests", "write"),
        ("issues", "write"),
        ("commit statuses", "write"),
        ("administration", "read"),
        ("rulesets", "read"),
    ]
    .into_iter()
    .map(|(permission, level)| (permission.to_owned(), level.to_owned()))
    .collect()
}

/// The octocrab-backed provider; the client is built once per instance
/// (theirs isolation: clients live inside the step context's lifetime).
#[derive(Clone)]
pub struct GithubProvider {
    mode: ClientMode,
    /// F-69's conditional-request half: the CR GETs (the reconciler
    /// sweep and the merge verification are the pollers) carry
    /// `If-None-Match` with the last ETag; a 304 answers from this
    /// cache — the request still happens but costs no quota (GitHub
    /// does not count conditional 304s against the primary rate
    /// limit). TTL-bounded: a stale entry only costs one fresh GET.
    cr_etags: moka::sync::Cache<String, CachedCr>,
}

/// The typed GraphQL query for the reconciler's batched lookups
/// (F-69, PRD 4.3): checked against the trimmed GitHub SDL at
/// compile time.
#[derive(graphql_client::GraphQLQuery)]
#[graphql(
    schema_path = "graphql/schema.graphql",
    query_path = "graphql/reconciler_pull_requests.graphql",
    response_derives = "Debug, Clone"
)]
struct ReconcilerPullRequests;

/// The ETag cache's key: the repo identity plus the CR number.
fn cr_cache_key(repo: &CoreRepoRef, number: u64) -> String {
    format!("{}:{}:{number}", repo.provider, repo.id)
}

/// One cached CR: the ETag that identifies the state (absent when
/// the server sent none — then no conditional request, but the global
/// node id still serves the batched lookups) and the state itself (a
/// 304 hands back this pair).
#[derive(Clone)]
struct CachedCr {
    etag: Option<String>,
    change_request: ChangeRequest,
}

impl GithubProvider {
    /// F-69's conditional form: the CR GET carries the last ETag as
    /// `If-None-Match` and a 304 answers from the cache — the request
    /// still happens but costs no quota (GitHub does not count
    /// conditional 304s against the primary rate limit). The result is
    /// always cached (with or without a server ETag): the batched
    /// lookups key on the cached global node id, and a stale entry only
    /// costs one fresh GET. The raw call (octocrab's own route) keeps
    /// the response's headers visible.
    async fn fetch_cr(
        &self,
        github: &octocrab::Octocrab,
        repo: &CoreRepoRef,
        number: u64,
    ) -> ProviderResult<ChangeRequest> {
        let (owner, name) = owner_name(self, repo).await?;
        let route = format!("/repos/{owner}/{name}/pulls/{number}");
        let key = cr_cache_key(repo, number);
        let mut headers = http::HeaderMap::new();
        if let Some(cached) = self.cr_etags.get(&key) {
            if let Some(etag) = cached.etag.clone() {
                let etag = http::HeaderValue::from_str(&etag).map_err(|failure| {
                    ProviderError::Request(format!(
                        "the cached ETag refused to re-serialise: {failure}"
                    ))
                })?;
                headers.insert("If-None-Match", etag);
            }
        }
        let conditional = if headers.is_empty() {
            None
        } else {
            Some(headers)
        };
        let response = github
            ._get_with_headers(route, conditional)
            .await
            .map_err(request_failure)?;
        if response.status() == http::StatusCode::NOT_MODIFIED {
            // The provider's own truth: unchanged since the fetch the
            // ETag came from.
            return self
                .cr_etags
                .get(&key)
                .map(|cached| cached.change_request)
                .ok_or_else(|| {
                    ProviderError::Request(
                        "a 304 arrived without a cached change request".to_owned(),
                    )
                });
        }
        let etag = response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let response = octocrab::map_github_error(response)
            .await
            .map_err(request_failure)?;
        let body = github
            .body_to_string(response)
            .await
            .map_err(request_failure)?;
        let pull: octocrab::models::pulls::PullRequest =
            serde_json::from_str(&body).map_err(|failure| {
                ProviderError::Request(format!("the change request refused to decode: {failure}"))
            })?;
        let change_request = change_request_shape(&pull);
        self.cr_etags.insert(
            key,
            CachedCr {
                etag,
                change_request: change_request.clone(),
            },
        );
        Ok(change_request)
    }

    /// Builds the client with the auth's token lifecycle absorbed at
    /// construction (the octocrab caches installation tokens internally).
    pub fn new(auth: &GithubAuth) -> Result<Self, ProviderError> {
        Self::new_with_api_url(auth, None)
    }

    /// The same build with a base URL: GitHub Enterprise Server's
    /// `api_url` config lands here (the default is github.com's).
    pub fn new_with_api_url(
        auth: &GithubAuth,
        api_url: Option<&str>,
    ) -> Result<Self, ProviderError> {
        let mut builder = octocrab::Octocrab::builder();
        if let Some(api_url) = api_url {
            builder = builder.base_uri(api_url).map_err(|failure| {
                ProviderError::Request(format!("the api_url refused to parse: {failure}"))
            })?;
        }
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
        let mode = match auth {
            GithubAuth::App {
                installation_id: Some(installation_id),
                ..
            } => ClientMode::Static(
                client
                    .installation(octocrab::models::InstallationId::from(*installation_id))
                    .map_err(|failure| {
                        ProviderError::Request(format!(
                            "the installation refused to resolve: {failure}"
                        ))
                    })?,
            ),
            // The F-122 shape: no pinned installation — each repository
            // resolves its own and the token is scoped per call.
            GithubAuth::App {
                installation_id: None,
                ..
            } => ClientMode::PerRepo {
                app: client,
                base_url: api_url.map(str::to_owned),
                cache: std::sync::Arc::new(tokio::sync::Mutex::new(
                    std::collections::BTreeMap::new(),
                )),
            },
            GithubAuth::Token(_) => ClientMode::Static(client),
        };
        Ok(Self {
            mode,
            cr_etags: Self::cr_etag_cache(),
        })
    }

    /// The conditional-request cache: one hour of TTL, bounded (the
    /// entries are small and the sweep's working set is per-app).
    fn cr_etag_cache() -> moka::sync::Cache<String, CachedCr> {
        moka::sync::Cache::builder()
            .time_to_live(std::time::Duration::from_secs(3600))
            .max_capacity(1024)
            .build()
    }

    /// A provider over a prebuilt octocrab client (the tests point one
    /// at a mocked server; production builds with [`Self::new`]).
    pub fn from_octocrab(github: octocrab::Octocrab) -> Self {
        Self {
            mode: ClientMode::Static(github),
            cr_etags: Self::cr_etag_cache(),
        }
    }

    /// A per-repo provider (F-122) over a prebuilt APP-level client —
    /// the tests point one at a mocked GitHub; production builds with
    /// [`Self::new`] (`GithubAuth::App { installation_id: None }`),
    /// which needs the real private key the app client's JWT signs
    /// with. The minting flow itself needs only the client.
    #[doc(hidden)]
    pub fn from_app_client(app: octocrab::Octocrab, base_url: Option<&str>) -> Self {
        Self {
            cr_etags: Self::cr_etag_cache(),
            mode: ClientMode::PerRepo {
                app,
                base_url: base_url.map(str::to_owned),
                cache: std::sync::Arc::new(tokio::sync::Mutex::new(
                    std::collections::BTreeMap::new(),
                )),
            },
        }
    }

    /// The client a repository's calls ride (F-122): the static shapes
    /// share one client; the per-repo shape resolves the repository's
    /// installation (`GET /repositories/{id}/installation`) and mints
    /// an installation token scoped to the single repository
    /// (`repository_ids`), cached until a minute before expiry.
    async fn client_for(&self, repo: &CoreRepoRef) -> Result<octocrab::Octocrab, ProviderError> {
        match &self.mode {
            ClientMode::Static(client) => Ok(client.clone()),
            ClientMode::PerRepo {
                app,
                base_url,
                cache,
            } => {
                let id_u64 = *reference_id(repo)?;
                let mut cache = cache.lock().await;
                if let Some(hit) = cache.get(&id_u64) {
                    if hit.expires_at > time::OffsetDateTime::now_utc() {
                        return Ok(hit.client.clone());
                    }
                }
                let id = octocrab::models::RepositoryId::from(id_u64);
                let installation = app
                    .repos_by_id(id)
                    .installation()
                    .await
                    .map_err(|failure| {
                        ProviderError::Request(format!(
                            "the repository's installation refused to resolve: {failure}"
                        ))
                    })?;
                let token = app
                    .apps()
                    .create_installation_access_token(
                        installation.id,
                        &octocrab::models::apps::CreateInstallationAccessToken {
                            repository_ids: Some(vec![id]),
                            repositories: None,
                            permissions: Some(minimal_permissions()),
                        },
                    )
                    .await
                    .map_err(|failure| {
                        ProviderError::Request(format!(
                            "the repository-scoped token refused to mint: {failure}"
                        ))
                    })?;
                // The expiry is RFC 3339 in the payload; a missing one
                // caches conservatively (5 minutes) — an over-long
                // cache would mint 401s an hour later.
                let expires_at = token
                    .expires_at
                    .as_deref()
                    .and_then(|text| {
                        time::OffsetDateTime::parse(
                            text,
                            &time::format_description::well_known::Rfc3339,
                        )
                        .ok()
                    })
                    .unwrap_or_else(|| {
                        time::OffsetDateTime::now_utc() + time::Duration::minutes(5)
                    })
                    - time::Duration::minutes(1);
                let mut minted_builder = octocrab::Octocrab::builder().personal_token(token.token);
                if let Some(base_url) = base_url {
                    minted_builder = minted_builder.base_uri(base_url).map_err(|failure| {
                        ProviderError::Request(format!("the api_url refused to parse: {failure}"))
                    })?;
                }
                let minted = minted_builder.build().map_err(|failure| {
                    ProviderError::Request(format!(
                        "the repository-scoped client refused to build: {failure}"
                    ))
                })?;
                cache.insert(
                    id_u64,
                    CachedScoped {
                        client: minted.clone(),
                        expires_at,
                    },
                );
                Ok(minted)
            }
        }
    }

    /// The immutable-ID repo reference for octocrab's handlers.
    fn repo_by_id<'a>(
        client: &'a octocrab::Octocrab,
        repo: &CoreRepoRef,
    ) -> Result<octocrab::repos::RepoHandler<'a>, ProviderError> {
        Ok(client.repos_by_id(reference_id(repo)?))
    }

    /// The git handler by the immutable ID.
    fn git_by_id<'a>(
        client: &'a octocrab::Octocrab,
        repo: &CoreRepoRef,
    ) -> Result<octocrab::git::GitHandler<'a>, ProviderError> {
        Ok(client.git_by_id(reference_id(repo)?))
    }
}

/// The octocrab failure mapped to the provider vocabulary (the a
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

#[async_trait]
impl Provider for GithubProvider {
    fn capabilities(&self) -> ProviderCaps {
        ProviderCaps {
            branch_protection: true,
            tag_protection: true,
            tag_lookup: true,
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
        let github = self.client_for(repo).await?;
        let repository = Self::repo_by_id(&github, repo)?
            .get()
            .await
            .map_err(request_failure)?;
        repository
            .full_name
            .ok_or(ProviderError::NotFound("repository"))
    }

    async fn branch_sha(&self, repo: &CoreRepoRef, branch: &str) -> ProviderResult<String> {
        let github = self.client_for(repo).await?;
        let handler = Self::git_by_id(&github, repo)?;

        let found = handler
            .get_ref(&Reference::Branch(branch.to_owned()))
            .await
            .map_err(request_failure)?;
        Ok(reference_sha(found))
    }

    /// F-95: the tag's COMMIT SHA — the version policy verifies
    /// against the provider's tag object, not any payload alone. An
    /// annotated tag's ref points at the tag object, so it
    /// dereferences one more hop to the commit.
    async fn tag_sha(&self, repo: &CoreRepoRef, tag: &str) -> ProviderResult<String> {
        let github = self.client_for(repo).await?;
        let handler = Self::git_by_id(&github, repo)?;
        let found = handler
            .get_ref(&Reference::Tag(tag.to_owned()))
            .await
            .map_err(request_failure)?;
        let (sha, kind) = match found.object {
            octocrab::models::repos::Object::Commit { sha, .. } => (sha, "commit"),
            octocrab::models::repos::Object::Tag { sha, .. } => (sha, "tag"),
            _ => return Err(ProviderError::NotFound("tag ref")),
        };
        if kind != "tag" {
            return Ok(sha);
        }
        // An annotated tag: the ref's object is the tag object; its
        // `object` field carries the commit.
        let annotated = handler.get_tag(sha).await.map_err(request_failure)?;
        annotated
            .object
            .map(|object| object.sha)
            .ok_or(ProviderError::NotFound("tag object"))
    }

    async fn create_branch(
        &self,
        repo: &CoreRepoRef,
        branch: &str,
        from_sha: &str,
    ) -> ProviderResult<()> {
        let github = self.client_for(repo).await?;
        let handler = Self::git_by_id(&github, repo)?;
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
        let github = self.client_for(repo).await?;
        let handler = Self::git_by_id(&github, repo)?;
        // The branch must exist and carry our parent (the fused branch+
        // commit: the provider creates it when absent — the caller's
        // `branch_sha` find-then-create already covers the general case;
        // here a missing branch means we are mid-recovery of one).
        let base_sha = match handler.get_ref(&Reference::Branch(branch.to_owned())).await {
            Ok(found) => reference_sha(found),
            Err(_) => {
                let github = self.client_for(repo).await?;
                let handler = Self::git_by_id(&github, repo)?;
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
                // : the branch moved under us; the caller's retry
                // path decides (a new commit would silently betray the
                // expected base).
                return Err(ProviderError::Request(format!(
                    "the branch moved: expected parent {at}, the ref is at {base_sha}"
                )));
            }
        }
        // Group the edits by file: one tree entry per file (the
        // same-file multi-edits previously raced as separate entries);
        // values arrive provision-stamped. replay no-op: when a
        // file's edited document equals its current content, the
        // commit already answered - such files contribute no entry.
        let mut grouped: std::collections::BTreeMap<String, Vec<cargobike_core::provider::Edit>> =
            std::collections::BTreeMap::new();
        for edit in edits {
            grouped
                .entry(edit.file.clone())
                .or_default()
                .push(edit.clone());
        }
        let mut entries = Vec::with_capacity(grouped.len());
        for (file, edits_of_file) in grouped {
            let current = self.read_file(repo, file.as_str(), branch).await?;
            let edit_list = edits_of_file.as_slice();
            let format = edit_list
                .first()
                .and_then(|edit| edit.format)
                .unwrap_or(EditFormat::Yaml);
            let document = apply_to_document(&current, edit_list, format, &serde_json::Value::Null)
                .map_err(|failure| ProviderError::Request(failure.to_string()))?;
            // The no-op test: re-serialising the untouched current doc
            // content must differ from the edit's applied text.
            if document == current {
                continue; // the desired values are already on the branch
            }
            let text = String::from_utf8(document).map_err(|_| {
                ProviderError::Request(format!("the edited `{file}` is not valid UTF-8"))
            })?;
            entries.push(octocrab::models::git::CreateTreeEntry {
                path: file,
                mode: "100644".to_owned(),
                r#type: "blob".to_owned(),
                sha: None,
                content: Some(text),
            });
        }
        if entries.is_empty() {
            // : the tree already matches; the branch tip is the answer.
            return Ok(CommitResult {
                sha: base_sha,
                branch: branch.to_owned(),
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
            .force(false) // a moved ref betrays the expected parent
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
        let github = self.client_for(repo).await?;
        self.fetch_cr(&github, repo, number).await
    }

    /// F-69's batched half: one GraphQL `nodes` request carries every
    /// pending CR of a repository — the group — instead of one REST GET
    /// per row. The group is the repository because the auth scope is:
    /// a per-repo installation token (F-122) cannot read another
    /// repository's nodes, and the static shapes share one client
    /// either way. Lookups whose CR the provider has not fetched yet
    /// (no cached global id) ride the REST lookup, which primes the id
    /// for the next sweep. GraphQL failures fall back to per-row REST
    /// for the whole group: correctness first, the batch is the
    /// optimisation.
    async fn get_change_requests(
        &self,
        lookups: &[CrLookup<'_>],
    ) -> ProviderResult<Vec<ChangeRequest>> {
        if lookups.is_empty() {
            return Ok(Vec::new());
        }
        let mut results: Vec<Option<ChangeRequest>> = lookups.iter().map(|_| None).collect();
        let mut groups: std::collections::BTreeMap<(String, String), Vec<usize>> =
            std::collections::BTreeMap::new();
        for (index, lookup) in lookups.iter().enumerate() {
            groups
                .entry((lookup.repo.provider.clone(), lookup.repo.id.clone()))
                .or_default()
                .push(index);
        }
        for ((provider, id), indexes) in &groups {
            let repo = CoreRepoRef::new(provider, id);
            // Priming: fetch (or serve) any CR without a cached global
            // id through the REST path first; those results are final.
            let mut batch: Vec<(usize, String)> = Vec::new();
            for &index in indexes {
                let number = lookups[index].number;
                let key = cr_cache_key(&repo, number);
                let cached_id = self
                    .cr_etags
                    .get(&key)
                    .and_then(|cached| cached.change_request.node_id.clone());
                match cached_id {
                    Some(node_id) => batch.push((index, node_id)),
                    None => {
                        let change_request = self.get_change_request(&repo, number).await?;
                        results[index] = Some(change_request);
                    }
                }
            }
            if batch.is_empty() {
                continue;
            }
            let github = self.client_for(&repo).await?;
            let ids: Vec<String> = batch.iter().map(|(_, id)| id.clone()).collect();
            use graphql_client::GraphQLQuery;
            let request = <ReconcilerPullRequests as GraphQLQuery>::build_query(
                reconciler_pull_requests::Variables { ids },
            );
            let response: Result<reconciler_pull_requests::ResponseData, _> =
                github.graphql(&request).await;
            let Ok(response) = response else {
                // Fall back per-row: the batch is best-effort.
                for &(index, _) in &batch {
                    let number = lookups[index].number;
                    results[index] = Some(self.get_change_request(&repo, number).await?);
                }
                continue;
            };
            let nodes = response.nodes;
            for (index, id) in &batch {
                let number = lookups[*index].number;
                let node = nodes
                    .iter()
                    .flatten()
                    .filter_map(|node| match node {
                        reconciler_pull_requests::ReconcilerPullRequestsNodes::PullRequest(
                            pull,
                        ) if pull.id == *id => Some(pull.clone()),
                        _ => None,
                    })
                    .find(|pull| pull.number as u64 == number);
                if let Some(pull) = node {
                    results[*index] = Some(ChangeRequest {
                        number,
                        url: pull.url.clone(),
                        head_sha: pull.head_ref_oid.clone(),
                        state: match pull.state {
                            reconciler_pull_requests::PullRequestState::MERGED => CrState::Merged,
                            reconciler_pull_requests::PullRequestState::CLOSED => CrState::Closed,
                            reconciler_pull_requests::PullRequestState::OPEN => CrState::Open,
                            // An unknown state deserialises as Other:
                            // open is the conservative read (an
                            // open-looking CR keeps its wait pending,
                            // never skips a merge).
                            reconciler_pull_requests::PullRequestState::Other(_) => CrState::Open,
                        },
                        node_id: Some(id.clone()),
                    });
                } else {
                    // The id no longer names this CR: the REST truth.
                    results[*index] = Some(self.get_change_request(&repo, number).await?);
                }
            }
        }
        // Every lookup has gone through the REST path or the GraphQL
        // node by this point — a hole would be a bookkeeping bug, and
        // fabricating a state would be a correctness hole.
        let mut resolved = Vec::with_capacity(lookups.len());
        for (result, _) in results.into_iter().zip(lookups) {
            resolved.push(result.ok_or_else(|| {
                ProviderError::Request("a batched lookup went unresolved".to_owned())
            })?);
        }
        Ok(resolved)
    }

    async fn create_change_request(
        &self,
        repo: &CoreRepoRef,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        labels: &[String],
        draft: bool,
    ) -> ProviderResult<ChangeRequest> {
        let (owner, name) = owner_name(self, repo).await?;
        let github = self.client_for(repo).await?;
        let pulls = github.pulls(owner.clone(), name.clone());
        let mut builder = pulls.create(title, head, base).body(body);
        if draft {
            builder = builder.draft(true);
        }
        let pull = builder.send().await.map_err(request_failure)?;
        // labels apply separately (the create's belt-and-bed check).
        if !labels.is_empty() {
            github
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
        let github = self.client_for(repo).await?;
        let page = github
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
        let github = self.client_for(repo).await?;
        let repository = Self::repo_by_id(&github, repo)?
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
        let github = self.client_for(repo).await?;
        let mut content = Self::repo_by_id(&github, repo)?
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
        let github = self.client_for(repo).await?;
        github
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
        let github = self.client_for(repo).await?;
        github
            .issues(owner, name)
            .add_labels(cr_number, labels)
            .await
            .map_err(request_failure)?;
        Ok(())
    }

    async fn update_branch(&self, _repo: &CoreRepoRef, _branch: &str) -> ProviderResult<()> {
        // `builtin/update-branch@1` wires this in v1.1; the
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
        let github = self.client_for(repo).await?;
        Self::repo_by_id(&github, repo)?
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
        let github = self.client_for(repo).await?;
        github
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
        let github = self.client_for(repo).await?;
        let mut page = github
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
        // The `next` link's `page=N` query parameter (the no extra parsing
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
            page = github
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

    async fn delete_branch(&self, repo: &CoreRepoRef, branch: &str) -> ProviderResult<()> {
        let github = self.client_for(repo).await?;
        let handler = Self::git_by_id(&github, repo)?;
        handler
            .delete_ref(&Reference::Branch(branch.to_owned()))
            .await
            .map_err(request_failure)?;
        Ok(())
    }

    async fn check_branch_protection(
        &self,
        repo: &CoreRepoRef,
        branch: &str,
    ) -> ProviderResult<cargobike_core::provider::BranchProtection> {
        let (owner, name) = owner_name(self, repo).await?;
        let github = self.client_for(repo).await?;
        let protection = github
            .repos(owner, name)
            .branches()
            .protection(branch)
            .get()
            .await
            .map_err(request_failure)?;
        Ok(cargobike_core::provider::BranchProtection {
            requires_reviews: protection.required_pull_request_reviews.is_some(),
            required_review_count: protection
                .required_pull_request_reviews
                .and_then(|reviews| reviews.required_approving_review_count)
                .map(|count| count as u16),
        })
    }

    async fn check_tag_protection(
        &self,
        repo: &CoreRepoRef,
        tag_pattern: &str,
    ) -> ProviderResult<bool> {
        let (owner, name) = owner_name(self, repo).await?;
        let github = self.client_for(repo).await?;
        let rulesets = github
            .repos(owner.clone(), name.clone())
            .rulesets()
            .list()
            .send()
            .await
            .map_err(request_failure)?;
        Ok(rulesets
            .iter()
            .filter(|ruleset| {
                ruleset.target == Some(octocrab::models::rulesets::RulesetTarget::Tag)
                    && ruleset.enforcement == octocrab::models::rulesets::RulesetEnforcement::Active
            })
            .any(|ruleset| pattern_protects(tag_pattern, ruleset)))
    }

    async fn enable_auto_merge(&self, repo: &CoreRepoRef, number: u64) -> ProviderResult<()> {
        // The mutation keys on the CR's GLOBAL node id; fetch it once
        // (the REST shape) before the GraphQL exchange.
        let (owner, name) = owner_name(self, repo).await?;
        let github = self.client_for(repo).await?;
        let pull = github
            .pulls(owner.clone(), name.clone())
            .get(number)
            .await
            .map_err(request_failure)?;
        let Some(node_id) = pull.node_id else {
            return Err(ProviderError::Request(format!(
                "the CR `{number}` carries no node id; auto-merge could not key it"
            )));
        };
        let payload = serde_json::json!({
            "query": "mutation($gh_pr: ID!) { enablePullRequestAutoMerge(pullRequestId: $gh_pr, mergeMethod: MERGE) { pullRequest { number } } }",
            "variables": { "gh_pr": node_id }
        });
        let github = self.client_for(repo).await?;
        let response: serde_json::Value =
            github.graphql(&payload).await.map_err(request_failure)?;
        // octocrab unwraps the response's data member; a GraphQL errors
        // member is the mutation's refusal.
        if response.get("errors").is_some() {
            return Err(ProviderError::Request(format!(
                "the auto-merge refused to enable: {response:?}"
            )));
        }
        Ok(())
    }

    async fn verify_webhook(
        &self,
        headers: &[(&str, &str)],
        body: &[u8],
        secrets: &[secrecy::SecretString],
    ) -> ProviderResult<cargobike_core::webhook::NormalisedEvent> {
        crate::webhook::verify_and_normalise(headers, body, secrets)
    }

    async fn create_tag(&self, repo: &CoreRepoRef, tag: &str, sha: &str) -> ProviderResult<()> {
        let github = self.client_for(repo).await?;
        let handler = Self::git_by_id(&github, repo)?;
        handler
            .create_tag(tag, format!("release {tag}"), sha, "commit")
            .send()
            .await
            .map_err(request_failure)?;
        Ok(())
    }

    async fn create_release(
        &self,
        repo: &CoreRepoRef,
        tag: &str,
        name: &str,
        body: &str,
    ) -> ProviderResult<()> {
        let (owner, repo_name) = owner_name(self, repo).await?;
        let github = self.client_for(repo).await?;
        github
            .repos(owner, repo_name)
            .releases()
            .create(tag)
            .name(name)
            .body(body)
            .send()
            .await
            .map_err(request_failure)?;
        Ok(())
    }
}

/// Whether the ruleset actively protects the trigger's tag pattern
/// . The include list is GitHub glob syntax: `~ALL` covers every
/// ref; otherwise inclusion requires the pattern EQUALS (the breadth
/// comparison between globs is not well-defined, so the conservative
/// false refuses over-broad cases — documented).
fn pattern_protects(pattern: &str, ruleset: &octocrab::models::rulesets::Ruleset) -> bool {
    let Some(conditions) = &ruleset.conditions else {
        return false;
    };
    let Some(ref_name) = &conditions.ref_name else {
        return false;
    };
    ref_name
        .include
        .iter()
        .any(|glob| glob == "~ALL" || glob == pattern)
}

/// The octocrab repository ID out of the immutable string id .
fn reference_id(repo: &CoreRepoRef) -> Result<octocrab::models::RepositoryId, ProviderError> {
    let parsed: u64 = repo.id.parse().map_err(|_| {
        ProviderError::Request(format!("the repo id `{}` is not a number", repo.id))
    })?;
    Ok(parsed.into())
}

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
        node_id: pull.node_id.clone(),
    }
}
