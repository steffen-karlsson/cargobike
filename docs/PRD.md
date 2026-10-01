# Cargobike &mdash; Product Requirements Document

> _Durable delivery, one pedal at a time._

**Version**: 0.5.0 (draft)
**Date**: 2026-10-01
**Status**: Draft

---

## 1. Overview & Rationale

Cargobike is an open-source, configurable, and extensible release orchestration
workflow engine. It coordinates the progression of software releases through
user-defined environments &mdash; creating branches, opening change requests,
waiting for merge signals, and tracking status &mdash; all backed by durable
execution that survives crashes and restarts.

The project is built from scratch as an independent, open-source project
that is not tied to any specific repository structure, deployment manifest
format, or environment hierarchy.

### What Cargobike is

- A **release orchestration engine** that drives releases through configurable
  environment sequences using durable, crash-resilient workflows.
- A **centralized server** that persists state in PostgreSQL, receives webhook
  events for auto-triggering, and exposes a REST API.
- A **CLI client** for creating, inspecting, and advancing releases &mdash;
  usable from a laptop or from CI pipelines.
- An **extensible platform** where git providers, step types, and custom
  logic are pluggable via WASM modules or out-of-process sidecars.

### What Cargobike is not

- A UI product. There is no web dashboard.
- A cluster manager. There is no Kubernetes integration.
- A CI/CD runner. Cargobike coordinates; it does not execute builds or tests.
- A deployment tool. Cargobike opens change requests and tracks merges; it
  does not deploy to clusters.

### Why durable workflows?

Release orchestration is inherently long-running. A release may take hours,
days, or weeks as it progresses through environments waiting for human
approval and merges. Traditional tools lose state on restart and require
external polling. Cargobike uses **DBOS Transact (Rust)** to checkpoint every
workflow step to PostgreSQL. If the server crashes, workflows resume from
their last completed step on restart &mdash; no lost progress.

DBOS step execution is **at-least-once**, not exactly-once: Rust has no
transactional step yet. Every step must therefore be idempotent &mdash;
producing the same result when re-run (see [A1](#a1-idempotent-steps)). The
engine designs for this from the start.

---

## 2. Goals / Non-goals

### Goals (v1.0)

| ID  | Goal |
|-----|------|
| G-1 | Durable, crash-resilient release workflows backed by PostgreSQL via DBOS Transact (Rust) |
| G-2 | Configurable workflows via YAML pipeline templates with a step-type DSL and CEL conditions |
| G-3 | Pluggable git provider abstraction with GitHub as the initial provider, extensible via sidecar (v1.0) or WASM (v1.1) |
| G-4 | Sidecar-based extension system for providers and step types in v1.0; WASM in v1.1 |
| G-5 | Server receives webhook events to auto-trigger release workflows; fast-ack, durable processing |
| G-6 | CLI client for creating, listing, inspecting, cancelling, watching, retrying, approving, and deleting releases; CI-friendly |
| G-7 | Authentication via OIDC tokens (with audience and claim checks) and multiple named API keys |
| G-8 | REST API (versioned `/api/v1`) with cursor pagination, SSE streaming, and OpenAPI spec |
| G-9 | Canonical HTTP error responses (RFC 9457 Problem Details) with structured error codes |
| G-10 | Two static binaries: `cargobike` (CLI, no DB deps) and `cargobike-server` (server) |
| G-11 | Conventional commit-driven semantic versioning for Cargobike's own releases |
| G-12 | Application registry: server-side declarations of which apps exist, their workflows, target repos, who may release them, and how versions are validated |
| G-13 | Immutable per-release event log for audit trail |
| G-14 | Embeddable: server published as a library for organisations that compile their own workflows (v1.1) |

### Non-goals (v1.0)

| ID   | Non-goal |
|------|----------|
| NG-1 | Web UI or dashboard |
| NG-2 | Kubernetes cluster management or infrastructure provisioning |
| NG-3 | Build execution, test running, or artifact creation |
| NG-4 | Deployment to clusters (Helm, kubectl, etc.) |
| NG-5 | Full RBAC with role hierarchies and group mappings (deferred to v2) |
| NG-6 | Multi-tenant isolation or per-team namespaces |
| NG-7 | OAuth device flow or browser-based authentication |
| NG-8 | Built-in notifications via email, Slack, or other external channels (extensible via `http-call` step type) |
| NG-9 | Multi-replica active high availability (single active replica for v1, enforced via leader election) |
| NG-10 | WASM extensions (v1.1) |

### Goals deferred to v1.1

- WASM extension system (G-4 partial)
- `wait-for-check` step type
- `tag` and `release` step types
- Missed-delivery recovery (reconciler covers the gap)
- `plan` command (server mode)
- Tier 3 library embedding (G-14)

---

## 2a. Naming Conventions

These rules apply to every user-facing configuration surface: server
config, application registry, application groups, OIDC trust entries, API
keys, providers, pipeline templates, step parameters, CLI commands and
flags, CLI config, and environment variables.

| # | Rule | Applies to | Example |
|---|---|---|---|
| R1 | `snake_case` for every config key | All YAML files | `allow_direct_commit` |
| R2 | Booleans are positive. `allow_*` defaults to `false`; `require_*` defaults to `true`. No `forbid_*`, `disable_*`, or `no_*` | All booleans | `allow_self_approval: false` instead of `forbid_self_approval: true` |
| R3 | Durations are always strings with a unit, parsed by `humantime` | `timeout`, `interval`, retention, skew | `72h`, `7d`, `30s` |
| R4 | Sizes are always strings with a binary unit | Body and output limits | `1MiB` |
| R5 | One secret-reference shape everywhere: `{ file: ... }`, `{ env: ... }`, or `{ secret: <name> }` (referencing the `secrets:` section). A key that may hold two secrets for rotation takes a list. Secret-valued fields accept a literal string (discouraged, warned at startup), `{ file }`, `{ env }`, or `{ secret }` | All secrets | `webhook_secrets: [{ file: ... }]` |
| R6 | Provider IDs are always quoted strings, in config *and* claims. Claim matching compares strings | `id`, `owner_id`, `repository_owner_id` | `"123456"` |
| R7 | One syntax per purpose (see below). Globs use `literal_separator(true)`: `*` matches within one path segment, `**` across segments. Literal asterisks are escaped with `\*` | Patterns, placeholders, expressions | `my-org/*` matches `my-org/repo` but not `my-org/team/repo`; `my-org/**` matches both |
| R8 | Placeholders use full words: `{application}`, `{environment}`, `{version}`, `{release_id}` | Format strings | Not `{name}` or `{env}` |
| R9 | Enum values are lower-case kebab-case in config and on the CLI | `type`, `scheme`, `concurrency`, CLI auth types | `github`, `github-actions` |
| R10 | Environment variables follow the binary: `CARGOBIKE_SERVER_*` for `cargobike-server`, `CARGOBIKE_*` (never `CARGOBIKE_SERVER_*`) for `cargobike`. Every CLI flag `--foo-bar` maps to `CARGOBIKE_FOO_BAR` | Env vars | `--url` &harr; `CARGOBIKE_URL`; `--listen` &harr; `CARGOBIKE_SERVER_LISTEN` |
| R11 | A setting lives in exactly one place. If two places are unavoidable, the template sets defaults, the registry overrides them; lists from the registry are appended to template lists. Security-relevant settings (`concurrency`, `require_branch_protection`, `allow_direct_commit`) are registry-only | Registry vs template | `concurrency` |
| R12 | Words reserved in YAML 1.1 (`on`, `off`, `yes`, `no`, `y`, `n`) are never used as keys or unquoted values | Triggers | `event: tag` instead of `on: tag` |

**Pattern syntax** (R7): four syntaxes, each for exactly one purpose.

| Syntax | Use only for |
|---|---|
| Glob (`*`, `**`, `?`) | All matching: repos, refs, tags, paths, claims |
| `{placeholder}` | Format strings in the registry (tag format, file paths, branch names) |
| `${{ ... }}` (CEL) | Template expressions and gates |
| `${VAR}` | Environment interpolation in the server config file (forbidden inside secret-valued fields) |

Glob matching uses `globset` with `literal_separator(true)`: `*` matches
within one path segment (does not cross `/`), `**` matches across
segments. Literal wildcards are escaped with `\*` or `\?`. Claim values
are glob by default (no opt-in needed), consistent with R7. Regex is
dropped. Where a glob cannot express a rule, an explicit `regex: "..."`
opt-in may be added later. Globset anchors by construction, so
unanchored-pattern checks are unnecessary.

**Principal selector**: a shared shape used for `releasers`,
`approval.approvers`, and trigger `senders`:

```yaml
- oidc: <trust-entry-name>      # references an auth.oidc entry
  claims: { ... }                # optional claim constraints
# or:
- api_key: <key-name>           # references an auth.api_keys entry
                                # claims are not valid on api_key selectors
```

`api_key` selectors are allowed in `releasers` and `senders` but
rejected in `approval.approvers` unless the approval block sets
`allow_machine_approvers: true` (default `false`).

---

## 3. User Stories

### US-1: Create a release from CI

**As a** developer using GitHub Actions,
**I want to** trigger a release from a CI step on tag push,
**so that** the release workflow starts automatically without manual
intervention.

**Acceptance**: Running `cargobike release create my-service 1.2.3` in a
GitHub Actions workflow creates a release and returns its ID. The CLI
auto-detects `GITHUB_REPOSITORY`, `GITHUB_SHA`, and `GITHUB_RUN_ID` from
the CI environment. The CLI requests a GitHub Actions OIDC token with
audience `cargobike` and uses it for authentication. The server validates
the token's issuer, audience, and repository claim against its trust
policy. The server verifies that the version `1.2.3` matches a tag pointing
at the token's `sha` claim (per the application's `versioning` block). The
create request is `{application, version}` only; edits, repos, and
template are resolved from the application registry.

### US-2: List releases

**As a** developer,
**I want to** list all releases with optional filters,
**so that** I can see what is in progress and what has completed.

**Acceptance**: `cargobike release list` shows a table of releases with ID,
application, version, phase, actor, and created-at. Filters:
`--application` (`-a`), `--phase`, `--version`, `--since`, `--limit`.
Output as table (default), JSON, or YAML
(`-o table|json|yaml`).

### US-3: Get release details

**As a** developer,
**I want to** view the full status of a specific release,
**so that** I can understand where it is in the pipeline and what change
requests have been opened.

**Acceptance**: `cargobike release get <id>` shows all metadata, spec, and
status fields including per-environment status, change request links, the
resolved template snapshot, and error information if failed. `--events`
shows the audit trail.

### US-4: Watch a release

**As a** CI pipeline,
**I want to** wait for a release to reach a specific environment and exit
with a meaningful code,
**so that** CI can gate on the outcome.

**Acceptance**: `cargobike release watch <id>` or `cargobike release create
... --wait` streams status updates via SSE
(`GET /api/v1/releases/{id}/watch`). `--until <environment>` returns once
that environment reaches a terminal state (e.g., wait until preview
complete). `Skipped` counts as success (exit 0 with a message). Without
`--until`, exit 0 means all environments completed. Reconnects resume via
`Last-Event-ID`. Exit codes:

| Exit code | Meaning |
|-----------|---------|
| 0 | Completed (all environments, or the `--until` environment completed or was skipped) |
| 1 | Failed |
| 2 | Canceled or Superseded |
| 3 | Timed out (CLI `--timeout`) |
| 10 | Authentication failed |
| 11 | Server unreachable |
| 12 | Invalid response |

### US-5: Cancel a release

**As a** developer,
**I want to** cancel a running release,
**so that** I can stop a release that should not proceed.

**Acceptance**: `cargobike release cancel <id>` (dedicated endpoint
`POST /api/v1/releases/{id}/cancel`) calls `DBOS::cancel`, marks the
release as `Canceled`, records the event in the audit log, and starts a
cleanup workflow to close any open change requests and delete branches.

### US-6: Delete a release

**As a** an admin,
**I want to** delete a terminal release,
**so that** I can clean up old records while preserving the audit trail.

**Acceptance**: `cargobike release delete <id>` is only allowed for releases
in a terminal phase and requires the `release:delete` grant. The release record is
deleted but the immutable event log is retained per the retention policy.

### US-7: Retry a failed release

**As a** developer,
**I want to** retry a failed release,
**so that** I don't have to re-enter all the details.

**Acceptance**: `cargobike release retry <id>` forks the failed DBOS workflow
from the last failure point (`ForkFrom::LastFailure`). The forked workflow
inherits recorded results of every step before the failure, so earlier
environments are not re-deployed. The release keeps the same ID; an attempt
record is appended to `status.attempts[]`. `--new` creates a new release
with `retried_from` set to the original ID.

### US-8: Auto-trigger via webhook

**As a** a repository owner,
**I want to** configure Cargobike to receive webhook events on tag push,
**so that** releases are triggered automatically when a new version is
tagged.

**Acceptance**: The server has `POST /webhooks/{provider}` endpoints. The
server verifies the webhook signature, persists the raw event, starts a
DBOS workflow to process it, and returns 202 within milliseconds. The
webhook-triggered release is authorized by "verified signature + matching
trigger declared in the application registry"; the provider's `sender` is
recorded as the actor.

**Recommended setup**: Use the webhook trigger to start the release
automatically on tag push. In CI, use `cargobike release watch <id>
--until <env>` to gate on the outcome. Both the webhook and the CLI may
fire for the same tag; the duplicate create returns the existing release
with 200 (F-109), so neither path is wasted.

### US-9: Define a custom workflow in YAML

**As a** a platform engineer,
**I want to** define a pipeline template with a custom environment sequence
(e.g., `preview` then `production`),
**so that** the release pipeline matches my organization's structure.

**Acceptance**: Pipeline templates are YAML files in a `templates/` directory.
Each template declares typed inputs, environments, per-environment step
sequences, and gate conditions using CEL expressions. The template
definition is snapshotted into each release at creation time, so editing
YAML never breaks an in-flight release. Templates are reusable across
applications: the application registry supplies the inputs.

### US-10: Install a sidecar extension

**As a** a user with a proprietary git provider,
**I want to** run a sidecar process that implements the provider interface,
**so that** I can use Cargobike with my internal git platform without
forking the project.

**Acceptance**: The user writes a sidecar in any language (Go, Python,
Rust) implementing the same WIT-derived schema over HTTP or gRPC. The
server config declares the sidecar endpoint and its secrets. The
sidecar holds its own credentials.

### US-11: Authenticate to the server

**As a** CI pipeline or developer,
**I want to** authenticate to the Cargobike server using an OIDC token or API
key,
**so that** I can make authenticated API calls.

**Acceptance**: The server validates Bearer tokens against configured OIDC
issuers, checking issuer, audience, signature, expiry, and claim patterns.
The CLI requests GitHub Actions OIDC tokens with audience `cargobike`. API
keys are multiple, named, hashed at rest, scoped to applications and
operations. Token is never logged.

### US-12: Observe release health

**As a** an operator,
**I want to** check server health and readiness,
**so that** I can configure load balancers and orchestration systems.

**Acceptance**: `GET /api/v1/live` returns 200 always. `GET /api/v1/ready`
returns 200 when the server is ready (and holds the leader-election lock),
503 otherwise. `GET /api/v1/startup` returns 200 after startup.

### US-13: Validate and plan

**As a** a developer,
**I want to** validate my config and workflow files and dry-run a release,
**so that** I can catch errors before creating a real release.

**Acceptance**: `cargobike validate` checks config and workflow YAML offline
(local files, mock provider). JSON Schemas are published for editor
autocompletion. `plan` (v1.1) performs a server-side dry run.

### US-14: Audit a release

**As a** an operator,
**I want to** view the complete event history of a release,
**so that** I can audit who shipped what to production and when.

**Acceptance**: `GET /api/v1/releases/{id}/events` returns an immutable log
of every state transition with actor, timestamp, and reason. Events have
deterministic IDs (workflow ID + step ID) with `ON CONFLICT DO NOTHING` to
handle at-least-once execution.

---

## 4. Functional Requirements

### 4.1 Release Model

| ID  | Requirement |
|-----|-------------|
| F-1 | A release has `metadata`, `spec`, and `status` as the only root-level properties |
| F-2 | Release IDs are UUIDv7 (time-ordered, sortable) |
| F-3 | A create request is `{application, version}`. The application registry resolves the template, edits, repos, and who may release. The application may be explicitly registered (section 4.13) or discovered via an application group (section 4.13a) |
| F-4 | `spec.version` is an opaque string. The application's `versioning` block validates it (e.g., "must match a tag pointing at the token's SHA"). The character set is validated per scheme (semver, calver, opaque) |
| F-5 | The template is registry-only: the application entry declares the template. The caller cannot select a template |
| F-6 | `status.phase` is one of: `Pending`, `Running`, `PendingApproval`, `Failed`, `Canceled`, `Superseded`, `Completed` |
| F-7 | `status.environments` is a per-environment status array: `{name, phase, change_request, started_at, completed_at}`. Environment phases include `Skipped` (gate false or no changes) and `Waiting` (held by concurrency policy) |
| F-8 | `status.error` contains `{code, message}` when the release has failed. Error codes: `StepFailed`, `ApprovalTimeout`, `ApprovalRejected`, `VersionNotVerified`, `ConcurrencyRejected`, `ChangeRequestModified` |
| F-9 | `metadata` includes typed timestamps (`created_at`, `updated_at`), `resource_version` for optimistic concurrency (honoured via `If-Match`), `retried_from`, free-form labels/annotations, and `attempts[]` listing workflow IDs |
| F-10 | Repositories are opaque: `RepoRef { provider, id }`. The `id` field is a string (the provider's immutable ID, e.g. GitHub repository ID), used for authorization. An optional `path` (e.g. `my-org/my-repo`) may be declared as a **verified label**: the server checks it against the provider at startup. On mismatch, the affected application is marked unavailable (visible in `GET /api/v1/applications` and `cargobike application list`) and retried in the background; the server does not fail. The strict, fail-on-error check runs in `cargobike validate --resolve`. The `path` is otherwise fetched from the provider at runtime via `get_repo_path`. Target repos must also declare `id` |
| F-11 | Source and target are separate: `spec.source` (where the version came from) is resolved from the application registry, not caller-supplied. Target repos are declared per environment in the registry |
| F-12 | Pre-release gating is defined per pipeline template in CEL, not hardcoded in the engine |

### 4.2 Workflow Engine

| ID  | Requirement |
|-----|-------------|
| F-13 | Workflows are durable DBOS Transact (Rust) workflows, checkpointed to PostgreSQL |
| F-14 | Step execution is at-least-once. Every step must be idempotent (see A1) |
| F-15 | One DBOS workflow is registered (`cargobike.interpret.v1`) at startup. The interpreter reads the snapshotted template and executes a sequence of DBOS operations. The sequence of DBOS operations is a pure function of the snapshot and the recorded outputs |
| F-16 | When a release is created, the resolved template definition and registry inputs are snapshotted into the release along with a content hash. The interpreter reads only that snapshot, so editing YAML or the registry never breaks an in-flight release |
| F-17 | In-flight safety rules apply only to changes in the interpreter itself, which the project controls |
| F-18 | Adding, removing, or reordering DBOS operations in the interpreter is breaking for running workflows. New interpreter versions must be registered alongside old ones until in-flight instances complete |
| F-19 | Modifying logic within an existing interpreter step (same position, same return type) is safe for in-flight workflows |
| F-20 | Workflows block on `dbos::recv` waiting for external signals (merge, approval). External systems call the notification/webhook endpoint, which calls `dbos::send` to unblock |
| F-20a | **Signal topics**: Each `dbos::recv` specifies a topic string so messages for different waits are never confused. Topics: `merge/{environment}/{step_id}` for `wait: merge`, `approval/{environment}/{step_id}` for `wait: approval`, `lease/{environment}` for queue hand-offs. Every `dbos::send_with` includes an idempotency key derived from the source event (delivery ID, approval ID) so a redelivered webhook or retried reconciler pass does not produce two signals |
| F-20b | **Early messages are rejected**: The approvals endpoint rejects submissions unless the environment is currently `PendingApproval` at that step (see F-96). This prevents stale messages from sitting in the queue and being consumed later by a different attempt |
| F-20c | **Fork routing**: A forked retry has a new workflow ID. Signals are addressed to the current attempt's workflow ID (`status.attempts[-1]`). Whether pending messages follow the fork is controlled by the crate's `Forks` option; the design chooses one and documents it |
| F-21 | On server restart, DBOS recovers in-flight workflows by replaying steps. Steps that already completed are skipped |
| F-22 | Recovery only picks up workflows whose recorded application version matches the running binary. The app version is pinned explicitly; workflow versioning is at the workflow-name level |
| F-23 | `DBOS::cancel` pauses a workflow. `DBOS::fork` with `ForkFrom::LastFailure` retries from the failing step. A forked retry keeps the same release ID |
| F-24 | There is no built-in scheduler. The cleanup job runs as a long-lived durable workflow looping on `dbos::sleep`, which resumes at the original wake time after a crash |

### 4.3 Workflow Templates (YAML)

| ID  | Requirement |
|-----|-------------|
| F-25 | Workflows are defined as YAML pipeline templates loaded from a `templates/` directory at server startup. No recompilation is needed to add or modify a template. Each template declares a `version` (e.g. `"1"`, `"2"`) so breaking changes to the step sequence are explicit. The version is snapshotted into the release (`status.workflow.template_version`). The file name is free; the `version` in the file is authoritative. Multiple versions of the same template name can coexist as separate files (`service@1.yaml`, `service@2.yaml`); the registry references the version |
| F-26 | Each template declares typed inputs, environments, per-environment step sequences, and gate conditions. Gates reference environments by name, never by index |
| F-27 | Gate conditions use CEL (the `cel` crate): sandboxed, deterministic, not Turing-complete. Cost and size limits are configured. No impure custom functions (no `now()`), since gates are evaluated during replay |
| F-27a | `cargobike validate` checks gate fields against the application's version scheme. A gate referencing `semver(release.version).prerelease()` errors for schemes without a pre-release component (`calver`, `opaque`) |
| F-28 | Each step has an optional `id` for output referencing. Steps reference a step type by name (`uses`) with parameters (`with`). Later steps can reference `steps.<id>.outputs.<field>`. `when` may be set on environments and on individual steps |
| F-29 | The template definition is snapshotted into the release at creation time, along with resolved registry inputs and built-in step-type versions |
| F-30 | `cargobike validate` checks template YAML offline. JSON Schemas are published for editor autocompletion |
| F-31 | Templates support a `concurrency` policy per environment: `supersede`, `queue`, or `reject` (see 4.8) |
| F-32 | Template expressions use `${{ ... }}` (CEL) syntax. Server config interpolation uses `${VAR}` syntax. The two never overlap because templates are loaded by a separate parser that does not expand `${VAR}`. However, the application registry lives *inside* the config file, so registry fields that reference the version (e.g. `tag_format`) use a simple `{version}` format-string placeholder, not `${{ }}` or `${}`. This avoids the `${{` being parsed as a malformed `${VAR}` by the config interpolation. A unit test loads the documented example config verbatim |
| F-32a | **Input scoping**: `inputs` are application-wide values supplied by the registry. `environment_inputs` are per-environment values (e.g. different target repos per environment). Validation requires every environment to supply every per-environment input. The registry's `environments.<name>.repo` and `environments.<name>.edits` are well-known entries in `env.inputs`. The registry may also supply custom `inputs` (application-wide) and `environments.<name>.inputs` (per-environment) as generic maps, accessible as `${{ inputs.<name> }}` and `${{ env.inputs.<name> }}` respectively |
| F-32b | **Reusable step lists**: Templates may define `step_groups:` and reference them from multiple environments (e.g. `steps: [include: deploy-steps]`), preventing duplicate step sequences from drifting apart |

### 4.4 Step Types

| ID  | Requirement |
|-----|-------------|
| F-33 | There are two categories of steps: **control steps** (interpreter-native, run in the workflow body, not extensible) and **action steps** (run inside `dbos::step`, extensible via built-in or sidecar) |
| F-34 | Control steps: `wait: merge` (recv + verification loop), `wait: approval` (recv), `wait: sleep` (dbos::sleep). These use a distinct `wait:` keyword in templates, are part of the interpreter, and are not registered step types. A sidecar cannot register a step named like a control step. `wait: sleep` takes a `duration` (not `timeout`). `wait: merge` and `wait: approval` take `timeout` (required, no default) and `on_timeout: fail | cancel` (default `fail`). For `wait: merge`, `on_timeout: fail` produces `MergeTimeout`; for `wait: approval`, it produces `ApprovalTimeout`. `on_timeout: cancel` sets the environment and release to `Canceled` (CLI exit code 2). `on_modified` defaults to `fail`. Wait steps that omit `id` get an auto-generated deterministic ID (e.g. `merge-0`, `approval-0`); uniqueness is enforced after `include` expansion |
| F-35 | Action steps (v1.0): `builtin/commit-files@1` (structured edits + commit, fused), `builtin/change-request@1`, `builtin/http-call@1`, `builtin/set-labels@1`. Action steps accept optional `timeout` and `retry: { attempts, backoff, max_delay }` (exposing `StepOptions`, §6.7). `retry.attempts` is total attempts (default `1`, no retry). `retry.backoff` is `fixed` or `exponential` (default `fixed`); `retry.initial_delay` is optional. `retry.max_delay` caps the backoff |
| F-36 | Action steps (v1.1): `builtin/tag@1`, `builtin/release@1`, `builtin/wait-for-check@1` |
| F-37 | Built-in step types are versioned (`@1`). The resolved versions are recorded in the snapshot. Changing a step's output schema is as breaking as changing the interpreter |
| F-38 | `set-status` and `close-superseded` are interpreter behaviour, not user-visible steps. A template cannot omit them |
| F-39 | Action steps receive a `StepContext` with a provider registry (resolve by `RepoRef.provider` name), credential references, the SSRF-guarded HTTP client, an idempotency key (`release_id / environment / step_id`), a cancellation token, and a logger |
| F-40 | `StepOutput` carries a `serde_json::Value` payload. Output size is limited (configurable, default 1 MB) to avoid storing large file contents in PostgreSQL |
| F-41 | `commit-files` uses structured edits (dot-notation paths for YAML/JSON, key paths for TOML), never text substitution. The `edits` list declares which files and fields to update. Each edit has `file`, optional `format` (yaml, json, toml; inferred from extension if omitted), `field` (dot-notation path), and optional `value` (defaults to the release version). Resolved file paths are confined to globs declared in the application registry |
| F-42 | Sidecar step types (v1.0): a sidecar can implement any action step type. The SHA-256 of the sidecar's declared schema is recorded in the snapshot. WASM step types (v1.1) record the module's SHA-256 in the snapshot |

### 4.5 Provider Abstraction

| ID  | Requirement |
|-----|-------------|
| F-43 | The server communicates with git providers through an async `Provider` trait (no DBOS dependency) |
| F-44 | The initial provider is GitHub (`cargobike-provider-github`), supporting GitHub Enterprise Server via `api_url` config |
| F-45 | Additional providers can be added as sidecars (v1.0) or WASM extensions (v1.1) |
| F-46 | The provider trait covers: creating branches, committing files, creating change requests, getting a CR, finding a CR by head branch, closing a CR, listing open CRs, reading files, commenting, adding labels, updating branch, default branch name, branch SHA, setting commit status, checking branch protection, checking tag protection, and optionally: auto-merge, tags/releases, webhook verification + parsing |
| F-47 | Providers expose capability discovery (`fn capabilities() -> ProviderCaps`) |
| F-48 | The neutral term "change request" is used throughout (GitHub PRs, GitLab MRs) |
| F-49 | Provider configuration is a `providers:` list with `type` (`github`, `extension`), `api_url`, optional `web_url`, and per-provider repository allow/deny lists using globs. A provider served by an extension is declared as `type: extension` with `extension: <name>`, linking to the `extensions:` section entry that declares the transport and channel auth. A convenience env-var shortcut exists for a single GitHub provider |

### 4.6 Webhook System

| ID  | Requirement |
|-----|-------------|
| F-50 | The server exposes `POST /webhooks/{provider}` endpoints |
| F-51 | The server verifies the signature before parsing the payload. Secrets are compared in constant time. Request body size is limited. For GitHub App deliveries, the installation and repository are checked against the allowlist. Two secrets are accepted at once for rotation (`webhook_secrets` list) |
| F-52 | The handler acknowledges fast: verify signature, persist the raw event, start a DBOS workflow whose ID is `{provider}-delivery-{delivery_id}`, return 202 within milliseconds |
| F-53 | Duplicate deliveries become no-ops (DBOS workflow ID deduplication) |
| F-54 | Trigger rules live in the application registry, not in a separate webhook config. A trigger entry (nested inside an application) specifies `event: tag`, optionally `require_tag_protection: true`, and optionally `senders` (a principal selector list). The tag pattern is derived from `versioning.tag_format` |
| F-55 | Reconciliation runs every N minutes (configurable, default 5). The server checks the actual CR state for each release in `PendingApproval`. Webhooks then become a latency optimisation rather than a single point of failure |
| F-56 | Unrecognized events are acknowledged (200) and ignored. Invalid signatures return 401 |
| F-57 | The CLI can also be used from CI to achieve the same trigger effect |
| F-58 | Webhook secrets are declared per-provider (`webhook_secrets` list of `{ file: ... }` references), not in a top-level credentials section. The list allows two secrets for rotation |

### 4.7 Notification, Approval & Merge Verification

| ID  | Requirement |
|-----|-------------|
| F-59 | `POST /api/v1/releases/{id}/approvals` advances a blocked `approval` step. The request specifies `(environment, step_id, decision)`. The caller must satisfy the step's `approval.approvers` policy. `allow_self_approval: false` is the default |
| F-60 | `POST /api/v1/releases/{id}/cancel` cancels the release (dedicated endpoint, not a notify event) |
| F-61 | Merges arrive via webhook correlation and the reconciler only. There is no public `Merged` notify event |
| F-62 | The `wait: merge` control step blocks on `dbos::recv` (topic `merge/{environment}/{step_id}`), then re-verifies with the provider that the CR is merged. If the CR is closed without merge, the environment fails with `ApprovalRejected`. If the CR is not yet merged, the step keeps waiting. For the merged case, the step verifies content: after the merge, it reads the target files on the base branch and checks that the intended values are present at the intended pointers. Alternatively, `on_modified: fail | accept` controls the behaviour when the head SHA has changed (e.g. a reviewer pushed a fix-up commit or `update_branch` merged the default branch). `fail` produces a clear `ChangeRequestModified` error instead of looping |
| F-63 | The server stores `(provider, repo_id, cr_number) -> (release_id, environment)` when opening a CR. A `pull_request.closed` webhook correlates automatically via this mapping |
| F-64 | The handler pre-verifies for fast feedback (synchronous 409 if the CR is clearly not merged). The workflow step re-verifies authoritatively |
| F-65 | The `change-request` step checks branch protection via a provider capability and fails closed (`require_branch_protection: true` by default, declared on the registry environment). A merge is only a review if the base branch requires reviews |
| F-66 | If a template commits straight to a default branch (`allow_direct_commit: true` in the registry), the GitHub App needs bypass rights. This is an explicit per-environment registry setting. The minimal GitHub App permission set is documented |

### 4.8 Cleanup & Reconciliation

| ID  | Requirement |
|-----|-------------|
| F-67 | A periodic reconciliation job finds releases in `PendingApproval` and checks the actual CR state. The reconciler only `send`s signals (merged, closed) to the workflow with an idempotency key; the workflow remains the only writer of its release's status |
| F-68 | The reconciliation job runs as a long-lived durable workflow looping on `dbos::sleep`, every N minutes (configurable, default 5) |
| F-69 | The reconciler batches CR lookups (GraphQL) and uses conditional requests with ETags to avoid exhausting API quotas |

### 4.9 Supersession

| ID  | Requirement |
|-----|-------------|
| F-70 | When a new release opens a CR for an application + environment, existing open CRs for the same app + env may be closed, subject to the environment's `concurrency` policy: `supersede`, `queue`, or `reject` |
| F-71 | Work per `(application, environment)` is serialised via a **lease row** `(application, environment) -> holder_release_id`, acquired in a step with a conditional INSERT. Each environment's lease is released when *that environment* completes, is skipped, or is superseded &mdash; not when the entire release reaches a terminal state. This prevents preview from being locked while the release waits days for production approval. For the `queue` policy, waiting releases block on a `recv` (topic `lease/{environment}`) that the previous holder (or the reconciler) sends when it releases the lease |
| F-72 | Never supersede a higher version (using the version scheme's ordering). When the version scheme is `opaque`, ordering falls back to creation time |
| F-73 | Superseded releases are cancelled (not written to directly). The new release sends a cancel signal to the old workflow. `cargobike.cleanup.v1` (started from the cancel path) closes the CR, deletes the branch, posts a comment linking to the new CR, and releases or transfers the lease. Under `supersede`, the lease is transferred atomically from the old holder to the new release in the same statement that records the hand-over, so there is no window in which a third release can grab it. A lower version arriving later (blocked by F-72) fails with `ConcurrencyRejected` |
| F-74 | Error code for `reject`: `ConcurrencyRejected` |

### 4.10 Cleanup After Cancel and Supersede

| ID  | Requirement |
|-----|-------------|
| F-75 | `DBOS::cancel` stops the workflow at its next step, so a cancelled workflow cannot run its own compensation. A separate `cargobike.cleanup.v1` workflow is started from the cancel and supersede paths to close the CR, delete the branch, post a comment, and release or transfer the lease with a conditional `UPDATE … WHERE holder = <old release>` |
| F-76 | `Canceled` is terminal in Cargobike's model. DBOS's ability to resume a cancelled workflow is not exposed |

### 4.11 Authentication

| ID  | Requirement |
|-----|-------------|
| F-77 | The server validates Bearer tokens against configured OIDC issuers. Validation: parse issuer (exact match), fetch OIDC discovery + JWKS (cached; refetch on unknown `kid` with rate limiting), verify signature, validate expiration, `nbf`/`iat` with clock-skew allowance (configurable via `clock_skew`, default `60s`), validate `aud` claim (audience is required). Optional `jwks_url` override and `algorithms` allowlist per issuer |
| F-78 | Allowlist signing algorithms: reject `none` and `HS*` (accept RS*, PS*, ES*) |
| F-79 | Claim matching: all listed claims must match (AND). Claim values are glob by default (R7). Array-valued claims (e.g., `groups`) match if any element matches. Prefer immutable IDs (`repository_id`, `repository_owner_id`) over names |
| F-80 | `cargobike validate` and server startup reject OIDC trust entries without claim constraints unless an explicit `allow_unconstrained: true` is set |
| F-81 | Placeholder issuers (e.g., `https://sts.windows.net/{tenant}/`) are documented with substitution semantics, or real values are shown |
| F-82 | Tags are a trust boundary: anyone with write access can push a tag. The quickstart recommends protected tags (GitHub rulesets). A provider capability checks that the trigger's tag pattern is protected, and the trigger is refused unless it is (`require_tag_protection: true` by default, declared on the trigger). Triggers may optionally declare a `senders` constraint (principal selector) |
| F-83 | **Actor model**: Record `(issuer, subject)` plus a display name. GitHub Actions tokens carry no email; for CI actors, store `CiContext { provider, repository, repository_id, workflow_ref, run_id, run_url }` (a generic type, not GitHub-specific) |
| F-84 | Verified values (commit SHA, ref, run ID) are taken from the OIDC token claims, not from caller-supplied body fields. Body-supplied values are stored only as untrusted annotations |
| F-85 | **API keys**: Multiple named keys, stored hashed (argon2), with grants, optional `expires` (date), and `description`. Two keys can be active at once for rotation. Compared in constant time. API key grants *are* the authorization (not "bypassed"). API keys carry `grants` only (no `applications` list); applications reference them via `releasers` with `api_key: <name>`, the same single-direction binding as OIDC entries |
| F-86 | A bootstrap admin key can be provided via `CARGOBIKE_SERVER_BOOTSTRAP_API_KEY` (plaintext, for first-time setup only). It is hashed at startup, logged as a warning, and must be replaced by adding a hashed key to the config file and removing the environment variable (there is no key-management API in v1). The bootstrap key is restricted to requests from localhost only, so a forgotten variable cannot become a network-reachable admin key. Alternatively, `CARGOBIKE_SERVER_BOOTSTRAP_API_KEY_FILE` is used |
| F-87 | Tokens and API keys are never logged. `secrecy::SecretString` for all secret values. Config structs hand-write `Debug` (do not derive). A `RedactingWriter` intercepts log output as defence in depth |
| F-88 | `*_FILE` variants for every secret: `CARGOBIKE_SERVER_BOOTSTRAP_API_KEY_FILE`, `CARGOBIKE_SERVER_GITHUB_PRIVATE_KEY_FILE`, `CARGOBIKE_SERVER_DATABASE_URL_FILE`, `CARGOBIKE_API_KEY_FILE` (CLI), etc. |
| F-89 | `${VAR}` interpolation in YAML config fails fast on unset variables. `${VAR:-default}` provides explicit defaults. Literal `${VAR}` is never left in place |
| F-90 | ServerArgs uses `SecretString` for `api_key` and `db` (database URLs contain passwords). `Debug` is hand-written |

### 4.12 Authorization

| ID  | Requirement |
|-----|-------------|
| F-91 | **Application registry**: Each application declares its workflow, target repos, edits (file edits), who may release it (`releasers`), versioning policy, and triggers. See section 4.13. Applications can also be discovered via application groups (section 4.13a) |
| F-92 | **Deny by default**: Repository allow/deny lists use globs (not regexes). Globset anchors by construction, so unanchored patterns are not a concern |
| F-93 | **Create authorization**: The token's `repository_id` must match the application's registered source. For explicitly registered apps, this is the per-repo `source.id`. For apps discovered via application groups, authorization uses `source.owner_id` (any repo in that org can release). Per-app overrides can pin `source.id` for sensitive services |
| F-94 | **Template is registry-only**: The caller cannot select a template. The application entry declares it |
| F-95 | **Version policy**: Per-application `versioning` block validates the version string (e.g., "must match a tag pointing at the token's SHA"). The `versioning` block declares `scheme` (semver, calver, opaque), `tag_format` (e.g. `"v{version}"`), and `require_tag` (default `true`). For OIDC-token creates, the SHA comes from the token's `sha` claim. For API-key creates (no SHA in token), the policy is "tag exists" only, or the request supplies a SHA which the server verifies against the provider. For webhook creates, the SHA in the push payload is verified against the provider's tag object, not the payload alone |
| F-96 | **Approval authorization**: The `approval` control step has an `approval` block per environment with `required` (minimum distinct count, default 1), `allow_self_approval` (default `false`), `allow_machine_approvers` (default `false`; when `true`, `api_key` selectors are accepted), and `approvers` (a list of principal selectors using the shared `{ oidc | api_key, claims }` shape). If the registry declares `approval` for an environment, validation fails unless the template contains an `approval` step for that environment (or the interpreter inserts one automatically). Approver selectors are scoped to a trust entry via `oidc: <name>` plus claims. `required` counts distinct `(issuer, subject)` pairs; one person approving twice does not count twice. A single `reject` decision fails the environment immediately. Approvals are accepted only while the step is in `PendingApproval` for the current attempt; submissions at other times return 409. Each approval is bound to the step's current attempt and the change request's head SHA, so approvals do not carry over to a retry or a changed CR |
| F-97 | **Webhook-triggered releases**: Authorized by "verified signature + matching trigger in the application registry". The provider's `sender` is recorded as the actor |
| F-98 | **Safe auto-discovery**: `/api/v1/clientconfig` supplies only issuer and audience hints. The exec command comes from local config or flags only. The CLI refuses `http://` server URLs except for localhost (override with `--allow-http`) |
| F-99 | v1 has no full RBAC. OIDC trust entries map claims to grants. Grant vocabulary: `release:read`, `release:create`, `release:approve`, `release:cancel`, `release:retry`, `release:delete`, `application:read`, `template:read`, and `"*"` (wildcard, includes all grants). There is no separate `admin` grant; use `"*"` |
| F-99a | **Releaser-grant validation**: To create a release, a caller needs both a matching trust entry with `release:create` *and* to be listed in the application's `releasers`. `cargobike validate` checks: (a) a trust entry referenced in `releasers` must grant `release:create`; one referenced in `approval.approvers` must grant `release:approve`; (b) a releaser's `ref` claim glob must be able to match the application's `tag_format` (e.g. `ref: refs/tags/v*` with `tag_format: "release-{version}"` is a misconfiguration) |
| F-99b | **API-key principal selectors**: `api_key` selectors are valid in `releasers` and `senders`. `claims` on an `api_key` selector are rejected by validation (API keys have no claims). `api_key` selectors in `approval.approvers` are rejected unless the approval block sets `allow_machine_approvers: true` (default `false`). When `api_key` is used as an approver, `required` counts distinct key names |

### 4.13 Application Registry

The application registry is the security core. It is defined in the server
config file (not via env vars for v1.0; a minimal env form may be added
later).

```yaml
applications:
  - name: my-service
    description: Customer-facing API
    labels: { team: payments }
    source:
      provider: github
      id: "123456"                    # immutable GitHub repository ID (string)
      path: my-org/my-service          # verified label: checked against provider at startup
    template: service@1              # registry-only, not caller-selectable (name@version)
    versioning:
      scheme: semver
      tag_format: "v{version}"        # {version} is a format-string placeholder, not CEL or ${VAR}
      require_tag: true
    triggers:
      - event: tag
        require_tag_protection: true
    releasers:
      - oidc: gha-my-org             # references OIDC trust entry by name
    environments:
      preview:
        repo:
          provider: github
          id: "789012"                # immutable GitHub repository ID
          path: my-org/deploy          # verified label
        edits:
          - file: apps/my-service/preview.yaml
            field: image.tag           # dot-notation path to the field to update
      production:
        repo:
          provider: github
          id: "789012"
          path: my-org/deploy
        edits:
          - file: apps/my-service/prod.yaml
            field: image.tag
        concurrency: supersede
        require_branch_protection: true
        allow_direct_commit: false
        change_request:
          labels: [release, production]
        approval:
          required: 1
          allow_self_approval: false
          approvers:
            - oidc: azure-ad
              claims: { roles: [release-managers] }
```

Authorization rules live in the OIDC trust entries (who can
authenticate) and the application registry (what they can do). OIDC
trust entries are named and referenced by `releasers` and
`approval.approvers` in the registry. The binding is single-direction:
trust entries carry `grants` only; applications reference them. This
means one place to audit "who can release my-service".

### 4.13a Application Groups

Application groups provide monorepo-style discovery for organisations that
manage hundreds of services with near-identical release configs. A group
declares shared config once and discovers applications by scanning the
deployment repository.

```yaml
application_groups:
  - name: monorepo-deploy
    template: service@1
    versioning:
      scheme: semver
      tag_format: "v{version}"
    triggers:
      - event: tag
        require_tag_protection: true
    releasers:
      - oidc: gha-my-org
    source:
      provider: github
      owner_id: "123456"            # any repo in this org can release
    discovery:
      repo: { provider: github, id: "789012", path: my-org/deploy }
      match: "apps/{application}/{environment}.yaml"
      ref: main                        # scanned branch (default: repo default branch)
      interval: 10m                    # periodic rescan (configurable)
    environments:
      preview:
        match: "apps/{application}/preview.yaml"
        edits:
          - file: "apps/{application}/preview.yaml"
            field: image.tag
      production:
        match: "apps/{application}/prod.yaml"
        edits:
          - file: "apps/{application}/prod.yaml"
            field: image.tag
        concurrency: supersede
        approval:
          required: 1
          allow_self_approval: false
          approvers:
            - oidc: azure-ad
              claims: { roles: [release-managers] }
```

**How it works**:

1. At startup and on every `interval` (e.g. 10m), the server scans the
   deployment repo for files matching `discovery.match`
   (e.g. `apps/{application}/{environment}.yaml`).
2. The `{application}` and `{environment}` placeholders in the match
   pattern extract the application name and environment from the file
   path (e.g. `apps/my-service/preview.yaml` matches with application
   `my-service` and environment `preview`).
3. If an environment declares its own `match`, that pattern replaces the
   global `discovery.match` for that environment. This is needed when
   file names differ from environment names (e.g. `prod.yaml` for
   `production`).
4. A virtual application entry is created for each discovered app,
   inheriting all group config. The `{application}` placeholder in
   `edits[].file` is resolved to the discovered name.
5. SIGHUP also triggers a rescan.

**Per-app overrides**:

An app discovered by a group can be overridden by an explicit entry in
`applications:` with `extends: <group-name>`. The override inherits
group config and replaces specific fields. Merge rules: maps merge
deeply, lists replace. Exception: `source` replaces as a whole (so
`source: { provider, owner_id }` is fully replaced by
`source: { provider, id }`). The server logs which apps are overridden.

An explicit application with the same name as a discovered one, but
without `extends`, is a validation error (not a silent override).

Discovered application names flow into branch names, change-request
titles, and API paths. They are validated against
`^[a-z0-9][a-z0-9-]{0,62}$`; invalid names are skipped with a warning.

```yaml
applications:
  - name: payment-service
    extends: monorepo-deploy          # inherit everything from the group
    source: { provider: github, id: "345678" }   # pin one repo for a sensitive service
    environments:
      production:
        approval:
          required: 2
          approvers:
            - oidc: azure-ad
              claims: { roles: [senior-release-managers] }
```

**Discovery details**:

- **Scanned branch**: `discovery.ref` (default: the repository's
  default branch). Recommend that the scanned branch is protected;
  otherwise anyone who can push a directory to any branch can create a
  releasable application.
- **Membership**: An application is discovered if it matches in every
  environment that declares `edits`. An app that only has `preview.yaml`
  but not `prod.yaml` is listed with a warning and production releases
  fail at creation time.
- **Triggers**: Groups may declare `triggers` at the group level;
  discovered apps inherit them. Without triggers, discovered apps can
  only be started via CLI, not tag webhooks.

**Security trade-off**:

| Explicit registry | Application group |
|---|---|
| Per-repo `source.id` &mdash; precise, one repo per app | Per-org `source.owner_id` &mdash; any repo in the org can release |
| Every app explicitly listed | Auto-discovered from file scan |
| No accidental exposure | Org members can release any discovered app |

Mitigation: `require_tag_protection: true` on triggers (F-82) ensures
only protected tags can trigger. The OIDC trust entry's `claims` still
constrains which tokens are accepted. Per-app overrides can pin
`source.id` for sensitive services.

### 4.14 API & Pagination

| ID  | Requirement |
|-----|-------------|
| F-100 | The API is versioned as `/api/v1/`. Semver policy: v1 is stable; breaking changes require v2 |
| F-101 | All list endpoints support cursor-based pagination via `limit` (1-500), `after`, and `before` |
| F-102 | `GET /api/v1/releases/{id}/watch` streams status updates via SSE. Events have an `id:` equal to the event-log sequence number. `Last-Event-ID` is supported for reconnects |
| F-103 | Error responses use RFC 9457 Problem Details: `{type, title, status, detail, instance}` with `code` as an extension member |
| F-104 | An OpenAPI spec is generated (with `utoipa`) |
| F-105 | `GET /api/v1/releases/{id}/events` returns an immutable event log. `GET /api/v1/releases/{id}/snapshot` returns the resolved template and registry inputs |
| F-106 | `GET /api/v1/applications` and `GET /api/v1/templates` list registered applications and templates. `GET /api/v1/whoami` returns the resolved identity, matched trust entry, and grants |
| F-107 | Health endpoints and `/api/v1/clientconfig` are unauthenticated |
| F-108 | Output format flag: `-o table|json|yaml` (consistent across CLI) |
| F-109 | Create idempotency: `(application, version)` is unique among non-terminal releases. A duplicate create returns the existing release with 200 (friendlier for CI retries and for the case where both the webhook trigger and the CLI fire for the same tag). An `Idempotency-Key` header is also accepted. `release retry --new` creates a new release only after the previous one is terminal; if the previous release is still active, it returns 409 `DuplicateRelease` |
| F-110 | Create response: 202 with the full release JSON and a `Location` header pointing to the release |

### 4.15 Audit & Event Log

| ID  | Requirement |
|-----|-------------|
| F-111 | Every release state transition is recorded in an immutable event log with actor, timestamp, and reason |
| F-112 | Event IDs are deterministic (workflow ID + step ID) with `ON CONFLICT DO NOTHING` to handle at-least-once execution |
| F-113 | The application's database role has INSERT-only rights on the event table. Retention runs under a separate role. A hash chain provides tamper evidence. Events for one release are written by the workflow and by API handlers (cancel, approval submissions), so appends are serialised per release via a per-release sequence under a row lock. The same sequence serves as the SSE `id:` (F-102). Signed or published checkpoints (the chain head at each retention cut) allow the remaining chain to be verified after old events are deleted |
| F-114 | Retention policy: events are retained even after the release record is deleted. Retention deletes events only after the configured period (default 365 days) |
| F-115 | Raw webhook payloads are stored but brought under the retention policy (they contain personal data: sender names and emails). `retention.webhook_payloads` has its own, shorter period (default `30d`) |
| F-116 | `GET /api/v1/releases/{id}/events` returns the event log |

### 4.16 Security

| ID  | Requirement |
|-----|-------------|
| F-117 | **SSRF protection**: Block link-local (169.254.0.0/16), loopback (127.0.0.0/8), RFC 1918 ranges (10/8, 172.16/12, 192.168/16), IPv6 loopback (::1), and unique-local (fc00::/7) by default. Check the address after DNS resolution, connect to the resolved IP (to defeat DNS rebinding), and re-check on every redirect. Template authors are server administrators. `network.egress.deny` / `network.egress.allow` configure the lists. `network.egress.proxy` enables an explicit proxy (the SSRF deny-list is applied to the requested host before proxying). `http-call` ignores `HTTPS_PROXY`/`NO_PROXY` unless `network.egress.proxy` is set |
| F-118 | **Rate limiting and body limits**: `limits.api.{max_body_size, rate, burst}` and `limits.webhooks.{max_body_size, rate, burst}`. Defaults: API `1MiB` / `100/s` / `200`; webhooks `25MiB` / `50/s` / `100`. Rate grammar: `<count>/<s|m|h>`. Rate-limit key: per client IP for unauthenticated endpoints, per authenticated principal for authenticated endpoints |
| F-119 | **Extension integrity**: SHA-256 pinning for sidecar schemas and WASM modules. A content-addressed module store ensures a release pinned to hash X still finds X after an upgrade. Schema hashes record the interface, not the implementation; in-flight safety for sidecar steps is the sidecar author's responsibility through versioned step types. Sigstore signatures considered for v2 |
| F-120 | **Secrets out of DBOS state**: DBOS persists workflow inputs and step outputs in Postgres in plaintext. A token must never be a step return value or workflow argument. A test scans DBOS's step-output table for known secret values |
| F-121 | **Extension credentials (WASM v1.1)**: Credentials are host-owned, declared per-provider in config. The guest calls `http.send(request, auth: "cred-name")`. The host injects the auth header only if the target host is on the extension's allowlist. Sidecars (v1.0) hold their own credentials, declared in the `extensions:` section |
| F-122 | **GitHub installation per-repo**: Look up the installation for each repository. Mint installation tokens scoped to the single repository and minimal permissions |

### 4.17 Configuration

| ID  | Requirement |
|-----|-------------|
| F-123 | The server can be fully bootstrapped from environment variables for simple deployments. Trust policies and the application registry require a config file |
| F-124 | The server is started with a YAML config file via `--config` or `CARGOBIKE_SERVER_CONFIG` env var. Default path: `/etc/cargobike/config.yaml`. The CLI uses `CARGOBIKE_CONFIG` for its config file (`~/.config/cargobike/config.yaml`). Separate names prevent collision on machines running both. The server config uses top-level sections (`server`, `database`, `leader_election`, `auth`, `providers`, `extensions`, `secrets`, `templates`, `engine`, `reconciler`, `retention`, `limits`, `network`, `logging`, `metrics`, `applications`, `application_groups`) |
| F-125 | Every server config scalar value has three sources: CLI flag, `CARGOBIKE_SERVER_*` environment variable, and config file key. Resolution: flag > env > config file > default. A curated, documented list of env vars and flags is published (not an automatic `__` mapping), because lists such as applications cannot be expressed as env vars and an automatic mapping would turn every internal key name into a stable API. Lists from flags/env replace the config file list |
| F-126 | The server uses clap (with `derive` and `env` features) for argument parsing |
| F-127 | Sensitive values use `hide_env_values = true` in clap and `secrecy::SecretString` in the config struct |
| F-128 | Bind addresses use `0.0.0.0:8080` format (Rust `SocketAddr`) |
| F-129 | Templates directory default: `/etc/cargobike/templates`. Hot reload on SIGHUP is safe (snapshot pinning) and supported. SIGHUP also reloads the application registry and OIDC trust entries (safe for the same reason: snapshots include registry inputs, F-16). Every application or group must declare an explicit `template` |
| F-130 | The CLI uses the same clap + env pattern with `CARGOBIKE_*` env vars (e.g. `CARGOBIKE_CONFIG` for the CLI config file, `CARGOBIKE_URL` for the server URL). The CLI honours `NO_COLOR`, `XDG_CONFIG_HOME`, and `OTEL_*` standard environment variables |

### 4.18 High Availability

| ID  | Requirement |
|-----|-------------|
| F-131 | v1: single active replica, enforced via Postgres advisory-lock leader election at startup. A standby does not launch DBOS until it holds the lock and reports not-ready (503) meanwhile. On loss of the lock connection, the executor stops immediately (exiting the process is the simplest reliable option) to prevent two active executors. Session advisory locks do not work through PgBouncer in transaction-pooling mode; the leader-election connection must be direct or session-pooled. A standby can still serve webhook endpoints (verify, persist, enqueue via `dbos::Client`) without launching the executor, so rolling updates do not lose tag-push triggers |
| F-132 | Deployment guidance: document `strategy: Recreate` for Kubernetes, but note that a standby serving webhooks makes rolling updates safe without downtime. Tag-push triggers are no longer lost on each deploy |
| F-133 | Executor ID: verified in the spike that a replacement instance recovers the previous instance's pending workflows |

### 4.19 Additional Configuration Parameters

| ID | Requirement |
|-----|-------------|
| F-134 | **Server**: `server.public_url` (required for webhooks, links in CR bodies, `Location` headers, `clientconfig`), `server.shutdown_timeout` (default `30s`), optional `server.tls.{certificate, private_key}` (secret-shaped, R5) |
| F-135 | **Database**: `database.url` (secret-shaped: literal, `{ file }`, `{ env }`, or `{ secret }`; literal values are warned at startup), `database.max_connections` (default `10`) |
| F-136 | **Leader election**: `leader_election: { enabled: true, database_url: { file: ... } }` (one section; `database_url` defaults to `database.url`; must bypass PgBouncer transaction pooling, F-131) |
| F-137 | **Engine**: `engine.max_step_output` (default `1MiB`, F-40), `engine.cel.max_cost` and `engine.cel.max_expression_length` (F-27), `engine.branch_format` (default `cargobike/{application}/{environment}/{release_id}`) |
| F-138 | **Reconciler**: `reconciler.interval` (default `5m`), `reconciler.batch_size` (default `100`, F-69) |
| F-139 | **Retention**: `retention.events` (default `365d`), `retention.webhook_payloads` (default `30d`, F-115), `retention.releases` (optional, default unset = keep forever) |
| F-140 | **Logging**: `logging.level` (default `info`), `logging.format` (default `json`). Precedence: `RUST_LOG` (per-module filters) > `CARGOBIKE_SERVER_LOG_LEVEL` (single level) > `logging.level` > default. `CARGOBIKE_SERVER_LOG_FORMAT` overrides `logging.format` |
| F-141 | **Metrics**: `metrics.listen` (optional, separate port for Prometheus scraping keeps metrics off the public API) |
| F-142 | **Applications**: `applications[].description`, `applications[].labels` (map for filtering), `applications[].paused` (default `false`; new releases rejected with a clear error while paused). Per-environment pause: `environments.<env>.paused: { reason: "..." }` (optional; freezes one environment while others continue) |
| F-143 | **Environments**: `environments.<env>.change_request.{title,body,labels,draft}` (custom CR title, body, labels, draft flag; format-string placeholders: `{application}`, `{environment}`, `{version}`, `{release_id}`, `{release_url}`), `environments.<env>.commit_message` (default `"Release {application} {version} to {environment}"`), `environments.<env>.inputs` (generic per-environment inputs map, accessible as `${{ env.inputs.<name> }}`) |
| F-144 | **Step parameters**: `http-call.with.{url,method,headers,body,expect_status}` (headers referencing secrets use `{ secret: <name> }` named references, never inline values), `change-request.with.{base,draft,auto_merge,labels}` (`base` defaults to the repo's default branch). Registry `change_request.labels` are appended to template `change-request.with.labels` (R11) |
| F-145 | **CLI**: `--context` / `CARGOBIKE_CONTEXT` (select a server profile from the CLI config file), `--auth` / `CARGOBIKE_AUTH` (auth type override: `none`, `api-key`, `exec`, `github-actions`), `-o` / `CARGOBIKE_OUTPUT` (default output format), `--audience` / `CARGOBIKE_AUDIENCE` (override OIDC audience, default `cargobike`), `--ca-file` / `CARGOBIKE_CA_FILE` (custom CA bundle for GHES and private CAs). CLI config file (`~/.config/cargobike/config.yaml`) defines named contexts with `url`, optional `ca_file`, and `auth` (§9.7). Precedence: flag > env var > selected context > `defaults` > built-in default |
| F-146 | **Named secrets**: A top-level `secrets:` section maps names to `{ file: ... }` or `{ env: ... }` references. Template steps reference them as `{ secret: <name> }`. The engine resolves the value internally so it never enters a step output (F-120) |
| F-147 | **Format-string placeholders and escaping**: `{{` and `}}` produce literal braces. Unknown placeholders are validation errors, not passed through. Placeholders per field: `versioning.tag_format`: `{version}`; `engine.branch_format`: `{application}`, `{environment}`, `{release_id}`; `commit_message`, `change_request.{title,body}`: `{application}`, `{environment}`, `{version}`, `{release_id}`, `{release_url}` (requires `server.public_url`); `edits[].file`, `discovery.match`: `{application}`, `{environment}`; `edits[].value`: `{version}` |

---

## 5. Release Model

### 5.1 Metadata

```rust
pub struct ReleaseMetadata {
    pub id: Uuid,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub resource_version: u64,       // optimistic concurrency (If-Match)
    pub retried_from: Option<Uuid>,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,  // untrusted caller-supplied values
}

pub struct Attempt {
    pub workflow_id: String,
    pub started_at: OffsetDateTime,
    pub fork_from: Option<String>,    // "last_failure" or a step ID
}
```

### 5.2 Spec

```rust
pub struct ReleaseSpec {
    pub application: String,          // references application registry
    pub version: String,              // opaque; validated by versioning block
    pub source: RepoRef,             // resolved from registry, not caller-supplied
    pub template: String,            // resolved from registry, not caller-selectable
}

pub struct RepoRef {
    pub provider: String,
    pub id: String,                 // immutable provider ID (e.g. GitHub repo ID as string)
    pub path: Option<String>,       // optional verified label: if set, checked against provider at startup
                                    // otherwise fetched from provider at runtime via get_repo_path
}
```

### 5.3 Status

```rust
pub struct ReleaseStatus {
    pub phase: Phase,
    pub actor: Actor,
    pub ci: Option<CiContext>,       // generic CI context (not GitHub-specific)
    pub workflow: WorkflowInfo,
    pub error: Option<ReleaseError>,
    pub environments: Vec<EnvironmentStatus>,
    pub attempts: Vec<Attempt>,       // workflow IDs + fork points (status, not metadata)
}

pub enum Phase {
    Pending, Running, PendingApproval, Failed, Canceled, Superseded, Completed,
}

// Phase rollup: the release phase is derived from environment phases.
// Running: at least one environment is Running.
// PendingApproval: at least one is PendingApproval and none are Failed/Canceled.
// Failed/Canceled/Superseded: any environment is in that state.
// Completed: all environments are Completed or Skipped.
// Pending: initial state before the first environment starts.

pub struct Actor {
    pub issuer: String,
    pub subject: String,
    pub display_name: String,
}

pub struct CiContext {
    pub provider: String,
    pub repository: String,
    pub repository_id: String,
    pub workflow_ref: String,
    pub run_id: String,
    pub run_url: String,
}

pub struct WorkflowInfo {
    pub id: String,                  // "cargobike.interpret.v1"
    pub template_name: String,
    pub template_version: String,
    pub template_hash: String,
    pub step_type_versions: BTreeMap<String, String>,  // "builtin/commit-files" -> "1"
}

pub enum EnvironmentPhase {
    Pending, Running, PendingApproval, Skipped, Waiting, Completed, Failed,
    Canceled, Superseded,
}

pub struct EnvironmentStatus {
    pub name: String,
    pub phase: EnvironmentPhase,
    pub change_request: Option<ChangeRequestRef>,
    pub started_at: Option<OffsetDateTime>,
    pub completed_at: Option<OffsetDateTime>,
}

pub struct ChangeRequestRef {
    pub number: u64,
    pub url: String,
    pub target_repo: RepoRef,        // for verification + webhook correlation
    pub head_sha: String,            // what Cargobike committed
    pub state: CrState,              // open, closed, merged
}

pub enum CrState { Open, Closed, Merged }

pub struct ReleaseError {
    pub code: String,                // StepFailed, ApprovalTimeout,
                                      // ApprovalRejected, VersionNotVerified,
                                      // ConcurrencyRejected, ChangeRequestModified
    pub message: String,
}
```

---

## 6. Workflow Architecture

### 6.1 DBOS Transact (Rust)

Cargobike uses [DBOS Transact for Rust](https://github.com/dbos-inc/dbos-transact-rust)
(the [`dbos` crate](https://docs.rs/dbos/latest/dbos/), v0.5.0+, MIT).
DBOS checkpoints every step to PostgreSQL.

The real API surface:

| API | Description |
|-----|-------------|
| `dbos::step` / `dbos::step_with` | Execute a function as a durable, retried step. `StepOptions` control retries and backoff |
| `dbos::recv` | Take the oldest message sent to the workflow, waiting up to a timeout |
| `dbos::send` / `dbos::send_with` | Send a message to a workflow (topic, idempotency key, fork fan-out) |
| `dbos::sleep` | Durable sleep; resumes at the original wake time after a crash |
| `DBOS::cancel` | Cancel (pause) a running workflow. Recorded steps are intact |
| `DBOS::fork` | Fork a workflow. `ForkFrom::LastFailure` retries from the failing step. Pass `ForkOptions::app_version` for upgrade-related retries |
| `DBOS::delete` | Permanently remove a workflow |
| Registration | Workflows can only be registered before `launch()`. The executor snapshots the registry |

Step execution is **at-least-once**. Every step must be idempotent.

### 6.2 Three-Tier Workflow Model

**Tier 1 (v1.0): YAML workflows, interpreted.**

One DBOS workflow is registered (`cargobike.interpret.v1`). The interpreter
reads the snapshotted template and executes a sequence of DBOS operations.
The sequence of DBOS operations is a pure function of the snapshot and the
recorded outputs.

**Tier 2: custom step types as sidecars (v1.0) or WASM (v1.1).**

`uses: acme/notify@1` resolves to a sidecar-implemented step type.

**Tier 3: "code" via embedding (v1.1).**

The server is published as a library.

### 6.3 Pipeline Template (YAML)

```yaml
name: service
version: "1"
description: |
  Standard two-environment release: preview then production.
  Production requires human approval.
inputs: {}                          # application-wide inputs (none needed here)
environment_inputs:                 # per-environment, supplied by the registry
  repo:
    type: repo
    description: Target deployment repository
  edits:
    type: edits
    description: List of file edits {file, format, field, value?}

step_groups:
  - name: deploy
    steps:
      - id: edit
        uses: builtin/commit-files@1
        with:
          repo: ${{ env.inputs.repo }}
          edits: ${{ env.inputs.edits }}
        retry: { attempts: 3, backoff: exponential, max_delay: 1m }
      - id: cr
        uses: builtin/change-request@1
        with:
          branch: ${{ steps.edit.outputs.branch }}
      - wait: merge
        timeout: 7d
        on_modified: fail
        on_timeout: fail

environments:
  - name: preview
    steps:
      - include: deploy

  - name: production
    when: 'semver(release.version).prerelease() == ""'
    concurrency: supersede
    steps:
      - include: deploy
      - wait: approval
        timeout: 72h
        on_timeout: cancel
```

Template expressions use `${{ ... }}` (CEL). The expression context
provides `release.{id, application, version}`, `env.{name, inputs}`,
`inputs`, and `steps.<id>.outputs`. `release.version` is a string; use
`semver(release.version)` to access structured fields like `prerelease()`.
Server config interpolation uses `${VAR}`. The two never overlap because
templates are loaded by a separate parser that does not expand `${VAR}`.
Dot-notation paths (`image.tag`) are used for structured edits, not
dotted keys.

### 6.4 Two Kinds of Steps

| Category | Runs in | Extensible? | Examples |
|----------|---------|-------------|----------|
| Control steps | Workflow body (interpreter-native) | No | `wait: merge`, `wait: approval`, `wait: sleep` |
| Action steps | `dbos::step` | Yes (built-in or sidecar/WASM) | `commit-files`, `change-request`, `http-call`, `set-labels` |

Control steps are part of the interpreter, not registered step types.
They use a distinct `wait:` keyword in templates (e.g. `wait: merge`,
`wait: approval`) so readers can tell them apart from action steps, and
so a sidecar can never register a step named like a control step.
`wait: merge` is a `recv` plus a verification step, and it loops if
verification fails.

```rust
#[async_trait]
pub trait StepType: Send + Sync {
    fn name(&self) -> &str;
    fn version(&self) -> &str;  // "1"
    async fn execute(
        &self,
        ctx: &StepContext,
        release: &Release,
        env: &EnvironmentSpec,
        params: &serde_json::Value,
    ) -> Result<StepOutput, StepError>;
}

pub struct StepContext {
    pub providers: ProviderRegistry,    // resolve by RepoRef.provider name
    pub http_client: ReqwestClient,     // SSRF-guarded
    pub credentials: ProviderCredentials, // per-provider secrets
    pub idempotency_key: String,        // release_id / environment / step_id
    pub cancel_token: CancellationToken,
    pub log: tracing::Span,
}

pub enum StepOutput {
    Continue(serde_json::Value),  // payload for later steps
    SkipEnvironment,              // only valid before any side effect; marks env Skipped
    Stop(String),                  // fail the environment with the given reason
}
```

### 6.5 Implicit Bookkeeping

`set-status` and `close-superseded` are interpreter behaviour, not
user-visible steps. A template cannot omit them. The interpreter
automatically updates the release status and handles supersession between
environments.

### 6.6 Template Loading

At server startup:
1. Scan the `templates/` directory for `*.yaml` files.
2. Parse each file into a `PipelineTemplate` struct.
3. Validate typed inputs against the application registry.
4. The interpreter workflow is registered once.
5. Hot reload on SIGHUP is safe (snapshot pinning).

### 6.7 Step Isolation Rules

- Each step creates its own provider client internally. The client never
  escapes step scope.
- The parent workflow stores only config, never credentials.
- Step options (max retries, backoff) via `StepOptions`.
- Extensions are never called from workflow bodies, only inside steps.
- Output size is limited (default `1MiB`, configurable via
  `engine.max_step_output`).
- The sidecar client uses a separate HTTP client with a fixed endpoint,
  not the SSRF-guarded client. This guarantees `http-call` can never reach
  the sidecar.

### 6.8 In-flight Safety

**Breaking** for running workflows:
- Adding, removing, or reordering DBOS operations in the interpreter
- Changing an interpreter step's return type
- Changing a built-in step type's output schema (versioned `@1` -> `@2`)
- Renaming the interpreter workflow registration

**Safe** for running workflows:
- Modifying logic within an existing interpreter step (same position, same
  return type)
- Adding or modifying pipeline templates (snapshotted at release creation)
- Adding new step types
- Changing CLI output
- Hot-reloading templates (SIGHUP)

### A1: Idempotent Steps

| Step | Idempotency strategy |
|------|---------------------|
| `create_branch` | Deterministic name: `cargobike/{application}/{environment}/{release_id}` (configurable via `engine.branch_format`). "Already exists at expected SHA" = success |
| `change-request` | Look up existing open CR by head branch before creating |
| `commit-files` | Pass expected parent SHA; no-op if tree already matches. Fused edit+commit removes intermediate state |
| `set-status` | Conditional `UPDATE ... WHERE phase = <expected>` |
| `http-call` | Caller responsibility; document idempotency requirements |
| `tag` | No-op if tag already exists at the same SHA |
| `lease` | Conditional INSERT `(app, env) -> release_id`; no-op if already held by this release. Released per-environment on completion/skip/supersede, or by `cleanup.v1` on cancel |

### A2: Serialisation Across Multi-Day Waits

The environment is modelled as a **lease row**
`(application, environment) -> holder_release_id`, acquired in a step with
a conditional INSERT. Each environment's lease is released when *that
environment* completes, is skipped, or is superseded.

- **`supersede`**: new release acquires the lease atomically from the old
  holder (conditional `UPDATE … WHERE holder = <old release>`), cancels
  the old release. A lower version arriving later fails with
  `ConcurrencyRejected`.
- **`queue`**: waiting releases block on `recv` (topic
  `lease/{environment}`); the previous holder (or the reconciler) sends a
  release signal.
- **`reject`**: returns `ConcurrencyRejected` immediately.
- **Cancel/supersede cleanup**: `cargobike.cleanup.v1` releases or
  transfers the lease, since the cancelled workflow cannot run anything.

Ordering falls back to creation time when the version scheme is `opaque`.

---

## 7. Provider Abstraction

### 7.1 Provider Trait

Async, decoupled from DBOS, includes webhooks in the contract:

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn capabilities(&self) -> ProviderCaps;

    async fn get_repo_path(&self, repo: &RepoRef) -> Result<String>;
    async fn create_branch(&self, repo: &RepoRef, branch: &str, from_sha: &str) -> Result<()>;
    async fn commit_files(&self, repo: &RepoRef, branch: &str, edits: &[Edit], message: &str, expected_parent: Option<&str>) -> Result<CommitResult>;
    async fn create_change_request(&self, repo: &RepoRef, head: &str, base: &str, title: &str, body: &str, labels: &[String]) -> Result<ChangeRequest>;
    async fn get_change_request(&self, repo: &RepoRef, number: u64) -> Result<ChangeRequest>;
    async fn find_change_request_by_head(&self, repo: &RepoRef, head: &str) -> Result<Option<ChangeRequest>>;
    async fn close_change_request(&self, repo: &RepoRef, number: u64, comment: &str) -> Result<()>;
    async fn list_open_change_requests(&self, repo: &RepoRef) -> Result<Vec<ChangeRequest>>;
    async fn read_file(&self, repo: &RepoRef, path: &str, ref_: &str) -> Result<Vec<u8>>;
    async fn comment(&self, repo: &RepoRef, cr_number: u64, body: &str) -> Result<()>;
    async fn add_labels(&self, repo: &RepoRef, cr_number: u64, labels: &[String]) -> Result<()>;
    async fn update_branch(&self, repo: &RepoRef, branch: &str) -> Result<()>;
    async fn default_branch(&self, repo: &RepoRef) -> Result<String>;
    async fn branch_sha(&self, repo: &RepoRef, branch: &str) -> Result<String>;
    async fn set_commit_status(&self, repo: &RepoRef, sha: &str, status: CommitStatus) -> Result<()>;
    async fn check_branch_protection(&self, repo: &RepoRef, branch: &str) -> Result<BranchProtection>;
    async fn check_tag_protection(&self, repo: &RepoRef, tag_pattern: &str) -> Result<bool>;

    // Optional (checked via capabilities()):
    // async fn enable_auto_merge(...)
    // async fn create_tag(...)
    // async fn create_release(...)
    // async fn verify_webhook(headers, body, secret) -> Result<NormalisedEvent>
    // async fn parse_webhook(headers, body) -> Result<NormalisedEvent>
}
```

### 7.2 Provider Configuration

Providers are configured as a list, not GitHub-specific server args.
Sidecar extensions are in a separate `extensions:` section:

```yaml
providers:
  - name: github
    type: github
    api_url: https://api.github.com   # or GitHub Enterprise Server URL
    web_url: https://github.com        # optional; for web links in CR comments
    auth:
      app_id: "123"
      private_key: { file: /path/to/key.pem }
    webhook_secrets:
      - { file: /run/secrets/github-webhook-secret }
    repositories:
      allow: ["my-org/*"]
      deny: ["my-org/*-test"]
  - name: gitlab-internal
    type: extension
    extension: gitlab-internal          # served by this extension
    webhook_secrets:
      - { file: /run/secrets/gitlab-webhook-secret }
    repositories:
      allow: ["gitlab-org/**"]

extensions:
  - name: gitlab-internal
    endpoint: unix:///run/cargobike/gitlab.sock
    auth:
      shared_secret: { file: /etc/cargobike/keys/sidecar-secret }
    provides:
      step_types: ["acme/notify@1"]
```

A convenience env-var shortcut (`CARGOBIKE_SERVER_GITHUB_APP_ID`, etc.)
exists for single-provider deployments.

### 7.3 Sidecar Extensions (v1.0)

An out-of-process "remote extension" over HTTP or gRPC, using the same
WIT-derived schema. Suits teams that prefer to run a private provider as a
Go or Python sidecar. The sidecar holds its own credentials.

**Trust boundaries**:

- **Authenticate the channel.** `endpoint: http://localhost:8081` is
  plaintext and unauthenticated. Prefer a Unix domain socket
  (`unix:///run/cargobike/gitlab.sock`) or mTLS
  (`auth: { tls: { ca_file, cert_file, key_file } }`), or at minimum an
  `auth: { shared_secret: { file } }` header in both directions. Any local
  process could otherwise call the sidecar (which holds write
  credentials) or impersonate it to the server.
- **A sidecar provider is inside the approval boundary.** Merge
  verification (F-62) trusts what the provider reports, so a compromised or
  buggy sidecar can approve a release by reporting "merged". This is noted
  in the threat model (§21.5).
- **Schema hashes don't pin behaviour.** F-119 records the SHA-256 of the
  sidecar's declared schema, but a sidecar's implementation can change
  behind an unchanged schema. In-flight safety for sidecar steps is the
  sidecar author's responsibility, through versioned step types
  (`acme/notify@1`, `@2`) kept side by side.
- **Keep the sidecar client separate.** The SSRF guard blocks loopback, so
  the sidecar client needs its own HTTP client with a fixed endpoint. This
  also guarantees `http-call` can never reach the sidecar.
- **The secrets invariant can't see sidecar secrets.** The DBOS step-output
  scan (F-120) only knows server-held secrets. Document that sidecar step
  outputs must not contain credentials.

### 7.4 WASM Extensions (v1.1)

- Rust crates compiled to `wasm32-wasip2`.
- Custom host `http` interface (not raw `wasi:http`).
- No filesystem, no environment variables by default.
- Epoch interruption or fuel, memory limiter, per-call timeout.
- Synchronous guest interface; host drives async underneath.
- Precompile and cache modules (keyed by content hash).
- SHA-256 pinning in config; content-addressed module store.
- Host-owned credentials declared per-provider.

---

## 8. Webhook System

### 8.1 Fast-Ack Processing

1. Verify signature before parsing payload. Compare secrets in constant
   time. Limit body size. Check installation + repository on allowlist.
2. Persist the raw event.
3. Start a DBOS workflow whose ID is `{provider}-delivery-{delivery_id}`.
4. Return 202 within milliseconds.

### 8.2 Event Mapping

Trigger rules live in the application registry, not a separate webhook
config. A `pull_request.closed` webhook correlates automatically via the
stored `(provider, repo_id, cr_number) -> (release_id, environment)`
mapping. No configuration rules needed for merge correlation.

For create triggers, trigger on tag push (`refs/tags/v*`), not branch
push. The version is extracted from the tag name (e.g. `v1.2.3` -> `1.2.3`).

### 8.3 Webhook Secrets

Declared per-provider in the provider config (`webhook_secrets` list of
`{ file: ... }` references), not in a top-level section. The list allows
two secrets for rotation, consistent with F-58 and F-51.

### 8.4 Reconciliation

Every N minutes (configurable, default 5), the server checks the actual
CR state for each release in `PendingApproval`. The reconciler only
`send`s signals to the workflow; it never writes release status directly.
The workflow remains the only writer of its release's status. The
reconciler batches CR lookups (GraphQL) and uses ETags.

---

## 9. Authentication & Authorization

### 9.1 OIDC Validation

1. Parse issuer. Exact match against allowlist.
2. Fetch OIDC discovery + JWKS (cached; refetch on unknown `kid`, rate-limited).
3. Verify signature. Reject `none` and `HS*`.
4. Validate expiration, `nbf`/`iat` with clock-skew allowance.
5. Validate `aud` (audience is required).
6. Extract actor identity and authorization claims.

### 9.2 OIDC Trust Entries

```yaml
auth:
  oidc:
    - name: gha-my-org
      issuer: https://token.actions.githubusercontent.com
      audience: cargobike
      claims:
        repository_owner_id: "123456"     # immutable ID, not name (quoted string, R6)
        ref: refs/tags/v*                    # glob, not exact, for tag pushes
      grants: [release:create, release:read]
    - name: azure-ad
      issuer: https://login.microsoftonline.com/00000000-0000-0000-0000-000000000000/v2.0
      audience: api://cargobike            # App ID URI, not a free-form string
      claims:
        roles: [release-managers]           # app roles, not group display names
                                          # (Entra ID emits group object IDs,
                                          #  and omits them for users in many groups)
      grants: [release:read, release:approve, release:cancel]
      # allow_unconstrained: true            # rejected by default
      # clock_skew: 60s                      # default 60s (F-77)
      # jwks_url: https://...                 # override discovery URL
      # algorithms: [RS256, ES256]            # narrower allowlist
```

OIDC trust entries carry `grants` only; applications reference them
via `releasers` and `approval.approvers`. There is no `applications`
list on the trust entry &mdash; the binding is single-direction to avoid
undefined behaviour when two lists disagree.

Matching rules: all listed claims must match (AND). Claim values are glob
by default (R7). Array-valued claims match if any element matches.

### 9.3 API Keys

```yaml
auth:
  api_keys:
    - name: ci-fallback
      description: Fallback key for the deploy pipeline
      hash: "$argon2id$..."
      grants: [release:create]
      expires: 2027-06-30
    - name: admin
      description: Full access
      hash: "$argon2id$..."
      grants: ["*"]
```

Multiple named keys, stored hashed, with grants, optional `expires`, and
`description`. Two keys active at once for rotation. Compared in constant
time. API key grants *are* the authorization (not "bypassed"). API keys
carry `grants` only (no `applications` list); applications reference them
via `releasers` with `api_key: <name>`, the same single-direction binding
as OIDC entries (F-85, F-99b). The grant vocabulary is the same
as OIDC trust entry grants (F-99).

A bootstrap admin key via `CARGOBIKE_SERVER_BOOTSTRAP_API_KEY` (plaintext)
or `CARGOBIKE_SERVER_BOOTSTRAP_API_KEY_FILE` is hashed at startup, logged
as a warning, and must be replaced by adding a hashed key to the config
file and removing the variable. The bootstrap key is restricted to
localhost requests only. To produce a hash for the config file, run
`cargobike-server hash-api-key` and paste the key on stdin.

### 9.4 Client-side Authentication

| Type | Description |
|------|-------------|
| `none` | No authentication (localhost development only) |
| `api-key` | Sends an API key from `CARGOBIKE_API_KEY`, `CARGOBIKE_API_KEY_FILE`, or the CLI config |
| `exec` | Runs a command from local config to obtain a token (never from auto-discovery) |
| `github-actions` | Exchanges the GitHub Actions runtime token for an OIDC ID token with audience `cargobike` (override with `--audience`) |

Auto-discovery from `/api/v1/clientconfig` supplies only issuer and
audience hints. The exec command comes from local config or flags only.
The CLI refuses `http://` server URLs except for localhost (`--allow-http`
override). A custom CA bundle can be provided via `--ca-file` /
`CARGOBIKE_CA_FILE` for GitHub Enterprise Server and internal deployments.

### 9.5 Human Authentication on Laptops

With NG-7 excluding device flow, laptop users authenticate via:
- API key (configured in `~/.config/cargobike/config.yaml`, type `api-key`)
- `exec` plugin against an OIDC identity provider. For Entra ID, use:
  `az account get-access-token --scope api://cargobike/.default`
  (without `--scope`, `az` returns a token for Azure Resource Manager, which
  will fail audience validation). Register an Entra ID app with App ID URI
  `api://cargobike`, configure app roles (not groups &mdash; Entra ID emits
  group object IDs, not display names, and omits the `groups` claim entirely
  for users in many groups, the "overage" limitation). Use the v2.0 endpoint
  (`login.microsoftonline.com/{tenant}/v2.0`) by setting the app
  registration's token version to 2.0.

GitHub PATs are not OIDC tokens and there is no OIDC trust entry path for them.
The CLI-side PAT option is removed from the docs.

### 9.6 Authorization Summary

| Token type | Authenticated | Authorized | Notes |
|------------|--------------|------------|-------|
| GitHub Actions OIDC (matching OIDC trust entry) | Yes | Yes (per grants + application `releasers`) | `repository_id` must match app source |
| Non-GitHub OIDC (matching OIDC trust entry) | Yes | Yes (per grants + application `releasers`) | |
| API key (valid, referenced in `releasers`) | Yes | Yes (per grants + application `releasers`) | |
| API key (expired/invalid) | No | N/A | 401 |
| No token | No | N/A | 401 |

### 9.7 CLI Config File

The CLI config file (`~/.config/cargobike/config.yaml`, override with
`CARGOBIKE_CONFIG`) defines named contexts for talking to different
Cargobike servers:

```yaml
current_context: production
contexts:
  - name: production
    url: https://cargobike.example.com
    ca_file: /etc/ssl/corp-ca.pem          # optional
    auth:
      type: exec
      command: az
      args: [account, get-access-token, --scope, "api://cargobike/.default",
             --query, accessToken, -o, tsv]
  - name: local
    url: http://localhost:8080              # http allowed for localhost
    auth:
      type: api-key
      api_key: { file: ~/.config/cargobike/local-key }
defaults:
  output: table
```

Precedence: flag > environment variable > selected context > `defaults` >
built-in default. In GitHub Actions no file is needed: `CARGOBIKE_URL`
plus auth type `github-actions` (detected automatically when the Actions
OIDC variables are present).

`cargobike context list` lists contexts; `cargobike context use <name>`
switches the current context.

---

## 10. Error Handling

### 10.1 Library Errors

`thiserror` for typed errors. Error messages: `failed to <action>: <cause>`.

### 10.2 HTTP Errors (RFC 9457)

```json
{
  "type": "https://cargobike.dev/errors/field-required",
  "title": "Bad Request",
  "status": 400,
  "detail": "The field `application` is required.",
  "instance": "/api/v1/releases",
  "code": "FieldRequired"
}
```

| Code | Status | Description |
|------|--------|-------------|
| `InvalidRequest` | 400 | Malformed or invalid request |
| `FieldRequired` | 400 | A required field is missing |
| `FieldInvalid` | 400 | A field has an invalid value |
| `MissingToken` | 401 | No auth token provided |
| `InvalidToken` | 401 | Token validation failed |
| `TokenExpired` | 401 | Token has expired |
| `ForbiddenResource` | 403 | Permission denied |
| `ApplicationNotFound` | 404 | Application not in registry |
| `ReleaseNotFound` | 404 | Release ID not found |
| `TemplateNotFound` | 404 | Pipeline template not found |
| `StepTypeNotFound` | 400 | Step type not in registry |
| `StateConflict` | 409 | Request conflicts with current state |
| `MergeNotVerified` | 409 | CR was not merged |
| `ConcurrencyRejected` | 409 | Concurrency policy is `reject` |
| `DuplicateRelease` | 409 | Release for this app+version is already active (only for `retry --new` on a non-terminal release) |
| `ApprovalForbidden` | 403 | Caller does not satisfy approvers policy |
| `VersionNotVerified` | 400 | Version does not match tag/SHA |
| `ChangeRequestModified` | 409 | CR head SHA changed and `on_modified: fail` |
| `InternalError` | 500 | Unexpected server error |

---

## 11. Crate Architecture

### 11.1 Workspace Layout

```
cargobike/
├── Cargo.toml
├── Makefile
├── rustfmt.toml
├── templates/
│   └── service.yaml
├── crates/
│   ├── cargobike-core/             # model, errors, traits (no I/O, compiles to wasm32-wasip2)
│   ├── cargobike-api/              # DTOs, OpenAPI, typed client
│   ├── cargobike-engine/           # DBOS interpreter, step runtime (depends on core only)
│   ├── cargobike-server/           # axum, auth, webhooks, extension host (lib + thin bin)
│   ├── cargobike-cli/              # CLI client (no DB deps)
│   ├── cargobike-extension-sdk/   # WIT + guest bindings (depends on core)
│   └── cargobike-provider-github/ # GitHub provider (depends on core, not extension-sdk)
```

### 11.2 Dependency Graph

```
cargobike-core
    ^
    |
    +--- cargobike-api (core)
    +--- cargobike-engine (core)
    +--- cargobike-extension-sdk (core; for WASM guest authors)
    +--- cargobike-provider-github (core; native provider, not extension-sdk)
    +--- cargobike-server (core, api, engine, provider-github; optional extension-sdk via feature)
    +--- cargobike-cli (core, api)
```

- `cargobike-engine` depends on `core` only, not `api`.
- `cargobike-provider-github` depends on `core`, not `extension-sdk` (it is a
  native provider, not a WASM guest).
- `cargobike-core` must compile for `wasm32-wasip2` (CI job checks this).
- The wasmtime host and sidecar client live in `cargobike-server` behind a
  `wasm` cargo feature.

### 11.3 Lints (enforced at Cargo level)

```toml
[workspace.lints.clippy]
unwrap_used = "deny"
expect_used = "deny"
dbg_macro = "deny"
print_stdout = "deny"
print_stderr = "deny"

[workspace.lints.rust]
dead_code = "deny"

[workspace.lints.rustdoc]
broken_intra_doc_links = "deny"
```

---

## 12. API Specification

### 12.1 Endpoints

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| `GET` | `/api/v1/clientconfig` | none | Issuer + audience hints |
| `GET` | `/api/v1/live` | none | Liveness |
| `GET` | `/api/v1/ready` | none | Readiness (503 if standby) |
| `GET` | `/api/v1/startup` | none | Startup |
| `GET` | `/api/v1/releases` | OIDC/API key | List (filters, pagination) |
| `POST` | `/api/v1/releases` | OIDC/API key | Create (202, `Location`, idempotent) |
| `GET` | `/api/v1/releases/{id}` | OIDC/API key | Get release |
| `DELETE` | `/api/v1/releases/{id}` | OIDC/API key (`release:delete`) | Delete terminal (204) |
| `POST` | `/api/v1/releases/{id}/cancel` | OIDC/API key | Cancel (204) |
| `POST` | `/api/v1/releases/{id}/retry` | OIDC/API key | Retry (fork or new, 202) |
| `POST` | `/api/v1/releases/{id}/approvals` | OIDC/API key | Submit approval |
| `GET` | `/api/v1/releases/{id}/watch` | OIDC/API key | SSE stream (single release) |
| `GET` | `/api/v1/releases/{id}/events` | OIDC/API key | Event log |
| `GET` | `/api/v1/releases/{id}/snapshot` | OIDC/API key | Resolved template + inputs |
| `GET` | `/api/v1/applications` | OIDC/API key | List applications |
| `GET` | `/api/v1/templates` | OIDC/API key | List templates |
| `GET` | `/api/v1/whoami` | OIDC/API key | Resolved identity, matched trust entry, grants |
| `POST` | `/webhooks/{provider}` | webhook secret | Receive webhook (202) |

### 12.2 Create Release

```
POST /api/v1/releases
Authorization: Bearer <token>
Idempotency-Key: <optional>

{ "application": "my-service", "version": "1.2.3" }
```

Response: `202 Accepted` with `Location: /api/v1/releases/{id}`.

---

## 13. Configuration

### 13.1 Server Config (YAML)

```yaml
server:
  listen: "0.0.0.0:8080"
  public_url: https://cargobike.example.com
  shutdown_timeout: 30s

database:
  url: { file: /run/secrets/cargobike-db-url }
  max_connections: 10

leader_election:
  enabled: true
  database_url: { file: /run/secrets/cargobike-db-direct-url }

auth:
  oidc:
    - name: gha-my-org
      issuer: https://token.actions.githubusercontent.com
      audience: cargobike
      claims:
        repository_owner_id: "123456"
        ref: "refs/tags/v*"
      grants: [release:create, release:read]
    - name: azure-ad
      issuer: https://login.microsoftonline.com/00000000-0000-0000-0000-000000000000/v2.0
      audience: api://cargobike
      claims:
        roles: [release-managers]
      grants: [release:read, release:approve, release:cancel]
  api_keys:
    - name: ci-fallback
      description: Fallback key for the deploy pipeline
      hash: "$argon2id$..."
      grants: [release:create]
      expires: 2027-06-30

secrets:
  slack-webhook-token: { file: /run/secrets/slack-token }

providers:
  - name: github
    type: github
    api_url: https://api.github.com
    web_url: https://github.com
    auth:
      app_id: "123"
      private_key: { file: /run/secrets/github-app-key.pem }
    webhook_secrets:
      - { file: /run/secrets/github-webhook-secret }
    repositories:
      allow: ["my-org/*"]
  - name: gitlab-internal
    type: extension
    extension: gitlab-internal
    webhook_secrets:
      - { file: /run/secrets/gitlab-webhook-secret }
    repositories:
      allow: ["gitlab-org/**"]

extensions:
  - name: gitlab-internal
    endpoint: unix:///run/cargobike/gitlab.sock
    auth:
      shared_secret: { file: /run/secrets/gitlab-sidecar-secret }
    provides:
      step_types: ["acme/notify@1"]

templates:
  directory: /etc/cargobike/templates

engine:
  max_step_output: 1MiB
  branch_format: "cargobike/{application}/{environment}/{release_id}"

reconciler:
  interval: 5m
  batch_size: 100

retention:
  events: 365d
  webhook_payloads: 30d

limits:
  api:      { max_body_size: 1MiB,  rate: 100/s, burst: 200 }
  webhooks: { max_body_size: 25MiB, rate: 50/s,  burst: 100 }

logging:
  level: info
  format: json

applications:
  - name: my-service
    description: Customer-facing API
    labels: { team: payments }
    source: { provider: github, id: "123456", path: my-org/my-service }
    template: service@1
    versioning:
      scheme: semver
      tag_format: "v{version}"
      require_tag: true
    triggers:
      - event: tag
        require_tag_protection: true
    releasers:
      - oidc: gha-my-org
      - api_key: ci-fallback
    inputs:
      notify_channel: "#payments-releases"
    environments:
      preview:
        repo: { provider: github, id: "789012", path: my-org/deploy }
        edits:
          - file: apps/my-service/preview.yaml
            field: image.tag
      production:
        repo: { provider: github, id: "789012", path: my-org/deploy }
        edits:
          - file: apps/my-service/prod.yaml
            field: image.tag
        concurrency: supersede
        require_branch_protection: true
        allow_direct_commit: false
        change_request:
          labels: [release, production]
        approval:
          required: 1
          allow_self_approval: false
          approvers:
            - oidc: azure-ad
              claims: { roles: [release-managers] }
```

### 13.2 Env-Only Bootstrap

```bash
CARGOBIKE_SERVER_DATABASE_URL=postgres://localhost:5432/cargobike \
CARGOBIKE_SERVER_GITHUB_APP_ID=123 \
CARGOBIKE_SERVER_GITHUB_PRIVATE_KEY_FILE=/etc/cargobike/key.pem \
CARGOBIKE_SERVER_BOOTSTRAP_API_KEY_FILE=/etc/cargobike/bootstrap-key \
cargobike-server
```

Note: OIDC trust entries and the application registry require a config file.
Env-only bootstrap is a development mode that binds to localhost and
cannot create releases (no application registry means no application can
be released, F-3). It is useful for testing the server's HTTP surface and
database connectivity.

---

## 14. Test Strategy

### 14.1 Unit Tests

| Module | What to test |
|--------|-------------|
| `core::model` | Default population, validation; `Release` root-level keys are only `metadata`/`spec`/`status` (F-1); UUIDv7 generation and ordering (F-2); `Phase` and `EnvironmentPhase` enums including `Canceled`/`Superseded` (F-6, F-7); `ReleaseError` codes: `StepFailed`, `ApprovalTimeout`, `ApprovalRejected`, `VersionNotVerified`, `ConcurrencyRejected`, `ChangeRequestModified` (F-8); `RepoRef` optional `path` as verified label (F-10) |
| `core::config` | Config loading, `${VAR}` fail-fast, `${VAR:-default}` (F-89); `CARGOBIKE_SERVER_CONFIG` vs `CARGOBIKE_CONFIG` separation (F-124); flag > env > file > default precedence; lists from env replace config (F-125); `SecretString` for all secrets, hand-written `Debug` (F-87, F-90, F-127); `*_FILE` variants load from files (F-88); placeholder issuer substitution (F-81); secret shapes: `{ file }`, `{ env }`, `{ secret }`, literal (F-146) |
| `core::version` | Version scheme validation per scheme (semver, calver, opaque) (F-4); version ordering for supersession (F-72) |
| `engine::template` | YAML parsing, CEL evaluation, path confinement, `${{ }}` vs `${}` separation (F-32); gate field validation against version scheme (F-27a); input scoping: `inputs` app-wide vs `environment_inputs` per-env, validation requires every env to supply per-env inputs (F-32a); `step_groups` resolve to same step sequence (F-32b); `StepOutput` size limit (1MiB default) (F-40); auto-generated step IDs for wait steps, uniqueness after `include` expansion (F-34); `include` keyword for step group references (F-32b) |
| `engine::interpreter` | Step sequence, gate evaluation, snapshot immutability (F-15, F-16, F-29); implicit `set-status` and `close-superseded` (F-38); step-type versions in snapshot (F-37); app-version recovery filter (F-22) |
| `engine::steps` | `commit-files` structured edits, dot-notation paths, path confinement (F-41); `StepContext` fields populated: provider registry, credentials, SSRF client, idempotency key, cancel token, logger (F-39); control steps use `wait:` keyword, sidecar cannot register `wait: merge` (F-33, F-34) |
| `server::release::service` | Create (idempotent, `{application, version}` only, template registry-only) (F-3, F-5, F-109); retry (fork, `--new` only if terminal) (F-23); delete (terminal only, `release:delete` grant) |
| `server::auth` | OIDC trust entry matching (glob, AND, array) (F-79); audience validation (F-77); algorithm allowlist (reject `none`, `HS*`) (F-78); API key constant-time, grants (F-85); unconstrained OIDC entries rejected without `allow_unconstrained` (F-80); actor model `CiContext` fields (F-83); verified values from token claims, not body (F-84); bootstrap key localhost-only (F-86); releaser-grant validation (F-99a); `api_key` selector rules (F-99b) |
| `server::webhooks` | Signature verification (constant time), body size, fast-ack (F-51, F-52); duplicate deliveries no-op via workflow ID dedup (F-53); unrecognized events 200 (F-56); webhook secrets from `webhook_secrets` list (F-58) |
| `server::config` | SIGHUP reloads templates, registry, OIDC trust entries (F-129); env-only bootstrap is dev/localhost mode (F-123); top-level sections match F-124 list; `secrets:` section resolves named references (F-146) |
| `server::registry` | Application registry loading and validation (F-91); repository allow/deny lists use globs (F-92); application group discovery, `{application}` placeholder, per-app override precedence + logging (§4.13a); per-env `match` replaces global `match`; periodic rescan interval (§4.13a); discovered name validation (§4.13a); `extends` merge rules: maps merge deeply, lists replace, `source` replaces as whole (§4.13a); name collision detection (§4.13a) |
| `server::reconciler` | Batched CR lookups with ETags (F-69); sends signals only, never writes release status (F-67); resumes at correct wake time after restart (F-68) |
| `server::security` | SSRF deny-lists (link-local, loopback, RFC 1918, IPv6 loopback, ULA) (F-117); DNS rebinding defence; redirect re-check (F-117); rate limiting 429 (F-118); extension schema SHA-256 pinning (F-119); GitHub installation per-repo, scoped tokens (F-122); named secrets never in step outputs (F-146) |

### 14.2 Integration Tests

**Full release lifecycle (E2E, chained)**:

- Create release &rarr; assert 202 + `Location` (F-110) &rarr; list &rarr; assert new release appears (F-101) &rarr; get &rarr; assert all metadata/spec/status fields (F-7, F-9) &rarr; watch via SSE &rarr; assert status updates stream + `Last-Event-ID` reconnect (F-102) &rarr; cancel &rarr; assert 204 + `Canceled` + cleanup workflow starts (F-60, F-75, F-76) &rarr; retry (fork) &rarr; assert same ID, new attempt in `status.attempts[]` (F-23) &rarr; delete (terminal only) &rarr; assert 204 + event log retained (F-114)

**Webhook lifecycle (E2E, chained)**:

- Tag-push webhook &rarr; signature verified &rarr; raw event persisted &rarr; 202 (F-50, F-51, F-52) &rarr; trigger matches app registry &rarr; release auto-created with sender as actor (F-54, F-97) &rarr; release progresses to `PendingApproval` &rarr; approval submitted via `POST /approvals` (F-59, F-96) &rarr; CR merge simulated via mock provider + `pull_request.closed` webhook &rarr; correlated via stored mapping (F-63) &rarr; `wait: merge` re-verifies with provider &rarr; content verified &rarr; release completes (F-62)

**Auth integration**:

- All OIDC trust entry combinations; cross-repo rejection (F-93); API key (valid/expired/wrong grant); approval (self-approval forbidden, insufficient distinct count, `reject` fails environment) (F-96); OIDC create with mismatched SHA &rarr; `VersionNotVerified` (F-95); API-key create: "tag exists" or verify SHA (F-95); webhook create: SHA verified against provider tag object (F-95); tag protection: unprotected tag trigger refused (F-82); releaser-grant validation: trust entry in `releasers` without `release:create` &rarr; validation error (F-99a)

**Merge verification**:

- CR merged &rarr; content verified &rarr; advance (F-62); CR open &rarr; keep waiting; CR closed without merge &rarr; `ApprovalRejected` (terminal, not loop); CR head SHA modified + `on_modified: fail` &rarr; `ChangeRequestModified`; `on_modified: accept` &rarr; advance (F-62); handler pre-verifies: unmerged CR &rarr; synchronous 409 (F-64); branch protection check fails closed (F-65)

**Concurrency**:

- Supersede: new release acquires lease atomically, old release cancelled, CR closed with comment (F-70, F-73); queue: waiting release blocks on `recv`, advances when lease released (F-71); reject: `ConcurrencyRejected` (F-74); lower version arriving later &rarr; `ConcurrencyRejected` (F-72)

**Reconciliation**:

- `PendingApproval` release + mock CR state change &rarr; reconciler sends signal &rarr; workflow advances; reconciler never writes release status (F-67); reconciler resumes at correct wake time after restart (F-68); batched lookups with ETags (F-69)

**Event log and audit**:

- Create &rarr; cancel &rarr; `GET /events` &rarr; assert all transitions logged with actor, timestamp, reason (F-111); event IDs deterministic, no duplicates on re-execution (F-112); hash chain integrity; per-release sequence under row lock; SSE `id:` = sequence (F-113); `GET /snapshot` returns resolved template + inputs (F-105); delete release &rarr; `GET /events` still returns events (F-114); raw webhook payloads stored and under retention (F-115)

**Application groups**:

- Group discovery: scan finds all matching apps, `{application}` resolves correctly (§4.13a); per-app override takes precedence and is logged; per-env `match` replaces global `match`; periodic rescan picks up new apps, removes deleted ones; `owner_id` authorization: any repo in org can release, cross-org rejected; discovered name validation rejects invalid names; name collision (explicit app same name as discovered, without `extends`) &rarr; validation error

**Sidecar extensions**:

- Mock sidecar provider &rarr; create release using sidecar step &rarr; assert step executes via sidecar (F-45); SHA-256 of sidecar schema in snapshot (F-42); extension-secret channel auth (F-119); sidecar client separate from SSRF-guarded client; `http-call` step cannot reach sidecar

**API surface**:

- `GET /api/v1/clientconfig` unauthenticated, returns only issuer/audience (F-98, F-107); `GET /applications` and `GET /templates` list registered apps and templates (F-106); `GET /whoami` returns resolved identity, matched trust entry, grants (F-106); health endpoints unauthenticated: `/live` 200, `/ready` 200/503, `/startup` 200 (F-107); error responses match RFC 9457 schema with `code` extension (F-103); OpenAPI spec generated (F-104); all endpoints under `/api/v1/` (F-100)

**Security**:

- SSRF: `http-call` targeting each blocked CIDR &rarr; refused; DNS rebinding &rarr; refused; redirect to blocked IP &rarr; refused (F-117); rate limiting: exceed limit &rarr; 429 (F-118); secrets never in log output (F-87); secrets-in-DBOS-state invariant: scan step-output table (F-120)

**Config and reload**:

- SIGHUP reloads templates + registry + OIDC trust entries (F-129); leader election: standby reports 503 on `/ready` (F-131); replacement instance recovers pending workflows (F-133); `path` verification marks app unavailable on mismatch, server continues (F-10); `cargobike validate --resolve` fails on `path` mismatch (F-10)

### 14.3 Specialised Tests

- **T1: Crash-injection harness**: Kill the server at every step boundary for every built-in step type. Assert the release converges to the same result. Built alongside the first action step and run in CI from Phase 3 onward.
- **T2: Replay-compatibility**: Record workflows with the previous release's binary and replay them against the new one in CI. Record fixtures from the last pre-release builds so v1.1 can be tested against them.
- **T3: Property tests**: CEL gate evaluation, version ordering per scheme, path confinement, dot-notation path resolution.
- **T4: Build `core` for `wasm32-wasip2`** in CI.
- **T5: New invariant tests**: (a) Loading the documented example config verbatim (F-32). (b) Registry `approval` block without a template `wait: approval` step fails validation (F-96). (c) A lease is released or transferred on cancel, supersede, and environment completion (F-71, F-75). (d) Two signals for different environments never unblock the wrong wait (F-20a). (e) Losing the leader-election connection stops the executor (F-131). (f) `--until` exit code 0 for a skipped environment (US-4). (g) Application group discovery: scan finds all matching apps, `{application}` placeholder resolves correctly, per-app override takes precedence and is logged (§4.13a). (h) Per-env `match` replaces global `match` (§4.13a). (i) Periodic rescan picks up new apps and removes deleted ones (§4.13a). (j) Discovered name validation rejects invalid names (§4.13a). (k) `api_key` selector with `claims` &rarr; validation error (F-99b). (l) `api_key` in `approval.approvers` without `allow_machine_approvers` &rarr; validation error (F-99b). (m) Named secret reference `{ secret: name }` never appears in step output (F-146).

### 14.4 E2E CLI Tests

- **Full CLI lifecycle** (`assert_cmd`): `release create` (auto-detect CI env vars, OIDC auth) &rarr; `release list` &rarr; `release get` (with `--events`) &rarr; `release watch --until <env>` (assert exit codes 0-3, 10-12) &rarr; `release cancel` &rarr; `release retry` (fork + `--new`) &rarr; `release delete` (terminal only) &rarr; `release approve <id> --environment <env>` (F-59)
- **Output formats**: `release list -o table|json|yaml` &rarr; assert valid output per format (F-108)
- **Validate CLI**: `cargobike validate` on valid config + templates &rarr; pass; unconstrained OIDC trust entry &rarr; fail; gate referencing non-existent input &rarr; fail with diagnostics; `step_groups` &rarr; pass and resolve (F-30, US-13); `cargobike validate --resolve` &rarr; `path` mismatch &rarr; error (F-10)
- **CI integration**: Simulated CI env (`GITHUB_REPOSITORY`, `GITHUB_SHA`, `GITHUB_RUN_ID`) &rarr; `release create` &rarr; assert `CiContext` populated, OIDC token requested with correct audience, version verified against tag/SHA (US-1)
- **CLI config**: `CARGOBIKE_URL` env var used; `--url` flag overrides; `http://` refused except localhost; `--allow-http` override (F-98, F-130); `cargobike context list` / `cargobike context use <name>` (§9.7); `cargobike whoami` prints resolved identity + grants (F-106)
- **Health E2E**: `GET /live` 200, `GET /ready` 200/503, `GET /startup` 200 (US-12)
- **Webhook E2E**: Tag-push webhook &rarr; auto-create &rarr; `release watch` &rarr; `POST /approvals` &rarr; mock merge &rarr; `watch` exits 0 (US-8)
- **Audit E2E**: Create &rarr; advance &rarr; approve &rarr; merge &rarr; complete &rarr; `GET /events` &rarr; assert all transitions present, deterministic IDs, no duplicates &rarr; delete release &rarr; `GET /events` still returns events (US-14)
- **Sidecar E2E**: Start mock sidecar &rarr; configure provider &rarr; create release &rarr; assert step executes via sidecar, schema SHA-256 in snapshot (US-10)
- **Application list**: `cargobike application list` shows registered apps (F-106); `cargobike template list` shows registered templates (F-106); `cargobike-server hash-api-key` produces argon2 hash from stdin (§9.3)

---

## 15. Build & CI

### 15.1 Makefile

```makefile
.PHONY: build build-release test lint fmt fmt-check clippy coverage audit ci clean

build:
	cargo build

build-release:
	cargo build --release

test:
	cargo test --all-targets

lint: fmt-check clippy

fmt:
	cargo fmt

fmt-check:
	cargo fmt --check

clippy:
	cargo clippy --all-targets -- -D warnings

coverage:
	cargo llvm-cov --workspace

audit:
	cargo audit

ci: fmt-check clippy test audit

clean:
	cargo clean
```

### 15.2 CI Pipeline

1. Format check
2. Clippy (`-D warnings`)
3. Test (with PostgreSQL service container)
4. Coverage (PR comment)
5. Audit (PR comment)
6. Build matrix: linux/amd64, linux/arm64, darwin/amd64, darwin/arm64
7. Build `core` for `wasm32-wasip2`
8. Replay-compatibility test (previous release vs new)

### 15.3 Release Pipeline

Conventional commit-driven versioning. Two binaries: `cargobike` (CLI) and
`cargobike-server`. Signed binaries (cosign), SBOM, GitHub artifact
attestations.

---

## 16. Incremental Development Plan

### 16.0 Task Dependency Guide

This section describes the dependencies between tasks so that work is
ordered to minimise duplication. Hard dependencies mean a task cannot
start without the dependency. Soft dependencies mean a task should follow
another to avoid building throwaway stubs or duplicating logic.

#### Dependency graph

```
1.2 (workspace) ──► 1.3 (core types) ──► everything
1.1 (DBOS spike) ──► 3.10 (signals), 3.9 (interpreter), 3.12 (leases),
                     3.13 (cleanup), 3.14 (reconciliation)
1.4 (CI) ──► 2.9 (TestServer), 3.15 (crash harness)

2.1 (server skeleton) ──► 2.2 (config) ──► 2.3 (database), 2.5 (OIDC),
                                          2.6 (API keys), 2.7 (registry),
                                          2.8 (health/leader)
2.7 (registry) ──► 2.4 (release service create)
2.5 + 2.6 (auth) ──► all authenticated endpoints

3.1 (template parsing) ──► 3.2 (CEL) ──► 3.3 (step registry)
3.10 (signals) ──► 3.4 (control steps) + 3.11 (merge verification)
3.3 ──► 3.5, 3.6, 3.7, 3.8 (action steps)
3.10 + 3.11 + 3.12 ──► 3.9 (interpreter)
3.9 + 3.12 + 3.13 ──► 3.14 (reconciliation)
3.9 + all steps ──► 3.15 (crash harness)

4.1 ──► 4.2 ──► 4.3, 4.4 (GitHub provider)
2.4 + 2.5 ──► 4.6 (CLI needs functional API + auth)
4.5 ──► 4.6, 4.7, 4.8 (CLI)

3.6 ──► 5.2 (webhook correlation needs CR mapping)
4.4 ──► 5.1 ──► 5.2 (webhook chain)
3.13 ──► 5.5 (cancel endpoint starts cleanup)
2.4 ──► 5.6 (create idempotency refines release service create)
5.4 ──► 4.7 (SSE event IDs from event log — cross-phase)
```

#### Ordering rules to avoid duplication

1. **Core types first (1.3)**: Every crate depends on `cargobike-core`.
   The Provider trait, StepType trait, Release model, PipelineTemplate,
   RepoRef, error types, and version schemes must exist before anything
   else.

2. **Application registry before release service (2.7 &rarr; 2.4)**: F-3
   says a create request is `{application, version}` and the registry
   resolves the template, source, edits, and releasers. Without 2.7, 2.4's
   create is a stub that gets reworked.

3. **Basic template parsing before registry validation (3.1 &rarr; 2.7)**:
   2.7 validates that a registry `approval` block requires a template
   `wait: approval` step. This needs template parsing. If 3.1 is not
   available, 2.7 duplicates YAML scanning logic. Pull basic template
   parsing into Phase 2 or accept the cross-phase dependency.

4. **Application groups after base registry (2.7 &rarr; 2.7a)**: Groups
   extend the registry with discovery, `{application}` placeholders,
   per-app overrides, and periodic rescan. The base registry must work
   first; groups inherit and override it. Splitting prevents a
   half-built base registry while group discovery is incomplete.

5. **Signal system before control steps (3.10 &rarr; 3.4)**: Control steps
   use `dbos::recv` with topics, which is the signal system. Without
   3.10, 3.4's recv calls have no topic/idempotency-key framework.

6. **Control steps and merge verification together (3.4 + 3.11)**:
   `wait: merge` is recv + verification loop. Merge verification is the
   logic within that loop. Doing them separately means 3.4 builds a
   placeholder that 3.11 replaces.

7. **Signal system, merge verification, and leases before interpreter
   (3.10 + 3.11 + 3.12 &rarr; 3.9)**: The interpreter integrates all
   three. Building 3.9 first creates a skeleton that is significantly
   modified when 3.10-3.12 are added.

8. **SSRF protection alongside step registry (5.7 &rarr; 3.3)**: 3.3
   builds a "SSRF-guarded HTTP client" in StepContext. 5.7 implements
   the full protection (DNS resolution, CIDR deny-lists, redirect
   re-check). If done 8 weeks apart, the client is built twice. Pull 5.7
   into Phase 3.

9. **Create idempotency with release service (5.6 &rarr; 2.4)**: 5.6
   refines 2.4's create (200 for duplicates, `Idempotency-Key` header).
   Doing it in Phase 5 means 2.4's create is incomplete for 8 weeks.
   Move 5.6 to Phase 2, right after 2.4.

10. **Event log sequence before CLI watch (5.4 &rarr; 4.7)**: SSE event
    IDs come from the event log's per-release sequence (F-102, F-113).
    4.7's `Last-Event-ID` reconnect needs 5.4. Move 5.4's per-release
    sequence to Phase 4, or accept 4.7 is incomplete until Phase 5.

11. **Shared authorization layer for OIDC and API keys (2.5 + 2.6)**:
    Both implement grant checking and `releasers` scoping. Build a
    common authorization layer to avoid duplicating grant-matching logic.

12. **`cargobike-api` DTOs before server and CLI (2.0a &rarr; 2.4, 4.5)**:
    The `cargobike-api` crate contains DTOs shared between server and
    CLI. Without an explicit task, DTOs are defined ad hoc in the server
    and CLI, risking duplication. Define DTOs in `cargobike-api` after
    1.3 (core types) and before 2.4 (server) and 4.5 (CLI).

13. **Webhook chain: GitHub parsing &rarr; receiver &rarr; correlation
    (4.4 &rarr; 5.1 &rarr; 5.2)**: 5.1 without 4.4 stubs out parsing;
    5.2 without 5.1 duplicates event receipt. Sequence tightly.

14. **Template validation shared between CLI and JSON Schemas
    (4.8 + 5.9)**: `cargobike validate` and `schemars`-derived JSON
    Schemas both validate templates. Share the same schemas to avoid
    duplicating validation logic.

#### Duplication risks summary

| Risk | Tasks involved | Mitigation |
|------|----------------|------------|
| Authorization logic duplicated | 2.5 (OIDC) + 2.6 (API keys) | Shared grant-checking layer |
| Template parsing duplicated | 2.7 (registry) + 3.1 (template parsing) | Pull basic parsing into Phase 2 |
| SSRF client built twice | 3.3 (StepContext) + 5.7 (SSRF) | Pull 5.7 into Phase 3 |
| Create endpoint reworked | 2.4 (release service) + 5.6 (idempotency) | Move 5.6 to Phase 2 |
| Merge verification placeholder | 3.4 (control steps) + 3.11 (verification) | Do together |
| DTOs defined independently | 2.1/2.4 (server) + 4.5/4.6 (CLI) | `cargobike-api` crate task |
| Template validation duplicated | 4.8 (CLI validate) + 5.9 (JSON Schemas) | Share `schemars` schemas |
| Interpreter reworked | 3.9 + 3.10/3.11/3.12 | Build mechanisms first, then interpreter |
| Webhook handling stubbed | 5.1 + 4.4 + 5.2 | Sequence: 4.4 &rarr; 5.1 &rarr; 5.2 |

### Phase 1: DBOS Spike & Foundation (weeks 1-2)

> Each task below is a unit of work that must be further broken down into
> individual commit-sized pieces before development begins.

**1.1 DBOS Rust SDK spike**
- Prototype `step`, `step_with`, `recv`, `send`, `send_with`, `sleep`,
  `cancel`, `fork` (`ForkFrom::LastFailure`), `delete`
- Verify step serialization and deserialization
- Verify app-version recovery and executor ID stability across restarts
- Verify `Forks` option for message routing on fork
- Document findings and gaps

**1.2 Cargo workspace setup**
- Root `Cargo.toml` with 7 crate members, workspace deps, lints, release
  profile
- `Makefile`, `rustfmt.toml`, `.gitignore`
- `templates/service.yaml` example
- `cargo-deny` configuration

**1.3 `cargobike-core` types**
- Release model structs (metadata, spec, status, phase enums, environment
  status, change request ref, actor, CI context, workflow info)
- Error types (`thiserror`)
- Provider trait + `ProviderCaps` + `Edit`/`CommitResult`/`ChangeRequest`/
  `BranchProtection`/`CommitStatus` types
- `StepType` trait, `StepContext`, `StepOutput`, `StepError`
- `RepoRef`, `CredentialStore`, `ProviderRegistry`
- `PipelineTemplate` struct (inputs, environments, steps, gates)
- Version scheme types (`semver`, `calver`, `opaque`)

**1.4 CI skeleton**
- GitHub Actions: fmt-check, clippy, test, audit, build matrix
- `wasm32-wasip2` build job for `cargobike-core`
- `testcontainers` Postgres service container

**Deliverable**: Spike proves the DBOS model. `cargobike-core` compiles,
passes tests, and builds for `wasm32-wasip2`.

### Phase 2: Server Core + Auth (weeks 3-4)

> Each task below is a unit of work that must be further broken down into
> individual commit-sized pieces before development begins.

**2.1 Server skeleton with axum**
- `cargobike-server` crate (lib + `main.rs`)
- Router setup, error handler middleware (RFC 9457 Problem Details)
- `tower-http` layers (TraceLayer, CatchPanicLayer, RequestBodyLimitLayer,
  SetSensitiveRequestHeadersLayer)
- `tracing`/`tracing-subscriber` setup

**2.2 Config builder**
- `figment` layered config: defaults &rarr; file &rarr; env &rarr; flags
- `clap` server args with `CARGOBIKE_SERVER_CONFIG` env var
- `secrecy::SecretString` for all secrets, hand-written `Debug`
- `${VAR}` / `${VAR:-default}` interpolation via `shellexpand` (fail-fast)
- `{version}` format-string placeholder for registry fields (not `${{ }}`)
- `humantime` duration parsing
- Config validation at startup

**2.3 Database setup**
- `sqlx` 0.9 pool (aligned with `dbos` crate), rustls TLS
- Migrations: releases, events, leases, webhook payloads, registry cache
- `ReleaseRepository` (insert, get, list with cursor pagination, update,
  delete)

**2.4 Release service**
- `ReleaseService`: create (idempotent, `(application, version)` unique),
  get, list (filters, pagination), cancel, delete (terminal only)
- `If-Match` / `resource_version` optimistic concurrency
- `POST /api/v1/releases` (202, `Location` header, `Idempotency-Key`)

**2.5 OIDC validation**
- `jsonwebtoken` token verification (signature, `iss`, `aud`, `exp`, `nbf`,
  `iat`, clock-skew)
- Algorithm allowlist (reject `none`, `HS*`; accept RS*, PS*, ES*)
- OIDC discovery + JWKS fetch with `moka` cache (refetch on unknown `kid`,
  rate-limited)
- Trust-policy claim matching (AND, glob, array-valued, immutable IDs)
- Actor model (`issuer`, `subject`, `display_name`, `CiContext`)

**2.6 API keys**
- `argon2` hashing and verification (constant-time)
- Named keys with grants (applications reference keys via `releasers`)
- Bootstrap key via `CARGOBIKE_SERVER_BOOTSTRAP_API_KEY` (hashed at startup, localhost-only)
- Two active keys for rotation

**2.7 Application registry**
- Registry loading from config file
- Application, source, template, versioning, triggers, releasers,
  environments (repo, edits, concurrency, approval block)
- Validation: `approval` requires template `wait: approval` step
- SIGHUP hot reload (safe via snapshot pinning)

**2.8 Health, clientconfig, leader election**
- `GET /api/v1/live`, `GET /api/v1/ready`, `GET /api/v1/startup`
- `GET /api/v1/clientconfig` (issuer + audience hints, unauthenticated)
- Postgres advisory-lock leader election (fence on lock loss, exit process)
- Standby serves webhooks without launching executor

**2.9 TestServer + integration tests**
- `TestServer` with mock provider, `testcontainers` Postgres
- Auth: all OIDC trust entry combinations, cross-repo rejection, API key
  (valid/expired/wrong grant/not referenced in `releasers`)
- Create, list, get, cancel, delete

**Deliverable**: Server runs with auth, accepts API calls, persists to
PostgreSQL. Auth is functional before any real GitHub integration.

### Phase 3: Pipeline Engine (weeks 5-6)

> Each task below is a unit of work that must be further broken down into
> individual commit-sized pieces before development begins.

**3.1 Template parsing and validation**
- `serde_yaml_ng` deserialization into `PipelineTemplate`
- Typed inputs (`inputs` app-wide, `environment_inputs` per-environment)
- `step_groups` for reusable step lists
- `cargobike validate` (offline mode, local files, mock provider)
- JSON Schemas via `schemars` for editor autocompletion
- Gate field validation against version scheme (F-27a)

**3.2 CEL expression engine**
- `cel` crate integration with safety limits (cost, size, regex,
  comprehensions)
- `${{ ... }}` expression evaluation (version, inputs, steps outputs)
- Pure functions only (no `now()`)

**3.3 Step registry**
- Built-in step type registration (versioned `@1`)
- Sidecar step type registration
- `StepContext` construction (provider registry, credential store,
  SSRF-guarded HTTP client, idempotency key, cancel token, logger)

**3.4 Control steps**
- `wait: merge` (recv on topic `merge/{env}/{step_id}` + verification loop)
- `wait: approval` (recv on topic `approval/{env}/{step_id}`)
- `wait: sleep` (dbos::sleep)

**3.5 Action step: `commit-files`**
- Structured edits (dot-notation paths for YAML/JSON, key paths for TOML)
- Format-preserving YAML/TOML editing (evaluate `yamlpatch`/`yaml-edit`/
  `toml_edit`)
- Fused edit + commit (single step, no intermediate state)
- Path confinement against registry-declared globs

**3.6 Action step: `change-request`**
- Create branch (deterministic name), create CR, look up existing open CR
  by head branch before creating
- Branch protection check via provider capability
- Store `(provider, repo_id, cr_number) -> (release_id, environment)`
  mapping for webhook correlation

**3.7 Action step: `http-call`**
- SSRF-guarded HTTP client (DNS resolution check, redirect re-check,
  deny-list CIDR ranges)
- Caller-documentation of idempotency requirements

**3.8 Action step: `set-labels`**
- Add labels to a change request via provider

**3.9 Interpreter workflow**
- Register `cargobike.interpret.v1` before `launch()`
- Read snapshotted template, execute sequence of DBOS operations
- Snapshot pinning (template + registry inputs + step-type versions +
  content hash)
- Implicit bookkeeping: `set-status`, `close-superseded` (not user-visible
  steps)

**3.10 Signal system**
- `dbos::send_with` with topics and idempotency keys
- Early-message rejection (approvals endpoint rejects unless
  `PendingApproval`)
- Fork routing to current attempt's workflow ID

**3.11 Merge verification**
- Content verification (read target files on base branch, check intended
  values at intended pointers)
- `on_modified: fail | accept` policy
- Distinguish closed-without-merge (`ApprovalRejected`, terminal) from
  not-merged-yet (keep waiting)

**3.12 Lease rows and concurrency**
- Conditional INSERT `(app, env) -> release_id`
- Per-environment release on completion/skip/supersede
- `supersede`: atomic transfer from old holder
- `queue`: block on `recv` (topic `lease/{env}`)
- `reject`: `ConcurrencyRejected`
- Lower version arriving later: `ConcurrencyRejected`

**3.13 Cleanup workflow**
- `cargobike.cleanup.v1`: close CR, delete branch, post comment, release
  or transfer lease
- Started from cancel and supersede paths (cancelled workflow cannot run
  compensation)

**3.14 Reconciliation**
- Long-lived durable workflow looping on `dbos::sleep` (default 5 min)
- Batched CR lookups (GraphQL), ETags
- Sends signals only (never writes release status directly)

**3.15 Crash-injection harness (T1)**
- Kill server at every step boundary for every built-in step type
- Assert release converges to the same result
- Run in CI from this phase onward

**Deliverable**: Full release lifecycle with mock provider.

### Phase 4: GitHub Provider & CLI (weeks 7-8)

> Each task below is a unit of work that must be further broken down into
> individual commit-sized pieces before development begins.

**4.1 GitHub provider: authentication**
- `octocrab` App authentication (JWT, installation tokens)
- Per-repo installation lookup, scoped installation tokens
- Minimal permission set documentation

**4.2 GitHub provider: REST operations**
- Branches, commits, change requests (PRs), files, labels, comments,
  commit status
- `check_branch_protection`, `check_tag_protection`
- `api_url` for GitHub Enterprise Server

**4.3 GitHub provider: GraphQL**
- `graphql_client` typed queries for batched reconciler lookups
- Compile-time schema checking

**4.4 GitHub provider: webhook parsing**
- Verify `X-Hub-Signature-256` (constant-time, two secrets)
- Parse and normalise webhook payloads
- Installation + repository allowlist check

**4.5 CLI: config and auth**
- `cargobike-cli` crate, `clap` with `CARGOBIKE_*` env vars
- `CARGOBIKE_CONFIG` for CLI config file
- Client auth types: `none`, `api-key`, `exec`, `github-actions`
- Auto-detect CI env vars (`GITHUB_REPOSITORY`, `GITHUB_SHA`,
  `GITHUB_RUN_ID`)
- Refuse `http://` server URLs except localhost (`--allow-http` override)

**4.6 CLI: release commands**
- `release create` (auto-resolve application, `--wait`)
- `release list` (filters, `-o table|json|yaml`)
- `release get` (`--events`)
- `release cancel`, `release delete`, `release retry` (`--new`)

**4.7 CLI: watch**
- `release watch <id>` with SSE (`reqwest-eventsource`)
- `--until <env>` with exit codes (0-3 release, 10-12 client)
- `Skipped` counts as success (exit 0 with message)
- `Last-Event-ID` reconnect
- `indicatif` progress (disabled in non-TTY)

**4.8 CLI: validate**
- `cargobike validate` (offline, local files, mock provider)
- `miette` diagnostics with line/column

**4.9 Integration tests with wiremock**
- Full lifecycle against mocked GitHub API
- CLI end-to-end tests with `assert_cmd`

**Deliverable**: Full lifecycle against a real GitHub repository.

### Phase 5: Webhooks, Extensions & Polish (weeks 9-10)

> Each task below is a unit of work that must be further broken down into
> individual commit-sized pieces before development begins.

**5.1 Webhook receiver**
- `POST /webhooks/{provider}` endpoint
- Verify before parse, constant-time secret comparison, body size limit
- Two secrets for rotation, per-provider `webhook_secrets` list (`{ file }` references)
- Fast-ack: verify, persist raw event, start DBOS workflow, return 202
- Duplicate deliveries no-op (workflow ID deduplication)
- Unrecognized events acknowledged (200) and ignored

**5.2 Webhook correlation**
- `pull_request.closed` correlates via stored
  `(provider, repo_id, cr_number) -> (release_id, environment)` mapping
- Tag-push triggers (`refs/tags/v*`) extract version from tag name
- Tag protection check (`require_tag_protection: true`)

**5.3 Sidecar extension transport**
- HTTP/gRPC transport (tonic + prost or JSON over axum/reqwest)
- Channel authentication (Unix socket, mTLS, or extension-secret header)
- Separate sidecar HTTP client (not SSRF-guarded)
- Schema SHA-256 pinning in snapshot

**5.4 Event log and audit**
- Immutable event log with per-release sequence (row lock)
- Hash chain with signed checkpoints at retention cuts
- `GET /api/v1/releases/{id}/events`
- `GET /api/v1/releases/{id}/snapshot`
- Retention policy (default 365 days, separate DB role)
- Raw webhook payloads under retention

**5.5 Cancel and approval endpoints**
- `POST /api/v1/releases/{id}/cancel` (204, starts cleanup workflow)
- `POST /api/v1/releases/{id}/approvals` (accept only while
  `PendingApproval`, bind to attempt + head SHA)

**5.6 Create idempotency**
- `(application, version)` unique among non-terminal releases
- Duplicate create returns 200 with existing release
- `Idempotency-Key` header

**5.7 SSRF protection and rate limiting**
- `hickory-resolver` DNS resolution, `ipnet` deny-lists
- Redirect policy with re-check on every hop
- `tower_governor` rate limiting on API and webhook routes

**5.8 OpenAPI spec**
- `utoipa` + `utoipa-axum` generation
- All endpoints, error codes, schemas

**5.9 JSON Schemas for templates**
- `schemars` derive from Rust types
- Published for editor autocompletion
- `jsonschema` validation in `cargobike validate`

**Deliverable**: Webhooks, sidecar extensions, audit trail, OpenAPI.

### Phase 6: Open-Source Release (weeks 11-12)

> Each task below is a unit of work that must be further broken down into
> individual commit-sized pieces before development begins.

**6.1 GitHub Action**
- `actions/setup-cargobike/action.yml` composite action
- Pulls container image, creates wrapper script on `PATH`
- OIDC authentication via environment variables

**6.2 Quickstart and examples**
- Docker-compose quickstart
- Example repo with two-environment workflow
- Protected tags guidance (GitHub rulesets)

**6.3 Community files**
- `CONTRIBUTING.md`, `SECURITY.md`, code of conduct, DCO
- License: dual MIT/Apache-2.0
- `clap_complete` shell completions, `clap_mangen` man pages

**6.4 Stability and supply chain**
- Semver policy for API, WIT interface, sidecar protocol, template schema
- `cargo-dist` multi-platform binaries
- Signed binaries (cosign), SBOM (`cargo-cyclonedx`), artifact attestations
- `cargo-auditable` dependency embedding
- `cargo-semver-checks` on public crates

**6.5 Threat model**
- Published before v1.0
- Sidecar trust boundaries documented
- Tag protection, confused deputy, token leak paths

**6.6 Documentation**
- README, installation, quickstart
- Architecture overview
- Configuration reference
- Template authoring guide

**6.7 Replay-compatibility fixtures (T2)**
- Record workflows from the last pre-release builds
- CI job replays them against the new binary

**6.8 Performance validation**
- Cold start < 2 seconds
- Release creation < 500ms
- Workflow recovery < 5 seconds
- CLI binary < 10 MB, server binary < 15 MB (no WASM)

**Deliverable**: v1.0.0 release.

---

## 17. Open Questions

### Q-1: WASM binary size

wasmtime + Cranelift will likely break the 15 MB server target. v1.0 ships
without WASM (sidecar only). v1.1 adds WASM behind a cargo feature. Measure
early.

### Q-2: Multi-replica recovery

v1: single active replica, enforced via leader election. v2: executor IDs,
dead-executor detection, multi-replica recovery.

### Q-3: Conventional commit versioning as a product feature

G-11 refers to Cargobike's own CI. A `builtin/bump-version` step type is
post-v1.

---

## 18. Risks & Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|-----------|
| DBOS Rust SDK gaps | Medium | High | Spike in week 1. Contribute upstream. |
| WASM binary size | High | Medium | v1.0 ships without WASM. Sidecar only. |
| Concurrent workflow execution | Medium | High | Single active replica (leader election). Idempotent steps (A1). Lease rows (A2). |
| Unauthorized repo creates/advances releases | Medium | High | OIDC trust entries + application registry. `repository_id` match. Version verification. |
| Caller spoofs merge signal | Medium | High | Merges arrive via webhook correlation + reconciler only. No public `Merged` event. |
| Confused deputy (GitHub App) | Medium | High | Application registry: create is `{application, version}`. Per-repo installation tokens. |
| Token leaked through Debug | Low | High | `SecretString`. Hand-written `Debug`. Regression tests. |
| Webhook missed while server down | Medium | Medium | Fast-ack + durable. Reconciler every 5 min. Standby serves webhooks without executor (F-131). |
| Secrets in DBOS state | Low | High | Never in step outputs/args. Invariant test. |
| `serde_yaml_ng` unmaintained | Low | Low | Pin and monitor RUSTSEC. |
| Upgrade breaks in-flight workflows | Medium | High | Snapshot pinning. Versioned step types. Replay-compatibility test (T2). |
| Approver policy silently ignored | Medium | High | Validation invariant: registry `approval` block requires template `wait: approval` step (F-96). |
| Sidecar compromised or buggy | Medium | High | Channel authentication. Sidecar inside approval boundary (threat model). Versioned step types. Separate client. |
| Signal routed to wrong wait | Medium | High | Topic strings per wait type. Early-message rejection. Idempotency keys on sends (F-20a). |

---

## 19. Success Metrics

| Metric | Target |
|--------|--------|
| Cold start time (server) | < 2 seconds |
| Release creation latency | < 500ms |
| Zero panics | No `unwrap()` in non-test code |
| Test coverage | >= 85% |
| CLI binary size | < 10 MB |
| Server binary size (no WASM) | < 15 MB |
| Workflow recovery time | < 5 seconds |

---

## 20. Development Guidelines

### 20.1 Commits

- **Reasonable size**: A commit must address one problem or one cohesive
  change. If a change spans multiple concerns, split it into multiple
  commits.
- **Conventional commits**: All commits use conventional commit format with
  a scope:

  ```
  <type>(<scope>): <description>
  ```

  Type must be `feat` or `fix`. Scope must be one of the crate names or
  `ci`. Examples:

  ```
  feat(core): add ReleaseMetadata struct
  fix(engine): handle missing template input gracefully
  feat(ci): add wasm32-wasip2 build job
  ```

### 20.2 Pre-commit Checks

Before committing, all three must pass with zero issues:

```bash
make lint   # clippy + fmt-check, no warnings
make test   # all tests pass
make build  # workspace compiles
```

If any check fails, fix the issue before committing. Do not commit code
that fails lint, tests, or build.

### 20.3 Code Quality

| Principle | Guideline |
|-----------|-----------|
| **Constants** | No hardcoded strings or numbers. Define constants for magic values (retry counts, timeouts, header names, default paths). |
| **DRY** | If code is duplicated, extract a utility function. One copy lives in the right crate; others import it. |
| **Domain-driven design** | Organise code by domain (release, auth, webhook, provider), not by technical layer. Each domain owns its model, service, and API surface. |
| **Single responsibility** | Functions do one thing. If a function has an `if`/`else` where each branch does different work, split it into two functions. |
| **KISS** | Prefer simple, readable solutions over clever ones. If a design needs a long comment to explain it, simplify it. |
| **Approved libraries** | Use the crates listed in Appendix A. Do not pull in a dependency for a single function that can be written in a few lines. |
| **Explanatory names** | Names describe what something does, not how. `create_release` not `handle_post`. `MAX_RETRY_COUNT` not `N`. |
| **Comments** | Comments explain *why*, not *what*. If the code is self-explanatory, no comment. If a non-obvious decision was made, explain it. Do not describe deleted code &mdash; focus on what is there now and its functionality. |

### 20.4 Testing

- Write unit tests for isolated functions. Each function with non-trivial
  logic gets its own test.
- Unit tests live in `#[cfg(test)] mod tests` within the same file.
  Integration tests live in each crate's `tests/` directory.
- Use `rstest` for parametric tests, `pretty_assertions` for readable diffs.
- Test names describe the scenario: `test_create_release_rejects_unknown_application`.

## 21. Open-Source Readiness

### 21.1 License

Dual MIT/Apache-2.0 (the Rust convention).

### 21.2 Community Files

- `CONTRIBUTING.md`
- `SECURITY.md` with a disclosure process
- Code of conduct
- DCO (Developer Certificate of Origin)

### 21.3 Stability Promises

- Semver policy for `/api/v1`
- WIT extension interface versioning
- Sidecar protocol versioning
- Template schema versioning (schema version field in template YAML)

### 21.4 Supply Chain

- Signed binaries and images (cosign)
- SBOM
- Build provenance via GitHub artifact attestations

### 21.5 Threat Model

A threat model document is published before v1.0, reviewed alongside the
security findings in this PRD.

---

## Appendix A: Dependency Selection

### A.1 Foundation

| Crate | Use in Cargobike | Notes |
|---|---|---|
| `tokio` | Async runtime for server and CLI | Already implied by `dbos`, axum and sqlx |
| `tokio-util` | `CancellationToken` for step contexts, codecs | `dbos` already depends on it |
| `futures` | Stream combinators for SSE and batching | |
| `serde`, `serde_json` | All DTOs, snapshots, step outputs | Without the `preserve_order` feature, `serde_json::Map` is a sorted `BTreeMap`, which helps canonical hashing of snapshots (F-16) |
| `thiserror` | Typed library errors (§10.1) | |
| `anyhow` | Binary entry points only (§10.1) | |
| `async-trait` | `dyn Provider` and `dyn StepType` | Native `async fn` in traits is not object-safe, so `async-trait` is still the pragmatic choice for trait objects |
| `bytes` | Raw webhook bodies (verify before parse, F-51) | |
| `strum` | `Display`/`FromStr`/iteration for `Phase`, grant names and error codes | Cuts boilerplate on enums that cross the API boundary |
| `uuid` (feature `v7`) | Release IDs (F-2) | |
| `time` | Timestamps | `dbos` uses `time`; standardising on it avoids carrying both `time` and `chrono` |

### A.2 Durable Execution and Database

| Crate | Use in Cargobike | Notes |
|---|---|---|
| `dbos` | Durable workflows, steps, `recv`/`send`, `sleep`, cancel/resume/fork | Core requirement |
| `sqlx` | Cargobike's own tables (releases, events, leases, registry cache), migrations via `sqlx::migrate!` | **Use the same major version as `dbos` (0.9)** so there is one sqlx in the tree and pools can be shared. Enable Postgres, the tokio runtime, rustls TLS, migrations, `uuid` and `time` features |

No extra crate is needed for leader election (F-131) or environment leases (F-71): `pg_try_advisory_lock` and conditional `INSERT … ON CONFLICT` through sqlx are enough.

### A.3 HTTP Server

| Crate | Use in Cargobike | Notes |
|---|---|---|
| `axum` | REST API, webhook endpoints | Built-in SSE support (`axum::response::sse`) covers `/watch` (F-102) |
| `axum-extra` | Typed headers (`If-Match`, `Last-Event-ID`, `Idempotency-Key`) | |
| `tower` | Middleware composition, timeouts, load shedding | |
| `tower-http` | Ready-made layers (see below) | |
| `tower_governor` / `governor` | Rate limiting on API and webhook routes (F-118) | `governor` is the underlying GCRA limiter |
| `tokio-stream` | Turning broadcast/watch channels into SSE streams | |
| `utoipa` (+ `utoipa-axum`) | OpenAPI generation (F-104) | Keeps the spec next to the handlers |

Useful `tower-http` layers, mapped to PRD requirements:

| Layer | Requirement |
|---|---|
| `RequestBodyLimitLayer` | Webhook and API body limits (F-51, plus API request body limit) |
| `SetSensitiveRequestHeadersLayer` | Marks `Authorization` and webhook signature headers as sensitive so tracing never prints them (F-87); complements `RedactingWriter` |
| `TraceLayer` | Request logging with `tracing` |
| `TimeoutLayer` | Bound handler time; webhook handler must answer well under GitHub's 10-second limit |
| `CatchPanicLayer` | Turns an unexpected panic into an RFC 9457 `InternalError` instead of a dropped connection |
| `CompressionLayer` | Optional, for large list responses |

### A.4 Outbound HTTP and SSRF Protection

| Crate | Use in Cargobike | Notes |
|---|---|---|
| `reqwest` | Provider calls, `http-call` step, OIDC discovery/JWKS, CLI → server | Disable default features and enable rustls, JSON and streaming |
| `url` | Parsing and normalising target URLs before policy checks | |
| `ipnet` | Deny-lists of CIDR ranges (loopback, RFC 1918, link-local, ULA) for F-117 | |
| `hickory-resolver` | Controlled DNS resolution, plugged into reqwest's `dns::Resolve` hook | Resolve once, check every address against the deny-list, then connect to the checked IP (defeats DNS rebinding) |
| `reqwest-middleware` + `reqwest-retry` | Retries for calls **outside** DBOS steps (e.g. JWKS fetch, CLI) | Inside steps, rely on `dbos::step_with` retries instead, to avoid retry-on-retry |

Also set a custom reqwest redirect policy that re-runs the SSRF check on every hop.

### A.5 Authentication and Cryptography

| Crate | Use in Cargobike | Notes |
|---|---|---|
| `jsonwebtoken` | OIDC token verification: signature, `iss`, `aud`, `exp`, `nbf`, algorithm allowlist (§9.1); also GitHub App JWTs | Supports JWK sets directly. Recent versions ask you to choose a crypto backend feature |
| `openidconnect` | Alternative if full discovery-document handling is wanted | Heavier; `jsonwebtoken` plus a small discovery fetcher is usually enough for token *validation* |
| `moka` | TTL caches for JWKS and discovery documents, and short-lived installation tokens | Tokens are cached in memory only, never returned from a step (F-120) |
| `argon2` | Hashing and verifying API keys (F-85) | RustCrypto; use the `password-hash` string format (`$argon2id$…`) shown in §9.3 |
| `subtle` | Constant-time comparison (GitLab token, webhook secrets) | `hmac`'s `verify_slice` is already constant-time for HMACs |
| `hmac`, `sha2` | GitHub `X-Hub-Signature-256` verification; SHA-256 extension pinning and snapshot hashes | |
| `secrecy` | `SecretString` for every secret in config and args (F-87, F-90) | Its `Debug` impl prints a redaction marker |
| `zeroize` | Wipe private-key bytes after loading | `secrecy` builds on it |
| `base64`, `hex` | Cursor encoding (F-101), signature/hash formatting | |
| `rustls` | TLS everywhere (via reqwest and sqlx) | No OpenSSL, keeps static builds simple |

### A.6 Git Providers

| Crate | Use in Cargobike | Notes |
|---|---|---|
| `octocrab` | GitHub REST client for `cargobike-provider-github`: App authentication, installation tokens, PRs, contents, checks | Also ships typed webhook payload models, which helps normalising events (F-46). Construct the client inside each step (§6.7) |
| `graphql_client` | Typed GraphQL queries (batched reconciler lookups, F-69) | Queries are checked against GitHub's schema at compile time |
| `gitlab` | Starting point for a future GitLab provider extension or sidecar | |
| `gix` (gitoxide) | Optional generic "plain git over SSH/HTTPS" provider for hosts without a usable API | Pure Rust; only worth it if such a provider is planned |

### A.7 Configuration and Schemas

| Crate | Use in Cargobike | Notes |
|---|---|---|
| `clap` (`derive`, `env`, `wrap-help`) | Server and CLI arguments (§4.17) | |
| `clap_complete`, `clap_mangen` | Shell completions and man pages for the CLI | Cheap polish for an open-source CLI |
| `figment` | Layered config: defaults → file → env → flags (F-125) | Tip: let figment own env handling (`Env::prefixed("CARGOBIKE_").split("__")`) and feed only explicitly-set clap flags in as the top layer, so env vars aren't parsed twice with different rules |
| `serde_yaml_ng` | YAML (de)serialisation for config, templates, `-o yaml` output | Maintained fork of the archived `serde_yaml`; `serde_yml` is RUSTSEC-flagged |
| `humantime` / `humantime-serde` | Durations such as `timeout: 7d`, `interval: 5m` | |
| `shellexpand` | `${VAR}` / `${VAR:-default}` expansion in server config (F-89) | Expand string values **after** parsing, never the raw file, and never in templates (F-32) |
| `directories` | Platform-correct CLI config paths on Linux/macOS | |
| `schemars` | Derive JSON Schemas for templates, registry and config from the Rust types (US-13) | One source of truth for editor autocompletion and validation |
| `jsonschema` | Validate YAML (after parsing to JSON values) against those schemas in `cargobike validate` | |

### A.8 Templates, Expressions and File Edits

| Crate | Use in Cargobike | Notes |
|---|---|---|
| `cel` | CEL gates and `${{ … }}` expressions (F-27) | Renamed from `cel-interpreter`. Register only pure functions; bound cost, regex and comprehensions |
| `semver` | The `semver` version scheme: parsing, pre-release detection, ordering for F-72 | By dtolnay, used by Cargo itself |
| `globset` | Repo, ref, path and claim patterns (F-92, F-49) | From ripgrep. Globs match the whole string, avoiding the unanchored-regex pitfall |
| `regex` | Where regexes remain (version grammars, `commit-files` regex edits) | Linear-time matching, so no ReDoS |
| `relative-path` | Repository-relative paths in `commit-files` edits | Normalise, reject `..`, then match against the registry's path globs (F-41) |
| `toml_edit` | Format- and comment-preserving TOML edits | The same crate Cargo uses for `cargo add` |
| `serde_json` (`pointer_mut`) | JSON edits by dot-notation path (F-41) | |
| `yamlpatch` **(evaluate)** | Comment- and format-preserving YAML patch operations (replace, add, remove) | Built for the zizmor GitHub Actions security tool; purpose-built for surgical edits |
| `yaml-edit` **(evaluate)** | Alternative lossless YAML editor built on a `rowan` syntax tree | Younger than `yamlpatch` |
| `similar` | Unified diffs in `cargobike plan` output (US-13) | Also what `insta` uses internally |

Format preservation matters more than it seems: a change request that rewrites a whole YAML manifest (dropping comments and reordering keys) is much harder for a human to review, and reviewability is part of the approval model (F-96). There is no long-established, format-preserving YAML editor in Rust yet, so prototype `yamlpatch` and `yaml-edit` against real manifests early, alongside the DBOS spike.

### A.9 Extensions (WASM and Sidecar)

| Crate / tool | Use in Cargobike | Notes |
|---|---|---|
| `wasmtime` | Host runtime: component model, `bindgen!` for host bindings, epoch interruption, fuel, `ResourceLimiter` (§7.4) | Also module precompilation/serialisation for cold start |
| `wasmtime-wasi` | Minimal WASI context, granting nothing by default | |
| `wit-bindgen` | Guest bindings in `cargobike-extension-sdk` | |
| `wasm-tools` (CLI) | Validate and inspect components in CI | |
| `tonic` + `prost` | gRPC transport for out-of-process (sidecar) extensions (§7.3) | Alternatively plain JSON over HTTP with axum/reqwest, which is easier for Python or Go authors |

### A.10 Observability

| Crate | Use in Cargobike | Notes |
|---|---|---|
| `tracing`, `tracing-subscriber` (`env-filter`, `json`) | Structured logs | Put `release_id`, `workflow_id` and `environment` on spans so every log line is correlated |
| `tracing-opentelemetry`, `opentelemetry`, `opentelemetry-otlp` | Optional distributed traces via OTLP | Keep behind a feature to protect binary size |
| `metrics` + `metrics-exporter-prometheus` | Prometheus metrics: releases by phase, step durations, webhook lag, reconciler corrections | |

### A.11 CLI Experience

| Crate | Use in Cargobike | Notes |
|---|---|---|
| `comfy-table` or `tabled` | `-o table` output (F-108) | |
| `indicatif` | Progress display for `watch` / `--wait` | Disable automatically when stdout isn't a terminal (CI logs) |
| `anstream`, `anstyle` | Colour that respects `NO_COLOR` and non-TTY output | Already used by clap |
| `reqwest-eventsource` or `eventsource-stream` | SSE client for `watch`, with `Last-Event-ID` reconnect (F-102) | |
| `miette` | Rich `cargobike validate` diagnostics pointing at the exact line and column in a YAML file | Pairs well with parsers that report spans |
| `dialoguer` | Confirmation prompts (e.g. `release delete`), skipped with `--yes` and in non-interactive shells | |

### A.12 Testing

| Crate / tool | Use in Cargobike | Notes |
|---|---|---|
| `rstest`, `pretty_assertions`, `wiremock` | Already in the PRD (§14) | |
| `testcontainers` (+ `testcontainers-modules`, Postgres) | Real Postgres for DBOS and sqlx integration tests; crash-injection harness (T1) | Run the same tests locally and in CI without a pre-provisioned database |
| `insta` | Snapshot tests for CLI tables, RFC 9457 bodies, OpenAPI output and `plan` diffs | Makes unintended output changes visible in review |
| `proptest` | Property tests for version ordering, glob/path confinement, CEL gates (T3) | |
| `assert_cmd` + `predicates` | End-to-end CLI tests, including exit codes (US-4) | |
| `cargo-nextest` | Faster, better-isolated test runs in CI | |
| `cargo-llvm-cov` | Coverage (§14.4) | Already in the PRD |

### A.13 Build, Release and Supply Chain

| Tool | Use in Cargobike | Notes |
|---|---|---|
| `cargo-deny` | Licence policy, duplicate-version bans (catches two sqlx versions), advisories, allowed sources | Covers everything `cargo audit` does and more |
| `cargo-audit` | RustSec advisory checks (already in `make ci`) | Can be dropped if `cargo-deny` is adopted |
| `release-plz` | Conventional-commit-driven version bumps, changelogs, tags and crates.io publishing (G-11, §15.3) | Opens a release PR you merge, which suits the project's own workflow |
| `git-cliff` | Changelog generation from conventional commits | `release-plz` can use it for changelog formatting |
| `cargo-dist` | Multi-platform binaries, installers, Homebrew tap and GitHub Releases | Active project with built-in support for GitHub Artifact Attestations |
| `cargo-auditable` | Embeds the dependency list into release binaries so they can be scanned later | Supported by `cargo-dist` |
| `cargo-cyclonedx` | SBOM generation | |
| `cargo-semver-checks` | Catches accidental breaking changes in public crates: `cargobike-core`, `cargobike-api`, `cargobike-extension-sdk` | Backs the stability promises in §20.3 |
| `cargo-machete` | Finds unused dependencies | Helps the CLI < 10 MB target |
| `cargo-zigbuild` or `cross` | Cross-compiling linux/arm64 and static musl builds | |

### A.14 Per-Crate Dependency Map

Keeping heavy dependencies out of the wrong crates is what makes the binary-size targets and the "CLI has no database dependencies" goal (G-10) achievable.

| Crate | Key dependencies | Must **not** depend on |
|---|---|---|
| `cargobike-core` | serde, serde_json, thiserror, time, uuid, semver, globset, strum | tokio, sqlx, reqwest, wasmtime (must build for `wasm32-wasip2`) |
| `cargobike-api` | core, serde, utoipa, reqwest (client, behind a feature) | sqlx, dbos, wasmtime |
| `cargobike-engine` | core, dbos, sqlx, cel, toml_edit, YAML editor, similar, async-trait, tokio-util | axum, api |
| `cargobike-server` | core, api, engine, axum, tower-http, tower_governor, jsonwebtoken, moka, argon2, hmac/sha2, subtle, secrecy, figment, metrics, wasmtime (feature `wasm`) | — |
| `cargobike-cli` | core, api, clap, reqwest, reqwest-eventsource, comfy-table/tabled, indicatif, miette, directories | sqlx, dbos, wasmtime, axum |
| `cargobike-extension-sdk` | core, wit-bindgen, serde | tokio, reqwest (guests use the host `http` interface) |
| `cargobike-provider-github` | core, octocrab, graphql_client, reqwest | extension-sdk, dbos |

`cargo-deny` can enforce the "must not" column with its `bans` configuration.

### A.15 Avoid

| Avoid | Reason | Use instead |
|---|---|---|
| `serde_yml` | Flagged by RUSTSEC-2025-0068 as unsound and unmaintained | `serde_yaml_ng` |
| `serde_yaml` | Archived by its author | `serde_yaml_ng` |
| `cel-interpreter` (old name) | Renamed upstream | `cel` |
| `openssl` / `native-tls` | Complicates static and cross-platform builds | `rustls` |
| `lazy_static`, `once_cell` | Superseded by the standard library | `std::sync::LazyLock` / `OnceLock` |
| Mixing `chrono` and `time` | Two date-time libraries in one tree | `time`, matching `dbos` |
| A second sqlx major version | Duplicate pools; no shared transactions with DBOS | The sqlx version `dbos` uses |

### A.16 Release Profile

```toml
[profile.release]
strip = true
lto = true
codegen-units = 1
```

### A.17 Sources

- RustSec &mdash; RUSTSEC-2025-0068 (`serde_yml`) and recommended alternatives: https://rustsec.org/advisories/RUSTSEC-2025-0068.html
- DBOS Transact for Rust, crate documentation and dependencies: https://docs.rs/dbos/latest/dbos/
- `cargo-dist` releases (GitHub Artifact Attestations support): https://github.com/axodotdev/cargo-dist/releases/tag/v0.32.0
- `yaml-edit` documentation: https://docs.rs/yaml-edit
- GitHub &mdash; Handling failed webhook deliveries (10-second response limit): https://docs.github.com/webhooks/using-webhooks/handling-failed-webhook-deliveries

---

## Appendix B: Glossary

| Term | Definition |
|------|-----------|
| **Release** | A request to promote a version of an application through environments |
| **Workflow** | A durable DBOS process that drives a release |
| **Pipeline template** | A YAML file defining environments, steps, and gates. Reusable across applications via typed inputs |
| **Step type** | A named, versioned operation. Control steps are interpreter-native; action steps run in `dbos::step` |
| **Control step** | Interpreter-native step: `wait: merge`, `wait: approval`, `wait: sleep`. Not extensible. Uses `wait:` keyword in templates |
| **Action step** | Extensible step running in `dbos::step`: `commit-files`, `change-request`, `http-call` |
| **Step context** | Provider registry, credential store, HTTP client, idempotency key, cancellation token, logger |
| **Snapshot** | The resolved template, registry inputs, and step-type versions stored in a release at creation time |
| **Environment** | A named stage in the pipeline (e.g., "preview", "production") |
| **Gate** | A CEL condition controlling whether an environment is deployed to |
| **Provider** | A git platform implementation implementing `Provider` |
| **Sidecar** | An out-of-process extension over HTTP/gRPC |
| **Change request** | Neutral term for PR (GitHub) or MR (GitLab) |
| **Merge verification** | Server-side provider API check that a CR is merged and the intended content is present on the base branch |
| **OIDC trust entry** | A mapping from OIDC claims to grants (issuer, audience, claim patterns -> permissions) |
| **Application registry** | Server-side declarations of applications, their sources, templates, edits, releasers, and versioning |
| **Lease row** | A DB row `(app, env) -> release_id` for serialising concurrent releases. Released per-environment on completion/skip/supersede, or by cleanup on cancel |
| **Supersession** | A new release closing the CRs of an older release for the same app + environment |
| **Skipped** | An environment phase: gate false or no changes needed |
| **Waiting** | An environment phase: held by the concurrency policy |
| **DBOS** | Database-Backed Durable Workflows |
| **WIT** | WebAssembly Interface Type |
| **CEL** | Common Expression Language |
| **Fork** | A DBOS operation that creates a workflow inheriting recorded step results |
| **CiContext** | Generic CI context (provider, repository, repository_id, workflow_ref, run_id, run_url) |
| **RepoRef** | Opaque repository reference: `{provider, id}` where `id` is a string (the immutable provider ID). `path` is fetched from the provider at runtime |
