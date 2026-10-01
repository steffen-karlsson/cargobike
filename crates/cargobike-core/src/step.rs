//! The step-type abstraction (PRD §6.4, F-33..F-39) and the SSRF-safe
//! HTTP seam steps use ([`HttpService`]).
//!
//! `StepContext` must compile to `wasm32-wasip2` with the rest of core
//! (A.14): the HTTP client is an abstract seam (`HttpService`), the
//! cancellation token is a cooperative flag, and logging is a
//! `tracing::Span`. The engine wires concrete implementations (reqwest
//! behind the guard, tokio tooling) at construction time (§6.7).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::Span;

use crate::registry::ProviderRegistry;

use crate::provider::HttpError;

pub use crate::provider::HttpError as StepHttpError;

/// Cooperative cancellation token handed to steps (F-39, §6.7).
///
/// The engine pairs this with its own `tokio_util` token: when a release is
/// cancelled, the flag flips and steps observe it at their next await point
/// or boundary check. `wait`-style polling IDs — simplest form available
/// without tokio in core.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A token that is not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Signals cancellation to all holders.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether cancellation has been signalled.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// An outbound HTTP request issued by a step (`builtin/http-call@1`, F-144).
#[derive(Clone, Debug)]
pub struct HttpRequest {
    /// Absolute URL; the engine's guard applies `network.egress` policy before
    /// connecting (F-117), so a blocked host fails before I/O.
    pub url: String,
    /// HTTP method (uppercase).
    pub method: String,
    /// Headers; values with `{ secret: <name> }` references are resolved by
    /// the engine before the request is sent (F-144, F-146).
    pub headers: Vec<(String, String)>,
    /// Request body, when any.
    pub body: Option<Vec<u8>>,
}

/// The response of an [`HttpService::send`].
#[derive(Clone, Debug)]
pub struct HttpResponse {
    /// Response status code.
    pub status: u16,
    /// Response headers.
    pub headers: Vec<(String, String)>,
    /// Response body.
    pub body: Vec<u8>,
}

/// The HTTP seam steps call through (F-39 `http_client`). Implemented in
/// the engine by the SSRF-guarded reqwest client (F-117); the sidecar
/// client is a *separate* implementation so `http-call` can never reach a
/// sidecar endpoint (§6.7).
#[async_trait]
pub trait HttpService: Send + Sync {
    /// Sends the request, applying the egress policy of the instance.
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpError>;
}

/// What a step produces (PRD §6.4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "outcome")]
pub enum StepOutput {
    /// Payload later steps reference as `steps.<id>.outputs.<field>` (F-28).
    Continue(Value),
    /// Only valid before any side effect; marks the environment `Skipped` (F-7).
    SkipEnvironment,
    /// Fail the environment with the given reason (F-41's stop path).
    Stop(StepFailureReason),
}

/// The payload of [`StepOutput::Continue`].
pub type Value = serde_json::Value;

/// A stable string reason resource for step failure.
pub type StepFailureReason = String;

/// Errors a step body reports to the interpreter.
#[derive(Clone, Debug, PartialEq, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case", tag = "failure")]
pub enum StepError {
    /// The step failed transiently; `StepOptions` retries apply (F-35).
    #[error("step failed transiently: {0}")]
    Transient(String),
    /// The step failed permanently; the release error carries `code`.
    #[error("{code}: {message}")]
    Failed {
        /// One of the code constants from [`crate::error`] (F-8).
        code: String,
        /// Human-readable message.
        message: String,
    },
    /// The step observed cancellation and abandoned its work (F-39).
    #[error("step was cancelled")]
    Cancelled,
}

/// The environment's provisioned view for a step run (F-32a's
/// well-known values as the snapshot supplies them).
#[derive(Clone, Debug)]
pub struct EnvRef {
    /// Environment name.
    pub name: String,
    /// Target repo (F-10/F-11: opaque, resolved in the registry).
    pub repo: Option<crate::model::RepoRef>,
    /// Structural edits for `commit-files` (F-41).
    pub edits: Vec<crate::provider::Edit>,
    /// The registry's commit message (F-143); placeholders resolved at
    /// provision time.
    pub commit_message: Option<String>,
    /// The engine's branch format, stamped by the provisioner (A1).
    pub branch_format: Option<String>,
    /// The release's version string (F-147's edit default: an edit
    /// without an explicit value writes the version; providers can't
    /// know it otherwise).
    pub release_version: String,
}

impl EnvRef {
    /// An environment view without a provisioned repo (templates whose
    /// steps take their own repo parameter).
    pub fn named(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            repo: None,
            edits: Vec::new(),
            commit_message: None,
            branch_format: None,
            release_version: String::new(),
        }
    }
}

/// One executable step type, versioned (F-36, F-37). The trait keeps the
/// types typed at the call site; the interpreter registers instances
/// under `builtin/<name>@<version>` or sidecar-derived names (F-33).
#[async_trait]
pub trait StepType: Send + Sync {
    /// Registry name, e.g. `builtin/commit-files`; the `@version` suffix in
    /// templates resolves against [`StepType::version`].
    fn name(&self) -> &str;
    /// Version of the step type's output schema (F-37 `@1`).
    fn version(&self) -> &str;
    /// Executes the step. Side effects must be idempotent (F-14, A1); the
    /// step may run again on recovery with a previously recorded result.
    async fn execute(
        &self,
        ctx: &StepContext,
        release: &crate::model::Release,
        env: &EnvRef,
        params: &serde_json::Value,
    ) -> Result<StepOutput, StepError>;
}

/// Everything a step receives from the engine (F-39, §6.7).
pub struct StepContext {
    /// Providers, resolved by `RepoRef.provider` name (F-39).
    pub providers: ProviderRegistry,
    /// SSRF-guarded HTTP client (F-117); never the sidecar's client (§6.7).
    pub http: Arc<dyn HttpService>,
    /// Named secrets resolved from the `secrets:` section (F-146).
    pub credentials: Arc<dyn crate::registry::CredentialStore>,
    /// `release_id/environment/step_id` — the idempotency inputs (F-39, A1).
    pub idempotency_key: String,
    /// Cooperative cancellation flag (F-39).
    pub cancel_token: CancelToken,
    /// Span to log inside; `release_id`, `workflow_id`, `environment` (A.10).
    pub log: Span,
}

impl std::fmt::Debug for StepContext {
    /// Hand-written Debug: never leaks provider or credential internals (F-87).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StepContext")
            .field("providers", &self.providers.len())
            .field("http", &"<ssrf-guarded client>")
            .field("credentials", &"<credential store>")
            .field("idempotency_key", &self.idempotency_key)
            .field("cancel_token", &self.cancel_token)
            .finish_non_exhaustive()
    }
}
