use autumn_plugin_linear::testing::EventFixture;

use super::*;
use crate::scenario::now_ms;

/// An event without the intake label never reaches camel: the handle here
/// has no routes, so a call would fail with 503.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_without_the_intake_label_are_skipped() {
    let app = app(CamelHandle::new(), "s".to_owned());
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
