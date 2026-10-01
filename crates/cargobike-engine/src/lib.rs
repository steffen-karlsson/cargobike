//! Cargobike engine: the template compiler, the CEL expression runtime,
//! the step registry, and (later phases) the durable interpreter that the
//! spike model (docs/spike-dbos.md) informs.
//!
//! Depends on `cargobike-core` only (PRD §11.2): no axum, no api crate.

pub mod expr;
pub mod interpreter;
pub mod leases;
pub mod signals;
pub mod snapshot;
pub mod steps;
pub mod template;

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
