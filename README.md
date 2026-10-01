# Cargobike

_Durable delivery, one pedal at a time._

Cargobike is an open-source, configurable, extensible **release orchestration
workflow engine**. It drives software releases through user-defined
environment sequences — creating branches, opening change requests, waiting
for merges and approvals, and tracking status — on durable, crash-resilient
workflow execution (DBOS Transact over PostgreSQL). See
[`docs/PRD.md`](docs/PRD.md) for the full product requirements document.

**What Cargobike is**: a release orchestration engine; a centralized server
persisting state in PostgreSQL, receiving webhooks and exposing a REST API;
a CI-friendly CLI; an extensible platform (sidecar extensions in v1.0, WASM
in v1.1).

**What it is not**: a UI, a cluster manager, a CI/CD runner, or a deployment
tool. Cargobike coordinates; it opens change requests and tracks merges.

## Workspace layout

Two binaries ship: `cargobike` (the CLI, no database dependencies) and
`cargobike-server` (the server). Built from seven crates — each has its own
`README.md` (purpose + usage) and `AGENTS.md` (development/test/contribution
rules):

| Crate | Role | Docs |
|---|---|---|
| `cargobike-core` | I/O-free shared model: release model, error codes, version schemes, templates, provider/step traits, edit applier | [README](crates/cargobike-core/README.md) · [AGENTS](crates/cargobike-core/AGENTS.md) |
| `cargobike-engine` | Durable interpreter over DBOS: template compiler, CEL, built-in steps, waits, leases, cleanup, reconciler | [README](crates/cargobike-engine/README.md) · [AGENTS](crates/cargobike-engine/AGENTS.md) |
| `cargobike-server` | axum REST API, auth (OIDC/API keys), config, PostgreSQL state, leader election | [README](crates/cargobike-server/README.md) · [AGENTS](crates/cargobike-server/AGENTS.md) |
| `cargobike-provider-github` | GitHub `Provider` implementation + webhook signing/normalisation | [README](crates/cargobike-provider-github/README.md) · [AGENTS](crates/cargobike-provider-github/AGENTS.md) |
| `cargobike-cli` | The `cargobike` CLI: contexts, auth, release commands, rendering | [README](crates/cargobike-cli/README.md) · [AGENTS](crates/cargobike-cli/AGENTS.md) |
| `cargobike-api` | Shared DTOs, OpenAPI surface, typed client (planned) | [README](crates/cargobike-api/README.md) · [AGENTS](crates/cargobike-api/AGENTS.md) |
| `cargobike-extension-sdk` | WIT + guest bindings for sidecar/WASM extensions | [README](crates/cargobike-extension-sdk/README.md) · [AGENTS](crates/cargobike-extension-sdk/AGENTS.md) |

Dependency direction: everything depends on `cargobike-core`; the CLI and
server stay free of database crates; the engine depends on core only.

## Quickstart (development)

Requirements: Rust 1.85 (edition 2024), PostgreSQL 16+.

```bash
make build          # the workspace
make lint           # fmt-check + clippy (-D warnings)
make test           # all crates; DB-backed tests need a database, see below

# A local Postgres for integration/crash tests:
scripts/spike-postgres.sh start   # trust auth on 127.0.0.1:54329
export CARGOBIKE_TEST_DATABASE_URL=postgres://localhost:54329/postgres
```

Run the server and CLI against it:

```bash
# A minimal dev config (env-only bootstrap, §13.2: no registry/release create).
printf 'server: {}\ndatabase: { url: postgres://localhost:54329/postgres }\nauth: {}\n' \
  > /tmp/cb-dev-config.yaml
CARGOBIKE_SERVER_CONFIG=/tmp/cb-dev-config.yaml \
  CARGOBIKE_SERVER_BOOTSTRAP_API_KEY=devkey \
  cargo run -p cargobike-server

cargo run -p cargobike-cli -- release list --url http://localhost:8080
```

(The bootstrap API key grants `*` from localhost only, F-86; env-only mode
cannot create releases until an application registry is configured — PRD
§13.2.) For the engine alone, the crash/lease harnesses demonstrate the
durable pipeline end to end:
[`crates/cargobike-engine/README.md`](crates/cargobike-engine/README.md).

## Project status

Currently mid-implementation against the PRD's phased plan:

- **Phases 1–4** (foundation, server core + auth, pipeline engine, GitHub
  provider + CLI) are built — with a shared template the release kinds in
  [`templates/service.yaml`](templates/service.yaml).
- The server↔engine wiring and the remaining Phase 4 CLI surfaces are tracked
  item-by-item in **[`docs/TODO-phase-audit.md`](docs/TODO-phase-audit.md)**,
  alongside the known gaps before Phase 5.
- **Phase 5** (webhooks, SSE watch, event log, extensions, OpenAPI) is next.

## Documentation

- [`docs/PRD.md`](docs/PRD.md) — the product requirements (single source of
  truth for behaviour, F-IDs cited across the code)
- [`docs/TODO-phase-audit.md`](docs/TODO-phase-audit.md) — code-vs-PRD audit
  and the open item list
- [`docs/examples.md`](docs/examples.md) — user-facing worked examples
- [`docs/spike-dbos.md`](docs/spike-dbos.md) — DBOS Rust SDK spike findings
- Per-crate docs — see the table above

## Contributing

Start with the root [`AGENTS.md`](AGENTS.md): commit conventions, pre-commit
gates (`make lint`, `make test`, `make build`), and the evaluation criteria
applied before a commit lands. Each crate's own `AGENTS.md` adds its
specific rules. License: dual MIT/Apache-2.0.
