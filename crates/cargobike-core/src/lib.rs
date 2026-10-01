//! Cargobike core: the shared, I/O-free model.
//!
//! Holds the release model ([`model`]), release error codes ([`error`]),
//! version schemes ([`version`]), the pipeline template model
//! ([`template`]), the provider abstraction ([`provider`], [`registry`]),
//! and the step-type abstraction ([`step`]).
//!
//! This crate must compile to `wasm32-wasip2`: no tokio,
//! sqlx, reqwest or wasmtime dependencies.

#![deny(clippy::all)]

pub mod edits;
pub mod error;
pub mod model;
pub mod provider;
pub mod registry;
pub mod step;
pub mod template;
pub mod version;
pub mod webhook;

pub use model::{
    ChangeRequestRef, CrState, EnvironmentPhase, EnvironmentStatus, Phase, Release, ReleaseSpec,
    ReleaseStatus, RepoRef,
};
