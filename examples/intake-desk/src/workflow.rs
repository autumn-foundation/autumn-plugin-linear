//! The harvest workflow and its activities.
//!
//! Workflow code must be deterministic, so only the activities call Linear.
//! Each activity sends a plugin [`Operation`] through camel and San Mateo,
//! and reads the answer with the plugin parser.

use std::time::Duration;

use autumn_harvest::failure::ActivityFailure;
use autumn_harvest::prelude::{ActivityContext, RetryPolicy, WorkflowContext, activity, workflow};
use autumn_plugin_linear::graphql::{
    Comment, CommentCreate, Connection, Issue, IssueUpdate, Operation, ops, parse_response,
    stable_uuid,
};
use harvest_camel::call_route_from_activity;
use serde::{Deserialize, Serialize};

use crate::camel::GRAPHQL;
use crate::triage::{IssueFacts, Routing, receipt_body, receipt_key, route_issue};

/// The workflow name, as the harvest connector starts it.
pub(crate) const WORKFLOW: &str = "intake_issue";
/// Attempts per activity for a transient error.
const ATTEMPTS: u32 = 8;
/// The first backoff. It doubles on each attempt.
const BACKOFF: Duration = Duration::from_millis(500);

/// The workflow id: one run per workspace and issue.
pub(crate) fn workflow_id(org: &str, issue_id: &str) -> String {
    format!("intake-{org}-{issue_id}")
}

/// The message the webhook hands to harvest, and the workflow input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct IssueRef {
    /// The Linear issue id.
    pub(crate) issue_id: String,
    /// The workspace of the issue.
    pub(crate) organization_id: String,
}

/// The input of the write activities.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Step {
    issue_id: String,
    routing: Routing,
}

/// The receipt comment and whether this run posted it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Receipt {
    id: String,
    created: bool,
}

/// What the workflow returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Outcome {
    issue: Option<String>,
    queue: String,
    priority: u8,
    receipt: String,
    receipt_created: bool,
}

/// Read the issue, route it, post one receipt, and set the priority.
#[workflow]
async fn intake_issue(ctx: &WorkflowContext, input: IssueRef) -> Result<Outcome, String> {
    let issue: IssueFacts = ctx
        .execute_activity(&fetch_issue_info(), input)
        .await
        .map_err(|e| e.to_string())?;
    let routing = route_issue(&issue);
    let step = Step {
        issue_id: issue.id.clone(),
        routing: routing.clone(),
    };
    let receipt: Receipt = ctx
        .execute_activity(&post_receipt_info(), step.clone())
        .await
        .map_err(|e| e.to_string())?;
    let _: String = ctx
        .execute_activity(&set_priority_info(), step)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Outcome {
        issue: issue.identifier,
        queue: routing.queue,
        priority: routing.priority,
        receipt: receipt.id,
        receipt_created: receipt.created,
    })
}

/// Read the issue fresh: the webhook data can be old by now.
#[activity(start_to_close = "30s", retry = RetryPolicy::exponential(ATTEMPTS, BACKOFF))]
async fn fetch_issue(
    ctx: &ActivityContext,
    input: IssueRef,
) -> Result<IssueFacts, ActivityFailure> {
    let issue: Issue = run_op(ctx, &ops::issue(&input.issue_id)).await?;
    Ok(issue.into())
}

/// Post the receipt one time. A retry finds the comment by its stable id.
#[activity(start_to_close = "30s", retry = RetryPolicy::exponential(ATTEMPTS, BACKOFF))]
async fn post_receipt(ctx: &ActivityContext, step: Step) -> Result<Receipt, ActivityFailure> {
    let key = receipt_key(&step.issue_id);
    if let Some(comment) = find_receipt(ctx, &key).await? {
        return Ok(Receipt {
            id: comment.id,
            created: false,
        });
    }
    let input =
        CommentCreate::new(&step.issue_id, receipt_body(&step.routing)).with_stable_id(&key);
    match run_op::<Comment>(ctx, &ops::create_comment(&input)).await {
        Ok(comment) => Ok(Receipt {
            id: comment.id,
            created: true,
        }),
        // A lost reply or an overlapping attempt can have made the comment.
        // Linear then refuses the second create of its id.
        Err(failure) => match find_receipt(ctx, &key).await? {
            Some(comment) => Ok(Receipt {
                id: comment.id,
                created: false,
            }),
            None => Err(failure),
        },
    }
}

/// Set the priority. A second run sets the same value. Returns the issue id.
#[activity(start_to_close = "30s", retry = RetryPolicy::exponential(ATTEMPTS, BACKOFF))]
async fn set_priority(ctx: &ActivityContext, step: Step) -> Result<String, ActivityFailure> {
    let update = IssueUpdate {
        priority: Some(step.routing.priority),
        ..IssueUpdate::default()
    };
    let issue: Issue = run_op(ctx, &ops::update_issue(&step.issue_id, &update)).await?;
    Ok(issue.id)
}

/// The receipt comment of `key`, if it exists.
async fn find_receipt(
    ctx: &ActivityContext,
    key: &str,
) -> Result<Option<Comment>, ActivityFailure> {
    let found: Connection<Comment> = run_op(ctx, &ops::find_comment(&stable_uuid(key))).await?;
    Ok(found.nodes.into_iter().next())
}

/// Send one operation through camel and San Mateo. The plugin parser keeps
/// the meaning of GraphQL errors and rate limits: a transient error is a
/// retryable failure.
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

/// The workflow, for the harvest builder and the connector.
pub(crate) fn workflow_info() -> autumn_harvest::WorkflowInfo {
    intake_issue_info()
}

/// The activities, for the harvest builder.
pub(crate) fn activity_infos() -> Vec<autumn_harvest::ActivityInfo> {
    vec![fetch_issue_info(), post_receipt_info(), set_priority_info()]
}
