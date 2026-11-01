//! Layered configuration for the Linear plugin.
//!
//! [`LinearConfig`] reads the `[linear]` section of `autumn.toml`, then the
//! `LINEAR_*` environment variables, then the builder closures from
//! [`LinearPlugin::configure`](crate::plugin::LinearPlugin::configure). A
//! later layer wins.
//!
//! Secrets come only from the environment (or the lookup that
//! [`LinearPlugin::env_lookup`](crate::plugin::LinearPlugin::env_lookup)
//! sets):
//!
//! | Variable | Holds |
//! |---|---|
//! | `LINEAR_WEBHOOK_SECRET` | The webhook signing secret. Webhook handlers need it. |
//! | `LINEAR_WEBHOOK_SECRET_PREVIOUS` | The old signing secret during rotation. |
//! | `LINEAR_CLIENT_SECRET` | The OAuth client secret. OAuth needs it. |
//! | `LINEAR_API_KEY` | A personal API key (one-workspace apps). |
//!
//! A secret key in `[linear]` stops config loading.
//!
//! ```toml
//! [linear]
//! client_id = "OAuth client id"
//! app_url = "https://app.example.com"   # turns OAuth on
//! scopes = ["read", "write"]
//! actor = "app"                          # or "user"
//! after_install_path = "/"
//! prompt_consent = true
//! webhook_tolerance_secs = 60
//! max_rate_limit_retries = 3
//! max_rate_limit_wait_secs = 10
//! ```

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use crate::client::RetryPolicy;
use crate::error::{Error, Result};
use crate::types::Actor;
use crate::verify::SecretSet;

/// Name of the `autumn.toml` section this plugin reads.
pub const CONFIG_SECTION: &str = "linear";
/// Environment variable for the webhook signing secret.
pub const WEBHOOK_SECRET_ENV: &str = "LINEAR_WEBHOOK_SECRET";
/// Environment variable for the previous webhook signing secret.
pub const PREVIOUS_WEBHOOK_SECRET_ENV: &str = "LINEAR_WEBHOOK_SECRET_PREVIOUS";
/// Environment variable for the OAuth client secret.
pub const CLIENT_SECRET_ENV: &str = "LINEAR_CLIENT_SECRET";
/// Environment variable for a personal API key.
pub const API_KEY_ENV: &str = "LINEAR_API_KEY";

/// The largest `max_rate_limit_retries`.
const MAX_RETRIES: u32 = crate::policy::MAX_RETRIES;
/// The largest `max_rate_limit_wait_secs`.
const MAX_WAIT_SECS: u64 = 300;
/// The largest `webhook_tolerance_secs`.
const MAX_TOLERANCE_SECS: u64 = crate::policy::MAX_TOLERANCE_MS / 1000;

/// Configuration for the Linear plugin.
///
/// `Debug` redacts the secrets. App code cannot read them:
///
/// ```compile_fail
/// let config = autumn_plugin_linear::LinearConfig::default();
/// let _ = config.api_key();
/// ```
#[derive(Clone)]
pub struct LinearConfig {
    /// The OAuth client id. OAuth needs it.
    pub client_id: Option<String>,
    /// The public base URL of the app. Setting it turns OAuth on.
    pub app_url: Option<String>,
    /// The scopes OAuth asks for.
    pub scopes: Vec<String>,
    /// Who acts with the OAuth token: the user or the app.
    pub actor: Actor,
    /// The local path the browser goes to after an install.
    pub after_install_path: String,
    /// Send `prompt=consent`, so that a user can install in a second workspace.
    pub prompt_consent: bool,
    /// The replay window for the signed webhook time, in seconds.
    pub webhook_tolerance_secs: u64,
    /// How many times a rate-limited call retries.
    pub max_rate_limit_retries: u32,
    /// The longest wait before one retry, in seconds. A longer
    /// `Retry-After` stops the retries.
    pub max_rate_limit_wait_secs: u64,
    webhook_secret: Option<String>,
    previous_webhook_secret: Option<String>,
    client_secret: Option<String>,
    api_key: Option<String>,
}

impl std::fmt::Debug for LinearConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redact = |value: &Option<String>| value.as_ref().map(|_| "<redacted>");
        f.debug_struct("LinearConfig")
            .field("client_id", &self.client_id)
            .field("app_url", &self.app_url)
            .field("scopes", &self.scopes)
            .field("actor", &self.actor)
            .field("after_install_path", &self.after_install_path)
            .field("prompt_consent", &self.prompt_consent)
            .field("webhook_tolerance_secs", &self.webhook_tolerance_secs)
            .field("max_rate_limit_retries", &self.max_rate_limit_retries)
            .field("max_rate_limit_wait_secs", &self.max_rate_limit_wait_secs)
            .field("webhook_secret", &redact(&self.webhook_secret))
            .field(
                "previous_webhook_secret",
                &redact(&self.previous_webhook_secret),
            )
            .field("client_secret", &redact(&self.client_secret))
            .field("api_key", &redact(&self.api_key))
            .finish()
    }
}

impl Default for LinearConfig {
    fn default() -> Self {
        Self {
            client_id: None,
            app_url: None,
            scopes: vec!["read".to_owned()],
            actor: Actor::User,
            after_install_path: "/".to_owned(),
            prompt_consent: true,
            webhook_tolerance_secs: crate::policy::DEFAULT_TOLERANCE_MS / 1000,
            max_rate_limit_retries: 3,
            max_rate_limit_wait_secs: 10,
            webhook_secret: None,
            previous_webhook_secret: None,
            client_secret: None,
            api_key: None,
        }
    }
}

/// The raw `[linear]` table. Every field is optional.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    client_id: Option<String>,
    app_url: Option<String>,
    scopes: Option<Scopes>,
    actor: Option<String>,
    after_install_path: Option<String>,
    prompt_consent: Option<bool>,
    webhook_tolerance_secs: Option<u64>,
    max_rate_limit_retries: Option<u32>,
    max_rate_limit_wait_secs: Option<u64>,
}

/// Scopes as a TOML array or as one comma-separated string.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Scopes {
    List(Vec<String>),
    Csv(String),
}

impl LinearConfig {
    /// Read `autumn.toml` and the process environment. Does not validate.
    ///
    /// # Errors
    ///
    /// See [`load_with`](Self::load_with).
    pub fn load() -> Result<Self> {
        Self::load_with(|name| std::env::var(name).ok())
    }

    /// Read `autumn.toml` and the variables from `env`. Does not validate.
    ///
    /// The loader reads the file from `AUTUMN_MANIFEST_DIR` first, then from
    /// the current directory. A missing file or section is not an error.
    /// The loader reads only `autumn.toml`: it does not apply Autumn profile
    /// layering. Use the `LINEAR_*` variables for values that change per
    /// profile.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when the file is not valid TOML, the
    /// section has a secret or an unknown key, or an env value is bad.
    pub fn load_with(env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let path = config_file_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                return Err(Error::Config(format!(
                    "cannot read {}: {err}",
                    path.display()
                )));
            }
        };
        Self::from_sources(text.as_deref(), env)
    }

    /// Build the config from a TOML document and an environment lookup.
    /// Does not validate. Use it in tests: pass a closure, not the process
    /// environment.
    ///
    /// # Errors
    ///
    /// See [`load_with`](Self::load_with).
    pub fn from_sources(
        toml_text: Option<&str>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let mut config = match toml_text {
            Some(text) => Self::from_toml_str(text)?,
            None => Self::default(),
        };
        config.apply_env_with(env)?;
        Ok(config)
    }

    /// Parse the `[linear]` table of a TOML document. Does not validate.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when the TOML is not valid, the section is
    /// not a table, or it has a secret or an unknown key.
    pub fn from_toml_str(document: &str) -> Result<Self> {
        let table: toml::Table = document
            .parse()
            .map_err(|err| Error::Config(format!("invalid autumn.toml: {err}")))?;
        let Some(section) = table.get(CONFIG_SECTION) else {
            return Ok(Self::default());
        };
        let section = section
            .as_table()
            .ok_or_else(|| Error::Config("[linear] must be a table".to_owned()))?;
        // Fail closed on secrets before reading any other key. The error
        // names the key, never the value.
        if let Some(key) = section.keys().find(|key| is_secret_key(key)) {
            return Err(Error::Config(format!(
                "[linear] {key} is not allowed: secrets come only from the \
                 {WEBHOOK_SECRET_ENV}, {PREVIOUS_WEBHOOK_SECRET_ENV}, {CLIENT_SECRET_ENV}, \
                 and {API_KEY_ENV} environment variables"
            )));
        }
        let raw: RawConfig = section
            .clone()
            .try_into()
            .map_err(|err| Error::Config(format!("invalid [linear] section: {err}")))?;
        let defaults = Self::default();
        Ok(Self {
            client_id: raw.client_id,
            app_url: raw.app_url,
            scopes: match raw.scopes {
                Some(Scopes::List(list)) => list,
                Some(Scopes::Csv(csv)) => split_csv(&csv),
                None => defaults.scopes,
            },
            actor: raw
                .actor
                .as_deref()
                .map(Actor::parse)
                .transpose()?
                .unwrap_or(defaults.actor),
            after_install_path: raw
                .after_install_path
                .unwrap_or(defaults.after_install_path),
            prompt_consent: raw.prompt_consent.unwrap_or(defaults.prompt_consent),
            webhook_tolerance_secs: raw
                .webhook_tolerance_secs
                .unwrap_or(defaults.webhook_tolerance_secs),
            max_rate_limit_retries: raw
                .max_rate_limit_retries
                .unwrap_or(defaults.max_rate_limit_retries),
            max_rate_limit_wait_secs: raw
                .max_rate_limit_wait_secs
                .unwrap_or(defaults.max_rate_limit_wait_secs),
            webhook_secret: None,
            previous_webhook_secret: None,
            client_secret: None,
            api_key: None,
        })
    }

    /// Apply the `LINEAR_*` variables from `get`. Blank values count as
    /// unset.
    ///
    /// Reads `LINEAR_CLIENT_ID`, `LINEAR_APP_URL`, `LINEAR_SCOPES`
    /// (comma-separated), `LINEAR_ACTOR`, `LINEAR_AFTER_INSTALL_PATH`,
    /// `LINEAR_PROMPT_CONSENT`, `LINEAR_WEBHOOK_TOLERANCE_SECS`,
    /// `LINEAR_MAX_RATE_LIMIT_RETRIES`, `LINEAR_MAX_RATE_LIMIT_WAIT_SECS`,
    /// and the four secret variables.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when a number, a flag, or the actor is bad.
    /// The error names the variable.
    pub fn apply_env_with(&mut self, get: impl Fn(&str) -> Option<String>) -> Result<()> {
        let get = |name: &str| get(name).filter(|value| !value.trim().is_empty());
        let set = |slot: &mut Option<String>, name: &str| {
            if let Some(value) = get(name) {
                *slot = Some(value);
            }
        };
        set(&mut self.client_id, "LINEAR_CLIENT_ID");
        set(&mut self.app_url, "LINEAR_APP_URL");
        set(&mut self.webhook_secret, WEBHOOK_SECRET_ENV);
        set(
            &mut self.previous_webhook_secret,
            PREVIOUS_WEBHOOK_SECRET_ENV,
        );
        set(&mut self.client_secret, CLIENT_SECRET_ENV);
        set(&mut self.api_key, API_KEY_ENV);
        if let Some(value) = get("LINEAR_SCOPES") {
            self.scopes = split_csv(&value);
        }
        if let Some(value) = get("LINEAR_AFTER_INSTALL_PATH") {
            self.after_install_path = value;
        }
        if let Some(value) = get("LINEAR_ACTOR") {
            self.actor = Actor::parse(&value)
                .map_err(|_| Error::Config("LINEAR_ACTOR must be user or app".to_owned()))?;
        }
        if let Some(value) = get("LINEAR_PROMPT_CONSENT") {
            self.prompt_consent = match value.trim() {
                "true" | "1" => true,
                "false" | "0" => false,
                _ => {
                    return Err(Error::Config(
                        "LINEAR_PROMPT_CONSENT must be true or false".to_owned(),
                    ));
                }
            };
        }
        if let Some(value) = get("LINEAR_WEBHOOK_TOLERANCE_SECS") {
            self.webhook_tolerance_secs = number(&value, "LINEAR_WEBHOOK_TOLERANCE_SECS")?;
        }
        if let Some(value) = get("LINEAR_MAX_RATE_LIMIT_RETRIES") {
            self.max_rate_limit_retries = number(&value, "LINEAR_MAX_RATE_LIMIT_RETRIES")?;
        }
        if let Some(value) = get("LINEAR_MAX_RATE_LIMIT_WAIT_SECS") {
            self.max_rate_limit_wait_secs = number(&value, "LINEAR_MAX_RATE_LIMIT_WAIT_SECS")?;
        }
        Ok(())
    }

    /// Check that the config is complete and well-formed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] for the first problem found.
    pub fn validate(&self) -> Result<()> {
        self.webhook_secrets()?;
        if self.webhook_tolerance_secs == 0 || self.webhook_tolerance_secs > MAX_TOLERANCE_SECS {
            return Err(Error::Config(format!(
                "webhook_tolerance_secs must be 1 to {MAX_TOLERANCE_SECS}"
            )));
        }
        if self.max_rate_limit_retries > MAX_RETRIES {
            return Err(Error::Config(format!(
                "max_rate_limit_retries must be at most {MAX_RETRIES}"
            )));
        }
        if self.max_rate_limit_wait_secs > MAX_WAIT_SECS {
            return Err(Error::Config(format!(
                "max_rate_limit_wait_secs must be at most {MAX_WAIT_SECS}"
            )));
        }
        validate_local_path(&self.after_install_path)?;
        if let Some(app_url) = &self.app_url {
            validate_app_url(app_url)?;
            if self
                .client_id
                .as_deref()
                .is_none_or(|id| id.trim().is_empty())
            {
                return Err(Error::Config("OAuth needs [linear] client_id".to_owned()));
            }
            if self.client_secret.is_none() {
                return Err(Error::Config(format!(
                    "OAuth needs the client secret; set the {CLIENT_SECRET_ENV} environment variable"
                )));
            }
            if self.scopes.is_empty() {
                return Err(Error::Config("OAuth needs [linear] scopes".to_owned()));
            }
            if let Some(scope) = self.scopes.iter().find(|scope| !is_scope(scope)) {
                return Err(Error::Config(format!(
                    "scope {scope:?} must be 1 to 64 letters, digits, ':', or '_'"
                )));
            }
        }
        Ok(())
    }

    /// True when OAuth is on (`app_url` is set).
    #[must_use]
    pub const fn oauth_enabled(&self) -> bool {
        self.app_url.is_some()
    }

    /// The replay window in ms.
    #[must_use]
    pub const fn tolerance_ms(&self) -> u64 {
        self.webhook_tolerance_secs.saturating_mul(1000)
    }

    /// The retry policy for GraphQL calls.
    #[must_use]
    pub const fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy::new(
            self.max_rate_limit_retries,
            Duration::from_secs(self.max_rate_limit_wait_secs),
        )
    }

    /// The OAuth redirect URI: `{app_url}/linear/auth/callback`.
    #[must_use]
    pub fn redirect_uri(&self) -> Option<String> {
        self.app_url.as_deref().map(|url| {
            format!(
                "{}{}",
                url.trim_end_matches('/'),
                crate::oauth::CALLBACK_PATH
            )
        })
    }

    /// The webhook secrets, or `None` when `LINEAR_WEBHOOK_SECRET` is unset.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when the secret is blank.
    pub(crate) fn webhook_secrets(&self) -> Result<Option<SecretSet>> {
        self.webhook_secret
            .clone()
            .map(|current| SecretSet::new(current, self.previous_webhook_secret.clone()))
            .transpose()
    }

    /// The OAuth client secret, if set.
    pub(crate) fn client_secret(&self) -> Option<&str> {
        self.client_secret.as_deref()
    }

    /// The personal API key, if set.
    pub(crate) fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }
}

/// Keys that must never appear in `[linear]`.
const SECRET_KEYS: [&str; 4] = ["secret", "token", "api_key", "password"];

/// True for a key that names a secret.
fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace('-', "_");
    SECRET_KEYS.contains(&key.as_str())
        || key.ends_with("_secret")
        || key.ends_with("_token")
        || key.ends_with("_key")
}

/// True for a well-formed scope, for example `comments:create`.
fn is_scope(scope: &str) -> bool {
    !scope.is_empty()
        && scope.len() <= 64
        && scope
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b':' || b == b'_')
}

/// Check `app_url`: absolute, `https` (or `http` on a loopback host), no
/// query, no fragment.
fn validate_app_url(app_url: &str) -> Result<()> {
    let url = url::Url::parse(app_url)
        .map_err(|_| Error::Config(format!("app_url {app_url:?} is not a valid URL")))?;
    let scheme_ok = url.scheme() == "https" || (url.scheme() == "http" && is_loopback(&url));
    if !scheme_ok {
        return Err(Error::Config(
            "app_url must use https (http only for localhost)".to_owned(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(Error::Config(
            "app_url must not have a query or a fragment".to_owned(),
        ));
    }
    Ok(())
}

/// True when the URL host is `localhost`, `127.0.0.1`, or `[::1]`.
pub(crate) fn is_loopback(url: &url::Url) -> bool {
    matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
}

/// Check `after_install_path`: a local path, never an absolute or
/// scheme-relative URL (no open redirect).
fn validate_local_path(path: &str) -> Result<()> {
    let local = path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains('\\')
        && !path.contains('#')
        && !path.chars().any(char::is_control);
    if local {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "after_install_path {path:?} must be a local path such as \"/\""
        )))
    }
}

/// Parse a whole number from an env value. The error names the variable.
fn number<T: std::str::FromStr>(value: &str, name: &str) -> Result<T> {
    value
        .trim()
        .parse()
        .map_err(|_| Error::Config(format!("{name} must be a whole number")))
}

/// Split a comma-separated list. Drop the blank items.
fn split_csv(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Use the framework's config-file discovery order.
fn config_file_path() -> PathBuf {
    if let Ok(manifest_dir) = std::env::var("AUTUMN_MANIFEST_DIR") {
        let candidate = PathBuf::from(manifest_dir).join("autumn.toml");
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from("autumn.toml")
}

#[cfg(test)]
mod tests;
