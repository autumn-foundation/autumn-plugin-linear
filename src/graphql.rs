//! Transport-free GraphQL operations and the response parser.
//!
//! [`ops`] builds an [`Operation`]: the query text, the variables, and the
//! root field of the answer. It does no IO. Any carrier can send
//! [`Operation::body`] to [`ENDPOINT`]: [`LinearClient`](crate::LinearClient),
//! the San Mateo proxy, or a camel route. See ADR 0003.
//!
//! [`parse_response`] reads the answer with the same rules on every
//! carrier:
//!
//! | Answer | Result |
//! |---|---|
//! | 429, or an error with `extensions.code = RATELIMITED` | [`Error::RateLimited`] with the `Retry-After` wait |
//! | An `errors` array, also next to partial `data` | [`Error::GraphQl`] with the messages |
//! | Another non-2xx status | [`Error::LinearApi`] with a truncated body |
//! | 2xx with no `data` | [`Error::UnexpectedResponse`] |
//!
//! ```
//! use autumn_plugin_linear::graphql::{ops, parse_response};
//!
//! let op = ops::issue("ENG-123");
//! assert_eq!(op.root, "issue");
//! let answer = br#"{"data":{"issue":{"id":"i-1","identifier":"ENG-123","title":"Crash"}}}"#;
//! let data = parse_response(200, None, answer).unwrap();
//! let issue: autumn_plugin_linear::graphql::Issue = op.extract(data).unwrap();
//! assert_eq!(issue.identifier.as_deref(), Some("ENG-123"));
//! ```

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{Error, Result};

/// The Linear GraphQL endpoint.
pub const ENDPOINT: &str = "https://api.linear.app/graphql";
/// The longest error body the parser keeps, in chars.
pub const MAX_ERROR_BODY_CHARS: usize = 500;
/// The error code Linear sets on a rate-limited answer.
pub const RATE_LIMITED_CODE: &str = "RATELIMITED";

/// One GraphQL operation, ready to send.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Operation {
    /// The query or mutation text.
    pub query: String,
    /// The variables.
    pub variables: Value,
    /// The root field of `data` that holds the answer.
    pub root: &'static str,
    /// The field of a mutation payload that holds the result, if any.
    pub payload_field: Option<&'static str>,
    /// True for a built-in mutation: its payload must have `success: true`.
    checks_success: bool,
}

impl Operation {
    /// Make an operation from your own query. `root` names the field of
    /// `data` that [`extract`](Self::extract) reads.
    #[must_use]
    pub fn new(query: impl Into<String>, variables: Value, root: &'static str) -> Self {
        Self {
            query: query.into(),
            variables,
            root,
            payload_field: None,
            checks_success: false,
        }
    }

    /// Make a mutation whose payload has `success` and `field`.
    const fn mutation(
        query: String,
        variables: Value,
        root: &'static str,
        field: Option<&'static str>,
    ) -> Self {
        Self {
            query,
            variables,
            root,
            payload_field: field,
            checks_success: true,
        }
    }

    /// The request body: `{"query": ..., "variables": ...}`.
    #[must_use]
    pub fn body(&self) -> Value {
        json!({ "query": self.query, "variables": self.variables })
    }

    /// Read the answer of this operation from `data`.
    ///
    /// For a mutation, the payload must have `success: true`; the result is
    /// the payload field (or the whole payload when there is none).
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnexpectedResponse`] when the root field is
    /// missing, `success` is not `true`, or the value does not match `T`.
    pub fn extract<T: DeserializeOwned>(&self, data: Value) -> Result<T> {
        let mut data = data;
        let root = data
            .get_mut(self.root)
            .map(Value::take)
            .filter(|value| !value.is_null())
            .ok_or_else(|| unexpected(&format!("the answer has no {}", self.root)))?;
        let value = if self.checks_success {
            if root.get("success").and_then(Value::as_bool) != Some(true) {
                return Err(unexpected(&format!("{} did not succeed", self.root)));
            }
            match self.payload_field {
                Some(field) => root.get(field).cloned().unwrap_or(Value::Null),
                None => root,
            }
        } else {
            root
        };
        serde_json::from_value(value)
            .map_err(|err| unexpected(&format!("{} does not match: {err}", self.root)))
    }
}

/// Read a GraphQL answer: status, `Retry-After` header value, and body.
///
/// # Errors
///
/// See the module table.
pub fn parse_response(status: u16, retry_after: Option<&str>, body: &[u8]) -> Result<Value> {
    let retry_after = retry_after.and_then(parse_retry_after);
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    let errors = parsed
        .as_ref()
        .and_then(|value| value.get("errors"))
        .and_then(Value::as_array)
        .filter(|errors| !errors.is_empty());
    let rate_limited = status == 429
        || errors.is_some_and(|errors| {
            errors.iter().any(|error| {
                error.pointer("/extensions/code").and_then(Value::as_str) == Some(RATE_LIMITED_CODE)
            })
        });
    if rate_limited {
        return Err(Error::RateLimited {
            retries: 0,
            retry_after,
        });
    }
    if let Some(errors) = errors {
        let messages = errors
            .iter()
            .map(|error| {
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .chars()
                    .take(MAX_ERROR_BODY_CHARS)
                    .collect()
            })
            .collect();
        return Err(Error::GraphQl(messages));
    }
    if !(200..300).contains(&status) {
        return Err(Error::LinearApi {
            status,
            body: String::from_utf8_lossy(body)
                .chars()
                .take(MAX_ERROR_BODY_CHARS)
                .collect(),
        });
    }
    let mut parsed = parsed.ok_or_else(|| unexpected("the answer is not JSON"))?;
    match parsed.get_mut("data").map(Value::take) {
        Some(data) if data.is_object() => Ok(data),
        _ => Err(unexpected("the answer has no data")),
    }
}

/// Parse a `Retry-After` value in seconds.
#[must_use]
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
}

/// A UUID v4-format id derived from `key` (SHA-256).
///
/// Linear accepts a client id for `commentCreate`, `issueCreate`, and
/// `agentActivityCreate`. Derive it from your work key, so that a retry of
/// the same work uses the same id and can find the earlier result.
#[must_use]
pub fn stable_uuid(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(key.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // RFC 9562: version 4 in the high nibble of byte 6, variant 10 in the
    // high bits of byte 8.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// An [`Error::UnexpectedResponse`] with a truncated detail.
pub(crate) fn unexpected(detail: &str) -> Error {
    Error::UnexpectedResponse(detail.chars().take(MAX_ERROR_BODY_CHARS).collect())
}

// ----------------------------------------------------------------- results

/// A page of a connection.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PageInfo {
    /// True when a next page exists.
    #[serde(default)]
    pub has_next_page: bool,
    /// The cursor for `after`.
    #[serde(default)]
    pub end_cursor: Option<String>,
}

/// A connection: `nodes` and `pageInfo`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Connection<T> {
    /// The items.
    pub nodes: Vec<T>,
    /// The page data.
    #[serde(default)]
    pub page_info: PageInfo,
}

impl<T> Connection<T> {
    /// The cursor of the next page, when one exists.
    #[must_use]
    pub fn next_cursor(&self) -> Option<&str> {
        self.page_info
            .end_cursor
            .as_deref()
            .filter(|_| self.page_info.has_next_page)
    }
}

/// The authenticated user.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct Viewer {
    /// The user id.
    pub id: String,
    /// The name.
    #[serde(default)]
    pub name: Option<String>,
    /// The email.
    #[serde(default)]
    pub email: Option<String>,
}

/// The workspace.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Organization {
    /// The workspace id. Webhooks carry it as `organizationId`.
    pub id: String,
    /// The name.
    #[serde(default)]
    pub name: Option<String>,
    /// The URL key: `linear.app/{url_key}`.
    #[serde(default)]
    pub url_key: Option<String>,
}

/// A short reference to a user.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct UserRef {
    /// The user id.
    pub id: String,
    /// The name.
    #[serde(default)]
    pub name: Option<String>,
}

/// An issue, as the issue queries return it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Issue {
    /// The issue id (UUID).
    pub id: String,
    /// The identifier, for example `ENG-123`.
    #[serde(default)]
    pub identifier: Option<String>,
    /// The title.
    #[serde(default)]
    pub title: Option<String>,
    /// The description in Markdown.
    #[serde(default)]
    pub description: Option<String>,
    /// The priority: 0 none, 1 urgent, 2 high, 3 medium, 4 low.
    #[serde(default)]
    pub priority: Option<f64>,
    /// The URL.
    #[serde(default)]
    pub url: Option<String>,
    /// The workflow state.
    #[serde(default)]
    pub state: Option<crate::event::StateRef>,
    /// The team.
    #[serde(default)]
    pub team: Option<crate::event::TeamRef>,
    /// The labels.
    #[serde(default)]
    pub labels: Option<Connection<crate::event::LabelRef>>,
    /// The assignee.
    #[serde(default)]
    pub assignee: Option<UserRef>,
    /// The delegate (agent).
    #[serde(default)]
    pub delegate: Option<UserRef>,
    /// The last update time.
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// A comment.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct Comment {
    /// The comment id.
    pub id: String,
    /// The body.
    #[serde(default)]
    pub body: Option<String>,
    /// The URL.
    #[serde(default)]
    pub url: Option<String>,
}

/// A webhook configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct Webhook {
    /// The webhook id.
    pub id: String,
    /// True when Linear sends to it.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// An id, for results that need only the id.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct Created {
    /// The id.
    pub id: String,
}

// ------------------------------------------------------------------ inputs

/// The input of [`ops::create_issue`].
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct IssueCreate {
    /// The team id. Required.
    pub team_id: String,
    /// The title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The description in Markdown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The priority: 0 none, 1 urgent, 2 high, 3 medium, 4 low.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<u8>,
    /// The state id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_id: Option<String>,
    /// The label ids.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_ids: Option<Vec<String>>,
    /// The assignee id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee_id: Option<String>,
    /// The delegate (agent) id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delegate_id: Option<String>,
    /// The parent issue id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// A client id in UUID v4 format. See [`stable_uuid`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

impl IssueCreate {
    /// Make an input for `team_id` with `title`.
    #[must_use]
    pub fn new(team_id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            team_id: team_id.into(),
            title: Some(title.into()),
            ..Self::default()
        }
    }
}

/// The input of [`ops::update_issue`]. Unset fields do not change.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct IssueUpdate {
    /// The title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The description in Markdown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The priority: 0 none, 1 urgent, 2 high, 3 medium, 4 low.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<u8>,
    /// The state id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_id: Option<String>,
    /// The assignee id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee_id: Option<String>,
    /// The delegate (agent) id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delegate_id: Option<String>,
    /// Label ids to add.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_label_ids: Option<Vec<String>>,
    /// Label ids to remove.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed_label_ids: Option<Vec<String>>,
}

/// The input of [`ops::create_comment`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct CommentCreate {
    /// The issue id.
    pub issue_id: String,
    /// The body in Markdown.
    pub body: String,
    /// A client id in UUID v4 format. See [`stable_uuid`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The parent comment id, to reply in a thread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

impl CommentCreate {
    /// Make an input for `issue_id` with `body`.
    #[must_use]
    pub fn new(issue_id: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            issue_id: issue_id.into(),
            body: body.into(),
            id: None,
            parent_id: None,
        }
    }

    /// Set a client id derived from `key` with [`stable_uuid`].
    #[must_use]
    pub fn with_stable_id(mut self, key: &str) -> Self {
        self.id = Some(stable_uuid(key));
        self
    }
}

/// The content of an agent activity.
///
/// Linear shows `thought` and `action` as progress, `elicitation` as a
/// question, `response` as the final answer, and `error` as a failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
#[non_exhaustive]
pub enum ActivityContent {
    /// An internal note.
    Thought {
        /// The text.
        body: String,
    },
    /// A question for the user.
    Elicitation {
        /// The text.
        body: String,
    },
    /// A tool call.
    Action {
        /// The action name, for example `Searching`.
        action: String,
        /// The parameter, for example the query.
        parameter: String,
        /// The result, when the action is done.
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<String>,
    },
    /// The final answer.
    Response {
        /// The text.
        body: String,
    },
    /// A failure report.
    Error {
        /// The text.
        body: String,
    },
}

/// The input of [`ops::create_webhook`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct WebhookCreate {
    /// The receiver URL, for example `https://app.example.com/linear/webhooks`.
    pub url: String,
    /// The entity types, for example `["Issue", "Comment"]`.
    pub resource_types: Vec<String>,
    /// One team. Leave it unset and set `all_public_teams` for all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    /// All public teams.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub all_public_teams: Option<bool>,
    /// A label for the settings page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The signing secret. Never log it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

// --------------------------------------------------------------- operations

/// The operations the plugin knows. Each returns an [`Operation`].
pub mod ops {
    use serde_json::{Value, json};

    use super::{
        ActivityContent, CommentCreate, IssueCreate, IssueUpdate, Operation, WebhookCreate,
    };

    /// The issue fields every issue operation reads.
    pub const ISSUE_FIELDS: &str = "id identifier title description priority url updatedAt state { id name type } team { id key name } labels { nodes { id name } } assignee { id name } delegate { id name }";

    /// `viewer { id name email }`.
    #[must_use]
    pub fn viewer() -> Operation {
        Operation::new(
            "query AutumnViewer { viewer { id name email } }",
            json!({}),
            "viewer",
        )
    }

    /// `organization { id name urlKey }`: the workspace of the token.
    #[must_use]
    pub fn organization() -> Operation {
        Operation::new(
            "query AutumnOrganization { organization { id name urlKey } }",
            json!({}),
            "organization",
        )
    }

    /// One issue by UUID or identifier (`ENG-123`).
    #[must_use]
    pub fn issue(id: &str) -> Operation {
        Operation::new(
            format!("query AutumnIssue($id: String!) {{ issue(id: $id) {{ {ISSUE_FIELDS} }} }}"),
            json!({ "id": id }),
            "issue",
        )
    }

    /// One page of issues. `filter` is an `IssueFilter`, for example
    /// `{"labels": {"name": {"eq": "intake"}}}`.
    #[must_use]
    pub fn issues(filter: &Value, first: u32, after: Option<&str>) -> Operation {
        Operation::new(
            format!(
                "query AutumnIssues($filter: IssueFilter, $first: Int, $after: String) {{ issues(filter: $filter, first: $first, after: $after) {{ nodes {{ {ISSUE_FIELDS} }} pageInfo {{ hasNextPage endCursor }} }} }}"
            ),
            json!({ "filter": filter, "first": first, "after": after }),
            "issues",
        )
    }

    /// `issueCreate`. The result is the issue.
    #[must_use]
    pub fn create_issue(input: &IssueCreate) -> Operation {
        Operation::mutation(
            format!(
                "mutation AutumnIssueCreate($input: IssueCreateInput!) {{ issueCreate(input: $input) {{ success issue {{ {ISSUE_FIELDS} }} }} }}"
            ),
            json!({ "input": input }),
            "issueCreate",
            Some("issue"),
        )
    }

    /// `issueUpdate`. The result is the issue.
    #[must_use]
    pub fn update_issue(id: &str, input: &IssueUpdate) -> Operation {
        Operation::mutation(
            format!(
                "mutation AutumnIssueUpdate($id: String!, $input: IssueUpdateInput!) {{ issueUpdate(id: $id, input: $input) {{ success issue {{ {ISSUE_FIELDS} }} }} }}"
            ),
            json!({ "id": id, "input": input }),
            "issueUpdate",
            Some("issue"),
        )
    }

    /// `commentCreate`. The result is the comment.
    #[must_use]
    pub fn create_comment(input: &CommentCreate) -> Operation {
        Operation::mutation(
            "mutation AutumnCommentCreate($input: CommentCreateInput!) { commentCreate(input: $input) { success comment { id body url } } }".to_owned(),
            json!({ "input": input }),
            "commentCreate",
            Some("comment"),
        )
    }

    /// Find a comment by id. The result is a connection with zero or one
    /// node, so a missing comment is not an error.
    #[must_use]
    pub fn find_comment(id: &str) -> Operation {
        Operation::new(
            "query AutumnFindComment($id: ID!) { comments(filter: { id: { eq: $id } }, first: 1) { nodes { id body url } } }",
            json!({ "id": id }),
            "comments",
        )
    }

    /// `agentActivityCreate`. `id` is an optional client id. The result
    /// has the activity id.
    #[must_use]
    pub fn create_agent_activity(
        session_id: &str,
        content: &ActivityContent,
        id: Option<&str>,
    ) -> Operation {
        let mut input = json!({ "agentSessionId": session_id, "content": content });
        if let Some(id) = id {
            input["id"] = json!(id);
        }
        Operation::mutation(
            "mutation AutumnAgentActivityCreate($input: AgentActivityCreateInput!) { agentActivityCreate(input: $input) { success agentActivity { id } } }".to_owned(),
            json!({ "input": input }),
            "agentActivityCreate",
            Some("agentActivity"),
        )
    }

    /// `agentSessionUpdate` with `addedExternalUrls`. Linear then does not
    /// mark the session as unresponsive. The result is the payload.
    #[must_use]
    pub fn add_agent_session_url(session_id: &str, label: &str, url: &str) -> Operation {
        Operation::mutation(
            "mutation AutumnAgentSessionUrl($id: String!, $input: AgentSessionUpdateInput!) { agentSessionUpdate(id: $id, input: $input) { success } }".to_owned(),
            json!({ "id": session_id, "input": { "addedExternalUrls": [{ "label": label, "url": url }] } }),
            "agentSessionUpdate",
            None,
        )
    }

    /// `webhookCreate`. Needs the `admin` scope. The result is the webhook.
    #[must_use]
    pub fn create_webhook(input: &WebhookCreate) -> Operation {
        Operation::mutation(
            "mutation AutumnWebhookCreate($input: WebhookCreateInput!) { webhookCreate(input: $input) { success webhook { id enabled } } }".to_owned(),
            json!({ "input": input }),
            "webhookCreate",
            Some("webhook"),
        )
    }

    /// `webhookDelete`. The result is the payload.
    #[must_use]
    pub fn delete_webhook(id: &str) -> Operation {
        Operation::mutation(
            "mutation AutumnWebhookDelete($id: String!) { webhookDelete(id: $id) { success } }"
                .to_owned(),
            json!({ "id": id }),
            "webhookDelete",
            None,
        )
    }
}

#[cfg(test)]
mod tests;
