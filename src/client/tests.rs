#![allow(clippy::expect_used)]

use std::time::{Duration, Instant};

use serde_json::json;

use super::*;
use crate::error::ErrorKind;
use crate::graphql::{CommentCreate, IssueCreate, IssueUpdate, WebhookCreate};
use crate::test_support::{Stub, StubResponse};

fn client(stub: &Stub, credential: Credential) -> LinearClient {
    LinearClient::new(credential)
        .expect("builds")
        .with_endpoint(stub.graphql_url())
        .with_retry(RetryPolicy::new(3, Duration::from_millis(50)))
}

fn limited(retry_after: &str) -> StubResponse {
    StubResponse::json(429, &json!({"errors": [{"message": "Rate limit exceeded", "extensions": {"code": "RATELIMITED"}}]}))
        .header("retry-after", retry_after)
}

#[tokio::test]
async fn an_api_key_goes_raw_and_an_oauth_token_as_bearer() {
    let stub = Stub::data(&[
        json!({"viewer": {"id": "u1"}}),
        json!({"viewer": {"id": "u2"}}),
    ])
    .await;
    let viewer = client(&stub, Credential::api_key("lin_api_1"))
        .viewer()
        .await
        .expect("viewer");
    assert_eq!(viewer.id, "u1");
    client(&stub, Credential::bearer("oauth-1"))
        .viewer()
        .await
        .expect("viewer");
    let seen = stub.requests();
    assert_eq!(seen[0].header("authorization"), Some("lin_api_1"));
    assert_eq!(seen[1].header("authorization"), Some("Bearer oauth-1"));
    assert_eq!(seen[0].path, "/graphql");
    assert_eq!(seen[0].header("content-type"), Some("application/json"));
    assert!(
        seen[0]
            .header("user-agent")
            .is_some_and(|ua| ua.starts_with("autumn-plugin-linear/"))
    );
    assert!(
        seen[0].json()["query"]
            .as_str()
            .is_some_and(|q| q.contains("viewer"))
    );
}

#[test]
fn the_default_endpoint_is_linear() {
    let client = LinearClient::new(Credential::api_key("k")).expect("builds");
    assert_eq!(client.endpoint(), "https://api.linear.app/graphql");
}

#[test]
fn a_blank_credential_is_refused() {
    let err = LinearClient::new(Credential::api_key("  ")).expect_err("blank");
    assert_eq!(err.kind(), ErrorKind::Config);
}

#[tokio::test]
async fn a_rate_limit_retries_after_the_wait_then_succeeds() {
    let stub = Stub::start(vec![
        limited("0.01"),
        StubResponse::json(
            200,
            &json!({"errors": [{"message": "slow", "extensions": {"code": "RATELIMITED"}}]}),
        )
        .header("retry-after", "0.01"),
        StubResponse::json(200, &json!({"data": {"viewer": {"id": "u1"}}})),
    ])
    .await;
    let started = Instant::now();
    let viewer = client(&stub, Credential::api_key("k"))
        .viewer()
        .await
        .expect("viewer");
    assert_eq!(viewer.id, "u1");
    assert_eq!(stub.requests().len(), 3);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn retries_run_out_and_keep_the_wait() {
    let stub = Stub::start(vec![
        limited("0"),
        limited("0"),
        limited("0"),
        limited("0.02"),
    ])
    .await;
    let err = client(&stub, Credential::api_key("k"))
        .viewer()
        .await
        .expect_err("limited");
    match err {
        Error::RateLimited {
            retries,
            retry_after,
        } => {
            assert_eq!(retries, 3);
            assert_eq!(retry_after, Some(Duration::from_millis(20)));
        }
        other => unreachable!("{other:?}"),
    }
    assert_eq!(stub.requests().len(), 4);
}

#[tokio::test]
async fn a_wait_longer_than_the_cap_stops_at_once() {
    let stub = Stub::start(vec![limited("3600")]).await;
    let started = Instant::now();
    let err = client(&stub, Credential::api_key("k"))
        .viewer()
        .await
        .expect_err("limited");
    assert!(
        matches!(err, Error::RateLimited { retries: 0, retry_after: Some(d) } if d == Duration::from_secs(3600)),
        "{err:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(stub.requests().len(), 1);
}

#[tokio::test]
async fn zero_retries_fail_on_the_first_limit() {
    let stub = Stub::start(vec![limited("0")]).await;
    let err = client(&stub, Credential::api_key("k"))
        .with_retry(RetryPolicy::new(0, Duration::from_secs(1)))
        .viewer()
        .await
        .expect_err("limited");
    assert_eq!(err.kind(), ErrorKind::RateLimited);
}

#[tokio::test]
async fn server_errors_and_graphql_errors_do_not_retry() {
    let stub = Stub::start(vec![StubResponse::text(502, "bad gateway")]).await;
    let err = client(&stub, Credential::api_key("k"))
        .create_comment(&CommentCreate::new("i-1", "hi"))
        .await
        .expect_err("fails");
    assert!(
        matches!(err, Error::LinearApi { status: 502, .. }),
        "{err:?}"
    );
    assert_eq!(
        stub.requests().len(),
        1,
        "a mutation must not run two times"
    );

    let stub = Stub::start(vec![StubResponse::json(
        200,
        &json!({"errors": [{"message": "Entity not found"}]}),
    )])
    .await;
    let err = client(&stub, Credential::api_key("k"))
        .issue("ENG-9")
        .await
        .expect_err("fails");
    assert_eq!(err.kind(), ErrorKind::GraphQl);
    assert_eq!(stub.requests().len(), 1);
}

#[tokio::test]
async fn a_redirect_is_not_followed() {
    let stub = Stub::start(vec![
        StubResponse::text(302, "").header("location", "https://evil.example.com/steal"),
    ])
    .await;
    let err = client(&stub, Credential::api_key("secret-key"))
        .viewer()
        .await
        .expect_err("fails");
    assert!(
        matches!(err, Error::LinearApi { status: 302, .. }),
        "{err:?}"
    );
    assert!(!err.to_string().contains("secret-key"));
}

#[tokio::test]
async fn a_transport_failure_is_an_http_error_without_the_token() {
    let client = LinearClient::new(Credential::bearer("tok-secret"))
        .expect("builds")
        .with_endpoint("http://127.0.0.1:9/graphql");
    let err = client.viewer().await.expect_err("fails");
    assert_eq!(err.kind(), ErrorKind::Http);
    assert!(!err.to_string().contains("tok-secret"));
}

#[test]
fn debug_redacts_the_credential() {
    let text = format!(
        "{:?}",
        LinearClient::new(Credential::api_key("lin_api_secret")).expect("builds")
    );
    assert!(!text.contains("lin_api_secret"), "{text}");
    assert!(text.contains("<redacted>"));
    assert!(!format!("{:?}", Credential::bearer("tok")).contains("tok\""));
}

#[derive(serde::Deserialize)]
struct Thing {
    thing: Value,
}

#[tokio::test]
#[allow(clippy::too_many_lines, reason = "one walk over every helper")]
async fn typed_helpers_send_their_operations() {
    let issue = json!({"id": "i-1", "identifier": "ENG-1", "title": "T", "labels": {"nodes": [{"id": "l-1", "name": "Bug"}]}});
    let stub = Stub::data(&[
        json!({"organization": {"id": "org-1", "name": "Acme", "urlKey": "acme"}}),
        json!({"issue": issue}),
        json!({"issues": {"nodes": [issue], "pageInfo": {"hasNextPage": true, "endCursor": "c1"}}}),
        json!({"issueCreate": {"success": true, "issue": issue}}),
        json!({"issueUpdate": {"success": true, "issue": issue}}),
        json!({"commentCreate": {"success": true, "comment": {"id": "c-1", "body": "hi"}}}),
        json!({"comments": {"nodes": [{"id": "c-1"}]}}),
        json!({"comments": {"nodes": []}}),
        json!({"agentActivityCreate": {"success": true, "agentActivity": {"id": "a-1"}}}),
        json!({"agentSessionUpdate": {"success": true}}),
        json!({"webhookCreate": {"success": true, "webhook": {"id": "w-1", "enabled": true}}}),
        json!({"webhookDelete": {"success": true}}),
        json!({"thing": {"n": 3}}),
    ])
    .await;
    let c = client(&stub, Credential::bearer("t"));
    assert_eq!(
        c.organization().await.expect("org").url_key.as_deref(),
        Some("acme")
    );
    let one = c.issue("ENG-1").await.expect("issue");
    assert_eq!(one.labels.map(|l| l.nodes.len()), Some(1));
    let page = c.issues(&json!({}), 10, None).await.expect("page");
    assert_eq!(page.next_cursor(), Some("c1"));
    assert_eq!(
        c.create_issue(&IssueCreate::new("t-1", "T"))
            .await
            .expect("create")
            .id,
        "i-1"
    );
    assert_eq!(
        c.update_issue("i-1", &IssueUpdate::default())
            .await
            .expect("update")
            .id,
        "i-1"
    );
    assert_eq!(
        c.create_comment(&CommentCreate::new("i-1", "hi"))
            .await
            .expect("comment")
            .id,
        "c-1"
    );
    assert_eq!(
        c.find_comment("c-1").await.expect("find").map(|c| c.id),
        Some("c-1".to_owned())
    );
    assert_eq!(c.find_comment("c-2").await.expect("find"), None);
    let thought = crate::graphql::ActivityContent::Thought {
        body: "on it".into(),
    };
    assert_eq!(
        c.create_agent_activity("s-1", &thought)
            .await
            .expect("activity"),
        "a-1"
    );
    c.add_agent_session_url("s-1", "Run", "https://ci.example.com")
        .await
        .expect("url");
    let hook = WebhookCreate {
        url: "https://x.example.com/linear/webhooks".into(),
        resource_types: vec!["Issue".into()],
        ..WebhookCreate::default()
    };
    assert_eq!(c.create_webhook(&hook).await.expect("webhook").id, "w-1");
    c.delete_webhook("w-1").await.expect("delete");
    let data: Thing = c
        .query("query { thing { n } }", json!({}))
        .await
        .expect("query");
    assert_eq!(data.thing["n"], 3);
    let names: Vec<String> = stub
        .requests()
        .iter()
        .map(|r| {
            r.json()["query"]
                .as_str()
                .unwrap_or_default()
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .split('(')
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(
        &names[..4],
        [
            "AutumnOrganization",
            "AutumnIssue",
            "AutumnIssues",
            "AutumnIssueCreate"
        ]
    );
}

#[tokio::test]
async fn a_missing_retry_after_uses_the_default_wait_against_the_cap() {
    // The default wait is 1 s. A 50 ms cap stops at once and keeps no wait.
    let stub = Stub::start(vec![StubResponse::json(429, &json!({}))]).await;
    let err = client(&stub, Credential::api_key("k"))
        .viewer()
        .await
        .expect_err("limited");
    assert!(
        matches!(
            err,
            Error::RateLimited {
                retries: 0,
                retry_after: None
            }
        ),
        "{err:?}"
    );
}
