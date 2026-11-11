# cargobike-engine

_The durable release interpreter over DBOS: templates in, environments progressed out._

## Purpose

`cargobike-engine` turns a validated pipeline template into a durable,
crash-resilient execution. It depends on `cargobike-core` only (plus `dbos`,
`sqlx`, `cel`); it knows nothing about axum or the API crate. The server
constructs the engine's services and starts releases (see
[the audit TODO](../../docs/TODO-phase-audit.md) §0 for the wiring status).

What lives here (PRD §16 Phase 3 tasks mapped to modules):

| Module | Contents | Task |
|---|---|---|
| `template` | YAML template compiler: structure validation, `include:` expansion, auto/declared step IDs, wait-rule bounds, scheme-specific gate checks  | 3.1 |
| `expr` | CEL runtime for `when:` gates and `${{ ... }}` parameters; pure functions only; length/depth limits (the config's `engine.cel.max_expression_length` wires through the context's `Limits`; `max_cost` stands for the depth bound — cel 0.14 has no runtime-cost API, documented per boot)  | 3.2 |
| `steps` | Versioned `StepRegistry` (`name@version`, versions side by side); registration refuses the reserved surface (`wait`-shaped control names and the `builtin/` namespace — a sidecar cannot shade them; the engine's own install goes through `register_built_in`) | 3.3 |
| `builtin` | The four action steps: `commit-files@1` (fused edit+commit per the idempotency strategy table), `change-request@1` (find-by-head first, then the F-65 fail-closed gate: the base branch must require reviews when the registry's `require_branch_protection` stands — default — and a provider without the capability is a refusal, not a skip; the registry's R11 `change_request` block replaces the template's title/body, appends its labels and forces the draft flag), `http-call@1`, `set-labels@1` | 3.5–3.8 |
| `interpreter` | The `cargobike.interpret.v2` durable workflow (F-40's output cap rides the services: a step's serialized output over `engine.max_step_output` is a permanent step refusal): control steps (`wait: merge` recv + provider re-verify + content verification, `wait: approval`, `wait: sleep`), snapshot-pinned execution, retry→`StepOptions` mapping | 3.4, 3.9, 3.11 |
| `signals` | Topic vocabulary (`merge/{env}/{step}`, `approval/{env}/{step}`, `lease/{env}`), typed `Signal` envelopes, idempotency keys, `Forks::Skip` decision  | 3.10 |
| `leases` / `concurrency` | Per-`(application, environment)` lease rows; `supersede` atomic transfer with the version-order guard, `queue` blocking, `reject` (the lease serialisation design); a policy refusal is part of the implicit set-status (F-38): it writes the environment's terminal `Failed` with `ConcurrencyRejected` — the release never stays `Running` while its workflow has exited | 3.12 |
| `cleanup` | `cargobike.cleanup.v1`: close CR with remarked comment, delete branch, release/transfers lease — each compensation its own `dbos::step` (a crash mid-cleanup resumes at the same compensation; transient provider wobble retries 3×, permanent failures are recorded and the cleanup moves on; §6.7) | 3.13 |
| `reconciler` | `cargobike.reconcile.v1`: durable loop sending merge/closed signals only (never writes status); the sweep batches its page's rows per provider through `Provider::get_change_requests` (F-69) | 3.14 |
| `correlation` | `(provider, repo_id, cr_number) → (release, environment, step, attempt)` rows for webhook correlation | 3.6 |
| `retry` | `cargobike.retry.v1` (F-23/US-7): the failed release's fork — the resolve reads `ForkFrom::LastFailure` off the rows' error column, the fork inherits the recorded steps below it and re-runs the failed step; the attempt record appends with `fork_from` | US-7 |
| `webhook` | `cargobike.webhook.v1` (Phase 5.1/5.2): the receiver's durable follow-through — a tag push creates the release(s) via the server's `TagPushCreator` seam (one durable step), a `pull_request.closed` looks up the correlation row and sends the same signal the reconciler sends | 5.1, 5.2 |
| `names` | Deterministic branch names via `engine.branch_format` format strings | 3.5 |
| `snapshot` | `ReleaseSnapshot`: compiled template + registry inputs + pinned step-type versions + hash  | 3.9 |
| `eventlog` | The immutable release event log (F-111/F-113): the `SqlEventLog` writes every transition (the interpreter's `status_write` seam appends one event per durable step with a DETERMINISTIC id, so a re-executed step converges on the first append; the server's create/cancel seams and the supersede/reopen paths append directly); appends serialise per release on the release row lock, assign the per-release sequence (the SSE id) under the lock, and chain `hash = sha256(prev ‖ canonical-json)` for tamper evidence; `list` reads the pages the server's `/events` and `/watch` endpoints serve | 5.4 |
| `status` | The `ReleaseStatusStore` trait + the sqlx store: the interpreter's status writes (attempt/running/waiting/pending-approval/CR reference/terminal + the release's rollup), conditional updates so a terminal release never re-phases; `load_document`/`environments_posted` (every provisioned environment opens `Pending` so a partial start can never roll `Completed`) and `release_reopened` (the F-23 fork's re-arm) |
| `crash` + `mock` + `bin/engine-harness` | Feature-gated crash hooks (`--features crash-hooks`) and the harness binary (the action-step milestone kills, plus the parked-wait reproducer: a raw SIGKILL while the workflow sits in `wait: merge`'s durable recv, the CR merging in the meantime, and the recovery's replay + re-park + reconciler wake to `Completed` — the harness boots the reconciler, seeds the release row (boot-unique version), runs a per-boot DBOS schema (a leaked holder must never adopt another boot's pending rows), and the driver wipes its app's lease rows before each case and kills its children on unwind); the mock provider is a small honest git host — state files with path-keyed texts, stateful change requests (a merged CR lands the head files on the base branch; the summary records title/labels/draft — the registry-override E2E reads them), tags the F-95 version policy reads (`seed_tag` stages them; `tag_sha` is the verification's truth), scripted repository paths the F-10 verified-path check reads (`set_repo_path`, `fail_next_path_lookup_for`), and webhook verification that mirrors the production HMAC contract; the one-shot commit failure is repo-keyed (`fail_next_commit_for`), so an adopted foreign replay cannot spend a test's flag | (the crash-injection harness) |

## Status

The engine runs its full release reach: the interpreter writes the
release's status at every boundary (attempt, running, waiting, the CR
reference + the correlation stamp, each terminal phase and the rollup)
via the `status` store, and the server's boot consumes the same services.
Known gaps (tracked in [`docs/TODO-phase-audit.md`](../../docs/TODO-phase-audit.md)):
the queue-wake sends, the supersede chain's cancel-and-cleanup half, the
production signal seam's ETag/GraphQL batching, and the full SSRF guard.

## Usage

Compile and inspect a template:

```rust
use cargobike_engine::template::{compile_with, CompiledTemplate};
use cargobike_core::version::VersionScheme;

let compiled: CompiledTemplate = compile_with(
 &yaml_source,
 &VersionScheme::Semver,
 &installed_steps, // the server-installed StepRegistry
)?;
```

Register and start the interpreter on a running DBOS instance:

```rust
let services = cargobike_engine::InterpreterServices {
 steps: Arc::new(steps), // built-ins registered via register_builtins
 providers: Arc::new(providers), // ProviderRegistry, resolved by RepoRef.provider
 credentials: Arc::new(credstore), // resolves { secret: <name> }
 http: Arc::new(ssrf_client), // HttpService seam
 leases: Arc::new(cargobike_engine::LeaseRepository::new(pool)),
};
let interpreter = cargobike_engine::register_interpreter(&instance, services)?;
interpreter.start_with(
 cargobike_engine::InterpretArgs { snapshot },
 dbos::StartOptions { workflow_id: Some(&workflow_id), ..Default::default }).await?;
```

The crash harness (the crash-injection harness): one process starts a release against the mock
provider, the driver kills it mid-flight, and a respawn must converge:

```bash
scripts/spike-postgres.sh start # or export CARGOBIKE_TEST_DATABASE_URL
CB_HARNESS_DB_URL=... cargo test -p cargobike-engine --features crash-hooks --test crash_harness
```

## Tests

```bash
cargo test -p cargobike-engine                                            # crate unit tests
cargo test -p cargobike-engine --features crash-hooks                     # + the harness modules
cargo test -p cargobike-engine --features crash-hooks \
    --test engine_integration --test crash_harness --test leases_behaviour
```

- `tests/engine_integration/` (in-process, over the mock provider): the
  reconciler's send-only contract — a stalled `wait: merge` is woken by the
  sweep and converges `Completed` — the queue's wake (F-71: A holds the
  lease at its merge wait, B queues, A's completion releases the lease and
  B's waiting attempt runs on), the supersede's cancel+cleanup, the reject
  policy's in-document failure (F-74/F-38), the fork reproducer, the
  output cap, and the secrets invariant (a named secret resolved for an
  `http-call` step reaches DBOS's persisted state on no surface).
- `tests/crash_harness.rs` kills a real harness process at every action
  step's boundary and asserts recovery converges without duplicated side
  effects.
- `tests/leases_behaviour.rs` covers the lease rows' conditional
  acquire/transfer/release.

DB-backed tests need a Postgres — the `cargobike-testsupport` dev
dependency provisions a throwaway container when
`CARGOBIKE_TEST_DATABASE_URL` is unset (with the exported URL, or
`scripts/spike-postgres.sh` start, the suites use that database).
Contributions: see [`AGENTS.md`](AGENTS.md).
