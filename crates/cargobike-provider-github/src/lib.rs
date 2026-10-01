//! GitHub provider implementation for Cargobike (PRD §7.2, Phase 4).
//!
//! The octocrab-backed implementation of the provider contract: REST for
//! repositories/PRs/commits, the App's JWT for installations (or a PAT),
//! and the webhook's signature + normalisation at the transport edge.
//!
//! Status: webhook verification + normalisation are complete (below, and
//! tested); the Provider operations land next over pinned octocrab
//! shapes — deliberately not drafted until the API surface is verified.

mod signature;
mod webhook;

pub use webhook::verify_and_normalise;
