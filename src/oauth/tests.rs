//! OAuth tests: install redirect, callback checks, code exchange.

#![allow(clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicUsize, Ordering};

use autumn_web::AutumnError;
use serde_json::json;

use super::*;
use crate::plugin::LinearPlugin;
use crate::test_support::{
    CLIENT_SECRET, NOW_MS, OAUTH_TOML, ORG, Stub, StubResponse, app, config, state,
};

fn oauth_state(plugin: LinearPlugin, stub: Option<&Stub>) -> AppState {
    let plugin = match stub {
        Some(stub) => plugin.api_base_url(stub.base_url.clone()),
        None => plugin,
    };
    state(Some(
        plugin
            .build_runtime(config(Some(OAUTH_TOML), &[]))
            .expect("runtime"),
    ))
}

fn query_of(location: &str) -> Vec<(String, String)> {
    let url = url::Url::parse(location).expect("absolute url");
    url.query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn get<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Start an install and return the nonce from the cookie.
async fn start_install(state: &AppState) -> (String, String) {
    let response = app(state).get("/linear/auth").send().await;
    response.assert_status(302);
    let location = response.header("location").expect("location").to_owned();
    let cookie = response.header("set-cookie").expect("cookie").to_owned();
    let nonce = cookie
        .strip_prefix(&format!("{STATE_COOKIE}="))
        .and_then(|rest| rest.split(';').next())
        .expect("nonce")
        .to_owned();
    (location, nonce)
}

#[tokio::test]
async fn install_redirects_to_linear_with_every_parameter_and_a_cookie() {
    let state = oauth_state(LinearPlugin::new(), None);
    let (location, nonce) = start_install(&state).await;
    assert!(
        location.starts_with("https://linear.app/oauth/authorize?"),
        "{location}"
    );
    let pairs = query_of(&location);
    assert_eq!(get(&pairs, "client_id"), Some("client-id"));
    assert_eq!(
        get(&pairs, "redirect_uri"),
        Some("https://app.example.com/linear/auth/callback")
    );
    assert_eq!(get(&pairs, "response_type"), Some("code"));
    assert_eq!(get(&pairs, "scope"), Some("read,write"));
    assert_eq!(get(&pairs, "actor"), Some("app"));
    assert_eq!(get(&pairs, "prompt"), Some("consent"));
    assert_eq!(get(&pairs, "state"), Some(nonce.as_str()));
    assert_eq!(nonce.len(), 64);
    assert!(nonce.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(get(&pairs, "code_challenge_method"), Some("S256"));
    let verifier = pkce_verifier(CLIENT_SECRET, &nonce);
    assert_eq!(
        get(&pairs, "code_challenge"),
        Some(pkce_challenge(&verifier).as_str())
    );
    assert!(!location.contains(CLIENT_SECRET) && !location.contains(&verifier));
    let response = app(&state).get("/linear/auth").send().await;
    let cookie = response.header("set-cookie").expect("cookie");
    for attribute in [
        "Path=/",
        "HttpOnly",
        "Secure",
        "SameSite=Lax",
        "Max-Age=600",
    ] {
        assert!(cookie.contains(attribute), "{cookie}");
    }
    assert_eq!(response.header("cache-control"), Some("no-store"));
}

#[tokio::test]
async fn the_nonce_comes_from_the_app_entropy() {
    let (_, first) = start_install(&oauth_state(LinearPlugin::new(), None)).await;
    let (_, again) = start_install(&oauth_state(LinearPlugin::new(), None)).await;
    assert_eq!(first, again, "a seeded entropy gives the same nonce");
}

#[test]
fn pkce_values_follow_rfc_7636() {
    let verifier = pkce_verifier("s", "n");
    assert_eq!(verifier.len(), 64);
    assert_ne!(verifier, pkce_verifier("s", "m"));
    assert_ne!(verifier, pkce_verifier("t", "n"));
    // RFC 7636 appendix B.
    assert_eq!(
        pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[tokio::test]
async fn oauth_routes_are_404_when_oauth_is_off() {
    let state = state(Some(
        LinearPlugin::new()
            .build_runtime(config(None, &[]))
            .expect("runtime"),
    ));
    app(&state)
        .get("/linear/auth")
        .send()
        .await
        .assert_status(404);
    app(&state)
        .get("/linear/auth/callback?code=c&state=s")
        .send()
        .await
        .assert_status(404);
}

fn token_answer() -> StubResponse {
    StubResponse::json(
        200,
        &json!({
            "access_token": "at-1", "token_type": "Bearer", "expires_in": 86_399,
            "scope": "read write", "refresh_token": "rt-1",
        }),
    )
}

fn org_answer(id: &str) -> StubResponse {
    StubResponse::json(
        200,
        &json!({"data": {"organization": {"id": id, "name": "Acme", "urlKey": "acme"}}}),
    )
}

async fn callback_with(
    state: &AppState,
    nonce: &str,
    query: &str,
) -> autumn_web::test::TestResponse {
    app(state)
        .get(&format!("/linear/auth/callback?{query}"))
        .header("cookie", &format!("other=1; {STATE_COOKIE}={nonce}"))
        .send()
        .await
}

#[tokio::test]
async fn the_callback_exchanges_the_code_stores_the_token_and_runs_the_handler() {
    let stub = Stub::start(vec![token_answer(), org_answer(ORG)]).await;
    let installs = std::sync::Arc::new(AtomicUsize::new(0));
    let plugin = LinearPlugin::new().on_install({
        let installs = std::sync::Arc::clone(&installs);
        move |ctx: InstallContext| {
            let installs = std::sync::Arc::clone(&installs);
            async move {
                assert_eq!(ctx.organization_id().expect("org").as_str(), ORG);
                assert_eq!(ctx.scope, "read write");
                assert!(format!("{ctx:?}").contains("Acme"));
                ctx.linear
                    .client_for(&ctx.organization_id().expect("org"))
                    .await
                    .expect("client at once");
                installs.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
    });
    let state = oauth_state(plugin, Some(&stub));
    let (_, nonce) = start_install(&state).await;
    let response = callback_with(&state, &nonce, &format!("code=the-code&state={nonce}")).await;
    response.assert_status(302);
    assert_eq!(
        response.header("location"),
        Some(format!("/welcome?organization={ORG}").as_str())
    );
    assert!(
        response
            .header("set-cookie")
            .is_some_and(|c| c.contains("Max-Age=0"))
    );
    assert_eq!(installs.load(Ordering::SeqCst), 1);

    let seen = stub.requests();
    assert_eq!(seen[0].path, "/oauth/token");
    let form = seen[0].form();
    let field = |key: &str| form.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    assert_eq!(field("grant_type").as_deref(), Some("authorization_code"));
    assert_eq!(field("code").as_deref(), Some("the-code"));
    assert_eq!(
        field("redirect_uri").as_deref(),
        Some("https://app.example.com/linear/auth/callback")
    );
    assert_eq!(field("client_id").as_deref(), Some("client-id"));
    assert_eq!(field("client_secret").as_deref(), Some(CLIENT_SECRET));
    assert_eq!(
        field("code_verifier"),
        Some(pkce_verifier(CLIENT_SECRET, &nonce))
    );
    assert_eq!(seen[1].path, "/graphql");
    assert_eq!(seen[1].header("authorization"), Some("Bearer at-1"));

    let runtime = state.extension::<LinearRuntime>().expect("runtime");
    let token = runtime
        .token_store()
        .get(&OrganizationId::parse(ORG).expect("org"))
        .await
        .expect("get")
        .expect("stored");
    assert_eq!(token.access_token, "at-1");
    assert_eq!(token.refresh_token.as_deref(), Some("rt-1"));
    assert_eq!(token.expires_at_ms, Some(NOW_MS + 86_399_000));
    assert_eq!(token.installed_at_ms, Some(NOW_MS));
}

#[tokio::test]
async fn an_array_scope_from_an_old_app_is_accepted() {
    let answer = StubResponse::json(
        200,
        &json!({"access_token": "at", "scope": ["read", "write"]}),
    );
    let stub = Stub::start(vec![answer, org_answer(ORG)]).await;
    let state = oauth_state(LinearPlugin::new(), Some(&stub));
    let (_, nonce) = start_install(&state).await;
    callback_with(&state, &nonce, &format!("code=c&state={nonce}"))
        .await
        .assert_status(302);
    let runtime = state.extension::<LinearRuntime>().expect("runtime");
    let token = runtime
        .token_store()
        .get(&OrganizationId::parse(ORG).expect("org"))
        .await
        .expect("get")
        .expect("stored");
    assert_eq!(token.scope, "read,write");
    // Linear documents a 24 hour token. A missing `expires_in` uses it.
    assert_eq!(
        token.expires_at_ms,
        Some(NOW_MS + DEFAULT_TOKEN_LIFETIME_MS)
    );
}

#[tokio::test]
async fn the_callback_checks_run_before_any_token_call() {
    let stub = Stub::start(Vec::new()).await;
    let state = oauth_state(LinearPlugin::new(), Some(&stub));
    let (_, nonce) = start_install(&state).await;
    // A wrong state, and a missing cookie.
    callback_with(&state, &nonce, "code=c&state=forged")
        .await
        .assert_status(403);
    app(&state)
        .get(&format!("/linear/auth/callback?code=c&state={nonce}"))
        .send()
        .await
        .assert_status(403);
    // An empty state against an empty cookie.
    callback_with(&state, "", "code=c&state=")
        .await
        .assert_status(403);
    // Linear sends `error` when the user refuses.
    let refused = callback_with(
        &state,
        &nonce,
        &format!("error=access_denied&state={nonce}"),
    )
    .await;
    refused.assert_status(403);
    assert!(refused.text().contains("access_denied"));
    // No code, and a repeated parameter.
    callback_with(&state, &nonce, &format!("state={nonce}"))
        .await
        .assert_status(400);
    callback_with(&state, &nonce, &format!("code=a&code=b&state={nonce}"))
        .await
        .assert_status(400);
    assert!(stub.requests().is_empty(), "no token call ran");
}

#[tokio::test]
async fn an_exchange_failure_is_502_without_secrets() {
    let refused = StubResponse::json(
        400,
        &json!({"error": "invalid_grant", "error_description": format!("bad client_secret {CLIENT_SECRET}")}),
    );
    let stub = Stub::start(vec![refused]).await;
    let state = oauth_state(LinearPlugin::new(), Some(&stub));
    let (_, nonce) = start_install(&state).await;
    let response = callback_with(&state, &nonce, &format!("code=c&state={nonce}")).await;
    response.assert_status(502);
    let text = response.text();
    assert!(!text.contains(CLIENT_SECRET), "{text}");
    assert!(text.contains("invalid_grant"), "{text}");
}

#[tokio::test]
async fn a_bad_organization_or_token_answer_is_502() {
    for script in [
        vec![StubResponse::text(200, "not json")],
        vec![StubResponse::json(200, &json!({"access_token": " "}))],
        vec![token_answer(), org_answer("bad id")],
        vec![
            token_answer(),
            StubResponse::json(200, &json!({"errors": [{"message": "no"}]})),
        ],
    ] {
        let stub = Stub::start(script).await;
        let state = oauth_state(LinearPlugin::new(), Some(&stub));
        let (_, nonce) = start_install(&state).await;
        callback_with(&state, &nonce, &format!("code=c&state={nonce}"))
            .await
            .assert_status(502);
    }
}

#[tokio::test]
async fn a_failed_install_handler_is_500_and_the_token_stays() {
    let stub = Stub::start(vec![token_answer(), org_answer(ORG)]).await;
    let plugin = LinearPlugin::new().on_install(|_ctx: InstallContext| async {
        Err(AutumnError::internal_server_error_msg("db down"))
    });
    let state = oauth_state(plugin, Some(&stub));
    let (_, nonce) = start_install(&state).await;
    callback_with(&state, &nonce, &format!("code=c&state={nonce}"))
        .await
        .assert_status(500);
    let runtime = state.extension::<LinearRuntime>().expect("runtime");
    assert!(
        runtime
            .token_store()
            .get(&OrganizationId::parse(ORG).expect("org"))
            .await
            .expect("get")
            .is_some()
    );
}

#[test]
fn cookie_value_refuses_a_repeated_cookie() {
    let mut headers = HeaderMap::new();
    headers.append(header::COOKIE, "a=1; b=2".parse().expect("value"));
    assert_eq!(cookie_value(&headers, "b"), Some("2"));
    headers.append(header::COOKIE, "b=3".parse().expect("value"));
    assert_eq!(cookie_value(&headers, "b"), None);
    assert_eq!(cookie_value(&headers, "c"), None);
}

#[test]
fn authorize_url_leaves_out_prompt_when_consent_is_off() {
    let mut cfg = config(Some(OAUTH_TOML), &[]);
    cfg.prompt_consent = false;
    let url = authorize_url(&cfg, "n", "c").expect("url");
    assert!(!url.contains("prompt="), "{url}");
    cfg.client_id = None;
    assert!(authorize_url(&cfg, "n", "c").is_err());
}

#[test]
fn dot_segments_cannot_make_an_open_redirect() {
    for path in [
        "/.//evil.example.com",
        "/..//evil.example.com",
        "/a/..//evil.example.com",
        "/./x",
    ] {
        let toml = format!("{OAUTH_TOML}\n").replace(
            "after_install_path = \"/welcome\"",
            &format!("after_install_path = \"{path}\""),
        );
        let cfg = config(Some(&toml), &[]);
        let err = cfg.validate().expect_err("refused");
        assert!(
            err.to_string().contains("after_install_path"),
            "{path}: {err}"
        );
    }
}

#[tokio::test]
async fn the_install_handler_gets_the_request_headers() {
    let stub = Stub::start(vec![token_answer(), org_answer(ORG)]).await;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let plugin = LinearPlugin::new().on_install({
        let seen = std::sync::Arc::clone(&seen);
        move |ctx: InstallContext| {
            let seen = std::sync::Arc::clone(&seen);
            async move {
                let session = cookie_value(&ctx.headers, "app_session").map(ToOwned::to_owned);
                *seen.lock().expect("lock") = session;
                Ok(())
            }
        }
    });
    let state = oauth_state(plugin, Some(&stub));
    let (_, nonce) = start_install(&state).await;
    app(&state)
        .get(&format!("/linear/auth/callback?code=c&state={nonce}"))
        .header(
            "cookie",
            &format!("app_session=s-1; {STATE_COOKIE}={nonce}"),
        )
        .send()
        .await
        .assert_status(302);
    assert_eq!(seen.lock().expect("lock").as_deref(), Some("s-1"));
}
