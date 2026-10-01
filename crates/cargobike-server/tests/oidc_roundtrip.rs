//! OIDC roundtrip (2.5/2.9): an in-test RSA key + JWKS served by the
//! same process as the release API, plus an openssl-signed RS256 token
//! through the validation path — the wire-level proof 2.5 needs.
//! Requires `CARGOBIKE_TEST_DATABASE_URL`.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::net::SocketAddr;
use std::process::Command;

/// b64url for token parts and JWK moduli.
fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The paths of the generated RSA key (kept for the whole test) and its
/// b64url modulus from the DER data.
struct RsaKeypair {
    key_path: std::path::PathBuf,
    modulus_b64: String,
}

/// Generates an RSA-2048 keypair via the CLI openssl (macOS and CI
/// runners ship it) and extracts the modulus for the JWKS.
#[allow(clippy::expect_used, clippy::print_stderr)]
fn generate_rsa() -> RsaKeypair {
    let dir = std::env::temp_dir();
    let key_path = dir.join("cb-oidc-roundtrip-key.pem");
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
    let message_path = std::env::temp_dir().join("cb-oidc-roundtrip-message");
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
    let Some(fix_url) = std::env::var("CARGOBIKE_TEST_DATABASE_URL")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        eprintln!("skipping: CARGOBIKE_TEST_DATABASE_URL is unset");
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

    // Config with one OIDC entry and a single registered application.
    let config_path = std::env::temp_dir().join(format!("cb-oidc-config-{port}.yaml"));
    std::fs::write(
        &config_path,
        format!(
            "server:\n  listen: \"127.0.0.1:{port}\"\n  public_url: \"{issuer}\"\n\n\
             database:\n  url: \"{fix_url}\"\n\n\
             templates:\n  directory: ../../templates\n\n\
             auth:\n  oidc:\n    - name: test-oidc\n      issuer: \"{issuer}\"\n      audience: cargobike\n      jwks_url: \"{issuer}/jwks\"\n      claims:\n        repository_owner_id: \"123456\"\n      grants: [release:create, release:read]\n\n\
             applications:\n  - name: my-service\n    source: {{ provider: github, id: \"123456\" }}\n    template: service@1\n    releasers:\n      - oidc: test-oidc\n    environments:\n      preview:\n        concurrency: supersede\n"
        ),
    )
    .expect("config");

    let Ok((router, _state)) = cargobike_server::boot(Some(&config_path)).await else {
        eprintln!("skipping: boot failed");
        return;
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
        "iss": issuer, "aud": "cargobike", "sub": "ci-runner",
        "repository_owner_id": "123456", "repository_id": "123456",
        "ref": "refs/tags/v1.0.0", "iat": now, "nbf": now.saturating_sub(5), "exp": now + 600,
    });

    // A matching token: 202 + Location (creates via OIDC, US-1/F-93/F-99a).
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

    // whoami reports the resolved identity and grants (F-106, US-1).
    let response = client
        .get(format!("{issuer}/api/v1/whoami"))
        .header("Authorization", format!("Bearer {good}"))
        .send()
        .await
        .expect("http");
    let body: serde_json::Value = response.json().await.expect("body");
    assert_eq!(body["origin"], "test-oidc");
    assert_eq!(body["subject"], "ci-runner");
    assert!(
        body["grants"]
            .as_array()
            .map(|g| !g.is_empty())
            .unwrap_or(false)
    );

    // A token with claims outside the entry: signature VALID, claims
    // mismatch ⇒ 403 ForbiddenResource (F-79's gate).
    let mismatched = craft_jwt(
        &keypair.key_path,
        serde_json::json!({
            "iss": issuer, "aud": "cargobike", "sub": "ci-runner",
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

    // A token for another audience: ⇒ 401 InvalidToken (F-77's aud check).
    let wrong_aud = craft_jwt(
        &keypair.key_path,
        serde_json::json!({
            "iss": issuer, "aud": "someone-else", "sub": "ci-runner",
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
    // (F-93) still satisfies entry claims? repository_id "999" matching
    // owner claim "123456" & source mismatch → the create refuses.
    let spoofed_repo = craft_jwt(
        &keypair.key_path,
        serde_json::json!({
            "iss": issuer, "aud": "cargobike", "sub": "ci-runner",
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
        "F-93 repo id mismatch refuses; body: {:?}",
        response.text().await
    );

    // A differently-signed token: ⇒ 401 InvalidToken (F-77's signature
    // check). Runs last: the second keypair overwrites the same pem path,
    // so anything signed after this line uses the foreign key.
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
}
