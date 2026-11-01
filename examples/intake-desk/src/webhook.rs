//! The autumn app: the Linear plugin and the intake handler.

use std::future::Future;
use std::pin::Pin;

use autumn_plugin_linear::testing::SignedEvent;
use autumn_plugin_linear::{LinearPlugin, WebhookContext};
use autumn_web::test::{TestApp, TestClient};
use autumn_web::{AutumnError, AutumnResult};
use harvest_camel::{CamelHandle, call_route};
use serde_json::json;

use crate::camel::{INTAKE, MESSAGE_ID};
use crate::settings::{INTAKE_LABEL, ORG};
use crate::workflow::IssueRef;

/// The future of the intake handler.
type HandlerFuture = Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>>;

/// The handler. It hands an issue that gets the intake label to harvest,
/// and returns only after harvest holds it.
fn intake_handler(
    camel: CamelHandle,
) -> impl Fn(WebhookContext) -> HandlerFuture + Send + Sync + 'static {
    move |ctx: WebhookContext| {
        let camel = camel.clone();
        Box::pin(async move {
            if !ctx.event.label_added(INTAKE_LABEL) {
                return Ok(());
            }
            // One workspace: the camel route fixes its San Mateo connection.
            if ctx.event.organization_id.as_str() != ORG {
                tracing::warn!(org = %ctx.event.organization_id, "intake desk: event from another workspace; skipped");
                return Ok(());
            }
            let issue = ctx
                .event
                .issue()
                .map_err(|err| err.to_autumn_error())?
                .ok_or_else(|| AutumnError::bad_request_msg("the event has no issue"))?;
            let message = IssueRef {
                issue_id: issue.id,
                organization_id: ctx.event.organization_id.to_string(),
            };
            let headers = [(MESSAGE_ID.to_owned(), json!(ctx.delivery_id.as_str()))];
            call_route(&camel, INTAKE, json!(message), headers)
                .await
                .map_err(|err| {
                    // Not 2xx: Linear sends the webhook again later.
                    AutumnError::internal_server_error_msg(format!(
                        "harvest did not take the issue: {err}"
                    ))
                    .with_status(autumn_web::reexports::http::StatusCode::SERVICE_UNAVAILABLE)
                })?;
            Ok(())
        })
    }
}

/// The app with the Linear plugin. `env_lookup` gives the plugin the
/// webhook secret without the process environment.
///
/// For production: build it with `autumn_web::app()` and `.run()`, and
/// point the Linear webhook at `https://<host>/linear/webhooks`.
pub(crate) fn app(camel: CamelHandle, webhook_secret: String) -> TestClient {
    let plugin = LinearPlugin::new()
        .env_lookup(move |name| (name == "LINEAR_WEBHOOK_SECRET").then(|| webhook_secret.clone()))
        .on("Issue", intake_handler(camel));
    TestApp::new().plugin(plugin).build()
}

/// POST one signed webhook. Returns the status.
pub(crate) async fn deliver(app: &TestClient, signed: &SignedEvent) -> u16 {
    let mut request = app.post("/linear/webhooks");
    for (name, value) in &signed.headers {
        request = request.header(name, value);
    }
    request
        .body(signed.body.clone())
        .send()
        .await
        .status
        .as_u16()
}

#[cfg(test)]
mod tests;
