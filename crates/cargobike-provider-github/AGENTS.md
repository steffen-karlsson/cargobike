# cargobike-provider-github — AGENTS.md

Crate-specific how-to; project-wide rules in the root
[`AGENTS.md`](../../AGENTS.md).

## How to develop

- **Pinned API surface**: shapes here are pinned against `octocrab` 0.54
 (`get_ref`, `create_ref`, `create_tree` with UTF-8 content,
 `create_commit`, `update_ref force=false`, pulls list/get/update,
 issues comments + labels, `repos_by_id`). Bump `octocrab` only with a
 re-check of every call site — fake-free compile errors are the norm here.
- **Repository identity is the immutable ID** (`RepoRef.id`), not the
 owner/name path. Operations that must resolve a path (`owner_name`) derive
 it via `get_repo_path` and treat it as a display label only .
- **Trait compatibility**: every method added to
 `cargobike_core::provider::Provider` must either be implemented here with a
 capability gate or match the engine/mock contract that the other Provider
 implementations follow. `capabilities` tells the truth — a `false` there
 must mean the operation returns `ProviderError::Unsupported`.
- **Idempotency contract (the idempotency strategy table)**: "already exists at the expected state" is
 success. `commit_files` re-verifies content and no-ops; CR operations find
 by head branch before creating. New provider operations must state their
 the idempotency strategy table strategy.
- **Secrets**: keys and tokens are `SecretString`; errors must never contain
 token material. HMAC checks use `verify_slice` (constant time) — .
- No DBOS, no `extension-sdk` dependency (PRD A.14).

## How to test

```bash
cargo test -p cargobike-provider-github
```

- `tests/lifecycle_wiremock.rs` runs the whole provider lifecycle against a
  wiremock-served GitHub API (wiremock under this crate's
  `[dev-dependencies]`, workspace-root deps untouched). New provider
  operations get their lifecycle step added there — the fixture is a small
  stateful GitHub (files, refs, pulls), so an operation usually just needs a
  route plus assertions.
- Webhook unit tests cover: valid/invalid signature, two-secret rotation, tag
 push vs branch push, `pull_request` closed/merged/opened, unknown events.
- Signature helpers use RFC 4231 case-2 as a known-answer vector.
- Every normalised event that later consumers rely on needs a
 round-trip/shape test; when the correlation flows through normalisation,
 test the field end-to-end.

## How to document

- Module docs state the provider's role and the octocrab version it is pinned
 to; method docs reference the PRD's requirements (e.g. verified
 paths, constant-time compare, neutral CR terms).
- When GitHub's API shape forces a deviation from the PRD's trait sketch,
 document the deviation in the code and file/or adjust the PRD with the
 reasoning (never silently diverge).

## How to contribute

- Conventional commits `<type>(cargobike-provider-github): <subject>`
 (PRD §20.1), one operation or one fix per commit.
- Provider changes that alter replay behaviour must be checked against the
 engine's `wait: merge` verification assumptions in the same commit.

## Criteria for evaluating contributions (before committing)

All must be true:

1. All four gates pass: `make lint`, `make test`, `make build`,
 `make docs-check` — webhook tests stay green.
2. No `unwrap`/`expect` in non-test code (workspace lints deny them); octocrab
 failures map to `ProviderError` with the request-failure mapping kept
 consistent (`NotFound` for HTTP 404, `Request` otherwise).
3. Auth material never appears in `Display`/`Debug` output (the `GithubAuth`
 hand-written `Debug` stays).
4. Capability flags are exact: `Unsupported` for all unimplemented optional
 operations, no half-implementations.
5. The [`README.md`](README.md) status/usage sections and this file are
 updated in the same commit for any new operation, test or process change.
6. New operations state their idempotency strategy (the idempotency strategy table) in rustdoc.
