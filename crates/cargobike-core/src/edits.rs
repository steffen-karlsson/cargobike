//! The F-41 edit application: dot-notation paths set into JSON/YAML/TOML
//! documents — structural, never text substitution. Providers share this
//! module so the committed file matches what the verifier walks
//! (identity: the engine's `get_by_dot` reads the same shape).

use crate::provider::{Edit, EditError, EditFormat};

/// One applied document: the edits that belong to a file applied over
/// its base content.
pub fn apply_to_document(
    base: &[u8],
    edits: &[Edit],
    format: EditFormat,
    default_value: &serde_json::Value,
) -> Result<Vec<u8>, EditError> {
    match format {
        EditFormat::Json => {
            let mut document = parse_document(base, format)?;
            for edit in edits {
                let value = desired_value(edit, default_value);
                set_json(&mut document, edit.field.as_str(), &value)?;
            }
            serde_json::to_vec_pretty(&document)
                .map_err(|failure| EditError::InvalidDocument(format!("json: {failure}")))
        }
        EditFormat::Yaml => {
            let mut document = parse_document(base, format)?;
            for edit in edits {
                let value = desired_value(edit, default_value);
                set_json(&mut document, edit.field.as_str(), &value)?;
            }
            serde_yaml_ng::to_string(&document)
                .map(String::into_bytes)
                .map_err(|failure| EditError::InvalidDocument(format!("yaml: {failure}")))
        }
        EditFormat::Toml => {
            let mut document = toml::from_str::<toml::Value>(
                std::str::from_utf8(base)
                    .map_err(|_| EditError::InvalidDocument("toml: utf8".into()))?,
            )
            .map_err(|failure| EditError::InvalidDocument(format!("toml: {failure}")))?;
            for edit in edits {
                let value = desired_value(edit, default_value);
                set_toml(&mut document, edit.field.as_str(), &value)?;
            }
            toml::to_string_pretty(&document)
                .map(String::into_bytes)
                .map_err(|failure| EditError::InvalidDocument(format!("toml: {failure}")))
        }
    }
}

/// F-41's extension-based inference (yaml by default): the ONE
/// implementation all consumers share (the verifier, the providers).
pub fn format_for_path(file: &str) -> EditFormat {
    match file.rsplit_once('.').map(|(_, extension)| extension) {
        Some("json") | Some("JSON") => EditFormat::Json,
        Some("toml") | Some("TOML") => EditFormat::Toml,
        _ => EditFormat::Yaml,
    }
}

/// The edit's desired value: absent OR explicit null reads the
/// release version (F-147's default), anything else wins. The merge
/// verifier shares this so both ends agree.
pub fn desired_value(edit: &Edit, default_value: &serde_json::Value) -> serde_json::Value {
    match &edit.value {
        None | Some(serde_json::Value::Null) => default_value.clone(),
        Some(value) => value.clone(),
    }
}

/// Parses a document in the edit's format (the merge verifier's read
/// side of [`apply_to_document`]); failures are contract errors, never
/// honoured as empty.
pub fn parse_document(base: &[u8], format: EditFormat) -> Result<serde_json::Value, EditError> {
    match format {
        EditFormat::Json => serde_json::from_slice(base)
            .map_err(|failure| EditError::InvalidDocument(format!("json: {failure}"))),
        EditFormat::Yaml => serde_yaml_ng::from_slice(base)
            .map_err(|failure| EditError::InvalidDocument(format!("yaml: {failure}"))),
        EditFormat::Toml => {
            let text = std::str::from_utf8(base)
                .map_err(|_| EditError::InvalidDocument("toml: utf8".to_owned()))?;
            toml::from_str(text)
                .map_err(|failure| EditError::InvalidDocument(format!("toml: {failure}")))
        }
    }
}

/// JSON/YAML share one tree: dot-notation walks objects by name, arrays
/// by integer index (the verifier's semantics).
fn set_json(
    document: &mut serde_json::Value,
    field: &str,
    value: &serde_json::Value,
) -> Result<(), EditError> {
    let mut segments = field.split('.').collect::<Vec<_>>();
    let Some(last) = segments.pop() else {
        return Err(EditError::InvalidPath("empty path".to_owned()));
    };
    let mut current = document;
    for (index, segment) in segments.iter().enumerate() {
        let next = down(current, segment)?;
        let _ = index;
        current = next;
    }
    // The leaf: an object entry or an array slot.
    match current {
        serde_json::Value::Object(entries) => {
            entries.insert(last.to_owned(), value.clone());
            Ok(())
        }
        serde_json::Value::Array(items) => {
            let at: usize = last
                .parse()
                .map_err(|_| EditError::InvalidPath(format!("`{last}` is not an array index")))?;
            let slot = items
                .get_mut(at)
                .ok_or_else(|| EditError::InvalidPath(format!("`{field}` walks past the array")))?;
            *slot = value.clone();
            Ok(())
        }
        _ => Err(EditError::InvalidPath(format!("`{field}` has no envelope"))),
    }
}

/// Walks one hop; missing parents are invalid (the F-41 verifier reads
/// the same shape — an absent envelope is a template error).
fn down<'tree>(
    current: &'tree mut serde_json::Value,
    segment: &str,
) -> Result<&'tree mut serde_json::Value, EditError> {
    match current {
        serde_json::Value::Object(entries) => entries.get_mut(segment),
        serde_json::Value::Array(items) => {
            items.get_mut(segment.parse::<usize>().ok().ok_or_else(|| {
                EditError::InvalidPath(format!("`{segment}` is not an array index"))
            })?)
        }
        _ => None,
    }
    .ok_or_else(|| EditError::InvalidPath(format!("`{segment}` has no envelope")))
}

/// The TOML walk mirrors the JSON one over `toml::Value`.
fn set_toml(
    document: &mut toml::Value,
    field: &str,
    value: &serde_json::Value,
) -> Result<(), EditError> {
    let mut segments = field.split('.').collect::<Vec<_>>();
    let Some(last) = segments.pop() else {
        return Err(EditError::InvalidPath("empty path".to_owned()));
    };
    let mut current = document;
    for segment in segments {
        match current {
            toml::Value::Table(entries) => {
                current = entries.get_mut(segment).ok_or_else(|| {
                    EditError::InvalidPath(format!("`{segment}` has no envelope"))
                })?;
            }
            toml::Value::Array(items) => {
                let at: usize = segment.parse().map_err(|_| {
                    EditError::InvalidPath(format!("`{segment}` is not an array index"))
                })?;
                current = items.get_mut(at).ok_or_else(|| {
                    EditError::InvalidPath(format!("`{field}` walks past the array"))
                })?;
            }
            _ => {
                return Err(EditError::InvalidPath(format!(
                    "`{segment}` has no envelope"
                )));
            }
        }
    }
    match current {
        toml::Value::Table(entries) => {
            entries.insert(last.to_owned(), json_to_toml(value));
            Ok(())
        }
        toml::Value::Array(items) => {
            let at: usize = last
                .parse()
                .map_err(|_| EditError::InvalidPath(format!("`{last}` is not an array index")))?;
            let slot = items
                .get_mut(at)
                .ok_or_else(|| EditError::InvalidPath(format!("`{field}` walks past the array")))?;
            *slot = json_to_toml(value);
            Ok(())
        }
        _ => Err(EditError::InvalidPath(format!("`{field}` has no envelope"))),
    }
}

fn json_to_toml(value: &serde_json::Value) -> toml::Value {
    match value {
        serde_json::Value::String(text) => toml::Value::String(text.clone()),
        serde_json::Value::Bool(flag) => toml::Value::Boolean(*flag),
        serde_json::Value::Number(number) if number.is_i64() => {
            toml::Value::Integer(number.as_i64().unwrap_or_default())
        }
        serde_json::Value::Number(number) if number.is_f64() => {
            toml::Value::Float(number.as_f64().unwrap_or_default())
        }
        _ => toml::Value::String(value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Edit;
    use serde_json::json;

    fn edit(file: &str, field: &str, value: serde_json::Value) -> Edit {
        Edit {
            file: file.to_owned(),
            format: None,
            field: field.to_owned(),
            value: Some(value),
        }
    }

    #[test]
    fn test_json_sets_nested_fields() {
        let applied = apply_to_document(
            br#"{"image": {"tag": "0.9.0"}}"#,
            &[edit("chart.yaml", "image.tag", json!("1.0.0"))],
            EditFormat::Json,
            &json!(null),
        )
        .expect("applies");
        let document: serde_json::Value = serde_json::from_slice(&applied).expect("parses");
        assert_eq!(document["image"]["tag"], json!("1.0.0"));
    }

    #[test]
    fn test_yaml_sets_and_verifies_the_same_walk() {
        let base = b"chart:\n  image:\n    tag: 0.9.0\n";
        let applied = apply_to_document(
            base,
            &[edit("values.yaml", "chart.image.tag", json!("1.0.0"))],
            EditFormat::Yaml,
            &json!(null),
        )
        .expect("applies");
        let back: serde_json::Value =
            serde_yaml_ng::from_slice(&applied).expect("the same walker reads it");
        assert_eq!(back["chart"]["image"]["tag"], json!("1.0.0"));
    }

    #[test]
    fn test_yaml_arrays_take_integer_indices() {
        let base = b"images:\n  - tag: 0.9.0\n  - tag: 0.9.1\n";
        let applied = apply_to_document(
            base,
            &[edit("values.yaml", "images.1.tag", json!("1.0.0"))],
            EditFormat::Yaml,
            &json!(null),
        )
        .expect("applies");
        let back: serde_json::Value = serde_yaml_ng::from_slice(&applied).expect("parses");
        assert_eq!(back["images"][1]["tag"], json!("1.0.0"));
        // The walk pointer semantics: `images.0.tag` unchanged.
        assert_eq!(back["images"][0]["tag"], json!("0.9.0"));
    }

    #[test]
    fn test_toml_sets_the_field_and_defaults_the_value() {
        let base = b"[package]\nversion = \"0.9.0\"\n[package.metadata]\n";
        let applied = apply_to_document(
            base,
            &[edit(
                "Cargo.toml",
                "package.version",
                serde_json::Value::Null,
            )],
            EditFormat::Toml,
            &json!("1.0.0"),
        )
        .expect("applies");
        let back: toml::Value =
            toml::from_str(&String::from_utf8(applied).expect("utf8")).expect("parses");
        assert_eq!(
            back["package"]["version"],
            toml::Value::String("1.0.0".to_owned())
        );
    }

    #[test]
    fn test_missing_envelopes_are_path_errors() {
        let failure = apply_to_document(
            b"{}",
            &[edit("f.json", "image.tag", json!("1.0.0"))],
            EditFormat::Json,
            &json!(null),
        )
        .expect_err("no envelope");
        assert!(matches!(failure, EditError::InvalidPath(_)));
    }
}
