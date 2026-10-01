//! Template compilation (PRD 3.1, F-25..F-32b): parse a template's YAML,
//! validate its structure (F-28's step exclusivity, F-34's wait rules and
//! auto IDs, C17's uniqueness after `include` expansion, F-27a's gate
//! checks against the version scheme), and produce the resolved form the
//! interpreter executes and the release snapshot stores (F-16/F-29).

use std::time::Duration as StdDuration;

use cargobike_core::template::{
    EnvironmentSpec, OnModified, OnTimeout, PipelineTemplate, RetryPolicy, StepSpec, WaitKind,
};
use cargobike_core::version::VersionScheme;

/// Durations are humantime strings in the source; the resolved form
/// carries parsed values.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum StepBody {
    /// An action step (`uses: builtin/commit-files@1`).
    Action {
        /// Registered step name@version (F-35).
        uses: String,
        /// Parameters (F-28); `{ secret: <name> }` references allowed (F-146).
        with: serde_json::Value,
        /// Bound of one attempt's body run (F-35).
        timeout: Option<StdDuration>,
        /// Retry policy (F-35).
        retry: Option<RetryPolicy>,
    },
    /// `wait: merge` — recv + provider verification loop (F-62).
    WaitMerge {
        /// Required: nothing waits forever by accident (F-34).
        timeout: StdDuration,
        /// Default `fail` (F-34's documented default).
        on_timeout: OnTimeout,
        /// Default `fail` (F-62, C23).
        on_modified: OnModified,
    },
    /// `wait: approval` — recv (F-59/F-96).
    WaitApproval {
        /// Required (F-34).
        timeout: StdDuration,
        /// Only `fail` in v1.0 (approval timeout surfaces `ApprovalTimeout`);
        /// cancel semantics arrive with the `wait: approval` work in 4.9.
        on_timeout: OnTimeout,
    },
    /// `wait: sleep` — durable sleep with its own `duration` (F-34).
    WaitSleep {
        /// The sleep length (not a deadline).
        duration: StdDuration,
    },
}

/// One fully-resolved step: the ID that signal topics and approvals
/// address (F-20a, F-59) plus its body.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ResolvedStep {
    /// Final ID: user-declared or auto-generated (`merge-0`), unique
    /// within the environment after `include` expansion (F-34, C17).
    pub id: String,
    /// The step's declared CEL gate, when any (F-28).
    pub when: Option<String>,
    /// Body.
    pub body: StepBody,
}

impl ResolvedStep {
    /// The action parameters as the template gives them (`with`).
    pub fn params(&self) -> serde_json::Value {
        match &self.body {
            StepBody::Action { with, .. } => with.clone(),
            _ => serde_json::Value::Null,
        }
    }

    /// The retry policy an action step carries (F-35).
    pub fn body_retry_policy(&self) -> Option<cargobike_core::template::RetryPolicy> {
        match &self.body {
            StepBody::Action { retry, .. } => retry.clone(),
            _ => None,
        }
    }

    /// One attempt's body bound (F-35's per-step `timeout`).
    pub fn action_kind_timeout(&self) -> Option<StdDuration> {
        match &self.body {
            StepBody::Action { timeout, .. } => *timeout,
            _ => None,
        }
    }
}

/// One environment's resolved step sequence.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ResolvedEnvironment {
    /// Environment name, referenced by gates and registry environments.
    pub name: String,
    /// Environment-level CEL gate (F-26).
    pub when: Option<String>,
    /// Template-default concurrency; the registry overrides (R11).
    pub concurrency: Option<cargobike_core::template::ConcurrencyPolicy>,
    /// Expanded, resolved steps.
    pub steps: Vec<ResolvedStep>,
    /// Per-environment inputs the release's provision stamps (F-32a:
    /// `repo` and `edits` are the well-known entries; the registry may
    /// also add generic ones — the interpreter reads them via env.inputs).
    #[serde(default)]
    pub env_inputs: std::collections::BTreeMap<String, serde_json::Value>,
}

/// The canonical, validated template (the F-16 snapshot's template half).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CompiledTemplate {
    /// Template name.
    pub name: String,
    /// Template version (snapshotted; F-25).
    pub version: String,
    /// Application-wide inputs the registry supplies (F-32a).
    pub inputs: std::collections::BTreeMap<String, cargobike_core::template::InputSpec>,
    /// Per-environment inputs the registry supplies (F-32a).
    pub environment_inputs: std::collections::BTreeMap<String, cargobike_core::template::InputSpec>,
    /// Environments in declaration order (gates reference names; F-26).
    pub environments: Vec<ResolvedEnvironment>,
}

/// Everything a template can fail to compile for (validated diagnostics
/// on the CLI side, 4.8; here they surface as strings).
#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
    /// YAML parse failure.
    #[error("failed to parse template: {0}")]
    Parse(String),
    /// A structural rule the template break.
    #[error("failed to compile template: {0}")]
    Invalid(String),
}

impl From<serde_yaml_ng::Error> for TemplateError {
    fn from(error: serde_yaml_ng::Error) -> Self {
        TemplateError::Parse(error.to_string())
    }
}

/// Compiles a template from its YAML source against the application's
/// version scheme (F-27a's gate checks happen here, before any release).
/// Structural only: the `uses:`-against-registry check needs the server's
/// installed steps and lives in [`compile_with`].
pub fn compile(
    source: &str,
    version_scheme: &VersionScheme,
) -> Result<CompiledTemplate, TemplateError> {
    let template: PipelineTemplate = serde_yaml_ng::from_str(source)?;
    validate(&template, version_scheme)?;
    resolve(template)
}

/// Compilation that also validates every `uses:` against the installed
/// step types (F-35: unregistered steps refuse at compile time).
pub fn compile_with(
    source: &str,
    version_scheme: &VersionScheme,
    registry: &crate::steps::StepRegistry,
) -> Result<CompiledTemplate, TemplateError> {
    let template: PipelineTemplate = serde_yaml_ng::from_str(source)?;
    validate(&template, version_scheme)?;
    check_uses(&template, registry)?;
    resolve(template)
}

/// Every action step's `uses:` must resolve in the registry (F-35); the
/// environment's declared steps include the ones `include:` pulls in.
fn check_uses(
    template: &PipelineTemplate,
    registry: &crate::steps::StepRegistry,
) -> Result<(), TemplateError> {
    fn uses_of(step: &cargobike_core::template::StepSpec) -> Option<String> {
        step.uses.clone()
    }
    let resolve_uses = |uses: &str| {
        registry
            .resolve(uses)
            .ok()
            .map(|_step| ())
            .ok_or_else(|| {
                TemplateError::Invalid(format!(
                    "the step `{uses}` is not installed (F-35: templates compile against installed step types)"
                ))
            })
    };
    for group in &template.step_groups {
        for step in &group.steps {
            if let Some(uses) = uses_of(step) {
                resolve_uses(&uses)?;
            }
        }
    }
    for environment in &template.environments {
        for step in &environment.steps {
            if let Some(uses) = uses_of(step) {
                resolve_uses(&uses)?;
            }
        }
    }
    Ok(())
}

/// Structural validation before resolution (F-25..F-34).
fn validate(template: &PipelineTemplate, scheme: &VersionScheme) -> Result<(), TemplateError> {
    if template.name.is_empty() {
        return Err(TemplateError::Invalid(
            "the template `name` must not be empty".to_owned(),
        ));
    }
    if template.version.is_empty() {
        return Err(TemplateError::Invalid(
            "the template `version` must not be empty".to_owned(),
        ));
    }
    if template.environments.is_empty() {
        return Err(TemplateError::Invalid(
            "the template must declare at least one environment (F-26)".to_owned(),
        ));
    }
    let mut names = std::collections::HashSet::new();
    for environment in &template.environments {
        if !names.insert(environment.name.clone()) {
            return Err(TemplateError::Invalid(format!(
                "environment `{}` is declared twice (F-26: gates reference names)",
                environment.name
            )));
        }
    }
    // F-32b: `include:` at the top level references a named group; groups
    // resolve inline (single level) — cycles have no place to hide.
    let groups: std::collections::HashMap<&str, &cargobike_core::template::StepGroup> = template
        .step_groups
        .iter()
        .map(|group| (group.name.as_str(), group))
        .collect();
    for group in &template.step_groups {
        for step in &group.steps {
            if step.include.is_some() {
                return Err(TemplateError::Invalid(format!(
                    "step group `{}` contains an `include:`; groups resolve one level deep (F-32b)",
                    group.name
                )));
            }
        }
    }
    for environment in &template.environments {
        for step in &environment.steps {
            check_step_shorthand(step)?;
            if let Some(include) = &step.include {
                groups
                    .get(include.as_str())
                    .ok_or_else(|| {
                        TemplateError::Invalid(format!(
                            "environment `{environment}` includes unknown step group `{include}` (F-32b)",
                            environment = environment.name
                        ))
                    })?;
            }
        }
    }
    // F-27a: gate functions that belong to specific version schemes.
    for environment in &template.environments {
        validate_gate(
            scheme,
            environment.name.as_str(),
            "environment",
            environment.when.as_deref(),
        )?;
        for step in &environment.steps {
            validate_gate(
                scheme,
                environment.name.as_str(),
                "step",
                step.when.as_deref(),
            )?;
        }
    }
    Ok(())
}

/// Step exclusivity (F-28): exactly one of `uses` / `wait` / `include`.
fn check_step_shorthand(step: &StepSpec) -> Result<(), TemplateError> {
    let chosen: [_; 3] = [
        step.uses.is_some(),
        step.wait.is_some(),
        step.include.is_some(),
    ];
    let count = chosen.iter().filter(|present| **present).count();
    if count != 1 {
        return Err(TemplateError::Invalid(format!(
            "a step selects exactly one of `uses`, `wait`, or `include`; {} selected",
            if count == 0 {
                "none".to_owned()
            } else {
                "two or more".to_owned()
            }
        )));
    }
    Ok(())
}

/// F-27a's gate-check: `semver(...)` gates are the semver scheme's alone.
fn validate_gate(
    scheme: &VersionScheme,
    environment: &str,
    kind: &str,
    when: Option<&str>,
) -> Result<(), TemplateError> {
    let Some(when) = when else { return Ok(()) };
    if when.contains("semver(") && !matches!(scheme, VersionScheme::Semver) {
        return Err(TemplateError::Invalid(format!(
            "environment `{environment}`: the {kind} gate uses `semver(release.version)` but the application's version scheme is not `semver` (F-27a)"
        )));
    }
    // runtime-validated placeholders only (F-147/D? for CEL: the expression
    // context is fixed; unknown identifiers fail at evaluation (3.2)).
    Ok(())
}

/// Resolves includes, generates ids, and parses durations (F-34, C17).
fn resolve(template: PipelineTemplate) -> Result<CompiledTemplate, TemplateError> {
    let groups: std::collections::HashMap<&str, &cargobike_core::template::StepGroup> = template
        .step_groups
        .iter()
        .map(|group| (group.name.as_str(), group))
        .collect();
    let mut environments = Vec::with_capacity(template.environments.len());
    for environment in &template.environments {
        let mut steps = Vec::new();
        resolve_sequence(environment, &groups, &mut steps)?;
        ensure_unique(&steps)?;
        environments.push(ResolvedEnvironment {
            name: environment.name.clone(),
            when: environment.when.clone(),
            concurrency: environment.concurrency,
            steps,
            env_inputs: std::collections::BTreeMap::new(),
        });
    }
    Ok(CompiledTemplate {
        name: template.name,
        version: template.version,
        inputs: template.inputs,
        environment_inputs: template.environment_inputs,
        environments,
    })
}

/// Expands one environment's steps: includes in place, and every resolved
/// step carries its final ID (declared or deterministic, C17/F-34).
fn resolve_sequence(
    environment: &EnvironmentSpec,
    groups: &std::collections::HashMap<&str, &cargobike_core::template::StepGroup>,
    into: &mut Vec<ResolvedStep>,
) -> Result<(), TemplateError> {
    for step in &environment.steps {
        if let Some(include) = &step.include {
            let group = *groups.get(include.as_str()).ok_or_else(|| {
                TemplateError::Invalid(format!(
                    "environment `{environment}` includes unknown step group `{include}`",
                    environment = environment.name
                ))
            })?;
            for group_step in &group.steps {
                resolve_step(group_step, into)?;
            }
        } else {
            resolve_step(step, into)?;
        }
    }
    Ok(())
}

fn resolve_step(step: &StepSpec, into: &mut Vec<ResolvedStep>) -> Result<(), TemplateError> {
    let id = step.id.clone().unwrap_or_else(|| auto_id(step, into.len()));
    let when = step.when.clone();
    let body = compare_body(step)?;
    into.push(ResolvedStep { id, when, body });
    Ok(())
}

/// Auto-id: `<wait-or-uses-name>-<index>` for a deterministic replay (F-34).
fn auto_id(step: &StepSpec, index: usize) -> String {
    let stem = match (&step.uses, step.wait) {
        (Some(uses), _) => uses
            .split_once('/')
            .map(|(_prefix, rest)| rest.to_owned())
            .unwrap_or(uses.clone()),
        (None, Some(WaitKind::Merge)) => "merge".to_owned(),
        (None, Some(WaitKind::Approval)) => "approval".to_owned(),
        (None, Some(WaitKind::Sleep)) => "sleep".to_owned(),
        _ => "step".to_owned(),
    };
    format!("{stem}-{index}")
}

/// Parses the source body (F-34's wait rules; F-35's action fields).
fn compare_body(step: &StepSpec) -> Result<StepBody, TemplateError> {
    if let Some(wait) = step.wait {
        return match wait {
            WaitKind::Merge => {
                let timeout = wait_timeout(step)?;
                Ok(StepBody::WaitMerge {
                    timeout,
                    on_timeout: step.on_timeout.unwrap_or(OnTimeout::Fail),
                    on_modified: step.on_modified.unwrap_or(OnModified::Fail),
                })
            }
            WaitKind::Approval => {
                let timeout = wait_timeout(step)?;
                Ok(StepBody::WaitApproval {
                    timeout,
                    on_timeout: step.on_timeout.unwrap_or(OnTimeout::Fail),
                })
            }
            WaitKind::Sleep => match (&step.duration, step.timeout.as_deref()) {
                (Some(duration), _) => {
                    let seconds = parse_duration(duration)?;
                    if step.timeout.is_some() {
                        return Err(TemplateError::Invalid(
                            "`wait: sleep` takes `duration`, not `timeout` (F-34)".to_owned(),
                        ));
                    }
                    Ok(StepBody::WaitSleep { duration: seconds })
                }
                (None, _) => Err(TemplateError::Invalid(
                    "`wait: sleep` requires `duration` (F-34)".to_owned(),
                )),
            },
        };
    }
    if let Some(uses) = &step.uses {
        return Ok(StepBody::Action {
            uses: uses.clone(),
            with: step.with.clone(),
            timeout: step.timeout.as_deref().map(parse_duration).transpose()?,
            retry: step.retry.clone(),
        });
    }
    Err(TemplateError::Invalid(
        "an `include:`-only step resolves inside its group; there is nothing to body".to_owned(),
    ))
}

/// Waits except sleep require an explicit timeout (F-34, C23).
fn wait_timeout(step: &StepSpec) -> Result<StdDuration, TemplateError> {
    match &step.timeout {
        Some(timeout) => parse_duration(timeout),
        None => Err(TemplateError::Invalid(
            "`wait: merge` / `wait: approval` require an explicit `timeout` (F-34; nothing waits forever by accident)".to_owned(),
        )),
    }
}

fn parse_duration(text: &str) -> Result<StdDuration, TemplateError> {
    humantime::parse_duration(text).map_err(|error| {
        TemplateError::Invalid(format!("failed to parse duration `{text}`: {error}"))
    })
}

/// C17: uniqueness after `include` expansion — declared and auto ids both.
fn ensure_unique(steps: &[ResolvedStep]) -> Result<(), TemplateError> {
    let mut seen = std::collections::HashSet::new();
    for step in steps {
        if !seen.insert(step.id.clone()) {
            return Err(TemplateError::Invalid(format!(
                "duplicate step id `{}` after include expansion (C17: approvals and signal topics address ids)",
                step.id
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    const SERVICE_TEMPLATE: &str = r#"
name: service
version: "1"
inputs: {}
environment_inputs:
  repo: { type: repo }
  edits: { type: edits }
step_groups:
  - name: deploy
    steps:
      - id: edit
        uses: builtin/commit-files@1
        with: {}
        retry: { attempts: 3, backoff: exponential, max_delay: 1m }
      - id: cr
        uses: builtin/change-request@1
        with: {}
      - wait: merge
        timeout: 7d
        on_modified: fail
        on_timeout: fail
environments:
  - name: preview
    steps: [include: deploy]
  - name: production
    when: 'semver(release.version).prerelease() == ""'
    steps:
      - include: deploy
      - wait: approval
        timeout: 72h
        on_timeout: cancel
"#;

    #[test]
    fn test_compiles_with_auto_resolved_steps() {
        let compiled = compile(SERVICE_TEMPLATE, &VersionScheme::Semver).expect("compiles");
        assert_eq!(compiled.name, "service");
        assert_eq!(compiled.environments.len(), 2);
        // ids unique after include expansion (C17)
        let steps: Vec<String> = compiled.environments[1]
            .steps
            .iter()
            .map(|step| step.id.clone())
            .collect();
        let unique: std::collections::HashSet<_> = steps.iter().collect();
        assert_eq!(
            unique.len(),
            steps.len(),
            "ids unique after include expansion"
        );
    }

    #[test]
    fn test_wait_sleep_takes_duration_not_timeout() {
        let source = r#"
name: x
version: "1"
environments:
  - name: a
    steps:
      - wait: sleep
        duration: 5m
"#;
        let compiled = compile(source, &VersionScheme::Opaque).expect("compiles");
        assert_eq!(
            compiled.environments[0].steps[0].body,
            StepBody::WaitSleep {
                duration: StdDuration::from_secs(300),
            }
        );
    }

    #[rstest]
    #[case::wait_merge_without_timeout(
        r#"
name: x
version: "1"
environments:
  - name: a
    steps:
      - wait: merge
        on_modified: fail
"#
    )]
    #[case::wait_approval_without_timeout(
        r#"
name: x
version: "1"
environments:
  - name: a
    steps:
      - wait: approval
        on_timeout: cancel
"#
    )]
    #[case::wait_sleep_without_duration(
        r#"
name: x
version: "1"
environments:
  - name: a
    steps:
      - wait: sleep
"#
    )]
    #[case::empty_step(
        r#"
name: x
version: "1"
environments:
  - name: a
    steps:
      - with: {}
"#
    )]
    fn test_waits_require_their_own_bounds(#[case] source: &str) {
        let error = compile(source, &VersionScheme::Opaque).expect_err("must refuse");
        assert!(matches!(error, TemplateError::Invalid(_)));
    }

    #[test]
    fn test_semver_gates_refuse_non_semver_schemes_f27a() {
        let source = r#"
name: x
version: "1"
environments:
  - name: a
    when: 'semver(release.version).prerelease() == ""'
    steps:
      - wait: merge
        timeout: 1h
"#;
        for scheme in [
            VersionScheme::Opaque,
            VersionScheme::Calver {
                calver_format: None,
            },
        ] {
            let error = compile(source, &scheme).expect_err("must refuse (F-27a)");
            assert!(matches!(error, TemplateError::Invalid(_)));
        }
        compile(source, &VersionScheme::Semver).expect("semver gate is fine for semver");
    }

    #[test]
    fn test_include_of_unknown_group_and_nested_include_refused_f32b() {
        let source = r#"
name: x
version: "1"
environments:
  - name: a
    steps: [include: nope]
"#;
        let error = compile(source, &VersionScheme::Opaque).expect_err("unknown group");
        assert!(matches!(error, TemplateError::Invalid(_)));

        let source = r#"
name: x
version: "1"
step_groups:
  - name: outer
    steps: [include: inner]
environments:
  - name: a
    steps: [include: outer]
"#;
        let error = compile(source, &VersionScheme::Opaque).expect_err("nested include");
        assert!(matches!(error, TemplateError::Invalid(_)));
    }
}
