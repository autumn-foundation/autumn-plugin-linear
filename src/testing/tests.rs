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
