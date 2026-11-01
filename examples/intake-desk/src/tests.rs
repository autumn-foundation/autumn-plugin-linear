#![allow(clippy::expect_used)]

use super::*;

const DB_ENV: &str = "INTAKE_DESK_TEST_DATABASE_URL";

/// The scenario tests share the `default` harvest queue. One at a time, so
/// that a worker of one test does not take the tasks of another.
static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The test database URL. With no URL, the test skips, but not in CI.
fn database_url() -> Option<String> {
    match std::env::var(DB_ENV) {
        Ok(url) if !url.is_empty() => Some(url),
        _ => {
            assert!(
                std::env::var_os("CI").is_none(),
                "CI must set {DB_ENV} to a Postgres URL"
            );
            eprintln!("skipping: set {DB_ENV} to a Postgres URL to run the example");
            None
        }
    }
}

fn issue(title: &str, description: &str) -> Issue {
    serde_json::from_value(json!({"id": "i", "title": title, "description": description}))
        .expect("issue")
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
    let empty: Issue = serde_json::from_value(json!({"id": "i"})).expect("issue");
    assert_eq!(route_issue(&empty).queue, "feature");
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

#[test]
fn a_wrong_summary_fails_the_check() {
    let scenario = Scenario::happy();
    let err = check(&json!({"workflow": {}}), &scenario).expect_err("wrong");
    assert!(err.starts_with("check failed:"), "{err}");
}

/// An event without the intake label never reaches camel: the handle here
/// has no routes, so a call would fail with 503.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_without_the_intake_label_are_skipped() {
    let app = autumn_app(CamelHandle::new(), "s".to_owned());
    let now = now_ms();
    let update = EventFixture::new("Issue", "update", ORG)
        .data(json!({"id": "i-1", "labelIds": [INTAKE_LABEL]}))
        .updated_from(json!({"title": "old"}))
        .at_ms(now);
    assert_eq!(deliver(&app, &update.sign("s", "d-1")).await, 200);
    let comment = EventFixture::new("Comment", "create", ORG)
        .data(json!({"id": "c-1"}))
        .at_ms(now);
    assert_eq!(deliver(&app, &comment.sign("s", "d-2")).await, 200);
    // Another workspace: skipped, because the route fixes one connection.
    let other = EventFixture::new("Issue", "create", "other-org")
        .data(json!({"id": "i-3", "labelIds": [INTAKE_LABEL]}))
        .at_ms(now);
    assert_eq!(deliver(&app, &other.sign("s", "d-4")).await, 200);
    // With the label, the handler calls camel. No route: 503, so Linear retries.
    let create = EventFixture::new("Issue", "create", ORG)
        .data(json!({"id": "i-2", "labelIds": [INTAKE_LABEL]}))
        .at_ms(now);
    assert_eq!(deliver(&app, &create.sign("s", "d-3")).await, 503);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_issue_gives_one_workflow_and_one_receipt() {
    let Some(url) = database_url() else { return };
    let _guard = DB_LOCK.lock().await;
    let scenario = Scenario::happy();
    let summary = run(&url, &scenario).await.expect("the example passes");
    check(&summary, &scenario).expect("run checks the summary");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_existing_receipt_is_not_posted_again() {
    let Some(url) = database_url() else { return };
    let _guard = DB_LOCK.lock().await;
    let issue_id = format!("i-{}", OsEntropy.hex(8));
    let fake = MemoryTransport::new();
    let earlier = json!({"id": stable_uuid(&receipt_key(&issue_id))});
    fake.reply(
        "POST",
        GRAPHQL_URL,
        data(&json!({"issue": issue_node(&issue_id)})),
    );
    fake.reply(
        "POST",
        GRAPHQL_URL,
        data(&json!({"comments": {"nodes": [earlier]}})),
    );
    fake.reply(
        "POST",
        GRAPHQL_URL,
        data(&json!({"issueUpdate": {"success": true, "issue": issue_node(&issue_id)}})),
    );
    let scenario = Scenario { issue_id, fake };
    let err = run(&url, &scenario)
        .await
        .expect_err("the check sees no new receipt");
    let err = err.to_string();
    assert!(err.starts_with("check failed:"), "{err}");
    // The message is "the summary {actual} is not {expected}".
    let actual = err.split(" is not ").next().unwrap_or_default();
    assert!(actual.contains(r#""receipt_created":false"#), "{err}");
    assert!(!actual.contains("AutumnCommentCreate"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rate_limit_is_retried_by_harvest() {
    let Some(url) = database_url() else { return };
    let _guard = DB_LOCK.lock().await;
    let issue_id = format!("i-{}", OsEntropy.hex(8));
    let fake = MemoryTransport::new();
    fake.reply(
        "POST",
        GRAPHQL_URL,
        HttpResponse::json(
            200,
            &json!({"errors": [{"message": "Rate limit exceeded", "extensions": {"code": "RATELIMITED"}}]}),
        ),
    );
    for reply in happy_replies(&issue_id) {
        fake.reply("POST", GRAPHQL_URL, reply);
    }
    let scenario = Scenario { issue_id, fake };
    let err = run(&url, &scenario).await.expect_err("one extra call");
    let err = err.to_string();
    // The workflow succeeds. The check sees the extra issue call.
    assert!(err.starts_with("check failed:"), "{err}");
    let actual = err.split(" is not ").next().unwrap_or_default();
    assert!(actual.contains(r#""receipt_created":true"#), "{err}");
    assert!(
        actual.contains(r#"["AutumnIssue","AutumnIssue","AutumnFindComment""#),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_graphql_error_fails_the_workflow_with_no_retry() {
    let Some(url) = database_url() else { return };
    let _guard = DB_LOCK.lock().await;
    let issue_id = format!("i-{}", OsEntropy.hex(8));
    let fake = MemoryTransport::new();
    fake.reply("POST", GRAPHQL_URL, data_error("Entity not found: Issue"));
    let scenario = Scenario {
        issue_id,
        fake: fake.clone(),
    };
    let err = run(&url, &scenario).await.expect_err("the workflow fails");
    let err = err.to_string();
    assert!(err.starts_with(&format!("workflow intake-{ORG}-")), "{err}");
    assert_eq!(fake.requests_to(GRAPHQL_URL).len(), 1, "{err}");
}

fn data_error(message: &str) -> HttpResponse {
    HttpResponse::json(200, &json!({"errors": [{"message": message}]}))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_http_429_is_retried_by_harvest() {
    let Some(url) = database_url() else { return };
    let _guard = DB_LOCK.lock().await;
    let issue_id = format!("i-{}", OsEntropy.hex(8));
    let fake = MemoryTransport::new();
    // Linear documents 429 with RATELIMITED and Retry-After.
    let mut limited = HttpResponse::json(
        429,
        &json!({"errors": [{"message": "Rate limit exceeded", "extensions": {"code": "RATELIMITED"}}]}),
    );
    limited
        .headers
        .push(("retry-after".to_owned(), "1".to_owned()));
    fake.reply("POST", GRAPHQL_URL, limited);
    for reply in happy_replies(&issue_id) {
        fake.reply("POST", GRAPHQL_URL, reply);
    }
    let scenario = Scenario { issue_id, fake };
    let err = run(&url, &scenario).await.expect_err("one extra call");
    let actual = err.to_string();
    let actual = actual
        .split(" is not ")
        .next()
        .unwrap_or_default()
        .to_owned();
    assert!(actual.contains(r#""receipt_created":true"#), "{err}");
    assert!(
        actual.contains(r#"["AutumnIssue","AutumnIssue","AutumnFindComment""#),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_create_finds_the_receipt_an_earlier_attempt_made() {
    let Some(url) = database_url() else { return };
    let _guard = DB_LOCK.lock().await;
    let issue_id = format!("i-{}", OsEntropy.hex(8));
    let receipt = json!({"id": stable_uuid(&receipt_key(&issue_id))});
    let fake = MemoryTransport::new();
    fake.reply(
        "POST",
        GRAPHQL_URL,
        data(&json!({"issue": issue_node(&issue_id)})),
    );
    fake.reply(
        "POST",
        GRAPHQL_URL,
        data(&json!({"comments": {"nodes": []}})),
    );
    // The create of a known id fails. The second find sees the comment.
    fake.reply(
        "POST",
        GRAPHQL_URL,
        data_error("Comment with this id already exists"),
    );
    fake.reply(
        "POST",
        GRAPHQL_URL,
        data(&json!({"comments": {"nodes": [receipt]}})),
    );
    fake.reply(
        "POST",
        GRAPHQL_URL,
        data(&json!({"issueUpdate": {"success": true, "issue": issue_node(&issue_id)}})),
    );
    let scenario = Scenario { issue_id, fake };
    let err = run(&url, &scenario)
        .await
        .expect_err("the check sees no new receipt");
    let actual = err.to_string();
    let actual = actual
        .split(" is not ")
        .next()
        .unwrap_or_default()
        .to_owned();
    assert!(actual.contains(r#""receipt_created":false"#), "{err}");
    assert!(
        actual.contains(r#""AutumnCommentCreate","AutumnFindComment","AutumnIssueUpdate""#),
        "{err}"
    );
}
