//! The crash-injection hook: a feature-gated,
//! production-inert exit that the harness uses to kill the process at a
//! named boundary — deterministically.
//!
//! Enabled by `--features crash-hooks`; the tests drive it via
//! `CB_CRASH_AT=after/{environment}/{step_id}`. Without the feature
//! the hook compiles to nothing.

/// The boundary check; `exit(9)` names the milestone in stderr (the
/// harness's driver looks for it when waiting for the kill).
#[cfg(feature = "crash-hooks")]
#[allow(clippy::print_stderr)]
pub fn milestone_maybe(environment: &str, step_id: &str) {
    let Some(target) = std::env::var("CB_CRASH_AT").ok() else {
        return;
    };
    let milestone = format!("after/{environment}/{step_id}");
    if target == milestone {
        // stderr text of the record (the driver's kill detection).
        eprintln!("CB_CRASHED_AT={milestone}");
        // Settle: the previous steps' record-writes may still be
        // committing; exit only after a grace so the recovery's
        // replay reads every recorded result (the flake's census).
        std::thread::sleep(std::time::Duration::from_secs(2));
        std::process::exit(9);
    }
}

/// No-op without the harness feature.
#[cfg(not(feature = "crash-hooks"))]
pub fn milestone_maybe(_environment: &str, _step_id: &str) {}
