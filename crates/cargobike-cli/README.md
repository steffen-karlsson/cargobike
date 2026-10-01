# cargobike-cli

_The release lifecycle in your terminal (and CI): no database, server API only._

## Purpose

`cargobike-cli` builds the `cargobike` binary (G-10: no DB dependencies). It
talks to a Cargobike server over the REST API, resolves its target from named
contexts with a strict precedence (flag > env > context > `defaults` >
built-in), authenticates with one of four auth shapes, renders output as
`table|json|yaml`, and maps every failure to a documented exit code (US-4).

Components:

| Module | Contents |
|---|---|
| `config` | The CLI config file (`CARGOBIKE_CONFIG`, default `~/.config/cargobike/config.yaml` or `$XDG_CONFIG_HOME/cargobike/config.yaml`): contexts, auth types (`none`, `api-key`, `exec`, `github-actions`), `ca_file`, `defaults.output`; the `http://` refusal rule (`--allow-http` override, F-98) and the precedence walk |
| `client` | `reqwest`-backed API caller: bearer per auth shape, RFC 9457 error surfacing, exit-code mapping (10 auth / 11 unreachable / 12 invalid response) |
| `ci` | GitHub Actions auto-detection: `GITHUB_REPOSITORY`/`GITHUB_SHA`/`GITHUB_RUN_ID` context struct, the OIDC request environment, the audience resolution (default `cargobike`) |
| `render` | Output rendering: table rows (`metadata.id`, `spec.application`, `spec.version`, `status.phase`) or full documents as JSON/YAML (F-108) |
| `main` | clap wiring: global flags with env fallbacks, `context list/use`, the release verbs, `create --wait` polling |

## Status

Phase 4 scope (4.5/4.6 complete; 4.9's CLI-half tests exist). Pending (audit
TODO §5): the `release watch` SSE command (4.7), `cargobike validate` (4.8),
`release retry`/`release approve`, `--since`, `--events`, GitHub-Actions OIDC
token minting, the `-o` flag honouring (`-o` is parsed but ignored by the
render calls today).

## Usage

```bash
# Configure a context (file layout in §9.7).
cargobike context list
cargobike context use production

# The release lifecycle.
cargobike release create my-service 1.2.3
cargobike release create my-service 1.2.3 --wait --timeout 30m   # polls to terminal
cargobike release list -a my-service --phase PendingApproval --limit 50
cargobike release get 0192f0d0-... 
cargobike release cancel 0192f0d0-...
cargobike release delete 0192f0d0-...

# Output control.
cargobike release get 0192f0d0-... -o json
cargobike release list -o yaml

# CI: no config file needed (auth: github-actions auto-detected).
CARGOBIKE_URL=https://cargobike.example.com cargobike release create my-service 1.2.3

# Development against localhost: plain http is allowed only there (F-98).
cargobike --url http://localhost:8080 release list
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
  binary's end-to-end via `assert_cmd` (rendered rows, exit codes 10/11/12).
- Unit tests per module (config precedence, URL refusals, auth shapes, exec
  token contract, CI detection).
[`AGENTS.md`](AGENTS.md) holds the contribution rules.
