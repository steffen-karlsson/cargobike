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
