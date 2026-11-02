//! The camel routes: into harvest, and out to San Mateo.

use camel_api::CamelError;
use camel_builder::{RouteBuilder, StepAccumulator};
use camel_component_direct::DirectComponent;
use camel_core::{CamelContext, RouteDefinition};
use harvest_camel::{CamelHandle, HarvestComponent, check_route};
use san_mateo::SanMateo;
use san_mateo_camel::SanMateoComponent;

use crate::BoxError;
use crate::auth::{CONNECTION, INTEGRATION};
use crate::settings::ENV;

/// The route into harvest.
pub(crate) const INTAKE: &str = "direct:linear-intake";
/// The route out to Linear.
pub(crate) const GRAPHQL: &str = "direct:linear-graphql";
/// The harvest stream.
pub(crate) const STREAM: &str = "linear-intake";
/// The header with the message id. Harvest uses it to find redeliveries.
pub(crate) const MESSAGE_ID: &str = "MessageId";

/// The routes. Do not add camel redelivery: harvest retries.
fn routes() -> Result<Vec<RouteDefinition>, CamelError> {
    // `ackTimeout` keeps the hand-off under Linear's 5 s.
    let intake = RouteBuilder::from(INTAKE)
        .route_id("linear-intake")
        .to(format!(
            "harvest:{STREAM}?idHeader={MESSAGE_ID}&ackTimeout=4000"
        ))
        .build()?;
    // The URI fixes the connection. `allowHeaderOverrides` stays `false`,
    // so a header cannot change the workspace.
    let graphql = RouteBuilder::from(GRAPHQL)
        .route_id("linear-graphql")
        .to(format!(
            "san-mateo:{INTEGRATION}?connectionId={CONNECTION}&method=POST&path=/graphql"
        ))
        .build()?;
    Ok(vec![intake, graphql])
}

/// Start camel with the harvest bridge, San Mateo, and the routes.
pub(crate) async fn start(
    bridge: HarvestComponent,
    san: SanMateo,
) -> Result<CamelContext, BoxError> {
    let mut camel = CamelContext::builder().build().await?;
    camel.register_component(DirectComponent::new());
    camel.register_component(bridge);
    camel.register_component(SanMateoComponent::new(san).environment(ENV));
    for route in routes()? {
        // Refuse error handlers that change an abandoned message into a
        // committed message.
        check_route(&route)?;
        camel.add_route_definition(route).await?;
    }
    camel.start().await?;
    Ok(camel)
}

/// A handle with the route `uri` resolved.
pub(crate) fn handle(camel: &CamelContext, uri: &str) -> Result<CamelHandle, BoxError> {
    let handle = CamelHandle::new();
    handle.resolve(camel, uri)?;
    Ok(handle)
}
