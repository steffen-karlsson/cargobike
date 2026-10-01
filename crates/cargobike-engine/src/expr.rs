//! The CEL expression engine (PRD 3.2, F-27, F-32).
//!
//! Gates and `${{ ... }}` parameter expressions evaluate against one
//! documented context (§6.3's note): `release.{id, application, version}`,
//! `env.{name, inputs}`, `inputs`, and `steps.<id>.outputs`. Pure
//! functions only — the registrar refuses `now()`-style impurity by
//! construction of what it registers (F-27).

use semver::Version as SemVerVersion;

use std::collections::HashMap;
use std::{collections::BTreeMap, sync::Arc};

use cel::Value;
use cel::objects::{Key, Map as CelMap};

/// Evaluation knobs (F-27: limits are configured; the server passes
/// `engine.cel.*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Maximum expression length (F-27; config `engine.cel.max_expression_length`).
    pub max_expression_length: usize,
    /// Cost approximation bound; fires on deep nesting (no runtime fuel
    /// in cel 0.14 — see the note in `Options` at `docs/spike-dbos.md`'s
    /// follow-up list).
    pub max_depth: usize,
}

/// F-27's documented default limits (config overrides).
pub const DEFAULT_LIMITS: Limits = Limits {
    max_expression_length: 4_096,
    max_depth: 24,
};

/// The expression's evaluation failure modes.
#[derive(Debug, thiserror::Error)]
pub enum ExprError {
    /// `${{ ... }}` framing missing where an expression is required.
    #[error("failed to evaluate `{expression}`: not a ${{ }}-wrapped expression")]
    NotWrapped {
        /// The violating source text.
        expression: String,
    },
    /// CEL parse failure.
    #[error("failed to parse `{expression}`: {cause}")]
    Parse {
        /// The violating source text.
        expression: String,
        /// Parser's message.
        cause: String,
    },
    /// Execution failure (unknown identifiers, type errors).
    #[error("failed to evaluate `{expression}`: {cause}")]
    Execution {
        /// The violating source text.
        expression: String,
        /// Runtime message.
        cause: String,
    },
    /// The limit check refused.
    #[error("the expression exceeds the configured limit: {0}")]
    Limit(String),
}

/// The pieces one expression sees (§6.3's context contract).
#[derive(Default, Clone, Debug)]
pub struct ExprContext {
    /// `release.{id, application, version}`.
    pub release: ReleaseFields,
    /// `env.{name, inputs}` — per-environment values.
    pub environment: EnvironmentFields,
    /// Application-wide inputs (F-32a).
    pub inputs: BTreeMap<String, serde_json::Value>,
    /// Recorded outputs of earlier steps, keyed by step ID (F-28).
    pub steps: HashMap<String, serde_json::Value>,
}

/// `release.*` (F-32's context note: `release.{id, application, version}`).
#[derive(Default, Clone, Debug)]
pub struct ReleaseFields {
    /// The release ID.
    pub id: String,
    /// The application name.
    pub application: String,
    /// The release version.
    pub version: String,
}

/// `env.*`.
#[derive(Default, Clone, Debug)]
pub struct EnvironmentFields {
    /// The environment name.
    pub name: String,
    /// Per-environment inputs (F-32a).
    pub inputs: BTreeMap<String, serde_json::Value>,
}

impl ExprContext {
    /// Builds the environment the Program evaluates against (F-32).
    pub fn cel_context(&self) -> cel::Context<'static> {
        let mut context = cel::Context::empty();
        // `now()`-style impurity is not registered (F-27's promise);
        // Context::empty() keeps the pure core's functions only.
        for (name, value) in self.variables() {
            context.add_variable(name, value);
        }
        context.add_function("semver", |candidate: cel::Value| {
            let candidate: String = match &candidate {
                Value::String(text) => (**text).clone(),
                Value::Int(value) => value.to_string(),
                Value::UInt(value) => value.to_string(),
                _ => return Ok(Value::Null),
            };
            let trimmed: String = candidate.trim().to_owned();
            let parsed = SemVerVersion::parse(trimmed.as_str());
            match parsed {
                Ok(version) => {
                    let mut fields: HashMap<Key, Value> = HashMap::new();
                    fields.insert(Key::from("major".to_owned()), Value::UInt(version.major));
                    fields.insert(Key::from("minor".to_owned()), Value::UInt(version.minor));
                    fields.insert(Key::from("patch".to_owned()), Value::UInt(version.patch));
                    fields.insert(
                        Key::from("prerelease".to_owned()),
                        Value::String(Arc::new(version.pre.to_string())),
                    );
                    Ok(Value::Map(CelMap::from(fields)))
                }
                Err(_error) => Ok(Value::Null),
            }
        });
        // The documented gates address the semver result as
        // `semver(v).prerelease()` — CEL resolves `.prerelease()` as a
        // method whose receiver arrives via the magic `This` wrapper.
        for field in ["major", "minor", "patch", "prerelease"] {
            context.add_function(field, move |receiver: cel::extractors::This<cel::Value>| {
                let entry = match &receiver.0 {
                    Value::Map(map) => map.get(&Key::from(field.to_owned())).cloned(),
                    _ => None,
                };
                Ok(entry.unwrap_or(Value::Null))
            });
        }
        context
    }

    /// The variable map of the expression contract (F-32/F-32a).
    fn variables(&self) -> HashMap<&'static str, Value> {
        let mut map = HashMap::new();
        let mut release: HashMap<Key, Value> = HashMap::new();
        release.insert(
            Key::from("id"),
            Value::String(Arc::new(self.release.id.clone())),
        );
        release.insert(
            Key::from("application"),
            Value::String(Arc::new(self.release.application.clone())),
        );
        release.insert(
            Key::from("version"),
            Value::String(Arc::new(self.release.version.clone())),
        );
        map.insert("release", Value::Map(release.clone().into()));
        let mut environment: HashMap<Key, Value> = HashMap::new();
        environment.insert(
            Key::from("name"),
            Value::String(Arc::new(self.environment.name.clone())),
        );
        let mut inputs: HashMap<Key, Value> = HashMap::new();
        for (name, value) in &self.environment.inputs {
            inputs.insert(Key::from(name.clone()), json_to_cel(value));
        }
        environment.insert(Key::from("inputs"), Value::Map(inputs.into()));
        map.insert("env", Value::Map(environment.into()));
        let mut inputs: HashMap<Key, Value> = HashMap::new();
        for (name, value) in &self.inputs {
            inputs.insert(Key::from(name.clone()), json_to_cel(value));
        }
        map.insert("inputs", Value::Map(inputs.into()));
        let mut steps: HashMap<Key, Value> = HashMap::new();
        for (name, value) in &self.steps {
            steps.insert(Key::from(name.clone()), json_to_cel(value));
        }
        map.insert("steps", Value::Map(steps.into()));
        map
    }

    /// Size and depth checks (F-27's limits; both configurable).
    fn check(&self, source: &str) -> Result<(), ExprError> {
        if source.len() > DEFAULT_LIMITS.max_expression_length {
            return Err(ExprError::Limit(format!(
                "expression length {} exceeds {}",
                source.len(),
                DEFAULT_LIMITS.max_expression_length
            )));
        }
        if source.matches('(').count() > DEFAULT_LIMITS.max_depth {
            return Err(ExprError::Limit(format!(
                "nesting deeper than {} is refused",
                DEFAULT_LIMITS.max_depth
            )));
        }
        Ok(())
    }
}

/// JSON → CEL values for the documented variables (a small conversion;
/// the cel crate has no explicit serde_json→Value bridge in 0.14).
fn json_to_cel(value: &serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(value) => Value::Bool(*value),
        serde_json::Value::Number(num) => {
            if let Some(int) = num.as_i64() {
                Value::Int(int)
            } else if let Some(uint) = num.as_u64() {
                Value::UInt(uint)
            } else {
                Value::Float(num.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(value) => Value::String(Arc::new(value.clone())),
        serde_json::Value::Array(items) => {
            Value::List(Arc::new(items.iter().map(json_to_cel).collect()))
        }
        serde_json::Value::Object(map) => {
            let mut entries: HashMap<Key, Value> = HashMap::new();
            for (key, value) in map {
                entries.insert(Key::from(key.clone()), json_to_cel(value));
            }
            Value::Map(entries.into())
        }
    }
}

/// Evaluates a `when` gate to its boolean (F-26/F-27).
pub fn eval_gate(source: &str, context: &ExprContext) -> Result<bool, ExprError> {
    context.check(source)?;
    let program = cel::Program::compile(source).map_err(|error| ExprError::Parse {
        expression: source.to_owned(),
        cause: error.errors[0].to_string(),
    })?;
    let value = program
        .execute(&context.cel_context())
        .map_err(|error| ExprError::Execution {
            expression: source.to_owned(),
            cause: error.to_string(),
        })?;
    match value {
        Value::Bool(result) => Ok(result),
        other => Err(ExprError::Execution {
            expression: source.to_owned(),
            cause: format!("a `when` gate must resolve to bool, got {other:?}"),
        }),
    }
}

/// Evaluates a `${{ ... }}`-wrapped parameter value typed (F-32).
pub fn eval_param(wrapped: &str, context: &ExprContext) -> Result<serde_json::Value, ExprError> {
    let expression = unwrap_expression(wrapped).ok_or_else(|| ExprError::NotWrapped {
        expression: wrapped.to_owned(),
    })?;
    context.check(expression)?;
    let program = cel::Program::compile(expression).map_err(|error| ExprError::Parse {
        expression: wrapped.to_owned(),
        cause: error.errors[0].to_string(),
    })?;
    let value = program
        .execute(&context.cel_context())
        .map_err(|error| ExprError::Execution {
            expression: wrapped.to_owned(),
            cause: error.to_string(),
        })?;
    value.json().map_err(|error| ExprError::Execution {
        expression: wrapped.to_owned(),
        cause: format!("cannot convert result to JSON: {error:?}"),
    })
}

/// Slices the inside of a `${{ ... }}`-wrapper (F-32's framing).
fn unwrap_expression(wrapped: &str) -> Option<&str> {
    let trimmed = wrapped.trim();
    let inner = trimmed.strip_prefix("${{")?;
    let inner = inner.strip_suffix("}}")?;
    Some(inner.trim())
}

/// Evaluates a parameter object: any string that is wholly `${{ ... }}`
/// becomes its typed value; mixed strings substitute each `${{ ... }}`
/// span interpolated as text (F-28's `with:` and F-147's placeholders).
pub fn interpolate_params(
    params: &serde_json::Value,
    context: &ExprContext,
) -> Result<serde_json::Value, ExprError> {
    Ok(match params {
        serde_json::Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.starts_with("${{") && trimmed.ends_with("}}") {
                eval_param(trimmed, context)?
            } else if text.contains("${{") {
                let mut result = String::new();
                replace_spans(text, context, &mut result)?;
                serde_json::Value::String(result)
            } else {
                params.clone()
            }
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|item| interpolate_params(item, context))
                .collect::<Result<Vec<serde_json::Value>, ExprError>>()?,
        ),
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, value) in map {
                out.insert(key.clone(), interpolate_params(value, context)?);
            }
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    })
}

/// Mixed-string substitution: each `${{ ... }}` span resolves typed then
/// renders text (integers without decimals; lists/objects JSON).
fn replace_spans(text: &str, context: &ExprContext, out: &mut String) -> Result<(), ExprError> {
    let mut rest = text;
    while let Some(open) = rest.find("${{") {
        out.push_str(&rest[..open]);
        let after_open = &rest[open + 3..];
        let Some(close) = after_open.find("}}") else {
            break;
        };
        let span = &after_open[..close];
        let value = eval_param(&format!("${{{{ {span} }}}}"), context)?;
        match value {
            serde_json::Value::String(value) => out.push_str(&value),
            serde_json::Value::Number(number) => out.push_str(&number.to_string()),
            other => out.push_str(&other.to_string()),
        }
        rest = &after_open[close + 2..];
    }
    out.push_str(rest);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;
    use serde_json::json;

    /// The documented context, per-variant.
    fn context(version: &str) -> ExprContext {
        let mut env_inputs: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        env_inputs.insert("cluster".to_owned(), json!("test"));
        let mut steps: HashMap<String, serde_json::Value> = HashMap::new();
        steps.insert("cr".to_owned(), json!({ "number": 42, "url": "https://x" }));
        ExprContext {
            release: ReleaseFields {
                id: "0192f0d0".to_owned(),
                application: "my-service".to_owned(),
                version: version.to_owned(),
            },
            environment: EnvironmentFields {
                name: "production".to_owned(),
                inputs: env_inputs.clone(),
            },
            inputs: env_inputs,
            steps,
        }
    }

    #[rstest]
    #[case("1.2.3-rc.1", false)]
    #[case("1.2.3", true)]
    fn test_semver_pure_function_resolves_prerelease(
        #[case] release_version: &str,
        #[case] is_release: bool,
    ) {
        let context = context(release_version);
        let gate = "semver(release.version).prerelease() == \"\"";
        assert_eq!(
            eval_gate(gate, &context).expect("resolves"),
            is_release,
            "prerelease gate of {release_version}"
        );
    }

    #[test]
    fn test_step_output_access_from_the_documented_context() {
        let context = context("1.2.3");
        let gate = "steps.cr.number == 42";
        assert!(eval_gate(gate, &context).expect("steps access"));
    }

    #[test]
    fn test_mixed_string_interpolation_renders_text() {
        let context = context("1.2.3");
        let params = serde_json::json!({ "title": "deploy: ${{ release.version }}" });
        let out = interpolate_params(&params, &context).expect("interpolates");
        assert_eq!(out["title"], json!("deploy: 1.2.3"));
    }

    #[test]
    fn test_wholly_wrapped_param_resolves_typed() {
        let context = context("1.2.3");
        let params = json!({ "repo": "${{ env.inputs.cluster }}" });
        let out = interpolate_params(&params, &context).expect("resolves");
        assert_eq!(out["repo"], json!("test"));
    }

    #[test]
    fn test_undeclared_identifier_is_a_clean_execution_error() {
        let context = context("1.2.3");
        let error = eval_gate("release.build_number == 3", &context).expect_err("must fail");
        assert!(matches!(error, ExprError::Execution { .. }));
    }

    #[test]
    fn test_limits_refuse_oversized_expressions() {
        let context = context("1.2.3");
        let long = "x".repeat(5_000);
        let error = eval_gate(&format!("{long} == 3"), &context).expect_err("too long");
        assert!(matches!(error, ExprError::Limit(_)));
    }

    #[test]
    fn test_not_wrapped_param_clean_error() {
        let context = context("1.2.3");
        let error = eval_param("release.version", &context).expect_err("unwrapped");
        assert!(matches!(error, ExprError::NotWrapped { .. }));
    }
}
