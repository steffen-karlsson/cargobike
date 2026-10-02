//! GitHub Actions auto-detection: no
//! config file needed when the Actions OIDC environment is present —
//! the CLI exchanges the workflow's request token for an audience
//! token, and carries the CI context on commands that create releases.

use std::env;

/// The CI facts the CLI auto-detects (the actor annotation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiContext {
    /// `GITHUB_REPOSITORY` (the owner/name).
    pub repository: String,
    /// `GITHUB_SHA` (the triggering commit).
    pub sha: String,
    /// `GITHUB_RUN_ID` (the workflow run).
    pub run_id: String,
}

impl CiContext {
    /// Reads the CI environment; `None` outside Actions.
    pub fn from_environment() -> Option<Self> {
        let repository = env::var("GITHUB_REPOSITORY").ok()?;
        let sha = env::var("GITHUB_SHA").ok()?;
        let run_id = env::var("GITHUB_RUN_ID").ok()?;
        if repository.is_empty() || sha.is_empty() || run_id.is_empty() {
            return None;
        }
        Some(Self {
            repository,
            sha,
            run_id,
        })
    }
}

/// The Actions OIDC request environment (the both variables are required).
pub fn oidc_request_environment() -> Option<(String, String)> {
    let url = env::var("ACTIONS_ID_TOKEN_REQUEST_URL").ok()?;
    let token = env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN").ok()?;
    if url.is_empty() || token.is_empty() {
        return None;
    }
    Some((url, token))
}

/// Whether auth type `github-actions` is usable right now.
pub fn actions_detected() -> bool {
    oidc_request_environment().is_some()
}

/// The audience an Actions token requests: the flag's value first
/// (`--audience`), then `CARGOBIKE_AUDIENCE`, then the built-in
/// default.
pub fn audience(prefer: Option<&str>) -> String {
    prefer
        .map(|value| value.to_owned())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var("CARGOBIKE_AUDIENCE")
                .ok()
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| "cargobike".to_owned())
}

/// The Actions OIDC token exchange: the workflow's request token from
/// `ACTIONS_ID_TOKEN_REQUEST_TOKEN` authorizes a GET to
/// `ACTIONS_ID_TOKEN_REQUEST_URL?audience=…`; the response's `value`
/// field carries the audience-scoped ID token. Any failure is the
/// caller's auth error text.
pub async fn actions_token(audience: &str) -> Result<secrecy::SecretString, String> {
    let Some((url, request_token)) = oidc_request_environment() else {
        return Err(
            "the GitHub Actions OIDC environment is missing (ACTIONS_ID_TOKEN_REQUEST_URL and \
             ACTIONS_ID_TOKEN_REQUEST_TOKEN are both required)"
                .to_owned(),
        );
    };
    let client = reqwest::Client::new();
    let response = client
        .get(&url)
        .bearer_auth(&request_token)
        .query(&[("audience", audience)])
        .send()
        .await
        .map_err(|failure| format!("the Actions OIDC endpoint refused: {failure}"))?;
    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|failure| format!("the Actions OIDC response is unreadable: {failure}"))?;
    if !status.is_success() {
        return Err(format!(
            "the Actions OIDC endpoint answered {}: {}",
            status,
            body.get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
        ));
    }
    let Some(token) = body.get("value").and_then(serde_json::Value::as_str) else {
        return Err("the Actions OIDC exchange returned no value field".to_owned());
    };
    if token.is_empty() {
        return Err("the Actions OIDC exchange's value is empty".to_owned());
    }
    Ok(secrecy::SecretString::from(token.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_actions_detection_reads_both_oidc_variables() {
        unsafe {
            env::set_var(
                "ACTIONS_ID_TOKEN_REQUEST_URL",
                "https://token.actions.githubusercontent.com",
            );
        }
        unsafe { env::remove_var("ACTIONS_ID_TOKEN_REQUEST_TOKEN") };
        assert!(!actions_detected());
        unsafe { env::set_var("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "tok") };
        assert!(actions_detected());
        unsafe {
            env::remove_var("ACTIONS_ID_TOKEN_REQUEST_URL");
            env::remove_var("ACTIONS_ID_TOKEN_REQUEST_TOKEN");
        }
        assert!(!actions_detected());
    }

    #[test]
    fn test_ci_context_carries_the_six_facts() {
        unsafe {
            env::set_var("GITHUB_REPOSITORY", "org/repo");
            env::set_var("GITHUB_SHA", "abc123");
            env::set_var("GITHUB_RUN_ID", "42");
        }
        let context = CiContext::from_environment().expect("detected");
        assert_eq!(context.repository, "org/repo");
        assert_eq!(context.sha, "abc123");
        assert_eq!(context.run_id, "42");
        unsafe {
            env::remove_var("GITHUB_REPOSITORY");
            env::remove_var("GITHUB_SHA");
            env::remove_var("GITHUB_RUN_ID");
        }
        assert!(CiContext::from_environment().is_none());
    }

    #[test]
    fn test_audience_defaults_to_cargobike() {
        unsafe { env::remove_var("CARGOBIKE_AUDIENCE") };
        assert_eq!(audience(None), "cargobike");
        unsafe { env::set_var("CARGOBIKE_AUDIENCE", "other") };
        assert_eq!(audience(None), "other");
        // The flag wins over the env.
        assert_eq!(audience(Some("from-a-flag")), "from-a-flag");
        unsafe { env::remove_var("CARGOBIKE_AUDIENCE") };
    }

    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::unwrap_used, clippy::print_stderr)]
    async fn test_the_actions_exchange_mints_audience_tokens() {
        use secrecy::ExposeSecret as _;
        use wiremock::matchers::{method, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        unsafe { std::env::remove_var("ACTIONS_ID_TOKEN_REQUEST_URL") };
        unsafe { std::env::remove_var("ACTIONS_ID_TOKEN_REQUEST_TOKEN") };
        let auth = actions_token("cargobike");
        let failure = auth.await;
        assert!(failure.is_err(), "no environment, no exchange");

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("audience", "cargobike"))
            .respond_with(|request: &wiremock::Request| {
                // The request must carry the ACTIONS bearer.
                let token_ok = request
                    .headers
                    .get("Authorization")
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value.contains("the-bridge-token"));
                if !token_ok {
                    return ResponseTemplate::new(401).set_body_json(
                        serde_json::json!({ "message": "the request token is missing" }),
                    );
                }
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "value": "cb-oidc-token-material" }))
            })
            .mount(&server)
            .await;

        unsafe {
            std::env::set_var("ACTIONS_ID_TOKEN_REQUEST_URL", server.uri());
            std::env::set_var("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "the-bridge-token");
        }
        let token = actions_token("cargobike")
            .await
            .expect("the exchange mints");
        assert_eq!(token.expose_secret(), "cb-oidc-token-material");
        // The bearer-auth header shape: the exchange's request carries
        // the request token (asserted inside the mock); the URL's
        // audience param too (the query_param matcher).
        unsafe {
            std::env::remove_var("ACTIONS_ID_TOKEN_REQUEST_URL");
            std::env::remove_var("ACTIONS_ID_TOKEN_REQUEST_TOKEN");
        }
    }
}
