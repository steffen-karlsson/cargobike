//! F-122's per-repo installation flow against a mocked GitHub: the
//! App-level client resolves the repository's installation, mints a
//! token scoped to that single repository (`repository_ids` + the
//! minimal permission subset), and the minted clients cache until a
//! minute before the token's expiry.

use cargobike_core::model::RepoRef;
use cargobike_core::provider::Provider as _;
use cargobike_provider_github::GithubProvider;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A far-future expiry: the cache must outlive the test.
const EXPIRES_AT: &str = "2099-01-01T00:00:00Z";

#[tokio::test]
async fn test_per_repo_calls_mint_a_repository_scoped_token() {
    let server = MockServer::start().await;

    // The repo→installation lookup (the app-authenticated GET).
    Mock::given(method("GET"))
        .and(path("/repositories/42/installation"))
        .respond_with(|_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 99,
                "account": {
                    "login": "my-org", "id": 7, "node_id": "O_kg", "type": "Organization",
                    "avatar_url": "https://x.invalid/a.png", "gravatar_id": "",
                    "url": "https://x.invalid/org", "html_url": "https://x.invalid/org",
                    "followers_url": "https://x.invalid/f", "following_url": "https://x.invalid/f",
                    "gists_url": "https://x.invalid/g", "starred_url": "https://x.invalid/s",
                    "subscriptions_url": "https://x.invalid/sub",
                    "organizations_url": "https://x.invalid/o", "repos_url": "https://x.invalid/r",
                    "events_url": "https://x.invalid/e", "received_events_url": "https://x.invalid/re",
                    "site_admin": false
                },
                "permissions": {},
                "events": [],
                "created_at": "2026-01-01T00:00:00Z",
            }))
        })
        .mount(&server)
        .await;

    // The scoped-token mint (the body's shape is asserted after the
    // flow from the recorded request).
    let mint_hits = std::sync::Arc::new(std::sync::Mutex::new(0_u32));
    let mints = std::sync::Arc::clone(&mint_hits);
    Mock::given(method("POST"))
        .and(path("/app/installations/99/access_tokens"))
        .respond_with(move |_: &wiremock::Request| {
            *mints.lock().expect("the mint counter") += 1;
            ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "token": "ghs_minted_for_42",
                "expires_at": EXPIRES_AT,
                "permissions": {},
                "repository_selection": "selected",
            }))
        })
        .mount(&server)
        .await;

    // The repository read the provider's call makes with the minted
    // token (the op under test: the path-less probe's `get_repo_path`).
    Mock::given(method("GET"))
        .and(path("/repositories/42"))
        .respond_with(|_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 42,
                "name": "my-service",
                "full_name": "my-org/my-service",
                "default_branch": "main",
                "url": "https://x.invalid/repo",
            }))
        })
        .mount(&server)
        .await;

    let app = octocrab::Octocrab::builder()
        .personal_token("app-level".to_owned())
        .base_uri(server.uri())
        .expect("the mocked base uri sets")
        .build()
        .expect("the app client builds");
    let provider = GithubProvider::from_app_client(app, Some(&server.uri()));
    let repo = RepoRef::new("github", "42");

    // The provider's truth answers through the MINTED token.
    let path = provider
        .get_repo_path(&repo)
        .await
        .expect("the repository path resolves");
    assert_eq!(path, "my-org/my-service");
    assert_eq!(*mint_hits.lock().expect("the mint counter"), 1);
    let mints = server
        .received_requests()
        .await
        .expect("the request log")
        .into_iter()
        .filter(|request| request.url.path() == "/app/installations/99/access_tokens")
        .collect::<Vec<_>>();
    let mint_body: serde_json::Value = mints[0].body_json().expect("the mint body");
    assert_eq!(
        mint_body["repository_ids"],
        serde_json::json!([42]),
        "the mint pins the single repository: {mint_body}"
    );
    assert!(
        mint_body["permissions"]
            .as_object()
            .is_some_and(|permissions| permissions.len() >= 6),
        "the mint carries the minimal permission subset: {mint_body}"
    );
    let repository_reads = server
        .received_requests()
        .await
        .expect("the request log")
        .into_iter()
        .filter(|request| request.url.path() == "/repositories/42")
        .collect::<Vec<_>>();
    assert_eq!(repository_reads.len(), 1, "one repository read happened");
    assert_eq!(
        repository_reads[0]
            .headers
            .get("authorization")
            .map(|value| value.to_str().unwrap()),
        Some("Bearer ghs_minted_for_42"),
        "the repository read rode the repository-scoped token"
    );

    // The cache: the second call reuses the minted client — no second
    // mint, no second installation lookup.
    let again = provider
        .get_repo_path(&repo)
        .await
        .expect("the cached client serves the second call");
    assert_eq!(again, "my-org/my-service");
    assert_eq!(
        *mint_hits.lock().expect("the mint counter"),
        1,
        "the fresh cache means no re-mint"
    );
}
