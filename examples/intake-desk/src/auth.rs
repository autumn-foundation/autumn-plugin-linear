//! San Mateo, the auth part: it keeps the Linear OAuth token and adds it
//! to each call.

use san_mateo::clock::{Entropy, OsEntropy};
use san_mateo::credentials::OAuth2Credentials;
use san_mateo::{
    Catalog, Credentials, EnvironmentConfig, ImportConnection, KeyRing, NewIntegration,
    ProxySettings, SanMateo, Secret,
};
use serde_json::{Map, Value};

use crate::BoxError;
use crate::settings::ENV;

/// The San Mateo integration key.
pub(crate) const INTEGRATION: &str = "linear";
/// The San Mateo connection of the Linear workspace.
pub(crate) const CONNECTION: &str = "acme-workspace";
/// The OAuth token that San Mateo keeps for the workspace.
pub(crate) const ACCESS_TOKEN: &str = "lin-oauth-example";

/// The service with the Linear integration and one workspace connection.
/// It calls the Linear API at `linear_url` over real HTTP.
///
/// For production: use `Catalog::builtin()` (its `linear` provider calls
/// `https://api.linear.app`), keep the default `ProxySettings`, use
/// `PgStore`, and connect the workspace with a connect session.
pub(crate) async fn service(linear_url: &str) -> Result<SanMateo, BoxError> {
    let mut key = [0_u8; 32];
    OsEntropy.fill(&mut key);
    let san = SanMateo::builder()
        .environment(EnvironmentConfig::new(
            ENV,
            format!("sk_example_{}", OsEntropy.hex(16)),
        ))
        .key_ring(KeyRing::new("k1", key)?)
        .catalog(catalog(linear_url)?)
        .proxy_settings(test_settings())
        .build()?;
    san.create_integration(
        ENV,
        NewIntegration {
            unique_key: INTEGRATION.to_owned(),
            provider: INTEGRATION.to_owned(),
            client_id: Some("linear-client".to_owned()),
            client_secret: Some(Secret::new(OsEntropy.hex(16))),
            ..NewIntegration::default()
        },
    )
    .await?;
    // A token with no expiry: San Mateo does not refresh it here. A real
    // connection refreshes before it expires.
    san.import_connection(
        ENV,
        ImportConnection {
            connection_id: Some(CONNECTION.to_owned()),
            provider_config_key: INTEGRATION.to_owned(),
            credentials: Credentials::OAuth2(OAuth2Credentials {
                access_token: Secret::new(ACCESS_TOKEN),
                refresh_token: None,
                expires_at: None,
                raw: Value::Null,
            }),
            connection_config: Map::new(),
            metadata: Map::new(),
            tags: san_mateo::Tags::new(),
            end_user: None,
            webhook_url_override: None,
        },
    )
    .await?;
    Ok(san)
}

/// The built-in `linear` provider, with its proxy base URL changed to
/// `base_url`. Every other field stays as San Mateo ships it.
fn catalog(base_url: &str) -> Result<Catalog, BoxError> {
    let mut linear = (*Catalog::builtin()?.require(INTEGRATION)?).clone();
    linear.proxy.get_or_insert_default().base_url = Some(base_url.to_owned());
    // JSON is valid YAML, so the catalog parser reads it. The parser reads
    // a `null` field as an empty section, so leave the unset fields out.
    let mut document = serde_json::json!({ INTEGRATION: linear });
    drop_nulls(&mut document);
    Ok(Catalog::from_yaml_str(&document.to_string())?)
}

/// Remove every `null` field from `value`, at every depth.
fn drop_nulls(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, field| !field.is_null());
            map.values_mut().for_each(drop_nulls);
        }
        Value::Array(items) => items.iter_mut().for_each(drop_nulls),
        _ => {}
    }
}

/// The mock is on loopback over http. Only tests and local development
/// may allow that.
fn test_settings() -> ProxySettings {
    ProxySettings {
        allow_private_hosts: true,
        allow_plain_http: true,
        ..ProxySettings::default()
    }
}

#[cfg(test)]
mod tests;
