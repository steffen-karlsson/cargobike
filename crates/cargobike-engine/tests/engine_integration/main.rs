//! The engine's in-process integration suite: the reconciler's
//! send-only contract end-to-end (a stalled merge wait is woken by the
//! sweep and converges), and the secrets invariant (a named secret
//! never reaches DBOS's persisted state).

#![cfg(feature = "crash-hooks")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::print_stderr)]

mod crash_harness;
mod fixture;

use std::sync::Arc;

use cargobike_core::model::EnvironmentPhase;
use pretty_assertions::assert_eq;

/// The app name the probe instance registers under (lowercase, dashed —
/// the launched-application naming rule).
const RECONCILER_APP: &str = "reconcile-probe";
/// The scan instance's app name.
const SCAN_APP: &str = "secrets-scan";
const SUPERSEDE_APP: &str = "supersede-probe";
/// The fork reproducer's app name.
const FORK_APP: &str = "fork-retry-probe";
/// The output-cap probe's app name.
const OUTCAP_APP: &str = "out-cap-probe";

/// The correlation stamp carries the sweep's join keys; the merge wait's
/// step id is the template's auto-generated one (two declared ids
/// precede it, so the wait's resolution index is 2 → `merge-2`).
const WAIT_STEP_ID: &str = "merge-2";

/// The secret's known material: injected through the credential store,
/// carried by the http-call step as a header value, excluded from every
/// persisted surface.
const SECRET_MATERIAL: &str = "cb-scan-only-secret-material-42";

/// The named secret the template's step references.
const SECRET_NAME: &str = "notify-token";

#[tokio::test(flavor = "current_thread")]
async fn test_reconciler_signals_a_stalled_merge_wait() {
    let _the_db = fixture::db_lock().await;
    let Some(database_url) = fixture::fixture_database_url() else {
        return;
    };
    let scratch = fixture::scratch_dir();
    let schema = fixture::schema_for_test("reconciler");
    let pool = fixture::fixture_pool(&database_url).await;

    let release_id = uuid::Uuid::now_v7().to_string();
    let release_uuid: uuid::Uuid = uuid::Uuid::parse_str(&release_id).expect("the probe uuid");
    let workflow_id = format!("{RECONCILER_APP}/{release_id}");

    let credentials: Arc<dyn cargobike_core::registry::CredentialStore> =
        Arc::new(cargobike_engine::mock::StubCredentials);
    let snapshot =
        fixture::probe_snapshot(&release_id, RECONCILER_APP, "1.0.0", fixture::WAIT_TEMPLATE);

    let mut config = dbos::Config::new(RECONCILER_APP, &database_url);
    config.schema = schema;
    config.app_version = Some("reconcile-probe-1".to_owned());
    let instance = dbos::DBOS::new(config);
    let (services, provider) =
        fixture::mock_services(&scratch, pool.clone(), &instance, credentials);

    // Reconciler first (it borrows the registry's providers); the
    // interpreter's registration consumes its services.
    let correlation_repository = Arc::new(
        cargobike_engine::correlation::CorrelationRepository::new(pool.clone()),
    );
    let reconciler = cargobike_engine::reconciler::register_reconciler(
        &instance,
        Arc::new(cargobike_engine::reconciler::ReconcilerServices {
            providers: services.providers.clone(),
            correlations: Arc::clone(&correlation_repository),
            releases: cargobike_engine::reconciler::InMemoryPendingReleaseSource::fixture(&[
                release_uuid,
            ]),
        }),
    )
    .expect("the reconciler registers before launch");
    let interpreter = cargobike_engine::register_interpreter(&instance, services)
        .expect("the interpreter registers before launch");
    instance.launch().await.expect("the instance launches");

    let reconcile_loop = reconciler
        .start_with(
            cargobike_engine::reconciler::ReconcileArgs {
                interval: std::time::Duration::from_millis(250),
                batch_size: 10,
            },
            dbos::StartOptions {
                workflow_id: Some("reconcile-loop-1"),
                ..dbos::StartOptions::default()
            },
        )
        .await
        .expect("the reconciler loop starts");
    let _ = reconcile_loop; // a polling handle; shutdown() ends the loop

    let interpreter_handle = interpreter
        .start_with(
            cargobike_engine::InterpretArgs { snapshot },
            dbos::StartOptions {
                workflow_id: Some(&workflow_id),
                ..dbos::StartOptions::default()
            },
        )
        .await
        .expect("the interpreter starts");

    // The interpreter reaches its merge wait (the CR exists, open) — the
    // state the reconciliation's latency-optimisation covers.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let state = cargobike_engine::mock::MockState::load(&provider.state_file());
        if state.change_requests.len() == 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the interpreter never opened the change request; state {state:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let cr_number = {
        let state = cargobike_engine::mock::MockState::load(&provider.state_file());
        state
            .change_requests
            .values()
            .next()
            .expect("the probe's one change request")
            .number
    };
    // The correlation row the sweep joins on. Today the test stamps it;
    // the production change-request step stamps it later (the wiring is
    // tracked in the audit TODO). Why the row exists is not the point —
    // the sweep's contract (signal-only, never status writes) is.
    correlation_repository
        .stamp(&cargobike_engine::correlation::CorrelationRow {
            provider: "github".to_owned(),
            repo_id: "42".to_owned(),
            cr_number: cr_number as i64,
            release_id: release_uuid,
            environment: "stage".to_owned(),
            step_id: WAIT_STEP_ID.to_owned(),
            workflow_id: workflow_id.clone(),
        })
        .await
        .expect("the correlation stamps");

    // The provider reports merged — what a merge event means to the
    // sweep (the CR merged, the content landed on the base branch).
    provider
        .mark_merged(cr_number)
        .expect("the mock marks the CR merged");

    // The sweep's next pass signals `merge/stage/{WAIT_STEP_ID}`; the
    // interpreter wakes, re-verifies against the provider, completes.
    let result = interpreter_handle
        .result()
        .await
        .expect("the interpreter converges");
    assert_eq!(
        result
            .environments
            .first()
            .expect("the stage environment resolves first")
            .phase,
        EnvironmentPhase::Completed,
        "the merge wait completes on the reconciler's signal"
    );
    instance.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_named_secrets_never_reach_dbos_state() {
    let _the_db = fixture::db_lock().await;
    let Some(database_url) = fixture::fixture_database_url() else {
        return;
    };
    let scratch = fixture::scratch_dir();
    let schema = fixture::schema_for_test("secrets");
    let pool = fixture::fixture_pool(&database_url).await;

    let credentials: Arc<dyn cargobike_core::registry::CredentialStore> = Arc::new(
        cargobike_engine::mock::MapCredentials(std::collections::BTreeMap::from([(
            SECRET_NAME.to_owned(),
            secrecy::SecretString::from(SECRET_MATERIAL.to_owned()),
        )])),
    );
    // The recorded request proves the secret actually flowed through the
    // step's header (resolved inside the step body), so the scan below
    // excludes real usage rather than an accidental non-event.
    let (http, seen_request) = RecordingHttpService::new();

    let mut config = dbos::Config::new(SCAN_APP, &database_url);
    config.schema = schema.clone();
    config.app_version = Some("secrets-scan-1".to_owned());
    let instance = dbos::DBOS::new(config);
    let (services, _provider) = fixture::mock_services_with_http(
        &scratch,
        pool.clone(),
        &instance,
        credentials,
        http.clone(),
    );
    let interpreter = cargobike_engine::register_interpreter(&instance, services)
        .expect("the interpreter registers before launch");
    instance.launch().await.expect("the instance launches");

    let release_id = uuid::Uuid::now_v7().to_string();
    let snapshot =
        fixture::probe_snapshot(&release_id, SCAN_APP, "1.0.0", fixture::SECRET_TEMPLATE);
    let handle = interpreter
        .start_with(
            cargobike_engine::InterpretArgs { snapshot },
            dbos::StartOptions {
                workflow_id: Some(&format!("{SCAN_APP}/{release_id}")),
                ..dbos::StartOptions::default()
            },
        )
        .await
        .expect("the scan workflow starts");

    let result = handle.result().await.expect("the scan converges");
    assert_eq!(
        result
            .environments
            .first()
            .expect("the stage environment resolves first")
            .phase,
        EnvironmentPhase::Completed,
        "the secret-bearing pass completes"
    );
    // The step used the secret: the recorded request's header carries it
    // (the resolution happened inside the step body).
    {
        let request = seen_request
            .lock()
            .expect("the recording lock")
            .clone()
            .expect("the http step issued its request");
        let authorization = request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .expect("the Authorization header present")
            .1
            .clone();
        assert_eq!(authorization, SECRET_MATERIAL, "the secret fed the header");
    }

    // The invariant: the material is not in any persisted state surface.
    // `operation_outputs` carries step results; `workflow_status`
    // carries the workflow's input (the snapshot), output and error.
    let outputs: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT output FROM \"{schema}\".operation_outputs ORDER BY function_id"
    )))
    .fetch_all(&pool)
    .await
    .expect("the operation outputs are readable");
    for output in &outputs {
        assert!(
            !output.contains(SECRET_MATERIAL),
            "a step output carries the secret: {output:?}"
        );
    }
    let rows = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(sqlx::AssertSqlSafe(
        format!("SELECT inputs, output, error FROM \"{schema}\".workflow_status"),
    ))
    .fetch_all(&pool)
    .await
    .expect("the workflow status rows are readable");
    for (inputs, output, error) in rows {
        assert!(
            !inputs.contains(SECRET_MATERIAL),
            "the workflow input carries the secret: {inputs:?}"
        );
        if let Some(output) = output {
            assert!(
                !output.contains(SECRET_MATERIAL),
                "the workflow output carries the secret: {output:?}"
            );
        }
        if let Some(error) = error {
            assert!(
                !error.contains(SECRET_MATERIAL),
                "the workflow error carries the secret: {error:?}"
            );
        }
    }
    instance.shutdown().await;
}

/// An HTTP seam that records the request it served: the secrets scan
/// uses it to prove the named secret reached the step and is excluded
/// from the persisted state.
#[derive(Clone)]
struct RecordingHttpService {
    seen: Arc<std::sync::Mutex<Option<cargobike_core::step::HttpRequest>>>,
}

impl RecordingHttpService {
    fn new() -> (
        Arc<Self>,
        Arc<std::sync::Mutex<Option<cargobike_core::step::HttpRequest>>>,
    ) {
        let seen = Arc::new(std::sync::Mutex::new(None));
        (
            Arc::new(Self {
                seen: Arc::clone(&seen),
            }),
            Arc::clone(&seen),
        )
    }
}

#[async_trait::async_trait]
impl cargobike_core::step::HttpService for RecordingHttpService {
    async fn send(
        &self,
        request: cargobike_core::step::HttpRequest,
    ) -> Result<cargobike_core::step::HttpResponse, cargobike_core::provider::HttpError> {
        self.seen
            .lock()
            .expect("the recording lock")
            .replace(request);
        Ok(cargobike_core::step::HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: Default::default(),
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_supersede_cancels_the_old_attempt_and_cleans_up() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "cargobike_engine=debug,dbos=warn".to_owned()),
        )
        .try_init();

    let _the_db = fixture::db_lock().await;
    let Some(database_url) = fixture::fixture_database_url() else {
        return;
    };
    let scratch = fixture::scratch_dir();
    let schema = fixture::schema_for_test("supersede");
    let pool = fixture::fixture_pool(&database_url).await;

    let credentials: Arc<dyn cargobike_core::registry::CredentialStore> =
        Arc::new(cargobike_engine::mock::StubCredentials);

    let mut config = dbos::Config::new(SUPERSEDE_APP, &database_url);
    config.schema = schema;
    config.app_version = Some("supersede-1".to_owned());
    let instance = dbos::DBOS::new(config);
    let (services, provider) =
        fixture::mock_services(&scratch, pool.clone(), &instance, credentials);

    let cleanup = cargobike_engine::register_cleanup(
        &instance,
        Arc::new(cargobike_engine::CleanupServices {
            providers: services.providers.clone(),
            leases: services.leases.clone(),
        }),
    )
    .expect("the cleanup registers before launch");
    let _ = services.cleanup_ref.set(cleanup.clone());

    let interpreter = cargobike_engine::register_interpreter(&instance, services)
        .expect("the interpreter registers before launch");
    instance.launch().await.expect("the instance launches");

    // The rows first: the interpreter's status writes the release's
    // documents; the server's insert is what would have created them.
    let release_a = uuid::Uuid::now_v7().to_string();
    let release_b = uuid::Uuid::now_v7().to_string();
    let id_a = uuid::Uuid::parse_str(&release_a).expect("A's id");
    let id_b = uuid::Uuid::parse_str(&release_b).expect("B's id");
    // The previous run's rows: unique versions would be one answer; the
    // simple one is a wipe of this test's own application rows.
    sqlx::query("DELETE FROM releases WHERE application = $1")
        .bind(SUPERSEDE_APP)
        .execute(&pool)
        .await
        .expect("this application's old rows gone");
    seed_release(&pool, &id_a, SUPERSEDE_APP, "1.0.0").await;
    seed_release(&pool, &id_b, SUPERSEDE_APP, "1.1.0").await;

    let workflow_a = cargobike_engine::interpreter::interpret_workflow_id(&release_a);
    let handle_a = interpreter
        .start_with(
            cargobike_engine::InterpretArgs {
                snapshot: fixture::probe_snapshot(
                    &release_a,
                    SUPERSEDE_APP,
                    "1.0.0",
                    fixture::WAIT_TEMPLATE,
                ),
            },
            dbos::StartOptions {
                workflow_id: Some(&workflow_a),
                ..dbos::StartOptions::default()
            },
        )
        .await
        .expect("A starts");

    // Waits until A's CR exists (the wait-merge point; the lease held).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let state = cargobike_engine::mock::MockState::load(&provider.state_file());
        if state.change_requests.len() == 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "A never reached its CR; {state:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let mut data = query_release(&pool, &id_a).await;
    assert_eq!(
        data.pointer("/status/phase")
            .and_then(serde_json::Value::as_str),
        Some("Running"),
        "A is running at its merge wait: {data}"
    );

    // Release B: the higher version; the supersede takes over: the lease
    // transfers atomically, A is cancelled and cleaned, B proceeds.
    let workflow_b = cargobike_engine::interpreter::interpret_workflow_id(&release_b);
    let _handle_b = interpreter
        .start_with(
            cargobike_engine::InterpretArgs {
                snapshot: fixture::probe_snapshot(
                    &release_b,
                    SUPERSEDE_APP,
                    "1.1.0",
                    fixture::WAIT_TEMPLATE,
                ),
            },
            dbos::StartOptions {
                workflow_id: Some(&workflow_b),
                ..dbos::StartOptions::default()
            },
        )
        .await
        .expect("B starts");

    // Asserts: the lease's holder is B; A's release is Superseded
    // (terminal); A's CR is closed (the cleanup; the comment links B).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        data = query_release(&pool, &id_a).await;
        let superseded = data
            .pointer("/status/phase")
            .and_then(serde_json::Value::as_str)
            == Some("Superseded");
        let holder_is_b = holder_of(&pool, SUPERSEDE_APP)
            .await
            .map(|holder| holder == id_b)
            .unwrap_or(false);
        if superseded && holder_is_b {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the supersede chain never settled: release {data}, holder {:?}",
            holder_of(&pool, SUPERSEDE_APP).await
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // The mock's CR state: A's CR is closed (the cleanup) and B's own
    // CR is the single open one.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let state = cargobike_engine::mock::MockState::load(&provider.state_file());
        let open = state
            .change_requests
            .values()
            .filter(|summary| summary.state == "open")
            .count();
        if state.change_requests.len() == 2 && open == 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the supersede's cleanup never closed A's CR: {state:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // A clean finish: B's attempt is cancelled (the lease and the CR
    // are settled by the engine's own bookkeeping or a later test); A's
    // handle is dropped (a cancelled attempt's result never resolves
    // here).
    drop(handle_a);
    let _ = instance.cancel(&workflow_b).await;
    instance.shutdown().await;
}

/// The release row's insert (a test's minimal document: the release the
/// interpreter's status writes will fill).
async fn seed_release(pool: &sqlx::PgPool, id: &uuid::Uuid, application: &str, version: &str) {
    let now = time::OffsetDateTime::now_utc();
    let document = serde_json::json!({
        "metadata": {
            "id": id.to_string(),
            "created_at": now.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
            "updated_at": now.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
            "resource_version": 1,
            "retried_from": serde_json::Value::Null,
            "labels": {},
            "annotations": {},
        },
        "spec": {
            "application": application,
            "version": version,
            "source": { "provider": "github", "id": "42", "path": null },
            "template": "harness@1",
        },
        "status": {
            "phase": "Pending",
            "error": serde_json::Value::Null,
        },
    });
    sqlx::query(
        "INSERT INTO releases (id, application, version, phase, terminal, resource_version, created_at, updated_at, document) VALUES ($1, $2, $3, 'Pending', FALSE, 1, $4, $4, $5)",
    )
    .bind(id)
    .bind(application)
    .bind(version)
    .bind(now)
    .bind(document)
    .execute(pool)
    .await
    .expect("the release row's insert");
}

/// Reads the release's document half (the pool's own query).
async fn query_release(pool: &sqlx::PgPool, id: &uuid::Uuid) -> serde_json::Value {
    sqlx::query_scalar::<_, serde_json::Value>("SELECT document FROM releases WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .expect("the release row's document readable")
        .expect("the release row exists")
}

/// The lease's holder for an application's environment.
async fn holder_of(pool: &sqlx::PgPool, application: &str) -> Option<uuid::Uuid> {
    sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT holder_release_id FROM leases WHERE application = $1",
    )
    .bind(application)
    .fetch_optional(pool)
    .await
    .expect("the lease readable")
}

/// The fork reproducer (the F-23 retry): the first attempt fails at the
/// injected `prod` commit; the retry workflow forks the failed attempt
/// from the last failure, INHERITS the recorded `stage` commit (its
/// replay must not re-run), completes, appends the attempt record with
/// `fork_from`.
#[tokio::test(flavor = "current_thread")]
async fn test_fork_retry_reruns_only_the_failed_step() {
    let _the_db = fixture::db_lock().await;
    let Some(database_url) = fixture::fixture_database_url() else {
        return;
    };
    let scratch = fixture::scratch_dir();
    let schema = fixture::schema_for_test("forkretry");
    let pool = fixture::fixture_pool(&database_url).await;

    let credentials: Arc<dyn cargobike_core::registry::CredentialStore> =
        Arc::new(cargobike_engine::mock::StubCredentials);
    let mut config = dbos::Config::new(FORK_APP, &database_url);
    config.schema = schema;
    config.app_version = Some("fork-retry-1".to_owned());
    let instance = dbos::DBOS::new(config);
    let (services, provider) =
        fixture::mock_services(&scratch, pool.clone(), &instance, credentials);

    let interpreter = cargobike_engine::register_interpreter(&instance, services)
        .expect("the interpreter registers before launch");
    let retry = cargobike_engine::register_retry(
        &instance,
        Arc::new(cargobike_engine::RetryServices {
            statuses: Arc::new(cargobike_engine::SqlReleaseStatusStore::new(pool.clone())),
            instance: instance.clone(),
        }),
    )
    .expect("the retry workflow registers before launch");
    instance.launch().await.expect("the instance launches");

    // The rows the interpreter's status writes land in + unique keys.
    sqlx::query("DELETE FROM releases WHERE application = $1")
        .bind(FORK_APP)
        .execute(&pool)
        .await
        .expect("this application's rows clear");
    let release_id = uuid::Uuid::now_v7().to_string();
    let release_uuid = uuid::Uuid::parse_str(&release_id).expect("the fork's uuid");
    seed_release(&pool, &release_uuid, FORK_APP, "1.0.0").await;

    // The first attempt: the `prod` commit fails via the injection.
    provider
        .fail_next_commit_for("42")
        .expect("the inject scripts");
    let workflow = cargobike_engine::interpreter::interpret_workflow_id(&release_id);
    let _handle = interpreter
        .start_with(
            cargobike_engine::InterpretArgs {
                snapshot: fixture::dual_env_snapshot(
                    &release_id,
                    FORK_APP,
                    "1.0.0",
                    fixture::DUAL_ENV_TEMPLATE,
                ),
            },
            dbos::StartOptions {
                workflow_id: Some(&workflow),
                ..dbos::StartOptions::default()
            },
        )
        .await
        .expect("the first attempt starts");

    // The failed release: terminal, one attempt, stage committed, prod's
    // step unrecorded.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut document = query_release(&pool, &release_uuid).await;
    loop {
        if document
            .pointer("/status/phase")
            .and_then(serde_json::Value::as_str)
            == Some("Failed")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the first attempt never failed: {document}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        document = query_release(&pool, &release_uuid).await;
    }

    // The retry: the fork inherits the stage's recorded steps.
    let retry_id = cargobike_engine::retry_workflow_id(&release_id, &workflow);
    let _retry_handle = retry
        .start_with(
            cargobike_engine::RetryArgs {
                release_id: release_id.clone(),
            },
            dbos::StartOptions {
                workflow_id: Some(&retry_id),
                ..dbos::StartOptions::default()
            },
        )
        .await
        .expect("the retry starts");

    // The completed release: the fork's id in the new attempt (with
    // fork_from naming the source), the stage's side effects replayed
    // without a re-run, and the production env's final outcome.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        document = query_release(&pool, &release_uuid).await;
        let attempts = document
            .pointer("/status/attempts")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let fork_ok = attempts.len() == 2
            && attempts[1]
                .pointer("/fork_from")
                .and_then(serde_json::Value::as_str)
                == Some(workflow.as_str())
            && attempts[1]
                .pointer("/workflow_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| !id.is_empty())
            && attempts[1]
                .pointer("/workflow_id")
                .and_then(serde_json::Value::as_str)
                != attempts[0]
                    .pointer("/workflow_id")
                    .and_then(serde_json::Value::as_str);
        if document
            .pointer("/status/phase")
            .and_then(serde_json::Value::as_str)
            == Some("Completed")
            && fork_ok
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fork never completed: {document}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // The mock's effects: exactly 2 commits (the stage's once, the prod's
    // once per the surviving attempt) — the fork re-ran a failing step,
    // not everything recorded.
    let state = cargobike_engine::mock::MockState::load(&provider.state_file());
    assert!(
        state.commit_runs == 2,
        "the fork must not re-run the recorded commit (commit_runs = {}): {state:?}",
        state.commit_runs
    );
    instance.shutdown().await;
}

/// The F-40 cap: a step's serialized output over `max_step_output`
/// is a permanent step refusal — the release fails naming the cap.
#[tokio::test(flavor = "current_thread")]
async fn test_a_step_output_over_the_cap_refuses_the_step() {
    let _the_db = fixture::db_lock().await;
    let Some(database_url) = fixture::fixture_database_url() else {
        return;
    };
    let scratch = fixture::scratch_dir();
    let schema = fixture::schema_for_test("outcap");
    let pool = fixture::fixture_pool(&database_url).await;

    let mut config = dbos::Config::new(OUTCAP_APP, &database_url);
    config.schema = schema;
    config.app_version = Some("out-cap-1".to_owned());
    let instance = dbos::DBOS::new(config);
    let (services, _provider) = fixture::mock_services(
        &scratch,
        pool.clone(),
        &instance,
        Arc::new(cargobike_engine::mock::StubCredentials),
    );
    // The cap: 4 bytes — the commit-files' output's own JSON already
    // exceeds it, so the first step's run refuses at the recording.
    let capped = Arc::new(cargobike_engine::InterpreterServices {
        max_step_output: 4,
        cel_limits: cargobike_engine::expr::DEFAULT_LIMITS,
        ..(*services).clone()
    });

    sqlx::query("DELETE FROM releases WHERE application = $1")
        .bind(OUTCAP_APP)
        .execute(&pool)
        .await
        .expect("this application's rows clear");
    let interpreter = cargobike_engine::register_interpreter(&instance, capped)
        .expect("the interpreter registers before launch");
    instance.launch().await.expect("the instance launches");

    let release_id = uuid::Uuid::now_v7().to_string();
    let release_uuid = uuid::Uuid::parse_str(&release_id).expect("the cap uuid");
    seed_release(&pool, &release_uuid, OUTCAP_APP, "1.0.0").await;
    let workflow = cargobike_engine::interpreter::interpret_workflow_id(&release_id);
    let _handle = interpreter
        .start_with(
            cargobike_engine::InterpretArgs {
                snapshot: fixture::dual_env_snapshot(
                    &release_id,
                    OUTCAP_APP,
                    "1.0.0",
                    fixture::DUAL_ENV_TEMPLATE,
                ),
            },
            dbos::StartOptions {
                workflow_id: Some(&workflow),
                ..dbos::StartOptions::default()
            },
        )
        .await
        .expect("the capped run starts");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut document = query_release(&pool, &release_uuid).await;
    loop {
        if document
            .pointer("/status/phase")
            .and_then(serde_json::Value::as_str)
            == Some("Failed")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the capped step never refused: {document}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        document = query_release(&pool, &release_uuid).await;
    }
    let message = document
        .pointer("/status/error/message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        message.contains("the cap is"),
        "the refusal names the cap: {message}"
    );
    instance.shutdown().await;
}
