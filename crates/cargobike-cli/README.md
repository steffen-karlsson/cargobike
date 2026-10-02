# cargobike-cli

_The release lifecycle in your terminal (and CI): no database, server API only._

## Purpose

`cargobike-cli` builds the `cargobike` binary (: no DB dependencies). It
talks to a Cargobike server over the REST API, resolves its target from named
contexts with a strict precedence (flag > env > context > `defaults` >
built-in), authenticates with one of four auth shapes, renders output as
`table|json|yaml`, and maps every failure to a documented exit code (US-4).

Components:

| Module | Contents |
|---|---|
| `config` | The CLI config file (`CARGOBIKE_CONFIG`, default `~/.config/cargobike/config.yaml` or `$XDG_CONFIG_HOME/cargobike/config.yaml`): contexts, auth types (`none`, `api-key`, `exec`, `github-actions`), `ca_file`, `defaults.output`; the `http://` refusal rule (`--allow-http` override) and the precedence walk |
| `client` | `reqwest`-backed API caller: bearer per auth shape, RFC 9457 error surfacing, exit-code mapping (10 auth / 11 unreachable / 12 invalid response) |
| `ci` | GitHub Actions auto-detection: `GITHUB_REPOSITORY`/`GITHUB_SHA`/`GITHUB_RUN_ID` context struct, the OIDC request environment, the audience resolution (`--audience` > `CARGOBIKE_AUDIENCE` > `cargobike`), and the ID-token exchange (`ACTIONS_ID_TOKEN_REQUEST_URL?audience=…` with the request token as bearer) |
| `render` | Output rendering: table rows (`metadata.id`, `spec.application`, `spec.version`, `status.phase`) or full documents as JSON/YAML |
| `main` | clap wiring: global flags with env fallbacks, `context list/use`, the release verbs, `create --wait` polling |

## Status

Phase 4 scope (4.5/4.6 complete; 4.9's CLI-half tests exist). The OIDC
exchange, the `-o` honouring, the auth/CA overrides, `--since`, and
`release retry` (`--new` included) landed (audit §5's ticked items).
Pending (audit TODO §5): the `release watch` SSE command (4.7),
`cargobike validate` (4.8), `release approve`, `--events`.

## Usage

```bash
# Configure a context (file layout in §9.7).
cargobike context list
cargobike context use production

# The release lifecycle.
cargobike release create my-service 1.2.3
cargobike release create my-service 1.2.3 --wait --timeout 30m # polls to terminal
cargobike release list -a my-service --phase PendingApproval --limit 50
cargobike release list --since 24h          # updated at-or-after; RFC 3339 also fine
cargobike release get 0192f0d0-...
cargobike release cancel 0192f0d0-...
cargobike release retry 0192f0d0-...          # fork from the last failure (same ID)
cargobike release retry 0192f0d0-... --new    # a fresh row carrying retried_from
cargobike release delete 0192f0d0-...

# Output control.
cargobike release get 0192f0d0-... -o json
cargobike release list -o yaml

# CI: no config file needed (auth: github-actions auto-detected).
CARGOBIKE_URL=https://cargobike.example.com cargobike release create my-service 1.2.3

# CI: force the Actions auth + the audience (the detection covers most runs).
CARGOBIKE_URL=https://cargobike.example.com \
  cargobike --auth github-actions --audience my-cargobike release create my-service 1.2.3

# Development against localhost: plain http is allowed only there.
cargobike --url http://localhost:8080 release list

# GHES / private CA: the bundle path (`~` expands against HOME).
cargobike --ca-file ~/certs/ghes-root.pem release list

# API-key auth outside a config file: the key material from _FILE (preferred)
# or the plain env.
CARGOBIKE_API_KEY_FILE=~/secrets/cb.key CARGOBIKE_URL=https://cb.internal \
  cargobike --auth api-key release list
```

Exit codes (US-4 vocabulary): `0` completed, `1` failed release, `2` canceled
or superseded, `3` timed out (`--timeout`), `10` authentication failed,
`11` server unreachable, `12` invalid response / refused call, `1` bad local
config (the load path could not resolve).

## Tests

```bash
cargo test -p cargobike-cli
```

- `tests/cli_server.rs`: client verbs against a wiremock server, plus the
 binary's end-to-end via `assert_cmd` (rendered rows, exit codes 10/11/12,
 the `-o`/env-format rendering, and the full Actions-OIDC exchange path).
- Unit tests per module (config precedence, URL refusals, auth shapes +
 overrides, tilde expansion, exec token contract, CI detection + the exchange).
[`AGENTS.md`](AGENTS.md) holds the contribution rules.
