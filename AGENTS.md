# AGENTS.md

The project-wide rules for working on Cargobike — for AI agents and humans
alike. Per-crate development/testing/documentation rules live in each
crate's own `README.md` (purpose + usage) and `AGENTS.md` (crate-specific
process); this file covers the whole repo and the rules that apply to every
commit.

## What this repo is

A Cargo workspace with seven crates (`crates/`) building two binaries:
`cargobike` (CLI) and `cargobike-server`. The product is a durable release
orchestration engine per [`docs/PRD.md`](docs/PRD.md) — read the PRD before
touching behaviour code; it is the single source of truth and its F-IDs are
cited throughout the code.

| Crate | Scope | Crate docs |
|---|---|---|
| `cargobike-core` | I/O-free model + traits; must build for `wasm32-wasip2` | [README](crates/cargobike-core/README.md) / [AGENTS](crates/cargobike-core/AGENTS.md) |
| `cargobike-api` | Shared DTOs, OpenAPI, typed client (stub) | [README](crates/cargobike-api/README.md) / [AGENTS](crates/cargobike-api/AGENTS.md) |
| `cargobike-engine` | Durable interpreter over DBOS (templates, CEL, steps, waits, leases, cleanup, reconciler) | [README](crates/cargobike-engine/README.md) / [AGENTS](crates/cargobike-engine/AGENTS.md) |
| `cargobike-server` | axum API, auth, config, DB, leader election | [README](crates/cargobike-server/README.md) / [AGENTS](crates/cargobike-server/AGENTS.md) |
| `cargobike-provider-github` | GitHub provider + webhooks | [README](crates/cargobike-provider-github/README.md) / [AGENTS](crates/cargobike-provider-github/AGENTS.md) |
| `cargobike-cli` | CLI client (no DB deps) | [README](crates/cargobike-cli/README.md) / [AGENTS](crates/cargobike-cli/AGENTS.md) |
| `cargobike-extension-sdk` | WIT + guest bindings (Phase 5+) | [README](crates/cargobike-extension-sdk/README.md) / [AGENTS](crates/cargobike-extension-sdk/AGENTS.md) |

`templates/service.yaml` is the canonical example template;
`docs/TODO-phase-audit.md` is the live code-vs-PRD gap list.

## How to develop

- **PRD-first**: behaviour changes must map to a PRD requirement (F-ID) or
  document the deviation. Implementation follows the phase plan (§16): each
  task is broken into commit-sized pieces before starting.
- **Architecture is enforced by dependencies**: core has no I/O deps; engine
  depends on core only; CLI has no DB deps; provider-github has no DBOS/SDK
  deps (PRD §11.2/A.14). `cargo-deny` backs the bans; do not add a dependency
  outside Appendix A without a written reason in the commit.
- Naming conventions R1–R12 (PRD §2a) apply to every user-facing surface:
  config keys, durations (`humantime` strings), sizes (`1MiB`), secret shapes
  (`{ file }` / `{ env }` / `{ secret }`), quoted string provider/owner IDs,
  kebab-case enums, `CARGOBIKE_*` / `CARGOBIKE_SERVER_*` env vars.
- Secrets never log. `secrecy::SecretString` everywhere; hand-written
  `Debug` for config structs; sensitive headers marked; redacting at the
  log boundary. Tests may set env vars only inside tests and clean up.
- Approved libraries only (Appendix A: e.g. `serde_yaml_ng`, never
  `serde_yml`; `time`, not `chrono`; `rustls`, not openssl; one `sqlx`
  version aligned with `dbos`).

## How to test

- Unit tests in `#[cfg(test)] mod tests` beside the code; integration tests
  in each crate's `tests/`; `rstest` parametric, `pretty_assertions` diffs,
  scenario-style names (`test_create_release_rejects_unknown_application`, or
  with the PRD id where one anchors the behaviour).
- Databases: DBOS and sqlx integration tests need Postgres. CI provides one
  and exports `CARGOBIKE_TEST_DATABASE_URL`; locally use
  `scripts/spike-postgres.sh start`. Until `testcontainers` wiring lands
  (audit TODO §9), tests **skip** without the URL — don't mistake a skip for
  a pass when reviewing.
- The crash-injection harness runs under `--features cargobike-engine/crash-hooks`
  (the CI test job does exactly this; T1).
- Before any commit, run the four gates:

```bash
make lint        # fmt --check + clippy -D warnings
make test        # all tests
make build       # workspace compiles
make docs-check  # crate README/AGENTS currency for the changes at hand
```

If any fails, fix before committing; never commit failing code (PRD §20.2).
CI additionally runs cargo-deny, the wasm32-wasip2 build of `cargobike-core`
(T4), and the four-platform build matrix (`.github/workflows/ci.yml`).

## How to document

- **README per crate** (`crates/*/README.md`): what the crate is for, its
  module map, current status (honestly — stubs and pending work are stated,
  pointing at `docs/TODO-phase-audit.md`), usage with runnable examples.
- **AGENTS.md per crate** (`crates/*/AGENTS.md`): how to develop, test,
  document, and contribute to that crate, plus the criteria evaluated before
  committing to it.
- This root README stays overview-level; it references the crate docs rather
  than duplicating them. The PRD cites stay the deep source.
- rustdoc: every public item documented; comments explain *why* with PRD
  references; `README.md`/`AGENTS.md` updated in the same commit as behaviour,
  test, or process changes.

## Standing documentation rule (per commit)

Every commit keeps the documentation truthful:

1. **READMEs**: update the crate's README for any change to the crate — new
   features, fixed behaviour, changed usage, new/changed examples. The API
   surface shown in examples must compile-check against the code as of that
   commit.
2. **AGENTS.md**: update the crate's AGENTS.md when the change touches
   development, testing, documentation, or contribution processes, or the
   evaluation criteria; the root pair is updated whenever a crate's rules
   change in a way that has repo-wide relevance (e.g. a new pre-commit gate).

This rule is enforced mechanically: `make docs-check`
(`scripts/docs-check.sh`) fails when a changeset modifies files under
`crates/<crate>/` without touching that crate's `README.md` or `AGENTS.md`.
Root-level changes print a review note instead of failing; the reviewer
criterion below covers the semantic side that a diff check cannot judge.

## How to contribute

- **Conventional commits (PRD §20.1)**: `<type>(<scope>): <description>`;
  type `feat` or `fix` (code), `chore`/`docs` for supporting work by
  established precedent; scope = crate name, `all`, or `ci`. One cohesive
  change per commit — split cross-cutting work.
- **Pre-commit gates** (§20.2): `make lint && make test && make build &&
  make docs-check` must pass with zero issues.
- **Contribution evaluation criteria** — a reviewer (human or agent) checks,
  before a commit lands:
  1. All four gates pass (see above) — including the DB-backed integrations
     when a database is available.
  2. The change maps to a PRD requirement or documents a deliberate
     deviation; scenarios added/changed have tests named after them.
  3. Naming conventions R1–R12 hold on all touched surfaces; approved-library
     rules hold on dependencies.
  4. Secrets never enter logs, errors, Debug output, or step outputs (F-87,
     F-120); auth surfaces have their negative tests.
  5. Error paths use typed errors/problem details with the vocabulary
     constants; no ad-hoc strings.
  6. In-flight safety respected for engine/API surfaces (§6.8): breaking
     interpreter/protocol changes bump versions deliberately and are called
     out in the commit message.
  7. The crate's README and AGENTS.md were updated per the standing rule;
     the audit TODO was ticked off if the commit closes an item in
     `docs/TODO-phase-audit.md`.
Crate-specific additions are in each crate's AGENTS.md (e.g.
  the engine's purity/idempotency gates, the provider's octocrab pinning,
  the server's verbatim-config test).

## Environment notes

- Rust 1.85+, edition 2024. Local scripts: `scripts/spike-postgres.sh`
  (Postgres 17 on port 54329 for tests/spikes).
- `Makefile` mirrors PRD §15.1 (`build`, `test`, `lint`, `fmt`, `coverage`,
  `audit`, `ci`, `clean`).
