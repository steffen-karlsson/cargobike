//! GitHub webhook deliveries (F-51/F-46): signature verification
//! (HMAC-SHA256 over the raw body, constant-time compare) and the
//! normalisation of the events Cargobike acts on (`push` tag pushes,
//! `pull_request` close).

use cargobike_core::provider::ProviderError;
use cargobike_core::webhook::{NormalisedEvent, TagPush};
use secrecy::ExposeSecret as _;

use crate::signature::hmac_sha256_hex;

/// The GitHub delivery headers Cargobike requires (F-51).
const SIGNATURE_HEADER: &str = "x-hub-signature-256";
const EVENT_HEADER: &str = "x-github-event";

/// Verifies the delivery's signature against each configured secret and
/// normalises the event. GitHub sends `sha256=<hex>`; matching any one
/// secret accepts the delivery (rotation needs two secrets).
pub fn verify_and_normalise(
    headers: &[(&str, &str)],
    body: &[u8],
    secrets: &[secrecy::SecretString],
) -> Result<NormalisedEvent, ProviderError> {
    if secrets.is_empty() {
        return Err(ProviderError::Unsupported);
    }
    let signature = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(SIGNATURE_HEADER))
        .map(|(_, value)| value)
        .ok_or(ProviderError::NotFound(SIGNATURE_HEADER))?;
    let expected = signature
        .strip_prefix("sha256=")
        .ok_or_else(|| ProviderError::Request("the signature is not a sha256 digest".to_owned()))?;
    let verified = secrets
        .iter()
        .any(|secret| hmac_sha256_hex(secret.expose_secret().as_bytes(), body) == expected);
    if !verified {
        // A failed verification is a transport rejection, not an event
        // (F-51: the server answers 401 and FORGETS).
        return Err(ProviderError::Request(
            "the delivery's signature does not verify".to_owned(),
        ));
    }
    let event_name = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(EVENT_HEADER))
        .map(|(_, value)| value)
        .ok_or(ProviderError::NotFound(EVENT_HEADER))?;
    normalise(event_name, body)
}

/// F-46's normalisation over the delivery body.
pub fn normalise(event_name: &str, body: &[u8]) -> Result<NormalisedEvent, ProviderError> {
    match event_name {
        // F-54: tag pushes as release candidates.
        "push" => {
            let delivery: serde_json::Value =
                serde_json::from_slice(body).map_err(|failure| unreadable(&failure))?;
            let reference = delivery
                .get("ref")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if let Some(tag) = reference.strip_prefix("refs/tags/") {
                return Ok(NormalisedEvent::TagPush(TagPush {
                    tag: tag.to_owned(),
                    sha: delivery
                        .get("after")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    repository_id: delivery
                        .get("repository")
                        .and_then(|repo| repo.get("id"))
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or_default()
                        .to_string(),
                    sender: delivery
                        .get("sender")
                        .and_then(|sender| sender.get("login"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                }));
            }
            Ok(NormalisedEvent::Unrecognised {
                provider_event: event_name.to_owned(),
            })
        }
        // F-63: the event a release's wait was listening for.
        "pull_request" => {
            let delivery: serde_json::Value =
                serde_json::from_slice(body).map_err(|failure| unreadable(&failure))?;
            let action = delivery
                .get("action")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if action != "closed" {
                return Ok(NormalisedEvent::Unrecognised {
                    provider_event: format!("pull_request/{action}"),
                });
            }
            let pull = delivery.get("pull_request");
            let merged = pull
                .and_then(|p| p.get("merged"))
                .and_then(|m| m.as_bool())
                .unwrap_or(false);
            let Some(number) = pull
                .and_then(|p| p.get("number"))
                .and_then(serde_json::Value::as_u64)
            else {
                return Err(malformed("no pull request number"));
            };
            Ok(NormalisedEvent::ChangeRequestClosed { number, merged })
        }
        other => Ok(NormalisedEvent::Unrecognised {
            provider_event: other.to_owned(),
        }),
    }
}

fn unreadable(failure: &serde_json::Error) -> ProviderError {
    ProviderError::Request(format!("the delivery body is unreadable: {failure}"))
}

/// A structured delivery missing a required field (F-51's boundary).
fn malformed(reason: impl std::fmt::Display) -> ProviderError {
    ProviderError::Request(format!("the delivery body is malformed: {reason}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::SecretString;

    fn secret(value: &str) -> SecretString {
        SecretString::from(value.to_owned())
    }

    #[test]
    fn test_signature_verifies_and_rejects() {
        let body = b"{\"zen\":\"shaking\"}";
        let valid = format!("sha256={}", hmac_sha256_hex(b"sekrit", body));
        let headers = [
            ("X-Hub-Signature-256", valid.as_str()),
            ("X-GitHub-Event", "ping"),
        ];
        let verified = verify_and_normalise(&headers, body, &[secret("sekrit")]).expect("verifies");
        // An unacted-on event normalises to Unrecognised (F-56).
        assert!(matches!(verified, NormalisedEvent::Unrecognised { .. }));

        let forged: [(&str, String); 2] = [
            ("X-Hub-Signature-256", "sha256=0000".to_owned()),
            ("X-GitHub-Event", "ping".to_owned()),
        ];
        let failed = verify_and_normalise(
            &forged
                .iter()
                .map(|(k, v)| (*k, v.as_str()))
                .collect::<Vec<_>>(),
            body,
            &[secret("sekrit")],
        )
        .expect_err("rejects");
        assert!(failed.to_string().contains("does not verify"));
    }

    #[test]
    fn test_any_one_rotated_secret_accepts() {
        let body = b"{\"ref\":\"refs/tags/v2.0.0\",\"after\":\"abc\",\"repository\":{\"id\":77},\"sender\":{\"login\":\"ska\"}}";
        let old = format!("sha256={}", hmac_sha256_hex(b"old-secret", body));
        let new = format!("sha256={}", hmac_sha256_hex(b"new-secret", body));
        let headers = [
            ("X-Hub-Signature-256", old.as_str()),
            ("X-GitHub-Event", "push"),
        ];
        let event = verify_and_normalise(
            &headers,
            body,
            &[secret("new-secret"), secret("old-secret")],
        )
        .expect("one of the secrets verifies");
        match event {
            NormalisedEvent::TagPush(tag) => {
                assert_eq!(tag.tag, "v2.0.0");
                assert_eq!(tag.sha, "abc");
                assert_eq!(tag.repository_id, "77");
                assert_eq!(tag.sender, "ska");
            }
            other => panic!("expected a tag push, got {other:?}"),
        }

        let stale_headers = [
            ("X-Hub-Signature-256", new.as_str()),
            ("X-GitHub-Event", "push"),
        ];
        let event_new = verify_and_normalise(&stale_headers, body, &[secret("new-secret")])
            .expect("the new secret verifies alone");
        assert!(matches!(event_new, NormalisedEvent::TagPush(_)));
    }

    #[test]
    fn test_merged_and_closed_pull_requests_normalise() {
        let body = b"{\"action\":\"closed\",\"pull_request\":{\"number\":12,\"merged\":true}}";
        let merged = normalise("pull_request", body).expect("merged event");
        assert_eq!(
            merged,
            NormalisedEvent::ChangeRequestClosed {
                number: 12,
                merged: true
            }
        );

        let closed = normalise(
            "pull_request",
            b"{\"action\":\"closed\",\"pull_request\":{\"number\":13,\"merged\":false}}",
        )
        .expect("closed event");
        assert_eq!(
            closed,
            NormalisedEvent::ChangeRequestClosed {
                number: 13,
                merged: false
            }
        );

        let opened = normalise(
            "pull_request",
            b"{\"action\":\"opened\",\"pull_request\":{\"number\":14}}",
        )
        .expect("opened records");
        assert!(matches!(opened, NormalisedEvent::Unrecognised { .. }));
    }

    #[test]
    fn test_non_tag_pushes_are_unrecognised() {
        let event = normalise("push", b"{\"ref\":\"refs/heads/main\"}").expect("branch push");
        assert!(matches!(event, NormalisedEvent::Unrecognised { .. }));
        let event = normalise("issue_comment", b"{}").expect("unknown event");
        assert!(matches!(event, NormalisedEvent::Unrecognised { .. }));
    }
}
