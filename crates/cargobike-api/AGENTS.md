# cargobike-api — AGENTS.md

Crate-specific how-to; project-wide rules in the root
[`AGENTS.md`](../../AGENTS.md).

## How to develop

- **One shape, one home**: a DTO that crosses the HTTP boundary gets its
  definition here — the server serialises it, the CLI deserialises it. If the
  same shape currently exists in another crate's module, the fix is to move
  it here and re-export; duplicating it "temporarily" is the bug the PRD's
  ordering rule 12 exists to prevent.
- Dependency ceiling (A.14): `cargobike-core`, `serde`, `serde_json`,
  `utoipa` (when the OpenAPI work starts), and `reqwest` behind a feature for
  the typed client. Never `sqlx`/`dbos`/`wasmtime`/`axum`.
- DTOs serialise as the API documents them: `serde(rename_all)` choices must
  match the PRD's API examples (§12, §10.2) and the naming conventions R1/R9
  (snake_case keys, kebab-case enum values).
- Error-code vocabulary constants eventually move here (the server currently
  keeps a survival subset in `http/errors.rs`); until then do not fork the
  vocabulary — import or extend the existing one.

## How to test

- Round-trip tests per DTO: `serde_json` and `serde_yaml_ng` encode/decode
  both directions with realistic fixtures.
- Boundary tests: the server's integration tests should assert against the
  crate's DTOs once handlers adopt them (typed instead of JSON-pointer
  reads). The CLI's wiremock tests assert deserialization through the crate's
  types.
- When the OpenAPI surface arrives: snapshot the generated spec; a PR that
  changes a DTO without a spec regeneration is incomplete.

## How to document

- Each DTO documents its PRD anchor (§12 endpoint, F-ID for field semantics).
- The README's status section states what has landed and what remains a
  server-side or CLI-side hand-roll — keep it honest in the same commit.

## How to contribute

- Conventional commits `<type>(cargobike-api): <subject>`; a DTO addition
  commit includes the DTO, its tests, and the adoption change (server/CLI)
  if the intent is to take over an existing shape.
- A `rename_all` mismatch or a stability-surface change (field removals,
  renames) is a breaking API change — treat it like §6.8 in-flight safety and
  say so in the commit message.

## Criteria for evaluating contributions (before committing)

All must be true:

1. `make lint`, `make test`, `make build` pass.
2. Round-trip tests pass for every new/changed DTO; fixtures match the PRD's
   §12 examples.
3. No leaked implementation details in public fields (no SQL/UI leakage;
   timestamps typed via `time`).
4. The dependency ceiling is respected; the crate stays wasm-hostile-free
   but does not pull host runtimes.
5. The server/CLI shapes adopting the DTO were replaced, not duplicated.
6. The crate's [`README.md`](README.md) status/usage and this file are
   updated in the same commit.
