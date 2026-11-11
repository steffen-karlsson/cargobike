//! The CLI's server-facing integration: the
//! client verbs against a wiremock server, plus the binary's
//! end-to-end behaviour (the render + exit vocabulary) via assert_cmd.
//! The GitHub lifecycle's wiremock half lives with 5.2's webhook chain.

use cargobike_cli::client::Client;
use cargobike_cli::config::{AuthConfig, OutputFormat, Resolved, SecretRef};
use std::path::Path;
use wiremock::matchers::{body_partial_json, header, method, path};
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

#[tokio::test]
#[allow(clippy::expect_used)]
/// The cursor walk: a first page that says `has_more` makes the CLI
/// request the next page with `after=<cursor>` until the pages run
/// out — deep listings no longer stop at page 1.
async fn test_the_list_walks_the_cursor_to_the_last_page() {
    use wiremock::matchers::query_param;

    let first_id = "0192f0d0-0000-7000-8000-000000000001";
    let second_id = "0192f0d0-0000-7000-8000-000000000002";
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/releases"))
        .and(query_param("after", "0192f0d0-0000-7000-8000-000000000001"))
        .respond_with(move |_: &Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [release_document(second_id)],
                "cursor": null,
                "has_more": false,
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/releases"))
        .respond_with(move |_: &Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [release_document(first_id)],
                "cursor": first_id,
                "has_more": true,
            }))
        })
        .mount(&server)
        .await;
    let output = Command::cargo_bin("cargobike")
        .expect("the binary")
        .env("CARGOBIKE_URL", server.uri())
        .arg("release")
        .arg("list")
        .output()
        .expect("runs");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains(first_id) && text.contains(second_id),
        "both pages' rows show: {text}"
    );
}

/// `application list` (F-106/F-10): the registry's names and their
/// verified-path availability — an unavailable app prints its reason.
#[tokio::test]
async fn test_application_list_renders_the_availability() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/applications"))
        .respond_with(|_: &Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "applications": [
                    {
                        "name": "web",
                        "template": "service@1",
                        "available": true,
                        "unavailable_reason": serde_json::Value::Null,
                    },
                    {
                        "name": "broken",
                        "template": "service@1",
                        "available": false,
                        "unavailable_reason":
                            "the registry declares the repository path `wrong` but the provider reports `truth`",
                    },
                ]
            }))
        })
        .mount(&server)
        .await;
    let output = Command::cargo_bin("cargobike")
        .expect("the binary")
        .env("CARGOBIKE_URL", server.uri())
        .arg("application")
        .arg("list")
        .output()
        .expect("runs");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("web\tservice@1\tavailable")
            && text.contains("broken\tservice@1\tunavailable:"),
        "both availability postures render: {text}"
    );
    assert!(
        text.contains("the provider reports `truth`"),
        "the unavailable reason shows: {text}"
    );
}

/// F-84's untrusted half: inside the Actions environment the CLI's
/// `release create` body carries the CI facts as `annotations` (the
/// server's verified claims stay the authority — nothing here ever
/// asserts provenance).
#[tokio::test]
async fn test_create_carries_the_ci_annotations_inside_actions() {
    let server = MockServer::start().await;
    let id = "0192f0d0-0000-7000-8000-000000000002";
    Mock::given(method("POST"))
        .and(path("/api/v1/releases"))
        .and(body_partial_json(serde_json::json!({
            "application": "web",
            "version": "1.2.3",
            "annotations": {
                "ci": "github-actions",
                "ci.repository": "org/repo",
                "ci.sha": "abc123",
                "ci.run-id": "42",
            }
        })))
        .respond_with(move |_: &Request| {
            ResponseTemplate::new(202).set_body_json(release_document(id))
        })
        .mount(&server)
        .await;
    let uri = server.uri();
    let output = tokio::task::spawn_blocking(move || {
        Command::cargo_bin("cargobike")
            .expect("the binary")
            .env("CARGOBIKE_URL", uri)
            .env("GITHUB_REPOSITORY", "org/repo")
            .env("GITHUB_SHA", "abc123")
            .env("GITHUB_RUN_ID", "42")
            .args(["release", "create", "web", "1.2.3"])
            .output()
            .expect("runs")
    })
    .await
    .expect("joined");
    assert!(output.status.success(), "the create sent");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains(id), "the created release renders: {text}");
    // The recorded request's body is the whole proof: a miss would have
    // answered 404 and failed above.
}

/// The watch's SSE frame vocabulary (the server's `event:` is the
/// event-log kind, `id:` the per-release sequence).
fn sse_frame(sequence: u64, kind: &str, environment: Option<&str>) -> String {
    let data = match environment {
        Some(environment) => serde_json::json!({
            "sequence": sequence,
            "at": "2026-01-01T00:00:00Z",
            "actor": { "subject": "ci" },
            "kind": kind,
            "reason": format!("the environment {environment} reached its phase"),
            "data": { "environment": environment, "phase": "Completed" },
            "hash": "00",
            "prev": "00",
        }),
        None => serde_json::json!({
            "sequence": sequence,
            "at": "2026-01-01T00:00:00Z",
            "actor": { "subject": "ci" },
            "kind": kind,
            "reason": "why",
            "data": {},
            "hash": "00",
            "prev": "00",
        }),
    };
    format!("id: {sequence}\nevent: {kind}\ndata: {data}\n\n")
}

/// Mounts the watch lane: the SSE endpoint (a canned stream) and the
/// release document the close path reads.
async fn mock_watch_lane(body: String, document_phase: &str) -> (MockServer, String) {
    let id = "0192f0d0-0000-7000-8000-000000000003";
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/releases/{id}/watch")))
        .respond_with(move |_: &Request| {
            // The raw-body form: wiremock's `set_body_string` stamps
            // its own `text/plain` mime over any header.
            ResponseTemplate::new(200).set_body_raw(body.clone().into_bytes(), "text/event-stream")
        })
        .mount(&server)
        .await;
    let mut document = release_document(id);
    document["status"]["phase"] = serde_json::Value::String(document_phase.to_owned());
    let phase = document_phase.to_owned();
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/releases/{id}")))
        .respond_with(move |_: &Request| {
            let mut document = document.clone();
            document["status"]["phase"] = serde_json::Value::String(phase.clone());
            ResponseTemplate::new(200).set_body_json(document.clone())
        })
        .mount(&server)
        .await;
    (server, id.to_owned())
}

/// The binary's (code, stdout) for a watch invocation.
#[allow(clippy::expect_used)]
async fn run_watch(
    url: &str,
    id: &str,
    until: Option<&str>,
    timeout: Option<&str>,
) -> (i32, String) {
    let url = url.to_owned();
    let id = id.to_owned();
    let until = until.map(str::to_owned);
    let timeout = timeout.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        let mut arguments = vec!["release".to_owned(), "watch".to_owned(), id];
        if let Some(until) = until.as_deref() {
            arguments.push("--until".to_owned());
            arguments.push(until.to_owned());
        }
        if let Some(timeout) = timeout.as_deref() {
            arguments.push("--timeout".to_owned());
            arguments.push(timeout.to_owned());
        }
        let output = Command::cargo_bin("cargobike")
            .expect("the binary")
            .env("CARGOBIKE_URL", url)
            .args(&arguments)
            .output()
            .expect("runs");
        let code = output.status.code().unwrap_or(-1);
        let stdout = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        (code, stdout)
    })
    .await
    .expect("joined")
}

/// US-4's matrix, the `--until` half: the environment's own terminal
/// event ends the watch — completed exits 0.
#[tokio::test]
async fn test_watch_until_completed_exits_zero() {
    let body = format!(
        "{}{}",
        sse_frame(1, "created", None),
        sse_frame(2, "environment-completed", Some("preview")),
    );
    let (server, id) = mock_watch_lane(body, "Running").await;
    let (code, out) = run_watch(&server.uri(), &id, Some("preview"), None).await;
    assert_eq!(code, 0, "completed exits 0: {out}");
    assert!(out.contains("completed"), "the message says it: {out}");
}

/// T5f: a Skipped `--until` environment counts as success (exit 0
/// with the message).
#[tokio::test]
async fn test_watch_until_skipped_exits_zero() {
    let body = sse_frame(1, "environment-skipped", Some("preview"));
    let (server, id) = mock_watch_lane(body, "Running").await;
    let (code, out) = run_watch(&server.uri(), &id, Some("preview"), None).await;
    assert_eq!(code, 0, "skipped counts as success: {out}");
    assert!(out.contains("skipped"), "the message says it: {out}");
}

/// The `--until` failures: the environment's `environment-failed`
/// exits 1; a canceled environment exits 2.
#[tokio::test]
async fn test_watch_until_failed_and_canceled_exit_one_and_two() {
    let body = sse_frame(1, "environment-failed", Some("preview"));
    let (server, id) = mock_watch_lane(body, "Running").await;
    let (code, out) = run_watch(&server.uri(), &id, Some("preview"), None).await;
    assert_eq!(code, 1, "failed exits 1: {out}");

    let body = sse_frame(1, "environment-canceled", Some("preview"));
    let (server, id) = mock_watch_lane(body, "Canceled").await;
    let (code, out) = run_watch(&server.uri(), &id, Some("preview"), None).await;
    assert_eq!(code, 2, "canceled exits 2: {out}");
}

/// Without `--until`: the stream's close (the server ends it on the
/// terminal phase) reads the document — the phase maps the code.
#[tokio::test]
async fn test_watch_without_until_reads_the_rolled_up_phase() {
    let body = sse_frame(1, "created", None);
    let (server, id) = mock_watch_lane(body.clone(), "Canceled").await;
    let (code, out) = run_watch(&server.uri(), &id, None, None).await;
    assert_eq!(code, 2, "a canceled release exits 2: {out}");
    assert!(out.contains("canceled"), "the message says it: {out}");

    let (server, id) = mock_watch_lane(body.clone(), "Failed").await;
    let (code, out) = run_watch(&server.uri(), &id, None, None).await;
    assert_eq!(code, 1, "a failed release exits 1: {out}");

    let (server, id) = mock_watch_lane(body.clone(), "Completed").await;
    let (code, _) = run_watch(&server.uri(), &id, None, None).await;
    assert_eq!(code, 0, "a completed release exits 0");
}

/// The CLI's `--timeout` bounds the idle too: exit 3 when the server
/// never answers the stream.
#[tokio::test]
async fn test_watch_timeout_exits_three() {
    let id = "0192f0d0-0000-7000-8000-000000000004";
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/releases/{id}/watch")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_delay(std::time::Duration::from_secs(3)),
        )
        .mount(&server)
        .await;
    let (code, out) = run_watch(&server.uri(), id, None, Some("1s")).await;
    assert_eq!(code, 3, "the CLI-side timeout exits 3: {out}");
}

/// The client column: a refused bearer is 10, a dead server is 11,
/// anything else the server answered is 12.
#[tokio::test]
async fn test_watch_client_failures_exit_ten_eleven_twelve() {
    let id = "0192f0d0-0000-7000-8000-000000000005";
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/releases/{id}/watch")))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let (code, _) = run_watch(&server.uri(), id, None, None).await;
    assert_eq!(code, 10, "a refused bearer exits 10");

    // The unreachable server: a dead port's transport failure is 11.
    let (code, _) = run_watch("http://127.0.0.1:1", id, None, Some("2s")).await;
    assert_eq!(code, 11, "an unreachable server exits 11");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/releases/{id}/watch")))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "code": "ReleaseNotFound", "detail": "no such release"
        })))
        .mount(&server)
        .await;
    let (code, _) = run_watch(&server.uri(), id, None, None).await;
    assert_eq!(code, 12, "an unknown release is an invalid response (12)");
}
