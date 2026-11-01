//! Health check for API-key apps.
//!
//! [`LinearHealth`] implements Autumn's [`HealthIndicator`]. It calls
//! [`LinearClient::viewer`] and reports `Up` with the user name, or `Down`
//! with the error. The plugin registers it as `"linear"` in the
//! health-only group (`/actuator/health`, not `/ready`) when
//! `LINEAR_API_KEY` is set.

use std::collections::HashMap;
use std::sync::Arc;

use autumn_web::actuator::{HealthCheckOutput, HealthIndicator, HealthStatus};

use crate::client::LinearClient;

/// Health indicator for one credential.
#[derive(Debug, Clone)]
pub struct LinearHealth {
    client: LinearClient,
}

impl LinearHealth {
    /// Build the indicator around a client.
    #[must_use]
    pub const fn new(client: LinearClient) -> Self {
        Self { client }
    }
}

impl HealthIndicator for LinearHealth {
    fn check(&self) -> futures::future::BoxFuture<'_, HealthCheckOutput> {
        Box::pin(async move {
            let (status, key, value) = match self.client.viewer().await {
                Ok(viewer) => (HealthStatus::Up, "viewer", viewer.name.unwrap_or(viewer.id)),
                Err(err) => (HealthStatus::Down, "error", err.to_string()),
            };
            let mut details = HashMap::new();
            details.insert(key.to_owned(), serde_json::Value::String(value));
            HealthCheckOutput { status, details }
        })
    }
}

/// Build the shared indicator handle the plugin registers.
#[must_use]
pub fn indicator(client: &LinearClient) -> Arc<LinearHealth> {
    Arc::new(LinearHealth::new(client.clone()))
}

#[cfg(test)]
mod tests;
