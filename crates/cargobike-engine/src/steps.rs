//! The step registry: what a template's `uses:`
//! resolves to, sidecar registration, and the step-type versions the
//! snapshot records .
//!
//! Built-ins register at startup; sidecars register when the extension
//! host brings their transports up. A template's `uses: name@version`
//! chases one entry: unregistered names are compile errors .

use std::collections::BTreeMap;
use std::sync::Arc;

use cargobike_core::step::StepType;

/// Installed step types, keyed `name -> version -> instance` (/:
/// versions side by side).
#[derive(Clone, Default)]
pub struct StepRegistry {
    entries: BTreeMap<String, BTreeMap<String, Arc<dyn StepType>>>,
}

impl StepRegistry {
    /// Empty registry (the built-ins register at startup).
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one versioned step instance; a later insert at the same
    /// name@version replaces the earlier one (the config reload semantics).
    pub fn register(&mut self, step: Arc<dyn StepType>) {
        self.entries
            .entry(step.name().to_owned())
            .or_default()
            .insert(step.version().to_owned(), step);
    }

    /// Resolves a template's `uses: builtin/commit-files@1` .
    pub fn resolve(&self, uses: &str) -> Result<Arc<dyn StepType>, StepRegistryError> {
        let (name, version) = uses
            .split_once('@')
            .ok_or_else(|| StepRegistryError::Malformed(uses.to_owned()))?;
        let versions = self
            .entries
            .get(name)
            .ok_or_else(|| StepRegistryError::UnknownStep(name.to_owned()))?;
        versions
            .get(version)
            .cloned()
            .ok_or_else(|| StepRegistryError::UnknownVersion {
                name: name.to_owned(),
                version: version.to_owned(),
                installed: versions.keys().cloned().collect(),
            })
    }

    /// Every installed `name -> version` pair; the snapshot's
    /// step_type_versions come from the resolved uses of one release .
    pub fn installed(&self) -> Vec<(String, String)> {
        self.entries
            .iter()
            .flat_map(|(name, versions)| {
                versions
                    .keys()
                    .map(move |version| (name.clone(), version.clone()))
            })
            .collect()
    }

    /// Whether any step of the base name is installed (the control-step
    /// reservation: `wait:` names are interpreter-native, never registered).
    pub fn has_step_name(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }
}

/// Registry-resolution failures (the diagnostics).
#[derive(Debug, thiserror::Error)]
pub enum StepRegistryError {
    /// No `@version` suffix.
    #[error("the step reference `{0}` must be `name@version`")]
    Malformed(String),
    /// The base name is not installed.
    #[error("the step `{0}` is not installed")]
    UnknownStep(String),
    /// The version is not installed; alternatives listed.
    #[error("the step `{name}` has no version {version}; installed: {installed:?}")]
    UnknownVersion {
        /// The requested step name.
        name: String,
        /// The requested version.
        version: String,
        /// Versions actually installed.
        installed: Vec<String>,
    },
}

/// Secrets privacy for debug : names appear, instances never.
impl std::fmt::Debug for StepRegistry {
    /// Hand-written Debug — a step registry's contents are code, printing
    /// them is noise; keep the surface summarised.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_map().entries(self.installed()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A do-nothing step type so registry resolution tests have a target.
    struct Dummy;

    #[async_trait::async_trait]
    impl StepType for Dummy {
        fn name(&self) -> &str {
            "builtin/dummy"
        }
        fn version(&self) -> &str {
            "1"
        }
        async fn execute(
            &self,
            _ctx: &cargobike_core::step::StepContext,
            _release: &cargobike_core::model::Release,
            _env: &cargobike_core::step::EnvRef,
            _params: &serde_json::Value,
        ) -> Result<cargobike_core::step::StepOutput, cargobike_core::step::StepError> {
            Ok(cargobike_core::step::StepOutput::Continue(
                serde_json::json!({ "ran": true }),
            ))
        }
    }

    /// Harness with one registered dummy step.
    fn registry() -> StepRegistry {
        let mut registry = StepRegistry::new();
        registry.register(Arc::new(Dummy));
        registry
    }

    #[test]
    fn test_resolve_honours_name_and_version_f35() {
        let registry = registry();
        assert!(registry.resolve("builtin/dummy@1").is_ok());
        let error: StepRegistryError = match registry.resolve("builtin/dummy@2") {
            Err(resolved_error) => resolved_error,
            Ok(_) => unreachable!("version 2 was never installed"),
        };
        assert!(
            matches!(error, StepRegistryError::UnknownVersion { installed, .. } if installed == vec!["1".to_owned()])
        );
        assert!(matches!(
            registry.resolve("builtin/other@1"),
            Err(StepRegistryError::UnknownStep(_))
        ));
        assert!(matches!(
            registry.resolve("builtin/dummy"),
            Err(StepRegistryError::Malformed(_))
        ));
    }

    #[test]
    fn test_installed_lists_name_version_pairs_for_the_snapshot() {
        let registry = registry();
        assert_eq!(
            registry.installed(),
            vec![("builtin/dummy".to_owned(), "1".to_owned())]
        );
    }

    /// A dummy commit-files instance for the positive compile check.
    struct CommitFilesDummy;

    #[async_trait::async_trait]
    impl StepType for CommitFilesDummy {
        fn name(&self) -> &str {
            "builtin/commit-files"
        }
        fn version(&self) -> &str {
            "1"
        }
        async fn execute(
            &self,
            _ctx: &cargobike_core::step::StepContext,
            _release: &cargobike_core::model::Release,
            _env: &cargobike_core::step::EnvRef,
            _params: &serde_json::Value,
        ) -> Result<cargobike_core::step::StepOutput, cargobike_core::step::StepError> {
            Ok(cargobike_core::step::StepOutput::Continue(
                serde_json::json!({ "sha": "x" }),
            ))
        }
    }

    #[test]
    fn test_unregistered_uses_rejects_at_compile_time_f35() {
        let template = r#"
name: x
version: "1"
inputs: {}
environment_inputs:
  repo: { type: repo }
step_groups:
  - name: deploy
    steps:
      - id: edit
        uses: builtin/commit-files@1
        with: {}
environments:
  - name: a
    steps: [include: deploy]
"#;
        let schemes = cargobike_core::version::VersionScheme::Opaque;
        assert!(crate::template::compile_with(template, &schemes, &registry()).is_err());

        // Registered instead: the same template compiles (the positive).
        let mut installed = registry();
        installed.register(std::sync::Arc::new(CommitFilesDummy));
        crate::template::compile_with(template, &schemes, &installed)
            .expect("registered step compiles");
    }
}
