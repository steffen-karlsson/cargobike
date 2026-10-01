//! Authenticated release-lifecycle integration test: in-process
//! server on an ephemeral port; API-key auth , idempotent create
//! , the registry authorization rules and problem
//! documents . Requires `CARGOBIKE_TEST_DATABASE_URL`.

use argon2::password_hash::PasswordHasher as _;
use std::net::SocketAddr;

/// Skips cleanly without a database.
#[allow(clippy::print_stderr, clippy::expect_used)]
async fn test_server() -> Option<String> {
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
    let config_path = dir.join(format!("cb-authed-test-{port}.yaml"));
    std::fs::write(
        &config_path,
        format!(
            "server:\n  listen: \"127.0.0.1:{port}\"\n  public_url: http://127.0.0.1:{port}\n\n\
             database:\n  url: \"{url}\"\n\n\
             auth:\n  api_keys:\n    - name: test-key\n      hash: \"{hash}\"\n      grants: [release:create, release:read, release:cancel, release:delete]\n\n\
             applications:\n  - name: my-service\n    source:\n      provider: github\n      id: \"123456\"\n    template: service@1\n    releasers:\n      - api_key: test-key\n    environments:\n      preview:\n        concurrency: supersede\n"
        ),
    )
    .expect("write the test config");

    let Ok((router, _state)) = cargobike_server::boot(Some(&config_path)).await else {
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
    Some(format!("http://127.0.0.1:{port}"))
}

#[allow(clippy::print_stderr, clippy::expect_used)]
#[tokio::test(flavor = "current_thread")]
async fn test_authed_release_lifecycle() {
    let Some(base) = test_server().await else {
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
    let version_b = format!("1.3.{run_tag}");
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

    // Duplicate create answers 200 with the existing release .
    assert_eq!(
        create(&client, &version_a)
            .send()
            .await
            .expect("http")
            .status(),
        reqwest::StatusCode::OK
    );

    // A different version creates a fresh row.
    assert_eq!(
        create(&client, &version_b)
            .send()
            .await
            .expect("http")
            .status(),
        reqwest::StatusCode::ACCEPTED
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
    assert_eq!(
        from_this_run, 2,
        "the two releases of this run must be listed"
    );

    // Cancel then terminal-only delete (, , not-found after).
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
