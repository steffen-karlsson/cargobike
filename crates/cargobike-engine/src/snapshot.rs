//! The release snapshot (PRD F-16/F-29): the resolved, validated
//! template plus the registry's supplied inputs, hashed at release
//! creation. The interpreter reads ONLY this (F-15's purity rule).

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use cargobike_core::model::RepoRef as CoreRepoRef;
use cargobike_core::provider::Edit;
use serde::{Deserialize, Serialize};

/// The release identity the snapshot carries (the interpreter's
/// `release.{id, application, version}` context and branch names).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseIdentity {
    /// The release ID (UUIDv7 text; F-2).
    pub id: String,
    /// The application name.
    pub application: String,
    /// The opaque version string (F-4).
    pub version: String,
    /// The release's versioning scheme (F-72's supersede guard); the
    /// provisioner stamps it from the registry's versioning block.
    pub version_scheme: Option<cargobike_core::version::VersionScheme>,
}

/// The canonical snapshot of a release (F-16).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReleaseSnapshot {
    /// The compiled template the interpreter executes.
    pub template: crate::template::CompiledTemplate,
    /// The release's identity (F-8/F-39's context).
    pub release: ReleaseIdentity,
    /// Application-wide inputs the registry supplied (F-32a).
    pub inputs: BTreeMap<String, serde_json::Value>,
    /// Step-type versions the release pins (F-37).
    pub step_type_versions: BTreeMap<String, String>,
    /// Content hash over template + inputs (canonical, F-16).
    pub content_hash: String,
}

/// The `edits` entries' known shape (`{ file, format?, field, value? }`).
#[derive(Clone, Debug, Deserialize)]
struct EditEntry {
    file: String,
    #[serde(default)]
    format: Option<String>,
    field: String,
    #[serde(default)]
    value: Option<serde_json::Value>,
}

/// The Edit format by its config name (`yaml`/`json`/`toml`); the F-41
/// inference is the provider of last resort when the name is missing.
fn format_from_name(name: &str) -> cargobike_core::provider::EditFormat {
    // F-41's declared name (one `format_from_name` consumer); the
    // FILE-PATH inference lives in core::edits::format_for_path.
    match name {
        "json" => cargobike_core::provider::EditFormat::Json,
        "toml" => cargobike_core::provider::EditFormat::Toml,
        _ => cargobike_core::provider::EditFormat::Yaml,
    }
}

impl ReleaseSnapshot {
    /// The expression context of one environment (F-32's contract).
    pub fn context(&self, environment: &str) -> crate::expr::ExprContext {
        let (name, env_inputs) = self
            .template
            .environments
            .iter()
            .find(|spec| spec.name == environment)
            .map(|spec| (spec.name.clone(), spec.env_inputs.clone()))
            .unwrap_or_else(|| (environment.to_owned(), BTreeMap::new()));
        crate::expr::ExprContext {
            release: self.release_fields(),
            environment: crate::expr::EnvironmentFields {
                name,
                inputs: env_inputs,
            },
            inputs: self.inputs.clone(),
            steps: std::collections::HashMap::new(),
        }
    }

    /// The release identity as context fields.
    pub fn release_fields(&self) -> crate::expr::ReleaseFields {
        crate::expr::ReleaseFields {
            id: self.release.id.clone(),
            application: self.release.application.clone(),
            version: self.release.version.clone(),
        }
    }

    /// The edits one environment supplies (F-32a's `edits` well-known
    /// entry); the interpreter and the verifier consume the same list.
    pub fn edits_of(&self, environment: &str) -> Vec<Edit> {
        let Some(inputs) = self
            .template
            .environments
            .iter()
            .find(|spec| spec.name == environment)
            .map(|spec| &spec.env_inputs)
        else {
            return Vec::new();
        };
        let Some(serde_json::Value::Array(entries)) = inputs.get("edits") else {
            return Vec::new();
        };
        entries
            .iter()
            .filter_map(|entry| serde_json::from_value::<EditEntry>(entry.clone()).ok())
            .map(|entry| Edit {
                file: entry.file,
                format: entry.format.as_deref().map(format_from_name),
                field: entry.field,
                value: entry.value,
            })
            .collect()
    }

    /// The target repo reference for one environment (F-32a's `repo`).
    pub fn repo_of(&self, environment: &str) -> Option<CoreRepoRef> {
        let inputs = self
            .template
            .environments
            .iter()
            .find(|spec| spec.name == environment)
            .map(|spec| &spec.env_inputs)?;
        serde_json::from_value::<CoreRepoRef>(inputs.get("repo")?.clone()).ok()
    }

    /// The core's EnvironmentSpec for a resolved environment (the
    /// StepType contract's `env` parameter).
    pub fn env_spec(&self, environment: &str) -> cargobike_core::template::EnvironmentSpec {
        let Some(spec) = self
            .template
            .environments
            .iter()
            .find(|spec| spec.name == environment)
        else {
            return cargobike_core::template::EnvironmentSpec {
                name: environment.to_owned(),
                when: None,
                concurrency: None,
                steps: Vec::new(),
            };
        };
        cargobike_core::template::EnvironmentSpec {
            name: spec.name.clone(),
            when: spec.when.clone(),
            concurrency: spec.concurrency,
            steps: spec.steps.iter().map(resolved_to_core).collect(),
        }
    }

    /// The release record view (F-1's three root properties; status is
    /// the interpreter's view — attempts/remotes stay server-side).
    pub fn read_release(&self) -> cargobike_core::model::Release {
        let now = time::OffsetDateTime::now_utc();
        let metadata = cargobike_core::model::ReleaseMetadata {
            id: uuid::Uuid::now_v7(),
            created_at: now,
            updated_at: now,
            resource_version: 1,
            retried_from: None,
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
        };
        let spec = cargobike_core::model::ReleaseSpec {
            application: self.release.application.clone(),
            version: self.release.version.clone(),
            source: CoreRepoRef::new("github", "0"),
            template: format!("{}@{}", self.template.name, self.template.version),
        };
        let status = cargobike_core::model::ReleaseStatus {
            phase: cargobike_core::model::Phase::Running,
            actor: cargobike_core::model::Actor {
                issuer: "system".to_owned(),
                subject: "cargobike".to_owned(),
                display_name: "Cargobike".to_owned(),
            },
            ci: None,
            workflow: cargobike_core::model::WorkflowInfo {
                id: crate::interpreter::INTERPRETER_WORKFLOW.to_owned(),
                template_name: self.template.name.clone(),
                template_version: self.template.version.clone(),
                template_hash: self.content_hash.clone(),
                step_type_versions: self.step_type_versions.clone(),
            },
            error: None,
            environments: Vec::new(),
            attempts: Vec::new(),
        };
        cargobike_core::model::Release {
            metadata,
            spec,
            status,
        }
    }
}

/// A resolved step flattened back into the core contract's step shape
/// (the StepType execute contract takes core types).
pub fn resolved_to_core(
    step: &crate::template::ResolvedStep,
) -> cargobike_core::template::StepSpec {
    use crate::template::StepBody as Body;
    let (uses, wait, with, timeout, duration, on_timeout, on_modified, retry) = match &step.body {
        Body::Action {
            uses,
            with,
            timeout: _action_timeout,
            retry,
        } => (
            Some(uses.clone()),
            None,
            with.clone(),
            None,
            None,
            None,
            None,
            retry.clone(),
        ),
        Body::WaitMerge {
            timeout,
            on_timeout,
            on_modified,
        } => (
            None,
            Some(cargobike_core::template::WaitKind::Merge),
            serde_json::Value::Null,
            Some(duration_text(timeout)),
            None,
            Some(*on_timeout),
            Some(*on_modified),
            None,
        ),
        Body::WaitApproval {
            timeout,
            on_timeout,
        } => (
            None,
            Some(cargobike_core::template::WaitKind::Approval),
            serde_json::Value::Null,
            Some(duration_text(timeout)),
            None,
            Some(*on_timeout),
            None,
            None,
        ),
        Body::WaitSleep { duration } => (
            None,
            Some(cargobike_core::template::WaitKind::Sleep),
            serde_json::Value::Null,
            None,
            Some(duration_text(duration)),
            None,
            None,
            None,
        ),
    };
    cargobike_core::template::StepSpec {
        id: Some(step.id.clone()),
        uses,
        wait,
        include: None,
        when: step.when.clone(),
        with,
        timeout,
        duration,
        on_timeout,
        on_modified,
        retry,
    }
}

/// humantime text of a parsed duration (the snapshot's canonical form).
pub fn duration_text(duration: &StdDuration) -> String {
    let seconds = duration.as_secs_f64();
    humantime::format_duration(StdDuration::from_secs_f64(seconds)).to_string()
}
