//! The triage rule: pure functions, so the workflow stays deterministic.

use autumn_plugin_linear::graphql::Issue;
use serde::{Deserialize, Serialize};

use crate::settings::ORG;

/// The issue fields the rule reads. The workflow keeps them in its
/// history, so they must serialize.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct IssueFacts {
    /// The issue id.
    pub(crate) id: String,
    /// The identifier, for example `ENG-42`.
    pub(crate) identifier: Option<String>,
    /// The title.
    pub(crate) title: Option<String>,
    /// The description.
    pub(crate) description: Option<String>,
}

impl From<Issue> for IssueFacts {
    fn from(issue: Issue) -> Self {
        Self {
            id: issue.id,
            identifier: issue.identifier,
            title: issue.title,
            description: issue.description,
        }
    }
}

/// Where an issue goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Routing {
    /// The work queue, for example `incident`.
    pub(crate) queue: String,
    /// The Linear priority: 1 urgent, 2 high, 3 medium, 4 low.
    pub(crate) priority: u8,
}

/// Route an issue by the words in its title and description.
pub(crate) fn route_issue(issue: &IssueFacts) -> Routing {
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
pub(crate) fn receipt_body(routing: &Routing) -> String {
    format!(
        "Intake desk: routed to the **{}** queue with priority {}.",
        routing.queue, routing.priority
    )
}

/// The work key of the receipt. Each retry derives the same comment id.
pub(crate) fn receipt_key(issue_id: &str) -> String {
    format!("intake-desk:receipt:{ORG}:{issue_id}")
}

#[cfg(test)]
mod tests;
