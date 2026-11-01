#![allow(clippy::expect_used)]

use proptest::prelude::*;

use super::*;

fn set(current: &str, previous: Option<&str>) -> SecretSet {
    SecretSet::new(current, previous.map(ToOwned::to_owned)).expect("valid secrets")
}

#[test]
fn sign_matches_a_reference_vector() {
    // python3 -c "import hmac,hashlib;print(hmac.new(b'lin_wh_secret', b'{\"action\":\"create\"}', hashlib.sha256).hexdigest())"
    assert_eq!(
        sign("lin_wh_secret", br#"{"action":"create"}"#),
        "11c5f2085f1b5109af0047bd3ba11bfa9526eead34858087e905ac45bd73c6b6"
    );
}

#[test]
fn a_valid_signature_verifies() {
    let body = br#"{"type":"Issue"}"#;
    let secrets = set("s1", None);
    assert!(verify_signature(&secrets, Some(&sign("s1", body)), body).is_ok());
    // Upper-case hex and spaces around the value also verify.
    let upper = sign("s1", body).to_uppercase();
    assert!(verify_signature(&secrets, Some(&format!(" {upper} ")), body).is_ok());
}

#[test]
fn the_previous_secret_verifies_during_rotation() {
    let body = b"{}";
    let secrets = set("new", Some("old"));
    assert!(verify_signature(&secrets, Some(&sign("old", body)), body).is_ok());
    assert!(verify_signature(&secrets, Some(&sign("new", body)), body).is_ok());
    assert!(verify_signature(&secrets, Some(&sign("other", body)), body).is_err());
}

#[test]
fn bad_signatures_fail_with_signature_errors() {
    let body = b"{}";
    let secrets = set("s1", None);
    let good = sign("s1", body);
    for header in [None, Some(""), Some("zz"), Some("abc"), Some(&good[..62])] {
        let err = verify_signature(&secrets, header, body).expect_err("refused");
        assert_eq!(err.kind(), crate::error::ErrorKind::Signature, "{header:?}");
    }
    let err = verify_signature(&secrets, Some(&good), b"{ }").expect_err("tampered");
    assert_eq!(err.kind(), crate::error::ErrorKind::Signature);
}

#[test]
fn blank_secrets_are_refused_or_dropped() {
    assert!(SecretSet::new("  ", None).is_err());
    let secrets = SecretSet::new("s1", Some("  ".into())).expect("valid");
    assert!(secrets.previous.is_none());
    assert_eq!(secrets.current(), "s1");
}

#[test]
fn debug_redacts_the_secrets() {
    let text = format!("{:?}", set("topsecret", Some("oldsecret")));
    assert!(
        !text.contains("topsecret") && !text.contains("oldsecret"),
        "{text}"
    );
    assert!(text.contains("<redacted>"));
}

proptest! {
    #[test]
    fn signed_bodies_always_verify(secret in "[ -~]{1,40}", body in proptest::collection::vec(any::<u8>(), 0..256)) {
        prop_assume!(!secret.trim().is_empty());
        let secrets = set(&secret, None);
        prop_assert!(verify_signature(&secrets, Some(&sign(&secret, &body)), &body).is_ok());
    }

    #[test]
    fn a_flipped_bit_never_verifies(body in proptest::collection::vec(any::<u8>(), 1..128), index: usize, bit in 0_u8..8) {
        let secrets = set("k", None);
        let signature = sign("k", &body);
        let mut changed = body;
        let at = index % changed.len();
        changed[at] ^= 1 << bit;
        prop_assert!(verify_signature(&secrets, Some(&signature), &changed).is_err());
    }
}
