//! Test helpers: a scripted local HTTP stub that stands in for Linear.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

/// One scripted answer.
#[derive(Debug, Clone)]
pub struct StubResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub delay: std::time::Duration,
}

impl StubResponse {
    pub fn json(status: u16, body: &serde_json::Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_string(),
            delay: std::time::Duration::ZERO,
        }
    }

    pub fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.to_owned(),
            delay: std::time::Duration::ZERO,
        }
    }

    pub const fn delayed(mut self, delay: std::time::Duration) -> Self {
        self.delay = delay;
        self
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// One recorded request.
#[derive(Debug, Clone)]
pub struct Recorded {
    #[allow(dead_code, reason = "kept for debugging failed tests")]
    pub method: Method,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Recorded {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("recorded body is json")
    }

    pub fn form(&self) -> Vec<(String, String)> {
        url::form_urlencoded::parse(&self.body)
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

#[derive(Clone, Default)]
struct Shared {
    script: Arc<Mutex<VecDeque<StubResponse>>>,
    seen: Arc<Mutex<Vec<Recorded>>>,
}

/// A running stub server.
pub struct Stub {
    pub base_url: String,
    shared: Shared,
}

impl Stub {
    /// Start a stub that answers with `script` in order. Extra requests get
    /// a 599 answer, which fails the test that made them.
    pub async fn start(script: Vec<StubResponse>) -> Self {
        let shared = Shared {
            script: Arc::new(Mutex::new(script.into())),
            seen: Arc::default(),
        };
        let app = axum::Router::new()
            .fallback(handle)
            .with_state(shared.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("stub binds");
        let addr = listener.local_addr().expect("stub has an addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            base_url: format!("http://{addr}"),
            shared,
        }
    }

    /// The requests seen so far.
    pub fn requests(&self) -> Vec<Recorded> {
        self.shared.seen.lock().expect("lock").clone()
    }
}

async fn handle(
    State(shared): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    shared.seen.lock().expect("lock").push(Recorded {
        method,
        path: uri
            .path_and_query()
            .map_or_else(|| uri.path().to_owned(), ToString::to_string),
        headers,
        body,
    });
    let next = shared.script.lock().expect("lock").pop_front();
    let Some(answer) = next else {
        return (
            StatusCode::from_u16(599).expect("valid"),
            "stub script is empty",
        )
            .into_response();
    };
    tokio::time::sleep(answer.delay).await;
    let mut response = Response::builder().status(answer.status);
    for (name, value) in &answer.headers {
        response = response.header(name.as_str(), value.as_str());
    }
    response
        .body(axum::body::Body::from(answer.body))
        .expect("stub response builds")
}

impl Stub {
    /// Start a stub that answers every request with GraphQL `data`.
    pub async fn data(answers: &[serde_json::Value]) -> Self {
        Self::start(
            answers
                .iter()
                .map(|data| StubResponse::json(200, &serde_json::json!({ "data": data })))
                .collect(),
        )
        .await
    }

    /// The GraphQL endpoint of the stub.
    pub fn graphql_url(&self) -> String {
        format!("{}/graphql", self.base_url)
    }
}

// ── Runtime and app helpers ──────────────────────────────────────────

use autumn_web::AppState;
use autumn_web::entropy::SeededEntropy;
use autumn_web::test::{TestApp, TestClient};
use autumn_web::time::FixedClock;

use crate::config::LinearConfig;
use crate::plugin::LinearRuntime;

/// The test webhook secret.
pub const SECRET: &str = "wh-secret";
/// The test client secret.
pub const CLIENT_SECRET: &str = "client-secret";
/// The test workspace.
pub const ORG: &str = "9cfb482a-81e3-4154-b5b9-2c805e70a02d";
/// A fixed test time (Unix ms, October 2026).
pub const NOW_MS: u64 = 1_793_000_000_000;

/// An OAuth-enabled TOML document.
pub const OAUTH_TOML: &str = r#"
[linear]
client_id = "client-id"
app_url = "https://app.example.com"
scopes = ["read", "write"]
actor = "app"
after_install_path = "/welcome"
"#;

/// An env lookup over fixed pairs.
pub fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + Send + Sync + use<> {
    let map: std::collections::HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    move |name| map.get(name).cloned()
}

/// A config from `toml` with the test secrets plus `extra` env pairs.
pub fn config(toml: Option<&str>, extra: &[(&str, &str)]) -> LinearConfig {
    let mut pairs = vec![
        ("LINEAR_WEBHOOK_SECRET", SECRET),
        ("LINEAR_CLIENT_SECRET", CLIENT_SECRET),
    ];
    pairs.extend_from_slice(extra);
    LinearConfig::from_sources(toml, env(&pairs)).expect("test config loads")
}

/// A test state at [`NOW_MS`] with seeded entropy and `runtime` installed.
pub fn state(runtime: Option<LinearRuntime>) -> AppState {
    state_at(runtime, NOW_MS)
}

/// A test state at `now_ms`.
pub fn state_at(runtime: Option<LinearRuntime>, now_ms: u64) -> AppState {
    let now = autumn_web::reexports::chrono::DateTime::from_timestamp_millis(
        i64::try_from(now_ms).expect("fits"),
    )
    .expect("valid time");
    let state = AppState::for_test()
        .with_clock(Arc::new(FixedClock::at(now)))
        .with_entropy(Arc::new(SeededEntropy::new(7)));
    if let Some(runtime) = runtime {
        state.insert_extension(runtime);
    }
    state
}

/// A test client over the plugin routes.
pub fn app(state: &AppState) -> TestClient {
    let (router, _) = crate::plugin::mount();
    let router = axum::Router::new()
        .nest("/linear", router)
        .with_state(state.clone());
    TestApp::from_router(router, state.clone())
}
