//! The scenario: the deliveries that the example sends, and the check of
//! the result.

use autumn_plugin_linear::graphql::stable_uuid;
use autumn_plugin_linear::testing::EventFixture;
use autumn_web::test::TestClient;
use san_mateo::clock::{Entropy, OsEntropy};
use serde_json::{Value, json};

use crate::auth::ACCESS_TOKEN;
use crate::mock_linear::MockLinear;
use crate::settings::{INTAKE_LABEL, ORG};
use crate::triage::receipt_key;
use crate::webhook::deliver;

/// One run: the issue id and the mock Linear API.
pub(crate) struct Scenario {
    pub(crate) issue_id: String,
    pub(crate) linear: MockLinear,
}

impl Scenario {
    /// A happy run with a new issue id.
    pub(crate) async fn happy() -> Self {
        let issue_id = new_issue_id();
        let linear = MockLinear::happy(&issue_id).await;
        Self { issue_id, linear }
    }
}

/// A new issue id, so that runs on one database do not meet.
pub(crate) fn new_issue_id() -> String {
    format!("i-{}", OsEntropy.hex(8))
}

/// Send the five deliveries of the scenario. Returns their statuses.
pub(crate) async fn deliver_all(app: &TestClient, secret: &str, scenario: &Scenario) -> Vec<u16> {
    let now = now_ms();
    let issue = json!({
        "id": scenario.issue_id,
        "identifier": "ENG-42",
        "title": "Checkout crash after the update",
        "labelIds": [INTAKE_LABEL],
    });
    let create = EventFixture::new("Issue", "create", ORG)
        .data(issue)
        .at_ms(now);
    let other = EventFixture::new("Issue", "update", ORG)
        .data(json!({"id": "i-other", "labelIds": []}))
        .updated_from(json!({"title": "old"}))
        .at_ms(now);
    let d1 = format!("d-{}", OsEntropy.hex(8));
    let d2 = format!("d-{}", OsEntropy.hex(8));
    let d3 = format!("d-{}", OsEntropy.hex(8));
    vec![
        // A forged signature.
        deliver(app, &create.sign("not-the-secret", &d1)).await,
        // An update that does not add the label: the handler skips it.
        deliver(app, &other.sign(secret, &d3)).await,
        // The intake event.
        deliver(app, &create.sign(secret, &d1)).await,
        // A retry with a new delivery id: the same workflow.
        deliver(app, &create.sign(secret, &d2)).await,
        // The same delivery again: the plugin skips it.
        deliver(app, &create.sign(secret, &d1)).await,
    ]
}

/// The wall clock in Unix ms. The autumn app checks the webhook time
/// against it.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Build the summary from the workflow output, the webhook answers, and
/// the calls that reached Linear.
pub(crate) async fn summarize(output: &Value, statuses: &[u16], linear: &MockLinear) -> Value {
    let calls = linear.calls().await;
    let bearer = format!("Bearer {ACCESS_TOKEN}");
    let auth = calls
        .iter()
        .all(|call| call.authorization.as_deref() == Some(bearer.as_str()));
    let operations: Vec<&str> = calls.iter().map(|call| call.operation.as_str()).collect();
    let variables_of = |name: &str| {
        calls
            .iter()
            .find(|call| call.operation == name)
            .map(|call| call.variables.clone())
            .unwrap_or_default()
    };
    let create = variables_of("AutumnCommentCreate");
    let update = variables_of("AutumnIssueUpdate");
    json!({
        "workflow": output,
        "webhook_statuses": statuses,
        "linear_operations": operations,
        "auth_added_by_san_mateo": auth,
        "sent": {
            "issue": variables_of("AutumnIssue")["id"],
            "find_comment_id": variables_of("AutumnFindComment")["id"],
            "comment_id": create["input"]["id"],
            "comment_issue": create["input"]["issueId"],
            "update_issue": update["id"],
            "update_priority": update["input"]["priority"],
        },
    })
}

/// Compare the summary with the expected values. The error starts with
/// `check failed:`.
pub(crate) fn check(summary: &Value, scenario: &Scenario) -> Result<(), String> {
    let receipt = stable_uuid(&receipt_key(&scenario.issue_id));
    let expected = json!({
        "workflow": {
            "issue": "ENG-42",
            "queue": "incident",
            "priority": 1,
            "receipt": receipt,
            "receipt_created": true,
        },
        "webhook_statuses": [401, 200, 200, 200, 200],
        "linear_operations": ["AutumnIssue", "AutumnFindComment", "AutumnCommentCreate", "AutumnIssueUpdate"],
        "auth_added_by_san_mateo": true,
        "sent": {
            "issue": scenario.issue_id,
            "find_comment_id": receipt,
            "comment_id": receipt,
            "comment_issue": scenario.issue_id,
            "update_issue": scenario.issue_id,
            "update_priority": 1,
        },
    });
    if *summary == expected {
        Ok(())
    } else {
        Err(format!(
            "check failed: the summary {summary} is not {expected}"
        ))
    }
}

#[cfg(test)]
mod tests;
