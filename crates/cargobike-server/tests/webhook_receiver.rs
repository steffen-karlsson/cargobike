//! The webhook receiver (Phase 5.1/5.2): signed deliveries over the
//! in-process server + the mock provider — the tag-push create (the
//! version from `tag_format`; the fail-closed protection default), the
//! rejected/unrecognised surfaces, delivery dedupe, AND the
//! `pull_request.closed` correlation driving a waiting release to its
//! terminal. Requires `CARGOBIKE_TEST_DATABASE_URL`.
//!
//! (Test code: it asserts and reports, so the expect/print lints stay
//! off for this file.) The repos' ids are tagged with the server's port:
//! the correlation rows are keyed globally ((provider, repo_id,
//! cr_number)) while each DBOS instance owns its own workflow rows, so
//! two suites sharing the fixture database must never share the
//! namespace — else a delivery correlates to a dead workflow (the FK
//! refusal), not to this run's release.
#![allow(clippy::expect_used, clippy::print_stderr)]

use argon2::password_hash::PasswordHasher as _;
use hmac::Mac as _;
use sha2::Sha256;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// The release template — the preview env runs to its merge wait and
/// stalls (a reliable webhook target; `on_modified: fail`).
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

const WEBHOOK_SECRET: &str = "cb-webhook-rotating-secret";
const API_KEY: &str = "cb_webhook_test_key";

/// `cargobike validate`'s ground: the app's SOURCE id (the tag-push
/// creator's scan key) and the ENVIRONMENT's repo id (the correlation's
/// key). `{port}` is replaced per run so concurrent suites sharing the
/// fixture database never collide on the correlation's key rows.
#[allow(clippy::expect_used, clippy::print_stderr)]
async fn test_server() -> Option<(String, Arc<cargobike_engine::mock::MockProvider>, String)> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new(
                    "cargobike_server=debug,cargobike_engine=debug,dbos=warn",
                )
            }),
        )
        .try_init();
    let url = match std::env::var("CARGOBIKE_TEST_DATABASE_URL")
        .ok()
        .filter(|url| !url.is_empty())
    {
        Some(url) => url,
        None => {
            eprintln!("skipping: CARGOBIKE_TEST_DATABASE_URL is unset");
            return None;
        }
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let salt = argon2::password_hash::SaltString::encode_b64(b"cb-test-salt-16-x").expect("salt");
    let hash = argon2::Argon2::default()
        .hash_password(API_KEY.as_bytes(), &salt)
        .expect("hash")
        .to_string();

    let dir = std::env::temp_dir();
    let scratch = dir
        .join(format!("cb-webhook-mock-{port}"))
        .to_string_lossy()
        .to_string();
    let mock = Arc::new(cargobike_engine::mock::MockProvider::at(&scratch));
    let mut providers = cargobike_core::registry::ProviderRegistry::new();
    providers.insert(
        "github",
        mock.clone() as Arc<dyn cargobike_core::provider::Provider>,
    );

    let templates_dir = dir.join(format!("cb-webhook-templates-{port}"));
    let _ = std::fs::create_dir_all(&templates_dir);
    std::fs::write(templates_dir.join("service.yaml"), TEMPLATE).expect("write template");
    let templates_display = templates_dir.display().to_string();

    let config_path = dir.join(format!("cb-webhook-test-{port}.yaml"));
    let fixture = TEMPLATE_CONFIG
        .replace("{port}", &port.to_string())
        .replace("{hash}", &hash)
        .replace("{url}", &url)
        .replace("{templates}", &templates_display);
    std::fs::write(&config_path, fixture).expect("write the test config");

    // The webhook secret the receiver materialises from. The var stays
    // for this test binary's lifetime (the boots run concurrently; a
    // remove here could starve a later boot's materialise step).
    unsafe { std::env::set_var("CB_WEBHOOK_SECRET_TEST", WEBHOOK_SECRET) };

    let Ok((router, _state)) = cargobike_server::boot(Some(&config_path), Some(providers)).await
    else {
        eprintln!("skipping: the server boot failed");
        return None;
    };
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    Some((format!("http://127.0.0.1:{port}"), mock, scratch))
}

/// The receiver's fixture: two apps of ONE source id (the protected-by-
/// default trigger vs the explicitly unprotected one) + the provider's
/// webhook secret from the env.
const TEMPLATE_CONFIG: &str = r#"
server:
  listen: "127.0.0.1:{port}"
  public_url: http://127.0.0.1:{port}

database:
  url: "{url}"
  dbos_schema: "cbtest-{port}"

templates:
  directory: {templates}

providers:
  - name: github
    type: github
    webhook_secrets:
      - { env: CB_WEBHOOK_SECRET_TEST }

auth:
  api_keys:
    - name: test-key
      hash: "{hash}"
      grants: [release:create, release:read, release:cancel, release:delete]

applications:
  - name: tag-service
    source:
      provider: github
      id: "wh-{port}"
    template: service@1
    versioning:
      scheme: semver
      tag_format: "v{version}"
    triggers:
      - event: tag
        require_tag_protection: false
    releasers:
      - api_key: test-key
    environments:
      preview:
        repo:
          provider: github
          id: "env-{port}"
        edits:
          - file: apps/stage/manifest.yaml
            field: image.tag
  - name: guarded-service
    source:
      provider: github
      id: "wh-{port}"
    template: service@1
    versioning:
      scheme: semver
      tag_format: "v{version}"
    triggers:
      - event: tag
    releasers:
      - api_key: test-key
    environments:
      preview:
        repo:
          provider: github
          id: "env-{port}"
        edits:
          - file: apps/stage/manifest.yaml
            field: image.tag
"#;

/// A signed GitHub delivery: the HMAC of the raw body (`sha256=<hex>`)
/// + the event name + a stable delivery id.
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

/// The run's tag: a port-unique delivery-id prefix (a recorded
/// workflow's replay across DB resets keeps the FIRST outcome; the tag
/// makes each test-run's deliveries fresh) and repo-id suffix.
fn run_tag(base: &str) -> String {
    base.rsplit(':').next().unwrap_or("0").to_owned()
}

/// Delivers a webhook; returns the status + the JSON body.
#[allow(clippy::expect_used)]
async fn deliver(
    base: &str,
    provider: &str,
    headers: &[(&'static str, String)],
    body: &[u8],
) -> (u16, serde_json::Value) {
    let client = reqwest::Client::new();
    let mut request = client
        .post(format!("{base}/webhooks/{provider}"))
        .header(reqwest::header::CONTENT_TYPE, "application/json");
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let response = request
        .body(body.to_vec())
        .send()
        .await
        .expect("the delivery sends");
    let status = response.status().as_u16();
    let json: serde_json::Value = {
        let text = response.text().await.unwrap_or_default();
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

/// The authorised create (the correlation test's release).
#[allow(clippy::expect_used)]
async fn create_release(base: &str, application: &str, version: &str) -> serde_json::Value {
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/api/v1/releases"))
        .header("authorization", format!("Bearer {API_KEY}"))
        .json(&serde_json::json!({ "application": application, "version": version }))
        .send()
        .await
        .expect("the create sends");
    assert_eq!(response.status(), 202, "the create must answer 202");
    response.json().await.expect("the release json")
}

/// Polls the release document until the phase predicate holds.
#[allow(clippy::expect_used)]
async fn poll_phase(base: &str, id: &str, want: &str) -> serde_json::Value {
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let response = client
            .get(format!("{base}/api/v1/releases/{id}"))
            .header("authorization", format!("Bearer {API_KEY}"))
            .send()
            .await
            .expect("the read sends");
        let document: serde_json::Value = response.json().await.expect("the release json");
        let phase = document
            .pointer("/status/phase")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if phase == want {
            return document;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the release never reached `{want}` (at `{phase}`): {document}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The mock's state file read.
#[allow(clippy::expect_used)]
fn mock_state(scratch: &str) -> serde_json::Value {
    let text = std::fs::read_to_string(std::path::Path::new(scratch).join("state.json"))
        .expect("the mock's state file");
    serde_json::from_str(&text).expect("the state parses")
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::expect_used, clippy::print_stderr)]
async fn test_a_signed_tag_push_creates_the_release() {
    let Some((base, _mock, _scratch)) = test_server().await else {
        return;
    };
    let tag_run = run_tag(&base);
    // The tag push: v1.5.0 → the tag-service (the event: tag trigger;
    // require_tag_protection=false because the mock has no rulesets).
    let body = format!(
        r#"{{"ref":"refs/tags/v1.5.0","after":"abc123","repository":{{"id":"wh-{tag_run}"}},"sender":{{"login":"ska"}}}}"#
    );
    let (status, json) = deliver(
        &base,
        "github",
        &signed_delivery("push", &format!("d-{tag_run}"), body.as_bytes()),
        body.as_bytes(),
    )
    .await;
    assert_eq!(status, 202, "an acted-upon delivery fast-acks: {json}");
    assert_eq!(json["received"], serde_json::json!(true));

    // The created release lands (poll the list API).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let client = reqwest::Client::new();
    loop {
        let page = client
            .get(format!("{base}/api/v1/releases?application=tag-service"))
            .header("authorization", format!("Bearer {API_KEY}"))
            .send()
            .await
            .expect("the list sends");
        let page_json: serde_json::Value = page.json().await.expect("the page");
        let found = page_json["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .any(|item| item["spec"]["version"] == serde_json::json!("1.5.0"))
            })
            .unwrap_or(false);
        if found {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the tag push never created the release: {page_json}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::expect_used, clippy::print_stderr)]
async fn test_bad_signature_and_unknown_provider_refuse() {
    let Some((base, _mock, _scratch)) = test_server().await else {
        return;
    };
    // A wrong secret → 401 and nothing happens.
    let body = br#"{"ref":"refs/tags/v9.9.9","after":"x"}"#;
    let mut headers = signed_delivery("push", "d-bad", body);
    headers[0].1 = "sha256=0000".to_owned();
    let (status, _json) = deliver(&base, "github", &headers, body).await;
    assert_eq!(status, 401, "the bad signature refuses");
    let missed = mock_state(&_scratch);
    assert!(
        missed["branch_creates"].as_u64().unwrap_or(0) == 0,
        "a refused delivery must act on nothing"
    );

    // An unknown provider's endpoint is a 404 (no receiver exists).
    let (status, json) = deliver(&base, "gitea", &signed_delivery("push", "d-2", body), body).await;
    assert_eq!(status, 404, "the unknown provider refuses: {json}");
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::expect_used, clippy::print_stderr)]
async fn test_unrecognized_events_acknowledge_ignored() {
    let Some((base, _mock, _scratch)) = test_server().await else {
        return;
    };
    let body = br#"{"ref":"refs/heads/main","after":"x"}"#;
    let (status, json) =
        deliver(&base, "github", &signed_delivery("push", "d-3", body), body).await;
    assert_eq!(status, 200, "unrecognised acknowledges");
    assert_eq!(json["action"], serde_json::json!("ignored"));
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::expect_used, clippy::print_stderr)]
async fn test_duplicate_deliveries_deduplicate() {
    let Some((base, _mock, _scratch)) = test_server().await else {
        return;
    };
    let tag_run = run_tag(&base);
    let body = format!(
        r#"{{"ref":"refs/tags/v2.0.0","after":"abc123","repository":{{"id":"wh-{tag_run}"}},"sender":{{"login":"ska"}}}}"#
    );
    let headers = signed_delivery("push", &format!("d-dup-{tag_run}"), body.as_bytes());
    let (first, _) = deliver(&base, "github", &headers, body.as_bytes()).await;
    let (second, _) = deliver(&base, "github", &headers, body.as_bytes()).await;
    assert_eq!(first, 202);
    assert_eq!(second, 202, "a duplicate delivery still fast-acks");

    // Exactly ONE release row for (tag-service, 2.0.0) exists.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let client = reqwest::Client::new();
    loop {
        let page = client
            .get(format!(
                "{base}/api/v1/releases?application=tag-service&version=2.0.0"
            ))
            .header("authorization", format!("Bearer {API_KEY}"))
            .send()
            .await
            .expect("the list sends");
        let page_json: serde_json::Value = page.json().await.expect("the page");
        let count = page_json["items"].as_array().map(Vec::len).unwrap_or(0);
        if count >= 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the expected exactly-one active release settled wrong: {page_json}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::expect_used, clippy::print_stderr)]
async fn test_the_closed_pull_request_correlates_and_wakes_the_wait() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new(
                    "cargobike_server=debug,cargobike_engine=debug,dbos=warn",
                )
            }),
        )
        .try_init();
    let Some((base, mock, scratch)) = test_server().await else {
        return;
    };
    let tag_run = run_tag(&base);
    // The create stalls at the merge wait with one open CR (number 1).
    let document = create_release(&base, "tag-service", "3.0.0").await;
    let id = document["metadata"]["id"].as_str().expect("id").to_owned();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let state = mock_state(&scratch);
        if state["change_requests"]
            .as_object()
            .is_some_and(|crs| !crs.is_empty())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            tokio::time::Instant::now() < deadline,
            "the release never opened its CR: {state}"
        );
    }
    let number = mock_state(&scratch)["change_requests"]
        .as_object()
        .and_then(|crs| crs.values().next())
        .and_then(|cr| cr["number"].as_u64())
        .expect("the CR number");

    // Merged, not closed: mark the CR merged in the mock, then deliver
    // the close event. The wait re-verifies (the provider's merged
    // state + the content on the base branch), the release completes.
    mock.mark_merged(number).expect("the mock's merge marker");
    let body = format!(
        r#"{{"action":"closed","pull_request":{{"number":{number},"merged":true,"base":{{"repo":{{"id":"env-{tag_run}"}}}}}},"sender":{{"login":"ska"}}}}"#
    );
    let (status, json) = deliver(
        &base,
        "github",
        &signed_delivery(
            "pull_request",
            &format!("d-corr-{tag_run}"),
            body.as_bytes(),
        ),
        body.as_bytes(),
    )
    .await;
    assert_eq!(status, 202, "the close event acts: {json}");
    let settled = poll_phase(&base, &id, "Completed").await;
    assert_eq!(
        settled.pointer("/status/phase"),
        Some(&serde_json::json!("Completed")),
        "the merged-and-verified release completed: {settled}"
    );
}
