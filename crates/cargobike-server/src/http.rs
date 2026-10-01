//! The server state, health surfaces (§12.1, US-12) and the leader
//! election gate (F-131's minimal v1: try-advisory at boot; the
//! exit-process fence follows in 2.8b).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use secrecy::ExposeSecret;

use crate::config::{Config, literal_secret_warnings, load};
use crate::http::errors::ApiError;

pub mod errors;

/// Shared server state.
pub struct AppState {
    /// The loaded config (hot-reload later swaps this under a RwLock).
    pub config: Config,
    /// Ready only when a started leader holds the election lock (§12.1).
    pub ready: AtomicBool,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("ready", &self.ready.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

/// The unauthenticated health + clientconfig routes (F-107).
pub fn health_router(state: Arc<AppState>) -> Router<()> {
    Router::new()
        .route("/api/v1/live", get(live))
        .route("/api/v1/ready", get(ready))
        .route("/api/v1/startup", get(startup))
        .route("/api/v1/clientconfig", get(clientconfig))
        .with_state(state)
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
    let issuers: Vec<String> = state
        .config
        .auth
        .oidc
        .iter()
        .map(|entry| entry.issuer.clone())
        .collect();
    let audiences: Vec<String> = state
        .config
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
    let url = crate::config::materialise(&config.database.url, &config.secrets)?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(config.database.max_connections)
        .connect(url.expose_secret())
        .await
        .map_err(|e| crate::config::ConfigError::Parse(format!("database connect failed: {e}")))?;
    let state = Arc::new(AppState {
        config: (*config).clone(),
        ready: AtomicBool::new(false),
    });
    elect_leader(&pool, config.leader_election.enabled, &state.ready)
        .await
        .map_err(|e| crate::config::ConfigError::Parse(format!("leader election failed: {e}")))?;
    Ok((health_router(Arc::clone(&state)), state))
}
