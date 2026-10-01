//! The engine's in-process integration suite: the reconciler's
//! send-only contract end-to-end (a stalled merge wait is woken by the
//! sweep and converges), and the secrets invariant (a named secret
//! never reaches DBOS's persisted state).

#![cfg(feature = "crash-hooks")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::print_stderr)]

mod fixture;

use std::sync::Arc;

use cargobike_core::model::EnvironmentPhase;
use pretty_assertions::assert_eq;

/// The app name the probe instance registers under (lowercase, dashed —
/// the launched-application naming rule).
const RECONCILER_APP: &str = "reconcile-probe";
/// The scan instance's app name.
const SCAN_APP: &str = "secrets-scan";

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
    let (services, provider) = fixture::mock_services(&scratch, pool.clone(), credentials);
    let snapshot = fixture::probe_snapshot(&release_id, RECONCILER_APP, fixture::WAIT_TEMPLATE);

    let mut config = dbos::Config::new(RECONCILER_APP, &database_url);
    config.schema = schema;
    config.app_version = Some("reconcile-probe-1".to_owned());
    let instance = dbos::DBOS::new(config);

    // Reconciler first (it borrows the registry's providers); the
    // interpreter's registration consumes its services.
    let correlation_repository =
        Arc::new(cargobike_engine::correlation::CorrelationRepository::new(pool.clone()));
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
    let (services, _provider) =
        fixture::mock_services_with_http(&scratch, pool.clone(), credentials, http.clone());

    let mut config = dbos::Config::new(SCAN_APP, &database_url);
    config.schema = schema.clone();
    config.app_version = Some("secrets-scan-1".to_owned());
    let instance = dbos::DBOS::new(config);
    let interpreter = cargobike_engine::register_interpreter(&instance, services)
        .expect("the interpreter registers before launch");
    instance.launch().await.expect("the instance launches");

    let release_id = uuid::Uuid::now_v7().to_string();
    let snapshot = fixture::probe_snapshot(&release_id, SCAN_APP, fixture::SECRET_TEMPLATE);
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
    let rows = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        sqlx::AssertSqlSafe(format!(
            "SELECT inputs, output, error FROM \"{schema}\".workflow_status"
        )),
    )
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
        (Arc::new(Self { seen: Arc::clone(&seen) }), Arc::clone(&seen))
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
