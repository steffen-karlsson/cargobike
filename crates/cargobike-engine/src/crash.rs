//! The T1 crash-injection hook (PRD §14.3): a feature-gated,
//! production-inert exit that the harness uses to kill the process at a
//! named boundary — deterministically.
//!
//! Enabled by `--features t1-crash-hooks`; the tests drive it via
//! `CB_T1_KILL_AT=after/{environment}/{step_id}`. Without the feature
//! the hook compiles to nothing.

/// The boundary check; `exit(9)` names the milestone in stderr (the
/// harness's driver looks for it when waiting for the kill).
#[cfg(feature = "t1-crash-hooks")]
pub fn milestone_maybe(environment: &str, step_id: &str) {
    let Some(target) = std::env::var("CB_T1_KILL_AT").ok() else {
        return;
    };
    let milestone = format!("after/{environment}/{step_id}");
    if target == milestone {
        // stderr text of the record (the driver's kill detection).
        eprintln!("CB_T1_KILLED_AT={milestone}");
        std::process::exit(9);
    }
}

/// No-op without the harness feature.
#[cfg(not(feature = "t1-crash-hooks"))]
pub fn milestone_maybe(_environment: &str, _step_id: &str) {}
