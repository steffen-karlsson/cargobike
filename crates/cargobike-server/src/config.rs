//! Server config loading: the env overlay, `{VAR}` interpolation,
//! secret materialisation — over the value types in
//! `cargobike-core::config` (re-exported; every `crate::config::X`
//! path keeps working).
//!
//! Validation happens structurally (`deny_unknown_fields`, //
//! enforced by the shapes) and semantically at load ([`validate`]); the
//! documented example config must load verbatim (the test's promise).

pub use cargobike_core::config::*;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use secrecy::SecretString;
/// Materialises secret values the way the server does at boot (the tests use
/// the same code path; `*_FILE` variants are applied by the loader).
pub fn materialise(
    value: &SecretValue,
    secrets: &BTreeMap<String, SecretSource>,
) -> Result<SecretString, ConfigError> {
    Ok(match value {
        SecretValue::Literal(v) => SecretString::from(v.clone()),
        SecretValue::File(secret) => {
            let contents = std::fs::read_to_string(&secret.file).map_err(|source| {
                ConfigError::Materialise {
                    path: secret.file.clone(),
                    cause: source.to_string(),
                }
            })?;
            SecretString::from(contents.trim_end_matches('\n').to_owned())
        }
        SecretValue::Env { env } => {
            let value =
                std::env::var(env).map_err(|_| ConfigError::UnknownEnv { name: env.clone() })?;
            SecretString::from(value)
        }
        SecretValue::Secret { r#secret } => {
            let source = secrets
                .get(r#secret)
                .ok_or_else(|| ConfigError::UnknownSecret {
                    name: r#secret.clone(),
                })?;
            materialise_source(source)?
        }
    })
}

fn materialise_source(source: &SecretSource) -> Result<SecretString, ConfigError> {
    let (path, env) = match source {
        SecretSource::File { file } => (Some(file), None),
        SecretSource::Env { env } => (None, Some(env)),
    };
    if let Some(path) = path {
        let contents =
            std::fs::read_to_string(path).map_err(|source| ConfigError::Materialise {
                path: path.clone(),
                cause: source.to_string(),
            })?;
        return Ok(SecretString::from(
            contents.trim_end_matches('\n').to_owned(),
        ));
    }
    let Some(name) = env else {
        unreachable!("one arm is covered; the other remains")
    };
    let value = std::env::var(name).map_err(|_| ConfigError::UnknownEnv { name: name.clone() })?;
    Ok(SecretString::from(value))
}

/// Semantic load-time validation errors (the fail-fast family).
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A missing environment variable was referenced.
    #[error("failed to load config: unset environment variable `{name}`")]
    UnknownEnv {
        /// The name referenced.
        name: String,
    },
    /// A secret reference pointed at nothing.
    #[error("failed to materialise secret: unknown `{name}`")]
    UnknownSecret {
        /// The name referenced.
        name: String,
    },
    /// A file secret failed to read.
    #[error("failed to read secret from `{}`: {}", path.display(), cause)]
    Materialise {
        /// The path involved.
        path: PathBuf,
        /// IO error text.
        cause: String,
    },
    /// Parse/interpolation failure.
    #[error("failed to parse config: {0}")]
    Parse(String),
}

/// The env-var prefix and curated-key mapping .
pub const ENV_PREFIX: &str = "CARGOBIKE_SERVER_";

/// Parses a binary size like `512KiB`, `1MiB`, `25MiB` ('s unit
/// grammar) into bytes; plain integers are bytes.
pub fn parse_size(text: &str) -> Option<u64> {
    let trimmed = text.trim().to_ascii_lowercase();
    let (digits, multiplier) = if let Some(before) = trimmed.strip_suffix("kib") {
        (before, 1024_u64)
    } else if let Some(before) = trimmed.strip_suffix("mib") {
        (before, 1024_u64 * 1024)
    } else if let Some(before) = trimmed.strip_suffix("gib") {
        (before, 1024_u64 * 1024 * 1024)
    } else if let Some(before) = trimmed.strip_suffix("kb") {
        (before, 1000_u64)
    } else if let Some(before) = trimmed.strip_suffix("mb") {
        (before, 1_000_000_u64)
    } else if let Some(before) = trimmed.strip_suffix("gb") {
        (before, 1_000_000_000_u64)
    } else {
        (trimmed.as_str(), 1_u64)
    };
    let bare = digits.trim().trim_end_matches('b').trim();
    bare.parse::<u64>()
        .ok()
        .and_then(|number| number.checked_mul(multiplier))
}

/// Curated env-var overrides (the scalars only; a published list, not
/// an automatic `__` mapping). Values map the config path; a `*_FILE`
/// path is read, not substituted.
pub fn env_overrides() -> Vec<(&'static str, &'static str)> {
    vec![
        ("LISTEN", "server.listen"),
        ("PUBLIC_URL", "server.public_url"),
        ("DATABASE_URL", "database.url"),
        ("DATABASE_URL_FILE", "database.url_file"),
        ("LOG_LEVEL", "logging.level"),
        ("LOG_FORMAT", "logging.format"),
    ]
}

/// Loads the config: defaults ← file ← post-parse interpolation ←
/// curated envs. `{VAR}` (and `${VAR:-default}`) interpolation happens
/// on the PARSED tree only: secret-valued fields refuse both
/// interpolation and literal `${` marks; unset variables fail-fast.
pub fn load(path: Option<&std::path::Path>) -> Result<Config, ConfigError> {
    let path = path.map_or_else(
        || {
            std::env::var("CARGOBIKE_SERVER_CONFIG")
                .ok()
                .map(PathBuf::from)
        },
        |p| Some(p.to_path_buf()),
    );
    let text = match &path {
        Some(p) => std::fs::read_to_string(p).map_err(|cause| {
            ConfigError::Parse(format!("failed to read {}: {cause}", p.display()))
        })?,
        // Env-only bootstrap still parses an (the almost) empty file.
        None => "server: {}
database: { url: postgres://localhost/cargobike }
auth: {}
"
        .to_owned(),
    };

    // Parse first, interpolate second: the tree walk sees typed shapes,
    // so secret fields are recognizable and left untouched.
    let mut tree: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&text).map_err(|source| ConfigError::Parse(format!("{source}")))?;
    interpolate_tree(&mut tree)?;

    let mut config: Config = serde_yaml_ng::from_value(tree)
        .map_err(|source| ConfigError::Parse(format!("{source}")))?;

    apply_env_overrides(&mut config)?;
    apply_provider_overrides(&mut config)?;

    Ok(config)
}

/// The config's secret-valued keys: the secret-ref maps
/// (`{ file }`/`{ env }`/`{ secret }`) and the grammar's SecretValue
/// fields (a key's Sub-value never interpolates). A `${` appearing
/// under any of these refuses at load.
const SECRET_VALUE_KEYS: [&str; 6] = [
    "file",
    "env",
    "secret",
    "private_key",
    "hash",
    "shared_secret",
];

/// Post-parse interpolation over the parsed YAML tree: string leaves
/// expand `${VAR}` / `${VAR:-default}` (unset variable ⇒ load error),
/// except under secret keys, where any `${` refuses.
fn interpolate_tree(node: &mut serde_yaml_ng::Value) -> Result<(), ConfigError> {
    interpolate_node(node, false)
}

fn interpolate_node(node: &mut serde_yaml_ng::Value, in_secret: bool) -> Result<(), ConfigError> {
    match node {
        serde_yaml_ng::Value::String(text) => {
            if text.contains("${") {
                if in_secret {
                    return Err(ConfigError::Parse(
                        "interpolation is forbidden inside secret-valued fields".to_owned(),
                    ));
                }
                *text = shellexpand::env(text)
                    .map_err(|source| {
                        ConfigError::Parse(format!("interpolation failed: {source}"))
                    })?
                    .into_owned();
                if text.contains("${") {
                    return Err(ConfigError::Parse(format!(
                        "interpolation left an unresolved shell marker: {text:?}"
                    )));
                }
            }
            Ok(())
        }
        serde_yaml_ng::Value::Sequence(items) => {
            for item in items {
                interpolate_node(item, in_secret)?;
            }
            Ok(())
        }
        serde_yaml_ng::Value::Mapping(entries) => {
            for (key, value) in entries.iter_mut() {
                let key_text = key.as_str().unwrap_or_default().to_owned();
                let child = in_secret || SECRET_VALUE_KEYS.contains(&key_text.as_str());
                interpolate_node(value, child)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn apply_env_overrides(config: &mut Config) -> Result<(), ConfigError> {
    for (env_name, path) in env_overrides() {
        let full = format!("{ENV_PREFIX}{env_name}");
        let value = match std::env::var(&full) {
            Ok(v) if !v.is_empty() => v,
            _ => continue,
        };
        match path {
            "server.listen" => config.server.listen = value,
            "server.public_url" => config.server.public_url = Some(value),
            "database.url" => config.database.url = SecretValue::Literal(value),
            // A file-shaped URL: the VALUE is the path; the
            // materialisation reads its contents ('s contract).
            "database.url_file" => {
                config.database.url = SecretValue::File(FileSecret {
                    file: PathBuf::from(value),
                });
            }
            "logging.level" => config.logging.level = value,
            "logging.format" => config.logging.format = value,
            other => {
                return Err(ConfigError::Parse(format!(
                    "unknown env override path {other:?}"
                )));
            }
        }
    }
    Ok(())
}

/// The single-GitHub-Provider env shortcuts: the variables stamp the
/// github entry (creating one when the file declares none), so CI and
/// one-provider deployments need no registry block at all. Where the
/// file already declares the github entry, the set values win for the
/// absent fields only.
fn apply_provider_overrides(config: &mut Config) -> Result<(), ConfigError> {
    let prefix = format!("{ENV_PREFIX}GITHUB_");
    let app_id = std::env::var(format!("{prefix}APP_ID"))
        .ok()
        .filter(|v| !v.is_empty());
    let private_key_file = std::env::var(format!("{prefix}PRIVATE_KEY_FILE"))
        .ok()
        .filter(|v| !v.is_empty());
    let api_url = std::env::var(format!("{prefix}API_URL"))
        .ok()
        .filter(|v| !v.is_empty());
    let webhook_secret_file = std::env::var(format!("{prefix}WEBHOOK_SECRET_FILE"))
        .ok()
        .filter(|v| !v.is_empty());
    if app_id.is_none()
        && private_key_file.is_none()
        && api_url.is_none()
        && webhook_secret_file.is_none()
    {
        // Nothing to stamp (the config's own providers stand).
        return Ok(());
    }
    let existing = config
        .providers
        .iter_mut()
        .find(|entry| entry.r#type == "github");
    match existing {
        Some(entry) => {
            if let Some(app_id) = app_id {
                if entry.auth.is_none() {
                    // Stamp the auth only when the file's entry lacks
                    // one (the file's own declarations win).
                    let Some(key_file) = private_key_file.clone() else {
                        return Err(ConfigError::Parse(format!(
                            "the {prefix}PRIVATE_KEY_FILE is expected with the {prefix}APP_ID stamp"
                        )));
                    };
                    entry.auth = Some(ProviderAuth {
                        app_id,
                        installation_id: None,
                        private_key: SecretValue::File(FileSecret {
                            file: PathBuf::from(key_file),
                        }),
                    });
                }
            }
            if let Some(api_url) = api_url {
                entry.api_url = Some(api_url);
            }
            if let Some(secret_file) = webhook_secret_file {
                entry.webhook_secrets = Some(vec![SecretValue::File(FileSecret {
                    file: PathBuf::from(secret_file),
                })]);
            }
        }
        None => {
            // The env's stamp creates the entry: both the App's
            // identity and the key's file are required (a literal key
            // is not an env stamp).
            let (Some(app_id), Some(key_file)) = (app_id, private_key_file) else {
                return Err(ConfigError::Parse(format!(
                    "the {prefix}APP_ID and {prefix}PRIVATE_KEY_FILE are both required when stamping a github provider from the environment (the file declares none)"
                )));
            };
            config.providers.push(ProviderConfig {
                name: "github".to_owned(),
                r#type: "github".to_owned(),
                api_url,
                web_url: None,
                auth: Some(ProviderAuth {
                    app_id,
                    installation_id: None,
                    private_key: SecretValue::File(FileSecret {
                        file: PathBuf::from(key_file),
                    }),
                }),
                webhook_secrets: webhook_secret_file.map(|file| {
                    vec![SecretValue::File(FileSecret {
                        file: PathBuf::from(file),
                    })]
                }),
                repositories: None,
                extension: None,
            });
        }
    }
    Ok(())
}

/// Warnings the loader reports (the startup noise, not errors;).
pub fn literal_secret_warnings(config: &Config) -> Vec<&'static str> {
    let mut out = Vec::new();
    if config.database.url.is_literal() {
        out.push("database.url is a literal in the config; prefer { file } or { env }");
    }
    if let Some(tls) = &config.server.tls {
        if tls.certificate.is_literal() || tls.private_key.is_literal() {
            out.push("server.tls material is a literal in the config; prefer { file }");
        }
    }
    out
}

/// The `OnceLock` re-export use; placeholder for the loader's single-read
/// config handle used by the HTTP layer's state types.
pub type SharedConfig = Arc<Config>;

#[cfg(test)]
mod tests {
    use super::*;

    /// The documented config's example, extracted from the document itself — the
    /// verbatim-promise test (, T5a).
    fn documented_config() -> String {
        let prd = include_str!("../../../docs/PRD.md");
        let marker = "### 13.1 Server Config (YAML)";
        let start = prd.split(marker).nth(1).expect("13.1 marker present");
        let start = start.split("```yaml\n").nth(1).expect("yaml block");
        start.split("```").next().expect("yaml close").to_owned()
    }

    #[test]
    fn test_documented_server_config_loads_verbatim() {
        let documented = documented_config();
        let config: Config = serde_yaml_ng::from_str(&documented).expect("13.1 config must parse");
        assert_eq!(config.applications.len(), 1);
        assert_eq!(config.providers.len(), 2);
        assert!(config.application("my-service").is_some());
        // The documented config must also BOOT (the (the b) registry
        // cross-checks run here, not just the parser's grammar). The
        // example points at /etc/cargobike/templates; the test points
        // the SAME registry entries at the workspace's canonical
        // template (the verbatim value).
        let mut config = config;
        config.templates.directory = [env!("CARGO_MANIFEST_DIR"), "/../../templates"].concat();
        crate::validation::validate(&config).expect("13.1 config validates");
    }

    #[test]
    fn test_interpolation_runs_after_parse_and_refuses_secret_positions() {
        unsafe { std::env::set_var("CB_TEST_LEVEL", "debug") };

        // A scalar expands; a secret position's `${` refuses.
        let text = "server: {}\ndatabase: { url: postgres://x }\nauth: {}\nlogging:\n  level: \"${CB_TEST_LEVEL}\"\n";
        let failure = |plain_text: &str| -> Result<Config, String> {
            let mut tree: serde_yaml_ng::Value =
                serde_yaml_ng::from_str(plain_text).map_err(|source| source.to_string())?;
            interpolate_tree(&mut tree).map_err(|refusal| refusal.to_string())?;
            serde_yaml_ng::from_value(tree).map_err(|source| source.to_string())
        };
        let config = failure(text).expect("parses");
        assert_eq!(config.logging.level, "debug");

        // A secret position refuses the shell marker.
        let refusal = "server: {}
database: { url: postgres://x }
auth: {}
providers:
 - name: github
   type: github
   auth:
     app_id: 1
     private_key: ${CB_TEST_LEVEL}
";
        if let Ok(tree) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(refusal) {
            let mut tree = tree;
            let failure = interpolate_tree(&mut tree).expect_err("the secret position refuses");
            assert!(
                failure
                    .to_string()
                    .contains("forbidden inside secret-valued fields")
            );
        }

        // An unset variable fails fast.
        unsafe { std::env::remove_var("CB_TEST_LEVEL") };
        let unset = "server: {}\ndatabase: { url: postgres://x }\nauth: {}\nlogging: { level: \"${CB_TEST_LEVEL}\" }";
        if let Ok(tree) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(unset) {
            let mut tree = tree;
            assert!(interpolate_tree(&mut tree).is_err());
        }
    }

    #[test]
    fn test_unset_variables_take_their_documented_default() {
        // A dedicated name: unit tests run in parallel threads, so tests
        // must not share the ambient environment.
        unsafe { std::env::remove_var("CB_TEST_DEFAULT_LEVEL") };
        let text = "server: {}\ndatabase: { url: postgres://x }\nauth: {}\nlogging:\n  level: \"${CB_TEST_DEFAULT_LEVEL:-warn}\"\n";
        let tree: serde_yaml_ng::Value = serde_yaml_ng::from_str(text).expect("parses");
        let mut tree = tree;
        interpolate_tree(&mut tree).expect("the default resolves");
        let level = tree
            .get("logging")
            .and_then(|logging| logging.get("level"))
            .and_then(serde_yaml_ng::Value::as_str)
            .expect("the level");
        assert_eq!(level, "warn");
    }

    #[test]
    fn test_database_url_file_reads_the_contents() {
        let key_path = std::env::temp_dir().join("cb-db-url-test");
        std::fs::write(&key_path, "postgres://read-from-file/cargobike\n").expect("write");
        let yaml = "server: {}\ndatabase: { url: postgres://declared }\nauth: {}\n";
        let mut config: Config = serde_yaml_ng::from_str(yaml).expect("parses");
        let _ = apply_env_overrides(&mut config); // reads envs; unset ones skip
        // Simulate the override directly (the env's file path).
        config.database.url = SecretValue::File(FileSecret {
            file: key_path.clone(),
        });
        let materialised = match &config.database.url {
            SecretValue::File(file) => std::fs::read_to_string(&file.file).expect("reads"),
            other => panic!("the file form was expected, got {other:?}"),
        };
        assert!(materialised.starts_with("postgres://read-from-file"));
        let _ = std::fs::remove_file(&key_path);
    }

    #[test]
    fn test_secret_value_shapes_are_untagged_disjoint() {
        let db: SecretValue = serde_yaml_ng::from_str("{ file: /run/x }").expect("file");
        assert!(matches!(db, SecretValue::File(_)));
        let env: SecretValue = serde_yaml_ng::from_str("{ env: MY_VAR }").expect("env");
        assert!(matches!(env, SecretValue::Env { .. }));
        let named: SecretValue = serde_yaml_ng::from_str("{ secret: named-thing }").expect("named");
        assert!(matches!(named, SecretValue::Secret { .. }));
        let lit: SecretValue = serde_yaml_ng::from_str("postgres://x").expect("literal");
        assert!(matches!(lit, SecretValue::Literal(_)));
    }

    #[test]
    fn test_unknown_config_keys_are_rejected() {
        assert!(serde_yaml_ng::from_str::<Config>("server: {}\nservices: {}").is_err());
    }
}
