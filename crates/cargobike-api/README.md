# cargobike-api

_Shared DTOs, the OpenAPI surface, and the typed client (planned)._

## Purpose

`cargobike-api` is intended to be the single source of truth for the shapes
that cross the server/CLI boundary (PRD §11.2, §16 ordering rule 12):

- request/response DTOs (`cargobike-api::dto`), reused by the server's axum
  handlers and the CLI's typed client,
- the OpenAPI surface definitions (with `utoipa`, F-104),
- a typed HTTP client wrapper so the CLI calls the API with compile-checked
  shapes instead of hand-rolled JSON pointers.

Keeping DTOs here prevents the "defined independently in the server and the
CLI" drift the PRD calls out as a duplication risk (§16, task 2.0a).

Must **not** depend on: `sqlx`, `dbos`, `wasmtime` (PRD A.14). The reqwest
client side sits behind a cargo feature.

## Status

**Stub.** Today the crate contains only its module documentation. The server
and CLI hand-roll the JSON shapes (release documents, problem details,
cursor pages); the projects' phase audit
([`docs/TODO-phase-audit.md`](../../docs/TODO-phase-audit.md)) itemises what
moves here first (release document bundle, error-code vocabulary constants,
conceptual `clientconfig`/`whoami` types used by both processes).

## Usage

Not usable yet. The planned shape:

```rust
use cargobike_api::dto::{CreateReleaseRequest, ReleaseDocument};

let request = CreateReleaseRequest {
    application: "my-service".into(),
    version: "1.2.3".into(),
};
let release: ReleaseDocument = client.create_release(&request).await?;
```

## Tests

```bash
cargo test -p cargobike-api
```

Once the first DTOs land: serde round-trip tests per DTO, fixture-based tests
asserting the *served* documents (server tests' actual JSON) and the *accepted*
bodies match, and an OpenAPI snapshot test. See [`AGENTS.md`](AGENTS.md).
