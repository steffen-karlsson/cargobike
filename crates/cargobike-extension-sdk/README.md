# cargobike-extension-sdk

_WIT definitions and guest bindings for Cargobike extensions._

## Purpose

The extension surface of the platform (PRD §7.3/§7.4, US-10, G-3/G-4): the
same WIT-derived schema served over HTTP or gRPC by **sidecars** (out-of-process
extensions, v1.0), or compiled into WASM components with `wit-bindgen`
guest bindings (v1.1). A sidecar in any language (Go, Python, Rust)
implements the provider interface and/or step types; the server's
`extensions:` config section declares the endpoint, channel auth, and what it
provides (F-49, F-121).

Position in the graph: depends on `cargobike-core` only. Guests must not pull
`tokio`/`reqwest` — they call the host's injected `http` interface instead
(F-121). The host side (wasmtime runtime, sidecar transports) lives in
`cargobike-server`.

## Status

**Stub.** The crate exists in the workspace (Phase 1's task 1.2 laid out the
seven members) with its module documentation only. The sidecar transport and
schema/SHA-pinning are Phase 5 (task 5.3); WASM cargo-feature work is v1.1
(NG-10). Config shapes for extensions/providers already parse in
`cargobike-server` (`ExtensionConfig`), and the engine's `StepRegistry`
accepts sidecar-registered step types.

## Usage

Not usable yet. The planned flow:

1. Enumerate the WIT interface in this crate (`wit/provider.wit`,
   `wit/step-type.wit`).
2. Generate guest bindings with `wit-bindgen` (`cargo` feature or build step).
3. Implement the guest in your language, ship it as a sidecar (HTTP/gRPC) or a
   `wasm32-wasip2` component.
4. Declare it in the server config:

```yaml
extensions:
  - name: gitlab-internal
    endpoint: unix:///run/cargobike/gitlab.sock
    auth:
      shared_secret: { file: /run/secrets/gitlab-sidecar-secret }
    provides:
      step_types: ["acme/notify@1"]
providers:
  - name: gitlab-internal
    type: extension
    extension: gitlab-internal
```

## Tests

```bash
cargo test -p cargobike-extension-sdk
```

Planned, with task 5.3: WIT shape round-trip tests, a mock sidecar server used
by the server integration tests, schema SHA-256 pinning assertions (F-119),
and the `http-call`-cannot-reach-the-sidecar client separation test. See
[`AGENTS.md`](AGENTS.md).
