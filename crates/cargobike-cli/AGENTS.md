# cargobike-cli — AGENTS.md

Crate-specific how-to; project-wide rules in the root
[`AGENTS.md`](../../AGENTS.md).

## How to develop

- **No database dependencies** (/A.14): no `sqlx`, `dbos`, `wasmtime`,
 `axum`. The server's REST API is the only peer; DTOs are currently
 hand-rolled `serde_json` — keep shapes stable while the shared `cargobike-api`
 DTOs are materialised, and move duplicated shapes there rather than growing
 local ones.
- **Exit codes are a contract (US-4)**: release outcomes 0–3, client failures
 10–12 (auth / unreachable / invalid response). Every new failure mode maps
 into that vocabulary; never invent ad-hoc codes, never conflate server
 problems with transport failures.
- **Trust placement **: the exec command and the token source come from
 the local config only — never from server-supplied hints or the
 `/clientconfig` response. Auto-discovery supplies issuer/audience hints
 only. Do not bind any server-provided string into an execution path.
- Precedence is fixed: flag > env var > selected context > `defaults` >
 built-in default. New flags wire, in order: clap (`env = "..."`), then the
 resolution walk in `config::resolve`, then tests asserting the walk.
- URL safety: `http://` is refused except for local hosts unless
 `--allow-http`; keep the scheme check URL-parsed (no string sniffing).
- Secrets stay `SecretString`/`SecretRef`; `Debug`/`Display` never expose
 them; exec command output checks the "PRINTS ONLY the token" contract.
- CI-auto-detection lives in `ci.rs`; keep the `CiContext` facts there and
 keep `audience` reading `CARGOBIKE_AUDIENCE` only.

## How to test

```bash
cargo test -p cargobike-cli
```

- Unit tests per module in `#[cfg(test)] mod tests`; scenario names, e.g.
 `test_the_exec_contract_prints_only_the_token`.
- `tests/cli_server.rs` runs the real binary via `assert_cmd` against
 `wiremock`: keep the exit-code and rendering coverage here, matching PRD
 §14.4 rows (output formats, config refusal, CI detection).
- Env-var tests use `unsafe { std::env::set_var }` only in tests, and always
 clean up — no global ambient state across tests. Tests must not DEPEND on
 ambient env either: GitHub's runners export variables like
 `XDG_CONFIG_HOME`, so env-reading logic under test controls and restores
 the variable itself (see the config-path test).
- New verbs need: happy-path render test, problem-document mapping test
 (exit 12), auth-failure test (exit 10), dead-server test (exit 11).

## How to document

- Every flag and env var is documented in `--help` text AND in the README's
 usage block; the PRD's §9.7 CLI-config file and §4.8 flag list are the
 reference — keep both in sync when adding flags.
- The exit-code table in the README is part of the contract; update it in the
 same commit as any code change to codes.
- Module docs summarise what each module owns (see `lib.rs`).

## How to contribute

- Conventional commits `<type>(cargobike-cli): <subject>` (PRD §20.1); one
 verb or one behaviour per commit.
- Adding a verb means: the handler, the render path, the exit codes, tests,
 README usage block — in the same commit.

## Criteria for evaluating contributions (before committing)

All must be true:

1. All four gates pass: `make lint`, `make test`, `make build`,
 `make docs-check` — including the cli wiremock suite
 (`tests/cli_server.rs`).
2. Output rendering honours the resolved format (currently violated by the
 `-o` bug tracked in the audit — do not add new render sites that hardcode a
 format; fix through `Resolved`).
3. No flag/env reads outside the declared precedence; `CARGOBIKE_*` naming
 exact .
4. No secret material in logs, errors or rendered output; panic-free (lints
 deny `unwrap`/`expect` outside tests).
5. Failure modes map to the exit-code vocabulary with tests.
6. The crate's [`README.md`](README.md) (status, usage, exit codes) and this
 file are updated in the same commit.
