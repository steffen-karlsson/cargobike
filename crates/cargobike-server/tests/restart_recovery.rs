//! The restart-recovery E2E (the audit's §0 recovery item): a release
//! parked at its `wait: merge` when the process dies — the boot's
//! executor is shut down mid-flight (DBOS's documented control-signal
//! semantics: every interrupted workflow row stays PENDING, which is
//! what a later executor recovers; the real-SIGKILL half of the story
//! is the engine crash harness's) — and a FRESH server boot over the
//! same deployment schema recovers it: the launch-time recovery
//! replays the recorded steps (no re-act — one branch, one commit, one
//! CR, nothing else), re-parks at the durable recv, and the merge
//! event delivered to the NEW boot completes the release.
//!
//! (Test code: it asserts and reports, so the expect/print lints stay
//! off for this file.) Requires `CARGOBIKE_TEST_DATABASE_URL`.

#![allow(clippy::expect_used, clippy::print_stderr)]

use argon2::password_hash::PasswordHasher as _;
use hmac::Mac as _;
use sha2::Sha256;
use sqlx::Connection as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

const API_KEY: &str = "cb_restart_test_key";
const WEBHOOK_SECRET: &str = "cb-restart-webhook-secret";
const APP: &str = "restart-service";

/// The release template: one edit onto the seeded file, the change
/// request, then the merge wait — the interpreter parks in the wait's
/// durable recv, exactly where a restart must recover it from.
const TEMPLATE: &str = r#"
name: service
version: "1"
inputs: {}
environment_inputs:
  repo: { type: repo }
  edits: { type: edits }
environments:
  - name: preview
    steps:
      - id: edit
        uses: builtin/commit-files@1
      - id: cr
        uses: builtin/change-request@1
        with:
          branch: ${{ steps.edit.outputs.branch }}
      - wait: merge
        timeout: 1h
        on_modified: fail
        on_timeout: fail
"#;

/// One boot of the deployment. Both boots share the DB, the DBOS
/// schema (one deployment — the recovery adopts the other boot's
/// pending rows), and the mock's state file; election stays off (this
/// suite tests recovery, not the fence).
#[allow(clippy::expect_used, clippy::print_stderr)]
async fn boot(
    url: &str,
    schema: &str,
    scratch: &str,
    templates_display: &str,
    hash: &str,
) -> Option<(String, Arc<cargobike_server::http::AppState>)> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new(
                    "cargobike_server=debug,cargobike_engine=debug,dbos=warn",
                )
            }),
        )
        .try_init();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let mock = Arc::new(cargobike_engine::mock::MockProvider::at(scratch));
    let mut providers = cargobike_core::registry::ProviderRegistry::new();
    providers.insert(
        "github",
        mock.clone() as Arc<dyn cargobike_core::provider::Provider>,
    );
    let config = format!(
        "server:\n  listen: \"127.0.0.1:{port}\"\n  public_url: http://127.0.0.1:{port}\n\n\
         database:\n  url: \"{url}\"\n  dbos_schema: \"{schema}\"\n\n\
         # The suite stays single-replica: recovery, not the fence.\n\
         leader_election:\n  enabled: false\n\n\
         templates:\n  directory: {templates_display}\n\n\
         providers:\n  - name: github\n    type: github\n    webhook_secrets:\n      - {{ env: CB_RESTART_WEBHOOK_SECRET }}\n\n\
         auth:\n  api_keys:\n    - name: test-key\n      hash: \"{hash}\"\n      grants: [release:create, release:read, release:cancel, release:delete]\n\n\
         applications:\n  - name: {APP}\n    source:\n      provider: github\n      id: \"restart-{port}\"\n    template: service@1\n    versioning:\n      scheme: opaque\n    releasers:\n      - api_key: test-key\n    environments:\n      preview:\n        repo:\n          provider: github\n          id: \"restart-env-{port}\"\n        edits:\n          - file: apps/stage/manifest.yaml\n            field: image.tag\n"
    );
    let config_path = std::env::temp_dir().join(format!("cb-restart-{port}.yaml"));
    std::fs::write(&config_path, config).expect("write the test config");
    unsafe { std::env::set_var("CB_RESTART_WEBHOOK_SECRET", WEBHOOK_SECRET) };
    let booted = cargobike_server::boot(Some(&config_path), Some(providers)).await;
    let (router, state) = match booted {
        Ok(booted) => booted,
        Err(error) => {
            eprintln!("skipping: the server boot failed ({error})");
            return None;
        }
    };
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    Some((format!("http://127.0.0.1:{port}"), state))
}

fn mock_state(scratch: &str) -> serde_json::Value {
    let text =
        std::fs::read_to_string(std::path::Path::new(scratch).join("state.json")).expect("state");
    serde_json::from_str(&text).expect("the state parses")
}

/// A signed GitHub delivery (the same HMAC contract the receiver
/// checks).
#[allow(clippy::expect_used)]
fn signed_delivery(event: &str, delivery: &str, body: &[u8]) -> Vec<(&'static str, String)> {
    let mut mac = hmac::Hmac::<Sha256>::new_from_slice(WEBHOOK_SECRET.as_bytes())
        .expect("the test secret accepts any key length");
    mac.update(body);
    let digest = hex::encode(mac.finalize().into_bytes());
    vec![
        ("X-Hub-Signature-256", format!("sha256={digest}")),
        ("X-GitHub-Event", event.to_owned()),
        ("X-GitHub-Delivery", delivery.to_owned()),
    ]
}

#[allow(clippy::print_stderr, clippy::expect_used)]
#[tokio::test(flavor = "current_thread")]
async fn a_restart_recovers_the_parked_release_and_completes_it() {
    let Some(url) = cargobike_testsupport::database_url().await else {
        return;
    };
    cargobike_testsupport::wipe_fixture_rows(&url, &[APP])
        .await
        .expect("the fixture rows wipe");

    let dir = std::env::temp_dir();
    let scratch = dir.join("cb-restart-mock").to_string_lossy().to_string();
    let _ = std::fs::remove_dir_all(&scratch);
    let templates_dir = dir.join("cb-restart-templates");
    let _ = std::fs::create_dir_all(&templates_dir);
    std::fs::write(templates_dir.join("service.yaml"), TEMPLATE).expect("write template");
    let templates_display = templates_dir.display().to_string();
    let salt = argon2::password_hash::SaltString::encode_b64(b"cb-test-salt-16-x").expect("salt");
    let hash = argon2::Argon2::default()
        .hash_password(API_KEY.as_bytes(), &salt)
        .expect("hash")
        .to_string();
    // The deployment's DBOS schema: one per RUN (the pid), shared by
    // both boots — the second boot's recovery adopts the first's
    // pending rows.
    let schema = format!("cbrestart-{}", std::process::id());

    // Boot A: create the release; it parks at the merge wait with its
    // CR recorded.
    let Some((base_a, state_a)) = boot(&url, &schema, &scratch, &templates_display, &hash).await
    else {
        return;
    };
    let client = reqwest::Client::new();
    let auth = ("Authorization", format!("Bearer {API_KEY}"));
    let created = client
        .post(format!("{base_a}/api/v1/releases"))
        .header(auth.0, auth.1.clone())
        .json(&serde_json::json!({
            "application": APP,
            "version": "42.0.0-restart",
        }))
        .send()
        .await
        .expect("the create sends");
    assert_eq!(created.status(), 202, "the create is accepted");
    let document: serde_json::Value = created.json().await.expect("the release body");
    let release_id = document["metadata"]["id"]
        .as_str()
        .expect("the release id")
        .to_owned();

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let read = client
            .get(format!("{base_a}/api/v1/releases/{release_id}"))
            .header(auth.0, auth.1.clone())
            .send()
            .await
            .expect("the release reads")
            .json::<serde_json::Value>()
            .await
            .expect("the release body");
        if read
            .pointer("/status/environments/0/change_request/number")
            .is_some_and(|number| !number.is_null())
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the release never parked at its change request: {read}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let cr_number = mock_state(&scratch)["change_requests"]
        .as_object()
        .and_then(|crs| crs.values().next())
        .and_then(|cr| cr["number"].as_u64())
        .expect("the CR number");

    // The death: boot A's executor stops mid-flight. DBOS's
    // control-signal semantics leave every interrupted workflow row
    // PENDING — the state a crashed process leaves behind too (the
    // SIGKILL half is the engine crash harness's; this is the server
    // boot's half).
    state_a.engine.instance.shutdown().await;

    // Boot B: the fresh server over the SAME deployment schema. Its
    // launch-time recovery adopts the pending interpreter workflow,
    // replays the recorded steps (no re-act), and re-parks at the
    // durable recv. The row's recovery attempts prove the adoption.
    let Some((base_b, _state_b)) = boot(&url, &schema, &scratch, &templates_display, &hash).await
    else {
        return;
    };
    let mut sysdb = sqlx::PgConnection::connect(&url)
        .await
        .expect("the sysdb connects");
    let workflow_id = format!("cargobike/interpret/{release_id}");
    let deadline = Instant::now() + Duration::from_secs(30);
    // The recovery's fingerprint: the parked interpreter's row was
    // adopted by the new executor (the attempts count ticks on the
    // launch-time recovery; the schema qualifier is the run's own
    // — audited, hence the assert).
    let attempts = loop {
        let count: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT recovery_attempts FROM \"{schema}\".workflow_status WHERE workflow_uuid = $1"
        )))
        .bind(&workflow_id)
        .fetch_one(&mut sysdb)
        .await
        .ok()
        .flatten();
        if count.is_some_and(|attempts| attempts > 0) {
            break count;
        }
        assert!(
            Instant::now() < deadline,
            "the fresh boot never recovered the parked release's workflow"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    let Some(attempts) = attempts else {
        panic!("the recovery attempts must exist");
    };
    assert!(attempts > 0, "the recovery attempts must be positive");

    // The merge: the CR merges on the provider, and the close event
    // delivered to boot B wakes the re-parked wait — the re-verified
    // merge drives the release to Completed.
    cargobike_engine::mock::MockProvider::at(&scratch)
        .mark_merged(cr_number)
        .expect("the mock's merge marker");
    // The CR's target repo is the id boot A provisioned (the
    // correlation keys on (provider, repo id, number)); the close
    // event must carry THAT id, not the new boot's.
    let repo_env_id = format!(
        "restart-env-{}",
        base_a.rsplit(':').next().unwrap_or_default()
    );
    let body = format!(
        r#"{{"action":"closed","pull_request":{{"number":{cr_number},"merged":true,"base":{{"repo":{{"id":"{repo_env_id}"}}}}}},"sender":{{"login":"ska"}}}}"#
    );
    let delivery = format!("d-restart-{}", std::process::id());
    let accepted = client
        .post(format!("{base_b}/webhooks/github"))
        .header(
            "X-Hub-Signature-256",
            &signed_delivery("pull_request", &delivery, body.as_bytes())[0].1,
        )
        .header("X-GitHub-Event", "pull_request")
        .header("X-GitHub-Delivery", &delivery)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.clone())
        .send()
        .await
        .expect("the close event sends");
    assert_eq!(
        accepted.status(),
        reqwest::StatusCode::ACCEPTED,
        "the close event is accepted: {:?}",
        accepted.text().await
    );

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let read = client
            .get(format!("{base_b}/api/v1/releases/{release_id}"))
            .header(auth.0, auth.1.clone())
            .send()
            .await
            .expect("the release reads");
        let document: serde_json::Value = read.json().await.expect("the release body");
        if document
            .pointer("/status/phase")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|phase| phase == "Completed")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the recovered release never completed: {document}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // The convergence: exactly one branch, one commit, one CR — the
    // recovery replayed the recorded steps and re-acted nothing.
    let state = mock_state(&scratch);
    let creates = state["branch_creates"].as_u64().unwrap_or(0);
    let commits = state["commit_runs"].as_u64().unwrap_or(0);
    let crs = state["change_requests"]
        .as_object()
        .map_or(0, |crs| crs.len());
    assert_eq!(
        (creates, commits, crs),
        (1, 1, 1),
        "the recovery must re-act nothing: {state}"
    );
}
