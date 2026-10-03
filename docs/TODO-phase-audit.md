# Code audit vs PRD Phases 1–4 — TODO

> Built 2026-10-01 by reviewing every crate against `docs/PRD.md` (§16 Phase 1–4
> task lists, §14 test strategy, and the F-requirements they cover).
> Items are ordered by impact within each section. Checked boxes are done.

## 0. The one structural finding (blocks everything below)

- [x] **The server depends on `cargobike-engine` (+ the GitHub provider).**
  `crate::engine::host` builds the services from the config and boots DBOS
  at startup: the interpreter, the cleanup workflow and the reconciler
  loop register before `launch()`; the `releases` table's join over the
  `cr_correlation` rows is the production signal seam.
  - [x] The interpreter starts at create: the provision compiles the
    registry's template, stages the environment's inputs (repo/edits/
    commit-message/custom), computes the snapshot's content hash, and
    `start_with` addresses the deduplicating workflow id
    (`cargobike/interpret/<release_id>`).
  - [x] The cancel endpoint stops the workflow (`DBOS::cancel` on the
    attempt's id) and starts the cleanup (targets read the release's
    recorded CRs; the comment/branch/lease work runs as its own workflow).
  - [x] The supersede chain lands in the engine: on the won transfer the
    arriving release cancels the old attempt (the DBOS recordable
    cancel from inside the workflow), marks the old environment
    `Superseded` (the rollup's terminal), and starts the cleanup child
    with the hand-over (`superseded_by`; the deterministic child id
    joins replays). The services carry the DBOS handle + the cleanup
    ref, so the engine no longer needs the server to cancel.
  - [x] The provider instances build from the config
    (`GithubProvider::new_with_api_url`; the App's identity + a pinned
    `installation_id`; extension-served providers log and skip until the
    transport lands). The credentials resolve from the config's `secrets:`
    section. The HTTP seam is a first-cut egress guard (resolved-IP
    checks, redirects refused) — the full PRD's SSRF set (proxy, redirect
    re-check, allow-lists) is with 5.7.
  - [ ] The leader-election fence: no lock-loss watchdog yet; the
    standby's webhook-serving path needs the receiver (Phase 5).
  - [ ] Recovery: the app-version filter is set (the binary's version),
    but the recovery of pending workflows off a restart is untested here.

## 1. Release status is now written (PRD §4.1/§5.3, F-38)

- [x] The engine gained `status.rs`: the `ReleaseStatusStore` trait and
  its sqlx implementation. The interpreter records the attempt, the
  environment's running/waiting/pending-approval states, the CR
  reference, and every terminal phase; the release's phase is the
  rollup (canceled > superseded > failed > completed only if all
  terminal-ok > pending-approval > running). The writes run as durable
  steps and carry the conditional-update guard (`AND NOT terminal`),
  so the cancel's phase never gets resurrected by a stale write.
- [x] The release document served by `GET` now carries the live status
  (`status.environments`, the CR references, the attempts, the
  `Running` phase).
- [x] The set_phase document bug is fixed en route: the status store
  syncs the phase/terminal columns with the document's status text in
  every write.
- [x] The snapshot content hash is real: sha256 over the canonical
  JSON of the template + the release identity + the inputs + the
  pinned step-type versions (`snapshot_content_hash`).
- [ ] The remaining status surface: the SSE `watch` (Phase 5), the
  event-log writes (Phase 5), the attempts' `fork_from` values (the
  retry work), and the metadata's actor/CiContext half (the caller's
  identity lands with 4.5's OIDC exchange).

## 2. Provider-side security checks wired to the engine (PRD §4.5/§4.7/§4.12)

- [ ] **F-65 branch protection**: the `change-request` step never calls
  `Provider::check_branch_protection`; `require_branch_protection` is parsed in
  config but consulted nowhere. Must fail closed by default.
- [x] **The correlation stamp happens**: the interpreter stamps the
  `(provider, repo_id, cr_number) → (release, environment, wait-step,
  attempt)` row right after the change-request step's checkpoint (a
  durable, idempotent upsert), using the engine context's own workflow
  id; the environment's status carries the CR reference at the same
  boundary. The integration stamp the test used to simulate is gone —
  the engine's write is the path now.
- [ ] **F-95/F-4 version verification, remainder**: the scheme check at
  create is live (`VersionNotVerified` on a bad version; 6104d43) and the
  webhook's tag-push path extracts the version from `tag_format`. Still
  missing: per-shape verification — OIDC creates verify the SHA claim,
  API-key creates verify the tag exists, webhook creates verify the tag
  object's SHA (§3 US-1, §9.3).
- [x] **F-82 tag protection enforced at the trigger**: only one trigger
  exists today (the webhook's tag push); its creator runs
  `check_tag_protection` and skips fail-closed on the default-true rule
  (the receiver's E2E asserts it against a protection-incapable
  provider, and the provider's ruleset logic is wiremock-covered in
  4.9's lifecycle suite). Launchpad/other triggers: n/a in v1.
- [ ] **F-10 verified-path labels**: no startup `get_repo_path` check, no
  "app unavailable on mismatch" marking, no background retry, and no
  `cargobike validate --resolve` strict mode.

## 3. Queue wake, event log, and remaining Phase 3 seams

- [x] **Queue-policy wake (F-71)**: `release_lease` reads the status
  store's `waiting_releases(application, environment)` (the jsonb
  scan) and sends `Signal::LeaseReleased` to each waiting attempt's
  workflow over `lease/{environment}` with the lease-release event's
  idempotency key; a missed send degrades to the wake-timeout re-poll.
  The dedicated wake E2E (a queued release actually waking) is still
  untested (§9).
- [x] Reconciler production seam: `SqlSignalSource` over the `releases`
  join (`engine.rs`'s PendingReleaseSource impl), registered with the
  reconciler at boot.
- [ ] Reconciler efficiency: ETag conditional requests and batched GraphQL
  lookups (F-69, 4.3) — current sweep does per-row REST `get_change_request`.
- [ ] Cleanup workflow body: provider calls run directly in the workflow body,
  not inside `dbos::step` (no checkpoint/backoff bounds; deviations from §6.7).
  Make each compensation durable/retryable.
- [x] F-34 reservation: `StepRegistry::register` refuses `wait`-shaped
  control names and `builtin/`-prefixed types (`StepRegistryError::
  Reserved`); the engine's own install goes through
  `register_built_in`, and the unit test refuses all four shapes
  (`wait`, `wait: merge`, `wait/merge`, `builtin/http-call`) while an
  unreserved sidecar name still installs.

## 4. CEL / engine knobs (F-27, F-40)

- [x] The CEL knobs wire: `engine.cel.max_expression_length` flows from
  the server's boot into the interpreter services' `Limits` and the
  contexts check against it (a unit test pins the bound's refusal);
  `engine.cel.max_cost` stands for the depth bound — cel 0.14 exposes
  no runtime-cost API (the substitution is documented in the spike doc
  and boot-noted when a non-default is configured).
- [x] F-40 / `engine.max_step_output` is enforced: the interpreter's
  step dispatch checks the serialized output's byte size before the
  checkpoint; over the cap is a permanent step refusal (the
  integration test runs a 4-byte cap and asserts the release fails
  naming the cap).

## 5. CLI gaps (PRD §3, §4.8, §9.7, Phase 4 tasks 4.6/4.7/4.8)

- [x] **GitHub Actions OIDC token exchange works (US-1, 4.5)**:
  `ci::actions_token` exchanges the request token for an audience-scoped
  ID token (the flag/env/default audience); `Client::bearer_for` mints on
  the `github-actions` shape (unit + binary E2E against wiremock, incl.
  the request-token header and the audience query param).
- [ ] **`release watch` (4.7) missing entirely** — SSE client
  (`reqwest-eventsource`), `--until <environment>` (incl. `Skipped` = exit 0),
  exit codes 0/1/2/3/10/11/12 (10/11/12 exist in `client::exit_code` but the
  RequestBodyLimit/409 nuance is untested), `Last-Event-ID` reconnect,
  `indicatif` progress (non-TTY disabled). `create --wait` polls 1s instead
  (stopgap; watch must replace it, incl. server `/watch` SSE endpoint from 5.4).
- [ ] **`cargobike validate` (4.8, US-13) missing** — offline config/template
  validation reusing server `validation.rs` + engine `compile_with`, with
  `miette` diagnostics; no `--resolve` mode yet.
- [x] `release retry <id> [--new]` landed: the server's `POST
  …/releases/{id}/retry` forks the failed attempt in place
  (`ForkFrom::LastFailure` reads the rows' error column; the attempt
  record appends with `fork_from`; the re-arm drops the row's
  terminal flag), `--new` copies to a fresh row with `retried_from`;
  the CLI verb covers both (guards: 404 / non-failed 409).
- [ ] `release approve` / approvals-submission UX missing (F-59; server endpoint is Phase 5.5 — keep in phase order).
- [x] `release list --since` present: an RFC 3339 stamp passes through; a
  humantime duration (`24h`) converts to now-minus (query encoding is a
  small private helper; the server's refusal surfaces unknown inputs).
- [ ] `release get --events` flag missing (US-3; server endpoint Phase 5.4).
- [x] **`-o` obeys the format now (F-108/F-145)**: every verb renders the
  resolved format; `CARGOBIKE_OUTPUT` fills in when the flag is missing
  (E2E: `-o json`, `-o yaml`, the env format on list/get).
- [x] `--auth`/`CARGOBIKE_AUTH` (none|api-key|exec|github-actions; exec keeps
  the context's shape, api-key materializes CARGOBIKE_API_KEY{,_FILE}) and
  `--ca-file`/`CARGOBIKE_CA_FILE` + `~`-expansion on CA/key file refs.
- [ ] CI env assembly: `ci::CiContext::from_environment` is never attached to a
  create request (US-1's auto-detect creates no annotations today; server is the
  authority on claims — verify the split before wiring).
- [ ] List paging: CLI sends no `after` cursor; deep listings stop at page 1.
- [x] `~` expansion for `ca_file`/key/CA file refs: the resolution walk
  tilde-expands the context's `ca_file`, the flag's `--ca-file`, and the
  api-key override's `CARGOBIKE_API_KEY_FILE`; the client's secret-ref
  materialisation expands too (unit + E2E). The config-doc example no
  longer lies (the README texts note the expansion).

## 6. GitHub provider gaps (4.1–4.3)

- [x] **GHES `api_url`** (F-44): the config's `api_url`/`web_url` feed
  `GithubProvider::new_with_api_url` at the server's provider build
  (engine.rs); GHES builds verified by the provider's unit tests.
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
- [x] `pull_request.closed` normalisation carries `repository_id` (the
  `base.repo.id`, `repository.id` fallback) and the `sender.login` — the
  correlation's full key and the actor both survive; the normalise lives
  in `cargobike_core::webhook::normalise_github` (the provider + the
  mock share it).
- [ ] Installation + repository allowlist check during webhook
  normalisation (4.4 / 8.1) — the receiver now exists and the tag-push
  creator's app-source scan is the first cut (only apps whose
  `source.id` match react); the `providers[].repositories` globs and the
  installation lookup are still unwired.

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

- [x] **Provider lifecycle over wiremock (4.9)**:
  `crates/cargobike-provider-github/tests/lifecycle_wiremock.rs` runs the full
  lifecycle against a mocked GitHub API (metadata by immutable id, branch
  create/read, fused edit+commit plus its no-op replay, pull create-twice,
  labels, comments, statuses, protection checks, GraphQL auto-merge, tag and
  release, branch delete). Found + fixed a real bug en route: the GraphQL
  result check now looks for the `errors` member (octocrab unwraps `data`
  itself).
- [ ] `testcontainers` Postgres (PRD 1.4/2.9): absent from dev-deps; DB tests
  *skip* when `CARGOBIKE_TEST_DATABASE_URL` is unset, so a bare `cargo test`
  quietly zeroes integration coverage. Make `make test` self-sufficient.
- [x] Interpreter status writes + phase rollup: the rollup's unit tests
  (incl. the queue-Waiting→Pending fix) + the suites exercise the writes
  end-to-end (the attempts, the CR stamps, the terminals).
- [x] Supersede end-to-end (F-70/F-73/F-74): `test_supersede_cancels_the_
  old_attempt_and_cleans_up` — the lease's holder becomes B, A's release
  goes Superseded/terminal, A's CR closes and B's stays open.
- [ ] Queue wake behaviour (F-71) — the send is wired; a QUEUED release's
  wake-from-block E2E (enter → HeldBy → the queue's hold → the release's
  wake → the run continues) is still untested.
- [ ] Approval auth matrix (F-96): self-approval, distinct count, reject,
  409-while-not-PendingApproval, attempt+head-SHA binding — approvals endpoint
  doesn't exist yet (Phase 5.5, but keep the CLI-side covered by 4.6/4.9 too).
- [ ] Watch exit-code matrix (US-4) incl. Skipped-success and reconnects —
  blocked by the watch command.
- [ ] Fork reproducer (`ForkFrom::LastFailure`, retry) is untested (US-7
  dependent). A stalled wait's wake/verify/complete half is covered in-process
  by the reconciler suite (`tests/engine_integration/`); a cross-process kill
  during a wait remains unprobed (the milestone hook fires at action steps
  only).
- [x] The webhook receiver's E2E (5.1/5.2):
  `crates/cargobike-server/tests/webhook_receiver.rs` — the signed
  tag-push, the closed-PR correlation driving the wait to `Completed`,
  a 401/404/ignored/dedupe.
- [x] F-120 secrets-in-DBOS-state scan test — the secrets invariant in
  `tests/engine_integration/`: a real interpreter pass records the returned
  request to prove the secret's flow, then scans `operation_outputs` and
  `workflow_status` (inputs/output/error) for the material.
- [ ] ETag/refetch-on-unknown-kid rate limiting unit tests.
- [x] `${VAR:-default}` interpolation test (F-89): shellexpand 3.1.2 covers
  the default syntax; the server config test suite asserts it.

## 10. Standing documentation tasks (from the project instructions)

- [x] Per-crate `README.md` ×7 (crate purpose + usage + examples).
- [x] Per-crate `AGENTS.md` ×7 (develop/test/document/contribute + evaluation criteria).
- [x] Global `README.md` rewrite (was a 2-line stub) referencing the crate docs.
- [x] Global `AGENTS.md` (repo-level) referencing the per-crate files.
- [ ] Keep both up-to-date per commit going forward (standing rule, now also
      encoded in the root `AGENTS.md` §"Standing documentation rule").
