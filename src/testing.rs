//! A test kit for apps that use the plugin.
//!
//! [`EventFixture`] builds a webhook body. [`EventFixture::sign`] signs it
//! and gives the headers that Linear sends, so that the plugin accepts the
//! request.
//!
//! ```
//! use autumn_plugin_linear::testing::EventFixture;
//!
//! let signed = EventFixture::new("Issue", "create", "org-1")
//!     .data(serde_json::json!({"id": "i-1", "title": "Crash"}))
//!     .at_ms(1_793_000_000_000)
//!     .sign("webhook-secret", "delivery-1");
//! assert!(signed.header("linear-signature").is_some());
//! ```

use serde_json::{Map, Value, json};

/// A signed webhook request: the headers and the exact body bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SignedEvent {
    /// The headers, lower-case names.
    pub headers: Vec<(String, String)>,
    /// The body bytes that the signature covers.
    pub body: Vec<u8>,
}

impl SignedEvent {
    /// Sign `body` with `secret` and add the Linear headers.
    #[must_use]
    pub fn new(
        secret: &str,
        body: Vec<u8>,
        delivery_id: &str,
        event_type: &str,
        at_ms: u64,
    ) -> Self {
        let headers = vec![
            (
                "content-type".to_owned(),
                "application/json; charset=utf-8".to_owned(),
            ),
            ("user-agent".to_owned(), "Linear-Webhook".to_owned()),
            (
                crate::webhooks::DELIVERY_HEADER.to_owned(),
                delivery_id.to_owned(),
            ),
            (
                crate::webhooks::EVENT_HEADER.to_owned(),
                event_type.to_owned(),
            ),
            (
                crate::webhooks::TIMESTAMP_HEADER.to_owned(),
                at_ms.to_string(),
            ),
            (
                crate::verify::SIGNATURE_HEADER.to_owned(),
                crate::verify::sign(secret, &body),
            ),
        ];
        Self { headers, body }
    }

    /// The value of header `name`.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The body as text.
    #[must_use]
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A webhook body builder.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct EventFixture {
    body: Map<String, Value>,
}

impl EventFixture {
    /// A body with `type`, `action`, `organizationId`, a `webhookId`, a
    /// `createdAt`, and `webhookTimestamp` 0. Set the time with
    /// [`at_ms`](Self::at_ms).
    pub fn new(event_type: &str, action: &str, organization_id: &str) -> Self {
        let mut body = Map::new();
        body.insert("type".to_owned(), json!(event_type));
        body.insert("action".to_owned(), json!(action));
        body.insert("organizationId".to_owned(), json!(organization_id));
        body.insert("webhookId".to_owned(), json!("webhook-fixture"));
        body.insert("createdAt".to_owned(), json!("2026-10-26T10:00:00.000Z"));
        body.insert("webhookTimestamp".to_owned(), json!(0));
        Self { body }
    }

    /// Set `data`.
    pub fn data(self, data: Value) -> Self {
        self.field("data", data)
    }

    /// Set `updatedFrom`.
    pub fn updated_from(self, updated_from: Value) -> Self {
        self.field("updatedFrom", updated_from)
    }

    /// Set `webhookTimestamp` (Unix ms).
    pub fn at_ms(self, at_ms: u64) -> Self {
        self.field("webhookTimestamp", json!(at_ms))
    }

    /// Set any top-level field, for example `agentSession`.
    pub fn field(mut self, name: &str, value: Value) -> Self {
        self.body.insert(name.to_owned(), value);
        self
    }

    /// The body as JSON.
    #[must_use]
    pub fn to_json(&self) -> Value {
        Value::Object(self.body.clone())
    }

    /// Sign the body and add the headers.
    #[must_use]
    pub fn sign(&self, secret: &str, delivery_id: &str) -> SignedEvent {
        let event_type = self
            .body
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let at_ms = self
            .body
            .get("webhookTimestamp")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        SignedEvent::new(
            secret,
            self.to_json().to_string().into_bytes(),
            delivery_id,
            event_type,
            at_ms,
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn a_fixture_signs_what_the_verifier_accepts() {
        let signed = EventFixture::new("Issue", "update", "org-1")
            .data(json!({"id": "i-1"}))
            .updated_from(json!({"title": "old"}))
            .at_ms(5)
            .sign("s", "d-1");
        let secrets = crate::verify::SecretSet::new("s", None).expect("valid");
        crate::verify::verify_signature(&secrets, signed.header("linear-signature"), &signed.body)
            .expect("verifies");
        assert_eq!(signed.header("linear-delivery"), Some("d-1"));
        assert_eq!(signed.header("linear-event"), Some("Issue"));
        assert_eq!(signed.header("linear-timestamp"), Some("5"));
        let event = crate::event::WebhookEvent::parse(&signed.body).expect("parses");
        assert!(event.changed("title"));
        assert!(signed.body_text().contains("\"updatedFrom\""));
        assert_eq!(signed.header("missing"), None);
    }
}
