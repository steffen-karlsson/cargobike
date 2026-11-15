//! Application-group discovery (§4.13a): the deployment repository
//! scan that materialises virtual applications into the live config.
//!
//! The scan is best-effort by design — an unavailable deployment repo
//! logs and keeps the CURRENT registry (a broken scan must not take
//! the server down, the same posture as F-10's availability check) —
//! but it never SILENTLY under-discovers: the provider refuses a
//! truncated tree listing (`list_tree`), the discovery skips carry
//! reasons, and every skip and collision is logged.

use std::collections::BTreeMap;
use std::sync::Arc;

use cargobike_core::config::{ApplicationEntry, ApplicationGroupEntry, Config};
use cargobike_core::groups;
use cargobike_core::model::RepoRef;
use cargobike_core::provider::ProviderError;
use cargobike_core::registry::ProviderRegistry;

/// One group's scan: the deployment repo's tree at the configured
/// ref (the repo's default branch otherwise) through the group's
/// provider, then the pure discovery over the paths. The returned
/// entries are the materialised virtual applications; the skips are
/// the caller's to log.
pub async fn scan_group(
    group: &ApplicationGroupEntry,
    providers: &ProviderRegistry,
) -> Result<(Vec<ApplicationEntry>, Vec<(String, String)>), String> {
    let discovery_repo = &group.discovery.repo;
    let Some(repo_id) = discovery_repo.id.as_deref() else {
        return Err(format!(
            "the group `{}` declares no discovery repository id",
            group.name
        ));
    };
    let repo = RepoRef::new(&discovery_repo.provider, repo_id);
    let provider = providers.resolve(&repo).map_err(|failure| {
        format!(
            "the group `{}` names the unregistered provider `{}`: {failure}",
            group.name, discovery_repo.provider
        )
    })?;
    let git_ref = match group.discovery.ref_.as_deref() {
        Some(declared) => declared.to_owned(),
        None => provider.default_branch(&repo).await.map_err(|failure| {
            format!(
                "the group `{}` reads the default branch: {failure}",
                group.name
            )
        })?,
    };
    let paths = provider.list_tree(&repo, &git_ref).await.map_err(|failure| {
        match failure {
            // A provider without the listing capability is a config
            // mismatch naming the provider, not an outage.
            ProviderError::Unsupported => format!(
                "the group `{}` scans through the provider `{}`, which does not support tree listings",
                group.name, discovery_repo.provider
            ),
            other => format!("the group `{}` scans `{git_ref}`: {other}", group.name),
        }
    })?;
    let discovery = groups::discover(group, &paths)
        .map_err(|error| format!("the group `{}` mis-declares its match: {error}", group.name))?;
    let virtuals = discovery
        .apps
        .iter()
        .map(|(name, environments)| groups::materialise(group, name, environments))
        .collect();
    Ok((virtuals, discovery.skipped))
}

/// The registry rebuild: the resolved explicit + extends applications
/// plus the groups' virtual applications. The precedence rules
/// (§4.13a): an explicit application with the discovered name WINS —
/// a non-`extends` collision is an ERROR (never a silent override),
/// an `extends` override is the per-app override the log names.
pub fn assemble(
    config: &Config,
    virtuals: Vec<ApplicationEntry>,
    group_name: &str,
) -> (Vec<ApplicationEntry>, Vec<String>) {
    let explicit: BTreeMap<&str, &ApplicationEntry> = config
        .applications
        .iter()
        .map(|app| (app.name.as_str(), app))
        .collect();
    let mut assembled = config.applications.clone();
    let mut notes = Vec::new();
    for virtual_app in virtuals {
        match explicit.get(virtual_app.name.as_str()) {
            Some(existing) if existing.extends.is_none() => notes.push(format!(
                "the discovered application `{}` collides with the explicit registry entry of the same name — a collision without `extends` is a validation error, the discovered entry is skipped",
                virtual_app.name
            )),
            Some(existing) => notes.push(format!(
                "the discovered application `{}` is overridden by the explicit `extends: {}` entry",
                virtual_app.name, existing.extends.as_deref().unwrap_or(group_name)
            )),
            None => assembled.push(virtual_app),
        }
    }
    (assembled, notes)
}

/// One full rescan pass: the registry REBUILDS from the resolved
/// EXPLICIT applications (the previous scan's virtuals never leak —
/// a deleted deployment file removes the app, §4.13a's
/// periodic-rescan promise), then every group scans and its virtuals
/// append. `None` when nothing changed (no groups, or every scan
/// failed — the current registry stands).
pub async fn rescan(config: &Arc<Config>, providers: &ProviderRegistry) -> Option<Arc<Config>> {
    if config.application_groups.is_empty() {
        return None;
    }
    // The explicit base: the raw applications resolved again (the
    // current config may carry the previous scan's virtuals).
    let mut fresh = Arc::unwrap_or_clone(Arc::clone(config));
    if let Err(error) = groups::resolve(&mut fresh) {
        tracing::error!(%error, "the discovery rescan could not resolve the explicit registry");
        return None;
    }
    let mut assembled = fresh.applications.clone();
    // The change detector compares against the CURRENT registry (the
    // previous scan's virtuals included): an unchanged rescan sends
    // nothing, so the task's own swap never wakes it in a loop.
    let current: BTreeMap<&str, usize> = config
        .applications
        .iter()
        .map(|app| (app.name.as_str(), app.environments.len()))
        .collect();
    for group in &config.application_groups {
        let (virtuals, skips) = match scan_group(group, providers).await {
            Ok(scan) => scan,
            Err(error) => {
                tracing::warn!(group = %group.name, %error, "the discovery scan failed; the current registry stands");
                continue;
            }
        };
        for (path, reason) in skips {
            tracing::warn!(group = %group.name, path = %path, %reason, "the discovery skipped a path");
        }
        let (merged, notes) = assemble(&fresh, virtuals, &group.name);
        for note in notes {
            tracing::info!(group = %group.name, "{note}");
        }
        assembled = merged;
    }
    fresh.applications = assembled;
    let next: BTreeMap<&str, usize> = fresh
        .applications
        .iter()
        .map(|app| (app.name.as_str(), app.environments.len()))
        .collect();
    if next == current {
        return None;
    }
    Some(Arc::new(fresh))
}

/// The background discovery task: an immediate rescan on every config
/// change (the SIGHUP reload triggers it, §4.13a), and the interval
/// tick rescans the groups whose interval elapsed. The tick is one
/// minute; a group's own `interval` bounds its scans.
pub fn spawn_discovery(
    state: Arc<crate::http::AppState>,
    config_tx: tokio::sync::watch::Sender<Arc<Config>>,
) {
    tokio::spawn(async move {
        let mut last_scan: BTreeMap<String, std::time::Instant> = BTreeMap::new();
        let mut config_watch = state.config.clone();
        loop {
            // The first pass runs immediately (the startup scan);
            // later passes run on the tick or on a config change.
            let changed = config_watch.changed();
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {}
                _ = changed => {}
            }
            let config = Arc::clone(&config_watch.borrow());
            for group in &config.application_groups {
                let interval = humantime::parse_duration(&group.discovery.interval)
                    .unwrap_or(std::time::Duration::from_secs(600));
                let due = last_scan
                    .get(&group.name)
                    .is_none_or(|scanned| scanned.elapsed() >= interval);
                if !due {
                    continue;
                }
                last_scan.insert(group.name.clone(), std::time::Instant::now());
            }
            if let Some(fresh) = rescan(&config, &state.engine.services.providers).await {
                let _ = config_tx.send(fresh);
            }
        }
    });
}
