//! The egress seam's F-117 suite: the full SSRF set over the real
//! service — the built-in deny ranges refused before any I/O, the
//! allowlist exception onto the private net, the configured deny
//! ADDITIONS, the redirect re-check (every hop passes the same gate),
//! the redirect chain's limit, the resolution PIN (a name's checked
//! addresses are the ones the connect uses — the rebinding defence;
//! a name with one refused record is refused whole), the environment's
//! proxies never consulted, and the config's explicit proxy receiving
//! the request.
//!
//! (Test code: it asserts and reports, so the expect/print lints stay
//! off for this file.) No database: the seam is I/O-local.

#![allow(clippy::expect_used, clippy::print_stderr)]

use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// Builds the service from a config carrying the given egress section.
#[allow(clippy::expect_used)]
fn service(egress_yaml: &str) -> cargobike_server::engine::EgressHttpService {
    // Raw newlines, never line continuations: a continuation eats the
    // next line's leading spaces and the YAML never parses (the
    // suite's standing trap).
    let yaml = format!(
        "server: {{}}\ndatabase: {{ url: postgres://localhost/cargobike }}\nauth: {{}}\nnetwork:\n  egress:\n{egress_yaml}"
    );
    // Unique per call: the binary's tests run in parallel and share
    // nothing (a shared path is a write/read race).
    static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "cb-egress-config-{}-{call}.yaml",
        std::process::id()
    ));
    std::fs::write(&path, yaml).expect("write the config");
    let config = cargobike_server::config::load(Some(&path)).expect("the config parses");
    let _ = std::fs::remove_file(&path);
    cargobike_server::engine::EgressHttpService::new(&config).expect("the service builds")
}

/// One raw HTTP responder: every accepted connection is read once and
/// answered with the canned bytes, and the request text is recorded.
/// The tests' targets are tiny canned responses (a body, a redirect,
/// or the proxy's echo).
#[allow(clippy::expect_used)]
async fn raw_server(canned: String) -> (std::net::SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the listener binds");
    raw_server_on(listener, canned).await
}

/// [`raw_server`] over an ALREADY-BOUND listener (a self-referencing
/// canned response needs the port before the bytes exist).
async fn raw_server_on(
    listener: tokio::net::TcpListener,
    canned: String,
) -> (std::net::SocketAddr, Arc<Mutex<Vec<String>>>) {
    let address = listener.local_addr().expect("the local address");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_writer = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let canned = canned.clone();
            let seen = Arc::clone(&seen_writer);
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 8192];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buffer[..read]).to_string();
                if let Ok(mut seen) = seen.lock() {
                    seen.push(text);
                }
                socket.write_all(canned.as_bytes()).await.ok();
                socket.shutdown().await.ok();
            });
        }
    });
    (address, seen)
}

/// One request through the service (a plain GET unless a body rides).
#[allow(clippy::expect_used)]
async fn send(
    service: &cargobike_server::engine::EgressHttpService,
    url: &str,
) -> Result<cargobike_core::step::HttpResponse, cargobike_core::provider::HttpError> {
    use cargobike_core::step::HttpService as _;
    service
        .send(cargobike_core::step::HttpRequest {
            url: url.to_owned(),
            method: "GET".to_owned(),
            headers: vec![],
            body: None,
        })
        .await
}

const BODY: &str = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nHELLO";

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::expect_used, clippy::print_stderr)]
async fn the_builtin_ranges_refuse_before_any_io() {
    let service = service("    deny:\n");
    for host in [
        "http://127.0.0.1:80/",
        "http://10.0.0.1/",
        "http://169.254.1.1/",
        "http://172.16.0.1/",
        "http://192.168.1.1/",
        "http://[::1]:80/",
        "http://[fc00::1]/",
    ] {
        let refused = send(&service, host).await;
        assert!(
            matches!(
                refused,
                Err(cargobike_core::provider::HttpError::Forbidden(_))
            ),
            "the built-in range must refuse {host} before any I/O: {refused:?}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::expect_used, clippy::print_stderr)]
async fn the_allowlist_exception_serves_the_private_net() {
    // Loopback is the private net here: the test's own listener is on
    // it, which is exactly the administrator's exception case.
    let service = service("    allow: [\"127.0.0.0/8\"]\n");
    let (address, _seen) = raw_server(BODY.to_owned()).await;
    let response = send(&service, &format!("http://{address}/"))
        .await
        .expect("the allowed address serves");
    assert_eq!(response.status, 200);
    assert_eq!(String::from_utf8_lossy(&response.body), "HELLO");
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::expect_used, clippy::print_stderr)]
async fn the_configured_deny_extends_the_builtins() {
    // 9.9.9.0/24 is public range — only the configured addition
    // refuses it.
    let service = service("    deny: [\"9.9.9.0/24\"]\n");
    let refused = send(&service, "http://9.9.9.9/").await;
    assert!(
        matches!(
            refused,
            Err(cargobike_core::provider::HttpError::Forbidden(ref used))
                if used.contains("9.9.9.9")
        ),
        "the configured deny must extend the built-ins: {refused:?}"
    );
    // And the addition never REMOVES the built-ins.
    let builtin = send(&service, "http://127.0.0.1/").await;
    assert!(
        matches!(
            builtin,
            Err(cargobike_core::provider::HttpError::Forbidden(_))
        ),
        "the built-ins are never removable by config: {builtin:?}"
    );
}

#[test]
#[allow(clippy::expect_used, clippy::print_stderr)]
fn a_malformed_cidr_refuses_the_build() {
    let yaml = "server: {}\ndatabase: { url: postgres://localhost/cargobike }\nauth: {}\nnetwork:\n  egress:\n    deny: [\"not-a-cidr\"]\n";
    let path =
        std::env::temp_dir().join(format!("cb-egress-bad-config-{}.yaml", std::process::id()));
    std::fs::write(&path, yaml).expect("write the config");
    let config = cargobike_server::config::load(Some(&path)).expect("the config parses");
    let _ = std::fs::remove_file(&path);
    let refused = match cargobike_server::engine::EgressHttpService::new(&config) {
        Err(refused) => refused,
        Ok(_) => panic!("the malformed deny entry must refuse the build"),
    };
    assert!(
        refused.contains("not-a-cidr"),
        "the malformed entry is named: {refused}"
    );
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::expect_used, clippy::print_stderr)]
async fn a_redirect_to_a_blocked_address_is_refused() {
    // The first hop is allowed (the exception covers exactly one
    // address); the redirect points at a blocked private address —
    // the re-check refuses it BEFORE any connect.
    let service = service("    allow: [\"127.0.0.1/32\"]\n");
    let (address, _seen) = raw_server(
        "HTTP/1.1 302 Found\r\nLocation: http://10.0.0.1/never\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_owned(),
    )
    .await;
    let refused = send(&service, &format!("http://{address}/")).await;
    assert!(
        matches!(
            refused,
            Err(cargobike_core::provider::HttpError::Forbidden(ref used))
                if used.contains("10.0.0.1")
        ),
        "the redirect's hop must pass the same gate: {refused:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::expect_used, clippy::print_stderr)]
async fn a_redirect_chain_lands_on_the_final_body() {
    let service = service("    allow: [\"127.0.0.0/8\"]\n");
    let (final_address, _final_seen) = raw_server(BODY.to_owned()).await;
    let (hop_address, _hop_seen) = raw_server(format!(
        "HTTP/1.1 302 Found\r\nLocation: http://{final_address}/landed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    ))
    .await;
    let response = send(&service, &format!("http://{hop_address}/")).await;
    let response = response.expect("the allowed chain lands");
    assert_eq!(response.status, 200);
    assert_eq!(String::from_utf8_lossy(&response.body), "HELLO");
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::expect_used, clippy::print_stderr)]
async fn a_redirect_loop_exceeds_the_chain_limit() {
    let service = service("    allow: [\"127.0.0.0/8\"]\n");
    // Bind first: the loop's Location names its own port (the
    // listener stays — every redirected hop lands back on it).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the loop's listener binds");
    let address = listener.local_addr().expect("the local address");
    let (address, _seen) = raw_server_on(
        listener,
        format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{address}/loop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
    )
    .await;
    let refused = send(&service, &format!("http://{address}/")).await;
    assert!(
        matches!(
            refused,
            Err(cargobike_core::provider::HttpError::Other(ref error))
                if error.contains("redirect chain")
        ),
        "a loop is an error, not a hang: {refused:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::expect_used, clippy::print_stderr)]
async fn the_pinned_resolution_serves_a_name_and_refuses_mixed_records() {
    // `localhost` resolves to BOTH 127.0.0.1 and ::1: with both
    // allowed the name serves through the PIN (the checked addresses
    // are the connect's); with ::1 off the allowlist the whole name is
    // refused — one refused record refuses the name (mixed records
    // are the rebinding attack's shape).
    let seam = service("    allow: [\"127.0.0.0/8\", \"::1/128\"]\n");
    let (address, _seen) = raw_server(BODY.to_owned()).await;
    let response = send(&seam, &format!("http://localhost:{}/", address.port()))
        .await
        .expect("the pinned name serves");
    assert_eq!(response.status, 200);

    let strict = service("    allow: [\"127.0.0.0/8\"]\n");
    let refused = send(&strict, &format!("http://localhost:{}/", address.port())).await;
    assert!(
        matches!(
            refused,
            Err(cargobike_core::provider::HttpError::Forbidden(_))
        ),
        "one refused record refuses the whole name: {refused:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::expect_used, clippy::print_stderr)]
async fn the_environments_proxies_are_never_consulted() {
    // A dead env proxy would fail every call if reqwest read it; the
    // seam must succeed DIRECTLY (the config's proxy is the only one).
    unsafe {
        std::env::set_var("HTTP_PROXY", "http://127.0.0.1:1");
        std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:1");
    }
    let service = service("    allow: [\"127.0.0.0/8\"]\n");
    let (address, _seen) = raw_server(BODY.to_owned()).await;
    let result = send(&service, &format!("http://{address}/")).await;
    unsafe {
        std::env::remove_var("HTTP_PROXY");
        std::env::remove_var("HTTPS_PROXY");
    }
    let response = result.expect("the env's proxy is never consulted");
    assert_eq!(response.status, 200);
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::expect_used, clippy::print_stderr)]
async fn the_configured_proxy_receives_the_request() {
    // The proxy is the local raw server: it must see the ABSOLUTE-URI
    // request line (the proxied form) and its answer is the step's.
    let (proxy_address, seen) = raw_server(BODY.to_owned()).await;
    let service = service(&format!(
        "    allow: [\"127.0.0.0/8\"]\n    proxy: \"http://{proxy_address}\"\n"
    ));
    // The requested host: an allowed loopback address (the deny-list
    // applies to it BEFORE the proxying); NO listener there — the
    // proxy answers.
    let response = send(&service, "http://127.0.0.1:9/x").await;
    let response = response.expect("the proxied call answers");
    assert_eq!(response.status, 200);
    let seen = seen.lock().expect("the seen lock").clone();
    assert!(
        seen.first()
            .is_some_and(|request| request.starts_with("GET http://127.0.0.1:9/x")),
        "the proxy receives the absolute-URI request: {seen:?}"
    );
}
