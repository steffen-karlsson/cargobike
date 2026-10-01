# cargobike-server — AGENTS.md

Crate-specific how-to; project-wide rules in the root
[`AGENTS.md`](../../AGENTS.md).

## How to develop

- **Config is the security core (§4.13)**: shapes are `deny_unknown_fields`
  end-to-end and the documented example config must load **verbatim** in a
  unit test (`config.rs::test_documented_server_config_loads_verbatim`, F-32).
  Changing config shapes means updating the PRD example in the same commit.
- **Secrets discipline (F-87, F-90, F-127)**: secret values are
  `SecretValue`/`SecretString`; `Debug` is hand-written (never derived) for
  anything that can carry secret material; tokens never appear in logs or
  problem documents. Driver error text goes to the log, the response gets the
  generic wording (`repository_to_api`).
- **Error style (F-103)**: every failure leaves the handler as an
  `ApiError` RFC 9457 problem with a `code` from the vocabulary — a bare
  status/JSON body is a review-defeatable bug. New codes belong in
  `http/errors.rs` with the slug.
- **Repository framing**: keep `release.rs` macro-free (sqlx 0.9 macros need a
  live database or a prepared snapshot; conditional SQL is written by hand
  like the idempotent create's `ON CONFLICT ... DO NOTHING`).
- Optimistic concurrency: state-changing routes honour `If-Match`
  (`resource_version`) and refuse terminal-state transitions with 409 — keep
  the `set_phase` guard rails when extending (F-9).
- **Auth layering**: grant checks are handler-level (`caller.has_grant`);
  create authorization is `authorize_create` (grant + `releasers` reference +
  `repository_id` claim — F-93/F-99). Never widen trust: the deny-by-default
  posture is the crate's invariant (F-92).
- Leader election (2.8/F-131): `elect_leader` currently takes the advisory
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
- OIDC roundtrips mint an RS256 token locally (no network). Keep the JWKS
  fixture self-contained; never test against live issuers.
- New endpoints: cover the grant-negative path (403 with the right `code`),
  the 409/404 vocabulary, and the If-Match behaviour when the route is
  state-changing.
- Tests skip without the database URL today; when testcontainers wiring
  lands, the skip becomes a fixture (`audit TODO` §9). Until then, note the
  skip in the test's doc comment.

## How to document

- Handlers get module-level 💡- free one-liners referencing the F-IDs; the
  crate README's module table is the map — add your module to it in the same
  commit.
- Config fields carry the R-rule they follow (R2 booleans, R3 humantime
  durations, R4 sizes, R5 secret shapes, R7 globs) — use the PRD's vocabulary.
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
2. The verbatim documented-config test still passes (F-32/T5a).
3. No secret value can reach a log, an error message or a problem document —
   check `Debug` impls, redaction layers, and the sensitive-header list.
4. Every filtered/protected route checks its grant; unauthenticated routes
   are exactly the §12/F-107 set.
5. New config keys enforce R1–R12 (kebab/snake case, positive booleans,
   humantime durations, binary sizes, secret shapes) and appear in the PRD's
   example/config docs.
6. Problem documents carry `code`; statuses match the API table (10.2).
7. The crate's [`README.md`](README.md) and this file are updated in the
   same commit when functionality, tests, or processes change.
