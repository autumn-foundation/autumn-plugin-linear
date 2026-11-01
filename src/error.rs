//! Error types for the Linear plugin.
//!
//! [`Error`] is the plugin error. [`ErrorKind`] classifies it.
//! [`Error::to_autumn_error`] maps each kind to an HTTP status.
//!
//! Do not use `?` to change an [`Error`] into an [`AutumnError`]. Autumn has
//! a blanket `From` impl that gives 500 for every error. Call
//! [`Error::to_autumn_error`].

use std::time::Duration;

use autumn_web::AutumnError;
use autumn_web::reexports::http::StatusCode;
use thiserror::Error;

/// Classifies an [`Error`] without its payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The `[linear]` config is missing or not valid.
    Config,
    /// A signature is missing, malformed, or wrong.
    Signature,
    /// The signed webhook time is outside the replay window.
    Replay,
    /// The request is malformed (headers, query, or ids).
    InvalidRequest,
    /// The request body is larger than the limit.
    PayloadTooLarge,
    /// A JSON payload does not parse.
    Json,
    /// A Linear call failed at the transport level.
    Http,
    /// Linear answered with a non-success status.
    LinearApi,
    /// The GraphQL response has errors.
    GraphQl,
    /// Linear rate-limited the call and the retries ran out.
    RateLimited,
    /// Linear answered 200, but the body has the wrong shape.
    UnexpectedResponse,
    /// Another delivery of this webhook still runs.
    InProgress,
    /// The OAuth state does not match the browser cookie.
    StateMismatch,
    /// The user or Linear refused the authorization.
    AuthorizationDenied,
    /// The workspace has no stored token.
    NotInstalled,
    /// The plugin cannot refresh the token. The workspace must install the app again.
    Reauthorize,
    /// OAuth is off (`[linear] app_url` is not set).
    OAuthDisabled,
    /// Webhooks are off (`LINEAR_WEBHOOK_SECRET` is not set).
    WebhooksDisabled,
    /// A token or delivery store failed.
    Store,
    /// The app does not use the plugin (no runtime in the app state).
    MissingRuntime,
}

/// The plugin error type.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// The `[linear]` config is missing or not valid.
    #[error("linear config error: {0}")]
    Config(String),

    /// A signature is missing, malformed, or wrong.
    #[error("linear signature error: {0}")]
    Signature(String),

    /// The signed webhook time is outside the replay window.
    #[error("linear webhook is outside the replay window: {0}")]
    Replay(String),

    /// The request is malformed.
    #[error("linear invalid request: {0}")]
    InvalidRequest(String),

    /// The request body is larger than the limit.
    #[error("linear request body is larger than {limit} bytes")]
    PayloadTooLarge {
        /// The limit in bytes.
        limit: usize,
    },

    /// A JSON payload does not parse.
    #[error("linear json error: {0}")]
    Json(#[from] serde_json::Error),

    /// A Linear call failed at the transport level.
    #[error("linear http error: {0}")]
    Http(#[from] reqwest::Error),

    /// Linear answered with a non-success status.
    #[error("linear api error: status {status}: {body}")]
    LinearApi {
        /// The HTTP status from Linear.
        status: u16,
        /// The response body, truncated to 500 chars.
        body: String,
    },

    /// The GraphQL response has errors.
    #[error("linear graphql error: {}", .0.join("; "))]
    GraphQl(Vec<String>),

    /// Linear rate-limited the call and the retries ran out.
    #[error("linear rate-limited the request after {retries} retries")]
    RateLimited {
        /// The number of retries done.
        retries: u32,
        /// The wait Linear asked for, if it sent one.
        retry_after: Option<Duration>,
    },

    /// Linear answered 200, but the body has the wrong shape.
    #[error("linear response has the wrong shape: {0}")]
    UnexpectedResponse(String),

    /// Another delivery of this webhook still runs.
    #[error("linear webhook delivery {0} is in progress")]
    InProgress(String),

    /// The OAuth state does not match the browser cookie.
    #[error("linear oauth state does not match")]
    StateMismatch,

    /// The user or Linear refused the authorization.
    #[error("linear authorization refused: {0}")]
    AuthorizationDenied(String),

    /// The workspace has no stored token.
    #[error("linear workspace {0} has no stored token; install the app")]
    NotInstalled(String),

    /// The plugin cannot refresh the token. The workspace must install the app again.
    #[error("linear workspace {0} must install the app again")]
    Reauthorize(String),

    /// OAuth is off.
    #[error("linear oauth is off; set [linear] app_url")]
    OAuthDisabled,

    /// Webhooks are off.
    #[error("linear webhooks are off; set LINEAR_WEBHOOK_SECRET")]
    WebhooksDisabled,

    /// A token or delivery store failed.
    #[error("linear store error: {0}")]
    Store(String),

    /// The app does not use the plugin (no runtime in the app state).
    #[error("linear runtime is missing; add LinearPlugin to the app")]
    MissingRuntime,
}

impl Error {
    /// Classify this error.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        match self {
            Self::Config(_) => ErrorKind::Config,
            Self::Signature(_) => ErrorKind::Signature,
            Self::Replay(_) => ErrorKind::Replay,
            Self::InvalidRequest(_) => ErrorKind::InvalidRequest,
            Self::PayloadTooLarge { .. } => ErrorKind::PayloadTooLarge,
            Self::Json(_) => ErrorKind::Json,
            Self::Http(_) => ErrorKind::Http,
            Self::LinearApi { .. } => ErrorKind::LinearApi,
            Self::GraphQl(_) => ErrorKind::GraphQl,
            Self::RateLimited { .. } => ErrorKind::RateLimited,
            Self::UnexpectedResponse(_) => ErrorKind::UnexpectedResponse,
            Self::InProgress(_) => ErrorKind::InProgress,
            Self::StateMismatch => ErrorKind::StateMismatch,
            Self::AuthorizationDenied(_) => ErrorKind::AuthorizationDenied,
            Self::NotInstalled(_) => ErrorKind::NotInstalled,
            Self::Reauthorize(_) => ErrorKind::Reauthorize,
            Self::OAuthDisabled => ErrorKind::OAuthDisabled,
            Self::WebhooksDisabled => ErrorKind::WebhooksDisabled,
            Self::Store(_) => ErrorKind::Store,
            Self::MissingRuntime => ErrorKind::MissingRuntime,
        }
    }

    /// The HTTP status this error maps to.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        match self.kind() {
            ErrorKind::Signature
            | ErrorKind::Replay
            | ErrorKind::NotInstalled
            | ErrorKind::Reauthorize => StatusCode::UNAUTHORIZED,
            ErrorKind::InvalidRequest | ErrorKind::Json => StatusCode::BAD_REQUEST,
            ErrorKind::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ErrorKind::StateMismatch | ErrorKind::AuthorizationDenied => StatusCode::FORBIDDEN,
            ErrorKind::OAuthDisabled | ErrorKind::WebhooksDisabled => StatusCode::NOT_FOUND,
            ErrorKind::InProgress => StatusCode::CONFLICT,
            ErrorKind::Http
            | ErrorKind::LinearApi
            | ErrorKind::GraphQl
            | ErrorKind::UnexpectedResponse => StatusCode::BAD_GATEWAY,
            ErrorKind::RateLimited | ErrorKind::MissingRuntime => StatusCode::SERVICE_UNAVAILABLE,
            ErrorKind::Config | ErrorKind::Store => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// True when a later try of the same call can succeed.
    ///
    /// Rate limits, transport errors, Linear 5xx answers, and in-flight
    /// deliveries are transient. Use this rule to tell a job queue or a
    /// workflow engine to retry. A retry of a mutation after a transport
    /// error or a 5xx can run it two times: give the mutation a client id.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        match self {
            Self::RateLimited { .. } | Self::Http(_) | Self::InProgress(_) => true,
            Self::LinearApi { status, .. } => *status >= 500,
            _ => false,
        }
    }

    /// Convert into an [`AutumnError`] with the status from
    /// [`status`](Self::status).
    #[must_use]
    pub fn to_autumn_error(&self) -> AutumnError {
        AutumnError::internal_server_error_msg(self.to_string()).with_status(self.status())
    }
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests;
