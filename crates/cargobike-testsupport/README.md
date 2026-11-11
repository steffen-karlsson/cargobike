# cargobike-testsupport

_Cargobike's shared test infrastructure — **dev-only**, never a product dependency._

## Purpose

The DB-backed test suites must be **self-sufficient**: a bare `cargo test`
must not quietly zero integration coverage. This crate provides the one
helper everything else needs — the test database URL with a fallback
container:

- `CARGOBIKE_TEST_DATABASE_URL` (CI provides one) always wins.
- Else **one throwaway Postgres 17 container per test process**
  (`testcontainers` + `testcontainers-modules`, `postgres:17-alpine` —
  version parity with `scripts/spike-postgres.sh`). The handle is
  deliberately leaked so the container lives for the process's whole
  run; the machine's teardown reaps it.
- Else (no Docker on the machine) the tests **skip with the reason
  printed** — a skip is never silent, and a reviewer reads the output.

Consumers: the engine's `tests/engine_integration/` fixture and
`tests/leases_behaviour.rs`, and the server's five DB-backed suites
(`authed_lifecycle`, `oidc_roundtrip`, `retry_endpoint`, `sighup_reload`,
`webhook_receiver`), all via `[dev-dependencies]` — nothing depends on
this crate in a production graph.

## Module map

| Module | Scope |
|---|---|
| `lib` | `database_url()` — the env-wins-else-container-else-skip helper (one async cell per process; the container work runs on the blocking pool — a nested runtime cannot start on a runtime thread) |

## Status

Complete for v1's ask (the audit §9 item). The container path needs a
Docker (or podman-socket) host; without one the suites degrade to the
printed skip, as before the wiring.

## Usage

```rust,ignore
// In a DB-backed test:
let Some(url) = cargobike_testsupport::database_url().await else {
    return; // the helper already printed why
};
```

## Tests

The crate itself has no DB-backed tests — it IS the test infrastructure.
The suites that consume it are the coverage.
