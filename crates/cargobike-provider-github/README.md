# cargobike-provider-github

_GitHub provider: `Provider` contract over `octocrab`, webhook signing and normalisation._

## Purpose

Implements the `cargobike_core::provider::Provider` trait for GitHub, plus the
webhook edge GitHub-specific half. It depends on `cargobike-core` only — it is
a **native** provider, not a WASM guest (PRD §11.2: no `extension-sdk`
dependency, no DBOS).

Components:

- **`lib.rs` — `GithubProvider`**: the full REST surface over `octocrab` 0.54:
 branches (by immutable repository ID), fused edit-find-then-commit
 (`commit_files` with expected-parent guard and re-commit no-op), change
 requests (create, get, find-by-head-branch, close+comment, list with paging),
 files, comments, labels, commit statuses, default-branch and branch-SHA
 readers, branch-protection and tag-protection (rulesets) checks, auto-merge
 (GraphQL), tag and release creation (`ProviderCaps`-gated), webhook
 verification delegating to `webhook.rs`.
- **`webhook.rs` — `verify_and_normalise`**: constant-time HMAC-SHA256
 verification (`X-Hub-Signature-256`, any of the two configured secrets for
 rotation — /) and normalisation of the events Cargobike acts on:
 tag pushes (`NormalisedEvent::TagPush`), `pull_request` close
 (`ChangeRequestClosed`), everything else `Unrecognised` .
- **`signature.rs`** — test-side HMAC helpers (RFC 4231 vectors checked).

## Status

The Phase 4 REST/webhook surface is implemented, with the wiremock lifecycle
suite green; only 4.3's `graphql_client` typed queries remain pending. Known
gaps tracked in [`docs/TODO-phase-audit.md`](../../docs/TODO-phase-audit.md)
§6: GHES `api_url` support, per-repo installation lookup (currently one
static installation id), minimal-GitHub-App permission documentation.

## Usage

Construction happens at the server's boot (single place), behind the provider
registry:

```rust
use cargobike_provider_github::{GithubAuth, GithubProvider};
use cargobike_core::registry::ProviderRegistry;

let auth = GithubAuth::App {
 app_id: 123,
 installation_id: 456,
 private_key_pem: key, // loaded from the config's secret shape
};
let provider = GithubProvider::new(&auth)?;

let mut registry = ProviderRegistry::new;
registry.insert("github", provider.into);
```

Webhook edge:

```rust
use cargobike_provider_github::verify_and_normalise;
use cargobike_core::webhook::NormalisedEvent;

let event = verify_and_normalise(&headers, &raw_body, &secrets)?; // 401 on failure
match event {
 NormalisedEvent::TagPush(push) => { /* trigger rules decide */ }
 NormalisedEvent::ChangeRequestClosed { number, merged } => { /* correlation */ }
 NormalisedEvent::Unrecognised { .. } => { /* 200 and ignore */ }
}
```

## Tests

```bash
cargo test -p cargobike-provider-github
```

- `tests/lifecycle_wiremock.rs` — the full provider lifecycle against a
  mocked GitHub API: repository metadata by immutable id, branch
  creation with the missing-then-present read, the fused edit+commit
  plus its no-op replay, change requests (create twice = one pull),
  labels, comments, commit statuses, the review/tag-protection checks,
  GraphQL auto-merge, tag and release creation, and the branch delete.
- Unit tests cover webhook verification/normalisation and the RFC 4231
  signing vectors.

[`AGENTS.md`](AGENTS.md) holds the contribution rules for this crate.
