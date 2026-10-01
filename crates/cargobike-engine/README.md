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
| `template` | YAML template compiler: structure validation, `include:` expansion, auto/declared step IDs, wait-rule bounds, scheme-specific gate checks (F-27a, F-32b) | 3.1 |
| `expr` | CEL runtime for `when:` gates and `${{ ... }}` parameters; pure functions only; length/depth limits (F-27, F-32) | 3.2 |
| `steps` | Versioned `StepRegistry` (`name@version`, versions side by side, F-37) | 3.3 |
| `builtin` | The four action steps: `commit-files@1` (fused edit+commit, F-41/A1), `change-request@1` (find-by-head first, A1), `http-call@1`, `set-labels@1` | 3.5–3.8 |
| `interpreter` | The `cargobike.interpret.v1` durable workflow: control steps (`wait: merge` recv + provider re-verify + content verification, `wait: approval`, `wait: sleep`), snapshot-pinned execution, retry→`StepOptions` mapping | 3.4, 3.9, 3.11 |
| `signals` | Topic vocabulary (`merge/{env}/{step}`, `approval/{env}/{step}`, `lease/{env}`), typed `Signal` envelopes, idempotency keys, `Forks::Skip` decision (F-20a, F-20c) | 3.10 |
| `leases` / `concurrency` | Per-`(application, environment)` lease rows; `supersede` atomic transfer with the version-order guard, `queue` blocking, `reject` (F-70–F-74, A2) | 3.12 |
| `cleanup` | `cargobike.cleanup.v1`: close CR with remarked comment, delete branch, release/transfers lease | 3.13 |
| `reconciler` | `cargobike.reconcile.v1`: durable loop sending merge/closed signals only (never writes status) | 3.14 |
| `correlation` | `(provider, repo_id, cr_number) → (release, environment, step, attempt)` rows for webhook correlation (F-63) | 3.6 |
| `names` | Deterministic branch names via `engine.branch_format` format strings (F-137) | 3.5 |
| `snapshot` | `ReleaseSnapshot`: compiled template + registry inputs + pinned step-type versions + hash (F-16, F-29) | 3.9 |
| `crash` + `mock` + `bin/engine-harness` | Feature-gated crash hooks (`--features crash-hooks`), file-backed mock provider, the kill-and-recover harness | 3.15 (T1) |

## Status

Phase 3 scope complete as library code. Known gaps (tracked in
[`docs/TODO-phase-audit.md`](../../docs/TODO-phase-audit.md)): release-status
persistence (F-38), correlation stamp wiring, queue-wake sends, production
`PendingReleaseSource`, and server integration.

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
    steps: Arc::new(steps),           // built-ins registered via register_builtins
    providers: Arc::new(providers),   // ProviderRegistry, resolved by RepoRef.provider
    credentials: Arc::new(credstore), // resolves { secret: <name> }
    http: Arc::new(ssrf_client),      // HttpService seam
    leases: Arc::new(cargobike_engine::LeaseRepository::new(pool)),
};
let interpreter = cargobike_engine::register_interpreter(&instance, services)?;
interpreter.start_with(
    cargobike_engine::InterpretArgs { snapshot },
    dbos::StartOptions { workflow_id: Some(&workflow_id), ..Default::default() },
).await?;
```

The crash harness (T1): one process starts a release against the mock
provider, the driver kills it mid-flight, and a respawn must converge:

```bash
scripts/spike-postgres.sh start   # or export CARGOBIKE_TEST_DATABASE_URL
CB_HARNESS_DB_URL=... cargo test -p cargobike-engine --features crash-hooks --test crash_harness
```

## Tests

```bash
cargo test -p cargobike-engine                                   # unit tests
cargo test -p cargobike-engine --features crash-hooks            # + harness modules
cargo test -p cargobike-engine --test leases_behaviour --test crash_harness
```

DB-backed tests need a Postgres (`CARGOBIKE_TEST_DATABASE_URL`, or
`scripts/spike-postgres.sh` start). Contributions: see
[`AGENTS.md`](AGENTS.md).
