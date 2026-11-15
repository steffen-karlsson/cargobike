//! The server config's value types (moved from the server so the
//! CLI's offline `validate` parses the SAME file, 4.8/US-13; §2a's
//! naming rules hold on every key).
//!
//! Validation happens structurally (`deny_unknown_fields` — enforced
//! by the shapes) and semantically at load (the server's
//! `validation::validate`); the documented example config must load
//! verbatim (the server's test's promise). The LOADER (env overlay,
//! `{VAR}` interpolation, secret materialisation) stays server-side:
//! core is I/O-free.

use std::collections::BTreeMap;
use std::path::PathBuf;

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
    /// The App's installation the provider drives. Present = the
    /// provider pins that installation (and the webhook receiver
    /// refuses deliveries from any other, F-51). ABSENT = F-122's
    /// per-repo shape: each repository call resolves its own
    /// installation and mints a repository-scoped token.
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
    /// The F-10 verified-path re-check (the background retry that
    /// heals a mismatch or an outage). Default 1m .
    pub verified_path_interval: String,
}

impl Default for ReconcilerSection {
    fn default() -> Self {
        Self {
            interval: "5m".to_owned(),
            batch_size: 100,
            verified_path_interval: "1m".to_owned(),
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
#[serde(untagged, rename_all = "kebab-case")]
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
#[derive(Clone, Debug, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ChangeRequestOverrides {
    /// Custom title; placeholders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Custom body; placeholders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Appended to template labels .
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
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
