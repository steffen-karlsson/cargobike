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
use crate::release::ReleaseRepository;
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
        .route("/api/v1/whoami", get(whoami))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            crate::auth::require_auth,
        ))
        .with_state(Arc::clone(&state));
    let body_limit = crate::config::parse_size(&state.config.borrow().limits.api.max_body_size)
        .unwrap_or(1024 * 1024);
    unauthenticated
        .merge(protected)
        // The 2.1 surface: trace, panic containment per RFC 9457, body
        // limit, sensitive headers (never traced/logged).
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer::new(
            sensitive_headers(),
        ))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(body_limit as usize))
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
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Result<axum::response::Response, ApiError> {
    let _ = &caller; // authorization below 
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

    let id = Uuid::now_v7();
    let now = sqlx::types::time::OffsetDateTime::now_utc();
    let document = serde_json::json!({
        "metadata": {
            "id": id.to_string(),
            "created_at": format_rfc3339(now),
            "updated_at": format_rfc3339(now),
            "resource_version": 1,
            "retried_from": serde_json::Value::Null,
            "labels": {},
            "annotations": {},
        },
        "spec": {
            "application": application,
            "version": version,
            "source": {
                "provider": app.source.provider,
                "id": app.source.id,
                "path": app.source.path.as_deref(),
            },
            "template": app.template,
        },
        "status": {
            "phase": "Pending",
            "error": serde_json::Value::Null,
        },
    });

    // The provision: the registry's template compile, the environment's
    // inputs staged, and the snapshot's hash — the interpreter reads
    // only that.
    let snapshot = provision_snapshot(state.as_ref(), app, version.as_str(), id).await?;

    let phase_of = "Pending";
    let existing = state
        .releases
        .create(&document, id, &application, &version, phase_of, now)
        .await
        .map_err(repository_to_api)?;
    if let Some(existing_json) = existing {
        // a duplicate create answers 200 with the existing release.
        return Ok((StatusCode::OK, axum::Json(existing_json)).into_response());
    }

    // The interpreter's start (the workflow id deduplicates; a replayed
    // create joins the workflow already running).
    let workflow_id = format!("cargobike/interpret/{id}");
    if let Err(failure) = state
        .engine
        .interpreter
        .start_with(
            cargobike_engine::InterpretArgs { snapshot },
            dbos::StartOptions {
                workflow_id: Some(workflow_id.as_str()),
                ..dbos::StartOptions::default()
            },
        )
        .await
    {
        tracing::error!(release = %id, %failure, "the interpreter's start refused");
    }
    let location = format!("/api/v1/releases/{id}");
    Ok((
        StatusCode::ACCEPTED,
        [("Location", location.as_str())],
        axum::Json(document),
    )
        .into_response())
}

/// The provision: the release snapshot (the compiled template + the
/// registry's staged inputs + the pinned step-type versions + the
/// content hash) — the interpreter's one durable argument.
async fn provision_snapshot(
    state: &AppState,
    app: &crate::config::ApplicationEntry,
    version: &str,
    id: Uuid,
) -> Result<cargobike_engine::ReleaseSnapshot, ApiError> {
    use crate::http::errors;
    let application = app.name.clone();

    // The version's scheme validation (the create's version policy's
    // first cut; the tag/SHA verification rides the provider work).
    version_scheme_of(app).validate(version).map_err(|error| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            errors::VERSION_NOT_VERIFIED,
            "version-not-verified",
            format!(
                "the version `{version}` does not match the application's versioning rules: {error}"
            ),
        )
    })?;
    let templates =
        crate::validation::load_templates(&state.config.borrow().clone()).map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                errors::INTERNAL_ERROR,
                "internal-error",
                format!("failed to load the templates: {error}"),
            )
        })?;
    let (template_name, template_version) = app.template.split_once('@').ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            errors::TEMPLATE_NOT_FOUND,
            "template-not-found",
            format!(
                "the template reference `{}` is not name@version",
                app.template
            ),
        )
    })?;
    let template = templates
        .get(&(template_name.to_owned(), template_version.to_owned()))
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                errors::TEMPLATE_NOT_FOUND,
                "template-not-found",
                format!(
                    "the template `{}` is not in the templates directory",
                    app.template
                ),
            )
        })?;
    // The template's staged YAML (the model's shapes held the source).
    let source = serde_yaml_ng::to_string(template).map_err(|failure| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            errors::INTERNAL_ERROR,
            "internal-error",
            format!("the template refused to serialise: {failure}"),
        )
    })?;
    let scheme = version_scheme_of(app);
    let mut compiled = cargobike_engine::template::compile(&source, &scheme).map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            errors::INTERNAL_ERROR,
            "internal-error",
            format!("the template refused to compile: {error}"),
        )
    })?;

    // The environments' staged inputs: the registry's repo/edits and
    // the custom per-environment inputs over the template's shape.
    let application_inputs = app.inputs.clone().unwrap_or_default();
    for environment in compiled.environments.iter_mut() {
        let Some(entry) = app.environments.get(&environment.name) else {
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                errors::INTERNAL_ERROR,
                "internal-error",
                format!(
                    "the application entry lacks the template's environment `{}`",
                    environment.name
                ),
            ));
        };
        let mut staged = environment.env_inputs.clone();
        if let Some(repo) = &entry.repo {
            staged.insert(
                "repo".to_owned(),
                serde_json::to_value(repo).unwrap_or_default(),
            );
        }
        if let Some(edits) = &entry.edits {
            staged.insert(
                "edits".to_owned(),
                serde_json::to_value(edits).unwrap_or_default(),
            );
        }
        if let Some(commit_message) = &entry.commit_message {
            staged.insert(
                "commit_message".to_owned(),
                serde_json::json!(commit_message),
            );
        }
        if let Some(inputs) = &entry.inputs {
            for (name, value) in inputs {
                staged.insert(name.clone(), value.clone());
            }
        }
        environment.env_inputs = staged;
    }

    let step_type_versions = state
        .engine
        .services
        .steps
        .installed()
        .into_iter()
        .collect();
    let release = cargobike_engine::ReleaseIdentity {
        id: id.to_string(),
        application,
        version: version.to_owned(),
        version_scheme: scheme,
    };
    let inputs = application_inputs;
    let content_hash =
        cargobike_engine::snapshot_content_hash(&compiled, &release, &inputs, &step_type_versions)
            .map_err(|failure| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    errors::INTERNAL_ERROR,
                    "internal-error",
                    format!("the snapshot's hash failed: {failure}"),
                )
            })?;
    Ok(cargobike_engine::ReleaseSnapshot {
        template: compiled,
        release,
        inputs,
        step_type_versions,
        content_hash,
    })
}

fn format_rfc3339(at: sqlx::types::time::OffsetDateTime) -> String {
    match at.format(&time::format_description::well_known::Rfc3339) {
        Ok(formatted) => formatted,
        // An OffsetDateTime is always RFC-3339 formattable; a failure would
        // be a format-crate bug, so surface it as an internal problem detail.
        Err(error) => {
            tracing::error!(%error, "failed to format rfc3339");
            String::new()
        }
    }
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

/// The registry's versioning block to the version scheme.
fn version_scheme_of(
    app: &crate::config::ApplicationEntry,
) -> cargobike_core::version::VersionScheme {
    match app.versioning.scheme.as_str() {
        "semver" => cargobike_core::version::VersionScheme::Semver,
        "calver" => cargobike_core::version::VersionScheme::Calver {
            calver_format: app.versioning.calver_format.clone(),
        },
        _ => cargobike_core::version::VersionScheme::Opaque,
    }
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
    let engine = crate::engine::host(&config, pool.clone(), provider_override).await?;
    let state = Arc::new(AppState {
        config: config_rx,
        ready: AtomicBool::new(false),
        releases: ReleaseRepository::new(pool.clone()),
        auth,
        config_path: config_path.map(|path| path.to_path_buf()),
        engine: Arc::new(engine),
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
