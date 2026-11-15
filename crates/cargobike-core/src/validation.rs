//! Registry and trust validation (F-80, F-96's wiring, F-99a, F-147,
//! §13.1) — the PURE rules, shared by the server's boot and the CLI's
//! offline `validate` (4.8, US-13): a semantically broken registry
//! refuses to start instead of starting half-authorised.
//!
//! The caller supplies the parsed templates (`(name, version) ->
//! PipelineTemplate`): the FILE half (reading `templates.directory`)
//! is I/O and stays with the callers (the server's boot, the CLI's
//! directory walk). The same-name@version rule holds on the map the
//! caller builds.

use std::collections::BTreeMap;

use crate::config::Config;
use crate::template::{PipelineTemplate, WaitKind};

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

/// A template's YAML parse failure (the callers' loaders convert).
impl From<serde_yaml_ng::Error> for ValidationError {
    fn from(error: serde_yaml_ng::Error) -> Self {
        ValidationError::Rule(format!("failed to parse template: {error}"))
    }
}

/// The templates the caller parsed: `(name, version) -> template`; the
/// caller's loader enforces the same-name dedupe.
pub type TemplateMap = BTreeMap<(String, String), PipelineTemplate>;

/// Validates the whole config (the semantic layer) over the caller's
/// parsed templates (the file-reading half is the caller's; a missing
/// directory with no applications is not an error, with applications
/// it is).
pub fn validate(config: &Config, templates: &TemplateMap) -> Result<(), ValidationError> {
    check_oidc_entries(config)?;
    check_api_keys(config)?;
    check_releaser_references(config)?;
    check_apply_policy_invariants(config, templates)?;
    check_versioning(config)?;
    check_environment_placeholders(config)?;
    check_input_references(config, templates)?;
    check_group_references(config)
}

/// F-147's validation half for the per-environment format strings:
/// `commit_message` and `change_request.{title,body}` accept exactly
/// the documented placeholders; unknown ones are boot errors, not
/// values passed through for the provisioner to trip on later.
fn check_environment_placeholders(config: &crate::config::Config) -> Result<(), ValidationError> {
    const ALLOWED: [&str; 5] = [
        "{application}",
        "{environment}",
        "{version}",
        "{release_id}",
        "{release_url}",
    ];
    for app in &config.applications {
        for (name, entry) in &app.environments {
            let context = format!("application `{}` environment `{name}`", app.name);
            if let Some(message) = entry.commit_message.as_deref() {
                check_placeholders(&context, "commit_message", message, &ALLOWED)?;
            }
            if let Some(overrides) = &entry.change_request {
                if let Some(title) = overrides.title.as_deref() {
                    check_placeholders(&context, "change_request.title", title, &ALLOWED)?;
                }
                if let Some(body) = overrides.body.as_deref() {
                    check_placeholders(&context, "change_request.body", body, &ALLOWED)?;
                }
            }
        }
    }
    Ok(())
}

/// claim constraints are mandatory unless `allow_unconstrained`.
fn check_oidc_entries(config: &crate::config::Config) -> Result<(), ValidationError> {
    for entry in &config.auth.oidc {
        if entry.claims.is_empty() && !entry.allow_unconstrained {
            return Err(ValidationError::Rule(format!(
                "the OIDC trust entry `{}` has no claim constraint; set claims or allow_unconstrained: true",
                entry.name
            )));
        }
    }
    Ok(())
}

/// a named API key must be unique and present a hash.
fn check_api_keys(config: &crate::config::Config) -> Result<(), ValidationError> {
    let mut named: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for entry in &config.auth.api_keys {
        if !named.insert(&entry.name) {
            return Err(ValidationError::Rule(format!(
                "the API key `{}` is declared twice (rotation means two DIFFERENT names that are both active)",
                entry.name
            )));
        }
        if !phc_hash_shape(&entry.hash) {
            return Err(ValidationError::Rule(format!(
                "the API key `{}` carries a hash that `cargobike-server hash-api-key` does not produce",
                entry.name
            )));
        }
    }
    Ok(())
}

/// A PHC-string's STRUCTURAL shape (`$argon2id$v=19$m=..,t=..,p=..$salt$hash`)
/// — the offline half of "a hash `hash-api-key` produces". The server's
/// boot additionally parses the string with argon2 itself (the exact
/// producer check); the CLI's offline validate cannot carry argon2
/// (core stays crypto-free), so a well-formed-but-wrong hash passes the
/// CLI and refuses at boot — fail-closed where it counts.
fn phc_hash_shape(hash: &str) -> bool {
    let mut parts = hash.split('$');
    let empty = parts.next();
    let family = parts.next();
    if empty != Some("") || !family.is_some_and(|f| f.starts_with("argon2")) {
        return false;
    }
    let version = parts.next();
    if !version.is_some_and(|v| {
        v.strip_prefix("v=")
            .is_some_and(|v| v.chars().all(|c| c.is_ascii_digit()) && !v.is_empty())
    }) {
        return false;
    }
    let params = parts.next();
    if !params.is_some_and(|p| {
        p.split(',').all(|pair| {
            pair.split_once('=').is_some_and(|(key, value)| {
                matches!(key, "m" | "t" | "p")
                    && !value.is_empty()
                    && value.chars().all(|c| c.is_ascii_digit())
            })
        })
    }) {
        return false;
    }
    // The salt must be present; the DIGEST is optional at this layer
    // (argon2's own parse — the boot-exact check — accepts a
    // digest-less PHC string, and the shape rule must never be
    // stricter than the boot check).
    parts.next().is_some_and(|salt| !salt.is_empty())
}

/// `releasers` selectors reference existing entries and those
/// entries must grant `release:create`.
fn check_releaser_references(config: &crate::config::Config) -> Result<(), ValidationError> {
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
                                "the releaser references `{oidc}`, which does not grant `release:create`"
                            ),
                        });
                    }
                    // (the b): a releaser's `ref` glob must be able to match tag_format.
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

/// A `ref` claim glob can plausibly match the tag format ((the b)): the
/// claim glob's literal prefix must cover the COMPOSED tag ref's literal
/// prefix — a real tag lives at `refs/tags/{tag_format}`, so
/// `refs/tags/v*` covers `v{version}`; `release-*` does not.
fn ref_may_match_tag(claim_glob: &str, tag_format: &str) -> bool {
    // The claim glob's literal prefix is everything before its first
    // wildcard or escape; the tag's real ref is `refs/tags/<format>`
    // ((the b)'s composed form), so the claim prefix must cover it.
    if claim_glob == "~ALL" {
        return true;
    }
    let claim_prefix = claim_glob.split(['*', '?', '[', '\\']).next().unwrap_or("");
    let tag_ref_prefix = format!("refs/tags/{}", tag_format.split('{').next().unwrap_or(""));
    tag_ref_prefix.starts_with(claim_prefix)
}

/// every environment with an `approval` block must have a template
/// `wait: approval` step; : `api_key` approvers refuse unless
/// `allow_machine_approvers: true`; selectors must exist and grant the
/// right request.
fn check_apply_policy_invariants(
    config: &Config,
    templates: &TemplateMap,
) -> Result<(), ValidationError> {
    let repositories = &config.applications;
    for app in repositories {
        for (env_name, env) in &app.environments {
            let Some(policy) = &env.approval else {
                continue;
            };
            let template_name_version = &app.template;
            let template =
                template_for_name_version(templates, template_name_version, app, env_name)?;
            if !template_has_approval_step(template, env_name) {
                return Err(ValidationError::Reference {
                    context: format!("application `{}` environment `{env_name}`", app.name),
                    problem: "the registry declares an `approval` block but the template has no `wait: approval` step for this environment".to_owned(),
                });
            }
            for selector in &policy.approvers {
                if let crate::config::PrincipalSelector::ApiKey { api_key } = selector {
                    if !policy.allow_machine_approvers {
                        return Err(ValidationError::Reference {
                            context: format!("application `{}` environment `{env_name}`", app.name),
                            problem: format!(
                                "an approver references the API key `{api_key}`; machine approvers need `allow_machine_approvers: true`"
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
                                "the approver references `{oidc}`, which does not grant `release:approve`"
                            ),
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

/// Resolves the app's template (the `name@version` reference).
fn template_for_name_version<'a>(
    templates: &'a TemplateMap,
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
            problem: format!("the template `{name}@{version}` is not in the templates directory"),
        })
}

/// Whether the template environment's steps contain a `wait: approval`
/// control step (the invariant).
fn template_has_approval_step(template: &PipelineTemplate, environment: &str) -> bool {
    template
        .environments
        .iter()
        .filter(|spec| spec.name == environment)
        .flat_map(|spec| &spec.steps)
        .any(|step| step.wait == Some(WaitKind::Approval))
}

/// versioning scheme is one of the kebab values and the
/// format strings use their documented placeholders.
fn check_versioning(config: &crate::config::Config) -> Result<(), ValidationError> {
    for app in &config.applications {
        let scheme_ok = ["semver", "calver", "opaque"]
            .iter()
            .any(|allowed| *allowed == app.versioning.scheme);
        if !scheme_ok {
            return Err(ValidationError::Rule(format!(
                "the application `{}` declares unknown versioning scheme `{}` (semver, calver, opaque)",
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

/// The placeholder check: unknown placeholders are errors (the not
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
                "the {context} {field} placeholder `{placeholder}` is not documented (allowed: {allowed:?})"
            )));
        }
        rest = &rest[close + 1..];
    }
    Ok(())
}

fn find_brace(rest: &[u8]) -> Option<usize> {
    rest.iter().position(|c| *c == b'{')
}

/// Every `inputs.<name>` / `env.inputs.<name>` a gate or a `${{ ... }}`
/// parameter references must be a DECLARED input — the template's
/// `inputs` (create-supplied) or `environment_inputs` (registry's
/// per-environment values), the application's generic `inputs`, or the
/// registry environment's own `inputs` (US-13's "gate referencing
/// non-existent input" offline half). The scan is textual over the
/// expression spans; the runtime evaluation stays the truth.
fn check_input_references(config: &Config, templates: &TemplateMap) -> Result<(), ValidationError> {
    for app in &config.applications {
        for (env_name, env) in &app.environments {
            let context = format!("application `{}` environment `{env_name}`", app.name);
            // The declared names this environment's expressions may touch.
            let template = template_for_name_version(templates, &app.template, app, env_name)?;
            let mut declared: Vec<&str> = template.inputs.keys().map(String::as_str).collect();
            declared.extend(template.environment_inputs.keys().map(String::as_str));
            declared.extend(
                app.inputs
                    .as_ref()
                    .map(|map| map.keys())
                    .into_iter()
                    .flatten()
                    .map(String::as_str),
            );
            declared.extend(
                env.inputs
                    .as_ref()
                    .map(|map| map.keys())
                    .into_iter()
                    .flatten()
                    .map(String::as_str),
            );
            // The template's environment spec (the steps that run for
            // this registry environment); an unlisted environment has
            // only the registry's facts.
            if let Some(spec) = template
                .environments
                .iter()
                .find(|spec| spec.name == *env_name)
            {
                for step in &spec.steps {
                    if let Some(when) = &step.when {
                        check_input_tokens(&context, "a gate", when, &declared)?;
                    }
                    check_input_tokens_json(&context, &step.with, &declared)?;
                }
            }
        }
    }
    Ok(())
}

/// Walks a `with:` JSON value's strings, checking each `${{ ... }}`
/// span's `inputs.<name>` references.
fn check_input_tokens_json(
    context: &str,
    value: &serde_json::Value,
    declared: &[&str],
) -> Result<(), ValidationError> {
    match value {
        serde_json::Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.starts_with("${{") && trimmed.ends_with("}}") {
                check_input_tokens(context, "a parameter", trimmed, declared)?;
            }
            Ok(())
        }
        serde_json::Value::Array(items) => {
            for item in items {
                check_input_tokens_json(context, item, declared)?;
            }
            Ok(())
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                let _ = key;
                check_input_tokens_json(context, item, declared)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The textual `inputs.<ident>` scan over one expression (gates are
/// wholly CEL; parameters are the `${{ ... }}` span).
fn check_input_tokens(
    context: &str,
    kind: &str,
    expression: &str,
    declared: &[&str],
) -> Result<(), ValidationError> {
    let mut rest = expression;
    while let Some(at) = rest.find("inputs.") {
        let after = &rest[at + "inputs.".len()..];
        let ident: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !ident.is_empty() && !declared.contains(&ident.as_str()) {
            return Err(ValidationError::Reference {
                context: context.to_owned(),
                problem: format!(
                    "{kind} references the input `{ident}`, which no `inputs`/`environment_inputs` block declares"
                ),
            });
        }
        rest = after;
    }
    Ok(())
}

/// `extends` names an existing group.
fn check_group_references(config: &crate::config::Config) -> Result<(), ValidationError> {
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

    fn config(yaml: &str) -> Config {
        from_str(yaml).expect("config")
    }

    /// The documented canonical pair agrees; the F-99a(b) documented
    /// misconfiguration cannot fit.
    #[test]
    fn test_ref_may_match_tag_composes_the_refs_tags_prefix() {
        assert!(ref_may_match_tag("refs/tags/v*", "v{version}"));
        assert!(ref_may_match_tag("refs/tags/**", "v{version}"));
        assert!(!ref_may_match_tag("refs/tags/release-*", "v{version}"));
        assert!(!ref_may_match_tag("refs/tags/v*", "release-{version}"));
        assert!(ref_may_match_tag("~ALL", "v{version}"));
    }

    #[test]
    fn test_unconstrained_oidc_entries_are_refused() {
        let config = config(
            "server: {}\ndatabase: { url: postgres://x }\nauth:\n  oidc:\n    - name: open\n      issuer: https://issuer\n      audience: cargobike\n      grants: [release:create]\n",
        );
        assert!(check_oidc_entries(&config).is_err());
    }

    #[test]
    fn test_releaser_without_release_create_grant_is_refused() {
        let config = config(
            "server: {}\ndatabase: { url: postgres://x }\nauth:\n  oidc:\n    - name: gha\n      issuer: https://issuer\n      audience: cargobike\n      claims:\n        repository_owner_id: \"1\"\n      grants: [release:read]\napplications:\n  - name: my-service\n    source: { provider: github, id: \"1\" }\n    template: service@1\n    releasers:\n      - oidc: gha\n    environments: {}\n",
        );
        assert!(check_releaser_references(&config).is_err());
    }

    #[test]
    fn test_tag_format_placeholder_violation_is_refused() {
        let config = config(
            "server: {}\ndatabase: { url: postgres://x }\nauth: {}\napplications:\n  - name: my-service\n    source: { provider: github, id: \"1\" }\n    template: service@1\n    versioning: { scheme: semver, tag_format: \"v{release}\" }\n    environments: {}\n",
        );
        assert!(check_versioning(&config).is_err());
    }

    #[test]
    fn test_branch_format_documented_placeholders_pass() {
        let config =
            config("server: {}\ndatabase: { url: postgres://x }\nauth: {}\napplications: []\n");
        check_versioning(&config).expect("documented defaults pass");
    }

    /// The PHC shape: the producer's real format passes; garbage and
    /// truncated forms refuse.
    #[test]
    fn test_phc_hash_shape_accepts_the_producer_format_only() {
        assert!(phc_hash_shape(
            "$argon2id$v=19$m=32768,t=3,p=4$YWJjZGVmZ2hpamtsbW5vcA$RdescudvJCsgt3ub+b+dWRWJTmaaJObG"
        ));
        assert!(!phc_hash_shape("not-a-hash"));
        assert!(!phc_hash_shape("$argon2id$v=19$m=32768,t=3,p=4$"));
        assert!(!phc_hash_shape(
            "$argon2id$x=19$m=32768,t=3,p=4$salt$digest"
        ));
        assert!(!phc_hash_shape(
            "$argon2id$v=19$m=32768,t=3,q=4$salt$digest"
        ));
        // argon2 itself accepts a digest-less string — so does the
        // shape rule (never stricter than the boot check).
        assert!(phc_hash_shape("$argon2id$v=19$m=32768,t=3,p=4$ZGVw"));
    }
}
