//! The `validate` verb's binary behaviour (4.8/US-13): the exit
//! vocabulary and the miette diagnostic rendering — offline, against
//! files on disk only.

// The scratch fixtures and assertions set up the world; a panic IS the
// test failure here.
#![allow(clippy::expect_used)]

use assert_cmd::Command;

struct Scratch {
    directory: std::path::PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let directory = std::env::temp_dir().join(format!("cb-validate-e2e-{tag}"));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(directory.join("templates")).expect("scratch");
        Scratch { directory }
    }

    fn config(&self, yaml: &str) -> &Self {
        std::fs::write(self.directory.join("cargobike.yaml"), yaml).expect("config");
        self
    }

    fn template(&self, yaml: &str) -> &Self {
        std::fs::write(self.directory.join("templates").join("service.yaml"), yaml)
            .expect("template");
        self
    }

    fn run(&self) -> assert_cmd::assert::Assert {
        Command::cargo_bin("cargobike")
            .expect("the binary")
            .env_remove("CARGOBIKE_SERVER_CONFIG")
            .arg("validate")
            .arg("--config")
            .arg(self.directory.join("cargobike.yaml"))
            .assert()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

const VALID_CONFIG: &str = "server: {}\ndatabase: { url: postgres://x }\ntemplates:\n  directory: TEMP\nauth:\n  oidc:\n    - name: gha\n      issuer: https://issuer\n      audience: cargobike\n      claims:\n        repository_owner_id: \"1\"\n      grants: [release:create]\napplications:\n  - name: web\n    source: { provider: github, id: \"1\" }\n    template: service@1\n    releasers:\n      - oidc: gha\n    environments:\n      production:\n        repo: { provider: github, id: \"1\" }\n";
const VALID_TEMPLATE: &str = "name: service\nversion: \"1\"\ninputs: {}\nenvironments:\n  - name: production\n    steps:\n      - uses: builtin/commit-files@1\n";

#[test]
fn test_validate_passes_and_prints_the_counts() {
    let scratch = Scratch::new("pass");
    scratch
        .config(&VALID_CONFIG.replace(
            "TEMP",
            scratch.directory.join("templates").to_str().expect("path"),
        ))
        .template(VALID_TEMPLATE);
    scratch.run().success().stdout(predicates::str::contains(
        "validate: ok (1 application, 1 template)",
    ));
}

#[test]
fn test_validate_fails_with_the_rule_named() {
    let scratch = Scratch::new("rule");
    // The unconstrained OIDC entry (F-80): the trust entry without
    // claim constraints refuses offline.
    let broken = VALID_CONFIG.replace("      claims:\n        repository_owner_id: \"1\"\n", "");
    scratch
        .config(&broken.replace(
            "TEMP",
            scratch.directory.join("templates").to_str().expect("path"),
        ))
        .template(VALID_TEMPLATE);
    scratch
        .run()
        .failure()
        .code(1)
        .stderr(predicates::str::contains("claim constraint"))
        .stderr(predicates::str::contains("failure(s)"));
}

#[test]
fn test_validate_renders_the_parse_span() {
    let scratch = Scratch::new("span");
    scratch.config("server: {\n");
    scratch
        .run()
        .failure()
        .code(1)
        .stderr(predicates::str::contains("does not parse"))
        .stderr(predicates::str::contains("cargobike.yaml"));
}
