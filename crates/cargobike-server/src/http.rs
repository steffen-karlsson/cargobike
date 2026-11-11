//! The server state, health surfaces and the leader
//! election gate (the minimal v1: try-advisory at boot; the
//! exit-process fence follows in 2.8b).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};

use crate::config::{Config, literal_secret_warnings, load};
use crate::db::RepositoryError;
use crate::http::errors::ApiError;
use crate::release::{ReleaseRepository, create_release_core};
use uuid::Uuid;

pub mod errors;

/// The engine's hosting in the state (the handles the handlers use).
use crate::engine::EngineHosting;

/// Shared server state.
pub struct AppState {
    /// The current config; a SIGHUP reload swaps this .
    pub config: tokio::sync::watch::Receiver<Arc<Config>>,
    /// Ready only when a started leader holds the election lock .
    pub ready: AtomicBool,
    /// Release reads/writes (2.3/2.4).
    pub releases: ReleaseRepository,
    /// Auth context (2.5/2.6).
    pub auth: Arc<crate::auth::AuthState>,
    /// The config file the reload task re-reads .
    pub config_path: Option<std::path::PathBuf>,
    /// The engine's hosting: the interpreter/cleanup/DBOS handles.
    pub engine: Arc<EngineHosting>,
    /// F-10 verified-path availability: the application → reason map
    /// of labels that do not check out against the provider.
    pub availability: Arc<crate::availability::Availability>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("ready", &self.ready.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

/// All Phase-2 routes: health + clientconfig unauthenticated ,
/// release endpoints carry their auth plumbing in milestone 2.5/2.6.
pub fn api_router(state: Arc<AppState>) -> Router<()> {
    let unauthenticated = Router::new()
        .route("/api/v1/live", get(live))
        .route("/api/v1/ready", get(ready))
        .route("/api/v1/startup", get(startup))
        .route("/api/v1/clientconfig", get(clientconfig))
        .with_state(Arc::clone(&state));
    let protected = Router::new()
        .route("/api/v1/releases", get(list_releases).post(create_release))
        .route(
            "/api/v1/releases/{id}",
            get(get_release).delete(delete_release),
        )
        .route("/api/v1/releases/{id}/cancel", post(cancel_release))
        .route("/api/v1/releases/{id}/retry", post(retry_release))
        .route("/api/v1/whoami", get(whoami))
        .route("/api/v1/applications", get(list_applications))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            crate::auth::require_auth,
        ))
        .with_state(Arc::clone(&state));
    let body_limit = crate::config::parse_size(&state.config.borrow().limits.api.max_body_size)
        .unwrap_or(1024 * 1024);
    // The webhook surface: no auth (the HMAC is the auth), its own
    // body-limit lane (5.1's WebhookVerify); the per-lane limit layers
    // the sub-router so the API's tighter cap never truncates the
    // deliveries.
    let webhook_limit =
        crate::config::parse_size(&state.config.borrow().limits.webhooks.max_body_size)
            .unwrap_or(25 * 1024 * 1024);
    let webhooks = Router::new()
        .route("/webhooks/{provider}", post(receive_webhook))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            webhook_limit as usize,
        ))
        .with_state(Arc::clone(&state));
    let rest =
        unauthenticated
            .merge(protected)
            .layer(tower_http::limit::RequestBodyLimitLayer::new(
                body_limit as usize,
            ));
    rest.merge(webhooks)
        // The 2.1 surface: trace, panic containment per RFC 9457, and
        // sensitive headers (never traced/logged) — the lanes' body
        // limits are layered before the merge.
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer::new(
            sensitive_headers(),
        ))
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(PanicProblem))
}

/// Headers the trace layer never puts in spans (the token values).
fn sensitive_headers() -> Vec<axum::http::HeaderName> {
    vec![
        axum::http::HeaderName::from_static("authorization"),
        axum::http::HeaderName::from_static("cookie"),
        axum::http::HeaderName::from_static("if-match"),
    ]
}

/// A handler's panic becomes the RFC 9457 internal problem; the panic's
/// own text stays out of the response.
#[derive(Clone, Default)]
struct PanicProblem;

impl tower_http::catch_panic::ResponseForPanic for PanicProblem {
    type ResponseBody = axum::body::Body;

    fn response_for_panic(
        &mut self,
        panic: std::boxed::Box<dyn std::any::Any + Send>,
    ) -> axum::response::Response<Self::ResponseBody> {
        use axum::http::StatusCode;
        use axum::response::IntoResponse as _;
        tracing::error!(
            ?panic,
            "a handler panicked; the request answers as an internal problem"
        );
        let body = serde_json::json!({
            "type": "https://cargobike.dev/errors/internal-error",
            "title": "Internal Server Error",
            "status": 500,
            "detail": "Internal request handling failed.",
            "instance": "",
            "code": "InternalError",
        });
        (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(body)).into_response()
    }
}

/// Lists releases : filters + the latest cursor .
#[derive(serde::Deserialize)]
struct ListParams {
    application: Option<String>,
    phase: Option<String>,
    version: Option<String>,
    since: Option<String>,
    limit: Option<u32>,
    after: Option<Uuid>,
    before: Option<Uuid>,
}

async fn list_releases(
    State(state): State<Arc<AppState>>,
    axum::Extension(caller): axum::Extension<crate::auth::AuthedCaller>,
    axum::extract::Query(params): axum::extract::Query<ListParams>,
) -> Result<axum::Json<serde_json::Value>, ApiError> {
    if !caller.has_grant("release:read") {
        return Err(ApiError::forbidden("The `release:read` grant is required."));
    }
    let since = match &params.since {
        Some(text) => Some(
            sqlx::types::time::OffsetDateTime::parse(
                text,
                &time::format_description::well_known::Rfc3339,
            )
            .map_err(|error| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    crate::http::errors::INVALID_REQUEST,
                    "field-invalid",
                    format!("failed to parse `since` as RFC 3339: {error}"),
                )
            })?,
        ),
        None => None,
    };
    let limit = params.limit.unwrap_or(50).clamp(1, 500); // documented bound
    let (items, cursor) = state
        .releases
        .list(
            params.application.as_deref(),
            params.phase.as_deref(),
            params.version.as_deref(),
            since,
            limit,
            params.after,
            params.before,
        )
        .await
        .map_err(repository_to_api)?;
    let looked = items;
    Ok(axum::Json(serde_json::json!({
        "items": looked,
        "cursor": cursor,
        "has_more": cursor.is_some(),
    })))
}

/// Create is `{application, version}` only ; duplicates answer 200
/// with the existing release ; a fresh row answers 202 + `Location`
/// .
async fn create_release(
    State(state): State<Arc<AppState>>,
    axum::Extension(caller): axum::Extension<crate::auth::AuthedCaller>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Result<axum::response::Response, ApiError> {
    let _ = &caller; // authorization below 
    // F-109: the caller-supplied create identity — a retried request
    // with the same key converges on the first attempt's release.
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned);
    let application = body
        .get("application")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                crate::http::errors::INVALID_REQUEST,
                "field-required",
                "The field `application` is required.",
            )
        })?
        .to_owned();
    let version = body
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                crate::http::errors::INVALID_REQUEST,
                "field-required",
                "The field `version` is required.",
            )
        })?
        .to_owned();

    // An API-key create may supply the SHA the release is cut from
    // (F-95: no SHA rides the token, so the body's is verified
    // against the provider's tag object; the token's own claim — a
    // VERIFIED value — always wins over the body's).
    let body_sha = body.get("sha").and_then(serde_json::Value::as_str);
    let provenance_sha = caller.sha_claim.as_deref().or(body_sha);

    let config = state.config.borrow().clone();
    let app = match crate::auth::authorize_create(&config, &caller, &application) {
        Ok(app) => app,
        Err(crate::auth::AuthError::ForbiddenResource) => {
            return match state.config.borrow().application(&application) {
                Some(_) => Err(ApiError::forbidden(
                    "The caller is not authorized to release this application.",
                )),
                None => Err(ApiError::new(
                    StatusCode::NOT_FOUND,
                    "ApplicationNotFound",
                    "application-not-found",
                    format!("The application `{application}` is not in the registry."),
                )),
            };
        }
        Err(error) => return Err(auth_to_api(&error)),
    };

    // F-83/F-84: the release records the caller's verified identity
    // (issuer + subject) and, when the token described a CI run, the
    // verified CI context — never body-supplied values.
    let caller_actor = crate::release::ReleaseActor {
        actor: cargobike_core::model::Actor {
            issuer: caller.issuer.clone(),
            subject: caller.subject.clone(),
            display_name: caller.display_name.clone(),
        },
        ci: caller.ci.clone(),
    };
    let outcome = create_release_core(
        state.as_ref(),
        app,
        &application,
        &version,
        crate::release::CreateOrigin {
            actor: Some(&caller_actor),
            retried_from: None,
            idempotency_key: idempotency_key.as_deref(),
            sha: provenance_sha,
        },
    )
    .await?;
    if !outcome.created {
        // a duplicate create answers 200 with the existing release.
        return Ok((StatusCode::OK, axum::Json(outcome.document)).into_response());
    }
    let location = format!(
        "/api/v1/releases/{}",
        outcome
            .document
            .pointer("/metadata/id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
    );
    Ok((
        StatusCode::ACCEPTED,
        [("Location", location.as_str())],
        axum::Json(outcome.document),
    )
        .into_response())
}

use axum::response::IntoResponse;

/// cancel is a dedicated endpoint starting a cleanup workflow .
async fn cancel_release(
    State(state): State<Arc<AppState>>,
    axum::Extension(caller): axum::Extension<crate::auth::AuthedCaller>,
    headers: axum::http::HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    // canceling is a grant .
    if !caller.has_grant("release:cancel") {
        return Err(ApiError::forbidden(
            "The `release:cancel` grant is required.",
        ));
    }
    // The If-Match guard: a supplied header pins the resource_version
    // (ETag's value with quotes tolerated).
    let match_text = headers_if_match(&headers);
    let expected_version = if match_text.is_empty() {
        None
    } else {
        let unquoted = match_text
            .trim()
            .trim_start_matches('"')
            .trim_end_matches('"');
        match unquoted.parse::<u64>() {
            Ok(version) => Some(version),
            Err(_) => {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    crate::http::errors::INVALID_REQUEST,
                    "field-invalid",
                    "The If-Match header must carry the resource version (an ETag number).",
                ));
            }
        }
    };
    let now = sqlx::types::time::OffsetDateTime::now_utc();
    match state
        .releases
        .set_phase(&id, "Canceled", true, now, expected_version)
        .await
    {
        Ok(()) => {
            // The cancelled workflow (stops at its next operation); the
            // cleanup workflow closes the CRs, deletes the branches and
            // settles the leases.
            let workflow_id = format!("cargobike/interpret/{}", id);
            let _ = state.engine.instance.cancel(&workflow_id).await;
            if let Err(failure) = start_cleanup(&state, id).await {
                tracing::error!(release = %id, %failure, "the cleanup workflow refused to start");
            }
            Ok(StatusCode::NO_CONTENT)
        }
        Err(RepositoryError::Conflict) => Err(ApiError::new(
            StatusCode::CONFLICT,
            crate::http::errors::STATE_CONFLICT,
            "state-conflict",
            "The release is already terminal; cancel only applies to in-flight releases.",
        )),
        Err(other) => Err(repository_to_api(other)),
    }
}

/// The cleanup workflow's start: the targets read the release's
/// environments status (the open CRs the interpreter recorded); a
/// cancelled release's books are source enough for the compensation.
async fn start_cleanup(state: &AppState, id: Uuid) -> Result<(), String> {
    let Some(document) = state.releases.get(&id).await.ok() else {
        return Err(format!("failed to read the release {id} for its cleanup"));
    };
    let environments = document
        .pointer("/status/environments")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let application = document
        .pointer("/spec/application")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let branch_format = state.config.borrow().engine.branch_format.clone();
    let mut targets: Vec<cargobike_engine::CleanupTarget> = Vec::new();
    for environment in environments {
        let Some(name) = environment.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let change_request = environment
            .get("change_request")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let repo = change_request
            .get("target_repo")
            .cloned()
            .and_then(|repo| serde_json::from_value(repo).ok());
        let branch = cargo_branch_name(&branch_format, &application, name, &id);
        // The CR summary the cleanup needs: number, repo, head sha.
        let number = change_request
            .get("number")
            .and_then(serde_json::Value::as_u64);
        let head_sha = change_request
            .get("head_sha")
            .and_then(serde_json::Value::as_str);
        let change_request = match (repo, number, head_sha) {
            (Some(target_repo), Some(number), Some(head_sha)) => {
                Some(cargobike_engine::CrSummary {
                    number,
                    repo: target_repo,
                    head_sha: head_sha.to_owned(),
                })
            }
            _ => None,
        };
        targets.push(cargobike_engine::CleanupTarget {
            application: application.clone(),
            environment: name.to_owned(),
            branch: Some(branch),
            change_request,
            superseded_by: None,
            to_version: None,
        });
    }
    state
        .engine
        .cleanup
        .start_with(
            cargobike_engine::CleanupArgs {
                release_id: id.to_string(),
                targets,
            },
            dbos::StartOptions::default(),
        )
        .await
        .map_err(|failure| format!("the cleanup's start failed: {failure}"))?;
    Ok(())
}

/// The release branch's name (the engine's deterministic default).
fn cargo_branch_name(
    format: &str,
    application: &str,
    environment: &str,
    release_id: &Uuid,
) -> String {
    let values = std::collections::BTreeMap::from([
        ("application".to_owned(), application.to_owned()),
        ("environment".to_owned(), environment.to_owned()),
        ("release_id".to_owned(), release_id.to_string()),
    ]);
    cargobike_engine::names::expand(format, &values)
}

/// The If-Match header's value (empty ⇒ an unconditional transition).
fn headers_if_match(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("if-match")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

async fn get_release(
    State(state): State<Arc<AppState>>,
    axum::Extension(caller): axum::Extension<crate::auth::AuthedCaller>,
    Path(id): Path<Uuid>,
) -> Result<axum::Json<serde_json::Value>, ApiError> {
    // reading is a grant too .
    if !caller.has_grant("release:read") {
        return Err(ApiError::forbidden("The `release:read` grant is required."));
    }
    let document = state.releases.get(&id).await.map_err(repository_to_api)?;
    Ok(axum::Json(document))
}

/// Terminal-only delete ; event log retained .
async fn delete_release(
    State(state): State<Arc<AppState>>,
    axum::Extension(caller): axum::Extension<crate::auth::AuthedCaller>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    // deletion is a grant .
    if !caller.has_grant("release:delete") {
        return Err(ApiError::forbidden(
            "The `release:delete` grant is required.",
        ));
    }
    state
        .releases
        .delete_terminal(&id)
        .await
        .map_err(repository_to_api)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/releases/{id}/retry` (F-23/US-7): the in-place fork
/// retries a FAILED release from the last failure point (the same ID, a
/// new attempt appended with `fork_from`); the `--new` body flag copies
/// the release into a fresh row with `retried_from` set. Both need the
/// release's terminal state; the fork's re-arm is `Failed`-only.
async fn retry_release(
    State(state): State<Arc<AppState>>,
    axum::Extension(caller): axum::Extension<crate::auth::AuthedCaller>,
    Path(id): Path<Uuid>,
    body: Option<axum::Json<serde_json::Value>>,
) -> Result<axum::response::Response, ApiError> {
    use crate::http::errors;
    if !caller.has_grant("release:create") {
        return Err(ApiError::forbidden(
            "The `release:create` grant is required to retry a release.",
        ));
    }
    let fork_new = body
        .as_ref()
        .and_then(|json| json.0.get("new"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let document = state.releases.get(&id).await.map_err(repository_to_api)?;
    let application = document
        .pointer("/spec/application")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let version = document
        .pointer("/spec/version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let phase = document
        .pointer("/status/phase")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();

    if !fork_new {
        // The in-place fork: a failed release only (a canceled release's
        // own books are settled; `--new` is the way back from those).
        if phase != "Failed" {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                errors::STATE_CONFLICT,
                "state-conflict",
                format!(
                    "A failed release forks in place; this release is `{phase}`. Use `--new` to copy it."
                ),
            ));
        }
        // The last attempt's workflow id: the fork's source.
        let Some(source) = document
            .pointer("/status/attempts")
            .and_then(serde_json::Value::as_array)
            .and_then(|attempts| attempts.last())
            .and_then(|attempt| attempt.get("workflow_id"))
            .and_then(serde_json::Value::as_str)
        else {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                errors::STATE_CONFLICT,
                "state-conflict",
                "The failed release carries no attempt to fork.",
            ));
        };
        let retry_id = cargobike_engine::retry_workflow_id(id.to_string().as_str(), source);
        if let Err(failure) = state
            .engine
            .retry
            .start_with(
                cargobike_engine::RetryArgs {
                    release_id: id.to_string(),
                },
                dbos::StartOptions {
                    workflow_id: Some(retry_id.as_str()),
                    ..dbos::StartOptions::default()
                },
            )
            .await
        {
            // A started-and-refused retry (the workflow's own guards
            // re-check: the phase may have moved between the read
            // and the start) surfaces as a conflict.
            tracing::warn!(release = %id, %failure, "the retry's start refused");
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                errors::STATE_CONFLICT,
                "state-conflict",
                "The retry's start refused; the release may have moved.",
            ));
        }
        let refreshed = state.releases.get(&id).await.map_err(repository_to_api)?;
        return Ok((StatusCode::ACCEPTED, axum::Json(refreshed)).into_response());
    }

    // The `--new` copy: any terminal release; the fresh row's
    // `retried_from` names the original.
    if !matches!(
        phase.as_str(),
        "Failed" | "Canceled" | "Superseded" | "Completed"
    ) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            errors::STATE_CONFLICT,
            "state-conflict",
            format!("The release is still in flight (`{phase}`); retry `--new` after it settles."),
        ));
    }
    let config = state.config.borrow().clone();
    let Some(app) = config.application(&application) else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            errors::PROVIDER_NOT_FOUND,
            "application-not-found",
            format!("The application `{application}` is not in the registry."),
        ));
    };
    let caller_actor = crate::release::ReleaseActor {
        actor: cargobike_core::model::Actor {
            issuer: caller.issuer.clone(),
            subject: caller.subject.clone(),
            display_name: caller.display_name.clone(),
        },
        ci: caller.ci.clone(),
    };
    let retried_from = id.to_string();
    let outcome = crate::release::create_release_core(
        state.as_ref(),
        app,
        &application,
        &version,
        crate::release::CreateOrigin {
            actor: Some(&caller_actor),
            retried_from: Some(retried_from.as_str()),
            idempotency_key: None,
            sha: caller.sha_claim.as_deref(),
        },
    )
    .await?;
    let location = format!(
        "/api/v1/releases/{}",
        outcome
            .document
            .pointer("/metadata/id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
    );
    let status = if outcome.created {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        [("Location", location.as_str())],
        axum::Json(outcome.document),
    )
        .into_response())
}

/// Maps repository errors to the RFC 9457 vocabulary .
fn repository_to_api(error: RepositoryError) -> ApiError {
    match error {
        RepositoryError::NotFound(id) => ApiError::new(
            StatusCode::NOT_FOUND,
            crate::http::errors::RELEASE_NOT_FOUND,
            "release-not-found",
            format!("The release `{id}` was not found."),
        ),
        e => {
            // 's internal hygiene: the driver's error text stays in
            // the server's logs; the served problem is generic.
            tracing::error!(%e, "the release repository failed");
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                crate::http::errors::INTERNAL_ERROR,
                "internal-error",
                "The release store failed; the server's log has the detail.",
            )
        }
    }
}

async fn live() -> impl axum::response::IntoResponse {
    StatusCode::OK
}

async fn ready(State(state): State<Arc<AppState>>) -> (StatusCode, &'static str) {
    if state.ready.load(Ordering::Acquire) {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "standby")
    }
}

async fn startup() -> impl axum::response::IntoResponse {
    StatusCode::OK
}

/// Issuer + audience hints only (the safe auto-discovery).
async fn clientconfig(
    State(state): State<Arc<AppState>>,
) -> Result<axum::Json<serde_json::Value>, ApiError> {
    let config = state.config.borrow().clone();
    let issuers: Vec<String> = config
        .auth
        .oidc
        .iter()
        .map(|entry| entry.issuer.clone())
        .collect();
    let audiences: Vec<String> = config
        .auth
        .oidc
        .iter()
        .map(|entry| entry.audience.clone())
        .collect();
    Ok(axum::Json(
        serde_json::json!({ "issuers": issuers, "audiences": audiences }),
    ))
}

/// Advisory-lock attempt for minimal leader election (the minimal v1: a standby
/// still serves webhooks; the exit-on-lock-loss fence lands with the executor).
pub async fn elect_leader(
    pool: &sqlx::PgPool,
    enabled: bool,
    lock: &AtomicBool,
) -> Result<(), sqlx::Error> {
    if !enabled {
        lock.store(true, Ordering::Release);
        return Ok(());
    }
    let held: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind::<i64>(LOCK_ID)
        .fetch_one(pool)
        .await?;
    lock.store(held, Ordering::Release);
    Ok(())
}

/// The Cargobike leader-election lock key.
pub const LOCK_ID: i64 = 0x0063_6172_676f_626b; // 'cargobk'

/// Boots the config with the startup warnings, the leader
/// gate and the router ready to serve.
pub async fn boot(
    config_path: Option<&std::path::Path>,
    provider_override: crate::engine::ProviderOverride,
) -> Result<(Router<()>, Arc<AppState>), crate::config::ConfigError> {
    let config = Arc::new(load(config_path)?);
    for literal in literal_secret_warnings(&config) {
        tracing::warn!("{literal}");
    }
    // semantic validation before anything else starts half-authorised.
    if let Err(error) = crate::validation::validate(&config) {
        return Err(crate::config::ConfigError::Parse(error.to_string()));
    }
    let pool = crate::db::connect(&config).await?;
    let (config_tx, config_rx) = tokio::sync::watch::channel(Arc::clone(&config));
    let auth = Arc::new(crate::auth::AuthState::new(config_rx.clone())?);
    // F-10: the verified-path availability view is created before the
    // engine hosting (the webhook's tag-push creator refuses against
    // it too), then the boot check fills it: a mismatch (or an
    // unverifiable label) marks the application unavailable — the
    // server does NOT fail (the strict, fail-on-error check is
    // `cargobike validate --resolve`, 4.8) — and the background sweep
    // retries against the provider.
    let availability = Arc::new(crate::availability::Availability::new());
    let engine = crate::engine::host(
        &config,
        config_rx.clone(),
        pool.clone(),
        provider_override,
        Arc::clone(&availability),
    )
    .await?;
    availability
        .refresh(&config, &engine.services.providers)
        .await;
    availability.spawn_background_sweep(config_rx.clone(), engine.services.providers.clone());
    let state = Arc::new(AppState {
        config: config_rx,
        ready: AtomicBool::new(false),
        releases: ReleaseRepository::new(pool.clone()),
        auth,
        config_path: config_path.map(|path| path.to_path_buf()),
        engine: Arc::new(engine),
        availability,
    });
    spawn_sighup_reload(Arc::clone(&state), config_tx);
    elect_leader(&pool, config.leader_election.enabled, &state.ready)
        .await
        .map_err(|e| crate::config::ConfigError::Parse(format!("leader election failed: {e}")))?;
    Ok((api_router(Arc::clone(&state)), state))
}

/// SIGHUP hot reload: re-load, re-validate, then swap the config
/// the whole server reads from. A broken reload logs and keeps the
/// previous config (the in-flight releases are pinned by / anyway).
fn spawn_sighup_reload(
    state: Arc<AppState>,
    sender: tokio::sync::watch::Sender<Arc<crate::config::Config>>,
) {
    tokio::spawn(async move {
        let Ok(mut signals) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        else {
            return; // not a unix host: reload by restart ( Recreate)
        };
        while signals.recv().await.is_some() {
            let path = state.config_path.clone().unwrap_or_else(|| {
                std::path::PathBuf::from("/etc/cargobike/config.yaml") // default
            });
            match crate::config::load(Some(&path)) {
                Ok(fresh) => match crate::validation::validate(&fresh) {
                    Ok(()) => {
                        tracing::info!(reload = "sighup", "config reloaded");
                        let _ = sender.send(Arc::new(fresh));
                    }
                    Err(error) => {
                        tracing::error!(%error, "reload refused: keeping the current config")
                    }
                },
                Err(error) => {
                    tracing::error!(%error, "reload failed to load: keeping the current config")
                }
            }
        }
    });
}

/// the resolved identity + grants (the summary in JSON).
async fn whoami(
    axum::Extension(caller): axum::Extension<crate::auth::AuthedCaller>,
) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "origin": caller.origin,
        "issuer": caller.issuer,
        "subject": caller.subject,
        "display_name": caller.display_name,
        "grants": caller.grants,
        "bootstrap": caller.bootstrap,
    }))
}

/// `GET /api/v1/applications` (F-106): the registered applications
/// with their F-10 availability — a verified-path mismatch (or an
/// unverifiable label) shows here with its reason, the same reason a
/// create refuses with.
async fn list_applications(
    State(state): State<Arc<AppState>>,
    axum::Extension(_caller): axum::Extension<crate::auth::AuthedCaller>,
) -> axum::Json<serde_json::Value> {
    let config = state.config.borrow().clone();
    let unavailable = state.availability.snapshot();
    let applications: Vec<serde_json::Value> = config
        .applications
        .iter()
        .map(|app| {
            let reason = unavailable.get(&app.name).cloned();
            serde_json::json!({
                "name": app.name,
                "description": app.description,
                "provider": app.source.provider,
                "repository": {
                    "id": app.source.id,
                    "path": app.source.path,
                },
                "template": app.template,
                "labels": app.labels,
                "paused": crate::release::paused_reason(app.paused.as_ref()).is_some(),
                "available": reason.is_none(),
                "unavailable_reason": reason,
            })
        })
        .collect();
    axum::Json(serde_json::json!({ "applications": applications }))
}

/// `POST /webhooks/{provider}` (5.1): verify before parse, the
/// per-surface body limit is the router's layer, duplicate deliveries
/// deduplicate (the delivery id IS the workflow id), unrecognized
/// events acknowledge 200 and are ignored; acted-upon events fast-ack
/// 202 while the webhook workflow follows through.
async fn receive_webhook(
    State(state): State<Arc<AppState>>,
    Path(provider_name): Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<axum::response::Response, ApiError> {
    use crate::http::errors;
    let Some(secrets) = state
        .engine
        .webhook_secrets
        .get(&provider_name)
        .filter(|s| !s.is_empty())
    else {
        // An unknown provider's endpoint never even exists (no secret
        // configured = no receiver).
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            errors::PROVIDER_NOT_FOUND,
            "provider-not-found",
            format!("No webhook receiver for the provider `{provider_name}`."),
        ));
    };
    let secrets = secrets.clone();
    let probe = cargobike_core::model::RepoRef::new(&provider_name, "0");
    let provider = state
        .engine
        .services
        .providers
        .resolve(&probe)
        .map_err(|failure| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                errors::PROVIDER_NOT_FOUND,
                "provider-not-found",
                format!("The provider `{provider_name}` is not registered: {failure}."),
            )
        })?;
    let pairs: Vec<(&str, &str)> = headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value_text| (name.as_str(), value_text))
        })
        .collect();
    let event = match provider.verify_webhook(&pairs, &body, &secrets).await {
        Ok(event) => event,
        Err(failure) => {
            // The provider's contract: the transport rejection is the
            // 401 and forgets; the detailed reason stays out.
            tracing::warn!(provider = %provider_name, "the webhook delivery refused");
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                errors::WEBHOOK_UNTRUSTED,
                "webhook-untrusted",
                format!("The delivery does not carry a trusted signature ({failure})."),
            ));
        }
    };
    let ignores = matches!(
        event,
        cargobike_core::webhook::NormalisedEvent::Unrecognised { .. }
    );
    let delivery = headers
        .get("x-github-delivery")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    if ignores {
        // 200 acknowledged (the payload is retained only for the audit
        // surface once the event log lands).
        return Ok((
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "received": true,
                "action": "ignored",
            })),
        )
            .into_response());
    }
    // F-49/F-51: the delivery's installation and repository are
    // checked against the provider's allowlist. The signature already
    // proved the sender — a violation here is a mis-scoped App, so the
    // delivery is acknowledged 202 and ignored (no action, no probe
    // feedback), with the reason logged.
    if let Some(reason) = allowlist_reason(&state, &provider_name, &event) {
        tracing::warn!(
            provider = %provider_name,
            %reason,
            "the webhook delivery is outside the provider's allowlist: ignored"
        );
        return Ok((
            StatusCode::ACCEPTED,
            axum::Json(serde_json::json!({
                "received": true,
                "action": "ignored",
            })),
        )
            .into_response());
    }
    let args = cargobike_engine::WebhookArgs {
        provider: provider_name.clone(),
        delivery_id: delivery,
        event,
    };
    let workflow_id = format!("cargobike/webhook/{provider_name}/{}", args.delivery_id);
    if let Err(failure) = state
        .engine
        .webhook
        .start_with(
            args,
            dbos::StartOptions {
                workflow_id: Some(workflow_id.as_str()),
                ..dbos::StartOptions::default()
            },
        )
        .await
    {
        tracing::error!(%failure, "the webhook workflow refused to start");
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            errors::INTERNAL_ERROR,
            "internal-error",
            "The delivery was verified but the follow-through refused to start.",
        ));
    }
    Ok((
        StatusCode::ACCEPTED,
        axum::Json(serde_json::json!({
            "received": true,
            "action": "accepted",
        })),
    )
        .into_response())
}

/// The provider allowlist verdict (F-49/F-51/F-92): `Some(reason)` =
/// the delivery is outside the provider's allowlist and must be
/// ignored. The checks, deny-by-default per F-92:
///
/// - `repositories.deny` globs match the delivery's repository path →
///   denied (deny wins over allow).
/// - `repositories.allow` non-empty: the path must match at least one
///   allow glob. A delivery WITHOUT a path cannot match — denied
///   fail-closed.
/// - A pinned `auth.installation_id`: the delivery's installation
///   must equal it (a delivery from another installation is another
///   App scope entirely). The per-repo shape (F-122, no pin) has the
///   repository allowlist as its boundary instead.
///
/// No `repositories` block and no installation pin = the provider is
/// unscoped (v1's default; the audit's defence-in-depth note) — allowed.
fn allowlist_reason(
    state: &Arc<AppState>,
    provider_name: &str,
    event: &cargobike_core::webhook::NormalisedEvent,
) -> Option<String> {
    let config = state.config.borrow().clone();
    let provider = config
        .providers
        .iter()
        .find(|provider| provider.name == provider_name)?;
    allowlist_verdict(provider, event)
}

/// The pure verdict (F-49/F-51/F-92): `Some(reason)` = the delivery is
/// outside the provider's allowlist and must be ignored. The checks,
/// deny-by-default per F-92:
///
/// - A pinned `auth.installation_id`: the delivery's installation
///   must equal it (a delivery from another installation is another
///   App scope entirely). The per-repo shape (F-122, no pin) has the
///   repository allowlist as its boundary instead.
/// - `repositories.deny` globs match the delivery's repository path →
///   denied (deny wins over allow).
/// - `repositories.allow` non-empty: the path must match at least one
///   allow glob. A delivery WITHOUT a path cannot match — denied
///   fail-closed.
///
/// No installation pin and no `repositories` block = the provider is
/// unscoped (v1's default; the audit's defence-in-depth note) —
/// allowed.
fn allowlist_verdict(
    provider: &crate::config::ProviderConfig,
    event: &cargobike_core::webhook::NormalisedEvent,
) -> Option<String> {
    if let Some(pinned) = provider
        .auth
        .as_ref()
        .and_then(|auth| auth.installation_id.as_deref())
    {
        if event
            .installation_id()
            .is_some_and(|delivery| pinned.parse::<u64>().is_ok_and(|pinned| pinned != delivery))
        {
            return Some(format!(
                "the delivery's installation does not match the provider's pinned installation `{pinned}`"
            ));
        }
    }
    let set = provider.repositories.as_ref()?;
    let matcher = |globs: &[String]| {
        globs
            .iter()
            .filter_map(|pattern| {
                globset::GlobBuilder::new(pattern)
                    .literal_separator(true)
                    .build()
                    .ok()
                    .map(|glob| glob.compile_matcher())
            })
            .collect::<Vec<_>>()
    };
    let deny = matcher(&set.deny);
    let allow = matcher(&set.allow);
    let path = event.repository_path();
    if let Some(path) = path {
        if deny.iter().any(|glob| glob.is_match(path)) {
            return Some(format!("the repository `{path}` matches the deny list"));
        }
    }
    if !set.allow.is_empty() {
        let matched = path.is_some_and(|path| allow.iter().any(|glob| glob.is_match(path)));
        if !matched {
            return Some(match path {
                Some(path) => {
                    format!("the repository `{path}` is not on the allow list")
                }
                None => {
                    "the delivery carries no repository path to match the allow list".to_owned()
                }
            });
        }
    }
    None
}

/// Maps an auth failure to the problem response when it surfaces in a
/// handler (the rather than the middleware).
fn auth_to_api(error: &crate::auth::AuthError) -> ApiError {
    use crate::auth::AuthError;
    let (status, slug) = match error {
        AuthError::MissingToken => (StatusCode::UNAUTHORIZED, "missing-token"),
        AuthError::InvalidToken => (StatusCode::UNAUTHORIZED, "invalid-token"),
        AuthError::TokenExpired => (StatusCode::UNAUTHORIZED, "token-expired"),
        AuthError::ForbiddenResource => (StatusCode::FORBIDDEN, "forbidden-resource"),
    };
    ApiError::new(status, error.code(), slug, format!("{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProviderAuth, ProviderConfig, RepositorySet, SecretValue};

    fn provider_with(
        repositories: Option<RepositorySet>,
        installation: Option<&str>,
    ) -> ProviderConfig {
        ProviderConfig {
            name: "github".to_owned(),
            r#type: "github".to_owned(),
            api_url: None,
            web_url: None,
            auth: installation.map(|pinned| ProviderAuth {
                app_id: "123".to_owned(),
                installation_id: Some(pinned.to_owned()),
                private_key: SecretValue::Env {
                    env: "never_read".to_owned(),
                },
            }),
            webhook_secrets: None,
            repositories,
            extension: None,
        }
    }

    fn tag_push(
        path: Option<&str>,
        installation: Option<u64>,
    ) -> cargobike_core::webhook::NormalisedEvent {
        cargobike_core::webhook::NormalisedEvent::TagPush(cargobike_core::webhook::TagPush {
            tag: "v1.0.0".to_owned(),
            sha: "abc".to_owned(),
            repository_id: "42".to_owned(),
            sender: "ska".to_owned(),
            repository_path: path.unwrap_or_default().to_owned(),
            installation_id: installation,
        })
    }

    fn set(allow: &[&str], deny: &[&str]) -> RepositorySet {
        RepositorySet {
            allow: allow.iter().map(|glob| glob.to_string()).collect(),
            deny: deny.iter().map(|glob| glob.to_string()).collect(),
        }
    }

    #[test]
    fn test_an_unscoped_provider_allows_everything() {
        let provider = provider_with(None, None);
        assert!(allowlist_verdict(&provider, &tag_push(None, None)).is_none());
    }

    #[test]
    fn test_the_installation_pin_refuses_foreign_installations() {
        let provider = provider_with(None, Some("7"));
        assert!(
            allowlist_verdict(&provider, &tag_push(Some("org/repo"), Some(8)))
                .expect("a foreign installation is out of the allowlist")
                .contains("pinned installation")
        );
        // The pinned installation itself, and a delivery carrying no
        // installation at all (a PAT-shaped webhook), pass the pin.
        assert!(allowlist_verdict(&provider, &tag_push(Some("org/repo"), Some(7))).is_none());
        assert!(allowlist_verdict(&provider, &tag_push(Some("org/repo"), None)).is_none());
    }

    #[test]
    fn test_deny_wins_and_allow_requires_a_match() {
        let provider = provider_with(
            Some(set(&["allowed-org/*"], &["allowed-org/secret-*"])),
            None,
        );
        assert!(allowlist_verdict(&provider, &tag_push(Some("allowed-org/repo"), None)).is_none());
        assert!(
            allowlist_verdict(&provider, &tag_push(Some("allowed-org/secret-repo"), None))
                .expect("the deny list wins")
                .contains("deny list")
        );
        assert!(
            allowlist_verdict(&provider, &tag_push(Some("other-org/repo"), None))
                .expect("a non-allowed repository is out")
                .contains("not on the allow list")
        );
    }

    #[test]
    fn test_a_pathless_delivery_is_fail_closed() {
        let provider = provider_with(Some(set(&["allowed-org/*"], &[])), None);
        let reason = allowlist_verdict(&provider, &tag_push(None, None))
            .expect("no path can never match the allow list");
        assert!(reason.contains("no repository path"));
    }
}
