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
 requests (create with the draft flag, get, find-by-head-branch, close+comment, list with paging),
 files, comments, labels, commit statuses, default-branch and branch-SHA
 readers, tag-object reads (`tag_sha` dereferences annotated tags to their
 commit — F-95's version policy verifies against the provider's own
 object), branch-protection and tag-protection (rulesets) checks, auto-merge
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
suite green; the GHES `api_url` base rides the provider's build (the config's
`api_url` reaches `Octocrab::builder().base_uri`). Still pending: 4.3's
`graphql_client` typed queries. Known gaps tracked in
[`docs/TODO-phase-audit.md`](../../docs/TODO-phase-audit.md) §6: per-repo
installation lookup (the config pins one static installation id today).

### Minimal GitHub App permission set (F-66)

The provider's REST surface maps to these repository permissions in the
GitHub App's settings — nothing broader is needed:

| Permission | Level | The calls it covers |
|---|---|---|
| Metadata | Read-only (mandatory) | repo lookups, `owner_name` |
| Contents | Read & write | `create_branch` (`git refs`), `commit_files`, `read_file`, `create_tag`, `tag_sha` (git refs/tags) |
| Pull requests | Read & write | `create_change_request`, find-by-head, get, list |
| Issues | Read & write | `add_labels` (labels ride the issues API), `comment` |
| Commit statuses | Read & write | `set_commit_status` (`builtin/wait-for-check`) |
| Administration | Read-only | `check_branch_protection` (`GET …/branches/{b}/protection`) |
| Repository rulesets | Read-only | `check_tag_protection` (`GET …/rulesets`) |

Notes:

- Webhook deliveries need **no** API permission — the receiver verifies the
  `X-Hub-Signature-256` HMAC against the configured secret; creating the
  webhook itself is the operator's (or the App's Webhooks permission, if you
  let it manage its own hooks).
- `enable_auto_merge` and CR merges need nothing beyond Pull requests above.
- An environment with `allow_direct_commit: true` against a protected default
  branch additionally needs **bypass** on that branch's protection rule (or
  the rule must exempt the App) — that is the registry's explicit opt-in,
  never a permission this provider grants itself.
- The installation token should be scoped to the repositories the applications
  actually release; per-repo installation lookup (F-122) is still pending.

### `update_branch` (F-46): a documented v1 deviation

The trait method exists with the default `Unsupported`; **no v1 step type
consumes it** (the `update-branch` step is v1.1). The GitHub implementation
deliberately did not land before its consumer: octocrab 0.54 has no wrapper for
`PUT /repos/{repo}/branches/{branch}/update` (its `merge_upstream` is the
fork-only sync endpoint), so the implementation will either wrap the update
endpoint with a raw request or fast-forward the ref via `git refs` (failing
closed on non-fast-forward) when the v1.1 step type arrives.

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
// `api_url: None` is github.com; a value points at GitHub Enterprise
// Server (or a local mock).
let provider = GithubProvider::new_with_api_url(&auth, None)?;

let mut registry = ProviderRegistry::new();
registry.insert(
    "github",
    Arc::new(provider) as Arc<dyn cargobike_core::provider::Provider>,
);
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
