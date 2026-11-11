//! F-10 verified-path labels: the registry's `source.path` is a
//! **label the server verifies against the provider** — a declared
//! path is an assertion, the provider's `get_repo_path` is the truth.
//!
//! Posture (fail-closed, consistent with F-65/F-95):
//! - **Match** → available (no entry).
//! - **Mismatch OR provider error OR unknown provider** → the
//!   application is marked *unavailable*: the create refuses (the
//!   registry's label cannot be trusted to name the repo the release
//!   would touch), and `GET /api/v1/applications` surfaces the reason.
//! - The check runs at boot (before the API serves) and a background
//!   task **retries** it — the PRD's "retried in the background; the
//!   server does not fail" — so a transient provider outage or a
//!   renamed-back repo heals without a restart.
//! - An app without a declared `path` has nothing to verify: it is
//!   available by definition (the runtime fetch path, F-10's last
//!   sentence, stays the provider's).
//!
//! The state is deliberately in-memory: it is a *view* of the
//! provider, rebuilt at boot and healed by the sweep — not durable
//! server-owned state. The probe is a path-less `RepoRef` clone so the
//! GitHub provider's declared-label short-circuit cannot echo the
//! label back as its own truth.

use std::collections::BTreeMap;
use std::sync::Arc;

use cargobike_core::registry::ProviderRegistry;

use crate::config::Config;

/// The per-application availability view (F-10): the absent entry
/// means available; the entry's string is the refusal reason.
#[derive(Default)]
pub struct Availability {
    unavailable: std::sync::Mutex<BTreeMap<String, String>>,
}

impl Availability {
    /// Empty view — every application available.
    pub fn new() -> Self {
        Self::default()
    }

    /// `Some(reason)` when the application is marked unavailable.
    pub fn reason(&self, application: &str) -> Option<String> {
        self.read().get(application).cloned()
    }

    /// The whole view (the `/applications` listing).
    pub fn snapshot(&self) -> BTreeMap<String, String> {
        self.read().clone()
    }

    /// The lock's guard (a poisoned lock recovers its data — the view
    /// is a rebuildable cache, never a consistency boundary).
    fn read(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, String>> {
        self.unavailable
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Swaps the view for a fresh verification pass (the boot check
    /// and every background sweep).
    fn replace(&self, fresh: BTreeMap<String, String>) {
        *self
            .unavailable
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = fresh;
    }

    /// One verification pass over the registry: the map of
    /// application name → reason for every unverified app.
    pub async fn verify(config: &Config, providers: &ProviderRegistry) -> BTreeMap<String, String> {
        let mut unavailable = BTreeMap::new();
        for app in &config.applications {
            if let Err(reason) = verify_application(app, providers).await {
                tracing::warn!(
                    application = %app.name,
                    %reason,
                    "verified-path label does not check out: application unavailable"
                );
                unavailable.insert(app.name.clone(), reason);
            }
        }
        unavailable
    }

    /// Runs one pass and swaps the view (the boot check and the
    /// background sweeps share this).
    pub async fn refresh(&self, config: &Config, providers: &ProviderRegistry) {
        let fresh = Self::verify(config, providers).await;
        self.replace(fresh);
    }

    /// The background retry (F-10: "retried in the background; the
    /// server does not fail"): every `verified_path_interval` the
    /// CURRENT config is re-verified — a renamed-back repo, a healed
    /// provider outage, or a reloaded registry all converge without a
    /// restart. The task lives for the process's life.
    pub fn spawn_background_sweep(
        self: &Arc<Self>,
        config: tokio::sync::watch::Receiver<Arc<Config>>,
        providers: Arc<ProviderRegistry>,
    ) {
        let interval = parse_interval(&config.borrow().reconciler.verified_path_interval);
        let availability = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let current = config.borrow().clone();
                availability.refresh(&current, &providers).await;
            }
        });
    }
}

/// The check for one application: a declared `source.path` is
/// compared (case-insensitive — GitHub paths are case-preserving but
/// case-insensitive, and a case-only mismatch marking an app
/// unavailable forever would be a false alarm) against the provider's
/// truth for the same `{provider, id}`.
async fn verify_application(
    app: &crate::config::ApplicationEntry,
    providers: &ProviderRegistry,
) -> Result<(), String> {
    let Some(declared) = app.source.path.as_deref() else {
        return Ok(());
    };
    // The probe is path-less: the provider must answer from its own
    // truth, never echo the label (the GitHub provider short-circuits
    // a declared path).
    let probe = cargobike_core::model::RepoRef::new(&app.source.provider, &app.source.id);
    let provider = providers.resolve(&probe).map_err(|failure| {
        format!(
            "the source provider `{}` is not registered: {failure}",
            app.source.provider
        )
    })?;
    let truth = provider
        .get_repo_path(&probe)
        .await
        .map_err(|failure| format!("the provider's path lookup failed: {failure}"))?;
    if truth.eq_ignore_ascii_case(declared) {
        Ok(())
    } else {
        Err(format!(
            "the registry declares the repository path `{declared}` but the provider reports `{truth}`"
        ))
    }
}

/// The sweep interval (`reconciler.verified_path_interval`, a
/// humantime string; a malformed value degrades to the default with a
/// warning — the sweep must keep running).
fn parse_interval(raw: &str) -> std::time::Duration {
    humantime::parse_duration(raw).unwrap_or_else(|_| {
        tracing::warn!(
            interval = raw,
            "reconciler.verified_path_interval is malformed: using the 1m default"
        );
        std::time::Duration::from_secs(60)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ApplicationEntry;
    use cargobike_engine::mock::MockProvider;
    use std::sync::Arc;

    /// A scratch-backed mock provider per case (the mock's truth is
    /// the state file, scripted through its seeding helpers).
    fn app_with_path(path: Option<&str>) -> ApplicationEntry {
        let mut app: ApplicationEntry = serde_json::from_value(serde_json::json!({
            "name": "svc",
            "source": { "provider": "mock", "id": "1" },
            "template": "service@1",
            "environments": {}
        }))
        .expect("app parses");
        app.source = crate::config::HashMapEntrySource {
            provider: "mock".to_owned(),
            id: "1".to_owned(),
            path: path.map(str::to_owned),
            owner_id: None,
        };
        app
    }

    fn registry() -> (ProviderRegistry, MockProvider) {
        let scratch = std::env::temp_dir().join(format!(
            "cargobike-availability-{}",
            uuid::Uuid::now_v7().simple()
        ));
        let scratch = scratch.to_string_lossy().to_string();
        let provider = MockProvider::at(&scratch);
        let mut registry = ProviderRegistry::new();
        registry.insert("mock", Arc::new(MockProvider::at(&scratch)));
        (registry, provider)
    }

    #[tokio::test]
    async fn test_matching_label_is_available() {
        let (registry, provider) = registry();
        provider.set_repo_path("1", "org/repo");
        let app = app_with_path(Some("org/repo"));
        assert!(verify_application(&app, &registry).await.is_ok());
    }

    #[tokio::test]
    async fn test_mismatch_marks_the_reason() {
        let (registry, provider) = registry();
        provider.set_repo_path("1", "org/other");
        let app = app_with_path(Some("org/repo"));
        let reason = verify_application(&app, &registry)
            .await
            .expect_err("a mismatch is unavailable");
        assert!(reason.contains("declares the repository path `org/repo`"));
        assert!(reason.contains("reports `org/other`"));
    }

    #[tokio::test]
    async fn test_case_only_difference_stays_available() {
        let (registry, provider) = registry();
        provider.set_repo_path("1", "Org/Repo");
        let app = app_with_path(Some("org/repo"));
        assert!(verify_application(&app, &registry).await.is_ok());
    }

    #[tokio::test]
    async fn test_provider_error_is_unavailable_fail_closed() {
        let (registry, provider) = registry();
        provider.fail_next_path_lookup_for("1");
        let app = app_with_path(Some("org/repo"));
        let reason = verify_application(&app, &registry)
            .await
            .expect_err("an unverifiable label is unavailable");
        assert!(reason.contains("path lookup failed"));
    }

    #[tokio::test]
    async fn test_undeclared_path_has_nothing_to_verify() {
        let app = app_with_path(None);
        let registry = ProviderRegistry::new(); // no provider at all
        assert!(verify_application(&app, &registry).await.is_ok());
    }

    #[tokio::test]
    async fn test_unknown_provider_is_unavailable_fail_closed() {
        let app = app_with_path(Some("org/repo"));
        let registry = ProviderRegistry::new();
        let reason = verify_application(&app, &registry)
            .await
            .expect_err("a label over an unregistered provider is unavailable");
        assert!(reason.contains("not registered"));
    }

    #[test]
    fn test_the_view_swaps_whole() {
        let view = Availability::new();
        assert!(view.reason("svc").is_none());
        view.replace(BTreeMap::from([("svc".to_owned(), "mismatch".to_owned())]));
        assert_eq!(view.reason("svc").as_deref(), Some("mismatch"));
        view.replace(BTreeMap::new());
        assert!(view.reason("svc").is_none());
    }
}
