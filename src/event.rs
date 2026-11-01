//! The webhook envelope and typed views of its data.
//!
//! [`WebhookEvent::parse`] reads the signed body. Call it only after the
//! signature check. Every payload has `type`, `action`, `organizationId`,
//! and `webhookTimestamp`. Data-change events also have `data`, `url`,
//! `actor`, and (on `update`) `updatedFrom`.
//!
//! The typed views ([`IssueData`], [`CommentData`],
//! [`AgentSessionEvent`]) keep most fields optional. Linear adds fields
//! over time, and a strict struct would refuse a valid event.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::types::{Action, EventType, OrganizationId};

/// The signed envelope of one webhook.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct WebhookEvent {
    /// The entity type (signed `type`), for example `Issue`.
    pub event_type: EventType,
    /// The action (signed `action`), for example `create`.
    pub action: Action,
    /// The workspace that sent the event. Use it as the tenant key.
    pub organization_id: OrganizationId,
    /// The id of the webhook configuration (`webhookId`), if sent.
    pub webhook_id: Option<String>,
    /// The send time in Unix ms (`webhookTimestamp`).
    pub webhook_timestamp_ms: u64,
    /// The time of the action (`createdAt`), if sent.
    pub created_at: Option<String>,
    /// The URL of the entity (`url`), if sent.
    pub url: Option<String>,
    /// The user, OAuth client, or integration that acted (`actor`).
    pub actor: Option<Value>,
    /// The entity (`data`). `Null` for events without it.
    pub data: Value,
    /// The old values of changed fields (`updatedFrom`), on `update`.
    pub updated_from: Option<Map<String, Value>>,
    /// The whole signed body.
    pub raw: Value,
}

impl WebhookEvent {
    /// Parse a signed body.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Replay`] when `webhookTimestamp` is missing or not
    /// a time, and [`Error::InvalidRequest`] when the body is not a JSON
    /// object or `type`, `action`, or `organizationId` is missing or bad.
    pub fn parse(body: &[u8]) -> Result<Self> {
        let raw: Value = serde_json::from_slice(body)
            .map_err(|_| Error::InvalidRequest("the webhook body is not JSON".to_owned()))?;
        let object = raw
            .as_object()
            .ok_or_else(|| Error::InvalidRequest("the webhook body is not an object".to_owned()))?;
        let text = |name: &str| object.get(name).and_then(Value::as_str);
        let required = |name: &str| {
            text(name)
                .ok_or_else(|| Error::InvalidRequest(format!("the webhook body has no {name}")))
        };
        let event_type = EventType::parse(required("type")?)?;
        let action = Action::parse(required("action")?)?;
        let organization_id = OrganizationId::parse(required("organizationId")?)?;
        let webhook_timestamp_ms = object
            .get("webhookTimestamp")
            .and_then(timestamp_ms)
            .ok_or_else(|| Error::Replay("webhookTimestamp is missing or not a time".to_owned()))?;
        Ok(Self {
            event_type,
            action,
            organization_id,
            webhook_id: text("webhookId").map(ToOwned::to_owned),
            webhook_timestamp_ms,
            created_at: text("createdAt").map(ToOwned::to_owned),
            url: text("url").map(ToOwned::to_owned),
            actor: object.get("actor").filter(|v| !v.is_null()).cloned(),
            data: object.get("data").cloned().unwrap_or(Value::Null),
            updated_from: object
                .get("updatedFrom")
                .and_then(Value::as_object)
                .cloned(),
            raw,
        })
    }

    /// The entity id (`data.id`), if any.
    #[must_use]
    pub fn entity_id(&self) -> Option<&str> {
        self.data.get("id").and_then(Value::as_str)
    }

    /// A key from signed fields: `{type}:{action}:{data.id}:{createdAt}`.
    ///
    /// Two deliveries of one change give the same key. Use it to make the
    /// work of a handler idempotent. A missing part is `-`.
    #[must_use]
    pub fn entity_key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.event_type,
            self.action,
            self.entity_id().unwrap_or("-"),
            self.created_at.as_deref().unwrap_or("-")
        )
    }

    /// True when the event type is `event_type`.
    #[must_use]
    pub fn is(&self, event_type: &str) -> bool {
        self.event_type.as_str() == event_type
    }

    /// Deserialize `data` into `T`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Json`] when `data` does not match `T`.
    pub fn data_as<T: DeserializeOwned>(&self) -> Result<T> {
        T::deserialize(&self.data).map_err(Error::Json)
    }

    /// The issue, for an `Issue` event.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Json`] when `data` is not an issue.
    pub fn issue(&self) -> Result<Option<IssueData>> {
        self.view(EventType::ISSUE)
    }

    /// The comment, for a `Comment` event.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Json`] when `data` is not a comment.
    pub fn comment(&self) -> Result<Option<CommentData>> {
        self.view(EventType::COMMENT)
    }

    /// The agent session event, for an `AgentSessionEvent`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Json`] when the body is not an agent session event.
    pub fn agent_session(&self) -> Result<Option<AgentSessionEvent>> {
        if !self.is(EventType::AGENT_SESSION_EVENT) {
            return Ok(None);
        }
        AgentSessionEvent::deserialize(&self.raw)
            .map(Some)
            .map_err(Error::Json)
    }

    /// `data` as `T` when the type is `event_type`.
    fn view<T: DeserializeOwned>(&self, event_type: &str) -> Result<Option<T>> {
        if self.is(event_type) {
            self.data_as().map(Some)
        } else {
            Ok(None)
        }
    }

    /// True when `updatedFrom` names `field`, so the field changed.
    #[must_use]
    pub fn changed(&self, field: &str) -> bool {
        self.updated_from
            .as_ref()
            .is_some_and(|from| from.contains_key(field))
    }

    /// The old value of `field` from `updatedFrom`.
    #[must_use]
    pub fn previous(&self, field: &str) -> Option<&Value> {
        self.updated_from.as_ref()?.get(field)
    }

    /// True when this event adds the label `label_id` to the entity: a
    /// `create` with the label, or an `update` whose old `labelIds` did not
    /// have it.
    #[must_use]
    pub fn label_added(&self, label_id: &str) -> bool {
        let now = has_id(self.data.get("labelIds"), label_id);
        match self.action.as_str() {
            Action::CREATE => now,
            Action::UPDATE => {
                now && self.changed("labelIds") && !has_id(self.previous("labelIds"), label_id)
            }
            _ => false,
        }
    }

    /// True when this `update` removes the label `label_id`.
    #[must_use]
    pub fn label_removed(&self, label_id: &str) -> bool {
        self.action.as_str() == Action::UPDATE
            && has_id(self.previous("labelIds"), label_id)
            && !has_id(self.data.get("labelIds"), label_id)
    }
}

/// True when `list` is a JSON array that holds the string `id`.
fn has_id(list: Option<&Value>, id: &str) -> bool {
    list.and_then(Value::as_array)
        .is_some_and(|ids| ids.iter().any(|v| v.as_str() == Some(id)))
}

/// Read a Unix ms time from a JSON number. `None` for a negative, a
/// non-finite, or a too-large value.
fn timestamp_ms(value: &Value) -> Option<u64> {
    if let Some(ms) = value.as_u64() {
        return Some(ms);
    }
    let ms = value.as_f64()?;
    // 2^53 is the largest whole number an f64 holds exactly.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the range check above makes the cast exact"
    )]
    (ms.is_finite() && (0.0..9_007_199_254_740_992.0).contains(&ms)).then_some(ms as u64)
}

/// A short reference to a workflow state.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct StateRef {
    /// The state id.
    pub id: String,
    /// The state name, for example `Todo`.
    #[serde(default)]
    pub name: Option<String>,
    /// The state type, for example `unstarted` or `completed`.
    #[serde(default, rename = "type")]
    pub state_type: Option<String>,
}

/// A short reference to a team.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct TeamRef {
    /// The team id.
    pub id: String,
    /// The team key, for example `ENG`.
    #[serde(default)]
    pub key: Option<String>,
    /// The team name.
    #[serde(default)]
    pub name: Option<String>,
}

/// A short reference to a label.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct LabelRef {
    /// The label id.
    pub id: String,
    /// The label name.
    #[serde(default)]
    pub name: Option<String>,
}

/// The `data` of an `Issue` event.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct IssueData {
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
    /// The issue URL.
    #[serde(default)]
    pub url: Option<String>,
    /// The team id.
    #[serde(default)]
    pub team_id: Option<String>,
    /// The team.
    #[serde(default)]
    pub team: Option<TeamRef>,
    /// The state id.
    #[serde(default)]
    pub state_id: Option<String>,
    /// The state.
    #[serde(default)]
    pub state: Option<StateRef>,
    /// The label ids.
    #[serde(default)]
    pub label_ids: Vec<String>,
    /// The labels.
    #[serde(default)]
    pub labels: Vec<LabelRef>,
    /// The assignee id.
    #[serde(default)]
    pub assignee_id: Option<String>,
    /// The delegate (agent) id.
    #[serde(default)]
    pub delegate_id: Option<String>,
    /// The creator id.
    #[serde(default)]
    pub creator_id: Option<String>,
    /// The creation time.
    #[serde(default)]
    pub created_at: Option<String>,
    /// The last update time.
    #[serde(default)]
    pub updated_at: Option<String>,
}

impl IssueData {
    /// True when the issue has a label with this name (ASCII case is
    /// ignored).
    #[must_use]
    pub fn has_label_named(&self, name: &str) -> bool {
        self.labels.iter().any(|label| {
            label
                .name
                .as_deref()
                .is_some_and(|label| label.eq_ignore_ascii_case(name))
        })
    }
}

/// The `data` of a `Comment` event.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct CommentData {
    /// The comment id.
    pub id: String,
    /// The body in Markdown.
    #[serde(default)]
    pub body: Option<String>,
    /// The issue id.
    #[serde(default)]
    pub issue_id: Option<String>,
    /// The author id. Compare it with your app user id to skip your own
    /// comments.
    #[serde(default)]
    pub user_id: Option<String>,
    /// The parent comment id, in a thread.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// The creation time.
    #[serde(default)]
    pub created_at: Option<String>,
}

/// The issue of an agent session.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct SessionIssue {
    /// The issue id.
    pub id: String,
    /// The identifier, for example `ENG-123`.
    #[serde(default)]
    pub identifier: Option<String>,
    /// The title.
    #[serde(default)]
    pub title: Option<String>,
    /// The description.
    #[serde(default)]
    pub description: Option<String>,
    /// The issue URL.
    #[serde(default)]
    pub url: Option<String>,
}

/// The agent session of an [`AgentSessionEvent`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct AgentSession {
    /// The session id. Activities name it.
    pub id: String,
    /// The session status, for example `pending` or `active`.
    #[serde(default)]
    pub status: Option<String>,
    /// The issue, when the session is on an issue.
    #[serde(default)]
    pub issue: Option<SessionIssue>,
    /// The comment that started the session, if any.
    #[serde(default)]
    pub comment: Option<CommentRef>,
    /// The session URL.
    #[serde(default)]
    pub url: Option<String>,
}

/// A short reference to a comment.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct CommentRef {
    /// The comment id.
    pub id: String,
    /// The body.
    #[serde(default)]
    pub body: Option<String>,
}

/// The activity of a `prompted` agent session event.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct ActivityData {
    /// The activity id.
    #[serde(default)]
    pub id: Option<String>,
    /// The activity content, for example `{"type": "prompt", "body": "..."}`.
    #[serde(default)]
    pub content: Value,
    /// The signal, for example `stop`.
    #[serde(default)]
    pub signal: Option<String>,
    /// The body, when Linear sends it next to `content`.
    #[serde(default)]
    pub body: Option<String>,
}

/// An `AgentSessionEvent` webhook.
///
/// Linear sends `created` when a user mentions the agent or delegates an
/// issue to it, and `prompted` when a user sends a new message. On
/// `created`, send an activity or an external URL within 10 seconds.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct AgentSessionEvent {
    /// `created` or `prompted`.
    pub action: String,
    /// The session.
    pub agent_session: AgentSession,
    /// The new user activity, on `prompted`.
    #[serde(default)]
    pub agent_activity: Option<ActivityData>,
    /// A ready prompt with the issue, comments, and guidance.
    #[serde(default)]
    pub prompt_context: Option<String>,
    /// The app user id of the agent.
    #[serde(default)]
    pub app_user_id: Option<String>,
    /// Workspace and team guidance for the agent.
    #[serde(default)]
    pub guidance: Option<Value>,
    /// Earlier comments in the thread.
    #[serde(default)]
    pub previous_comments: Option<Value>,
}

impl AgentSessionEvent {
    /// The session id.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.agent_session.id
    }

    /// The text the user sent, on `prompted`.
    #[must_use]
    pub fn prompt_body(&self) -> Option<&str> {
        let activity = self.agent_activity.as_ref()?;
        activity
            .content
            .get("body")
            .and_then(Value::as_str)
            .or(activity.body.as_deref())
    }

    /// True when the user asked the agent to stop. The agent must take no
    /// further action, then send a final `response` or `error` activity.
    #[must_use]
    pub fn is_stop(&self) -> bool {
        self.agent_activity
            .as_ref()
            .and_then(|activity| activity.signal.as_deref())
            == Some("stop")
    }
}

#[cfg(test)]
mod tests;
