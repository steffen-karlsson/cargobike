//! The DBOS spike (PRD task 1.1): prototype the durable-execution
//! substrate the engine builds on.
//!
//! Verifies steps with serde args/returns, topics + idempotency keys on
//! signals, durable sleep across a killed process, fork from LastFailure,
//! and executor recovery. Results go to `docs/spike-dbos.md`; this crate
//! is NOT part of the workspace.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// App version pinned for both processes of a recovery experiment (F-22:
/// only executors running the starting version recover the workflow).
/// Version NAME is global to the database, so every experiment gets its own.
pub fn app_version(app: &str) -> String {
    format!("{app}-v")
}

/// The workflow name the interpreter registers, mirrored here (F-15).
pub const WORKFLOW_NAME: &str = "spike.interpret.v1";

/// Host port the spike's postgres listens on.
pub const PG_PORT: u16 = 54329;

/// Error the spike workflows fail with; `DurableError` is a blanket impl
/// for `Serialize + DeserializeOwned + Error + 'static`, so the derives
/// below are all it takes.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("spike failure: {0}")]
pub struct SpikeError(pub String);

/// Workflow argument: exactly one value, serde round-tripped (F-16).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpikeInput {
    /// Which experiment the workflow runs.
    pub mode: String,
    /// Directory for marker files (cross-process side-effect ledger).
    pub scratch: String,
    /// Experiment parameters.
    pub params: Value,
}

/// Workflow result (F-16).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SpikeResult {
    /// Observed marker state.
    pub markers: Value,
}

/// Registers the spike workflow; registration only works before
/// `launch()` (F-15). The closure's future resolves to the engine's own
/// error envelope: `dbos::Result<R, E>` = `Result<R, Error<E>>`.
pub fn register_spike_workflow(
    instance: &dbos::DBOS,
) -> dbos::Result<dbos::WorkflowRef<SpikeInput, SpikeResult, SpikeError>> {
    instance.register_workflow(WORKFLOW_NAME, |input: SpikeInput| async move {
        run(input).await
    })
}

/// Runs one step; the step body's error channel pins the app error type.
macro_rules! step_or_fail {
    ($name:expr, $body:expr) => {
        dbos::step::<(), SpikeError, _, _>($name, $body).await?
    };
}

/// The interpreter analogue: a pure function of the inputs and recorded
/// results; each `dbos::step` records what it produced (F-14, A1).
async fn run(input: SpikeInput) -> dbos::Result<SpikeResult, SpikeError> {
    match input.mode.as_str() {
        "counter" | "sleep-restart" | "executor-recovery" => sequence(input).await,
        "await-message" => await_message(input).await,
        "will-fail" => failing(input).await,
        other => Err(dbos::Error::Application(SpikeError(format!("unknown mode {other}")))),
    }
}

/// bump marker step s1, then (`sleep-restart` / `executor-recovery` only)
/// a durable sleep and bump s2, and finish.
async fn sequence(input: SpikeInput) -> dbos::Result<SpikeResult, SpikeError> {
    let scratch = input.scratch.clone();
    step_or_fail!("s1", || async {
        bump(&scratch, "s1");
        dbos::Result::Ok(())
    });

    if input.mode != "counter" {
        let seconds = input
            .params
            .get("sleep_seconds")
            .and_then(Value::as_f64)
            .unwrap_or(60.0);
        dbos::sleep::<SpikeError>(std::time::Duration::from_secs_f64(seconds)).await;
        let scratch2 = scratch.clone();
        step_or_fail!("s2", || async {
            bump(&scratch2, "s2");
            dbos::Result::Ok(())
        });
    }

    dbos::Result::Ok(SpikeResult { markers: json!({ "done": true }) })
}

/// Waits for a signal on the topic in params; reports what it received
/// (F-20: messages confuse different waits when topics are missing).
async fn await_message(input: SpikeInput) -> dbos::Result<SpikeResult, SpikeError> {
    let topic = input
        .params
        .get("topic")
        .and_then(Value::as_str)
        .unwrap_or("p")
        .to_owned();
    let first: Option<Value> =
        dbos::recv::<Value, SpikeError>(Some(topic.as_str()), std::time::Duration::from_secs(30))
            .await?;
    // A redelivery attempt with the same key cannot be seen again.
    let second: Option<Value> =
        dbos::recv::<Value, SpikeError>(Some(topic.as_str()), std::time::Duration::from_secs(10))
            .await?;
    dbos::Result::Ok(SpikeResult {
        markers: json!({ "received": first, "second": second }),
    })
}

/// s1 always succeeds; s2 fails while `failed-once` exists; s3 runs after.
async fn failing(input: SpikeInput) -> dbos::Result<SpikeResult, SpikeError> {
    let scratch = input.scratch.clone();
    step_or_fail!("s1", || async {
        bump(&scratch, "s1");
        dbos::Result::Ok(())
    });

    dbos::step::<(), SpikeError, _, _>("s2", || async {
        bump(&scratch, "s2");
        if marker_path(&scratch, "failed-once").exists() {
            // The app failure: wrapped in the engine envelope so the replay
            // reads back the same error (`Error::Application`, F-16's fidelity).
            dbos::Result::Err(dbos::Error::Application(SpikeError(
                "permanent failure while failed-once exists".to_owned(),
            )))
        } else {
            dbos::Result::Ok(())
        }
    })
    .await?;

    let scratch3 = scratch;
    step_or_fail!("s3", || async {
        bump(&scratch3, "s3");
        dbos::Result::Ok(())
    });

    dbos::Result::Ok(SpikeResult { markers: json!({ "done": true }) })
}

/// Marker helpers are the cross-process stand-in for side effects that
/// must not repeat (A1): a replay that skips a recorded step leaves the
/// count alone.
pub fn bump(scratch: &str, marker: &str) {
    let count = count_marker(scratch, marker).wrapping_add(1);
    write_marker(scratch, marker, count);
}

/// Writes the counter for a marker.
pub fn write_marker(scratch: &str, marker: &str, count: u64) {
    let path = marker_path(scratch, marker);
    std::fs::create_dir_all(path.parent().expect("parent exists")).ok();
    std::fs::write(path, count.to_string()).ok();
}

/// Path of a marker count file.
fn marker_path(scratch: &str, marker: &str) -> PathBuf {
    PathBuf::from(scratch).join(format!("{marker}.count"))
}

/// Reads the counter for a marker.
pub fn count_marker(scratch: &str, marker: &str) -> u64 {
    std::fs::read_to_string(marker_path(scratch, marker))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// All known step counters for the scratch dir as a JSON map.
pub fn counts(scratch: &str) -> Value {
    let mut out = serde_json::Map::new();
    for name in ["s1", "s2", "s3"] {
        if marker_path(scratch, name).exists() {
            out.insert(name.to_owned(), json!(count_marker(scratch, name)));
        }
    }
    Value::Object(out)
}

/// Removes a marker file entirely (used for failure flags).
pub fn clear(scratch: &str, marker: &str) {
    std::fs::remove_file(marker_path(scratch, marker)).ok();
}

/// Boot a DBOS instance for the app, registering the workflow before
/// `launch()` (F-15) and returning the typed `WorkflowRef` for callers.
pub async fn boot(
    app: &str,
    url: &str,
) -> dbos::Result<(
    dbos::DBOS,
    dbos::WorkflowRef<SpikeInput, SpikeResult, SpikeError>,
)> {
    let cfg = dbos::Config {
        app_version: Some(app_version(app)),
        ..dbos::Config::new(app.to_owned(), url.to_owned())
    };
    let instance = dbos::DBOS::new(cfg);
    let workflow = register_spike_workflow(&instance)?;
    instance.launch().await?;
    Ok((instance, workflow))
}
