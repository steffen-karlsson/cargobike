# cargobike-engine — AGENTS.md

Crate-specific how-to for developing, testing, documenting and contributing;
project-wide rules live in the root [`AGENTS.md`](../../AGENTS.md).

## How to develop

- **Purity rule **: the interpreter body is a pure function of the
 `ReleaseSnapshot` and recorded outputs. Any logic that would read "current"
 config or wall-clock time must either live inside a `dbos::step` (recorded
 result) or take its input from the snapshot (see `now_unix_millis` feeding
 the recorded merge anchor, and `snapshot.read_release` deriving a
 deterministic release view).
- **At-least-once discipline (the idempotency strategy table)**: every side effect needs an
 idempotency answer. Provider calls that create objects must first look
 for them (`change-request` find-by-head; `commit-files` re-commit no-op).
 If you add a step type, add its row to the PRD's idempotency table or the
 PRD itself.
- **Status bookkeeping**: the release's status writes are the engine's job
 (the `status` store): every write rides its own durable step and the
 store's updates are conditional (`AND NOT terminal`), so a cancel's
 phase is never resurrected by a stale attempt's write. New interpreter
 boundaries that change status facts go through `status_write`.
- **Signal addressing **: never invent topic strings inline. Use
 `signals::merge_topic` / `approval_topic` / `lease_topic` and the
 `*_signal_key` helpers; a new wait means a new topic family + documented key.
- `cargobike-engine` depends on `cargobike-core` only — no `api` types, no
 axum (PRD §11.2). Registry-input provisioning is config-space work in the
 server; the engine consumes already-stamped `env_inputs`.
- DB access (leases, correlation) uses plain conditional SQL through `sqlx`
 with no macros (macro-extensions need a live DATABASE_URL; keep the
 macro-free pattern used in `leases.rs`).
- Durations are humantime strings at the config edge, `std::time::Duration`
 inside `ResolvedStep` (parse once at compile).

## How to test

- Unit tests in-module (`#[cfg(test)]`), integration tests in `tests/`:
 `engine_integration/` runs as ONE binary (in-process: the reconciler's
 send-only signaling contract, the secrets invariant, the supersede
 E2E, and the crash-harness module `crash_harness::…`) with a fixture-DB
 lock for the connection budget; `leases_behaviour.rs` standalone. The
 webhook receiver's end-to-end lives with the server
 (`crates/cargobike-server/tests/webhook_receiver.rs`) because the
 receiver is server-hosted.
- Crash hooks are feature-gated: `--features crash-hooks` compiles
 `mock`, `crash::milestone_maybe` and the harness bin. Without the feature
 they are inert — production builds take none of that path.
- DB-backed tests need Postgres (`CARGOBIKE_TEST_DATABASE_URL`), e.g. via
 `scripts/spike-postgres.sh`.
- The migration schema lives with `cargobike-server/migrations`; tests that
 need it run `sqlx::migrate!` idempotently (see `bin/engine-harness.rs`).
- Assertion style: scenario names describe the behaviour plus the PRD id
 (`test_sends_only_signals_never_status_f67`).

## How to document

- Engine decisions in rustdoc cite the PRD's requirement they implement
  (topics, purity, idempotency).
- The spike findings live in `docs/spike-dbos.md`; when the engine's design
 changes because of a DBOS finding, update that document in the same commit.
- In-flight safety (§6.8): adding/removing/reordering DBOS operations in the
 interpreter or changing a step's output schema is BREAKING. When you do
 that deliberately, bump the workflow name (e.g. `cargobike.interpret.v2`),
 register both versions until in-flight recoveries drain, and record the
 reasoning in the commit message and the spike doc.

## How to contribute

- Conventional commits `<type>(cargobike-engine): <subject>` (PRD §20.1);
 one mechanism or one fix per commit.
- Anything that modifies `interpreter.rs`'s DBOS operation sequence or a
 built-in step's output schema must carry the in-flight-safety note (§6.8)
 in the commit body and a version bump decision.

## Criteria for evaluating contributions (before committing)

All must be true:

1. All four gates pass: `make lint`, `make test`, `make build`,
 `make docs-check`.
2. New engine logic has scenario-named tests; crash-harness-affected changes
 re-run `cargo test -p cargobike-engine --features crash-hooks --test
 engine_integration` with Postgres available.
3. Steps added to the interpreter are idempotent with the idempotency strategy table strategy stated
 in their rustdoc; signals use topic+idempotency-key helpers.
4. No secret values can enter step outputs : credentials resolve only
 through `CredentialStore`, and the value never crosses a `StepOutput`.
5. Template compile errors are actionable strings (`TemplateError::Invalid`)
 and tested.
6. The crate's [`README.md`](README.md) usage/examples and this file are
 updated in the same commit when behaviour, tests or processes change.
