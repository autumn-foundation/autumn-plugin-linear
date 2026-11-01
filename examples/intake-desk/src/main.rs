//! The intake desk: Linear issues flow through the whole autumn ecosystem.
//!
//! ```text
//! Linear ──signed webhook──► autumn app + LinearPlugin   (HMAC, replay window, dedupe)
//!   handler: an issue gets the "intake" label
//!     ─► camel direct:linear-intake ─► harvest:linear-intake        (durable hand-off, then 200)
//!   harvest connector: one workflow per issue ("intake-{org}-{issue id}")
//!   workflow intake_issue (harvest worker, Postgres)
//!     1. activity fetch_issue    ─► direct:linear-graphql ─► san-mateo:linear  issue(id)
//!     2. route_issue (pure)      ─► a work queue and a priority
//!     3. activity post_receipt   ─► find the receipt comment by its stable id, else create it
//!     4. activity set_priority   ─► issueUpdate(priority)
//! ```
//!
//! [`run`] below wires the parts in order. Each module holds one part:
//!
//! | Module | Part | Job |
//! |---|---|---|
//! | [`webhook`] | `autumn-plugin-linear` | Verify, dedupe, and route the webhook. Hand the issue to harvest. |
//! | [`camel`] | rust-camel | Carry messages: into harvest, and out to San Mateo. |
//! | [`harvest`] | autumn-harvest | Keep the work durable. Start one workflow per issue. |
//! | [`workflow`] | autumn-harvest | The workflow and its activities. Retry failed activities. |
//! | [`triage`] | none | The pure routing rule and the receipt key. |
//! | [`auth`] | San Mateo | Keep the Linear OAuth token and add it to each call. |
//! | [`mock_linear`] | wiremock | A local Linear GraphQL API. San Mateo calls it over real HTTP. |
//! | [`scenario`] | none | The deliveries that the example sends, and the check of the result. |
//!
//! The webhook handler returns only after harvest holds the message, so a
//! 200 means the work is safe. A second delivery for the same issue (a
//! retry with a new delivery id) maps to the same workflow id, so harvest
//! starts one workflow. A retry of `post_receipt` first looks for its
//! comment by a stable id, so Linear gets one comment.
//!
//! Failures:
//!
//! - A transient error (429, 5xx, a network error) retries with backoff:
//!   8 attempts per activity, up to about a minute. Camel drops the
//!   `Retry-After` value, so the backoff does not follow it.
//! - A permanent error (a GraphQL error, a refused token) ends the run
//!   FAILED. One run per issue: a later delivery for the issue attaches to
//!   that run. Re-drive a FAILED run with the harvest reset path, or add a
//!   reconciler. A production app needs one of the two.
//! - The example serves one workspace. The handler skips events from other
//!   workspaces, because the camel route fixes one San Mateo connection.
//!   For many workspaces, map the signed `organizationId` to a connection
//!   on the server side, never from a header.
//! - The harvest hand-off has a 4 s budget (`ackTimeout`), under Linear's
//!   5 s. A slow hand-off gives 503, and Linear retries.
//! - Production also needs a connector dead-letter sink
//!   (`PostgresDeadLetterSink`, with the harvest plugin migrations), so a
//!   bad message is recorded and not refused forever.
//!
//! For production: point San Mateo at the real Linear API (see
//! [`auth::service`]), serve the autumn app with `.run()` (see
//! [`webhook::app`]), and point the Linear webhook at
//! `https://<host>/linear/webhooks`.
//!
//! The example checks its result and returns an error when it is wrong.
//!
//! ```sh
//! DATABASE_URL=postgres://user:password@localhost/db \
//!   cargo run --locked --manifest-path examples/intake-desk/Cargo.toml
//! ```

// camel 0.57 `CamelError` is 168 bytes. The camel API returns it by value.
#![allow(clippy::result_large_err)]

mod auth;
mod camel;
mod harvest;
mod mock_linear;
mod scenario;
mod settings;
mod triage;
mod webhook;
mod workflow;

use std::time::Duration;

use autumn_harvest::migrate;
use autumn_harvest::pool::{DbRole, EngineDbTimeouts, engine_pool};
use autumn_harvest::worker::DbPool;
use camel_core::CamelContext;
use harvest_camel::{CamelSource, HarvestBridge};
use san_mateo::clock::{Entropy, OsEntropy};
use serde_json::Value;

use crate::harvest::Harvest;
use crate::scenario::Scenario;
use crate::settings::ORG;
use crate::workflow::workflow_id;

/// The error type of the example.
pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The longest time for the scenario. CI must not hang.
const RUN_TIMEOUT: Duration = Duration::from_secs(120);
/// The longest wait for the workflow. It is shorter than the run budget,
/// so that the normal path always stops every part.
const WORKFLOW_TIMEOUT: Duration = Duration::from_secs(60);

/// Run the scenario against the Postgres database at `url`, stop all
/// parts, and check the result.
async fn run(url: &str, scenario: &Scenario) -> Result<Value, BoxError> {
    tokio::time::timeout(RUN_TIMEOUT, run_unbounded(url, scenario))
        .await
        .map_err(|_| format!("the intake desk did not stop in {RUN_TIMEOUT:?}"))?
}

async fn run_unbounded(url: &str, scenario: &Scenario) -> Result<Value, BoxError> {
    // In production, apply the harvest migrations before you start.
    migrate::apply(url, &migrate::embedded()).await?;
    let pool = engine_pool(url, 8, DbRole::Hot, &EngineDbTimeouts::default())?;

    // San Mateo holds the Linear token. Camel carries messages into
    // harvest and out to San Mateo.
    let san = auth::service(&scenario.linear.uri()).await?;
    let (source, bridge) = HarvestBridge::new(camel::STREAM);
    let mut routes = camel::start(bridge, san).await?;

    let result = intake(&routes, &pool, url, source, scenario).await;

    // Stop camel in all cases. An error of the run comes first.
    let stopped = routes.stop().await;
    let summary = result?;
    stopped?;
    scenario::check(&summary, scenario)?;
    Ok(summary)
}

/// Start harvest, send the webhooks, wait for the workflow, and stop
/// harvest.
async fn intake(
    routes: &CamelContext,
    pool: &DbPool,
    url: &str,
    source: std::sync::Arc<CamelSource>,
    scenario: &Scenario,
) -> Result<Value, BoxError> {
    let harvest = Harvest::start(routes, pool, url, source).await?;
    let result = async {
        // The autumn app hands each intake issue to the camel route.
        let secret = format!("whsec_{}", OsEntropy.hex(16));
        let app = webhook::app(camel::handle(routes, camel::INTAKE)?, secret.clone());
        let statuses = scenario::deliver_all(&app, &secret, scenario).await;
        let output = harvest
            .result(&workflow_id(ORG, &scenario.issue_id), WORKFLOW_TIMEOUT)
            .await?;
        Ok::<_, BoxError>(scenario::summarize(&output, &statuses, &scenario.linear).await)
    }
    .await;
    // Stop in all cases.
    harvest.stop().await?;
    result
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let url = std::env::var("DATABASE_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .ok_or("set DATABASE_URL to a Postgres URL")?;
    let summary = run(&url, &Scenario::happy().await).await?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}
