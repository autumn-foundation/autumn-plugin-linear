//! `autumn-plugin-linear`: build Linear apps on Autumn.
//!
//! - Signed webhooks, routed by type and action, with a replay window and
//!   a duplicate guard.
//! - OAuth install (PKCE, `actor=app`) with token refresh.
//! - A GraphQL client that retries rate-limited calls, and transport-free
//!   operations for other carriers.
//!
//! ```no_run
//! use autumn_plugin_linear::{LinearPlugin, WebhookContext};
//! use autumn_web::prelude::*;
//!
//! async fn issue_created(ctx: WebhookContext) -> AutumnResult<()> {
//!     let issue = ctx.event.issue().map_err(|e| e.to_autumn_error())?;
//!     tracing::info!(org = %ctx.event.organization_id, issue = ?issue.map(|i| i.id), "issue created");
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
//! ## Modules
//!
//! | Module | Purpose |
//! |---|---|
//! | [`config`] | `[linear]` config, `LINEAR_*` env, builder closures |
//! | [`types`] | [`OrganizationId`], [`EventType`], [`Action`], [`DeliveryId`], [`Actor`] |
//! | [`policy`] | The verified core: replay window, refresh, retry, claim state |
//! | [`verify`] | Webhook HMAC check |
//! | [`event`] | [`WebhookEvent`] and typed views |
//! | [`webhooks`] | The webhook route, extractor, and handler registry |
//! | [`delivery`] | Duplicate-delivery guard |
//! | [`oauth`] | Install and callback routes, code exchange, refresh |
//! | [`tokens`] | Token storage |
//! | [`graphql`] | Transport-free operations and the response parser |
//! | [`client`] | [`LinearClient`] over `reqwest` |
//! | [`health`] | Health check |
//! | [`plugin`] | [`LinearPlugin`], [`LinearRuntime`], [`Linear`] |
//! | [`testing`] | Signed event fixtures for app tests |
//! | [`error`] | [`Error`], [`ErrorKind`], HTTP status mapping |

#![forbid(unsafe_code)]

pub mod client;
pub mod config;
pub mod delivery;
pub mod error;
pub mod event;
pub mod graphql;
pub mod health;
pub mod oauth;
pub mod plugin;
pub mod policy;
pub mod testing;
pub mod tokens;
pub mod types;
pub mod verify;
pub mod webhooks;

#[cfg(test)]
mod test_support;

pub use client::{Credential, LinearClient, RetryPolicy};
pub use config::LinearConfig;
pub use delivery::{Claim, DeliveryStore, InMemoryDeliveryStore};
pub use error::{Error, ErrorKind, Result};
pub use event::{AgentSessionEvent, CommentData, IssueData, WebhookEvent};
pub use graphql::{ActivityContent, CommentCreate, IssueCreate, IssueUpdate, Operation, ops};
pub use health::LinearHealth;
pub use plugin::{InstallContext, Linear, LinearPlugin, LinearRuntime};
pub use tokens::{InMemoryTokenStore, TokenStore, WorkspaceToken};
pub use types::{Action, Actor, DeliveryId, EventType, OrganizationId};
pub use verify::{SecretSet, sign, verify_signature};
pub use webhooks::{VerifiedWebhook, WebhookContext, WebhookOutcome, WebhookRegistry};
