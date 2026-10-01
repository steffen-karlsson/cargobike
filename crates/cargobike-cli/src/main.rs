//! The Cargobike CLI binary (PRD §9, tasks 4.5-4.8). This build carries
//! the context machine (`context list`/`use`); the release commands
//! land with 4.6 on the same resolution walk.

use std::path::PathBuf;

use cargobike_cli::config;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cargobike", about = "Talk to a Cargobike release server")]
struct Cli {
    /// The config file (F-124: `CARGOBIKE_CONFIG`,
    /// `~/.config/cargobike/config.yaml`).
    #[arg(long, global = true, env = "CARGOBIKE_CONFIG")]
    config: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Named server contexts (§9.7).
    Context {
        #[command(subcommand)]
        command: ContextCommand,
    },
}

#[derive(Subcommand)]
enum ContextCommand {
    /// Lists the config's contexts (the current one marked).
    List,
    /// Switches the current context (rewrites the config file).
    Use {
        /// The context name to activate.
        name: String,
    },
}

#[allow(clippy::print_stderr)] // the CLI contract: errors on stderr
fn main() {
    std::process::exit(real_main());
}

/// real_main: an i32 return simplifies the error paths (no panics).
#[allow(clippy::print_stderr)]
fn real_main() -> i32 {
    let cli = Cli::parse();
    let path = config::config_path(cli.config, home_dir());
    let config = match config::load(&path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    match cli.command {
        Command::Context { command } => match command {
            ContextCommand::List => {
                list_contexts(&config);
                0
            }
            ContextCommand::Use { name } => use_context(&path, &config, name.as_str()),
        },
    }
}

/// `context list`: the current context marked `*`.
#[allow(clippy::print_stdout)]
fn list_contexts(config: &config::ConfigFile) {
    if config.contexts.is_empty() {
        println!("no contexts in the config file; set CARGOBIKE_URL, or add a contexts: entry");
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

/// `context use <name>`: rewrites the config file's current marker.
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

/// The home directory for the config file; HOME is required (no shim).
#[allow(clippy::print_stderr)]
fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            eprintln!("the CLI needs a HOME directory for the config file's default path");
            std::process::exit(1);
        })
}
