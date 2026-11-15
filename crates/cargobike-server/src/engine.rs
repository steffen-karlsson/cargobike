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

/// The DBOS schema the config resolves to — the instance's own, the
/// boot's standby-enqueue lane's, one expression so they cannot
/// disagree (a client naming the wrong schema reads an empty
/// database, not an error).
pub fn dbos_schema(config: &Config) -> String {
    if config.database.dbos_schema.is_empty() {
        DBOS_SCHEMA.to_owned()
    } else {
        config.database.dbos_schema.clone()
    }
}

/// The engine's hosting; held in the server state.
pub struct EngineHosting {
    /// The DBOS instance (cancel/fork/management needs it).
    pub instance: Arc<dbos::DBOS>,
    /// The immutable event log (F-111): the API's create/cancel seams
    /// append through it; the events list and the SSE watch read it.
    pub events: Arc<cargobike_engine::SqlEventLog>,
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
    /// The webhook workflow's registration (the receiver's follow-through).
    pub webhook:
        dbos::WorkflowRef<cargobike_engine::WebhookArgs, (), cargobike_engine::InterpreterError>,
    /// The retry workflow's registration (the failed release's fork;
    /// its return value is the fork's own workflow id).
    pub retry:
        dbos::WorkflowRef<cargobike_engine::RetryArgs, String, cargobike_engine::InterpreterError>,
    /// The approval workflow's registration (the approvals endpoint's
    /// durable follow-through, Phase 5.5).
    pub approval:
        dbos::WorkflowRef<cargobike_engine::ApprovalArgs, (), cargobike_engine::InterpreterError>,
    /// The per-provider materialised webhook secrets (the receiver's
    /// verification input; the two-secret rotation is the list).
    pub webhook_secrets: std::collections::BTreeMap<String, Vec<secrecy::SecretString>>,
    /// The reconciler workflow's registration (the loop's start is
    /// `activate`'s — only a leader runs the sweep).
    pub reconciler: dbos::WorkflowRef<
        cargobike_engine::reconciler::ReconcileArgs,
        (),
        cargobike_engine::InterpreterError,
    >,
    /// The reconciler loop's arguments (interval, batch size) —
    /// resolved once at host time, used at activation time.
    pub reconciler_args: cargobike_engine::reconciler::ReconcileArgs,
    /// F-131's activation latch: the executor launches at most once
    /// (a promotion race would double the reconciler loop otherwise).
    activated: std::sync::atomic::AtomicBool,
}

impl EngineHosting {
    /// F-131: the executor's launch — the recovery, the dequeue loop
    /// and the reconciler start — is the LEADER's alone. `host` builds
    /// everything un-launched; the boot activates when the election is
    /// won (immediately, when election is disabled).
    pub async fn activate(&self) -> Result<(), crate::config::ConfigError> {
        if self
            .activated
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Ok(());
        }
        self.instance.launch().await.map_err(|failure| {
            crate::config::ConfigError::Parse(format!("the DBOS launch failed: {failure}"))
        })?;
        // The reconciler loop runs for the instance's life.
        let reconciled = self
            .reconciler
            .clone()
            .start_with(self.reconciler_args.clone(), dbos::StartOptions::default())
            .await
            .map_err(|failure| {
                crate::config::ConfigError::Parse(format!(
                    "the reconciler loop refused to start: {failure}"
                ))
            })?;
        let _ = reconciled;
        Ok(())
    }
}

/// A factory over the config's providers (the tests install a mock
/// provider registry here; production builds the GitHub provider).
pub type ProviderOverride = Option<ProviderRegistry>;

/// F-131's standby webhook lane: the `dbos::Client` enqueue. A
/// standby does not launch the executor, but it still receives
/// deliveries (rolling updates must not lose tag-push triggers) — it
/// verifies (the provider's HMAC, no DBOS), and enqueues the webhook
/// workflow BY NAME onto the internal queue; the leader's executor
/// claims the row and runs it.
///
/// The client connects lazily and is RETAINED: `Client::connect`
/// verifies (never migrates) the DBOS schema, so a database no leader
/// has ever launched refuses there — the delivery answers 503 and
/// GitHub's redelivery retries it.
pub struct StandbyEnqueue {
    /// The DBOS system database's URL (the instance's own).
    url: secrecy::SecretString,
    /// The DBOS schema the leader's executor reads (the client must
    /// name the application's own — a wrong schema reads as an empty
    /// database, not an error).
    schema: String,
    /// The lazily-connected client (one per process; reused).
    client: tokio::sync::OnceCell<dbos::Client>,
}

impl StandbyEnqueue {
    /// The lane over the DBOS system database's URL and schema (the
    /// same pair the instance's own config carries).
    pub fn new(url: secrecy::SecretString, schema: String) -> Self {
        Self {
            url,
            schema,
            client: tokio::sync::OnceCell::const_new(),
        }
    }

    /// Enqueues the webhook workflow by name under the receiver's
    /// deterministic id (`cargobike/webhook/<provider>/<delivery>` —
    /// the redelivery dedupe keeps working across the leader/standby
    /// split, whichever replica takes the delivery).
    pub async fn enqueue_webhook(
        &self,
        args: &cargobike_engine::WebhookArgs,
        workflow_id: &str,
    ) -> Result<(), String> {
        let url = self.url.expose_secret();
        let client = self
            .client
            .get_or_try_init(|| async {
                let mut config = dbos::ClientConfig::new(url);
                // The claim's scope: the row must read as the
                // leader's application's work (an unnamed client
                // writes an unclaimed row instead — that works, but
                // naming ours is the deliberate, checked claim).
                config.app_name = Some(DBOS_APP_NAME.to_owned());
                config.schema = self.schema.clone();
                dbos::Client::connect(config).await
            })
            .await
            .map_err(|failure| failure.to_string())?;
        let mut options = dbos::EnqueueOptions::new(dbos::sysdb::INTERNAL_QUEUE);
        options.workflow_id = Some(workflow_id);
        client
            .enqueue_with::<cargobike_engine::WebhookArgs, (), cargobike_engine::InterpreterError>(
                cargobike_engine::WEBHOOK_WORKFLOW,
                args.clone(),
                options,
            )
            .await
            .map(|_| ())
            .map_err(|failure| failure.to_string())
    }
}

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

/// One CIDR entry: an address plus a prefix length, the deny/allow
/// lists' unit (F-117).
#[derive(Clone, Copy, Debug)]
struct Cidr {
    network: std::net::IpAddr,
    bits: u32,
}

impl Cidr {
    /// Parses `a.b.c.d/nn`, an IPv6 `..../nn`, or a bare address
    /// (the full /32 or /128); a malformed entry parses to `None` and
    /// the boot refuses the config naming it.
    fn parse(text: &str) -> Option<Self> {
        let (address, bits) = match text.split_once('/') {
            Some((address, bits)) => (address, bits.parse::<u32>().ok()?),
            None => (text, u32::MAX),
        };
        let network: std::net::IpAddr = address.parse().ok()?;
        match network {
            std::net::IpAddr::V4(_) if bits <= 32 => {}
            std::net::IpAddr::V6(_) if bits == u32::MAX || bits <= 128 => {}
            _ => return None,
        }
        Some(Self { network, bits })
    }

    /// The masked containment (an address family mismatch is false).
    fn contains(&self, address: &std::net::IpAddr) -> bool {
        match (self.network, *address) {
            (std::net::IpAddr::V4(network), std::net::IpAddr::V4(address)) => {
                let bits = self.bits.min(32);
                let mask = if bits == 0 {
                    0
                } else {
                    u32::MAX << (32 - bits)
                };
                (u32::from(address) & mask) == (u32::from(network) & mask)
            }
            (std::net::IpAddr::V6(network), std::net::IpAddr::V6(address)) => {
                let bits = self.bits.min(128);
                let mask = if bits == 0 {
                    0
                } else {
                    u128::MAX << (128 - bits)
                };
                (u128::from(address) & mask) == (u128::from(network) & mask)
            }
            _ => false,
        }
    }
}

/// The parsed egress policy (F-117): the configured deny ADDITIONS and
/// allow exceptions over the BUILT-IN ranges. The built-ins (the
/// loopback/link-local/RFC1918/ULA grouping the PRD requires) are
/// never removable by config — the deny list only extends, and the
/// allow list is the one exception path (the administrator's explicit
/// escape hatch onto the private net).
#[derive(Clone, Debug, Default)]
struct EgressPolicy {
    deny: Vec<Cidr>,
    allow: Vec<Cidr>,
}

impl EgressPolicy {
    /// Parses the config's lists, refusing a malformed entry by name
    /// (fail-closed: a policy that cannot be read is not a policy).
    fn parse(section: &crate::config::EgressSection) -> Result<Self, String> {
        let deny = section
            .deny
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|entry| {
                Cidr::parse(entry)
                    .ok_or_else(|| format!("the egress deny entry `{entry}` is not a CIDR"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let allow = section
            .allow
            .iter()
            .map(|entry| {
                Cidr::parse(entry)
                    .ok_or_else(|| format!("the egress allow entry `{entry}` is not a CIDR"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { deny, allow })
    }

    /// One address against the whole policy: the allow exceptions win
    /// first, then the built-in ranges, then the configured additions.
    fn refuses(&self, address: &std::net::IpAddr) -> bool {
        if self.allow.iter().any(|cidr| cidr.contains(address)) {
            return false;
        }
        egress_builtin_refuses(address) || self.deny.iter().any(|cidr| cidr.contains(address))
    }
}

/// The built-in deny ranges (F-117): IPv4 loopback, link-local, and
/// RFC 1918; IPv6 loopback and unique-local. The global-scope and
/// multicast ranges stay reachable (a provider's endpoints live
/// there); the deny list extends when they must not.
fn egress_builtin_refuses(address: &std::net::IpAddr) -> bool {
    matches!(
        address,
        std::net::IpAddr::V4(ip) if ip.is_loopback() || ip.is_link_local() || ip.is_private()
    ) || matches!(
        address,
        std::net::IpAddr::V6(ip) if ip.is_loopback() || ip.is_unique_local()
    )
}

/// The pinned resolver: whatever name the client asks for, it gets the
/// addresses the egress check approved — the check and the connect
/// share ONE resolution, so a name that flips its records between the
/// two (the DNS-rebinding attack, F-117) connects nowhere the policy
/// has not already seen.
struct Pinned(Vec<std::net::SocketAddr>);

impl reqwest::dns::Resolve for Pinned {
    fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let addrs = self.0.clone();
        Box::pin(async move { Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs) })
    }
}

/// The egress-guarded HTTP seam (F-117's full set): every hop — the
/// first request and each redirect — resolves its host, checks EVERY
/// resolved address against the policy (a name with one refused record
/// is refused: mixed records are the rebinding attack's shape),
/// connects through a resolver pinned to the checked addresses, and
/// re-checks on every redirect. The proxy is the config's explicit one
/// or none at all — the environment's `HTTP(S)_PROXY`/`NO_PROXY` are
/// never consulted, and the deny-list applies to the requested host
/// before any proxying.
pub struct EgressHttpService {
    policy: EgressPolicy,
    proxy: Option<String>,
}

/// The redirect chain's bound (each hop pays a fresh egress check;
/// a loop that never lands is an error, not a hang).
const EGRESS_REDIRECT_LIMIT: usize = 5;

impl EgressHttpService {
    /// Builds the service with the config's egress rules (the rules
    /// are a boot-time snapshot; a reload restarts the hosting). A
    /// malformed deny/allow entry refuses the boot naming it.
    pub fn new(config: &Config) -> Result<Self, String> {
        Ok(Self {
            policy: EgressPolicy::parse(&config.network.egress)?,
            proxy: config.network.egress.proxy.clone(),
        })
    }

    /// Resolves the hop's host and checks every address against the
    /// policy; the returned addresses are the ONLY ones the connect
    /// may use (the pinning is F-117's rebinding defence).
    async fn resolve_and_check(
        &self,
        url: &url::Url,
    ) -> Result<Vec<std::net::SocketAddr>, HttpError> {
        // The host form (a bracketed IPv6 literal parses here, not as
        // a name with brackets).
        enum HopHost {
            Literal(std::net::IpAddr),
            Name(String),
        }
        let host = match url.host() {
            Some(url::Host::Domain(name)) => HopHost::Name(name.to_owned()),
            Some(url::Host::Ipv4(address)) => HopHost::Literal(std::net::IpAddr::V4(address)),
            Some(url::Host::Ipv6(address)) => HopHost::Literal(std::net::IpAddr::V6(address)),
            None => {
                return Err(HttpError::Forbidden("the url carries no host".to_owned()));
            }
        };
        let port = url
            .port_or_known_default()
            .ok_or_else(|| HttpError::Other("the url's scheme carries no port".to_owned()))?;
        let resolved: Vec<std::net::SocketAddr> = match host {
            HopHost::Literal(address) => vec![std::net::SocketAddr::new(address, port)],
            HopHost::Name(name) => tokio::net::lookup_host((name.as_str(), port))
                .await
                .map_err(|failure| {
                    HttpError::Connect(format!("the host's resolution failed: {failure}"))
                })?
                .collect(),
        };
        if resolved.is_empty() {
            return Err(HttpError::Connect(
                "the host resolved to no address".to_owned(),
            ));
        }
        for socket in &resolved {
            if self.policy.refuses(&socket.ip()) {
                return Err(HttpError::Forbidden(format!(
                    "blocked by the egress rules: {}",
                    socket.ip()
                )));
            }
        }
        Ok(resolved)
    }

    /// The per-hop client: redirects off (the loop above owns them),
    /// the environment's proxies suppressed (the config's proxy is
    /// the only one, F-117), and the resolver pinned to the checked
    /// addresses. Built per hop: the pin is per resolution.
    fn pinned_client(
        &self,
        addrs: Vec<std::net::SocketAddr>,
    ) -> Result<reqwest::Client, HttpError> {
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("cargobike-server/", env!("CARGO_PKG_VERSION")))
            // F-117: the environment's proxies are never consulted —
            // the config's proxy is the only one.
            .no_proxy();
        match &self.proxy {
            Some(proxy) => {
                // Through a proxy the connection's addressing belongs
                // to the proxy (the admin's own escape hatch); the
                // deny-list applied to the REQUESTED host above is the
                // policy, and no pin is meaningful here — the client
                // would resolve the proxy with the target's pinned
                // addresses.
                let proxy = reqwest::Proxy::all(proxy).map_err(|failure| {
                    HttpError::Other(format!("the configured egress proxy is invalid: {failure}"))
                })?;
                builder = builder.proxy(proxy);
            }
            None => {
                builder = builder.dns_resolver(std::sync::Arc::new(Pinned(addrs)));
            }
        }
        builder.build().map_err(|failure| {
            HttpError::Other(format!("the outbound client refused to build: {failure}"))
        })
    }
}

#[async_trait]
impl cargobike_core::step::HttpService for EgressHttpService {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        let mut url = url::Url::parse(&request.url)
            .map_err(|failure| HttpError::Other(format!("the url refused to parse: {failure}")))?;
        let mut method = request.method.clone();
        let mut body = request.body.clone();
        for _hop in 0..EGRESS_REDIRECT_LIMIT {
            let pinned = self.resolve_and_check(&url).await?;
            let client = self.pinned_client(pinned)?;
            let parsed_method = <reqwest::Method as std::str::FromStr>::from_str(&method)
                .map_err(|failure| HttpError::Other(failure.to_string()))?;
            let mut built = client.request(parsed_method, url.clone());
            for (name, value) in &request.headers {
                built = built.header(name.as_str(), value.as_str());
            }
            if let Some(body) = &body {
                built = built.body(body.clone());
            }
            let response = built
                .send()
                .await
                .map_err(|failure| HttpError::Connect(failure.to_string()))?;
            let status = response.status();
            // F-117's redirect re-check: every hop passes the same
            // gate. A 3xx without a Location is the step's answer (the
            // response is returned, not an error).
            let redirected = if status.is_redirection() {
                response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .map(|location| {
                        url.join(location)
                            .map_err(|failure| HttpError::Other(failure.to_string()))
                    })
            } else {
                None
            };
            match redirected {
                Some(Ok(next)) => {
                    url = next;
                    // 303's semantics: the follow-up is a GET with no
                    // body (RFC 9110; the others keep the method).
                    if status.as_u16() == 303 {
                        method = "GET".to_owned();
                        body = None;
                    }
                }
                Some(Err(failure)) => return Err(failure),
                None => {
                    let status = status.as_u16();
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
                    return Ok(HttpResponse {
                        status,
                        headers,
                        body,
                    });
                }
            }
        }
        Err(HttpError::Other(
            "the redirect chain exceeded the limit".to_owned(),
        ))
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
    config_rx: tokio::sync::watch::Receiver<Arc<Config>>,
    pool: sqlx::PgPool,
    provider_override: ProviderOverride,
    availability: std::sync::Arc<crate::availability::Availability>,
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

    // The providers' webhook secrets (the { file }/env/secret refs read
    // at boot; the map is the receiver's verification input).
    let mut webhook_secrets = std::collections::BTreeMap::new();
    for provider in &config.providers {
        let values = materialise_secret_values(&provider.webhook_secrets, &secrets).await?;
        webhook_secrets.insert(provider.name.clone(), values);
    }

    let steps = {
        let mut steps = cargobike_engine::StepRegistry::new();
        cargobike_engine::builtin::register_builtins(&mut steps);
        Arc::new(steps)
    };

    // DBOS: the app identity + the pinned binary version (the recovery
    // filter reads the recorded one).
    let database_url = materialise(&config.database.url, &config.secrets)?;
    let mut dbos_config = dbos::Config::new(DBOS_APP_NAME, database_url.expose_secret());
    // The config's DBOS schema (the boot's own namespace; tests point
    // each boot at a fresh one so a previous suite's abandoned
    // workflow rows are never adopted by this boot's recovery).
    dbos_config.schema = dbos_schema(config);
    dbos_config.app_version = Some(env!("CARGO_PKG_VERSION").to_owned());
    let instance = dbos::DBOS::new(dbos_config);

    // F-111/F-113: the immutable event log — one sink shared by the
    // interpreter's status steps, the API's create/cancel seams, and
    // the watch/list readers.
    let events = Arc::new(cargobike_engine::SqlEventLog::new(pool.clone()));
    let services = Arc::new(InterpreterServices {
        steps,
        providers: Arc::new(providers),
        credentials: Arc::new(ConfigCredentials { secrets }),
        http: Arc::new(EgressHttpService::new(config).map_err(crate::config::ConfigError::Parse)?),
        leases: Arc::new(LeaseRepository::new(pool.clone())),
        statuses: Arc::new(SqlReleaseStatusStore::new(pool.clone())),
        correlations: Arc::new(CorrelationRepository::new(pool.clone())),
        events: events.clone(),
        instance: instance.clone(),
        cleanup_ref: Arc::new(std::sync::OnceLock::new()),
        max_step_output: crate::config::parse_size(&config.engine.max_step_output)
            .unwrap_or(1024 * 1024) as usize,
        cel_limits: cargobike_engine::expr::Limits {
            max_expression_length: config.engine.cel_max_expression_length as usize,
            max_depth: cargobike_engine::expr::DEFAULT_LIMITS.max_depth,
        },
    });

    // The honest knob reading (F-27's spike note): cel 0.14 exposes no
    // runtime-cost API, so `engine.cel.max_cost` stands for the depth
    // bound's substitute; surfaced here, once per boot.
    if config.engine.cel_max_cost != 2_000_000 {
        tracing::debug!(
            configured = config.engine.cel_max_cost,
            depth_bound = cargobike_engine::expr::DEFAULT_LIMITS.max_depth,
            "engine.cel.max_cost has no runtime-cost API in cel 0.14; the depth bound stands"
        );
    }

    let interpreter = cargobike_engine::register_interpreter(&instance, Arc::clone(&services))
        .map_err(|failure| crate::config::ConfigError::Parse(failure.to_string()))?;

    let cleanup = cargobike_engine::register_cleanup(
        &instance,
        Arc::new(cargobike_engine::CleanupServices {
            providers: services.providers.clone(),
            leases: services.leases.clone(),
        }),
    )
    .map_err(|failure| crate::config::ConfigError::Parse(failure.to_string()))?;

    // The supersede path's starter gets the cleanup's handle.
    let _ = services.cleanup_ref.set(cleanup.clone());

    // The webhook workflow registers BEFORE launch (the durable
    // follow-through the receiver starts).
    let webhook = cargobike_engine::register_webhook(
        &instance,
        Arc::new(cargobike_engine::WebhookServices {
            correlations: services.correlations.clone(),
            interpreter: interpreter.clone(),
            creator: Arc::new(ServerTagPushCreator {
                pool: pool.clone(),
                config: config_rx,
                providers: services.providers.clone(),
                steps: services.steps.clone(),
                availability,
                events: events.clone(),
            }),
        }),
    )
    .map_err(|failure| {
        crate::config::ConfigError::Parse(format!(
            "the webhook workflow refused to register: {failure}"
        ))
    })?;

    let retry = cargobike_engine::register_retry(
        &instance,
        Arc::new(cargobike_engine::RetryServices {
            statuses: services.statuses.clone(),
            events: events.clone(),
            instance: instance.clone(),
        }),
    )
    .map_err(|failure| {
        crate::config::ConfigError::Parse(format!(
            "the retry workflow refused to register: {failure}"
        ))
    })?;

    let reconciler = cargobike_engine::reconciler::register_reconciler(
        &instance,
        Arc::new(cargobike_engine::reconciler::ReconcilerServices {
            providers: services.providers.clone(),
            correlations: services.correlations.clone(),
            releases: Arc::new(SqlSignalSource { pool: pool.clone() }),
        }),
    )
    .map_err(|failure| crate::config::ConfigError::Parse(failure.to_string()))?;

    let approval = cargobike_engine::register_approval(
        &instance,
        std::sync::Arc::new(cargobike_engine::ApprovalServices {
            events: events.clone(),
        }),
    )
    .map_err(|failure| {
        crate::config::ConfigError::Parse(format!(
            "the approval workflow refused to register: {failure}"
        ))
    })?;

    // The reconciler loop's arguments resolve here (the loop itself is
    // `activate`'s — F-131: only a leader starts the sweep).
    let interval = humantime::parse_duration(&config.reconciler.interval).map_err(|failure| {
        crate::config::ConfigError::Parse(format!(
            "the reconciler interval refused to parse: {failure}"
        ))
    })?;
    let reconciler_args = cargobike_engine::reconciler::ReconcileArgs {
        interval,
        batch_size: config.reconciler.batch_size,
    };

    Ok(EngineHosting {
        instance: Arc::new(instance),
        events,
        services: services.clone(),
        interpreter,
        cleanup,
        webhook,
        retry,
        approval,
        webhook_secrets,
        reconciler,
        reconciler_args,
        activated: std::sync::atomic::AtomicBool::new(false),
    })
}

/// The providers' webhook secrets' materialisation (the { file }/env/
/// `{ secret }` refs; literals already warned at startup).
async fn materialise_secret_values(
    entries: &Option<Vec<crate::config::SecretValue>>,
    named: &BTreeMap<String, secrecy::SecretString>,
) -> Result<Vec<secrecy::SecretString>, crate::config::ConfigError> {
    use secrecy::ExposeSecret as _;
    let Some(entries) = entries else {
        return Ok(vec![]);
    };
    let mut values = Vec::with_capacity(entries.len());
    for entry in entries {
        let value = match entry {
            crate::config::SecretValue::Literal(text) => text.clone(),
            crate::config::SecretValue::File(file) => secrecy::SecretString::from(
                tokio::fs::read_to_string(&file.file)
                    .await
                    .map_err(|failure| crate::config::ConfigError::Materialise {
                        path: file.file.clone(),
                        cause: failure.to_string(),
                    })?
                    .trim_end_matches('\n')
                    .to_owned(),
            )
            .expose_secret()
            .to_owned(),
            crate::config::SecretValue::Env { env } => std::env::var(env)
                .map_err(|_| crate::config::ConfigError::UnknownEnv { name: env.clone() })?,
            crate::config::SecretValue::Secret { r#secret } => named
                .get(&secret.clone())
                .map(|found| found.expose_secret().to_owned())
                .ok_or_else(|| {
                    crate::config::ConfigError::Parse(format!(
                        "the webhook secret's `{secret}` is not in the `secrets:` map"
                    ))
                })?,
        };
        values.push(secrecy::SecretString::from(value));
    }
    Ok(values)
}

/// The tag-push creator (the engine's seam): scans the live registry
/// for the app whose source + tag trigger match the delivery, extracts
/// the version per `tag_format`, enforces the tag-protection rule
/// (F-82: fail-closed on the default), and runs the shared create core.
struct ServerTagPushCreator {
    pool: sqlx::PgPool,
    config: tokio::sync::watch::Receiver<Arc<Config>>,
    providers: Arc<cargobike_core::registry::ProviderRegistry>,
    /// F-10's availability (the tag-push create refuses while the
    /// matched app's verified-path label does not check out).
    availability: Arc<crate::availability::Availability>,
    steps: Arc<cargobike_engine::StepRegistry>,
    /// F-111: the created-release event rides the log the API seam
    /// shares.
    events: Arc<cargobike_engine::SqlEventLog>,
}

/// The version inside a tag: the format's `{version}` placeholder
/// pieces stripped (empty results refuse — the tag FORMAT's middle
/// text must match exactly).
fn version_from_tag(tag_format: &str, tag: &str) -> Option<String> {
    let (prefix, suffix) = match tag_format.split_once("{version}") {
        Some((before, after)) => (before, after),
        None => return None,
    };
    let rest = tag.strip_prefix(prefix)?.strip_suffix(suffix)?;
    (!rest.is_empty()).then(|| rest.to_owned())
}

#[async_trait::async_trait]
impl cargobike_engine::TagPushCreator for ServerTagPushCreator {
    async fn create_from_tag(
        &self,
        provider: &str,
        repository_id: &str,
        tag: &str,
        sha: &str,
        sender: &str,
    ) -> Result<cargobike_engine::TagPushOutcome, String> {
        let config = self.config.borrow().clone();
        // F-97: the verified delivery's pusher is the actor the created
        // releases record — webhook-triggered creates are authorized by
        // signature + trigger, so the sender is provenance, not a claim
        // to trust.
        let sender_actor = crate::release::ReleaseActor {
            actor: cargobike_core::model::Actor {
                issuer: format!("webhook/{provider}"),
                subject: sender.to_owned(),
                display_name: sender.to_owned(),
            },
            ci: None,
        };
        let mut outcome = cargobike_engine::TagPushOutcome {
            created: vec![],
            duplicates: vec![],
            skipped: vec![],
        };
        for app in &config.applications {
            let matches = app.source.provider == provider
                && app.source.id.as_deref() == Some(repository_id)
                && app.triggers.iter().any(|trigger| trigger.event == "tag");
            if !matches {
                continue;
            }
            let Some(format) = app.versioning.tag_format.clone() else {
                outcome.skipped.push(format!(
                    "{}: the app has no tag_format for tag pushes",
                    app.name
                ));
                continue;
            };
            let Some(version) = version_from_tag(&format, tag) else {
                outcome.skipped.push(format!(
                    "{}: the tag `{tag}` does not match the tag_format `{format}`",
                    app.name
                ));
                continue;
            };
            // F-82: the tag-protection rule, fail-closed on the
            // default (a check that answers not-protected or refuses
            // to answer skips THIS APP entirely; the `continue` on the
            // app loop is the whole point).
            if app
                .triggers
                .iter()
                .any(|trigger| trigger.event == "tag" && trigger.require_tag_protection)
            {
                let repo = cargobike_core::model::RepoRef::new(provider, repository_id);
                let checked: cargobike_core::provider::ProviderResult<bool> =
                    match self.providers.resolve(&repo) {
                        Ok(provider) => provider.check_tag_protection(&repo, tag).await,
                        Err(failure) => Err(cargobike_core::provider::ProviderError::Request(
                            format!("the provider's lookup failed: {failure}"),
                        )),
                    };
                match checked {
                    Ok(true) => {}
                    Ok(false) => {
                        outcome.skipped.push(format!(
                            "{}: the tag `{tag}` is unprotected and the trigger requires protection",
                            app.name
                        ));
                        continue;
                    }
                    Err(failure) => {
                        outcome.skipped.push(format!(
                            "{}: the tag-protection check failed ({failure}); refusing fail-closed",
                            app.name
                        ));
                        continue;
                    }
                }
            }
            let parts = crate::release::CoreParts {
                releases: &crate::release::ReleaseRepository::new(self.pool.clone()),
                config: config.clone(),
                step_types: self.steps.installed().into_iter().collect(),
                providers: &self.providers,
                availability: &self.availability,
                events: &self.events,
            };
            // The row half only: the interpreter's start is refused
            // from inside a step, so the creator plans and the webhook
            // workflow's body addresses the interpreters. F-95: the
            // payload's SHA is a claim — the create verifies the tag
            // against the provider's own tag object.
            match crate::release::create_release_row(
                &parts,
                app,
                &app.name,
                &version,
                crate::release::CreateOrigin {
                    actor: Some(&sender_actor),
                    retried_from: None,
                    idempotency_key: None,
                    sha: Some(sha),
                    annotations: &crate::release::NO_ANNOTATIONS,
                },
            )
            .await
            {
                Ok(created_row) => {
                    let id = created_row
                        .outcome
                        .document
                        .pointer("/metadata/id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let parsed = Uuid::parse_str(&id).map_err(|failure| {
                        format!("the created release's id refused to parse: {failure}")
                    })?;
                    if created_row.outcome.created {
                        outcome.created.push(cargobike_engine::PlannedRelease {
                            release_id: parsed,
                            snapshot: created_row.snapshot.unwrap_or_else(|| {
                                unreachable!("a fresh row always carries its snapshot")
                            }),
                        });
                    } else {
                        outcome.duplicates.push(parsed);
                    }
                }
                Err(failure) => {
                    outcome.skipped.push(format!(
                        "{}: the tag `{tag}`'s create refused ({})",
                        app.name, failure.detail
                    ));
                }
            }
        }
        Ok(outcome)
    }
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
    // F-122: a pinned `installation_id` binds the provider to that
    // installation; ABSENT means per-repo resolution — each repository
    // call resolves its own installation and mints a repository-scoped
    // token (the provider's client cache).
    let installation = match auth.installation_id.as_deref() {
        Some(text) => Some(text.parse::<u64>().map_err(|failure| {
            crate::config::ConfigError::Parse(format!(
                "the github provider `{}`'s installation_id does not parse: {failure}",
                provider.name
            ))
        })?),
        None => None,
    };
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
