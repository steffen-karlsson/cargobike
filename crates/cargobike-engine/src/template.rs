//! The registry-aware compile half: the structural compiler lives in
//! core (`cargobike_core::template::compile` — the CLI's offline
//! `validate` shares it per 4.8/US-13); the re-exports keep every
//! historical `cargobike_engine::template` path working.

pub use cargobike_core::template::compile::{
    CompiledTemplate, ResolvedEnvironment, ResolvedStep, StepBody, TemplateError, compile,
    parse_and_validate, resolve,
};

use cargobike_core::template::{PipelineTemplate, StepSpec};

/// Compilation that also validates every `uses:` against the installed
/// step types (the unregistered steps refuse at compile time).
pub fn compile_with(
    source: &str,
    version_scheme: &cargobike_core::version::VersionScheme,
    registry: &crate::steps::StepRegistry,
) -> Result<CompiledTemplate, TemplateError> {
    let template = parse_and_validate(source, version_scheme)?;
    check_uses(&template, registry)?;
    resolve(template)
}

/// Every action step's `uses:` must resolve in the registry; the
/// environment's declared steps include the ones `include:` pulls in.
fn check_uses(
    template: &PipelineTemplate,
    registry: &crate::steps::StepRegistry,
) -> Result<(), TemplateError> {
    fn uses_of(step: &StepSpec) -> Option<String> {
        step.uses.clone()
    }
    let resolve_uses = |uses: &str| {
        registry
            .resolve(uses)
            .ok()
            .map(|_step| ())
            .ok_or_else(|| {
                TemplateError::Invalid(format!(
                    "the step `{uses}` is not installed (templates compile against installed step types)"
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

#[cfg(test)]
mod tests {
    use super::*;
    use cargobike_core::version::VersionScheme;

    fn registry() -> crate::steps::StepRegistry {
        let mut registry = crate::steps::StepRegistry::new();
        crate::builtin::register_builtins(&mut registry);
        registry
    }

    /// `compile_with` refuses a template whose `uses:` is not
    /// installed, while the structural `compile` (the core half)
    /// accepts the same source — the registry is the only difference.
    #[test]
    fn test_compile_with_refuses_uninstalled_steps() {
        let source = "name: t\nversion: \"1\"\nenvironments:\n  - name: prod\n    steps:\n      - uses: sidecar/frobnicate@1\n";
        let scheme = VersionScheme::Opaque;
        assert!(compile(source, &scheme).is_ok());
        assert!(compile_with(source, &scheme, &registry()).is_err());
        let installed = "name: t\nversion: \"1\"\nenvironments:\n  - name: prod\n    steps:\n      - uses: builtin/commit-files@1\n";
        assert!(compile_with(installed, &scheme, &registry()).is_ok());
    }
}
