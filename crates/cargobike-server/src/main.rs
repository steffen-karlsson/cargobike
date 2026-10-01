//! The `cargobike-server` entry point: clap args (..), logging
//! init , and `axum::serve` on the configured listener.

use clap::Parser;

/// Server arguments; every flag maps to a `CARGOBIKE_SERVER_*` variable .
#[derive(Parser, Debug)]
#[command(
    name = "cargobike-server",
    version,
    about = "Cargobike release orchestration server"
)]
pub struct ServerArgs {
    /// Config file path (; default /etc/cargobike/config.yaml).
    #[arg(
        long,
        env = "CARGOBIKE_SERVER_CONFIG",
        default_value = "/etc/cargobike/config.yaml"
    )]
    pub config: String,
    /// Bind-address override.
    #[arg(long, hide_env_values = true)]
    pub listen: Option<String>,
    /// Public-URL override.
    #[arg(long, hide_env_values = true)]
    pub public_url: Option<String>,
}

fn main() {
    // Pre-logging bootstrap only: no tracing yet, so fallback writes go to
    // stderr by hand. Everything else logs .
    let mut args: Vec<String> = std::env::args().collect();
    if args
        .get(1)
        .map(|command| command == "hash-api-key")
        .unwrap_or(false)
    {
        let _ = args.remove(1);
        std::process::exit(hash_api_key());
    }
    let args = ServerArgs::parse();
    #[allow(clippy::expect_used)]
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime must build");
    runtime.block_on(run(args));
}

/// reads a plaintext key on stdin (the never argv) and prints an
/// argon2id hash for the `auth.api_keys[].hash` field of the config.
#[allow(clippy::print_stderr, clippy::print_stdout, unused_imports)]
fn hash_api_key() -> i32 {
    use argon2::password_hash::{PasswordHasher, SaltString};
    use rand_core::RngCore as _;
    use std::io::Read as _;
    let mut buffer = String::new();
    std::io::stdin().read_line(&mut buffer).unwrap_or_default();
    let presented = buffer.trim();
    if presented.is_empty() {
        eprintln!("cargobike-server hash-api-key: a key is required on stdin");
        return 2;
    }
    let mut random = [0_u8; 16];
    rand_core::OsRng.fill_bytes(&mut random);
    let salt = match SaltString::encode_b64(&random) {
        Ok(salt) => salt,
        Err(error) => {
            eprintln!("cargobike-server hash-api-key: {error}");
            return 2;
        }
    };
    match argon2::Argon2::default().hash_password(presented.as_bytes(), &salt) {
        Ok(hash) => println!("{hash}"),
        Err(error) => {
            eprintln!("cargobike-server hash-api-key: {error}");
            return 2;
        }
    }
    0
}

async fn run(args: ServerArgs) {
    let config = match cargobike_server::config::load(Some(std::path::Path::new(&args.config))) {
        Ok(config) => config,
        Err(error) => {
            exit_before_logging(&error);
        }
    };
    init_logging(&config);
    let listen = args.listen.unwrap_or_else(|| config.server.listen.clone());
    let router = match cargobike_server::boot(Some(std::path::Path::new(&args.config))).await {
        Ok((router, _state)) => router,
        Err(error) => {
            exit_before_logging(&error);
        }
    };
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .unwrap_or_else(|error| {
            tracing::error!(%error, "bind {listen} failed");
            std::process::exit(2);
        });
    tracing::info!(%listen, "cargobike-server listening");
    if let Err(error) = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    {
        tracing::error!(%error, "serve failed");
        std::process::exit(1);
    }
}

/// The exit path before logging is initialised (the main only).
#[allow(clippy::print_stderr)]
fn exit_before_logging(error: &impl std::fmt::Display) -> ! {
    eprintln!("cargobike-server: {error}");
    std::process::exit(2);
}

/// Doc comment for the logging init .
#[allow(unused)] // rewritten by 2.8b's reload wiring; kept honest for now
fn init_logging(config: &cargobike_server::Config) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(config.logging.level.clone()));
    if config.logging.format == "json" {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}
