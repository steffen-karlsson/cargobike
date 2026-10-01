//! The server client (PRD §5's API surface, F-145's resolution + F-98's
//! refusals materialised at the call site): one bearer token per auth
//! shape, RFC 9457 problem details on failure.

use crate::config::{AuthConfig, Resolved, SecretRef};
use secrecy::{ExposeSecret as _, SecretString};

/// A reqwest-backed client for one resolved context.
pub struct Client {
    http: reqwest::Client,
    base: String,
    bearer: Option<SecretString>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Client")
            .field("base", &self.base)
            .finish()
    }
}

/// Why a call failed; the CLI prints the message + exit code.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    /// The auth shape could not resolve (file/env/exec).
    #[error("{0}")]
    Auth(String),
    /// The HTTP machinery refused (TLS, DNS, timeout).
    #[error("{0}")]
    Transport(String),
    /// The server answered a problem details document (RFC 9457).
    #[error("the server refused: {status} {title} {detail}")]
    Refused {
        status: u16,
        title: String,
        detail: String,
    },
    /// The bearer is not valid (401 specifically).
    #[error("{0}")]
    Unauthorized(String),
}

impl Client {
    /// Builds the resolved target into a client (F-98's refusal already
    /// ran).
    pub fn new(resolved: &Resolved) -> Result<Self, CallError> {
        let mut http = reqwest::Client::builder()
            .user_agent(concat!("cargobike-cli/", env!("CARGO_PKG_VERSION")));
        if let Some(bundle_path) = &resolved.ca_file {
            let bundle = pem_bytes(bundle_path)?;
            let certificates =
                reqwest::Certificate::from_pem_bundle(&bundle).map_err(|failure| {
                    CallError::Auth(format!("the CA bundle refused to load: {failure}"))
                })?;
            for certificate in certificates {
                http = http.add_root_certificate(certificate);
            }
        }
        Ok(Self {
            http: http
                .build()
                .map_err(|failure| CallError::Transport(failure.to_string()))?,
            base: resolved.url.trim_end_matches('/').to_owned(),
            bearer: bearer_for(&resolved.auth)?,
        })
    }

    /// The folder the commands call.
    pub async fn get_json(&self, path: &str) -> Result<serde_json::Value, CallError> {
        self.send(reqwest::Method::GET, path, None).await
    }

    /// POSTs a JSON body.
    pub async fn post_json(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }

    /// DELETEs the path (204/200 both fine).
    pub async fn delete(&self, path: &str) -> Result<serde_json::Value, CallError> {
        self.send(reqwest::Method::DELETE, path, None).await
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, CallError> {
        let mut request = self.http.request(method, format!("{}{}", self.base, path));
        if let Some(token) = &self.bearer {
            request = request.bearer_auth(token.expose_secret());
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|failure| CallError::Transport(failure.to_string()))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let body: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        if status.as_u16() >= 400 {
            let title = body
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let detail = body
                .get("detail")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let refused = CallError::Refused {
                status: status.as_u16(),
                title,
                detail: if detail.is_empty() {
                    if text.is_empty() {
                        format!("{status}")
                    } else {
                        text
                    }
                } else {
                    detail
                },
            };
            return Err(if status.as_u16() == 401 {
                CallError::Unauthorized(format!("the server did not accept the token ({refused})"))
            } else {
                refused
            });
        }
        Ok(body)
    }
}

/// The bearer token per auth shape (F-93's four surfaces).
fn bearer_for(auth: &AuthConfig) -> Result<Option<SecretString>, CallError> {
    Ok(match auth {
        AuthConfig::None | AuthConfig::GithubActions { .. } => None,
        AuthConfig::ApiKey { api_key } => Some(materialise_ref(api_key)?),
        AuthConfig::Exec { command, args } => {
            let output = std::process::Command::new(command)
                .args(args)
                .stdin(std::process::Stdio::null())
                .output()
                .map_err(|failure| CallError::Auth(format!("the exec auth refused: {failure}")))?;
            if !output.status.success() {
                return Err(CallError::Auth(format!(
                    "the exec auth returned {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim_end()
                )));
            }
            let token = String::from_utf8(output.stdout)
                .map_err(|failure| CallError::Auth(format!("the exec auth's output: {failure}")))?
                .trim_end()
                .to_owned();
            if token.is_empty() || token.contains('\n') {
                return Err(CallError::Auth(
                    "the exec auth must print ONLY the token on stdout".to_owned(),
                ));
            }
            Some(SecretString::from(token))
        }
    })
}

/// The `{ file }`/`{ env }` materialisation (F-146's CLI half).
fn materialise_ref(reference: &SecretRef) -> Result<SecretString, CallError> {
    match reference {
        SecretRef::File { file } => {
            let text = std::fs::read_to_string(file).map_err(|failure| {
                CallError::Auth(format!(
                    "the `{}` key file refused: {failure}",
                    file.display()
                ))
            })?;
            Ok(SecretString::from(text.trim_end().to_owned()))
        }
        SecretRef::Env { env } => {
            let value = std::env::var(env).map_err(|_| {
                CallError::Auth(format!("the `{env}` environment variable is unset"))
            })?;
            Ok(SecretString::from(value))
        }
    }
}

fn pem_bytes(path: &std::path::Path) -> Result<Vec<u8>, CallError> {
    std::fs::read(path)
        .map_err(|failure| CallError::Auth(format!("the CA bundle refused: {failure}")))
}

/// The client failure's exit code (§6.2's client vocabulary).
pub fn exit_code(failure: &CallError) -> i32 {
    match failure {
        CallError::Auth(_) | CallError::Unauthorized(_) => 10,
        CallError::Transport(_) => 11,
        CallError::Refused { .. } => 12,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthConfig, OutputFormat, Resolved};

    #[test]
    fn test_bearer_for_each_auth_shape() {
        // None && github-actions: no bearer until the 4.7 exchange.
        assert!(bearer_for(&AuthConfig::None).unwrap().is_none());
        assert!(
            bearer_for(&AuthConfig::GithubActions { audience: None })
                .unwrap()
                .is_none()
        );

        // env reference.
        unsafe { std::env::set_var("CB_TEST_TOKEN", "cb-tok-1") };
        let reference = SecretRef::Env {
            env: String::from("CB_TEST_TOKEN"),
        };
        assert_eq!(
            bearer_for(&AuthConfig::ApiKey { api_key: reference })
                .unwrap()
                .unwrap()
                .expose_secret(),
            "cb-tok-1"
        );
        unsafe { std::env::remove_var("CB_TEST_TOKEN") };

        // Missing env refuses with the vocabulary.
        let failure = bearer_for(&AuthConfig::ApiKey {
            api_key: SecretRef::Env {
                env: String::from("CB_TEST_TOKEN_MISSING"),
            },
        })
        .expect_err("refuses");
        assert!(failure.to_string().contains("unset"));
    }

    #[test]
    fn test_the_exec_contract_prints_only_the_token() {
        let auth = AuthConfig::Exec {
            command: String::from("/bin/sh"),
            args: vec![String::from("-c"), String::from("printf 'tok'")],
        };
        assert_eq!(bearer_for(&auth).unwrap().unwrap().expose_secret(), "tok");
        // A multi-line output is a contract violation.
        let auth = AuthConfig::Exec {
            command: String::from("/bin/sh"),
            args: vec![String::from("-c"), String::from("printf 'a\\nb'")],
        };
        let failure = bearer_for(&auth).expect_err("refuses");
        assert!(failure.to_string().contains("ONLY the token"));
    }

    #[test]
    fn test_the_client_carries_the_authorization_header() {
        let resolved = Resolved {
            url: "http://localhost:8080".to_owned(),
            auth: AuthConfig::ApiKey {
                api_key: SecretRef::File {
                    file: std::path::PathBuf::from("/tmp/cb-cfgtest-key"),
                },
            },
            ca_file: None,
            output: OutputFormat::Table,
        };
        std::fs::write("/tmp/cb-cfgtest-key", "cb-key-1").expect("write key");
        let client = Client::new(&resolved).expect("builds");
        assert!(client.bearer.is_some());
        std::fs::remove_file("/tmp/cb-cfgtest-key").expect("cleanup");
    }
}
