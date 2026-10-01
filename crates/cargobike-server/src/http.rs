//! The server state, health surfaces (§12.1, US-12) and the leader
//! election gate (F-131's minimal v1: try-advisory at boot; the
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

/// Shared server state.
pub struct AppState {
    /// The current config; a SIGHUP reload swaps this (F-129).
    pub config: tokio::sync::watch::Receiver<Arc<Config>>,
    /// Ready only when a started leader holds the election lock (§12.1).
    pub ready: AtomicBool,
    /// Release reads/writes (2.3/2.4).
    pub releases: ReleaseRepository,
    /// Auth context (2.5/2.6).
    pub auth: Arc<crate::auth::AuthState>,
    /// The config file the reload task re-reads (F-129).
    pub config_path: Option<std::path::PathBuf>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("ready", &self.ready.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

/// All Phase-2 routes: health + clientconfig unauthenticated (F-107),
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
        .with_state(state);
    unauthenticated.merge(protected)
}

/// Lists releases (§5.3): filters + the latest cursor (F-101).
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
    let limit = params.limit.unwrap_or(50).clamp(1, 500); // F-101's documented bound
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
    Ok(axum::Json(serde_json::json!({
        "items": items,
        "cursor": cursor,
        "has_more": cursor.is_some() && usize::try_from(limit).is_ok(),
    })))
}

/// Create is `{application, version}` only (F-3); duplicates answer 200
/// with the existing release (F-109); a fresh row answers 202 + `Location`
/// (F-110).
async fn create_release(
    State(state): State<Arc<AppState>>,
    axum::Extension(caller): axum::Extension<crate::auth::AuthedCaller>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Result<axum::response::Response, ApiError> {
    let _ = &caller; // authorization below (F-99a)
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

    let phase_of = "Pending";
    let existing = state
        .releases
        .create(&document, id, &application, &version, phase_of, now)
        .await
        .map_err(repository_to_api)?;
    if let Some(existing_json) = existing {
        // F-109: a duplicate create answers 200 with the existing release.
        return Ok((StatusCode::OK, axum::Json(existing_json)).into_response());
    }

    // TODO(2.4b): start the interpreter workflow (durable) at creation; the
    // row is persisted first, so recovery can prove correctness (F-15/F-21).
    let location = format!("/api/v1/releases/{id}");
    Ok((
        StatusCode::ACCEPTED,
        [("Location", location.as_str())],
        axum::Json(document),
    )
        .into_response())
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

/// F-60: cancel is a dedicated endpoint starting a cleanup workflow (F-75).
async fn cancel_release(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let _ = state;
    let now = sqlx::types::time::OffsetDateTime::now_utc();
    state
        .releases
        .set_phase(&id, "Canceled", true, now)
        .await
        .map_err(repository_to_api)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_release(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<axum::Json<serde_json::Value>, ApiError> {
    let document = state.releases.get(&id).await.map_err(repository_to_api)?;
    Ok(axum::Json(document))
}

/// Terminal-only delete (US-6); event log retained (F-114).
async fn delete_release(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    state
        .releases
        .delete_terminal(&id)
        .await
        .map_err(repository_to_api)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Maps repository errors to the RFC 9457 vocabulary (§10.2).
fn repository_to_api(error: RepositoryError) -> ApiError {
    match error {
        RepositoryError::NotFound(id) => ApiError::new(
            StatusCode::NOT_FOUND,
            crate::http::errors::RELEASE_NOT_FOUND,
            "release-not-found",
            format!("The release `{id}` was not found."),
        ),
        e => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::http::errors::INTERNAL_ERROR,
            "internal-error",
            format!("{e}"),
        ),
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

/// Issuer + audience hints only (F-98's safe auto-discovery).
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

/// Advisory-lock attempt for minimal leader election (F-131 v1: a standby
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

/// Boots the config (F-89/F-125) with the startup warnings, the leader
/// gate and the router ready to serve.
pub async fn boot(
    config_path: Option<&std::path::Path>,
) -> Result<(Router<()>, Arc<AppState>), crate::config::ConfigError> {
    let config = Arc::new(load(config_path)?);
    for literal in literal_secret_warnings(&config) {
        tracing::warn!("{literal}");
    }
    // 2.7: semantic validation before anything else starts half-authorised.
    if let Err(error) = crate::validation::validate(&config) {
        return Err(crate::config::ConfigError::Parse(error.to_string()));
    }
    let pool = crate::db::connect(&config).await?;
    let (config_tx, config_rx) = tokio::sync::watch::channel(Arc::clone(&config));
    let auth = Arc::new(crate::auth::AuthState::new(config_rx.clone())?);
    let state = Arc::new(AppState {
        config: config_rx,
        ready: AtomicBool::new(false),
        releases: ReleaseRepository::new(pool.clone()),
        auth,
        config_path: config_path.map(|path| path.to_path_buf()),
    });
    spawn_sighup_reload(Arc::clone(&state), config_tx);
    elect_leader(&pool, config.leader_election.enabled, &state.ready)
        .await
        .map_err(|e| crate::config::ConfigError::Parse(format!("leader election failed: {e}")))?;
    Ok((api_router(Arc::clone(&state)), state))
}

/// F-129's SIGHUP hot reload: re-load, re-validate, then swap the config
/// the whole server reads from. A broken reload logs and keeps the
/// previous config (in-flight releases are pinned by F-6/F-16 anyway).
fn spawn_sighup_reload(
    state: Arc<AppState>,
    sender: tokio::sync::watch::Sender<Arc<crate::config::Config>>,
) {
    tokio::spawn(async move {
        let Ok(mut signals) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        else {
            return; // not a unix host: reload by restart (F-132's Recreate)
        };
        while signals.recv().await.is_some() {
            let path = state.config_path.clone().unwrap_or_else(|| {
                std::path::PathBuf::from("/etc/cargobike/config.yaml") // F-124's default
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

/// F-106: the resolved identity + grants (§9.6 summary in JSON).
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
/// handler (rather than the middleware).
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
