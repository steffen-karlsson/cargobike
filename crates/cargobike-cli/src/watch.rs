//! The SSE watch (US-4, F-102): `release watch` and `create --wait`
//! stream the release's event log live and exit with the CI-gateable
//! code matrix — 0 completed (a skipped `--until` environment counts,
//! with a message), 1 failed, 2 canceled/superseded, 3 the CLI's
//! `--timeout` expiry; the client failures keep `client::exit_code`'s
//! 10/11/12. Reconnects resume via `Last-Event-ID` (the server's SSE
//! ids are the event-log sequences; `reqwest-eventsource` replays
//! them on a broken stream), and `indicatif` progress shows only on
//! a TTY (CI logs get plain event lines).

use crate::client::{CallError, Client};
use futures_util::StreamExt;
use std::io::IsTerminal;

/// Why the watch ended with its code (the caller prints it).
pub struct WatchOutcome {
    /// US-4's matrix: 0/1/2/3.
    pub code: i32,
    /// The human line ("" when the exit code alone says it).
    pub message: String,
}

/// Watches `id` until the `--until` environment reaches a terminal
/// phase (or the whole release does, without `--until`), bounded by
/// the CLI-side `--timeout` (exit 3).
pub async fn watch(
    client: &Client,
    id: &str,
    until: Option<&str>,
    timeout: Option<&str>,
) -> Result<WatchOutcome, (i32, String)> {
    let deadline = timeout
        .and_then(|text| humantime::parse_duration(text).ok())
        .map(|cap| tokio::time::Instant::now() + cap);
    let bar = progress_bar(id, until);
    let mut source = reqwest_eventsource::EventSource::new(
        client.event_source(&format!("/api/v1/releases/{id}/watch")),
    )
    .map_err(|failure| (11, failure.to_string()))?;
    loop {
        // The deadline bounds the idle too (the select races the
        // stream's wait): `--timeout` is the CLI's own promise.
        let next = if let Some(deadline) = deadline {
            tokio::select! {
                event = source.next() => event,
                _ = tokio::time::sleep_until(deadline) => {
                    return Ok(WatchOutcome {
                        code: 3,
                        message: "the watch timed out".to_owned(),
                    });
                }
            }
        } else {
            source.next().await
        };
        let event = match next {
            None | Some(Err(reqwest_eventsource::Error::StreamEnded)) => break,
            Some(Ok(reqwest_eventsource::Event::Message(event))) => event,
            Some(Ok(reqwest_eventsource::Event::Open)) => continue,
            Some(Err(failure)) => return Err(stream_failure(&failure)),
        };
        let kind = event.event.clone();
        let data: serde_json::Value = serde_json::from_str(&event.data).unwrap_or_default();
        announce(&bar, &data, &kind);
        // `--until <env>`: the environment's own terminal phase ends
        // the watch (a Skipped counts as success, US-4).
        if let Some(until) = until {
            let environment = data
                .pointer("/data/environment")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if environment == until {
                match kind.as_str() {
                    "environment-completed" => {
                        return Ok(WatchOutcome {
                            code: 0,
                            message: format!("the environment {until} completed"),
                        });
                    }
                    "environment-skipped" => {
                        return Ok(WatchOutcome {
                            code: 0,
                            message: format!(
                                "the environment {until} was skipped (counts as success)"
                            ),
                        });
                    }
                    "environment-failed" => {
                        return Ok(WatchOutcome {
                            code: 1,
                            message: format!("the environment {until} failed"),
                        });
                    }
                    "environment-canceled" | "environment-superseded" => {
                        return Ok(WatchOutcome {
                            code: 2,
                            message: format!("the environment {until} ended {kind}"),
                        });
                    }
                    _ => {}
                }
            }
        }
    }
    // The stream closed on the terminal phase — the document is the
    // rollup's truth (a canceled release's environments never get
    // their own terminal events; the phase text is the answer).
    let document = client
        .get_json(&format!("/api/v1/releases/{id}"))
        .await
        .map_err(call_failure)?;
    let phase = document
        .pointer("/status/phase")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let (code, message) = match phase.as_str() {
        "Completed" => (0, "the release completed".to_owned()),
        "Failed" => (1, "the release failed".to_owned()),
        "Canceled" => (2, "the release was canceled".to_owned()),
        "Superseded" => (2, "the release was superseded".to_owned()),
        other => (12, format!("the release's phase `{other}` is not terminal")),
    };
    Ok(WatchOutcome { code, message })
}

/// The client failure's (code, message) — the shared mapping.
fn call_failure(failure: CallError) -> (i32, String) {
    (crate::client::exit_code(&failure), failure.to_string())
}

/// The stream failure's (code, message): 10 for the auth refusal
/// (401/403), 11 for the unreachable server, 12 for anything else
/// the server answered (US-4's client column).
fn stream_failure(failure: &reqwest_eventsource::Error) -> (i32, String) {
    match failure {
        reqwest_eventsource::Error::Transport(transport) => match transport.status() {
            Some(status) if status.as_u16() == 401 || status.as_u16() == 403 => {
                (10, format!("the watch's stream was refused: {transport}"))
            }
            Some(_) => (12, format!("the watch's stream was refused: {transport}")),
            None => (11, format!("the server is unreachable: {transport}")),
        },
        reqwest_eventsource::Error::InvalidStatusCode(status, _)
            if status.as_u16() == 401 || status.as_u16() == 403 =>
        {
            (
                10,
                "the watch's stream was refused: the bearer was not accepted".to_owned(),
            )
        }
        reqwest_eventsource::Error::InvalidStatusCode(status, _) => {
            (12, format!("the watch's stream was refused: {status}"))
        }
        reqwest_eventsource::Error::InvalidContentType(_, _) => (
            12,
            "the server's watch endpoint did not answer an event stream".to_owned(),
        ),
        other => (12, format!("the watch's stream failed: {other}")),
    }
}

/// The progress display: a spinner with the last event on a TTY, the
/// plain event lines everywhere else (CI logs stay readable).
fn progress_bar(id: &str, until: Option<&str>) -> Option<indicatif::ProgressBar> {
    if !std::io::stderr().is_terminal() {
        return None;
    }
    let bar = indicatif::ProgressBar::new_spinner();
    bar.set_style(
        indicatif::ProgressStyle::with_template("{spinner} {msg}")
            .unwrap_or_else(|_| indicatif::ProgressStyle::default_spinner()),
    );
    bar.set_message(format!(
        "watching {id}{}",
        until
            .map(|env| format!(" (until {env})"))
            .unwrap_or_default()
    ));
    Some(bar)
}

/// One event line: the bar prints above itself on a TTY (the spinner
/// keeps the bottom line), plain stdout otherwise.
#[allow(clippy::print_stdout)] // the watch's output IS the command's purpose
fn announce(bar: &Option<indicatif::ProgressBar>, data: &serde_json::Value, kind: &str) {
    use std::fmt::Write as _;
    let mut line = String::new();
    let at = data
        .get("at")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let reason = data
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let environment = data
        .pointer("/data/environment")
        .and_then(serde_json::Value::as_str);
    if let Some(environment) = environment {
        let _ = write!(line, "{at} {kind} [{environment}]: {reason}");
    } else {
        let _ = write!(line, "{at} {kind}: {reason}");
    }
    match bar {
        Some(bar) => bar.println(line),
        None => println!("{line}"),
    }
}
