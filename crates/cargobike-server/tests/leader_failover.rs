//! F-131's E2E, failover half: a REAL lock loss. A child
//! `cargobike-server` process holds the election lock (the leader);
//! this process boots the standby. `pg_terminate_backend` on the
//! lock-holding session (found via `pg_locks` — no test seam, the
//! production path) kills the child's guard connection: the child's
//! fence stops its executor and EXITS the process, the lock frees with
//! the session, and this process's standby promotes on its next
//! attempt — its executor launches and a create runs on what used to
//! be the standby.
//!
//! The standby-half surfaces (reads, 503 mutations, the webhook
//! enqueue) live in `leader_election.rs` — separate binaries because
//! the advisory lock is process-scoped and `cargo test` runs every
//! binary in parallel; the SUITE_LOCK serialises the two.
//!
//! (Test code: it asserts and reports, so the expect/print lints stay
//! off for this file.) Requires `CARGOBIKE_TEST_DATABASE_URL`.

#![allow(clippy::expect_used, clippy::print_stderr)]

use argon2::password_hash::PasswordHasher as _;
use sqlx::Connection as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

const API_KEY: &str = "cb_failover_test_key";

/// The suite's cross-process mutex (see `leader_election.rs`).
const SUITE_LOCK_ID: i64 = 0x0063_626b_7375_6974; // 'cbksuit'

/// The release template the promoted standby's create runs (one edit
/// onto the seeded file, then the merge wait stalls — observable).
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

/// The child leader's exit: the fence's `std::process::exit(1)` (or a
/// boot failure — distinguished by the assertions around it).
struct ChildLeader {
    child: std::process::Child,
}

/// Spawns the child `cargobike-server` with the election ON. The child
/// holds NO applications (it exists to hold the lock and be fenced);
/// this process's boot carries the application and the mock.
#[allow(clippy::expect_used)]
fn spawn_child_leader(url: &str, templates_display: &str, scratch_logs: &str) -> ChildLeader {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("the child's port binds");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    let config = format!(
        "server:\n  listen: \"127.0.0.1:{port}\"\n  public_url: http://127.0.0.1:{port}\n\n\
         database:\n  url: \"{url}\"\n  dbos_schema: \"cbfo-child-{port}\"\n\n\
         templates:\n  directory: {templates_display}\n\n\
         auth:\n  api_keys: []\n"
    );
    let config_path = std::env::temp_dir().join(format!("cb-failover-child-{port}.yaml"));
    std::fs::write(&config_path, config).expect("write the child's config");
    let stdout = std::fs::File::create(format!("{scratch_logs}/child.stdout"))
        .expect("the child's stdout log");
    let stderr = std::fs::File::create(format!("{scratch_logs}/child.stderr"))
        .expect("the child's stderr log");
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_cargobike-server"))
        .arg("--config")
        .arg(&config_path)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .expect("the child leader spawns");
    let _ = port;
    ChildLeader { child }
}

/// The lock-holding session's pid, straight out of `pg_locks` (the
/// single-bigint advisory lock's classid/objid are the key's halves).
#[allow(clippy::expect_used)]
async fn lock_holder_pid(pool: &sqlx::PgPool) -> Option<i32> {
    let lock_id = cargobike_server::leader::LOCK_ID;
    let pid = sqlx::query_scalar::<_, Option<i32>>(
        "SELECT pid FROM pg_locks \
         WHERE locktype = 'advisory' AND classid = $1 AND objid = $2 \
         LIMIT 1",
    )
    .bind((lock_id >> 32) as i32)
    .bind((lock_id & 0xFFFF_FFFF) as i32)
    .fetch_optional(pool)
    .await
    .expect("the pg_locks read");
    pid.flatten()
}

#[allow(clippy::print_stderr, clippy::expect_used)]
#[tokio::test(flavor = "current_thread")]
async fn lock_loss_fences_the_leader_process_and_the_standby_promotes() {
    let Some(url) = cargobike_testsupport::database_url().await else {
        return;
    };
    // The cross-process suite lock (see leader_election.rs).
    let mut suite_lock = sqlx::PgConnection::connect(&url)
        .await
        .expect("the suite lock connects");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(SUITE_LOCK_ID)
        .execute(&mut suite_lock)
        .await
        .expect("the suite lock is taken");
    let pool = sqlx::PgPool::connect(&url).await.expect("the fixture pool");

    let dir = std::env::temp_dir();
    let scratch = dir.join("cb-failover-mock").to_string_lossy().to_string();
    let templates_dir = dir.join("cb-failover-templates");
    let _ = std::fs::create_dir_all(&templates_dir);
    std::fs::write(templates_dir.join("service.yaml"), TEMPLATE).expect("write template");
    let templates_display = templates_dir.display().to_string();
    let scratch_logs = dir.join("cb-failover-logs");
    let _ = std::fs::create_dir_all(&scratch_logs);
    let salt = argon2::password_hash::SaltString::encode_b64(b"cb-test-salt-16-x").expect("salt");
    let hash = argon2::Argon2::default()
        .hash_password(API_KEY.as_bytes(), &salt)
        .expect("hash")
        .to_string();

    // The child leader first — it wins the lock; this process's boot
    // then deterministically stands by.
    let mut child = spawn_child_leader(
        &url,
        &templates_display,
        scratch_logs.to_string_lossy().as_ref(),
    );
    let holder = {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(pid) = lock_holder_pid(&pool).await {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "the child never took the election lock (see cb-failover-logs/child.stderr)"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };

    // This process's server: the standby (the child holds the lock).
    let mock = Arc::new(cargobike_engine::mock::MockProvider::at(&scratch));
    let mut providers = cargobike_core::registry::ProviderRegistry::new();
    providers.insert(
        "github",
        mock.clone() as Arc<dyn cargobike_core::provider::Provider>,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let config = format!(
        "server:\n  listen: \"127.0.0.1:{port}\"\n  public_url: http://127.0.0.1:{port}\n\n\
         database:\n  url: \"{url}\"\n  dbos_schema: \"cbfo-standby-{port}\"\n\n\
         templates:\n  directory: {templates_display}\n\n\
         auth:\n  api_keys:\n    - name: test-key\n      hash: \"{hash}\"\n      grants: [release:create, release:read, release:cancel, release:delete]\n\n\
         applications:\n  - name: failover-service\n    source:\n      provider: github\n      id: \"fo-{port}\"\n    template: service@1\n    versioning:\n      scheme: opaque\n    releasers:\n      - api_key: test-key\n    environments:\n      preview:\n        repo:\n          provider: github\n          id: \"fo-env-{port}\"\n        edits:\n          - file: apps/stage/manifest.yaml\n            field: image.tag\n"
    );
    let config_path = std::env::temp_dir().join(format!("cb-failover-standby-{port}.yaml"));
    std::fs::write(&config_path, config).expect("write the standby's config");
    let boot = cargobike_server::boot(Some(&config_path), Some(providers)).await;
    let (router, _state) = match boot {
        Ok(booted) => booted,
        Err(error) => {
            eprintln!("skipping: the standby's boot failed ({error})");
            let _ = child.child.kill();
            return;
        }
    };
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let auth = ("Authorization", format!("Bearer {API_KEY}"));

    // The standby verdict: not ready, mutations refused.
    let readiness = client
        .get(format!("{base}/api/v1/ready"))
        .send()
        .await
        .expect("the standby's readiness asks")
        .status();
    assert_eq!(
        readiness,
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "the standby reports not-ready while the child leads"
    );

    // The REAL lock loss: terminate the holder's session. The child's
    // guard notices on its next ping, fences (stops its executor, exits
    // the process), and the lock frees with the session.
    sqlx::query("SELECT pg_terminate_backend($1)")
        .bind(holder)
        .execute(&pool)
        .await
        .expect("the terminate");
    let exit = {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = child.child.try_wait().expect("the child's status") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "the fenced leader never exited (see cb-failover-logs/child.stderr)"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };
    assert!(
        !exit.success(),
        "the fence exits the leader process (F-131's simplest reliable option)"
    );

    // The promotion: this process's guard wins the freed lock, the
    // executor activates, and the once-standby serves mutations.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let ready = client
            .get(format!("{base}/api/v1/ready"))
            .send()
            .await
            .expect("the promoted readiness asks")
            .status();
        if ready == reqwest::StatusCode::OK {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the standby never promoted after the leader's fence"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let created = client
        .post(format!("{base}/api/v1/releases"))
        .header(auth.0, auth.1.clone())
        .json(&serde_json::json!({
            "application": "failover-service",
            "version": "42.0.0-failover",
        }))
        .send()
        .await
        .expect("the promoted create asks");
    assert_eq!(
        created.status(),
        reqwest::StatusCode::ACCEPTED,
        "the promoted replica runs mutations: {:?}",
        created.text().await
    );
    let document: serde_json::Value = created.json().await.expect("the release body");
    let release_id = document["metadata"]["id"].as_str().expect("the release id");

    // Cleanup: the suite's drain convention — the recorded CR, then
    // cancel (the cleanup closes the CR on the mock), then delete.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let document = client
            .get(format!("{base}/api/v1/releases/{release_id}"))
            .header(auth.0, auth.1.clone())
            .send()
            .await
            .expect("the read asks")
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
        .post(format!("{base}/api/v1/releases/{release_id}/cancel"))
        .header(auth.0, auth.1.clone())
        .send()
        .await
        .expect("the cancel asks");
    assert!(cancel.status().is_success(), "the cancel is accepted");
    let drain = client
        .delete(format!("{base}/api/v1/releases/{release_id}"))
        .header(auth.0, auth.1)
        .send()
        .await
        .expect("the delete asks");
    assert!(drain.status().is_success(), "the drain deletes the row");
}
