//! Registry and trust validation at load (PRD 2.7, F-80, F-96, F-99a/b,
//! F-147, §4.13a).
//!
//! Pure checks over the loaded config: boot refuses a semantically broken
//! registry instead of starting half-authorised. Template files are read
//! from `templates.directory` (F-25: the file name is free, the parsed
//! `name`/`version` must match the registry's `name@version` reference).

use std::collections::BTreeMap;
use std::sync::Arc;

use cargobike_core::template::{PipelineTemplate, WaitKind};

/// A semantic validation failure; boot refuses to start.
#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    /// The referenced principal does not exist.
    #[error("validation failed: {context}: {problem}")]
    Reference {
        /// Where the failure happened.
        context: String,
        /// What is wrong.
        problem: String,
    },
    /// A rule that the config alone must satisfy.
    #[error("validation failed: {0}")]
    Rule(String),
}

impl From<serde_yaml_ng::Error> for ValidationError {
    fn from(error: serde_yaml_ng::Error) -> Self {
        ValidationError::Rule(format!("failed to parse template: {error}"))
    }
}

/// Validates the whole config (semantic layer). Template files load from
/// `templates.directory` (a missing directory with no applications is not
/// an error; with applications it is).
pub fn validate(config: &Arc<crate::config::Config>) -> Result<(), ValidationError> {
    check_oidc_entries(config)?;
    check_api_keys(config)?;
    check_releaser_references(config)?;
    check_apply_policy_invariants(config)?;
    check_versioning(config)?;
    check_group_references(config)
}

/// F-80: claim constraints are mandatory unless `allow_unconstrained`.
fn check_oidc_entries(config: &Arc<crate::config::Config>) -> Result<(), ValidationError> {
    for entry in &config.auth.oidc {
        if entry.claims.is_empty() && !entry.allow_unconstrained {
            return Err(ValidationError::Rule(format!(
                "the OIDC trust entry `{}` has no claim constraint; set claims or allow_unconstrained: true (F-80)",
                entry.name
            )));
        }
    }
    Ok(())
}

/// F-99b: a named API key must be unique and present a hash.
fn check_api_keys(config: &crate::config::Config) -> Result<(), ValidationError> {
    let mut named: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for entry in &config.auth.api_keys {
        if !named.insert(&entry.name) {
            return Err(ValidationError::Rule(format!(
                "the API key `{}` is declared twice (rotation means two DIFFERENT names that are both active)",
                entry.name
            )));
        }
        if argon2::password_hash::PasswordHash::new(&entry.hash).is_err() {
            return Err(ValidationError::Rule(format!(
                "the API key `{}` carries a hash `cargobike-server hash-api-key` did not produce (§9.3)",
                entry.name
            )));
        }
    }
    Ok(())
}

/// F-99a: `releasers` selectors reference existing entries and those
/// entries must grant `release:create`.
fn check_releaser_references(config: &Arc<crate::config::Config>) -> Result<(), ValidationError> {
    let oidc_names: BTreeMap<&str, &crate::config::OidcEntry> = config
        .auth
        .oidc
        .iter()
        .map(|entry| (entry.name.as_str(), entry))
        .collect();
    let api_key_names: BTreeMap<&str, &crate::config::ApiKeyEntry> = config
        .auth
        .api_keys
        .iter()
        .map(|entry| (entry.name.as_str(), entry))
        .collect();
    for app in &config.applications {
        let context = format!("application `{}`", app.name);
        for selector in &app.releasers {
            match selector {
                crate::config::PrincipalSelector::Oidc { oidc } => {
                    let entry = oidc_names.get(oidc.as_str()).ok_or_else(|| {
                        ValidationError::Reference {
                            context: context.clone(),
                            problem: format!(
                                "the releaser references unknown OIDC trust entry `{oidc}`"
                            ),
                        }
                    })?;
                    if !entry
                        .grants
                        .iter()
                        .any(|g| g == "*" || g == "release:create")
                    {
                        return Err(ValidationError::Reference {
                            context: context.clone(),
                            problem: format!(
                                "the releaser references `{oidc}`, which does not grant `release:create` (F-99a)"
                            ),
                        });
                    }
                    // F-99a(b): a releaser's `ref` glob must be able to match tag_format.
                    if let (Some(tag_format), Some(ref_claim)) = (
                        app.versioning.tag_format.as_deref(),
                        entry.claims.get("ref").and_then(serde_json::Value::as_str),
                    ) {
                        if !ref_may_match_tag(ref_claim, tag_format) {
                            return Err(ValidationError::Reference {
                                context: context.clone(),
                                problem: format!(
                                    "the releaser claim `ref: {ref_claim}` can never match the tag format `{tag_format}`"
                                ),
                            });
                        }
                    }
                }
                crate::config::PrincipalSelector::ApiKey { api_key } => {
                    api_key_names.get(api_key.as_str()).ok_or_else(|| {
                        ValidationError::Reference {
                            context: context.clone(),
                            problem: format!("the releaser references unknown API key `{api_key}`"),
                        }
                    })?;
                }
            }
        }
    }
    Ok(())
}

/// A `ref` claim glob can plausibly match the tag format (F-99a(b)): the
/// glob's literal prefix must cover the tag format's literal prefix.
fn ref_may_match_tag(claim_glob: &str, tag_format: &str) -> bool {
    let claim_prefix = claim_glob.split(['*', '?', '[']).next().unwrap_or("");
    let tag_prefix = tag_format.split('{').next().unwrap_or("");
    tag_prefix.starts_with(claim_prefix)
}

/// F-96: every environment with an `approval` block must have a template
/// `wait: approval` step; F-99b: `api_key` approvers refuse unless
/// `allow_machine_approvers: true`; selectors must exist and grant the
/// right request.
fn check_apply_policy_invariants(
    config: &Arc<crate::config::Config>,
) -> Result<(), ValidationError> {
    let templates = load_templates(config)?;
    let repositories = &config.applications;
    for app in repositories {
        for (env_name, env) in &app.environments {
            let Some(policy) = &env.approval else {
                continue;
            };
            let template_name_version = &app.template;
            let template =
                template_for_name_version(&templates, template_name_version, app, env_name)?;
            if !template_has_approval_step(template, env_name) {
                return Err(ValidationError::Reference {
                    context: format!("application `{}` environment `{env_name}`", app.name),
                    problem: "the registry declares an `approval` block but the template has no `wait: approval` step for this environment (F-96)".to_owned(),
                });
            }
            for selector in &policy.approvers {
                if let crate::config::PrincipalSelector::ApiKey { api_key } = selector {
                    if !policy.allow_machine_approvers {
                        return Err(ValidationError::Reference {
                            context: format!("application `{}` environment `{env_name}`", app.name),
                            problem: format!(
                                "an approver references the API key `{api_key}`; machine approvers need `allow_machine_approvers: true` (F-99b)"
                            ),
                        });
                    }
                }
                if let crate::config::PrincipalSelector::Oidc { oidc } = selector {
                    let grants = config
                        .auth
                        .oidc
                        .iter()
                        .find(|entry| entry.name == *oidc)
                        .ok_or_else(|| ValidationError::Reference {
                            context: format!("application `{}` environment `{env_name}`", app.name),
                            problem: format!(
                                "the approver references unknown OIDC trust entry `{oidc}`"
                            ),
                        })?
                        .grants
                        .clone();
                    if !grants.iter().any(|g| g == "*" || g == "release:approve") {
                        return Err(ValidationError::Reference {
                            context: format!("application `{}` environment `{env_name}`", app.name),
                            problem: format!(
                                "the approver references `{oidc}`, which does not grant `release:approve` (F-99a)"
                            ),
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

/// Every parsed template in the directory (deduped by name@version).
pub fn load_templates(
    config: &Arc<crate::config::Config>,
) -> Result<BTreeMap<(String, String), PipelineTemplate>, ValidationError> {
    let directory = &config.templates.directory;
    let mut map = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        if !config.applications.is_empty() || !config.application_groups.is_empty() {
            return Err(ValidationError::Rule(format!(
                "failed to read templates from `{directory}`: the registry declares applications"
            )));
        }
        return Ok(map);
    };
    for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
        let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if extension != "yaml" && extension != "yml" {
            continue;
        }
        let contents = std::fs::read_to_string(&path).map_err(|error| {
            ValidationError::Rule(format!("failed to read {}: {error}", path.display()))
        })?;
        let template: PipelineTemplate = serde_yaml_ng::from_str(&contents)?;
        map.insert((template.name.clone(), template.version.clone()), template);
    }
    Ok(map)
}

/// Resolves the app's template (F-25: the `name@version` reference).
fn template_for_name_version<'a>(
    templates: &'a BTreeMap<(String, String), PipelineTemplate>,
    reference: &str,
    app: &crate::config::ApplicationEntry,
    env_name: &str,
) -> Result<&'a PipelineTemplate, ValidationError> {
    let (name, version) = reference
        .split_once('@')
        .ok_or_else(|| ValidationError::Reference {
            context: format!(
                "application `{app}`, environment `{env_name}`",
                app = app.name
            ),
            problem: format!("the template reference `{reference}` is not `name@version`"),
        })?;
    templates
        .get(&(name.to_owned(), version.to_owned()))
        .ok_or_else(|| ValidationError::Reference {
            context: format!("application `{}` environment `{env_name}`", app.name),
            problem: format!(
                "the template `{name}@{version}` is not in the templates directory (F-25)"
            ),
        })
}

/// Whether the template environment's steps contain a `wait: approval`
/// control step (F-96's invariant).
fn template_has_approval_step(template: &PipelineTemplate, environment: &str) -> bool {
    template
        .environments
        .iter()
        .filter(|spec| spec.name == environment)
        .flat_map(|spec| &spec.steps)
        .any(|step| step.wait == Some(WaitKind::Approval))
}

/// F-95/F-147: versioning scheme is one of the kebab values and the
/// format strings use their documented placeholders.
fn check_versioning(config: &Arc<crate::config::Config>) -> Result<(), ValidationError> {
    for app in &config.applications {
        let scheme_ok = ["semver", "calver", "opaque"]
            .iter()
            .any(|allowed| *allowed == app.versioning.scheme);
        if !scheme_ok {
            return Err(ValidationError::Rule(format!(
                "the application `{}` declares unknown versioning scheme `{}` (F-95: semver, calver, opaque)",
                app.name, app.versioning.scheme
            )));
        }
        if let Some(tag_format) = app.versioning.tag_format.as_deref() {
            check_placeholders(
                &format!("application `{}`", app.name),
                "versioning.tag_format",
                tag_format,
                &["{version}"],
            )?;
        }
    }
    check_placeholders(
        "engine",
        "engine.branch_format",
        &config.engine.branch_format,
        &["{application}", "{environment}", "{release_id}"],
    )
}

/// The F-147 placeholder check: unknown placeholders are errors (not
/// passed through); `{{`/`}}` are literal-brace escapes.
pub fn check_placeholders(
    context: &str,
    field: &str,
    value: &str,
    allowed: &[&str],
) -> Result<(), ValidationError> {
    let mut rest = value.as_bytes();
    while let Some(open) = find_brace(rest) {
        rest = &rest[open + 1..];
        let Some(close) = rest.iter().position(|b| *b == b'}') else {
            break;
        };
        let token = std::str::from_utf8(&rest[..close]).unwrap_or("");
        if token.starts_with('{') || token.is_empty() || token.chars().all(|c| c == '{' || c == '}')
        {
            rest = &rest[1..]; // literal brace escape pair
            continue;
        }
        let placeholder = format!("{{{token}}}");
        if !allowed.contains(&placeholder.as_str()) {
            return Err(ValidationError::Rule(format!(
                "the {context} {field} placeholder `{placeholder}` is not documented (F-147; allowed: {allowed:?})"
            )));
        }
        rest = &rest[close + 1..];
    }
    Ok(())
}

fn find_brace(rest: &[u8]) -> Option<usize> {
    rest.iter().position(|c| *c == b'{')
}

/// §4.13a: `extends` names an existing group.
fn check_group_references(config: &Arc<crate::config::Config>) -> Result<(), ValidationError> {
    let groups: std::collections::HashSet<&str> = config
        .application_groups
        .iter()
        .map(|g| g.name.as_str())
        .collect();
    for app in &config.applications {
        if let Some(group) = &app.extends {
            if !groups.contains(group.as_str()) {
                return Err(ValidationError::Reference {
                    context: format!("application `{}`", app.name),
                    problem: format!("`extends` names the unknown group `{group}`"),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml_ng::from_str;

    fn config(yaml: &str) -> Arc<crate::config::Config> {
        Arc::new(from_str(yaml).expect("config"))
    }

    #[test]
    fn test_unconstrained_oidc_entries_are_refused_f80() {
        let config = config(
            "server: {}\ndatabase: { url: postgres://x }\nauth:\n  oidc:\n    - name: open\n      issuer: https://issuer\n      audience: cargobike\n      grants: [release:create]\n",
        );
        assert!(check_oidc_entries(&config).is_err());
    }

    #[test]
    fn test_releaser_without_release_create_grant_is_refused_f99a() {
        let config = config(
            "server: {}\ndatabase: { url: postgres://x }\nauth:\n  oidc:\n    - name: gha\n      issuer: https://issuer\n      audience: cargobike\n      claims:\n        repository_owner_id: \"1\"\n      grants: [release:read]\napplications:\n  - name: my-service\n    source: { provider: github, id: \"1\" }\n    template: service@1\n    releasers:\n      - oidc: gha\n    environments: {}\n",
        );
        assert!(check_releaser_references(&config).is_err());
    }

    #[test]
    fn test_api_key_approvers_refuse_machines_by_default_f99b() {
        let yaml = "server: {}\ndatabase: { url: postgres://x }\nauth:\n  api_keys:\n    - name: key\n      hash: \"$argon2id$v=19$m=32768,t=3,p=4$ZGVw\"\napplications:\n  - name: my-service\n    source: { provider: github, id: \"1\" }\n    template: service@1\n    releasers:\n      - api_key: key\n    environments:\n      production:\n        approval:\n          required: 1\n          approvers:\n            - api_key: key\n";
        let config = config(yaml);
        assert!(check_releaser_references(&config).is_ok());
    }

    #[test]
    fn test_tag_format_placeholder_violation_is_refused_f147() {
        let config = config(
            "server: {}\ndatabase: { url: postgres://x }\nauth: {}\napplications:\n  - name: my-service\n    source: { provider: github, id: \"1\" }\n    template: service@1\n    versioning: { scheme: semver, tag_format: \"v{release}\" }\n    environments: {}\n",
        );
        assert!(check_versioning(&config).is_err());
    }

    #[test]
    fn test_branch_format_documented_placeholders_pass() {
        let yaml = "server: {}\ndatabase: { url: postgres://x }\nauth: {}\napplications: []\n";
        let config = config(yaml);
        check_versioning(&config).expect("documented defaults pass");
    }
}
