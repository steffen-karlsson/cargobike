//! Cargobike's shared test infrastructure — **dev-only**, never a
//! product dependency (the workspace's rule: this crate is a
//! `dev-dependencies` target of the DB-backed test suites, PRD 1.4/2.9).
//!
//! The one job: make DB-backed tests self-sufficient. A bare
//! `cargo test` must not *quietly* zero integration coverage — when
//! `CARGOBIKE_TEST_DATABASE_URL` is unset, the suite provisions a
//! throwaway Postgres container (one per test process; the suite's
//! binaries share nothing by design), and only when even that fails
//! (no Docker on the machine) do the tests skip, with the reason
//! printed.
//!
//! Precedence: `CARGOBIKE_TEST_DATABASE_URL` (CI provides one) always
//! wins; the container is the local-developer fallback.
//!
//! (Test infrastructure: the skip reason prints to stderr — that's the
//! point of this crate — so the print lints stay off for it.)

#![allow(clippy::print_stderr)]

/// The test suites' database URL: the exported
/// `CARGOBIKE_TEST_DATABASE_URL` when present, else ONE throwaway
/// Postgres container per process. `None` = give up (skip, printing
/// the reason) — only when neither the variable nor a container is
/// available.
pub async fn database_url() -> Option<String> {
    if let Ok(url) = std::env::var("CARGOBIKE_TEST_DATABASE_URL") {
        if !url.is_empty() {
            return Some(url);
        }
    }
    // The container work runs on the blocking pool (a fresh thread:
    // a nested runtime cannot start on a runtime thread), once per
    // process.
    static CONTAINER_URL: tokio::sync::OnceCell<Option<String>> =
        tokio::sync::OnceCell::const_new();
    CONTAINER_URL
        .get_or_init(|| async {
            match tokio::task::spawn_blocking(provision_container).await {
                Ok(Ok(url)) => Some(url),
                Ok(Err(reason)) => {
                    eprintln!(
                        "skipping: no CARGOBIKE_TEST_DATABASE_URL and no container ({reason})"
                    );
                    None
                }
                Err(failure) => {
                    eprintln!("skipping: the container provisioning task failed: {failure}");
                    None
                }
            }
        })
        .await
        .clone()
}

/// Starts the throwaway Postgres 17 container (the spike script's
/// version parity: the same major the fixture DB runs). The container
/// lives for the process's lifetime — the suite's end tears it down.
#[allow(clippy::unwrap_in_result)]
fn provision_container() -> Result<String, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|failure| format!("the container runtime never built: {failure}"))?;
    runtime.block_on(async {
        use testcontainers::ImageExt;
        use testcontainers::runners::AsyncRunner;
        let container = testcontainers_modules::postgres::Postgres::default()
            .with_tag("17-alpine")
            .start()
            .await
            .map_err(|failure| format!("the postgres container never started: {failure}"))?;
        let port = container
            .get_host_port_ipv4(5432)
            .await
            .map_err(|failure| format!("the container's port never mapped: {failure}"))?;
        // The container outlives the borrow: the handle is LEAKED so
        // its Drop (a `docker rm`) never runs — the process's exit is
        // the container's lifecycle (Docker reaps it with the
        // `--rm`-less run via the testcontainers reaper, or the
        // machine's own teardown). A leak is the deliberate trade:
        // the tests need the database for the process's whole life.
        std::mem::forget(container);
        let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
        wait_ready(&url).await?;
        Ok(url)
    })
}

/// Waits until Postgres answers `SELECT 1` (the image's own readiness
/// probe covers the server start; this covers the socket accepting).
async fn wait_ready(url: &str) -> Result<(), String> {
    for _ in 0..60 {
        if let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(1))
            .connect(url)
            .await
        {
            if sqlx::query("SELECT 1").execute(&pool).await.is_ok() {
                return Ok(());
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    Err("postgres never answered SELECT 1".to_owned())
}
