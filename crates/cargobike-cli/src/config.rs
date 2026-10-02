//! The CLI's config file: named contexts,
//! auth kinds, and the `flag > env > context > defaults` precedence.

use std::path::{Path, PathBuf};

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
        let resolved = resolve(&config, &Overrides::default()).expect("resolves");
        assert_eq!(resolved.url, "https://cargobike.example.com");
        assert!(matches!(resolved.auth, AuthConfig::ApiKey { .. }));
        // Flag over context .
        let resolved = resolve(
            &config,
            &Overrides {
                url: Some("http://localhost:8080".to_owned()),
                allow_http: false,
                ..Overrides::default()
            },
        )
        .expect("resolves");
        assert_eq!(resolved.url, "http://localhost:8080");
        // No context + no env + not Actions => refuses to guess.
        let resolved = resolve(&ConfigFile::default(), &Overrides::default());
        assert!(resolved.is_err());
        // The refusal rule applies to context urls too.
        let hostile = parse(
            "contexts:\n  - name: prod\n    url: http://cargobike.example.com\n    auth: {type: none}\n",
        )
        .expect("parses");
        let resolved = resolve(&hostile, &Overrides::default());
        assert!(resolved.is_err(), "remote http refuses without the flag");
    }

    #[test]
    fn test_the_auth_override_switches_shapes() {
        unsafe { std::env::remove_var("CARGOBIKE_API_KEY_FILE") };
        let config = parse(
            "current_context: prod\ncontexts:\n  - name: prod\n    url: https://cargobike.example.com\n    auth: {type: none}\n",
        )
        .expect("parses");
        // none + api-key + github-actions overrides on a no-auth context.
        let resolved = resolve(
            &config,
            &Overrides {
                auth: Some("github-actions".to_owned()),
                audience: Some("aud-1".to_owned()),
                ..Overrides::default()
            },
        )
        .expect("resolves");
        assert!(matches!(
            resolved.auth,
            AuthConfig::GithubActions { audience: Some(a) } if a == "aud-1"
        ));
        // api-key rides the CARGOBIKE_API_KEY env ref when no file is set.
        let resolved = resolve(
            &config,
            &Overrides {
                auth: Some("api-key".to_owned()),
                ..Overrides::default()
            },
        )
        .expect("resolves");
        assert!(matches!(
            resolved.auth,
            AuthConfig::ApiKey { api_key: SecretRef::Env { env } } if env == "CARGOBIKE_API_KEY"
        ));
        // The exec override refuses on a non-exec context.
        let resolved = resolve(
            &config,
            &Overrides {
                auth: Some("exec".to_owned()),
                ..Overrides::default()
            },
        );
        assert!(resolved.is_err(), "exec needs an exec context");
        // Unknown vocab is a refusal.
        let resolved = resolve(
            &config,
            &Overrides {
                auth: Some("tpm".to_owned()),
                ..Overrides::default()
            },
        );
        assert!(resolved.is_err());
    }

    #[test]
    fn test_the_ca_and_tilde_overrides() {
        let config = parse(
            "contexts:\n  - name: prod\n    url: https://cargobike.example.com\n    ca_file: /etc/ca.pem\n",
        )
        .expect("parses");
        // A plain context path passes through; the flag's tilde expands.
        let resolved = resolve(
            &config,
            &Overrides {
                ca_file: Some(PathBuf::from("~/certs/ca.pem")),
                ..Overrides::default()
            },
        )
        .expect("resolves");
        let ca = resolved.ca_file.expect("the ca file");
        assert!(!ca.display().to_string().starts_with('~'), "expanded");
        // The env output format flows at the env level (restored after).
        let restored = std::env::var("CARGOBIKE_OUTPUT").ok();
        unsafe { std::env::set_var("CARGOBIKE_OUTPUT", "yaml") };
        let resolved = resolve(
            &config,
            &Overrides {
                output: None,
                ..Overrides::default()
            },
        )
        .expect("resolves");
        assert!(matches!(resolved.output, OutputFormat::Yaml));
        if let Some(restored) = restored {
            unsafe { std::env::set_var("CARGOBIKE_OUTPUT", restored) };
        } else {
            unsafe { std::env::remove_var("CARGOBIKE_OUTPUT") };
        }
    }

    #[test]
    fn test_config_path_honours_xdg_and_the_env_var() {
        // The ambient XDG variable varies by machine (GitHub's runners
        // export one); the test controls it and asserts both branches.
        let restored_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };

        let home = PathBuf::from("/home/ska");
        assert_eq!(
            config_path(None, home.clone()),
            PathBuf::from("/home/ska/.config/cargobike/config.yaml")
        );
        let explicit = config_path(Some("/etc/cargobike-cli.yaml".to_owned()), home.clone());
        assert_eq!(explicit, PathBuf::from("/etc/cargobike-cli.yaml"));

        unsafe {
            match restored_xdg {
                Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
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

/// The CLI's flag-level overrides (the flag beats the env, the env
/// beats the selected context, the context beats `defaults`).
#[derive(Debug, Default, Clone)]
pub struct Overrides {
    /// `--url` / `CARGOBIKE_URL`.
    pub url: Option<String>,
    /// `--allow-http`.
    pub allow_http: bool,
    /// `--context` / `CARGOBIKE_CONTEXT`.
    pub context: Option<String>,
    /// `--auth` / `CARGOBIKE_AUTH`: the auth's type override
    /// (`none`, `api-key`, `exec`, `github-actions`).
    pub auth: Option<String>,
    /// `--audience` / `CARGOBIKE_AUDIENCE`; the Actions' token's aud.
    pub audience: Option<String>,
    /// `--ca-file` / `CARGOBIKE_CA_FILE`; a CA bundle for the context.
    pub ca_file: Option<PathBuf>,
    /// `-o` / `CARGOBIKE_OUTPUT`.
    pub output: Option<OutputFormat>,
}

/// A tilde-expanding path read: the `~/`-prefixed forms join the HOME.
pub fn tilde(path: &Path) -> PathBuf {
    if let Some(rest) = path.display().to_string().strip_prefix("~/") {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .map(|home| (!home.is_empty()).then_some(home));
        if let Ok(Some(home)) = home {
            return PathBuf::from(home).join(rest);
        }
    }
    path.to_path_buf()
}

/// The `--auth`/`CARGOBIKE_AUTH`'s text: one of the four shapes' names.
fn auth_override_text(text: &str) -> Option<String> {
    match text {
        "none" | "api-key" | "exec" | "github-actions" => Some(text.to_owned()),
        _ => None,
    }
}

/// The walk's one stop: the context's state (its config's presence)
/// turns into the resolved target; the flag-level overrides apply to
/// whatever shape fell out.
pub fn resolve(config: &ConfigFile, overrides: &Overrides) -> Result<Resolved, ConfigError> {
    let resolved = if let Some(text) = overrides.url.clone().filter(|text| !text.is_empty()) {
        resolved_from(text, None, config, overrides.allow_http)?
    } else {
        let context_name = overrides
            .context
            .clone()
            .or_else(|| {
                std::env::var("CARGOBIKE_CONTEXT")
                    .ok()
                    .filter(|name| !name.is_empty())
            })
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
            overrides.allow_http,
        )?
    };

    apply_auth_override(resolved, overrides)
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
            ca_file: context.ca_file.clone().map(|path| tilde(&path)),
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

/// The auth override's walk: the flag (`--auth`) beats the env
/// (`CARGOBIKE_AUTH`) beats the context's own shape.
fn apply_auth_override(
    mut resolved: Resolved,
    overrides: &Overrides,
) -> Result<Resolved, ConfigError> {
    let auth_text = overrides.auth.clone().or_else(|| {
        std::env::var("CARGOBIKE_AUTH")
            .ok()
            .filter(|value| !value.is_empty())
    });
    if let Some(text) = auth_text {
        let Some(text) = auth_override_text(&text) else {
            return Err(ConfigError(format!(
                "the auth override `{text}` is not one of none|api-key|exec|github-actions"
            )));
        };
        resolved.auth = match text.as_str() {
            "none" => AuthConfig::None,
            "api-key" => AuthConfig::ApiKey {
                api_key: api_key_override_ref(),
            },
            "exec" => match resolved.auth {
                AuthConfig::Exec { .. } => resolved.auth,
                other => {
                    return Err(ConfigError(format!(
                        "the exec override needs an exec context ({:?} was resolved)",
                        std::mem::discriminant(&other)
                    )));
                }
            },
            "github-actions" => AuthConfig::GithubActions {
                audience: overrides.audience.clone(),
            },
            _ => unreachable!("the override's vocabulary is checked above"),
        };
    }
    // The Actions' audience: the flag's value rides the github-actions
    // shape (the ci fallback applies an unset audience later).
    if let (AuthConfig::GithubActions { audience }, Some(flag)) =
        (&mut resolved.auth, overrides.audience.clone())
    {
        *audience = Some(flag);
    }
    // The CA override (the flag's path wins over the context's).
    if let Some(ca_file) = overrides.ca_file.clone() {
        resolved.ca_file = Some(tilde(&ca_file));
    }
    // The output override: the flag's value wins; the env's
    // (`CARGOBIKE_OUTPUT`) fills in otherwise.
    resolved.output = overrides
        .output
        .or_else(output_from_env)
        .unwrap_or(resolved.output);
    Ok(resolved)
}

/// The `CARGOBIKE_OUTPUT`'s value to the format (unknown texts are
/// ignored; the default stands).
fn output_from_env() -> Option<OutputFormat> {
    let text = std::env::var("CARGOBIKE_OUTPUT")
        .ok()
        .filter(|value| !value.is_empty())?;
    match text.as_str() {
        "table" => Some(OutputFormat::Table),
        "json" => Some(OutputFormat::Json),
        "yaml" => Some(OutputFormat::Yaml),
        _ => None,
    }
}

/// The `api-key` override's material ref: the FILE form (the key's on
/// disk) wins over the environment form (`CARGOBIKE_API_KEY`).
fn api_key_override_ref() -> SecretRef {
    let path =
        std::env::var("CARGOBIKE_API_KEY_FILE").map(|path| (!path.is_empty()).then_some(path));
    if let Ok(Some(path)) = path {
        return SecretRef::File {
            file: tilde(std::path::Path::new(&path)),
        };
    }
    SecretRef::Env {
        env: "CARGOBIKE_API_KEY".to_owned(),
    }
}

/// One-context configs select it without `current_context`.
fn single_context(config: &ConfigFile) -> Option<String> {
    if config.contexts.len() == 1 && config.current_context.is_none() {
        return config.contexts.first().map(|found| found.name.clone());
    }
    None
}
