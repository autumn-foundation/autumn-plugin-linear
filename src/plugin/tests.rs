//! Plugin and runtime tests: builder, contract, conformance, clients,
//! token refresh, startup warnings.

#![allow(clippy::expect_used, clippy::panic)]

use std::time::Duration;

use serde_json::json;

use super::*;
use crate::error::ErrorKind;
use crate::test_support::{
    CLIENT_SECRET, NOW_MS, OAUTH_TOML, ORG, SECRET, Stub, StubResponse, config, env, state,
};

fn org() -> OrganizationId {
    OrganizationId::parse(ORG).expect("org")
}

fn noop(_ctx: crate::webhooks::WebhookContext) -> crate::webhooks::WebhookFuture {
    Box::pin(async { Ok(()) })
}

// ── Plugin surface ───────────────────────────────────────────────────

#[test]
fn the_plugin_name_and_contract_are_stable() {
    let plugin = LinearPlugin::new();
    assert_eq!(plugin.name(), "autumn-plugin-linear");
    let contract = plugin.contract().expect("contract");
    let text = format!("{contract:?}");
    assert!(text.contains("0.10"), "{text}");
    assert!(text.contains(env!("CARGO_PKG_VERSION")), "{text}");
}

#[test]
fn supported_autumn_web_matches_the_manifest() {
    let manifest = include_str!("../../Cargo.toml");
    assert!(
        manifest.contains(&format!(
            "autumn-web = {{ version = \"{SUPPORTED_AUTUMN_WEB}\""
        )),
        "keep SUPPORTED_AUTUMN_WEB equal to Cargo.toml"
    );
}

#[test]
fn the_plugin_passes_the_conformance_harness() {
    use autumn_web::plugin_conformance::{ConformanceConfig, run_conformance};
    let plugin = LinearPlugin::new();
    let contract = plugin.contract().expect("contract");
    let app = autumn_web::app().plugin(plugin);
    assert!(app.has_plugin("autumn-plugin-linear"));
    let routes = app.plugin_route_infos().expect("routes build");
    for (method, path) in [
        ("POST", "/linear/webhooks"),
        ("GET", "/linear/auth"),
        ("GET", "/linear/auth/callback"),
    ] {
        let route = routes.iter().find(|r| r.method == method && r.path == path);
        let route = route.unwrap_or_else(|| panic!("{method} {path} is missing: {routes:?}"));
        assert!(
            format!("{route:?}").to_lowercase().contains("public"),
            "{route:?}"
        );
    }
    let config = ConformanceConfig::new("autumn-plugin-linear")
        .prefix("/linear")
        .contract(contract);
    let report = run_conformance(&config, &routes);
    assert!(report.passed(), "{}", report.to_text_report());
}

#[test]
fn debug_shows_keys_but_no_secrets() {
    let plugin = LinearPlugin::new()
        .on("Issue", noop)
        .on_action("Comment", "create", noop);
    assert!(format!("{plugin:?}").contains("Comment:create"));
    let runtime = plugin
        .build_runtime(config(
            Some(OAUTH_TOML),
            &[("LINEAR_API_KEY", "lin_api_secret")],
        ))
        .expect("runtime");
    let text = format!("{runtime:?}");
    for secret in [SECRET, CLIENT_SECRET, "lin_api_secret"] {
        assert!(!text.contains(secret), "{text}");
    }
    assert!(text.contains("Issue"));
}

// ── build_runtime ────────────────────────────────────────────────────

#[test]
fn build_runtime_needs_a_valid_config() {
    let err = LinearPlugin::new()
        .build_runtime(config(None, &[("LINEAR_WEBHOOK_TOLERANCE_SECS", "0")]))
        .expect_err("invalid");
    assert_eq!(err.kind(), ErrorKind::Config);
}

#[test]
fn handlers_need_the_webhook_secret() {
    let no_secret = LinearConfig::from_sources(None, env(&[])).expect("config");
    let err = LinearPlugin::new()
        .on("Issue", noop)
        .build_runtime(no_secret.clone())
        .expect_err("needs secret");
    assert!(err.to_string().contains("LINEAR_WEBHOOK_SECRET"), "{err}");
    LinearPlugin::new()
        .build_runtime(no_secret)
        .expect("no handlers, no secret is fine");
}

#[test]
fn build_runtime_refuses_bad_and_repeated_keys() {
    for plugin in [
        LinearPlugin::new().on("issue", noop),
        LinearPlugin::new().on_action("Issue", "Create", noop),
        LinearPlugin::new().on("Issue", noop).on("Issue", noop),
    ] {
        let err = plugin
            .build_runtime(config(None, &[]))
            .expect_err("refused");
        assert_eq!(err.kind(), ErrorKind::Config);
    }
}

#[test]
fn api_base_url_must_be_loopback() {
    for base in [
        "https://api.linear.app",
        "http://10.0.0.1",
        "not a url",
        "ftp://localhost",
    ] {
        let err = LinearPlugin::new()
            .api_base_url(base)
            .build_runtime(config(None, &[]))
            .expect_err("refused");
        assert_eq!(err.kind(), ErrorKind::Config, "{base}");
    }
    for base in ["http://127.0.0.1:9", "http://localhost:1", "http://[::1]:2"] {
        LinearPlugin::new()
            .api_base_url(base)
            .build_runtime(config(None, &[]))
            .expect(base);
    }
}

#[test]
fn configure_closures_win_over_env() {
    let runtime = LinearPlugin::new()
        .configure(|c| c.webhook_tolerance_secs = 120)
        .configure(|c| c.max_rate_limit_retries = 7)
        .build_runtime(config(None, &[("LINEAR_WEBHOOK_TOLERANCE_SECS", "30")]))
        .expect("runtime");
    assert_eq!(runtime.config().webhook_tolerance_secs, 120);
    assert_eq!(runtime.config().max_rate_limit_retries, 7);
}

#[test]
fn env_lookup_replaces_the_process_environment() {
    let plugin = LinearPlugin::new().env_lookup(env(&[
        ("LINEAR_API_KEY", "from-vault"),
        ("LINEAR_SCOPES", "read,admin"),
    ]));
    let loaded = plugin.load_config().expect("loads");
    assert_eq!(loaded.scopes, vec!["read".to_owned(), "admin".to_owned()]);
    let runtime = plugin.build_runtime(loaded).expect("runtime");
    assert!(runtime.client().is_ok());
}

// ── Startup ──────────────────────────────────────────────────────────

#[test]
fn start_installs_the_runtime_and_the_health_indicator() {
    let state = state(None);
    LinearPlugin::new()
        .start(&state, config(None, &[("LINEAR_API_KEY", "k")]))
        .expect("starts");
    assert!(state.extension::<LinearRuntime>().is_some());
    assert!(state.health_indicator_registry().contains("linear"));
}

#[test]
fn start_skips_health_without_an_api_key() {
    let state = state(None);
    LinearPlugin::new()
        .start(&state, config(None, &[]))
        .expect("starts");
    assert!(!state.health_indicator_registry().contains("linear"));
}

#[test]
fn start_fails_on_a_bad_config() {
    let state = state(None);
    let err = LinearPlugin::new()
        .start(
            &state,
            config(Some("[linear]\napp_url = \"https://x.example.com\"\n"), &[]),
        )
        .expect_err("no client id");
    assert_eq!(err.kind(), ErrorKind::Config);
    assert!(state.extension::<LinearRuntime>().is_none());
}

#[test]
fn warnings_name_the_csrf_exemption_and_the_unused_api_key() {
    let runtime = LinearPlugin::new()
        .build_runtime(config(Some(OAUTH_TOML), &[("LINEAR_API_KEY", "k")]))
        .expect("runtime");
    let mut app_config = autumn_web::config::AutumnConfig::default();
    app_config.security.csrf.enabled = true;
    let warnings = startup_warnings(&runtime, &app_config);
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("security.csrf.exempt_paths"))
    );
    assert!(warnings.iter().any(|w| w.contains("LINEAR_API_KEY")));
    app_config.security.csrf.exempt_paths = vec!["/linear".to_owned()];
    assert_eq!(startup_warnings(&runtime, &app_config).len(), 1);
    app_config.security.csrf.exempt_paths = vec!["/lin".to_owned()];
    assert_eq!(
        startup_warnings(&runtime, &app_config).len(),
        2,
        "/lin does not cover /linear/webhooks"
    );
}

// ── The Linear handle ────────────────────────────────────────────────

#[tokio::test]
async fn the_extractor_is_503_without_the_runtime() {
    let state = state(None);
    let err = Linear::from_state(&state).expect_err("missing");
    assert_eq!(err.status().as_u16(), 503);
    let (mut parts, ()) = autumn_web::reexports::http::Request::new(()).into_parts();
    let rejection = Linear::from_request_parts(&mut parts, &state)
        .await
        .expect_err("rejected");
    assert_eq!(rejection.status().as_u16(), 503);
}

#[tokio::test]
async fn the_api_key_client_serves_every_workspace_when_oauth_is_off() {
    let stub = Stub::data(&[json!({"viewer": {"id": "u1"}})]).await;
    let runtime = LinearPlugin::new()
        .api_base_url(stub.base_url.clone())
        .build_runtime(config(None, &[("LINEAR_API_KEY", "lin_api_k")]))
        .expect("runtime");
    let state = state(Some(runtime));
    let linear = Linear::from_state(&state).expect("handle");
    assert!(format!("{linear:?}").contains("LinearRuntime"));
    assert_eq!(Linear::install_url(), "/linear/auth");
    linear
        .client_for(&org())
        .await
        .expect("client")
        .viewer()
        .await
        .expect("viewer");
    assert_eq!(
        stub.requests()[0].header("authorization"),
        Some("lin_api_k")
    );
    assert!(linear.client().is_ok());
}

#[test]
fn client_needs_an_api_key() {
    let runtime = LinearPlugin::new()
        .build_runtime(config(None, &[]))
        .expect("runtime");
    let err = runtime.client().expect_err("no key");
    assert!(err.to_string().contains("LINEAR_API_KEY"), "{err}");
}

// ── OAuth tokens and refresh ─────────────────────────────────────────

fn oauth_runtime(stub: &Stub) -> LinearRuntime {
    LinearPlugin::new()
        .api_base_url(stub.base_url.clone())
        .build_runtime(config(Some(OAUTH_TOML), &[]))
        .expect("runtime")
}

fn token(access: &str, refresh: Option<&str>, expires_in_ms: Option<i64>) -> WorkspaceToken {
    let mut token = WorkspaceToken::new(
        access,
        refresh.map(ToOwned::to_owned),
        expires_in_ms.map(|ms| NOW_MS.saturating_add_signed(ms)),
        "read",
    );
    token.installed_at_ms = Some(NOW_MS - 1_000);
    token
}

async fn put(runtime: &LinearRuntime, token: WorkspaceToken) {
    runtime
        .keeper()
        .store_install(&org(), token)
        .await
        .expect("stored");
}

async fn current(runtime: &LinearRuntime) -> Option<WorkspaceToken> {
    runtime.token_store().get(&org()).await.expect("get")
}

fn refreshed(access: &str, refresh: &str) -> StubResponse {
    StubResponse::json(
        200,
        &json!({"access_token": access, "refresh_token": refresh, "expires_in": 86_399, "token_type": "Bearer", "scope": "read"}),
    )
}

#[tokio::test]
async fn an_uninstalled_workspace_is_not_installed() {
    let stub = Stub::start(Vec::new()).await;
    let err = oauth_runtime(&stub)
        .client_for(&org(), NOW_MS)
        .await
        .expect_err("missing");
    assert_eq!(err.kind(), ErrorKind::NotInstalled);
    assert_eq!(err.status().as_u16(), 401);
}

#[tokio::test]
async fn a_fresh_token_makes_no_refresh() {
    let stub = Stub::data(&[json!({"viewer": {"id": "u1"}})]).await;
    let runtime = oauth_runtime(&stub);
    put(&runtime, token("at-0", Some("rt-0"), Some(3_600_000))).await;
    runtime
        .client_for(&org(), NOW_MS)
        .await
        .expect("client")
        .viewer()
        .await
        .expect("viewer");
    let seen = stub.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].header("authorization"), Some("Bearer at-0"));
}

#[tokio::test]
async fn a_token_near_expiry_refreshes_and_stores_the_rotated_pair() {
    let stub = Stub::start(vec![
        refreshed("at-1", "rt-1"),
        StubResponse::json(200, &json!({"data": {"viewer": {"id": "u"}}})),
    ])
    .await;
    let runtime = oauth_runtime(&stub);
    put(&runtime, token("at-0", Some("rt-0"), Some(60_000))).await;
    runtime
        .client_for(&org(), NOW_MS)
        .await
        .expect("client")
        .viewer()
        .await
        .expect("viewer");
    let seen = stub.requests();
    let form = seen[0].form();
    assert!(form.contains(&("grant_type".into(), "refresh_token".into())));
    assert!(form.contains(&("refresh_token".into(), "rt-0".into())));
    assert!(form.contains(&("client_secret".into(), CLIENT_SECRET.into())));
    assert_eq!(seen[1].header("authorization"), Some("Bearer at-1"));
    let stored = current(&runtime).await.expect("stored");
    assert_eq!(stored.access_token, "at-1");
    assert_eq!(stored.refresh_token.as_deref(), Some("rt-1"));
    assert_eq!(
        stored.installed_at_ms,
        Some(NOW_MS - 1_000),
        "the install time stays"
    );
}

#[tokio::test]
async fn concurrent_callers_share_one_refresh() {
    let stub = Stub::start(vec![
        refreshed("at-1", "rt-1").delayed(Duration::from_millis(50)),
    ])
    .await;
    let runtime = Arc::new(oauth_runtime(&stub));
    put(&runtime, token("at-0", Some("rt-0"), Some(1_000))).await;
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let runtime = Arc::clone(&runtime);
        tasks.push(tokio::spawn(async move {
            runtime.client_for(&org(), NOW_MS).await.map(|_| ())
        }));
    }
    for task in tasks {
        task.await.expect("joins").expect("client");
    }
    assert_eq!(stub.requests().len(), 1, "one refresh call");
}

#[tokio::test]
async fn a_refused_refresh_deletes_the_pair_and_asks_for_a_reinstall() {
    for refusal in [
        StubResponse::json(400, &json!({"error": "invalid_grant"})),
        StubResponse::json(403, &json!({"error": "invalid_grant"})),
    ] {
        let stub = Stub::start(vec![refusal]).await;
        let runtime = oauth_runtime(&stub);
        put(&runtime, token("at-0", Some("rt-0"), Some(1_000))).await;
        let err = runtime
            .client_for(&org(), NOW_MS)
            .await
            .expect_err("refused");
        assert_eq!(err.kind(), ErrorKind::Reauthorize);
        assert!(current(&runtime).await.is_none());
    }
}

/// A store that swaps in a fresh pair after the first read, as another
/// replica can.
struct RacingStore {
    inner: InMemoryTokenStore,
    reads: std::sync::atomic::AtomicUsize,
}

impl TokenStore for RacingStore {
    fn get<'a>(
        &'a self,
        org: &'a OrganizationId,
    ) -> crate::tokens::StoreFuture<'a, Option<WorkspaceToken>> {
        Box::pin(async move {
            let n = self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 2 {
                self.inner
                    .put(org, token("at-other", Some("rt-other"), Some(86_000_000)))
                    .await?;
            }
            self.inner.get(org).await
        })
    }
    fn put<'a>(
        &'a self,
        org: &'a OrganizationId,
        token: WorkspaceToken,
    ) -> crate::tokens::StoreFuture<'a, ()> {
        self.inner.put(org, token)
    }
    fn delete<'a>(&'a self, org: &'a OrganizationId) -> crate::tokens::StoreFuture<'a, ()> {
        self.inner.delete(org)
    }
}

#[tokio::test]
async fn a_refused_refresh_uses_and_keeps_a_pair_another_replica_stored() {
    let stub = Stub::start(vec![StubResponse::json(
        400,
        &json!({"error": "invalid_grant"}),
    )])
    .await;
    let store = Arc::new(RacingStore {
        inner: InMemoryTokenStore::new(),
        reads: std::sync::atomic::AtomicUsize::new(0),
    });
    let runtime = LinearPlugin::new()
        .api_base_url(stub.base_url.clone())
        .token_store(store.clone())
        .build_runtime(config(Some(OAUTH_TOML), &[]))
        .expect("runtime");
    put(&runtime, token("at-0", Some("rt-0"), Some(1_000))).await;
    runtime
        .client_for(&org(), NOW_MS)
        .await
        .expect("uses the other pair");
    assert_eq!(
        current(&runtime).await.map(|t| t.access_token).as_deref(),
        Some("at-other")
    );
}

#[tokio::test]
async fn a_transient_failure_keeps_a_valid_old_token_and_backs_off() {
    let stub = Stub::start(vec![StubResponse::text(503, "busy")]).await;
    let runtime = oauth_runtime(&stub);
    put(&runtime, token("at-0", Some("rt-0"), Some(60_000))).await;
    runtime.client_for(&org(), NOW_MS).await.expect("old token");
    runtime
        .client_for(&org(), NOW_MS + 5_000)
        .await
        .expect("old token, no call");
    assert_eq!(
        stub.requests().len(),
        1,
        "the backoff skips the second refresh"
    );
    assert_eq!(
        current(&runtime).await.map(|t| t.access_token).as_deref(),
        Some("at-0")
    );
}

#[tokio::test]
async fn an_expired_token_with_a_transient_failure_fails() {
    let stub = Stub::start(vec![StubResponse::text(503, "busy")]).await;
    let runtime = oauth_runtime(&stub);
    put(&runtime, token("at-0", Some("rt-0"), Some(-1))).await;
    let err = runtime
        .client_for(&org(), NOW_MS)
        .await
        .expect_err("expired");
    assert!(err.is_transient(), "{err:?}");
    assert!(
        current(&runtime).await.is_some(),
        "a transient failure keeps the pair"
    );
}

#[tokio::test]
async fn an_expired_token_without_a_refresh_token_needs_a_reinstall() {
    let stub = Stub::start(Vec::new()).await;
    let runtime = oauth_runtime(&stub);
    put(&runtime, token("at-0", None, Some(-1))).await;
    let err = runtime
        .client_for(&org(), NOW_MS)
        .await
        .expect_err("expired");
    assert_eq!(err.kind(), ErrorKind::Reauthorize);
    assert!(current(&runtime).await.is_none());
    assert!(stub.requests().is_empty());
}

#[tokio::test]
async fn a_refresh_without_a_new_refresh_token_keeps_the_old_one() {
    let answer = StubResponse::json(200, &json!({"access_token": "at-1", "expires_in": 86_399}));
    let stub = Stub::start(vec![answer]).await;
    let runtime = oauth_runtime(&stub);
    put(&runtime, token("at-0", Some("rt-0"), Some(1_000))).await;
    runtime.client_for(&org(), NOW_MS).await.expect("client");
    assert_eq!(
        current(&runtime)
            .await
            .and_then(|t| t.refresh_token)
            .as_deref(),
        Some("rt-0")
    );
}

#[tokio::test]
async fn install_context_new_carries_the_handle() {
    let stub = Stub::start(Vec::new()).await;
    let state = state(Some(oauth_runtime(&stub)));
    let linear = Linear::from_state(&state).expect("handle");
    let organization: crate::graphql::Organization =
        serde_json::from_value(json!({"id": ORG, "name": "Acme"})).expect("org");
    let ctx = InstallContext::new(organization, "read", linear);
    assert_eq!(ctx.organization_id().expect("org").as_str(), ORG);
    assert!(ctx.linear.runtime().config().oauth_enabled());
}

#[tokio::test]
async fn an_expired_token_backs_off_after_a_failed_refresh() {
    let stub = Stub::start(vec![StubResponse::text(503, "busy")]).await;
    let runtime = oauth_runtime(&stub);
    put(&runtime, token("at-0", Some("rt-0"), Some(-1))).await;
    assert!(runtime.client_for(&org(), NOW_MS).await.is_err());
    let err = runtime
        .client_for(&org(), NOW_MS + 1_000)
        .await
        .expect_err("backs off");
    assert_eq!(err.kind(), ErrorKind::RefreshBackoff);
    assert!(err.is_transient());
    assert_eq!(err.status().as_u16(), 503);
    assert_eq!(
        stub.requests().len(),
        1,
        "no second refresh call inside the backoff"
    );
}

#[tokio::test]
async fn a_refresh_answer_without_scope_or_expiry_keeps_safe_values() {
    let answer = StubResponse::json(
        200,
        &json!({"access_token": "at-1", "refresh_token": "rt-1"}),
    );
    let stub = Stub::start(vec![answer]).await;
    let runtime = oauth_runtime(&stub);
    put(&runtime, token("at-0", Some("rt-0"), Some(1_000))).await;
    runtime.client_for(&org(), NOW_MS).await.expect("client");
    let stored = current(&runtime).await.expect("stored");
    assert_eq!(stored.scope, "read", "the old scope stays");
    assert_eq!(
        stored.expires_at_ms,
        Some(NOW_MS + crate::oauth::DEFAULT_TOKEN_LIFETIME_MS)
    );
}

#[tokio::test]
async fn a_refused_client_secret_keeps_the_tokens() {
    // RFC 6749: 401 `invalid_client` is the app's own secret, not a dead
    // grant. The workspace token stays.
    let stub = Stub::start(vec![StubResponse::json(
        401,
        &json!({"error": "invalid_client"}),
    )])
    .await;
    let runtime = oauth_runtime(&stub);
    put(&runtime, token("at-0", Some("rt-0"), Some(-1))).await;
    let err = runtime
        .client_for(&org(), NOW_MS)
        .await
        .expect_err("refused");
    assert_eq!(err.kind(), ErrorKind::Config, "{err:?}");
    assert!(current(&runtime).await.is_some());
}

#[tokio::test]
async fn a_cancelled_caller_does_not_lose_the_rotated_pair() {
    let stub = Stub::start(vec![
        refreshed("at-1", "rt-1").delayed(Duration::from_millis(200)),
    ])
    .await;
    let runtime = Arc::new(oauth_runtime(&stub));
    put(&runtime, token("at-0", Some("rt-0"), Some(1_000))).await;
    let caller = tokio::spawn({
        let runtime = Arc::clone(&runtime);
        async move { runtime.client_for(&org(), NOW_MS).await.map(|_| ()) }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    caller.abort();
    let _ = caller.await;
    for _ in 0..100 {
        if current(&runtime).await.map(|t| t.access_token).as_deref() == Some("at-1") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the rotated pair was lost");
}

#[test]
fn the_plugin_declares_the_linear_section_and_public_routes() {
    let app = autumn_web::app().plugin(LinearPlugin::new());
    assert!(app.has_config_section("linear"));
    let (_, infos) = mount();
    assert_eq!(infos.len(), 3);
    for info in &infos {
        assert_eq!(
            info.classification,
            autumn_web::route_listing::RouteClassification::Public,
            "{info:?}"
        );
    }
}

#[test]
fn a_bad_config_fails_the_startup_hook() {
    let plugin = LinearPlugin::new()
        .on("Issue", noop)
        .env_lookup(env(&[("LINEAR_API_KEY", "k")]));
    let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        autumn_web::test::TestApp::new().plugin(plugin).build()
    }));
    assert!(
        built.is_err(),
        "handlers without a webhook secret stop the startup"
    );
}

#[tokio::test]
async fn the_indicator_is_in_health_and_not_in_ready() {
    let stub = Stub::data(&[json!({"viewer": {"id": "u1", "name": "Bot"}})]).await;
    let state = state(None);
    LinearPlugin::new()
        .api_base_url(stub.base_url.clone())
        .start(&state, config(None, &[("LINEAR_API_KEY", "k")]))
        .expect("starts");
    let registry = state.health_indicator_registry();
    let all = registry.run_all().await;
    let linear = all.iter().find(|r| r.name == "linear").expect("in health");
    assert_eq!(linear.group, IndicatorGroup::HealthOnly);
    assert!(
        registry
            .run_readiness()
            .await
            .iter()
            .all(|r| r.name != "linear"),
        "not in ready"
    );
}

#[test]
fn env_lookup_is_the_only_source_of_linear_variables() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let plugin = LinearPlugin::new().env_lookup({
        let calls = Arc::clone(&calls);
        move |_name: &str| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            None
        }
    });
    let loaded = plugin.load_config().expect("loads");
    assert!(
        calls.load(std::sync::atomic::Ordering::SeqCst) >= 13,
        "every variable goes through the lookup"
    );
    assert!(loaded.client_id.is_none());
}
