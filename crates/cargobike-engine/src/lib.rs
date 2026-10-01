//! Cargobike engine: the template compiler, the CEL expression runtime,
//! the step registry, and (later phases) the durable interpreter that the
//! spike model (docs/spike-dbos.md) informs.
//!
//! Depends on `cargobike-core` only (PRD §11.2): no axum, no api crate.

pub mod template;

pub use template::{CompiledTemplate, TemplateError, compile};
