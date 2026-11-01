#![allow(clippy::expect_used)]

use proptest::prelude::*;

use super::*;

#[test]
fn organization_id_accepts_uuids_and_refuses_hostile_values() {
    let uuid = "9cfb482a-81e3-4154-b5b9-2c805e70a02d";
    assert_eq!(
        OrganizationId::parse(uuid)
            .map(|o| o.to_string())
            .ok()
            .as_deref(),
        Some(uuid)
    );
    for bad in ["", "-x", "a b", "a/b", "a\nb", "ä", "a.b", &"a".repeat(65)] {
        assert!(OrganizationId::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn organization_id_round_trips_through_serde() {
    let id: OrganizationId = serde_json::from_str("\"org-1\"").expect("parses");
    assert_eq!(serde_json::to_string(&id).expect("writes"), "\"org-1\"");
    assert!(serde_json::from_str::<OrganizationId>("\"a b\"").is_err());
}

#[test]
fn event_type_parses_pascal_case_only() {
    for good in [
        EventType::ISSUE,
        EventType::AGENT_SESSION_EVENT,
        EventType::OAUTH_APP,
        "IssueSLA",
    ] {
        assert_eq!(
            EventType::parse(good)
                .map(|t| t.as_str().to_owned())
                .ok()
                .as_deref(),
            Some(good)
        );
    }
    for bad in [
        "",
        "issue",
        "Issue Label",
        "Issue-1",
        "Iss/ue",
        &"A".repeat(65),
    ] {
        assert!(EventType::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn action_parses_lower_camel_letters_only() {
    for good in [
        Action::CREATE,
        Action::UPDATE,
        Action::REMOVE,
        Action::REVOKED,
        Action::PROMPTED,
        "highRisk",
    ] {
        assert!(Action::parse(good).is_ok(), "{good:?}");
    }
    for bad in ["", "Create", "create1", "re move", "x_y", &"a".repeat(33)] {
        assert!(Action::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn delivery_id_rules() {
    assert!(DeliveryId::parse("2f6f3c1e-1b1a-4c3e-9f1d-0a6b0c9d8e7f").is_ok());
    assert!(DeliveryId::parse("a_b-C9").is_ok());
    for bad in ["", "a b", "a/b", "a:b", &"a".repeat(129)] {
        assert!(DeliveryId::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn actor_parses_and_prints() {
    assert_eq!(Actor::parse("app").ok(), Some(Actor::App));
    assert_eq!(Actor::parse(" user ").ok(), Some(Actor::User));
    assert_eq!(Actor::default(), Actor::User);
    assert_eq!(Actor::App.as_str(), "app");
    assert!(Actor::parse("bot").is_err());
}

proptest! {
    #[test]
    fn parsed_organization_ids_are_always_safe(value in "\\PC{0,80}") {
        if let Ok(id) = OrganizationId::parse(&value) {
            prop_assert!(id.as_str().bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
            prop_assert!(id.as_str().len() <= 64 && !id.as_str().is_empty());
        }
    }

    #[test]
    fn every_valid_organization_id_parses(value in "[A-Za-z0-9][A-Za-z0-9-]{0,63}") {
        prop_assert!(OrganizationId::parse(&value).is_ok());
    }

    #[test]
    fn parsed_delivery_ids_never_hold_separators(value in "\\PC{0,140}") {
        if let Ok(id) = DeliveryId::parse(&value) {
            prop_assert!(!id.as_str().contains([':', '/', ' ', '\n']));
        }
    }
}
