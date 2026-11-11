//! The four built-in action steps. Each is stateless, delegates provider calls
//! and returns the output schema later steps reference.
//!
//! 's isolation rules: provider clients are constructed INSIDE the
//! step context lifetime; nothing escapes step scope.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value as JsonValue, json};

use cargobike_core::error::STEP_FAILED;
use cargobike_core::model::{CrState, Release, RepoRef};
use cargobike_core::provider::{Edit, Provider, ProviderError};
use cargobike_core::step::{EnvRef, StepContext, StepError, StepOutput, StepType};
use secrecy::ExposeSecret as _;

/// `builtin/commit-files@1` : branch then commit fused —
/// "already exists at the expected SHA" is success; the edits list is
/// structural (the never text substitution) and the resolved file paths are
/// confined by the registry's globs at provision time.
pub struct CommitFiles;

#[async_trait]
impl StepType for CommitFiles {
    fn name(&self) -> &str {
        "builtin/commit-files"
    }

    fn version(&self) -> &str {
        "1"
    }

    async fn execute(
        &self,
        ctx: &StepContext,
        release: &Release,
        env: &EnvRef,
        _params: &JsonValue,
    ) -> Result<StepOutput, StepError> {
        let repo = env.repo.clone().ok_or_else(env_ref_missing)?;
        let provider = provider_from_ctx(ctx, &repo)?;
        let branch = crate::names::branch_name(release, env);

        // create the branch once; a recovered attempt finds its SHA.
        let branch_tip = match provider.branch_sha(&repo, &branch).await {
            Ok(sha) => sha,
            Err(ProviderError::NotFound(_)) => {
                let base = provider.default_branch(&repo).await.map_err(step_failure)?;
                let base_sha = provider
                    .branch_sha(&repo, &base)
                    .await
                    .map_err(step_failure)?;
                provider
                    .create_branch(&repo, &branch, &base_sha)
                    .await
                    .map_err(step_failure)?;
                base_sha
            }
            Err(other) => return Err(step_failure(other)),
        };

        // fused half: edits + commit against the branch tip we just
        // established (the expected-parent; a replay run that already
        // committed will see the committed content — the provider's no-op
        // contract answers the same tip).
        if env.edits.is_empty() {
            return Ok(StepOutput::Continue(
                json!({ "branch": branch, "sha": branch_tip }),
            ));
        }
        let message = env
            .commit_message
            .clone()
            .unwrap_or_else(|| default_commit_message(release, env));
        let committed = provider
            .commit_files(
                &repo,
                &branch,
                &env.edits,
                &message,
                Some(branch_tip.as_str()),
            )
            .await
            .map_err(step_failure)?;
        Ok(StepOutput::Continue(json!({
            "branch": committed.branch,
            "sha": committed.sha,
        })))
    }
}

/// `builtin/change-request@1` : an open CR with this head
/// branch returns instead of a second.
pub struct ChangeRequest;

#[async_trait]
impl StepType for ChangeRequest {
    fn name(&self) -> &str {
        "builtin/change-request"
    }

    fn version(&self) -> &str {
        "1"
    }

    async fn execute(
        &self,
        ctx: &StepContext,
        release: &Release,
        env: &EnvRef,
        params: &JsonValue,
    ) -> Result<StepOutput, StepError> {
        let repo = env.repo.clone().ok_or_else(env_ref_missing)?;
        let provider = provider_from_ctx(ctx, &repo)?;
        let head = param_str(params, "branch").ok_or_else(|| param_missing("branch"))?;
        let defaults = provider.default_branch(&repo).await.map_err(step_failure)?;
        let base = param_str(params, "base").unwrap_or(&defaults).to_owned();
        let title = param_str(params, "title")
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{}: {}", env.name, release.spec.version));
        let body_text = param_str(params, "body")
            .unwrap_or("Automated release change request.")
            .to_owned();
        let labels = param_labels(params);
        // find by head branch first.
        let found = provider
            .find_change_request_by_head(&repo, head)
            .await
            .map_err(step_failure)?;
        if let Some(existing) = found {
            return Ok(StepOutput::Continue(cr_output(&existing)));
        }
        // F-65 fail-closed gate: a merge is only a review if the base
        // branch requires reviews. The check runs on the create path only,
        // so a recovery replay that finds its own CR never re-fails on a
        // protection change made mid-flight. Capability absence is a
        // refusal, not a skip: the default is `require_branch_protection:
        // true` and the registry-only flag (R11) is the opt-out.
        if env.require_branch_protection {
            if !provider.capabilities().branch_protection {
                return Err(StepError::Failed {
                    code: STEP_FAILED.to_owned(),
                    message: format!(
                        "the provider `{}` cannot check branch protection; refusing to open a \
                         change request against unprotected `{base}` (require_branch_protection)",
                        repo.provider
                    ),
                });
            }
            let protection = provider
                .check_branch_protection(&repo, base.as_str())
                .await
                .map_err(|failure| match failure {
                    // GitHub answers 404 on the protection endpoint for an
                    // unprotected branch: a definitive "not protected", not
                    // a transient lookup failure.
                    ProviderError::NotFound(_) => StepError::Failed {
                        code: STEP_FAILED.to_owned(),
                        message: format!(
                            "the base branch `{base}` has no protection record; a merge is \
                             only a review when it requires reviews (require_branch_protection)"
                        ),
                    },
                    // The capability is advertised but refused at runtime: no
                    // answer means no review guarantee, so the step fails closed.
                    ProviderError::Unsupported => StepError::Failed {
                        code: STEP_FAILED.to_owned(),
                        message: format!(
                            "the provider `{}` refused the branch protection check on `{base}`",
                            repo.provider
                        ),
                    },
                    other => step_failure(other),
                })?;
            if !protection.requires_reviews {
                return Err(StepError::Failed {
                    code: STEP_FAILED.to_owned(),
                    message: format!(
                        "the base branch `{base}` does not require reviews; a merge is only a \
                         review when it does (require_branch_protection)"
                    ),
                });
            }
        }
        let created = provider
            .create_change_request(&repo, head, base.as_str(), &title, &body_text, &labels)
            .await
            .map_err(step_failure)?;
        Ok(StepOutput::Continue(cr_output(&created)))
    }
}

/// The output schema for CR steps (`steps.<id>.outputs.{...}`).
fn cr_output(created: &cargobike_core::provider::ChangeRequest) -> JsonValue {
    json!({
        "number": created.number,
        "url": created.url,
        "head_sha": created.head_sha,
        "state": match created.state {
            CrState::Open => "open",
            CrState::Closed => "closed",
            CrState::Merged => "merged",
        },
    })
}

/// `builtin/http-call@1` : the SSRF-guarded client issues the
/// request; headers carry `{ secret: <name> }` references resolved by
/// the engine so the value never enters a step output .
pub struct HttpCall;

#[async_trait]
impl StepType for HttpCall {
    fn name(&self) -> &str {
        "builtin/http-call"
    }

    fn version(&self) -> &str {
        "1"
    }

    async fn execute(
        &self,
        ctx: &StepContext,
        _release: &Release,
        _env: &EnvRef,
        params: &JsonValue,
    ) -> Result<StepOutput, StepError> {
        let url = param_str(params, "url")
            .ok_or_else(|| param_missing("url"))?
            .to_owned();
        let method = param_str(params, "method").unwrap_or("POST").to_uppercase();
        let mut headers: Vec<(String, String)> = Vec::new();
        if let Some(map) = params.get("headers").and_then(JsonValue::as_object) {
            for (name, value) in map {
                if let Some(secret_reference) = value.get("secret").and_then(JsonValue::as_str) {
                    let resolved = ctx
                        .credentials
                        .resolve(secret_reference)
                        .map_err(step_failure_library)?;
                    headers.push((name.clone(), resolved.expose_secret().to_owned()));
                } else if let Some(value) = value.as_str() {
                    headers.push((name.clone(), value.to_owned()));
                }
            }
        }
        let body_bytes = match params.get("body") {
            Some(JsonValue::Null) | None => None,
            Some(value) => Some(
                serde_json::to_vec(value).map_err(|error| StepError::Failed {
                    code: STEP_FAILED.to_owned(),
                    message: format!("the body failed to encode: {error}"),
                })?,
            ),
        };
        let request = cargobike_core::step::HttpRequest {
            url,
            method: method.clone(),
            headers,
            body: body_bytes,
        };
        let response = ctx.http.send(request).await.map_err(http_failure)?;
        if response.status >= 500 {
            // Server-side failures are transient (the retry counts them).
            return Err(StepError::Transient {
                reason: format!("http {method} responded {}", response.status),
            });
        }
        if let Some(expected) = params.get("expect_status").and_then(JsonValue::as_u64) {
            if response.status != expected as u16 {
                return Err(StepError::Failed {
                    code: STEP_FAILED.to_owned(),
                    message: format!(
                        "http {method} responded {} (expected {expected})",
                        response.status
                    ),
                });
            }
        }
        Ok(StepOutput::Continue(json!({ "status": response.status })))
    }
}

/// `builtin/set-labels@1` : adds labels to a change request.
pub struct SetLabels;

#[async_trait]
impl StepType for SetLabels {
    fn name(&self) -> &str {
        "builtin/set-labels"
    }

    fn version(&self) -> &str {
        "1"
    }

    async fn execute(
        &self,
        ctx: &StepContext,
        _release: &Release,
        env: &EnvRef,
        params: &JsonValue,
    ) -> Result<StepOutput, StepError> {
        let repo = env.repo.clone().ok_or_else(env_ref_missing)?;
        let provider = provider_from_ctx(ctx, &repo)?;
        let number = param_u64(params, "cr_number").ok_or_else(|| param_missing("cr_number"))?;
        let labels = param_labels(params);
        provider
            .add_labels(&repo, number, &labels)
            .await
            .map_err(step_failure)?;
        Ok(StepOutput::Continue(json!({ "added": labels })))
    }
}

/// Registers the four built-ins (the startup's install).
pub fn register_builtins(registry: &mut crate::steps::StepRegistry) {
    registry.register_built_in(Arc::new(CommitFiles));
    registry.register_built_in(Arc::new(ChangeRequest));
    registry.register_built_in(Arc::new(HttpCall));
    registry.register_built_in(Arc::new(SetLabels));
}

/// The step's provider (the resolved by `RepoRef.provider`).
fn provider_from_ctx(ctx: &StepContext, repo: &RepoRef) -> Result<Arc<dyn Provider>, StepError> {
    ctx.providers
        .resolve(repo)
        .map_err(|error| StepError::Failed {
            code: STEP_FAILED.to_owned(),
            message: error.to_string(),
        })
}

fn env_ref_missing() -> StepError {
    StepError::Failed {
        code: STEP_FAILED.to_owned(),
        message: "the environment has no target repo".to_owned(),
    }
}

fn param_missing(name: &str) -> StepError {
    StepError::Failed {
        code: STEP_FAILED.to_owned(),
        message: format!("the `{name}` parameter is required"),
    }
}

fn param_str<'a>(params: &'a JsonValue, name: &str) -> Option<&'a str> {
    params.get(name).and_then(JsonValue::as_str)
}

fn param_u64(params: &JsonValue, name: &str) -> Option<u64> {
    params.get(name).and_then(JsonValue::as_u64)
}

fn param_labels(params: &JsonValue) -> Vec<String> {
    params
        .get("labels")
        .and_then(JsonValue::as_array)
        .map(|labels| {
            labels
                .iter()
                .filter_map(JsonValue::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn default_commit_message(release: &Release, env: &EnvRef) -> String {
    format!(
        "Release {} {} to {}",
        release.spec.application, release.spec.version, env.name
    )
}

fn step_failure(failure: ProviderError) -> StepError {
    match failure {
        ProviderError::NotFound(object) => StepError::Failed {
            code: STEP_FAILED.to_owned(),
            message: format!("{object}: not found"),
        },
        other => StepError::Transient {
            reason: other.to_string(),
        },
    }
}

fn step_failure_library(failure: cargobike_core::error::LibraryError) -> StepError {
    match failure {
        cargobike_core::error::LibraryError::UnknownSecret(name) => StepError::Failed {
            code: STEP_FAILED.to_owned(),
            message: format!("failed to resolve secret: no secret named `{name}`"),
        },
        other => StepError::Failed {
            code: STEP_FAILED.to_owned(),
            message: other.to_string(),
        },
    }
}

fn http_failure(failure: cargobike_core::provider::HttpError) -> StepError {
    match failure {
        cargobike_core::provider::HttpError::Timeout
        | cargobike_core::provider::HttpError::Connect(_) => StepError::Transient {
            reason: failure.to_string(),
        },
        other => StepError::Failed {
            code: STEP_FAILED.to_owned(),
            message: other.to_string(),
        },
    }
}

/// The F-2A provider's edit construction helper (the tests use it).
pub fn edit(file: &str, field: &str, value: JsonValue) -> Edit {
    Edit {
        file: file.to_owned(),
        format: Some(cargobike_core::edits::format_for_path(file)),
        field: field.to_owned(),
        value: Some(value),
    }
}

/// The interpreter's unified run of one action step invocation.
pub async fn execute_step_run(
    action: &Arc<dyn StepType>,
    step_context: &StepContext,
    release_view: &Release,
    environment_view: &EnvRef,
    params: &JsonValue,
) -> Result<StepOutput, StepError> {
    action
        .execute(step_context, release_view, environment_view, params)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::ReleaseSnapshot;
    use cargobike_core::provider::{BranchProtection, ChangeRequest as CrShape, ProviderResult};
    use cargobike_core::registry::CredentialStore;
    use cargobike_core::step::StepOutput;
    use secrecy::SecretString;

    /// A provider whose protection answers are fixed at construction:
    /// the F-65 gate's matrix needs capable/incapable and
    /// protected/unprotected shapes without a network.
    struct GateProvider {
        capable: bool,
        protection: GateProtection,
        existing: Option<CrShape>,
    }

    /// The protection endpoint's canned answer.
    #[derive(Clone)]
    enum GateProtection {
        Answer(BranchProtection),
        NotFound,
    }

    impl GateProvider {
        fn protected() -> Self {
            Self {
                capable: true,
                protection: GateProtection::Answer(BranchProtection {
                    requires_reviews: true,
                    required_review_count: Some(1),
                }),
                existing: None,
            }
        }
    }

    #[async_trait]
    impl cargobike_core::provider::Provider for GateProvider {
        fn capabilities(&self) -> cargobike_core::provider::ProviderCaps {
            cargobike_core::provider::ProviderCaps {
                branch_protection: self.capable,
                tag_protection: false,
                auto_merge: false,
                releases: false,
                tags: false,
                webhook_parsing: false,
                webhook_verification: false,
                branch_delete: false,
            }
        }

        async fn get_repo_path(&self, _repo: &RepoRef) -> ProviderResult<String> {
            Ok("gate/repo".to_owned())
        }

        async fn create_branch(
            &self,
            _repo: &RepoRef,
            _branch: &str,
            _from_sha: &str,
        ) -> ProviderResult<()> {
            Ok(())
        }

        async fn commit_files(
            &self,
            _repo: &RepoRef,
            _branch: &str,
            _edits: &[cargobike_core::provider::Edit],
            _message: &str,
            _expected_parent: Option<&str>,
        ) -> ProviderResult<cargobike_core::provider::CommitResult> {
            Ok(cargobike_core::provider::CommitResult {
                sha: "sha".to_owned(),
                branch: "branch".to_owned(),
            })
        }

        async fn create_change_request(
            &self,
            _repo: &RepoRef,
            _head: &str,
            _base: &str,
            _title: &str,
            _body: &str,
            _labels: &[String],
        ) -> ProviderResult<CrShape> {
            Ok(CrShape {
                number: 7,
                url: "https://cr/7".to_owned(),
                head_sha: "sha".to_owned(),
                state: CrState::Open,
            })
        }

        async fn get_change_request(
            &self,
            _repo: &RepoRef,
            _number: u64,
        ) -> ProviderResult<CrShape> {
            Err(ProviderError::Unsupported)
        }

        async fn find_change_request_by_head(
            &self,
            _repo: &RepoRef,
            _head: &str,
        ) -> ProviderResult<Option<CrShape>> {
            Ok(self.existing.clone())
        }

        async fn close_change_request(
            &self,
            _repo: &RepoRef,
            _number: u64,
            _comment: &str,
        ) -> ProviderResult<()> {
            Ok(())
        }

        async fn list_open_change_requests(&self, _repo: &RepoRef) -> ProviderResult<Vec<CrShape>> {
            Ok(Vec::new())
        }

        async fn read_file(
            &self,
            _repo: &RepoRef,
            _path: &str,
            _git_ref: &str,
        ) -> ProviderResult<Vec<u8>> {
            Ok(Vec::new())
        }

        async fn comment(
            &self,
            _repo: &RepoRef,
            _cr_number: u64,
            _body: &str,
        ) -> ProviderResult<()> {
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
            Err(ProviderError::Unsupported)
        }

        async fn default_branch(&self, _repo: &RepoRef) -> ProviderResult<String> {
            Ok("main".to_owned())
        }

        async fn branch_sha(&self, _repo: &RepoRef, _branch: &str) -> ProviderResult<String> {
            Ok("sha".to_owned())
        }

        async fn set_commit_status(
            &self,
            _repo: &RepoRef,
            _sha: &str,
            _status: cargobike_core::provider::CommitStatus,
        ) -> ProviderResult<()> {
            Ok(())
        }

        async fn check_branch_protection(
            &self,
            _repo: &RepoRef,
            _branch: &str,
        ) -> ProviderResult<BranchProtection> {
            match self.protection.clone() {
                GateProtection::Answer(answer) => Ok(answer),
                GateProtection::NotFound => Err(ProviderError::NotFound("branch protection")),
            }
        }
    }

    /// The no-network HTTP seam (the gate never calls out).
    struct NoHttp;

    #[async_trait]
    impl cargobike_core::step::HttpService for NoHttp {
        async fn send(
            &self,
            _request: cargobike_core::step::HttpRequest,
        ) -> Result<cargobike_core::step::HttpResponse, cargobike_core::provider::HttpError>
        {
            Err(cargobike_core::provider::HttpError::Other(
                "no network in tests".to_owned(),
            ))
        }
    }

    /// A credential store that refuses: the F-65 gate never resolves a
    /// secret, so a refusal proves no unexpected resolution happened.
    struct NoCredentials;

    impl CredentialStore for NoCredentials {
        fn resolve(&self, name: &str) -> Result<SecretString, cargobike_core::error::LibraryError> {
            Err(cargobike_core::error::LibraryError::UnknownSecret(
                name.to_owned(),
            ))
        }
    }

    /// The step context over one gate provider.
    fn context(provider: GateProvider) -> StepContext {
        let mut providers = cargobike_core::registry::ProviderRegistry::new();
        providers.insert("github", Arc::new(provider));
        StepContext {
            providers,
            http: Arc::new(NoHttp),
            credentials: Arc::new(NoCredentials),
            idempotency_key: "release/env/cr".to_owned(),
            cancel_token: cargobike_core::step::CancelToken::new(),
            log: tracing::Span::none(),
        }
    }

    /// A minimal release view (the gate reads only application/version).
    fn release() -> Release {
        let mut release = ReleaseSnapshot {
            template: crate::template::CompiledTemplate {
                name: "gate".to_owned(),
                version: "1".to_owned(),
                inputs: Default::default(),
                environment_inputs: Default::default(),
                environments: Vec::new(),
            },
            release: crate::snapshot::ReleaseIdentity {
                id: "0198c0de-0000-7000-8000-000000000001".to_owned(),
                application: "gate-app".to_owned(),
                version: "1.2.3".to_owned(),
                version_scheme: cargobike_core::version::VersionScheme::Semver,
            },
            inputs: Default::default(),
            step_type_versions: Default::default(),
            content_hash: String::new(),
        }
        .read_release();
        release.spec.template = "gate@1".to_owned();
        release
    }

    /// The environment view with the registry's protection flag.
    fn env_ref(require: bool) -> EnvRef {
        EnvRef {
            name: "production".to_owned(),
            repo: Some(RepoRef::new("github", "789012")),
            edits: Vec::new(),
            commit_message: None,
            branch_format: None,
            release_version: "1.2.3".to_owned(),
            require_branch_protection: require,
        }
    }

    fn params() -> JsonValue {
        json!({ "branch": "cargobike/gate-app/production/r1" })
    }

    #[tokio::test]
    async fn test_change_request_opens_when_the_base_branch_requires_reviews() {
        let output = ChangeRequest
            .execute(
                &context(GateProvider::protected()),
                &release(),
                &env_ref(true),
                &params(),
            )
            .await
            .expect("a review-protected base branch opens the CR");
        assert_eq!(
            output,
            StepOutput::Continue(
                json!({ "number": 7, "url": "https://cr/7", "head_sha": "sha", "state": "open" })
            )
        );
    }

    #[tokio::test]
    async fn test_change_request_fails_closed_when_the_provider_cannot_check_protection() {
        let mut provider = GateProvider::protected();
        provider.capable = false;
        let failure = ChangeRequest
            .execute(&context(provider), &release(), &env_ref(true), &params())
            .await
            .expect_err("an incapable provider is a refusal, not a skip");
        assert!(
            failure
                .to_string()
                .contains("cannot check branch protection")
        );
    }

    #[tokio::test]
    async fn test_change_request_fails_closed_when_the_base_branch_requires_no_reviews() {
        let mut provider = GateProvider::protected();
        provider.protection = GateProtection::Answer(BranchProtection {
            requires_reviews: false,
            required_review_count: None,
        });
        let failure = ChangeRequest
            .execute(&context(provider), &release(), &env_ref(true), &params())
            .await
            .expect_err("an unprotected base branch is refused");
        assert!(failure.to_string().contains("does not require reviews"));
    }

    #[tokio::test]
    async fn test_change_request_fails_closed_when_the_protection_record_is_missing() {
        let mut provider = GateProvider::protected();
        provider.protection = GateProtection::NotFound;
        let failure = ChangeRequest
            .execute(&context(provider), &release(), &env_ref(true), &params())
            .await
            .expect_err("a missing protection record is a refusal");
        assert!(failure.to_string().contains("no protection record"));
    }

    #[tokio::test]
    async fn test_change_request_skips_the_gate_when_the_registry_opts_out() {
        let mut provider = GateProvider::protected();
        provider.capable = false;
        let output = ChangeRequest
            .execute(&context(provider), &release(), &env_ref(false), &params())
            .await
            .expect("the registry-only opt-out skips the gate");
        assert!(matches!(output, StepOutput::Continue(_)));
    }

    #[tokio::test]
    async fn test_change_request_finding_its_own_cr_never_re_runs_the_gate() {
        // Recovery safety: a replay that finds the CR it opened before a
        // mid-flight protection change must not re-fail (F-19).
        let mut provider = GateProvider::protected();
        provider.capable = false;
        provider.existing = Some(CrShape {
            number: 7,
            url: "https://cr/7".to_owned(),
            head_sha: "sha".to_owned(),
            state: CrState::Open,
        });
        let output = ChangeRequest
            .execute(&context(provider), &release(), &env_ref(true), &params())
            .await
            .expect("the existing CR returns before the gate");
        assert!(matches!(output, StepOutput::Continue(_)));
    }
}
