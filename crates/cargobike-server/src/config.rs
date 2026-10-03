//! Server config value types and the loader (,
//! .., a naming rules).
//!
//! Validation happens structurally (`deny_unknown_fields`, //
//! enforced by the shapes) and semantically at load ([`validate`]); the
//! documented example config must load verbatim (the test's promise).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use secrecy::SecretString;
use serde::{Deserialize, Serialize};

/// The whole server config (the top-level sections).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// HTTP surface.
    pub server: ServerSection,
    /// System database (Cargobike's own tables; also DBOS's when the
    /// schema is shared).
    pub database: DatabaseSection,
    /// Leader election (, one section).
    #[serde(default)]
    pub leader_election: LeaderElectionSection,
    /// Who may call the API (OIDC trust entries + API keys).
    pub auth: AuthSection,
    /// Named secrets referenced by template steps as `{ secret: <name> }`.
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretSource>,
    /// Git providers (the github + extension-served).
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    /// Sidecar extensions (the transport + channel auth;).
    #[serde(default)]
    pub extensions: Vec<ExtensionConfig>,
    /// Pipeline templates directory.
    #[serde(default)]
    pub templates: TemplatesSection,
    /// Interpreter/engine knobs .
    #[serde(default)]
    pub engine: EngineSection,
    /// Reconciler .
    #[serde(default)]
    pub reconciler: ReconcilerSection,
    /// Retention .
    #[serde(default)]
    pub retention: RetentionSection,
    /// Rate limits and body sizes .
    #[serde(default)]
    pub limits: LimitsSection,
    /// Egress/SSRF policy .
    #[serde(default)]
    pub network: NetworkSection,
    /// Logging .
    #[serde(default)]
    pub logging: LoggingSection,
    /// Optional metrics port .
    #[serde(default)]
    pub metrics: MetricsSection,
    /// The application registry (; the security core).
    #[serde(default)]
    pub applications: Vec<ApplicationEntry>,
    /// Application groups for monorepo discovery .
    #[serde(default)]
    pub application_groups: Vec<ApplicationGroupEntry>,
}

/// Defers the "one `default: serde` problem" — sections with defaults only.
impl Config {
    /// The application by name, registry entry or group-discovered name.
    pub fn application(&self, name: &str) -> Option<&ApplicationEntry> {
        self.applications.iter().find(|a| a.name == name)
    }
}

/// HTTP surface .
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    /// Bind address, `0.0.0.0:8080` .
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Externally visible URL (the webhooks, CR links, `Location`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    /// Graceful shutdown window while steps are in flight.
    #[serde(default = "default_shutdown_timeout")]
    pub shutdown_timeout: String,
    /// Optional built-in TLS; otherwise TLS terminates at a proxy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsSection>,
}

fn default_listen() -> String {
    "0.0.0.0:8080".to_owned()
}

fn default_shutdown_timeout() -> String {
    "30s".to_owned()
}

/// Built-in TLS .
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsSection {
    /// Certificate (the secret-shaped).
    pub certificate: SecretValue,
    /// Private key (the secret-shaped).
    pub private_key: SecretValue,
}

/// Database pool .
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseSection {
    /// Secret-shaped connection string (the literal one is warned at startup).
    pub url: SecretValue,
    /// Pool cap (the default 10).
    #[serde(default = "default_pool")]
    pub max_connections: u32,
    /// The DBOS system tables' schema (default `dbos`); a deployment's
    /// own namespace. Test suites point each boot at its own schema so
    /// one boot's recovery cannot adopt another's abandoned workflow
    /// rows (the CI flake's disease).
    #[serde(default = "default_dbos_schema")]
    pub dbos_schema: String,
}

fn default_dbos_schema() -> String {
    "dbos".to_owned()
}

fn default_pool() -> u32 {
    10
}

/// One section for leader election : enabled + dedicated URL.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LeaderElectionSection {
    /// On by default ; off simplifies local tests.
    pub enabled: bool,
    /// Must bypass PgBouncer transaction pooling ; defaults to `database.url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database_url: Option<SecretValue>,
}

impl Default for LeaderElectionSection {
    fn default() -> Self {
        Self {
            enabled: true,
            database_url: None,
        }
    }
}

/// Authentication section (auth.oidc + auth.api_keys).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthSection {
    /// OIDC trust entries; single-direction binding .
    #[serde(default)]
    pub oidc: Vec<OidcEntry>,
    /// Hashed API keys; single-direction binding like the OIDC entries .
    #[serde(default)]
    pub api_keys: Vec<ApiKeyEntry>,
}

/// One OIDC trust entry (..).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcEntry {
    /// Name other sections reference: `releasers: [{ oidc: <name> }]`.
    pub name: String,
    /// Exact issuer match.
    pub issuer: String,
    /// Audience the token must carry.
    pub audience: String,
    /// Claim constraints; glob by default .
    #[serde(default)]
    pub claims: BTreeMap<String, serde_json::Value>,
    /// Grants mapped by this entry .
    #[serde(default)]
    pub grants: Vec<String>,
    /// Unconstrained entries are refused without this .
    #[serde(default)]
    pub allow_unconstrained: bool,
    /// nbf/iat skew allowance (the default 60s;).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_skew: Option<String>,
    /// JWKS override for issuers without usable discovery .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwks_url: Option<String>,
    /// Algorithm allowlist per issuer .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub algorithms: Option<Vec<String>>,
}

/// One configurable API key .
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiKeyEntry {
    /// Name referenced by `releasers: [{ api_key: <name> }]`.
    pub name: String,
    /// Argon2id hash to compare against .
    pub hash: String,
    /// Grants for the key .
    #[serde(default)]
    pub grants: Vec<String>,
    /// Rotation expiry (the date).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    /// Free description for audit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Where a named secret's value comes from ; only file/env — a
/// secret cannot point at another secret.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum SecretSource {
    /// Path on disk: `{ file: /x }`.
    File { file: PathBuf },
    /// Environment variable: `{ env: NAME }`.
    Env { env: String },
}

/// A secret-shaped value: a literal (the discouraged), `{ file }`, `{ env }`
/// or `{ secret: <name> }` .
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum SecretValue {
    /// Literal string in the config — warned at startup in secret slots.
    Literal(String),
    /// Path on disk.
    File(FileSecret),
    /// Environment name reference.
    Env { env: String },
    /// Reference into `secrets:` .
    Secret { r#secret: String },
}

/// A path-typed file secret so `{ file: /x }` stays unambiguous.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct FileSecret {
    /// Path on disk.
    pub file: PathBuf,
}

impl<'de> Deserialize<'de> for SecretValue {
    /// The derive's untagged form rejects matches unevenly across shapes; a
    /// hand impl keeps literals and `{ file }`/`{ env }`/`{ secret }` maps
    /// distinct (the one-shape promise, tested by the verbatim example).
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Literal(String),
            Map {
                file: Option<PathBuf>,
                env: Option<String>,
                secret: Option<String>,
            },
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(match raw {
            Raw::Literal(value) => SecretValue::Literal(value),
            Raw::Map {
                file: Some(file), ..
            } => SecretValue::File(FileSecret { file }),
            Raw::Map { env: Some(env), .. } => SecretValue::Env { env },
            Raw::Map {
                secret: Some(secret),
                ..
            } => SecretValue::Secret { r#secret },
            _ => {
                return Err(serde::de::Error::custom(
                    "secret value: expected a string, { file }, { env } or { secret }",
                ));
            }
        })
    }
}

impl SecretValue {
    /// Renders the value for safe display: literals redact, references name.
    pub fn describe(&self) -> String {
        match self {
            SecretValue::Literal(_) => "<literal>".to_owned(),
            SecretValue::File(value) => format!("file:{}", value.file.to_string_lossy()),
            SecretValue::Env { env } => format!("env:{env}"),
            SecretValue::Secret { r#secret } => format!("secret:{secret}"),
        }
    }

    /// Whether the value is a literal (the startup warning;).
    pub fn is_literal(&self) -> bool {
        matches!(self, SecretValue::Literal(_))
    }
}

impl std::fmt::Debug for SecretValue {
    /// Hand-written Debug: never prints a literal .
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.describe())
    }
}

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

/// A git provider — github or extension-served.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Name referenced by `RepoRef.provider` .
    pub name: String,
    /// `github` or `extension` .
    pub r#type: String,
    /// For `type: github`: API base (GitHub Enterprise Server support).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
    /// For `type: github`: the web UI base for CR links.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_url: Option<String>,
    /// For `type: github`: App authentication .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ProviderAuth>,
    /// Webhook secrets; list allows two for rotation .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_secrets: Option<Vec<SecretValue>>,
    /// Defence-in-depth repository globs; authorization uses IDs .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repositories: Option<RepositorySet>,
    /// For `type: extension`: the extensions entry that serves it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension: Option<String>,
}

/// GitHub App authentication block .
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAuth {
    /// The App ID, string .
    pub app_id: String,
    /// The App's installation the provider drives (the per-repo
    /// installation lookup is pending work; until then the config
    /// pins one installation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation_id: Option<String>,
    /// Private key (the secret-shaped).
    pub private_key: SecretValue,
}

/// Repository allow/deny globs .
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RepositorySet {
    /// Allow globs.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Deny globs.
    #[serde(default)]
    pub deny: Vec<String>,
}

/// One sidecar extension: transport, channel auth, what it provides .
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionConfig {
    /// Name referenced by `providers[].extension`.
    pub name: String,
    /// `http://…`, `https://…` or `unix://…` .
    pub endpoint: String,
    /// `json` (the default) or `grpc` .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// Channel auth: `shared_secret` or `tls` .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ExtensionAuth>,
    /// The action step types this sidecar implements (`acme/notify@1`).
    #[serde(default)]
    pub provides: ExtensionProvides,
}

/// What a sidecar provides (, : providers come from `providers[]`).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ExtensionProvides {
    /// Versioned step type names served by this extension.
    #[serde(default)]
    pub step_types: Vec<String>,
}

/// Sidecar channel authentication — the map shape of the documented
/// config (`auth: { shared_secret: { file } }` or `auth: { tls: {...} }`).
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum ExtensionAuth {
    /// A pre-shared secret header in both directions.
    SharedSecret { shared_secret: SecretValue },
    /// mTLS pinning.
    Tls {
        ca_file: PathBuf,
        cert_file: PathBuf,
        key_file: PathBuf,
    },
}

/// Templates directory .
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TemplatesSection {
    /// The default: `/etc/cargobike/templates`.
    pub directory: String,
}

fn default_templates() -> String {
    "/etc/cargobike/templates".to_owned()
}

impl Default for TemplatesSection {
    fn default() -> Self {
        Self {
            directory: default_templates(),
        }
    }
}

/// Engine knobs .
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EngineSection {
    /// Step-output cap .
    pub max_step_output: String,
    /// Branch name format ; placeholders per .
    pub branch_format: String,
    /// CEL cost cap .
    pub cel_max_cost: u64,
    /// CEL expression length cap .
    pub cel_max_expression_length: u64,
}

impl Default for EngineSection {
    fn default() -> Self {
        Self {
            max_step_output: "1MiB".to_owned(),
            branch_format: "cargobike/{application}/{environment}/{release_id}".to_owned(),
            cel_max_cost: 2_000_000,
            cel_max_expression_length: 4_096,
        }
    }
}

/// Reconciler .
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ReconcilerSection {
    /// Default 5m .
    pub interval: String,
    /// Batched lookup size .
    pub batch_size: u32,
}

impl Default for ReconcilerSection {
    fn default() -> Self {
        Self {
            interval: "5m".to_owned(),
            batch_size: 100,
        }
    }
}

/// Retention .
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RetentionSection {
    /// Event-log retention (the default 365d).
    pub events: String,
    /// Raw webhook payloads (the personal data; default 30d).
    pub webhook_payloads: String,
    /// Optional automatic deletion of terminal releases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub releases: Option<String>,
}

impl Default for RetentionSection {
    fn default() -> Self {
        Self {
            events: "365d".to_owned(),
            webhook_payloads: "30d".to_owned(),
            releases: None,
        }
    }
}

/// Limits per surface .
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LimitsSection {
    /// REST API limits.
    pub api: SurfaceLimits,
    /// Webhook endpoint limits.
    pub webhooks: SurfaceLimits,
}

impl Default for LimitsSection {
    fn default() -> Self {
        Self {
            api: SurfaceLimits {
                max_body_size: "1MiB".to_owned(),
                rate: "100/s".to_owned(),
                burst: Some(200),
            },
            webhooks: SurfaceLimits {
                max_body_size: "25MiB".to_owned(),
                rate: "50/s".to_owned(),
                burst: Some(100),
            },
        }
    }
}

/// One surface's limits (the per-surface grammar `<count>/<s|m|h>`).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SurfaceLimits {
    /// Body size cap, e.g. `1MiB` .
    pub max_body_size: String,
    /// Sustained rate like `100/s`.
    pub rate: String,
    /// Optional burst allowance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<u32>,
}

impl Default for SurfaceLimits {
    fn default() -> Self {
        Self {
            max_body_size: "1MiB".to_owned(),
            rate: "100/s".to_owned(),
            burst: None,
        }
    }
}

/// Egress/SSRF policy .
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkSection {
    /// Egress guard.
    pub egress: EgressSection,
}

/// The SSRF deny-list, allow exceptions and optional proxy (, proxy).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressSection {
    /// Deny CIDRs; built-in when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<Vec<String>>,
    /// Optional allowlist exceptions on the private net.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Vec<String>,
    /// Explicit egress proxy; otherwise proxy env vars are ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
}

/// Logging .
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LoggingSection {
    /// Single level (the default `info`).
    pub level: String,
    /// `json` (the default) or `text`.
    pub format: String,
}

impl Default for LoggingSection {
    fn default() -> Self {
        Self {
            level: "info".to_owned(),
            format: "json".to_owned(),
        }
    }
}

/// Optional metrics port .
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MetricsSection {
    /// Separate scrape port; metrics stay off the public API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
}

/// One registry application entry . The shapes follow the
/// documented example verbatim (the test's promise).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationEntry {
    /// Application name in the registry .
    pub name: String,
    /// Human description .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Filter labels .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,
    /// Where the version comes from; `source` replaces as a whole on
    /// `extends` .
    pub source: HashMapEntrySource,
    /// Registry-only template reference .
    pub template: String,
    /// Version validation + tag policy .
    #[serde(default)]
    pub versioning: VersioningEntry,
    /// Tag-push triggers ; `event: tag` .
    #[serde(default)]
    pub triggers: Vec<TriggerEntry>,
    /// Who may release: principal selectors , single-direction.
    #[serde(default)]
    pub releasers: Vec<PrincipalSelector>,
    /// Group inheritance . An app of this name must have been
    /// discovered by the group or validation fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    /// Generic app-wide inputs supplied to templates as `${{ inputs.<name> }}`
    /// .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<BTreeMap<String, serde_json::Value>>,
    /// Application freeze .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<PausedEntry>,
    /// Per-environment config .
    pub environments: BTreeMap<String, EnvironmentEntry>,
}

/// A principal selector: `oidc: <name>` or `api_key: <name>` (the a).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum PrincipalSelector {
    /// References an `auth.oidc` entry by name.
    Oidc { oidc: String },
    /// References an `auth.api_keys` entry by name.
    ApiKey { api_key: String },
}

/// A source repo entry: provider + immutable id + optional verified path.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HashMapEntrySource {
    /// Provider name.
    pub provider: String,
    /// Immutable provider ID .
    pub id: String,
    /// Verified label, checked at startup .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Group-source owner id — present only on group `source` blocks;
    /// per-app `source` replaces the whole object .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
}

/// Alias the source-type to swap between group and app shapes cleanly.
pub type HashMapEntrySourceAlias = HashMapEntrySource;

/// The versioning block of an application or group .
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct VersioningEntry {
    /// Scheme name: `semver`, `calver`, `opaque` .
    pub scheme: String,
    /// Tag format like `"v{version}"` (the placeholders).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag_format: Option<String>,
    /// Tag existence required for creates (the default true).
    pub require_tag: bool,
    /// CalVer layout .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calver_format: Option<String>,
}

impl Default for VersioningEntry {
    fn default() -> Self {
        Self {
            scheme: "opaque".to_owned(),
            tag_format: None,
            require_tag: true,
            calver_format: None,
        }
    }
}

/// A tag-push trigger .
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerEntry {
    /// Trigger event; only `tag` in v1 (the use `event`, not `on`).
    pub event: String,
    /// Refuse unprotected tags unless explicitly allowed .
    #[serde(default = "default_true")]
    pub require_tag_protection: bool,
    /// Optional sender constraints (the principal selectors;).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub senders: Option<Vec<PrincipalSelector>>,
}

fn default_true() -> bool {
    true
}

/// Application-level pause .
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PausedEntry {
    /// Boolean pause, no reason.
    Bool(bool),
    /// Pause with a shown reason .
    Reason { reason: String },
}

/// One environment's registry config .
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentEntry {
    /// Target repo, opaque — well-known `env.inputs.repo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<HashMapEntrySource>,
    /// File edits — well-known `env.inputs.edits` .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edits: Option<Vec<EditEntry>>,
    /// Concurrency policy (the registry-only per).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<ConcurrencyValue>,
    /// fail closed when capability is available.
    #[serde(default = "default_true")]
    pub require_branch_protection: bool,
    /// explicit bypass mechanism for template direct commits.
    #[serde(default)]
    pub allow_direct_commit: bool,
    /// Custom CR title/body/labels/draft .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_request: Option<ChangeRequestOverrides>,
    /// Custom commit message .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_message: Option<String>,
    /// Generic per-environment inputs .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<BTreeMap<String, serde_json::Value>>,
    /// Approval policy block .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalPolicy>,
    /// Environment freeze .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<PausedEntry>,
}

/// Registry-level edit shape .
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EditEntry {
    /// Repository-relative file; `{application}` placeholders allowed .
    pub file: String,
    /// Inferred from extension when omitted .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Dot-notation field path (`image.tag`).
    pub field: String,
    /// Optional value; defaults to the release version .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// Concurrency policy value .
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConcurrencyValue {
    /// Close the old CR and take the lease over .
    Supersede,
    /// Queue behind an active release .
    Queue,
    /// Refuse with `ConcurrencyRejected` .
    Reject,
}

/// Custom CR shaping per environment .
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ChangeRequestOverrides {
    /// Custom title; placeholders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Custom body; placeholders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Appended to template labels .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Vec<String>,
    /// Open as draft.
    #[serde(default)]
    pub draft: bool,
}

/// Approval policy per environment .
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ApprovalPolicy {
    /// Distinct approver count (the default 1).
    pub required: u32,
    /// Self-approval default `false` .
    pub allow_self_approval: bool,
    /// Machine approvers default off; `api_key` selectors refuse.
    pub allow_machine_approvers: bool,
    /// Principal selectors (the any match counts).
    pub approvers: Vec<PrincipalSelector>,
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self {
            required: 1,
            allow_self_approval: false,
            allow_machine_approvers: false,
            approvers: Vec::new(),
        }
    }
}

/// One application group .
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationGroupEntry {
    /// Group name referenced by `extends`.
    pub name: String,
    /// Registry-only template .
    pub template: String,
    /// Versioning defaults .
    #[serde(default)]
    pub versioning: VersioningEntry,
    /// Group triggers inherited by discovered apps .
    #[serde(default)]
    pub triggers: Vec<TriggerEntry>,
    /// Who may release discovered apps.
    #[serde(default)]
    pub releasers: Vec<PrincipalSelector>,
    /// The org the group releases from (`owner_id`;).
    pub source: HashMapEntrySource,
    /// Discovery configuration.
    pub discovery: DiscoveryEntry,
    /// Per-environment defaults for discovered apps.
    pub environments: BTreeMap<String, EnvironmentEntry>,
}

/// Discovery configuration ().
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryEntry {
    /// The repository scanned.
    pub repo: HashMapEntrySource,
    /// Match pattern with `{application}` and `{environment}` captures.
    pub match_: String,
    /// Branch scanned (the default: repository default branch); protect it .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_: Option<String>,
    /// Rescan interval (the default 10m).
    #[serde(default = "default_discovery_interval")]
    pub interval: String,
}

fn default_discovery_interval() -> String {
    "10m".to_owned()
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
