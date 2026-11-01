//! Webhook route tests: HMAC, replay window, headers, duplicates,
//! dispatch, revoke cleanup.

#![allow(clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use autumn_web::AutumnError;
use serde_json::{Value, json};

use super::*;
use crate::delivery::DeliveryStore;
use crate::error::ErrorKind;
use crate::plugin::LinearPlugin;
use crate::test_support::{NOW_MS, OAUTH_TOML, ORG, SECRET, app, config, env, state};
use crate::testing::{EventFixture, SignedEvent};
use crate::tokens::{StoreFuture, WorkspaceToken};
use crate::types::OrganizationId;

type Seen = Arc<Mutex<Vec<WebhookContext>>>;

fn recorder(
    seen: &Seen,
    tag: &'static str,
) -> impl Fn(WebhookContext) -> WebhookFuture + Send + Sync + 'static {
    let seen = Arc::clone(seen);
    move |ctx| {
        let seen = Arc::clone(&seen);
        Box::pin(async move {
            let mut ctx = ctx;
            ctx.event.raw["handled_by"] = json!(tag);
            seen.lock().expect("lock").push(ctx);
            Ok(())
        })
    }
}

fn plugin_with(seen: &Seen) -> LinearPlugin {
    LinearPlugin::new()
        .on_action("Issue", "create", recorder(seen, "issue-create"))
        .on("Issue", recorder(seen, "issue-any"))
        .on("OAuthApp", recorder(seen, "oauth-app"))
}

fn setup(plugin: &LinearPlugin) -> AppState {
    state(Some(
        plugin.build_runtime(config(None, &[])).expect("runtime"),
    ))
}

fn issue(action: &str) -> EventFixture {
    EventFixture::new("Issue", action, ORG)
        .data(json!({"id": "i-1", "identifier": "ENG-1", "title": "Crash", "labelIds": []}))
        .at_ms(NOW_MS)
}

async fn send(state: &AppState, signed: &SignedEvent) -> autumn_web::test::TestResponse {
    send_without(state, signed, "").await
}

/// Send `signed` without header `skip`.
async fn send_without(
    state: &AppState,
    signed: &SignedEvent,
    skip: &str,
) -> autumn_web::test::TestResponse {
    let client = app(state);
    let mut request = client.post("/linear/webhooks");
    for (name, value) in &signed.headers {
        if name != skip {
            request = request.header(name, value);
        }
    }
    request.body(signed.body.clone()).send().await
}

fn handled(seen: &Seen) -> Vec<String> {
    seen.lock()
        .expect("lock")
        .iter()
        .map(|ctx| {
            ctx.event.raw["handled_by"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

#[tokio::test]
async fn a_valid_delivery_runs_the_exact_handler_with_full_context() {
    let seen = Seen::default();
    let state = setup(&plugin_with(&seen));
    send(&state, &issue("create").sign(SECRET, "d-1"))
        .await
        .assert_status(200);
    assert_eq!(handled(&seen), vec!["issue-create"]);
    let ctx = seen.lock().expect("lock").remove(0);
    assert_eq!(ctx.delivery_id.as_str(), "d-1");
    assert_eq!(ctx.event.organization_id.as_str(), ORG);
    assert_eq!(ctx.event.entity_id(), Some("i-1"));
    assert!(!ctx.body.is_empty());
    assert_eq!(ctx.json::<Value>().expect("json")["type"], "Issue");
    assert!(format!("{ctx:?}").contains("d-1"));
    assert!(
        !format!("{ctx:?}").contains("Crash"),
        "Debug leaves out the body"
    );
}

#[tokio::test]
async fn the_type_handler_takes_other_actions() {
    let seen = Seen::default();
    let state = setup(&plugin_with(&seen));
    send(&state, &issue("update").sign(SECRET, "d-2"))
        .await
        .assert_status(200);
    send(&state, &issue("remove").sign(SECRET, "d-3"))
        .await
        .assert_status(200);
    assert_eq!(handled(&seen), vec!["issue-any", "issue-any"]);
}

#[tokio::test]
async fn an_event_with_no_handler_is_200_and_runs_nothing() {
    let seen = Seen::default();
    let state = setup(&plugin_with(&seen));
    let comment = EventFixture::new("Comment", "create", ORG)
        .data(json!({"id": "c-1"}))
        .at_ms(NOW_MS);
    send(&state, &comment.sign(SECRET, "d-4"))
        .await
        .assert_status(200);
    assert_eq!(handled(&seen), Vec::<String>::new());
}

#[tokio::test]
async fn signature_failures_are_401_before_any_parsing() {
    let seen = Seen::default();
    let state = setup(&plugin_with(&seen));
    let good = issue("create").sign(SECRET, "d-5");
    // Missing header.
    send_without(&state, &good, "linear-signature")
        .await
        .assert_status(401);
    // Wrong secret.
    send(&state, &issue("create").sign("wrong", "d-5"))
        .await
        .assert_status(401);
    // A changed body byte.
    let mut tampered = good.clone();
    tampered.body = tampered.body_text().replace("Crash", "Crush").into_bytes();
    send(&state, &tampered).await.assert_status(401);
    // A body that is not JSON and no valid signature: 401, not 400.
    let garbage = SignedEvent::new("wrong", b"not json".to_vec(), "d-5", "Issue", NOW_MS);
    send(&state, &garbage).await.assert_status(401);
    assert_eq!(handled(&seen), Vec::<String>::new());
}

#[tokio::test]
async fn the_previous_secret_verifies_during_rotation() {
    let seen = Seen::default();
    let runtime = plugin_with(&seen)
        .build_runtime(config(
            None,
            &[
                ("LINEAR_WEBHOOK_SECRET", "new"),
                ("LINEAR_WEBHOOK_SECRET_PREVIOUS", SECRET),
            ],
        ))
        .expect("runtime");
    let state = state(Some(runtime));
    send(&state, &issue("create").sign(SECRET, "d-6"))
        .await
        .assert_status(200);
    send(&state, &issue("create").sign("new", "d-7"))
        .await
        .assert_status(200);
    assert_eq!(handled(&seen).len(), 2);
}

#[tokio::test]
async fn stale_future_and_missing_times_are_401() {
    let seen = Seen::default();
    let state = setup(&plugin_with(&seen));
    for (at, id) in [(NOW_MS - 60_001, "d-8"), (NOW_MS + 60_001, "d-9")] {
        send(&state, &issue("create").at_ms(at).sign(SECRET, id))
            .await
            .assert_status(401);
    }
    // The edges of the window pass.
    send(
        &state,
        &issue("create").at_ms(NOW_MS - 60_000).sign(SECRET, "d-10"),
    )
    .await
    .assert_status(200);
    send(
        &state,
        &issue("create").at_ms(NOW_MS + 60_000).sign(SECRET, "d-11"),
    )
    .await
    .assert_status(200);
    // A signed body with no time.
    let mut body = issue("create").to_json();
    body.as_object_mut()
        .expect("object")
        .remove("webhookTimestamp");
    let signed = SignedEvent::new(
        SECRET,
        body.to_string().into_bytes(),
        "d-12",
        "Issue",
        NOW_MS,
    );
    send(&state, &signed).await.assert_status(401);
    assert_eq!(handled(&seen).len(), 2);
}

#[tokio::test]
async fn the_window_follows_the_config() {
    let seen = Seen::default();
    let runtime = plugin_with(&seen)
        .build_runtime(config(None, &[("LINEAR_WEBHOOK_TOLERANCE_SECS", "300")]))
        .expect("runtime");
    let state = state(Some(runtime));
    send(
        &state,
        &issue("create").at_ms(NOW_MS - 299_000).sign(SECRET, "d-13"),
    )
    .await
    .assert_status(200);
}

#[tokio::test]
async fn an_oversized_body_is_413() {
    let seen = Seen::default();
    let state = setup(&plugin_with(&seen));
    let big = issue("create")
        .field("padding", json!("x".repeat(MAX_BODY_BYTES)))
        .sign(SECRET, "d-14");
    send(&state, &big).await.assert_status(413);
}

#[tokio::test]
async fn a_missing_delivery_id_or_a_bad_envelope_is_400() {
    let seen = Seen::default();
    let state = setup(&plugin_with(&seen));
    send_without(
        &state,
        &issue("create").sign(SECRET, "d-15"),
        "linear-delivery",
    )
    .await
    .assert_status(400);
    send(&state, &issue("create").sign(SECRET, "bad id with spaces"))
        .await
        .assert_status(400);
    let no_org = SignedEvent::new(
        SECRET,
        json!({"type": "Issue", "action": "create", "webhookTimestamp": NOW_MS})
            .to_string()
            .into_bytes(),
        "d-16",
        "Issue",
        NOW_MS,
    );
    send(&state, &no_org).await.assert_status(400);
    assert_eq!(handled(&seen), Vec::<String>::new());
}

#[tokio::test]
async fn routing_uses_the_signed_type_not_the_header() {
    let seen = Seen::default();
    let state = setup(&plugin_with(&seen));
    let mut signed = issue("create").sign(SECRET, "d-17");
    for (name, value) in &mut signed.headers {
        if name == "linear-event" {
            *value = "Comment".to_owned();
        }
    }
    send(&state, &signed).await.assert_status(200);
    assert_eq!(handled(&seen), vec!["issue-create"]);
}

#[tokio::test]
async fn a_done_duplicate_is_200_and_runs_nothing() {
    let seen = Seen::default();
    let state = setup(&plugin_with(&seen));
    let signed = issue("create").sign(SECRET, "d-18");
    send(&state, &signed).await.assert_status(200);
    send(&state, &signed).await.assert_status(200);
    assert_eq!(handled(&seen).len(), 1);
}

#[tokio::test]
async fn an_in_flight_duplicate_is_409() {
    let release = Arc::new(tokio::sync::Notify::new());
    let started = Arc::new(tokio::sync::Notify::new());
    let plugin = LinearPlugin::new().on("Issue", {
        let (release, started) = (Arc::clone(&release), Arc::clone(&started));
        move |_ctx: WebhookContext| {
            let (release, started) = (Arc::clone(&release), Arc::clone(&started));
            async move {
                started.notify_one();
                release.notified().await;
                Ok(())
            }
        }
    });
    let state = setup(&plugin);
    let signed = issue("create").sign(SECRET, "d-19");
    let first = tokio::spawn({
        let (state, signed) = (state.clone(), signed.clone());
        async move { send(&state, &signed).await.status.as_u16() }
    });
    started.notified().await;
    send(&state, &signed).await.assert_status(409);
    release.notify_one();
    assert_eq!(first.await.expect("joins"), 200);
}

#[tokio::test]
async fn a_failed_handler_is_500_and_its_retry_runs_again() {
    let calls = Arc::new(AtomicUsize::new(0));
    let plugin = LinearPlugin::new().on("Issue", {
        let calls = Arc::clone(&calls);
        move |_ctx: WebhookContext| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    Err(AutumnError::internal_server_error_msg("boom"))
                } else {
                    Ok(())
                }
            }
        }
    });
    let state = setup(&plugin);
    let signed = issue("create").sign(SECRET, "d-20");
    send(&state, &signed).await.assert_status(500);
    send(&state, &signed).await.assert_status(200);
    send(&state, &signed).await.assert_status(200);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_panic_is_500_and_releases_the_claim() {
    let calls = Arc::new(AtomicUsize::new(0));
    let plugin = LinearPlugin::new().on("Issue", {
        let calls = Arc::clone(&calls);
        move |_ctx: WebhookContext| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                assert!(n > 0, "first call panics");
                Ok(())
            }
        }
    });
    let state = setup(&plugin);
    let signed = issue("create").sign(SECRET, "d-21");
    send(&state, &signed).await.assert_status(500);
    send(&state, &signed).await.assert_status(200);
}

#[tokio::test]
async fn a_dropped_request_does_not_stop_the_handler() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let finished = Arc::new(tokio::sync::Notify::new());
    let plugin = LinearPlugin::new().on("Issue", {
        let (started, release, finished) = (
            Arc::clone(&started),
            Arc::clone(&release),
            Arc::clone(&finished),
        );
        move |_ctx: WebhookContext| {
            let (started, release, finished) = (
                Arc::clone(&started),
                Arc::clone(&release),
                Arc::clone(&finished),
            );
            async move {
                started.notify_one();
                release.notified().await;
                finished.notify_one();
                Ok(())
            }
        }
    });
    let state = setup(&plugin);
    let signed = issue("create").sign(SECRET, "d-22");
    let request = tokio::spawn({
        let (state, signed) = (state.clone(), signed.clone());
        async move { send(&state, &signed).await }
    });
    started.notified().await;
    // The caller goes away while the handler runs.
    request.abort();
    let _ = request.await;
    release.notify_one();
    finished.notified().await;
    // The claim settles after the handler: a retry is then a duplicate.
    let mut status = 0;
    for _ in 0..100 {
        status = send(&state, &signed).await.status.as_u16();
        if status == 200 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(status, 200);
}

struct FailingStore;

impl DeliveryStore for FailingStore {
    fn claim<'a>(&'a self, _: &'a str, _: u64) -> StoreFuture<'a, Claim> {
        Box::pin(async { Err(Error::Store("down".into())) })
    }
    fn complete<'a>(&'a self, _: &'a str, _: u64) -> StoreFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn release<'a>(&'a self, _: &'a str, _: u64) -> StoreFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn a_store_failure_is_500_and_runs_nothing() {
    let seen = Seen::default();
    let state = setup(&plugin_with(&seen).delivery_store(Arc::new(FailingStore)));
    send(&state, &issue("create").sign(SECRET, "d-23"))
        .await
        .assert_status(500);
    assert_eq!(handled(&seen), Vec::<String>::new());
}

#[tokio::test]
async fn webhooks_are_404_with_no_secret() {
    let runtime = LinearPlugin::new()
        .build_runtime(
            crate::config::LinearConfig::from_sources(None, env(&[("LINEAR_API_KEY", "k")]))
                .expect("config"),
        )
        .expect("runtime");
    let state = state(Some(runtime));
    send(&state, &issue("create").sign(SECRET, "d-24"))
        .await
        .assert_status(404);
}

async fn install_token(state: &AppState, installed_at_ms: u64) {
    let runtime = state.extension::<LinearRuntime>().expect("runtime");
    let mut token = WorkspaceToken::new("at", Some("rt".into()), None, "read");
    token.installed_at_ms = Some(installed_at_ms);
    runtime
        .keeper()
        .store_install(&OrganizationId::parse(ORG).expect("org"), token)
        .await
        .expect("stored");
}

async fn stored(state: &AppState) -> Option<WorkspaceToken> {
    let runtime = state.extension::<LinearRuntime>().expect("runtime");
    runtime
        .token_store()
        .get(&OrganizationId::parse(ORG).expect("org"))
        .await
        .expect("get")
}

#[tokio::test]
async fn a_revoke_deletes_the_token_then_runs_the_handler() {
    let seen = Seen::default();
    let runtime = plugin_with(&seen)
        .build_runtime(config(Some(OAUTH_TOML), &[]))
        .expect("runtime");
    let state = state(Some(runtime));
    install_token(&state, NOW_MS - 3_600_000).await;
    let revoke = EventFixture::new("OAuthApp", "revoked", ORG)
        .field("oauthClientId", json!("client-id"))
        .field("createdAt", json!("2026-10-26T07:33:20.000Z"))
        .at_ms(NOW_MS);
    send(&state, &revoke.sign(SECRET, "d-25"))
        .await
        .assert_status(200);
    assert!(stored(&state).await.is_none());
    assert_eq!(handled(&seen), vec!["oauth-app"]);
}

#[tokio::test]
async fn a_revoke_keeps_a_token_from_a_later_install() {
    let seen = Seen::default();
    let state = setup(&LinearPlugin::new());
    install_token(&state, NOW_MS + 1).await;
    let revoke = EventFixture::new("OAuthApp", "revoked", ORG)
        .field("createdAt", json!("2026-10-26T07:33:20.000Z"))
        .at_ms(NOW_MS);
    send(&state, &revoke.sign(SECRET, "d-26"))
        .await
        .assert_status(200);
    assert!(stored(&state).await.is_some(), "the later install stays");
    assert_eq!(handled(&seen), Vec::<String>::new());
}

#[test]
fn the_registry_refuses_bad_and_repeated_keys() {
    let seen = Seen::default();
    let mut registry = WebhookRegistry::new();
    assert!(registry.is_empty());
    registry
        .register("Issue", None, Arc::new(recorder(&seen, "a")))
        .expect("ok");
    registry
        .register("Issue", Some("create"), Arc::new(recorder(&seen, "b")))
        .expect("ok");
    for (event_type, action) in [
        ("Issue", None),
        ("Issue", Some("create")),
        ("issue", None),
        ("Issue", Some("Create")),
    ] {
        let err = registry
            .register(event_type, action, Arc::new(recorder(&seen, "c")))
            .expect_err("refused");
        assert_eq!(err.kind(), ErrorKind::Config);
    }
    assert_eq!(registry.keys(), vec!["Issue", "Issue:create"]);
    assert!(format!("{registry:?}").contains("Issue:create"));
}

#[autumn_web::post("/hooks/linear")]
#[autumn_web::public]
async fn custom_route(
    State(state): State<AppState>,
    webhook: VerifiedWebhook,
) -> AutumnResult<String> {
    assert!(format!("{webhook:?}").contains("Issue"));
    let _: Value = webhook.json().map_err(|e| e.to_autumn_error())?;
    let outcome = process(&state, webhook).await?;
    Ok(format!("{outcome:?}"))
}

#[tokio::test]
async fn the_extractor_and_process_work_on_a_custom_route() {
    let seen = Seen::default();
    let state = setup(&LinearPlugin::new().on_action("Issue", "create", recorder(&seen, "x")));
    let mut router = axum::Router::new();
    for route in autumn_web::routes![custom_route] {
        router = router.route(route.path, route.handler);
    }
    let client = autumn_web::test::TestApp::from_router(router.with_state(state.clone()), state);
    let post = |signed: SignedEvent| {
        let mut request = client.post("/hooks/linear");
        for (name, value) in &signed.headers {
            request = request.header(name, value);
        }
        request.body(signed.body).send()
    };
    post(issue("create").sign("wrong", "c-1"))
        .await
        .assert_status(401);
    let response = post(issue("create").sign(SECRET, "c-2")).await;
    response.assert_status(200);
    assert_eq!(response.text(), "Handled");
    assert_eq!(
        post(issue("create").sign(SECRET, "c-2")).await.text(),
        "Duplicate"
    );
    assert_eq!(
        post(issue("update").sign(SECRET, "c-3")).await.text(),
        "Unhandled"
    );
}

#[tokio::test]
async fn a_context_built_in_a_test_reaches_the_client() {
    let runtime = LinearPlugin::new()
        .build_runtime(config(None, &[("LINEAR_API_KEY", "k")]))
        .expect("runtime");
    let state = state(Some(runtime));
    let linear = crate::plugin::Linear::from_state(&state).expect("handle");
    let body = issue("create").to_json().to_string();
    let ctx =
        WebhookContext::new(DeliveryId::parse("t-1").expect("id"), body, linear).expect("ctx");
    assert_eq!(ctx.event.action.as_str(), "create");
    assert!(ctx.client().await.is_ok(), "API-key mode gives a client");
    let linear = crate::plugin::Linear::from_state(&state).expect("handle");
    assert!(WebhookContext::new(DeliveryId::parse("t-2").expect("id"), "nope", linear).is_err());
}

/// A revoke with no `createdAt` has no stable time: the plugin asks Linear
/// whether the token still works, and keeps a working one.
#[tokio::test]
async fn a_revoke_without_a_time_keeps_a_working_token() {
    let stub = crate::test_support::Stub::data(&[json!({"viewer": {"id": "u1"}})]).await;
    let runtime = LinearPlugin::new()
        .api_base_url(stub.base_url.clone())
        .build_runtime(config(Some(OAUTH_TOML), &[]))
        .expect("runtime");
    let state = state(Some(runtime));
    install_token(&state, NOW_MS - 1_000).await;
    let mut body = EventFixture::new("OAuthApp", "revoked", ORG)
        .at_ms(NOW_MS)
        .to_json();
    body.as_object_mut().expect("object").remove("createdAt");
    let signed = SignedEvent::new(
        SECRET,
        body.to_string().into_bytes(),
        "d-27",
        "OAuthApp",
        NOW_MS,
    );
    send(&state, &signed).await.assert_status(200);
    assert!(stored(&state).await.is_some(), "a working token stays");
    assert_eq!(
        stub.requests()[0].header("authorization"),
        Some("Bearer at")
    );
}

#[tokio::test]
async fn a_revoke_without_a_time_deletes_a_dead_token() {
    let stub = crate::test_support::Stub::start(vec![crate::test_support::StubResponse::json(
        401,
        &json!({"errors": [{"message": "Authentication required"}]}),
    )])
    .await;
    let runtime = LinearPlugin::new()
        .api_base_url(stub.base_url.clone())
        .build_runtime(config(Some(OAUTH_TOML), &[]))
        .expect("runtime");
    let state = state(Some(runtime));
    install_token(&state, NOW_MS - 1_000).await;
    let mut body = EventFixture::new("OAuthApp", "revoked", ORG)
        .at_ms(NOW_MS)
        .to_json();
    body.as_object_mut().expect("object").remove("createdAt");
    let signed = SignedEvent::new(
        SECRET,
        body.to_string().into_bytes(),
        "d-28",
        "OAuthApp",
        NOW_MS,
    );
    send(&state, &signed).await.assert_status(200);
    assert!(stored(&state).await.is_none(), "a dead token goes");
}

/// A revoke for another OAuth app is not ours: the token stays.
#[tokio::test]
async fn a_revoke_for_another_client_keeps_the_token() {
    let runtime = LinearPlugin::new()
        .build_runtime(config(Some(OAUTH_TOML), &[]))
        .expect("runtime");
    let state = state(Some(runtime));
    install_token(&state, NOW_MS - 3_600_000).await;
    let revoke = EventFixture::new("OAuthApp", "revoked", ORG)
        .field("oauthClientId", json!("someone-else"))
        .field("createdAt", json!("2026-10-26T07:33:20.000Z"))
        .at_ms(NOW_MS);
    send(&state, &revoke.sign(SECRET, "d-29"))
        .await
        .assert_status(200);
    assert!(stored(&state).await.is_some());
}

/// The revoke cleanup runs before the handler, so the handler sees no token.
#[tokio::test]
async fn the_revoke_handler_sees_no_token() {
    let saw: Arc<Mutex<Vec<bool>>> = Arc::default();
    let plugin = LinearPlugin::new().on("OAuthApp", {
        let saw = Arc::clone(&saw);
        move |ctx: WebhookContext| {
            let saw = Arc::clone(&saw);
            async move {
                let present = ctx
                    .linear
                    .runtime()
                    .token_store()
                    .get(&ctx.event.organization_id)
                    .await
                    .map_err(|e| e.to_autumn_error())?
                    .is_some();
                saw.lock().expect("lock").push(present);
                Ok(())
            }
        }
    });
    let runtime = plugin
        .build_runtime(config(Some(OAUTH_TOML), &[]))
        .expect("runtime");
    let state = state(Some(runtime));
    install_token(&state, NOW_MS - 3_600_000).await;
    let revoke = EventFixture::new("OAuthApp", "revoked", ORG)
        .field("oauthClientId", json!("client-id"))
        .field("createdAt", json!("2026-10-26T07:33:20.000Z"))
        .at_ms(NOW_MS);
    send(&state, &revoke.sign(SECRET, "d-30"))
        .await
        .assert_status(200);
    assert_eq!(*saw.lock().expect("lock"), vec![false]);
}
