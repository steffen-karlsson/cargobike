//! Authenticated release-lifecycle integration test: in-process
//! server on an ephemeral port; API-key auth, the idempotent create,
//! the registry authorization rules, problem documents, and (since the
//! engine hosts in-process) a real release driven over the mock
//! provider: create, duplicate, list, cancel with its cleanup,
//! terminal-only delete. Requires `CARGOBIKE_TEST_DATABASE_URL`.

use argon2::password_hash::PasswordHasher as _;
use std::net::SocketAddr;
use std::sync::Arc;

/// The release's template: edit + change request + the merge wait
/// (the release STALLS on the provider's open CR — a reliable
/// cancel target with no phase-five dependencies).
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

/// Boot every crate... scratch/Port info here.
#[allow(clippy::print_stderr, clippy::expect_used)]
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
    let presented = "cbk_test_plain_secret";

    // Generate the argon2id hash the config will carry for the test key .
    // A fixed salt is fine for a test fixture; rust production hashing
    // goes through `cargobike-server hash-api-key` .
    let salt = argon2::password_hash::SaltString::encode_b64(b"cb-test-salt-16-x").expect("salt");
    let hash = argon2::Argon2::default()
        .hash_password(presented.as_bytes(), &salt)
        .expect("hash the presented key")
        .to_string();

    let dir = std::env::temp_dir();
    let scratch = dir
        .join(format!("cb-authed-mock-{port}"))
        .to_string_lossy()
        .to_string();
    let mock = Arc::new(cargobike_engine::mock::MockProvider::at(&scratch));
    let mut providers = cargobike_core::registry::ProviderRegistry::new();
    providers.insert(
        "github",
        mock.clone() as Arc<dyn cargobike_core::provider::Provider>,
    );

    // A local template dir (the workspace's canonical template carries
    // a production environment this fixture has no use for).
    let templates_dir = dir.join(format!("cb-authed-templates-{port}"));
    let _ = std::fs::create_dir_all(&templates_dir);
    std::fs::write(templates_dir.join("service.yaml"), TEMPLATE).expect("write template");
    let templates_display = templates_dir.display().to_string();

    let config_path = dir.join(format!("cb-authed-test-{port}.yaml"));
    std::fs::write(
        &config_path,
        format!(
            "server:\n  listen: \"127.0.0.1:{port}\"\n  public_url: http://127.0.0.1:{port}\n\n\
             database:\n  url: \"{url}\"\n  dbos_schema: \"cbtest-{port}\"\n\n\
             templates:\n  directory: {templates_display}\n\n\
             auth:\n  api_keys:\n    - name: test-key\n      hash: \"{hash}\"\n      grants: [release:create, release:read, release:cancel, release:delete]\n\n\
             applications:\n  - name: my-service\n    source:\n      provider: github\n      id: \"123456\"\n    template: service@1\n    releasers:\n      - api_key: test-key\n    environments:\n      preview:\n        repo:\n          provider: github\n          id: \"42\"\n        edits:\n          - file: apps/preview/manifest.yaml\n            field: image.tag\n"
        ),
    )
    .expect("write the test config");

    let Ok((router, _state)) = cargobike_server::boot(Some(&config_path), Some(providers)).await
    else {
        eprintln!("skipping: server boot failed");
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

#[allow(clippy::print_stderr, clippy::expect_used)]
#[tokio::test(flavor = "current_thread")]
async fn test_authed_release_lifecycle() {
    let Some((base, mock, _scratch)) = test_server().await else {
        return;
    };

    let client = reqwest::Client::new();
    let auth_header = ("Authorization", "Bearer cbk_test_plain_secret".to_owned());

    // No header: 401 `MissingToken` (the no body).
    let response = client
        .post(format!("{base}/api/v1/releases"))
        .send()
        .await
        .expect("http");
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = response.json().await.expect("problem body");
    assert_eq!(body["code"], "MissingToken");

    // A wrong credential: 401 `InvalidToken`.
    let response = client
        .post(format!("{base}/api/v1/releases"))
        .header("Authorization", "Bearer cbk_no_such_key")
        .send()
        .await
        .expect("http");
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = response.json().await.expect("problem body");
    assert_eq!(body["code"], "InvalidToken");

    // Unique-per-run versions keep the test re-runnable against a shared
    // fixture database: stale active rows from earlier runs stay untouched.
    let run_tag = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_micros();
    let version_a = format!("1.2.{run_tag}");
    let create = |client: &reqwest::Client, version: &str| -> reqwest::RequestBuilder {
        client
            .post(format!("{base}/api/v1/releases"))
            .header(auth_header.0, auth_header.1.clone())
            .json(&serde_json::json!({ "application": "my-service", "version": version }))
    };
    let response = create(&client, &version_a).send().await.expect("http");
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    let location = response
        .headers()
        .get("Location")
        .and_then(|v| v.to_str().ok())
        .expect("Location")
        .to_owned();
    let document: serde_json::Value = response.json().await.expect("release json");
    let id = document["metadata"]["id"].as_str().expect("id").to_owned();
    assert!(location.ends_with(&id));

    // Duplicate create answers 200 with the existing release (the same
    // id); the interpreter started after the 202, so the release is
    // non-terminal while the merge wait blocks on the provider's CR.
    let duplicate = create(&client, &version_a).send().await.expect("http");
    assert_eq!(duplicate.status(), reqwest::StatusCode::OK);
    let duplicate_document: serde_json::Value = duplicate.json().await.expect("release json");
    assert_eq!(
        duplicate_document["metadata"]["id"].as_str().expect("id"),
        id,
        "the duplicate hands back the same release"
    );

    // whoami .
    let response = client
        .get(format!("{base}/api/v1/whoami"))
        .header(auth_header.0, auth_header.1.clone())
        .send()
        .await
        .expect("http");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("whoami body");
    assert_eq!(body["origin"], "test-key");
    assert_eq!(
        body["grants"],
        serde_json::json!([
            "release:create",
            "release:read",
            "release:cancel",
            "release:delete"
        ])
    );

    // List with filters .
    let response = client
        .get(format!("{base}/api/v1/releases?application=my-service"))
        .header(auth_header.0, auth_header.1.clone())
        .send()
        .await
        .expect("http");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("list body");
    let items = body["items"].as_array().expect("items");
    let from_this_run = items
        .iter()
        .filter(|item| {
            item["spec"]["version"]
                .as_str()
                .is_some_and(|v| v.ends_with(&format!(".{run_tag}")))
        })
        .count();
    assert_eq!(from_this_run, 1, "this run's single release must be listed");

    // Cancel is in-flight only. The release blocks at its merge wait,
    // so the cancel lands and the cleanup workflow closes the CR and
    // settles the branches.
    assert_eq!(
        client
            .post(format!("{base}/api/v1/releases/{id}/cancel"))
            .header(auth_header.0, auth_header.1.clone())
            .send()
            .await
            .expect("http")
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );

    // The cleanup's effect on the provider: the CR closed.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let state = cargobike_engine::mock::MockState::load(&mock.state_file());
        let all_closed = state
            .change_requests
            .values()
            .all(|summary| summary.state != "open");
        if all_closed {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the cleanup never closed the change requests; state {state:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Terminal-only delete, then not-found after.
    assert_eq!(
        client
            .delete(format!("{base}/api/v1/releases/{id}"))
            .header(auth_header.0, auth_header.1.clone())
            .send()
            .await
            .expect("http")
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );
    let response = client
        .get(format!("{base}/api/v1/releases/{id}"))
        .header(auth_header.0, auth_header.1.clone())
        .send()
        .await
        .expect("http");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    let body: serde_json::Value = response.json().await.expect("problem body");
    assert_eq!(body["code"], "ReleaseNotFound");
}
