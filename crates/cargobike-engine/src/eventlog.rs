//! The immutable release event log (F-111/F-113).
//!
//! Every release state transition appends one event here: the
//! interpreter's status writes (each a durable step), the API's
//! create/cancel/reopen/supersede seams, and — later — the approval
//! submissions. The log is the audit trail: `actor`, `at`, `kind`,
//! `reason`, plus a `prev`/`hash` chain for tamper evidence (F-113)
//! and a per-release `sequence` that doubles as the SSE id (F-102).
//!
//! At-least-once execution: every append carries a DETERMINISTIC
//! `event_id` chosen by the caller (the release id, the attempt's
//! workflow id, and the step's label — the same combination every
//! replay computes). The insert is `ON CONFLICT (id) DO NOTHING`: a
//! re-executed durable step converges on the first append instead of
//! duplicating it, and the returned sequence is the first's (a
//! replayed step never observes a fresh number).
//!
//! Per-release serialisation (F-113): appends lock the release row
//! (`SELECT ... FOR UPDATE`) for the whole read-prev/assign-sequence/
//! insert arc, so the chain and the sequence stay single-threaded per
//! release even with concurrent writers.

use sqlx::PgPool;
use uuid::Uuid;

/// Why an event-log operation failed.
#[derive(Debug, thiserror::Error)]
pub enum EventLogError {
    /// The release row is gone: there is nothing to append to (the
    /// interpreter's writes are lost with the row; the delete's
    /// event-log story is the auditable half).
    #[error("failed to append an event: the release {0} was not found")]
    NotFound(Uuid),
    /// The log's own failure (driver text stays here, not a panic).
    #[error("failed to append an event: {0}")]
    Internal(String),
}

impl From<sqlx::Error> for EventLogError {
    fn from(error: sqlx::Error) -> Self {
        EventLogError::Internal(error.to_string())
    }
}

/// One stored event (the API listing's read model): the sequence is
/// the SSE id, the `hash`/`prev` pair lets a reader verify the chain
/// (F-113) — `hash = sha256(prev || canonical_json)` over the event's
/// own fields (`serde_json` maps are sorted without `preserve_order`,
/// so the JSON text is canonical by construction).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EventRecord {
    /// The per-release order (the SSE `id:`).
    pub sequence: i64,
    /// When the transition happened (RFC 3339).
    pub at: String,
    /// Who caused the transition: the release's stamped actor for
    /// interpreter-driven writes, the caller for API-driven ones.
    pub actor: serde_json::Value,
    /// What happened (kebab-case; e.g. `environment-running`).
    pub kind: String,
    /// Why it happened, in prose.
    pub reason: String,
    /// The transition's facts (environment, phase, error, ...).
    pub data: serde_json::Value,
    /// This event's hash (F-113's tamper evidence).
    pub hash: String,
    /// The predecessor's hash (empty for the genesis event).
    pub prev: String,
}

/// The event-log write seam. The interpreter appends through this
/// (`InterpreterServices.events`); the server's API seams call the
/// [`SqlEventLog`] directly (they are not DBOS steps — the create's
/// dedupe and the cancel's terminal guard make their replays safe).
#[async_trait::async_trait]
pub trait EventSink: Send + Sync {
    /// Appends one event. `event_id` is the DETERMINISTIC id (a
    /// re-executed caller converges on the first append); `actor`
    /// `None` resolves to the release document's stamped actor (the
    /// verified identity F-83 wrote — the interpreter's writes carry
    /// the release's own actor, the API's carry the caller's).
    /// Returns the event's sequence.
    async fn append(
        &self,
        release_id: Uuid,
        event_id: &str,
        kind: &str,
        reason: &str,
        data: serde_json::Value,
        actor: Option<serde_json::Value>,
    ) -> Result<i64, EventLogError>;
}

/// What the interpreter records for one status transition (the
/// `status_write` seam builds the event id; the note carries the
/// semantics only the call site knows).
#[derive(Debug, Clone)]
pub struct EventNote {
    /// The kebab-case kind (`environment-running`, ...).
    pub kind: String,
    /// Why the transition happened, in prose.
    pub reason: String,
    /// The transition's facts (environment, phase, error, ...).
    pub data: serde_json::Value,
}

impl EventNote {
    /// A plain note.
    pub fn new(kind: &str, reason: impl Into<String>) -> Self {
        Self {
            kind: kind.to_owned(),
            reason: reason.into(),
            data: serde_json::Value::Null,
        }
    }

    /// The note's facts.
    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = data;
        self
    }

    /// An environment's terminal note: the kind names the phase
    /// (`environment-completed`, `environment-failed`, ...).
    pub fn terminal(
        environment: &str,
        phase: &cargobike_core::model::EnvironmentPhase,
        error: Option<&cargobike_core::error::ReleaseError>,
    ) -> Self {
        let phase_text = phase.to_string();
        let mut data = serde_json::json!({
            "environment": environment,
            "phase": phase_text,
        });
        if let Some(error) = error {
            data["error"] = serde_json::to_value(error).unwrap_or(serde_json::Value::Null);
        }
        Self::new(
            &format!("environment-{}", phase_text.to_lowercase()),
            format!("the environment {environment} reached {phase_text}"),
        )
        .with_data(data)
    }
}

/// The sink for tests and fixtures that do not exercise the log: the
/// appends succeed and go nowhere.
#[derive(Debug, Default)]
pub struct NoopEventSink;

#[async_trait::async_trait]
impl EventSink for NoopEventSink {
    async fn append(
        &self,
        _release_id: Uuid,
        _event_id: &str,
        _kind: &str,
        _reason: &str,
        _data: serde_json::Value,
        _actor: Option<serde_json::Value>,
    ) -> Result<i64, EventLogError> {
        Ok(0)
    }
}

/// The event's hash (F-113): `sha256(prev || canonical_json)` over
/// the fields a reader re-derives from the stored row. The JSON is
/// canonical by construction (`serde_json` sorts map keys without
/// the `preserve_order` feature, PRD Appendix A's F-16 note).
fn event_hash(prev: &str, event: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    let canonical = serde_json::to_string(event).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(prev.as_bytes());
    hasher.update(canonical.as_bytes());
    hex::encode(hasher.finalize())
}

/// The sqlx-backed log over the `events` table.
pub struct SqlEventLog {
    pool: PgPool,
}

impl SqlEventLog {
    /// A log over the shared pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The RFC 3339 form the `at` column keeps.
    fn rfc3339(at: time::OffsetDateTime) -> String {
        at.format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default()
    }

    /// The events after `after` (the sequence cursor; `None` reads
    /// from the genesis), oldest first, at most `limit`.
    pub async fn list(
        &self,
        release_id: Uuid,
        after: Option<i64>,
        limit: i64,
    ) -> Result<Vec<EventRecord>, EventLogError> {
        let rows = sqlx::query_as::<
            _,
            (
                i64,
                time::OffsetDateTime,
                serde_json::Value,
                String,
                String,
                serde_json::Value,
                String,
                String,
            ),
        >(
            "SELECT sequence, at, actor, kind, reason, data, prev, hash \
             FROM events WHERE release_id = $1 AND sequence > $2 \
             ORDER BY sequence ASC LIMIT $3",
        )
        .bind(release_id)
        .bind(after.unwrap_or(0))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(sequence, at, actor, kind, reason, data, prev, hash)| EventRecord {
                    sequence,
                    at: Self::rfc3339(at),
                    actor,
                    kind,
                    reason,
                    data,
                    hash,
                    prev,
                },
            )
            .collect())
    }

    /// Whether the release row exists (the watch's 404 upfront).
    pub async fn release_exists(&self, release_id: Uuid) -> Result<bool, EventLogError> {
        let found = sqlx::query_scalar::<_, Uuid>("SELECT id FROM releases WHERE id = $1")
            .bind(release_id)
            .fetch_optional(&self.pool)
            .await?
            .is_some();
        Ok(found)
    }

    /// Whether the release row is terminal (the watch's close).
    pub async fn release_terminal(&self, release_id: Uuid) -> Result<bool, EventLogError> {
        let terminal = sqlx::query_scalar::<_, bool>("SELECT terminal FROM releases WHERE id = $1")
            .bind(release_id)
            .fetch_optional(&self.pool)
            .await?
            .unwrap_or(true);
        Ok(terminal)
    }
}

#[async_trait::async_trait]
impl EventSink for SqlEventLog {
    async fn append(
        &self,
        release_id: Uuid,
        event_id: &str,
        kind: &str,
        reason: &str,
        data: serde_json::Value,
        actor: Option<serde_json::Value>,
    ) -> Result<i64, EventLogError> {
        let mut tx = self.pool.begin().await?;
        // F-113: the per-release serialisation — the row lock holds
        // for the whole prev-read/sequence-assign/insert arc.
        let locked =
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM releases WHERE id = $1 FOR UPDATE")
                .bind(release_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(EventLogError::NotFound(release_id))?;
        let _ = locked;
        let prev: String = sqlx::query_scalar::<_, String>(
            "SELECT hash FROM events WHERE release_id = $1 \
             ORDER BY sequence DESC LIMIT 1",
        )
        .bind(release_id)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or_default();
        let sequence: i64 = sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM events WHERE release_id = $1",
        )
        .bind(release_id)
        .fetch_one(&mut *tx)
        .await?;
        // The actor: the caller's when given (the API seams), else the
        // release document's stamped actor (the interpreter's writes).
        let actor: serde_json::Value = match actor {
            Some(actor) => actor,
            None => sqlx::query_scalar::<_, Option<serde_json::Value>>(
                "SELECT document #> '{status,actor}' FROM releases WHERE id = $1",
            )
            .bind(release_id)
            .fetch_one(&mut *tx)
            .await?
            .unwrap_or(serde_json::Value::Null),
        };
        // One timestamp for the row AND the hash (a reader re-derives
        // the canonical JSON from the listed item; second precision —
        // Postgres's TIMESTAMPTZ keeps microseconds, so a nanosecond
        // would hash differently than the row stores).
        let at_instant = time::OffsetDateTime::now_utc()
            .replace_nanosecond(0)
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
        let at = Self::rfc3339(at_instant);
        // The hashed fields are exactly the stored row's (F-113): a
        // reader re-derives the canonical JSON from the item the API
        // lists.
        let hashed = serde_json::json!({
            "release": release_id.to_string(),
            "sequence": sequence,
            "at": at,
            "actor": actor,
            "kind": kind,
            "reason": reason,
            "data": data,
        });
        let hash = event_hash(&prev, &hashed);
        let inserted = sqlx::query(
            "INSERT INTO events \
             (id, release_id, sequence, at, actor, kind, reason, data, prev, hash) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(event_id)
        .bind(release_id)
        .bind(sequence)
        .bind(at_instant)
        .bind(&actor)
        .bind(kind)
        .bind(reason)
        .bind(&data)
        .bind(&prev)
        .bind(&hash)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            // The re-executed caller's append: converge on the
            // first's sequence (at-least-once, F-113).
            let existing: i64 =
                sqlx::query_scalar::<_, i64>("SELECT sequence FROM events WHERE id = $1")
                    .bind(event_id)
                    .fetch_one(&mut *tx)
                    .await?;
            tx.commit().await?;
            return Ok(existing);
        }
        tx.commit().await?;
        Ok(sequence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_hash_is_deterministic_and_order_sensitive() {
        let event = serde_json::json!({
            "release": Uuid::nil().to_string(),
            "sequence": 1,
            "at": "2026-01-01T00:00:00Z",
            "actor": {"name": "alice"},
            "kind": "created",
            "reason": "why",
            "data": {},
        });
        let first = event_hash("", &event);
        let same = event_hash("", &event);
        pretty_assertions::assert_eq!(first, same);
        let other = event_hash(&first, &event);
        pretty_assertions::assert_ne!(first, other);
        assert_eq!(first.len(), 64, "sha256 hex");
    }
}
