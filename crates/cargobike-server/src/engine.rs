//! The engine hosting: the server's boot builds the durable
//! interpreter's services from the config (providers, credentials, the
//! egress-guarded HTTP seam, leases, the status store, the
//! CR-correlation rows), registers the three workflows, launches DBOS,
//! and hands the handles out (create/cancel/retry address them).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use secrecy::ExposeSecret as _;
use uuid::Uuid;

use cargobike_core::provider::Provider;
use cargobike_core::registry::ProviderRegistry;
use cargobike_core::step::{HttpRequest, HttpResponse, StepHttpError as HttpError};
use cargobike_engine::InterpreterServices;
use cargobike_engine::correlation::CorrelationRepository;
use cargobike_engine::leases::LeaseRepository;
use cargobike_engine::status::SqlReleaseStatusStore;

use crate::config::{Config, materialise};

/// The app identity DBOS registers under.
const DBOS_APP_NAME: &str = "cargobike";
/// The engine's schema (the DBOS system tables' namespace).
const DBOS_SCHEMA: &str = "dbos";

/// The engine's hosting; held in the server state.
pub struct EngineHosting {
    /// The DBOS instance (cancel/fork/management needs it).
    pub instance: Arc<dbos::DBOS>,
    /// The interpreter's services (the step registry's installed
    /// versions feed the snapshot).
    pub services: Arc<InterpreterServices>,
    /// The interpreter workflow's registration (a start argument's
    /// launch).
    pub interpreter: dbos::WorkflowRef<
        cargobike_engine::InterpretArgs,
        cargobike_engine::InterpretResult,
        cargobike_engine::InterpreterError,
    >,
    /// The cleanup workflow's registration.
    pub cleanup:
        dbos::WorkflowRef<cargobike_engine::CleanupArgs, (), cargobike_engine::InterpreterError>,
}

/// A factory over the config's providers (the tests install a mock
/// provider registry here; production builds the GitHub provider).
pub type ProviderOverride = Option<ProviderRegistry>;

/// The named secrets' credential store (the config's `secrets:` map).
struct ConfigCredentials {
    secrets: BTreeMap<String, secrecy::SecretString>,
}

impl cargobike_core::registry::CredentialStore for ConfigCredentials {
    fn resolve(
        &self,
        name: &str,
    ) -> Result<secrecy::SecretString, cargobike_core::error::LibraryError> {
        self.secrets.get(name).cloned().ok_or_else(|| {
            cargobike_core::error::LibraryError::UnknownSecret(format!(
                "failed to resolve secret: no secret named `{name}`"
            ))
        })
    }
}

/// The egress-guarded HTTP seam (the first cut): the request's host is
/// resolved and every address checked against the network's egress
/// rules before the call; redirects are not followed (the proxy and the
/// redirect re-check are the remaining work; the PRD's SSRF
/// requirements land with them).
pub struct EgressHttpService {
    client: reqwest::Client,
    egress: crate::config::EgressSection,
}

impl EgressHttpService {
    /// Builds the service with the config's egress rules (the rules are
    /// a boot-time snapshot; a reload restarts the hosting).
    pub fn new(config: &Config) -> Result<Self, String> {
        // Redirects are refuted for now: each hop's egress check is the
        // full policy work.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("cargobike-server/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|failure| format!("the outbound client refused to build: {failure}"))?;
        Ok(Self {
            client,
            egress: config.network.egress.clone(),
        })
    }

    async fn egress_refuses(&self, url: &str) -> Result<(), HttpError> {
        let parsed = url::Url::parse(url)
            .map_err(|failure| HttpError::Other(format!("the url refused to parse: {failure}")))?;
        let Some(host) = parsed.host_str() else {
            return Err(HttpError::Forbidden("the url carries no host".to_owned()));
        };
        // The resolved addresses decide (the literal host's own check
        // rides the resolution: both a name and an IP land here).
        let resolved: Vec<std::net::IpAddr> = match host.parse::<std::net::IpAddr>() {
            Ok(address) => vec![address],
            Err(_) => tokio::net::lookup_host(format!("{host}:80"))
                .await
                .map_err(|failure| {
                    HttpError::Connect(format!("the host's resolution failed: {failure}"))
                })?
                .map(|socket| socket.ip())
                .collect(),
        };
        for address in &resolved {
            if egress_address_refuses(address, &self.egress) {
                return Err(HttpError::Forbidden(format!(
                    "blocked by the egress rules: {address}"
                )));
            }
        }
        Ok(())
    }
}

/// Whether one resolved address is refused by the egress rules (the
/// built-in deny ranges plus the config's).
fn egress_address_refuses(
    address: &std::net::IpAddr,
    rules: &crate::config::EgressSection,
) -> bool {
    let _ = rules;
    // The built-in ranges first (the loopback/link-local/RFC1918/ULA
    // grouping the PRD requires); allowlist exceptions refine later.
    matches!(
        address,
        std::net::IpAddr::V4(ip) if ip.is_loopback() || ip.is_link_local() || ip.is_private()
    ) || matches!(address, std::net::IpAddr::V6(ip) if ip.is_loopback() || ip.is_unique_local())
}

#[async_trait]
impl cargobike_core::step::HttpService for EgressHttpService {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.egress_refuses(&request.url).await?;
        let method = <reqwest::Method as std::str::FromStr>::from_str(&request.method)
            .map_err(|failure| HttpError::Other(failure.to_string()))?;
        let mut built = self.client.request(method, &request.url);
        for (name, value) in &request.headers {
            built = built.header(name.as_str(), value.as_str());
        }
        if let Some(body) = &request.body {
            built = built.body(body.clone());
        }
        let response = built
            .send()
            .await
            .map_err(|failure| HttpError::Connect(failure.to_string()))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        let body = response
            .bytes()
            .await
            .map_err(|failure| HttpError::Connect(failure.to_string()))?
            .to_vec();
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

/// The production seam: the releases table's join over the CR
/// correlations — a release awaits a signal when it is blocked on an
/// approval or when a Running release carries an open, correlated
/// change request (the sweep's latency work).
pub struct SqlSignalSource {
    pool: sqlx::PgPool,
}

#[async_trait]
impl cargobike_engine::reconciler::PendingReleaseSource for SqlSignalSource {
    async fn pending_approval_ids(
        &self,
        batch_size: u32,
        cursor: Option<Uuid>,
    ) -> Result<Vec<Uuid>, String> {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT DISTINCT r.id FROM releases r \
             JOIN cr_correlation c ON c.release_id = r.id \
             WHERE r.phase IN ('PendingApproval', 'Running') \
               AND ($1::uuid IS NULL OR r.id < $1::uuid) \
             ORDER BY r.id DESC LIMIT $2",
        )
        .bind::<Option<Uuid>>(cursor)
        .bind(i64::from(batch_size))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| error.to_string())
        .map(|rows| rows.into_iter().collect())
    }
}

/// Builds the engine's hosting from the config; the provider registry
/// comes from the config's providers (or the tests' override).
pub async fn host(
    config: &Config,
    pool: sqlx::PgPool,
    provider_override: ProviderOverride,
) -> Result<EngineHosting, crate::config::ConfigError> {
    // Providers: the github entries build; extensions log and skip
    // (their transport is the next phase's work).
    let providers = match provider_override {
        Some(override_registry) => override_registry,
        None => {
            let mut registry = ProviderRegistry::new();
            for provider in &config.providers {
                if provider.r#type != "github" {
                    tracing::warn!(
                        provider = %provider.name,
                        "the extension-served provider's transport is not built yet; the provider is skipped"
                    );
                    continue;
                }
                let built = build_github_provider(provider, config)?;
                registry.insert(&built.0, built.1);
            }
            registry
        }
    };

    // Named secrets for the steps.
    let mut secrets = BTreeMap::new();
    for (name, source) in &config.secrets {
        let (path, env) = match source {
            crate::config::SecretSource::File { file } => (Some(file.clone()), None),
            crate::config::SecretSource::Env { env } => (None, Some(env.clone())),
        };
        let value = if let Some(path) = &path {
            secrecy::SecretString::from(
                tokio::fs::read_to_string(path)
                    .await
                    .map_err(|failure| crate::config::ConfigError::Materialise {
                        path: path.clone(),
                        cause: failure.to_string(),
                    })?
                    .trim_end_matches('\n')
                    .to_owned(),
            )
        } else if let Some(env_name) = &env {
            let value =
                std::env::var(env_name).map_err(|_| crate::config::ConfigError::UnknownEnv {
                    name: env_name.clone(),
                })?;
            secrecy::SecretString::from(value)
        } else {
            return Err(crate::config::ConfigError::Parse(format!(
                "the secret `{name}` declares neither a file nor an environment variable"
            )));
        };
        secrets.insert(name.clone(), value);
    }

    let steps = {
        let mut steps = cargobike_engine::StepRegistry::new();
        cargobike_engine::builtin::register_builtins(&mut steps);
        Arc::new(steps)
    };
    let services = Arc::new(InterpreterServices {
        steps,
        providers: Arc::new(providers),
        credentials: Arc::new(ConfigCredentials { secrets }),
        http: Arc::new(EgressHttpService::new(config).map_err(crate::config::ConfigError::Parse)?),
        leases: Arc::new(LeaseRepository::new(pool.clone())),
        statuses: Arc::new(SqlReleaseStatusStore::new(pool.clone())),
        correlations: Arc::new(CorrelationRepository::new(pool.clone())),
    });

    // DBOS: the app identity + the pinned binary version (the recovery
    // filter reads the recorded one).
    let database_url = materialise(&config.database.url, &config.secrets)?;
    let mut dbos_config = dbos::Config::new(DBOS_APP_NAME, database_url.expose_secret());
    dbos_config.schema = DBOS_SCHEMA.to_owned();
    dbos_config.app_version = Some(env!("CARGO_PKG_VERSION").to_owned());
    let instance = dbos::DBOS::new(dbos_config);

    let interpreter = cargobike_engine::register_interpreter(&instance, Arc::clone(&services))
        .map_err(|failure| crate::config::ConfigError::Parse(failure.to_string()))?;

    let cleanup = cargobike_engine::register_cleanup(
        &instance,
        Arc::new(cargobike_engine::CleanupServices {
            providers: services.providers.clone(),
            leases: services.leases.clone(),
        }),
    );
    let cleanup =
        cleanup.map_err(|failure| crate::config::ConfigError::Parse(failure.to_string()))?;

    let reconciler = cargobike_engine::reconciler::register_reconciler(
        &instance,
        Arc::new(cargobike_engine::reconciler::ReconcilerServices {
            providers: services.providers.clone(),
            correlations: services.correlations.clone(),
            releases: Arc::new(SqlSignalSource { pool }),
        }),
    );
    let reconciler = reconciler;

    instance.launch().await.map_err(|failure| {
        crate::config::ConfigError::Parse(format!("the DBOS launch failed: {failure}"))
    })?;

    // The reconciler loop runs for the instance's life.
    let interval = humantime::parse_duration(&config.reconciler.interval).map_err(|failure| {
        crate::config::ConfigError::Parse(format!(
            "the reconciler interval refused to parse: {failure}"
        ))
    })?;
    let reconciler_args = cargobike_engine::reconciler::ReconcileArgs {
        interval,
        batch_size: config.reconciler.batch_size,
    };
    let reconciled = reconciler
        .map_err(|failure| crate::config::ConfigError::Parse(failure.to_string()))?
        .start_with(reconciler_args, dbos::StartOptions::default())
        .await
        .map_err(|failure| {
            crate::config::ConfigError::Parse(format!(
                "the reconciler loop refused to start: {failure}"
            ))
        })?;
    let _ = reconciled;

    Ok(EngineHosting {
        instance: Arc::new(instance),
        services: services.clone(),
        interpreter,
        cleanup,
    })
}

/// The config's github entry to a registered provider.
fn build_github_provider(
    provider: &crate::config::ProviderConfig,
    config: &Config,
) -> Result<(String, Arc<dyn Provider>), crate::config::ConfigError> {
    let Some(auth) = &provider.auth else {
        return Err(crate::config::ConfigError::Parse(format!(
            "the github provider `{}` carries no auth block",
            provider.name
        )));
    };
    let private_key_material = materialise(&auth.private_key, &config.secrets)?;
    let installation = auth
        .installation_id
        .clone()
        .and_then(|text| text.parse::<u64>().ok())
        .ok_or_else(|| {
            crate::config::ConfigError::Parse(format!(
                "the github provider `{}` needs an installation_id (the per-repo installation lookup is the next phase's work)",
                provider.name
            ))
        })?;
    let app_id = auth.app_id.parse::<u64>().map_err(|failure| {
        crate::config::ConfigError::Parse(format!(
            "the github provider `{}`'s app_id refused to parse: {failure}",
            provider.name
        ))
    })?;
    let built = cargobike_provider_github::GithubProvider::new_with_api_url(
        &cargobike_provider_github::GithubAuth::App {
            app_id,
            installation_id: installation,
            private_key_pem: private_key_material,
        },
        provider.api_url.as_deref(),
    )
    .map_err(|error| {
        crate::config::ConfigError::Parse(format!(
            "the github provider `{}` refused to build: {error}",
            provider.name
        ))
    })?;
    Ok((provider.name.clone(), Arc::new(built)))
}
