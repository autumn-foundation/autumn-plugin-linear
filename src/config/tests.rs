#![allow(clippy::expect_used)]

use super::*;
use crate::error::ErrorKind;

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
    let map: std::collections::HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    move |name| map.get(name).cloned()
}

const FULL: &str = r#"
[linear]
client_id = "client-1"
app_url = "https://app.example.com/"
scopes = ["read", "write", "comments:create", "app:assignable"]
actor = "app"
after_install_path = "/welcome"
prompt_consent = false
webhook_tolerance_secs = 120
max_rate_limit_retries = 5
max_rate_limit_wait_secs = 30
"#;

const OAUTH_SECRETS: [(&str, &str); 2] = [
    ("LINEAR_CLIENT_SECRET", "cs"),
    ("LINEAR_WEBHOOK_SECRET", "ws"),
];

#[test]
fn defaults_are_the_documented_values() {
    let config = LinearConfig::default();
    assert_eq!(config.client_id, None);
    assert_eq!(config.app_url, None);
    assert_eq!(config.scopes, vec!["read".to_owned()]);
    assert_eq!(config.actor, Actor::User);
    assert_eq!(config.after_install_path, "/");
    assert!(config.prompt_consent);
    assert_eq!(config.webhook_tolerance_secs, 60);
    assert_eq!(config.max_rate_limit_retries, 3);
    assert_eq!(config.max_rate_limit_wait_secs, 10);
    assert!(!config.oauth_enabled());
}

#[test]
fn the_toml_section_sets_every_field() {
    let config = LinearConfig::from_toml_str(FULL).expect("parses");
    assert_eq!(config.client_id.as_deref(), Some("client-1"));
    assert_eq!(config.app_url.as_deref(), Some("https://app.example.com/"));
    assert_eq!(config.scopes.len(), 4);
    assert_eq!(config.actor, Actor::App);
    assert_eq!(config.after_install_path, "/welcome");
    assert!(!config.prompt_consent);
    assert_eq!(config.webhook_tolerance_secs, 120);
    assert_eq!(config.max_rate_limit_retries, 5);
    assert_eq!(config.max_rate_limit_wait_secs, 30);
    assert_eq!(
        config.redirect_uri().as_deref(),
        Some("https://app.example.com/linear/auth/callback")
    );
}

#[test]
fn a_missing_section_gives_defaults_and_scopes_take_csv() {
    let config = LinearConfig::from_toml_str("[other]\nx = 1\n").expect("parses");
    assert_eq!(config.scopes, vec!["read".to_owned()]);
    let config =
        LinearConfig::from_toml_str("[linear]\nscopes = \"read, write,,\"\n").expect("parses");
    assert_eq!(config.scopes, vec!["read".to_owned(), "write".to_owned()]);
}

#[test]
fn unknown_keys_and_bad_toml_fail() {
    for doc in [
        "[linear]\nclient = \"x\"\n",
        "[linear\n",
        "linear = 3\n",
        "[linear]\nactor = \"bot\"\n",
    ] {
        let err = LinearConfig::from_toml_str(doc).expect_err("refused");
        assert_eq!(err.kind(), ErrorKind::Config, "{doc}");
    }
}

#[test]
fn secret_keys_in_the_file_fail_closed() {
    for key in [
        "api_key",
        "client_secret",
        "webhook_secret",
        "token",
        "access_token",
        "secret",
        "signing_key",
        "Webhook-Secret",
    ] {
        let doc = format!("[linear]\n{key} = \"hunter2-value\"\n");
        let err = LinearConfig::from_toml_str(&doc).expect_err("refused");
        let text = err.to_string();
        assert!(text.contains(key), "{text}");
        assert!(!text.contains("hunter2-value"), "{text}");
    }
}

#[test]
fn env_overrides_the_file_and_carries_secrets() {
    let config = LinearConfig::from_sources(
        Some(FULL),
        env(&[
            ("LINEAR_CLIENT_ID", "client-2"),
            ("LINEAR_APP_URL", "https://other.example.com"),
            ("LINEAR_SCOPES", "read,issues:create"),
            ("LINEAR_ACTOR", "user"),
            ("LINEAR_AFTER_INSTALL_PATH", "/done"),
            ("LINEAR_PROMPT_CONSENT", "true"),
            ("LINEAR_WEBHOOK_TOLERANCE_SECS", "30"),
            ("LINEAR_MAX_RATE_LIMIT_RETRIES", "1"),
            ("LINEAR_MAX_RATE_LIMIT_WAIT_SECS", "2"),
            ("LINEAR_WEBHOOK_SECRET", "ws"),
            ("LINEAR_WEBHOOK_SECRET_PREVIOUS", "old-ws"),
            ("LINEAR_CLIENT_SECRET", "cs"),
            ("LINEAR_API_KEY", "lin_api_x"),
        ]),
    )
    .expect("loads");
    assert_eq!(config.client_id.as_deref(), Some("client-2"));
    assert_eq!(config.app_url.as_deref(), Some("https://other.example.com"));
    assert_eq!(
        config.scopes,
        vec!["read".to_owned(), "issues:create".to_owned()]
    );
    assert_eq!(config.actor, Actor::User);
    assert_eq!(config.after_install_path, "/done");
    assert!(config.prompt_consent);
    assert_eq!(config.webhook_tolerance_secs, 30);
    assert_eq!(config.max_rate_limit_retries, 1);
    assert_eq!(config.max_rate_limit_wait_secs, 2);
    assert!(config.webhook_secrets().expect("ok").is_some());
    assert_eq!(config.client_secret(), Some("cs"));
    assert_eq!(config.api_key(), Some("lin_api_x"));
}

#[test]
fn blank_env_values_count_as_unset() {
    let config = LinearConfig::from_sources(
        Some(FULL),
        env(&[
            ("LINEAR_CLIENT_ID", "  "),
            ("LINEAR_API_KEY", ""),
            ("LINEAR_WEBHOOK_SECRET", " "),
        ]),
    )
    .expect("loads");
    assert_eq!(config.client_id.as_deref(), Some("client-1"));
    assert_eq!(config.api_key(), None);
    assert!(config.webhook_secrets().expect("ok").is_none());
}

#[test]
fn bad_env_numbers_and_flags_fail() {
    for (name, value) in [
        ("LINEAR_WEBHOOK_TOLERANCE_SECS", "x"),
        ("LINEAR_MAX_RATE_LIMIT_RETRIES", "-1"),
        ("LINEAR_MAX_RATE_LIMIT_WAIT_SECS", "1.5"),
        ("LINEAR_PROMPT_CONSENT", "maybe"),
        ("LINEAR_ACTOR", "robot"),
    ] {
        let err = LinearConfig::from_sources(None, env(&[(name, value)])).expect_err("refused");
        assert_eq!(err.kind(), ErrorKind::Config, "{name}");
        assert!(err.to_string().contains(name), "{err}");
    }
}

#[test]
fn debug_redacts_secrets() {
    let config = LinearConfig::from_sources(
        None,
        env(&[
            ("LINEAR_WEBHOOK_SECRET", "wsecret-1"),
            ("LINEAR_WEBHOOK_SECRET_PREVIOUS", "wsecret-0"),
            ("LINEAR_CLIENT_SECRET", "csecret-1"),
            ("LINEAR_API_KEY", "lin_api_secret"),
        ]),
    )
    .expect("loads");
    let text = format!("{config:?}");
    for secret in ["wsecret-1", "wsecret-0", "csecret-1", "lin_api_secret"] {
        assert!(!text.contains(secret), "{text}");
    }
    assert!(text.contains("<redacted>"));
}

#[test]
fn an_api_key_only_config_validates() {
    let config = LinearConfig::from_sources(None, env(&[("LINEAR_API_KEY", "k")])).expect("loads");
    config.validate().expect("valid");
    assert_eq!(config.tolerance_ms(), 60_000);
}

#[test]
fn a_full_oauth_config_validates() {
    let config = LinearConfig::from_sources(Some(FULL), env(&OAUTH_SECRETS)).expect("loads");
    config.validate().expect("valid");
    assert!(config.oauth_enabled());
}

fn invalid(toml: &str, extra: &[(&str, &str)]) -> String {
    let mut pairs = OAUTH_SECRETS.to_vec();
    pairs.extend_from_slice(extra);
    let config = LinearConfig::from_sources(Some(toml), env(&pairs)).expect("loads");
    let err = config.validate().expect_err("invalid");
    assert_eq!(err.kind(), ErrorKind::Config);
    err.to_string()
}

#[test]
fn validate_checks_the_oauth_fields() {
    let base = "[linear]\nclient_id = \"c\"\napp_url = \"https://app.example.com\"\n";
    assert!(
        invalid("[linear]\napp_url = \"https://app.example.com\"\n", &[]).contains("client_id")
    );
    assert!(invalid(&format!("{base}scopes = []\n"), &[]).contains("scopes"));
    assert!(invalid(&format!("{base}scopes = [\"read write\"]\n"), &[]).contains("scope"));
    for url in [
        "http://app.example.com",
        "ftp://x",
        "not a url",
        "https://app.example.com/?q=1",
        "https://app.example.com/#f",
    ] {
        assert!(
            invalid(
                &format!("[linear]\nclient_id = \"c\"\napp_url = \"{url}\"\n"),
                &[]
            )
            .contains("app_url"),
            "{url}"
        );
    }
    // http is fine on a loopback host.
    let config = LinearConfig::from_sources(
        Some("[linear]\nclient_id = \"c\"\napp_url = \"http://localhost:3000\"\n"),
        env(&OAUTH_SECRETS),
    )
    .expect("loads");
    config.validate().expect("loopback http is valid");
    // OAuth needs the client secret.
    let config = LinearConfig::from_sources(Some(base), env(&[])).expect("loads");
    assert!(
        config
            .validate()
            .expect_err("no secret")
            .to_string()
            .contains("LINEAR_CLIENT_SECRET")
    );
}

#[test]
fn validate_bounds_numbers_and_paths() {
    assert!(
        invalid("[linear]\nwebhook_tolerance_secs = 0\n", &[]).contains("webhook_tolerance_secs")
    );
    assert!(
        invalid("[linear]\nwebhook_tolerance_secs = 601\n", &[]).contains("webhook_tolerance_secs")
    );
    assert!(
        invalid("[linear]\nmax_rate_limit_retries = 11\n", &[]).contains("max_rate_limit_retries")
    );
    assert!(
        invalid("[linear]\nmax_rate_limit_wait_secs = 301\n", &[])
            .contains("max_rate_limit_wait_secs")
    );
    for path in [
        "https://evil.example.com",
        "//evil.example.com",
        "/a\\b",
        "/a#b",
        "relative",
    ] {
        assert!(
            invalid(&format!("[linear]\nafter_install_path = \"{path}\"\n"), &[])
                .contains("after_install_path"),
            "{path}"
        );
    }
}

#[test]
fn derived_values_follow_the_fields() {
    let config = LinearConfig::from_toml_str(FULL).expect("parses");
    assert_eq!(config.tolerance_ms(), 120_000);
    let retry = config.retry_policy();
    assert_eq!(retry.max_retries, 5);
    assert_eq!(retry.max_wait, std::time::Duration::from_secs(30));
}
