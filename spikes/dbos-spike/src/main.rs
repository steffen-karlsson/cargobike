//! The waiter process: boots DBOS for an app, optionally starts the
//! experiment workflow, and stays alive until killed. Both `start` and
//! `hold` register the same workflow before launch, which is what makes a
//! killed process's pending work recoverable by the replacement (F-21).
//!
//! Usage:
//! - `waiter start <app> <url> <workflow-id> <scratch-dir> <mode> <sleep-secs>`
//! - `waiter hold <app> <url>`

use spike_lib::SpikeInput;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("start") => start(
            args.get(2).expect("app"),
            args.get(3).expect("url"),
            args.get(4).expect("workflow-id"),
            args.get(5).expect("scratch"),
            args.get(6).expect("mode"),
            args.get(7).and_then(|s| s.parse().ok()).unwrap_or(60.0),
        ),
        Some("hold") => hold(args.get(2).expect("app"), args.get(3).expect("url")),
        other => anyhow::bail!("usage: waiter start|hold ... (got {other:?})"),
    }
}

fn start(
    app: &str,
    url: &str,
    workflow_id: &str,
    scratch: &str,
    mode: &str,
    sleep_seconds: f64,
) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let (_, reference) = spike_lib::boot(app, url).await
                .map_err(|error| anyhow::anyhow!("failed to boot: {error}"))?;
            {
                let input = SpikeInput {
                    mode: mode.to_owned(),
                    scratch: scratch.to_owned(),
                    params: serde_json::json!({ "sleep_seconds": sleep_seconds }),
                };
                let options = dbos::StartOptions {
                    workflow_id: Some(workflow_id),
                    ..dbos::StartOptions::default()
                };
                let _handle = reference
                    .start_with(input, options)
                    .await
                    .map_err(|error| anyhow::anyhow!("failed to start {workflow_id}: {error}"))?;
                // Hold the process alive: the driving test kills it.
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                }
                #[allow(unreachable_code)]
                {
                    anyhow::Ok(())
                }
            }
        })
}

fn hold(app: &str, url: &str) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            match spike_lib::boot(app, url).await {
                Ok(instance) => {
                    // Recovered executions run on this executor; hold until killed.
                    let _ = instance;
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                    }
                }
                Err(error) => anyhow::bail!("failed to boot: {error}"),
            }
        })
}

