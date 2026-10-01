//! The T1 crash harness (PRD §14.3): kill the process at every built-in
//! action step's boundary, recover, and assert the release converges to
//! the same result with no duplicated side effects.
//!
//! Requires `--features t1-crash-hooks` (the milestone hooks + the mock
//! provider) and `CARGOBIKE_TEST_DATABASE_URL`; skips cleanly otherwise.

#![cfg(feature = "t1-crash-hooks")]

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The three built-ins the harness template mounts; a milestone is
/// `after/{environment}/{step_id}` (the boundary right after the step's
/// durable checkpoint attempt).
const MILESTONES: [&str; 3] = ["after/stage/edit", "after/stage/commit", "after/stage/cr"];

fn fixture_database_url() -> Option<String> {
    match std::env::var("CARGOBIKE_TEST_DATABASE_URL")
        .ok()
        .filter(|value| !value.is_empty())
    {
        Some(url) => Some(url),
        None => {
            eprintln!("skipping: no fixture database (CARGOBIKE_TEST_DATABASE_URL)");
            None
        }
    }
}

/// Spawns one harness process. `start_milestone` runs `start` with the
/// self-exit point; `None` runs `hold` (DBOS's boot-time recovery offer,
/// the spike's E3/E5 pattern). Both streams go to log files in the
/// scratch dir for diagnosis.
fn spawn_harness(
    binary: &str,
    app: &str,
    scratch: &std::path::Path,
    release_id: &str,
    database_url: &str,
    kill_milestone: Option<&str>,
) -> Result<std::process::Child, String> {
    let name = if kill_milestone.is_some() {
        "killed"
    } else {
        "hold"
    };
    let stdout = std::fs::File::create(scratch.join(format!("{name}.stdout")))
        .map_err(|error| format!("log file: {error}"))?;
    let stderr = std::fs::File::create(scratch.join(format!("{name}.stderr")))
        .map_err(|error| format!("log file: {error}"))?;
    let mut command = Command::new(binary);
    command
        .arg(app)
        .arg(scratch)
        .arg(release_id)
        .env("CB_HARNESS_DB_URL", database_url)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    command.arg(if kill_milestone.is_some() {
        "start"
    } else {
        "hold"
    });
    if let Some(milestone) = kill_milestone {
        command.env("CB_T1_KILL_AT", milestone);
    }
    command.spawn().map_err(|error| format!("spawn: {error}"))
}

/// Waits for the child's exit code; the deadline force-kills with an
/// error (the milestone or the recovery failure surfaces through logs).
fn wait_child(
    mut child: std::process::Child,
    deadline: Duration,
    label: &str,
) -> Result<i32, String> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return Ok(status.code().unwrap_or(-1));
        }
        if start.elapsed() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(label.to_owned());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The mock's state file (convergence facts across the kill).
fn mock_state(scratch: &std::path::Path) -> Option<String> {
    let mut contents = String::new();
    std::fs::File::open(scratch.join("state.json"))
        .ok()?
        .read_to_string(&mut contents)
        .ok()?;
    Some(contents)
}

/// Polls until the release converged: one branch, one commit, one CR —
/// and nothing else (T1's no-duplicate-side-effects assertion).
fn assert_convergence(scratch: &std::path::Path, milestone: &str) {
    let deadline = Instant::now() + Duration::from_secs(90);
    let state = loop {
        if let Some(text) = mock_state(scratch) {
            let value: serde_json::Value = serde_json::from_str(&text).expect("state parses");
            let creates = value
                .get("branch_creates")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let commits = value
                .get("commit_runs")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let crs = value
                .get("change_requests")
                .and_then(serde_json::Value::as_object)
                .map(|entries| entries.len())
                .unwrap_or(0);
            if creates == 1 && commits == 1 && crs == 1 {
                break value;
            }
        }
        if Instant::now() > deadline {
            panic!(
                "release did not converge after the kill at {milestone}: state {:?}",
                mock_state(scratch)
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let files = state
        .get("files")
        .and_then(serde_json::Value::as_object)
        .expect("files map");
    assert!(
        files.keys().any(|key| key.ends_with("image.tag")),
        "the committed edit result is absent after {milestone}: {state:?}"
    );
}

#[allow(clippy::expect_used)]
#[test]
fn crash_at_every_step_boundary_converges() {
    let Some(database_url) = fixture_database_url() else {
        return;
    };
    let binary = env!("CARGO_BIN_EXE_engine-harness");
    let base = std::env::temp_dir();
    for milestone in MILESTONES {
        // Unique per case: DBOS scopes by app, the scratch holds the
        // shared mock state. The release UUID keys the workflow ID, so
        // re-running the test never resumes a stale record.
        let app = format!("t1-{}", milestone.replace(['/', ' '], "-"));
        let scratch = base.join(&app);
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).expect("scratch dir");
        let release_id = uuid::Uuid::now_v7().to_string();

        // Run 1: `start` with the self-exit at the named boundary. Exit
        // code 9 proves the deterministic kill actually fired.
        let killed = spawn_harness(
            binary,
            &app,
            &scratch,
            &release_id,
            &database_url,
            Some(milestone),
        );
        let code = wait_child(
            killed.expect("spawn killed run"),
            Duration::from_secs(120),
            format!("harness at {milestone}: timed out").as_str(),
        )
        .expect("killed run exits");
        assert_eq!(code, 9, "the kill milestone {milestone} did not fire");

        // Run 2: `hold` — a fresh process boots without starting; DBOS's
        // launch-time recovery offer replays recorded steps (bodies
        // skipped) and drives the release to completion. The driver
        // polls convergence, then stops the holder.
        let mut holder = spawn_harness(binary, &app, &scratch, &release_id, &database_url, None)
            .expect("spawn recovery run");

        assert_convergence(&scratch, milestone);
        let _ = holder.kill();
        let _ = holder.wait();
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
