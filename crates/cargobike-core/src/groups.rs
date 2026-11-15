//! The application-group machinery (§4.13a): the config FILE's raw
//! applications (`extends` overrides may omit `source`/`template`)
//! and the application groups merge into the RESOLVED registry every
//! consumer reads — shared by the server's boot, the periodic
//! discovery rescan, and the CLI's offline `validate` (4.8). Pure:
//! the provider I/O (the repo scan that feeds [`discover`]) is the
//! caller's.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::{
    ApplicationEntry, ApplicationGroupEntry, Config, EnvironmentEntry, RawApplicationEntry,
};

/// The registry resolution's failures (the load-time family; boot and
/// the offline validate refuse on them).
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// An `extends` names an unknown group.
    #[error("application `{application}` extends the unknown group `{group}`")]
    UnknownGroup {
        /// The app that carries the reference.
        application: String,
        /// The referenced group name.
        group: String,
    },
    /// A non-extends app omitted a required field.
    #[error(
        "application `{application}` must declare `{field}` (only an `extends` override may omit it)"
    )]
    MissingField {
        /// The app that omitted the field.
        application: String,
        /// The field's config key.
        field: &'static str,
    },
    /// A discovery match pattern is not a usable pattern.
    #[error("the match pattern `{pattern}` is invalid: {problem}")]
    BadPattern {
        /// The offending pattern.
        pattern: String,
        /// What is wrong with it.
        problem: &'static str,
    },
}

/// One segment of a discovery match pattern: at most one placeholder,
/// with literal text around it (`apps-{application}.yaml` — the
/// placeholders sit INSIDE segments; `apps/{application}/{environment}.yaml`
/// is two single-placeholder segments).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
    /// A literal path segment.
    Literal(String),
    /// One placeholder with its literal prefix and suffix.
    Capture {
        /// The literal before the placeholder.
        prefix: String,
        /// What the segment captures.
        kind: CaptureKind,
        /// The literal after the placeholder.
        suffix: String,
    },
}

/// What a pattern segment captures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureKind {
    /// `{application}`.
    Application,
    /// `{environment}`.
    Environment,
}

/// A discovery match pattern, e.g. `apps/{application}/{environment}.yaml`
/// (§4.13a): placeholders capture the text between the segment's
/// literal prefix and suffix; a segment holds at most one placeholder.
#[derive(Clone, Debug)]
pub struct MatchPattern {
    segments: Vec<Segment>,
    /// The pattern as written (the diagnostics keep the source).
    source: String,
}

impl MatchPattern {
    /// Compiles a pattern. `{application}` is required (the discovery
    /// extracts names with it); `{environment}` captures the
    /// environment when the pattern carries it. A segment with two
    /// placeholders refuses (ambiguous capture boundaries).
    pub fn new(pattern: &str) -> Result<Self, RegistryError> {
        let mut segments = Vec::new();
        let mut has_application = false;
        for segment in pattern.split('/') {
            let mut pieces: Vec<(usize, CaptureKind)> = Vec::new();
            for (needle, kind) in [
                ("{application}", CaptureKind::Application),
                ("{environment}", CaptureKind::Environment),
            ] {
                let mut search = 0;
                while let Some(at) = segment[search..].find(needle) {
                    pieces.push((search + at, kind));
                    search += at + needle.len();
                }
            }
            pieces.sort_by_key(|(at, _)| *at);
            if pieces.is_empty() {
                segments.push(Segment::Literal(segment.to_owned()));
                continue;
            }
            if pieces.len() > 1 {
                return Err(RegistryError::BadPattern {
                    pattern: pattern.to_owned(),
                    problem: "a pattern segment holds at most one `{...}` placeholder",
                });
            }
            let (at, kind) = pieces[0];
            let placeholder_len = if kind == CaptureKind::Application {
                "{application}".len()
            } else {
                "{environment}".len()
            };
            has_application |= kind == CaptureKind::Application;
            segments.push(Segment::Capture {
                prefix: segment[..at].to_owned(),
                kind,
                suffix: segment[at + placeholder_len..].to_owned(),
            });
        }
        if !has_application {
            return Err(RegistryError::BadPattern {
                pattern: pattern.to_owned(),
                problem: "the pattern must contain an `{application}` placeholder",
            });
        }
        Ok(Self {
            segments,
            source: pattern.to_owned(),
        })
    }

    /// The pattern as written.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Extracts `(application, environment)` from a repository path;
    /// `None` when the path does not match. A pattern without an
    /// `{environment}` placeholder yields `None` for the environment —
    /// the per-environment patterns are declared ON an environment, so
    /// the name comes from the declaration, not the path.
    pub fn captures(&self, path: &str) -> Option<(String, Option<String>)> {
        let mut application = None;
        let mut environment = None;
        let mut segments = self.segments.iter();
        let mut path_segments = path.split('/');
        loop {
            match (segments.next(), path_segments.next()) {
                (None, None) => break,
                (Some(Segment::Literal(literal)), Some(segment)) if literal == segment => {}
                (
                    Some(Segment::Capture {
                        prefix,
                        kind,
                        suffix,
                    }),
                    Some(segment),
                ) => {
                    let stripped = segment.strip_prefix(prefix.as_str())?;
                    let captured = stripped.strip_suffix(suffix.as_str())?;
                    if captured.is_empty() {
                        return None;
                    }
                    match kind {
                        CaptureKind::Application => application = Some(captured.to_owned()),
                        CaptureKind::Environment => environment = Some(captured.to_owned()),
                    }
                }
                _ => return None,
            }
        }
        application.map(|application| (application, environment))
    }
}

/// The valid discovered-name shape (§4.13a): `^[a-z0-9][a-z0-9-]{0,62}$`
/// — discovered names flow into branch names, change-request titles,
/// and API paths.
pub fn valid_discovered_name(name: &str) -> bool {
    let mut characters = name.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => return false,
    }
    let rest = characters.as_str();
    rest.len() <= 62
        && rest
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// One group's discovery over a listing of repository paths: the
/// applications found (name → the environments whose match hit) and
/// the skips (path + reason — the caller logs them; a skip is never a
/// silent drop).
#[derive(Clone, Debug, Default)]
pub struct Discovery {
    /// Application name → the environment names with a matching file.
    pub apps: BTreeMap<String, BTreeSet<String>>,
    /// Skipped paths with the reason.
    pub skipped: Vec<(String, String)>,
}

/// Runs one group's discovery over the scanned paths: each declared
/// environment matches its own `match` pattern (the group's global
/// pattern otherwise); a global-pattern capture naming an undeclared
/// environment skips that path (the group did not declare it).
pub fn discover(
    group: &ApplicationGroupEntry,
    paths: &[String],
) -> Result<Discovery, RegistryError> {
    let global = MatchPattern::new(&group.discovery.match_)?;
    let mut discovery = Discovery::default();
    let mut matched: Vec<&String> = Vec::new();
    for (environment_name, entry) in &group.environments {
        let pattern = match entry.match_.as_deref() {
            Some(per_environment) => MatchPattern::new(per_environment)?,
            None => global.clone(),
        };
        for path in paths {
            let Some((application, captured_environment)) = pattern.captures(path) else {
                continue;
            };
            // A pattern that names no environment is declared ON this
            // environment — the declaration, not the path, names it.
            // A capture naming ANOTHER environment is that
            // environment's business (its own pattern claims the
            // path); the unmatched-path pass below reports a path the
            // group does not declare at all.
            if captured_environment
                .as_deref()
                .is_some_and(|captured| captured != environment_name)
            {
                continue;
            }
            if !valid_discovered_name(&application) {
                discovery.skipped.push((
                    path.clone(),
                    format!(
                        "the discovered name `{application}` fails `^[a-z0-9][a-z0-9-]{{0,62}}$`"
                    ),
                ));
                continue;
            }
            matched.push(path);
            discovery
                .apps
                .entry(application)
                .or_default()
                .insert(environment_name.clone());
        }
    }
    // The paths no environment claimed: the ones the GLOBAL pattern
    // captured for an environment the group never declared — a skip
    // with the reason, never a silent drop.
    for path in paths {
        if matched.contains(&path) {
            continue;
        }
        if let Some((application, Some(environment))) = global.captures(path) {
            if valid_discovered_name(&application) {
                discovery.skipped.push((
                    path.clone(),
                    format!(
                        "the pattern captured the environment `{environment}`, which the group does not declare"
                    ),
                ));
            }
        }
    }
    Ok(discovery)
}

/// Materialises one discovered application from its group: the group's
/// config with the `{application}` placeholders in `edits[].file`
/// resolved to the discovered name (§4.13a), restricted to the
/// environments the discovery found live.
pub fn materialise(
    group: &ApplicationGroupEntry,
    application: &str,
    environments: &BTreeSet<String>,
) -> ApplicationEntry {
    let resolved_environments: BTreeMap<String, EnvironmentEntry> = group
        .environments
        .iter()
        .filter(|(name, _)| environments.contains(name.as_str()))
        .map(|(name, entry)| {
            (
                name.clone(),
                resolve_environment_placeholders(entry, application, &group.discovery.repo),
            )
        })
        .collect();
    ApplicationEntry {
        name: application.to_owned(),
        description: None,
        labels: None,
        source: group.source.clone(),
        template: group.template.clone(),
        versioning: group.versioning.clone(),
        triggers: group.triggers.clone(),
        releasers: group.releasers.clone(),
        extends: Some(group.name.clone()),
        inputs: None,
        paused: None,
        environments: resolved_environments,
    }
}

/// Resolves `{application}` inside one environment's `edits[].file`,
/// and defaults the environment's `repo` to the group's DISCOVERY
/// repository: a group's edits target the deployment repository the
/// discovery scanned (an environment may still pin its own repo).
fn resolve_environment_placeholders(
    entry: &EnvironmentEntry,
    application: &str,
    discovery_repo: &crate::config::HashMapEntrySource,
) -> EnvironmentEntry {
    let mut resolved = entry.clone();
    if resolved.repo.is_none() {
        resolved.repo = Some(discovery_repo.clone());
    }
    if let Some(edits) = &mut resolved.edits {
        for edit in edits {
            edit.file = edit.file.replace("{application}", application);
        }
    }
    resolved
}

/// Merges an `extends` override onto the group's base: maps merge
/// deeply, lists replace, `source` replaces as a whole (§4.13a's merge
/// rules). Scalars that serde cannot distinguish from their defaults
/// (the environment bool flags) do NOT inherit — an override declares
/// them explicitly when they matter.
fn merge_override(base: ApplicationEntry, over: &RawApplicationEntry) -> ApplicationEntry {
    let mut merged = base;
    if let Some(source) = &over.source {
        merged.source = source.clone();
    }
    if let Some(template) = &over.template {
        merged.template = template.clone();
    }
    if over.versioning != crate::config::VersioningEntry::default() {
        merged.versioning = over.versioning.clone();
    }
    if !over.triggers.is_empty() {
        merged.triggers = over.triggers.clone();
    }
    if !over.releasers.is_empty() {
        merged.releasers = over.releasers.clone();
    }
    if over.description.is_some() {
        merged.description = over.description.clone();
    }
    if over.labels.is_some() {
        merged.labels = over.labels.clone();
    }
    if over.inputs.is_some() {
        merged.inputs = over.inputs.clone();
    }
    if over.paused.is_some() {
        merged.paused = over.paused.clone();
    }
    // The environments map merges deeply: the group's environments the
    // override does not name survive as-is; a named environment merges
    // field-wise (the override's `Some`s and lists win).
    for (name, over_environment) in &over.environments {
        let merged_environment = match merged.environments.remove(name) {
            Some(group_environment) => merge_environment(group_environment, over_environment),
            None => over_environment.clone(),
        };
        merged.environments.insert(name.clone(), merged_environment);
    }
    merged
}

/// The per-environment field-wise merge (the override's set fields win;
/// lists replace).
fn merge_environment(
    group_environment: EnvironmentEntry,
    over: &EnvironmentEntry,
) -> EnvironmentEntry {
    let mut merged = group_environment;
    if over.repo.is_some() {
        merged.repo = over.repo.clone();
    }
    if over.edits.is_some() {
        merged.edits = over.edits.clone();
    }
    if over.concurrency.is_some() {
        merged.concurrency = over.concurrency;
    }
    if over.change_request.is_some() {
        merged.change_request = over.change_request.clone();
    }
    if over.commit_message.is_some() {
        merged.commit_message = over.commit_message.clone();
    }
    if over.inputs.is_some() {
        merged.inputs = over.inputs.clone();
    }
    merged.require_branch_protection = over.require_branch_protection;
    merged.allow_direct_commit = over.allow_direct_commit;
    merged
}

/// Resolves the config's registry: every raw application becomes a
/// resolved entry — explicit apps carry their own `source` and
/// `template`; `extends` overrides merge onto their group's base (the
/// group's environments resolved for the app's name, the placeholder
/// resolved). Discovery's virtual apps are the caller's (the scan is
/// I/O); this is the merge the boot and the rescan share.
pub fn resolve(config: &mut Config) -> Result<(), RegistryError> {
    let groups: BTreeMap<&str, &ApplicationGroupEntry> = config
        .application_groups
        .iter()
        .map(|group| (group.name.as_str(), group))
        .collect();
    let mut resolved = Vec::with_capacity(config.raw_applications.len());
    for raw in &config.raw_applications {
        let entry = match &raw.extends {
            Some(group_name) => {
                let group = groups
                    .get(group_name.as_str())
                    .ok_or(RegistryError::UnknownGroup {
                        application: raw.name.clone(),
                        group: group_name.clone(),
                    })?;
                // The group's base resolved for THIS app's name: the
                // placeholders (`edits[].file`) resolve to it, and the
                // whole declared environment set is the base.
                let all: BTreeSet<String> = group.environments.keys().cloned().collect();
                let base = materialise(group, &raw.name, &all);
                merge_override(base, raw)
            }
            None => ApplicationEntry {
                name: raw.name.clone(),
                description: raw.description.clone(),
                labels: raw.labels.clone(),
                source: raw.source.clone().ok_or(RegistryError::MissingField {
                    application: raw.name.clone(),
                    field: "source",
                })?,
                template: raw.template.clone().ok_or(RegistryError::MissingField {
                    application: raw.name.clone(),
                    field: "template",
                })?,
                versioning: raw.versioning.clone(),
                triggers: raw.triggers.clone(),
                releasers: raw.releasers.clone(),
                extends: None,
                inputs: raw.inputs.clone(),
                paused: raw.paused.clone(),
                environments: raw.environments.clone(),
            },
        };
        resolved.push(entry);
    }
    config.applications = resolved;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)] // the fixtures' panics are the failures
mod tests {
    use super::*;
    use serde_yaml_ng::from_str;

    fn group_yaml() -> ApplicationGroupEntry {
        from_str(
            "name: monorepo-deploy\ntemplate: service@1\nversioning: { scheme: semver, tag_format: \"v{version}\" }\nreleasers: [{ oidc: gha }]\nsource: { provider: github, owner_id: \"123456\" }\ndiscovery:\n  repo: { provider: github, id: \"789012\" }\n  match: \"apps/{application}/{environment}.yaml\"\nenvironments:\n  preview:\n    edits:\n      - file: \"apps/{application}/preview.yaml\"\n        field: image.tag\n  production:\n    match: \"apps/{application}/prod.yaml\"\n    concurrency: supersede\n    edits:\n      - file: \"apps/{application}/prod.yaml\"\n        field: image.tag\n",
        )
        .expect("the group")
    }

    /// The global pattern captures both names; the per-environment
    /// pattern replaces the global one for its environment.
    #[test]
    fn test_match_pattern_captures_application_and_environment() {
        let pattern = MatchPattern::new("apps/{application}/{environment}.yaml").expect("pattern");
        assert_eq!(
            pattern.captures("apps/web/preview.yaml"),
            Some(("web".to_owned(), Some("preview".to_owned())))
        );
        assert_eq!(pattern.captures("apps/web/nested/preview.yaml"), None);
        assert_eq!(pattern.captures("docs/readme.md"), None);
        let per_environment = MatchPattern::new("apps/{application}/prod.yaml").expect("pattern");
        assert_eq!(
            per_environment.captures("apps/web/prod.yaml"),
            Some(("web".to_owned(), None))
        );
    }

    /// A pattern without `{application}` refuses.
    #[test]
    fn test_match_pattern_requires_the_application_segment() {
        let failure = MatchPattern::new("apps/*.yaml");
        assert!(failure.is_err());
    }

    /// The discovery: the declared environments match their patterns,
    /// the invalid names skip with the reason, a global capture of an
    /// undeclared environment skips.
    #[test]
    fn test_discovery_finds_apps_and_skips_with_reasons() {
        let group = group_yaml();
        let paths: Vec<String> = [
            "apps/web/preview.yaml",
            "apps/web/whatever.yaml", // the global pattern captures an undeclared env
            "apps/web/prod.yaml",     // production's own pattern
            "apps/BAD_NAME/preview.yaml",
            "apps/billing/preview.yaml",
        ]
        .iter()
        .map(|p| (*p).to_owned())
        .collect();
        let discovery = discover(&group, &paths).expect("the discovery");
        let web = discovery.apps.get("web").expect("web");
        assert!(web.contains("preview") && web.contains("production"));
        let billing = discovery.apps.get("billing").expect("billing");
        assert!(billing.contains("preview"));
        assert_eq!(discovery.skipped.len(), 2, "{:?}", discovery.skipped);
        assert!(
            discovery
                .skipped
                .iter()
                .any(|(path, _)| path.ends_with("BAD_NAME/preview.yaml")),
            "the invalid name skips: {:?}",
            discovery.skipped
        );
    }

    /// The materialisation: the `{application}` placeholder resolves
    /// in `edits[].file`, and only the discovered environments ride.
    #[test]
    fn test_materialise_resolves_placeholders_and_filters_environments() {
        let group = group_yaml();
        let mut environments = BTreeSet::new();
        environments.insert("preview".to_owned());
        let app = materialise(&group, "web", &environments);
        assert_eq!(app.name, "web");
        assert_eq!(app.template, "service@1");
        assert_eq!(app.source.owner_id.as_deref(), Some("123456"));
        assert_eq!(app.environments.len(), 1, "only the discovered env");
        let preview = app.environments.get("preview").expect("preview");
        let edit = preview.edits.as_ref().expect("edits")[0].clone();
        assert_eq!(edit.file, "apps/web/preview.yaml");
        // The environment's repo defaults to the DISCOVERY repository
        // — a group's edits target the deployment repo it scans.
        assert_eq!(
            preview.repo.as_ref().expect("the defaulted repo").id,
            Some("789012".to_owned())
        );
    }

    /// The extends merge: the override's `source` replaces whole, the
    /// environments merge deeply (the untouched group environment
    /// survives; the named one merges field-wise).
    #[test]
    fn test_extends_merge_replaces_source_and_merges_environments() {
        let group = group_yaml();
        let all: BTreeSet<String> = group.environments.keys().cloned().collect();
        let base = materialise(&group, "payment-service", &all);
        let over: RawApplicationEntry = from_str(
            "name: payment-service\nextends: monorepo-deploy\nsource: { provider: github, id: \"345678\" }\nenvironments:\n  production:\n    concurrency: queue\n",
        )
        .expect("the override");
        let merged = merge_override(base, &over);
        assert_eq!(merged.source.id.as_deref(), Some("345678"));
        assert_eq!(merged.source.owner_id, None, "source replaces WHOLE");
        assert_eq!(merged.template, "service@1", "the template inherits");
        // The group's production edits survive the deep merge; the
        // concurrency the override set wins.
        let production = merged.environments.get("production").expect("production");
        assert_eq!(
            production.concurrency,
            Some(crate::config::ConcurrencyValue::Queue)
        );
        assert!(
            production.edits.as_ref().is_some_and(|edits| edits
                .iter()
                .any(|edit| edit.file == "apps/payment-service/prod.yaml")),
            "the group's edits survive: {production:?}"
        );
        assert!(
            merged.environments.contains_key("preview"),
            "the untouched group environment survives"
        );
    }

    /// The resolution: explicit apps carry their own fields; the
    /// extends override merges; a missing `source` on a non-extends
    /// app refuses naming the app.
    #[test]
    fn test_resolve_explicit_and_extends_apps() {
        let mut config: Config = from_str(
            "server: {}\ndatabase: { url: postgres://x }\nauth: {}\napplications:\n  - name: web\n    source: { provider: github, id: \"42\" }\n    template: service@1\n    environments: {}\n  - name: payment-service\n    extends: monorepo-deploy\n    source: { provider: github, id: \"345678\" }\n    environments:\n      production:\n        concurrency: queue\napplication_groups:\n  - name: monorepo-deploy\n    template: service@1\n    source: { provider: github, owner_id: \"123456\" }\n    discovery:\n      repo: { provider: github, id: \"789012\" }\n      match: \"apps/{application}/{environment}.yaml\"\n    environments:\n      production:\n        edits:\n          - file: \"apps/{application}/prod.yaml\"\n            field: image.tag\n",
        )
        .expect("the config");
        resolve(&mut config).expect("resolves");
        assert_eq!(config.applications.len(), 2);
        let web = config.applications.iter().find(|app| app.name == "web");
        assert!(web.is_some());
        let payment = config
            .applications
            .iter()
            .find(|app| app.name == "payment-service")
            .expect("payment");
        assert_eq!(payment.source.id.as_deref(), Some("345678"));
        assert_eq!(payment.environments.len(), 1, "the group env merged in");
    }

    #[test]
    fn test_resolve_refuses_missing_source_on_explicit_app() {
        let mut config: Config = from_str(
            "server: {}\ndatabase: { url: postgres://x }\nauth: {}\napplications:\n  - name: web\n    template: service@1\n    environments: {}\n",
        )
        .expect("the config");
        let failure = resolve(&mut config);
        assert!(
            failure
                .as_ref()
                .is_err_and(|error| error.to_string().contains("must declare `source`")),
            "{failure:?}"
        );
    }

    /// The discovered-name shape: the leading alnum, the 63-cap, the
    /// lowercase-only rule.
    #[test]
    fn test_valid_discovered_name_shape() {
        assert!(valid_discovered_name("web"));
        assert!(valid_discovered_name("payment-service"));
        assert!(valid_discovered_name("a1"));
        assert!(!valid_discovered_name("Web"));
        assert!(!valid_discovered_name("-web"));
        assert!(!valid_discovered_name("web_thing"));
        assert!(!valid_discovered_name(&"w".repeat(64)));
        assert!(valid_discovered_name(&"w".repeat(63)));
    }
}
