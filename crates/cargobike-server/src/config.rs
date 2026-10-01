//! Server config value types and the loader (PRD §13.1, F-124..F-129,
//! F-134..F-147, §2a naming rules).
//!
//! Validation happens structurally (`deny_unknown_fields`, R1/R9/R12
//! enforced by the shapes) and semantically at load ([`validate`]); the
//! documented example config must load verbatim (F-32's test promise).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use secrecy::SecretString;
use serde::{Deserialize, Serialize};

/// The whole server config (top-level sections, F-124).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// HTTP surface.
    pub server: ServerSection,
    /// System database (Cargobike's own tables; also DBOS's when the
    /// schema is shared).
    pub database: DatabaseSection,
    /// Leader election (F-136, one section).
    #[serde(default)]
    pub leader_election: LeaderElectionSection,
    /// Who may call the API (OIDC trust entries + API keys).
    pub auth: AuthSection,
    /// Named secrets referenced by template steps as `{ secret: <name> }`.
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretSource>,
    /// Git providers (github + extension-served).
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    /// Sidecar extensions (transport + channel auth; F-121).
    #[serde(default)]
    pub extensions: Vec<ExtensionConfig>,
    /// Pipeline templates directory.
    #[serde(default)]
    pub templates: TemplatesSection,
    /// Interpreter/engine knobs (F-137).
    #[serde(default)]
    pub engine: EngineSection,
    /// Reconciler (F-138).
    #[serde(default)]
    pub reconciler: ReconcilerSection,
    /// Retention (F-139).
    #[serde(default)]
    pub retention: RetentionSection,
    /// Rate limits and body sizes (F-118).
    #[serde(default)]
    pub limits: LimitsSection,
    /// Egress/SSRF policy (F-117).
    #[serde(default)]
    pub network: NetworkSection,
    /// Logging (F-140).
    #[serde(default)]
    pub logging: LoggingSection,
    /// Optional metrics port (F-141).
    #[serde(default)]
    pub metrics: MetricsSection,
    /// The application registry (§4.13; the security core).
    #[serde(default)]
    pub applications: Vec<ApplicationEntry>,
    /// Application groups for monorepo discovery (§4.13a).
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

/// HTTP surface (F-128, F-134).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    /// Bind address, `0.0.0.0:8080` (F-128).
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Externally visible URL (webhooks, CR links, `Location`).
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

/// Built-in TLS (F-134).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsSection {
    /// Certificate (secret-shaped).
    pub certificate: SecretValue,
    /// Private key (secret-shaped).
    pub private_key: SecretValue,
}

/// Database pool (F-135).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseSection {
    /// Secret-shaped connection string (literal one is warned at startup).
    pub url: SecretValue,
    /// Pool cap (default 10).
    #[serde(default = "default_pool")]
    pub max_connections: u32,
}

fn default_pool() -> u32 {
    10
}

/// One section for leader election (F-136): enabled + dedicated URL.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LeaderElectionSection {
    /// On by default (F-136); off simplifies local tests.
    pub enabled: bool,
    /// Must bypass PgBouncer transaction pooling (F-131); defaults to `database.url`.
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

/// Authentication section (auth.oidc + auth.api_keys, §9.2/9.3).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthSection {
    /// OIDC trust entries; single-direction binding (F-80, F-99a).
    #[serde(default)]
    pub oidc: Vec<OidcEntry>,
    /// Hashed API keys; single-direction binding like the OIDC entries (F-85).
    #[serde(default)]
    pub api_keys: Vec<ApiKeyEntry>,
}

/// One OIDC trust entry (F-77..F-81).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcEntry {
    /// Name other sections reference: `releasers: [{ oidc: <name> }]`.
    pub name: String,
    /// Exact issuer match.
    pub issuer: String,
    /// Audience the token must carry.
    pub audience: String,
    /// Claim constraints; glob by default (R7).
    #[serde(default)]
    pub claims: BTreeMap<String, serde_json::Value>,
    /// Grants mapped by this entry (F-99).
    #[serde(default)]
    pub grants: Vec<String>,
    /// Unconstrained entries are refused without this (F-80).
    #[serde(default)]
    pub allow_unconstrained: bool,
    /// nbf/iat skew allowance (default 60s; F-77).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_skew: Option<String>,
    /// JWKS override for issuers without usable discovery (F-77).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwks_url: Option<String>,
    /// Algorithm allowlist per issuer (F-78).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub algorithms: Option<Vec<String>>,
}

/// One configurable API key (F-85, F-99b).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiKeyEntry {
    /// Name referenced by `releasers: [{ api_key: <name> }]`.
    pub name: String,
    /// Argon2id hash to compare against (§9.3).
    pub hash: String,
    /// Grants for the key (F-99).
    #[serde(default)]
    pub grants: Vec<String>,
    /// Rotation expiry (date).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    /// Free description for audit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Where a named secret's value comes from (F-146); only file/env — a
/// secret cannot point at another secret.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum SecretSource {
    /// Path on disk: `{ file: /x }`.
    File { file: PathBuf },
    /// Environment variable: `{ env: NAME }`.
    Env { env: String },
}

/// A secret-shaped value: a literal (discouraged), `{ file }`, `{ env }`
/// or `{ secret: <name> }` (R5, C13).
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum SecretValue {
    /// Literal string in the config — warned at startup in secret slots.
    Literal(String),
    /// Path on disk.
    File(FileSecret),
    /// Environment name reference.
    Env { env: String },
    /// Reference into `secrets:` (F-146).
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
    /// distinct (R5's one shape promise, tested by the verbatim example).
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

    /// Whether the value is a literal (startup warning; C13).
    pub fn is_literal(&self) -> bool {
        matches!(self, SecretValue::Literal(_))
    }
}

impl std::fmt::Debug for SecretValue {
    /// Hand-written Debug: never prints a literal (F-87).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.describe())
    }
}

/// Materialises secret values the way the server does at boot (tests use
/// the same code path; F-88 `*_FILE` variants are applied by the loader).
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

/// A git provider (F-49, §7.2) — github or extension-served.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Name referenced by `RepoRef.provider` (F-39).
    pub name: String,
    /// `github` or `extension` (F-49, C11).
    pub r#type: String,
    /// For `type: github`: API base (GitHub Enterprise Server support, F-44).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
    /// For `type: github`: the web UI base for CR links.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_url: Option<String>,
    /// For `type: github`: App authentication (§7.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ProviderAuth>,
    /// Webhook secrets; list allows two for rotation (F-51, F-58).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_secrets: Option<Vec<SecretValue>>,
    /// Defence-in-depth repository globs; authorization uses IDs (F-92).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repositories: Option<RepositorySet>,
    /// For `type: extension`: the extensions entry that serves it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension: Option<String>,
}

/// GitHub App authentication block (§7.2, R5).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAuth {
    /// The App ID, string (R6).
    pub app_id: String,
    /// Private key (secret-shaped).
    pub private_key: SecretValue,
}

/// Repository allow/deny globs (F-92).
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

/// One sidecar extension: transport, channel auth, what it provides (C11/C12).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionConfig {
    /// Name referenced by `providers[].extension`.
    pub name: String,
    /// `http://…`, `https://…` or `unix://…` (§7.3).
    pub endpoint: String,
    /// `json` (default) or `grpc` (A.9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// Channel auth: `shared_secret` or `tls` (§7.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ExtensionAuth>,
    /// The action step types this sidecar implements (`acme/notify@1`).
    #[serde(default)]
    pub provides: ExtensionProvides,
}

/// What a sidecar provides (F-49, C11: providers come from `providers[]`).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ExtensionProvides {
    /// Versioned step type names served by this extension.
    #[serde(default)]
    pub step_types: Vec<String>,
}

/// Sidecar channel authentication (§7.3) — the map shape of the documented
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

/// Templates directory (F-129).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TemplatesSection {
    /// Default per PRD: `/etc/cargobike/templates`.
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

/// Engine knobs (F-137).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EngineSection {
    /// Step-output cap (F-40).
    pub max_step_output: String,
    /// Branch name format (A1); placeholders per F-147.
    pub branch_format: String,
    /// CEL cost cap (F-27).
    pub cel_max_cost: u64,
    /// CEL expression length cap (F-27).
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

/// Reconciler (F-138).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ReconcilerSection {
    /// Default 5m (F-55).
    pub interval: String,
    /// Batched lookup size (F-138).
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

/// Retention (F-139).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RetentionSection {
    /// Event-log retention (default 365d, F-114).
    pub events: String,
    /// Raw webhook payloads (personal data; default 30d, F-115).
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

/// Limits per surface (F-118, C14).
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

/// One surface's limits (F-118: type grammar `<count>/<s|m|h>`).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SurfaceLimits {
    /// Body size cap, e.g. `1MiB` (R4).
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

/// Egress/SSRF policy (F-117).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkSection {
    /// Egress guard.
    pub egress: EgressSection,
}

/// The SSRF deny-list, allow exceptions and optional proxy (F-117, C13 proxy).
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

/// Logging (F-140).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LoggingSection {
    /// Single level (default `info`).
    pub level: String,
    /// `json` (default) or `text`.
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

/// Optional metrics port (F-141).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MetricsSection {
    /// Separate scrape port; metrics stay off the public API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
}

/// One registry application entry (§4.13, F-91). The shapes follow the
/// documented example verbatim (F-32's test promise).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationEntry {
    /// Application name in the registry (F-3).
    pub name: String,
    /// Human description (F-142).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Filter labels (F-142).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,
    /// Where the version comes from; `source` replaces as a whole on
    /// `extends` (§4.13a).
    pub source: HashMapEntrySource,
    /// Registry-only template reference (F-5).
    pub template: String,
    /// Version validation + tag policy (F-95).
    #[serde(default)]
    pub versioning: VersioningEntry,
    /// Tag-push triggers (F-54); `event: tag` (R12).
    #[serde(default)]
    pub triggers: Vec<TriggerEntry>,
    /// Who may release: principal selectors (F-99a), single-direction.
    #[serde(default)]
    pub releasers: Vec<PrincipalSelector>,
    /// Group inheritance (§4.13a). An app of this name must have been
    /// discovered by the group or validation fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    /// Generic app-wide inputs supplied to templates as `${{ inputs.<name> }}`
    /// (F-32a, C16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<BTreeMap<String, serde_json::Value>>,
    /// Application freeze (F-142).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<PausedEntry>,
    /// Per-environment config (§4.13).
    pub environments: BTreeMap<String, EnvironmentEntry>,
}

/// A principal selector: `oidc: <name>` or `api_key: <name>` (§2a, F-99b).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum PrincipalSelector {
    /// References an `auth.oidc` entry by name.
    Oidc { oidc: String },
    /// References an `auth.api_keys` entry by name.
    ApiKey { api_key: String },
}

/// A source repo entry: provider + immutable id + optional verified path.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HashMapEntrySource {
    /// Provider name.
    pub provider: String,
    /// Immutable provider ID (R6).
    pub id: String,
    /// Verified label, checked at startup (F-10, C20).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Group-source owner id — present only on group `source` blocks;
    /// per-app `source` replaces the whole object (§4.13a).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
}

/// Alias the source-type to swap between group and app shapes cleanly.
pub type HashMapEntrySourceAlias = HashMapEntrySource;

/// The versioning block of an application or group (F-95).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct VersioningEntry {
    /// Scheme name: `semver`, `calver`, `opaque` (F-4).
    pub scheme: String,
    /// Tag format like `"v{version}"` (F-147 placeholders).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag_format: Option<String>,
    /// Tag existence required for creates (default true).
    pub require_tag: bool,
    /// CalVer layout (F-95).
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

/// A tag-push trigger (F-54, F-82).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerEntry {
    /// Trigger event; only `tag` in v1 (R12: `event`, not `on`).
    pub event: String,
    /// Refuse unprotected tags unless explicitly allowed (F-82).
    #[serde(default = "default_true")]
    pub require_tag_protection: bool,
    /// Optional sender constraints (principal selectors; F-82).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub senders: Option<Vec<PrincipalSelector>>,
}

fn default_true() -> bool {
    true
}

/// Application-level pause (F-142).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PausedEntry {
    /// Boolean pause, no reason.
    Bool(bool),
    /// Pause with a shown reason (C22).
    Reason { reason: String },
}

/// One environment's registry config (§4.13).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentEntry {
    /// Target repo, opaque (F-10/F-11) — well-known `env.inputs.repo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<HashMapEntrySource>,
    /// File edits — well-known `env.inputs.edits` (F-41).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edits: Option<Vec<EditEntry>>,
    /// Concurrency policy (registry-only per R11).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<ConcurrencyValue>,
    /// F-65: fail closed when capability is available.
    #[serde(default = "default_true")]
    pub require_branch_protection: bool,
    /// F-66: explicit bypass mechanism for template direct commits.
    #[serde(default)]
    pub allow_direct_commit: bool,
    /// Custom CR title/body/labels/draft (F-143).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_request: Option<ChangeRequestOverrides>,
    /// Custom commit message (F-143).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_message: Option<String>,
    /// Generic per-environment inputs (F-32a/C16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<BTreeMap<String, serde_json::Value>>,
    /// Approval policy block (F-96).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalPolicy>,
    /// Environment freeze (F-142, C22).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<PausedEntry>,
}

/// Registry-level edit shape (F-41).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditEntry {
    /// Repository-relative file; `{application}` placeholders allowed (§4.13a).
    pub file: String,
    /// Inferred from extension when omitted (F-41).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Dot-notation field path (`image.tag`, F-41).
    pub field: String,
    /// Optional value; defaults to the release version (F-41).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// Concurrency policy value (F-31).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConcurrencyValue {
    /// Close the old CR and take the lease over (F-70).
    Supersede,
    /// Queue behind an active release (F-71).
    Queue,
    /// Refuse with `ConcurrencyRejected` (F-74).
    Reject,
}

/// Custom CR shaping per environment (F-143).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ChangeRequestOverrides {
    /// Custom title; F-147 placeholders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Custom body; F-147 placeholders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Appended to template labels (R11).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Vec<String>,
    /// Open as draft.
    #[serde(default)]
    pub draft: bool,
}

/// Approval policy per environment (F-96).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ApprovalPolicy {
    /// Distinct approver count (default 1).
    pub required: u32,
    /// Self-approval default `false` (R2).
    pub allow_self_approval: bool,
    /// Machine approvers default off; `api_key` selectors refuse.
    pub allow_machine_approvers: bool,
    /// Principal selectors (any match counts).
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

/// One application group (§4.13a).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationGroupEntry {
    /// Group name referenced by `extends`.
    pub name: String,
    /// Registry-only template (F-5).
    pub template: String,
    /// Versioning defaults (F-95).
    #[serde(default)]
    pub versioning: VersioningEntry,
    /// Group triggers inherited by discovered apps (§4.13a).
    #[serde(default)]
    pub triggers: Vec<TriggerEntry>,
    /// Who may release discovered apps.
    #[serde(default)]
    pub releasers: Vec<PrincipalSelector>,
    /// The org the group releases from (`owner_id`; §4.13a F-93).
    pub source: HashMapEntrySource,
    /// Discovery configuration.
    pub discovery: DiscoveryEntry,
    /// Per-environment defaults for discovered apps.
    pub environments: BTreeMap<String, EnvironmentEntry>,
}

/// Discovery configuration (§4.13a, C19).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryEntry {
    /// The repository scanned.
    pub repo: HashMapEntrySource,
    /// Match pattern with `{application}` and `{environment}` captures.
    pub match_: String,
    /// Branch scanned (default: repository default branch); protect it (§4.13a).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_: Option<String>,
    /// Rescan interval (default 10m).
    #[serde(default = "default_discovery_interval")]
    pub interval: String,
}

fn default_discovery_interval() -> String {
    "10m".to_owned()
}

/// Semantic load-time validation errors (F-89's fail-fast family).
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

/// The env-var prefix and curated-key mapping (F-125, R10).
pub const ENV_PREFIX: &str = "CARGOBIKE_SERVER_";

/// Curated env-var overrides (F-125: scalars only; a published list, not
/// an automatic `__` mapping). Values map the config path.
pub fn env_overrides() -> Vec<(&'static str, &'static str)> {
    vec![
        ("LISTEN", "server.listen"),
        ("PUBLIC_URL", "server.public_url"),
        ("DATABASE_URL", "database.url"),
        ("DATABASE_URL_FILE", "database.url"),
        ("LOG_LEVEL", "logging.level"),
        ("LOG_FORMAT", "logging.format"),
    ]
}

/// Loads the config: defaults ← file ← curated envs (F-125). `${VAR}`
/// interpolation is applied to non-secret string fields only (R5; F-32),
/// fail-fast on unset variables (F-89).
pub fn load(path: Option<&std::path::Path>) -> Result<Config, ConfigError> {
    let path = path.map_or_else(
        || {
            std::env::var("CARGOBIKE_SERVER_CONFIG")
                .ok()
                .map(PathBuf::from)
        },
        |p| Some(p.to_path_buf()),
    );
    let mut text = match &path {
        Some(p) => {
            std::fs::read_to_string(p)
                .map_err(|cause| ConfigError::Parse(format!("failed to read {}: {cause}", p.display())))?
        }
        // Env-only bootstrap (F-123) still parses an (almost) empty file.
        None => "server: {}\ndatabase: { url: ${CARGOBIKE_SERVER_DATABASE_URL:-postgres://localhost/cargobike} }\nauth: {}\n".to_owned(),
    };

    // Fail-fast ${VAR} expansion for non-secret scalars (F-89).
    if text.contains("${") {
        text = shellexpand::env(&text)
            .map_err(|source| ConfigError::Parse(format!("interpolation failed: {source}")))?
            .into_owned();
    }

    let mut config: Config =
        serde_yaml_ng::from_str(&text).map_err(|source| ConfigError::Parse(format!("{source}")))?;

    // Curated env overrides — scalars only (F-125): a hand-rolled path set
    // applied after parse, so mapping is explicit and testable.
    apply_env_overrides(&mut config)?;

    // Literal-database-URL warning (C13) and the secrets are materialised
    // lazily by the caller, never into Debug (F-87).
    Ok(config)
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

/// Warnings the loader reports (startup noise, not errors; C13).
pub fn literal_secret_warnings(config: &Config) -> Vec<&'static str> {
    let mut out = Vec::new();
    if config.database.url.is_literal() {
        out.push("database.url is a literal in the config; prefer { file } or { env } (R5)");
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

    /// PRD §13.1's example, extracted from the document itself — the
    /// verbatim-promise test (F-32, T5a).
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
