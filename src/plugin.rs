//! The Autumn plugin: [`LinearPlugin`], [`LinearRuntime`], and the
//! [`Linear`] extractor.
//!
//! ```no_run
//! use autumn_plugin_linear::{LinearPlugin, WebhookContext};
//! use autumn_web::prelude::*;
//!
//! async fn issue_created(ctx: WebhookContext) -> AutumnResult<()> {
//!     tracing::info!(org = %ctx.event.organization_id, "issue created");
//!     Ok(())
//! }
//!
//! #[autumn_web::main]
//! async fn main() {
//!     autumn_web::app()
//!         .plugin(LinearPlugin::new().on_action("Issue", "create", issue_created))
//!         .run()
//!         .await;
//! }
//! ```
//!
//! The plugin declares the `[linear]` section, mounts its routes under
//! `/linear`, and installs an `on_startup` hook. The hook loads and checks
//! the config, builds the [`LinearRuntime`], and puts it in the
//! [`AppState`] extensions. A bad config stops the startup.

use std::borrow::Cow;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use autumn_web::actuator::IndicatorGroup;
use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;
use autumn_web::plugin_contract::PluginContract;
use autumn_web::reexports::axum::Router;
use autumn_web::reexports::axum::extract::FromRequestParts;
use autumn_web::reexports::http::request::Parts;
use autumn_web::route_listing::RouteInfo;
use autumn_web::{AppState, AutumnError, AutumnResult};

use crate::client::{Credential, LinearClient, RetryPolicy};
use crate::config::LinearConfig;
use crate::delivery::{DeliveryStore, InMemoryDeliveryStore};
use crate::error::{Error, Result};
use crate::graphql::Organization;
use crate::policy::REFRESH_MARGIN_MS;
use crate::tokens::{InMemoryTokenStore, TokenStore, WorkspaceToken};
use crate::types::OrganizationId;
use crate::verify::SecretSet;
use crate::webhooks::{IntoWebhookHandler, WebhookHandler, WebhookRegistry};

/// The `autumn-web` range this plugin supports. Keep it equal to the
/// `autumn-web` requirement in `Cargo.toml`.
const SUPPORTED_AUTUMN_WEB: &str = "0.10";

/// After a failed refresh, callers use the old token (while it is valid)
/// for this many ms before the next try.
pub const REFRESH_BACKOFF_MS: u64 = 10_000;

/// A builder closure for [`LinearPlugin::configure`].
type ConfigureFn = Arc<dyn Fn(&mut LinearConfig) + Send + Sync + 'static>;

/// An environment lookup for [`LinearPlugin::env_lookup`].
type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync + 'static>;

/// The future an install handler returns.
pub type InstallFuture = Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>>;

/// A type-erased install handler.
pub type InstallHandler = Arc<dyn Fn(InstallContext) -> InstallFuture + Send + Sync + 'static>;

/// The time of the app clock in Unix ms.
pub(crate) fn now_ms(state: &AppState) -> u64 {
    u64::try_from(state.clock().now().timestamp_millis()).unwrap_or(0)
}

/// What an install handler receives. The token is already stored.
#[derive(Clone)]
#[non_exhaustive]
pub struct InstallContext {
    /// The workspace that installed the app.
    pub organization: Organization,
    /// The granted scopes, as Linear sent them.
    pub scope: String,
    /// The app state.
    pub state: AppState,
    /// The plugin handle. `client_for(&org)` works at once.
    pub linear: Linear,
    /// The callback request headers. Read your session cookie here to bind
    /// the workspace to your user. Never trust the `organization` query
    /// value on the page after the install: anyone can type it.
    pub headers: autumn_web::reexports::http::HeaderMap,
}

impl InstallContext {
    /// Make a context, for example to test an install handler.
    #[must_use]
    pub fn new(organization: Organization, scope: impl Into<String>, linear: Linear) -> Self {
        Self {
            organization,
            scope: scope.into(),
            state: linear.state().clone(),
            linear,
            headers: autumn_web::reexports::http::HeaderMap::new(),
        }
    }

    /// The workspace id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] when the id is not valid.
    pub fn organization_id(&self) -> Result<OrganizationId> {
        OrganizationId::parse(&self.organization.id)
    }
}

impl std::fmt::Debug for InstallContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallContext")
            .field("organization", &self.organization)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

/// The per-workspace refresh state, behind the per-workspace lock.
#[derive(Debug, Default)]
struct RefreshState {
    failed_at_ms: Option<u64>,
}

/// The per-workspace lock table.
type LockTable = HashMap<OrganizationId, Arc<tokio::sync::Mutex<RefreshState>>>;

/// Token work: storage, refresh, install, and revoke, serialized per
/// workspace. It lives behind an `Arc`, so that a refresh can run in its
/// own task.
pub(crate) struct Keeper {
    http: reqwest::Client,
    api_base_url: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    tokens: Arc<dyn TokenStore>,
    locks: Mutex<LockTable>,
}

impl Keeper {
    /// The shared HTTP pool.
    pub(crate) const fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// The OAuth client id.
    pub(crate) fn client_id(&self) -> Result<&str> {
        self.client_id
            .as_deref()
            .ok_or_else(|| Error::Config("token calls need [linear] client_id".to_owned()))
    }

    /// The OAuth client secret. Never log it.
    pub(crate) fn client_secret(&self) -> Result<&str> {
        self.client_secret.as_deref().ok_or_else(|| {
            Error::Config(format!(
                "token calls need {}",
                crate::config::CLIENT_SECRET_ENV
            ))
        })
    }

    /// The token endpoint.
    pub(crate) fn token_url(&self) -> String {
        self.api_base_url.as_deref().map_or_else(
            || crate::oauth::TOKEN_URL.to_owned(),
            |base| format!("{}/oauth/token", base.trim_end_matches('/')),
        )
    }

    /// The GraphQL endpoint.
    pub(crate) fn graphql_url(&self) -> String {
        self.api_base_url.as_deref().map_or_else(
            || crate::graphql::ENDPOINT.to_owned(),
            |base| format!("{}/graphql", base.trim_end_matches('/')),
        )
    }

    /// The lock for `org`. The entry stays in the table: a removed entry
    /// lets a waiter on the old lock and a new writer on a new lock run at
    /// one time.
    fn lock_for(&self, org: &OrganizationId) -> Result<Arc<tokio::sync::Mutex<RefreshState>>> {
        let mut locks = self
            .locks
            .lock()
            .map_err(|_| Error::Store("token lock table is poisoned".to_owned()))?;
        Ok(Arc::clone(locks.entry(org.clone()).or_default()))
    }

    /// A token for `org` that is fresh at `now_ms`. Refreshes it when needed.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the per-workspace lock must cover the read, the refresh, and the write"
    )]
    async fn fresh_token(&self, org: &OrganizationId, now_ms: u64) -> Result<WorkspaceToken> {
        let lock = self.lock_for(org)?;
        let mut state = lock.lock().await;
        let token = self
            .tokens
            .get(org)
            .await?
            .ok_or_else(|| Error::NotInstalled(org.to_string()))?;
        if !token.needs_refresh(now_ms, REFRESH_MARGIN_MS) {
            return Ok(token);
        }
        let since_failure = state.failed_at_ms.map(|at| now_ms.saturating_sub(at));
        if let Some(elapsed) = since_failure.filter(|elapsed| *elapsed < REFRESH_BACKOFF_MS) {
            if !token.is_expired(now_ms) {
                return Ok(token);
            }
            // The token is dead and Linear failed a short time ago. Each
            // waiting caller must not make its own slow call.
            return Err(Error::RefreshBackoff {
                retry_after: std::time::Duration::from_millis(REFRESH_BACKOFF_MS - elapsed),
            });
        }
        let Some(refresh) = token.refresh_token.clone() else {
            if token.is_expired(now_ms) {
                self.delete_if_unchanged(org, &token).await?;
                return Err(Error::Reauthorize(org.to_string()));
            }
            return Ok(token);
        };
        let outcome = crate::oauth::refresh_token(self, &refresh, now_ms).await;
        self.settle_refresh(org, token, refresh, outcome, &mut state, now_ms)
            .await
    }

    /// Store a refreshed pair, or decide what a failed refresh means.
    async fn settle_refresh(
        &self,
        org: &OrganizationId,
        token: WorkspaceToken,
        refresh: String,
        outcome: Result<WorkspaceToken>,
        state: &mut RefreshState,
        now_ms: u64,
    ) -> Result<WorkspaceToken> {
        match outcome {
            Ok(mut fresh) => {
                fresh.installed_at_ms = token.installed_at_ms;
                if fresh.refresh_token.is_none() {
                    fresh.refresh_token = Some(refresh);
                }
                if fresh.scope.is_empty() {
                    fresh.scope.clone_from(&token.scope);
                }
                self.tokens.put(org, fresh.clone()).await?;
                state.failed_at_ms = None;
                tracing::info!(org = %org, "linear token refreshed");
                Ok(fresh)
            }
            Err(Error::Reauthorize(_)) => {
                // Another replica or a new install can have stored a new
                // pair. Use it when it is fresh; else delete the dead pair.
                if let Some(current) = self.tokens.get(org).await?
                    && current != token
                    && !current.needs_refresh(now_ms, REFRESH_MARGIN_MS)
                {
                    return Ok(current);
                }
                self.delete_if_unchanged(org, &token).await?;
                tracing::warn!(org = %org, "linear refused the refresh token; the workspace must install the app again");
                Err(Error::Reauthorize(org.to_string()))
            }
            Err(err) => {
                state.failed_at_ms = Some(now_ms);
                if token.is_expired(now_ms) {
                    Err(err)
                } else {
                    tracing::warn!(org = %org, error = %err, "linear refresh failed; the old token is still valid");
                    Ok(token)
                }
            }
        }
    }

    /// Delete the token of `org` only when the store still holds `seen`.
    async fn delete_if_unchanged(&self, org: &OrganizationId, seen: &WorkspaceToken) -> Result<()> {
        self.tokens.delete_if(org, seen).await.map(|_| ())
    }

    /// Delete the token of `org` when Linear no longer accepts it. Use it
    /// for a revoke with no stable event time. Returns `true` when the
    /// token is gone.
    ///
    /// # Errors
    ///
    /// Returns a transient error when the probe cannot reach Linear, so
    /// that the webhook fails and Linear retries.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the per-workspace lock must cover the probe and the delete"
    )]
    pub(crate) async fn delete_if_dead(&self, org: &OrganizationId) -> Result<bool> {
        let lock = self.lock_for(org)?;
        let _guard = lock.lock().await;
        let Some(token) = self.tokens.get(org).await? else {
            return Ok(true);
        };
        let probe = LinearClient::from_parts(
            self.http.clone(),
            Credential::bearer(token.access_token.clone()),
        )
        .with_endpoint(self.graphql_url())
        .with_retry(RetryPolicy::new(0, std::time::Duration::ZERO))
        .viewer()
        .await;
        match probe {
            Ok(_) => Ok(false),
            Err(err) if err.is_transient() => Err(err),
            Err(_) => self.tokens.delete_if(org, &token).await,
        }
    }

    /// Store the token from an install.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the per-workspace lock must cover the write"
    )]
    pub(crate) async fn store_install(
        &self,
        org: &OrganizationId,
        token: WorkspaceToken,
    ) -> Result<()> {
        let lock = self.lock_for(org)?;
        let mut state = lock.lock().await;
        self.tokens.put(org, token).await?;
        state.failed_at_ms = None;
        Ok(())
    }

    /// Delete the token of `org` for an event at `event_at_ms`. A token
    /// from an install after the event stays. Returns `true` when the
    /// token is gone.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the per-workspace lock must cover the read and the delete"
    )]
    pub(crate) async fn delete_for_event(
        &self,
        org: &OrganizationId,
        event_at_ms: Option<u64>,
    ) -> Result<bool> {
        let lock = self.lock_for(org)?;
        let _guard = lock.lock().await;
        let current = self.tokens.get(org).await?;
        let newer = matches!(
            (current.as_ref().and_then(|t| t.installed_at_ms), event_at_ms),
            (Some(installed), Some(event)) if installed > event
        );
        if newer {
            return Ok(false);
        }
        self.tokens.delete(org).await?;
        Ok(true)
    }
}

/// Runtime state shared by the routes and the [`Linear`] extractor.
///
/// The startup hook puts it in the [`AppState`] extensions.
pub struct LinearRuntime {
    config: LinearConfig,
    secrets: Option<SecretSet>,
    retry: RetryPolicy,
    registry: WebhookRegistry,
    deliveries: Arc<dyn DeliveryStore>,
    on_install: Option<InstallHandler>,
    keeper: Arc<Keeper>,
    api_key: Option<String>,
}

/// `Debug` shows no secrets and no tokens.
impl std::fmt::Debug for LinearRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinearRuntime")
            .field("config", &self.config)
            .field("webhooks", &self.secrets.is_some())
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("handlers", &self.registry.keys())
            .finish_non_exhaustive()
    }
}

impl LinearRuntime {
    /// The resolved, validated config.
    #[must_use]
    pub const fn config(&self) -> &LinearConfig {
        &self.config
    }

    /// The webhook secrets. `None` when webhooks are off.
    pub(crate) const fn secrets(&self) -> Option<&SecretSet> {
        self.secrets.as_ref()
    }

    /// The webhook handlers.
    #[must_use]
    pub const fn registry(&self) -> &WebhookRegistry {
        &self.registry
    }

    /// The token store, to read tokens.
    ///
    /// Do not write or delete tokens through it: the plugin serializes
    /// writes per workspace, and a direct write can race a refresh.
    #[must_use]
    pub fn token_store(&self) -> &Arc<dyn TokenStore> {
        &self.keeper.tokens
    }

    /// The duplicate-delivery store.
    #[must_use]
    pub const fn delivery_store(&self) -> &Arc<dyn DeliveryStore> {
        &self.deliveries
    }

    /// The install handler, if set.
    pub(crate) const fn on_install(&self) -> Option<&InstallHandler> {
        self.on_install.as_ref()
    }

    /// The token keeper.
    pub(crate) fn keeper(&self) -> &Keeper {
        &self.keeper
    }

    /// The client with the personal API key.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when `LINEAR_API_KEY` is not set.
    pub fn client(&self) -> Result<LinearClient> {
        let key = self.api_key.as_ref().ok_or_else(|| {
            Error::Config(format!(
                "no API-key client: set {}",
                crate::config::API_KEY_ENV
            ))
        })?;
        Ok(self.make_client(Credential::api_key(key.clone())))
    }

    /// A client on the shared pool, with the configured retry.
    fn make_client(&self, credential: Credential) -> LinearClient {
        LinearClient::from_parts(self.keeper.http.clone(), credential)
            .with_endpoint(self.keeper.graphql_url())
            .with_retry(self.retry)
    }

    /// The client for workspace `org`, at Unix time `now_ms`.
    ///
    /// With OAuth off, this is the API-key client. With OAuth on, the
    /// client uses the stored token of `org`. When the token expires
    /// within [`REFRESH_MARGIN_MS`], it refreshes first. Only one refresh
    /// for a workspace runs at a time, in its own task: a cancelled caller
    /// does not lose the new token pair. When a refresh fails and the old
    /// token is still valid, the client uses the old token.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotInstalled`] when the workspace has no token,
    /// [`Error::Reauthorize`] when Linear refuses the refresh token, or a
    /// store or HTTP error.
    pub async fn client_for(&self, org: &OrganizationId, now_ms: u64) -> Result<LinearClient> {
        if !self.config.oauth_enabled() {
            return self.client();
        }
        let token = self
            .keeper
            .tokens
            .get(org)
            .await?
            .ok_or_else(|| Error::NotInstalled(org.to_string()))?;
        if !token.needs_refresh(now_ms, REFRESH_MARGIN_MS) {
            return Ok(self.make_client(Credential::bearer(token.access_token)));
        }
        let keeper = Arc::clone(&self.keeper);
        let owned = org.clone();
        let token = tokio::spawn(async move { keeper.fresh_token(&owned, now_ms).await })
            .await
            .map_err(|_| Error::Store("the token refresh task stopped".to_owned()))??;
        Ok(self.make_client(Credential::bearer(token.access_token)))
    }
}

/// The plugin handle for app routes.
///
/// ```no_run
/// use autumn_plugin_linear::Linear;
/// use autumn_web::prelude::*;
///
/// #[get("/me")]
/// #[secured]
/// async fn me(linear: Linear) -> AutumnResult<String> {
///     let client = linear.client().map_err(|e| e.to_autumn_error())?;
///     let viewer = client.viewer().await.map_err(|e| e.to_autumn_error())?;
///     Ok(viewer.name.unwrap_or_default())
/// }
/// ```
///
/// Extraction fails with 503 when the app does not use the plugin.
#[derive(Clone)]
pub struct Linear {
    runtime: Arc<LinearRuntime>,
    state: AppState,
}

impl std::fmt::Debug for Linear {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Linear")
            .field("runtime", &self.runtime)
            .finish_non_exhaustive()
    }
}

impl Linear {
    /// Read the handle from the app state.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingRuntime`] when the app does not use the
    /// plugin.
    pub fn from_state(state: &AppState) -> Result<Self> {
        let runtime = state
            .extension::<LinearRuntime>()
            .ok_or(Error::MissingRuntime)?;
        Ok(Self::from_runtime(runtime, state.clone()))
    }

    /// Make a handle from a runtime and a state.
    pub(crate) const fn from_runtime(runtime: Arc<LinearRuntime>, state: AppState) -> Self {
        Self { runtime, state }
    }

    /// The runtime.
    #[must_use]
    pub const fn runtime(&self) -> &Arc<LinearRuntime> {
        &self.runtime
    }

    /// The app state.
    #[must_use]
    pub const fn state(&self) -> &AppState {
        &self.state
    }

    /// The API-key client.
    ///
    /// # Errors
    ///
    /// See [`LinearRuntime::client`].
    pub fn client(&self) -> Result<LinearClient> {
        self.runtime.client()
    }

    /// The client for workspace `org`. Time comes from the app clock.
    ///
    /// # Errors
    ///
    /// See [`LinearRuntime::client_for`].
    pub async fn client_for(&self, org: &OrganizationId) -> Result<LinearClient> {
        self.runtime.client_for(org, now_ms(&self.state)).await
    }

    /// The local URL that starts an install. Send the browser there on
    /// [`Error::NotInstalled`] or [`Error::Reauthorize`].
    #[must_use]
    pub const fn install_url() -> &'static str {
        crate::oauth::AUTH_PATH
    }
}

impl FromRequestParts<AppState> for Linear {
    type Rejection = AutumnError;

    fn from_request_parts(
        _parts: &mut Parts,
        state: &AppState,
    ) -> impl Future<Output = std::result::Result<Self, Self::Rejection>> {
        std::future::ready(Self::from_state(state).map_err(|err| err.to_autumn_error()))
    }
}

/// One registered handler: type, optional action, handler.
type HandlerEntry = (String, Option<String>, WebhookHandler);

/// Linear as an Autumn plugin.
#[derive(Clone, Default)]
pub struct LinearPlugin {
    configure: Vec<ConfigureFn>,
    handlers: Vec<HandlerEntry>,
    token_store: Option<Arc<dyn TokenStore>>,
    delivery_store: Option<Arc<dyn DeliveryStore>>,
    api_base_url: Option<String>,
    on_install: Option<InstallHandler>,
    env: Option<EnvLookup>,
}

impl std::fmt::Debug for LinearPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: Vec<String> = self
            .handlers
            .iter()
            .map(|(t, a, _)| a.as_ref().map_or_else(|| t.clone(), |a| format!("{t}:{a}")))
            .collect();
        f.debug_struct("LinearPlugin")
            .field("handlers", &keys)
            .finish_non_exhaustive()
    }
}

impl LinearPlugin {
    /// Make the plugin.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Change config values in code. The closure runs after the file and
    /// the environment, so its values win. It cannot set secrets.
    #[must_use]
    pub fn configure(
        mut self,
        configure: impl Fn(&mut LinearConfig) + Send + Sync + 'static,
    ) -> Self {
        self.configure.push(Arc::new(configure));
        self
    }

    /// Read the `LINEAR_*` variables from `lookup` instead of the process
    /// environment. Use it to read secrets from a vault, or in tests.
    #[must_use]
    pub fn env_lookup(
        mut self,
        lookup: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.env = Some(Arc::new(lookup));
        self
    }

    /// Handle every action of `event_type`, for example `Issue`.
    ///
    /// A handler for the exact action (see [`on_action`](Self::on_action))
    /// wins. A malformed or repeated key stops the startup.
    #[must_use]
    pub fn on(mut self, event_type: impl Into<String>, handler: impl IntoWebhookHandler) -> Self {
        self.handlers
            .push((event_type.into(), None, handler.into_handler()));
        self
    }

    /// Handle one action of `event_type`, for example `("Issue", "create")`.
    ///
    /// A malformed or repeated key stops the startup.
    #[must_use]
    pub fn on_action(
        mut self,
        event_type: impl Into<String>,
        action: impl Into<String>,
        handler: impl IntoWebhookHandler,
    ) -> Self {
        self.handlers.push((
            event_type.into(),
            Some(action.into()),
            handler.into_handler(),
        ));
        self
    }

    /// Replace the in-memory token store.
    #[must_use]
    pub fn token_store(mut self, store: Arc<dyn TokenStore>) -> Self {
        self.token_store = Some(store);
        self
    }

    /// Replace the in-memory duplicate-delivery store.
    #[must_use]
    pub fn delivery_store(mut self, store: Arc<dyn DeliveryStore>) -> Self {
        self.delivery_store = Some(store);
        self
    }

    /// Run `handler` after each install, when the token is stored. Use it
    /// to save a workspace record or to create webhooks. An error gives
    /// 500. The token stays in the store.
    #[must_use]
    pub fn on_install<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(InstallContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = AutumnResult<()>> + Send + 'static,
    {
        self.on_install = Some(Arc::new(move |ctx| Box::pin(handler(ctx))));
        self
    }

    /// Send GraphQL and token calls to `base_url` (`{base}/graphql`,
    /// `{base}/oauth/token`). For tests only: the startup fails unless the
    /// host is `localhost`, `127.0.0.1`, or `[::1]`, because the calls
    /// carry the client secret and the tokens.
    #[must_use]
    pub fn api_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.api_base_url = Some(base_url.into());
        self
    }

    /// Load the config with the plugin lookup (or the process environment).
    ///
    /// # Errors
    ///
    /// See [`LinearConfig::load_with`].
    pub fn load_config(&self) -> Result<LinearConfig> {
        self.env.as_ref().map_or_else(LinearConfig::load, |env| {
            LinearConfig::load_with(|name| env(name))
        })
    }

    /// Build the runtime from a loaded config: apply the builder closures,
    /// validate, and register the webhook handlers.
    ///
    /// The startup hook calls this with [`load_config`](Self::load_config).
    /// Tests call it with [`LinearConfig::from_sources`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when the config is not valid, a handler
    /// key is malformed or repeated, or handlers exist with no webhook
    /// secret.
    pub fn build_runtime(&self, config: LinearConfig) -> Result<LinearRuntime> {
        let mut config = config;
        for configure in &self.configure {
            configure(&mut config);
        }
        config.validate()?;
        let secrets = config.webhook_secrets()?;
        let mut registry = WebhookRegistry::new();
        for (event_type, action, handler) in &self.handlers {
            registry.register(event_type, action.as_deref(), Arc::clone(handler))?;
        }
        if secrets.is_none() && !registry.is_empty() {
            return Err(Error::Config(format!(
                "webhook handlers need the signing secret; set {}",
                crate::config::WEBHOOK_SECRET_ENV
            )));
        }
        if let Some(base) = &self.api_base_url {
            check_loopback(base)?;
        }
        let keeper = Keeper {
            http: crate::client::http_client()?,
            api_base_url: self.api_base_url.clone(),
            client_id: config.client_id.clone(),
            client_secret: config.client_secret().map(ToOwned::to_owned),
            tokens: self
                .token_store
                .clone()
                .unwrap_or_else(|| Arc::new(InMemoryTokenStore::new())),
            locks: Mutex::new(HashMap::new()),
        };
        Ok(LinearRuntime {
            secrets,
            retry: config.retry_policy(),
            registry,
            deliveries: self
                .delivery_store
                .clone()
                .unwrap_or_else(|| Arc::new(InMemoryDeliveryStore::default())),
            on_install: self.on_install.clone(),
            keeper: Arc::new(keeper),
            api_key: config.api_key().map(ToOwned::to_owned),
            config,
        })
    }

    /// The startup work: build the runtime, log warnings, register the
    /// health indicator, and put the runtime in the app state.
    pub(crate) fn start(&self, state: &AppState, config: LinearConfig) -> Result<()> {
        let runtime = self.build_runtime(config)?;
        for warning in startup_warnings(&runtime, &state.config_arc()) {
            tracing::warn!("{warning}");
        }
        if let Ok(client) = runtime.client() {
            // Health only, not readiness: a Linear outage must not take the
            // app out of the load balancer, or webhooks fail too. No
            // retries: a probe must answer fast.
            let client = client.with_retry(RetryPolicy::new(0, std::time::Duration::ZERO));
            // A duplicate name means the app adds the plugin twice. The
            // framework already warns about that.
            let _ = state.health_indicator_registry().register(
                "linear",
                IndicatorGroup::HealthOnly,
                crate::health::indicator(&client),
            );
        }
        tracing::info!(
            handlers = runtime.registry().keys().len(),
            webhooks = runtime.secrets().is_some(),
            oauth = runtime.config().oauth_enabled(),
            api_key = runtime.api_key.is_some(),
            "linear plugin ready"
        );
        state.extension_or_insert_with(|| runtime);
        Ok(())
    }
}

impl Plugin for LinearPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("autumn-plugin-linear")
    }

    /// Declare the supported `autumn-web` range. A mismatch fails at
    /// registration with both versions named.
    fn contract(&self) -> Option<PluginContract> {
        Some(
            PluginContract::new(env!("CARGO_PKG_NAME"))
                .plugin_version(env!("CARGO_PKG_VERSION"))
                .autumn_web(SUPPORTED_AUTUMN_WEB),
        )
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        let (router, infos) = mount();
        app.config_section(crate::config::CONFIG_SECTION)
            .on_startup(move |state: AppState| {
                let plugin = self.clone();
                async move {
                    let config = plugin.load_config().map_err(|err| err.to_autumn_error())?;
                    plugin
                        .start(&state, config)
                        .map_err(|err| err.to_autumn_error())
                }
            })
            .nest("/linear", router)
            .declare_plugin_routes(infos)
    }
}

/// Startup warnings: a webhook path that CSRF checks block, and an API key
/// that OAuth mode does not use for webhook contexts.
pub(crate) fn startup_warnings(
    runtime: &LinearRuntime,
    app_config: &autumn_web::config::AutumnConfig,
) -> Vec<String> {
    let mut warnings = Vec::new();
    if runtime.config().oauth_enabled() && runtime.api_key.is_some() {
        warnings.push(format!(
            "linear: OAuth is on, so client_for(org) uses workspace tokens; {} serves only client()",
            crate::config::API_KEY_ENV
        ));
    }
    let csrf = &app_config.security.csrf;
    if runtime.secrets().is_some()
        && csrf.enabled
        && !csrf
            .exempt_paths
            .iter()
            .any(|path| covers(path, WEBHOOK_PATH))
    {
        warnings.push(format!(
            "linear: CSRF checks block {WEBHOOK_PATH}; add it to security.csrf.exempt_paths"
        ));
    }
    warnings
}

/// The plugin routes under `/linear`, with their route metadata.
///
/// [`AppBuilder::declare_plugin_routes`] gets the metadata, so
/// `autumn routes audit` sees the mount as enumerable. Autumn derives each
/// route classification from `#[public]` on the handler.
pub(crate) fn mount() -> (Router<AppState>, Vec<RouteInfo>) {
    let all_routes: Vec<autumn_web::Route> = crate::webhooks::webhook_routes()
        .into_iter()
        .chain(crate::oauth::oauth_routes())
        .collect();
    // These routes declare no API version, so this call cannot fail.
    let mut infos = autumn_web::route_listing::collect_route_infos(&all_routes, &[], &[], &[])
        .unwrap_or_default();
    for info in &mut infos {
        info.path = format!("/linear{}", info.path);
    }
    let mut router = Router::<AppState>::new();
    for route in all_routes {
        router = router.route(route.path, route.handler);
    }
    (router, infos)
}

/// Check that `base` is an `http(s)` URL on a loopback host.
fn check_loopback(base: &str) -> Result<()> {
    let url = url::Url::parse(base)
        .map_err(|_| Error::Config(format!("api_base_url {base:?} is not a valid URL")))?;
    if crate::config::is_loopback(&url) && matches!(url.scheme(), "http" | "https") {
        Ok(())
    } else {
        Err(Error::Config(
            "api_base_url is for tests only and must use a loopback host".to_owned(),
        ))
    }
}

/// The webhook route path.
pub const WEBHOOK_PATH: &str = "/linear/webhooks";

/// True when the exempt prefix `prefix` covers `path`, with the same
/// segment rule as Autumn's CSRF layer.
fn covers(prefix: &str, path: &str) -> bool {
    path == prefix
        || (path.starts_with(prefix)
            && (prefix.ends_with('/') || path.as_bytes().get(prefix.len()) == Some(&b'/')))
}

#[cfg(test)]
mod tests;
