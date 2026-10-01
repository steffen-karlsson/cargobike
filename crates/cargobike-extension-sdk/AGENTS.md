# cargobike-extension-sdk — AGENTS.md

Crate-specific how-to; project-wide rules in the root
[`AGENTS.md`](../../AGENTS.md).

## How to develop

- **WIT is a published contract.** The `.wit` files define the extension
  interface; changes follow the stability promises (§21.3): additive shapes
  only within a version, breaking changes mean a new interface version
  (side by side with the old), never an in-place break.
- Guest authors' constraint (F-121 / §7.4): no filesystem, no env vars, no
  async runtime in guests. The bindings must not leak host capabilities
  beyond the declared `http` interface.
- Schema hashes (F-119/F-42) are computed over the *declared schema*, not the
  implementation; keep the hashing input pure (the WIT text) so snapshots
  pin interface versions, not code.
- Dependency ceiling (A.14): `cargobike-core`, `wit-bindgen`, `serde`.
  Guests must not depend on `tokio`/`reqwest`; the host does.
- Until the crate is real, keep it compiling as a workspace member without
  other crates depending on it; the server feature-gates future use.

## How to test

- WIT-driven tests: validate the interface with `wasm-tools` in CI (task from
  PRD A.9) — a `.wit` change that fails validation is a broken contract.
- With bindings generated: round-trip the message types (serde⇄WIT shapes),
  then integration-test through the server's extension host with a mock
  sidecar (Phase 5's plan).
- Schema SHA pinning: a test computes the hash over the WIT text, changes a
  comment, and asserts stability (hash covers interface, not comments —
  decide and document the canonical input).

## How to document

- The README's usage section shows a working guest call-site; keep the
  sidecar trust boundaries (§7.3) linked rather than re-explained.
- Every WIT interface version documents its compatibility window (§21.3).

## How to contribute

- Conventional commits `<type>(cargobike-extension-sdk): <subject>`; WIT and
  bindings changes are separate, reviewable commits.
- A WIT change requires: the `.wit` edit, regenerated bindings, tests, and
  the PRD's §7.3/§7.4 cross-check when trust boundaries are touched.

## Criteria for evaluating contributions (before committing)

All must be true:

1. `make lint`, `make test`, `make build` pass; `wasm-tools validate` (once
   wired) passes on the WIT.
2. Guest bindings generation is reproducible from the committed WIT.
3. No host capability leaks (no std::env/std::fs access surfaced to guests).
4. Interface changes carry version bumps where breaking; old versions stay.
5. The crate's [`README.md`](README.md) status/usage and this file are
   updated in the same commit.
6. Schema-hash canonicalization is documented and tested when the hashing
   implementation lands.
