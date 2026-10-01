//! The GitHub provider's full lifecycle against a mocked GitHub REST
//! API (the PRD's 4.9 provider half): every `Provider` operation the
//! engine drives a real release through, over wiremock — branch
//! creation, the fused edit+commit with its no-op replay, change
//! requests (create with the find-by-head first pass, get, close),
//! labels, comments, commit statuses, branch/tag protection checks,
//! auto-merge, tag and release creation, and the branch delete.
//!
//! The fixture is a small stateful GitHub: file texts live in a map the
//! tree-create mock stamps, refs track their tips, and the open pull
//! map feeds both the find-by-head and the list-open routes.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use cargobike_core::model::{CrState, RepoRef};
use cargobike_core::provider::{CommitStatus, Edit, Provider};
use cargobike_provider_github::GithubProvider;

/// The immutable repository id the fixtures' RepoRef carries.
const REPO_ID: &str = "42";
/// The owner/name the mocked repository answers with.
const REPO_PATH: &str = "my-org/my-service";
/// The owner/name pair as octocrab's pulls/issues routes spell it.
const REPO_FULL_PATH: &str = "/repos/my-org/my-service";
/// The release branch a `commit-files` step creates.
const RELEASE_BRANCH: &str = "cargobike/my-service/stage/rel-1";
/// The personal token the mocked API accepts.
const TOKEN: &str = "cb-wiremock-personal-token";
/// The head SHA the mocked pull request carries.
const READY_HEAD_SHA: &str = "release-branch-tip";

/// The yaml the mocked repository serves before the release's edit.
const PRE_EDIT_YAML: &str = "image:\n  tag: 0.0.0\n";
/// The file the release's commit-files step applies.
const THE_FILE: &str = "apps/stage/manifest.yaml";

/// Mutable fixture state shared between the mocks: committed file
/// texts, branch tips, open pulls by number, and the commit counter the
/// no-op replay assertions read.
#[derive(Default)]
struct Fixture {
    manifests: Arc<Mutex<BTreeMap<String, String>>>,
    branch_tips: Arc<Mutex<BTreeMap<String, String>>>,
    pulls: Arc<Mutex<BTreeMap<u64, serde_json::Value>>>,
    commits: Arc<AtomicU64>,
}

impl Fixture {
    /// The branch tip (the seed when unset).
    fn tip(&self, branch: &str) -> String {
        self.branch_tips
            .lock()
            .expect("the ref lock")
            .get(branch)
            .cloned()
            .unwrap_or_else(|| "main-seed-sha".to_owned())
    }
}

/// A debugging URL the Url-typed model fields require.
fn fake_url(suffix: &str) -> String {
    format!("https://api.github.com/fake/{suffix}")
}

/// A GET ref response for a tip.
fn ref_json(sha: &str) -> serde_json::Value {
    serde_json::json!({
        "ref": "refs/heads/the-branch",
        "node_id": "REF1",
        "url": fake_url("ref"),
        "object": { "type": "commit", "sha": sha, "url": fake_url("commit") }
    })
}

/// A pull request body in the shape octocrab's model parses.
fn pull_json(number: u64, state: &str, merged: bool, head_sha: &str) -> serde_json::Value {
    serde_json::json!({
        "id": 1000 + number,
        "number": number,
        "url": fake_url("pull"),
        "node_id": format!("PR_{number}"),
        "html_url": format!("https://github.com/{REPO_PATH}/pull/{number}"),
        "state": state,
        "merged": merged,
        "title": "Release my-service 1.2.3 to stage",
        "head": { "label": "my-org:cargobike", "ref": RELEASE_BRANCH, "sha": head_sha },
        "base": { "label": "my-org:main", "ref": "main", "sha": "base-sha" },
    })
}

/// The branch protection response: reviews are required once.
fn branch_protection_json() -> serde_json::Value {
    serde_json::json!({
        "required_pull_request_reviews": {
            "required_approving_review_count": 1,
            "dismiss_stale_reviews": true,
        }
    })
}

/// The ruleset list: one ruleset activates tags for `~ALL`.
fn rulesets_json(include: &[&str]) -> serde_json::Value {
    serde_json::json!([{
        "id": 1,
        "name": "protect-tags",
        "target": "tag",
        "source": REPO_PATH,
        "enforcement": "active",
        "rules": [],
        "conditions": {
            "ref_name": {
                "include": include,
                "exclude": [],
            }
        }
    }])
}

/// A release object (the `create_release` response).
fn release_json() -> serde_json::Value {
    serde_json::json!({
        "id": 88,
        "node_id": "REL_88",
        "tag_name": "v1.2.3",
        "name": "Release 1.2.3",
        "body": "the release body",
        "draft": false,
        "prerelease": false,
        "assets": [],
        "created_at": "2026-10-01T00:00:00Z",
        "published_at": "2026-10-01T00:00:00Z",
        "url": fake_url("release"),
        "html_url": fake_url("release"),
        "assets_url": fake_url("assets"),
        "upload_url": fake_url("upload"),
        "tarball_url": fake_url("tar"),
        "zipball_url": fake_url("zip"),
        "target_commitish": "main",
    })
}

/// The provider over the mocked GitHub, wired to the fixture.
async fn fixture_provider(fixture: Arc<Fixture>) -> (GithubProvider, wiremock::MockServer) {
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let builder = octocrab::Octocrab::builder().personal_token(TOKEN.to_owned());
    let builder = builder
        .base_uri(server.uri())
        .expect("the mocked base uri sets");
    let client = builder.build().expect("the test client builds");
    let provider = GithubProvider::from_octocrab(client);

    // Repository metadata (by immutable id).
    Mock::given(method("GET"))
        .and(path(format!("/repositories/{REPO_ID}")))
        .respond_with(|_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 42,
                "name": "my-service",
                "full_name": REPO_PATH,
                "default_branch": "main",
                "url": fake_url("repo"),
            }))
        })
        .mount(&server)
        .await;

    // Git refs: read tips (404 until created), create, update, delete;
    // tag objects.
    Mock::given(method("GET"))
        .and(path(format!("/repositories/{REPO_ID}/git/ref/heads/main")))
        .respond_with(move |_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(ref_json("main-seed-sha"))
        })
        .mount(&server)
        .await;
    let tips = Arc::clone(&fixture.branch_tips);
    Mock::given(method("GET"))
        .and(path(format!(
            "/repositories/{REPO_ID}/git/ref/heads/{RELEASE_BRANCH}"
        )))
        .respond_with(move |_: &wiremock::Request| {
            let tips = tips.lock().expect("the ref lock");
            let Some(sha) = tips.get(RELEASE_BRANCH) else {
                return ResponseTemplate::new(404).set_body_json(
                    serde_json::json!({ "message": "Not Found", "status": 404 }),
                );
            };
            ResponseTemplate::new(200).set_body_json(ref_json(sha))
        })
        .mount(&server)
        .await;
    let tips = Arc::clone(&fixture.branch_tips);
    Mock::given(method("POST"))
        .and(path(format!("/repositories/{REPO_ID}/git/refs")))
        .respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = req.body_json().unwrap_or_default();
            let branch = body["ref"]
                .as_str()
                .and_then(|value| value.strip_prefix("refs/heads/"))
                .unwrap_or_default()
                .to_owned();
            let from_sha = body["sha"].as_str().unwrap_or_default().to_owned();
            tips.lock()
                .expect("the ref lock")
                .insert(branch, from_sha);
            ResponseTemplate::new(201).set_body_json(ref_json("new-branch-sha"))
        })
        .mount(&server)
        .await;
    let tips = Arc::clone(&fixture.branch_tips);
    Mock::given(method("PATCH"))
        .and(path(format!(
            "/repositories/{REPO_ID}/git/refs/heads/{RELEASE_BRANCH}"
        )))
        .respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = req.body_json().unwrap_or_default();
            let moved_to = body["sha"].as_str().unwrap_or_default().to_owned();
            tips.lock()
                .expect("the ref lock")
                .insert(RELEASE_BRANCH.to_owned(), moved_to.clone());
            ResponseTemplate::new(200).set_body_json(ref_json(&moved_to))
        })
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!(
            "/repositories/{REPO_ID}/git/refs/heads/{RELEASE_BRANCH}"
        )))
        .respond_with(|_: &wiremock::Request| ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repositories/{REPO_ID}/git/tags")))
        .respond_with(|_: &wiremock::Request| {
            ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "tag": "v1.2.3",
                "name": "v1.2.3",
                "url": fake_url("tag"),
                "node_id": "TAG1",
                "sha": "tag-object-sha",
                "message": "release v1.2.3",
                "verifier": null,
                "tagger": {
                    "name": "cargobike",
                    "email": "releases@example.com",
                    "date": "2026-10-01T00:00:00Z"
                },
                "commit": { "sha": "tagged-sha", "url": fake_url("commit") },
                "zipball_url": fake_url("zip"),
                "tarball_url": fake_url("tar"),
            }))
        })
        .mount(&server)
        .await;

    // Content reads: serve the committed text from the manifest map.
    let manifests = Arc::clone(&fixture.manifests);
    Mock::given(method("GET"))
        .and(path(format!("/repositories/{REPO_ID}/contents/{THE_FILE}")))
        .respond_with(move |_: &wiremock::Request| {
            let manifests = manifests.lock().expect("the manifest lock");
            let text = manifests
                .get(THE_FILE)
                .cloned()
                .unwrap_or_else(|| PRE_EDIT_YAML.to_owned());
            let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
            drop(manifests);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "manifest.yaml",
                "path": THE_FILE,
                "sha": "blob-sha",
                "encoding": "base64",
                "content": encoded,
                "size": encoded.len(),
                "url": fake_url("content"),
                "html_url": fake_url("content"),
                "git_url": fake_url("content"),
                "download_url": fake_url("content"),
                "_links": {
                    "self": fake_url("content"),
                    "git": fake_url("content"),
                    "html": fake_url("content"),
                },
                "type": "file",
            }))
        })
        .mount(&server)
        .await;

    // Trees: stamp the entries' contents (the committed state).
    let manifests = Arc::clone(&fixture.manifests);
    Mock::given(method("POST"))
        .and(path(format!("/repositories/{REPO_ID}/git/trees")))
        .respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = req.body_json().unwrap_or_default();
            let entries = body
                .get("tree")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut manifests = manifests.lock().expect("the manifest lock");
            for entry in entries {
                let Some(stamped_path) =
                    entry.get("path").and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                let Some(content) =
                    entry.get("content").and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                manifests.insert(stamped_path.to_owned(), content.to_owned());
            }
            drop(manifests);
            ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "sha": "tree-sha",
                "url": fake_url("tree"),
                "truncated": false,
                "tree": [],
            }))
        })
        .mount(&server)
        .await;

    // Commits.
    let commits = Arc::clone(&fixture.commits);
    Mock::given(method("POST"))
        .and(path(format!("/repositories/{REPO_ID}/git/commits")))
        .respond_with(move |_: &wiremock::Request| {
            let sha = format!("commit-sha-{}", commits.fetch_add(1, Ordering::SeqCst) + 1);
            ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "sha": sha,
                "node_id": "C1",
                "url": fake_url("commit"),
                "html_url": fake_url("commit"),
                "message": "commit message",
                "tree": { "sha": "tree-sha", "url": fake_url("tree") },
                "parents": [],
                "comments_url": fake_url("comments"),
                "author": { "name": "cargobike", "email": "releases@example.com" },
                "committer": { "name": "cargobike", "email": "releases@example.com" },
                "verification": { "verified": false, "reason": "unsigned" },
            }))
        })
        .mount(&server)
        .await;

    // Pull requests: create, list, get, close-via-patch.
    let pulls = Arc::clone(&fixture.pulls);
    Mock::given(method("POST"))
        .and(path(format!("{REPO_FULL_PATH}/pulls")))
        .respond_with(move |req: &wiremock::Request| {
            let _: serde_json::Value = req.body_json().unwrap_or_default();
            let number = 7; // the fixture's first and only pull
            let document = pull_json(number, "open", false, READY_HEAD_SHA);
            pulls
                .lock()
                .expect("the pull lock")
                .insert(number, document.clone());
            ResponseTemplate::new(201).set_body_json(document)
        })
        .mount(&server)
        .await;
    let pulls = Arc::clone(&fixture.pulls);
    Mock::given(method("GET"))
        .and(path(format!("{REPO_FULL_PATH}/pulls")))
        .respond_with(move |_: &wiremock::Request| {
            let pulls = pulls.lock().expect("the pull lock");
            let open: Vec<&serde_json::Value> = pulls
                .values()
                .filter(|pull| pull["state"] == "open")
                .collect();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total_count": open.len(),
                "items": open,
            }))
        })
        .mount(&server)
        .await;
    let pulls = Arc::clone(&fixture.pulls);
    Mock::given(method("GET"))
        .and(path(format!("{REPO_FULL_PATH}/pulls/7")))
        .respond_with(move |_: &wiremock::Request| {
            let pull = pulls
                .lock()
                .expect("the pull lock")
                .get(&7)
                .cloned()
                .unwrap_or_else(|| pull_json(7, "open", false, READY_HEAD_SHA));
            ResponseTemplate::new(200).set_body_json(pull)
        })
        .mount(&server)
        .await;
    let pulls = Arc::clone(&fixture.pulls);
    Mock::given(method("PATCH"))
        .and(path(format!("{REPO_FULL_PATH}/pulls/7")))
        .respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = req.body_json().unwrap_or_default();
            let state = body["state"].as_str().unwrap_or("open").to_owned();
            // Closing is not merging: the fixture distinguishes them
            // exactly how the provider's mapping does.
            let closed = pull_json(7, &state, false, READY_HEAD_SHA);
            pulls
                .lock()
                .expect("the pull lock")
                .insert(7, closed.clone());
            ResponseTemplate::new(200).set_body_json(closed)
        })
        .mount(&server)
        .await;

    // Labels + comments + statuses.
    Mock::given(method("POST"))
        .and(path(format!("{REPO_FULL_PATH}/issues/7/labels")))
        .respond_with(|_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "id": 1,
                    "node_id": "L1",
                    "url": fake_url("label"),
                    "name": "release",
                    "color": "112233",
                    "default": false,
                }
            ]))
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{REPO_FULL_PATH}/issues/7/comments")))
        .respond_with(|_: &wiremock::Request| {
            ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 9001,
                "node_id": "C_9001",
                "user": {
                    "id": 7,
                    "node_id": "U1",
                    "login": "release-bot",
                    "type": "Bot",
                    "url": fake_url("user"),
                    "avatar_url": fake_url("avatar"),
                    "gravatar_id": "",
                    "site_admin": false,
                    "html_url": fake_url("user-html"),
                    "followers_url": fake_url("followers"),
                    "following_url": fake_url("following"),
                    "gists_url": fake_url("gists"),
                    "starred_url": fake_url("starred"),
                    "subscriptions_url": fake_url("subs"),
                    "organizations_url": fake_url("orgs"),
                    "repos_url": fake_url("repos"),
                    "events_url": fake_url("events"),
                    "received_events_url": fake_url("received"),
                },
                "body": "the comment",
                "url": fake_url("comment"),
                "html_url": fake_url("comment"),
                "author_association": "none",
                "created_at": "2026-10-01T00:00:00Z",
                "updated_at": "2026-10-01T00:00:00Z",
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(format!("/repositories/{REPO_ID}/statuses/.+")))
        .respond_with(|_: &wiremock::Request| {
            ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 5,
                "node_id": "S1",
                "state": "success",
                "url": fake_url("status"),
            }))
        })
        .mount(&server)
        .await;

    // Protection surfaces.
    Mock::given(method("GET"))
        .and(path(format!("{REPO_FULL_PATH}/branches/main/protection")))
        .respond_with(|_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(branch_protection_json())
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "{REPO_FULL_PATH}/branches/{RELEASE_BRANCH}/protection"
        )))
        .respond_with(|_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(branch_protection_json())
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{REPO_FULL_PATH}/rulesets")))
        .respond_with(move |_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(rulesets_json(&["~ALL"]))
        })
        .mount(&server)
        .await;

    // GraphQL (auto-merge).
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(|_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {
                    "enablePullRequestAutoMerge": {
                        "pullRequest": { "number": 7 }
                    }
                }
            }))
        })
        .mount(&server)
        .await;

    // Releases.
    Mock::given(method("POST"))
        .and(path(format!("{REPO_FULL_PATH}/releases")))
        .respond_with(|_: &wiremock::Request| {
            ResponseTemplate::new(201).set_body_json(release_json())
        })
        .mount(&server)
        .await;

    (provider, server)
}

#[tokio::test]
async fn test_the_provider_lifecycle_against_mocked_github() {
    let fixture = Arc::new(Fixture::default());
    let (provider, _server) = fixture_provider(Arc::clone(&fixture)).await;
    let repo = RepoRef::new("github", REPO_ID);

    // Metadata: full name and default branch by immutable id.
    assert_eq!(
        provider.get_repo_path(&repo).await.expect("repo path"),
        REPO_PATH
    );
    assert_eq!(
        provider.default_branch(&repo).await.expect("default"),
        "main"
    );
    assert_eq!(
        provider
            .branch_sha(&repo, "main")
            .await
            .expect("the main sha"),
        "main-seed-sha"
    );

    // Branch: missing until created, then present.
    assert!(
        provider.branch_sha(&repo, RELEASE_BRANCH).await.is_err(),
        "the release branch does not exist before creation"
    );
    provider
        .create_branch(&repo, RELEASE_BRANCH, "main-seed-sha")
        .await
        .expect("the branch creates");

    // Fused edit + commit: the release edit lands as one commit.
    let edit = Edit {
        file: THE_FILE.to_owned(),
        format: None,
        field: "image.tag".to_owned(),
        value: Some(serde_json::json!("1.2.3")),
    };
    let tip = fixture.tip(RELEASE_BRANCH);
    let committed = provider
        .commit_files(
            &repo,
            RELEASE_BRANCH,
            &[edit.clone()],
            "Release my-service 1.2.3",
            Some(&tip),
        )
        .await
        .expect("the fused commit");
    assert_eq!(committed.branch, RELEASE_BRANCH);
    assert!(committed.sha.starts_with("commit-sha-"), "the commit's sha");

    // The file on the branch now carries the intended value.
    let committed_text = String::from_utf8(
        provider
            .read_file(&repo, THE_FILE, RELEASE_BRANCH)
            .await
            .expect("the committed file"),
    )
    .expect("the committed text");
    assert!(
        committed_text.contains("tag: 1.2.3"),
        "the committed file carries the edit: {committed_text:?}"
    );

    // The no-op replay: the same edit over the committed state writes
    // nothing.
    let tip = committed.sha.clone();
    let replay = provider
        .commit_files(
            &repo,
            RELEASE_BRANCH,
            &[edit],
            "Release my-service 1.2.3",
            Some(&tip),
        )
        .await
        .expect("the replayed commit");
    assert_eq!(replay.sha, tip, "the replay no-ops at the same tip");
    assert_eq!(
        fixture.commits.load(Ordering::SeqCst),
        1,
        "one commit only"
    );

    // Change request: created once; a second pass finds the open one.
    let created = provider
        .create_change_request(
            &repo,
            RELEASE_BRANCH,
            "main",
            "Release my-service 1.2.3 to stage",
            "Automated release change request.",
            &["release".to_owned()],
        )
        .await
        .expect("the pull creates");
    assert_eq!(created.number, 7);
    assert!(matches!(created.state, CrState::Open));
    let second = provider
        .create_change_request(
            &repo,
            RELEASE_BRANCH,
            "main",
            "Release my-service 1.2.3 to stage",
            "Automated release change request.",
            &[],
        )
        .await
        .expect("the second pull pass");
    assert_eq!(
        second.number, 7,
        "the find-by-head pass returns the open pull"
    );

    // Labels, comment, commit status.
    provider
        .add_labels(&repo, 7, &["release".to_owned()])
        .await
        .expect("the labels add");
    provider
        .comment(&repo, 7, "the comment")
        .await
        .expect("the comment posts");
    provider
        .set_commit_status(&repo, committed.sha.as_str(), CommitStatus::Success)
        .await
        .expect("the commit status");

    // Protection surfaces (the review check and the tag pattern's).
    let protection = provider
        .check_branch_protection(&repo, RELEASE_BRANCH)
        .await
        .expect("the protection reads");
    assert!(protection.requires_reviews, "the pull's base requires reviews");
    assert_eq!(protection.required_review_count, Some(1));
    assert!(
        provider
            .check_tag_protection(&repo, "v1.2.3")
            .await
            .expect("tag protection"),
        "the ruleset protects ~ALL tags"
    );

    // Auto-merge: GraphQL keyed on the pull's node id.
    provider
        .enable_auto_merge(&repo, 7)
        .await
        .expect("auto-merge enables");

    // Tag + release (the v1.1 builtins' seam, capability-gated).
    provider
        .create_tag(&repo, "v1.2.3", committed.sha.as_str())
        .await
        .expect("the tag creates");
    provider
        .create_release(&repo, "v1.2.3", "Release 1.2.3", "the release body")
        .await
        .expect("the release creates");

    // The reconciliation's lookup: open pull listing sees exactly one.
    let open = provider
        .list_open_change_requests(&repo)
        .await
        .expect("the open pull");
    assert_eq!(open.len(), 1, "the one pull is open");

    // Close without merged: state flips closed, no more open pulls.
    provider
        .close_change_request(&repo, 7, "closing")
        .await
        .expect("the pull closes");
    let closed = provider
        .get_change_request(&repo, 7)
        .await
        .expect("the pull");
    assert!(
        matches!(closed.state, CrState::Closed),
        "the closed pull reads closed"
    );
    assert!(
        provider
            .list_open_change_requests(&repo)
            .await
            .expect("the open pull after close")
            .is_empty(),
        "the closed pull no longer lists"
    );

    // Branch delete (the cleanup's compensation).
    provider
        .delete_branch(&repo, RELEASE_BRANCH)
        .await
        .expect("the branch deletes");
}
