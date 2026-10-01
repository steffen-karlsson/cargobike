# cargobike-core

_I/O-free shared model for the Cargobike release engine._

## Purpose

`cargobike-core` is the shared vocabulary of the project. Every other crate
depends on it, and it depends on nothing but serde-ish foundations — it carries
**no tokio, no sqlx, no reqwest, no wasmtime** so it compiles for
`wasm32-wasip2` (CI enforces the wasm target, see the PRD's workspace-crate rules).

What lives here:

| Module | Contents |
|---|---|
| `model` | The `Release` document model: `metadata`/`spec`/`status` roots: `Phase`, `EnvironmentPhase`, `Actor`, `CiContext`, `WorkflowInfo`, `EnvironmentStatus`, `ChangeRequestRef`, `RepoRef` (§5) |
| `error` | Release error codes (`StepFailed`, `MergeTimeout`, `ApprovalTimeout`, `ApprovalRejected`, `VersionNotVerified`, `ConcurrencyRejected`, `ChangeRequestModified`) and the typed `LibraryError` (§10.1) |
| `version` | Version schemes `semver` / `calver` / `opaque`: validation plus the ordering supersession uses  |
| `template` | `PipelineTemplate`: inputs, environment inputs, `step_groups`, steps, gates; source strings kept verbatim so snapshots stay faithful to the source  |
| `provider` | The async `Provider` trait (branches, commits, CRs, files, labels, statuses, protection checks, optional auto-merge/tag/release/webhooks), `ProviderCaps`, edit types |
| `edits` | Structural edit application over JSON/YAML/TOML with dot-notation paths — never text substitution |
| `step` | `StepType` trait, `StepContext`, `StepOutput`, `EnvRef`, the `HttpService` seam for SSRF-guarded HTTP (the server and sidecar clients implement it)  |
| `registry` | `ProviderRegistry` (resolve by name) and `CredentialStore` (resolve `{ secret: <name> }`)  |
| `webhook` | Provider-neutral `NormalisedEvent` shapes: `TagPush`, `ChangeRequestClosed`, `Unrecognised` (webhook normalisation) |

## Status

Complete for its Phase 1 scope. Growth is intentional and slow: it is the
stability anchor of the workspace (breaking changes here ripple into every
crate).

## Usage

Library consumers build the model types directly (the engine, provider and
server crates' tests do this):

```rust
use cargobike_core::model::RepoRef;
use cargobike_core::version::VersionScheme;

let repo = RepoRef::new("github", "789012"); // id is the immutable provider id
let scheme = VersionScheme::Semver;

// Structural edit: set image.tag without touching comments/format
// (providers share the same applier, so what is committed is what is verified).
let applied = cargobike_core::edits::apply_to_document(
 base,
 &edits,
 cargobike_core::provider::EditFormat::Yaml,
 &serde_json::Value::Null)?;
```

Editing YAML manifests of a real deployment file:

```rust
use cargobike_core::edits::{apply_to_document, format_for_path};
use cargobike_core::provider::Edit;

let edits = [Edit {
 file: "apps/my-service/preview.yaml".into,
 format: None, // inferred from the extension
 field: "image.tag".into,
 value: Some(serde_json::json!("1.2.3")),
}];
let out = apply_to_document(&base_bytes, &edits, format_for_path("apps/my-service/preview.yaml"), &serde_json::Value::Null)?;
```

## Tests

```bash
cargo test -p cargobike-core
cargo build -p cargobike-core --target wasm32-wasip2 # the wasm contract
```

Unit tests live in `#[cfg(test)] mod tests` next to the code (PRD §20.4),
using `rstest` and `pretty_assertions`. See [`AGENTS.md`](AGENTS.md) for the
contribution rules that gate commits touching this crate.
