//! Registry validation's I/O halves: the template-directory loader and
//! the boot-time wrapper over the pure rules in
//! `cargobike-core::validation` (F-80, F-96's wiring, F-99a, F-147).
//!
//! The server's boot reads `templates.directory` and parses each
//! template (the file name is free; the parsed `name`/`version` must
//! match the registry's `name@version` reference), then runs the
//! shared rules. Boot additionally parses every API-key hash with
//! argon2 itself — the exact "a hash `hash-api-key` produces" check
//! the offline (argon2-free) shape rule approximates.

use std::collections::BTreeMap;
use std::sync::Arc;

use cargobike_core::config::Config;
use cargobike_core::template::PipelineTemplate;
use cargobike_core::validation::TemplateMap;

pub use cargobike_core::validation::{ValidationError, check_placeholders};

/// Validates the whole config (the semantic layer): loads the template
/// directory, runs the shared pure rules, and parses the API-key
/// hashes with argon2 (the boot-exact producer check).
pub fn validate(config: &Config) -> Result<(), ValidationError> {
    let templates = load_templates(&Arc::new(config.clone()))?;
    cargobike_core::validation::validate(config, &templates)?;
    check_api_key_hashes(config)
}

/// Every API-key hash parses as a real argon2 PHC string — the exact
/// shape `cargobike-server hash-api-key` produces (the shared shape
/// rule already refuses the obvious garbage offline).
fn check_api_key_hashes(config: &Config) -> Result<(), ValidationError> {
    for entry in &config.auth.api_keys {
        if argon2::password_hash::PasswordHash::new(&entry.hash).is_err() {
            return Err(ValidationError::Rule(format!(
                "the API key `{}` carries a hash that `cargobike-server hash-api-key` does not produce",
                entry.name
            )));
        }
    }
    Ok(())
}

/// Every parsed template in the directory (deduped by name@version).
pub fn load_templates(config: &Arc<Config>) -> Result<TemplateMap, ValidationError> {
    let directory = &config.templates.directory;
    let mut map = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        if !config.raw_applications.is_empty() || !config.application_groups.is_empty() {
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

#[cfg(test)]
mod tests {
    #[test]
    fn test_api_key_approvers_refuse_machines_by_default() {
        use serde_yaml_ng::from_str;

        // The template invariant first requires a wait-approval
        // step, so the refusal needs a REAL template at a temp
        // directory; validate walks the whole surface.
        let directory = std::env::temp_dir().join("cb-validation-f99b");
        let _ = std::fs::create_dir_all(&directory);
        let template_path = directory.join("service.yaml");
        std::fs::write(
            &template_path,
            "name: service\nversion: \"1\"\nenvironments:\n  - name: production\n    steps:\n      - id: wait-0\n        wait: approval\n        timeout: 1m\n",
        )
        .expect("the template");
        let yaml = "server: {}\ndatabase: { url: postgres://x }\ntemplates:\n  directory: TEMP\nauth:\n  api_keys:\n    - name: key\n      hash: \"$argon2id$v=19$m=32768,t=3,p=4$ZGVw\"\napplications:\n  - name: my-service\n    source: { provider: github, id: \"1\" }\n    template: service@1\n    releasers:\n      - api_key: key\n    environments:\n      production:\n        approval:\n          required: 1\n          approvers:\n            - api_key: key\n".replace("TEMP", directory.to_str().expect("temp path"));
        let mut config: crate::config::Config = from_str(&yaml).expect("config");
        cargobike_core::groups::resolve(&mut config).expect("the registry resolves");
        let failure = super::validate(&config);
        assert!(
            failure
                .as_ref()
                .is_err_and(|value| value.to_string().contains("allow_machine_approvers")),
            "the machine-approver refusal is the validate's outcome: {failure:?}"
        );
        let _ = std::fs::remove_file(&template_path);
        let _ = std::fs::remove_dir(&directory);
    }

    /// The boot-exact argon2 layer: a PHC-shaped string whose
    /// parameters argon2 cannot parse (the overflow) refuses at boot
    /// even though the offline shape rule passes it.
    #[test]
    fn test_boot_refuses_hashes_argon2_cannot_parse() {
        use serde_yaml_ng::from_str;

        let yaml = "server: {}\ndatabase: { url: postgres://x }\nauth:\n  api_keys:\n    - name: key\n      hash: \"$argon2id$v=19$m=99999999999999999999,t=3,p=4$c2FsdA$ZGlnZXN0\"\n";
        let mut config: crate::config::Config = from_str(yaml).expect("config");
        cargobike_core::groups::resolve(&mut config).expect("the registry resolves");
        let failure = super::validate(&config);
        assert!(
            failure
                .as_ref()
                .is_err_and(|value| value.to_string().contains("hash-api-key")),
            "the unparseable-parameter hash refuses at boot: {failure:?}"
        );
    }
}
