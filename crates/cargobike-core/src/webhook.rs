//! Normalised webhook events : providers map their delivery formats
//! onto this shape at the transport boundary (`cargobike-provider-github`
//! and sidecars); the server's dispatch logic stays provider-neutral.

use serde::{Deserialize, Serialize};

/// A webhook delivery reduced to what Cargobike acts on .
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum NormalisedEvent {
    /// A tag push: release creation candidates (`event: tag` triggers, ).
    TagPush(TagPush),
    /// A change request reached a customer state (the correlation).
    ChangeRequestClosed {
        /// CR number.
        number: u64,
        /// State the CR reached (`merged` or `closed`; ).
        merged: bool,
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
    /// `versioning.tag_format` in the server, ).
    pub tag: String,
    /// Commit SHA the tag points at, verified against the provider .
    pub sha: String,
    /// Repository the push landed in (the immutable ID; ).
    pub repository_id: String,
    /// Pusher display name (the actor; the reconciler uses the same).
    pub sender: String,
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
}
