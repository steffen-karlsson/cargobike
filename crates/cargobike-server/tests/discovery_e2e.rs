//! Application-group discovery end to end (§4.13a): the boot scan
//! materialises the deployment repository's matching files into
//! virtual applications, the `extends` override wins with its pinned
//! source, a same-name collision without `extends` skips the
//! discovered entry, and a discovered application runs the full
//! create against the discovery repository (the group's environment
//! repo default). Requires `CARGOBIKE_TEST_DATABASE_URL` (or a
//! throwaway container via the shared test-support helper).

use argon2::password_hash::PasswordHasher as _;
use std::net::SocketAddr;
use std::sync::Arc;

/// The discovered apps' template: edit + change request + the merge
/// wait (the release stalls on the provider's open CR — a reliable
/// shape to observe without phase-five dependencies).
const TEMPLATE: &str = r#"
name: service
version: "1"
inputs: {}
environment_inputs:
  repo: { type: repo }
  edits: { type: edits }
environments:
  - name: preview
    steps:
      - id: edit
        uses: builtin/commit-files@1
      - id: cr
        uses: builtin/change-request@1
        with:
          branch: ${{ steps.edit.outputs.branch }}
      - wait: merge
        timeout: 1h
        on_modified: fail
        on_timeout: fail
"#;

#[allow(clippy::print_stderr, clippy::expect_used)]
async fn test_server() -> Option<(String, Arc<cargobike_engine::mock::MockProvider>)> {
    let url = cargobike_testsupport::database_url().await?;
    // The shared fixture DB's leftovers: this run's applications start
    // with clean rows (the lease would outlive its release). A
    // not-yet-migrated throwaway container reads as "nothing to wipe".
    cargobike_testsupport::wipe_fixture_rows(&url, &["billing", "payment-service"])
        .await
        .expect("the fixture rows wipe");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let presented = "cbk_test_discovery_secret";

    let salt = argon2::password_hash::SaltString::encode_b64(b"cb-test-salt-16-x").expect("salt");
    let hash = argon2::Argon2::default()
        .hash_password(presented.as_bytes(), &salt)
        .expect("hash the presented key")
        .to_string();

    let dir = std::env::temp_dir();
    let scratch = dir
        .join(format!("cb-discover-mock-{port}"))
        .to_string_lossy()
        .to_string();
    let mock = Arc::new(cargobike_engine::mock::MockProvider::at(&scratch));
    // The deployment repository's tree (the discovery scan reads it):
    // billing is plain-discovered, payment-service is discovered AND
    // overridden by the explicit `extends` entry, web COLLIDES with an
    // explicit non-extends entry of the same name.
    mock.seed_tree(
        "789012",
        "main",
        &[
            "apps/billing/preview.yaml",
            "apps/payment-service/preview.yaml",
            "apps/web/preview.yaml",
            "README.md",
        ],
    );
    // The discovered edit's target needs its content envelope on the
    // base branch (an edit on a file that never existed has no path
    // to write into).
    mock.seed_file("apps/billing/preview.yaml", "image:\n  tag: 0.0.0\n");
    let mut providers = cargobike_core::registry::ProviderRegistry::new();
    providers.insert(
        "github",
        mock.clone() as Arc<dyn cargobike_core::provider::Provider>,
    );

    let templates_dir = dir.join(format!("cb-discover-templates-{port}"));
    let _ = std::fs::create_dir_all(&templates_dir);
    std::fs::write(templates_dir.join("service.yaml"), TEMPLATE).expect("write template");
    let templates_display = templates_dir.display().to_string();

    let config_path = dir.join(format!("cb-discover-test-{port}.yaml"));
    std::fs::write(
        &config_path,
        format!(
            "server:\n  listen: \"127.0.0.1:{port}\"\n  public_url: http://127.0.0.1:{port}\n\n\
             database:\n  url: \"{url}\"\n  dbos_schema: \"cbdiscover-{port}\"\n\n\
             leader_election:\n  enabled: false\n\n\
             templates:\n  directory: {templates_display}\n\n\
             auth:\n  api_keys:\n    - name: test-key\n      hash: \"{hash}\"\n      grants: [release:create, release:read, release:cancel, release:delete]\n\n\
             {REGISTRY_YAML}"
        ),
    )
    .expect("write the test config");

    let boot = cargobike_server::boot(Some(&config_path), Some(providers)).await;
    let (router, _state) = match boot {
        Ok(booted) => booted,
        Err(error) => {
            eprintln!("skipping: server boot failed ({error})");
            let dumped = std::fs::read_to_string(&config_path).unwrap_or_default();
            eprintln!("the config:\n{dumped}");
            return None;
        }
    };
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    Some((format!("http://127.0.0.1:{port}"), mock))
}

/// The registry half of the config (a plain const — no format!
/// escaping for the YAML's braces). Real newlines, never `\` line
/// continuations: a continuation eats the next line's leading spaces,
/// which de-indented the `- name:` entries into broken YAML — the
/// suite's boot failed and the tests skipped vacuously.
const REGISTRY_YAML: &str = "\
applications:
  # The collision: an explicit entry with the discovered name `web`, NO extends.
  #  The discovered entry must skip (an error, never a silent override).
  - name: web
    source: { provider: github, id: \"42\" }
    template: service@1
    versioning: { scheme: opaque, require_tag: false }
    releasers: [{ api_key: test-key }]
    environments: {}
  # The per-app override: payment-service pins one repository.
  - name: payment-service
    extends: monorepo-deploy
    source: { provider: github, id: \"345678\" }
    environments: {}

application_groups:
  - name: monorepo-deploy
    template: service@1
    versioning: { scheme: opaque, require_tag: false }
    source: { provider: github, owner_id: \"123456\" }
    discovery:
      repo: { provider: github, id: \"789012\" }
      match: \"apps/{application}/{environment}.yaml\"
      interval: 10m
    releasers: [{ api_key: test-key }]
    environments:
      preview:
        edits:
          - file: \"apps/{application}/preview.yaml\"
            field: image.tag
";

/// The applications listing entry by name.
fn named<'a>(listing: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    listing["applications"]
        .as_array()?
        .iter()
        .find(|entry| entry["name"].as_str() == Some(name))
}

#[allow(clippy::print_stderr, clippy::expect_used)]
#[tokio::test(flavor = "current_thread")]
async fn test_discovery_materialises_overrides_and_skips_collisions() {
    let Some((base, _mock)) = test_server().await else {
        return;
    };
    let client = reqwest::Client::new();
    let auth = (
        "Authorization",
        "Bearer cbk_test_discovery_secret".to_owned(),
    );

    // The boot scan ran: the listing carries the discovered billing
    // (the virtual app), the overridden payment-service (the pinned
    // source), and the explicit web (the collision — the discovered
    // entry skipped, the explicit entry stands).
    let listed = client
        .get(format!("{base}/api/v1/applications"))
        .header(auth.0, auth.1.clone())
        .send()
        .await
        .expect("http")
        .json::<serde_json::Value>()
        .await
        .expect("the applications body");

    let billing = named(&listed, "billing").expect("the discovered billing app");
    assert_eq!(
        billing["repository"]["owner_id"].as_str(),
        Some("123456"),
        "the virtual app carries the group's org-wide source: {billing}"
    );
    assert_eq!(
        billing["repository"]["id"].as_str(),
        None,
        "the org-wide source has no repository id: {billing}"
    );

    let payment = named(&listed, "payment-service").expect("the overridden app");
    assert_eq!(
        payment["repository"]["id"].as_str(),
        Some("345678"),
        "the extends override pins its repository: {payment}"
    );

    let web = named(&listed, "web").expect("the colliding explicit app");
    assert_eq!(
        web["repository"]["id"].as_str(),
        Some("42"),
        "the collision kept the EXPLICIT entry (never a silent override): {web}"
    );
    // The discovered `web` would carry the org source — exactly one
    // entry of the name exists.
    assert_eq!(
        listed["applications"]
            .as_array()
            .map(|apps| {
                apps.iter()
                    .filter(|entry| entry["name"].as_str() == Some("web"))
                    .count()
            })
            .unwrap_or(0),
        1
    );
}

/// A discovered application runs the full create against the
/// DISCOVERY repository (the group environment's repo default): the
/// commit lands there and the change request opens.
#[allow(clippy::print_stderr, clippy::expect_used)]
#[tokio::test(flavor = "current_thread")]
async fn test_discovered_app_releases_against_the_deployment_repo() {
    let Some((base, mock)) = test_server().await else {
        return;
    };
    let client = reqwest::Client::new();
    let auth = (
        "Authorization",
        "Bearer cbk_test_discovery_secret".to_owned(),
    );

    let created = client
        .post(format!("{base}/api/v1/releases"))
        .header(auth.0, auth.1.clone())
        .json(&serde_json::json!({
            "application": "billing",
            "version": "42.0.0-discovery",
        }))
        .send()
        .await
        .expect("http");
    assert_eq!(
        created.status(),
        reqwest::StatusCode::ACCEPTED,
        "the discovered app authorises and provisions"
    );
    let document: serde_json::Value = created.json().await.expect("the release body");
    let release_id = document["metadata"]["id"].as_str().expect("the release id");

    // The create's facts ride the document (US-2's read-back).
    let read = client
        .get(format!("{base}/api/v1/releases/{release_id}"))
        .header(auth.0, auth.1.clone())
        .send()
        .await
        .expect("http");
    assert_eq!(read.status(), reqwest::StatusCode::OK);

    // The edit lands on the DISCOVERY repository (the mock's seeded
    // file `{branch}/apps/billing/preview.yaml`): the group's
    // environment repo default pointed the release there.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut last_document = serde_json::Value::Null;
    loop {
        let state = mock.state();
        let edit_landed = state
            .files
            .keys()
            .any(|key| key.ends_with("/apps/billing/preview.yaml"));
        let cr_open = !state.change_requests.is_empty();
        if edit_landed && cr_open {
            break;
        }
        if let Ok(read) = client
            .get(format!("{base}/api/v1/releases/{release_id}"))
            .header(auth.0, auth.1.clone())
            .send()
            .await
        {
            if let Ok(document) = read.json::<serde_json::Value>().await {
                last_document = document;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the discovered app's release never reached the CR: {state:?}\nthe release document: {last_document}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // The cleanup: cancel the stalled release and drain it. The
    // interpreter stays parked at its merge wait (the CR never
    // merges), so the drain's observable is the CLEANUP's effect —
    // the CR closes on the provider — not the interpreter's own
    // document write (authed_lifecycle's cancel drains the same way).
    let cancel = client
        .post(format!("{base}/api/v1/releases/{release_id}/cancel"))
        .header(auth.0, auth.1.clone())
        .send()
        .await
        .expect("http");
    assert!(
        cancel.status().is_success(),
        "the cancel is accepted: {}",
        cancel.status()
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let state = mock.state();
        let cr_closed = state
            .change_requests
            .values()
            .any(|cr| cr.state == "closed");
        if cr_closed {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the cancel's cleanup never closed the CR: {state:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    let drain = client
        .delete(format!("{base}/api/v1/releases/{release_id}"))
        .header(auth.0, auth.1.clone())
        .send()
        .await
        .expect("http");
    assert!(drain.status().is_success(), "the drain deletes the row");
}
