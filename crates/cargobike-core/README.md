# cargobike-core

_I/O-free shared model for the Cargobike release engine._

## Purpose

`cargobike-core` is the shared vocabulary of the project. Every other crate
depends on it, and it depends on nothing but serde-ish foundations — it carries
**no tokio, no sqlx, no reqwest, no wasmtime** so it compiles for
`wasm32-wasip2` (CI enforces this, PRD §11.2, T4).

What lives here:

| Module | Contents |
|---|---|
| `model` | The `Release` document model: `metadata`/`spec`/`status` roots (F-1), `Phase`, `EnvironmentPhase`, `Actor`, `CiContext`, `WorkflowInfo`, `EnvironmentStatus`, `ChangeRequestRef`, `RepoRef` (F-2, F-6, F-10, §5) |
| `error` | Release error codes (`StepFailed`, `MergeTimeout`, `ApprovalTimeout`, `ApprovalRejected`, `VersionNotVerified`, `ConcurrencyRejected`, `ChangeRequestModified`) and the typed `LibraryError` (F-8, §10.1) |
| `version` | Version schemes `semver` / `calver` / `opaque`: validation plus ordering for supersession (F-4, F-72) |
| `template` | `PipelineTemplate`: inputs, environment inputs, `step_groups`, steps, gates; source strings kept verbatim so snapshots stay faithful (F-25, F-28, F-32b) |
| `provider` | The async `Provider` trait (branches, commits, CRs, files, labels, statuses, protection checks, optional auto-merge/tag/release/webhooks), `ProviderCaps`, edit types (F-43–F-47, F-41) |
| `edits` | Structural edit application over JSON/YAML/TOML with dot-notation paths — never text substitution (F-41) |
| `step` | `StepType` trait, `StepContext`, `StepOutput`, `EnvRef`, the `HttpService` seam for SSRF-guarded HTTP (F-33, F-39, F-40) |
| `registry` | `ProviderRegistry` (resolve by name) and `CredentialStore` (resolve `{ secret: <name> }`) (F-39, F-146) |
| `webhook` | Provider-neutral `NormalisedEvent` shapes: `TagPush`, `ChangeRequestClosed`, `Unrecognised` (F-44 webhook normalisation) |

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
    &serde_json::Value::Null,
)?;
```

Editing YAML manifests of a real deployment file:

```rust
use cargobike_core::edits::{apply_to_document, format_for_path};
use cargobike_core::provider::Edit;

let edits = [Edit {
    file: "apps/my-service/preview.yaml".into(),
    format: None, // inferred from the extension
    field: "image.tag".into(),
    value: Some(serde_json::json!("1.2.3")),
}];
let out = apply_to_document(&base_bytes, &edits, format_for_path("apps/my-service/preview.yaml"), &serde_json::Value::Null)?;
```

## Tests

```bash
cargo test -p cargobike-core
cargo build -p cargobike-core --target wasm32-wasip2   # the wasm contract
```

Unit tests live in `#[cfg(test)] mod tests` next to the code (PRD §20.4),
using `rstest` and `pretty_assertions`. See [`AGENTS.md`](AGENTS.md) for the
contribution rules that gate commits touching this crate.
