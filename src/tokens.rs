//! Token storage for OAuth installs, one token per workspace.
//!
//! [`TokenStore`] is the storage trait. [`InMemoryTokenStore`] is the
//! default. It loses data on a restart and does not share data across
//! replicas: bring your own store for production.
//!
//! Plugin code writes tokens only through the per-workspace lock in
//! `plugin::Keeper`. App code reads them with
//! [`LinearRuntime::token_store`](crate::plugin::LinearRuntime::token_store).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::types::OrganizationId;

/// The future a store method returns.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// The OAuth token of one workspace.
///
/// `Serialize` writes both tokens in clear text. Encrypt them at rest in
/// your store. `Debug` redacts them.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceToken {
    /// The access token.
    pub access_token: String,
    /// The refresh token, if Linear sent one.
    pub refresh_token: Option<String>,
    /// When the access token expires (Unix ms), if known.
    pub expires_at_ms: Option<u64>,
    /// The granted scopes, as Linear sent them.
    pub scope: String,
    /// When the install happened (Unix ms).
    pub installed_at_ms: Option<u64>,
}

impl std::fmt::Debug for WorkspaceToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceToken")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at_ms", &self.expires_at_ms)
            .field("scope", &self.scope)
            .field("installed_at_ms", &self.installed_at_ms)
            .finish()
    }
}

impl WorkspaceToken {
    /// Make a token.
    #[must_use]
    pub fn new(
        access_token: impl Into<String>,
        refresh_token: Option<String>,
        expires_at_ms: Option<u64>,
        scope: impl Into<String>,
    ) -> Self {
        Self {
            access_token: access_token.into(),
            refresh_token,
            expires_at_ms,
            scope: scope.into(),
            installed_at_ms: None,
        }
    }

    /// True when the token expires within `margin_ms` of `now_ms`. A token
    /// with no expiry never needs a refresh.
    #[must_use]
    pub fn needs_refresh(&self, now_ms: u64, margin_ms: u64) -> bool {
        self.expires_at_ms
            .is_some_and(|at| crate::policy::needs_refresh(now_ms, at, margin_ms))
    }

    /// True when the token is expired at `now_ms`.
    #[must_use]
    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms
            .is_some_and(|at| crate::policy::needs_refresh(now_ms, at, 0))
    }

    /// The granted scopes. Linear separates them with spaces or commas.
    #[must_use]
    pub fn scopes(&self) -> Vec<&str> {
        self.scope
            .split([' ', ','])
            .filter(|scope| !scope.is_empty())
            .collect()
    }
}

/// Storage for workspace tokens.
pub trait TokenStore: Send + Sync + 'static {
    /// Read the token of `org`.
    fn get<'a>(&'a self, org: &'a OrganizationId) -> StoreFuture<'a, Option<WorkspaceToken>>;
    /// Write the token of `org`.
    fn put<'a>(&'a self, org: &'a OrganizationId, token: WorkspaceToken) -> StoreFuture<'a, ()>;
    /// Delete the token of `org`. Deleting a missing token is not an error.
    fn delete<'a>(&'a self, org: &'a OrganizationId) -> StoreFuture<'a, ()>;

    /// Delete the token of `org` only when the store holds `seen`. Returns
    /// `true` when it deleted the token.
    ///
    /// The default reads, compares, and deletes in three steps. A store
    /// that replicas share must override it with one atomic step (for
    /// example `DELETE ... WHERE access_token = $seen`). Else a replica can
    /// delete a pair that another replica just stored.
    fn delete_if<'a>(
        &'a self,
        org: &'a OrganizationId,
        seen: &'a WorkspaceToken,
    ) -> StoreFuture<'a, bool> {
        Box::pin(async move {
            if self.get(org).await?.as_ref() == Some(seen) {
                self.delete(org).await?;
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }
}

/// A process-local [`TokenStore`].
#[derive(Debug, Default)]
pub struct InMemoryTokenStore {
    tokens: RwLock<HashMap<OrganizationId, WorkspaceToken>>,
}

impl InMemoryTokenStore {
    /// Make an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// The error for a poisoned lock.
fn poisoned() -> Error {
    Error::Store("token store lock is poisoned".to_owned())
}

impl TokenStore for InMemoryTokenStore {
    fn get<'a>(&'a self, org: &'a OrganizationId) -> StoreFuture<'a, Option<WorkspaceToken>> {
        let result = self
            .tokens
            .read()
            .map(|tokens| tokens.get(org).cloned())
            .map_err(|_| poisoned());
        Box::pin(async move { result })
    }

    fn put<'a>(&'a self, org: &'a OrganizationId, token: WorkspaceToken) -> StoreFuture<'a, ()> {
        let result = self
            .tokens
            .write()
            .map(|mut tokens| {
                tokens.insert(org.clone(), token);
            })
            .map_err(|_| poisoned());
        Box::pin(async move { result })
    }

    fn delete<'a>(&'a self, org: &'a OrganizationId) -> StoreFuture<'a, ()> {
        let result = self
            .tokens
            .write()
            .map(|mut tokens| {
                tokens.remove(org);
            })
            .map_err(|_| poisoned());
        Box::pin(async move { result })
    }

    fn delete_if<'a>(
        &'a self,
        org: &'a OrganizationId,
        seen: &'a WorkspaceToken,
    ) -> StoreFuture<'a, bool> {
        // One write lock covers the compare and the delete.
        let result = self
            .tokens
            .write()
            .map(|mut tokens| {
                let same = tokens.get(org) == Some(seen);
                if same {
                    tokens.remove(org);
                }
                same
            })
            .map_err(|_| poisoned());
        Box::pin(async move { result })
    }
}

#[cfg(test)]
mod tests;
