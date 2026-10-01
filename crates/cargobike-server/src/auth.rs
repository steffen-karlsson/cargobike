//! Claim matching for OIDC trust entries (F-79) - the pure half of
//! PRD 2.5's auth. Token plumbing (extractor, JWKS fetch, argon2
//! api-key verify) is the next auth milestone.

/// Claim matching (F-79): all listed claims must match (AND); string
/// claims are glob by default (R7, literal_separator semantics);
/// array-valued claims match if any element matches.
pub fn claim_match(
    expected: &std::collections::BTreeMap<String, serde_json::Value>,
    claims: &serde_json::Value,
) -> Option<()> {
    for (name, expected_value) in expected {
        let actual = claims.get(name)?;
        let matched = match (actual, expected_value) {
            (serde_json::Value::Array(items), expected_value) => {
                items.iter().any(|item| value_match(item, expected_value))
            }
            (actual, expected_value) => value_match(actual, expected_value),
        };
        if !matched {
            return None;
        }
    }
    Some(())
}
/// Matches one claim value: strings compare as globs (R7, using
/// GlobBuilder's literal_separator so `*` stays inside one segment),
/// everything else as exact JSON equality.
fn value_match(actual: &serde_json::Value, expected: &serde_json::Value) -> bool {
    match (expected, actual) {
        (serde_json::Value::String(expected), serde_json::Value::String(actual)) => {
            globset::GlobBuilder::new(expected)
                .literal_separator(true)
                .build()
                .map(|glob| glob.compile_matcher().is_match(actual))
                .unwrap_or(false)
        }
        _ => expected == actual,
    }
}

/// The documented algorithm allowlist (F-78): reject `none` and `HS*`.
pub const ALGORITHM_ALLOWLIST: [&str; 8] = [
    "RS256", "RS384", "RS512", "PS256", "PS384", "PS512", "ES256", "ES384",
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn expected(pairs: &[(&str, serde_json::Value)]) -> BTreeMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn test_claim_match_is_and_with_glob_defaults() {
        let expected = expected(&[
            ("repository_owner_id", json!("123456")),
            ("ref", json!("refs/tags/v*")),
        ]);
        let claims = json!({
            "repository_owner_id": "123456",
            "ref": "refs/tags/v1.2.3"
        });
        assert!(claim_match(&expected, &claims).is_some());
        let wrong = json!({ "repository_owner_id": "999", "ref": "refs/tags/v1.2.3" });
        assert!(claim_match(&expected, &wrong).is_none());
        let nested = json!("refs/tags/v1/2/3");
        assert!(claim_match(&expected, &nested).is_none());
    }

    #[test]
    fn test_array_claims_match_any_element() {
        let expected = expected(&[("roles", json!("release-managers"))]);
        let claims = json!({ "roles": ["ci", "release-managers"] });
        assert!(claim_match(&expected, &claims).is_some());
        let denied = json!({ "roles": ["ci"] });
        assert!(claim_match(&expected, &denied).is_none());
    }

    #[test]
    fn test_missing_claim_never_matches() {
        let expected = expected(&[("roles", json!("release-managers"))]);
        assert!(claim_match(&expected, &json!({})).is_none());
    }

    #[test]
    fn test_algorithm_allowlist_rejects_none_and_hs() {
        for name in ALGORITHM_ALLOWLIST {
            assert!(!name.starts_with("HS"), "{name}");
            assert_ne!(name, "none");
        }
    }
}
