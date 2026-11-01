//! Signed webhooks, routed by type and action.
//!
//! `POST /linear/webhooks` receives every webhook Linear sends. The
//! [`VerifiedWebhook`] extractor:
//!
//! 1. Checks the HMAC on the raw body (401).
//! 2. Parses the signed envelope (400; a bad `webhookTimestamp` is 401).
//! 3. Checks the signed `webhookTimestamp` against the replay window (401).
//! 4. Reads the `Linear-Delivery` id (400).
//!
//! The route then calls [`process`]:
//!
//! 1. Claims the delivery id. A done id gets 200 and no handler runs. An id
//!    that still runs gets 409, so that Linear tries again later.
//! 2. On `OAuthApp` `revoked`, deletes the workspace token.
//! 3. Runs the handler for `(type, action)`, else for `type`. No handler:
//!    200.
//! 4. Answers 200 on success. On a failure it releases the claim and
//!    answers with the error status (never 2xx), so that Linear retries.
//!
//! Linear waits 5 seconds. Hand slow work to a job queue or a workflow
//! engine, then return.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;

use autumn_web::reexports::axum::body::{Bytes, to_bytes};
use autumn_web::reexports::axum::extract::{FromRequest, Request};
use autumn_web::reexports::http::{HeaderMap, StatusCode};
use autumn_web::{AppState, AutumnError, AutumnResult, State, post, public, routes};
use futures::FutureExt as _;
use serde::de::DeserializeOwned;

use crate::delivery::Claim;
use crate::error::{Error, Result};
use crate::event::WebhookEvent;
use crate::plugin::{Linear, LinearRuntime, now_ms};
use crate::types::{Action, DeliveryId, EventType};
use crate::verify::{SIGNATURE_HEADER, verify_signature};

/// The largest webhook body the extractor reads: 1 MiB.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Header with the delivery id: one id per payload.
pub const DELIVERY_HEADER: &str = "linear-delivery";
/// Header with the entity type. It is not signed: routing uses the body.
pub const EVENT_HEADER: &str = "linear-event";
/// Header with the send time in ms. It is not signed: the replay check
/// uses the body.
pub const TIMESTAMP_HEADER: &str = "linear-timestamp";

/// A webhook whose HMAC and replay window verified.
///
/// Use it on a custom route. `Debug` leaves out the body.
#[non_exhaustive]
pub struct VerifiedWebhook {
    /// The `Linear-Delivery` id. It is not signed.
    pub delivery_id: DeliveryId,
    /// The signed envelope.
    pub event: WebhookEvent,
    /// The raw, verified body.
    pub body: Bytes,
    /// The plugin runtime.
    pub runtime: Arc<LinearRuntime>,
}

impl std::fmt::Debug for VerifiedWebhook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedWebhook")
            .field("delivery_id", &self.delivery_id)
            .field("type", &self.event.event_type)
            .field("action", &self.event.action)
            .field("organization_id", &self.event.organization_id)
            .field("body_len", &self.body.len())
            .finish_non_exhaustive()
    }
}

impl VerifiedWebhook {
    /// Verify the HMAC and the replay window at `now_ms`, then read the
    /// envelope and the headers.
    ///
    /// # Errors
    ///
    /// - [`Error::WebhooksDisabled`]: no signing secret.
    /// - [`Error::Signature`]: the HMAC fails.
    /// - [`Error::Replay`]: the signed time is outside the window.
    /// - [`Error::InvalidRequest`]: a bad envelope or no delivery id.
    pub fn from_parts(
        runtime: Arc<LinearRuntime>,
        headers: &HeaderMap,
        body: Bytes,
        now_ms: u64,
    ) -> Result<Self> {
        let secrets = runtime.secrets().ok_or(Error::WebhooksDisabled)?;
        // The HMAC check runs first, on the raw bytes, before any parsing.
        verify_signature(secrets, header(headers, SIGNATURE_HEADER), &body)?;
        let event = WebhookEvent::parse(&body)?;
        let tolerance = runtime.config().tolerance_ms();
        if !crate::policy::timestamp_fresh(now_ms, event.webhook_timestamp_ms, tolerance) {
            return Err(Error::Replay(format!(
                "webhookTimestamp is more than {} s from now",
                tolerance / 1000
            )));
        }
        let delivery_id =
            DeliveryId::parse(header(headers, DELIVERY_HEADER).ok_or_else(|| {
                Error::InvalidRequest(format!("missing {DELIVERY_HEADER} header"))
            })?)?;
        if let Some(named) = header(headers, EVENT_HEADER)
            && named != event.event_type.as_str()
        {
            tracing::warn!(header = named, body = %event.event_type, "linear-event header differs from the signed type; routing uses the type");
        }
        Ok(Self {
            delivery_id,
            event,
            body,
            runtime,
        })
    }

    /// Parse the body as JSON.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Json`] when the body does not match `T`.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.body).map_err(Error::Json)
    }
}

impl FromRequest<AppState> for VerifiedWebhook {
    type Rejection = AutumnError;

    async fn from_request(
        req: Request,
        state: &AppState,
    ) -> std::result::Result<Self, Self::Rejection> {
        let runtime = state
            .extension::<LinearRuntime>()
            .ok_or_else(|| Error::MissingRuntime.to_autumn_error())?;
        let (parts, body) = req.into_parts();
        let body = to_bytes(body, MAX_BODY_BYTES).await.map_err(|err| {
            if is_length_limit(&err) {
                Error::PayloadTooLarge {
                    limit: MAX_BODY_BYTES,
                }
            } else {
                Error::InvalidRequest("the webhook body does not read".to_owned())
            }
            .to_autumn_error()
        })?;
        Self::from_parts(runtime, &parts.headers, body, now_ms(state)).map_err(|err| {
            tracing::warn!(error = %err, "linear webhook rejected");
            err.to_autumn_error()
        })
    }
}

/// What a webhook handler receives.
#[derive(Clone)]
#[non_exhaustive]
pub struct WebhookContext {
    /// The `Linear-Delivery` id. It is not signed.
    pub delivery_id: DeliveryId,
    /// The signed envelope: type, action, workspace, data.
    pub event: WebhookEvent,
    /// The raw, verified body.
    pub body: Bytes,
    /// The app state: enqueue jobs, reach the database.
    pub state: AppState,
    /// The plugin handle: GraphQL clients.
    pub linear: Linear,
}

impl std::fmt::Debug for WebhookContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookContext")
            .field("delivery_id", &self.delivery_id)
            .field("type", &self.event.event_type)
            .field("action", &self.event.action)
            .field("organization_id", &self.event.organization_id)
            .field("body_len", &self.body.len())
            .finish_non_exhaustive()
    }
}

impl WebhookContext {
    /// Make a context, for example to test a handler.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`WebhookEvent::parse`].
    pub fn new(delivery_id: DeliveryId, body: impl Into<Bytes>, linear: Linear) -> Result<Self> {
        let body = body.into();
        let event = WebhookEvent::parse(&body)?;
        Ok(Self {
            delivery_id,
            event,
            body,
            state: linear.state().clone(),
            linear,
        })
    }

    /// The GraphQL client for the workspace that sent the webhook.
    ///
    /// # Errors
    ///
    /// See [`LinearRuntime::client_for`].
    pub async fn client(&self) -> Result<crate::client::LinearClient> {
        self.linear.client_for(&self.event.organization_id).await
    }

    /// Parse the body as JSON.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Json`] when the body does not match `T`.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.body).map_err(Error::Json)
    }
}

/// The future a webhook handler returns.
pub type WebhookFuture = Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>>;

/// A type-erased webhook handler.
pub type WebhookHandler = Arc<dyn Fn(WebhookContext) -> WebhookFuture + Send + Sync + 'static>;

/// Turns an async function into a [`WebhookHandler`].
pub trait IntoWebhookHandler {
    /// Erase the handler type.
    fn into_handler(self) -> WebhookHandler;
}

impl<F, Fut> IntoWebhookHandler for F
where
    F: Fn(WebhookContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = AutumnResult<()>> + Send + 'static,
{
    fn into_handler(self) -> WebhookHandler {
        Arc::new(move |ctx| Box::pin(self(ctx)))
    }
}

/// The key of a handler: a type and an optional action.
type HandlerKey = (EventType, Option<Action>);

/// Webhook handlers by type and action.
#[derive(Clone, Default)]
pub struct WebhookRegistry {
    handlers: HashMap<HandlerKey, WebhookHandler>,
}

impl std::fmt::Debug for WebhookRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookRegistry")
            .field("keys", &self.keys())
            .finish()
    }
}

impl WebhookRegistry {
    /// Make an empty registry.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Register `handler` for `event_type` and, if set, `action`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when the type or the action is malformed,
    /// or the key already has a handler.
    pub(crate) fn register(
        &mut self,
        event_type: &str,
        action: Option<&str>,
        handler: WebhookHandler,
    ) -> Result<()> {
        let event_type =
            EventType::parse(event_type).map_err(|err| Error::Config(err.to_string()))?;
        let action = action
            .map(Action::parse)
            .transpose()
            .map_err(|err| Error::Config(err.to_string()))?;
        let key = (event_type, action);
        if self.handlers.contains_key(&key) {
            return Err(Error::Config(format!(
                "webhook key {} has two handlers",
                key_text(&key)
            )));
        }
        self.handlers.insert(key, handler);
        Ok(())
    }

    /// The registered keys (`Type` or `Type:action`), sorted.
    #[must_use]
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.handlers.keys().map(key_text).collect();
        keys.sort_unstable();
        keys
    }

    /// True when no handler is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }

    /// The handler for `(event_type, action)`, else for `event_type`.
    #[must_use]
    pub fn get(&self, event_type: &EventType, action: &Action) -> Option<&WebhookHandler> {
        self.handlers
            .get(&(event_type.clone(), Some(action.clone())))
            .or_else(|| self.handlers.get(&(event_type.clone(), None)))
    }
}

/// `Type` or `Type:action`.
fn key_text((event_type, action): &HandlerKey) -> String {
    action
        .as_ref()
        .map_or_else(|| event_type.to_string(), |a| format!("{event_type}:{a}"))
}

/// What the webhook route did with one delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WebhookOutcome {
    /// A handler ran and succeeded.
    Handled,
    /// The store already has this delivery id as done. No handler ran.
    Duplicate,
    /// No handler matches.
    Unhandled,
}

/// Run one verified webhook: claim, revoke cleanup, dispatch.
///
/// The plugin route calls this. Call it from a custom route that uses
/// [`VerifiedWebhook`].
///
/// The handler runs in its own task. A dropped request (timeout, closed
/// connection) does not stop the handler. The handler result sets the
/// claim: success marks it done, and a failure or a panic releases it, so
/// that the next Linear retry runs the handler again.
///
/// # Errors
///
/// Returns [`Error::InProgress`] (409) when another delivery of the id
/// runs. Returns the handler error when the handler fails. Returns 500
/// when the handler panics or a store fails.
pub async fn process(state: &AppState, webhook: VerifiedWebhook) -> AutumnResult<WebhookOutcome> {
    let runtime = Arc::clone(&webhook.runtime);
    let claimed_at = now_ms(state);
    let delivery = webhook.delivery_id.to_string();
    let claim = runtime
        .delivery_store()
        .claim(&delivery, claimed_at)
        .await
        .map_err(|err| err.to_autumn_error())?;
    match claim {
        Claim::Fresh => {}
        Claim::Done => {
            tracing::info!(delivery = %delivery, "linear webhook is a duplicate; skipped");
            return Ok(WebhookOutcome::Duplicate);
        }
        _ => return Err(Error::InProgress(delivery).to_autumn_error()),
    }
    let state = state.clone();
    let task = tokio::spawn(async move {
        let deliveries = Arc::clone(webhook.runtime.delivery_store());
        let outcome = AssertUnwindSafe(dispatch(&state, webhook))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| {
                tracing::error!(delivery = %delivery, "linear webhook handler panicked");
                Err(AutumnError::internal_server_error_msg(
                    "the linear webhook handler panicked",
                ))
            });
        let settled = if outcome.is_ok() {
            deliveries.complete(&delivery, claimed_at).await
        } else {
            deliveries.release(&delivery, claimed_at).await
        };
        if let Err(err) = settled {
            tracing::error!(error = %err, delivery = %delivery, "linear webhook claim update failed");
        }
        outcome
    });
    task.await.unwrap_or_else(|_| {
        Err(AutumnError::internal_server_error_msg(
            "the linear webhook task stopped",
        ))
    })
}

/// Revoke cleanup, then the handler for the type and action.
async fn dispatch(state: &AppState, webhook: VerifiedWebhook) -> AutumnResult<WebhookOutcome> {
    let runtime = Arc::clone(&webhook.runtime);
    let event = &webhook.event;
    if event.is(EventType::OAUTH_APP) && event.action.as_str() == Action::REVOKED {
        revoke_cleanup(&runtime, event)
            .await
            .map_err(|err| err.to_autumn_error())?;
    }
    let Some(handler) = runtime.registry().get(&event.event_type, &event.action) else {
        tracing::debug!(r#type = %event.event_type, action = %event.action, "linear webhook has no handler; answered 200");
        return Ok(WebhookOutcome::Unhandled);
    };
    let label = format!("{}:{}", event.event_type, event.action);
    let org = event.organization_id.clone();
    let ctx = WebhookContext {
        delivery_id: webhook.delivery_id,
        event: webhook.event,
        body: webhook.body,
        state: state.clone(),
        linear: Linear::from_runtime(Arc::clone(&runtime), state.clone()),
    };
    handler(ctx).await.map_err(|err| {
        tracing::warn!(event = %label, org = %org, error = %err, "linear webhook handler failed");
        err
    })?;
    Ok(WebhookOutcome::Handled)
}

/// Delete the workspace token on `OAuthApp` `revoked`.
///
/// - A revoke for another OAuth client changes nothing.
/// - With a signed `createdAt`, a token from a later install stays. The
///   time is the same on every retry of the event.
/// - With no `createdAt`, the plugin asks Linear whether the token still
///   works. The send time of a retry is no event time: it is later than a
///   reinstall, and a new, valid token would go.
async fn revoke_cleanup(runtime: &LinearRuntime, event: &WebhookEvent) -> Result<()> {
    let org = &event.organization_id;
    let client = event
        .raw
        .get("oauthClientId")
        .and_then(serde_json::Value::as_str);
    if let (Some(client), Some(ours)) = (client, runtime.config().client_id.as_deref())
        && client != ours
    {
        tracing::warn!(org = %org, "linear revoke for another OAuth client; token kept");
        return Ok(());
    }
    let deleted = match event.created_at.as_deref().and_then(parse_time_ms) {
        Some(event_at) => {
            runtime
                .keeper()
                .delete_for_event(org, Some(event_at))
                .await?
        }
        None => runtime.keeper().delete_if_dead(org).await?,
    };
    if deleted {
        tracing::info!(org = %org, "linear app revoked; token deleted");
    } else {
        tracing::info!(org = %org, "linear token still works or is from a later install; kept");
    }
    Ok(())
}

/// Parse an RFC 3339 time into Unix ms.
fn parse_time_ms(text: &str) -> Option<u64> {
    autumn_web::reexports::chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
}

/// True when `err` comes from the body length limit.
fn is_length_limit(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(err);
    while let Some(err) = current {
        if err.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        current = err.source();
    }
    false
}

/// The header value. `None` when it is missing, blank, or not ASCII.
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)?
        .to_str()
        .ok()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// The plugin webhook route.
#[post("/webhooks")]
#[public]
async fn webhooks_route(
    State(state): State<AppState>,
    webhook: VerifiedWebhook,
) -> AutumnResult<StatusCode> {
    process(&state, webhook).await?;
    Ok(StatusCode::OK)
}

/// The webhook route, for the plugin to mount under `/linear`.
pub(crate) fn webhook_routes() -> Vec<autumn_web::Route> {
    routes![webhooks_route]
}

#[cfg(test)]
mod tests;
