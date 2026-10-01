# Cargobike &mdash; User-Facing Examples

> _Durable delivery, one pedal at a time._

This document shows full examples of every configuration file, CLI command,
and API interaction that a user of Cargobike encounters. It is the reference
for anyone setting up, operating, or integrating with Cargobike.

---

## 1. Server Configuration

File: `/etc/cargobike/config.yaml` (override with `--config` or
`CARGOBIKE_SERVER_CONFIG`)

```yaml
# ─────────────────────────────────────────────
# Cargobike Server Configuration
# ─────────────────────────────────────────────

server:
  listen: "0.0.0.0:8080"
  public_url: "https://cargobike.internal"
  shutdown_timeout: 30s

database:
  # Secret-shaped (R5): a literal string, { file: ... }, { env: ... } or
  # { secret: <name> }. A literal string is accepted but warned at startup.
  url: { file: /run/secrets/cargobike-db-url }
  max_connections: 10

# ── Leader election ─────────────────────────
# One section (F-136). The leader-election connection must bypass PgBouncer
# transaction pooling (F-131), so it has its own URL.
leader_election:
  enabled: true
  database_url: { file: /run/secrets/cargobike-db-direct-url }

# ── Authentication ──────────────────────────
auth:
  oidc:
    - name: gha-my-org
      issuer: https://token.actions.githubusercontent.com
      audience: cargobike
      claims:
        repository_owner_id: "123456"   # quoted string (R6)
        ref: "refs/tags/v*"             # glob (R7)
      grants: [release:create, release:read]

    - name: azure-ad
      issuer: https://login.microsoftonline.com/00000000-0000-0000-0000-000000000000/v2.0
      audience: api://cargobike
      claims:
        roles: [release-managers]
      grants: [release:read, release:approve, release:cancel]

  api_keys:
    - name: ci-prod
      description: "CI production key"
      hash: "$argon2id$v=19$m=32768,t=3,p=4$ZGVwVr4dImL0T3vH..."
      grants: [release:create]
      expires: 2027-06-30               # optional (date)

    - name: admin-key
      description: "Admin key"
      hash: "$argon2id$v=19$m=32768,t=3,p=4$AbC123dEfG456hIj..."
      grants: ["*"]

# ── Named secrets ───────────────────────────
# Maps a name to where the value comes from ({ file } or { env }).
# Template steps reference entries as { secret: <name> } (F-146).
secrets:
  slack-webhook-token: { file: /run/secrets/slack-token }

# ── Providers ──────────────────────────────
providers:
  - name: github
    type: github
    api_url: https://api.github.com
    web_url: https://github.com
    auth:
      app_id: "123456"
      private_key:
        file: /etc/cargobike/keys/github-app.pem
    repositories:
      allow: ["my-org/*"]
      deny: ["my-org/*-test"]

  - name: github-enterprise
    type: github
    api_url: https://github.enterprise.internal/api/v3
    web_url: https://github.enterprise.internal
    auth:
      app_id: "789012"
      private_key:
        file: /etc/cargobike/keys/ghes-app.pem
    repositories:
      allow: ["enterprise/*"]

  - name: gitlab-internal
    type: extension
    extension: gitlab-internal
    webhook_secrets:
      - file: /run/secrets/gitlab-webhook-secret
    repositories:
      allow: ["gitlab-org/**"]     # ** crosses nested GitLab group segments (R7)

# ── Extensions ─────────────────────────────
extensions:
  - name: gitlab-internal
    endpoint: http://localhost:8081
    transport: json
    auth:
      shared_secret:
        file: /etc/cargobike/keys/sidecar-extension-secret
    provides:
      step_types: []

# ── Templates ─────────────────────────────
templates:
  directory: "/etc/cargobike/templates"

# ── Reconciler ─────────────────────────────
reconciler:
  interval: 5m
  batch_size: 100

# ── Retention ─────────────────────────────
retention:
  events: 365d
  webhook_payloads: 30d

# ── Limits ─────────────────────────────────
# Rate grammar: <count>/<s|m|h>. Rate-limit key: per client IP for
# unauthenticated endpoints, per authenticated principal otherwise (F-118).
limits:
  api:
    max_body_size: 1MiB
    rate: 100/s
    burst: 200
  webhooks:
    max_body_size: 25MiB
    rate: 50/s
    burst: 100

# ── Network / SSRF protection ───────────────
# network.egress.deny blocks addresses for the http-call step and provider
# calls (F-117). Cargobike resolves DNS itself, checks every resolved address,
# connects to the checked IP, and re-checks on every redirect.
network:
  egress:
    # Built-in deny-list, shown here for reference. Omit `deny` to get the
    # defaults; extend it to block more.
    deny:
      - 169.254.0.0/16   # link-local
      - 127.0.0.0/8      # loopback
      - 10.0.0.0/8       # RFC 1918
      - 172.16.0.0/12    # RFC 1918
      - 192.168.0.0/16   # RFC 1918
      - ::1/128          # IPv6 loopback
      - fc00::/7         # IPv6 unique-local
    # allow: ["192.0.2.10/32"]            # permit-list exceptions
    # proxy: socks5://proxy.internal:1080 # explicit egress proxy (SSRF check
    #                                     # is applied to the host before proxying)

# ── Engine ──────────────────────────────────
engine:
  max_step_output: 1MiB
  branch_format: "cargobike/{application}/{environment}/{release_id}"

# ── Logging ────────────────────────────────
# Precedence: RUST_LOG (per-module) > CARGOBIKE_SERVER_LOG_LEVEL (single
# level) > logging.level > default (F-140).
logging:
  level: info
  format: json

# ── Application Registry ──────────────────
applications:
  - name: my-service
    description: Customer-facing API
    labels: { team: payments }
    source:
      provider: github
      id: "123456"
      path: my-org/my-service         # verified label (F-10): checked at startup
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
    inputs:                             # app-wide inputs -> ${{ inputs.<name> }} (F-32a)
      notify_channel: "#payments-releases"
    environments:
      preview:
        repo:
          provider: github
          id: "789012"
          path: my-org/deploy
        edits:
          - file: apps/my-service/preview.yaml
            field: image.tag
      production:
        repo:
          provider: github
          id: "789012"
          path: my-org/deploy
        edits:
          - file: apps/my-service/prod.yaml
            field: image.tag
        concurrency: supersede        # security-relevant: registry-only (R11)
        require_branch_protection: true
        allow_direct_commit: false
        change_request:
          labels: [release, production]   # appended to template labels (R11)
        approval:
          required: 1
          allow_self_approval: false
          approvers:
            - oidc: azure-ad
              claims:
                roles: [release-managers]

  - name: other-service
    source:
      provider: github-enterprise
      id: "654321"
      path: enterprise/other-service
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
    environments:
      staging:
        repo:
          provider: github-enterprise
          id: "111111"
          path: enterprise/deploy
        edits:
          - file: apps/other-service/staging.yaml
            field: image.tag
      production:
        repo:
          provider: github-enterprise
          id: "111111"
          path: enterprise/deploy
        edits:
          - file: apps/other-service/prod.yaml
            field: image.tag
        concurrency: queue
        require_branch_protection: true
        approval:
          required: 2
          allow_self_approval: false
          approvers:
            - oidc: azure-ad
              claims:
                roles: [release-managers, senior-release-managers]
```

### Server Environment Variables

All values have three sources: CLI flag > `CARGOBIKE_SERVER_*` env var >
config file > default (F-125).

| Variable | Config key | Description |
|----------|-----------|-------------|
| `CARGOBIKE_SERVER_CONFIG` | (file path) | Path to the YAML config file |
| `CARGOBIKE_SERVER_DATABASE_URL` | `database.url` | PostgreSQL connection string |
| `CARGOBIKE_SERVER_DATABASE_URL_FILE` | `database.url` | Same, from a file |
| `CARGOBIKE_SERVER_LISTEN` | `server.listen` | Bind address (R10: `--listen` ↔ `CARGOBIKE_SERVER_LISTEN`) |
| `CARGOBIKE_SERVER_PUBLIC_URL` | `server.public_url` | Externally visible URL (webhooks, CR links, `Location`) |
| `CARGOBIKE_SERVER_GITHUB_APP_ID` | `providers[github].auth.app_id` | GitHub App ID (single-provider shortcut) |
| `CARGOBIKE_SERVER_GITHUB_PRIVATE_KEY_FILE` | `providers[github].auth.private_key.file` | GitHub App private key path |
| `CARGOBIKE_SERVER_GITHUB_API_URL` | `providers[github].api_url` | GitHub API URL |
| `CARGOBIKE_SERVER_GITHUB_WEBHOOK_SECRET_FILE` | `providers[github].webhook_secrets[0].file` | GitHub webhook secret path |
| `CARGOBIKE_SERVER_BOOTSTRAP_API_KEY` | (bootstrap) | Plaintext admin key, hashed at startup, localhost-only |
| `CARGOBIKE_SERVER_BOOTSTRAP_API_KEY_FILE` | (bootstrap) | Same, from a file |
| `CARGOBIKE_SERVER_LOG_LEVEL` | `logging.level` | Single log level (`RUST_LOG` takes precedence, F-140) |
| `CARGOBIKE_SERVER_LOG_FORMAT` | `logging.format` | `json` or `text` |

`${VAR}` interpolation in the config file fails fast on unset variables.
`${VAR:-default}` provides a fallback. The registry's `{version}` is a
format-string placeholder, not `${{ }}` or `${}`. Interpolation is
forbidden inside secret-valued fields (R5) &mdash; use `{ file }`, `{ env }`
or `{ secret }` there.

---

## 2. CLI Configuration

File: `~/.config/cargobike/config.yaml` (override with `CARGOBIKE_CONFIG`)

The CLI config file defines **named contexts**: one per Cargobike server
you talk to (§9.7). Schema:

```yaml
current_context: production
contexts:
  - name: production
    url: https://cargobike.example.com
    ca_file: /etc/ssl/corp-ca.pem          # optional custom CA bundle
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

The command must print **only the token on stdout** (TSV), which is why
`az` is called with `--query accessToken -o tsv`. The token is never logged.

Precedence: flag > environment variable > selected context > `defaults` >
built-in default (F-145).

### 2.1 Minimal (API key, env-only)

No config file is required for a single-context, CI-style setup:

```bash
export CARGOBIKE_URL=https://cargobike.example.com
export CARGOBIKE_API_KEY_FILE=~/.config/cargobike/local-key
```

or one context in the file:

```yaml
contexts:
  - name: default
    url: https://cargobike.example.com
    auth:
      type: api-key
      api_key: { file: ~/.config/cargobike/local-key }
current_context: default
```

### 2.2 GitHub Actions (no config file)

In GitHub Actions no file is needed. The CLI sets `CARGOBIKE_URL` and
auto-detects auth type `github-actions` when the Actions OIDC variables
(`ACTIONS_ID_TOKEN_REQUEST_URL`, `ACTIONS_ID_TOKEN_REQUEST_TOKEN`) are
present, requesting an audience `cargobike` ID token:

```yaml
permissions:
  id-token: write     # required for the OIDC exchange
  contents: read
env:
  CARGOBIKE_URL: https://cargobike.example.com
```

Override the audience with `--audience` / `CARGOBIKE_AUDIENCE`.

The CLI auto-detects `GITHUB_REPOSITORY`, `GITHUB_SHA`, and
`GITHUB_RUN_ID` from the CI environment and sends them as `CiContext`.

### 2.3 Exec plugin (Entra ID, in a context)

```yaml
contexts:
  - name: production
    url: https://cargobike.example.com
    auth:
      type: exec
      command: az
      args: [account, get-access-token, --scope, "api://cargobike/.default",
             --query, accessToken, -o, tsv]
current_context: production
```

### 2.4 Full example

```yaml
current_context: production
defaults:
  output: table       # table | json | yaml
  timeout: 30s        # default per-request timeout
contexts:
  - name: production
    url: https://cargobike.example.com
    ca_file: /etc/ssl/corp-ca.pem
    auth:
      type: exec
      command: az
      args: [account, get-access-token, --scope, "api://cargobike/.default",
             --query, accessToken, -o, tsv]
  - name: local
    url: http://localhost:8080
    auth:
      type: api-key
      api_key: { file: ~/.config/cargobike/local-key }
```

The CLI refuses `http://` server URLs except for localhost
(`--allow-http` override).

### CLI Environment Variables

| Variable | Config key | Description |
|----------|-----------|-------------|
| `CARGOBIKE_CONFIG` | (file path) | Path to the CLI config file |
| `CARGOBIKE_CONTEXT` | `current_context` | Context to use (`--context`) |
| `CARGOBIKE_URL` | `contexts[].url` | Server URL (`--url`) |
| `CARGOBIKE_API_KEY` | `auth.api_key` | API key value (type `api-key`) |
| `CARGOBIKE_API_KEY_FILE` | `auth.api_key` | API key from a file |
| `CARGOBIKE_AUTH` | `auth.type` | Auth type override: `none`, `api-key`, `exec`, `github-actions` |
| `CARGOBIKE_AUDIENCE` | (OIDC audience) | Audience for `github-actions` tokens (default `cargobike`) |
| `CARGOBIKE_CA_FILE` | `contexts[].ca_file` | Custom CA bundle |
| `CARGOBIKE_OUTPUT` | `defaults.output` | Default output format |

The CLI honours `NO_COLOR`, `XDG_CONFIG_HOME`, and `OTEL_*` variables as
standard (F-130).

---

## 3. Pipeline Template

File: `/etc/cargobike/templates/service.yaml`

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
  edits:
    type: edits

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
          title: "deploy: ${{ release.version }}"
          body: |
            Automated deployment for ${{ release.version }}.

            Release ID: ${{ release.id }}
          labels: [automated-deploy]
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

### Template with `http-call` and `set-labels`

The Slack-style webhook credential lives in the server config's `secrets:`
section (see §1) and is referenced as `{ secret: <name> }` — the value is
resolved inside the engine and never enters a step output (F-120, F-146).

```yaml
name: service-with-notify
version: "1"
inputs: {}
environment_inputs:
  repo: { type: repo }
  edits: { type: edits }

step_groups:
  - name: deploy
    steps:
      - id: edit
        uses: builtin/commit-files@1
        with:
          repo: ${{ env.inputs.repo }}
          edits: ${{ env.inputs.edits }}
      - id: cr
        uses: builtin/change-request@1
        with:
          branch: ${{ steps.edit.outputs.branch }}
      - uses: builtin/set-labels@1
        with:
          repo: ${{ env.inputs.repo }}
          cr_number: ${{ steps.cr.outputs.number }}
          labels: [pending-review, automated-deploy]
      - uses: builtin/http-call@1
        with:
          url: https://release-hooks.internal/slack
          method: POST
          headers:
            Authorization: { secret: slack-webhook-token }   # resolved in-engine
          body:
            text: "New deployment CR opened: ${{ steps.cr.outputs.url }}"
          expect_status: 200
        timeout: 30s
        retry: { attempts: 3, backoff: exponential, max_delay: 1m }
      - wait: merge
        timeout: 7d
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

---

## 4. Sidecar Extension

### 4.1 Sidecar Server (HTTP/JSON)

A sidecar provider implements the Cargobike provider interface over HTTP.
Below is an example contract for a GitLab sidecar.

#### `GET /capabilities`

```json
{
  "auto_merge": false,
  "tags": true,
  "releases": true,
  "branch_protection": true,
  "webhook_verification": true,
  "webhook_parsing": true
}
```

#### `POST /create-branch`

```
POST /create-branch
X-Cargobike-Secret: <extension-secret>

{
  "repo": { "provider": "gitlab-internal", "id": "42" },
  "branch": "cargobike/my-service/preview/0192f0d0-...",
  "from_sha": "abc123def456"
}
```

Response: `200 OK` (empty body on success)

#### `POST /commit-files`

```
POST /commit-files
X-Cargobike-Secret: <extension-secret>

{
  "repo": { "provider": "gitlab-internal", "id": "42" },
  "branch": "cargobike/my-service/preview/0192f0d0-...",
  "edits": [
    { "file": "apps/my-service/preview.yaml", "field": "image.tag" }
  ],
  "message": "deploy: 1.2.3",
  "expected_parent": "abc123def456"
}
```

Response:

```json
{
  "sha": "new-commit-sha",
  "branch": "cargobike/my-service/preview/0192f0d0-..."
}
```

#### `POST /create-change-request`

```
POST /create-change-request
X-Cargobike-Secret: <extension-secret>

{
  "repo": { "provider": "gitlab-internal", "id": "42" },
  "head": "cargobike/my-service/preview/0192f0d0-...",
  "base": "main",
  "title": "deploy: 1.2.3",
  "body": "Automated deployment for 1.2.3.",
  "labels": ["automated-deploy"]
}
```

Response:

```json
{
  "number": 42,
  "url": "https://gitlab.internal/gitlab-org/my-repo/-/merge_requests/42",
  "head_sha": "new-commit-sha",
  "state": "open"
}
```

#### Other Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/capabilities` | Provider capability discovery |
| `POST` | `/create-branch` | Create a branch from a SHA |
| `POST` | `/commit-files` | Apply edits and commit |
| `POST` | `/create-change-request` | Open a CR (PR/MR) |
| `GET` | `/change-request/{repo_id}/{number}` | Get a CR by number |
| `GET` | `/change-request-by-head/{repo_id}?head=...` | Find an open CR by head branch |
| `POST` | `/close-change-request/{repo_id}/{number}` | Close a CR with a comment |
| `GET` | `/open-change-requests/{repo_id}` | List open CRs |
| `GET` | `/file/{repo_id}?path=...&ref=...` | Read a file at a ref |
| `POST` | `/comment/{repo_id}/{cr_number}` | Comment on a CR |
| `POST` | `/add-labels/{repo_id}/{cr_number}` | Add labels to a CR |
| `POST` | `/update-branch/{repo_id}` | Merge base into head branch |
| `GET` | `/default-branch/{repo_id}` | Get the default branch name |
| `GET` | `/branch-sha/{repo_id}?branch=...` | Get the SHA of a branch tip |
| `POST` | `/set-commit-status/{repo_id}` | Set a commit status |
| `GET` | `/branch-protection/{repo_id}?branch=...` | Check branch protection |
| `GET` | `/tag-protection/{repo_id}?pattern=...` | Check tag protection |
| `POST` | `/verify-webhook` | Verify webhook signature and parse |
| `POST` | `/parse-webhook` | Parse webhook payload (no verification) |

All requests include `X-Cargobike-Secret: <extension-secret>` for channel
authentication.

### 4.2 Sidecar in Server Config

```yaml
providers:
  - name: gitlab-internal
    type: extension
    extension: gitlab-internal
    webhook_secrets:
      - file: /run/secrets/gitlab-webhook-secret
    repositories:
      allow: ["gitlab-org/**"]     # ** crosses nested GitLab group segments (R7)

extensions:
  - name: gitlab-internal
    endpoint: http://localhost:8081
    transport: json
    auth:
      shared_secret:
        file: /etc/cargobike/keys/sidecar-extension-secret
    provides:
      step_types: []
```

---

## 5. API Specification

### 5.1 Error Response (RFC 9457 Problem Details)

All errors use this format:

```json
{
  "type": "https://cargobike.dev/errors/release-not-found",
  "title": "Not Found",
  "status": 404,
  "detail": "The release `0192f0d0-...` was not found.",
  "instance": "/api/v1/releases/0192f0d0-...",
  "code": "ReleaseNotFound"
}
```

Error codes:

| Code | Status | Description |
|------|--------|-------------|
| `InvalidRequest` | 400 | Malformed or invalid request |
| `FieldRequired` | 400 | A required field is missing |
| `FieldInvalid` | 400 | A field has an invalid value |
| `MissingToken` | 401 | No auth token provided |
| `InvalidToken` | 401 | Token validation failed |
| `TokenExpired` | 401 | Token has expired |
| `ForbiddenResource` | 403 | Permission denied |
| `ApprovalForbidden` | 403 | Caller does not satisfy approval policy |
| `ApplicationNotFound` | 404 | Application not in registry |
| `ReleaseNotFound` | 404 | Release ID not found |
| `TemplateNotFound` | 404 | Template not found |
| `StepTypeNotFound` | 400 | Step type not in registry |
| `StateConflict` | 409 | Request conflicts with current state |
| `MergeNotVerified` | 409 | CR was not merged |
| `ConcurrencyRejected` | 409 | Concurrency policy is `reject` |
| `DuplicateRelease` | 409 | Release for this app+version is already active |
| `ChangeRequestModified` | 409 | CR head SHA changed and `on_modified: fail` |
| `VersionNotVerified` | 400 | Version does not match tag/SHA |
| `InternalError` | 500 | Unexpected server error |

### 5.2 Create Release

```
POST /api/v1/releases
Authorization: Bearer <token>
Content-Type: application/json
Idempotency-Key: optional-uuid

{
  "application": "my-service",
  "version": "1.2.3"
}
```

Response: `202 Accepted`

```
Location: /api/v1/releases/0192f0d0-7c5e-7f2a-a3b4-5e6f7a8b9c0d
Content-Type: application/json

{
  "metadata": {
    "id": "0192f0d0-7c5e-7f2a-a3b4-5e6f7a8b9c0d",
    "created_at": "2026-09-30T12:00:00Z",
    "updated_at": "2026-09-30T12:00:00Z",
    "resource_version": 1,
    "retried_from": null,
    "labels": {},
    "annotations": {}
  },
  "spec": {
    "application": "my-service",
    "version": "1.2.3",
    "source": {
      "provider": "github",
      "id": "123456",
      "path": "my-org/my-service"
    },
    "template": "service@1"
  },
  "status": {
    "phase": "Pending",
    "actor": {
      "issuer": "https://token.actions.githubusercontent.com",
      "subject": "repo:my-org/my-service:ref:refs/tags/v1.2.3",
      "display_name": "my-org/my-service"
    },
    "ci": {
      "provider": "github",
      "repository": "my-org/my-service",
      "repository_id": "123456",
      "workflow_ref": ".github/workflows/release.yml@refs/tags/v1.2.3",
      "run_id": "9876543210",
      "run_url": "https://github.com/my-org/my-service/actions/runs/9876543210"
    },
    "workflow": {
      "id": "cargobike.interpret.v1",
      "template_name": "service",
      "template_version": "1",
      "template_hash": "sha256:abc123...",
      "step_type_versions": {
        "builtin/commit-files": "1",
        "builtin/change-request": "1"
      }
    },
    "error": null,
    "environments": [],
    "attempts": [
      {
        "workflow_id": "cargobike.interpret.v1-0192f0d0...",
        "started_at": "2026-09-30T12:00:00Z",
        "fork_from": null
      }
    ]
  }
}
```

Duplicate create (same `application` + `version`, non-terminal existing):
returns `200 OK` with the existing release (same body).

### 5.3 List Releases

```
GET /api/v1/releases?application=my-service&limit=20&after=0192f0d0-...
Authorization: Bearer <token>
```

Response: `200 OK`

```json
{
  "items": [ { "metadata": {...}, "spec": {...}, "status": {...} } ],
  "cursor": "0192f0d1-...",
  "has_more": true
}
```

Query parameters:

| Param | Type | Description |
|-------|------|-------------|
| `application` | string | Filter by application name |
| `phase` | string | Filter by phase |
| `version` | string | Filter by version |
| `since` | string | Filter by timestamp |
| `limit` | int (1-500) | Page size (default 50) |
| `after` | string | Cursor for next page |
| `before` | string | Cursor for previous page |

### 5.4 Get Release

```
GET /api/v1/releases/0192f0d0-7c5e-7f2a-a3b4-5e6f7a8b9c0d
Authorization: Bearer <token>
```

Response: `200 OK` with the full release JSON (same shape as create response,
but with updated status).

### 5.5 Delete Release

```
DELETE /api/v1/releases/0192f0d0-7c5e-7f2a-a3b4-5e6f7a8b9c0d
Authorization: Bearer <token>
If-Match: 42
```

Response: `204 No Content`

Only allowed for releases in a terminal phase (`Completed`, `Failed`,
`Canceled`, `Superseded`). Requires the `release:delete` grant (F-99). The
event log is retained per the retention policy.

### 5.6 Cancel Release

```
POST /api/v1/releases/0192f0d0-7c5e-7f2a-a3b4-5e6f7a8b9c0d/cancel
Authorization: Bearer <token>
```

Response: `204 No Content`

Calls `DBOS::cancel`, marks the release as `Canceled`, records the event,
and starts `cargobike.cleanup.v1` to close open CRs, delete branches, and
release/transfer leases.

### 5.7 Retry Release

```
POST /api/v1/releases/0192f0d0-7c5e-7f2a-a3b4-5e6f7a8b9c0d/retry
Authorization: Bearer <token>
```

Response: `202 Accepted` with the release JSON.

Forks the workflow from `ForkFrom::LastFailure`. The release keeps the same
ID; a new attempt is appended to `status.attempts[]`.

With `--new`:

```
POST /api/v1/releases/0192f0d0-7c5e-7f2a-a3b4-5e6f7a8b9c0d/retry?new=true
Authorization: Bearer <token>
```

Creates a new release with `metadata.retried_from` set to the original ID.
Only allowed when the original release is terminal.

### 5.8 Submit Approval

```
POST /api/v1/releases/0192f0d0-7c5e-7f2a-a3b4-5e6f7a8b9c0d/approvals
Authorization: Bearer <token>
Content-Type: application/json

{
  "environment": "production",
  "step_id": "approval",
  "decision": "approve"
}
```

Response: `204 No Content`

Only accepted while the environment is `PendingApproval` at the specified
step for the current attempt. The caller must satisfy the approval policy
(`allow_self_approval: false` by default; `api_key` callers are rejected
unless `allow_machine_approvers: true`). The decision is bound to the
current attempt and the CR's head SHA.

`decision` may be `approve` or `reject`. A single `reject` fails the
environment immediately.

### 5.9 Watch Release (SSE)

```
GET /api/v1/releases/0192f0d0-7c5e-7f2a-a3b4-5e6f7a8b9c0d/watch
Authorization: Bearer <token>
Accept: text/event-stream
```

Response: `200 OK` with SSE stream:

```
id: 42
event: phase-change
data: {"phase": "Running"}

id: 43
event: environment-started
data: {"environment": "preview", "phase": "Running"}

id: 44
event: environment-completed
data: {"environment": "preview", "phase": "Completed", "change_request": {"number": 42, "url": "https://github.com/my-org/deploy/pull/42"}}

id: 45
event: environment-started
data: {"environment": "production", "phase": "PendingApproval"}

id: 46
event: environment-completed
data: {"environment": "production", "phase": "Completed"}

id: 47
event: phase-change
data: {"phase": "Completed"}
```

Reconnect with `Last-Event-ID: 44` to resume from event 45.

### 5.10 Get Events

```
GET /api/v1/releases/0192f0d0-7c5e-7f2a-a3b4-5e6f7a8b9c0d/events
Authorization: Bearer <token>
```

Response: `200 OK`

```json
{
  "items": [
    {
      "id": "cargobike.interpret.v1-0192f0d0.../set-status",
      "release_id": "0192f0d0-7c5e-7f2a-a3b4-5e6f7a8b9c0d",
      "sequence": 1,
      "timestamp": "2026-09-30T12:00:00Z",
      "actor": {
        "issuer": "https://token.actions.githubusercontent.com",
        "subject": "repo:my-org/my-service:ref:refs/tags/v1.2.3",
        "display_name": "my-org/my-service"
      },
      "type": "ReleaseCreated",
      "reason": "Created via POST /api/v1/releases",
      "data": null
    },
    {
      "id": "cargobike.interpret.v1-0192f0d0.../set-status/1",
      "release_id": "0192f0d0-7c5e-7f2a-a3b4-5e6f7a8b9c0d",
      "sequence": 2,
      "timestamp": "2026-09-30T12:00:01Z",
      "actor": { "issuer": "system", "subject": "cargobike", "display_name": "Cargobike" },
      "type": "EnvironmentStarted",
      "reason": "preview",
      "data": { "environment": "preview" }
    },
    {
      "id": "cargobike.interpret.v1-0192f0d0.../set-status/2",
      "release_id": "0192f0d0-7c5e-7f2a-a3b4-5e6f7a8b9c0d",
      "sequence": 3,
      "timestamp": "2026-09-30T12:30:00Z",
      "actor": { "issuer": "system", "subject": "cargobike", "display_name": "Cargobike" },
      "type": "EnvironmentCompleted",
      "reason": "preview merged",
      "data": { "environment": "preview", "cr_number": 42 }
    },
    {
      "id": "approval/production/approval/abc@azure-ad",
      "release_id": "0192f0d0-7c5e-7f2a-a3b4-5e6f7a8b9c0d",
      "sequence": 4,
      "timestamp": "2026-09-30T13:00:00Z",
      "actor": {
        "issuer": "https://login.microsoftonline.com/.../v2.0",
        "subject": "abc@azure-ad",
        "display_name": "Alice"
      },
      "type": "ApprovalSubmitted",
      "reason": "approve",
      "data": { "environment": "production", "step_id": "approval", "decision": "approve" }
    }
  ],
  "cursor": null,
  "has_more": false
}
```

### 5.11 Get Snapshot

```
GET /api/v1/releases/0192f0d0-7c5e-7f2a-a3b4-5e6f7a8b9c0d/snapshot
Authorization: Bearer <token>
```

Response: `200 OK`

```json
{
  "template": {
    "name": "service",
    "version": "1",
    "inputs": { ... },
    "environments": [ ... ]
  },
  "registry_inputs": {
    "repo": { "provider": "github", "id": "789012", "path": "my-org/deploy" },
    "edits": [
      { "file": "apps/my-service/preview.yaml", "field": "image.tag" }
    ]
  },
  "step_type_versions": {
    "builtin/commit-files": "1",
    "builtin/change-request": "1"
  },
  "template_hash": "sha256:abc123..."
}
```

### 5.12 List Applications

```
GET /api/v1/applications
Authorization: Bearer <token>
```

Response: `200 OK`

```json
{
  "items": [
    {
      "name": "my-service",
      "description": "Customer-facing API",
      "labels": { "team": "payments" },
      "paused": false,
      "available": true,
      "source": { "provider": "github", "id": "123456" },
      "template": "service@1",
      "versioning": { "scheme": "semver", "tag_format": "v{version}", "require_tag": true },
      "environments": ["preview", "production"]
    }
  ]
}
```

`available` is `false` when a declared `path` label failed startup
verification (F-10): the application is retried in the background and new
releases are rejected until it verifies.

### 5.13 List Templates

```
GET /api/v1/templates
Authorization: Bearer <token>
```

Response: `200 OK`

```json
{
  "items": [
    {
      "name": "service",
      "version": "1",
      "environments": ["preview", "production"]
    }
  ]
}
```

### 5.14 Client Config (Unauthenticated)

```
GET /api/v1/clientconfig
```

Response: `200 OK`

```json
{
  "issuers": [
    "https://token.actions.githubusercontent.com",
    "https://login.microsoftonline.com/00000000-0000-0000-0000-000000000000/v2.0"
  ],
  "audiences": ["cargobike", "api://cargobike"]
}
```

### 5.15 Health

```
GET /api/v1/live     -> 200 (always)
GET /api/v1/ready    -> 200 (holds leader lock) or 503 (standby)
GET /api/v1/startup  -> 200 (startup complete)
```

### 5.16 Webhook

```
POST /webhooks/github
X-Hub-Signature-256: sha256=abc123...
X-GitHub-Delivery: 12345-67890
Content-Type: application/json

{ "ref": "refs/tags/v1.2.3", ... }
```

Response: `202 Accepted` (fast-ack within milliseconds)

---

## 6. CLI Commands

### 6.1 Command Tree

```
cargobike
├── release
│   ├── create <application> <version>   # Create a release
│   ├── list                               # List releases
│   ├── get <id>                           # Show release details
│   ├── cancel <id>                        # Cancel a running release
│   ├── delete <id>                        # Delete a terminal release
│   ├── retry <id>                         # Retry a failed release
│   ├── watch <id>                         # Watch a release via SSE
│   ├── approve <id>                       # Approve or reject an environment
│   └── events <id>                        # Show release event log
├── application
│   └── list                               # List registered applications
├── template
│   └── list                               # List registered templates
├── validate                               # Validate config and templates
├── config
│   ├── show                               # Show current config
│   └── init                               # Initialize config file
├── context
│   ├── list                               # List saved contexts
│   └── use <name>                         # Switch to a saved context
├── whoami                                 # Show authenticated identity
└── completion [shell]                     # Generate shell completions

cargobike-server
└── hash-api-key                            # Hash a plaintext API key for config
```

### 6.2 `release create`

```
cargobike release create <application> <version> [flags]

Arguments:
  application    Application name (must exist in registry)
  version       Version string (validated by versioning scheme)

Flags:
  --wait              Wait for the release to complete (streams SSE)
  --until <env>       Wait until the named environment reaches a terminal state
  --timeout <dur>     Maximum wait time (default: none)
  -o, --output <fmt>  Output format: table | json | yaml (default: table)
  --yes               Skip confirmation prompts
  --allow-http        Allow http:// server URLs (localhost only)
  --url <url>         Override server URL
  --auth <type>       Override auth type: api-key | github-actions | exec | none
  --context <name>    Use a saved context from the CLI config (§9.7)
  --ca-file <path>    Custom CA bundle for TLS
  --audience <aud>    OIDC audience for github-actions tokens (default: cargobike)
  -h, --help          Show help

Environment:
  CARGOBIKE_URL            Server URL
  CARGOBIKE_CONTEXT        Context to use (overrides current_context)
  CARGOBIKE_API_KEY        API key (type api-key)
  CARGOBIKE_AUDIENCE       OIDC audience override
  CARGOBIKE_CA_FILE        Custom CA bundle
  GITHUB_REPOSITORY        Auto-detected for CI context
  GITHUB_SHA               Auto-detected for CI context
  GITHUB_RUN_ID            Auto-detected for CI context

Examples:
  # Create a release from CI (OIDC auth auto-detected)
  cargobike release create my-service 1.2.3 --wait --until preview

  # Create and output JSON
  cargobike release create my-service 1.2.3 -o json

  # Create with an API key
  cargobike release create my-service 1.2.3 --auth api-key
```

Exit codes (with `--wait` or `watch`):

| Code | Meaning |
|------|---------|
| 0 | Completed (all environments, or `--until` env completed or skipped) |
| 1 | Failed |
| 2 | Canceled or Superseded |
| 3 | Timed out (`--timeout`) |
| 10 | Authentication failed |
| 11 | Server unreachable |
| 12 | Invalid response |

### 6.3 `release list`

```
cargobike release list [flags]

Flags:
  -a, --application <name>  Filter by application
  --phase <phase>          Filter by phase
  --version <glob>         Filter by version (glob matches ignored versions too)
  --since <duration>       Only releases newer than this (e.g. 24h)
  --limit <int>            Page size (1-500, default 50)
  -o, --output <fmt>       Output format: table | json | yaml (default: table)

Examples:
  cargobike release list
  cargobike release list --application my-service -o json
  cargobike release list --version "1.2" --limit 10
```

Table output (default):

```
ID                                    APPLICATION    VERSION  PHASE      ACTOR  CREATED
0192f0d0-7c5e-7f2a-a3b4-5e6f7a8b9c0d my-service    1.2.3    Completed my-org 2026-09-30T12:00:00Z
0192f0d1-8d6f-803b-b4c5-6f7a8b9c0d1e my-service    1.3.0    Running    my-org 2026-09-30T14:00:00Z
0192f0d2-9e7a-914c-c5d6-7a8b9c0d1e2f other-service 2.0.0   PendingApproval alice  2026-09-30T15:00:00Z
```

### 6.4 `release get`

```
cargobike release get <id> [flags]

Arguments:
  id    Release ID (UUIDv7)

Flags:
  --events            Include the audit trail
  -o, --output <fmt>  Output format: table | json | yaml (default: table)

Examples:
  cargobike release get 0192f0d0-7c5e-7f2a-a3b4-5e6f7a8b9c0d
  cargobike release get 0192f0d0-... --events -o json
```

### 6.5 `release cancel`

```
cargobike release cancel <id> [flags]

Arguments:
  id    Release ID

Flags:
  --yes  Skip confirmation prompt
  -o, --output <fmt>  Output format (default: table)

Examples:
  cargobike release cancel 0192f0d0-...
  cargobike release cancel 0192f0d0-... --yes
```

### 6.6 `release delete`

```
cargobike release delete <id> [flags]

Arguments:
  id    Release ID (must be terminal)

Flags:
  --yes  Skip confirmation prompt
  -o, --output <fmt>  Output format (default: table)

Examples:
  cargobike release delete 0192f0d0-... --yes
```

### 6.7 `release retry`

```
cargobike release retry <id> [flags]

Arguments:
  id    Release ID (must be failed)

Flags:
  --new            Create a new release instead of forking (only if terminal)
  --wait           Wait for the release to complete
  --until <env>    Wait until the named environment
  --timeout <dur>  Maximum wait time
  -o, --output <fmt>  Output format (default: table)

Examples:
  # Fork from last failure point (same release ID)
  cargobike release retry 0192f0d0-...

  # Create a new release (only if original is terminal)
  cargobike release retry 0192f0d0-... --new --wait
```

### 6.8 `release watch`

```
cargobike release watch <id> [flags]

Arguments:
  id    Release ID

Flags:
  --until <env>     Exit when the named environment reaches a terminal state
  --timeout <dur>   Maximum watch time (default: none)
  -o, --output <fmt>  Output format: table | json | yaml (default: table)

Examples:
  # Watch until preview completes
  cargobike release watch 0192f0d0-... --until preview

  # Watch the full release
  cargobike release watch 0192f0d0-...

  # Watch with a timeout
  cargobike release watch 0192f0d0-... --timeout 1h
```

Exit codes: same as `release create --wait` (see 6.2).

### 6.9 `release approve`

```
cargobike release approve <id> --environment <env> [flags]

Arguments:
  id    Release ID

Flags:
  --environment <env>  Environment to approve (required)
  --step <id>          Step ID (default: the environment's single waiting
                       approval step; errors if there are several, C17)
  --reject             Reject instead of approve
  --comment <text>     Optional comment
  -o, --output <fmt>   Output format: table | json | yaml (default: table)

Examples:
  # Approve production
  cargobike release approve 0192f0d0-... --environment production

  # Reject with a comment
  cargobike release approve 0192f0d0-... --environment production --reject --comment "Version mismatch"
```

### 6.10 `release events`

```
cargobike release events <id> [flags]

Arguments:
  id    Release ID

Flags:
  -o, --output <fmt>  Output format: table | json | yaml (default: table)

Examples:
  cargobike release events 0192f0d0-...
  cargobike release events 0192f0d0-... -o json
```

### 6.11 `application list`

```
cargobike application list [flags]

Flags:
  -o, --output <fmt>  Output format: table | json | yaml (default: table)

Examples:
  cargobike application list
  cargobike application list -o json
```

### 6.12 `template list`

```
cargobike template list [flags]

Flags:
  -o, --output <fmt>  Output format: table | json | yaml (default: table)

Examples:
  cargobike template list
  cargobike template list -o json
```

### 6.13 `validate`

```
cargobike validate [paths...] [flags]

Arguments:
  paths    Config files, template files or directories to validate

Flags:
  --server-config <file>  Validate this server config file (incl. registry)
  --templates <dir>       Validate the templates in this directory
  --application <name>    Validate a specific application's config
  --resolve               Look up repo IDs and check declared `path` labels
                          against live providers (F-10)
  -o, --output <fmt>      Output format: table | json | yaml (default: table)

Examples:
  # Validate everything
  cargobike validate --server-config config.yaml --templates templates/

  # Validate templates only (offline, mock provider)
  cargobike validate --templates ./templates

  # Validate and resolve against live providers
  cargobike validate --server-config config.yaml --resolve

  # Validate and show diagnostics
  cargobike validate --server-config config.yaml -o json
```

Checks:
- Config file parses and interpolates (`${VAR}`, `{version}`)
- OIDC trust entries have claim constraints (or `allow_unconstrained: true`)
- OIDC entry referenced in `releasers` grants `release:create`; one in
  `approval.approvers` grants `release:approve` (F-99a); releaser `ref`
  globs can match `versioning.tag_format` (F-99a)
- `api_key` selectors carry no `claims`; `api_key` in `approval.approvers`
  sets `allow_machine_approvers: true` (F-99b)
- Application registry: edits have `file` + `field`; `approval` requires a
  template `wait: approval` step
- Templates: CEL gates valid against versioning scheme (`semver(...)`,
  F-27a), `${{ }}` expressions parse, `when` expressions, step IDs unique
  after `include` expansion (F-34), required `timeout` on wait steps
- Format strings use only their documented placeholders (F-147)
- `--resolve`: every repo `id` resolves, declared `path` labels match (F-10)

### 6.14 `config show`

```
cargobike config show [flags]

Flags:
  -o, --output <fmt>  Output format: table | json | yaml (default: table)

Shows the resolved CLI configuration (server URL, auth type, output
format). Secrets are redacted.
```

### 6.15 `config init`

```
cargobike config init [flags]

Flags:
  --url <url>         Server URL
  --name <name>       Context name (default: "default")
  --auth <type>       Auth type: api-key | github-actions | exec | none
  --api-key <key>     API key (if auth=api-key; prefer --api-key-file)
  --force             Overwrite existing config

Creates ~/.config/cargobike/config.yaml with a single context of the
given name and sets it as `current_context`.
```

### 6.16 `context list`

```
cargobike context list [flags]

Flags:
  -o, --output <fmt>  Output format: table | json | yaml (default: table)

Lists all saved CLI contexts (named server + auth combinations).
```

### 6.17 `context use`

```
cargobike context use <name>

Arguments:
  name    Context name to switch to

Switches the active CLI context. Subsequent commands use the selected
context's server URL and auth type.
```

### 6.18 `whoami`

```
cargobike whoami [flags]

Flags:
  -o, --output <fmt>  Output format: table | json | yaml (default: table)

Shows the authenticated identity (issuer, subject, display name) and
grants for the current auth type, as resolved from the selected context
or environment variables.
```

### 6.19 `completion`

```
cargobike completion <shell>

Arguments:
  shell    bash | zsh | fish | powershell

Generates shell completion scripts.
```

### 6.20 `hash-api-key` (cargobike-server)

```
cargobike-server hash-api-key

Reads the plaintext API key from stdin (so it never appears in process
arguments or shell history) and prints the `$argon2id$...` hash suitable
for pasting into the `auth.api_keys[].hash` field of the server config.

Example:
  $ cargobike-server hash-api-key
  # paste the key, press Enter:
  $argon2id$v=19$m=32768,t=3,p=4$ZGVwVr4dImL0T3vH...
```

---

## 7. GitHub Action

File: `actions/setup-cargobike/action.yml`

```yaml
name: Setup Cargobike
description: Install Cargobike CLI and configure OIDC authentication
inputs:
  server:
    description: Cargobike server URL
    required: true
  version:
    description: Cargobike image tag
    required: false
    default: latest
  azure-client-id-pr:
    description: Azure Client ID for PR/dispatch events
    required: true
  azure-client-id-main:
    description: Azure Client ID for push to main
    required: true
  azure-tenant-id:
    description: Azure Tenant ID
    required: false

runs:
  using: composite
  steps:
    - name: Azure login
      uses: corticph/actions/azure-login@v1
      with:
        client-id: ${{ inputs.azure-client-id-pr }}
        tenant-id: ${{ inputs.azure-tenant-id }}

    - name: Pull Cargobike image
      shell: bash
      run: |
        docker pull cargobike/cargobike:${{ inputs.version }}

    - name: Create wrapper script
      shell: bash
      run: |
        cat > /usr/local/bin/cargobike << 'EOF'
        #!/bin/bash
        exec docker run --rm \
          -e CARGOBIKE_URL=${CARGOBIKE_URL} \
          -e CARGOBIKE_AUTH=exec \
          -e AZURE_CLIENT_ID=${AZURE_CLIENT_ID} \
          -e AZURE_TENANT_ID=${AZURE_TENANT_ID} \
          cargobike/cargobike:${{ inputs.version }} \
          cargobike "$@"
        EOF
        chmod +x /usr/local/bin/cargobike

    - name: Configure Cargobike
      shell: bash
      run: |
        echo "CARGOBIKE_URL=${{ inputs.server }}" >> $GITHUB_ENV
        echo "CARGOBIKE_AUTH=exec" >> $GITHUB_ENV
```

### Usage in a workflow

```yaml
jobs:
  release:
    runs-on: ubuntu-latest
    permissions:
      id-token: write
      contents: read
    steps:
      - uses: actions/checkout@v4

      - uses: cargobike/setup-cargobike@v1
        with:
          server: https://cargobike.internal
          azure-client-id-pr: ${{ vars.AZURE_CLIENT_ID_PR }}
          azure-client-id-main: ${{ vars.AZURE_CLIENT_ID_MAIN }}
          azure-tenant-id: ${{ vars.AZURE_TENANT_ID }}

      - name: Create release
        run: |
          ID=$(cargobike release create my-service 1.2.3 -o json | jq -r '.metadata.id')
          echo "RELEASE_ID=$ID" >> "$GITHUB_ENV"

      - name: Wait for preview
        run: cargobike release watch "$RELEASE_ID" --until preview --timeout 30m
```

---

## 8. Docker Compose Quickstart

```yaml
# docker-compose.yml
version: "3.8"

services:
  postgres:
    image: postgres:16
    environment:
      POSTGRES_DB: cargobike
      POSTGRES_USER: cargobike
      POSTGRES_PASSWORD: secret
    volumes:
      - pgdata:/var/lib/postgresql/data
    ports:
      - "5432:5432"

  cargobike-server:
    image: cargobike/cargobike-server:latest
    depends_on:
      - postgres
    ports:
      - "8080:8080"
    volumes:
      - ./config.yaml:/etc/cargobike/config.yaml:ro
      - ./templates:/etc/cargobike/templates:ro
      - ./keys:/etc/cargobike/keys:ro
      - ./secrets:/run/secrets:ro
    command:
      - --config
      - /etc/cargobike/config.yaml

volumes:
  pgdata:
```

---

## 9. Environment-Variable Reference

### Server (`cargobike-server`)

| Variable | Description |
|----------|-------------|
| `CARGOBIKE_SERVER_CONFIG` | Config file path |
| `CARGOBIKE_SERVER_DATABASE_URL` | PostgreSQL URL |
| `CARGOBIKE_SERVER_DATABASE_URL_FILE` | PostgreSQL URL (from file) |
| `CARGOBIKE_SERVER_LISTEN` | Bind address (override `server.listen`) |
| `CARGOBIKE_SERVER_PUBLIC_URL` | Externally visible URL |
| `CARGOBIKE_SERVER_GITHUB_APP_ID` | GitHub App ID (single-provider shortcut) |
| `CARGOBIKE_SERVER_GITHUB_PRIVATE_KEY_FILE` | GitHub App private key path |
| `CARGOBIKE_SERVER_GITHUB_API_URL` | GitHub API URL |
| `CARGOBIKE_SERVER_GITHUB_WEBHOOK_SECRET_FILE` | GitHub webhook secret path |
| `CARGOBIKE_SERVER_BOOTSTRAP_API_KEY` | Bootstrap admin key (plaintext, hashed at startup, localhost-only) |
| `CARGOBIKE_SERVER_BOOTSTRAP_API_KEY_FILE` | Bootstrap admin key (from file) |
| `CARGOBIKE_SERVER_LOG_LEVEL` | Single log level (`RUST_LOG` takes precedence) |
| `CARGOBIKE_SERVER_LOG_FORMAT` | `json` or `text` |

### CLI (`cargobike`)

| Variable | Description |
|----------|-------------|
| `CARGOBIKE_CONFIG` | CLI config file path |
| `CARGOBIKE_CONTEXT` | Context to use |
| `CARGOBIKE_URL` | Server URL |
| `CARGOBIKE_API_KEY` | API key value |
| `CARGOBIKE_API_KEY_FILE` | API key (from file) |
| `CARGOBIKE_AUTH` | Auth type override: `none`, `api-key`, `exec`, `github-actions` |
| `CARGOBIKE_AUDIENCE` | OIDC audience for `github-actions` tokens (default `cargobike`) |
| `CARGOBIKE_CA_FILE` | Custom CA bundle |
| `CARGOBIKE_OUTPUT` | Default output format |
| `GITHUB_REPOSITORY` | Auto-detected CI context |
| `GITHUB_SHA` | Auto-detected CI context |
| `GITHUB_RUN_ID` | Auto-detected CI context |

### Both binaries

| Variable | Description |
|----------|-------------|
| `NO_COLOR` | Disable colour output in the CLI |
| `XDG_CONFIG_HOME` | Base for the CLI config path |
| `OTEL_EXPORTER_OTLP_ENDPOINT` (and other `OTEL_*`) | Distributed tracing export |
