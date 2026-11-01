#![allow(clippy::expect_used)]

use std::time::Duration;

use proptest::prelude::*;
use serde_json::json;

use super::*;
use crate::error::ErrorKind;

fn body(value: &Value) -> Vec<u8> {
    value.to_string().into_bytes()
}

#[test]
fn data_comes_back_on_success() {
    let data =
        parse_response(200, None, &body(&json!({"data": {"viewer": {"id": "u1"}}}))).expect("ok");
    assert_eq!(data["viewer"]["id"], "u1");
}

#[test]
fn errors_win_over_partial_data() {
    let answer = json!({
        "data": {"issue": null},
        "errors": [{"message": "Entity not found", "extensions": {"code": "INVALID_INPUT"}}, {"message": "second"}],
    });
    let err = parse_response(200, None, &body(&answer)).expect_err("errors");
    assert_eq!(err.kind(), ErrorKind::GraphQl);
    assert_eq!(
        err.to_string(),
        "linear graphql error: Entity not found; second"
    );
    // An error without a message still counts.
    let err = parse_response(200, None, &body(&json!({"errors": [{}]}))).expect_err("errors");
    assert_eq!(err.to_string(), "linear graphql error: unknown error");
}

#[test]
fn a_400_with_graphql_errors_gives_the_messages() {
    let answer = json!({"errors": [{"message": "Variable \"$id\" is required"}]});
    let err = parse_response(400, None, &body(&answer)).expect_err("errors");
    assert_eq!(err.kind(), ErrorKind::GraphQl);
}

#[test]
fn rate_limits_are_found_by_status_and_by_code() {
    let err = parse_response(429, Some("7"), b"slow down").expect_err("limited");
    match err {
        Error::RateLimited {
            retries,
            retry_after,
        } => {
            assert_eq!(retries, 0);
            assert_eq!(retry_after, Some(Duration::from_secs(7)));
        }
        other => panic_kind(&other),
    }
    let coded = json!({"errors": [{"message": "Rate limit exceeded", "extensions": {"code": "RATELIMITED"}}]});
    for status in [200, 400] {
        let err = parse_response(status, None, &body(&coded)).expect_err("limited");
        assert_eq!(err.kind(), ErrorKind::RateLimited, "{status}");
    }
}

#[expect(clippy::panic, reason = "a test helper")]
fn panic_kind(err: &Error) {
    panic!("wrong error: {err:?}");
}

#[test]
fn http_errors_keep_the_status_and_truncate_the_body() {
    let long = "x".repeat(2_000);
    let err = parse_response(503, None, long.as_bytes()).expect_err("fails");
    match &err {
        Error::LinearApi { status, body } => {
            assert_eq!(*status, 503);
            assert_eq!(body.chars().count(), MAX_ERROR_BODY_CHARS);
        }
        other => panic_kind(other),
    }
    assert!(err.is_transient());
    let err = parse_response(401, None, br#"{"error":"bad token"}"#).expect_err("fails");
    assert!(
        matches!(err, Error::LinearApi { status: 401, .. }),
        "{err:?}"
    );
}

#[test]
fn bad_success_bodies_are_unexpected() {
    for answer in [
        b"not json".to_vec(),
        body(&json!({})),
        body(&json!({"data": null})),
        body(&json!([1])),
    ] {
        let err = parse_response(200, None, &answer).expect_err("bad");
        assert_eq!(
            err.kind(),
            ErrorKind::UnexpectedResponse,
            "{}",
            String::from_utf8_lossy(&answer)
        );
    }
}

#[test]
fn retry_after_reads_whole_and_decimal_seconds() {
    assert_eq!(parse_retry_after("3"), Some(Duration::from_secs(3)));
    assert_eq!(
        parse_retry_after(" 1.5 "),
        Some(Duration::from_millis(1_500))
    );
    assert_eq!(parse_retry_after("-1"), None);
    assert_eq!(parse_retry_after("soon"), None);
    assert_eq!(parse_retry_after("inf"), None);
}

#[test]
fn extract_reads_the_root_field() {
    let viewer: Viewer = ops::viewer()
        .extract(json!({"viewer": {"id": "u1", "name": "Ada"}}))
        .expect("viewer");
    assert_eq!(viewer.name.as_deref(), Some("Ada"));
    let err = ops::viewer()
        .extract::<Viewer>(json!({"other": {}}))
        .expect_err("missing");
    assert_eq!(err.kind(), ErrorKind::UnexpectedResponse);
    let err = ops::viewer()
        .extract::<Viewer>(json!({"viewer": {"name": "no id"}}))
        .expect_err("shape");
    assert_eq!(err.kind(), ErrorKind::UnexpectedResponse);
}

#[test]
fn extract_checks_mutation_success_and_reads_the_payload_field() {
    let op = ops::create_comment(&CommentCreate::new("i-1", "hi"));
    let ok = json!({"commentCreate": {"success": true, "comment": {"id": "c-1", "body": "hi"}}});
    let comment: Comment = op.extract(ok).expect("comment");
    assert_eq!(comment.id, "c-1");
    let refused = json!({"commentCreate": {"success": false, "comment": null}});
    let err = op.extract::<Comment>(refused).expect_err("refused");
    assert!(err.to_string().contains("commentCreate"), "{err}");
    // A payload with no field gives the whole payload.
    let op = ops::delete_webhook("w-1");
    let payload: Value = op
        .extract(json!({"webhookDelete": {"success": true}}))
        .expect("payload");
    assert_eq!(payload["success"], true);
}

#[test]
fn operation_bodies_name_their_inputs() {
    let op = ops::issue("ENG-1");
    assert_eq!(op.body()["variables"], json!({"id": "ENG-1"}));
    assert!(op.query.contains("issue(id: $id)") && op.query.contains("identifier"));

    let op = ops::issues(
        &json!({"labels": {"name": {"eq": "intake"}}}),
        25,
        Some("cur"),
    );
    assert_eq!(op.variables["first"], 25);
    assert_eq!(op.variables["after"], "cur");
    assert!(op.query.contains("pageInfo { hasNextPage endCursor }"));

    let mut create = IssueCreate::new("t-1", "Title");
    create.priority = Some(2);
    let op = ops::create_issue(&create);
    assert_eq!(
        op.variables["input"],
        json!({"teamId": "t-1", "title": "Title", "priority": 2})
    );

    let update = IssueUpdate {
        state_id: Some("s-2".into()),
        added_label_ids: Some(vec!["l-1".into()]),
        ..IssueUpdate::default()
    };
    let op = ops::update_issue("i-1", &update);
    assert_eq!(
        op.variables,
        json!({"id": "i-1", "input": {"stateId": "s-2", "addedLabelIds": ["l-1"]}})
    );

    let comment = CommentCreate::new("i-1", "Routed").with_stable_id("intake:i-1");
    let op = ops::create_comment(&comment);
    assert_eq!(op.variables["input"]["id"], stable_uuid("intake:i-1"));
    assert_eq!(op.variables["input"]["issueId"], "i-1");

    let op = ops::find_comment("c-1");
    assert!(
        op.query
            .contains("comments(filter: { id: { eq: $id } }, first: 1)")
    );

    let thought = ActivityContent::Thought {
        body: "Looking".into(),
    };
    let op = ops::create_agent_activity("sess-1", &thought, Some("a-1"));
    assert_eq!(
        op.variables["input"],
        json!({"agentSessionId": "sess-1", "content": {"type": "thought", "body": "Looking"}, "id": "a-1"})
    );
    let action = ActivityContent::Action {
        action: "Searching".into(),
        parameter: "logs".into(),
        result: None,
    };
    assert_eq!(
        serde_json::to_value(&action).expect("json"),
        json!({"type": "action", "action": "Searching", "parameter": "logs"})
    );

    let op = ops::add_agent_session_url("sess-1", "Run", "https://ci.example.com/1");
    assert_eq!(
        op.variables["input"]["addedExternalUrls"][0]["url"],
        "https://ci.example.com/1"
    );

    let hook = WebhookCreate {
        url: "https://app.example.com/linear/webhooks".into(),
        resource_types: vec!["Issue".into()],
        all_public_teams: Some(true),
        ..WebhookCreate::default()
    };
    let op = ops::create_webhook(&hook);
    assert_eq!(
        op.variables["input"],
        json!({"url": "https://app.example.com/linear/webhooks", "resourceTypes": ["Issue"], "allPublicTeams": true})
    );
    assert_eq!(ops::organization().root, "organization");
}

#[test]
fn connections_report_the_next_cursor() {
    let page: Connection<Created> = serde_json::from_value(
        json!({"nodes": [{"id": "a"}], "pageInfo": {"hasNextPage": true, "endCursor": "c1"}}),
    )
    .expect("page");
    assert_eq!(page.next_cursor(), Some("c1"));
    let last: Connection<Created> = serde_json::from_value(
        json!({"nodes": [], "pageInfo": {"hasNextPage": false, "endCursor": "c2"}}),
    )
    .expect("page");
    assert_eq!(last.next_cursor(), None);
    let bare: Connection<Created> = serde_json::from_value(json!({"nodes": []})).expect("page");
    assert_eq!(bare.next_cursor(), None);
}

#[test]
fn stable_uuid_is_a_v4_format_uuid() {
    let id = stable_uuid("intake:i-1");
    assert_eq!(id, stable_uuid("intake:i-1"));
    assert_ne!(id, stable_uuid("intake:i-2"));
    let parts: Vec<&str> = id.split('-').collect();
    assert_eq!(
        parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
        vec![8, 4, 4, 4, 12]
    );
    assert!(parts[2].starts_with('4'), "{id}");
    assert!(
        matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'),
        "{id}"
    );
    assert!(
        id.bytes()
            .all(|b| b == b'-' || b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
}

proptest! {
    #[test]
    fn stable_uuids_always_have_the_v4_shape(key in "\\PC{0,64}") {
        let id = stable_uuid(&key);
        prop_assert_eq!(id.len(), 36);
        prop_assert_eq!(&id[14..15], "4");
        prop_assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn the_parser_never_panics(status in 100_u16..600, body in proptest::collection::vec(any::<u8>(), 0..200)) {
        let _ = parse_response(status, None, &body);
    }
}

#[test]
fn a_custom_operation_reads_its_root_without_a_success_check() {
    let op = Operation::new("mutation Mine { thing { id } }", json!({}), "thing");
    let created: Created = op.extract(json!({"thing": {"id": "t-1"}})).expect("thing");
    assert_eq!(created.id, "t-1");
}

#[test]
fn a_rate_limit_code_next_to_data_is_not_retried() {
    // Data means the call ran. A retry could run a mutation two times.
    let answer = json!({
        "data": {"issueCreate": {"success": true, "issue": {"id": "i-1"}}},
        "errors": [{"message": "slow down", "extensions": {"code": "RATELIMITED"}}],
    });
    let err = parse_response(200, None, &body(&answer)).expect_err("errors");
    assert_eq!(err.kind(), ErrorKind::GraphQl);
    assert!(!err.is_transient());
}

#[test]
fn debug_hides_the_webhook_secret_and_variables() {
    let hook = WebhookCreate {
        url: "https://x.example.com".into(),
        resource_types: vec!["Issue".into()],
        secret: Some("whsec-top".into()),
        ..WebhookCreate::default()
    };
    assert!(!format!("{hook:?}").contains("whsec-top"));
    let op = ops::create_webhook(&hook);
    assert!(!format!("{op:?}").contains("whsec-top"), "{op:?}");
    assert_eq!(
        op.variables["input"]["secret"], "whsec-top",
        "the request still carries it"
    );
}

#[test]
fn constructors_for_app_code() {
    let org = Organization::new("org-1");
    assert_eq!(org.id, "org-1");
    let create = IssueCreate::new("t-1", "T").with_stable_id("k");
    assert_eq!(create.id, Some(stable_uuid("k")));
    let op = ops::issues(&json!({}), u32::MAX, None);
    assert_eq!(op.variables["first"], i64::from(i32::MAX), "Int is 32-bit");
}
