# cargobike-server

_The centralized release server: REST API, auth, webhooks (Phase 5), durable execution._

## Purpose

`cargobike-server` is the server binary (lib + thin `main`): it owns the HTTP
surface (axum), the PostgreSQL state, the auth stack, and the process
bootstrap. It coordinates releases; it never builds or deploys anything
(PRD §1). Its deps include `cargobike-core` today; the durable engine
(`cargobike-engine`) and the GitHub provider wiring are the top audit item
([`docs/TODO-phase-audit.md`](../../docs/TODO-phase-audit.md) §0).

Components:

| Module | Contents |
|---|---|
| `config` (§4.17, §13) | The YAML config: top-level sections (`server`, `database`, `leader_election`, `auth`, `secrets`, `providers`, `extensions`, `templates`, `engine`, `reconciler`, `retention`, `limits`, `network`, `logging`, `metrics`, `applications`, `application_groups`), secret-shaped values (`{ file }` / `{ env }` / `{ secret }`, literals warned), post-parse `${VAR}` / `${VAR:-default}` interpolation that refuses inside secret fields, curated `CARGOBIKE_SERVER_*` overrides, GitHub-provider env stamps |
| `validation` (2.7) | Semantic boot validation: unusable OIDC entries, API-key hash checks, releaser references + grants, ref/tag-format cross-check, registry-`approval`-needs-template-`wait: approval` invariant, tag-format/branch-format placeholder grammar, per-environment format-string placeholder grammar (`commit_message`, `change_request.{title,body}`), `extends` group references |
| `auth` (2.5/2.6) | Claim matching (glob by default, AND, array-or —), algorithm allowlist, OIDC verification (discovery document → `jwks_uri`, cached per issuer with the legacy direct path as fallback — the GitHub Actions issuer's `/.well-known/jwks` resolves without a `jwks_url` override; unknown-`kid` refetch rate-limited to one per 30s per URL), the CI context + actor + `sha` claim built from VERIFIED claims only (F-83/F-84/F-95: the Actions provider label keys on the real issuer, the run URL synthesised from the verified repository+run id), argon2 API keys with rotation + expiry, the localhost-only bootstrap key, `authorize_create` (grant + `releasers` + `repository_id` —), the Bearer middleware |
| `http` (2.1/2.4/2.8) | Router with RFC 9457 problem details (`http::errors`), health surfaces (`live`/`ready`/`startup`), `clientconfig` (issuer+audience hints only), release endpoints (list with filters + cursor pagination, idempotent create with 202/200 — the `(application, version)` dedupe among non-terminal rows plus the caller's `Idempotency-Key` header converging retries on the first attempt's release —, get, cancel with If-Match guard, retry: the failed release forks in place at the same ID with `fork_from` appended, `--new` copies to a fresh row carrying `retried_from`, terminal-only delete), `whoami`, trace/panic/body-limit/sensitive-header middleware; plus the webhook lane (5.1): `POST /webhooks/{provider}` verifies before any parse (the shared HMAC normalise), deduplicates on the delivery id, fast-acks 202 (200 acknowledge-ignored on unrecognised events), under its own body-limit lane |
| `db` (2.3) | Pool + migrations: `releases`, events, leases, CR correlation, create-idempotency-key tables |
| `release` (2.3/2.4) | `ReleaseRepository`: JSONB documents + column indexes, idempotent create over `(application, version)` among non-terminal rows, phase/terminal updates with optimistic concurrency (`If-Match`/`resource_version`), terminal-only delete (event log retained); the create's provision stages per-environment inputs (repo, edits, commit message, custom inputs, the registry-only `require_branch_protection` flag the `change-request` step's F-65 fail-closed gate reads, and the registry's R11 `change_request` overrides), resolves the F-147 format-string placeholders at provision time (`commit_message` and `change_request.{title,body}`: `{application}`/`{environment}`/`{version}`/`{release_id}`/`{release_url}` + `{{`/`}}` escapes), refuses paused applications/environments with 409 `StateConflict` naming the reason (F-142), stamps the caller's verified identity (`status.actor` + `status.ci`, F-83/F-84; the webhook path records the verified sender, F-97), and verifies the version against the provider's tag object when `require_tag` + `tag_format` demand it (F-95: the tag must exist, and the provenance SHA — the OIDC token's verified `sha` claim, the webhook payload's SHA, or an API-key request's SHA — must match the tag's commit; providers without `tag_lookup` fail closed) |
| `engine` | The engine's hosting: the services from the config (the providers' build incl. the GHES `api_url`, the named secrets' credential store, the egress-guarded HTTP seam, the leases/status/correlation stores), the DBOS boot with the interpreter (v2; the legacy v1 name registered for the drain) + cleanup + webhook + retry + reconciler registered before `launch()`; the engine knobs wire from the config (the step-output cap, the CEL's length bound), the materialised per-provider webhook secrets, the tag-push creator (the registry scan + `tag_format` extraction + the F-82 fail-closed tag-protection check + the shared create core), the production signal seam over the releases join, and the reconcile loop's start |
| `main` | clap args (`--config`, `--listen`, `--public-url`; `CARGOBIKE_SERVER_CONFIG` env), `hash-api-key` stdin subcommand (§9.3), logging init |

## Status

The server runs the durable engine end to end: the boot registers the
interpreter/cleanup/reconciler before `launch()`, the create provisions a
snapshot and starts the interpreter (the release status advances live via
the engine's status store), and the cancel stops the workflow and starts
the cleanup that closes recorded CRs, deletes branches and settles the
leases. Pending work: webhooks + SSE watch/event/snapshot/approvals/retry
endpoints (Phase 5), application-group discovery, the supersede chain's
server side, the leader-lock-loss fence, and the full SSRF set (the HTTP
seam is a first-cut guard).

## Usage

```bash
# Development bootstrap without a config file (§13.2).
CARGOBIKE_SERVER_DATABASE_URL_FILE=/run/secrets/db-url \
CARGOBIKE_SERVER_GITHUB_APP_ID=123 \
CARGOBIKE_SERVER_GITHUB_PRIVATE_KEY_FILE=/etc/cargobike/key.pem \
CARGOBIKE_SERVER_BOOTSTRAP_API_KEY_FILE=/etc/cargobike/bootstrap-key \
cargobike-server
```

With a config file (`/etc/cargobike/config.yaml` by default; flag or env
override):

```bash
cargobike-server --config /etc/cargobike/config.yaml
```

Produce a hash for `auth.api_keys[].hash`:

```bash
printf 'the-plaintext-key' | cargobike-server hash-api-key
```

The SIGHUP reload re-reads config, re-validates, and swaps the shared
config the auth layer and handlers read; a broken file logs and keeps the
previous one.

Health/discovery (unauthenticated):

```bash
curl -s http://localhost:8080/api/v1/live # 200 always
curl -s http://localhost:8080/api/v1/ready # 503 while a standby
curl -s http://localhost:8080/api/v1/clientconfig
```

## Tests

```bash
CARGOBIKE_TEST_DATABASE_URL=postgres://... cargo test -p cargobike-server
```

- `tests/authed_lifecycle.rs` — API-key auth over an in-process server:
  idempotent create, filters, problem documents, cancel/delete rules.
- `tests/oidc_roundtrip.rs` — locally issued RS256 tokens through the trust
  policy (claims, audience, expiry).
- `tests/sighup_reload.rs` — the hot-reload swap without a restart.
- `tests/retry_endpoint.rs` — the failed-release fork completing at the
  same ID (attempts' `fork_from` + the fork's own id), the `--new` copy
  with `retried_from`, the 409/404 guards.
- `tests/webhook_receiver.rs` — signed deliveries end-to-end: the tag-push
  creating the release (the fail-closed skip on the ruleset-capable default),
  the closed-PR correlation waking a waiting release to `Completed`, the
  401/404/ignored/dedupe surfaces. Delivery ids are port-tagged: a recorded
  workflow's replay would satisfy the duplicate-detection FOREVER, so each
  test run gets fresh ids.
- Config unit tests cover the secret shapes, the interpolation fail-fast,
  and the `${VAR:-default}` default syntax.

Tests skip without `CARGOBIKE_TEST_DATABASE_URL` (testcontainers wiring is a
TODO). See [`AGENTS.md`](AGENTS.md) for contribution rules.
