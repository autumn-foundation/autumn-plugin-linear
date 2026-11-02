//! A local Linear GraphQL API on wiremock.
//!
//! Each mock matches one plugin operation by its name in the query text,
//! so the order of the calls does not matter. A `*_once` mock answers the
//! first matching call only and wins over the others: use it for a failure
//! before a success.

use autumn_plugin_linear::graphql::stable_uuid;
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::settings::INTAKE_LABEL;
use crate::triage::receipt_key;

/// `issue(id)`.
pub(crate) const ISSUE: &str = "AutumnIssue(";
/// `comments(filter: {id})`.
pub(crate) const FIND_COMMENT: &str = "AutumnFindComment(";
/// `commentCreate`.
pub(crate) const CREATE_COMMENT: &str = "AutumnCommentCreate(";
/// `issueUpdate`.
pub(crate) const UPDATE_ISSUE: &str = "AutumnIssueUpdate(";

/// One recorded call.
#[derive(Debug, Clone)]
pub(crate) struct Call {
    /// The operation name, for example `AutumnIssue`.
    pub(crate) operation: String,
    /// The GraphQL variables.
    pub(crate) variables: Value,
    /// The `Authorization` header that San Mateo added.
    pub(crate) authorization: Option<String>,
}

/// The mock Linear API.
pub(crate) struct MockLinear {
    server: MockServer,
}

impl MockLinear {
    /// Start an API with no mocks. An unmatched call gets 404.
    pub(crate) async fn start() -> Self {
        Self {
            server: MockServer::start().await,
        }
    }

    /// An API for a happy run: the issue, no receipt yet, the new receipt,
    /// the updated issue.
    pub(crate) async fn happy(issue_id: &str) -> Self {
        let linear = Self::start().await;
        linear.issue(issue_id).await;
        linear.no_receipt().await;
        linear.receipt_created(issue_id).await;
        linear.priority_set(issue_id).await;
        linear
    }

    /// The base URL. San Mateo calls `{uri}/graphql`.
    pub(crate) fn uri(&self) -> String {
        self.server.uri()
    }

    /// `issue(id)` returns the issue.
    pub(crate) async fn issue(&self, issue_id: &str) {
        self.answer(ISSUE, &json!({"issue": issue_node(issue_id)}))
            .await;
    }

    /// The receipt lookup finds nothing.
    pub(crate) async fn no_receipt(&self) {
        self.answer(FIND_COMMENT, &json!({"comments": {"nodes": []}}))
            .await;
    }

    /// `commentCreate` makes the receipt.
    pub(crate) async fn receipt_created(&self, issue_id: &str) {
        let receipt =
            json!({"id": stable_uuid(&receipt_key(issue_id)), "body": "Intake desk: ..."});
        self.answer(
            CREATE_COMMENT,
            &json!({"commentCreate": {"success": true, "comment": receipt}}),
        )
        .await;
    }

    /// `issueUpdate` succeeds.
    pub(crate) async fn priority_set(&self, issue_id: &str) {
        self.answer(
            UPDATE_ISSUE,
            &json!({"issueUpdate": {"success": true, "issue": issue_node(issue_id)}}),
        )
        .await;
    }

    /// The calls so far, in order.
    pub(crate) async fn calls(&self) -> Vec<Call> {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(|request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap_or_default();
                Call {
                    operation: body["query"]
                        .as_str()
                        .and_then(|q| q.split_whitespace().nth(1))
                        .map(|name| name.split('(').next().unwrap_or(name).to_owned())
                        .unwrap_or_default(),
                    variables: body["variables"].clone(),
                    authorization: request
                        .headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(ToOwned::to_owned),
                }
            })
            .collect()
    }

    async fn answer(&self, operation: &str, value: &Value) {
        self.mount(operation, data(value), false).await;
    }

    async fn mount(&self, operation: &str, response: ResponseTemplate, once: bool) {
        let mock = Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains(operation))
            .respond_with(response);
        let mock = if once {
            mock.up_to_n_times(1).with_priority(1)
        } else {
            mock
        };
        mock.mount(&self.server).await;
    }
}

/// The issue as Linear returns it.
pub(crate) fn issue_node(issue_id: &str) -> Value {
    json!({
        "id": issue_id,
        "identifier": "ENG-42",
        "title": "Checkout crash after the update",
        "description": "The app crashes when I press Pay.",
        "priority": 0,
        "labels": {"nodes": [{"id": INTAKE_LABEL, "name": "intake"}]},
    })
}

/// A GraphQL `data` answer.
fn data(value: &Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "data": value }))
}

/// Failure answers for the scenario tests.
#[cfg(test)]
impl MockLinear {
    /// The receipt lookup finds nothing on the first call only.
    pub(crate) async fn no_receipt_once(&self) {
        self.answer_once(FIND_COMMENT, data(&json!({"comments": {"nodes": []}})))
            .await;
    }

    /// The receipt lookup finds the receipt of `issue_id`.
    pub(crate) async fn receipt_found(&self, issue_id: &str) {
        let receipt = json!({"id": stable_uuid(&receipt_key(issue_id))});
        self.answer(FIND_COMMENT, &json!({"comments": {"nodes": [receipt]}}))
            .await;
    }

    /// `operation` always gets a GraphQL error.
    pub(crate) async fn graphql_error(&self, operation: &str, message: &str) {
        self.mount(operation, graphql_error(message), false).await;
    }

    /// The first `operation` call gets a 200 with `RATELIMITED`.
    pub(crate) async fn rate_limited_once(&self, operation: &str) {
        self.answer_once(operation, rate_limited(200)).await;
    }

    /// The first `operation` call gets the documented 429 with `Retry-After`.
    pub(crate) async fn http_429_once(&self, operation: &str) {
        self.answer_once(
            operation,
            rate_limited(429).insert_header("retry-after", "1"),
        )
        .await;
    }

    async fn answer_once(&self, operation: &str, response: ResponseTemplate) {
        self.mount(operation, response, true).await;
    }
}

/// A GraphQL error answer.
#[cfg(test)]
fn graphql_error(message: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"errors": [{"message": message}]}))
}

/// A rate-limit answer with `status`.
#[cfg(test)]
fn rate_limited(status: u16) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({
        "errors": [{"message": "Rate limit exceeded", "extensions": {"code": "RATELIMITED"}}],
    }))
}
