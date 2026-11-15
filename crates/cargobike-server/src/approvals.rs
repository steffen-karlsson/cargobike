//! The approvals surface's server half (F-59/F-96, Phase 5.5): the
//! decision store and the authorisation policy the endpoint runs. The
//! durable follow-through (the signal send + the event) is the
//! engine's `cargobike.approval.v1` workflow, which the endpoint
//! starts once the decision crossed the threshold — or is a reject.
//!
//! The counting (F-96's `required` DISTINCT principals) happens HERE,
//! not in the workflow: the workflow receives one decision, the
//! parked single-recv wait consumes one wake.

use sqlx::PgPool;
use uuid::Uuid;

use crate::config::{ApplicationEntry, ApprovalPolicy, PrincipalSelector};

/// One recorded decision (the store's row shape).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ApprovalRecord {
    /// The decision's release.
    pub release_id: Uuid,
    /// The environment the wait parks in.
    pub environment: String,
    /// The parked `wait: approval` step.
    pub step_id: String,
    /// The attempt ordinal the submission observed (approvals never
    /// carry over to a retry).
    pub attempt: i32,
    /// The distinct principal (F-96's counting unit).
    pub principal: String,
    /// The CR head SHA the decision binds to (NULL: no CR in flow).
    pub head_sha: Option<String>,
    /// `approve` or `reject`.
    pub decision: String,
    /// Display name at submission time.
    pub approver: String,
    /// Optional comment.
    pub comment: Option<String>,
}

/// The approvals store: one row per principal per attempt, the
/// threshold count reads the current attempt's approving principals.
#[derive(Clone)]
pub struct ApprovalRepository {
    pool: PgPool,
}

impl ApprovalRepository {
    /// A repository over the pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Records one decision. `Ok(None)` = the principal already
    /// decided this step of this attempt (the caller answers 409).
    pub async fn record(
        &self,
        record: &ApprovalRecord,
        at: sqlx::types::time::OffsetDateTime,
    ) -> Result<Option<ApprovalRecord>, String> {
        // `RETURNING 1` is a postgres int (i32), not a bigint: the
        // scalar type must match or the decode fails AFTER the insert
        // already landed.
        let inserted = sqlx::query_scalar::<_, i32>(
            "INSERT INTO approvals \
             (release_id, environment, step_id, attempt, principal, head_sha, \
              decision, approver, comment, at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (release_id, environment, step_id, attempt, principal) \
             DO NOTHING RETURNING 1",
        )
        .bind(record.release_id)
        .bind(&record.environment)
        .bind(&record.step_id)
        .bind(record.attempt)
        .bind(&record.principal)
        .bind(&record.head_sha)
        .bind(&record.decision)
        .bind(&record.approver)
        .bind(&record.comment)
        .bind(at)
        .fetch_optional(&self.pool)
        .await
        .map_err(|failure| format!("the approval insert failed: {failure}"))?;
        Ok((inserted.is_some()).then(|| record.clone()))
    }

    /// The DISTINCT approving principals of the current attempt (the
    /// threshold's count; head-SHA-bound per F-96).
    pub async fn approving_count(
        &self,
        release_id: Uuid,
        environment: &str,
        step_id: &str,
        attempt: i32,
        head_sha: Option<&str>,
    ) -> Result<u32, String> {
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(DISTINCT principal) FROM approvals \
             WHERE release_id = $1 AND environment = $2 AND step_id = $3 \
             AND attempt = $4 AND decision = 'approve' AND head_sha IS NOT DISTINCT FROM $5",
        )
        .bind(release_id)
        .bind(environment)
        .bind(step_id)
        .bind(attempt)
        .bind(head_sha)
        .fetch_one(&self.pool)
        .await
        .map_err(|failure| format!("the approval count failed: {failure}"))?;
        u32::try_from(count).map_err(|_| "the approval count overflowed".to_owned())
    }
}

/// Why an approval submission is refused (the endpoint's 403 shapes).
#[derive(Debug, PartialEq, Eq)]
pub enum ApprovalRefusal {
    /// The application declares no approval policy for the
    /// environment (fail closed: no policy = nobody may approve).
    NoPolicy,
    /// The caller matches no approver selector.
    NotAnApprover,
    /// The caller IS the release's creator and
    /// `allow_self_approval` is false (the default).
    SelfApproval,
    /// An `api_key` approver while `allow_machine_approvers` is off
    /// (the default), or a bootstrap key.
    MachineApprover,
}

/// The approvers policy (F-96): whether the CALLER may submit the
/// decision. The grant check (`release:approve`, F-99) is the
/// endpoint's separate first gate.
pub fn authorize_approval(
    policy: &ApprovalPolicy,
    caller: &crate::auth::AuthedCaller,
    app: &ApplicationEntry,
    environment: &str,
    release_actor: Option<(&str, &str)>,
) -> Result<(), ApprovalRefusal> {
    // The selector list: a match counts (any-of).
    let matched = policy.approvers.iter().any(|selector| match selector {
        PrincipalSelector::Oidc { oidc } => *oidc == caller.origin,
        PrincipalSelector::ApiKey { api_key } => {
            *api_key == caller.origin && policy.allow_machine_approvers && !caller.bootstrap
        }
    });
    if !matched {
        return Err(ApprovalRefusal::NotAnApprover);
    }
    // Self-approval: the release's own creator may not wave it
    // through (the default; the document's stamped actor is the
    // VERIFIED creator, never a body-supplied value).
    if !policy.allow_self_approval {
        if let Some((issuer, subject)) = release_actor {
            if issuer == caller.issuer && subject == caller.subject {
                return Err(ApprovalRefusal::SelfApproval);
            }
        }
    }
    // The policy argument came from the application's environment
    // entry (the endpoint fetches it; a missing entry is the
    // `NoPolicy` refusal BEFORE this function runs).
    let _ = (app, environment);
    Ok(())
}

/// The caller's distinct-principal identity (F-96's counting unit):
/// `issuer/subject` for OIDC, `api-key/<name>` for keys.
pub fn principal_of(caller: &crate::auth::AuthedCaller) -> String {
    if caller.issuer == "api-key" {
        format!("api-key/{}", caller.subject)
    } else {
        format!("{}/{}", caller.issuer, caller.subject)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(origin: &str, issuer: &str, subject: &str) -> crate::auth::AuthedCaller {
        crate::auth::AuthedCaller {
            origin: origin.to_owned(),
            issuer: issuer.to_owned(),
            subject: subject.to_owned(),
            display_name: subject.to_owned(),
            grants: vec![],
            repository_id: None,
            repository_owner_id: None,
            sha_claim: None,
            ci: None,
            bootstrap: false,
        }
    }

    fn app_with(approval: serde_json::Value) -> ApplicationEntry {
        // The registry's resolved shape comes from the raw file entry —
        // the same resolution the boot runs.
        let raw: crate::config::RawApplicationEntry = serde_json::from_value(serde_json::json!({
            "name": "an-app",
            "source": { "provider": "github", "id": "1" },
            "template": "service@1",
            "environments": {
                "preview": { "approval": approval },
                "production": {},
            }
        }))
        .expect("the raw application entry parses");
        let mut config: crate::config::Config = serde_yaml_ng::from_str(
            "server: {}\ndatabase: { url: postgres://x }\nauth: {}\napplications: []\n",
        )
        .expect("the minimal config");
        config.raw_applications = vec![raw];
        cargobike_core::groups::resolve(&mut config).expect("resolves");
        config.applications.pop().expect("the resolved entry")
    }

    fn oidc_policy() -> ApprovalPolicy {
        serde_json::from_value(serde_json::json!({
            "approvers": [{ "oidc": "trust-a" }]
        }))
        .expect("the policy parses")
    }

    fn machine_policy(machine: bool) -> ApprovalPolicy {
        serde_json::from_value(serde_json::json!({
            "allow_machine_approvers": machine,
            "approvers": [{ "api_key": "key-a" }]
        }))
        .expect("the policy parses")
    }

    #[test]
    fn test_principal_of_distinguishes_the_key_shape() {
        let oidc = caller("trust-a", "https://iss", "alice");
        assert_eq!(principal_of(&oidc), "https://iss/alice");
        let key = caller("key-a", "api-key", "key-a");
        assert_eq!(principal_of(&key), "api-key/key-a");
    }

    #[test]
    fn test_only_matching_selectors_approve() {
        let approvers = oidc_policy();
        let app = app_with(serde_json::json!({ "approvers": [{ "oidc": "trust-a" }] }));
        let matched = caller("trust-a", "https://iss", "alice");
        assert_eq!(
            authorize_approval(&approvers, &matched, &app, "preview", None),
            Ok(())
        );
        let stranger = caller("trust-b", "https://iss", "mallory");
        assert_eq!(
            authorize_approval(&approvers, &stranger, &app, "preview", None),
            Err(ApprovalRefusal::NotAnApprover)
        );
    }

    #[test]
    fn test_the_creator_may_not_self_approve_by_default() {
        let approvers = oidc_policy();
        let app = app_with(serde_json::json!({ "approvers": [{ "oidc": "trust-a" }] }));
        let creator = caller("trust-a", "https://iss", "alice");
        assert_eq!(
            authorize_approval(
                &approvers,
                &creator,
                &app,
                "preview",
                Some(("https://iss", "alice"))
            ),
            Err(ApprovalRefusal::SelfApproval)
        );
        let permissive_json = serde_json::json!({ "allow_self_approval": true, "approvers": [{ "oidc": "trust-a" }] });
        let permissive: ApprovalPolicy =
            serde_json::from_value(permissive_json.clone()).expect("the policy parses");
        let app = app_with(permissive_json);
        assert_eq!(
            authorize_approval(
                &permissive,
                &creator,
                &app,
                "preview",
                Some(("https://iss", "alice"))
            ),
            Ok(())
        );
    }

    #[test]
    fn test_machine_approvers_are_off_by_default() {
        let off = machine_policy(false);
        let app = app_with(serde_json::json!({
            "allow_machine_approvers": false,
            "approvers": [{ "api_key": "key-a" }]
        }));
        let key = caller("key-a", "api-key", "key-a");
        assert_eq!(
            authorize_approval(&off, &key, &app, "preview", None),
            Err(ApprovalRefusal::NotAnApprover)
        );
        let machines = machine_policy(true);
        let app = app_with(serde_json::json!({
            "allow_machine_approvers": true,
            "approvers": [{ "api_key": "key-a" }]
        }));
        assert_eq!(
            authorize_approval(&machines, &key, &app, "preview", None),
            Ok(())
        );
        // The bootstrap key never approves (F-99b's trust boundary).
        let mut bootstrap = caller("key-a", "api-key", "key-a");
        bootstrap.bootstrap = true;
        assert_eq!(
            authorize_approval(&machines, &bootstrap, &app, "preview", None),
            Err(ApprovalRefusal::NotAnApprover)
        );
    }
}
