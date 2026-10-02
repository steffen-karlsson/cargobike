//! SIGHUP hot-reload test : the server runs as a real process; a
//! config edit + `kill -HUP` swaps the trust entries; requests through
//! the old key start failing and the new key starts succeeding, without
//! a restart. Requires `CARGOBIKE_TEST_DATABASE_URL`.

#[allow(clippy::print_stderr, clippy::expect_used)]
#[tokio::test(flavor = "current_thread")]
async fn test_sighup_swap_is_observed_without_restart() {
    let Some(fix_url) = std::env::var("CARGOBIKE_TEST_DATABASE_URL")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        eprintln!("skipping: CARGOBIKE_TEST_DATABASE_URL is unset");
        return;
    };

    // Port acquisition with the listener released before the server spawns.
    let port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        listener.local_addr().expect("port").port()
    };
    let issuer = format!("http://127.0.0.1:{port}");
    let templates_dir = std::env::temp_dir().join(format!("cb-reload-templates-{port}"));
    let _ = std::fs::create_dir_all(&templates_dir);
    std::fs::write(
        templates_dir.join("service.yaml"),
        r#"
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
"#,
    )
    .expect("template");
    let templates_dir_display = templates_dir.display().to_string();
    let config_path = std::env::temp_dir().join(format!("cb-reload-{port}.yaml"));
    // Two keys across the whole test: v1 knows only key-one; v2 only key-two.
    // DISTINCT plaintexts: argon2 verifies a value, not an entry name, so
    // "the old key is refused" needs the hash to disappear with the entry.
    let plaintext_one = "cbk_reload_one";
    let plaintext_two = "cbk_reload_two";
    let hash_one = argon_hash(plaintext_one);
    let hash_two = argon_hash(plaintext_two);

    std::fs::write(
        &config_path,
        format!(
            "server:\n  listen: \"127.0.0.1:{port}\"\n  public_url: \"{issuer}\"\n\n\
             database:\n  url: \"{fix_url}\"\n\n\
             templates:\n  directory: {templates_dir_display}\n\n\
             auth:\n  api_keys:\n    - name: key-one\n      hash: \"{hash_one}\"\n      grants: [release:create, application:read, template:read]\n\n\
             applications:\n  - name: my-service\n    source: {{ provider: github, id: \"123456\" }}\n    template: service@1\n    releasers:\n      - api_key: key-one\n    environments:\n      preview:\n        concurrency: supersede\n"
        ),
    )
    .expect("config v1");

    let mut child = Command::new(env!("CARGO_BIN_EXE_cargobike-server"))
        .arg("--config")
        .arg(&config_path)
        .env("CARGOBIKE_SERVER_LOG_FORMAT", "text")
        // The platform's temp dir: a hard-coded mac path has no
        // business in a CI job on another host family.
        .stdout(
            std::fs::File::create(std::env::temp_dir().join("cb-reload-server.log"))
                .expect("log file"),
        )
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("server spawns");
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    // v1: key-one verifies and creates (202).
    assert!(wait_for(&client, &base).await, "server became ready");
    let response = client
        .post(format!("{base}/api/v1/releases"))
        .header("Authorization", format!("Bearer {plaintext_one}"))
        .json(&serde_json::json!({ "application": "my-service", "version": format!("1.0.{port}") }))
        .send()
        .await
        .expect("http");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::ACCEPTED,
        "v1 create via key-one must succeed"
    );

    // v2: replace the key list with key-two; HUP; the old key must fail.
    std::fs::write(
        &config_path,
        format!(
            "server:\n  listen: \"127.0.0.1:{port}\"\n  public_url: \"{issuer}\"\n\n\
             database:\n  url: \"{fix_url}\"\n\n\
             templates:\n  directory: {templates_dir_display}\n\n\
             auth:\n  api_keys:\n    - name: key-two\n      hash: \"{hash_two}\"\n      grants: [release:create, release:read, application:read, template:read]\n\n\
             applications:\n  - name: my-service\n    source: {{ provider: github, id: \"123456\" }}\n    template: service@1\n    versioning:\n      scheme: semver\n      tag_format: \"v{{version}}\"\n    releasers:\n      - api_key: key-two\n    environments:\n      preview:\n        repo: {{ provider: github, id: \"42\" }}\n        edits:\n          - file: apps/preview/manifest.yaml\n            field: image.tag\n"
        ),
    )
    .expect("config v2");
    Command::new("kill")
        .arg("-HUP")
        .arg(child.id().to_string())
        .output()
        .expect("HUP");

    // Poll: the old key (the removed in v2) refuses…
    let mut refused = false;
    for _ in 0..50 {
        let response = client
            .get(format!("{base}/api/v1/whoami"))
            .header("Authorization", format!("Bearer {plaintext_one}"))
            .send()
            .await
            .expect("http");
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            refused = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(refused, "the old key must be refused after the reload");

    // …key-two accepts (the same plaintext, new config): the create works.
    let response = client
        .post(format!("{base}/api/v1/releases"))
        .header("Authorization", format!("Bearer {plaintext_two}"))
        .json(&serde_json::json!({ "application": "my-service", "version": format!("1.1.{port}") }))
        .send()
        .await
        .expect("http");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::ACCEPTED,
        "v2 create via key-two must succeed after the reload"
    );

    child.kill().expect("kill the server");
    let _ = child.wait();
}

/// The readiness probe of the freshly spawned server.
async fn wait_for(client: &reqwest::Client, base: &str) -> bool {
    for _ in 0..50 {
        if client
            .get(format!("{base}/api/v1/live"))
            .send()
            .await
            .map(|response| response.status() == reqwest::StatusCode::OK)
            .unwrap_or(false)
        {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    false
}

use std::process::Command;

#[allow(clippy::expect_used)]
fn argon_hash(plain: &str) -> String {
    use argon2::password_hash::{PasswordHasher, SaltString};
    let mut random = [0_u8; 16];
    rand_core::OsRng.fill_bytes(&mut random);
    let salt = SaltString::encode_b64(&random).expect("salt");
    argon2::Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .expect("hash")
        .to_string()
}

use rand_core::RngCore as _;
