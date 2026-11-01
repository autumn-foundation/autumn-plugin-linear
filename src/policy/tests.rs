#![allow(clippy::expect_used, clippy::panic)]

use proptest::prelude::*;

use super::*;

// ------------------------------------------------------------ drift check

/// Removes `/* ... */` comments.
fn strip_block_comments(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("/*") {
        out.push_str(&rest[..at]);
        rest = rest[at..]
            .find("*/")
            .map_or("", |end| &rest[at + end + 2..]);
    }
    out.push_str(rest);
    out
}

/// Each `pub const NAME: T = V;` line, with no spaces.
fn constants(text: &str) -> std::collections::BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("pub const "))
        .filter(|l| !l.contains("fn "))
        .filter_map(|l| {
            let (name, rest) = l.split_once(':')?;
            Some((
                name.trim().to_owned(),
                rest.chars().filter(|c| !c.is_whitespace()).collect(),
            ))
        })
        .collect()
}

/// The body of `pub fn name` (or `pub const fn name`) with no comments and
/// no white space. In the spec, the body starts at a line with only `{`.
fn body(text: &str, name: &str, spec: bool) -> String {
    let at = text
        .find(&format!("pub fn {name}("))
        .or_else(|| text.find(&format!("pub const fn {name}(")))
        .unwrap_or_else(|| panic!("no fn {name}"));
    let open = if spec { "\n{\n" } else { " {\n" };
    let start = at
        + text[at..]
            .find(open)
            .unwrap_or_else(|| panic!("no body for {name}"))
        + open.len();
    let mut depth = 1_i32;
    let mut end = text.len();
    for (i, c) in text[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = start + i;
                    break;
                }
            }
            _ => {}
        }
    }
    strip_block_comments(&text[start..end])
        .lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<String>()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

#[test]
fn verus_bodies_match() {
    let spec = include_str!("../../verus/policy.rs");
    let code = include_str!("../policy.rs");
    for name in [
        "timestamp_fresh",
        "needs_refresh",
        "retry_wait_ms",
        "claim",
        "complete",
        "release",
    ] {
        assert_eq!(body(spec, name, true), body(code, name, false), "{name}");
    }
    let spec_consts = constants(spec);
    let code_consts = constants(code);
    assert_eq!(spec_consts.len(), 6, "{spec_consts:?}");
    assert_eq!(spec_consts, code_consts);
}

#[test]
fn the_drift_check_sees_changes() {
    let code =
        "pub const A: u64 = 1;\npub fn f(x: u64) -> u64 {\n    x /* note */ + 1 // tail\n}\n";
    assert_eq!(body(code, "f", false), "x+1");
    assert_eq!(constants(code).get("A").map(String::as_str), Some("u64=1;"));
    assert_eq!(strip_block_comments("a/*b*/c/*d"), "ac");
}

#[test]
fn constants_match_the_plan() {
    // docs/planning.md: AC-08 (60 s window, 1–600 s), AC-35 (5 min margin),
    // AC-05 (0–10 retries), Linear's 1 s default wait.
    assert_eq!(DEFAULT_TOLERANCE_MS, 60 * 1000);
    assert_eq!(MAX_TOLERANCE_MS, 600 * 1000);
    assert_eq!(REFRESH_MARGIN_MS, 5 * 60 * 1000);
    assert_eq!(DEFAULT_LEASE_MS, 5 * 60 * 1000);
    assert_eq!(MAX_RETRIES, 10);
    assert_eq!(DEFAULT_RETRY_WAIT_MS, 1000);
}

// ------------------------------------------------------------ replay window

/// 2026-10-26 in ms. Small test times miss overflow and operator mistakes.
const NOW: u64 = 1_793_000_000_000;

#[test]
fn replay_window_edges() {
    assert!(timestamp_fresh(NOW, NOW, 0));
    assert!(timestamp_fresh(NOW, NOW - 60_000, 60_000));
    assert!(!timestamp_fresh(NOW, NOW - 60_001, 60_000));
    assert!(timestamp_fresh(NOW, NOW + 60_000, 60_000));
    assert!(!timestamp_fresh(NOW, NOW + 60_001, 60_000));
    assert!(!timestamp_fresh(NOW, 0, 60_000));
    assert!(!timestamp_fresh(NOW, u64::MAX, 60_000));
    assert!(!timestamp_fresh(0, u64::MAX, u64::MAX - 1));
    assert!(timestamp_fresh(0, u64::MAX, u64::MAX));
}

proptest! {
    #[test]
    fn fresh_matches_the_spec(now: u64, sent: u64, tol: u64) {
        let gap = (i128::from(now) - i128::from(sent)).abs();
        prop_assert_eq!(timestamp_fresh(now, sent, tol), gap <= i128::from(tol));
    }

    #[test]
    fn fresh_is_symmetric(a: u64, b: u64, tol: u64) {
        prop_assert_eq!(timestamp_fresh(a, b, tol), timestamp_fresh(b, a, tol));
    }

    #[test]
    fn a_wider_window_accepts_more(now: u64, sent: u64, t1: u64, extra: u64) {
        let t2 = t1.saturating_add(extra);
        if timestamp_fresh(now, sent, t1) {
            prop_assert!(timestamp_fresh(now, sent, t2));
        }
    }
}

// ------------------------------------------------------------------ refresh

#[test]
fn refresh_edges() {
    assert!(needs_refresh(NOW, NOW, 0));
    assert!(needs_refresh(NOW, NOW - 1, 0));
    assert!(!needs_refresh(NOW, NOW + 1, 0));
    assert!(needs_refresh(NOW, NOW + 299_999, REFRESH_MARGIN_MS));
    assert!(!needs_refresh(NOW, NOW + 300_000, REFRESH_MARGIN_MS));
    assert!(!needs_refresh(NOW, NOW + 86_399_000, REFRESH_MARGIN_MS));
    assert!(!needs_refresh(0, u64::MAX, u64::MAX));
}

proptest! {
    #[test]
    fn refresh_matches_the_spec(now: u64, exp: u64, margin: u64) {
        let want = exp <= now || i128::from(exp) - i128::from(now) < i128::from(margin);
        prop_assert_eq!(needs_refresh(now, exp, margin), want);
    }

    #[test]
    fn time_only_moves_toward_a_refresh(n1: u64, step: u64, exp: u64, margin: u64) {
        let n2 = n1.saturating_add(step);
        if needs_refresh(n1, exp, margin) {
            prop_assert!(needs_refresh(n2, exp, margin));
        }
    }
}

// -------------------------------------------------------------------- retry

#[test]
fn retry_edges() {
    assert_eq!(retry_wait_ms(true, 0, 3, 1_000, 10_000), Some(1_000));
    assert_eq!(retry_wait_ms(true, 2, 3, 10_000, 10_000), Some(10_000));
    assert_eq!(retry_wait_ms(true, 3, 3, 1_000, 10_000), None);
    assert_eq!(retry_wait_ms(true, 0, 0, 1_000, 10_000), None);
    assert_eq!(retry_wait_ms(true, 0, 3, 10_001, 10_000), None);
    assert_eq!(retry_wait_ms(false, 0, 3, 1_000, 10_000), None);
}

proptest! {
    #[test]
    fn retry_matches_the_spec(limited: bool, done: u32, max: u32, hint: u64, cap: u64) {
        let r = retry_wait_ms(limited, done, max, hint, cap);
        let may = limited && done < max && hint <= cap;
        prop_assert_eq!(r.is_some(), may);
        if let Some(wait) = r {
            prop_assert_eq!(wait, hint);
            prop_assert!(wait <= cap);
        }
    }

    #[test]
    fn a_retry_loop_always_stops(max in 0_u32..=MAX_RETRIES, hint in 0_u64..2_000) {
        let mut done = 0;
        while retry_wait_ms(true, done, max, hint, 10_000).is_some() {
            done += 1;
            prop_assert!(done <= max);
        }
        prop_assert_eq!(done, max);
    }
}

// ------------------------------------------------------------- claim state

#[test]
fn claim_state_machine_walk() {
    let lease = DEFAULT_LEASE_MS;
    let (slot, first) = claim(Slot::Empty, NOW, lease);
    assert_eq!((slot, first), (Slot::Held { at: NOW }, Claim::Fresh));
    let (slot, second) = claim(slot, NOW + 1, lease);
    assert_eq!((slot, second), (Slot::Held { at: NOW }, Claim::InFlight));
    // A clock that moves back keeps the claim.
    assert_eq!(claim(slot, NOW - 5, lease).1, Claim::InFlight);
    // After the lease, a new delivery takes over.
    let (taken, third) = claim(slot, NOW + lease, lease);
    assert_eq!(
        (taken, third),
        (Slot::Held { at: NOW + lease }, Claim::Fresh)
    );
    // The first owner is late: it cannot settle the new claim.
    assert_eq!(complete(taken, NOW), taken);
    assert_eq!(release(taken, NOW), taken);
    // The new owner completes. Done is final.
    let done = complete(taken, NOW + lease);
    assert_eq!(done, Slot::Done { at: NOW + lease });
    assert_eq!(claim(done, NOW + 10 * lease, lease).1, Claim::Done);
    assert_eq!(release(done, NOW + lease), done);
    // A release reopens the id.
    let open = release(Slot::Held { at: NOW }, NOW);
    assert_eq!(open, Slot::Empty);
    assert_eq!(claim(open, NOW, lease).1, Claim::Fresh);
}

#[derive(Debug, Clone)]
enum Op {
    Claim(u64),
    Complete(u64),
    Release(u64),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0_u64..1_000).prop_map(Op::Claim),
        (0_u64..1_000).prop_map(Op::Complete),
        (0_u64..1_000).prop_map(Op::Release),
    ]
}

proptest! {
    /// Random operations keep the invariants of the spec: done is final, a
    /// fresh claim holds its own time, and only the owner settles a claim.
    #[test]
    fn claim_invariants_hold(ops in proptest::collection::vec(op(), 0..40), lease in 0_u64..200) {
        let mut slot = Slot::Empty;
        for op in ops {
            let before = slot;
            match op {
                Op::Claim(now) => {
                    let (next, answer) = claim(slot, now, lease);
                    if answer == Claim::Fresh {
                        prop_assert_eq!(next, Slot::Held { at: now });
                    }
                    if matches!(before, Slot::Done { .. }) {
                        prop_assert_eq!(answer, Claim::Done);
                    }
                    if let Slot::Held { at } = before
                        && (now < at || now - at < lease)
                    {
                        prop_assert_eq!(answer, Claim::InFlight);
                    }
                    slot = next;
                }
                Op::Complete(owner) => {
                    slot = complete(slot, owner);
                    match before {
                        Slot::Held { at } if at == owner => prop_assert_eq!(slot, Slot::Done { at }),
                        _ => prop_assert_eq!(slot, before),
                    }
                }
                Op::Release(owner) => {
                    slot = release(slot, owner);
                    match before {
                        Slot::Held { at } if at == owner => prop_assert_eq!(slot, Slot::Empty),
                        _ => prop_assert_eq!(slot, before),
                    }
                }
            }
        }
    }
}
