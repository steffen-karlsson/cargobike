//! GitHub Actions auto-detection (PRD §9.7, F-130, §2.2): no
//! config file needed when the Actions OIDC environment is present —
//! the CLI exchanges the workflow's request token for an audience
//! token, and carries the CI context on commands that create releases.

use std::env;

/// The CI facts the CLI auto-detects (F-83's actor annotation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiContext {
    /// `GITHUB_REPOSITORY` (owner/name).
    pub repository: String,
    /// `GITHUB_SHA` (the triggering commit).
    pub sha: String,
    /// `GITHUB_RUN_ID` (the workflow run).
    pub run_id: String,
}

impl CiContext {
    /// Reads the CI environment; `None` outside Actions.
    pub fn from_environment() -> Option<Self> {
        let repository = env::var("GITHUB_REPOSITORY").ok()?;
        let sha = env::var("GITHUB_SHA").ok()?;
        let run_id = env::var("GITHUB_RUN_ID").ok()?;
        if repository.is_empty() || sha.is_empty() || run_id.is_empty() {
            return None;
        }
        Some(Self {
            repository,
            sha,
            run_id,
        })
    }
}

/// The Actions OIDC request environment (both variables are required).
pub fn oidc_request_environment() -> Option<(String, String)> {
    let url = env::var("ACTIONS_ID_TOKEN_REQUEST_URL").ok()?;
    let token = env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN").ok()?;
    if url.is_empty() || token.is_empty() {
        return None;
    }
    Some((url, token))
}

/// Whether auth type `github-actions` is usable right now.
pub fn actions_detected() -> bool {
    oidc_request_environment().is_some()
}

/// The audience an Actions token requests (CARGOBIKE_AUDIENCE or the
/// built-in default).
pub fn audience() -> String {
    env::var("CARGOBIKE_AUDIENCE")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "cargobike".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_actions_detection_reads_both_oidc_variables() {
        unsafe {
            env::set_var(
                "ACTIONS_ID_TOKEN_REQUEST_URL",
                "https://token.actions.githubusercontent.com",
            );
        }
        unsafe { env::remove_var("ACTIONS_ID_TOKEN_REQUEST_TOKEN") };
        assert!(!actions_detected());
        unsafe { env::set_var("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "tok") };
        assert!(actions_detected());
        unsafe {
            env::remove_var("ACTIONS_ID_TOKEN_REQUEST_URL");
            env::remove_var("ACTIONS_ID_TOKEN_REQUEST_TOKEN");
        }
        assert!(!actions_detected());
    }

    #[test]
    fn test_ci_context_carries_the_six_facts() {
        unsafe {
            env::set_var("GITHUB_REPOSITORY", "org/repo");
            env::set_var("GITHUB_SHA", "abc123");
            env::set_var("GITHUB_RUN_ID", "42");
        }
        let context = CiContext::from_environment().expect("detected");
        assert_eq!(context.repository, "org/repo");
        assert_eq!(context.sha, "abc123");
        assert_eq!(context.run_id, "42");
        unsafe {
            env::remove_var("GITHUB_REPOSITORY");
            env::remove_var("GITHUB_SHA");
            env::remove_var("GITHUB_RUN_ID");
        }
        assert!(CiContext::from_environment().is_none());
    }

    #[test]
    fn test_audience_defaults_to_cargobike() {
        unsafe { env::remove_var("CARGOBIKE_AUDIENCE") };
        assert_eq!(audience(), "cargobike");
        unsafe { env::set_var("CARGOBIKE_AUDIENCE", "other") };
        assert_eq!(audience(), "other");
        unsafe { env::remove_var("CARGOBIKE_AUDIENCE") };
    }
}
