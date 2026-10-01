//! The CLI's config file: named contexts,
//! auth kinds, and the `flag > env > context > defaults` precedence.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The CLI config file's root ( `CARGOBIKE_CONFIG`,
/// `~/.config/cargobike/config.yaml`).
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    /// The context selected by the file; `cargobike context use` rewrites it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_context: Option<String>,
    /// The named servers.
    #[serde(default)]
    pub contexts: Vec<ContextConfig>,
    /// Cross-context defaults .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<DefaultsConfig>,
}

/// One server context.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextConfig {
    /// The name a `cargobike context use <name>` selects by.
    pub name: String,
    /// The server's base URL; `http://` is refused unless the host is a
    /// local one (, `--allow-http` bypasses).
    pub url: String,
    /// A custom CA bundle for corporate roots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_file: Option<PathBuf>,
    /// How the client proves itself (theirs vocabulary).
    #[serde(default)]
    pub auth: AuthConfig,
}

/// The user-facing output default (`table|json|yaml`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum OutputFormat {
    /// Human tables (the default).
    #[default]
    Table,
    /// Machine-readable JSON.
    Json,
    /// Machine-readable YAML.
    Yaml,
}

/// The config's `defaults:` section.
#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultsConfig {
    /// How commands render.
    #[serde(default)]
    pub output: OutputFormat,
}

/// The client auth vocabulary (/ shapes).
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum AuthConfig {
    /// No auth (a dev server without the auth layer).
    #[default]
    None,
    /// An API key's secret reference (`{ file }`/`{ env }` only — the
    /// CLI reads no server-side named secrets).
    ApiKey {
        /// The bearer's material nested under `api_key:` .
        api_key: SecretRef,
    },
    /// A local command that prints ONLY the token on stdout (TSV).
    Exec {
        /// The command (the from the local config only — never a server hint).
        command: String,
        /// The command's arguments.
        #[serde(default)]
        args: Vec<String>,
    },
    /// GitHub Actions: the OIDC endpoint's request-token exchange
    /// (ACTIONS_ID_TOKEN_REQUEST_URL, ACTIONS_ID_TOKEN_REQUEST_TOKEN).
    GithubActions {
        /// The `aud` the Actions token requests (the default `cargobike`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audience: Option<String>,
    },
}

/// A bearer-shaped secret reference (the file/env forms only here).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum SecretRef {
    /// `{ file: /path/to/key }`.
    File {
        /// Path on disk.
        file: PathBuf,
    },
    /// `{ env: NAME }`.
    Env {
        /// Environment variable name.
        env: String,
    },
}

impl<'de> Deserialize<'de> for SecretRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            File { file: PathBuf },
            Env { env: String },
        }
        match Raw::deserialize(deserializer)? {
            Raw::File { file } => Ok(Self::File { file }),
            Raw::Env { env } => Ok(Self::Env { env }),
        }
    }
}

/// The config's load path contract: explicit path, else the XDG config;
/// an absent file is a valid empty config (theirs env-only setup).
pub fn config_path(environment: Option<String>, home: PathBuf) -> PathBuf {
    if let Some(path) = environment {
        return PathBuf::from(path);
    }
    let mut path = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    path.extend(["cargobike", "config.yaml"]);
    path
}

/// Reads the config file; a missing file defaults to the empty config.
pub fn load(path: &PathBuf) -> Result<ConfigFile, ConfigError> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(ConfigFile::default());
    };
    serde_yaml_ng::from_str(&text).map_err(|failure| {
        ConfigError(format!(
            "the config file `{}` did not parse: {failure}",
            path.display()
        ))
    })
}

/// Parses a config's text (the tests and diagnostics).
pub fn parse(text: &str) -> Result<ConfigFile, ConfigError> {
    serde_yaml_ng::from_str(text).map_err(|failure| ConfigError(failure.to_string()))
}

/// The serialised rewrite of the config after `context use` (the file's
/// shape preserved as close as YAML round-trips).
pub fn save(path: &PathBuf, config: &ConfigFile) -> Result<(), ConfigError> {
    let text =
        serde_yaml_ng::to_string(config).map_err(|failure| ConfigError(failure.to_string()))?;
    std::fs::write(path, text).map_err(|failure| {
        ConfigError(format!(
            "the config file `{}` refused to write: {failure}",
            path.display()
        ))
    })
}

/// Why a config refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct ConfigError(pub String);

/// The URL pin a flag/env supplied (the precedence walk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlOverride {
    /// The URL text (the unparsed until the refusal rule runs).
    pub value: String,
    /// Whether `--allow-http` blesses plain http.
    pub allow_http: bool,
}

/// refusal: `http://` only for local hosts.
pub fn refusal_error(url: &str) -> ConfigError {
    ConfigError(format!(
        "the server URL `{url}` uses plain http; only local hosts may (override with --allow-http)"
    ))
}

/// Whether the URL's host is local (the http rule's exception).
pub fn is_local_host(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "::1" | "0.0.0.0"
    ) || host.starts_with("127.")
}

/// The URL the CLI would talk to: the refused cases return the error.
/// Parses the URL text first so the check is scheme+host-based.
pub fn resolved_url(url_text: &str, allow_http: bool) -> Result<String, ConfigError> {
    let parsed = url::Url::parse(url_text).map_err(|failure| {
        ConfigError(format!(
            "the server URL `{url_text}` is malformed: {failure}"
        ))
    });
    let parsed = parsed?;
    match parsed.scheme() {
        "https" => Ok(url_text.to_owned()),
        "http" if allow_http || is_local_host(parsed.host_str().unwrap_or_default()) => {
            Ok(url_text.to_owned())
        }
        "http" => Err(refusal_error(url_text)),
        other => Err(ConfigError(format!(
            "the server URL `{url_text}` uses the unsupported scheme `{other}`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_a_context_file_roundtrips_the_example() {
        let text = r#"
current_context: production
contexts:
  - name: production
    url: https://cargobike.example.com
    ca_file: /etc/ssl/corp-ca.pem
    auth:
      type: exec
      command: az
      args: [account, get-access-token, --scope, "api://cargobike/.default", --query, accessToken, -o, tsv]
  - name: local
    url: http://localhost:8080
    auth:
      type: api-key
      api_key: { file: ~/.config/cargobike/local-key }
defaults:
  output: table
"#;
        let config = parse(text).expect("parses");
        assert_eq!(config.current_context.as_deref(), Some("production"));
        assert_eq!(config.contexts.len(), 2);
        assert_eq!(
            config.contexts.first().expect("there").auth,
            AuthConfig::Exec {
                command: "az".to_owned(),
                args: vec![
                    "account".to_owned(),
                    "get-access-token".to_owned(),
                    "--scope".to_owned(),
                    "api://cargobike/.default".to_owned(),
                    "--query".to_owned(),
                    "accessToken".to_owned(),
                    "-o".to_owned(),
                    "tsv".to_owned(),
                ],
            }
        );
        assert_eq!(
            config.contexts.get(1).expect("there").auth,
            AuthConfig::ApiKey {
                api_key: SecretRef::File {
                    file: PathBuf::from("~/.config/cargobike/local-key"),
                },
            }
        );
        assert_eq!(
            config.defaults.clone().expect("there").output,
            OutputFormat::Table
        );
    }

    #[test]
    fn test_github_actions_auth_configures() {
        let config = parse(
            "contexts:\n  - name: ci\n    url: https://cargobike.example.com\n    auth:\n      type: github-actions\n      audience: my-cargobike\n",
        )
        .expect("parses");
        assert_eq!(
            config.contexts.first().expect("there").auth,
            AuthConfig::GithubActions {
                audience: Some("my-cargobike".to_owned()),
            }
        );
    }

    #[test]
    fn test_url_refusals_match() {
        // Local http passes without a flag.
        assert_eq!(
            resolved_url("http://localhost:8080", false).as_deref(),
            Ok("http://localhost:8080")
        );
        assert_eq!(
            resolved_url("http://127.0.0.1:8080", false).as_deref(),
            Ok("http://127.0.0.1:8080")
        );
        // Remote http refuses.
        assert!(resolved_url("http://cargobike.example.com", false).is_err());
        // The flag blesses it.
        assert_eq!(
            resolved_url("http://cargobike.example.com", true).as_deref(),
            Ok("http://cargobike.example.com")
        );
        // https always passes.
        assert_eq!(
            resolved_url("https://cargobike.example.com", false).as_deref(),
            Ok("https://cargobike.example.com")
        );
        // Other schemes refuse.
        assert!(resolved_url("ftp://cargobike.example.com", true).is_err());
    }

    #[test]
    fn test_is_local_host_covers_the_loopback_family() {
        assert!(is_local_host("localhost"));
        assert!(is_local_host("127.0.0.2"));
        assert!(is_local_host("LOCALHOST"));
        assert!(is_local_host("::1"));
        assert!(!is_local_host("cargobike.internal"));
    }

    #[test]
    fn test_resolve_walks_the_precedence() {
        let config = parse(
            "current_context: production\ncontexts:\n  - name: production\n    url: https://cargobike.example.com\n    auth:\n      type: api-key\n      api_key: { file: /tmp/key }\n",
        )
        .expect("parses");
        let resolved = resolve(&config, None, false, None).expect("resolves");
        assert_eq!(resolved.url, "https://cargobike.example.com");
        assert!(matches!(resolved.auth, AuthConfig::ApiKey { .. }));
        // Flag over context .
        let resolved = resolve(
            &config,
            Some("http://localhost:8080".to_owned()),
            false,
            None,
        )
        .expect("resolves");
        assert_eq!(resolved.url, "http://localhost:8080");
        // No context + no env + not Actions => refuses to guess.
        let resolved = resolve(&ConfigFile::default(), None, false, None);
        assert!(resolved.is_err());
        // The refusal rule applies to context urls too.
        let hostile = parse(
            "contexts:\n  - name: prod\n    url: http://cargobike.example.com\n    auth: {type: none}\n",
        )
        .expect("parses");
        let resolved = resolve(&hostile, None, false, None);
        assert!(resolved.is_err(), "remote http refuses without the flag");
    }

    #[test]
    fn test_config_path_honours_xdg_and_the_env_var() {
        let home = PathBuf::from("/home/ska");
        assert_eq!(
            config_path(None, home.clone()),
            PathBuf::from("/home/ska/.config/cargobike/config.yaml")
        );
        let explicit = config_path(Some("/etc/cargobike-cli.yaml".to_owned()), home.clone());
        assert_eq!(explicit, PathBuf::from("/etc/cargobike-cli.yaml"));
    }
}

/// The resolved target the commands talk to (the precedence walk
/// finally materialised). `context_url` is `None` when no context was
/// selected; `default_url` when neither the flag nor env supplied one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The effective URL (the refusal-checked).
    pub url: String,
    /// The effective auth.
    pub auth: AuthConfig,
    /// The CA bundle from the context.
    pub ca_file: Option<PathBuf>,
    /// The output format (the defaults' section).
    pub output: OutputFormat,
}

/// precedence: flag > env > context > defaults > built-in.
pub fn resolve(
    config: &ConfigFile,
    flag_url: Option<String>,
    allow_http: bool,
    selected: Option<String>,
) -> Result<Resolved, ConfigError> {
    if let Some(text) = flag_url {
        return resolved_from(text, None, config, allow_http);
    }
    let context_name = selected
        .or_else(|| config.current_context.clone())
        .or_else(|| single_context(config));
    let found = context_name.and_then(|name| {
        config
            .contexts
            .iter()
            .find(|candidate| candidate.name == name)
            .cloned()
    });
    resolved_from(
        found
            .as_ref()
            .map_or(String::new(), |found| found.url.clone()),
        found,
        config,
        allow_http,
    )
}

/// The refusal-walk from one URL source (the context may be absent —
/// `CARGOBIKE_URL`-only setups with Actions or no auth).
fn resolved_from(
    url_source: String,
    context: Option<ContextConfig>,
    config: &ConfigFile,
    allow_http: bool,
) -> Result<Resolved, ConfigError> {
    if url_source.is_empty() {
        return Err(ConfigError(
            "no server url supplied: pass --url, set CARGOBIKE_URL, or configure a context"
                .to_owned(),
        ));
    }
    if let Some(context) = context {
        return Ok(Resolved {
            url: resolved_url(url_source.as_str(), allow_http)?,
            auth: context.auth.clone(),
            ca_file: context.ca_file.clone(),
            output: config
                .defaults
                .as_ref()
                .map_or_else(OutputFormat::default, |found| found.output),
        });
    }
    let auth = if crate::ci::actions_detected() {
        AuthConfig::GithubActions { audience: None }
    } else {
        AuthConfig::None
    };
    Ok(Resolved {
        url: resolved_url(url_source.as_str(), allow_http)?,
        auth,
        ca_file: None,
        output: config
            .defaults
            .as_ref()
            .map_or_else(OutputFormat::default, |found| found.output),
    })
}

/// One-context configs select it without `current_context`.
fn single_context(config: &ConfigFile) -> Option<String> {
    if config.contexts.len() == 1 && config.current_context.is_none() {
        return config.contexts.first().map(|found| found.name.clone());
    }
    None
}
