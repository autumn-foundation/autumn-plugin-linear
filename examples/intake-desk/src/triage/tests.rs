#![allow(clippy::expect_used)]

use autumn_plugin_linear::graphql::stable_uuid;
use serde_json::json;

use super::*;

fn issue(title: &str, description: &str) -> IssueFacts {
    IssueFacts {
        id: "i".into(),
        identifier: None,
        title: Some(title.into()),
        description: Some(description.into()),
    }
}

#[test]
fn the_rule_routes_by_words() {
    let cases = [
        ("Checkout crash", "", "incident", 1),
        ("Login is down", "", "incident", 1),
        ("Export fails", "", "bug", 2),
        ("Wrong total", "an error in the cart", "bug", 2),
        ("How do I export?", "", "question", 4),
        ("Dark mode", "please add it", "feature", 3),
    ];
    for (title, description, queue, priority) in cases {
        let routing = route_issue(&issue(title, description));
        assert_eq!(routing.queue, queue, "{title}");
        assert_eq!(routing.priority, priority, "{title}");
    }
    // Linear's answer becomes facts with no title or description.
    let linear: Issue = serde_json::from_value(json!({"id": "i"})).expect("issue");
    assert_eq!(route_issue(&IssueFacts::from(linear)).queue, "feature");
}

#[test]
fn the_receipt_id_is_stable_per_issue() {
    assert_eq!(
        stable_uuid(&receipt_key("i-1")),
        stable_uuid(&receipt_key("i-1"))
    );
    assert_ne!(
        stable_uuid(&receipt_key("i-1")),
        stable_uuid(&receipt_key("i-2"))
    );
    let body = receipt_body(&Routing {
        queue: "bug".into(),
        priority: 2,
    });
    assert!(body.contains("**bug**") && body.contains('2'), "{body}");
}
