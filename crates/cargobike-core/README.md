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
| `model` | The `Release` document model: `metadata`/`spec`/`status` roots: `Phase`, `EnvironmentPhase`, `Actor`, `CiContext`, `WorkflowInfo`, `EnvironmentStatus` (incl. `pending_step` — the step a `PendingApproval` environment parks on, F-20b's approvals-409 key), `ChangeRequestRef`, `RepoRef` (§5) |
| `error` | Release error codes (`StepFailed`, `MergeTimeout`, `ApprovalTimeout`, `ApprovalRejected`, `VersionNotVerified`, `ConcurrencyRejected`, `ChangeRequestModified`) and the typed `LibraryError` (§10.1) |
| `version` | Version schemes `semver` / `calver` / `opaque`: validation plus the ordering supersession uses  |
| `template` | `PipelineTemplate`: inputs, environment inputs, `step_groups`, steps, gates; source strings kept verbatim so snapshots stay faithful to the source. `template::compile` (moved from the engine so the CLI's offline `validate` shares it, 4.8/US-13/F-30): the offline compiler — structural validation, `include:` expansion, auto/declared step IDs, wait bounds, F-27a gate-vs-scheme checks, duration parsing (`humantime`); the registry-aware `compile_with` stays engine-side (it owns the step registry)  |
| `groups` | The application-group machinery (§4.13a): `MatchPattern` (placeholders capture INSIDE segments — `apps/{application}/{environment}.yaml`), `discover` (each declared environment matches its own `match`, the global pattern otherwise; skips carry reasons — invalid names, undeclared environments), `materialise` (a discovered app from its group with `{application}` resolved in `edits[].file`), the `extends` merge (maps deep, lists replace, `source` whole; the environment bools do not inherit — serde cannot distinguish unset), and `resolve` (the config's RAW applications become the RESOLVED registry every consumer reads; explicit apps must declare `source`/`template`) |
| `config` | The server config's VALUE TYPES (§4.17/§13; moved from the server so the CLI's offline `validate` parses the same file, 4.8/US-13): every top-level section (`server`, `database`, `leader_election`, `auth`, `secrets`, `providers`, `extensions`, `templates`, `engine`, `reconciler`, `retention`, `limits`, `network`, `logging`, `metrics`, `applications`, `application_groups`), the registry entries (`RawApplicationEntry` — the FILE shape whose `extends` overrides may omit `source`/`template`; the resolved `ApplicationEntry` the resolution produces; `EnvironmentEntry` incl. the group-only per-env `match`; `ApprovalPolicy`, `PrincipalSelector`, versioning, triggers, groups/discovery — the group's org-wide `source` carries `owner_id`, `id` is optional there and required on a resolved app) and the secret-shaped values (`{ file }` / `{ env }` / `{ secret }`); the loader (env overlay, interpolation, materialisation) stays server-side — core is I/O-free |
| `validation` | The registry/trust validation's PURE rules (F-80, F-96's wiring, F-99a, F-147, §13.1 — shared by the server's boot and the CLI's offline `validate`, 4.8/US-13): OIDC claim constraints, API-key uniqueness + the PHC hash SHAPE (the boot-exact argon2 parse stays server-side), releaser/approver references + grants, the ref-glob/tag-format cross-check, the registry-`approval`-needs-template-`wait: approval` invariant, the placeholder grammars (tag format, branch format, `commit_message`, `change_request.{title,body}`), the offline `inputs.<name>`/`env.inputs.<name>` reference check (a gate or `${{ ... }}` parameter naming an input no `inputs`/`environment_inputs` block declares refuses), `extends` group references. The caller supplies the parsed template map — the file reading is I/O and stays with the callers |
| `provider` | The async `Provider` trait (branches, commits, CRs — incl. the draft flag on create, files, labels, statuses, protection checks, tag-object reads for the F-95 version policy, optional auto-merge/tag/release/webhooks, and the batched `get_change_requests` whose default serves per-row — F-69, plus `list_tree` — the recursive path listing §4.13a's discovery scans match against, refusing a truncated tree rather than under-discovering), `ProviderCaps` (incl. `tag_lookup` — the tag verification fails CLOSED when off), edit types |
| `edits` | Structural edit application over JSON/YAML/TOML with dot-notation paths — never text substitution |
| `step` | `StepType` trait, `StepContext`, `StepOutput`, `EnvRef` (carries the registry's per-environment facts: repo, edits, commit message, the `require_branch_protection` flag — default `true`, so a missing stamp fails closed — and the R11 `change_request` overrides, `ChangeRequestEnv`), the `HttpService` seam for SSRF-guarded HTTP (the server and sidecar clients implement it)  |
| `registry` | `ProviderRegistry` (resolve by name) and `CredentialStore` (resolve `{ secret: <name> }`)  |
| `webhook` | Provider-neutral `NormalisedEvent` shapes: `TagPush`, `ChangeRequestClosed` (carry the repo id + the sender for the correlation, plus the repository path and the installation id the receiver's F-49/F-51 allowlist matches), `Unrecognised`; plus `normalise_github` — the shared HMAC-SHA256 delivery verification (constant-time, two-secret rotation) both the GitHub provider and the engine's harness mock run, so their behaviour cannot drift |

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
