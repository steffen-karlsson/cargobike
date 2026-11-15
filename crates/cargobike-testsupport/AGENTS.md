# cargobike-testsupport — development rules

Test-only crate: **never** a `[dependencies]` entry of any product crate.
It exists for `[dev-dependencies]` consumers (the engine and server DB
suites) and holds the shared test database helper.

## How to develop here

- The dependency discipline is the point: `testcontainers` and
  `testcontainers-modules` live HERE and nowhere else — adding them to
  a product crate is a review failure, not a convenience.
- Precedence is fixed: `CARGOBIKE_TEST_DATABASE_URL` wins over the
  container; the container wins over skipping; a skip always prints its
  reason (a silent skip is the failure mode this crate exists to kill).
- One container per test **process** (the async cell); the handle is
  leaked on purpose — the container's lifecycle is the process's.

## How to test

- `cargo test -p cargobike-testsupport` (nothing DB-backed here).
- The real proof is consumer-side: run a DB suite with the variable
  unset — it either provisions (Docker present) or prints the skip
  reason (no Docker).
- The wipe helper's fresh-container path: a suite that wipes before
  its server boot must pass BOTH ways — with the shared fixture URL
  (pre-migrated tables) and on a throwaway container (the wipe runs
  before any migration; `42P01` is "nothing to wipe", not a panic —
  the CI coverage job hit exactly this).

## How to document

- The crate README states the precedence and the leak; update it with
  any behaviour change. New helpers get a module-table row.

## Contribution criteria

1. All four gates pass (`make lint && make test && make build &&
   make docs-check`).
2. No product crate gains this crate (or testcontainers) as a
   non-dev dependency — `cargo tree` in CI's cargo-deny backs the rule.
3. A skip is never silent: the reason prints.
