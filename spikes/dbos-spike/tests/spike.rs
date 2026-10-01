//! Spike experiments (PRD task 1.1), driven sequentially in one test so
//! they share the postgres container without clobbering each other. Each
//! experiment runs under its own `app_name`, so rows never mix.
//!
//! Run with docker available: `cargo test -p dbos-spike`.

use std::time::{Duration, Instant};

use spike_lib::SpikeInput;

use spike_lib as spike_lib;

/// The spike's postgres, run natively (brew postgresql@17 on port
/// PG_PORT; started by `scripts/spike-postgres.sh`, not by these tests).
/// The docker path is kept for Linux CI, which has a usable container runtime.
fn pg_url() -> String {
    // Native port first: the local macOS fixture.
    if std::env::var("CB_SPIKE_NATIVE_PG").is_ok() {
        return format!("postgres://postgres@127.0.0.1:{}/postgres", spike_lib::PG_PORT);
    }
    let name = "cb-spike-pg";
    let _ = std::process::Command::new("docker").args(["rm", "-f", name]).output();
    let result = std::process::Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            name,
            "-e",
            "POSTGRES_PASSWORD=postgres",
            "-e",
            "POSTGRES_DB=postgres",
            "-p",
            &format!("{}:5432", spike_lib::PG_PORT),
            "postgres:16-alpine",
        ])
        .output()
        .expect("docker must be available");
    assert!(result.status.success(), "docker run failed: {:?}", String::from_utf8_lossy(&result.stderr));
    format!("postgres://postgres:postgres@127.0.0.1:{}/postgres", spike_lib::PG_PORT)
}

/// Boots, retrying while the container initializes.
async fn boot_checked(
    app: &str,
    url: &str,
) -> (
    dbos::DBOS,
    dbos::WorkflowRef<spike_lib::SpikeInput, spike_lib::SpikeResult, spike_lib::SpikeError>,
) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match spike_lib::boot(app, url).await {
            Ok(pair) => return pair,
            Err(ref error) if Instant::now() < deadline => {
                eprintln!("boot retrying: {error}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(error) => panic!("boot failed: {error:?}"),
        }
    }
}

/// Scratch dir unique per experiment.
fn scratch(name: &str) -> String {
    let base = std::env::temp_dir().join("cb-spike");
    std::fs::create_dir_all(base.join(name)).expect("scratch mkdir");
    base.join(name).to_string_lossy().into_owned()
}

fn clear_markers(dir: &str) {
    for marker in ["s1", "s2", "s3"] {
        let _ = std::fs::remove_file(
            std::path::Path::new(dir).join(format!("{marker}.count")),
        );
    }
    let _ = std::fs::remove_file(std::path::Path::new(dir).join("failed-once"));
}

#[tokio::test(flavor = "current_thread")]
async fn test_spike_experiments() {
    let url = pg_url();

    // E1: steps record results; runs of the same workflow keep working
    // (F-14, F-15). The second run with a fresh id re-executes bodies.
    {
        let app = "spike-e1";
        let (instance, reference) = boot_checked(app, &url).await;
        let input = SpikeInput {
            mode: "counter".to_owned(),
            scratch: scratch("e1"),
            params: serde_json::json!({}),
        };
        clear_markers(&input.scratch);
        let result = reference.run(input).await.expect("counter run succeeds");
        assert_eq!(result.markers["done"], true);
        assert_eq!(spike_lib::count_marker(&scratch("e1"), "s1"), 1);
        instance.shutdown().await;
    }

    // E2: topics separate waits; the idempotency key makes re-sends no-ops
    // (F-20a). The workflow consumes A, waits again, and never sees B.
    {
        let (instance, reference) = spike_lib::boot("spike-e2", &url).await.expect("boot e2");
        let options = dbos::StartOptions {
            workflow_id: Some("spike-e2-wait"),
            ..dbos::StartOptions::default()
        };
        let input = SpikeInput {
            mode: "await-message".to_owned(),
            scratch: scratch("e2"),
            params: serde_json::json!({ "topic": "spike-e2/s0" }),
        };
        let handle = reference.start_with(input, options).await.expect("start e2");
        instance
            .send_with(
                "spike-e2-wait",
                &serde_json::json!({ "text": "a" }),
                dbos::SendOptions {
                    topic: Some("spike-e2/s0"),
                    idempotency_key: Some("spike-e2/k"),
                    ..dbos::SendOptions::default()
                },
            )
            .await
            .expect("send a");
        let _ = tokio::time::sleep(Duration::from_millis(200)).await;
        // A redelivered webhook would send again with the same key: refuse.
        instance
            .send_with(
                "spike-e2-wait",
                &serde_json::json!({ "text": "b" }),
                dbos::SendOptions {
                    topic: Some("spike-e2/s0"),
                    idempotency_key: Some("spike-e2/k"),
                    ..dbos::SendOptions::default()
                },
            )
            .await
            .expect("send b (deduped, same key)");
        let result = handle.result().await.expect("e2 receives a once");
        assert_eq!(result.markers["received"], serde_json::json!({ "text": "a" }));
        assert_eq!(result.markers["second"], serde_json::Value::Null);
        instance.shutdown().await;
    }

    // E4: fork from LastFailure replays recorded steps without re-running
    // their bodies (US-7, F-23).
    {
        let app = "spike-e4";
        let dir = scratch("e4");
        clear_markers(&dir);
        spike_lib::write_marker(&dir, "failed-once", 1);
        let (instance, reference) = spike_lib::boot(app, &url).await.expect("boot e4");
        let input = SpikeInput {
            mode: "will-fail".to_owned(),
            scratch: dir.clone(),
            params: serde_json::json!({}),
        };
        let options = dbos::StartOptions {
            workflow_id: Some("spike-e4-original"),
            ..dbos::StartOptions::default()
        };
        let original = reference.start_with(input, options).await.expect("start e4");
        let _ = original.result().await; // permanent failure expected
        assert_eq!(spike_lib::count_marker(&dir, "s1"), 1);
        assert_eq!(spike_lib::count_marker(&dir, "s2"), 1);
        assert!(spike_lib::count_marker(&dir, "s3") == 0);

        // Remove the failure flag; fork from LastFailure.
        spike_lib::clear(&dir, "failed-once");
        let fork = instance
            .fork_with::<spike_lib::SpikeResult, spike_lib::SpikeError>(
                "spike-e4-original",
                dbos::ForkFrom::LastFailure,
                dbos::ForkOptions::default(),
            )
            .await
            .expect("fork");
        let forked = fork.result().await.expect("fork completes");
        assert_eq!(forked.markers["done"], true);
        // s1 was replayed from its recorded result (body NOT re-run).
        assert_eq!(spike_lib::counts(&dir)["s1"], serde_json::json!(1));
        assert_eq!(spike_lib::counts(&dir)["s3"], serde_json::json!(1));
        instance.shutdown().await;
    }

    // E3 + E5: the process dies mid-workflow; the replacement (same app
    // version) recovers the PENDING row, the durable sleep resumes at the
    // original wake time, and recorded steps are not re-run (F-21, F-24).
    let dir = scratch("e3");
    clear_markers(&dir);
    let app = "spike-e3";
    let url2 = url.clone();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_waiter"))
        .args(["start", app, &url2, "e3-wire", &dir, "sleep-restart", "20"])
        .spawn()
        .expect("waiter spawns");
    wait_for_marker(&dir, "s1", Duration::from_secs(60)).await;
    child.kill().expect("kill waiter");
    let _ = child.wait();

    let started = Instant::now();
    let mut holder = std::process::Command::new(env!("CARGO_BIN_EXE_waiter"))
        .args(["hold", app, &url2])
        .spawn()
        .expect("hold spawns");
    // s2's marker appears ≈ 20s from the ORIGINAL start, not from the restart.
    wait_for_marker(&dir, "s2", Duration::from_secs(60)).await;
    let elapsed = started.elapsed();
    holder.kill().expect("kill holder");
    let _ = holder.wait();
    assert!(
        elapsed < Duration::from_secs(45),
        "sleep resumed at the original wake time (took {elapsed:?})"
    );
    assert_eq!(spike_lib::count_marker(&dir, "s1"), 1, "s1 not re-run after recovery");
}

/// Polls a marker file until it exists.
async fn wait_for_marker(dir: &str, marker: &str, limit: Duration) {
    let path = std::path::Path::new(dir).join(format!("{marker}.count"));
    let deadline = Instant::now() + limit;
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {path:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
