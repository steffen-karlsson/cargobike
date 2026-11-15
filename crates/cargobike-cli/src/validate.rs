//! `cargobike validate` (4.8, US-13): offline config + template
//! validation — the shared rules (`cargobike-core::validation`) and the
//! offline template compiler (`cargobike-core::template::compile`)
//! over the operator's own files. No server, no provider, no network.
//!
//! The CLI validates the FILE as written — the server's env overlay
//! and `${VAR}` interpolation happen at boot and cannot be checked
//! offline; a plain string field carrying an unexpanded `${VAR}` parses
//! and validates as the raw text. The strict, provider-resolving
//! `--resolve` mode (F-10's mismatch check) needs provider credentials
//! and is NOT part of the offline verb; the server's boot check covers
//! it fail-closed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cargobike_core::config::Config;
use cargobike_core::template::PipelineTemplate;

/// One validation outcome failure, rendered as a miette diagnostic.
#[derive(Debug)]
pub struct ValidationFailure {
    /// The file the failure is about (config or template path).
    pub source: PathBuf,
    /// The failure's message.
    pub message: String,
    /// The 1-based line/column when the failure is a parse error.
    pub span: Option<(usize, usize)>,
}

/// The verb's outcome: the counted surface on success.
#[derive(Debug)]
pub struct Validated {
    /// Applications checked.
    pub applications: usize,
    /// Templates compiled.
    pub templates: usize,
}

/// Resolves the config path: the explicit flag, then
/// `CARGOBIKE_SERVER_CONFIG`, then `./cargobike.yaml` when present,
/// then the server's documented default `/etc/cargobike/config.yaml`.
pub fn default_config_path(explicit: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    if let Ok(path) = std::env::var("CARGOBIKE_SERVER_CONFIG") {
        return PathBuf::from(path);
    }
    let local = PathBuf::from("cargobike.yaml");
    if local.exists() {
        return local;
    }
    PathBuf::from("/etc/cargobike/config.yaml")
}

/// The whole offline check. Every failure is collected (the operator
/// fixes the file in one pass, not one round-trip per rule).
pub fn run(config_path: &Path) -> Result<Validated, Vec<ValidationFailure>> {
    let mut failures = Vec::new();
    let text = match std::fs::read_to_string(config_path) {
        Ok(text) => text,
        Err(error) => {
            return Err(vec![ValidationFailure {
                source: config_path.to_path_buf(),
                message: format!("failed to read {}: {error}", config_path.display()),
                span: None,
            }]);
        }
    };
    let config: Config = match serde_yaml_ng::from_str(&text) {
        Ok(config) => config,
        Err(error) => {
            let span = error.location().map(|location| {
                // serde_yaml_ng's markers are 1-based already.
                (location.line(), location.column())
            });
            return Err(vec![ValidationFailure {
                source: config_path.to_path_buf(),
                message: format!("the config does not parse: {error}"),
                span,
            }]);
        }
    };

    // The registry resolution (§4.13a): the raw entries become the
    // resolved registry the rules and the template compile read. An
    // `extends` override naming an unknown group, or an explicit app
    // omitting `source`/`template`, is a validation failure.
    let mut config = config;
    if let Err(error) = cargobike_core::groups::resolve(&mut config) {
        failures.push(ValidationFailure {
            source: config_path.to_path_buf(),
            message: error.to_string(),
            span: None,
        });
    }

    // The templates directory (the server loader's semantics: .yaml/.yml,
    // deduped by name@version; the SOURCE is kept for the offline compile).
    let mut templates: BTreeMap<(String, String), (PathBuf, String, PipelineTemplate)> =
        BTreeMap::new();
    let directory = &config.templates.directory;
    match std::fs::read_dir(directory) {
        Ok(entries) => {
            for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
                let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
                    continue;
                };
                if extension != "yaml" && extension != "yml" {
                    continue;
                }
                let source = match std::fs::read_to_string(&path) {
                    Ok(source) => source,
                    Err(error) => {
                        failures.push(ValidationFailure {
                            message: format!("failed to read {}: {error}", path.display()),
                            source: path.clone(),
                            span: None,
                        });
                        continue;
                    }
                };
                match serde_yaml_ng::from_str::<PipelineTemplate>(&source) {
                    Ok(template) => {
                        templates.insert(
                            (template.name.clone(), template.version.clone()),
                            (path, source, template),
                        );
                    }
                    Err(error) => {
                        let span = error
                            .location()
                            .map(|location| (location.line(), location.column()));
                        failures.push(ValidationFailure {
                            message: format!("the template does not parse: {error}"),
                            source: path,
                            span,
                        });
                    }
                }
            }
        }
        Err(error) => {
            if !config.raw_applications.is_empty() || !config.application_groups.is_empty() {
                failures.push(ValidationFailure {
                    source: PathBuf::from(directory),
                    message: format!(
                        "failed to read templates from `{directory}`: {error} — the registry declares applications"
                    ),
                    span: None,
                });
            }
        }
    }

    // The shared pure rules over the parsed templates.
    let pure: BTreeMap<(String, String), PipelineTemplate> = templates
        .iter()
        .map(|(key, (_, _, template))| (key.clone(), template.clone()))
        .collect();
    if let Err(error) = cargobike_core::validation::validate(&config, &pure) {
        failures.push(ValidationFailure {
            source: config_path.to_path_buf(),
            message: error.to_string(),
            span: None,
        });
    }

    // F-30's offline half: every application's template compiles
    // structurally against the application's version scheme (F-27a's
    // gate check rides inside).
    for app in &config.applications {
        let reference = &app.template;
        let Some((name, version)) = reference.split_once('@') else {
            failures.push(ValidationFailure {
                source: config_path.to_path_buf(),
                message: format!(
                    "application `{}`: the template reference `{reference}` is not `name@version`",
                    app.name
                ),
                span: None,
            });
            continue;
        };
        match templates.get(&(name.to_owned(), version.to_owned())) {
            Some((path, source, _)) => {
                if let Err(error) = cargobike_core::template::compile::compile(
                    source,
                    &cargobike_core::version::VersionScheme::of(&app.versioning),
                ) {
                    failures.push(ValidationFailure {
                        source: path.clone(),
                        message: format!("application `{}`: {}", app.name, error),
                        span: None,
                    });
                }
            }
            None => {
                failures.push(ValidationFailure {
                    source: config_path.to_path_buf(),
                    message: format!(
                        "application `{}`: the template `{name}@{version}` is not in the templates directory",
                        app.name
                    ),
                    span: None,
                });
            }
        }
    }

    if failures.is_empty() {
        Ok(Validated {
            applications: config.applications.len(),
            templates: templates.len(),
        })
    } else {
        Err(failures)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // the fixtures' panics are the failures
mod tests {
    use super::*;

    /// One scratch directory per test: config.yaml + templates/.
    struct Scratch {
        directory: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            let directory =
                std::env::temp_dir().join(format!("cb-validate-{}-{}", tag, std::process::id()));
            let _ = std::fs::remove_dir_all(&directory);
            std::fs::create_dir_all(directory.join("templates")).expect("scratch");
            Scratch { directory }
        }

        fn config(&self, yaml: &str) -> PathBuf {
            let path = self.directory.join("cargobike.yaml");
            std::fs::write(&path, yaml).expect("config");
            path
        }

        fn template(&self, file: &str, yaml: &str) {
            std::fs::write(self.directory.join("templates").join(file), yaml).expect("template");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    const VALID_CONFIG: &str = "server: {}\ndatabase: { url: postgres://x }\ntemplates:\n  directory: TEMP\nauth:\n  oidc:\n    - name: gha\n      issuer: https://issuer\n      audience: cargobike\n      claims:\n        repository_owner_id: \"1\"\n      grants: [release:create]\napplications:\n  - name: web\n    source: { provider: github, id: \"1\" }\n    template: service@1\n    releasers:\n      - oidc: gha\n    environments:\n      production:\n        repo: { provider: github, id: \"1\" }\n";
    const VALID_TEMPLATE: &str = "name: service\nversion: \"1\"\ninputs: {}\nenvironments:\n  - name: production\n    steps:\n      - uses: builtin/commit-files@1\n";

    fn config_with(yaml: &str, temp: &str) -> String {
        yaml.replace("TEMP", temp)
    }

    #[test]
    fn test_valid_config_and_templates_pass() {
        let scratch = Scratch::new("pass");
        let temp = scratch.directory.join("templates");
        scratch.config(&config_with(VALID_CONFIG, temp.to_str().expect("path")));
        scratch.template("service.yaml", VALID_TEMPLATE);
        let outcome = run(&scratch.directory.join("cargobike.yaml"));
        let validated = outcome.expect("valid");
        assert_eq!(validated.applications, 1);
        assert_eq!(validated.templates, 1);
    }

    #[test]
    fn test_unconstrained_oidc_entry_fails() {
        let scratch = Scratch::new("oidc");
        let temp = scratch.directory.join("templates");
        let yaml = VALID_CONFIG.replace("      claims:\n        repository_owner_id: \"1\"\n", "");
        scratch.config(&config_with(&yaml, temp.to_str().expect("path")));
        scratch.template("service.yaml", VALID_TEMPLATE);
        let failures = run(&scratch.directory.join("cargobike.yaml")).expect_err("refused");
        assert!(
            failures
                .iter()
                .any(|f| f.message.contains("claim constraint")),
            "{failures:?}"
        );
    }

    /// F-27a: a `semver(...)` gate under a calver application refuses
    /// offline — the compile runs with the APPLICATION's scheme.
    #[test]
    fn test_semver_gate_under_calver_fails() {
        let scratch = Scratch::new("gate");
        let temp = scratch.directory.join("templates");
        let gated = VALID_CONFIG.replace("template: service@1", "template: gated@1");
        scratch.config(&config_with(&gated, temp.to_str().expect("path")));
        scratch.template(
            "gated.yaml",
            "name: gated\nversion: \"1\"\ninputs: {}\nenvironments:\n  - name: production\n    when: semver(release.version).prerelease() == null\n    steps:\n      - uses: builtin/commit-files@1\n",
        );
        let failures = run(&scratch.directory.join("cargobike.yaml")).expect_err("refused");
        assert!(
            failures
                .iter()
                .any(|f| f.message.contains("version scheme")),
            "{failures:?}"
        );
    }

    /// US-13's "gate referencing non-existent input": the shared rule
    /// refuses an `inputs.<name>` no block declares.
    #[test]
    fn test_gate_referencing_undeclared_input_fails() {
        let scratch = Scratch::new("input");
        let temp = scratch.directory.join("templates");
        scratch.config(&config_with(VALID_CONFIG, temp.to_str().expect("path")));
        scratch.template(
            "service.yaml",
            "name: service\nversion: \"1\"\ninputs: {}\nenvironments:\n  - name: production\n    steps:\n      - uses: builtin/commit-files@1\n        with: { message: \"${{ inputs.notes }}\" }\n",
        );
        let failures = run(&scratch.directory.join("cargobike.yaml")).expect_err("refused");
        assert!(
            failures
                .iter()
                .any(|f| f.message.contains("the input `notes`")),
            "{failures:?}"
        );
    }

    #[test]
    fn test_missing_templates_directory_with_applications_fails() {
        let scratch = Scratch::new("missing");
        let temp = scratch.directory.join("nowhere");
        scratch.config(&config_with(VALID_CONFIG, temp.to_str().expect("path")));
        let failures = run(&scratch.directory.join("cargobike.yaml")).expect_err("refused");
        assert!(
            failures
                .iter()
                .any(|f| f.message.contains("the registry declares applications")),
            "{failures:?}"
        );
    }

    /// A YAML parse failure carries the source span for the diagnostic.
    #[test]
    fn test_config_parse_failure_carries_the_span() {
        let scratch = Scratch::new("parse");
        scratch.config("server: {\n");
        let failures = run(&scratch.directory.join("cargobike.yaml")).expect_err("refused");
        assert_eq!(failures.len(), 1);
        assert!(failures[0].span.is_some(), "{failures:?}");
    }
}
