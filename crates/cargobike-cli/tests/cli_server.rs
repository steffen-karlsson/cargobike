//! The CLI's server-facing integration: the
//! client verbs against a wiremock server, plus the binary's
//! end-to-end behaviour (the render + exit vocabulary) via assert_cmd.
//! The GitHub lifecycle's wiremock half lives with 5.2's webhook chain.

use cargobike_cli::client::Client;
use cargobike_cli::config::{AuthConfig, OutputFormat, Resolved, SecretRef};
use std::path::Path;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// The document the mock's server hands out (the document's root shape).
fn release_document(id: &str) -> serde_json::Value {
    serde_json::json!({
        "metadata": {
            "id": id,
            "created_at": "2026-01-01T00:00:00.000000Z",
            "updated_at": "2026-01-01T00:00:00.000000Z",
            "resource_version": 1,
            "retried_from": serde_json::Value::Null,
            "labels": {},
            "annotations": {},
        },
        "spec": {
            "application": "web",
            "version": "1.2.3",
            "source": { "provider": "github", "id": "42", "path": null },
            "template": "service",
        },
        "status": {
            "phase": "PendingApproval",
            "error": serde_json::Value::Null,
        },
    })
}

/// One mock server serving the document, a one-item page, and cancel.
async fn mock_release_surface() -> (MockServer, String) {
    let id = "0192f0d0-0000-7000-8000-000000000001";
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/releases/{id}")))
        .respond_with(move |_: &Request| {
            ResponseTemplate::new(200).set_body_json(release_document(id))
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/releases"))
        .respond_with(|_: &Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [release_document(id)],
                "cursor": null,
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/api/v1/releases/{id}/cancel")))
        .respond_with(|_: &Request| ResponseTemplate::new(204))
        .mount(&server)
        .await;
    (server, id.to_owned())
}

/// The bearer-checked surface: 200 when the key file's value was sent.
async fn mock_whoami_with_key(expected_bearer: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/whoami"))
        .and(header(
            "Authorization",
            format!("Bearer {expected_bearer}").as_str(),
        ))
        .respond_with(|_: &Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "grants": [] }))
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(|_: &Request| {
            ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "type": "https://cargobike.dev/errors/unauthenticated",
                "title": "Unauthorized",
                "status": 401,
                "detail": "the token was invalid",
                "code": "MissingToken",
            }))
        })
        .mount(&server)
        .await;
    server
}

fn target(url: &str) -> Resolved {
    Resolved {
        url: url.to_owned(),
        auth: AuthConfig::None,
        ca_file: None,
        output: OutputFormat::Table,
    }
}

fn target_with_key(url: &str, key_path: &Path) -> Resolved {
    Resolved {
        url: url.to_owned(),
        auth: AuthConfig::ApiKey {
            api_key: SecretRef::File {
                file: key_path.to_path_buf(),
            },
        },
        ca_file: None,
        output: OutputFormat::Table,
    }
}

#[tokio::test]
async fn test_the_cli_client_walks_the_document_and_page() {
    let (server, id) = mock_release_surface().await;
    let client = Client::new(&target(server.uri().as_str()))
        .await
        .expect("the client builds");

    let document = client
        .get_json(&format!("/api/v1/releases/{id}"))
        .await
        .expect("the document");
    assert_eq!(
        document
            .pointer("/metadata/id")
            .and_then(serde_json::Value::as_str),
        Some(id.as_str())
    );

    let page = client
        .get_json("/api/v1/releases?limit=50")
        .await
        .expect("the page");
    assert!(
        page.get("items")
            .and_then(serde_json::Value::as_array)
            .is_some()
    );
}

#[tokio::test]
async fn test_the_api_key_caught_by_the_bearer_header() {
    let key_path = std::env::temp_dir().join("cb-wiremock-test-key");
    std::fs::write(&key_path, "cb-wiremock-value").expect("write key");
    let server = mock_whoami_with_key("cb-wiremock-value").await;
    let client = Client::new(&target_with_key(server.uri().as_str(), &key_path))
        .await
        .expect("the client builds");
    let identity = client.get_json("/api/v1/whoami").await;
    assert!(identity.is_ok(), "the key rode the header: {identity:?}");
    std::fs::remove_file(&key_path).expect("cleanup");
}

#[tokio::test]
async fn test_problem_details_surface_the_refused_vocabulary() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/releases/missing"))
        .respond_with(|_: &Request| {
            ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "type": "https://cargobike.dev/errors/release-not-found",
                "title": "Not Found",
                "status": 404,
                "detail": "release 0192 not found",
                "code": "ReleaseNotFound",
            }))
        })
        .mount(&server)
        .await;
    let client = Client::new(&target(server.uri().as_str()))
        .await
        .expect("the client builds");
    let failure = client
        .get_json("/api/v1/releases/missing")
        .await
        .expect_err("refused");
    assert_eq!(cargobike_cli::client::exit_code(&failure), 12);
    assert!(failure.to_string().contains("the server refused: 404"));
}

#[tokio::test]
async fn test_unauthenticated_calls_hit_the_401_vocabulary() {
    let server = mock_whoami_with_key("nobody-has-this-key").await;
    let client = Client::new(&target(server.uri().as_str()))
        .await
        .expect("the client builds");
    let failure = client
        .get_json("/api/v1/whoami")
        .await
        .expect_err("refused");
    assert_eq!(cargobike_cli::client::exit_code(&failure), 10);
}

// ---- the binary's end-to-end ------------------------------------------------

use assert_cmd::Command;

#[tokio::test]
async fn test_the_binary_renders_the_document() {
    let (server, id) = mock_release_surface().await;
    let output = Command::cargo_bin("cargobike")
        .expect("the binary")
        .env("CARGOBIKE_URL", server.uri())
        .arg("release")
        .arg("get")
        .arg(id)
        .output()
        .expect("runs");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("PendingApproval"), "the phase shows: {text}");
}

#[tokio::test]
async fn test_the_output_flag_obliges_every_verb() {
    let (server, id) = mock_release_surface().await;
    // -o json on get prints JSON (the document's root), not the table.
    let json_out = Command::cargo_bin("cargobike")
        .expect("the binary")
        .env("CARGOBIKE_URL", server.uri())
        .args(["release", "get", id.as_str(), "-o", "json"])
        .output()
        .expect("runs");
    assert!(json_out.status.success());
    let document: serde_json::Value =
        serde_json::from_slice(&json_out.stdout).expect("a JSON document prints");
    assert_eq!(
        document["status"]["phase"],
        serde_json::json!("PendingApproval")
    );

    // -o yaml on list prints YAML with the items.
    let yaml_out = Command::cargo_bin("cargobike")
        .expect("the binary")
        .env("CARGOBIKE_URL", server.uri())
        .args(["release", "list", "-o", "yaml"])
        .output()
        .expect("runs");
    assert!(yaml_out.status.success());
    let text = String::from_utf8_lossy(&yaml_out.stdout);
    assert!(text.contains("items:"), "the YAML page shape: {text}");
    assert!(text.contains("PendingApproval"), "the phase shows: {text}");

    // The env-format (CARGOBIKE_OUTPUT) fills in when the flag is away.
    let env_out = Command::cargo_bin("cargobike")
        .expect("the binary")
        .env("CARGOBIKE_URL", server.uri())
        .env("CARGOBIKE_OUTPUT", "json")
        .args(["release", "list"])
        .output()
        .expect("runs");
    assert!(env_out.status.success());
    serde_json::from_slice::<serde_json::Value>(&env_out.stdout)
        .expect("the env format renders json");
}

#[tokio::test]
async fn test_the_actions_exchange_reaches_the_oauth_surface() {
    // The binary with the Actions env: the exchange + the authenticated
    // release get against the SAME mock (the OIDC surface then the API).
    let (server, id) = mock_actions_surface().await;
    let output = Command::cargo_bin("cargobike")
        .expect("the binary")
        .env(
            "ACTIONS_ID_TOKEN_REQUEST_URL",
            format!("{}/exchange", server.uri()),
        )
        .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "the-actions-runner-token")
        .env("CARGOBIKE_URL", server.uri())
        .args(["release", "get", id.as_str()])
        .output()
        .expect("runs");
    assert!(
        output.status.success(),
        "the exchange + the authenticated get succeed: {} {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The mock serving the OIDC exchange and the release surface: the
/// Actions' detection is caller-env-driven (no CI in the test env), so
/// the exchange rides CARGOBIKE_AUTH=github-actions.
async fn mock_actions_surface() -> (MockServer, String) {
    let id = "0192f0d0-0000-7000-8000-000000000003";
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/exchange"))
        .and(header("Authorization", "Bearer the-actions-runner-token"))
        .respond_with(|_: &Request| {
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "value": "the-actions-audience-token" }))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/releases/{id}")))
        .and(header("Authorization", "Bearer the-actions-audience-token"))
        .respond_with(move |_: &Request| {
            ResponseTemplate::new(200).set_body_json(release_document(id))
        })
        .expect(1)
        .mount(&server)
        .await;
    let _id = id.to_owned();
    (server, _id)
}

#[tokio::test]
async fn test_the_binary_maps_a_dead_server_to_exit_11() {
    let output = Command::cargo_bin("cargobike")
        .expect("the binary")
        .env("CARGOBIKE_URL", "http://localhost:1")
        .arg("release")
        .arg("get")
        .arg("0192f0d0-0000-7000-8000-000000000002")
        .output()
        .expect("runs");
    assert_eq!(output.status.code(), Some(11), "unreachable maps to 11");
}

#[tokio::test]
async fn test_the_binary_lists_the_table_rows() {
    let (server, id) = mock_release_surface().await;
    let output = Command::cargo_bin("cargobike")
        .expect("the binary")
        .env("CARGOBIKE_URL", server.uri())
        .arg("release")
        .arg("list")
        .arg("--limit")
        .arg("10")
        .output()
        .expect("runs");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains(id.as_str()), "the id shows the row: {text}");
    assert!(text.contains("PendingApproval"), "the phase shows: {text}");
}
