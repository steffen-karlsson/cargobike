//! The release status store: the interpreter is the only writer of its
//! release's status — the bookkeeping the engine does itself (set
//! status, the environment's records, the rollup), never the user's
//! job. Writes carry the conditional-update graces (a terminal release
//! never re-phases; a replay's recorded write is skipped rather than
//! re-decided), so the at-least-once executions converge on the
//! same row.

use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use cargobike_core::error::ReleaseError;
use cargobike_core::model::{ChangeRequestRef, EnvironmentPhase, EnvironmentStatus, Phase};

/// The status writes the interpreter performs on its release.
///
/// One trait so the server (and the tests) plug their storage in; the
/// sqlx-backed implementation reads the release's JSONB document,
/// applies the status fact, and writes the document back — bumping the
/// resource version the optimistic-concurrency surface honours (F-9).
#[async_trait]
pub trait ReleaseStatusStore: Send + Sync {
    /// A workflow attempt (the workflow id, its start). Appended to the
    /// records in arrival order; the release's phase moves to Running.
    async fn attempt_started(
        &self,
        release_id: Uuid,
        workflow_id: &str,
        at: time::OffsetDateTime,
    ) -> Result<(), StatusError>;

    /// An environment entered execution (running, timestamped); the
    /// release's phase rolls to Running.
    async fn environment_running(
        &self,
        release_id: Uuid,
        environment: &str,
        at: time::OffsetDateTime,
    ) -> Result<(), StatusError>;

    /// One environment's record from the document (a None when the
    /// release or the record is absent; the supersede chain reads the
    /// old attempt's CR).
    async fn load_environment(
        &self,
        release_id: Uuid,
        environment: &str,
    ) -> Result<Option<EnvironmentStatus>, StatusError>;

    /// The (release id, current attempt's workflow id) pairs of the
    /// releases waiting on this application+environment's lease (the
    /// queue's wake list). Empty when nothing waits.
    async fn waiting_releases(
        &self,
        application: &str,
        environment: &str,
    ) -> Result<Vec<(Uuid, String)>, StatusError>;

    /// An environment reached PendingApproval (the wait's entry); the
    /// release's phase follows.
    async fn environment_pending_approval(
        &self,
        release_id: Uuid,
        environment: &str,
    ) -> Result<(), StatusError>;

    /// An environment held by the concurrency policy (the queue's wait).
    async fn environment_waiting(
        &self,
        release_id: Uuid,
        environment: &str,
    ) -> Result<(), StatusError>;

    /// A change request the interpreter opened for the environment: the
    /// reference lands in the environment's status record.
    async fn environment_change_request(
        &self,
        release_id: Uuid,
        environment: &str,
        reference: ChangeRequestRef,
    ) -> Result<(), StatusError>;

    /// An environment reached its terminal phase: completed_at lands,
    /// the release's phase is the rollup of the environments' phases,
    /// and the error half rides with a failure's rollup. Refused for
    /// a release already terminal (the cancel path set it first).
    async fn environment_terminal(
        &self,
        release_id: Uuid,
        environment: &str,
        phase: EnvironmentPhase,
        error: Option<ReleaseError>,
        at: time::OffsetDateTime,
    ) -> Result<(), StatusError>;
}

/// Why a status write refused.
#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    /// The release row is gone; the interpreter's writes are lost with
    /// it (the delete's event-log story is the auditable half).
    #[error("failed to record status: the release {0} was not found")]
    NotFound(Uuid),
    /// The release is terminal; a status write is refused (a stale
    /// attempt's bookkeeping).
    #[error("failed to record status: the release is terminal")]
    Terminal,
    /// The store's own failure (driver text stays here, not a panic).
    #[error("failed to record status: {0}")]
    Internal(String),
}

/// Whether the document's `status.phase` text names a terminal phase.
fn phase_text_is_terminal(phase: &str) -> bool {
    matches!(phase, "Failed" | "Canceled" | "Superseded" | "Completed")
}

/// The release phase derived from the environments' phases (the rollup).
pub fn rollup(env_phases: &[EnvironmentPhase]) -> Phase {
    if env_phases.is_empty() {
        return Phase::Pending;
    }
    if env_phases.contains(&EnvironmentPhase::Canceled) {
        return Phase::Canceled;
    }
    if env_phases.contains(&EnvironmentPhase::Superseded) {
        return Phase::Superseded;
    }
    if env_phases.contains(&EnvironmentPhase::Failed) {
        return Phase::Failed;
    }
    if env_phases.iter().all(|phase| {
        matches!(
            phase,
            EnvironmentPhase::Completed | EnvironmentPhase::Skipped
        )
    }) {
        return Phase::Completed;
    }
    if env_phases.contains(&EnvironmentPhase::PendingApproval) {
        return Phase::PendingApproval;
    }
    // Nothing has actually started (the lease's queue holds every env
    // still): the release remains Pending until an environment runs.
    if env_phases
        .iter()
        .all(|phase| matches!(phase, EnvironmentPhase::Pending | EnvironmentPhase::Waiting))
    {
        return Phase::Pending;
    }
    Phase::Running
}

/// The RFC 3339 form the document's timestamps keep.
fn rfc3339(at: time::OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// The sqlx-backed store over the releases table's JSONB documents:
/// every write reads the document, applies the fact, and writes it back
/// with the phase/terminal columns synced to the status text (the
/// repository's indexes read those columns; the document is the body).
pub struct SqlReleaseStatusStore {
    pool: PgPool,
}

impl SqlReleaseStatusStore {
    /// A store over the shared pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    async fn load_document(&self, release_id: Uuid) -> Result<serde_json::Value, StatusError> {
        sqlx::query_scalar::<_, serde_json::Value>("SELECT document FROM releases WHERE id = $1")
            .bind(release_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| StatusError::Internal(error.to_string()))?
            .ok_or(StatusError::NotFound(release_id))
    }

    /// Writes the document back: the phase/terminal columns follow the
    /// status text, the resource version bumps, the row must exist.
    async fn write_document(
        &self,
        release_id: Uuid,
        document: &serde_json::Value,
    ) -> Result<(), StatusError> {
        let phase = document
            .pointer("/status/phase")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let result = sqlx::query(
            "UPDATE releases \
             SET document = $2, phase = $3, terminal = $4, \
                 resource_version = resource_version + 1, \
                 updated_at = $5 \
             WHERE id = $1 AND NOT terminal",
        )
        .bind(release_id)
        .bind(document)
        .bind(&phase)
        .bind(phase_text_is_terminal(&phase))
        .bind(time::OffsetDateTime::now_utc())
        .execute(&self.pool)
        .await
        .map_err(|error| StatusError::Internal(error.to_string()))?;
        if result.rows_affected() == 0 {
            return Err(StatusError::NotFound(release_id));
        }
        Ok(())
    }

    /// The environment statuses the document carries.
    fn environments_of(document: &serde_json::Value) -> Vec<EnvironmentStatus> {
        document
            .pointer("/status/environments")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| serde_json::from_value(item.clone()).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Ensures the environments array exists in the document and carries
    /// a record for `environment`.
    fn ensure_environment(
        document: &mut serde_json::Value,
        environment: &str,
    ) -> Result<(), StatusError> {
        let status = document
            .get_mut("status")
            .ok_or_else(|| StatusError::Internal("the document lacks a status".to_owned()))?;
        if !matches!(
            status.get("environments"),
            Some(serde_json::Value::Array(_))
        ) {
            status["environments"] = serde_json::Value::Array(Vec::new());
        }
        let Some(environments) = status
            .get_mut("environments")
            .and_then(serde_json::Value::as_array_mut)
        else {
            return Err(StatusError::Internal(
                "the environments field is not an array".to_owned(),
            ));
        };
        let present = environments
            .iter()
            .any(|item| item.get("name").and_then(serde_json::Value::as_str) == Some(environment));
        if !present {
            environments.push(serde_json::json!({
                "name": environment,
                "phase": "Pending",
                "change_request": serde_json::Value::Null,
                "started_at": serde_json::Value::Null,
                "completed_at": serde_json::Value::Null,
            }));
        }
        Ok(())
    }
}

#[async_trait]
impl ReleaseStatusStore for SqlReleaseStatusStore {
    async fn load_environment(
        &self,
        release_id: Uuid,
        environment: &str,
    ) -> Result<Option<EnvironmentStatus>, StatusError> {
        let document = match self.load_document(release_id).await {
            Ok(document) => document,
            Err(StatusError::NotFound(_)) => return Ok(None),
            Err(other) => return Err(other),
        };
        let found = document
            .pointer("/status/environments")
            .and_then(serde_json::Value::as_array)
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| {
                        item.get("name").and_then(serde_json::Value::as_str) == Some(environment)
                    })
                    .cloned()
            });
        if let Some(item) = &found {
            match serde_json::from_value::<EnvironmentStatus>(item.clone()) {
                Ok(record) => return Ok(Some(record)),
                Err(failure) => {
                    tracing::error!(%failure, "the env record refused to decode");
                    return Ok(None);
                }
            }
        }
        Ok(None)
    }

    async fn waiting_releases(
        &self,
        application: &str,
        environment: &str,
    ) -> Result<Vec<(Uuid, String)>, StatusError> {
        sqlx::query_as::<_, (Uuid, Option<String>)>(
            "SELECT r.id, \
             (SELECT elem->>'workflow_id' \
              FROM jsonb_array_elements(r.document #> '{status,attempts}') elem \
              ORDER BY elem->>'started_at' DESC LIMIT 1) \
             FROM releases r, \
                  LATERAL jsonb_array_elements(r.document #> '{status,environments}') env \
             WHERE r.document #>> '{spec,application}' = $1 \
               AND env->>'name' = $2 \
               AND env->>'phase' = 'Waiting'",
        )
        .bind(application)
        .bind(environment)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| StatusError::Internal(error.to_string()))
        .map(|rows| {
            rows.into_iter()
                .filter_map(|(id, workflow)| workflow.map(|wf| (id, wf)))
                .collect()
        })
    }

    async fn attempt_started(
        &self,
        release_id: Uuid,
        workflow_id: &str,
        at: time::OffsetDateTime,
    ) -> Result<(), StatusError> {
        let mut document = self.load_document(release_id).await?;
        let attempts = document
            .pointer("/status/attempts")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let attempts = {
            let mut attempts = attempts;
            attempts.push(serde_json::json!({
                "workflow_id": workflow_id,
                "started_at": rfc3339(at),
                "fork_from": serde_json::Value::Null,
            }));
            attempts
        };
        let Some(status) = document.pointer_mut("/status") else {
            return Err(StatusError::Internal(
                "the document lacks a status".to_owned(),
            ));
        };
        status
            .as_object_mut()
            .ok_or_else(|| StatusError::Internal("the status is not an object".to_owned()))?
            .insert("attempts".to_owned(), serde_json::Value::Array(attempts));
        if document.pointer_mut("/status/phase").is_some() {
            document["status"]["phase"] = serde_json::Value::String("Running".to_owned());
        }
        self.write_document(release_id, &document).await
    }

    async fn environment_running(
        &self,
        release_id: Uuid,
        environment: &str,
        at: time::OffsetDateTime,
    ) -> Result<(), StatusError> {
        let mut document = self.load_document(release_id).await?;
        Self::ensure_environment(&mut document, environment)?;
        set_environment_record(&mut document, environment, |record| {
            record["phase"] = serde_json::Value::String("Running".to_owned());
            record["started_at"] = serde_json::Value::String(rfc3339(at));
        })?;
        if document.pointer_mut("/status/phase").is_some() {
            document["status"]["phase"] = serde_json::Value::String("Running".to_owned());
        }
        self.write_document(release_id, &document).await
    }

    async fn environment_pending_approval(
        &self,
        release_id: Uuid,
        environment: &str,
    ) -> Result<(), StatusError> {
        let mut document = self.load_document(release_id).await?;
        Self::ensure_environment(&mut document, environment)?;
        set_environment_record(&mut document, environment, |record| {
            record["phase"] = serde_json::Value::String("PendingApproval".to_owned());
        })?;
        if document.pointer_mut("/status/phase").is_some() {
            document["status"]["phase"] = serde_json::Value::String("PendingApproval".to_owned());
        }
        self.write_document(release_id, &document).await
    }

    async fn environment_waiting(
        &self,
        release_id: Uuid,
        environment: &str,
    ) -> Result<(), StatusError> {
        let mut document = self.load_document(release_id).await?;
        Self::ensure_environment(&mut document, environment)?;
        set_environment_record(&mut document, environment, |record| {
            record["phase"] = serde_json::Value::String("Waiting".to_owned());
        })?;
        self.write_document(release_id, &document).await
    }

    async fn environment_change_request(
        &self,
        release_id: Uuid,
        environment: &str,
        reference: ChangeRequestRef,
    ) -> Result<(), StatusError> {
        let mut document = self.load_document(release_id).await?;
        Self::ensure_environment(&mut document, environment)?;
        set_environment_record(&mut document, environment, |record| {
            record["change_request"] =
                serde_json::to_value(&reference).unwrap_or(serde_json::Value::Null);
        })?;
        self.write_document(release_id, &document).await
    }

    async fn environment_terminal(
        &self,
        release_id: Uuid,
        environment: &str,
        phase: EnvironmentPhase,
        error: Option<ReleaseError>,
        at: time::OffsetDateTime,
    ) -> Result<(), StatusError> {
        let mut document = self.load_document(release_id).await?;
        if document
            .pointer("/status/phase")
            .and_then(serde_json::Value::as_str)
            .is_some_and(phase_text_is_terminal)
        {
            return Err(StatusError::Terminal);
        }
        Self::ensure_environment(&mut document, environment)?;
        set_environment_record(&mut document, environment, |record| {
            record["phase"] = serde_json::Value::String(phase.to_string());
            record["completed_at"] = serde_json::Value::String(rfc3339(at));
        })?;
        // The rollup over every environment's phase; a failed rollup
        // carries the error fact at the release's status.
        let rolled = rollup(
            &Self::environments_of(&document)
                .iter()
                .map(|status| status.phase)
                .collect::<Vec<_>>(),
        );
        if let Some(field) = document.pointer_mut("/status/phase") {
            *field = serde_json::Value::String(rolled.to_string());
        }
        if matches!(rolled, Phase::Failed) {
            if let (Some(error), Some(field)) = (&error, document.pointer_mut("/status/error")) {
                *field = serde_json::to_value(error).unwrap_or(serde_json::Value::Null);
            }
        }
        self.write_document(release_id, &document).await
    }
}

/// Mutates the environment's record in the document's array.
fn set_environment_record(
    document: &mut serde_json::Value,
    environment: &str,
    apply: impl FnOnce(&mut serde_json::Value),
) -> Result<(), StatusError> {
    let Some(items) = document
        .pointer_mut("/status/environments")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return Err(StatusError::Internal(
            "the document lacks an environments array".to_owned(),
        ));
    };
    let Some(record) = items
        .iter_mut()
        .find(|item| item.get("name").and_then(serde_json::Value::as_str) == Some(environment))
    else {
        return Err(StatusError::Internal(format!(
            "the environments array lacks {environment}"
        )));
    };
    apply(record);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn phases(values: &[EnvironmentPhase]) -> Vec<EnvironmentPhase> {
        values.to_vec()
    }

    #[test]
    fn test_the_rollup_follows_the_phase_rules() {
        assert_eq!(rollup(&phases(&[])), Phase::Pending);
        assert_eq!(
            rollup(&phases(&[EnvironmentPhase::Running])),
            Phase::Running
        );
        assert_eq!(
            rollup(&phases(&[
                EnvironmentPhase::Completed,
                EnvironmentPhase::Skipped
            ])),
            Phase::Completed
        );
        assert_eq!(
            rollup(&phases(&[
                EnvironmentPhase::Completed,
                EnvironmentPhase::PendingApproval
            ])),
            Phase::PendingApproval
        );
        assert_eq!(
            rollup(&phases(&[
                EnvironmentPhase::Completed,
                EnvironmentPhase::Failed
            ])),
            Phase::Failed
        );
        assert_eq!(
            rollup(&phases(&[
                EnvironmentPhase::Completed,
                EnvironmentPhase::Canceled
            ])),
            Phase::Canceled
        );
        assert_eq!(
            rollup(&phases(&[
                EnvironmentPhase::Completed,
                EnvironmentPhase::Superseded
            ])),
            Phase::Superseded
        );
        // The queue's hold is not a start: an all-Waiting release stays
        // Pending until an environment actually runs.
        assert_eq!(
            rollup(&phases(&[EnvironmentPhase::Waiting])),
            Phase::Pending
        );
        assert_eq!(
            rollup(&phases(&[
                EnvironmentPhase::Waiting,
                EnvironmentPhase::Running
            ])),
            Phase::Running
        );
    }

    #[test]
    fn test_phase_texts_of_terminal_states() {
        for (text, expected) in [
            ("Failed", true),
            ("Canceled", true),
            ("Superseded", true),
            ("Completed", true),
            ("Running", false),
            ("Pending", false),
            ("PendingApproval", false),
        ] {
            assert_eq!(phase_text_is_terminal(text), expected, "{text}");
        }
    }
}
