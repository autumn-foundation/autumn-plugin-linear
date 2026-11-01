//! Validated domain types: [`OrganizationId`], [`EventType`], [`Action`],
//! [`DeliveryId`], and the OAuth [`Actor`].
//!
//! Each type checks its value at parse time, so a value of the type is
//! always well-formed. The rules are strict on purpose: these values go
//! into logs, store keys, and URLs.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The longest id the plugin accepts, in bytes.
const MAX_ID_LEN: usize = 64;
/// The longest delivery id the plugin accepts, in bytes.
const MAX_DELIVERY_LEN: usize = 128;

/// A Linear workspace (organization) id, for example a UUID.
///
/// Rule: 1 to 64 ASCII letters, digits, and `-`; no `-` at the start.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct OrganizationId(String);

impl OrganizationId {
    /// Parse an organization id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] when the value breaks the rule.
    pub fn parse(value: &str) -> Result<Self> {
        let ok = !value.is_empty()
            && value.len() <= MAX_ID_LEN
            && !value.starts_with('-')
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if ok {
            Ok(Self(value.to_owned()))
        } else {
            Err(Error::InvalidRequest(
                "the organization id must be 1 to 64 letters, digits, or '-'".to_owned(),
            ))
        }
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OrganizationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for OrganizationId {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(&value)
    }
}

impl From<OrganizationId> for String {
    fn from(value: OrganizationId) -> Self {
        value.0
    }
}

/// A webhook entity type, for example `Issue` or `AgentSessionEvent`.
///
/// Rule: 1 to 64 ASCII letters and digits; the first is an uppercase letter.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EventType(String);

impl EventType {
    /// Issues.
    pub const ISSUE: &'static str = "Issue";
    /// Issue comments.
    pub const COMMENT: &'static str = "Comment";
    /// Issue attachments.
    pub const ATTACHMENT: &'static str = "Attachment";
    /// Issue labels.
    pub const ISSUE_LABEL: &'static str = "IssueLabel";
    /// Comment reactions.
    pub const REACTION: &'static str = "Reaction";
    /// Projects.
    pub const PROJECT: &'static str = "Project";
    /// Project updates.
    pub const PROJECT_UPDATE: &'static str = "ProjectUpdate";
    /// Cycles.
    pub const CYCLE: &'static str = "Cycle";
    /// Documents.
    pub const DOCUMENT: &'static str = "Document";
    /// Users.
    pub const USER: &'static str = "User";
    /// OAuth app events. The action `revoked` means the workspace removed the app.
    pub const OAUTH_APP: &'static str = "OAuthApp";
    /// Agent session events. Actions: `created`, `prompted`.
    pub const AGENT_SESSION_EVENT: &'static str = "AgentSessionEvent";

    /// Parse an entity type.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] when the value breaks the rule.
    pub fn parse(value: &str) -> Result<Self> {
        let ok = value.len() <= MAX_ID_LEN
            && value.bytes().next().is_some_and(|b| b.is_ascii_uppercase())
            && value.bytes().all(|b| b.is_ascii_alphanumeric());
        if ok {
            Ok(Self(value.to_owned()))
        } else {
            Err(Error::InvalidRequest(format!(
                "event type {value:?} must be 1 to 64 letters or digits and start with an uppercase letter"
            )))
        }
    }

    /// The type as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A webhook action, for example `create`, `update`, or `revoked`.
///
/// Rule: 1 to 32 ASCII letters; the first is a lowercase letter.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Action(String);

impl Action {
    /// An entity was created.
    pub const CREATE: &'static str = "create";
    /// An entity changed. `updatedFrom` holds the old values.
    pub const UPDATE: &'static str = "update";
    /// An entity was removed.
    pub const REMOVE: &'static str = "remove";
    /// The workspace revoked the OAuth app.
    pub const REVOKED: &'static str = "revoked";
    /// An agent session started.
    pub const CREATED: &'static str = "created";
    /// A user sent a new message in an agent session.
    pub const PROMPTED: &'static str = "prompted";

    /// Parse an action.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] when the value breaks the rule.
    pub fn parse(value: &str) -> Result<Self> {
        let ok = value.len() <= 32
            && value.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
            && value.bytes().all(|b| b.is_ascii_alphabetic());
        if ok {
            Ok(Self(value.to_owned()))
        } else {
            Err(Error::InvalidRequest(format!(
                "action {value:?} must be 1 to 32 letters and start with a lowercase letter"
            )))
        }
    }

    /// The action as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The `Linear-Delivery` header value: one id per payload.
///
/// Rule: 1 to 128 ASCII letters, digits, `-`, and `_`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeliveryId(String);

impl DeliveryId {
    /// Parse a delivery id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] when the value breaks the rule.
    pub fn parse(value: &str) -> Result<Self> {
        let ok = !value.is_empty()
            && value.len() <= MAX_DELIVERY_LEN
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if ok {
            Ok(Self(value.to_owned()))
        } else {
            Err(Error::InvalidRequest(
                "the delivery id must be 1 to 128 letters, digits, '-', or '_'".to_owned(),
            ))
        }
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeliveryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who acts with an OAuth token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Actor {
    /// The user who installed the app. Linear default.
    #[default]
    User,
    /// The app itself. Use it for agents and service accounts.
    App,
}

impl Actor {
    /// Parse `user` or `app`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] for another value.
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "user" => Ok(Self::User),
            "app" => Ok(Self::App),
            other => Err(Error::Config(format!(
                "actor {other:?} must be \"user\" or \"app\""
            ))),
        }
    }

    /// The value of the `actor` authorize parameter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::App => "app",
        }
    }
}

#[cfg(test)]
mod tests;
