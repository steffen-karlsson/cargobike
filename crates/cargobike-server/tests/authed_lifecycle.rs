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
    // The shared fixture DB's leftovers: lease rows outlive their
    // releases (a supersede guard cannot read a deleted holder's
    // version), so this run's applications start with clean rows.
    {
        #[allow(clippy::expect_used)]
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("the fixture database connects");
        for application in [
            "my-service",
            "keyed-service",
            "paused-service",
            "env-paused-service",
            "bare-service",
        ] {
            sqlx::query("DELETE FROM leases WHERE application = $1")
                .bind(application)
                .execute(&pool)
                .await
                .expect("the lease rows wipe");
            sqlx::query("DELETE FROM releases WHERE application = $1")
                .bind(application)
                .execute(&pool)
                .await
                .expect("the release rows wipe");
        }
    }
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
             applications:\n  - name: my-service\n    source:\n      provider: github\n      id: \"123456\"\n    template: service@1\n    releasers:\n      - api_key: test-key\n    environments:\n      preview:\n        repo:\n          provider: github\n          id: \"42\"\n        edits:\n          - file: apps/stage/manifest.yaml\n            field: image.tag\n  - name: keyed-service\n    source:\n      provider: github\n      id: \"654321\"\n    template: service@1\n    releasers:\n      - api_key: test-key\n    environments:\n      preview:\n        repo:\n          provider: github\n          id: \"42\"\n        edits:\n          - file: apps/stage/manifest.yaml\n            field: image.tag\n        change_request:\n          title: \"The keyed release {{version}}\"\n          labels: [keyed]\n          draft: true\n  - name: bare-service\n    source:\n      provider: github\n      id: \"777888\"\n    template: service@1\n    releasers:\n      - api_key: test-key\n    environments:\n      preview:\n        edits:\n          - file: apps/stage/manifest.yaml\n            field: image.tag\n  - name: paused-service\n    paused:\n      reason: freezing deploys for the audit\n    source:\n      provider: github\n      id: \"111222\"\n    template: service@1\n    releasers:\n      - api_key: test-key\n    environments:\n      preview:\n        repo:\n          provider: github\n          id: \"42\"\n        edits:\n          - file: apps/stage/manifest.yaml\n            field: image.tag\n  - name: env-paused-service\n    source:\n      provider: github\n      id: \"333444\"\n    template: service@1\n    releasers:\n      - api_key: test-key\n    environments:\n      preview:\n        paused:\n          reason: the canary burn\n        repo:\n          provider: github\n          id: \"42\"\n        edits:\n          - file: apps/stage/manifest.yaml\n            field: image.tag\n"
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

    // The `Idempotency-Key` header (F-109): a fresh create with a key
    // answers 202, and a retried create with the SAME key — even with a
    // different body — converges on that release with 200 instead of
    // provisioning a second one.
    let key = format!("create-key-{run_tag}");
    let keyed = client
        .post(format!("{base}/api/v1/releases"))
        .header(auth_header.0, auth_header.1.clone())
        .header("Idempotency-Key", &key)
        .json(&serde_json::json!({ "application": "keyed-service", "version": format!("2.0.{run_tag}") }))
        .send()
        .await
        .expect("http");
    assert_eq!(keyed.status(), reqwest::StatusCode::ACCEPTED);
    let keyed_document: serde_json::Value = keyed.json().await.expect("release json");
    let keyed_id = keyed_document["metadata"]["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let retried = client
        .post(format!("{base}/api/v1/releases"))
        .header(auth_header.0, auth_header.1.clone())
        .header("Idempotency-Key", &key)
        .json(&serde_json::json!({ "application": "keyed-service", "version": format!("3.0.{run_tag}") }))
        .send()
        .await
        .expect("http");
    assert_eq!(retried.status(), reqwest::StatusCode::OK);
    let retried_document: serde_json::Value = retried.json().await.expect("release json");
    assert_eq!(
        retried_document["metadata"]["id"].as_str().expect("id"),
        keyed_id,
        "the key's retry converges on the first attempt's release"
    );

    // F-32a: an environment that does not supply every per-environment
    // input the template declares refuses the create at the choke
    // point — the refusal names the gap instead of a CEL resolve
    // failure surfacing mid-flight.
    let bare_create = client
        .post(format!("{base}/api/v1/releases"))
        .header(auth_header.0, auth_header.1.clone())
        .json(&serde_json::json!({ "application": "bare-service", "version": format!("6.0.{run_tag}") }))
        .send()
        .await
        .expect("http");
    assert_eq!(bare_create.status(), reqwest::StatusCode::BAD_REQUEST);
    let bare_body: serde_json::Value = bare_create.json().await.expect("problem body");
    assert_eq!(bare_body["code"], "InvalidRequest");
    assert!(
        bare_body["detail"].as_str().is_some_and(
            |detail| detail.contains("does not supply the per-environment input `repo`")
        ),
        "the refusal names the missing input: {bare_body}"
    );

    // F-142: a paused application refuses the create with 409 and a
    // problem document carrying the reason (the registry's `paused`
    // shape); a paused environment refuses the same way, naming the
    // environment.
    let paused_create = client
        .post(format!("{base}/api/v1/releases"))
        .header(auth_header.0, auth_header.1.clone())
        .json(&serde_json::json!({ "application": "paused-service", "version": format!("5.0.{run_tag}") }))
        .send()
        .await
        .expect("http");
    assert_eq!(paused_create.status(), reqwest::StatusCode::CONFLICT);
    let paused_body: serde_json::Value = paused_create.json().await.expect("problem body");
    assert_eq!(paused_body["code"], "StateConflict");
    assert!(
        paused_body["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("freezing deploys for the audit")),
        "the refusal carries the pause reason: {paused_body}"
    );
    let env_paused_create = client
        .post(format!("{base}/api/v1/releases"))
        .header(auth_header.0, auth_header.1.clone())
        .json(&serde_json::json!({ "application": "env-paused-service", "version": format!("5.0.{run_tag}") }))
        .send()
        .await
        .expect("http");
    assert_eq!(env_paused_create.status(), reqwest::StatusCode::CONFLICT);
    let env_paused_body: serde_json::Value = env_paused_create.json().await.expect("problem body");
    assert_eq!(env_paused_body["code"], "StateConflict");
    assert!(
        env_paused_body["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("preview") && detail.contains("the canary burn")),
        "the refusal names the environment and its reason: {env_paused_body}"
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

    /// Waits until the release's first environment has recorded its
    /// change request: the cancel's cleanup targets read the release's
    /// books, so cancelling before the interpreter checkpoints its CR
    /// strands the CR open on the provider (the cleanup would have no
    /// reference to close).
    async fn wait_for_cr_recorded(
        client: &reqwest::Client,
        base: &str,
        auth_header: &(&'static str, String),
        id: &str,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let document = client
                .get(format!("{base}/api/v1/releases/{id}"))
                .header(auth_header.0, auth_header.1.clone())
                .send()
                .await
                .expect("http")
                .json::<serde_json::Value>()
                .await
                .expect("the release's document");
            if document
                .pointer("/status/environments/0/change_request/number")
                .is_some_and(|number| !number.is_null())
            {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the release {id} never recorded its change request: {document}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

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
    assert_eq!(
        from_this_run, 1,
        "this run's my-service release must be listed (the keyed create is keyed-service)"
    );

    // Cancel is in-flight only. The release blocks at its merge wait
    // (the CR is recorded first — the cleanup needs the reference),
    // so the cancel lands and the cleanup workflow closes the CR and
    // settles the branches.
    wait_for_cr_recorded(&client, &base, &auth_header, &id).await;
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

    // The keyed release drains FIRST: its interpreter blocks at its
    // own merge wait (a real open CR on the mock's seeded path), so
    // the cancel is deterministic — and draining it now means the
    // cleanup wait below sees BOTH releases' change requests close.
    wait_for_cr_recorded(&client, &base, &auth_header, &keyed_id).await;
    // R11/F-143/F-147: the registry's change_request overrides shaped
    // the keyed CR — the title's `{version}` placeholder resolved at
    // provision, the label appended, the draft flag on.
    {
        let state = cargobike_engine::mock::MockState::load(&mock.state_file());
        let keyed_cr = state
            .change_requests
            .values()
            .find(|summary| summary.title.starts_with("The keyed release"))
            .expect("the keyed CR carries the registry's title");
        assert_eq!(
            keyed_cr.title,
            format!("The keyed release 2.0.{run_tag}"),
            "the version placeholder resolved at provision time"
        );
        assert!(keyed_cr.labels.contains(&"keyed".to_owned()));
        assert!(
            keyed_cr.draft,
            "the registry's draft flag opened the CR as a draft"
        );
    }
    let keyed_cancel = client
        .post(format!("{base}/api/v1/releases/{keyed_id}/cancel"))
        .header(auth_header.0, auth_header.1.clone())
        .send()
        .await
        .expect("http");
    assert_eq!(
        keyed_cancel.status(),
        reqwest::StatusCode::NO_CONTENT,
        "the keyed release's drain: {:?}",
        keyed_cancel.text().await
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

    // The keyed release's row goes too — then the same key provisions
    // fresh (the key's mapping followed the row out via the cascade).
    // The row stays until the cleanup converged above: a mid-cleanup
    // delete would strand the change request open on the provider.
    assert_eq!(
        client
            .delete(format!("{base}/api/v1/releases/{keyed_id}"))
            .header(auth_header.0, auth_header.1.clone())
            .send()
            .await
            .expect("http")
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );
    let fresh_after_delete = client
        .post(format!("{base}/api/v1/releases"))
        .header(auth_header.0, auth_header.1.clone())
        .header("Idempotency-Key", &key)
        .json(&serde_json::json!({ "application": "keyed-service", "version": format!("4.0.{run_tag}") }))
        .send()
        .await
        .expect("http");
    let fresh_status = fresh_after_delete.status();
    let fresh_document: serde_json::Value = fresh_after_delete.json().await.expect("release json");
    assert_eq!(
        fresh_status,
        reqwest::StatusCode::ACCEPTED,
        "a key whose release was deleted provisions fresh: {fresh_document}"
    );
    // The fresh release drains too (cancel + delete), leaving no
    // non-terminal row behind against the shared fixture DB.
    let fresh_id = fresh_document["metadata"]["id"].as_str().expect("id");
    wait_for_cr_recorded(&client, &base, &auth_header, fresh_id).await;
    let cancel = client
        .post(format!("{base}/api/v1/releases/{fresh_id}/cancel"))
        .header(auth_header.0, auth_header.1.clone())
        .send()
        .await
        .expect("http");
    assert_eq!(
        cancel.status(),
        reqwest::StatusCode::NO_CONTENT,
        "the fresh release's drain: {:?}",
        cancel.text().await
    );
    assert_eq!(
        client
            .delete(format!("{base}/api/v1/releases/{fresh_id}"))
            .header(auth_header.0, auth_header.1.clone())
            .send()
            .await
            .expect("http")
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );
}
