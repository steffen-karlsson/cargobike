//! The Cargobike CLI binary (PRD §9, tasks 4.5-4.8): the context machine
//! plus the release lifecycle (§5.3's commands). No database
//! dependencies; the server's API is the only peer.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use cargobike_cli::client::{CallError, Client};
use cargobike_cli::config::{self, OutputFormat, Resolved};
use cargobike_cli::render;
use clap::{Parser, Subcommand};
use uuid::Uuid;

#[derive(Parser, Clone)]
#[command(name = "cargobike", about = "Talk to a Cargobike release server")]
struct Cli {
    /// The config file (F-124: `CARGOBIKE_CONFIG`,
    /// `~/.config/cargobike/config.yaml`).
    #[arg(long, global = true, env = "CARGOBIKE_CONFIG")]
    config: Option<String>,

    /// The server base URL (F-145: the flag overrides the context).
    #[arg(long, global = true, env = "CARGOBIKE_URL")]
    url: Option<String>,

    /// Bless plain `http://` for development hosts (F-98).
    #[arg(long, global = true)]
    allow_http: bool,

    /// The context to use instead of the file's current one
    /// (`CARGOBIKE_CONTEXT` env).
    #[arg(long, global = true, env = "CARGOBIKE_CONTEXT")]
    context: Option<String>,

    /// The output format (F-145: the flag overrides the defaults).
    #[arg(short = 'o', long, global = true)]
    output: Option<OutputFormat>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Subcommand)]
enum Command {
    /// Named server contexts (§9.7).
    Context {
        #[command(subcommand)]
        command: ContextCommand,
    },
    /// The release lifecycle (§5.3, US-1..US-6).
    Release {
        #[command(subcommand)]
        command: ReleaseCommand,
    },
}

#[derive(Clone, Subcommand)]
enum ContextCommand {
    /// Lists the config's contexts (the current one marked).
    List,
    /// Switches the current context (rewrites the config file).
    Use {
        /// The context name to activate.
        name: String,
    },
}

#[derive(Clone, Subcommand)]
enum ReleaseCommand {
    /// Creates a release (US-1; the server answers 202 or the duplicate's
    /// 200 with the existing release, F-109).
    Create {
        /// The application name (its allowlist decides who may release).
        application: String,
        /// The release version (its template's versioning scheme).
        version: String,
        /// Wait until the release reaches a terminal phase (coarse
        /// polling until 4.7's SSE watch).
        #[arg(long)]
        wait: bool,
        /// The wait's cap (humantime, e.g. `30m`); exit 3 on expiry.
        #[arg(long)]
        timeout: Option<String>,
    },
    /// Lists releases (F-101's cursor paging; newest first).
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
        /// The page size (1-500, F-101).
        #[arg(long, default_value = "50")]
        limit: u32,
    },
    /// Prints the release document.
    Get {
        /// The release ID (UUIDv7).
        id: Uuid,
    },
    /// Cancels a pending release (US-6; the server's cleanup starts).
    Cancel {
        /// The release ID (UUIDv7).
        id: Uuid,
    },
    /// Deletes a terminal release's row (the event log is retained
    /// F-114); terminal-only releases.
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

/// The release verb: resolve the target, then run it under tokio.
fn run_release(command: ReleaseCommand, cli: &Cli) -> Result<i32, (i32, String)> {
    let config = load_config(cli)?;
    let resolved = resolve_target(&config, cli)?;
    let client = Client::new(&resolved).map_err(call_failure)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failure| (12, format!("the runtime refused: {failure}")))?;
    runtime.block_on(release_verb(command, &client))
}

#[allow(clippy::print_stderr)]
fn load_config(cli: &Cli) -> Result<config::ConfigFile, (i32, String)> {
    let path = config::config_path(cli.config.clone(), home_dir());
    config::load(&path).map_err(|failure| (1, failure.0))
}

/// F-145's walk with the `-o` output overriding the defaults.
fn resolve_target(config: &config::ConfigFile, cli: &Cli) -> Result<Resolved, (i32, String)> {
    let resolved = config::resolve(config, cli.url.clone(), cli.allow_http, cli.context.clone())
        .map_err(|failure| (1, failure.0))?;
    Ok(match cli.output {
        Some(format) => Resolved {
            output: format,
            ..resolved
        },
        None => resolved,
    })
}

/// The (exit, message) pair a client failure becomes.
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

/// The verbs (§5.3's shapes: flat bodies; the problem details surfaced).
#[allow(clippy::print_stdout, clippy::print_stderr)]
async fn release_verb(command: ReleaseCommand, client: &Client) -> Result<i32, (i32, String)> {
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
            let rendered = render::documents(OutputFormat::Json, &document)
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
            limit,
        } => {
            let mut query = vec![format!("limit={limit}")];
            if let Some(application) = application {
                query.push(format!("application={application}"));
            }
            if let Some(phase) = phase {
                query.push(format!("phase={phase}"));
            }
            if let Some(version) = version {
                query.push(format!("version={version}"));
            }
            let path = format!("{}?{}", url, query.join("&"));
            let page = client.get_json(&path).await.map_err(call_failure)?;
            let rendered = render::documents(OutputFormat::Table, &page)
                .map_err(|failure| (12, failure.to_string()))?;
            print!("{rendered}");
            Ok(0)
        }
        ReleaseCommand::Get { id } => {
            let document = client
                .get_json(&format!("{url}/{id}"))
                .await
                .map_err(call_failure)?;
            let rendered = render::documents(OutputFormat::Json, &document)
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

// A clippy-friendly shim around std::time::Instant (no clock trait).
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
