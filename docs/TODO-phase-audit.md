# Code audit vs PRD Phases 1–4 — TODO

> Built 2026-10-01 by reviewing every crate against `docs/PRD.md` (§16 Phase 1–4
> task lists, §14 test strategy, and the F-requirements they cover).
> Items are ordered by impact within each section. Checked boxes are done.

## 0. The one structural finding (blocks everything below)

- [ ] **The server does not depend on `cargobike-engine` (or on `cargobike-provider-github`).**
  Everything Phase 3 built — interpreter, signals, leases, reconciler, cleanup —
  currently runs only inside engine tests and `engine-harness`. The server
  exposes release rows over HTTP but never boots DBOS, registers
  `cargobike.interpret.v1`, starts a workflow, or talks to a real provider
  (PRD §11.2 dependency graph, task 2.4b's own `TODO(2.4b)` in
  `crates/cargobike-server/src/http.rs`, F-13, F-15, F-21, F-131).
  Concretely in `cargobike-server`:
  - [ ] Boot DBOS at startup: register interpreter + cleanup + reconciler before `launch()`; wire `releases` table as the `PendingReleaseSource` adapter (3.14 seam is still the in-memory fixture).
  - [ ] Start `cargobike.interpret.v1` at create (HTTP 202 path), building the `ReleaseSnapshot` from the registry entry (provision). Today `create_release` writes a doc with only `phase: "Pending"` and no snapshot.
  - [ ] Cancel endpoint → `DBOS::cancel` + start `cargobike.cleanup.v1` (F-60, F-75). Today it only flips a column.
  - [ ] Supersede path (engine `concurrency::supersede`) must cancel the old release's workflow and start cleanup with `superseded_by` (F-73); today the lease is transferred but the old release is never cancelled, never marked `Superseded`, and no CR is closed.
  - [ ] Construct provider instances from `config.providers` (GithubProvider, api_url/GHES, webhook secrets) into the `ProviderRegistry`; construct `CredentialStore` from `config.secrets` (today engine only knows `StubCredentials`); build the real SSRF-guarded HTTP client (F-117).
  - [ ] Leader-election fence: on lock loss the executor stops (process exit) per F-131/2.8; today no watchdog exists. Same for the standby "serve webhooks without executor" path (needs Phase 5 receiver, note in spike §4 gaps).
  - [ ] Recovery: app-version filter for workflow recovery (F-22, spike finding).

## 1. Release status is never written (PRD §4.1/§5.3, F-38)

- [ ] Nothing persists `status.environments`, `status.attempts`, per-env
  phases, `ChangeRequestRef`s, or the phase rollup. F-38's implicit
  `set-status` bookkeeping and A1's "conditional `UPDATE ... WHERE phase =
  <expected>`" are unimplemented; `InterpretResult` has no consumer.
- [ ] Server `create_release` writes a minimal document (no
  `status.actor`, `status.workflow`, `status.environments`, `status.attempts`,
  `spec.source` path labels are there but provisions are not) — violates §5.3
  model served by `GET /releases/{id}`.
- **Bug**: `ReleaseRepository::set_phase` updates only the SQL columns, not the
  JSON document — after `cancel`, `GET /releases/{id}` still reports
  `"phase": "Pending"` because the served document never changes.
- [ ] Per-env `Waiting`/`PendingApproval` (F-7) states and
  `change_request` refs are unrecorded (depends on the set-status bookkeeping).
- [ ] Snapshot content hash: F-16's canonical hash is unimplemented — engine
  tests pass `"crash-harness"` / `"sha256:x"` literals; no sha2 in the engine.

## 2. Provider-side security checks wired to the engine (PRD §4.5/§4.7/§4.12)

- [ ] **F-65 branch protection**: the `change-request` step never calls
  `Provider::check_branch_protection`; `require_branch_protection` is parsed in
  config but consulted nowhere. Must fail closed by default.
- [ ] **F-63 correlation stamp**: `CorrelationRepository::stamp` exists but is
  never called. `StepContext` has no handle to it, so the
  `(provider, repo_id, cr_number) → (release, environment, step, attempt)`
  row is never written when a CR opens — webhook correlation (5.2) and the
  reconciler sweep have no data.
- [ ] **F-95/F-4 version verification**: no create path validates the version
  against the scheme, resolves the tag from `tag_format`, or checks tag/SHA
  (`VersionNotVerified` is defined and never produced). Server-side verification for
  OIDC-token creates (SHA claim), API-key creates (tag exists / request SHA), and
  webhook creates (tag object SHA) are all missing (§3 US-1, §9.3).
- [ ] **F-82 tag protection check at trigger time** — provider capability
  implemented (`check_tag_protection`), but no trigger-time enforcement anywhere.
- [ ] **F-10 verified-path labels**: no startup `get_repo_path` check, no
  "app unavailable on mismatch" marking, no background retry, and no
  `cargobike validate --resolve` strict mode.

## 3. Queue wake, event log, and remaining Phase 3 seams

- [ ] **Queue-policy wake (F-71)**: releasing a lease does not `send`
  `Signal::LeaseReleased` on `lease/{environment}`; queued releases only wake on
  the 1-hour `QUEUE_WAKE_TIMEOUT` re-poll. Needs the queued-membership query
  (membership noted as living in the release store — unwired).
- [ ] Reconciler production seam: sqlx-backed `PendingReleaseSource` over the
  `releases` table (currently `InMemoryPendingReleaseSource` fixture only).
- [ ] Reconciler efficiency: ETag conditional requests and batched GraphQL
  lookups (F-69, 4.3) — current sweep does per-row REST `get_change_request`.
- [ ] Cleanup workflow body: provider calls run directly in the workflow body,
  not inside `dbos::step` (no checkpoint/backoff bounds; deviations from §6.7).
  Make each compensation durable/retryable.
- [ ] F-34 reservation: `StepRegistry::register` accepts any name;
  `has_step_name` (control-step reservation) exists but is not enforced, so a
  sidecar could still register `wait: merge`.

## 4. CEL / engine knobs (F-27, F-40)

- [ ] `engine.cel.max_cost` / `cel_max_expression_length` config exist but the
  evaluator ignores them (`expr::check` hardcodes `DEFAULT_LIMITS`); wire the
  config through. Alternatively drop the config keys or document the depth
  limit substitution (spike follow-up notes cel 0.14 has no runtime cost API).
- [ ] F-40 / `engine.max_step_output` is parsed but never enforced — no output
  size cap on `StepOutput::Continue` payloads anywhere.

## 5. CLI gaps (PRD §3, §4.8, §9.7, Phase 4 tasks 4.6/4.7/4.8)

- [ ] **GitHub Actions OIDC token exchange is missing (US-1, 4.5)**:
  `Client::bearer_for` returns `None` for `AuthConfig::GithubActions`;
  `ci::oidc_request_environment` supplies the URL/token for detection only.
  Nothing calls `ACTIONS_ID_TOKEN_REQUEST_URL` to mint an ID token with
  audience `cargobike` (`--audience`), so CI creates go out unauthenticated.
- [ ] **`release watch` (4.7) missing entirely** — SSE client
  (`reqwest-eventsource`), `--until <environment>` (incl. `Skipped` = exit 0),
  exit codes 0/1/2/3/10/11/12 (10/11/12 exist in `client::exit_code` but the
  RequestBodyLimit/409 nuance is untested), `Last-Event-ID` reconnect,
  `indicatif` progress (non-TTY disabled). `create --wait` polls 1s instead
  (stopgap; watch must replace it, incl. server `/watch` SSE endpoint from 5.4).
- [ ] **`cargobike validate` (4.8, US-13) missing** — offline config/template
  validation reusing server `validation.rs` + engine `compile_with`, with
  `miette` diagnostics; no `--resolve` mode yet.
- [ ] `release retry <id> [--new]` missing (US-7/F-23; server endpoint too).
- [ ] `release approve` / approvals-submission UX missing (F-59; server endpoint is Phase 5.5 — keep in phase order).
- [ ] `release list --since` missing (US-2 accepted filters; server supports it).
- [ ] `release get --events` flag missing (US-3; server endpoint Phase 5.4).
- **Bug (F-108/F-145)**: `-o`/`--output` and `defaults.output` are resolved
  into `Resolved.output` but every verb hardcodes the format
  (`list` → `Table`, `create`/`get` → `Json`). `-o yaml` is silently ignored.
- [ ] `-o` lacks `CARGOBIKE_OUTPUT` env wiring (F-145); `--auth`/`CARGOBIKE_AUTH`
  override and `--ca-file`/`CARGOBIKE_CA_FILE` flag/env missing.
- [ ] CI env assembly: `ci::CiContext::from_environment` is never attached to a
  create request (US-1's auto-detect creates no annotations today; server is the
  authority on claims — verify the split before wiring).
- [ ] List paging: CLI sends no `after` cursor; deep listings stop at page 1.
- [ ] `~` expansion for `ca_file`/`api_key` file refs (the documented example
  stores `~/.config/cargobike/local-key` verbatim).

## 6. GitHub provider gaps (4.1–4.3)

- [ ] **GHES `api_url`** (F-44): `GithubProvider::new` builds on octocrab's
  default github.com base; config's `api_url` is never applied.
- [ ] **Per-repo installation lookup (F-122, 4.1)**: `GithubAuth::App` takes one
  static `installation_id`; no repo→installation resolution, no per-repo scoped
  token minting.
- [ ] Minimal GitHub App permission set: not documented (4.1 third bullet).
- [ ] `graphql_client` typed queries (4.3): absent — reconciler batching and
  compile-time schema checks; current GraphQL is an ad hoc
  `enablePullRequestAutoMerge` payload.
- [ ] `update_branch` returns `Unsupported` (F-46 includes it in the trait
  contract; deferred to a v1.1 step type — either implement the provider op or
  reconcile the PRD deviation).
- [ ] `pull_request.closed` normalisation drops `repository_id` and `sender`
  (the correlation needs repo_id; actor recording needs sender).
- [ ] Installation + repository allowlist check during webhook
  normalisation (4.4 / 8.1) — not present on the provider path (server receiver
  is Phase 5; the check needs a home there).

## 7. F-32a provision-time checks (registry ↔ template)

- [ ] "Every environment must supply every per-environment input" is enforced
  nowhere (engine compile doesn't see env_inputs; server doesn't provision).
  Also `inputs`/`env.inputs.*` provision stamping from the registry
  (`environments.<name>.repo/edits` as well-known `env.inputs`) lives only in
  the harness binary.
- [ ] Registry environment `change_request.{title,body,labels,draft}` appended
  to template `with` per R11 — labels/title flows partially hand-rolled in
  `builtin::ChangeRequest`; registry overrides not plumbed.

## 8. Auth / API polish (Phase 2 leftovers + §9)

- [ ] OIDC discovery (F-77, 2.5): server fetches `{issuer}/.well-known/jwks.json`
  directly and never reads the discovery document (`loose` jwks_uri); GitHub
  Actions' real JWKS path is `/.well-known/jwks` — default issuer breaks unless
  every entry sets `jwks_url`.
- [ ] JWKS refetch-on-unknown-`kid` with rate limiting: cache is TTL-only;
  unknown kid falls back to "single-key doc" heuristic.
- [ ] `CiContext` extraction from token claims (F-83) and F-84
  "verified values from claims, not body".
- [ ] Application groups (§4.13a, the planned 2.7a): discovery scan, virtual
  app materialisation, `{application}` placeholder resolution in edits/match,
  `extends` merge (maps deep, lists replace, `source` whole), ownership
  (`owner_id`) authorization in `authorize_create`, periodic rescan +
  SIGHUP rescan, name-collision error, discovered-name validation + skip
  logging, per-env custom `match`. Config shapes parse; nothing consumes them.
- [ ] `Idempotency-Key` header acceptance on create (F-109 second half;
  primary dedupe exists).
- [ ] Create response should also surface `paused`-application rejection
  (F-142) — `paused` shapes parse but enforce nowhere (per-app and per-env).
- [ ] Curated env overrides list (F-125) covers 6 scalars; PRD's curated list +
  documented surfaces (flags) not published — docs task.

## 9. Test coverage vs §14 (what's untested that must be)

- [ ] **Provider lifecycle over wiremock (4.9)**: `cargobike-provider-github`
  has no tests/ dir and no dev-dependencies (wiremock). Full lifecycle against a
  mocked GitHub API (create-branch → commit → CR → merge verify → close) is
  untested; only webhook signature/normalisation unit tests exist.
- [ ] `testcontainers` Postgres (PRD 1.4/2.9): absent from dev-deps; DB tests
  *skip* when `CARGOBIKE_TEST_DATABASE_URL` is unset, so a bare `cargo test`
  quietly zeroes integration coverage. Make `make test` self-sufficient.
- [ ] Interpreter status writes + phase rollup (blocked by §1 above).
- [ ] Supersede end-to-end (F-70/F-73/F-74): lease transfer is unit-tested;
  cancel-old-release + cleanup + comment-link chain is not (blocked by §0.4).
- [ ] Queue wake behaviour (F-71) — blocked by §3 queue wake.
- [ ] Approval auth matrix (F-96): self-approval, distinct count, reject,
  409-while-not-PendingApproval, attempt+head-SHA binding — approvals endpoint
  doesn't exist yet (Phase 5.5, but keep the CLI-side covered by 4.6/4.9 too).
- [ ] Watch exit-code matrix (US-4) incl. Skipped-success and reconnects —
  blocked by the watch command.
- [ ] Crash harness covers the action-step path only; waits (merge/approval)
  and reproducer for fork (`ForkFrom::LastFailure`) are untested (F-23,
  US-7 dependent).
- [ ] F-120 secrets-in-DBOS-state scan test (a test scanning the step-output
  table for known secret values) — not yet present anywhere.
- [ ] ETag/refetch-on-unknown-kid rate limiting unit tests.
- [ ] `${VAR:-default}` interpolation test (F-89): only plain `${VAR}` is
  tested; confirm shellexpand's grammar covers `:-` (it may not — then
  hand-roll before shipping).

## 10. Standing documentation tasks (from the project instructions)

- [x] Per-crate `README.md` ×7 (crate purpose + usage + examples).
- [x] Per-crate `AGENTS.md` ×7 (develop/test/document/contribute + evaluation criteria).
- [x] Global `README.md` rewrite (was a 2-line stub) referencing the crate docs.
- [x] Global `AGENTS.md` (repo-level) referencing the per-crate files.
- [ ] Keep both up-to-date per commit going forward (standing rule, now also
      encoded in the root `AGENTS.md` §"Standing documentation rule").
