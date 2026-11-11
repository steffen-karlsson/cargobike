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

impl ReleaseRepository {
    /// The release a create idempotency key already named (F-109):
    /// `None` when the key is new or its release was deleted (the
    /// retried create provisions fresh then).
    pub async fn find_by_idempotency_key(
        &self,
        key: &str,
    ) -> Result<Option<serde_json::Value>, RepositoryError> {
        let row: Option<Row> = sqlx::query_as::<_, Row>(
            "SELECT releases.id, releases.application, releases.version, releases.phase, \
             releases.terminal, releases.resource_version, releases.document \
             FROM create_idempotency_keys \
             JOIN releases ON releases.id = create_idempotency_keys.release_id \
             WHERE create_idempotency_keys.key = $1",
        )
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| RepositoryError::Internal(error.to_string()))?;
        Ok(row.map(Row::into_json))
    }

    /// Records a key → release mapping (F-109). First writer wins: a
    /// racing create with the same key keeps its own release and the
    /// mapping points at whoever landed first.
    pub async fn record_idempotency_key(
        &self,
        key: &str,
        release_id: &Uuid,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO create_idempotency_keys (key, release_id) VALUES ($1, $2) \
             ON CONFLICT (key) DO NOTHING",
        )
        .bind(key)
        .bind(release_id)
        .execute(&self.pool)
        .await
        .map_err(|error| RepositoryError::Internal(error.to_string()))?;
        Ok(())
    }
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
        // F-147's provision half: the per-environment format strings
        // resolve their placeholders here (the snapshot carries values,
        // not templates).
        let format_values = FormatValues {
            application: &application,
            environment: &environment.name,
            version,
            release_id: &id.to_string(),
            release_url: parts
                .config
                .server
                .public_url
                .as_ref()
                .map(|public| format!("{public}/api/v1/releases/{id}")),
        };
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
                serde_json::json!(resolve_format_string(commit_message, &format_values)),
            );
        }
        // F-65: the registry-only branch-protection flag rides the
        // snapshot (R11: security-relevant settings are registry-only,
        // and the interpreter reads only the snapshot).
        staged.insert(
            "require_branch_protection".to_owned(),
            serde_json::json!(entry.require_branch_protection),
        );
        // R11's change-request half (F-144): the registry's per-env
        // `change_request` block rides the snapshot the same way; the
        // step merges it over the template's own `with` params.
        if let Some(overrides) = &entry.change_request {
            let mut resolved = overrides.clone();
            if let Some(title) = resolved.title.take() {
                resolved.title = Some(resolve_format_string(&title, &format_values));
            }
            if let Some(body) = resolved.body.take() {
                resolved.body = Some(resolve_format_string(&body, &format_values));
            }
            staged.insert(
                "change_request".to_owned(),
                serde_json::to_value(&resolved).unwrap_or_default(),
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

/// The create's origin context: who asked (the actor the document
/// records), which release a `--new` retry forks from, and the
/// idempotency key deduplicating retries. Bundled so the create path's
/// signatures stay under clippy's argument budget.
#[derive(Clone, Copy, Default)]
pub(crate) struct CreateOrigin<'a> {
    /// The caller's verified identity (F-83/F-84), when known.
    pub actor: Option<&'a ReleaseActor>,
    /// The release a `--new` retry forks from .
    pub retried_from: Option<&'a str>,
    /// The Idempotency-Key deduplicating create retries .
    pub idempotency_key: Option<&'a str>,
    /// The provenance SHA (F-95): the OIDC token's verified `sha`
    /// claim, the webhook payload's pushed SHA, or an API-key
    /// request's supplied SHA — the version policy verifies the tag
    /// against the provider's object with it.
    pub sha: Option<&'a str>,
}

/// The caller's identity as the release document records it (F-83):
/// the verified `(issuer, subject)` pair plus a display name, and the
/// CI context when the token carried one (F-84: claims only —
/// body-supplied values are never part of this).
#[derive(Clone, Debug)]
pub(crate) struct ReleaseActor {
    /// The `(issuer, subject)` + display name triple .
    pub actor: cargobike_core::model::Actor,
    /// The verified CI context , when the token described a run.
    pub ci: Option<cargobike_core::model::CiContext>,
}

/// The actor as the document's status object carries it.
fn actor_json(actor: Option<&ReleaseActor>) -> serde_json::Value {
    let Some(stamp) = actor else {
        return serde_json::Value::Null;
    };
    serde_json::json!({
        "issuer": stamp.actor.issuer,
        "subject": stamp.actor.subject,
        "display_name": stamp.actor.display_name,
    })
}

/// The CI context as the document's status object carries it.
fn ci_json(actor: Option<&ReleaseActor>) -> serde_json::Value {
    actor
        .and_then(|stamp| stamp.ci.as_ref())
        .and_then(|ci| serde_json::to_value(ci).ok())
        .unwrap_or(serde_json::Value::Null)
}

pub(crate) async fn create_release_core(
    state: &crate::http::AppState,
    app: &crate::config::ApplicationEntry,
    application: &str,
    version: &str,
    origin: CreateOrigin<'_>,
) -> Result<CreateOutcome, crate::http::errors::ApiError> {
    let parts = CoreParts {
        releases: &state.releases,
        config: state.config.borrow().clone(),
        providers: &state.engine.services.providers,
        step_types: state
            .engine
            .services
            .steps
            .installed()
            .into_iter()
            .collect(),
    };
    create_from_parts(
        &parts,
        &state.engine.interpreter,
        app,
        application,
        version,
        origin,
    )
    .await
}

/// The create core's separable dependencies (the parts the webhook
/// creator assembles without an AppState).
pub(crate) struct CoreParts<'a> {
    pub releases: &'a crate::release::ReleaseRepository,
    pub config: Arc<crate::config::Config>,
    pub step_types: BTreeMap<String, String>,
    /// The provider registry (F-95's tag verification reads the
    /// provider's tag object).
    pub providers: &'a cargobike_core::registry::ProviderRegistry,
}

/// The row half of a create: validation + provision + the
/// non-terminal-unique insert. The interpreter's start is a separate
/// step in the caller's flow — a start from inside a durable step is
/// refused by the DBOS surface, so the webhook's creator runs THIS and
/// the workflow's body starts the interpreters. The fresh outcome
/// carries the snapshot for the start; a duplicate carries none.
pub(crate) struct CreateRow {
    pub outcome: CreateOutcome,
    /// The row's id (a fresh insert's own; nil on the duplicate answer —
    /// the duplicate carries no snapshot so nothing starts).
    pub release_id: Uuid,
    pub snapshot: Option<cargobike_engine::ReleaseSnapshot>,
}

/// The tag name a version maps to under the app's `tag_format`.
pub(crate) fn tag_of_format(tag_format: &str, version: &str) -> String {
    tag_format.replace("{version}", version)
}

/// The format-string values a provision knows .
struct FormatValues<'a> {
    application: &'a str,
    environment: &'a str,
    version: &'a str,
    release_id: &'a str,
    /// The release's address (`{public_url}/api/v1/releases/{id}`);
    /// `None` when no `server.public_url` is set — `{release_url}`
    /// then stays literal (the config opted out of the public surface).
    release_url: Option<String>,
}

/// F-147's resolution half at provision time: the documented
/// placeholders substitute, `{{`/`}}` become literal braces, and
/// anything else passes through verbatim (validation already refused
/// unknown placeholders at boot; a literal that reaches here is a
/// missed validation, not a silent lie).
fn resolve_format_string(text: &str, values: &FormatValues<'_>) -> String {
    let mut resolved = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find(['{', '}']) {
        resolved.push_str(&rest[..open]);
        rest = &rest[open..];
        // The escapes first: `{{`/`}}` are literal braces.
        if let Some(escaped) = rest.strip_prefix("{{") {
            resolved.push('{');
            rest = escaped;
            continue;
        }
        if let Some(escaped) = rest.strip_prefix("}}") {
            resolved.push('}');
            rest = escaped;
            continue;
        }
        if rest.starts_with('}') {
            resolved.push('}');
            rest = &rest[1..];
            continue;
        }
        let Some(close) = rest.find('}') else {
            break;
        };
        let placeholder = &rest[..=close];
        let after = &rest[close + 1..];
        let value = match placeholder {
            "{application}" => values.application.to_owned(),
            "{environment}" => values.environment.to_owned(),
            "{version}" => values.version.to_owned(),
            "{release_id}" => values.release_id.to_owned(),
            "{release_url}" => values
                .release_url
                .clone()
                .unwrap_or_else(|| placeholder.to_owned()),
            _ => placeholder.to_owned(),
        };
        resolved.push_str(&value);
        rest = after;
    }
    resolved.push_str(rest);
    resolved
}

/// F-95's per-shape version verification: `require_tag` demands the
/// provider's own tag object. The tag named by `tag_format` + version
/// must EXIST, and when the caller carries a provenance SHA (the
/// OIDC token's verified `sha` claim, the webhook payload's pushed
/// SHA, or an API-key request's supplied SHA), the tag must point at
/// exactly it. The provider is the truth — payloads and claims alone
/// never pass. Providers without the `tag_lookup` capability fail
/// CLOSED (the policy demands a check the provider cannot make).
/// Apps without a `tag_format` cannot name their tag, so the policy
/// degrades to the scheme check alone (the webhook's tag trigger
/// refuses them instead, F-82).
async fn verify_version_provenance(
    parts: &CoreParts<'_>,
    app: &crate::config::ApplicationEntry,
    version: &str,
    origin: &CreateOrigin<'_>,
) -> Result<(), crate::http::errors::ApiError> {
    use crate::http::errors::{self as errors, ApiError};
    if !app.versioning.require_tag {
        return Ok(());
    }
    let Some(tag_format) = app.versioning.tag_format.as_deref() else {
        return Ok(());
    };
    let tag = tag_of_format(tag_format, version);
    let repo = cargobike_core::model::RepoRef::new(&app.source.provider, &app.source.id);
    let provider = parts.providers.resolve(&repo).map_err(|failure| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            errors::PROVIDER_NOT_FOUND,
            "provider-not-found",
            format!(
                "The application's source provider `{}` is not registered: {failure}.",
                app.source.provider
            ),
        )
    })?;
    let refusal = |detail: String| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            errors::VERSION_NOT_VERIFIED,
            "version-not-verified",
            format!("the version `{version}` does not verify against the provider: {detail}"),
        )
    };
    if !provider.capabilities().tag_lookup {
        return Err(refusal(format!(
            "the provider `{}` cannot read tag objects (the `tag_lookup` capability is off)",
            app.source.provider
        )));
    }
    let actual = provider
        .tag_sha(&repo, &tag)
        .await
        .map_err(|failure| match failure {
            cargobike_core::provider::ProviderError::NotFound(_) => {
                refusal(format!("the tag `{tag}` does not exist"))
            }
            other => ApiError::new(
                StatusCode::BAD_GATEWAY,
                errors::INTERNAL_ERROR,
                "internal-error",
                format!("the provider's tag lookup failed: {other}"),
            ),
        })?;
    if let Some(claimed) = origin.sha {
        if !claimed.eq_ignore_ascii_case(&actual) {
            return Err(refusal(format!(
                "the tag `{tag}` points at `{actual}`, not the caller's `{claimed}`"
            )));
        }
    }
    Ok(())
}

pub(crate) async fn create_release_row(
    parts: &CoreParts<'_>,
    app: &crate::config::ApplicationEntry,
    application: &str,
    version: &str,
    origin: CreateOrigin<'_>,
) -> Result<CreateRow, crate::http::errors::ApiError> {
    let id = Uuid::now_v7();
    let now = sqlx::types::time::OffsetDateTime::now_utc();

    // The key precedes everything (F-109): a retried create with the
    // same `Idempotency-Key` converges on the release the first attempt
    // produced, whatever the body says — the caller already has this
    // release, and a re-provision would only fork the caller's intent.
    if let Some(key) = origin.idempotency_key {
        if let Some(existing_json) = parts
            .releases
            .find_by_idempotency_key(key)
            .await
            .map_err(repository_to_api)?
        {
            let existing_id = existing_json
                .pointer("/metadata/id")
                .and_then(serde_json::Value::as_str)
                .and_then(|text| Uuid::parse_str(text).ok())
                .unwrap_or_else(Uuid::nil);
            return Ok(CreateRow {
                outcome: CreateOutcome {
                    document: existing_json,
                    created: false,
                },
                release_id: existing_id,
                snapshot: None,
            });
        }
    }

    // F-142: a paused application refuses the create with a clear 409 —
    // the release could make no progress, and a zombie `Running` row
    // helps nobody.
    if let Some(reason) = paused_reason(app.paused.as_ref()) {
        return Err(paused_refusal("application", application, reason));
    }

    // The provision first: the compiled template's environment list is
    // the doc's initial shape (every environment opens `Pending`, so a
    // partial start can never roll the release-phase `Completed` while
    // a later environment never ran — the rollup otherwise only sees
    // the environments a write touched).
    let snapshot = provision_snapshot_parts(parts, app, version, id).await?;
    // F-142, per-environment half: a paused environment refuses the
    // create the same way. v1 documents this as a deviation from the
    // PRD's in-flight wording (snapshot pinning means a mid-flight
    // pause needs a live pause/wake control plane, not a registry
    // flag); refusing the create is the fail-safe stopgap.
    for environment in snapshot.template.environments.iter() {
        if let Some(reason) = app
            .environments
            .get(&environment.name)
            .and_then(|entry| paused_reason(entry.paused.as_ref()))
        {
            return Err(paused_refusal("environment", &environment.name, reason));
        }
    }
    // F-95: the version verifies against the provider's tag object
    // before the row lands — a forged payload or a stale claim must
    // never open a release.
    verify_version_provenance(parts, app, version, &origin).await?;
    let provisions = snapshot
        .template
        .environments
        .iter()
        .map(|env| {
            serde_json::json!({
                "name": env.name,
                "phase": "Pending",
                "change_request": serde_json::Value::Null,
                "started_at": serde_json::Value::Null,
                "completed_at": serde_json::Value::Null,
            })
        })
        .collect::<Vec<_>>();
    let retried = origin
        .retried_from
        .map(|from| serde_json::Value::String(from.to_owned()))
        .unwrap_or(serde_json::Value::Null);
    let document = serde_json::json!({
        "metadata": {
            "id": id.to_string(),
            "created_at": format_rfc3339(now),
            "updated_at": format_rfc3339(now),
            "resource_version": 1,
            "retried_from": retried,
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
            "actor": actor_json(origin.actor),
            "ci": ci_json(origin.actor),
            "environments": provisions,
        },
    });
    let existing = parts
        .releases
        .create(&document, id, application, version, "Pending", now)
        .await
        .map_err(repository_to_api)?;
    if let Some(existing_json) = existing {
        // a duplicate create answers 200 with the existing release.
        let existing_id = existing_json
            .pointer("/metadata/id")
            .and_then(serde_json::Value::as_str)
            .and_then(|text| Uuid::parse_str(text).ok())
            .unwrap_or_else(Uuid::nil);
        return Ok(CreateRow {
            outcome: CreateOutcome {
                document: existing_json,
                created: false,
            },
            release_id: existing_id,
            snapshot: None,
        });
    }
    Ok(CreateRow {
        outcome: CreateOutcome {
            document,
            created: true,
        },
        release_id: id,
        snapshot: Some(snapshot),
    })
}

/// The pause decision of a registry entry (F-142): `Some(reason)` means
/// paused (the reason is optional — an empty text is a bare `paused:
/// true`), `None` means running.
fn paused_reason(paused: Option<&crate::config::PausedEntry>) -> Option<&str> {
    match paused {
        Some(crate::config::PausedEntry::Reason { reason }) => Some(reason.as_str()),
        Some(crate::config::PausedEntry::Bool(true)) => Some(""),
        _ => None,
    }
}

/// The paused refusal (F-142): a `StateConflict` naming what is frozen
/// and why — the caller's problem document must be actionable.
fn paused_refusal(kind: &str, name: &str, reason: &str) -> crate::http::errors::ApiError {
    let detail = if reason.is_empty() {
        format!("The {kind} `{name}` is paused; new releases are refused while it is.")
    } else {
        format!("The {kind} `{name}` is paused ({reason}); new releases are refused while it is.")
    };
    crate::http::errors::ApiError::new(
        axum::http::StatusCode::CONFLICT,
        crate::http::errors::STATE_CONFLICT,
        "state-conflict",
        detail,
    )
}

/// The key half of a fresh create (F-109): first writer wins; the
/// racing loser keeps its own release but the mapping names the
/// winner, so the NEXT retry converges.
async fn record_create_key(
    parts: &CoreParts<'_>,
    idempotency_key: Option<&str>,
    release_id: &Uuid,
) {
    if let Some(key) = idempotency_key {
        if let Err(failure) = parts.releases.record_idempotency_key(key, release_id).await {
            tracing::error!(
                release = %release_id,
                %failure,
                "the create idempotency key failed to record"
            );
        }
    }
}

/// The interpreter's start for a provisioned, already-inserted release
/// (the workflow id deduplicates; a replayed create joins the running
/// workflow).
pub(crate) async fn start_release_interpreter(
    interpreter: &dbos::WorkflowRef<
        cargobike_engine::InterpretArgs,
        cargobike_engine::InterpretResult,
        cargobike_engine::InterpreterError,
    >,
    release_id: Uuid,
    snapshot: cargobike_engine::ReleaseSnapshot,
) {
    let workflow_id = format!("cargobike/interpret/{release_id}");
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
        tracing::error!(release = %release_id, %failure, "the interpreter's start refused");
    }
}

/// The full create: row + interpreter start (the HTTP create's and the
/// webhook's compat shape; the webhook body uses the split halves).
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
    origin: CreateOrigin<'_>,
) -> Result<CreateOutcome, crate::http::errors::ApiError> {
    let created = create_release_row(parts, app, application, version, origin).await?;
    let Some(snapshot) = created.snapshot else {
        return Ok(created.outcome);
    };
    record_create_key(parts, origin.idempotency_key, &created.release_id).await;
    start_release_interpreter(interpreter, created.release_id, snapshot).await;
    Ok(created.outcome)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// F-147's resolution: documented placeholders substitute,
    /// `{{`/`}}` become literal braces, unknown text passes through
    /// verbatim (validation refused the unknowns at boot).
    #[test]
    fn test_resolve_format_string_substitutes_and_escapes() {
        let values = FormatValues {
            application: "my-service",
            environment: "preview",
            version: "1.2.3",
            release_id: "018f-abc",
            release_url: Some("http://localhost:8080/api/v1/releases/018f-abc".to_owned()),
        };
        assert_eq!(
            resolve_format_string("Release {application} {version} to {environment}", &values),
            "Release my-service 1.2.3 to preview"
        );
        assert_eq!(
            resolve_format_string("See {release_url} ({release_id})", &values),
            "See http://localhost:8080/api/v1/releases/018f-abc (018f-abc)"
        );
        assert_eq!(
            resolve_format_string("Literal {{braces}} stay", &values),
            "Literal {braces} stay"
        );
        assert_eq!(
            resolve_format_string("The tag_format-like {version} and literal brace {", &values),
            "The tag_format-like 1.2.3 and literal brace {"
        );
        // No public_url: the {release_url} placeholder stays literal
        // (the config opted out of the public surface).
        let no_url = FormatValues {
            application: "my-service",
            environment: "preview",
            version: "1.2.3",
            release_id: "018f-abc",
            release_url: None,
        };
        assert_eq!(
            resolve_format_string("At {release_url}", &no_url),
            "At {release_url}"
        );
    }
}
