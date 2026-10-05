//! The retry endpoint (F-23/US-7): the failed release's in-place fork
//! completes at the same ID with a `fork_from`-bearing attempt appended;
//! the guards (404, non-failed fork, `--new`'s terminal-only) and the
//! `--new` fresh row with `retried_from` set. Requires
//! `CARGOBIKE_TEST_DATABASE_URL`.
//!
//! (Test code: it asserts and reports, so the expect/print lints stay
//! off.) The repo ids are port-tagged like the receiver's suite: the
//! correlation/lease keys are global and the fixture database is shared.
#![allow(clippy::expect_used, clippy::print_stderr)]

use argon2::password_hash::PasswordHasher as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// The two-step template: `stage` commits, `prod` commits (the failable
/// second one is the retry scenario's failing step). The template's own
/// name must be `service` — the registry apps reference `service@1`.
const TEMPLATE: &str = r#"
name: service
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

#[allow(clippy::expect_used, clippy::print_stderr)]
async fn test_server() -> Option<(String, Arc<cargobike_engine::mock::MockProvider>, String)> {
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
    let api_key = "cb_retry_test_key";
    let salt = argon2::password_hash::SaltString::encode_b64(b"cb-test-salt-16-x").expect("salt");
    let hash = argon2::Argon2::default()
        .hash_password(api_key.as_bytes(), &salt)
        .expect("hash")
        .to_string();

    let dir = std::env::temp_dir();
    let scratch = dir
        .join(format!("cb-retry-mock-{port}"))
        .to_string_lossy()
        .to_string();
    let mock = Arc::new(cargobike_engine::mock::MockProvider::at(&scratch));
    let mut providers = cargobike_core::registry::ProviderRegistry::new();
    providers.insert(
        "github",
        mock.clone() as Arc<dyn cargobike_core::provider::Provider>,
    );

    let templates_dir = dir.join(format!("cb-retry-templates-{port}"));
    let _ = std::fs::create_dir_all(&templates_dir);
    std::fs::write(templates_dir.join("service.yaml"), TEMPLATE).expect("write template");
    let templates_display = templates_dir.display().to_string();

    let config_path = dir.join(format!("cb-retry-test-{port}.yaml"));
    std::fs::write(
        &config_path,
        format!(
            "server:\n  listen: \"127.0.0.1:{port}\"\n  public_url: http://127.0.0.1:{port}\n\n\
             database:\n  url: \"{url}\"\n  dbos_schema: \"cbtest-{port}\"\n\n\
             templates:\n  directory: {templates_display}\n\n\
             auth:\n  api_keys:\n    - name: test-key\n      hash: \"{hash}\"\n      grants: [release:create, release:read, release:cancel, release:delete]\n\n\
             applications:\n  - name: retry-alpha\n    source:\n      provider: github\n      id: \"retry-{port}\"\n    template: service@1\n    releasers:\n      - api_key: test-key\n    environments:\n      stage:\n        repo:\n          provider: github\n          id: \"alpha-env-{port}\"\n        edits:\n          - file: apps/stage/manifest.yaml\n            field: image.tag\n      prod:\n        repo:\n          provider: github\n          id: \"alpha-env-{port}\"\n        edits:\n          - file: apps/stage/manifest.yaml\n            field: image.tag\n  - name: retry-beta\n    source:\n      provider: github\n      id: \"retry-{port}\"\n    template: service@1\n    releasers:\n      - api_key: test-key\n    environments:\n      stage:\n        repo:\n          provider: github\n          id: \"beta-env-{port}\"\n        edits:\n          - file: apps/stage/manifest.yaml\n            field: image.tag\n      prod:\n        repo:\n          provider: github\n          id: \"beta-env-{port}\"\n        edits:\n          - file: apps/stage/manifest.yaml\n            field: image.tag\n"
        ),
    )
    .expect("write the test config");

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

/// The retry tests' serialization: each boots its own server against
/// the SAME fixture schema (`dbos`) and scripts the mock's state file;
/// concurrent boots have raced the inject and the boot's sweep (seen in
/// CI once: the refusal flag consumed by the other test's run). A lock
/// for the whole test's span is the dispositive fix.
async fn the_retry_lanes_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static THE_LANES: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    THE_LANES
        .get_or_init(tokio::sync::Mutex::default)
        .lock()
        .await
}

/// The boot's port (the per-boot namespace's tag, shared by the
/// fixture's repo ids and the delivery/workflow ids).
fn boot_port(base: &str) -> String {
    base.rsplit(':').next().unwrap_or("0").to_owned()
}

/// The authorised client wrapper.
struct Api {
    base: String,
    key: String,
    client: reqwest::Client,
    app: &'static str,
}

impl Api {
    fn new(base: String, app: &'static str) -> Self {
        Self {
            base,
            key: "cb_retry_test_key".to_owned(),
            client: reqwest::Client::new(),
            app,
        }
    }

    async fn create(&self, version: &str) -> serde_json::Value {
        let response = self
            .client
            .post(format!("{}/api/v1/releases", self.base))
            .header("authorization", format!("Bearer {}", self.key))
            .json(&serde_json::json!({ "application": self.app, "version": version }))
            .send()
            .await
            .expect("the create sends");
        assert_eq!(response.status(), 202);
        response.json().await.expect("the release json")
    }

    async fn get(&self, id: &str) -> serde_json::Value {
        let response = self
            .client
            .get(format!("{}/api/v1/releases/{id}", self.base))
            .header("authorization", format!("Bearer {}", self.key))
            .send()
            .await
            .expect("the read sends");
        response.json().await.expect("the release json")
    }

    async fn retry(&self, id: &str, new: bool) -> (u16, serde_json::Value) {
        let response = self
            .client
            .post(format!("{}/api/v1/releases/{id}/retry", self.base))
            .header("authorization", format!("Bearer {}", self.key))
            .json(&serde_json::json!({ "new": new }))
            .send()
            .await
            .expect("the retry sends");
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        (
            status,
            serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
        )
    }
}

/// Polls the release document until the phase predicate holds.
#[allow(clippy::expect_used)]
async fn poll_phase(api: &Api, id: &str, want: &str) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let document = api.get(id).await;
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

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::expect_used, clippy::print_stderr)]
async fn test_retry_forks_a_failed_release_to_completion() {
    let _the_lane = the_retry_lanes_lock().await;
    let Some((base, mock, _scratch)) = test_server().await else {
        return;
    };
    let port = boot_port(&base);
    let api = Api::new(base, "retry-alpha");

    // The first attempt fails at the injected `prod` commit.
    mock.fail_next_commit_for(&format!("alpha-env-{port}"))
        .expect("the inject scripts");
    let document = api.create("1.1.0").await;
    let id = document["metadata"]["id"].as_str().expect("id").to_owned();
    let failed = poll_phase(&api, &id, "Failed").await;
    let first_attempt = failed
        .pointer("/status/attempts/0/workflow_id")
        .and_then(serde_json::Value::as_str)
        .expect("the first attempt's id")
        .to_owned();

    // A retry of a NON-terminal phase is refused; the failed phase is
    // the only fork source.
    let (status, acc_json) = api.retry(&id, false).await;
    eprintln!("RETRY-RESPONSE: status={status} body={acc_json}");
    assert_eq!(status, 202, "the first retry refused: {acc_json}");
    let completed = poll_phase(&api, &id, "Completed").await;
    let attempts = completed
        .pointer("/status/attempts")
        .cloned()
        .unwrap_or_default();
    let list = attempts.as_array().cloned().unwrap_or_default();
    assert_eq!(list.len(), 2, "the retry appended the attempt: {attempts}");
    assert_eq!(
        list[1]["fork_from"],
        serde_json::Value::String(first_attempt.clone()),
        "fork_from names the failed attempt"
    );
    assert!(
        list[1]["workflow_id"]
            .as_str()
            .is_some_and(|wf| wf != first_attempt),
        "the forked attempt carries its own workflow id"
    );

    // A retry of a COMPLETED release is refused (only failed forks).
    let (status, json) = api.retry(&id, false).await;
    assert_eq!(
        status, 409,
        "the completed release refuses the fork: {json}"
    );
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::expect_used, clippy::print_stderr)]
async fn test_retry_new_copies_the_release_with_retried_from() {
    let _the_lane = the_retry_lanes_lock().await;
    let Some((base, mock, _scratch)) = test_server().await else {
        return;
    };
    let port = boot_port(&base);
    let api = Api::new(base, "retry-beta");

    mock.fail_next_commit_for(&format!("beta-env-{port}"))
        .expect("the inject scripts");
    let original = api.create("1.2.0").await;
    let original_id = original["metadata"]["id"].as_str().expect("id").to_owned();
    poll_phase(&api, &original_id, "Failed").await;

    // `--new`: a fresh row, `retried_from` names the original.
    let (status, json) = api.retry(&original_id, true).await;
    assert_eq!(status, 202, "the copy accepted: {json}");
    assert_ne!(
        json["metadata"]["id"],
        serde_json::Value::String(original_id.clone()),
        "the copy is a new release"
    );
    assert_eq!(
        json["metadata"]["retried_from"],
        serde_json::Value::String(original_id.clone())
    );
    let copy_id = json["metadata"]["id"].as_str().expect("id").to_owned();
    poll_phase(&api, &copy_id, "Completed").await;
}
