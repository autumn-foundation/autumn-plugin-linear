#![allow(clippy::expect_used)]

use serde_json::json;

use super::*;
use crate::error::ErrorKind;

const ORG: &str = "9cfb482a-81e3-4154-b5b9-2c805e70a02d";
const TS: u64 = 1_793_000_000_000;

fn envelope(event_type: &str, action: &str, data: &Value) -> Value {
    json!({
        "action": action,
        "type": event_type,
        "actor": {"id": "u-1", "type": "user", "name": "Ada"},
        "createdAt": "2026-10-26T10:00:00.000Z",
        "data": data,
        "url": "https://linear.app/acme/issue/ENG-1",
        "organizationId": ORG,
        "webhookId": "wh-1",
        "webhookTimestamp": TS,
    })
}

fn parse(value: &Value) -> WebhookEvent {
    WebhookEvent::parse(value.to_string().as_bytes()).expect("parses")
}

fn issue_data() -> Value {
    json!({
        "id": "i-1",
        "identifier": "ENG-1",
        "title": "Crash on save",
        "description": "Steps...",
        "priority": 2,
        "teamId": "t-1",
        "team": {"id": "t-1", "key": "ENG", "name": "Engineering"},
        "stateId": "s-1",
        "state": {"id": "s-1", "name": "Triage", "type": "triage"},
        "labelIds": ["l-intake", "l-bug"],
        "labels": [{"id": "l-intake", "name": "Intake"}, {"id": "l-bug", "name": "Bug"}],
        "createdAt": "2026-10-26T10:00:00.000Z",
        "updatedAt": "2026-10-26T10:00:00.000Z",
        "url": "https://linear.app/acme/issue/ENG-1",
        "unknownFutureField": {"x": 1},
    })
}

#[test]
fn parse_reads_every_envelope_field() {
    let event = parse(&envelope("Issue", "create", &issue_data()));
    assert_eq!(event.event_type.as_str(), "Issue");
    assert_eq!(event.action.as_str(), "create");
    assert_eq!(event.organization_id.as_str(), ORG);
    assert_eq!(event.webhook_id.as_deref(), Some("wh-1"));
    assert_eq!(event.webhook_timestamp_ms, TS);
    assert_eq!(
        event.created_at.as_deref(),
        Some("2026-10-26T10:00:00.000Z")
    );
    assert_eq!(
        event.url.as_deref(),
        Some("https://linear.app/acme/issue/ENG-1")
    );
    assert_eq!(
        event.actor.as_ref().and_then(|a| a["name"].as_str()),
        Some("Ada")
    );
    assert_eq!(event.entity_id(), Some("i-1"));
    assert!(event.updated_from.is_none());
    assert!(event.is("Issue") && !event.is("Comment"));
    assert_eq!(event.raw["webhookId"], "wh-1");
}

#[test]
fn a_float_timestamp_is_read_as_ms() {
    let mut body = envelope("Issue", "create", &issue_data());
    body["webhookTimestamp"] = json!(1_793_000_000_123.0_f64);
    assert_eq!(parse(&body).webhook_timestamp_ms, 1_793_000_000_123);
}

#[test]
fn a_missing_or_bad_timestamp_is_a_replay_error() {
    for bad in [
        Value::Null,
        json!("1793000000000"),
        json!(-1),
        json!(f64::MAX),
    ] {
        let mut body = envelope("Issue", "create", &issue_data());
        if bad.is_null() {
            body.as_object_mut()
                .expect("object")
                .remove("webhookTimestamp");
        } else {
            body["webhookTimestamp"] = bad.clone();
        }
        let err = WebhookEvent::parse(body.to_string().as_bytes()).expect_err("refused");
        assert_eq!(err.kind(), ErrorKind::Replay, "{bad:?}");
    }
}

#[test]
fn a_bad_envelope_is_an_invalid_request() {
    let base = envelope("Issue", "create", &issue_data());
    let mut cases = vec![b"not json".to_vec(), b"[1,2]".to_vec()];
    for (field, value) in [
        ("type", Value::Null),
        ("type", json!("issue")),
        ("action", Value::Null),
        ("action", json!("Create")),
        ("organizationId", Value::Null),
        ("organizationId", json!("a b")),
    ] {
        let mut body = base.clone();
        if value.is_null() {
            body.as_object_mut().expect("object").remove(field);
        } else {
            body[field] = value;
        }
        cases.push(body.to_string().into_bytes());
    }
    for case in cases {
        let err = WebhookEvent::parse(&case).expect_err("refused");
        assert_eq!(
            err.kind(),
            ErrorKind::InvalidRequest,
            "{}",
            String::from_utf8_lossy(&case)
        );
    }
}

#[test]
fn entity_key_uses_signed_fields_only() {
    let event = parse(&envelope("Issue", "update", &issue_data()));
    assert_eq!(
        event.entity_key(),
        "Issue:update:i-1:2026-10-26T10:00:00.000Z"
    );
    let mut bare = envelope("OAuthApp", "revoked", &Value::Null);
    bare.as_object_mut().expect("object").remove("createdAt");
    assert_eq!(parse(&bare).entity_key(), "OAuthApp:revoked:-:-");
}

#[test]
fn issue_view_reads_the_data_and_ignores_new_fields() {
    let event = parse(&envelope("Issue", "create", &issue_data()));
    let issue = event.issue().expect("an issue").expect("present");
    assert_eq!(issue.identifier.as_deref(), Some("ENG-1"));
    assert_eq!(issue.priority, Some(2.0));
    assert_eq!(
        issue.team.as_ref().and_then(|t| t.key.as_deref()),
        Some("ENG")
    );
    assert_eq!(
        issue.state.as_ref().and_then(|s| s.state_type.as_deref()),
        Some("triage")
    );
    assert_eq!(issue.label_ids, vec!["l-intake", "l-bug"]);
    assert!(issue.has_label_named("intake") && issue.has_label_named("BUG"));
    assert!(!issue.has_label_named("feature"));
    assert!(event.comment().expect("no error").is_none());
    assert!(event.agent_session().expect("no error").is_none());
}

#[test]
fn a_wrong_issue_shape_is_a_json_error() {
    let event = parse(&envelope("Issue", "create", &json!({"title": "no id"})));
    assert_eq!(event.issue().expect_err("no id").kind(), ErrorKind::Json);
    assert!(event.data_as::<IssueData>().is_err());
}

#[test]
fn comment_view_reads_the_data() {
    let data = json!({"id": "c-1", "body": "Looks good", "issueId": "i-1", "userId": "u-1"});
    let event = parse(&envelope("Comment", "create", &data));
    let comment = event.comment().expect("a comment").expect("present");
    assert_eq!(comment.body.as_deref(), Some("Looks good"));
    assert_eq!(comment.issue_id.as_deref(), Some("i-1"));
    assert!(event.issue().expect("no error").is_none());
}

#[test]
fn updated_from_helpers_find_changes_and_label_moves() {
    let mut body = envelope("Issue", "update", &issue_data());
    body["updatedFrom"] = json!({"labelIds": ["l-bug"], "priority": 0, "updatedAt": "x"});
    let event = parse(&body);
    assert!(event.changed("labelIds") && event.changed("priority"));
    assert!(!event.changed("title"));
    assert_eq!(event.previous("priority"), Some(&json!(0)));
    assert!(event.label_added("l-intake"));
    assert!(!event.label_added("l-bug"), "the label was there before");
    assert!(!event.label_removed("l-bug"));

    // The update removes l-intake: the old list had it, the new one not.
    let mut data = issue_data();
    data["labelIds"] = json!(["l-bug"]);
    let mut body = envelope("Issue", "update", &data);
    body["updatedFrom"] = json!({"labelIds": ["l-intake", "l-bug"]});
    let event = parse(&body);
    assert!(event.label_removed("l-intake"));
    assert!(!event.label_added("l-intake"));
}

#[test]
fn a_create_with_the_label_counts_as_added() {
    let event = parse(&envelope("Issue", "create", &issue_data()));
    assert!(event.label_added("l-intake"));
    assert!(!event.label_removed("l-intake"));
    // An update that does not change the labels adds nothing.
    let mut body = envelope("Issue", "update", &issue_data());
    body["updatedFrom"] = json!({"title": "old"});
    assert!(!parse(&body).label_added("l-intake"));
    // A remove adds nothing.
    assert!(!parse(&envelope("Issue", "remove", &issue_data())).label_added("l-intake"));
}

fn agent_body(action: &str, activity: &Value) -> Value {
    json!({
        "type": "AgentSessionEvent",
        "action": action,
        "organizationId": ORG,
        "webhookTimestamp": TS,
        "webhookId": "wh-2",
        "appUserId": "app-user-1",
        "oauthClientId": "client-1",
        "createdAt": "2026-10-26T10:00:00.000Z",
        "promptContext": "<issue identifier=\"ENG-1\">Crash on save</issue>",
        "agentSession": {
            "id": "sess-1",
            "status": "pending",
            "issue": {"id": "i-1", "identifier": "ENG-1", "title": "Crash on save"},
            "comment": {"id": "c-9", "body": "@agent please look"},
        },
        "agentActivity": activity,
    })
}

#[test]
fn agent_session_view_reads_created_events() {
    let event = parse(&agent_body("created", &Value::Null));
    let session = event.agent_session().expect("no error").expect("present");
    assert_eq!(session.session_id(), "sess-1");
    assert_eq!(session.action, "created");
    assert_eq!(session.app_user_id.as_deref(), Some("app-user-1"));
    assert!(
        session
            .prompt_context
            .as_deref()
            .is_some_and(|p| p.contains("ENG-1"))
    );
    let issue = session.agent_session.issue.as_ref().expect("issue");
    assert_eq!(issue.identifier.as_deref(), Some("ENG-1"));
    assert_eq!(
        session
            .agent_session
            .comment
            .as_ref()
            .map(|c| c.id.as_str()),
        Some("c-9")
    );
    assert_eq!(session.prompt_body(), None);
    assert!(!session.is_stop());
    assert_eq!(event.entity_id(), None);
}

#[test]
fn agent_session_view_reads_prompts_and_the_stop_signal() {
    let prompt = json!({"id": "a-1", "content": {"type": "prompt", "body": "try again"}});
    let session = parse(&agent_body("prompted", &prompt))
        .agent_session()
        .expect("ok")
        .expect("present");
    assert_eq!(session.prompt_body(), Some("try again"));
    assert!(!session.is_stop());

    let stop =
        json!({"id": "a-2", "content": {"type": "prompt", "body": "stop"}, "signal": "stop"});
    let session = parse(&agent_body("prompted", &stop))
        .agent_session()
        .expect("ok")
        .expect("present");
    assert!(session.is_stop());

    // Linear also documents `agentActivity.body`.
    let flat = json!({"id": "a-3", "body": "flat body"});
    let session = parse(&agent_body("prompted", &flat))
        .agent_session()
        .expect("ok")
        .expect("present");
    assert_eq!(session.prompt_body(), Some("flat body"));
}
