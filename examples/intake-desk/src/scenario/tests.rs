//! The scenario tests. They run against Postgres.

#![allow(clippy::expect_used)]

use super::*;
use crate::mock_linear::{CREATE_COMMENT, ISSUE};
use crate::run;

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

/// The actual summary in a `check failed:` error. The message is
/// "the summary {actual} is not {expected}".
fn actual(err: &crate::BoxError) -> String {
    let text = err.to_string();
    assert!(text.starts_with("check failed:"), "{text}");
    text.split(" is not ").next().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn a_wrong_summary_fails_the_check() {
    let scenario = Scenario::happy().await;
    let err = check(&json!({"workflow": {}}), &scenario).expect_err("wrong");
    assert!(err.starts_with("check failed:"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_issue_gives_one_workflow_and_one_receipt() {
    let Some(url) = database_url() else { return };
    let _guard = DB_LOCK.lock().await;
    let scenario = Scenario::happy().await;
    let summary = run(&url, &scenario).await.expect("the example passes");
    check(&summary, &scenario).expect("run checks the summary");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_existing_receipt_is_not_posted_again() {
    let Some(url) = database_url() else { return };
    let _guard = DB_LOCK.lock().await;
    let issue_id = new_issue_id();
    let linear = MockLinear::start().await;
    linear.issue(&issue_id).await;
    linear.receipt_found(&issue_id).await;
    linear.priority_set(&issue_id).await;
    let scenario = Scenario { issue_id, linear };
    let err = run(&url, &scenario)
        .await
        .expect_err("the check sees no new receipt");
    let actual = actual(&err);
    assert!(actual.contains(r#""receipt_created":false"#), "{err}");
    assert!(!actual.contains("AutumnCommentCreate"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rate_limit_is_retried_by_harvest() {
    let Some(url) = database_url() else { return };
    let _guard = DB_LOCK.lock().await;
    let issue_id = new_issue_id();
    let linear = MockLinear::happy(&issue_id).await;
    linear.rate_limited_once(ISSUE).await;
    let scenario = Scenario { issue_id, linear };
    let err = run(&url, &scenario).await.expect_err("one extra call");
    // The workflow succeeds. The check sees the extra issue call.
    let actual = actual(&err);
    assert!(actual.contains(r#""receipt_created":true"#), "{err}");
    assert!(
        actual.contains(r#"["AutumnIssue","AutumnIssue","AutumnFindComment""#),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_http_429_is_retried_by_harvest() {
    let Some(url) = database_url() else { return };
    let _guard = DB_LOCK.lock().await;
    let issue_id = new_issue_id();
    let linear = MockLinear::happy(&issue_id).await;
    // Linear documents 429 with RATELIMITED and Retry-After.
    linear.http_429_once(ISSUE).await;
    let scenario = Scenario { issue_id, linear };
    let err = run(&url, &scenario).await.expect_err("one extra call");
    let actual = actual(&err);
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
    let issue_id = new_issue_id();
    let linear = MockLinear::start().await;
    linear.graphql_error(ISSUE, "Entity not found: Issue").await;
    let scenario = Scenario { issue_id, linear };
    let err = run(&url, &scenario).await.expect_err("the workflow fails");
    let err = err.to_string();
    assert!(err.starts_with(&format!("workflow intake-{ORG}-")), "{err}");
    assert_eq!(scenario.linear.calls().await.len(), 1, "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_create_finds_the_receipt_an_earlier_attempt_made() {
    let Some(url) = database_url() else { return };
    let _guard = DB_LOCK.lock().await;
    let issue_id = new_issue_id();
    let linear = MockLinear::start().await;
    linear.issue(&issue_id).await;
    // The first lookup finds nothing. The create of a known id fails. The
    // second lookup sees the comment.
    linear.no_receipt_once().await;
    linear.receipt_found(&issue_id).await;
    linear
        .graphql_error(CREATE_COMMENT, "Comment with this id already exists")
        .await;
    linear.priority_set(&issue_id).await;
    let scenario = Scenario { issue_id, linear };
    let err = run(&url, &scenario)
        .await
        .expect_err("the check sees no new receipt");
    let actual = actual(&err);
    assert!(actual.contains(r#""receipt_created":false"#), "{err}");
    assert!(
        actual.contains(r#""AutumnCommentCreate","AutumnFindComment","AutumnIssueUpdate""#),
        "{err}"
    );
}
