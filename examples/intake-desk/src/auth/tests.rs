#![allow(clippy::expect_used)]

use san_mateo::proxy::check_upstream;

use super::*;

const LINEAR_GRAPHQL: &str = "https://api.linear.app/graphql";

/// The built-in provider is what production uses. The example changes
/// only its base URL, so the two cannot drift.
#[test]
fn the_example_provider_is_the_built_in_one_with_a_local_base_url() {
    let built_in = Catalog::builtin()
        .expect("catalog")
        .require(INTEGRATION)
        .expect("linear provider");
    let base = built_in.proxy.as_ref().and_then(|p| p.base_url.as_deref());
    assert_eq!(base, Some("https://api.linear.app"));

    let example = catalog("http://127.0.0.1:9")
        .expect("catalog")
        .require(INTEGRATION)
        .expect("linear provider");
    let mut expected = (*built_in).clone();
    if let Some(proxy) = expected.proxy.as_mut() {
        proxy.base_url = Some("http://127.0.0.1:9".to_owned());
    }
    assert_eq!(*example, expected);
}

/// The default proxy settings accept the real endpoint. They refuse the
/// local mock, so the example must allow it.
#[test]
fn default_settings_accept_linear_and_refuse_the_mock() {
    let strict = ProxySettings::default();
    assert!(check_upstream(LINEAR_GRAPHQL, None, false, &strict).is_ok());
    assert!(check_upstream("http://127.0.0.1:9/graphql", None, false, &strict).is_err());
    assert!(check_upstream("http://127.0.0.1:9/graphql", None, false, &test_settings()).is_ok());
}
