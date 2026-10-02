# cargobike-server — AGENTS.md

Crate-specific how-to; project-wide rules in the root
[`AGENTS.md`](../../AGENTS.md).

## How to develop

- **Config is the security core (§4.13)**: shapes are `deny_unknown_fields`
 end-to-end and the documented example config must load **verbatim** in a
 unit test (`config.rs::test_documented_server_config_loads_verbatim`).
 Changing config shapes means updating the PRD example in the same commit.
- **Secrets discipline **: secret values are
 `SecretValue`/`SecretString`; `Debug` is hand-written (never derived) for
 anything that can carry secret material; tokens never appear in logs or
 problem documents. Driver error text goes to the log, the response gets the
 generic wording (`repository_to_api`).
- **Error style **: every failure leaves the handler as an
 `ApiError` RFC 9457 problem with a `code` from the vocabulary — a bare
 status/JSON body is a review-defeatable bug. New codes belong in
 `http/errors.rs` with the slug.
- **Repository framing**: keep `release.rs` macro-free (sqlx 0.9 macros need a
 live database or a prepared snapshot; conditional SQL is written by hand
 like the idempotent create's `ON CONFLICT ... DO NOTHING`).
- Optimistic concurrency: state-changing routes honour `If-Match`
 (`resource_version`) and refuse terminal-state transitions with 409 — keep
 the `set_phase` guard rails when extending .
- **Auth layering**: grant checks are handler-level (`caller.has_grant`);
 create authorization is `authorize_create` (grant + `releasers` reference +
 `repository_id` claim — /). Never widen trust: the deny-by-default
 posture is the crate's invariant .
- Leader election (2.8/): `elect_leader` currently takes the advisory
 lock at boot and sets the readiness flag; the lock-loss fence and standby
 webhook-serving land with the executor wiring — flag the gap rather than
 pretending readiness.

## How to test

```bash
CARGOBIKE_TEST_DATABASE_URL=... cargo test -p cargobike-server
```

- Integration tests boot the *real* server in-process on an ephemeral port
 (`tests/authed_lifecycle.rs`); prefer that over handler unit tests so the
 middleware stack and problem documents stay exercised.
- No hardcoded machine paths in tests: scratch/log locations come from the
 platform temp dir (`std::env::temp_dir()`); GitHub's runners are linux
 hosts and a mac-first absolute path crashes the suite there (seen with the
 reload test's log file).
- OIDC roundtrips mint an RS256 token locally (no network). Keep the JWKS
 fixture self-contained; never test against live issuers.
- New endpoints: cover the grant-negative path (403 with the right `code`),
 the 409/404 vocabulary, and the If-Match behaviour when the route is
 state-changing.
- Tests skip without the database URL today; when testcontainers wiring
 lands, the skip becomes a fixture (`audit TODO` §9). Until then, note the
 skip in the test's doc comment.

## How to document

- Handlers get module-level one-liners citing the PRD's requirement; the
 crate README's module table is the map — add your module to it in the same
 commit.
- Config fields carry the R-rule they follow ( booleans, humantime
 durations, sizes, secret shapes, globs) — use the PRD's vocabulary.
- `docs/examples.md` is the user-facing config/example document; changing
 config behaviour requires updating it too.

## How to contribute

- Conventional commits `<type>(cargobike-server): <subject>` (PRD §20.1),
 one route/fix/behaviour per commit.
- Auth-adjacent changes (trust entries, grants, bootstrap, secrets) must
 include the negative test in the same commit — positive-only auth changes
 are incomplete commits.

## Criteria for evaluating contributions (before committing)

All must be true:

1. All four gates pass: `make lint`, `make test`, `make build`,
 `make docs-check` — server integration tests run (or skip
 explicitly with a documented reason).
2. The verbatim documented-config test still passes (/T5a).
3. No secret value can reach a log, an error message or a problem document —
 check `Debug` impls, redaction layers, and the sensitive-header list.
4. Every filtered/protected route checks its grant; unauthenticated routes
 are exactly the §12/ set.
5. New config keys enforce the naming conventions (kebab/snake case, positive booleans,
 humantime durations, binary sizes, secret shapes) and appear in the PRD's
 example/config docs.
6. Problem documents carry `code`; statuses match the API table (10.2).
7. The crate's [`README.md`](README.md) and this file are updated in the
 same commit when functionality, tests, or processes change.
