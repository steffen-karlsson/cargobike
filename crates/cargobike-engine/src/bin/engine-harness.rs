//! The harness binary (the feature `crash-hooks`): registers the real
//! interpreter over the mock provider and starts one release; the DRIVER
//! test kills it at a milestone and the respawn recovers.
//!
//! Usage: cargo run --features crash-hooks --bin engine-harness \
//!          -- <app-name> <scratch-dir> <release-id> <start|hold> [actions|waits]
//! Env: CB_HARNESS_DB_URL (the fixture Postgres), CB_CRASH_AT.
//!
//! (Test tooling: stdout/stderr reporting and panic-on-impossible state
//! are the contract, so the panic/print lints stay off here.)

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

#[cfg(not(feature = "crash-hooks"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("the harness builds only with --features crash-hooks")
}

#[cfg(feature = "crash-hooks")]
mod harness {
    use std::sync::Arc;

    use cargobike_core::registry::ProviderRegistry;
    use cargobike_core::version::VersionScheme;

    /// The harness's wait-free template: three action steps (the waits' wire
    /// behavior already lives in the spike's E3/E5 findings).
    pub const TEMPLATE: &str = r#"
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
      - id: notify
        uses: builtin/http-call@1
        with:
          url: http://127.0.0.1:1/notify
          body:
            release: ${{ release.id }}
      - id: labels
        uses: builtin/set-labels@1
        with:
          cr_number: ${{ steps.cr.outputs.number }}
          labels: [release]
environments:
  - name: stage
    steps: [include: deploy]
"#;

    /// The waits variant: actions, then the durable merge wait — a
    /// recovery crossing into `wait: merge` replays the recorded steps
    /// and blocks on the topic (the probes drive it).
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

    /// The template selection argument: `actions` (the default shape) or
    /// `waits` (the merge-wait variant; the deadline is the template's
    /// own).
    pub fn template_for(kind: Option<&str>) -> anyhow::Result<&'static str> {
        match kind.unwrap_or("actions") {
            "actions" => Ok(TEMPLATE),
            "waits" => Ok(WAIT_TEMPLATE),
            other => Err(anyhow::anyhow!("unknown template kind `{other}`")),
        }
    }

    pub fn run() -> anyhow::Result<()> {
        let args: Vec<String> = std::env::args().collect();
        let app = args
            .get(1)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("application"))?;
        let scratch = args
            .get(2)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("scratch dir"))?;
        let release_id = args
            .get(3)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("release id"))?;
        let mode = args
            .get(4)
            .cloned()
            .filter(|m| m == "start" || m == "hold")
            .ok_or_else(|| anyhow::anyhow!("mode is start or hold"))?;
        let database_url = std::env::var("CB_HARNESS_DB_URL")
            .map_err(|_| anyhow::anyhow!("CB_HARNESS_DB_URL unset"))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let template_kind = args.get(5).cloned();
        runtime.block_on(boot(
            app,
            scratch,
            release_id,
            mode,
            template_kind.as_deref(),
            database_url,
        ))
    }

    /// One DBOS boot + one interpreter start; the process holds until the
    /// driver kills it.
    async fn boot(
        app: String,
        scratch: String,
        release_id: String,
        mode: String,
        template_kind: Option<&str>,
        database_url: String,
    ) -> anyhow::Result<()> {
        let config = dbos::Config {
            // The executor id stays shared ('local', the default): the
            // recovery's re-enqueue adopts PENDING rows by executor id,
            // so the recovering process must own the killed one's rows.
            app_version: Some(format!("{app}-1")),
            ..dbos::Config::new(app.clone(), database_url.clone())
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .map_err(|error| anyhow::anyhow!("pool: {error}"))?;

        eprintln!("boot: pool connected");
        // The engine's lease repository needs the server schema; the
        // driver harness owns the fixture, so it migrates idempotently.
        sqlx::migrate!("../cargobike-server/migrations")
            .run(&pool)
            .await
            .map_err(|error| anyhow::anyhow!("migrations: {error}"))?;
        // The mock's state file lives in the shared scratch directory:
        // both the killed and the recovering processes converge there.
        let mock = Arc::new(cargobike_engine::mock::MockProvider::at(&scratch));
        let mut steps = cargobike_engine::StepRegistry::new();
        cargobike_engine::builtin::register_builtins(&mut steps);
        let mut providers = ProviderRegistry::new();
        providers.insert(
            "github",
            mock.clone() as Arc<dyn cargobike_core::provider::Provider>,
        );

        let instance = dbos::DBOS::new(config);
        let services = Arc::new(cargobike_engine::InterpreterServices {
            steps: Arc::new(steps),
            providers: Arc::new(providers),
            credentials: Arc::new(cargobike_engine::mock::StubCredentials),
            http: Arc::new(cargobike_engine::mock::StubHttpService),
            leases: Arc::new(cargobike_engine::LeaseRepository::new(pool.clone())),
            statuses: Arc::new(cargobike_engine::SqlReleaseStatusStore::new(pool.clone())),
            correlations: Arc::new(cargobike_engine::correlation::CorrelationRepository::new(
                pool,
            )),
            instance: instance.clone(),
            cleanup_ref: Arc::new(std::sync::OnceLock::new()),
        });

        eprintln!("boot: services ready");
        let interpreter = cargobike_engine::register_interpreter(&instance, services)?;
        eprintln!("boot: registered interpreter");
        instance
            .launch()
            .await
            .map_err(|failure| anyhow::anyhow!("launch: {failure:?}"))?;
        eprintln!("boot: launched");

        // The snapshot: the compiled template with the mock repo + edits
        // stamped into its one environment (the provision).
        let mut compiled = cargobike_engine::template::compile(
            template_for(template_kind)?,
            &VersionScheme::Semver,
        )
        .map_err(|error| anyhow::anyhow!("template: {error}"))?;
        let mut env_inputs = std::collections::BTreeMap::new();
        env_inputs.insert(
            "repo".to_owned(),
            serde_json::json!({"provider": "github", "id": "42"}),
        );
        env_inputs.insert(
            "edits".to_owned(),
            serde_json::json!([
                { "file": "apps/stage/manifest.yaml", "field": "image.tag", "value": "1.0.0" },
            ]),
        );
        if let Some(stage) = compiled.environments.first_mut() {
            stage.env_inputs = env_inputs;
        }

        let snapshot = cargobike_engine::ReleaseSnapshot {
            template: compiled,
            release: cargobike_engine::ReleaseIdentity {
                id: release_id.clone(),
                application: app.clone(),
                version: "1.0.0".to_owned(),
                version_scheme: VersionScheme::Semver,
            },
            inputs: std::collections::BTreeMap::new(),
            step_type_versions: {
                let mut map = std::collections::BTreeMap::new();
                map.insert("builtin/commit-files".to_owned(), "1".to_owned());
                map.insert("builtin/change-request".to_owned(), "1".to_owned());
                map.insert("builtin/http-call".to_owned(), "1".to_owned());
                map.insert("builtin/set-labels".to_owned(), "1".to_owned());
                map
            },
            content_hash: "crash-harness".to_owned(),
        };
        eprintln!("boot: compiled template");
        if mode == "start" {
            interpreter
                .start_with(
                    cargobike_engine::InterpretArgs { snapshot },
                    dbos::StartOptions {
                        workflow_id: Some(&format!("{app}/{release_id}")),
                        ..dbos::StartOptions::default()
                    },
                )
                .await
                .map_err(|failure| anyhow::anyhow!("start: {failure:?}"))?;
            eprintln!("boot: started; holding");
        } else {
            eprintln!("boot: holding for DBOS recovery");
        }
        // The hold self-expires: a driver that died leaves no eternal
        // holder hoarding the database's connections.
        let hold_started = std::time::Instant::now();
        loop {
            if hold_started.elapsed() > std::time::Duration::from_secs(600) {
                eprintln!("boot: the hold expired; the driver's run is over");
                anyhow::bail!("the hold expired");
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }
}

#[cfg(feature = "crash-hooks")]
fn main() -> anyhow::Result<()> {
    harness::run()
}
