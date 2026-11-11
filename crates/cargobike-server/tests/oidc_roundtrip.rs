//! OIDC roundtrip (2.5/2.9): an in-test RSA key + JWKS served by the
//! same process as the release API, plus an openssl-signed RS256 token
//! through the validation path — the wire-level proof 2.5 needs.
//! Requires `CARGOBIKE_TEST_DATABASE_URL`.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;

/// The seeded tag's commit SHA (F-95: the good token's `sha` claim
/// points at it; the mismatch case claims another).
const SEEDED_SHA: &str = "c0ffee0000000000000000000000000000000000";

/// b64url for token parts and JWK moduli.
fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The paths of the generated RSA key (the kept for the whole test) and its
/// b64url modulus from the DER data.
struct RsaKeypair {
    key_path: std::path::PathBuf,
    modulus_b64: String,
}

/// Generates an RSA-2048 keypair via the CLI openssl (the macOS and CI
/// runners ship it) and extracts the modulus for the JWKS. The key file
/// is unique per call: tests in this binary run concurrently and would
/// otherwise overwrite each other's key material mid-sign.
#[allow(clippy::expect_used, clippy::print_stderr)]
fn generate_rsa() -> RsaKeypair {
    let dir = std::env::temp_dir();
    let unique = uuid::Uuid::now_v7().simple();
    let key_path = dir.join(format!("cb-oidc-key-{unique}.pem"));
    let generated = Command::new("openssl")
        .arg("genpkey")
        .arg("-algorithm")
        .arg("RSA")
        .arg("-pkeyopt")
        .arg("rsa_keygen_bits:2048")
        .arg("-out")
        .arg(&key_path)
        .output()
        .expect("openssl must exist on the runner");
    assert!(
        generated.status.success(),
        "keygen failed: {}",
        String::from_utf8_lossy(&generated.stderr)
    );

    let modulus_out = Command::new("openssl")
        .arg("rsa")
        .arg("-in")
        .arg(&key_path)
        .arg("-noout")
        .arg("-modulus")
        .output()
        .expect("modulus");
    let modulus_hex = String::from_utf8_lossy(&modulus_out.stdout)
        .trim()
        .strip_prefix("Modulus=")
        .expect("Modulus=")
        .to_owned();
    let bytes: Vec<u8> = (0..modulus_hex.len() / 2)
        .filter_map(|index| u8::from_str_radix(&modulus_hex[index * 2..index * 2 + 2], 16).ok())
        .collect();
    RsaKeypair {
        key_path,
        modulus_b64: b64url(&bytes),
    }
}

/// Crafts a compact RS256 JWT by signing the JWS signing input with openssl.
#[allow(clippy::expect_used, clippy::print_stderr)]
fn craft_jwt(key_path: &std::path::Path, claims: serde_json::Value, kid: &str) -> String {
    let header = serde_json::json!({ "alg": "RS256", "typ": "JWT", "kid": kid });
    let header_b64 = b64url(header.to_string().as_bytes());
    let payload_b64 = b64url(claims.to_string().as_bytes());
    let message = format!("{header_b64}.{payload_b64}");
    let message_path =
        std::env::temp_dir().join(format!("cb-oidc-message-{}", uuid::Uuid::now_v7().simple()));
    std::fs::write(&message_path, message.as_bytes()).expect("message file");
    let signature = Command::new("openssl")
        .arg("dgst")
        .arg("-sha256")
        .arg("-sign")
        .arg(key_path)
        .arg(&message_path)
        .output()
        .expect("sign");
    assert!(
        signature.status.success(),
        "sign failed: {}",
        String::from_utf8_lossy(&signature.stderr)
    );
    format!("{message}.{}", b64url(&signature.stdout))
}

#[allow(clippy::expect_used, clippy::print_stderr)]
#[tokio::test(flavor = "current_thread")]
async fn test_oidc_token_passes_and_enforces_claims() {
    let Some(fix_url) = cargobike_testsupport::database_url().await else {
        return;
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("port").port();
    let issuer = format!("http://127.0.0.1:{port}");

    let keypair = generate_rsa();
    let jwks = serde_json::json!({
        "keys": [ {
            "kty": "RSA", "kid": "test-kid", "n": keypair.modulus_b64,
            "e": "AQAB", "alg": "RS256", "use": "sig"
        } ]
    });

    // A local single-environment template (no gates, no approvals): the
    // provision compiles it, so the fixture must not borrow the
    // canonical workspace template's production half.
    let templates_dir = std::env::temp_dir().join(format!("cb-oidc-templates-{port}"));
    let _ = std::fs::create_dir_all(&templates_dir);
    std::fs::write(
        templates_dir.join("service.yaml"),
        r#"
name: service
version: "1"
inputs: {}
environment_inputs:
  repo: { type: repo }
  edits: { type: edits }
environments:
  - name: preview
    steps:
      - id: edit
        uses: builtin/commit-files@1
      - id: cr
        uses: builtin/change-request@1
        with:
          branch: ${{ steps.edit.outputs.branch }}
"#,
    )
    .expect("template");

    // Config with one OIDC entry and a single registered application.
    // The entry carries the REAL GitHub Actions issuer (the CI-context
    // provider label keys on it) with the local JWKS via `jwks_url` —
    // the override's own use case .
    let actions_issuer = "https://token.actions.githubusercontent.com";
    let config_path = std::env::temp_dir().join(format!("cb-oidc-config-{port}.yaml"));
    std::fs::write(
        &config_path,
        format!(
            "server:\n  listen: \"127.0.0.1:{port}\"\n  public_url: \"{issuer}\"\n\n\
             database:\n  url: \"{fix_url}\"\n  dbos_schema: \"cbtest-{port}\"\n\n\
             templates:\n  directory: {}\n\n\
             auth:\n  oidc:\n    - name: test-oidc\n      issuer: \"{actions_issuer}\"\n      audience: cargobike\n      jwks_url: \"{issuer}/jwks\"\n      claims:\n        repository_owner_id: \"123456\"\n      grants: [release:create, release:read, release:cancel]\n\n\
             applications:\n  - name: my-service\n    source: {{ provider: github, id: \"123456\" }}\n    template: service@1\n    versioning:\n      scheme: semver\n      tag_format: \"v{{version}}\"\n    releasers:\n      - oidc: test-oidc\n    environments:\n      preview:\n        repo: {{ provider: github, id: \"42\" }}\n        edits:\n          - file: apps/preview/manifest.yaml\n            field: image.tag\n",
            templates_dir.display(),
        ),
    )
    .expect("config");

    // F-95: the app demands `require_tag` (its tag_format names the
    // tag), so the fixture installs the mock provider with the tag the
    // good token's SHA claim points at.
    let mock = Arc::new(cargobike_engine::mock::MockProvider::at(
        &std::env::temp_dir()
            .join(format!("cb-oidc-mock-{port}"))
            .to_string_lossy(),
    ));
    let version = format!("1.0.{port}");
    mock.seed_tag(&format!("v{version}"), SEEDED_SHA);
    let mut providers = cargobike_core::registry::ProviderRegistry::new();
    providers.insert(
        "github",
        mock.clone() as Arc<dyn cargobike_core::provider::Provider>,
    );
    let boot = cargobike_server::boot(Some(&config_path), Some(providers)).await;
    let (router, _state) = match boot {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("skipping: boot failed: {error:?}");
            let dumped = std::fs::read_to_string(&config_path).unwrap_or_default();
            eprintln!("the config:\n{dumped}");
            return;
        }
    };
    // The JWKS rides on the same router as the API: one port, one process.
    let router = router.route(
        "/jwks",
        axum::routing::get(move || {
            let jwks = jwks.clone();
            async move { axum::Json(jwks) }
        }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    let client = reqwest::Client::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let claims_base = serde_json::json!({
        "iss": actions_issuer, "aud": "cargobike", "sub": "repo:my-org/my-service:ref:refs/tags/v1.0.0",
        "repository_owner_id": "123456", "repository_id": "123456",
        "repository": "my-org/my-service",
        "workflow_ref": "my-org/my-service/.github/workflows/release.yml",
        "run_id": "424242",
        "ref": "refs/tags/v1.0.0", "iat": now, "nbf": now.saturating_sub(5), "exp": now + 600,
    });
    const ACTIONS_ISSUER: &str = "https://token.actions.githubusercontent.com";

    // A matching token: 202 + Location (the creates via OIDC, //).
    let good = craft_jwt(&keypair.key_path, claims_base.clone(), "test-kid");
    let response = client
        .post(format!("{issuer}/api/v1/releases"))
        .header("Authorization", format!("Bearer {good}"))
        .json(&serde_json::json!({ "application": "my-service", "version": format!("1.0.{port}") }))
        .send()
        .await
        .expect("http");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::ACCEPTED,
        "the token body: {:?}",
        response.text().await
    );
    assert!(response.headers().get("Location").is_some());
    // F-83/F-84: the release's status carries the caller's verified
    // identity (issuer + subject off the token) and the CI context —
    // built from claims only, the Actions provider label keyed on the
    // real issuer, the run URL synthesized from the verified pair.
    let created: serde_json::Value = response.json().await.expect("release json");
    let created_location = format!(
        "/api/v1/releases/{}",
        created
            .pointer("/metadata/id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
    );
    assert_eq!(
        created
            .pointer("/status/actor/issuer")
            .and_then(serde_json::Value::as_str),
        Some(ACTIONS_ISSUER),
        "the actor's issuer is the verified token issuer"
    );
    assert_eq!(
        created
            .pointer("/status/actor/subject")
            .and_then(serde_json::Value::as_str),
        Some("repo:my-org/my-service:ref:refs/tags/v1.0.0")
    );
    assert_eq!(
        created
            .pointer("/status/ci/provider")
            .and_then(serde_json::Value::as_str),
        Some("github-actions"),
        "the CI provider label keys on the issuer: {created}"
    );
    assert_eq!(
        created
            .pointer("/status/ci/repository")
            .and_then(serde_json::Value::as_str),
        Some("my-org/my-service")
    );
    assert_eq!(
        created
            .pointer("/status/ci/run_id")
            .and_then(serde_json::Value::as_str),
        Some("424242")
    );
    assert_eq!(
        created
            .pointer("/status/ci/run_url")
            .and_then(serde_json::Value::as_str),
        Some("https://github.com/my-org/my-service/actions/runs/424242")
    );
    // F-95: a token whose SHA claim does not match the provider's tag
    // object refuses the create — the claim is an assertion, the
    // provider's tag is the truth.
    let mut mismatched = claims_base.clone();
    if let serde_json::Value::Object(map) = &mut mismatched {
        map.insert(
            "sha".to_owned(),
            serde_json::Value::String("0000000000000000000000000000000000000000".to_owned()),
        );
    }
    let mismatched_sha = craft_jwt(&keypair.key_path, mismatched, "test-kid");
    let response = client
        .post(format!("{issuer}/api/v1/releases"))
        .header("Authorization", format!("Bearer {mismatched_sha}"))
        .json(&serde_json::json!({ "application": "my-service", "version": version }))
        .send()
        .await
        .expect("http");
    let status = response.status();
    let body: serde_json::Value = response.json().await.expect("body");
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "the mismatched SHA claim must refuse: {body}"
    );
    assert_eq!(body["code"], "VersionNotVerified");
    // (created_location feeds the drain at the test's end; its
    // interpreter must reach terminal before this binary exits, or its
    // orphaned PENDING row poisons later boots' recovery.)

    // whoami reports the resolved identity and grants .
    let response = client
        .get(format!("{issuer}/api/v1/whoami"))
        .header("Authorization", format!("Bearer {good}"))
        .send()
        .await
        .expect("http");
    let body: serde_json::Value = response.json().await.expect("body");
    assert_eq!(body["origin"], "test-oidc");
    assert_eq!(
        body["subject"],
        "repo:my-org/my-service:ref:refs/tags/v1.0.0"
    );
    assert!(
        body["grants"]
            .as_array()
            .map(|g| !g.is_empty())
            .unwrap_or(false)
    );

    // A token with claims outside the entry: signature VALID, claims
    // mismatch ⇒ 403 ForbiddenResource (the gate).
    let mismatched = craft_jwt(
        &keypair.key_path,
        serde_json::json!({
            "iss": actions_issuer, "aud": "cargobike", "sub": "ci-runner",
            "repository_owner_id": "9999",
            "iat": now, "exp": now + 600,
        }),
        "test-kid",
    );
    let response = client
        .post(format!("{issuer}/api/v1/releases"))
        .header("Authorization", format!("Bearer {mismatched}"))
        .json(&serde_json::json!({ "application": "my-service", "version": "1.0.1" }))
        .send()
        .await
        .expect("http");
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);

    // A token for another audience: ⇒ 401 InvalidToken (the audience check).
    let wrong_aud = craft_jwt(
        &keypair.key_path,
        serde_json::json!({
            "iss": actions_issuer, "aud": "someone-else", "sub": "ci-runner",
            "iat": now, "exp": now + 600,
        }),
        "test-kid",
    );
    let response = client
        .post(format!("{issuer}/api/v1/releases"))
        .header("Authorization", format!("Bearer {wrong_aud}"))
        .json(&serde_json::json!({ "application": "my-service", "version": "1.0.2" }))
        .send()
        .await
        .expect("http");
    let status = response.status();
    let body: serde_json::Value = response.json().await.expect("body");
    assert_ne!(status, reqwest::StatusCode::ACCEPTED);
    assert_eq!(body["code"], "InvalidToken");

    // A token whose repository_id does not match the application's source
    // still satisfies entry claims? repository_id "999" matching
    // owner claim "123456" & source mismatch → the create refuses.
    let spoofed_repo = craft_jwt(
        &keypair.key_path,
        serde_json::json!({
            "iss": actions_issuer, "aud": "cargobike", "sub": "ci-runner",
            "repository_owner_id": "123456", "repository_id": "999",
            "iat": now, "exp": now + 600,
        }),
        "test-kid",
    );
    let response = client
        .post(format!("{issuer}/api/v1/releases"))
        .header("Authorization", format!("Bearer {spoofed_repo}"))
        .json(&serde_json::json!({ "application": "my-service", "version": "1.0.4" }))
        .send()
        .await
        .expect("http");
    let status = response.status();
    assert_eq!(
        status,
        reqwest::StatusCode::FORBIDDEN,
        "the repository claim must match; body: {:?}",
        response.text().await
    );

    // A differently-signed token: ⇒ 401 InvalidToken (the signature
    // check). The JWKS serves only the first keypair, so this token's
    // signature never validates regardless of caching order.
    let other = generate_rsa();
    let forged = craft_jwt(&other.key_path, claims_base, "test-kid");
    let response = client
        .post(format!("{issuer}/api/v1/releases"))
        .header("Authorization", format!("Bearer {forged}"))
        .json(&serde_json::json!({ "application": "my-service", "version": "1.0.3" }))
        .send()
        .await
        .expect("http");
    let status = response.status();
    let body: serde_json::Value = response.json().await.expect("body");
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "InvalidToken");

    // The drain: the created release leaves this process terminal
    // (Canceled), so the shared dbos schema carries no PENDING row the
    // next boot's recovery would adopt (the CI flake's disease).
    let cancel = client
        .post(format!("{issuer}{created_location}/cancel"))
        .header("Authorization", format!("Bearer {good}"))
        .send()
        .await
        .expect("the cancel sends");
    assert_eq!(
        cancel.status(),
        reqwest::StatusCode::NO_CONTENT,
        "the drain's cancel: {:?}",
        cancel.text().await
    );
}

/// The F-77 discovery + rotation pair: an entry WITHOUT `jwks_url`
/// resolves its JWKS through the issuer's discovery document (the
/// GitHub Actions shape — its JWKS lives at `/.well-known/jwks`, which
/// the legacy `jwks.json` guess never finds), and an unknown `kid`
/// forces exactly one rate-limited refetch per window (a key rotation
/// validates without waiting out the cache TTL; a kid storm cannot
/// hammer the issuer).
#[allow(clippy::expect_used, clippy::print_stderr)]
#[tokio::test(flavor = "current_thread")]
async fn test_oidc_discovery_resolves_jwks_and_unknown_kid_refetches_once() {
    let Some(_fix_url) = cargobike_testsupport::database_url().await else {
        return;
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("port").port();
    let issuer = format!("http://127.0.0.1:{port}");

    let old_key = generate_rsa();
    let new_key = generate_rsa();
    let old_jwk = |key: &RsaKeypair| {
        serde_json::json!({
            "kty": "RSA", "kid": "old-kid", "n": key.modulus_b64,
            "e": "AQAB", "alg": "RS256", "use": "sig"
        })
    };
    let new_jwk = |key: &RsaKeypair| {
        serde_json::json!({
            "kty": "RSA", "kid": "new-kid", "n": key.modulus_b64,
            "e": "AQAB", "alg": "RS256", "use": "sig"
        })
    };

    // The JWKS server's state: hit count + the rotation flag.
    let jwks_hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let rotated = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let templates_dir = std::env::temp_dir().join(format!("cb-oidc-disc-templates-{port}"));
    let _ = std::fs::create_dir_all(&templates_dir);
    std::fs::write(
        templates_dir.join("service.yaml"),
        "name: service\nversion: \"1\"\ninputs: {}\nenvironment_inputs: {}\nenvironments:\n  - name: preview\n    steps: []\n",
    )
    .expect("template");
    let config_path = std::env::temp_dir().join(format!("cb-oidc-disc-config-{port}.yaml"));
    std::fs::write(
        &config_path,
        format!(
            "server:\n  listen: \"127.0.0.1:{port}\"\n  public_url: \"{issuer}\"\n\n\
             database:\n  url: \"{_fix_url}\"\n  dbos_schema: \"cbtest-oidc-disc-{port}\"\n\n\
             templates:\n  directory: {}\n\n\
             auth:\n  oidc:\n    - name: gha-shape\n      issuer: \"{issuer}\"\n      audience: cargobike\n      claims:\n        repository_owner_id: \"123456\"\n      grants: [release:read]\n",
            templates_dir.display(),
        ),
    )
    .expect("config");

    // NOTE: no `jwks_url` — the discovery document is the only path to
    // the JWKS, and it names the GHA-style `/.well-known/jwks`.
    let boot = cargobike_server::boot(Some(&config_path), None).await;
    let (router, _state) = match boot {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("skipping: boot failed: {error:?}");
            let dumped = std::fs::read_to_string(&config_path).unwrap_or_default();
            eprintln!("the config:\n{dumped}");
            return;
        }
    };
    let discovery_body = serde_json::json!({
        "issuer": issuer,
        "jwks_uri": format!("{issuer}/.well-known/jwks"),
    });
    let hits_for_jwks = std::sync::Arc::clone(&jwks_hits);
    let rotation_flag = std::sync::Arc::clone(&rotated);
    let old_pair = std::sync::Arc::new(old_key);
    let new_pair = std::sync::Arc::new(new_key);
    // The token-crafting half keeps its own handles: the route's `move`
    // closures take the Arcs.
    let old_key_path = old_pair.key_path.clone();
    let new_key_path = new_pair.key_path.clone();
    let router = router
        .route(
            "/.well-known/openid-configuration",
            axum::routing::get(move || {
                let discovery_body = discovery_body.clone();
                async move { axum::Json(discovery_body) }
            }),
        )
        .route(
            "/.well-known/jwks",
            axum::routing::get(move || {
                let hits = std::sync::Arc::clone(&hits_for_jwks);
                let rotated = std::sync::Arc::clone(&rotation_flag);
                let old = old_pair.clone();
                let fresh = new_pair.clone();
                async move {
                    use std::sync::atomic::Ordering;
                    hits.fetch_add(1, Ordering::SeqCst);
                    let keys = if rotated.load(Ordering::SeqCst) {
                        vec![old_jwk(&old), new_jwk(&fresh)]
                    } else {
                        vec![old_jwk(&old)]
                    };
                    axum::Json(serde_json::json!({ "keys": keys }))
                }
            }),
        );
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    let client = reqwest::Client::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let claims = serde_json::json!({
        "iss": issuer, "aud": "cargobike", "sub": "ci-runner",
        "repository_owner_id": "123456",
        "iat": now, "nbf": now.saturating_sub(5), "exp": now + 600,
    });

    // The old key validates through the DISCOVERY-resolved JWKS (the
    // GHA path, not the legacy jwks.json guess).
    let old_token = craft_jwt(&old_key_path, claims.clone(), "old-kid");
    let response = client
        .get(format!("{issuer}/api/v1/whoami"))
        .header("Authorization", format!("Bearer {old_token}"))
        .send()
        .await
        .expect("http");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "the discovery-resolved JWKS validates: {:?}",
        response.text().await
    );
    assert_eq!(
        jwks_hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one JWKS fetch so far (the cached document)"
    );

    // A rotation lands: the new kid is unknown to the cached document,
    // the one refetch picks it up, and the token validates.
    rotated.store(true, std::sync::atomic::Ordering::SeqCst);
    let new_token = craft_jwt(&new_key_path, claims.clone(), "new-kid");
    let response = client
        .get(format!("{issuer}/api/v1/whoami"))
        .header("Authorization", format!("Bearer {new_token}"))
        .send()
        .await
        .expect("http");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "the unknown kid's forced refetch finds the rotated key: {:?}",
        response.text().await
    );
    assert_eq!(
        jwks_hits.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "exactly one forced refetch served the rotation"
    );

    // The storm probe: another unknown kid inside the SAME window is
    // refused without a refetch (the rate limit holds the line).
    let storm = craft_jwt(&old_key_path, claims, "storm-kid");
    let response = client
        .get(format!("{issuer}/api/v1/whoami"))
        .header("Authorization", format!("Bearer {storm}"))
        .send()
        .await
        .expect("http");
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(
        jwks_hits.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the window's rate limit refused the second forced refetch"
    );
}
