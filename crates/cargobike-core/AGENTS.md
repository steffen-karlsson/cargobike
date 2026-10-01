# cargobike-core — AGENTS.md

Rules for developing, testing, documenting and contributing to the
`cargobike-core` crate. Project-wide conventions live in the root
[`AGENTS.md`](../../AGENTS.md); this file only adds what is specific to this
crate.

## How to develop

- **This crate is the workspace's floor.** It depends on nothing async: no
 `tokio`, `sqlx`, `reqwest`, `wasmtime`. It must keep compiling for
 `wasm32-wasip2` (`cargo build -p cargobike-core --target wasm32-wasip2`).
 If a change needs I/O, it belongs in `cargobike-engine`/`cargobike-server`,
 behind a trait seam defined here (that is how `Provider`, `StepType` and
 `HttpService` were introduced).
- Organise by domain, not by layer (PRD §20.3): release model in `model`,
 git-platform abstraction in `provider`, pipeline shape in `template`, etc.
- Error messages follow the `failed to <action>: <cause>` style; release error
 codes are the constants in `error.rs` — never an ad-hoc string .
- Version-scheme logic is pure: `version.rs` validation/ordering must not
 depend on wall-clock time (supersession uses creation time as a fallback
 parameter instead —).
- Template strings stay verbatim (source durations, CEL sources): parsing and
 evaluation belong to the engine (see `template.rs` module doc).

## How to test

- Unit tests in `#[cfg(test)] mod tests` within the same file (PRD §20.4);
 use `rstest` for parametric cases and `pretty_assertions` for diffs.
 Every function with non-trivial logic gets a test with a scenario name
 (e.g. `test_uuidv7_ids_sort_by_creation_time`).
- For every PRD requirement implemented here, name the test with the
 requirement's shape (`test_..._f74`, `test_..._f10`) so the §14 test matrix
 stays auditable.
- Run:

```bash
cargo test -p cargobike-core
cargo build -p cargobike-core --target wasm32-wasip2
```

## How to document

- Every public item gets a rustdoc comment. Comments explain *why*, and cite
 PRD identifiers (F-x / §-references) where a rule comes from the spec.
- Keep `lib.rs`'s module map current when adding a module.
- When a README example would help (a struct with construction subtleties),
 add a short example to [`README.md`](README.md) in the same commit.

## How to contribute

- Commits touching this crate use `cargo test -p cargobike-core` scope-free
 chunks; conventional-commit format `<type>(core): <description>` (PRD §20.1).
 A commit is one cohesive change.
- Breaking changes to public types here are workspace-wide events (§6.8
 in-flight safety). Merge them only with an explicit note in the commit
 message about which in-flight surfaces they affect.

## Criteria for evaluating contributions (before committing)

All must be true:

1. The four gates pass for the workspace: `make lint`, `make test`,
 `make build`, `make docs-check`.
2. `cargo test -p cargobike-core` passes; new logic has scenario-named tests.
3. `cargo build -p cargobike-core --target wasm32-wasip2` still succeeds.
4. No new dependencies beyond the PRD Appendix A.1/A.6/A.7 list; nothing
 async, nothing I/O.
5. Public API changes are documented (rustdoc + README) and, if breaking,
 justified under §6.8 in-flight safety.
6. Error paths use the `error.rs` code constants; no magic strings.
7. The crate's [`README.md`](README.md) and this file reflect any changed
 usage, testing or contribution rule (kept up-to-date per commit).
