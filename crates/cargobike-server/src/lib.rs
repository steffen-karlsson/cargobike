//! Cargobike server: axum API, auth, webhooks, extension host.

pub mod config;
pub mod http;

pub use config::Config;
pub use http::boot;
