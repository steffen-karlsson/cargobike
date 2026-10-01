//! The output rendering (theirs table|json|yaml): one canonical pass
//! per format, so CLI outputs stay greppable and script-parsable. The
//! rows read real homes: `metadata.id`, `spec.application`,
//! `spec.version`, `status.phase`.

use crate::config::OutputFormat;

/// Rows: the doc's `metadata.{id, application, version, phase}` quartet
/// (the root view) — other fields print in the full document form.
pub fn rows(document: &serde_json::Value) -> Vec<[String; 4]> {
    match document.get("items").and_then(serde_json::Value::as_array) {
        Some(items) => items
            .iter()
            .map(row_of)
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default(),
        None => row_of(document).into_iter().collect(),
    }
}

fn row_of(document: &serde_json::Value) -> Option<[String; 4]> {
    let metadata = document.get("metadata")?;
    let id = metadata.get("id").and_then(serde_json::Value::as_str)?;
    Some([
        id.to_owned(),
        document
            .pointer("/spec/application")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        document
            .pointer("/spec/version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        document
            .pointer("/status/phase")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    ])
}

/// Full document rendering per format.
pub fn documents(
    format: OutputFormat,
    body: &serde_json::Value,
) -> Result<String, std::fmt::Error> {
    match format {
        OutputFormat::Json => rendered(serde_json::to_string_pretty(body)),
        OutputFormat::Yaml => rendered(serde_yaml_ng::to_string(body)),
        OutputFormat::Table => {
            let mut out = String::new();
            for row in rows(body) {
                out.push_str(&row.join("\t"));
                out.push('\n');
            }
            Ok(out)
        }
    }
}

fn rendered(result: Result<String, impl std::fmt::Display>) -> Result<String, std::fmt::Error> {
    result.map_err(|_| std::fmt::Error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_table_lists_the_metadata_quartet() {
        let body = json!({ "items": [
            { "metadata": { "id": "0192-1" }, "spec": { "application": "web", "version": "1.2.3" },
              "status": { "phase": "PendingApproval" } },
            { "metadata": { "id": "0192-2" }, "spec": { "application": "web", "version": "1.3.0" },
              "status": { "phase": "Completed" } },
        ]});
        let rendered = documents(OutputFormat::Table, &body).expect("renders");
        assert_eq!(
            rendered,
            "0192-1\tweb\t1.2.3\tPendingApproval\n0192-2\tweb\t1.3.0\tCompleted\n"
        );
    }

    #[test]
    fn test_single_documents_render_without_items() {
        let body = json!({
            "metadata": { "id": "0192-3" },
            "spec": { "application": "api", "version": "2.0.0" },
            "status": { "phase": "Running" },
        });
        assert_eq!(
            documents(OutputFormat::Table, &body).expect("renders"),
            "0192-3\tapi\t2.0.0\tRunning\n"
        );
        assert_eq!(
            documents(OutputFormat::Json, &body).expect("renders"),
            serde_json::to_string_pretty(&body).expect("json")
        );
        let yaml = documents(OutputFormat::Yaml, &body).expect("renders");
        assert!(yaml.starts_with("metadata:"));
    }
}
