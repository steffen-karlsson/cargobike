//! GitHub's delivery verification now lives in the core's shared
//! routine (`cargobike_core::webhook::normalise_github`), so the
//! provider and the engine's harness mock behave identically; this
//! module keeps the thin provider-facing alias.

pub use cargobike_core::webhook::normalise_github as verify_and_normalise;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature::hmac_sha256_hex;
    use cargobike_core::webhook::{NormalisedEvent, normalise};
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
        // An unacted-on event normalises to Unrecognised .
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
        let body = b"{\"action\":\"closed\",\"pull_request\":{\"number\":12,\"merged\":true,\"base\":{\"repo\":{\"id\":7777}}},\"sender\":{\"login\":\"ska\"}}";
        let merged = normalise("pull_request", body).expect("merged event");
        assert_eq!(
            merged,
            NormalisedEvent::ChangeRequestClosed {
                number: 12,
                merged: true,
                repository_id: "7777".to_owned(),
                sender: "ska".to_owned(),
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
                merged: false,
                // no repo in the payload: correlation errs on the empty side.
                repository_id: "0".to_owned(),
                sender: String::new(),
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
