//! F-10 verified-path labels end-to-end: the registry's declared
//! `source.path` is checked against the provider at boot, a mismatch
//! marks the application unavailable (visible in `GET
//! /api/v1/applications`, refused at the create), and the background
//! sweep heals it — the renamed-back repo's next create passes without
//! a restart. Requires `CARGOBIKE_TEST_DATABASE_URL` (or a throwaway
//! container via the shared test-support helper).

use argon2::password_hash::PasswordHasher as _;
use std::net::SocketAddr;
use std::sync::Arc;

/// The release's template: edit + change request + the merge wait (the
/// release STALLS on the provider's open CR — a reliable cancel target
/// with no phase-five dependencies).
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

#[allow(clippy::print_stderr, clippy::expect_used)]
async fn test_server() -> Option<(String, Arc<cargobike_engine::mock::MockProvider>, String)> {
    let url = cargobike_testsupport::database_url().await?;
    // The shared fixture DB's leftovers: this run's application starts
    // with clean rows (the lease would outlive its release).
    {
        #[allow(clippy::expect_used)]
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("the fixture database connects");
        sqlx::query("DELETE FROM leases WHERE application = 'path-service'")
            .execute(&pool)
            .await
            .expect("the lease rows wipe");
        sqlx::query("DELETE FROM releases WHERE application = 'path-service'")
            .execute(&pool)
            .await
            .expect("the release rows wipe");
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let presented = "cbk_test_plain_secret";

    let salt = argon2::password_hash::SaltString::encode_b64(b"cb-test-salt-16-x").expect("salt");
    let hash = argon2::Argon2::default()
        .hash_password(presented.as_bytes(), &salt)
        .expect("hash the presented key")
        .to_string();

    let dir = std::env::temp_dir();
    let scratch = dir
        .join(format!("cb-avail-mock-{port}"))
        .to_string_lossy()
        .to_string();
    let mock = Arc::new(cargobike_engine::mock::MockProvider::at(&scratch));
    let mut providers = cargobike_core::registry::ProviderRegistry::new();
    providers.insert(
        "github",
        mock.clone() as Arc<dyn cargobike_core::provider::Provider>,
    );

    let templates_dir = dir.join(format!("cb-avail-templates-{port}"));
    let _ = std::fs::create_dir_all(&templates_dir);
    std::fs::write(templates_dir.join("service.yaml"), TEMPLATE).expect("write template");
    let templates_display = templates_dir.display().to_string();

    // The declared path mismatches the mock's truth (`mock/42`): the
    // app boots unavailable. The sweep runs every second so the test
    // watches the heal converge.
    let config_path = dir.join(format!("cb-avail-test-{port}.yaml"));
    std::fs::write(
        &config_path,
        format!(
            "server:\n  listen: \"127.0.0.1:{port}\"\n  public_url: http://127.0.0.1:{port}\n\n\
             database:\n  url: \"{url}\"\n  dbos_schema: \"cbavail-{port}\"\n\n\
             templates:\n  directory: {templates_display}\n\n\
             reconciler:\n  verified_path_interval: 1s\n\n\
             auth:\n  api_keys:\n    - name: test-key\n      hash: \"{hash}\"\n      grants: [release:create, release:read, release:cancel, release:delete]\n\n\
             applications:\n  - name: path-service\n    source:\n      provider: github\n      id: \"42\"\n      path: wrong-org/wrong-repo\n    versioning:\n      scheme: opaque\n      require_tag: false\n    template: service@1\n    releasers:\n      - api_key: test-key\n    environments:\n      preview:\n        repo:\n          provider: github\n          id: \"42\"\n        edits:\n          - file: apps/stage/manifest.yaml\n            field: image.tag\n"
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

/// Waits until the applications listing reports the app available
/// (the background sweep's heal; every second, watched for ~10s).
#[allow(clippy::expect_used)]
async fn wait_available(
    client: &reqwest::Client,
    base: &str,
    auth_header: &(&'static str, String),
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let document = client
            .get(format!("{base}/api/v1/applications"))
            .header(auth_header.0, auth_header.1.clone())
            .send()
            .await
            .expect("http")
            .json::<serde_json::Value>()
            .await
            .expect("the applications body");
        let entry = &document["applications"][0];
        if entry["available"].as_bool() == Some(true) {
            return document;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the background sweep never healed the mismatch: {document}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

#[allow(clippy::print_stderr, clippy::expect_used)]
#[tokio::test(flavor = "current_thread")]
async fn test_mismatched_path_label_refuses_then_heals() {
    let Some((base, mock, _scratch)) = test_server().await else {
        return;
    };

    let client = reqwest::Client::new();
    let auth_header = ("Authorization", "Bearer cbk_test_plain_secret".to_owned());

    // The boot check already ran: the mismatch is visible in the
    // applications listing, with the reason naming both paths.
    let listed = client
        .get(format!("{base}/api/v1/applications"))
        .header(auth_header.0, auth_header.1.clone())
        .send()
        .await
        .expect("http");
    assert_eq!(listed.status(), reqwest::StatusCode::OK);
    let listed: serde_json::Value = listed.json().await.expect("the applications body");
    let entry = &listed["applications"][0];
    assert_eq!(entry["name"].as_str(), Some("path-service"));
    assert_eq!(
        entry["repository"]["path"].as_str(),
        Some("wrong-org/wrong-repo"),
        "the listing carries the declared label: {entry}"
    );
    assert_eq!(entry["available"].as_bool(), Some(false));
    let reason = entry["unavailable_reason"].as_str().expect("the reason");
    assert!(
        reason.contains("wrong-org/wrong-repo") && reason.contains("mock/42"),
        "the reason names the declared label and the provider's truth: {reason}"
    );

    // The create refuses while the mismatch stands — the registry's
    // repo identity cannot be trusted (409, the reason carried).
    let refused = client
        .post(format!("{base}/api/v1/releases"))
        .header(auth_header.0, auth_header.1.clone())
        .json(&serde_json::json!({ "application": "path-service", "version": "9.9.9" }))
        .send()
        .await
        .expect("http");
    assert_eq!(refused.status(), reqwest::StatusCode::CONFLICT);
    let refused_body: serde_json::Value = refused.json().await.expect("problem body");
    assert_eq!(refused_body["code"], "StateConflict");
    assert_eq!(
        refused_body["type"].as_str(),
        Some("https://cargobike.dev/errors/application-unavailable")
    );
    assert!(
        refused_body["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("path-service") && detail.contains("mock/42")),
        "the refusal names the application and the provider's truth: {refused_body}"
    );

    // The heal: the provider's truth changes to agree with the label
    // (the repo was renamed back), and the background sweep flips the
    // view — no restart, the server never failed.
    mock.set_repo_path("42", "wrong-org/wrong-repo");
    let healed = wait_available(&client, &base, &auth_header).await;
    assert_eq!(
        healed["applications"][0]["available"].as_bool(),
        Some(true),
        "the sweep healed the application: {healed}"
    );

    // And the create now passes — the drain is the fixture's
    // discipline (cancel only after the CR is recorded, then delete
    // the row; the lease row was wiped at boot).
    let run_tag = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_micros();
    let created = client
        .post(format!("{base}/api/v1/releases"))
        .header(auth_header.0, auth_header.1.clone())
        .json(&serde_json::json!({ "application": "path-service", "version": format!("1.0.{run_tag}") }))
        .send()
        .await
        .expect("http");
    assert_eq!(created.status(), reqwest::StatusCode::ACCEPTED);
    let document: serde_json::Value = created.json().await.expect("release json");
    let id = document["metadata"]["id"].as_str().expect("id").to_owned();

    // Cancel-before-CR-recorded strands the CR open (the cleanup
    // reads the release's books), so the drain waits for the CR first.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let status = client
            .get(format!("{base}/api/v1/releases/{id}"))
            .header(auth_header.0, auth_header.1.clone())
            .send()
            .await
            .expect("http")
            .json::<serde_json::Value>()
            .await
            .expect("the release's document");
        if status
            .pointer("/status/environments/0/change_request/number")
            .is_some_and(|number| !number.is_null())
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the release never recorded its change request: {status}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let cancel = client
        .post(format!("{base}/api/v1/releases/{id}/cancel"))
        .header(auth_header.0, auth_header.1.clone())
        .send()
        .await
        .expect("http");
    assert!(
        cancel.status().is_success(),
        "the cancel: {:?}",
        cancel.status()
    );
    let deleted = client
        .delete(format!("{base}/api/v1/releases/{id}"))
        .header(auth_header.0, auth_header.1.clone())
        .send()
        .await
        .expect("http");
    assert!(
        deleted.status().is_success(),
        "the delete: {:?}",
        deleted.status()
    );
}
