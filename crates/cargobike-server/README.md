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
| `config` (§4.17, §13) | The YAML config: top-level sections (`server`, `database`, `leader_election`, `auth`, `secrets`, `providers`, `extensions`, `templates`, `engine`, `reconciler`, `retention`, `limits`, `network`, `logging`, `metrics`, `applications`, `application_groups`), secret-shaped values (`{ file }` / `{ env }` / `{ secret }`, literals warned, F-89), post-parse `${VAR}` interpolation that refuses inside secret fields, curated `CARGOBIKE_SERVER_*` overrides, GitHub-provider env stamps |
| `validation` (2.7) | Semantic boot validation: unusable OIDC entries (F-80), API-key hash checks, releaser references + grants (F-99a), ref/tag-format cross-check, registry-`approval`-needs-template-`wait: approval` invariant (F-96), tag-format/branch-format placeholder grammar (F-147), `extends` group references |
| `auth` (2.5/2.6) | Claim matching (glob by default, AND, array-or — F-79), algorithm allowlist (F-78), OIDC verification with cached JWKS (F-77), argon2 API keys with rotation + expiry (F-85), the localhost-only bootstrap key (F-86), `authorize_create` (grant + `releasers` + `repository_id` — F-93), the Bearer middleware |
| `http` (2.1/2.4/2.8) | Router with RFC 9457 problem details (`http::errors`), health surfaces (`live`/`ready`/`startup`), `clientconfig` (issuer+audience hints only, F-98), release endpoints (list with filters + cursor pagination, idempotent create with 202/200, get, cancel with If-Match guard, terminal-only delete), `whoami`, trace/panic/body-limit/sensitive-header middleware |
| `db` (2.3) | Pool + migrations: `releases`, events, leases, CR correlation tables |
| `release` (2.3/2.4) | `ReleaseRepository`: JSONB documents + column indexes, idempotent create over `(application, version)` among non-terminal rows (F-109), phase/terminal updates with optimistic concurrency (`If-Match`/`resource_version`, F-9), terminal-only delete (event log retained, US-6) |
| `main` | clap args (`--config`, `--listen`, `--public-url`; `CARGOBIKE_SERVER_CONFIG` env), `hash-api-key` stdin subcommand (§9.3), logging init (F-140) |

## Status

Phase 2 scope is complete in-scope; Phase 4's CLI runs against it. Not yet in
place (audit TODO): engine boot/workflow start at create (the `TODO(2.4b)`
marker), webhooks + SSE + events + snapshots + approvals + retry endpoints
(Phase 5), application-group discovery, provider construction, metrics port.

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

The SIGHUP reload (F-129) re-reads config, re-validates, and swaps the shared
config the auth layer and handlers read; a broken file logs and keeps the
previous one.

Health/discovery (unauthenticated, F-107):

```bash
curl -s http://localhost:8080/api/v1/live     # 200 always
curl -s http://localhost:8080/api/v1/ready    # 503 while a standby
curl -s http://localhost:8080/api/v1/clientconfig
```

## Tests

```bash
CARGOBIKE_TEST_DATABASE_URL=postgres://... cargo test -p cargobike-server
```

`tests/authed_lifecycle.rs` (API-key auth over an in-process server, idempotent
create, problem documents), `tests/oidc_roundtrip.rs` (locally issued RS256
tokens through the trust policy), `tests/sighup_reload.rs`. Tests skip without
`CARGOBIKE_TEST_DATABASE_URL` (testcontainers wiring is a TODO). See
[`AGENTS.md`](AGENTS.md) for contribution rules.
