//! The pipeline template model.
//!
//! Durations and CEL expressions stay as their exact source strings: the
//! snapshot stores the template verbatim , validation and evaluation
//! happen in the engine, and a rename in the engine never rewrites the
//! recorded form.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Versioned, reusable pipeline template .
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PipelineTemplate {
    /// Template name; versions of the same name coexist as separate files.
    pub name: String,
    /// Template version string (`"1"`, `"2"`), snapshotted . The file
    /// name is free; this field is authoritative.
    pub version: String,
    /// Human description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Application-wide inputs supplied by the registry .
    #[serde(default)]
    pub inputs: BTreeMap<String, InputSpec>,
    /// Per-environment inputs supplied by the registry . `repo` and
    /// `edits` are the two well-known entries.
    #[serde(default)]
    pub environment_inputs: BTreeMap<String, InputSpec>,
    /// Reusable step lists referenced with `include:` from environments
    /// .
    #[serde(default)]
    pub step_groups: Vec<StepGroup>,
    /// Environments and their step sequences.
    pub environments: Vec<EnvironmentSpec>,
}

/// A typed template input .
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct InputSpec {
    /// What the registry supplies for this input.
    #[serde(rename = "type")]
    pub kind: InputType,
    /// Human description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The well-known and generic input types .
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputType {
    /// A target repository reference (`env.inputs.repo`).
    Repo,
    /// A list of file edits (`env.inputs.edits`).
    Edits,
    /// Free-form string.
    String,
    /// Numeric value.
    Number,
    /// Boolean value.
    Boolean,
}

/// A named, reusable step list .
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StepGroup {
    /// Group name referenced by `include: <name>`.
    pub name: String,
    /// Steps of the group.
    pub steps: Vec<StepSpec>,
}

/// One environment and its step sequence.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentSpec {
    /// Environment name; gates and registry keys reference this .
    pub name: String,
    /// CEL gate controlling whether the environment runs .
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// Concurrency policy; the registry value takes precedence over this
    /// template default (the security-relevant settings are registry-only,
    /// so the template field exists to keep tests self-contained).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<ConcurrencyPolicy>,
    /// Steps (the mix of action steps, control steps, and `include:` groups).
    pub steps: Vec<StepSpec>,
}

/// Concurrency policy per environment .
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConcurrencyPolicy {
    /// Close the old CR and take over (..).
    Supersede,
    /// Queue behind the current release .
    Queue,
    /// Refuse with `ConcurrencyRejected` .
    Reject,
}

/// One step in a sequence .
///
/// Exactly one of [`StepSpec::uses`], [`StepSpec::wait`], or
/// [`StepSpec::include`] is set; validation rejects otherwise (the
/// interpreter must not half-run `include` groups in-place).
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StepSpec {
    /// Output-referencing ID; auto-generated deterministically when omitted
    /// .
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Action step registry name, `builtin/commit-files@1` .
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uses: Option<String>,
    /// Control step kind (`merge`, `approval`, `sleep` — ).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait: Option<WaitKind>,
    /// `include:` a `step_groups` entry by name .
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<String>,
    /// Parameters for the action step . Secrets travel as
    /// `{ secret: <name> }` references , never inline.
    #[serde(default)]
    pub with: serde_json::Value,
    /// Wait/action deadline; required on `wait: merge` and `wait: approval`,
    /// not valid on `wait: sleep` (the which takes [`StepSpec::duration`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// `wait: sleep` length .
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,
    /// Wait deadline outcome : fail or cancel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_timeout: Option<OnTimeout>,
    /// Wait-for-merge behaviour on head SHA change .
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_modified: Option<OnModified>,
    /// Step-level CEL gate .
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// Retry policy for action steps .
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryPolicy>,
}

/// Control-step kinds .
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WaitKind {
    /// `wait: merge` — recv + provider verification loop .
    Merge,
    /// `wait: approval` — recv .
    Approval,
    /// `wait: sleep` — durable sleep, takes [`StepSpec::duration`].
    Sleep,
}

/// What happens when a wait deadline passes .
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OnTimeout {
    /// Fail the environment (`MergeTimeout` / `ApprovalTimeout` by wait kind).
    Fail,
    /// Cancel the environment and the release (CLI exit 2).
    Cancel,
}

/// What happens when the CR head SHA changes while waiting .
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OnModified {
    /// Error with `ChangeRequestModified` (the default).
    Fail,
    /// Accept the modified CR and keep going.
    Accept,
}

/// Action-step retry policy .
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RetryPolicy {
    /// Total attempts (the default 1, no retry).
    pub attempts: u32,
    /// Backoff shape (the default fixed).
    pub backoff: Backoff,
    /// First delay (the default implementation-chosen).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_delay: Option<String>,
    /// Backoff cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_delay: Option<String>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 1,
            backoff: Backoff::Fixed,
            initial_delay: None,
            max_delay: None,
        }
    }
}

/// Backoff shape for action-step retries .
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backoff {
    /// Same delay every time (the default).
    #[default]
    Fixed,
    /// Exponential growth up to `max_delay`.
    Exponential,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// The documented template, verbatim.
    const SERVICE_TEMPLATE: &str = include_str!("../../../templates/service.yaml");

    #[test]
    fn test_service_template_parses_and_matches_f32_test_promise() {
        // promises a unit test loads the documented example verbatim.
        assert_eq!(SERVICE_TEMPLATE.lines().next(), Some("name: service"));
        let template: PipelineTemplate =
            serde_yaml_ng::from_str(SERVICE_TEMPLATE).expect("template must parse");
        assert_eq!(template.name, "service");
        assert_eq!(template.version, "1");
        assert_eq!(template.environment_inputs.len(), 2);
        assert_eq!(template.environments.len(), 2);
    }

    #[test]
    fn test_template_roundtrips_through_serde_yaml() {
        let template: PipelineTemplate =
            serde_yaml_ng::from_str(SERVICE_TEMPLATE).expect("template must parse");
        let back: PipelineTemplate = serde_yaml_ng::from_str(
            &serde_yaml_ng::to_string(&template).expect("template must serialise"),
        )
        .expect("reparse must succeed");
        assert_eq!(back, template);
    }

    #[test]
    fn test_unknown_template_fields_are_rejected() {
        let result = serde_yaml_ng::from_str::<PipelineTemplate>(
            "name: service\nversion: \"1\"\nnot_a_field: 1\nenvironments: []",
        );
        assert!(
            result.is_err(),
            "unknown fields must fail (deny_unknown_fields)"
        );
    }

    #[test]
    fn test_wait_kind_and_policy_enums_kebab_case() {
        let yaml = "- wait: merge\n- wait: approval\n- wait: sleep";
        let waits: Vec<StepSpec> = serde_yaml_ng::from_str(yaml).expect("wait kinds must parse");
        assert_eq!(waits[0].wait, Some(WaitKind::Merge));
        assert_eq!(waits[1].wait, Some(WaitKind::Approval));
        assert_eq!(waits[2].wait, Some(WaitKind::Sleep));
    }
}
