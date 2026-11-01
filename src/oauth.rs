//! OAuth install, code exchange, and refresh.
//!
//! `GET /linear/auth` starts an install. It sets a state cookie. Then it
//! sends the browser to `https://linear.app/oauth/authorize`. The URL has
//! the client id, the scopes, the actor, the state, and a PKCE S256
//! challenge.
//!
//! `GET /linear/auth/callback` checks, in this order:
//!
//! 1. An `error` answer (403).
//! 2. The state against the cookie (403).
//! 3. The code (400).
//!
//! Then it exchanges the code with the PKCE verifier and reads the
//! workspace. It stores the token, runs the install handler, and redirects
//! to `after_install_path?organization={id}`. Anyone can type that query
//! value: bind the workspace to your user in the install handler.
//!
//! The flow keeps no server state (ADR 0001). The cookie holds a random
//! nonce. The verifier is `hex(HMAC-SHA256(client_secret, "pkce:" + nonce))`,
//! so the callback derives it again and the browser never sees it.
//!
//! Both routes answer 404 when OAuth is off.

use std::collections::HashSet;
use std::sync::Arc;

use autumn_web::reexports::axum::body::Body;
use autumn_web::reexports::axum::extract::RawQuery;
use autumn_web::reexports::axum::response::Response;
use autumn_web::reexports::http::{HeaderMap, StatusCode, header};
use autumn_web::{AppState, AutumnResult, State, get, public, routes};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::Mac as _;
use serde::Deserialize;
use subtle::ConstantTimeEq as _;

use crate::client::{Credential, LinearClient};
use crate::config::LinearConfig;
use crate::error::{Error, Result};
use crate::graphql::Organization;
use crate::plugin::{InstallContext, Keeper, Linear, LinearRuntime, now_ms};
use crate::tokens::WorkspaceToken;
use crate::types::OrganizationId;

/// The install route path.
pub const AUTH_PATH: &str = "/linear/auth";
/// The callback route path.
pub const CALLBACK_PATH: &str = "/linear/auth/callback";
/// The Linear authorize URL.
pub const AUTHORIZE_URL: &str = "https://linear.app/oauth/authorize";
/// The Linear token URL.
pub const TOKEN_URL: &str = "https://api.linear.app/oauth/token";
/// The state cookie. `__Host-` stops a sibling subdomain from setting it.
pub const STATE_COOKIE: &str = "__Host-linear_oauth_state";
/// The life of the state cookie: 10 minutes.
pub const STATE_COOKIE_MAX_AGE_SECS: u32 = 600;

/// The cookie attributes. `__Host-` needs `Path=/` and `Secure`.
const COOKIE_ATTRIBUTES: &str = "Path=/; HttpOnly; Secure; SameSite=Lax";
/// The token life when Linear sends no `expires_in`: Linear documents 24
/// hours.
pub const DEFAULT_TOKEN_LIFETIME_MS: u64 = 24 * 60 * 60 * 1000;

/// Random bytes in the state nonce.
const NONCE_BYTES: usize = 32;

/// The PKCE verifier for `nonce`: hex HMAC-SHA256 under the client secret.
///
/// 64 hex chars, inside the RFC 7636 length (43–128) and character set.
#[must_use]
pub fn pkce_verifier(client_secret: &str, nonce: &str) -> String {
    let message = format!("pkce:{nonce}");
    crate::verify::mac(client_secret.as_bytes(), message.as_bytes())
        .map_or_else(String::new, |mac| hex::encode(mac.finalize().into_bytes()))
}

/// The PKCE S256 challenge: base64url (no padding) of SHA-256(verifier).
#[must_use]
pub fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// The authorize URL for `nonce`.
///
/// # Errors
///
/// Returns [`Error::Config`] when OAuth fields are missing.
pub fn authorize_url(config: &LinearConfig, nonce: &str, challenge: &str) -> Result<String> {
    let client_id = config
        .client_id
        .as_deref()
        .ok_or_else(|| Error::Config("OAuth needs [linear] client_id".to_owned()))?;
    let redirect_uri = config.redirect_uri().ok_or(Error::OAuthDisabled)?;
    let mut url = url::Url::parse(AUTHORIZE_URL)
        .map_err(|_| Error::Config("the authorize URL does not parse".to_owned()))?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("response_type", "code")
            .append_pair("scope", &config.scopes.join(","))
            .append_pair("state", nonce)
            .append_pair("actor", config.actor.as_str())
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256");
        if config.prompt_consent {
            query.append_pair("prompt", "consent");
        }
    }
    Ok(url.into())
}

/// The token endpoint answer.
#[derive(Debug, Deserialize)]
struct TokenAnswer {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<ScopeField>,
}

/// `scope` is a string, or an array for apps from before 2023-12-01.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ScopeField {
    Text(String),
    List(Vec<String>),
}

/// The token error answer (RFC 6749 section 5.2).
#[derive(Debug, Default, Deserialize)]
struct TokenError {
    #[serde(default)]
    error: Option<String>,
}

/// POST a form to the token endpoint and read a token at `now_ms`.
///
/// # Errors
///
/// Returns [`Error::Reauthorize`] for a refused grant (400 `invalid_grant`
/// or 401) when `terminal_on_refusal` is set, [`Error::LinearApi`] for
/// another non-success status, and [`Error::UnexpectedResponse`] for a bad
/// answer.
async fn token_call(
    keeper: &Keeper,
    form: &[(&str, &str)],
    now_ms: u64,
    terminal_on_refusal: Option<&str>,
) -> Result<WorkspaceToken> {
    let response = keeper
        .http()
        .post(keeper.token_url())
        .form(form)
        .send()
        .await
        .map_err(Error::Http)?;
    let status = response.status().as_u16();
    let bytes = response.bytes().await.map_err(Error::Http)?;
    if !(200..300).contains(&status) {
        let error: TokenError = serde_json::from_slice(&bytes).unwrap_or_default();
        let code = error.error.as_deref();
        if let Some(subject) = terminal_on_refusal
            && code == Some("invalid_grant")
        {
            return Err(Error::Reauthorize(subject.to_owned()));
        }
        // RFC 6749 5.2: `invalid_client` (often 401) is the app secret, not a
        // dead grant. The token stays; the operator fixes the secret.
        if status == 401 || code == Some("invalid_client") {
            return Err(Error::Config(format!(
                "linear refused the client credentials ({}); check {}",
                code.unwrap_or("401"),
                crate::config::CLIENT_SECRET_ENV
            )));
        }
        // The body can echo request fields. Keep only the error code.
        return Err(Error::LinearApi {
            status,
            body: error.error.unwrap_or_default(),
        });
    }
    let answer: TokenAnswer = serde_json::from_slice(&bytes)
        .map_err(|_| crate::graphql::unexpected("the token answer has the wrong shape"))?;
    if answer.access_token.trim().is_empty() {
        return Err(crate::graphql::unexpected(
            "the token answer has no access_token",
        ));
    }
    let scope = match answer.scope {
        Some(ScopeField::Text(text)) => text,
        Some(ScopeField::List(list)) => list.join(","),
        None => String::new(),
    };
    Ok(WorkspaceToken::new(
        answer.access_token,
        answer.refresh_token,
        Some(
            now_ms.saturating_add(
                answer
                    .expires_in
                    .map_or(DEFAULT_TOKEN_LIFETIME_MS, |secs| secs.saturating_mul(1000)),
            ),
        ),
        scope,
    ))
}

/// Exchange an authorization code at `now_ms`.
///
/// # Errors
///
/// See [`token_call`]. A refused code is [`Error::LinearApi`] (502).
pub(crate) async fn exchange_code(
    keeper: &Keeper,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    now_ms: u64,
) -> Result<WorkspaceToken> {
    let mut token = token_call(
        keeper,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", keeper.client_id()?),
            ("client_secret", keeper.client_secret()?),
            ("code_verifier", verifier),
        ],
        now_ms,
        None,
    )
    .await?;
    token.installed_at_ms = Some(now_ms);
    Ok(token)
}

/// Refresh a token at `now_ms`. Linear rotates the pair.
///
/// # Errors
///
/// Returns [`Error::Reauthorize`] when Linear refuses the refresh token
/// (the workspace must install again), and other errors for a transient
/// failure.
pub(crate) async fn refresh_token(
    keeper: &Keeper,
    refresh: &str,
    now_ms: u64,
) -> Result<WorkspaceToken> {
    token_call(
        keeper,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", keeper.client_id()?),
            ("client_secret", keeper.client_secret()?),
        ],
        now_ms,
        Some("the workspace"),
    )
    .await
}

/// Parse a raw query into pairs. The parser refuses a repeated key.
fn parse_query(raw: Option<&str>) -> Result<Vec<(String, String)>> {
    let mut seen = HashSet::new();
    let mut pairs = Vec::new();
    for (key, value) in url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
        if !seen.insert(key.clone()) {
            return Err(Error::InvalidRequest(format!(
                "query parameter {key:?} appears two times"
            )));
        }
        pairs.push((key.into_owned(), value.into_owned()));
    }
    Ok(pairs)
}

/// The value of query parameter `key`.
fn param<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, value)| value.as_str())
}

/// The value of cookie `name` in the `Cookie` headers.
///
/// Returns `None` when the cookie is missing or appears more than once.
pub(crate) fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .filter(|(key, _)| *key == name)
        .map(|(_, value)| value);
    let first = values.next()?;
    values.next().is_none().then_some(first)
}

/// The `Set-Cookie` value that stores `nonce`.
pub(crate) fn state_cookie(nonce: &str) -> String {
    format!("{STATE_COOKIE}={nonce}; Max-Age={STATE_COOKIE_MAX_AGE_SECS}; {COOKIE_ATTRIBUTES}")
}

/// The `Set-Cookie` value that clears the state cookie.
pub(crate) fn clear_state_cookie() -> String {
    format!("{STATE_COOKIE}=; Max-Age=0; {COOKIE_ATTRIBUTES}")
}

/// The runtime, when OAuth is on.
fn oauth_runtime(state: &AppState) -> Result<Arc<LinearRuntime>> {
    let runtime = state
        .extension::<LinearRuntime>()
        .ok_or(Error::MissingRuntime)?;
    if runtime.config().oauth_enabled() {
        Ok(runtime)
    } else {
        Err(Error::OAuthDisabled)
    }
}

/// The install route.
#[get("/auth")]
#[public]
async fn install_route(State(state): State<AppState>) -> AutumnResult<Response> {
    install(&state).map_err(|err| err.to_autumn_error())
}

/// The install flow.
fn install(state: &AppState) -> Result<Response> {
    let runtime = oauth_runtime(state)?;
    let mut nonce = [0_u8; NONCE_BYTES];
    state.entropy().fill_bytes(&mut nonce);
    let nonce = hex::encode(nonce);
    let verifier = pkce_verifier(runtime.keeper().client_secret()?, &nonce);
    let location = authorize_url(runtime.config(), &nonce, &pkce_challenge(&verifier))?;
    tracing::info!("linear install started");
    redirect(&location, &state_cookie(&nonce))
}

/// The callback route.
#[get("/auth/callback")]
#[public]
async fn callback_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> AutumnResult<Response> {
    callback(&state, &headers, query.as_deref())
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "linear install callback failed");
            err.to_autumn_error()
        })
}

/// The callback flow. No token call runs before the state and the code
/// pass.
async fn callback(state: &AppState, headers: &HeaderMap, query: Option<&str>) -> Result<Response> {
    let runtime = oauth_runtime(state)?;
    let params = parse_query(query)?;
    if let Some(error) = param(&params, "error") {
        let error: String = error
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(64)
            .collect();
        return Err(Error::AuthorizationDenied(error));
    }
    let cookie = cookie_value(headers, STATE_COOKIE).unwrap_or_default();
    let given = param(&params, "state").unwrap_or_default();
    let matches: bool = given.as_bytes().ct_eq(cookie.as_bytes()).into();
    if cookie.is_empty() || !matches {
        return Err(Error::StateMismatch);
    }
    let code = param(&params, "code")
        .filter(|code| !code.is_empty())
        .ok_or_else(|| Error::InvalidRequest("the callback has no code".to_owned()))?;
    let keeper = runtime.keeper();
    let redirect_uri = runtime
        .config()
        .redirect_uri()
        .ok_or(Error::OAuthDisabled)?;
    let verifier = pkce_verifier(keeper.client_secret()?, cookie);
    let now = now_ms(state);
    let token = exchange_code(keeper, code, &redirect_uri, &verifier, now).await?;
    let client = LinearClient::from_parts(
        keeper.http().clone(),
        Credential::bearer(token.access_token.clone()),
    )
    .with_endpoint(keeper.graphql_url());
    let organization: Organization = client.organization().await?;
    let org = OrganizationId::parse(&organization.id)
        .map_err(|_| crate::graphql::unexpected("the organization id is not valid"))?;
    let scope = token.scope.clone();
    keeper.store_install(&org, token).await?;
    tracing::info!(org = %org, "linear app installed");
    if let Some(handler) = runtime.on_install() {
        let linear = Linear::from_runtime(Arc::clone(&runtime), state.clone());
        let mut ctx = InstallContext::new(organization, scope, linear);
        ctx.headers = headers.clone();
        handler(ctx)
            .await
            .map_err(|err| Error::Store(format!("the install handler failed: {err}")))?;
    }
    let mut location = url::Url::parse("http://local")
        .and_then(|base| base.join(&runtime.config().after_install_path))
        .map_err(|_| Error::Config("after_install_path does not parse".to_owned()))?;
    location
        .query_pairs_mut()
        .append_pair("organization", org.as_str());
    let path = location.query().map_or_else(
        || location.path().to_owned(),
        |query| format!("{}?{query}", location.path()),
    );
    // A scheme-relative result would leave the app.
    if path.starts_with("//") || !path.starts_with('/') {
        return Err(Error::Config(
            "after_install_path must stay local".to_owned(),
        ));
    }
    redirect(&path, &clear_state_cookie())
}

/// A 302 redirect that sets one cookie.
fn redirect(location: &str, cookie: &str) -> Result<Response> {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, location)
        .header(header::SET_COOKIE, cookie)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::empty())
        .map_err(|err| Error::InvalidRequest(format!("the redirect does not build: {err}")))
}

/// The OAuth routes, for the plugin to mount under `/linear`.
pub(crate) fn oauth_routes() -> Vec<autumn_web::Route> {
    routes![install_route, callback_route]
}

#[cfg(test)]
mod tests;
