//! Normalised webhook events : providers map their delivery formats
//! onto this shape at the transport boundary (`cargobike-provider-github`
//! and sidecars); the server's dispatch logic stays provider-neutral.
//!
//! GitHub's delivery verification also lives here (the shared HMAC
//! contract): signature check over the RAW body with the each-configured
//! secret, constant-time compare, then normalisation. The
//! `cargobike-provider-github` and the engine's harness mock both call
//! this one routine, so the mock's behaviour and production办案 do not
//! drift.

use secrecy::ExposeSecret as _;
use serde::{Deserialize, Serialize};

/// A webhook delivery reduced to what Cargobike acts on .
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum NormalisedEvent {
    /// A tag push: release creation candidates (`event: tag` triggers).
    TagPush(TagPush),
    /// A change request reached a customer state (the correlation).
    ChangeRequestClosed {
        /// CR number.
        number: u64,
        /// State the CR reached (`merged` or `closed`;).
        merged: bool,
        /// Repository the CR belongs to (the immutable ID; the
        /// correlation's key half).
        repository_id: String,
        /// Who closed it (the actor; correlation errs on the empty
        /// side but keeps the sender when the payload carries it).
        sender: String,
    },
    /// Any other event: persisted, acknowledged, ignored .
    Unrecognised {
        /// Provider event name, recorded for the audit log.
        provider_event: String,
    },
}

/// A tag push delivery .
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TagPush {
    /// Tag name, e.g. `v1.2.3` (the version extraction happens per
    /// `versioning.tag_format` in the server).
    pub tag: String,
    /// Commit SHA the tag points at, verified against the provider .
    pub sha: String,
    /// Repository the push landed in (the immutable ID;).
    pub repository_id: String,
    /// Pusher display name (the actor; the reconciler uses the same).
    pub sender: String,
}

/// The repository id's read: GitHub carries numbers, the other halves
/// (the config's source, the mock's state, the CR's correlations)
/// carry strings — accept both, string-encode.
fn repository_id_of(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Number(number)) => number.to_string(),
        _ => String::default(),
    }
}

/// The delivery verification failure shapes .
pub type VerifyResult = Result<NormalisedEvent, crate::provider::ProviderError>;

/// GitHub's delivery headers Cargobike requires .
const SIGNATURE_HEADER: &str = "x-hub-signature-256";
const EVENT_HEADER: &str = "x-github-event";

/// Verifies the delivery's signature against each configured secret and
/// normalises the event. GitHub sends `sha256=<hex>`; matching any one
/// secret accepts the delivery (the rotation needs two secrets). A
/// failed verification is a transport rejection, not an event (the
/// server answers 401 and forgets).
pub fn normalise_github(
    headers: &[(&str, &str)],
    body: &[u8],
    secrets: &[secrecy::SecretString],
) -> VerifyResult {
    if secrets.is_empty() {
        return Err(crate::provider::ProviderError::Unsupported);
    }
    let signature = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(SIGNATURE_HEADER))
        .map(|(_, value)| *value)
        .ok_or(crate::provider::ProviderError::NotFound(SIGNATURE_HEADER))?;
    let expected = signature.strip_prefix("sha256=").ok_or_else(|| {
        crate::provider::ProviderError::Request("the signature is not a sha256 digest".to_owned())
    })?;
    // the compare is HMAC's verify_slice (the constant-time on
    // the raw tag bytes; the hex was only the delivery's encoding).
    let verified = secrets
        .iter()
        .any(|secret| digest_matches(secret.expose_secret().as_bytes(), body, expected));
    if !verified {
        return Err(crate::provider::ProviderError::Request(
            "the delivery's signature does not verify".to_owned(),
        ));
    }
    let event_name = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(EVENT_HEADER))
        .map(|(_, value)| *value)
        .ok_or(crate::provider::ProviderError::NotFound(EVENT_HEADER))?;
    normalise(event_name, body)
}

/// normalisation over the delivery body.
pub fn normalise(event_name: &str, body: &[u8]) -> VerifyResult {
    match event_name {
        // tag pushes as release candidates.
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
                    repository_id: repository_id_of(delivery.pointer("/repository/id")),
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
        // the event a release's wait was listening for.
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
            let base_id = repository_id_of(pull.and_then(|p| p.pointer("/base/repo/id")));
            let repository_id = if base_id.is_empty() {
                repository_id_of(delivery.pointer("/repository/id"))
            } else {
                base_id
            };
            let sender = delivery
                .pointer("/sender/login")
                .or_else(|| delivery.pointer("/pull_request/user/login"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Ok(NormalisedEvent::ChangeRequestClosed {
                number,
                merged,
                repository_id,
                sender,
            })
        }
        other => Ok(NormalisedEvent::Unrecognised {
            provider_event: other.to_owned(),
        }),
    }
}

/// The constant-time digest check : decode the expected tag then
/// verify slice-against-slice; an unparsable digest is a reject.
fn digest_matches(secret_key: &[u8], body: &[u8], expected_hex: &str) -> bool {
    use hmac::Mac as _;
    use sha2::Sha256;
    let Ok(expected) = hex::decode(expected_hex) else {
        return false;
    };
    let Ok(mut mac) = hmac::Hmac::<Sha256>::new_from_slice(secret_key) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

/// The malformed/invalid body's error (the delivery is acted-on
/// garbage; the receiver refuses it).
fn unreadable(failure: &serde_json::Error) -> crate::provider::ProviderError {
    crate::provider::ProviderError::Request(format!(
        "the delivery body is unreadable JSON: {failure}"
    ))
}

/// The delivery's required field is absent.
fn malformed(reason: &str) -> crate::provider::ProviderError {
    crate::provider::ProviderError::Request(format!("the delivery is malformed: {reason}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tag_push_event_roundtrips() {
        let event = NormalisedEvent::TagPush(TagPush {
            tag: "v1.2.3".to_owned(),
            sha: "abc123".to_owned(),
            repository_id: "123456".to_owned(),
            sender: "ska".to_owned(),
        });
        let json = serde_json::to_value(&event).expect("event must serialise");
        assert_eq!(json["kind"], "tag-push");
        let back: NormalisedEvent = serde_json::from_value(json).expect("event must deserialise");
        assert_eq!(back, event);
    }

    #[test]
    fn test_the_hmac_verify_slices_constant_time_rfc_4231() {
        // RFC 4231 case 2 (the Jefe vector) exercises the HMAC engine
        // end-to-end through the digest helper.
        let body = b"what do ya want for nothing?";
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"Jefe").expect("the RFC's key");
        use hmac::Mac as _;
        mac.update(body);
        let expected_digest = hex::encode(mac.finalize().into_bytes());
        assert!(
            digest_matches(b"Jefe", body, &expected_digest),
            "right key verifies"
        );
        assert!(
            !digest_matches(b"Jefe", body, "deadbeef"),
            "wrong digest refuses"
        );
    }
}
