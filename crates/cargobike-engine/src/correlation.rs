//! CR correlation: the mapping the webhook receiver and the
//! reconciler use — `(the provider, repo_id, cr_number) -> (the release,
//! environment, step, attempt)`.
//!
//! The change-request step writes it when the CR opens (the engine); the
//! webhook path (Phase 5 lands it) and the reconciler sweep read it. The
//! schema lives with the server's migrations (0003).

use sqlx::PgPool;
use uuid::Uuid;

/// The correlation row the engine stamps and sweeps consume.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct CorrelationRow {
    /// Provider name (RepoRef.provider).
    pub provider: String,
    /// The repository's immutable ID.
    pub repo_id: String,
    /// The CR number.
    pub cr_number: i64,
    /// The release this CR advances.
    pub release_id: Uuid,
    /// The environment the release was progressing through.
    pub environment: String,
    /// The wait step's ID (the topic's step half).
    pub step_id: String,
    /// The current attempt's workflow ID (the addressing).
    pub workflow_id: String,
}

/// SQL access for the correlation rows.
pub struct CorrelationRepository {
    pool: PgPool,
}

impl CorrelationRepository {
    /// A repository over the shared pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Records the mapping when a CR opens (the an idempotent upsert: the
    /// same head branch re-building a CR re-stamps the row).
    pub async fn stamp(&self, row: &CorrelationRow) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO cr_correlation \
             (provider, repo_id, cr_number, release_id, environment, step_id, workflow_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (provider, repo_id, cr_number) DO UPDATE SET \
             release_id = EXCLUDED.release_id, environment = EXCLUDED.environment, \
             step_id = EXCLUDED.step_id, workflow_id = EXCLUDED.workflow_id",
        )
        .bind(&row.provider)
        .bind(&row.repo_id)
        .bind(row.cr_number)
        .bind(row.release_id)
        .bind(&row.environment)
        .bind(&row.step_id)
        .bind(&row.workflow_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The correlation a delivery resolves to (the webhook path's row).
    pub async fn find(
        &self,
        provider: &str,
        repo_id: &str,
        cr_number: i64,
    ) -> Result<Option<CorrelationRow>, sqlx::Error> {
        sqlx::query_as::<_, CorrelationRow>(
            "SELECT provider, repo_id, cr_number, release_id, environment, step_id, workflow_id \
             FROM cr_correlation WHERE provider = $1 AND repo_id = $2 AND cr_number = $3",
        )
        .bind(provider)
        .bind(repo_id)
        .bind(cr_number)
        .fetch_optional(&self.pool)
        .await
    }

    /// Every open correlation (the reconciler's sweep input,
    /// batching starts here: the batch page comes from the caller).
    pub async fn page(
        &self,
        limit: u32,
        after: Option<(String, String, i64)>,
    ) -> Result<Vec<CorrelationRow>, sqlx::Error> {
        match after {
            Some((provider, repo_id, cr_number)) => {
                sqlx::query_as::<_, CorrelationRow>(
                    "SELECT provider, repo_id, cr_number, release_id, environment, step_id, workflow_id \
                     FROM cr_correlation \
                     WHERE (provider, repo_id, cr_number) > ($1, $2, $3) \
                     ORDER BY provider, repo_id, cr_number LIMIT $4",
                )
                .bind(provider)
                .bind(repo_id)
                .bind(cr_number)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await
            }
            None => {
                sqlx::query_as::<_, CorrelationRow>(
                    "SELECT provider, repo_id, cr_number, release_id, environment, step_id, workflow_id \
                     FROM cr_correlation ORDER BY provider, repo_id, cr_number LIMIT $1",
                )
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await
            }
        }
    }

    /// Removes a correlation once the release no longer waits (the
    /// cleanup's post-terminal tidying).
    pub async fn forget(
        &self,
        provider: &str,
        repo_id: &str,
        cr_number: i64,
    ) -> Result<bool, sqlx::Error> {
        let removed = sqlx::query(
            "DELETE FROM cr_correlation WHERE provider = $1 AND repo_id = $2 AND cr_number = $3",
        )
        .bind(provider)
        .bind(repo_id)
        .bind(cr_number)
        .execute(&self.pool)
        .await?;
        Ok(removed.rows_affected() > 0)
    }
}
