//! The intake desk: Linear issues flow through the whole autumn ecosystem.
//!
//! ```text
//! Linear ──signed webhook──► autumn app + LinearPlugin   (HMAC, replay window, dedupe)
//!   handler: an issue gets the "intake" label
//!     ─► camel direct:linear-intake ─► harvest:linear-intake        (durable hand-off, then 200)
//!   harvest connector: one workflow per issue ("intake-{issue id}")
//!   workflow intake_issue (harvest worker, Postgres)
//!     1. activity fetch_issue    ─► direct:linear-graphql ─► san-mateo:linear  issue(id)
//!     2. route_issue (pure)      ─► a work queue and a priority
//!     3. activity post_receipt   ─► find the receipt comment by its stable id, else create it
//!     4. activity set_priority   ─► issueUpdate(priority)
//! ```
//!
//! Each piece has one job:
//!
//! | Piece | Job |
//! |---|---|
//! | `autumn-plugin-linear` | Verify, dedupe, and route the webhook. Build the GraphQL operations and read the answers. |
//! | rust-camel | Carry messages: into harvest, and out to San Mateo. |
//! | autumn-harvest | Keep the work durable. Start one workflow per issue. Retry failed activities. |
//! | San Mateo | Keep the Linear OAuth token and add it to each call. |
//!
//! The webhook handler returns only after harvest holds the message, so a
//! 200 means the work is safe. A second delivery for the same issue (a
//! retry with a new delivery id) maps to the same workflow id, so harvest
//! starts one workflow. A retry of `post_receipt` first looks for its
//! comment by a stable id, so Linear gets one comment.
//!
//! `MemoryTransport` is the fake Linear API. For production: remove
//! `.transport(..)`, use `PgStore` in San Mateo, connect the workspace with a
//! San Mateo connect session, serve the autumn app with `.run()`, and point
//! the Linear webhook at `https://<host>/linear/webhooks`.
//!
//! The example checks its result and returns an error when it is wrong.
//!
//! ```sh
//! DATABASE_URL=postgres://user:password@localhost/db \
//!   cargo run --locked --manifest-path examples/intake-desk/Cargo.toml
//! ```

// camel 0.57 `CamelError` is 168 bytes. The camel API returns it by value.
#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use autumn_harvest::execution::resolve_execution_id_by_workflow_id;
use autumn_harvest::failure::ActivityFailure;
use autumn_harvest::handle::WorkflowHandleClient;
use autumn_harvest::migrate;
use autumn_harvest::pool::{DbRole, EngineDbTimeouts, engine_pool};
use autumn_harvest::prelude::{ActivityContext, RetryPolicy, WorkflowContext, activity, workflow};
use autumn_harvest::telemetry::NoOpMetrics;
use autumn_harvest::worker::DbPool;
use autumn_harvest_plugin::api::HarvestApiState;
use autumn_harvest_plugin::connector::{ConnectorRuntime, MappedMessage, SourceBinding};
use autumn_harvest_plugin::{
    HarvestDatabaseConfig, HarvestMode, HarvestRunner, HarvestRunnerResources, HarvestRuntimeConfig,
};
use autumn_plugin_linear::graphql::{
    Comment, CommentCreate, Connection, Issue, IssueUpdate, Operation, ops, parse_response,
    stable_uuid,
};
use autumn_plugin_linear::testing::{EventFixture, SignedEvent};
use autumn_plugin_linear::{LinearPlugin, WebhookContext};
use autumn_web::AutumnError;
use autumn_web::test::{TestApp, TestClient};
use camel_api::CamelError;
use camel_builder::{RouteBuilder, StepAccumulator};
use camel_component_direct::DirectComponent;
use camel_core::{CamelContext, RouteDefinition};
use harvest_camel::{CamelHandle, HarvestBridge, call_route, call_route_from_activity, check_route};
use san_mateo::clock::{Entropy, OsEntropy};
use san_mateo::credentials::OAuth2Credentials;
use san_mateo::transport::HttpResponse;
use san_mateo::{
    Catalog, Credentials, EnvironmentConfig, ImportConnection, KeyRing, MemoryTransport,
    NewIntegration, SanMateo, Secret,
};
use san_mateo_camel::SanMateoComponent;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The longest time for the scenario. CI must not hang.
const RUN_TIMEOUT: Duration = Duration::from_secs(120);

const ENV: &str = "dev";
/// The San Mateo integration key. It uses the built-in `linear` provider.
const INTEGRATION: &str = "linear";
/// The San Mateo connection of the Linear workspace.
const CONNECTION: &str = "acme-workspace";
/// The Linear workspace id.
const ORG: &str = "9cfb482a-81e3-4154-b5b9-2c805e70a02d";
/// The id of the "intake" label in the workspace.
const INTAKE_LABEL: &str = "l-intake";
/// The camel route into harvest.
const INTAKE: &str = "direct:linear-intake";
/// The camel route out to Linear.
const GRAPHQL: &str = "direct:linear-graphql";
/// The harvest stream.
const STREAM: &str = "linear-intake";
/// The header with the message id. Harvest uses it to find redeliveries.
const MESSAGE_ID: &str = "MessageId";
/// The Linear GraphQL URL, as San Mateo calls it.
const GRAPHQL_URL: &str = "https://api.linear.app/graphql";
/// The OAuth token that San Mateo keeps for the workspace.
const ACCESS_TOKEN: &str = "lin-oauth-example";

// ───────────────────────────── the triage rule ─────────────────────────────

/// Where an issue goes. In an agent mesh, this is where a dispatcher picks
/// a worker. Here it is a fixed rule, so the workflow stays deterministic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Routing {
    queue: String,
    priority: u8,
}

/// Route an issue by the words in its title and description.
fn route_issue(issue: &Issue) -> Routing {
    let text = format!(
        "{} {}",
        issue.title.as_deref().unwrap_or_default(),
        issue.description.as_deref().unwrap_or_default()
    )
    .to_lowercase();
    let has = |words: &[&str]| words.iter().any(|word| text.contains(word));
    let (queue, priority) = if has(&["outage", "down", "crash", "data loss"]) {
        ("incident", 1)
    } else if has(&["bug", "error", "broken", "fails"]) {
        ("bug", 2)
    } else if has(&["how do", "how to", "question", "?"]) {
        ("question", 4)
    } else {
        ("feature", 3)
    };
    Routing {
        queue: queue.to_owned(),
        priority,
    }
}

/// The receipt comment for an issue.
fn receipt_body(routing: &Routing) -> String {
    format!(
        "Intake desk: routed to the **{}** queue with priority {}.",
        routing.queue, routing.priority
    )
}

/// The work key of the receipt. Each retry derives the same comment id.
fn receipt_key(issue_id: &str) -> String {
    format!("intake-desk:receipt:{issue_id}")
}

// ─────────────────────────── harvest: workflow ───────────────────────────

/// The workflow. Workflow code must be deterministic, so only the
/// activities call Linear.
#[workflow]
async fn intake_issue(ctx: &WorkflowContext, input: Value) -> Result<Value, String> {
    let issue: Value = ctx
        .execute_activity(&fetch_issue_info(), input)
        .await
        .map_err(|e| e.to_string())?;
    let issue: Issue = serde_json::from_value(issue).map_err(|e| e.to_string())?;
    let routing = route_issue(&issue);
    let step = json!({"issue_id": issue.id, "routing": routing});
    let receipt: Value = ctx
        .execute_activity(&post_receipt_info(), step.clone())
        .await
        .map_err(|e| e.to_string())?;
    let _: Value = ctx
        .execute_activity(&set_priority_info(), step)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "issue": issue.identifier,
        "queue": routing.queue,
        "priority": routing.priority,
        "receipt": receipt["id"],
        "receipt_created": receipt["created"],
    }))
}

/// Read the issue fresh: the webhook data can be old by now.
#[activity(start_to_close = "30s", retry = RetryPolicy::exponential(3, Duration::from_millis(200)))]
async fn fetch_issue(ctx: &ActivityContext, input: Value) -> Result<Value, ActivityFailure> {
    let issue_id = text(&input, "issue_id")?;
    run_op(ctx, &ops::issue(issue_id)).await
}

/// Post the receipt one time. A retry finds the comment by its stable id.
#[activity(start_to_close = "30s", retry = RetryPolicy::exponential(3, Duration::from_millis(200)))]
async fn post_receipt(ctx: &ActivityContext, input: Value) -> Result<Value, ActivityFailure> {
    let issue_id = text(&input, "issue_id")?;
    let routing: Routing = serde_json::from_value(input["routing"].clone())
        .map_err(|e| ActivityFailure::non_retryable("intake.Input", e.to_string()))?;
    let key = receipt_key(issue_id);
    let found: Connection<Comment> = run_op(ctx, &ops::find_comment(&stable_uuid(&key))).await?;
    if let Some(comment) = found.nodes.into_iter().next() {
        return Ok(json!({"id": comment.id, "created": false}));
    }
    let input = CommentCreate::new(issue_id, receipt_body(&routing)).with_stable_id(&key);
    let comment: Comment = run_op(ctx, &ops::create_comment(&input)).await?;
    Ok(json!({"id": comment.id, "created": true}))
}

/// Set the priority. A second run sets the same value.
#[activity(start_to_close = "30s", retry = RetryPolicy::exponential(3, Duration::from_millis(200)))]
async fn set_priority(ctx: &ActivityContext, input: Value) -> Result<Value, ActivityFailure> {
    let issue_id = text(&input, "issue_id")?;
    let routing: Routing = serde_json::from_value(input["routing"].clone())
        .map_err(|e| ActivityFailure::non_retryable("intake.Input", e.to_string()))?;
    let update = IssueUpdate {
        priority: Some(routing.priority),
        ..IssueUpdate::default()
    };
    let issue: Issue = run_op(ctx, &ops::update_issue(issue_id, &update)).await?;
    Ok(json!({"id": issue.id}))
}

/// Send one operation through camel and San Mateo. Read the answer with
/// the plugin parser, so that GraphQL errors and rate limits keep their
/// meaning.
async fn run_op<T: serde::de::DeserializeOwned>(
    ctx: &ActivityContext,
    op: &Operation,
) -> Result<T, ActivityFailure> {
    let reply = call_route_from_activity(ctx, GRAPHQL, op.body()).await?;
    let bytes = serde_json::to_vec(&reply)
        .map_err(|e| ActivityFailure::non_retryable("intake.Reply", e.to_string()))?;
    parse_response(200, None, &bytes)
        .and_then(|data| op.extract(data))
        .map_err(|err| {
            let kind = format!("linear.{:?}", err.kind());
            if err.is_transient() {
                ActivityFailure::retryable(kind, err.to_string())
            } else {
                ActivityFailure::non_retryable(kind, err.to_string())
            }
        })
}

/// A string field of an activity input.
fn text<'a>(input: &'a Value, field: &str) -> Result<&'a str, ActivityFailure> {
    input[field]
        .as_str()
        .ok_or_else(|| ActivityFailure::non_retryable("intake.Input", format!("no {field}")))
}

// ─────────────────────────── autumn: the webhook ───────────────────────────

/// The webhook handler. It hands an issue that gets the intake label to
/// harvest, and returns after harvest holds it.
fn intake_handler(
    camel: CamelHandle,
) -> impl Fn(WebhookContext) -> std::pin::Pin<Box<dyn Future<Output = autumn_web::AutumnResult<()>> + Send>>
+ Send
+ Sync
+ 'static {
    move |ctx: WebhookContext| {
        let camel = camel.clone();
        Box::pin(async move {
            if !ctx.event.label_added(INTAKE_LABEL) {
                return Ok(());
            }
            let issue = ctx
                .event
                .issue()
                .map_err(|err| err.to_autumn_error())?
                .ok_or_else(|| AutumnError::bad_request_msg("the event has no issue"))?;
            let message = json!({
                "issue_id": issue.id,
                "organization_id": ctx.event.organization_id,
            });
            let headers = [(MESSAGE_ID.to_owned(), json!(ctx.delivery_id.as_str()))];
            call_route(&camel, INTAKE, message, headers)
                .await
                .map_err(|err| {
                    // Not 2xx: Linear sends the webhook again later.
                    AutumnError::internal_server_error_msg(format!("harvest did not take the issue: {err}"))
                        .with_status(autumn_web::reexports::http::StatusCode::SERVICE_UNAVAILABLE)
                })?;
            Ok(())
        })
    }
}

/// The autumn app with the Linear plugin. `env_lookup` gives the plugin the
/// webhook secret without the process environment.
fn autumn_app(camel: CamelHandle, webhook_secret: String) -> TestClient {
    let plugin = LinearPlugin::new()
        .env_lookup(move |name| (name == "LINEAR_WEBHOOK_SECRET").then(|| webhook_secret.clone()))
        .on("Issue", intake_handler(camel));
    TestApp::new().plugin(plugin).build()
}

/// POST one signed webhook. Returns the status.
async fn deliver(app: &TestClient, signed: &SignedEvent) -> u16 {
    let mut request = app.post("/linear/webhooks");
    for (name, value) in &signed.headers {
        request = request.header(name, value);
    }
    request.body(signed.body.clone()).send().await.status.as_u16()
}

// ─────────────────────────── camel: the routes ───────────────────────────

/// The camel routes. Do not add camel redelivery: harvest retries.
fn routes() -> Result<Vec<RouteDefinition>, CamelError> {
    let intake = RouteBuilder::from(INTAKE)
        .route_id("linear-intake")
        .to(format!("harvest:{STREAM}?idHeader={MESSAGE_ID}"))
        .build()?;
    // The URI fixes the connection. `allowHeaderOverrides` stays `false`,
    // so a header cannot change the workspace.
    let graphql = RouteBuilder::from(GRAPHQL)
        .route_id("linear-graphql")
        .to(format!(
            "san-mateo:{INTEGRATION}?connectionId={CONNECTION}&method=POST&path=/graphql"
        ))
        .build()?;
    Ok(vec![intake, graphql])
}

// ─────────────────────────── San Mateo ───────────────────────────

/// The service with the Linear integration and one workspace connection.
async fn san_mateo(fake: &MemoryTransport) -> Result<SanMateo, BoxError> {
    let mut key = [0_u8; 32];
    OsEntropy.fill(&mut key);
    let san = SanMateo::builder()
        .environment(EnvironmentConfig::new(ENV, format!("sk_example_{}", OsEntropy.hex(16))))
        .key_ring(KeyRing::new("k1", key)?)
        .catalog(Catalog::builtin()?)
        .transport(Arc::new(fake.clone()))
        .build()?;
    san.create_integration(
        ENV,
        NewIntegration {
            unique_key: INTEGRATION.to_owned(),
            provider: INTEGRATION.to_owned(),
            client_id: Some("linear-client".to_owned()),
            client_secret: Some(Secret::new(OsEntropy.hex(16))),
            ..NewIntegration::default()
        },
    )
    .await?;
    // A token with no expiry: San Mateo does not refresh it here. A real
    // connection refreshes before it expires.
    san.import_connection(
        ENV,
        ImportConnection {
            connection_id: Some(CONNECTION.to_owned()),
            provider_config_key: INTEGRATION.to_owned(),
            credentials: Credentials::OAuth2(OAuth2Credentials {
                access_token: Secret::new(ACCESS_TOKEN),
                refresh_token: None,
                expires_at: None,
                raw: Value::Null,
            }),
            connection_config: Map::new(),
            metadata: Map::new(),
            tags: san_mateo::Tags::new(),
            end_user: None,
            webhook_url_override: None,
        },
    )
    .await?;
    Ok(san)
}

// ─────────────────────────── the fake Linear API ───────────────────────────

/// A GraphQL `data` answer.
fn data(value: &Value) -> HttpResponse {
    HttpResponse::json(200, &json!({ "data": value }))
}

/// The issue as Linear returns it.
fn issue_node(issue_id: &str) -> Value {
    json!({
        "id": issue_id,
        "identifier": "ENG-42",
        "title": "Checkout crash after the update",
        "description": "The app crashes when I press Pay.",
        "priority": 0,
        "labels": {"nodes": [{"id": INTAKE_LABEL, "name": "intake"}]},
    })
}

/// The fake Linear API for a happy run: the issue, no receipt yet, the new
/// receipt, the updated issue.
fn fake_linear(issue_id: &str) -> MemoryTransport {
    let fake = MemoryTransport::new();
    let receipt = json!({"id": stable_uuid(&receipt_key(issue_id)), "body": "Intake desk: ..."});
    fake.reply("POST", GRAPHQL_URL, data(&json!({"issue": issue_node(issue_id)})));
    fake.reply("POST", GRAPHQL_URL, data(&json!({"comments": {"nodes": []}})));
    fake.reply(
        "POST",
        GRAPHQL_URL,
        data(&json!({"commentCreate": {"success": true, "comment": receipt}})),
    );
    fake.reply(
        "POST",
        GRAPHQL_URL,
        data(&json!({"issueUpdate": {"success": true, "issue": issue_node(issue_id)}})),
    );
    fake
}

// ─────────────────────────── wiring ───────────────────────────

/// One run: the issue id and the fake Linear API.
struct Scenario {
    issue_id: String,
    fake: MemoryTransport,
}

impl Scenario {
    /// A happy run with a new issue id.
    fn happy() -> Self {
        let issue_id = format!("i-{}", OsEntropy.hex(8));
        let fake = fake_linear(&issue_id);
        Self { issue_id, fake }
    }
}

/// Runs the scenario against the Postgres database at `url`, stops all
/// parts, and checks the result.
async fn run(url: &str, scenario: &Scenario) -> Result<Value, BoxError> {
    tokio::time::timeout(RUN_TIMEOUT, run_unbounded(url, scenario))
        .await
        .map_err(|_| format!("the intake desk did not stop in {RUN_TIMEOUT:?}"))?
}

async fn run_unbounded(url: &str, scenario: &Scenario) -> Result<Value, BoxError> {
    // In production, apply the harvest migrations before you start.
    migrate::apply(url, &migrate::embedded()).await?;
    let pool = engine_pool(url, 8, DbRole::Hot, &EngineDbTimeouts::default())?;

    // camel: the harvest bridge, San Mateo, and the routes.
    let (source, harvest_component) = HarvestBridge::new(STREAM);
    let mut camel = CamelContext::builder().build().await?;
    camel.register_component(DirectComponent::new());
    camel.register_component(harvest_component);
    camel.register_component(
        SanMateoComponent::new(san_mateo(&scenario.fake).await?).environment(ENV),
    );
    for route in routes()? {
        // Refuse error handlers that change an abandoned message into a
        // committed message.
        check_route(&route)?;
        camel.add_route_definition(route).await?;
    }
    camel.start().await?;

    let result = with_harvest(&camel, &pool, url, source, scenario).await;
    // Stop camel in all cases. An error of the run comes first.
    let stopped = camel.stop().await;
    let summary = result?;
    stopped?;
    check(&summary, scenario)?;
    Ok(summary)
}

/// Starts the harvest worker and connector, runs the deliveries, and stops
/// both.
async fn with_harvest(
    camel: &CamelContext,
    pool: &DbPool,
    url: &str,
    source: harvest_camel::CamelSource,
    scenario: &Scenario,
) -> Result<Value, BoxError> {
    // Activities get this handle. Resolve their endpoint before they run.
    let activities = CamelHandle::new();
    activities.resolve(camel, GRAPHQL)?;
    let runner = HarvestRunner::start(
        autumn_harvest::HarvestBuilder::new()
            .workflows(vec![intake_issue_info()])
            .activities(vec![fetch_issue_info(), post_receipt_info(), set_priority_info()])
            .state(activities)
            .build(),
        &HarvestRuntimeConfig {
            mode: HarvestMode::External,
            database: HarvestDatabaseConfig {
                url: Some(url.to_owned()),
            },
            ..HarvestRuntimeConfig::default()
        },
        HarvestRunnerResources::new(pool.clone()),
    )
    .await
    .map_err(|e| e.to_string())?;

    // One workflow per issue: the workflow id is the issue id.
    let api = HarvestApiState::new();
    api.install_storage_pool(runner.storage_pool());
    api.install(runner.api_runtime());
    let binding = SourceBinding::starts("linear-intake", STREAM, "intake_issue").map_json(
        |_ctx, message: Value| {
            let id = message["issue_id"].as_str().ok_or("missing issue_id")?;
            Ok::<_, &str>(MappedMessage::new(format!("intake-{id}"), message))
        },
    );
    let connector = ConnectorRuntime::for_binding(
        Arc::new(binding),
        source,
        api,
        Arc::new(NoOpMetrics),
        Some(&intake_issue_info()),
    );
    let cancel = CancellationToken::new();
    let connector = tokio::spawn({
        let cancel = cancel.clone();
        async move { connector.run(cancel).await }
    });

    let result = deliver_and_wait(camel, pool, url, scenario).await;

    // Stop in all cases: the connector, then the worker.
    cancel.cancel();
    connector.await?;
    runner.stop().await;
    result
}

/// Sends the webhooks, waits for the workflow, and builds the summary.
async fn deliver_and_wait(
    camel: &CamelContext,
    pool: &DbPool,
    url: &str,
    scenario: &Scenario,
) -> Result<Value, BoxError> {
    let broker = CamelHandle::new();
    broker.resolve(camel, INTAKE)?;
    let secret = format!("whsec_{}", OsEntropy.hex(16));
    let app = autumn_app(broker, secret.clone());
    let now = now_ms();
    let issue = json!({
        "id": scenario.issue_id,
        "identifier": "ENG-42",
        "title": "Checkout crash after the update",
        "labelIds": [INTAKE_LABEL],
    });
    let create = EventFixture::new("Issue", "create", ORG).data(issue).at_ms(now);
    let other = EventFixture::new("Issue", "update", ORG)
        .data(json!({"id": "i-other", "labelIds": []}))
        .updated_from(json!({"title": "old"}))
        .at_ms(now);
    let d1 = format!("d-{}", OsEntropy.hex(8));
    let d2 = format!("d-{}", OsEntropy.hex(8));
    let statuses = vec![
        // A forged signature.
        deliver(&app, &create.sign("not-the-secret", &d1)).await,
        // An update that does not add the label: the handler skips it.
        deliver(&app, &other.sign(&secret, &format!("d-{}", OsEntropy.hex(8)))).await,
        // The intake event.
        deliver(&app, &create.sign(&secret, &d1)).await,
        // A retry with a new delivery id: the same workflow.
        deliver(&app, &create.sign(&secret, &d2)).await,
        // The same delivery again: the plugin skips it.
        deliver(&app, &create.sign(&secret, &d1)).await,
    ];

    let workflow_id = format!("intake-{}", scenario.issue_id);
    let mut conn = pool.get().await?;
    let execution = resolve_execution_id_by_workflow_id(&mut conn, "intake_issue", &workflow_id)
        .await?
        .ok_or("the workflow did not start")?;
    drop(conn);
    let output = WorkflowHandleClient::single(pool.clone(), url)
        .handle(execution.exec_id)
        .result_raw_with_timeout(RUN_TIMEOUT)
        .await
        .map_err(|e| format!("workflow {workflow_id}: {e}"))?;
    Ok(summarize(&output, &statuses, &scenario.fake))
}

/// The wall clock in Unix ms. The autumn app checks the webhook time
/// against it.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Builds the summary from the workflow output, the webhook answers, and
/// the fake log.
fn summarize(output: &Value, statuses: &[u16], fake: &MemoryTransport) -> Value {
    let calls = fake.requests_to(GRAPHQL_URL);
    let operations: Vec<String> = calls
        .iter()
        .map(|call| {
            let body: Value = serde_json::from_slice(&call.body).unwrap_or_default();
            body["query"]
                .as_str()
                .and_then(|q| q.split_whitespace().nth(1))
                .map(|name| name.split('(').next().unwrap_or(name).to_owned())
                .unwrap_or_default()
        })
        .collect();
    let auth = calls
        .iter()
        .all(|call| call.header_value("authorization") == Some(&format!("Bearer {ACCESS_TOKEN}")));
    json!({
        "workflow": output,
        "webhook_statuses": statuses,
        "linear_operations": operations,
        "auth_added_by_san_mateo": auth,
    })
}

/// Compares the summary with the expected values. The error starts with
/// `check failed:`.
fn check(summary: &Value, scenario: &Scenario) -> Result<(), String> {
    let expected = json!({
        "workflow": {
            "issue": "ENG-42",
            "queue": "incident",
            "priority": 1,
            "receipt": stable_uuid(&receipt_key(&scenario.issue_id)),
            "receipt_created": true,
        },
        "webhook_statuses": [401, 200, 200, 200, 200],
        "linear_operations": ["AutumnIssue", "AutumnFindComment", "AutumnCommentCreate", "AutumnIssueUpdate"],
        "auth_added_by_san_mateo": true,
    });
    if *summary == expected {
        Ok(())
    } else {
        Err(format!("check failed: the summary {summary} is not {expected}"))
    }
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let url = std::env::var("DATABASE_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .ok_or("set DATABASE_URL to a Postgres URL")?;
    let summary = run(&url, &Scenario::happy()).await?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

#[cfg(test)]
mod tests;
