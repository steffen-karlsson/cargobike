//! The crash harness: kills the process at every built-in
//! action step's boundary, recover, and assert the release converges to
//! the same result with no duplicated side effects.
//!
//! Requires `--features crash-hooks` (the milestone hooks + the mock
//! provider) and `CARGOBIKE_TEST_DATABASE_URL`; skips cleanly otherwise.
//!
//! (Test code: asserts and reports, so the panic/print lints stay off.)

#![cfg(feature = "crash-hooks")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::print_stderr)]

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The built-ins the harness template mounts; a milestone is
/// `after/{environment}/{step_id}` (the boundary right after the step's
/// durable checkpoint attempt).
const MILESTONES: [&str; 4] = [
    "after/stage/edit",
    "after/stage/cr",
    "after/stage/notify",
    "after/stage/labels",
];

async fn fixture_database_url() -> Option<String> {
    cargobike_testsupport::database_url().await
}

/// Spawns one harness process. `start_milestone` runs `start` with the
/// self-exit point; `None` runs `hold` (DBOS's boot-time recovery offer,
/// the spike's E3/E5 pattern). Both streams go to log files in the
/// scratch dir for diagnosis.
#[allow(clippy::expect_used, clippy::too_many_arguments)]
fn spawn_harness(
    binary: &str,
    app: &str,
    scratch: &std::path::Path,
    release_id: &str,
    database_url: &str,
    mode: &str,
    kill_milestone: Option<&str>,
    template_kind: Option<&str>,
) -> Result<std::process::Child, String> {
    let name = mode;
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
    command.arg(mode);
    if let Some(milestone) = kill_milestone {
        command.env("CB_CRASH_AT", milestone);
    }
    if let Some(kind) = template_kind {
        command.arg(kind);
    }
    command.spawn().map_err(|error| format!("spawn: {error}"))
}

/// A child that dies with its test: a panic unwinds past the manual
/// kills, and a leaked holder is worse than a dead test — with the
/// shared DBOS schema it used to adopt other boots' pending workflows
/// (the per-boot schema now fences that too; this guard keeps the
/// process table honest regardless).
struct KillOnDrop(Option<std::process::Child>);

impl KillOnDrop {
    fn at(child: std::process::Child) -> Self {
        Self(Some(child))
    }

    /// Waits for the child's exit code; the deadline force-kills with
    /// an error (the milestone or the recovery failure surfaces
    /// through logs).
    fn wait_exit(&mut self, deadline: Duration, label: &str) -> Result<i32, String> {
        let start = Instant::now();
        loop {
            if let Some(status) = self
                .0
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten())
            {
                return Ok(status.code().unwrap_or(-1));
            }
            if start.elapsed() > deadline {
                return Err(label.to_owned());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The explicit stop (the test's own done-with-it point).
    fn stop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Wipes the app's own rows before a case: the leases table is the
/// shared fixture's LIVE state (a lease outlives its release), and the
/// harness's app names repeat across suite runs — a stale holder from
/// a previous run is exactly the `enter` refusal the flake census
/// kept meeting (the environment terminal-refuses before any action
/// step, so the milestone never fires). The engine's in-process tests
/// wipe the same way (`clear_application_rows`).
#[allow(clippy::expect_used)]
async fn wipe_app_rows(database_url: &str, app: &str) {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(database_url)
        .await
        .expect("wipe pool");
    sqlx::query("DELETE FROM leases WHERE application = $1")
        .bind(app)
        .execute(&pool)
        .await
        .expect("the lease wipe");
    sqlx::query("DELETE FROM releases WHERE application = $1")
        .bind(app)
        .execute(&pool)
        .await
        .expect("the release wipe");
}

/// The mock's state file (the convergence facts across the kill).
fn mock_state(scratch: &std::path::Path) -> Option<String> {
    let mut contents = String::new();
    std::fs::File::open(scratch.join("state.json"))
        .ok()?
        .read_to_string(&mut contents)
        .ok()?;
    Some(contents)
}

/// Polls until the release converged: one branch, one commit, one CR —
/// and nothing else (the no-duplicate-side-effects assertion).
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
        files.keys().any(|key| key.ends_with("manifest.yaml")),
        "the committed edit result is absent after {milestone}: {state:?}"
    );
}

#[allow(clippy::expect_used, clippy::needless_borrow)]
#[tokio::test(flavor = "current_thread")]
async fn crash_at_every_step_boundary_converges() {
    let _the_db = crate::fixture::db_lock().await;
    let Some(database_url) = fixture_database_url().await else {
        return;
    };
    let binary = env!("CARGO_BIN_EXE_engine-harness");
    let base = std::env::temp_dir();
    for milestone in MILESTONES {
        // Unique per case: DBOS scopes by app, the scratch holds the
        // shared mock state. The release UUID keys the workflow ID, so
        // re-running the test never resumes a stale record.
        let app = format!("harness-{}", milestone.replace(['/', ' '], "-"));
        let scratch = base.join(&app);
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).expect("scratch dir");
        let release_id = uuid::Uuid::now_v7().to_string();
        wipe_app_rows(&database_url, &app).await;

        // Run 1: `start` with the self-exit at the named boundary. Exit
        // code 9 proves the deterministic kill actually fired.
        let mut killed = KillOnDrop::at(
            spawn_harness(
                binary,
                &app,
                &scratch,
                &release_id,
                &database_url,
                "start",
                Some(milestone),
                None,
            )
            .expect("spawn killed run"),
        );
        // The killed-run deadline is generous on purpose: under the
        // full workspace's parallel suites the shared fixture Postgres
        // slows the workflow's step commits (the engine suite
        // serialises itself via the fixture's db lock, but the OTHER
        // crates' suites run beside it).
        let code = killed
            .wait_exit(
                Duration::from_secs(300),
                format!("harness at {milestone}: timed out").as_str(),
            )
            .expect("killed run exits");
        assert_eq!(code, 9, "the kill milestone {milestone} did not fire");

        // Run 2: `hold` — a fresh process boots without starting; DBOS's
        // launch-time recovery offer replays recorded steps (the bodies
        // skipped) and drives the release to completion. The driver
        // polls convergence, then stops the holder.
        let mut holder = KillOnDrop::at(
            spawn_harness(
                binary,
                &app,
                &scratch,
                &release_id,
                &database_url,
                "hold",
                None,
                None,
            )
            .expect("spawn recovery run"),
        );

        assert_convergence(&scratch, milestone);
        holder.stop();
        let _ = std::fs::remove_dir_all(&scratch);
    }
}

/// The fork reproducer's parked-wait half (US-7's durable substrate,
/// the audit's "a cross-process kill during a wait"): the process dies
/// WHILE parked in `wait: merge`'s durable recv (no milestone — a raw
/// SIGKILL once the CR is open and the workflow has settled into the
/// wait), the change request merges in the meantime, and the
/// recovering process re-enqueues the interpreter — the recorded steps
/// replay, the recv re-parks, and the RECONCILER's signal (sent into
/// the durable topic while the workflow was dead) wakes it to
/// completion. The convergence is exact: one commit, one CR, the
/// phase `Completed`.
#[allow(clippy::expect_used)]
#[tokio::test(flavor = "current_thread")]
async fn crash_while_parked_in_the_merge_wait_converges() {
    let _the_db = crate::fixture::db_lock().await;
    let Some(database_url) = fixture_database_url().await else {
        return;
    };
    let binary = env!("CARGO_BIN_EXE_engine-harness");
    let app = "harness-wait-kill".to_owned();
    let scratch = std::env::temp_dir().join(&app);
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("scratch dir");
    let release_id = uuid::Uuid::now_v7().to_string();
    wipe_app_rows(&database_url, &app).await;

    // Run 1: the waits template, no crash milestone — the process
    // parks in the merge wait once the CR opens.
    let mut parked = KillOnDrop::at(
        spawn_harness(
            binary,
            &app,
            &scratch,
            &release_id,
            &database_url,
            "start",
            None,
            Some("waits"),
        )
        .expect("spawn parked run"),
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let cr_open = mock_state(&scratch).is_some_and(|text| {
            serde_json::from_str::<serde_json::Value>(&text).is_ok_and(|state| {
                state["change_requests"].as_object().is_some_and(|crs| {
                    crs.values()
                        .any(|summary| summary["state"] == "open" && summary["number"].is_number())
                })
            })
        });
        if cr_open {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the parked run never opened its change request"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Settle: the workflow commits the anchor and parks in the recv
    // within milliseconds of the CR; the grace makes the parked state
    // the kill's truth.
    std::thread::sleep(Duration::from_secs(2));
    // The guard's own kill is the test's mechanism here (the raw
    // SIGKILL, deliberately NOT the milestone hook).
    parked.stop();

    // The merge lands while the workflow is dead: the mock's own file
    // state is the provider's truth, and the killed process holds no
    // share of it.
    let mock = cargobike_engine::mock::MockProvider::at(scratch.to_string_lossy().as_ref());
    let number = serde_json::from_str::<serde_json::Value>(&mock_state(&scratch).expect("state"))
        .expect("state parses")["change_requests"]
        .as_object()
        .expect("the change requests")
        .values()
        .find_map(|summary| summary["number"].as_u64())
        .expect("the CR number");
    mock.mark_merged(number).expect("the merge lands");

    // Run 2: the recovery — the interpreter replays its recorded
    // steps, re-parks in the recv, and the reconciler's sweep (its own
    // killed loop row re-enqueued) sends the merged signal into the
    // durable topic; the wait wakes, verifies, completes.
    let mut holder = KillOnDrop::at(
        spawn_harness(
            binary,
            &app,
            &scratch,
            &release_id,
            &database_url,
            "hold",
            None,
            Some("waits"),
        )
        .expect("spawn recovery run"),
    );

    // Convergence: the mock's no-duplicate facts, then the release's
    // own phase — the wait completed across the kill.
    assert_convergence(&scratch, "the parked-wait kill");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("driver pool");
    let deadline = Instant::now() + Duration::from_secs(150);
    let release_uuid: uuid::Uuid = release_id.parse().expect("the release id");
    loop {
        let phase = sqlx::query_scalar::<_, String>("SELECT phase FROM releases WHERE id = $1")
            .bind(release_uuid)
            .fetch_optional(&pool)
            .await
            .expect("the phase read")
            .unwrap_or_default();
        if phase == "Completed" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the release never completed after the parked-wait kill: phase {phase}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    holder.stop();
    let _ = std::fs::remove_dir_all(&scratch);
}
