//! The GraphQL API client over `reqwest`.
//!
//! [`LinearClient`] sends [`Operation`]s to `https://api.linear.app/graphql`.
//! A personal API key goes raw in `Authorization`; an OAuth token goes as
//! `Bearer`. See [`Credential`].
//!
//! Linear does not run a rate-limited call, so a retry is safe. The client
//! retries an HTTP 429 and a `RATELIMITED` error, up to
//! [`RetryPolicy::max_retries`], after the `Retry-After` wait. A wait
//! longer than [`RetryPolicy::max_wait`] stops the retries. The error keeps
//! the wait, so a job queue can run the work later. With no `Retry-After`,
//! the wait comes from `X-RateLimit-Requests-Reset`.
//!
//! The client does not retry other failures: a second run of a mutation
//! can change data two times. It does not follow redirects.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::graphql::{
    ActivityContent, Comment, CommentCreate, Connection, Created, ENDPOINT, Issue, IssueCreate,
    IssueUpdate, Operation, Organization, Viewer, Webhook, WebhookCreate, ops, parse_response,
};
use crate::policy::DEFAULT_RETRY_WAIT_MS;

/// Request timeout for Linear calls.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How the client retries rate-limited calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RetryPolicy {
    /// The number of retries after the first try.
    pub max_retries: u32,
    /// The longest wait before one retry. A longer `Retry-After` stops the
    /// retries.
    pub max_wait: Duration,
}

impl RetryPolicy {
    /// Make a policy.
    #[must_use]
    pub const fn new(max_retries: u32, max_wait: Duration) -> Self {
        Self {
            max_retries,
            max_wait,
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::new(3, Duration::from_secs(10))
    }
}

/// The credential of a [`LinearClient`]. `Debug` redacts it.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    /// A personal API key. Linear wants it raw in `Authorization`.
    ApiKey(String),
    /// An OAuth access token. Linear wants `Bearer <token>`.
    Bearer(String),
}

impl Credential {
    /// A personal API key.
    #[must_use]
    pub fn api_key(key: impl Into<String>) -> Self {
        Self::ApiKey(key.into())
    }

    /// An OAuth access token.
    #[must_use]
    pub fn bearer(token: impl Into<String>) -> Self {
        Self::Bearer(token.into())
    }

    /// The `Authorization` header value. Never log it.
    fn header(&self) -> String {
        match self {
            Self::ApiKey(key) => key.clone(),
            Self::Bearer(token) => format!("Bearer {token}"),
        }
    }

    fn is_blank(&self) -> bool {
        match self {
            Self::ApiKey(value) | Self::Bearer(value) => value.trim().is_empty(),
        }
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey(_) => f.write_str("ApiKey(<redacted>)"),
            Self::Bearer(_) => f.write_str("Bearer(<redacted>)"),
        }
    }
}

/// A GraphQL client for one credential.
///
/// A clone is cheap. `Debug` redacts the credential.
#[derive(Clone)]
pub struct LinearClient {
    http: reqwest::Client,
    endpoint: String,
    credential: Credential,
    retry: RetryPolicy,
}

impl std::fmt::Debug for LinearClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinearClient")
            .field("endpoint", &self.endpoint)
            .field("credential", &self.credential)
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

impl LinearClient {
    /// Make a client with a new HTTP connection pool.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when the credential is blank, or
    /// [`Error::Http`] when the HTTP client does not build.
    pub fn new(credential: Credential) -> Result<Self> {
        if credential.is_blank() {
            return Err(Error::Config("the Linear credential is blank".to_owned()));
        }
        Ok(Self::from_parts(http_client()?, credential))
    }

    /// Make a client on a shared HTTP pool.
    pub(crate) fn from_parts(http: reqwest::Client, credential: Credential) -> Self {
        Self {
            http,
            endpoint: ENDPOINT.to_owned(),
            credential,
            retry: RetryPolicy::default(),
        }
    }

    /// Send requests to `endpoint` instead of Linear. Use it for a local
    /// stub in tests. The requests carry the credential: use only a host
    /// you trust. This call has no loopback check; `LinearPlugin::api_base_url`
    /// has one.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Set the retry policy.
    #[must_use]
    pub const fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// The GraphQL endpoint URL.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Send `op` and return `data`. Rate limits retry (see the module docs).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Http`] on a transport failure, and the errors of
    /// [`parse_response`]. [`Error::RateLimited`] counts the retries done.
    pub async fn execute(&self, op: &Operation) -> Result<Value> {
        let body = op.body();
        let mut retries = 0;
        loop {
            let response = self
                .http
                .post(&self.endpoint)
                .header(reqwest::header::AUTHORIZATION, self.credential.header())
                .json(&body)
                .send()
                .await
                .map_err(Error::Http)?;
            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned);
            let reset_wait = reset_wait(response.headers());
            let bytes = response.bytes().await.map_err(Error::Http)?;
            let retry_after = match parse_response(status, retry_after.as_deref(), &bytes) {
                Err(Error::RateLimited { retry_after, .. }) => retry_after,
                other => return other,
            };
            // `Retry-After` first, then the reset time Linear sends in ms,
            // then one second.
            let hint = retry_after
                .or(reset_wait)
                .unwrap_or(Duration::from_millis(DEFAULT_RETRY_WAIT_MS));
            let Some(wait_ms) = crate::policy::retry_wait_ms(
                true,
                retries,
                self.retry.max_retries,
                millis(hint),
                millis(self.retry.max_wait),
            ) else {
                return Err(Error::RateLimited {
                    retries,
                    retry_after,
                });
            };
            retries += 1;
            tracing::debug!(retries, wait_ms, "linear rate-limited the call; retrying");
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;
        }
    }

    /// Send `op` and read its answer with [`Operation::extract`].
    ///
    /// # Errors
    ///
    /// See [`execute`](Self::execute) and [`Operation::extract`].
    pub async fn run<T: DeserializeOwned>(&self, op: &Operation) -> Result<T> {
        op.extract(self.execute(op).await?)
    }

    /// Run your own GraphQL document and return `data`.
    ///
    /// # Errors
    ///
    /// See [`execute`](Self::execute).
    pub async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        self.execute(&Operation::new(query, variables, "")).await
    }

    /// Run your own GraphQL document and deserialize `data` into `T`.
    ///
    /// # Errors
    ///
    /// See [`execute`](Self::execute). Also returns
    /// [`Error::UnexpectedResponse`] when `data` does not match `T`.
    pub async fn query<T: DeserializeOwned>(&self, query: &str, variables: Value) -> Result<T> {
        let data = self.graphql(query, variables).await?;
        serde_json::from_value(data)
            .map_err(|err| crate::graphql::unexpected(&format!("data does not match: {err}")))
    }

    /// The authenticated user. The health check uses it.
    ///
    /// # Errors
    ///
    /// See [`run`](Self::run).
    pub async fn viewer(&self) -> Result<Viewer> {
        self.run(&ops::viewer()).await
    }

    /// The workspace of the credential.
    ///
    /// # Errors
    ///
    /// See [`run`](Self::run).
    pub async fn organization(&self) -> Result<Organization> {
        self.run(&ops::organization()).await
    }

    /// One issue by UUID or identifier (`ENG-123`).
    ///
    /// # Errors
    ///
    /// See [`run`](Self::run). A missing issue is a [`Error::GraphQl`].
    pub async fn issue(&self, id: &str) -> Result<Issue> {
        self.run(&ops::issue(id)).await
    }

    /// One page of issues. See [`ops::issues`].
    ///
    /// # Errors
    ///
    /// See [`run`](Self::run).
    pub async fn issues(
        &self,
        filter: &Value,
        first: u32,
        after: Option<&str>,
    ) -> Result<Connection<Issue>> {
        self.run(&ops::issues(filter, first, after)).await
    }

    /// Create an issue.
    ///
    /// # Errors
    ///
    /// See [`run`](Self::run).
    pub async fn create_issue(&self, input: &IssueCreate) -> Result<Issue> {
        self.run(&ops::create_issue(input)).await
    }

    /// Update an issue.
    ///
    /// # Errors
    ///
    /// See [`run`](Self::run).
    pub async fn update_issue(&self, id: &str, input: &IssueUpdate) -> Result<Issue> {
        self.run(&ops::update_issue(id, input)).await
    }

    /// Create a comment. Give it a client id (see
    /// [`CommentCreate::with_stable_id`]) so that a retry can find it.
    ///
    /// # Errors
    ///
    /// See [`run`](Self::run).
    pub async fn create_comment(&self, input: &CommentCreate) -> Result<Comment> {
        self.run(&ops::create_comment(input)).await
    }

    /// Create the comment only when no comment has its id. Returns the
    /// comment and `true` when this call created it.
    ///
    /// Use it with [`CommentCreate::with_stable_id`]: a retry of the same
    /// work finds the earlier comment and posts nothing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] when the input has no id, and the
    /// errors of [`run`](Self::run).
    pub async fn create_comment_once(&self, input: &CommentCreate) -> Result<(Comment, bool)> {
        let id = input.id.as_deref().ok_or_else(|| {
            Error::InvalidRequest("create_comment_once needs a client id".to_owned())
        })?;
        if let Some(found) = self.find_comment(id).await? {
            return Ok((found, false));
        }
        Ok((self.create_comment(input).await?, true))
    }

    /// Find a comment by id. `None` when it does not exist.
    ///
    /// # Errors
    ///
    /// See [`run`](Self::run).
    pub async fn find_comment(&self, id: &str) -> Result<Option<Comment>> {
        let page: Connection<Comment> = self.run(&ops::find_comment(id)).await?;
        Ok(page.nodes.into_iter().next())
    }

    /// Send an agent activity. Returns its id.
    ///
    /// # Errors
    ///
    /// See [`run`](Self::run).
    pub async fn create_agent_activity(
        &self,
        session_id: &str,
        content: &ActivityContent,
    ) -> Result<String> {
        let created: Created = self
            .run(&ops::create_agent_activity(session_id, content, None))
            .await?;
        Ok(created.id)
    }

    /// Send an agent activity with a client id (UUID v4 format). A retry
    /// with the same id cannot post a second activity.
    ///
    /// # Errors
    ///
    /// See [`run`](Self::run).
    pub async fn create_agent_activity_with_id(
        &self,
        session_id: &str,
        content: &ActivityContent,
        id: &str,
    ) -> Result<String> {
        let created: Created = self
            .run(&ops::create_agent_activity(session_id, content, Some(id)))
            .await?;
        Ok(created.id)
    }

    /// Add an external URL to an agent session.
    ///
    /// # Errors
    ///
    /// See [`run`](Self::run).
    pub async fn add_agent_session_url(
        &self,
        session_id: &str,
        label: &str,
        url: &str,
    ) -> Result<()> {
        let _: Value = self
            .run(&ops::add_agent_session_url(session_id, label, url))
            .await?;
        Ok(())
    }

    /// Create a webhook. Needs the `admin` scope.
    ///
    /// # Errors
    ///
    /// See [`run`](Self::run).
    pub async fn create_webhook(&self, input: &WebhookCreate) -> Result<Webhook> {
        self.run(&ops::create_webhook(input)).await
    }

    /// Delete a webhook.
    ///
    /// # Errors
    ///
    /// See [`run`](Self::run).
    pub async fn delete_webhook(&self, id: &str) -> Result<()> {
        let _: Value = self.run(&ops::delete_webhook(id)).await?;
        Ok(())
    }
}

/// The wait until `X-RateLimit-Requests-Reset` (UTC epoch ms), if sent.
fn reset_wait(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let reset_ms: u64 = headers
        .get("x-ratelimit-requests-reset")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(millis)?;
    Some(Duration::from_millis(reset_ms.saturating_sub(now_ms)))
}

/// A duration in whole ms, saturated to `u64`.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Build the shared HTTP client: timeout, user agent, and no redirects.
///
/// # Errors
///
/// Returns [`Error::Http`] when the client does not build.
pub(crate) fn http_client() -> Result<reqwest::Client> {
    // No redirects: a redirect can carry the token or the client secret to
    // another host.
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("autumn-plugin-linear/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(Error::Http)
}

#[cfg(test)]
mod tests;
