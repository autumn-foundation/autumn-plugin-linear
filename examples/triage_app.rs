//! A triage bot on Autumn: new Linear issues get a priority and a comment.
//!
//! ## Setup
//!
//! 1. In Linear, open Settings → API. Make a personal API key and a
//!    webhook to `https://<your-host>/linear/webhooks` for `Issue` events.
//!    Copy the webhook signing secret.
//! 2. Export the secrets:
//!
//!    ```sh
//!    export LINEAR_API_KEY="lin_api_..."
//!    export LINEAR_WEBHOOK_SECRET="lin_wh_..."
//!    ```
//!
//! 3. Run `cargo run --example triage_app`.
//!
//! For many workspaces, set `[linear] app_url`, `client_id`, and
//! `LINEAR_CLIENT_SECRET`, then open `https://<your-host>/linear/auth`.
//! `ctx.client()` then uses the token of the workspace that sent the event.
//!
//! The handler calls Linear before it returns. That is fine for two short
//! calls. Linear waits 5 seconds: hand longer work to a job queue or a
//! workflow engine (see `examples/intake-desk`).

use autumn_plugin_linear::graphql::{CommentCreate, IssueUpdate};
use autumn_plugin_linear::{Linear, LinearPlugin, WebhookContext};
use autumn_web::prelude::*;

/// A priority from the words in a title: 1 urgent, 2 high, 3 medium.
fn suggest_priority(title: &str) -> u8 {
    let title = title.to_lowercase();
    if ["outage", "down", "crash"]
        .iter()
        .any(|w| title.contains(w))
    {
        1
    } else if ["bug", "error", "fails"].iter().any(|w| title.contains(w)) {
        2
    } else {
        3
    }
}

/// Triage a new issue that has no priority yet.
async fn issue_created(ctx: WebhookContext) -> AutumnResult<()> {
    let issue = ctx.event.issue().map_err(|e| e.to_autumn_error())?;
    let Some(issue) = issue else { return Ok(()) };
    if issue.priority.is_some_and(|p| p > 0.0) {
        return Ok(());
    }
    let priority = suggest_priority(issue.title.as_deref().unwrap_or_default());
    let client = ctx.client().await.map_err(|e| e.to_autumn_error())?;
    let update = IssueUpdate {
        priority: Some(priority),
        ..IssueUpdate::default()
    };
    client
        .update_issue(&issue.id, &update)
        .await
        .map_err(|e| e.to_autumn_error())?;
    // A retry of this webhook derives the same comment id, finds the
    // comment, and does not post it again.
    let comment = CommentCreate::new(&issue.id, format!("Triage bot: priority {priority}."))
        .with_stable_id(&format!("triage:{}", issue.id));
    let id = comment.id.clone().unwrap_or_default();
    if client
        .find_comment(&id)
        .await
        .map_err(|e| e.to_autumn_error())?
        .is_none()
    {
        client
            .create_comment(&comment)
            .await
            .map_err(|e| e.to_autumn_error())?;
    }
    tracing::info!(org = %ctx.event.organization_id, issue = %issue.id, priority, "issue triaged");
    Ok(())
}

/// Show an issue. The route uses the API-key client.
#[get("/issues/{id}")]
#[secured]
async fn show_issue(linear: Linear, Path(id): Path<String>) -> AutumnResult<String> {
    let client = linear.client().map_err(|e| e.to_autumn_error())?;
    let issue = client.issue(&id).await.map_err(|e| e.to_autumn_error())?;
    Ok(format!(
        "{} {}",
        issue.identifier.unwrap_or_default(),
        issue.title.unwrap_or_default()
    ))
}

/// The plugin with the handler.
fn plugin() -> LinearPlugin {
    LinearPlugin::new().on_action("Issue", "create", issue_created)
}

#[autumn_web::main]
#[allow(clippy::large_futures, reason = "the app future runs once, in main")]
async fn main() {
    autumn_web::app()
        .plugin(plugin())
        .routes(routes![show_issue])
        .run()
        .await;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::sync::{Arc, Mutex};

    use autumn_plugin_linear::testing::EventFixture;
    use serde_json::{Value, json};

    use super::*;

    const ORG: &str = "org-1";
    const SECRET: &str = "wh-secret";

    /// A local Linear stub: it answers each GraphQL call by its operation
    /// name and records the calls.
    async fn stub() -> (String, Arc<Mutex<Vec<Value>>>) {
        let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
        let app = axum::Router::new().route(
            "/graphql",
            axum::routing::post({
                let seen = Arc::clone(&seen);
                move |axum::Json(body): axum::Json<Value>| {
                    let seen = Arc::clone(&seen);
                    async move {
                        let query = body["query"].as_str().unwrap_or_default().to_owned();
                        seen.lock().expect("lock").push(body);
                        let issue = json!({"id": "i-1", "identifier": "ENG-1"});
                        let data = if query.contains("issueUpdate") {
                            json!({"issueUpdate": {"success": true, "issue": issue}})
                        } else if query.contains("comments(") {
                            json!({"comments": {"nodes": []}})
                        } else {
                            json!({"commentCreate": {"success": true, "comment": {"id": "c-1"}}})
                        };
                        axum::Json(json!({ "data": data }))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (base, seen)
    }

    #[test]
    fn priorities_follow_the_title() {
        assert_eq!(suggest_priority("Checkout CRASH"), 1);
        assert_eq!(suggest_priority("Export fails"), 2);
        assert_eq!(suggest_priority("Dark mode"), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_issue_gets_a_priority_and_one_comment() {
        let (base, seen) = stub().await;
        let plugin = plugin().api_base_url(base).env_lookup(|name| match name {
            "LINEAR_WEBHOOK_SECRET" => Some(SECRET.to_owned()),
            "LINEAR_API_KEY" => Some("lin_api_test".to_owned()),
            _ => None,
        });
        let app = autumn_web::test::TestApp::new().plugin(plugin).build();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_millis();
        let signed = EventFixture::new("Issue", "create", ORG)
            .data(json!({"id": "i-1", "title": "Export fails", "priority": 0}))
            .at_ms(u64::try_from(now).expect("fits"))
            .sign(SECRET, "d-1");
        let mut request = app.post("/linear/webhooks");
        for (name, value) in &signed.headers {
            request = request.header(name, value);
        }
        request
            .body(signed.body.clone())
            .send()
            .await
            .assert_status(200);

        let seen = seen.lock().expect("lock").clone();
        assert_eq!(seen.len(), 3, "{seen:?}");
        assert_eq!(seen[0]["variables"]["input"]["priority"], 2);
        assert!(
            seen[2]["variables"]["input"]["body"]
                .as_str()
                .is_some_and(|b| b.contains("priority 2"))
        );
    }
}
