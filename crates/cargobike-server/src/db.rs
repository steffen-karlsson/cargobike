//! Database access: the pool, migrations (2.3) and the release
//! repository (..).

use sqlx::PgPool;

use secrecy::ExposeSecret;

use crate::config::{Config, materialise};

/// Connects the pool and applies migrations .
pub async fn connect(config: &Config) -> Result<PgPool, crate::config::ConfigError> {
    let url = materialise(&config.database.url, &config.secrets)?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(config.database.max_connections)
        .connect(url.expose_secret())
        .await
        .map_err(|error| {
            crate::config::ConfigError::Parse(format!("database connect failed: {error}"))
        })?;
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .map_err(|error| {
            crate::config::ConfigError::Parse(format!("migrations failed: {error}"))
        })?;
    Ok(pool)
}

#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
    /// The id was not found (→ 404, family).
    #[error("failed to find release {0}")]
    NotFound(String),
    /// The create conflict — an active release for (the app, version) exists.
    #[error("failed to create release: an active release exists for the application and version")]
    DuplicateActive,
    /// Optimistic-concurrency mismatch (If-Match).
    #[error("failed to update release: resource version mismatch")]
    ResourceVersionMismatch,
    /// A state transition the store refuses (terminal re-phase).
    #[error("failed to update release: the release is already terminal")]
    Conflict,
    /// Anything else (→ 500 InternalError).
    #[error("failed to access the database: {0}")]
    Internal(String),
}
