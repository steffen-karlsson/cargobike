//! Auth (PRD 2.5/2.6): claim matching (F-79), the authenticated caller
//! (F-83/F-85), argon2 API-key verification with rotation, the
//! localhost-only bootstrap key (F-86), create authorization
//! (F-93/F-99a), and the Bearer middleware that protects routes.
//! OIDC-token validation against JWKS is the next milestone; tokens
//! answer `InvalidToken` until then.

use secrecy::ExposeSecret;
use std::sync::Arc;

use crate::http::AppState;
use crate::http::errors::ApiError;

/// Claim matching (F-79): all listed claims must match (AND); string
/// claims are glob by default (R7, literal_separator semantics);
/// array-valued claims match if any element matches.
pub fn claim_match(
    expected: &std::collections::BTreeMap<String, serde_json::Value>,
    claims: &serde_json::Value,
) -> Option<()> {
    for (name, expected_value) in expected {
        let actual = claims.get(name)?;
        let matched = match (actual, expected_value) {
            (serde_json::Value::Array(items), expected_value) => {
                items.iter().any(|item| value_match(item, expected_value))
            }
            (actual, expected_value) => value_match(actual, expected_value),
        };
        if !matched {
            return None;
        }
    }
    Some(())
}

/// Matches one claim value: strings compare as globs (R7, with
/// GlobBuilder's literal_separator so `*` stays inside one segment),
/// everything else as exact JSON equality.
fn value_match(actual: &serde_json::Value, expected: &serde_json::Value) -> bool {
    match (expected, actual) {
        (serde_json::Value::String(expected), serde_json::Value::String(actual)) => {
            globset::GlobBuilder::new(expected)
                .literal_separator(true)
                .build()
                .map(|glob| glob.compile_matcher().is_match(actual))
                .unwrap_or(false)
        }
        _ => expected == actual,
    }
}

/// The documented algorithm allowlist (F-78): reject `none` and `HS*`.
pub const ALGORITHM_ALLOWLIST: [&str; 8] = [
    "RS256", "RS384", "RS512", "PS256", "PS384", "PS512", "ES256", "ES384",
];

/// The authenticated caller (F-83's actor, F-85's key grants).
#[derive(Clone, Debug)]
pub struct AuthedCaller {
    /// OIDC trust entry name (api keys: their own key name).
    pub origin: String,
    /// Token issuer (api keys: `"api-key"`).
    pub issuer: String,
    /// Token subject or the api-key name.
    pub subject: String,
    /// Presentable name.
    pub display_name: String,
    /// Grants of the matched entry (F-99).
    pub grants: Vec<String>,
    /// `repository_id` claim when the token carried one (F-93).
    pub repository_id: Option<String>,
    /// Bootstrap-key caller (never network-legal, F-86).
    pub bootstrap: bool,
}

impl AuthedCaller {
    /// Whether a grant covers the operation (F-99: exact or `"*"`).
    pub fn has_grant(&self, grant: &str) -> bool {
        self.grants.iter().any(|g| g == "*" || g == grant)
    }

    /// The `(issuer, subject)` pair distinct-approver counting uses (F-96).
    pub fn principal(&self) -> String {
        format!("{}/{}", self.issuer, self.subject)
    }
}

/// Auth failures map to the §10.2 vocabulary (F-103 problem details).
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// No Authorization header at all.
    #[error("no token provided")]
    MissingToken,
    /// Malformed header, unknown key, bad token.
    #[error("token validation failed")]
    InvalidToken,
    /// Token expired.
    #[error("token has expired")]
    TokenExpired,
    /// The caller lacks the grant or `releasers` scope for the operation.
    #[error("permission denied")]
    ForbiddenResource,
}

impl AuthError {
    /// The code the problem documents carry.
    pub const fn code(&self) -> &'static str {
        match self {
            AuthError::MissingToken => "MissingToken",
            AuthError::InvalidToken => "InvalidToken",
            AuthError::TokenExpired => "TokenExpired",
            AuthError::ForbiddenResource => "ForbiddenResource",
        }
    }
}

/// Constant-time argon2 verify across the stored hashes (F-85; two keys
/// can be active at once for rotation).
fn verify_api_keys<'a>(
    entries: &'a [crate::config::ApiKeyEntry],
    presented: &str,
) -> Option<&'a crate::config::ApiKeyEntry> {
    let verifier = argon2::Argon2::default();
    entries.iter().find(|entry| {
        argon2::password_hash::PasswordHash::new(&entry.hash)
            .ok()
            .is_some_and(|stored| {
                argon2::password_hash::PasswordVerifier::verify_password(
                    &verifier,
                    presented.as_bytes(),
                    &stored,
                )
                .is_ok()
            })
    })
}

/// The auth context the middleware consults (F-77: JWKS cached, refetch
/// on unknown `kid` is rate-limited by the moka TTL window).
pub struct AuthState {
    /// The server config snapshot.
    pub config: Arc<crate::config::Config>,
    /// The localhost bootstrap key, when provided (F-86).
    bootstrap: Option<secrecy::SecretString>,
    /// Cached JWKS documents keyed by URL (F-77).
    jwks: moka::sync::Cache<String, Arc<jsonwebtoken::jwk::JwkSet>>,
    /// The outbound client used for JWKS fetches only.
    client: reqwest::Client,
}

/// The bootstrap-key environment variables (F-86).
pub const BOOTSTRAP_ENV: &str = "CARGOBIKE_SERVER_BOOTSTRAP_API_KEY";
pub const BOOTSTRAP_ENV_FILE: &str = "CARGOBIKE_SERVER_BOOTSTRAP_API_KEY_FILE";

impl AuthState {
    /// Builds auth state, taking the bootstrap key from the environment.
    pub fn new(config: Arc<crate::config::Config>) -> Result<Self, crate::config::ConfigError> {
        let explicit = std::env::var(BOOTSTRAP_ENV).ok().filter(|v| !v.is_empty());
        let from_file = std::env::var(BOOTSTRAP_ENV_FILE)
            .ok()
            .filter(|v| !v.is_empty())
            .map(|path| {
                std::fs::read_to_string(path.clone())
                    .map(|contents| contents.trim_end_matches('\n').to_owned())
                    .map_err(|cause| crate::config::ConfigError::Materialise {
                        path: path.into(),
                        cause: cause.to_string(),
                    })
            })
            .transpose()?;
        let plaintext = explicit.or(from_file);
        tracing::warn!(
            present = plaintext.is_some(),
            "the bootstrap key is for first-time setup; localhost requests only (F-86)"
        );
        Ok(Self {
            config,
            bootstrap: plaintext.map(secrecy::SecretString::from),
            jwks: moka::sync::Cache::builder()
                .time_to_live(std::time::Duration::from_secs(300))
                .build(),
            client: reqwest::Client::builder()
                .user_agent("cargobike")
                .build()
                .map_err(|error| {
                    crate::config::ConfigError::Parse(format!("http client: {error}"))
                })?,
        })
    }

    /// The JWKS document for an entry, cached by URL (F-77's `jwks_url`
    /// override when the issuer lacks usable discovery).
    async fn jwks_for(&self, url: &str) -> Result<Arc<jsonwebtoken::jwk::JwkSet>, AuthError> {
        if let Some(cached) = self.jwks.get(url) {
            return Ok(cached);
        }
        let response = self
            .client
            .get(url)
            .send()
            .await
            .ok()
            .and_then(|response| response.error_for_status().ok())
            .ok_or(AuthError::InvalidToken)?;
        let fresh = response
            .json::<jsonwebtoken::jwk::JwkSet>()
            .await
            .ok()
            .ok_or(AuthError::InvalidToken)?;
        let fresh = Arc::new(fresh);
        self.jwks.insert(url.to_owned(), Arc::clone(&fresh));
        Ok(fresh)
    }

    /// Resolves a Bearer-credential caller: API keys first (F-85), then
    /// the bootstrap key from loopback peers only (F-86), then OIDC
    /// validation per trust entry (F-77..F-79).
    pub async fn resolve_bearer(
        &self,
        presented: &str,
        peer: Option<std::net::IpAddr>,
    ) -> Result<AuthedCaller, AuthError> {
        if let Some(key) = verify_api_keys(&self.config.auth.api_keys, presented) {
            return Ok(AuthedCaller {
                origin: key.name.clone(),
                issuer: "api-key".to_owned(),
                subject: key.name.clone(),
                display_name: key.description.clone().unwrap_or_else(|| key.name.clone()),
                grants: key.grants.clone(),
                repository_id: None,
                bootstrap: false,
            });
        }
        if let Some(bootstrap) = &self.bootstrap {
            let is_local = peer.map(|ip| ip.is_loopback()).unwrap_or(false);
            if is_local && bootstrap.expose_secret() == presented {
                return Ok(AuthedCaller {
                    origin: "bootstrap".to_owned(),
                    issuer: "bootstrap".to_owned(),
                    subject: "bootstrap".to_owned(),
                    display_name: "bootstrap".to_owned(),
                    grants: vec!["*".to_owned()],
                    repository_id: None,
                    bootstrap: true,
                });
            }
        }
        self.decode_oidc(presented).await
    }

    /// The OIDC validation path (F-77..F-79). The first trust entry whose
    /// full validation passes wins; a strictly expired token is surfaced
    /// as its own 401 code; claim mismatches surface 403.
    async fn decode_oidc(&self, token: &str) -> Result<AuthedCaller, AuthError> {
        let header = jsonwebtoken::decode_header(token).map_err(|_| AuthError::InvalidToken)?;
        let mut expired = false;
        for entry in &self.config.auth.oidc {
            let matches_algorithm = entry
                .algorithms
                .as_ref()
                .map(|names| {
                    names
                        .iter()
                        .any(|name| name == &format!("{:?}", header.alg))
                })
                .unwrap_or_else(|| {
                    ALGORITHM_ALLOWLIST
                        .iter()
                        .any(|a| *a == format!("{:?}", header.alg))
                });
            if !matches_algorithm {
                continue;
            }
            let jwks_url = entry.jwks_url.clone().unwrap_or_else(|| {
                format!(
                    "{}/.well-known/jwks.json",
                    entry.issuer.trim_end_matches('/')
                )
            });
            let jwks = self.jwks_for(&jwks_url).await?;
            // Key selection by `kid`; a single-key document needs none.
            let jwk = jwks
                .keys
                .iter()
                .find(|jwk| jwk.common.key_id.as_deref() == header.kid.as_deref())
                .or_else(|| (jwks.keys.len() == 1).then(|| &jwks.keys[0]));
            let Some(jwk) = jwk else { continue };
            let Ok(key) = jsonwebtoken::DecodingKey::from_jwk(jwk) else {
                continue;
            };
            let mut validation = jsonwebtoken::Validation::new(header.alg);
            validation.set_issuer(std::slice::from_ref(&entry.issuer));
            validation.set_audience(std::slice::from_ref(&entry.audience));
            validation.leeway = entry
                .clock_skew
                .as_deref()
                .and_then(|skew| humantime::parse_duration(skew).ok())
                .map(|skew| skew.as_secs())
                .unwrap_or(60); // F-77's documented default skew is 60s
            let Ok(decoded) = jsonwebtoken::decode::<serde_json::Value>(token, &key, &validation)
            else {
                expired = true; // an ExpiredSignature anywhere in the chain surfaces 401 token-expired
                continue;
            };
            let claims = decoded.claims;
            // F-79's matching; F-80's unconstrained guard double-checks the
            // config-validated entry (defence in depth at runtime).
            if entry.claims.is_empty() && !entry.allow_unconstrained {
                return Err(AuthError::InvalidToken);
            }
            claim_match(&entry.claims, &claims).ok_or(AuthError::ForbiddenResource)?;
            return Ok(AuthedCaller {
                origin: entry.name.clone(),
                issuer: entry.issuer.clone(),
                subject: claims
                    .get("sub")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                display_name: claims
                    .get("display_name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                grants: entry.grants.clone(),
                repository_id: claims
                    .get("repository_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                bootstrap: false,
            });
        }
        if expired {
            Err(AuthError::TokenExpired)
        } else {
            Err(AuthError::InvalidToken)
        }
    }
}

/// Create authorization (F-93/F-99a): the caller needs `release:create`,
/// must be referenced by the application's `releasers`, and — when its
/// token carries `repository_id` — the claim must match the registered
/// source. Returns the matched application.
pub fn authorize_create<'a>(
    config: &'a crate::config::Config,
    caller: &AuthedCaller,
    application: &str,
) -> Result<&'a crate::config::ApplicationEntry, AuthError> {
    if !caller.has_grant("release:create") {
        return Err(AuthError::ForbiddenResource);
    }
    let app = config
        .application(application)
        .ok_or(AuthError::ForbiddenResource)?;
    let referenced = app.releasers.iter().any(|selector| match selector {
        crate::config::PrincipalSelector::Oidc { oidc } => *oidc == caller.origin,
        crate::config::PrincipalSelector::ApiKey { api_key } => {
            *api_key == caller.origin && !caller.bootstrap
        }
    });
    if !referenced {
        return Err(AuthError::ForbiddenResource);
    }
    if let Some(claimed) = &caller.repository_id {
        if claimed != &app.source.id {
            return Err(AuthError::ForbiddenResource);
        }
    }
    Ok(app)
}

/// The Bearer middleware: resolves the caller, inserts it as a request
/// extension (handlers read `Extension<AuthedCaller>`), or answers
/// 401/403 with the problem document.
pub async fn require_auth(
    state: axum::extract::State<Arc<AppState>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let presented = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_owned)
        .filter(|value| !value.is_empty());
    let Some(presented) = presented else {
        return problem(request.uri().path(), AuthError::MissingToken);
    };
    let resolved = state.auth.resolve_bearer(&presented, Some(peer.ip())).await;
    match resolved {
        Ok(caller) => {
            request.extensions_mut().insert(caller);
            next.run(request).await
        }
        Err(error) => problem(request.uri().path(), error),
    }
}

/// Turns an auth failure into the RFC 9457 problem response (§10.2/F-103).
pub fn problem(path: &str, error: AuthError) -> axum::response::Response {
    let (status, slug) = match error {
        AuthError::MissingToken => (axum::http::StatusCode::UNAUTHORIZED, "missing-token"),
        AuthError::InvalidToken => (axum::http::StatusCode::UNAUTHORIZED, "invalid-token"),
        AuthError::TokenExpired => (axum::http::StatusCode::UNAUTHORIZED, "token-expired"),
        AuthError::ForbiddenResource => (axum::http::StatusCode::FORBIDDEN, "forbidden-resource"),
    };
    use axum::response::IntoResponse;
    ApiError::new(status, error.code(), slug, format!("{error}: {path}")).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn expected(pairs: &[(&str, serde_json::Value)]) -> BTreeMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn test_claim_match_is_and_with_glob_defaults() {
        let expected = expected(&[
            ("repository_owner_id", json!("123456")),
            ("ref", json!("refs/tags/v*")),
        ]);
        let claims = json!({
            "repository_owner_id": "123456",
            "ref": "refs/tags/v1.2.3"
        });
        assert!(claim_match(&expected, &claims).is_some());
        let wrong = json!({ "repository_owner_id": "999", "ref": "refs/tags/v1.2.3" });
        assert!(claim_match(&expected, &wrong).is_none());
        // `*` does not cross separators (R7): nested refs need `**`.
        let nested = json!("refs/tags/v1/2/3");
        assert!(claim_match(&expected, &nested).is_none());
    }

    #[test]
    fn test_array_claims_match_any_element() {
        let expected = expected(&[("roles", json!("release-managers"))]);
        let claims = json!({ "roles": ["ci", "release-managers"] });
        assert!(claim_match(&expected, &claims).is_some());
        let denied = json!({ "roles": ["ci"] });
        assert!(claim_match(&expected, &denied).is_none());
    }

    #[test]
    fn test_missing_claim_never_matches() {
        let expected = expected(&[("roles", json!("release-managers"))]);
        assert!(claim_match(&expected, &json!({})).is_none());
    }

    #[test]
    fn test_algorithm_allowlist_rejects_none_and_hs() {
        for name in ALGORITHM_ALLOWLIST {
            assert!(!name.starts_with("HS"), "{name}");
            assert_ne!(name, "none");
        }
    }
}
