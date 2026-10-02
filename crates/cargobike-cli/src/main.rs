//! The Cargobike CLI binary: the context machine
//! plus the release lifecycle (theirs commands). No database
//! dependencies; the server's API is the only peer.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use cargobike_cli::client::{CallError, Client};
use cargobike_cli::config::{self, OutputFormat};
use cargobike_cli::render;
use clap::{Parser, Subcommand};
use uuid::Uuid;

#[derive(Parser, Clone)]
#[command(name = "cargobike", about = "Talk to a Cargobike release server")]
struct Cli {
    /// The config file ( `CARGOBIKE_CONFIG`,
    /// `~/.config/cargobike/config.yaml`).
    #[arg(long, global = true, env = "CARGOBIKE_CONFIG")]
    config: Option<String>,

    /// The server base URL (the flag overrides the context).
    #[arg(long, global = true, env = "CARGOBIKE_URL")]
    url: Option<String>,

    /// Bless plain `http://` for development hosts .
    #[arg(long, global = true)]
    allow_http: bool,

    /// The context to use instead of the file's current one
    /// (`CARGOBIKE_CONTEXT` env).
    #[arg(long, global = true, env = "CARGOBIKE_CONTEXT")]
    context: Option<String>,

    /// The output format (the flag overrides the defaults;
    /// `CARGOBIKE_OUTPUT` fills in when the flag is missing).
    #[arg(short = 'o', long, global = true, env = "CARGOBIKE_OUTPUT")]
    output: Option<OutputFormat>,

    /// The auth's type override (`none`, `api-key`, `exec`,
    /// `github-actions`; `CARGOBIKE_AUTH` fills in).
    #[arg(long, global = true, env = "CARGOBIKE_AUTH")]
    auth: Option<String>,

    /// The Actions token's audience override (`CARGOBIKE_AUDIENCE`
    /// fills in; the built-in default is `cargobike`).
    #[arg(long, global = true, env = "CARGOBIKE_AUDIENCE")]
    audience: Option<String>,

    /// A custom CA bundle for the server (GHES and private CAs;
    /// `CARGOBIKE_CA_FILE` fills in; the `~`-prefixed paths join HOME).
    #[arg(long, global = true, env = "CARGOBIKE_CA_FILE")]
    ca_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Subcommand)]
enum Command {
    /// Named server contexts .
    Context {
        #[command(subcommand)]
        command: ContextCommand,
    },
    /// The release lifecycle (, ..).
    Release {
        #[command(subcommand)]
        command: ReleaseCommand,
    },
}

#[derive(Clone, Subcommand)]
enum ContextCommand {
    /// Lists the config's contexts (the current one marked).
    List,
    /// Switches the current context (the rewrites the config file).
    Use {
        /// The context name to activate.
        name: String,
    },
}

#[derive(Clone, Subcommand)]
enum ReleaseCommand {
    /// Creates a release (; the server answers 202 or the duplicate's
    /// 200 with the existing release).
    Create {
        /// The application name (the its allowlist decides who may release).
        application: String,
        /// The release version (the its template's versioning scheme).
        version: String,
        /// Wait until the release reaches a terminal phase (the coarse
        /// polling; the SSE watch replaces this later).
        #[arg(long)]
        wait: bool,
        /// The wait's cap (the humantime, e.g. `30m`); exit 3 on expiry.
        #[arg(long)]
        timeout: Option<String>,
    },
    /// Lists releases (the cursor paging; newest first).
    List {
        /// Filter by the application name.
        #[arg(short = 'a', long)]
        application: Option<String>,
        /// Filter by the phase.
        #[arg(long)]
        phase: Option<String>,
        /// Filter by the version string.
        #[arg(long)]
        version: Option<String>,
        /// Filter to releases updated at-or-after this instant: an RFC
        /// 3339 stamp (2026-10-02T00:00:00Z) or a humantime duration
        /// (24h, 90m) relative to now.
        #[arg(long)]
        since: Option<String>,
        /// The page size (1-500).
        #[arg(long, default_value = "50")]
        limit: u32,
    },
    /// Prints the release document.
    Get {
        /// The release ID (UUIDv7).
        id: Uuid,
    },
    /// Cancels a pending release (; the server's cleanup starts).
    Cancel {
        /// The release ID (UUIDv7).
        id: Uuid,
    },
    /// Deletes a terminal release's row (the event log is retained
    ///); terminal-only releases.
    Delete {
        /// The release ID (UUIDv7).
        id: Uuid,
    },
}

fn main() {
    std::process::exit(real_main());
}

/// The CLI's error paths: 1 config, 2 the verb's vocabulary below.
#[allow(clippy::print_stderr)] // the CLI contract: errors on stderr
fn real_main() -> i32 {
    let cli = Cli::parse();
    let path = config::config_path(cli.config.clone(), home_dir());
    let config = match config::load(&path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    match cli.command.clone() {
        Command::Context { command } => match command {
            ContextCommand::List => {
                list_contexts(&config);
                0
            }
            ContextCommand::Use { name } => use_context(&path, &config, name.as_str()),
        },
        Command::Release { command } => match run_release(command, &cli) {
            Ok(exit_code) => exit_code,
            Err((code, message)) => {
                eprintln!("{message}");
                code
            }
        },
    }
}

/// The release verb: resolve the target, then run it under tokio (the
/// client's construction waits: the Actions' token's exchange is a
/// request).
fn run_release(command: ReleaseCommand, cli: &Cli) -> Result<i32, (i32, String)> {
    let config = load_config(cli)?;
    let overrides = config::Overrides {
        url: cli.url.clone(),
        allow_http: cli.allow_http,
        context: cli.context.clone(),
        auth: cli.auth.clone(),
        audience: cli.audience.clone(),
        ca_file: cli.ca_file.clone(),
        output: cli.output,
    };
    let resolved = config::resolve(&config, &overrides).map_err(|failure| (1, failure.0))?;
    let output = resolved.output;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failure| (12, format!("the runtime refused: {failure}")))?;
    runtime.block_on(async {
        let client = Client::new(&resolved).await.map_err(call_failure)?;
        release_verb(command, &client, output).await
    })
}

#[allow(clippy::print_stderr)]
fn load_config(cli: &Cli) -> Result<config::ConfigFile, (i32, String)> {
    let path = config::config_path(cli.config.clone(), home_dir());
    config::load(&path).map_err(|failure| (1, failure.0))
}

/// The (the exit, message) pair a client failure becomes.
fn call_failure(failure: CallError) -> (i32, String) {
    (exit_code(&failure), failure.to_string())
}

use cargobike_cli::client::exit_code;

/// HOME is the config path's root; the CLI prints and exits when it
/// is missing (the server config in `CARGOBIKE_CONFIG`-hide setups
/// doesn't run this).
#[allow(clippy::print_stderr)]
fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            eprintln!("the CLI needs HOME for the config file's default path");
            std::process::exit(1);
        })
}

/// The query value's minimal percent-encoding (the reserved set; a
/// three-liner keeps `urlencoding` out of the appendix).
fn encode_query_value(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

/// The `--since` arg: an RFC 3339 instant passes through; a duration
/// (humantime, e.g. `24h`, `30m`) subtracts from the current instant.
/// Anything else passes through (the server's RFC 3339 parse refusal
/// surfaces the problem document).
fn encode_since_arg(text: &str) -> String {
    if time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).is_ok() {
        return encode_query_value(text);
    }
    match humantime::parse_duration(text) {
        Ok(span) => {
            let now = time::OffsetDateTime::now_utc();
            let past =
                now.checked_sub(time::Duration::try_from(span).unwrap_or(time::Duration::ZERO));
            let stamp = past
                .unwrap_or(now)
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default();
            encode_query_value(&stamp)
        }
        Err(_) => encode_query_value(text),
    }
}

/// The verbs (theirs shapes: flat bodies; the problem details surfaced);
/// `output` is the resolved format every render obeys.
#[allow(clippy::print_stdout, clippy::print_stderr)]
async fn release_verb(
    command: ReleaseCommand,
    client: &Client,
    output: OutputFormat,
) -> Result<i32, (i32, String)> {
    let url = "/api/v1/releases".to_owned();
    match command {
        ReleaseCommand::Create {
            application,
            version,
            wait,
            timeout,
        } => {
            let body = serde_json::json!({ "application": application, "version": version });
            let document = client.post_json(&url, body).await.map_err(call_failure)?;
            let rendered = render::documents(output, &document)
                .map_err(|failure| (12, failure.to_string()))?;
            print!("{rendered}");
            let Some(id) = document
                .pointer("/metadata/id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
            else {
                return Err((12, "the create answered without an id".to_owned()));
            };
            if !wait {
                return Ok(0);
            }
            poll_until_terminal(client, id.as_str(), timeout).await
        }
        ReleaseCommand::List {
            application,
            phase,
            version,
            since,
            limit,
        } => {
            let mut query = vec![format!("limit={limit}")];
            if let Some(application) = application {
                query.push(format!("application={}", encode_query_value(&application)));
            }
            if let Some(phase) = phase {
                query.push(format!("phase={}", encode_query_value(&phase)));
            }
            if let Some(version) = version {
                query.push(format!("version={}", encode_query_value(&version)));
            }
            if let Some(since) = since {
                query.push(format!("since={}", encode_since_arg(&since)));
            }
            let path = format!("{}?{}", url, query.join("&"));
            let page = client.get_json(&path).await.map_err(call_failure)?;
            let rendered =
                render::documents(output, &page).map_err(|failure| (12, failure.to_string()))?;
            print!("{rendered}");
            Ok(0)
        }
        ReleaseCommand::Get { id } => {
            let document = client
                .get_json(&format!("{url}/{id}"))
                .await
                .map_err(call_failure)?;
            let rendered = render::documents(output, &document)
                .map_err(|failure| (12, failure.to_string()))?;
            print!("{rendered}");
            Ok(0)
        }
        ReleaseCommand::Cancel { id } => {
            client
                .post_json(&format!("{url}/{id}/cancel"), serde_json::json!({}))
                .await
                .map_err(call_failure)?;
            println!("cancel accepted; the drain will run inline");
            Ok(0)
        }
        ReleaseCommand::Delete { id } => {
            client
                .delete(&format!("{url}/{id}"))
                .await
                .map_err(call_failure)?;
            println!("deleted");
            Ok(0)
        }
    }
}

/// The wait: polls every second until a terminal phase.
async fn poll_until_terminal(
    client: &Client,
    id: &str,
    timeout: Option<String>,
) -> Result<i32, (i32, String)> {
    let deadline: Option<Instant> = timeout
        .as_deref()
        .and_then(|text| humantime::parse_duration(text).ok())
        .map(|cap| current_time() + cap);
    let path = format!("/api/v1/releases/{id}");
    loop {
        let document = client.get_json(&path).await.map_err(call_failure)?;
        let phase = document
            .pointer("/status/phase")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        match phase.as_str() {
            "Completed" => return Ok(0),
            "Failed" => return Ok(1),
            "Canceled" | "Superseded" => return Ok(2),
            _ => {}
        }
        if deadline.as_ref().is_some_and(|due| &current_time() >= due) {
            return Ok(3);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// A clippy-friendly shim around std::time::Instant (the no clock trait).
fn current_time() -> Instant {
    Instant::now()
}

// ---- context commands -------------------------------------------------------

/// `context list`: the current context marked `*`.
#[allow(clippy::print_stdout, clippy::print_stderr)]
fn list_contexts(config: &config::ConfigFile) {
    if config.contexts.is_empty() {
        println!("no contexts in the config file; set CARGOBIKE_URL, or add contexts:");
        return;
    }
    for context in &config.contexts {
        let marker = config
            .current_context
            .as_deref()
            .is_some_and(|name| name == context.name);
        let marker = if marker { '*' } else { ' ' };
        println!("{marker} {} {}", context.name, context.url);
    }
}

/// `context use <name>`: rewrites the file; exit 2 on an unknown name.
#[allow(clippy::print_stderr, clippy::print_stdout)]
fn use_context(path: &PathBuf, config: &config::ConfigFile, name: &str) -> i32 {
    if !config.contexts.iter().any(|context| context.name == name) {
        eprintln!("no context named `{name}`; see `cargobike context list`");
        return 2;
    }
    let mut updated = config.clone();
    updated.current_context = Some(name.to_owned());
    if let Err(refusal) = config::save(path, &updated) {
        eprintln!("{refusal}");
        return 3;
    }
    println!("now using the `{name}` context");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_query_value_percent_encodes_the_reserved_set() {
        assert_eq!(encode_query_value("my-service"), "my-service");
        assert_eq!(
            encode_query_value("2026-10-02T00:00:00Z"),
            "2026-10-02T00%3A00%3A00Z"
        );
        assert_eq!(encode_query_value("a b&c=d"), "a%20b%26c%3Dd");
    }

    #[test]
    fn test_since_args_parse_before_the_request() {
        // RFC 3339 passes through verbatim.
        assert_eq!(
            encode_since_arg("2026-10-02T00:00:00Z"),
            "2026-10-02T00%3A00%3A00Z"
        );
        // A humantime duration becomes a parseable RFC 3339 instant.
        let encoded = encode_since_arg("24h");
        let decoded = encoded
            .replace("%3A", ":")
            .replace("%2B", "+")
            .replace("%2D", "-");
        time::OffsetDateTime::parse(&decoded, &time::format_description::well_known::Rfc3339)
            .expect("a duration since yields an RFC 3339 stamp");
        // Garbage passes through (the server's refusal is the surface).
        assert_eq!(encode_since_arg("not-a-time"), "not-a-time");
    }

    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::print_stderr)]
    async fn test_the_list_sends_the_since_query() {
        use cargobike_cli::client::Client;
        use cargobike_cli::config::{AuthConfig, Resolved};
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/releases"))
            .and(query_param("since", "2026-10-02T00:00:00Z"))
            .respond_with(|_: &wiremock::Request| {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "items": [],
                    "cursor": null
                }))
            })
            .expect(1)
            .mount(&server)
            .await;

        let resolved = Resolved {
            url: server.uri(),
            auth: AuthConfig::None,
            ca_file: None,
            output: cargobike_cli::config::OutputFormat::Json,
        };
        let client = Client::new(&resolved).await.expect("the client builds");
        let page = client
            .get_json(&format!(
                "/api/v1/releases?limit=50&since={}",
                encode_since_arg("2026-10-02T00:00:00Z")
            ))
            .await
            .expect("the list");
        assert_eq!(page["items"], serde_json::json!([]));
    }
}
