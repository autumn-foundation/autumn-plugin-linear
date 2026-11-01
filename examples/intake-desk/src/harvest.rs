//! harvest: the worker and the connector that starts one workflow per
//! issue.

use std::sync::Arc;
use std::time::Duration;

use autumn_harvest::execution::resolve_execution_id_by_workflow_id;
use autumn_harvest::handle::WorkflowHandleClient;
use autumn_harvest::telemetry::NoOpMetrics;
use autumn_harvest::worker::DbPool;
use autumn_harvest_plugin::api::HarvestApiState;
use autumn_harvest_plugin::connector::{ConnectorRuntime, MappedMessage, SourceBinding};
use autumn_harvest_plugin::{
    HarvestDatabaseConfig, HarvestMode, HarvestRunner, HarvestRunnerResources, HarvestRuntimeConfig,
};
use camel_core::CamelContext;
use harvest_camel::CamelSource;
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::BoxError;
use crate::camel::{GRAPHQL, STREAM};
use crate::workflow::{IssueRef, WORKFLOW, activity_infos, workflow_id, workflow_info};

/// A running worker and connector.
pub(crate) struct Harvest {
    runner: HarvestRunner,
    cancel: CancellationToken,
    connector: JoinHandle<()>,
    pool: DbPool,
    url: String,
}

impl Harvest {
    /// Start the worker and the connector on the stream of `source`.
    pub(crate) async fn start(
        camel: &CamelContext,
        pool: &DbPool,
        url: &str,
        source: Arc<CamelSource>,
    ) -> Result<Self, BoxError> {
        // Activities get this handle. Resolve their route before they run.
        let activities = crate::camel::handle(camel, GRAPHQL)?;
        let runner = HarvestRunner::start(
            autumn_harvest::HarvestBuilder::new()
                .workflows(vec![workflow_info()])
                .activities(activity_infos())
                .state(activities)
                .build(),
            &HarvestRuntimeConfig {
                mode: HarvestMode::External,
                database: HarvestDatabaseConfig {
                    url: Some(url.to_owned()),
                },
                ..HarvestRuntimeConfig::default()
            },
            HarvestRunnerResources::new(pool.clone()),
        )
        .await
        .map_err(|e| e.to_string())?;

        // One workflow per issue: the message maps to its workflow id.
        let api = HarvestApiState::new();
        api.install_storage_pool(runner.storage_pool());
        api.install(runner.api_runtime());
        let binding = SourceBinding::starts("linear-intake", STREAM, WORKFLOW).map_json(
            |_ctx, message: IssueRef| {
                let id = workflow_id(&message.organization_id, &message.issue_id);
                let payload = serde_json::to_value(&message).map_err(|e| e.to_string())?;
                Ok::<_, String>(MappedMessage::new(id, payload))
            },
        );
        let connector = ConnectorRuntime::for_binding(
            Arc::new(binding),
            source,
            api,
            Arc::new(NoOpMetrics),
            Some(&workflow_info()),
        );
        let cancel = CancellationToken::new();
        let connector = tokio::spawn({
            let cancel = cancel.clone();
            async move { connector.run(cancel).await }
        });
        Ok(Self {
            runner,
            cancel,
            connector,
            pool: pool.clone(),
            url: url.to_owned(),
        })
    }

    /// Wait up to `timeout` for the result of the workflow `workflow_id`.
    pub(crate) async fn result(
        &self,
        workflow_id: &str,
        timeout: Duration,
    ) -> Result<Value, BoxError> {
        let mut conn = self.pool.get().await?;
        let execution = resolve_execution_id_by_workflow_id(&mut conn, WORKFLOW, workflow_id)
            .await?
            .ok_or("the workflow did not start")?;
        drop(conn);
        let output = WorkflowHandleClient::single(self.pool.clone(), &self.url)
            .handle(execution.exec_id)
            .result_raw_with_timeout(timeout)
            .await
            .map_err(|e| format!("workflow {workflow_id}: {e}"))?;
        Ok(output)
    }

    /// Stop the connector, then the worker. A connector panic must not skip
    /// the worker stop.
    pub(crate) async fn stop(self) -> Result<(), BoxError> {
        self.cancel.cancel();
        let joined = self.connector.await;
        self.runner.stop().await;
        joined?;
        Ok(())
    }
}
