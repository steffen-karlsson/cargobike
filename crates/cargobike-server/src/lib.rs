//! Cargobike server: axum API, auth, webhooks, extension host.

pub mod approvals;
pub mod auth;
pub mod availability;
pub mod config;
pub mod db;
pub mod discovery;
pub mod engine;
pub mod http;
pub mod leader;
pub mod release;
pub mod validation;

pub use config::Config;
pub use http::boot;
