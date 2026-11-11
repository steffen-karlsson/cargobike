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
- [x] The remaining status surface: the event-log writes and the SSE
  `watch` landed (Phase 5.4's server half); the CLI's `release watch`
  consumer is the open §5 item below.
  - F-111/F-113: `cargobike-engine/src/eventlog.rs` — every transition
    appends (the interpreter's `status_write` seam, one event per
    durable step with a DETERMINISTIC id `{release}/{attempt}/{label}`
    and `ON CONFLICT (id) DO NOTHING`, so a re-executed step converges
    on the first append; the API's create/cancel seams and the
    supersede/reopen paths append directly). Appends serialise per
    release on the release row lock (`FOR UPDATE`), assign the
    per-release sequence under the lock, and chain
    `hash = sha256(prev || canonical-json)` (second-precision `at` —
    Postgres keeps microseconds, so a nanosecond would hash
    differently than the row stores; both read the same
    `OffsetDateTime`). Migration `0006` adds the `prev`/`hash`
    columns. The known trade-off, documented in code: a FAILED append
    warns and does not fail the write (the status fact is primary;
    the release's liveness wins).
  - F-105/F-116: `GET /api/v1/releases/{id}/events` — cursor-paged by
    the per-release sequence (`after`/`limit`), every item carrying
    `hash`/`prev` so a reader re-verifies the chain (both E2Es do:
    the engine supersede test and the authed lifecycle recompute the
    hashes).
  - F-102: `GET /api/v1/releases/{id}/watch` — SSE over the same
    table (a 250ms poll; `Last-Event-ID` resumes at the break, each
    frame's `id:` is the sequence, `event:` the kind), closing once
    the release is terminal, with a settle drain first (the terminal
    flag flips microseconds before the cancel's own event appends —
    the only post-terminal writer). The authed lifecycle's E2E
    streams a live release, cancels it mid-stream, and asserts the
    events and the close.
- [x] The status's actor/CiContext half (F-83/F-84): the create stamps
  `status.actor` (the verified `(issuer, subject)` + display name)
  and `status.ci` (claims-built; the Actions provider label keys on
  the real issuer) into the release document; the webhook path
  records the verified delivery's sender (F-97).

## 2. Provider-side security checks wired to the engine (PRD §4.5/§4.7/§4.12)

- [x] **F-65 branch protection enforced at the change-request step**: the
  registry's `require_branch_protection` (default `true`) rides the
  snapshot's per-environment inputs; the step's create path checks the
  provider's capability first (an incapable provider is a refusal, not
  a skip), then `check_branch_protection` on the base branch — no
  protection record or no required reviews is a permanent failure. The
  gate runs only when the step is about to create: a recovery replay
  that finds its own CR never re-fails on a mid-flight protection
  change (F-19 safety). Unit matrix in `builtin.rs` (capable/incapable,
  protected/unprotected, opt-out, found-CR replay); the mock provider
  models a review-protected `main` so every lifecycle path runs the
  gate.
- [x] **The correlation stamp happens**: the interpreter stamps the
  `(provider, repo_id, cr_number) → (release, environment, wait-step,
  attempt)` row right after the change-request step's checkpoint (a
  durable, idempotent upsert), using the engine context's own workflow
  id; the environment's status carries the CR reference at the same
  boundary. The integration stamp the test used to simulate is gone —
  the engine's write is the path now.
- [x] **F-95/F-4 version verification, remainder**: the scheme check at
  create is live (`VersionNotVerified` on a bad version; 6104d43) and the
  webhook's tag-push path extracts the version from `tag_format`. The
  per-shape verification is wired at the create's choke point: the
  provider trait grows `tag_sha` (the GitHub impl dereferences annotated
  tags to their commit; the mock serves seeded state, `seed_tag`),
  `ProviderCaps.tag_lookup` gates it and the create FAILS CLOSED when a
  `require_tag` app meets an incapable provider. OIDC creates verify the
  token's `sha` claim against the tag's commit, API-key creates demand
  "tag exists" (or verify a body-supplied SHA), webhook creates verify
  the payload's SHA against the provider's tag object (§3 US-1, §9.3).
  Roundtrip coverage: the oidc suite seeds the mock's tag and asserts a
  matching claim creates while a mismatching claim refuses with 400
  `VersionNotVerified`; the webhook suite's pushes seed their tags; the
  sighup fixture (a spawned process, no provider) runs with
  `require_tag: false`.
- [x] **F-82 tag protection enforced at the trigger**: only one trigger
  exists today (the webhook's tag push); its creator runs
  `check_tag_protection` and skips fail-closed on the default-true rule
  (the receiver's E2E asserts it against a protection-incapable
  provider, and the provider's ruleset logic is wiremock-covered in
  4.9's lifecycle suite). Launchpad/other triggers: n/a in v1.
- [x] **F-10 verified-path labels** (the `cargobike validate --resolve`
  half stays with the `validate` item below — `validate` does not
  exist yet, so the strict mode rides that build): the boot check
  runs before the API serves — the declared `source.path` is compared
  against the provider's `get_repo_path` over a PATH-LESS probe (the
  GitHub provider short-circuits a declared label to itself, so the
  probe must not carry it); mismatch, provider error, or an unknown
  provider = the application is *unavailable*, fail-closed (the
  server does NOT fail). Unavailable apps: the create refuses 409
  naming the reason (the idempotency-key replay still answers — the
  caller owns that release), `GET /api/v1/applications` (F-106's
  first half, now real; `/templates` still pending) surfaces the
  state, and the background sweep
  (`reconciler.verified_path_interval`, default 1m, configurable so
  the E2E watches the heal converge in ~1s) re-verifies against the
  CURRENT config — a renamed-back repo heals without a restart.
  Case-insensitive compare (GitHub paths are case-preserving but
  case-insensitive; a case-only mismatch would be a permanent false
  alarm). `cargobike application list` renders the state (CLI F-106
  half). The mock provider grew scripted repo paths
  (`set_repo_path`, `fail_next_path_lookup_for` — one-shot,
  repo-keyed like the commit failure). E2E:
  `test_mismatched_path_label_refuses_then_heals` (boot mismatch →
  listing + refusal → rename-back → sweep heals → create passes →
  drained); unit: the availability module's seven cases.

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
- [x] Cleanup workflow body: each compensation (CR close, branch
  delete, lease settle) now runs inside its own `dbos::step` with a
  bounded retry policy (transient provider wobble retries 3×; a
  permanent failure is recorded and the cleanup moves on; an
  already-gone CR/branch is converged, not an error). A crash
  mid-cleanup resumes at the same compensation instead of re-running
  provider work.
- [x] F-34 reservation: `StepRegistry::register` refuses `wait`-shaped
  control names and `builtin/`-prefixed types (`StepRegistryError::
  Reserved`); the engine's own install goes through
  `register_built_in`, and the unit test refuses all four shapes
  (`wait`, `wait: merge`, `wait/merge`, `builtin/http-call`) while an
  unreserved sidecar name still installs.
- [x] Implicit set-status on concurrency refusals (F-38/F-74): a
  `reject` policy (or the supersede guard's refusal) used to exit the
  workflow silently — the release stayed `Running` forever. The
  enter-failure paths now write the environment's terminal `Failed`
  with `ConcurrencyRejected` (`test_reject_policy_fails_the_environment_
  in_the_document`); the integration tests also wipe their own lease
  rows, so reruns against the shared fixture DB no longer meet a
  version-less leftover holder.

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
  (stopgap; watch must replace it — the server's `/watch` SSE endpoint
  landed in Phase 5.4, see §1's tick; only the CLI consumer remains).
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
- [x] CI env assembly: `ci::CiContext::from_environment` now rides the
  create body as UNTRUSTED annotations — `CiContext::annotations()`
  (`ci`, `ci.repository`, `ci.sha`, `ci.run-id`, kebab-case keys), the
  CLI's `release create` attaches them when the Actions environment is
  present, and the server stores the flat string map verbatim in
  `metadata.annotations` (F-84's whole shape: the verified claims stay
  the authority — a body value can become nothing else). The refusal
  is strict: a structured annotation value or a non-object
  `annotations` field is 400 `InvalidRequest` naming the annotation;
  the webhook and retry paths carry none. E2E: the authed lifecycle's
  create roundtrips the annotations and refuses the structured value;
  the CLI's wiremock create asserts the annotated body inside the
  Actions environment (the `body_partial_json` matcher is the
  whole proof).
- [x] List paging: the CLI's list walks the cursor — while a page
  says `has_more`, the next request carries `after=<cursor>` and the
  items accumulate until the pages run out (the rendered output is
  the union). E2E: a wiremock two-page surface asserts both pages'
  rows render.
- [x] `~` expansion for `ca_file`/key/CA file refs: the resolution walk
  tilde-expands the context's `ca_file`, the flag's `--ca-file`, and the
  api-key override's `CARGOBIKE_API_KEY_FILE`; the client's secret-ref
  materialisation expands too (unit + E2E). The config-doc example no
  longer lies (the README texts note the expansion).

## 6. GitHub provider gaps (4.1–4.3)

- [x] **GHES `api_url`** (F-44): the config's `api_url`/`web_url` feed
  `GithubProvider::new_with_api_url` at the server's provider build
  (engine.rs); GHES builds verified by the provider's unit tests.
- [x] **Per-repo installation lookup (F-122, 4.1)**: `GithubAuth::App`'s
  `installation_id` is now `Option` — present = the pinned static
  client (today's behaviour, AND the webhook receiver's installation
  allowlist, F-51); ABSENT = per-repo resolution: every call resolves
  `GET /repositories/{id}/installation` on the app-level client and
  mints a token scoped to that single repository
  (`create_installation_access_token` with `repository_ids: [id]` +
  the README's permission table as the `permissions` subset — a mint
  can only NARROW the installation's grants). The minted clients
  cache per repo id until a minute before expiry and INHERIT the
  provider's `api_url` base (a fresh builder would default to
  github.com — found the hard way in the wiremock test, which was
  calling the real GitHub). The provider's `from_octocrab` Deref
  shortcut is gone (no single client exists in per-repo mode); every
  trait method resolves its client through `client_for` first (the
  static shapes return the one client, zero overhead). Wiremock E2E:
  `test_per_repo_calls_mint_a_repository_scoped_token` — the mint
  body asserts `repository_ids` + the permission subset, the op rides
  the minted token (`Authorization: Bearer ghs_minted_for_42`), and
  the second call re-mints nothing (the cache holds).
- [x] Minimal GitHub App permission set (4.1/F-66): documented in the
  provider's README — the permission table (metadata/contents/
  pull requests/issues/commit statuses/administration/rulesets), the
  webhook note (signature verification needs no API permission),
  the `allow_direct_commit` bypass caveat, and the installation-scope
  recommendation.
- [ ] `graphql_client` typed queries (4.3): absent — reconciler batching and
  compile-time schema checks; current GraphQL is an ad hoc
  `enablePullRequestAutoMerge` payload.
- [x] `update_branch` returns `Unsupported` (F-46): reconciled as a
  documented v1 deviation — no v1 step type consumes the op (the
  `update-branch` step is v1.1), and octocrab 0.54 has no wrapper for
  the branch-update endpoint (its `merge_upstream` is fork-only), so
  the implementation (raw request or git-refs fast-forward) lands
  WITH its consumer. The deviation is spelled out in the provider's
  README.
- [x] `pull_request.closed` normalisation carries `repository_id` (the
  `base.repo.id`, `repository.id` fallback) and the `sender.login` — the
  correlation's full key and the actor both survive; the normalise lives
  in `cargobike_core::webhook::normalise_github` (the provider + the
  mock share it).
- [x] Installation + repository allowlist check during webhook
  normalisation (4.4 / 8.1): the normalised event now carries the
  delivery's repository path (the tag push's `repository.full_name`;
  the CR close's `base.repo.full_name` with the delivery repository's
  as fallback) and its `installation.id` — both `#[serde(default)]`,
  so old persisted events stay readable — and the receiver enforces
  F-49/F-51/F-92 on them: a pinned `auth.installation_id` refuses
  foreign installations; `repositories.deny` globs win;
  `repositories.allow` is deny-by-default (a path-less delivery can
  never match — fail-closed); no pin and no `repositories` block =
  the unscoped v1 default. Out-of-list deliveries are acknowledged
  202-and-ignored (the signature already proved the sender — a
  violation is a mis-scoped App, not a probe to give feedback to)
  with the reason logged. globset with `literal_separator` (F-92's
  anchoring). Unit: `http::tests` covers unscoped/pin/deny-wins/
  pathless; E2E: the receiver suite's allowlist boot (allow +
  deny-list injected) drives four deliveries — accepted, not-allowed,
  deny-wins, path-less — and only the allowed one creates a release
  (run-unique versions: the shared fixture DB's non-terminal dedupe
  would otherwise turn the accepted create into a duplicate); the
  accepted release drains (CR-recorded, cancel, delete).

## 7. F-32a provision-time checks (registry ↔ template)

- [x] "Every environment must supply every per-environment input"
  (F-32a) is enforced at the create's choke point: the provision
  checks the staged env inputs against the template's declared
  `environment_inputs` (nulls count as missing) and refuses with 400
  naming the environment + the input. The second half of the old
  claim was stale — the registry's `repo`/`edits`/custom-inputs /
  app-wide `inputs` stamping lives in the SERVER's provision
  (`provision_snapshot_parts`), not only the harness binary; the
  interpreter resolves `${{ env.inputs.* }}` off the staged
  snapshot. E2E: `bare-service` (an environment without the
  template's `repo` input) refuses the create.
- [x] Registry environment `change_request.{title,body,labels,draft}` appended
  to template `with` per R11: the provision stamps the registry's
  block into the snapshot (the same well-known env-input ride as
  `require_branch_protection`), `EnvRef` carries it as
  `ChangeRequestEnv`, and the `change-request` step applies R11 —
  title/body REPLACE the template's `with` values, labels APPEND to
  the template's list, draft forces the flag (F-144's `with.draft`
  is read too, both YAML shapes). The trait's
  `create_change_request` grows the `draft` param (octocrab's
  create-PR builder carries it; the mock records title/labels/draft
  for the E2E read). En route, F-147's missing halves for these
  fields landed: the provision RESOLVES the placeholders
  ({application}/{environment}/{version}/{release_id}/{release_url} +
  `{{`/`}}` escapes; commit_message included — it also resolved
  nothing before) and boot validation refuses unknown placeholders
  in `commit_message` and `change_request.{title,body}`.

## 8. Auth / API polish (Phase 2 leftovers + §9)

- [x] OIDC discovery (F-77, 2.5): the JWKS URL resolves through the
  issuer's discovery document (`{issuer}/.well-known/openid-configuration`
  → `jwks_uri`, cached 1h per issuer) — the GitHub Actions issuer's real
  `/.well-known/jwks` path works without a `jwks_url` override now; the
  `jwks_url` entry override still wins, and the legacy
  `{issuer}/.well-known/jwks.json` guess is the logged fallback for
  issuers without discovery.
- [x] JWKS refetch-on-unknown-`kid` with rate limiting: an unknown kid
  forces one refetch per URL per 30s window (a `moka` entry marker),
  then the selection (kid match, single-key fallback) runs again; a
  rotation validates without waiting out the 5m cache TTL and a kid
  storm cannot hammer the issuer (`tests/oidc_roundtrip.rs` counts
  the JWKS hits: 1 cached, 2 after the rotation, still 2 after the
  storm probe — also the §9 rate-limit test item).
- [x] `CiContext` extraction from token claims (F-83) and F-84
  "verified values from claims, not body": a token carrying
  `repository_id` describes a CI run, so `decode_oidc` builds the
  `CiContext` (provider label keyed on the issuer — the real Actions
  issuer → `github-actions`, else `oidc`; repository/workflow_ref/
  run_id off claims; run_url synthesised from the verified pair for
  Actions) and the create stamps `status.actor` (the verified
  `(issuer, subject)` + display name) and `status.ci` into the release
  document. The webhook path records the verified delivery's sender
  as the actor (issuer `webhook/{provider}`, F-97). The roundtrip test
  runs the REAL Actions issuer through the `jwks_url` override and
  asserts both status halves; en route its silent boot-skip was fixed
  (the stub template lacked `environments`), and the single-key JWKS
  heuristic now answers only kid-less headers so a declared unknown
  `kid` reaches the rotation refetch.
- [ ] Application groups (§4.13a, the planned 2.7a): discovery scan, virtual
  app materialisation, `{application}` placeholder resolution in edits/match,
  `extends` merge (maps deep, lists replace, `source` whole), ownership
  (`owner_id`) authorization in `authorize_create`, periodic rescan +
  SIGHUP rescan, name-collision error, discovered-name validation + skip
  logging, per-env custom `match`. Config shapes parse; nothing consumes them.
- [x] `Idempotency-Key` header acceptance on create (F-109 second
  half): the key precedes everything — a retried create with the same
  key converges on the first attempt's release with 200 (whatever the
  body), a fresh key records `key → release` after the insert
  (first writer wins, `ON DELETE CASCADE` follows the release out so a
  deleted release's key provisions fresh). The lifecycle E2E covers
  202 → same-key 200 → delete → same-key 202.
- [x] `paused`-application rejection (F-142) enforced at the create: a
  paused application (or a release whose template contains a paused
  environment) is refused with 409 `StateConflict`, the detail naming
  what is frozen and the registry's reason. En route the audit's own
  claim was corrected: the `paused` shapes did NOT parse (an
  externally-tagged enum); they are untagged now (`paused: true` /
  `paused: { reason: ... }`, per-app and per-env). Deviation, called
  out: v1 refuses the create rather than freezing an in-flight
  release at the paused environment — snapshot pinning means a
  mid-flight pause needs a live pause/wake control plane, which is
  post-v1 work.
- [x] Curated env overrides list (F-125): the published list lives
  in the server README's "Curated overrides" table — the three flags
  (`--config`/`--listen`/`--public-url`), the six scalar envs
  (`CARGOBIKE_SERVER_{LISTEN,PUBLIC_URL,DATABASE_URL,DATABASE_URL_FILE,
  LOG_LEVEL,LOG_FORMAT}`), the bootstrap key pair, and the four
  GitHub provider env stamps — flag > env > file > default, the
  `*_FILE` paths read (never interpolated).

## 9. Test coverage vs §14 (what's untested that must be)

- [x] **Provider lifecycle over wiremock (4.9)**:
  `crates/cargobike-provider-github/tests/lifecycle_wiremock.rs` runs the full
  lifecycle against a mocked GitHub API (metadata by immutable id, branch
  create/read, fused edit+commit plus its no-op replay, pull create-twice,
  labels, comments, statuses, protection checks, GraphQL auto-merge, tag and
  release, branch delete). Found + fixed a real bug en route: the GraphQL
  result check now looks for the `errors` member (octocrab unwraps `data`
  itself).
- [x] `testcontainers` Postgres (PRD 1.4/2.9): done as the dev-only
  `cargobike-testsupport` crate (a new workspace member, never a
  `[dependencies]` entry of a product crate). Precedence:
  `CARGOBIKE_TEST_DATABASE_URL` (CI) wins, else ONE throwaway
  Postgres 17 container per test process (`testcontainers` +
  `testcontainers-modules`, `postgres:17-alpine` — parity with the
  spike script; the handle is leaked so the container lives for the
  process, the async cell ensures one per process, the container work
  runs on the blocking pool because a nested runtime cannot start on a
  runtime thread), else the tests **skip with the reason printed** —
  the silent skip (a bare `cargo test` quietly zeroing integration
  coverage) was the failure mode. Consumers: the engine suite's
  fixture + `leases_behaviour`, the server's five DB suites — all raw
  `std::env::var` reads routed through `database_url()`. The container
  path needs a Docker host (verified locally: with none, the suites
  degrade to the printed skip; with a container host they
  provision — the same code path, only the client differing).
- [x] Interpreter status writes + phase rollup: the rollup's unit tests
  (incl. the queue-Waiting→Pending fix) + the suites exercise the writes
  end-to-end (the attempts, the CR stamps, the terminals).
- [x] Supersede end-to-end (F-70/F-73/F-74): `test_supersede_cancels_the_
  old_attempt_and_cleans_up` — the lease's holder becomes B, A's release
  goes Superseded/terminal, A's CR closes and B's stays open.
- [x] Queue wake behaviour (F-71): the E2E lives in the engine's
  integration suite (`test_queue_wakes_when_the_lease_releases`) —
  A acquires the lease and blocks at its merge wait, B queues
  (`Waiting`, the holder still A), A's merge is noticed by the
  reconciler sweep (the merge wait is signal-driven), A completes,
  and the lease release sends `LeaseReleased` to B's waiting attempt:
  the holder flips to B and B's own CR opens (the run continued, not
  the 3600s wake-timeout re-poll).
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
- [x] ETag/refetch-on-unknown-kid rate limiting unit tests — the kid half
      is covered (`tests/oidc_roundtrip.rs`'s hit counter); the reconciler's
      ETag half is with the F-69 efficiency item below.
- [x] `${VAR:-default}` interpolation test (F-89): shellexpand 3.1.2 covers
  the default syntax; the server config test suite asserts it.

## 10. Standing documentation tasks (from the project instructions)

- [x] Per-crate `README.md` ×7 (crate purpose + usage + examples).
- [x] Per-crate `AGENTS.md` ×7 (develop/test/document/contribute + evaluation criteria).
- [x] Global `README.md` rewrite (was a 2-line stub) referencing the crate docs.
- [x] Global `AGENTS.md` (repo-level) referencing the per-crate files.
- [x] Keep both up-to-date per commit going forward (standing rule, now also
      encoded in the root `AGENTS.md` §"Standing documentation rule") —
      the rule is mechanically enforced (`make docs-check` refuses a
      changeset that touches `crates/<crate>/` without the crate's
      README/AGENTS pair; the pre-commit hook runs the check on the
      staged diff), and every commit this audit has produced updated
      the touched crates' pairs in the same commit. The item is a
      process invariant, and the invariant is now carried by the
      tooling, not by memory.
