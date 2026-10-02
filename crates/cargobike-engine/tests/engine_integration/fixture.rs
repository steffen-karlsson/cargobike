//! The engine's in-process integration fixtures: the database seam, the
//! mock-provider boot, and the probe template (edits → change request →
//! `wait: merge`).

#![cfg(feature = "crash-hooks")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::print_stderr)]

use std::sync::Arc;

/// The fixture database's serialization: the DB-backed tests in this
/// binary + the harness-drive cases run concurrently under cargo's
/// test threads and together exceed the fixture's connection budget;
/// each test holds the lock for its whole span. The tokio mutex is
/// async-aware, so a test's awaits don't block its owning thread.
pub async fn db_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static THE_DB_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    THE_DB_LOCK
        .get_or_init(tokio::sync::Mutex::default)
        .lock()
        .await
}

/// The database the durable tests use: the fixture URL (CI passes it via
/// `CARGOBIKE_TEST_DATABASE_URL`; local development spins
/// `scripts/spike-postgres.sh`).
pub fn fixture_database_url() -> Option<String> {
    match std::env::var("CARGOBIKE_TEST_DATABASE_URL")
        .ok()
        .filter(|value| !value.is_empty())
    {
        Some(url) => Some(url),
        None => {
            eprintln!("skipping: no fixture database (CARGOBIKE_TEST_DATABASE_URL)");
            None
        }
    }
}

/// A per-test DBOS schema (lowercase alphanumeric): the DBOS system
/// tables are namespaced so parallel test binaries never collide.
pub fn schema_for_test(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default();
    format!("cbtest_{prefix}_{nanos:x}")
}

/// A disposable scratch directory keyed by test-time identity.
pub fn scratch_dir() -> String {
    std::env::temp_dir()
        .join(uuid::Uuid::now_v7().simple().to_string())
        .to_string_lossy()
        .to_string()
}

/// A pool over the fixture database, with the Cargobike row schema
/// (leases, CR correlation) applied — the server owns those migrations.
pub async fn fixture_pool(database_url: &str) -> sqlx::PgPool {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("the fixture database connects");
    sqlx::migrate!("../cargobike-server/migrations")
        .run(&pool)
        .await
        .expect("the cargobike migrations apply");
    pool
}

/// The probe template (the harness's waits variant, verbatim): two
/// action steps then `wait: merge` under the durable deadline.
pub const WAIT_TEMPLATE: &str = r#"
name: harness
version: "1"
inputs: {}
environment_inputs:
  repo: { type: repo }
  edits: { type: edits }
step_groups:
  - name: deploy
    steps:
      - id: edit
        uses: builtin/commit-files@1
      - id: cr
        uses: builtin/change-request@1
        with:
          branch: ${{ steps.edit.outputs.branch }}
      - wait: merge
        timeout: 7d
        on_modified: fail
        on_timeout: fail
environments:
  - name: stage
    steps: [include: deploy]
"#;

/// A one-step template for the secrets scan: an `http-call` whose header
/// value references the named secret.
pub const SECRET_TEMPLATE: &str = r#"
name: scan
version: "1"
inputs: {}
environment_inputs: {}
environments:
  - name: stage
    steps:
      - id: notify
        uses: builtin/http-call@1
        with:
          url: http://127.0.0.1:1/notify
          headers:
            Authorization: { secret: notify-token }
          body:
            release: ${{ release.id }}
"#;

/// A two-environment template: `stage` commits once; `prod` commits
/// again. The fork reproducer injects a single commit failure into the
/// `prod` commit so the first attempt fails there and a retry inherits
/// `stage`'s recorded steps.
pub const DUAL_ENV_TEMPLATE: &str = r#"
name: dual
version: "1"
inputs: {}
environment_inputs:
  repo: { type: repo }
  edits: { type: edits }
environments:
  - name: stage
    steps:
      - id: edit
        uses: builtin/commit-files@1
  - name: prod
    steps:
      - id: edit
        uses: builtin/commit-files@1
"#;

/// A compiled probe template with the mock repo + the release edit
/// staged into its one environment (interpreter-run shape).
pub fn probe_snapshot(
    release_id: &str,
    application: &str,
    version: &str,
    template: &str,
) -> cargobike_engine::ReleaseSnapshot {
    let mut compiled = cargobike_engine::template::compile(
        template,
        &cargobike_core::version::VersionScheme::Semver,
    )
    .expect("the probe template compiles");
    if let Some(stage) = compiled.environments.first_mut() {
        stage.env_inputs = std::collections::BTreeMap::from([
            (
                "repo".to_owned(),
                serde_json::json!({ "provider": "github", "id": "42" }),
            ),
            (
                "edits".to_owned(),
                serde_json::json!([
                    {
                        "file": "apps/stage/manifest.yaml",
                        "field": "image.tag",
                        "value": "1.0.0",
                    }
                ]),
            ),
        ]);
    }
    cargobike_engine::ReleaseSnapshot {
        template: compiled,
        release: cargobike_engine::ReleaseIdentity {
            id: release_id.to_owned(),
            application: application.to_owned(),
            version: version.to_owned(),
            version_scheme: cargobike_core::version::VersionScheme::Semver,
        },
        inputs: std::collections::BTreeMap::new(),
        step_type_versions: ["builtin/commit-files", "builtin/change-request"]
            .iter()
            .map(|name| (name.to_string(), "1".to_owned()))
            .collect(),
        content_hash: "probe".to_owned(),
    }
}

/// A compiled two-environment snapshot (the fork reproducer's shape):
/// every environment gets the mock repo and the staged manifest edit.
#[allow(clippy::expect_used)]
pub fn dual_env_snapshot(
    release_id: &str,
    application: &str,
    version: &str,
    template: &str,
) -> cargobike_engine::ReleaseSnapshot {
    let mut compiled = cargobike_engine::template::compile(
        template,
        &cargobike_core::version::VersionScheme::Semver,
    )
    .expect("the dual template compiles");
    for environment in compiled.environments.iter_mut() {
        environment.env_inputs = std::collections::BTreeMap::from([
            (
                "repo".to_owned(),
                serde_json::json!({ "provider": "github", "id": "42" }),
            ),
            (
                "edits".to_owned(),
                serde_json::json!([
                    {
                        "file": "apps/stage/manifest.yaml",
                        "field": "image.tag",
                        "value": version,
                    }
                ]),
            ),
        ]);
    }
    cargobike_engine::ReleaseSnapshot {
        template: compiled,
        release: cargobike_engine::ReleaseIdentity {
            id: release_id.to_owned(),
            application: application.to_owned(),
            version: version.to_owned(),
            version_scheme: cargobike_core::version::VersionScheme::Semver,
        },
        inputs: std::collections::BTreeMap::new(),
        step_type_versions: ["builtin/commit-files", "builtin/change-request"]
            .iter()
            .map(|name| (name.to_string(), "1".to_owned()))
            .collect(),
        content_hash: "dual-probe".to_owned(),
    }
}

/// The interpreter's services over the mock provider for one scratch
/// directory; the caller reads the mock state file through the returned
/// provider.
pub fn mock_services(
    scratch: &str,
    pool: sqlx::PgPool,
    instance: &dbos::DBOS,
    credentials: Arc<dyn cargobike_core::registry::CredentialStore>,
) -> (
    Arc<cargobike_engine::InterpreterServices>,
    Arc<cargobike_engine::mock::MockProvider>,
) {
    mock_services_with_http(
        scratch,
        pool,
        instance,
        credentials,
        Arc::new(cargobike_engine::mock::StubHttpService),
    )
}

/// The same boot with a caller-provided HTTP seam (the secrets scan
/// records the request to prove the secret's flow).
pub fn mock_services_with_http(
    scratch: &str,
    pool: sqlx::PgPool,
    instance: &dbos::DBOS,
    credentials: Arc<dyn cargobike_core::registry::CredentialStore>,
    http: Arc<dyn cargobike_core::step::HttpService>,
) -> (
    Arc<cargobike_engine::InterpreterServices>,
    Arc<cargobike_engine::mock::MockProvider>,
) {
    let provider = Arc::new(cargobike_engine::mock::MockProvider::at(scratch));
    let mut steps = cargobike_engine::StepRegistry::new();
    cargobike_engine::builtin::register_builtins(&mut steps);
    let mut providers = cargobike_core::registry::ProviderRegistry::new();
    providers.insert(
        "github",
        provider.clone() as Arc<dyn cargobike_core::provider::Provider>,
    );
    let services = Arc::new(cargobike_engine::InterpreterServices {
        steps: Arc::new(steps),
        providers: Arc::new(providers),
        credentials,
        http,
        leases: Arc::new(cargobike_engine::LeaseRepository::new(pool.clone())),
        statuses: Arc::new(cargobike_engine::SqlReleaseStatusStore::new(pool.clone())),
        correlations: Arc::new(cargobike_engine::correlation::CorrelationRepository::new(
            pool,
        )),
        instance: instance.clone(),
        cleanup_ref: Arc::new(std::sync::OnceLock::new()),
    });
    (services, provider)
}
