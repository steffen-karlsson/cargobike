//! `cargobike validate` (4.8, US-13): offline config + template
//! validation — the shared rules (`cargobike-core::validation`) and the
//! offline template compiler (`cargobike-core::template::compile`)
//! over the operator's own files. No server, no provider, no network.
//!
//! The CLI validates the FILE as written — the server's env overlay
//! and `${VAR}` interpolation happen at boot and cannot be checked
//! offline; a plain string field carrying an unexpanded `${VAR}` parses
//! and validates as the raw text.
//!
//! `--resolve` adds F-10's STRICT half on top of the offline pass:
//! every application's declared `source.path` is verified against its
//! provider's own truth (`get_repo_path` over a PATH-LESS probe — the
//! GitHub provider short-circuits a declared label to itself, so the
//! probe must not carry it). The mode is fail-on-error: a mismatch, a
//! provider error, or a provider with no built transport is a
//! validation failure (the server's boot check is the
//! fail-closed-unavailable twin).

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

/// The offline pass's outcome: the resolved config (the strict half
/// reads its providers and applications), the counted surface, and
/// the collected failures (empty when the config is sound).
struct Offline {
    config: Config,
    validated: Validated,
    failures: Vec<ValidationFailure>,
}

/// The whole offline check: read + parse, the registry resolution, the
/// template directory, the shared rules, the per-application compile.
/// Every failure is collected (the operator fixes the file in one
/// pass, not one round-trip per rule). A config that does not parse
/// has nothing further to check — its failures are the whole outcome.
fn offline(config_path: &Path) -> Result<Offline, Vec<ValidationFailure>> {
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

    let validated = Validated {
        applications: config.applications.len(),
        templates: templates.len(),
    };
    Ok(Offline {
        config,
        validated,
        failures,
    })
}

/// The offline verb: the shared rules and the template compiler over
/// the local files — no server, no provider, no network.
pub fn run(config_path: &Path) -> Result<Validated, Vec<ValidationFailure>> {
    let offline = offline(config_path)?;
    if offline.failures.is_empty() {
        Ok(offline.validated)
    } else {
        Err(offline.failures)
    }
}

/// `--resolve` (F-10's strict half): the offline pass first, then
/// every declared `source.path` verified against its provider. The
/// mode is fail-on-error — the operator's pre-flight, unlike the
/// server boot's fail-closed-unavailable.
pub async fn run_resolve(config_path: &Path) -> Result<Validated, Vec<ValidationFailure>> {
    let offline = offline(config_path)?;
    let mut failures = offline.failures;
    failures.extend(strict_paths(&offline.config, config_path).await);
    if failures.is_empty() {
        Ok(offline.validated)
    } else {
        Err(failures)
    }
}

/// One `SecretValue` to its material — the server's `materialise`
/// mirrored (the CLI cannot take the server's copy; the shapes are
/// core's and the semantics are the config contract): a literal, a
/// file (trailing newline trimmed), an env name, or `{ secret: name }`
/// into the `secrets:` section.
fn materialise_secret(
    value: &cargobike_core::config::SecretValue,
    secrets: &BTreeMap<String, cargobike_core::config::SecretSource>,
) -> Result<secrecy::SecretString, String> {
    use cargobike_core::config::{SecretSource, SecretValue};
    match value {
        SecretValue::Literal(text) => Ok(secrecy::SecretString::from(text.clone())),
        SecretValue::File(secret) => {
            let contents = std::fs::read_to_string(&secret.file)
                .map_err(|error| format!("{}: {error}", secret.file.display()))?;
            Ok(secrecy::SecretString::from(
                contents.trim_end_matches('\n').to_owned(),
            ))
        }
        SecretValue::Env { env } => {
            let value =
                std::env::var(env).map_err(|_| format!("the env var `{env}` is not set"))?;
            Ok(secrecy::SecretString::from(value))
        }
        SecretValue::Secret { r#secret } => {
            let name = r#secret;
            let source = secrets
                .get(name)
                .ok_or_else(|| format!("the secret `{name}` is not in the secrets section"))?;
            match source {
                SecretSource::File { file } => {
                    let contents = std::fs::read_to_string(file)
                        .map_err(|error| format!("{}: {error}", file.display()))?;
                    Ok(secrecy::SecretString::from(
                        contents.trim_end_matches('\n').to_owned(),
                    ))
                }
                SecretSource::Env { env } => {
                    let value = std::env::var(env)
                        .map_err(|_| format!("the env var `{env}` is not set"))?;
                    Ok(secrecy::SecretString::from(value))
                }
            }
        }
    }
}

/// The github provider build — the server's `build_github_provider`
/// mirrored (the provider crate owns the auth logic; the CLI cannot
/// reach the server's copy). Every error names the provider.
fn build_github(
    provider: &cargobike_core::config::ProviderConfig,
    config: &Config,
) -> Result<std::sync::Arc<dyn cargobike_core::provider::Provider>, String> {
    let Some(auth) = &provider.auth else {
        return Err(format!(
            "the github provider `{}` carries no auth block",
            provider.name
        ));
    };
    let private_key = materialise_secret(&auth.private_key, &config.secrets)?;
    let installation = match auth.installation_id.as_deref() {
        Some(text) => Some(text.parse::<u64>().map_err(|failure| {
            format!(
                "the github provider `{}`'s installation_id does not parse: {failure}",
                provider.name
            )
        })?),
        None => None,
    };
    let app_id = auth.app_id.parse::<u64>().map_err(|failure| {
        format!(
            "the github provider `{}`'s app_id refused to parse: {failure}",
            provider.name
        )
    })?;
    let built = cargobike_provider_github::GithubProvider::new_with_api_url(
        &cargobike_provider_github::GithubAuth::App {
            app_id,
            installation_id: installation,
            private_key_pem: private_key,
        },
        provider.api_url.as_deref(),
    )
    .map_err(|error| {
        format!(
            "the github provider `{}` refused to build: {error}",
            provider.name
        )
    })?;
    Ok(std::sync::Arc::new(built))
}

/// F-10's strict half: every application's declared `source.path` is
/// compared against its provider's truth — `get_repo_path` over a
/// PATH-LESS probe (a probe carrying the label short-circuits to
/// itself in the GitHub provider and verifies nothing). A mismatch, a
/// provider error, or a provider with no built transport is a
/// FAILURE (fail-on-error; the boot's fail-closed-unavailable is the
/// server's story). An application without a declared path has
/// nothing to verify; one without an `id` is the offline rules'
/// failure to report.
async fn strict_paths(config: &Config, config_path: &Path) -> Vec<ValidationFailure> {
    let mut failures = Vec::new();
    let mut providers: BTreeMap<String, std::sync::Arc<dyn cargobike_core::provider::Provider>> =
        BTreeMap::new();
    for provider in &config.providers {
        if provider.r#type != "github" {
            // Not an error at the provider level (the transport is the
            // next phase's); an APPLICATION declaring a path on it is.
            continue;
        }
        match build_github(provider, config) {
            Ok(built) => {
                providers.insert(provider.name.clone(), built);
            }
            Err(error) => failures.push(ValidationFailure {
                source: config_path.to_path_buf(),
                message: error,
                span: None,
            }),
        }
    }
    for app in &config.applications {
        let Some(declared) = &app.source.path else {
            continue;
        };
        let Some(id) = app.source.id.as_deref() else {
            continue;
        };
        match providers.get(app.source.provider.as_str()) {
            Some(provider) => {
                let probe = cargobike_core::model::RepoRef::new(&app.source.provider, id);
                match provider.get_repo_path(&probe).await {
                    Ok(actual) if &actual == declared => {}
                    Ok(actual) => failures.push(ValidationFailure {
                        source: config_path.to_path_buf(),
                        message: format!(
                            "application `{}`: the declared source.path `{declared}` does not match the provider's `{actual}` (F-10)",
                            app.name
                        ),
                        span: None,
                    }),
                    Err(error) => failures.push(ValidationFailure {
                        source: config_path.to_path_buf(),
                        message: format!(
                            "application `{}`: the source.path check refused: {error} — fail-on-error (F-10's strict mode)",
                            app.name
                        ),
                        span: None,
                    }),
                }
            }
            None => failures.push(ValidationFailure {
                source: config_path.to_path_buf(),
                message: format!(
                    "application `{}`: the provider `{}` has no built transport (only `github` resolves) — the declared source.path `{declared}` cannot be verified",
                    app.name, app.source.provider
                ),
                span: None,
            }),
        }
    }
    failures
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

    // ---- F-10's strict half: the wiremock GitHub over the App auth ----

    /// A throwaway 1024-bit RSA key (PKCS#8): the App auth's PEM parse
    /// needs a real shape; the key never leaves the test.
    const TEST_APP_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIICdgIBADANBgkqhkiG9w0BAQEFAASCAmAwggJcAgEAAoGBAOJhaQLA8WUeB4B8
jZ4VPJ7M8Ud38MO0ahBy+bXXXq4bMxPRLaxo+0dnHyX38niBuTAQiEcYXfJ2BeLI
QWB9XdKAjRaczsweLTBLDv7EjjBA9914v4ZuM2kwf5E2GjNvOm/1DHzgLsMo7XC/
DAmq8D35sIBvpv0WZA3d4BpDC80TAgMBAAECgYBrI+EYPYrB6aFxTQGVZMWo/qPe
yEQ5y2Q7Xr8SMdkMm7Dj2yc7pv/t/j8+sAegwvyLY/erdjvYacVFXLOcYBkLiExj
lgTMId8peZ05enFI/K0wx371+g9Np6y774jaVCP28i2EuXWwRPxsSOD2ayfPK4d8
8xyCCmyVzN1t+JdaiQJBAPx+Lj+WrY6aO6EXvgyNvTmsCj2hIyDdrT7iJP+DMVPr
uZlWYgJSIJv8WFjBgEb0qvmv/AZIQvPD2TVFUEFaxI8CQQDlhmDrkrV++2SkO2sD
PBX6ehtxmB2NH8qYEa9thvTLNFdw/k7F+QNK/6CnnanzJY6qAt+w14ZNuHo9kAcf
txk9AkBAW+Vr/ftVSwkCAexGhOaDyc9+cBFlMjwTuE8X9hffvUk8xaEL2GVJfWQD
k6cf7wOZy3rhGxA91GEMTBOt5PYPAkAKCZBffPD29qN2Eysg3xXO+FwO4aoZK9S+
Rbe4QHigEk/onxZuze747qjl650tBu1wPePuZd53s6OU4aGOH4G5AkEAgxZQMDMp
FoRUpOE4m7H0NdP8lvCiaa9ZsYHtvH20mRpXfSIvxdQG4S27TliCgs/59bZ1DfYr
knRiQmITBUx3Jg==
-----END PRIVATE KEY-----";

    /// The strict config: a github provider over a wiremock API (a
    /// pinned installation), one app whose declared `source.path` is
    /// the `declared` argument (the offline rules must pass — the
    /// strict half runs on top of them).
    /// The strict config: a github provider over a wiremock API (a
    /// pinned installation), one app whose declared `source.path` is
    /// the `declared` argument — the offline rules must pass (the
    /// strict half runs on top of them).
    fn strict_config(scratch: &Scratch, api: Option<&str>, declared: Option<&str>) -> PathBuf {
        let temp = scratch.directory.join("templates");
        let providers = match api {
            Some(api) => format!(
                "providers:\n  - name: github\n    type: github\n    api_url: \"{api}\"\n    auth:\n      app_id: \"123\"\n      installation_id: \"77\"\n      private_key: \"{}\"\n",
                TEST_APP_KEY.replace('\n', "\\n")
            ),
            None => String::new(),
        };
        let source_path = declared
            .map(|path| format!("      path: {path}\n"))
            .unwrap_or_default();
        let yaml = format!(
            "server: {{}}\ndatabase: {{ url: postgres://x }}\ntemplates:\n  directory: {}\nauth:\n  oidc:\n    - name: gha\n      issuer: https://issuer\n      audience: cargobike\n      claims:\n        repository_owner_id: \"1\"\n      grants: [release:create]\n{providers}applications:\n  - name: web\n    source:\n      provider: github\n      id: \"42\"\n{source_path}    template: service@1\n    releasers:\n      - oidc: gha\n    environments:\n      production:\n        repo: {{ provider: github, id: \"42\" }}\n",
            temp.display(),
        );
        let path = scratch.config(&yaml);
        scratch.template(
            "service.yaml",
            "name: service\nversion: \"1\"\ninputs: {}\nenvironments:\n  - name: production\n    steps:\n      - uses: builtin/commit-files@1\n",
        );
        path
    }

    /// The wiremock GitHub: the pinned installation's token mint, then
    /// the repository read the PATH-LESS probe makes. `truth: None` is
    /// the provider-error case (the read 404s).
    async fn github_api(truth: Option<&str>) -> wiremock::MockServer {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/app/installations/77/access_tokens"))
            .respond_with(|_: &wiremock::Request| {
                ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "token": "ghs_strict",
                    "expires_at": "2099-01-01T00:00:00Z",
                    "permissions": {},
                }))
            })
            .mount(&server)
            .await;
        let repository = match truth {
            Some(full_name) => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 42,
                "name": "truth",
                "full_name": full_name,
                "default_branch": "main",
                "url": "https://x.invalid/repo",
            })),
            None => ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "message": "Not Found",
            })),
        };
        Mock::given(method("GET"))
            .and(path("/repositories/42"))
            .respond_with(move |_: &wiremock::Request| repository.clone())
            .mount(&server)
            .await;
        server
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_resolve_matches_the_providers_truth() {
        let scratch = Scratch::new("resolve-match");
        let server = github_api(Some("org/truth")).await;
        let config = strict_config(&scratch, Some(&server.uri()), Some("org/truth"));
        let validated = run_resolve(&config).await.expect("the strict pass");
        assert_eq!(validated.applications, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_resolve_fails_on_a_path_mismatch() {
        let scratch = Scratch::new("resolve-mismatch");
        let server = github_api(Some("org/truth")).await;
        let config = strict_config(&scratch, Some(&server.uri()), Some("org/wrong"));
        let failures = run_resolve(&config)
            .await
            .expect_err("the mismatch refuses");
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures[0].message.contains("org/wrong") && failures[0].message.contains("org/truth"),
            "the failure names both paths: {failures:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_resolve_fails_on_a_provider_error() {
        let scratch = Scratch::new("resolve-error");
        let server = github_api(None).await;
        let config = strict_config(&scratch, Some(&server.uri()), Some("org/truth"));
        let failures = run_resolve(&config).await.expect_err("fail-on-error");
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures[0].message.contains("fail-on-error"),
            "the failure names the strict mode: {failures:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_resolve_fails_on_an_unbuilt_transport() {
        // An app declaring a path on a provider whose transport is not
        // built (the extension phase) cannot be verified — strictly a
        // failure, never a skip that reads as a pass.
        let scratch = Scratch::new("resolve-transport");
        let temp = scratch.directory.join("templates");
        let yaml = "server: {}\ndatabase: { url: postgres://x }\ntemplates:\n  directory: TEMP\nauth:\n  oidc:\n    - name: gha\n      issuer: https://issuer\n      audience: cargobike\n      claims:\n        repository_owner_id: \"1\"\n      grants: [release:create]\nproviders:\n  - name: gitea\n    type: extension\n    extension: gitea\napplications:\n  - name: web\n    source:\n      provider: gitea\n      id: \"42\"\n      path: org/truth\n    template: service@1\n    releasers:\n      - oidc: gha\n    environments:\n      production:\n        repo: { provider: gitea, id: \"42\" }\n"
            .replace("TEMP", temp.to_str().expect("path"));
        scratch.config(&yaml);
        scratch.template(
            "service.yaml",
            "name: service\nversion: \"1\"\ninputs: {}\nenvironments:\n  - name: production\n    steps:\n      - uses: builtin/commit-files@1\n",
        );
        let config = scratch.directory.join("cargobike.yaml");
        let failures = run_resolve(&config)
            .await
            .expect_err("no transport, no verify");
        assert!(
            failures
                .iter()
                .any(|failure| failure.message.contains("no built transport")),
            "the failure names the unbuilt transport: {failures:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_apps_without_a_declared_path_are_not_checked() {
        // No declared path, no providers at all: the strict half has
        // nothing to verify and the offline pass stands alone.
        let scratch = Scratch::new("resolve-skip");
        let config = strict_config(&scratch, None, None);
        let validated = run_resolve(&config).await.expect("nothing to verify");
        assert_eq!(validated.applications, 1);
    }
}
