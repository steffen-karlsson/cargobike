//! Auth: claim matching, the authenticated caller
//! , argon2 API-key verification with rotation, the
//! localhost-only bootstrap key , create authorization
//! , and the Bearer middleware that protects routes.
//! OIDC-token validation against JWKS is the next milestone; tokens
//! answer `InvalidToken` until then.

use std::sync::Arc;

use crate::http::AppState;
use crate::http::errors::ApiError;

/// Claim matching : all listed claims must match (AND); string
/// claims are glob by default (, literal_separator semantics);
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

/// Matches one claim value: strings compare as globs (, with
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

/// The documented algorithm allowlist : reject `none` and `HS*`.
pub const ALGORITHM_ALLOWLIST: [&str; 8] = [
    "RS256", "RS384", "RS512", "PS256", "PS384", "PS512", "ES256", "ES384",
];

/// The authenticated caller (the actor identity + the key's grants).
#[derive(Clone, Debug)]
pub struct AuthedCaller {
    /// OIDC trust entry name (the api keys: their own key name).
    pub origin: String,
    /// Token issuer (the api keys: `"api-key"`).
    pub issuer: String,
    /// Token subject or the api-key name.
    pub subject: String,
    /// Presentable name.
    pub display_name: String,
    /// Grants of the matched entry .
    pub grants: Vec<String>,
    /// `repository_id` claim when the token carried one .
    pub repository_id: Option<String>,
    /// Bootstrap-key caller (the never network-legal).
    pub bootstrap: bool,
}

impl AuthedCaller {
    /// Whether a grant covers the operation (the exact or `"*"`).
    pub fn has_grant(&self, grant: &str) -> bool {
        self.grants.iter().any(|g| g == "*" || g == grant)
    }

    /// The `(the issuer, subject)` pair distinct-approver counting uses .
    pub fn principal(&self) -> String {
        format!("{}/{}", self.issuer, self.subject)
    }
}

/// Auth failures map to the vocabulary (the problem details).
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

/// Constant-time argon2 verify across the stored hashes (; two keys
/// can be active at once for rotation). Expired keys ( `expires`)
/// never verify: rotation is meant to dislodge them.
fn verify_api_keys<'a>(
    entries: &'a [crate::config::ApiKeyEntry],
    presented: &str,
) -> Option<&'a crate::config::ApiKeyEntry> {
    let verifier = argon2::Argon2::default();
    let now = time::OffsetDateTime::now_utc();
    entries.iter().find(|entry| {
        if !entry_is_active(entry, &now) {
            tracing::warn!(key = %entry.name, "an expired API key was not used");
            return false;
        }
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

/// An active key: `expires` (a date, or an RFC 3339 stamp) has not
/// passed. Unparsable expires texts never validate (the fail closed).
fn entry_is_active(entry: &crate::config::ApiKeyEntry, now: &time::OffsetDateTime) -> bool {
    let Some(text) = &entry.expires else {
        return true;
    };
    let date = time::Date::parse(
        text,
        &time::macros::format_description!("[year]-[month]-[day]"),
    )
    .ok()
    .map(|date| date.with_time(time::Time::MIDNIGHT).assume_utc())
    .or_else(|| {
        time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()
    });
    match date {
        Some(parsable_expiry) => *now < parsable_expiry,
        None => false,
    }
}

/// The auth context the middleware consults ( JWKS cached, refetch
/// on unknown `kid` is rate-limited ). Trust
/// entries come from the shared watch, so SIGHUP reloads reach auth too.
pub struct AuthState {
    /// The shared config source (the hot reload swaps this).
    config: tokio::sync::watch::Receiver<Arc<crate::config::Config>>,
    /// The localhost bootstrap key hashed at startup (the PHC
    /// string persists in the process only; verification is argon2).
    bootstrap: Option<String>,
    /// Cached JWKS documents keyed by URL .
    jwks: moka::sync::Cache<String, Arc<jsonwebtoken::jwk::JwkSet>>,
    /// Discovered `jwks_uri`s keyed by issuer .
    discovery: moka::sync::Cache<String, Arc<String>>,
    /// The forced-refetch windows, keyed by JWKS URL: one unknown-`kid`
    /// refetch per window per URL, so a rotation storm cannot
    /// hammer the issuer.
    refetch_windows: moka::sync::Cache<String, ()>,
    /// The outbound client used for discovery and JWKS fetches only.
    client: reqwest::Client,
}

/// The discovery document's path off an issuer ( ).
const DISCOVERY_PATH: &str = "/.well-known/openid-configuration";
/// The legacy direct JWKS path, the fallback when the issuer serves no
/// discovery document (pre-discovery behaviour, kept for exotic issuers).
const LEGACY_JWKS_PATH: &str = "/.well-known/jwks.json";
/// How long a discovered `jwks_uri` stands before re-discovery.
const DISCOVERY_TTL: std::time::Duration = std::time::Duration::from_secs(3600);
/// How long a cached JWKS stands before a plain refresh.
const JWKS_TTL: std::time::Duration = std::time::Duration::from_secs(300);
/// The unknown-`kid` forced-refetch window (the rate limit).
const JWKS_REFETCH_WINDOW: std::time::Duration = std::time::Duration::from_secs(30);

/// The salt's randomness: 16 bytes from the OS (Argon2's PHC b64).
fn rand_materials() -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    rand_core::OsRng.fill_bytes(&mut bytes);
    bytes
}

use rand_core::RngCore as _;

/// Constant-time argon2 verify against a startup-hashed at-rest value
/// (the bootstrap key; /).
fn verify_password_at_rest(hashed: &str, presented: &str) -> bool {
    match argon2::password_hash::PasswordHash::new(hashed) {
        Ok(stored) => argon2::password_hash::PasswordVerifier::verify_password(
            &argon2::Argon2::default(),
            presented.as_bytes(),
            &stored,
        )
        .is_ok(),
        Err(_) => false,
    }
}

/// The bootstrap-key environment variables .
pub const BOOTSTRAP_ENV: &str = "CARGOBIKE_SERVER_BOOTSTRAP_API_KEY";
pub const BOOTSTRAP_ENV_FILE: &str = "CARGOBIKE_SERVER_BOOTSTRAP_API_KEY_FILE";

impl AuthState {
    /// Builds auth state over the shared config watch, with the bootstrap
    /// key taken from the environment.
    #[allow(clippy::expect_used)] // the fallback salt's constant is valid b64 by construction
    pub fn new(
        config: tokio::sync::watch::Receiver<Arc<crate::config::Config>>,
    ) -> Result<Self, crate::config::ConfigError> {
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
            "the bootstrap key is for first-time setup; localhost requests only"
        );
        Ok(Self {
            config,
            // hash AT startup; the plaintext does not survive past
            // this line (the argon2 PHC string is the kept form).
            bootstrap: plaintext.map(|plaintext| {
                use argon2::password_hash::{PasswordHasher, SaltString};
                let salt = SaltString::encode_b64(&rand_materials())
                    .ok()
                    .unwrap_or_else(|| {
                        SaltString::from_b64("cargobike-bootstrap-salt-000000000000000")
                            .expect("the fallback salt")
                    });
                argon2::Argon2::default()
                    .hash_password(plaintext.as_bytes(), &salt)
                    .map(|hashed| hashed.to_string())
                    .unwrap_or_default()
            }),
            jwks: moka::sync::Cache::builder().time_to_live(JWKS_TTL).build(),
            discovery: moka::sync::Cache::builder()
                .time_to_live(DISCOVERY_TTL)
                .build(),
            refetch_windows: moka::sync::Cache::builder()
                .time_to_live(JWKS_REFETCH_WINDOW)
                .build(),
            client: reqwest::Client::builder()
                .user_agent("cargobike")
                .build()
                .map_err(|error| {
                    crate::config::ConfigError::Parse(format!("http client: {error}"))
                })?,
        })
    }

    /// The config as of now (the cheap Arc clone; updated on every SIGHUP swap).
    pub fn config(&self) -> Arc<crate::config::Config> {
        self.config.borrow().clone()
    }

    /// The JWKS URL an entry validates against (F-77): the explicit
    /// `jwks_url` override wins, then the issuer's discovery document's
    /// `jwks_uri` (the GitHub Actions issuer's real path,
    /// `/.well-known/jwks`, only appears there), then the legacy direct
    /// `{issuer}/.well-known/jwks.json` guess. The discovered URI is
    /// cached per issuer; a discovery failure falls back and is logged
    /// once per TTL window.
    async fn jwks_url_for(&self, entry: &crate::config::OidcEntry) -> String {
        if let Some(explicit) = &entry.jwks_url {
            return explicit.clone();
        }
        if let Some(discovered) = self.discovery.get(&entry.issuer) {
            return discovered.to_string();
        }
        let issuer = entry.issuer.trim_end_matches('/');
        let document_url = format!("{issuer}{DISCOVERY_PATH}");
        let discovered = async {
            let response = self.client.get(&document_url).send().await.ok()?;
            let response = response.error_for_status().ok()?;
            let document: serde_json::Value = response.json().await.ok()?;
            let uri = document.get("jwks_uri")?.as_str()?;
            Some(uri.to_owned()).filter(|uri| !uri.is_empty())
        }
        .await;
        match discovered {
            Some(uri) => {
                self.discovery
                    .insert(entry.issuer.clone(), Arc::new(uri.clone()));
                uri
            }
            None => {
                tracing::warn!(
                    issuer = %entry.issuer,
                    "no OIDC discovery document; falling back to {LEGACY_JWKS_PATH}"
                );
                let fallback = format!("{issuer}{LEGACY_JWKS_PATH}");
                // The fallback also fills the discovery slot: within the
                // TTL window the issuer is not re-asked.
                self.discovery
                    .insert(entry.issuer.clone(), Arc::new(fallback.clone()));
                fallback
            }
        }
    }

    /// The JWKS document for a URL, from the TTL cache when fresh.
    async fn jwks_cached(&self, url: &str) -> Result<Arc<jsonwebtoken::jwk::JwkSet>, AuthError> {
        if let Some(cached) = self.jwks.get(url) {
            return Ok(cached);
        }
        self.jwks_fetch(url).await
    }

    /// Fetches a JWKS document and admits it to the cache (a fresh fetch
    /// also refreshes the document for every other caller of the URL).
    async fn jwks_fetch(&self, url: &str) -> Result<Arc<jsonwebtoken::jwk::JwkSet>, AuthError> {
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

    /// The unknown-`kid` forced refetch (F-77): at most one per URL per
    /// [`JWKS_REFETCH_WINDOW`], so a rotation (or an attacker spraying
    /// random kids) cannot turn validation into a fetch storm. `None`
    /// means the window refused; the cached document stands.
    async fn jwks_refetch(&self, url: &str) -> Option<Arc<jsonwebtoken::jwk::JwkSet>> {
        // The window marker: `or_insert_with` computes only when the
        // marker is absent, and the returned entry reports whether THIS
        // call was the fresh one — so the first unknown kid in a window
        // takes the fetch and every later one finds the marker standing.
        let marker = self
            .refetch_windows
            .entry(url.to_owned())
            .or_insert_with(|| ());
        if marker.is_fresh() {
            self.jwks_fetch(url).await.ok()
        } else {
            None
        }
    }

    /// The key for a token header out of a JWKS: the `kid` match first,
    /// a single-key document second (kid-less issuers).
    fn select_key<'a>(
        jwks: &'a jsonwebtoken::jwk::JwkSet,
        kid: &Option<String>,
    ) -> Option<&'a jsonwebtoken::jwk::Jwk> {
        jwks.keys
            .iter()
            .find(|jwk| jwk.common.key_id.as_deref() == kid.as_deref())
            .or_else(|| (jwks.keys.len() == 1).then(|| &jwks.keys[0]))
    }

    /// Resolves a Bearer-credential caller: API keys first , then
    /// the bootstrap key from loopback peers only , then OIDC
    /// validation per trust entry (..).
    pub async fn resolve_bearer(
        &self,
        presented: &str,
        peer: Option<std::net::IpAddr>,
    ) -> Result<AuthedCaller, AuthError> {
        let config = self.config();
        if let Some(key) = verify_api_keys(&config.auth.api_keys, presented) {
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
        if let Some(bootstrap_hash) = &self.bootstrap {
            let is_local = peer.map(|ip| ip.is_loopback()).unwrap_or(false);
            // the bootstrap key was hashed at startup; the verify
            // is the same argon2 path, constant against key material.
            if is_local && verify_password_at_rest(bootstrap_hash, presented) {
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

    /// The OIDC validation path (..). The first trust entry whose
    /// full validation passes wins; a strictly expired token is surfaced
    /// as its own 401 code; claim mismatches surface 403.
    async fn decode_oidc(&self, token: &str) -> Result<AuthedCaller, AuthError> {
        let header = jsonwebtoken::decode_header(token).map_err(|_| AuthError::InvalidToken)?;
        let mut expired = false;
        for entry in &self.config().auth.oidc {
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
            let jwks_url = self.jwks_url_for(entry).await;
            let mut jwks = self.jwks_cached(&jwks_url).await?;
            // Key selection by `kid`; a single-key document needs none.
            let mut jwk = Self::select_key(&jwks, &header.kid);
            if jwk.is_none() {
                // F-77: an unknown `kid` may be a rotation the cached
                // document predates — one rate-limited refetch, then the
                // selection runs again.
                if let Some(fresh) = self.jwks_refetch(&jwks_url).await {
                    jwks = fresh;
                    jwk = Self::select_key(&jwks, &header.kid);
                }
            }
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
                .unwrap_or(60); // documented default skew is 60s
            let decoded = match jsonwebtoken::decode::<serde_json::Value>(token, &key, &validation)
            {
                Ok(decoded) => decoded,
                Err(error) => {
                    // Only a strictly expired token surfaces `TokenExpired`;
                    // bad signature/audience stay `InvalidToken` .
                    tracing::warn!(entry = %entry.name, ?error, "failed to decode");
                    if *error.kind() == jsonwebtoken::errors::ErrorKind::ExpiredSignature {
                        expired = true;
                    }
                    continue;
                }
            };
            let claims = decoded.claims;
            // matching; unconstrained guard double-checks the
            // config-validated entry (the defence in depth at runtime).
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

/// Create authorization : the caller needs `release:create`,
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
/// extension (the handlers read `Extension<AuthedCaller>`), or answers
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

/// Turns an auth failure into the RFC 9457 problem response .
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
        // `*` does not cross separators : nested refs need `**`.
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
