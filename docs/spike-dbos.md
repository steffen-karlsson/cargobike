# DBOS Transact (Rust) SDK Spike — Findings (PRD task 1.1)

**Crate**: `dbos` 0.5.0 (MIT, released 2026-09-09). **Toolchain**: Rust 1.98.1.
**Spike**: `spikes/dbos-spike/` (outside the workspace, `cargo test -p dbos-spike`
from its own directory); sequential one-test driver boots its own Postgres.

All five experiments pass. Mismatch between PRD assumptions and the real
API surfaced the items noted below; nothing invalidates the Phase 3 design.

## 1. Verified API surface (engine plan ↔ real signatures)

| PRD assumption | Real API (0.5.0) | Verdict |
|---|---|---|
| `dbos::step` / `step_with(name, options, body)` | `step<T, E, F, Fut>(name: &str, body: F) -> PendingStep<T, E>`; body: `FnMut() -> Future<Output = dbos::Result<T, E>>` | ✓ |
| `StepOptions` control retries | `StepOptions<E> { max_attempts (default 1), interval, backoff_rate, max_interval, should_retry ... }` | ✓ same shape family as F-35's `retry` block |
| `dbos::recv` with topics | `recv<T, E>(topic: Option<&str>, timeout) -> PendingStep<Option<T>, E>` — topics optional *by type* | ✓ |
| `dbos::send` / `send_with` | `DBOS::send(dest, &msg)`; `send_with(dest, &msg, SendOptions { topic, idempotency_key, forks })` | ✓ |
| Idempotency keys dedupe re-sends | The key becomes the message row's identity: same key ⇒ discarded | **verified in E2** |
| `dbos::sleep` resumes at original wake time | `sleep<E>(Duration)` — durable; wake time computed from original start | **verified in E3/E5** (45 s wall-time assert on a 20 s sleep across a killed process) |
| `DBOS::cancel` pauses | Row ⇒ `CANCELLED`; `resume` re-enqueues with steps intact | ✓ documented |
| `DBOS::fork` + `ForkFrom::LastFailure` | `fork_with::<R, E>(id, ForkFrom::LastFailure, ForkOptions::default())` | **verified in E4**: the failed step's body re-runs; earlier steps replay (bump counts prove s1 != re-run) |
| `DBOS::delete` | Cascade delete; self-delete refused | ✓ documented |
| Registration before `launch()` | `register_workflow<P, R, E, F, Fut>(name, workflow) -> WorkflowRef<P, R, E>`; after launch ⇒ `Error::AlreadyLaunched` | ✓ (hit it first-hand) |
| Executor + app version recovery | `Config { executor_id, app_version, ... }`; recovery only picks up workflows whose app version matches | ✓ |

## 2. Things the PRD text did not spell out

1. **Workflows take exactly one argument, never a context** (`_: ()` if
   nothing). The interpreter's snapshot (`template + inputs +
   step_type_versions`) is therefore *one struct* — matching F-16's
   snapshot design. Registration closures must not capture `DBOS` (self-referential
   leak); the crate enforces it by wording, so the interpreter reads the
   ambient context through free functions (`step`, `recv`, `send`, `get_event`).
2. **Error envelope**: workflow and step closures return
   `dbos::Result<T, E>` = `std::result::Result<T, Error<E>>`, where
   application failures are `Error::Application(E)`. `DurableError` is a
   blanket impl — a `thiserror` + `serde` error type qualifies as-is. So
   the engine's interpreter error (carrying `ReleaseError`) derives
   `Serialize/Deserialize` with no extra traits (F-8 fidelity on replay).
3. **Identity registry**: an `app_name` registers a claim on its
   `app_version`; a *different* app claiming the same version name fails
   with `RegisteredByAnother`. The engine must derive version names per
   scope (e.g. `{app_name}` from config) instead of a shared constant.
   Every experiment in the spike uses per-app versions for that reason.
4. **`launch()` fails when neither `Config.app_version` nor `DBOS__APPVERSION`
   is set.** The engine always sets it explicitly (F-22); also a
   convenience in tests, which never accidentally inherit a foreign
   version.
5. **Step retries never record intermediate failures.** One attempt
   sequence ⇒ one recorded outcome; the reconciler-visible "attempt" is
   Cargobike's own `status.attempts[]`, not DBOS's.

## 3. Forks option — chosen and documented (F-20c)

`Forks` has two variants, on `SendOptions.forks`: `Skip` (default) — only the
named destination — and `Include` — the destination plus everything
recursively forked from it.

**Cargobike chooses `Forks::Skip`.** Rationale: F-20c already addresses
signals to the current attempt's workflow ID (`status.attempts[-1]`)
explicitly, and approval submissions are bound to attempt + CR head SHA
(F-96). Fork fan-out would deliver a stale-approval-shaped message into a
fork the caller never intended to talk to, weakening early-message
rejection (F-20b). A forked retry that needs the same signal will be sent
again — the idempotency key derived from the source event prevents double
delivery to whichever attempt is current (verified in E2).

## 4. Gaps / follow-ups

| Gap | Impact | Follows up in |
|---|---|---|
| `Client` (standby enqueue) not runtime-verified in the spike | F-133's "standby serves webhooks without executor" path | Phase 5 task 5.1 (with the webhook receiver boots) |
| Queues not exercised (`RegisterQueue` etc.) — leases (A2) use rows + conditional INSERT instead, per A.2 | None; design unchanged | Phase 3 task 3.12 |
| No signals-and-macros overlap: `select_step!` macro is optional; we do not need it (gates are interpreter logic) | None | — |

## 5. Environment notes

- macOS 26 (26A428): podman-on-libkrun (`krunkit`) aborts (`SIGABRT`) under
  mach-o changes in 26 — container runtime unusable here. The spike
  provisioned **native Postgres 17 via Homebrew** (script:
  `scripts/spike-postgres.sh`, port 54329, trust auth, data dir
  `/private/tmp/cb-spike-pgdata`). The docker path stays for Linux CI,
  where containers work (`tests/spike.rs` keeps both; `CB_SPIKE_NATIVE_PG`
  selects the native fixture).
- E2's dedupe behaviour is per `(destination, topic, idempotency_key)` — the
  key does not span destinations. Cargobike's keying scheme in F-20a
  (`merge/{env}/{step}` + delivery/approval ID) composes with that.
