//! F-131's E2E, standby half: two boots in ONE process against the
//! shared fixture database — the first boot's guard wins the election
//! lock, the second stands by. The standby serves reads, refuses
//! mutations with 503, and enqueues a verified webhook delivery onto
//! the LEADER's executor (the rolling-update promise: a tag push that
//! arrives while the replica swap is in flight is never lost).
//!
//! The failover half (a real lock loss fencing the leader process and
//! promoting the standby) lives in `leader_failover.rs` — the two are
//! separate binaries because the advisory lock is process-scoped: this
//! file's guards die with this process, the failover's with its own.
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

const WEBHOOK_SECRET: &str = "cb-leader-webhook-secret";
const API_KEY: &str = "cb_leader_test_key";

/// The suite's cross-process mutex: both leader E2E binaries elect on
/// the ONE production lock id, and `cargo test` runs every binary in
/// parallel — the suite lock (a session advisory lock on a held
/// connection, dropped at the test's end) serialises them so the
/// election's verdicts stay deterministic.
const SUITE_LOCK_ID: i64 = 0x0063_626b_7375_6974; // 'cbksuit'

/// The release template (the webhook's tag-push target): one edit step
/// onto the seeded file, then the merge wait stalls — a durable,
/// observable release the leader runs.
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

/// One server boot: its own port, the SHARED deployment schema (both
/// boots are replicas of ONE deployment — the standby's webhook lane
/// enqueues into the schema the leader's executor polls), the mock
/// provider, and the election ON (the production default) — the FIRST
/// boot of the process wins the advisory lock; the second stands by.
#[allow(clippy::expect_used, clippy::print_stderr)]
async fn electing_server(
    url: &str,
    schema: &str,
    scratch: &str,
    templates_display: &str,
    hash: &str,
) -> Option<String> {
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
         templates:\n  directory: {templates_display}\n\n\
         providers:\n  - name: github\n    type: github\n    webhook_secrets:\n      - {{ env: CB_LEADER_WEBHOOK_SECRET }}\n\n\
         auth:\n  api_keys:\n    - name: test-key\n      hash: \"{hash}\"\n      grants: [release:create, release:read, release:cancel, release:delete]\n\n\
         applications:\n  - name: lead-service-{port}\n    source:\n      provider: github\n      id: \"lead-{port}\"\n    template: service@1\n    versioning:\n      scheme: semver\n      tag_format: \"v{{version}}\"\n    triggers:\n      - event: tag\n        require_tag_protection: false\n    releasers:\n      - api_key: test-key\n    environments:\n      preview:\n        repo:\n          provider: github\n          id: \"lead-env-{port}\"\n        edits:\n          - file: apps/stage/manifest.yaml\n            field: image.tag\n"
    );
    let config_path = std::env::temp_dir().join(format!("cb-leader-election-{port}.yaml"));
    std::fs::write(&config_path, config).expect("write the test config");
    let boot = cargobike_server::boot(Some(&config_path), Some(providers)).await;
    let (router, _state) = match boot {
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
    Some(format!("http://127.0.0.1:{port}"))
}

/// A signed GitHub delivery: the HMAC of the raw body + the event and
/// delivery ids.
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
async fn the_standby_serves_reads_enqueues_webhooks_and_refuses_mutations() {
    let Some(url) = cargobike_testsupport::database_url().await else {
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new(
                    "cargobike_server=debug,cargobike_engine=debug,dbos=warn",
                )
            }),
        )
        .try_init();
    // The cross-process suite lock (see SUITE_LOCK_ID): held for the
    // test's whole duration, released when the connection drops.
    let mut suite_lock = sqlx::PgConnection::connect(&url)
        .await
        .expect("the suite lock connects");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(SUITE_LOCK_ID)
        .execute(&mut suite_lock)
        .await
        .expect("the suite lock is taken");

    let dir = std::env::temp_dir();
    let scratch = dir.join("cb-leader-mock").to_string_lossy().to_string();
    let templates_dir = dir.join("cb-leader-templates");
    let _ = std::fs::create_dir_all(&templates_dir);
    std::fs::write(templates_dir.join("service.yaml"), TEMPLATE).expect("write template");
    let templates_display = templates_dir.display().to_string();
    let salt = argon2::password_hash::SaltString::encode_b64(b"cb-test-salt-16-x").expect("salt");
    let hash = argon2::Argon2::default()
        .hash_password(API_KEY.as_bytes(), &salt)
        .expect("hash")
        .to_string();
    unsafe { std::env::set_var("CB_LEADER_WEBHOOK_SECRET", WEBHOOK_SECRET) };

    // The FIRST boot wins the election (its guard connects before the
    // second boot's exists); the second stands by.
    // The deployment's DBOS schema: one per RUN (the pid), shared by
    // both boots — cross-suite isolation with in-run deployment
    // semantics.
    let schema = format!("cblead-{}", std::process::id());
    let Some(leader) = electing_server(&url, &schema, &scratch, &templates_display, &hash).await
    else {
        return;
    };
    let Some(standby) = electing_server(&url, &schema, &scratch, &templates_display, &hash).await
    else {
        return;
    };
    let client = reqwest::Client::new();
    let auth = ("Authorization", format!("Bearer {API_KEY}"));

    // The readiness split (F-131): the leader 200, the standby 503.
    let leader_ready = client
        .get(format!("{leader}/api/v1/ready"))
        .send()
        .await
        .expect("the leader's readiness asks")
        .status();
    assert_eq!(leader_ready, reqwest::StatusCode::OK, "the leader is ready");
    let standby_ready = client
        .get(format!("{standby}/api/v1/ready"))
        .send()
        .await
        .expect("the standby's readiness asks")
        .status();
    assert_eq!(
        standby_ready,
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "the standby reports not-ready"
    );

    // Reads serve on the standby; mutations refuse 503 with the
    // problem naming the standby (the require_leader gate).
    let listed = client
        .get(format!("{standby}/api/v1/releases"))
        .header(auth.0, auth.1.clone())
        .send()
        .await
        .expect("the standby's list asks");
    assert_eq!(
        listed.status(),
        reqwest::StatusCode::OK,
        "reads serve on the standby"
    );
    let refused = client
        .post(format!("{standby}/api/v1/releases"))
        .header(auth.0, auth.1.clone())
        .json(&serde_json::json!({
            "application": "lead-service",
            "version": "1.0.0",
        }))
        .send()
        .await
        .expect("the standby's create asks");
    assert_eq!(
        refused.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "the standby refuses mutations"
    );
    let problem: serde_json::Value = refused.json().await.expect("the problem document");
    assert_eq!(
        problem["type"].as_str(),
        Some("https://cargobike.dev/errors/standby"),
        "the problem types itself the standby (the vocabulary code stays StateConflict): {problem}"
    );

    // The rolling-update promise: a tag push delivered to the STANDBY
    // is verified there and enqueued by name — the LEADER's executor
    // claims the row and runs it (the release appears; the standby
    // never launched an executor of its own). F-95: the pushed SHA is
    // a claim — the tag must exist on the provider (the mock's state
    // file backs both boots' provider handles).
    cargobike_engine::mock::MockProvider::at(&scratch).seed_tag("v3.1.0", "abc123");
    let leader_port = leader.rsplit(':').next().unwrap_or_default().to_owned();
    let body = format!(
        r#"{{"ref":"refs/tags/v3.1.0","after":"abc123","repository":{{"id":"lead-{leader_port}"}},"sender":{{"login":"ska"}}}}"#
    );
    let delivery = format!("d-lead-{leader_port}");
    let accepted = client
        .post(format!("{standby}/webhooks/github"))
        .header(
            "X-Hub-Signature-256",
            &signed_delivery("push", &delivery, body.as_bytes())[0].1,
        )
        .header("X-GitHub-Event", "push")
        .header("X-GitHub-Delivery", &delivery)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.clone())
        .send()
        .await
        .expect("the standby's delivery sends");
    assert_eq!(
        accepted.status(),
        reqwest::StatusCode::ACCEPTED,
        "the standby accepts the verified delivery: {:?}",
        accepted.text().await
    );

    // The leader ran the enqueued workflow: the release exists and its
    // interpreter is under way (the edit landed on the mock).
    let deadline = Instant::now() + Duration::from_secs(30);
    let release_id = loop {
        let page = client
            .get(format!(
                "{leader}/api/v1/releases?application=lead-service-{leader_port}&version=3.1.0"
            ))
            .header(auth.0, auth.1.clone())
            .send()
            .await
            .expect("the leader's list asks");
        let page_json: serde_json::Value = page.json().await.expect("the list body");
        if let Some(id) = page_json["items"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(|item| item["metadata"]["id"].as_str())
        {
            break id.to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "the leader never ran the standby's enqueued webhook: {page_json}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    // The release stalls at its merge wait (the interpreter parks at
    // the durable recv). Wait for the recorded CR, then the suite's
    // drain convention: cancel (the cleanup workflow closes the CR on
    // the mock), delete the row.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let document = client
            .get(format!("{leader}/api/v1/releases/{release_id}"))
            .header(auth.0, auth.1.clone())
            .send()
            .await
            .expect("the leader's read asks")
            .json::<serde_json::Value>()
            .await
            .expect("the release body");
        if document
            .pointer("/status/environments/0/change_request/number")
            .is_some_and(|number| !number.is_null())
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the release never recorded its change request: {document}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let cancel = client
        .post(format!("{leader}/api/v1/releases/{release_id}/cancel"))
        .header(auth.0, auth.1.clone())
        .send()
        .await
        .expect("the leader's cancel asks");
    assert!(cancel.status().is_success(), "the cancel is accepted");
    let drain = client
        .delete(format!("{leader}/api/v1/releases/{release_id}"))
        .header(auth.0, auth.1)
        .send()
        .await
        .expect("the leader's delete asks");
    assert!(drain.status().is_success(), "the drain deletes the row");
}
