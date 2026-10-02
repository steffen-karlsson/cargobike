//! Release service: idempotent create, list with cursor
//! pagination , get, cancel and terminal-only delete
//! under optimistic concurrency (If-Match / resource_version).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::RepositoryError;

/// The SQL access for releases; JSONB documents + indexed columns.
pub struct ReleaseRepository {
    pool: PgPool,
}

/// Row storage shape (the non-macro query; sqlx 0.9 needs a live DATABASE_URL
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

    /// Inserts when no non-terminal release for (the application, version)
    /// exists ; `Ok(Some(the existing))` names the active duplicate —
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

    /// Updates the phase (the cancel). Safety rails: a mismatched
    /// `resource_version` (the If-Match guard) refuses, and a terminal
    /// release never re-phases.
    pub async fn set_phase(
        &self,
        id: &Uuid,
        phase: &str,
        terminal: bool,
        when: sqlx::types::time::OffsetDateTime,
        expected_version: Option<u64>,
    ) -> Result<(), RepositoryError> {
        let result = sqlx::query(
            "UPDATE releases SET phase = $2, terminal = $3, \
             resource_version = resource_version + 1, updated_at = $4 \
             WHERE id = $1 \
               AND NOT terminal \
               AND ($5::bigint IS NULL OR resource_version = $5::bigint)",
        )
        .bind(id)
        .bind(phase)
        .bind(terminal)
        .bind(when)
        .bind(expected_version.map(|v| v as i64))
        .execute(&self.pool)
        .await
        .map_err(|error| RepositoryError::Internal(error.to_string()))?;
        if result.rows_affected() == 0 {
            let existing =
                sqlx::query_as::<_, (bool,)>("SELECT terminal FROM releases WHERE id = $1")
                    .bind(id)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(|error| RepositoryError::Internal(error.to_string()))?;
            return match existing {
                Some((true,)) => Err(RepositoryError::Conflict),
                Some((false,)) => Err(RepositoryError::ResourceVersionMismatch),
                None => Err(RepositoryError::NotFound(id.to_string())),
            };
        }
        Ok(())
    }

    /// Deletes a terminal release ; the event log is retained.
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

    /// Lists releases with the filters, newest first, cursor over
    /// UUIDv7 ids (the limit: 1-500, `after`/`before`).
    #[allow(clippy::too_many_arguments)] // the query surface is the query surface
    pub async fn list(
        &self,
        application: Option<&str>,
        phase: Option<&str>,
        version: Option<&str>,
        since: Option<sqlx::types::time::OffsetDateTime>,
        limit: u32,
        after: Option<Uuid>,
        before: Option<Uuid>,
    ) -> Result<(Vec<serde_json::Value>, Option<Uuid>), RepositoryError> {
        let sql = "SELECT id, application, version, phase, terminal, resource_version, document FROM releases WHERE \
               ($1::text IS NULL OR application = $1::text) \
               AND ($2::text IS NULL OR phase = $2::text) \
               AND ($3::text IS NULL OR version = $3::text) \
               AND ($4::timestamptz IS NULL OR created_at >= $4) \
               AND ($5::uuid IS NULL OR id < $5::uuid) \
               AND ($6::uuid IS NULL OR id > $6::uuid) \
               ORDER BY id DESC LIMIT $7";

        // Look-ahead: one extra row decides `has_more` (the cursor is
        // a real sentinel, not just 'a page was returned'); published
        // items snap to the requested limit.
        let rows = sqlx::query_as::<_, Row>(sql)
            .bind(application)
            .bind(phase)
            .bind(version)
            .bind(since)
            .bind(after)
            .bind(before)
            .bind((limit as i64) + 1)
            .fetch_all(&self.pool)
            .await
            .map_err(|error| RepositoryError::Internal(error.to_string()))?;
        let has_more = rows.len() as u32 > limit;
        let mut rows = rows;
        if has_more {
            rows.truncate(limit as usize);
        }
        let items: Vec<serde_json::Value> = rows.into_iter().map(|row| row.into_json()).collect();
        let cursor = if has_more {
            items
                .last()
                .and_then(|doc| doc["metadata"]["id"].as_str())
                .and_then(|v| Uuid::parse_str(v).ok())
        } else {
            None
        };
        Ok((items, cursor))
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

/// The provision: the release snapshot (the compiled template + the
/// registry's staged inputs + the pinned step-type versions + the
/// content hash) — the interpreter's one durable argument.
pub(crate) async fn provision_snapshot_parts(
    parts: &CoreParts<'_>,
    app: &crate::config::ApplicationEntry,
    version: &str,
    id: Uuid,
) -> Result<cargobike_engine::ReleaseSnapshot, crate::http::errors::ApiError> {
    use crate::http::errors::{self as errors, ApiError};
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
    let templates = crate::validation::load_templates(&parts.config).map_err(|error| {
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

    let step_type_versions = parts.step_types.clone();
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

/// The registry's versioning block to the version scheme.
pub(crate) fn version_scheme_of(
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

/// The create core (the HTTP handler and the webhook's tag-push creator
/// share it): the version-policy validation, the provision, the
/// non-terminal-unique insert and the interpreter's deduplicating start.
/// `created` is false for the active-duplicate answer (200 with the
/// existing document); fresh releases return the document for 202.
pub(crate) struct CreateOutcome {
    pub document: serde_json::Value,
    pub created: bool,
}

pub(crate) async fn create_release_core(
    state: &crate::http::AppState,
    app: &crate::config::ApplicationEntry,
    application: &str,
    version: &str,
) -> Result<CreateOutcome, crate::http::errors::ApiError> {
    let parts = CoreParts {
        releases: &state.releases,
        config: state.config.borrow().clone(),
        step_types: state
            .engine
            .services
            .steps
            .installed()
            .into_iter()
            .collect(),
    };
    create_from_parts(&parts, &state.engine.interpreter, app, application, version).await
}

/// The create core's separable dependencies (the parts the webhook
/// creator assembles without an AppState).
pub(crate) struct CoreParts<'a> {
    pub releases: &'a crate::release::ReleaseRepository,
    pub config: Arc<crate::config::Config>,
    pub step_types: BTreeMap<String, String>,
}

pub(crate) async fn create_from_parts(
    parts: &CoreParts<'_>,
    interpreter: &dbos::WorkflowRef<
        cargobike_engine::InterpretArgs,
        cargobike_engine::InterpretResult,
        cargobike_engine::InterpreterError,
    >,
    app: &crate::config::ApplicationEntry,
    application: &str,
    version: &str,
) -> Result<CreateOutcome, crate::http::errors::ApiError> {
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
    let snapshot = provision_snapshot_parts(parts, app, version, id).await?;

    let existing = parts
        .releases
        .create(&document, id, application, version, "Pending", now)
        .await
        .map_err(repository_to_api)?;
    if let Some(existing_json) = existing {
        // a duplicate create answers 200 with the existing release.
        return Ok(CreateOutcome {
            document: existing_json,
            created: false,
        });
    }

    // The interpreter's start (the workflow id deduplicates; a replayed
    // create joins the workflow already running).
    let workflow_id = format!("cargobike/interpret/{id}");
    if let Err(failure) = interpreter
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
    Ok(CreateOutcome {
        document,
        created: true,
    })
}

/// repository_to_api lives at the http edge; the core's shared mapping.
pub(crate) fn repository_to_api(error: RepositoryError) -> crate::http::errors::ApiError {
    match error {
        RepositoryError::NotFound(id) => crate::http::errors::ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            crate::http::errors::RELEASE_NOT_FOUND,
            "release-not-found",
            format!("The release `{id}` was not found."),
        ),
        e => {
            tracing::error!(%e, "the release repository failed");
            crate::http::errors::ApiError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                crate::http::errors::INTERNAL_ERROR,
                "internal-error",
                "The release store failed; the server's log has the detail.",
            )
        }
    }
}
