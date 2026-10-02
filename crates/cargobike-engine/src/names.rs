//! Deterministic names : branches derive from the release
//! and the environment; the default format here is the contract and the
//! server's `engine.branch_format` overrides it via the snapshot.

use std::collections::BTreeMap;

use cargobike_core::model::Release;
use cargobike_core::step::EnvRef;

/// `cargobike/{application}/{environment}/{release_id}` (the default).
pub const DEFAULT_BRANCH_FORMAT: &str = "cargobike/{application}/{environment}/{release_id}";

/// Expands the branch name for a release + environment. The env's
/// `branch_format` input (the stamped by the provisioner) wins over the
/// default when present (the placeholders below).
pub fn branch_name(release: &Release, env: &EnvRef) -> String {
    branch_name_with(
        env.branch_format
            .as_deref()
            .unwrap_or(DEFAULT_BRANCH_FORMAT),
        release,
        env,
    )
}

/// The expansion with the name set.
pub fn branch_name_with(format: &str, release: &Release, env: &EnvRef) -> String {
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    values.insert("application".to_owned(), release.spec.application.clone());
    values.insert("environment".to_owned(), env.name.clone());
    values.insert("release_id".to_owned(), release.metadata.id.to_string());
    expand(format, &values)
}

/// format-string expansion: `{placeholder}` from `values`,
/// `{{`/`}}` as literal-brace escapes, unknown placeholders pass
/// through unvalidated here (the validation lives in the compile-time
/// checks, references).
pub fn expand(format: &str, values: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(format.len());
    let mut rest = format;
    while let Some(open) = rest.find('{') {
        let (before, after_open) = rest.split_at(open);
        out.push_str(before);
        let Some(close) = after_open.find('}') else {
            // unterminated placeholder: the remainder is literal
            out.push_str(after_open);
            break;
        };
        let token = &after_open[1..close];
        match token {
            "" | "{" | "}" => out.push_str(&after_open[..close + 1]),
            _ => {
                if let Some(value) = values.get(token) {
                    out.push_str(value);
                } else {
                    // Unknown names stay with their braces (the validation
                    // refuses the named pattern at compile; runtime is exact).
                    out.push('{');
                    out.push_str(token);
                    out.push('}');
                }
            }
        }
        rest = &after_open[close + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cargobike_core::model::{Actor, Phase, ReleaseMetadata, ReleaseSpec, ReleaseStatus};
    use pretty_assertions::assert_eq;

    /// A release with the pinned application/version/id.
    fn release(id: uuid::Uuid) -> Release {
        let now = time::OffsetDateTime::now_utc();
        Release {
            metadata: ReleaseMetadata {
                id,
                created_at: now,
                updated_at: now,
                resource_version: 1,
                retried_from: None,
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
            },
            spec: ReleaseSpec {
                application: "my-service".to_owned(),
                version: "1.2.3".to_owned(),
                source: cargobike_core::model::RepoRef::new("github", "123456"),
                template: "service@1".to_owned(),
            },
            status: ReleaseStatus {
                phase: Phase::Running,
                actor: Actor {
                    issuer: "system".to_owned(),
                    subject: "cargobike".to_owned(),
                    display_name: "Cargobike".to_owned(),
                },
                ci: None,
                workflow: cargobike_core::model::WorkflowInfo {
                    id: "cargobike.interpret.v2".to_owned(),
                    template_name: "service".to_owned(),
                    template_version: "1".to_owned(),
                    template_hash: "sha256:x".to_owned(),
                    step_type_versions: BTreeMap::new(),
                },
                error: None,
                environments: Vec::new(),
                attempts: Vec::new(),
            },
        }
    }

    #[test]
    fn test_branch_name_matches_the_a1_default() {
        let id = uuid::Uuid::parse_str("51b47e34-4fc1-11f0-a33b-e7293bc40499").expect("uuid");
        let release = release(id);
        let env = EnvRef::named("preview");
        assert_eq!(
            branch_name(&release, &env),
            format!("cargobike/my-service/preview/{id}")
        );
    }

    #[test]
    fn test_expansion_is_deterministic_and_exact() {
        let format = "rel/{application}+{environment}";
        let mut values: BTreeMap<String, String> = BTreeMap::new();
        values.insert("application".to_owned(), "my-service".to_owned());
        values.insert("environment".to_owned(), "preview".to_owned());
        assert_eq!(expand(format, &values), "rel/my-service+preview");
    }

    #[test]
    fn test_unknown_placeholders_stay_verbatim() {
        let mut values: BTreeMap<String, String> = BTreeMap::new();
        values.insert("application".to_owned(), "svc".to_owned());
        assert_eq!(expand("{application}/{unknown}", &values), "svc/{unknown}");
        assert_eq!(expand("{a.pp}", &values), "{a.pp}");
    }
}
