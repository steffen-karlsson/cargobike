//! Cargobike engine: the template compiler, the CEL expression runtime,
//! the step registry, and (the later phases) the durable interpreter that the
//! spike model (the docs/spike-dbos.md) informs.
//!
//! Depends on `cargobike-core` only: no axum, no api crate.

pub mod builtin;
pub mod cleanup;
pub mod concurrency;
pub mod correlation;
pub mod crash;
pub mod expr;
pub mod interpreter;
pub mod leases;
#[cfg(feature = "crash-hooks")]
pub mod mock;
pub mod names;
pub mod reconciler;
pub mod signals;
pub mod snapshot;
pub mod steps;
pub mod template;

pub use cleanup::{CLEANUP_WORKFLOW, CleanupArgs, CleanupTarget, register_cleanup};
pub use expr::{ExprContext, ExprError, eval_gate, eval_param, interpolate_params};
pub use interpreter::{
    INTERPRETER_WORKFLOW, InterpretArgs, InterpretResult, InterpreterServices,
    environment_phase_of, register_interpreter,
};
pub use leases::{LeaseAttempt, LeaseRepository, LeaseTransfer};
pub use signals::{InterpreterError, Signal};
pub use snapshot::{ReleaseIdentity, ReleaseSnapshot, duration_text};
pub use steps::{StepRegistry, StepRegistryError};
pub use template::{CompiledTemplate, TemplateError, compile, compile_with};
