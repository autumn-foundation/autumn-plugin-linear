//! HMAC-SHA256 checks for Linear webhooks.
//!
//! Linear signs the raw request body with the webhook signing secret. The
//! hex digest travels in `Linear-Signature`. See [`verify_signature`].
//!
//! The check refuses a missing or malformed value and compares in constant
//! time. A [`SecretSet`] holds the current secret and, during rotation,
//! the previous one.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::{Error, Result};

pub(crate) type HmacSha256 = Hmac<Sha256>;

/// Header that carries the hex webhook signature.
pub const SIGNATURE_HEADER: &str = "linear-signature";

/// The webhook signing secret, plus the previous one during rotation.
///
/// `Debug` redacts both values. App code cannot read them back:
///
/// ```compile_fail
/// let set = autumn_plugin_linear::SecretSet::new("s", None).unwrap();
/// let _ = set.current();
/// ```
#[derive(Clone)]
pub struct SecretSet {
    current: String,
    previous: Option<String>,
}

impl std::fmt::Debug for SecretSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretSet")
            .field("current", &"<redacted>")
            .field("previous", &self.previous.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl SecretSet {
    /// Make a set from the current secret and an optional previous one.
    /// A blank previous secret counts as unset.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when the current secret is blank.
    pub fn new(current: impl Into<String>, previous: Option<String>) -> Result<Self> {
        let current = current.into();
        if current.trim().is_empty() {
            return Err(Error::Config(
                "the webhook signing secret is blank".to_owned(),
            ));
        }
        let previous = previous.filter(|secret| !secret.trim().is_empty());
        Ok(Self { current, previous })
    }

    /// The secrets to try, current first.
    fn iter(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.current.as_str()).chain(self.previous.as_deref())
    }

    /// The current secret. Never log it.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn current(&self) -> &str {
        &self.current
    }
}

/// Sign a webhook body: hex HMAC-SHA256 with `secret`.
///
/// Use it in tests to make deliveries that the plugin accepts.
#[must_use]
pub fn sign(secret: &str, body: &[u8]) -> String {
    mac(secret.as_bytes(), body)
        .map_or_else(String::new, |mac| hex::encode(mac.finalize().into_bytes()))
}

/// Verify a `Linear-Signature` value against the raw body.
///
/// # Errors
///
/// Returns [`Error::Signature`] when the value is missing, is not hex, or
/// matches no secret in `secrets`.
pub fn verify_signature(secrets: &SecretSet, header: Option<&str>, body: &[u8]) -> Result<()> {
    let header =
        header.ok_or_else(|| Error::Signature(format!("missing {SIGNATURE_HEADER} header")))?;
    let expected = hex::decode(header.trim())
        .map_err(|_| Error::Signature(format!("{SIGNATURE_HEADER} is not hex")))?;
    let mut matched = false;
    for secret in secrets.iter() {
        // `verify_slice` compares in constant time. Try every secret, so
        // that the time does not show which secret matched.
        matched |=
            mac(secret.as_bytes(), body).is_some_and(|mac| mac.verify_slice(&expected).is_ok());
    }
    if matched {
        Ok(())
    } else {
        Err(Error::Signature("the signature does not match".to_owned()))
    }
}

/// The HMAC-SHA256 of `message` under `key`.
///
/// HMAC accepts a key of any length, so `None` does not occur. The callers
/// still fail closed on `None`.
pub(crate) fn mac(key: &[u8], message: &[u8]) -> Option<HmacSha256> {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).ok()?;
    mac.update(message);
    Some(mac)
}

#[cfg(test)]
mod tests;
