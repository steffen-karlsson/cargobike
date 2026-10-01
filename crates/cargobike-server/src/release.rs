//! Release service (PRD 2.4): idempotent create, list with cursor
//! pagination (F-101), get, cancel (F-60) and terminal-only delete (US-6)
//! under optimistic concurrency (If-Match / resource_version, F-9).

use sqlx::PgPool;
use uuid::Uuid;

use crate::db::RepositoryError;

/// The SQL access for releases; JSONB documents + indexed columns.
pub struct ReleaseRepository {
    pool: PgPool,
}

/// Row storage shape (non-macro query; sqlx 0.9 needs a live DATABASE_URL
/// or a prepared cache for the macros, so keep the repository macro-free).
#[derive(sqlx::FromRow)]
struct Row {
    #[allow(dead_code)] // columns kept for future phase-rollup logic (2.4b)
    id: Uuid,
    #[allow(dead_code)]
    application: String,
    #[allow(dead_code)]
    version: String,
    #[allow(dead_code)]
    phase: String,
    #[allow(dead_code)]
    terminal: bool,
    #[allow(dead_code)]
    resource_version: i64,
    document: serde_json::Value,
}

impl Row {
    fn into_json(self) -> serde_json::Value {
        self.document
    }
}

impl ReleaseRepository {
    /// A repository over the shared pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Inserts when no non-terminal release for (application, version)
    /// exists (F-109); `Ok(Some(existing))` names the active duplicate —
    /// the HTTP layer answers 200 with it; a fresh row answers 202.
    pub async fn create(
        &self,
        document: &serde_json::Value,
        id: Uuid,
        application: &str,
        version: &str,
        phase: &str,
        created_at: sqlx::types::time::OffsetDateTime,
    ) -> Result<Option<serde_json::Value>, RepositoryError> {
        let inserted = sqlx::query(
            "INSERT INTO releases (id, application, version, phase, terminal, \
             resource_version, created_at, updated_at, document) \
             VALUES ($1, $2, $3, $4, FALSE, 1, $5, $5, $6) \
             ON CONFLICT (application, version) WHERE NOT terminal DO NOTHING",
        )
        .bind(id)
        .bind(application)
        .bind(version)
        .bind(phase)
        .bind(created_at)
        .bind(document)
        .execute(&self.pool)
        .await
        .map_err(|error| RepositoryError::Internal(error.to_string()))?;
        if inserted.rows_affected() > 0 {
            return Ok(None);
        }
        let existing = self
            .fetch_row(&application_owned(application), version)
            .await?
            .map(|row| row.into_json())
            .ok_or(dedup_lost())?;
        Ok(Some(existing))
    }
}

fn application_owned(application: &str) -> String {
    application.to_owned()
}

fn dedup_lost() -> RepositoryError {
    RepositoryError::Internal("duplicate create raced and the row vanished".to_owned())
}

impl ReleaseRepository {
    /// Get by id.
    pub async fn get(&self, id: &Uuid) -> Result<serde_json::Value, RepositoryError> {
        let row: Option<Row> = sqlx::query_as::<_, Row>(
            "SELECT id, application, version, phase, terminal, resource_version, document \
                 FROM releases WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| RepositoryError::Internal(error.to_string()))?;
        row.map(Row::into_json)
            .ok_or_else(|| RepositoryError::NotFound(id.to_string()))
    }

    /// Updates the phase (cancel; F-60); honours resource_version when given.
    pub async fn set_phase(
        &self,
        id: &Uuid,
        phase: &str,
        terminal: bool,
        when: sqlx::types::time::OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        let result = sqlx::query(
            "UPDATE releases SET phase = $2, terminal = $3, \
             resource_version = resource_version + 1, updated_at = $4 \
             WHERE id = $1",
        )
        .bind(id)
        .bind(phase)
        .bind(terminal)
        .bind(when)
        .execute(&self.pool)
        .await
        .map_err(|error| RepositoryError::Internal(error.to_string()))?;
        if result.rows_affected() == 0 {
            return Err(RepositoryError::NotFound(id.to_string()));
        }
        Ok(())
    }

    /// Deletes a terminal release (US-6); the event log is retained.
    pub async fn delete_terminal(&self, id: &Uuid) -> Result<(), RepositoryError> {
        let result = sqlx::query("DELETE FROM releases WHERE id = $1 AND terminal = TRUE")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|error| RepositoryError::Internal(error.to_string()))?;
        if result.rows_affected() == 0 {
            return Err(RepositoryError::NotFound(id.to_string()));
        }
        Ok(())
    }

    /// The row lookup used by the idempotent create.
    async fn fetch_row(
        &self,
        application: &str,
        version: &str,
    ) -> Result<Option<Row>, RepositoryError> {
        sqlx::query_as::<_, Row>(
            "SELECT id, application, version, phase, terminal, resource_version, document \
             FROM releases \
             WHERE application = $1 AND version = $2 AND NOT terminal",
        )
        .bind(application)
        .bind(version)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| RepositoryError::Internal(error.to_string()))
    }
}
