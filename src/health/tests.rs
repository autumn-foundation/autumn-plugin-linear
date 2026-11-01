#![allow(clippy::expect_used)]

use autumn_web::actuator::{HealthIndicator, HealthStatus};
use serde_json::json;

use super::*;
use crate::client::{Credential, RetryPolicy};
use crate::test_support::{Stub, StubResponse};

fn client(stub: &Stub, key: &str) -> LinearClient {
    LinearClient::new(Credential::api_key(key))
        .expect("builds")
        .with_endpoint(stub.graphql_url())
        .with_retry(RetryPolicy::new(0, std::time::Duration::ZERO))
}

#[tokio::test]
async fn a_working_key_reports_up_with_the_viewer_name() {
    let stub = Stub::data(&[json!({"viewer": {"id": "u1", "name": "Ada"}})]).await;
    let output = indicator(&client(&stub, "k")).check().await;
    assert_eq!(output.status, HealthStatus::Up);
    assert_eq!(output.details["viewer"], "Ada");
}

#[tokio::test]
async fn a_failing_key_reports_down_without_the_key() {
    let stub = Stub::start(vec![StubResponse::json(
        401,
        &json!({"errors": [{"message": "Authentication required"}]}),
    )])
    .await;
    let output = LinearHealth::new(client(&stub, "lin_api_secret"))
        .check()
        .await;
    assert_eq!(output.status, HealthStatus::Down);
    let text = output.details["error"].as_str().expect("text").to_owned();
    assert!(text.contains("Authentication required"), "{text}");
    assert!(!text.contains("lin_api_secret"), "{text}");
}
