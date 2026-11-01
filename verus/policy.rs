//! Verus spec and proof for `src/policy.rs`.
//!
//! Keep this file in step with `src/policy.rs`. The test
//! `policy::tests::verus_bodies_match` compares the function bodies and the
//! constants. Run: `verus verus/policy.rs`.

use vstd::prelude::*;

verus! {

/// Default replay window: 60 seconds. Linear asks for this value.
pub const DEFAULT_TOLERANCE_MS: u64 = 60_000;
/// Largest replay window: 10 minutes.
pub const MAX_TOLERANCE_MS: u64 = 600_000;
/// Refresh a token that expires within 5 minutes.
pub const REFRESH_MARGIN_MS: u64 = 300_000;
/// Default lease of an in-flight delivery claim: 5 minutes.
pub const DEFAULT_LEASE_MS: u64 = 300_000;
/// Largest number of rate-limit retries for one call.
pub const MAX_RETRIES: u32 = 10;
/// Wait before a retry when Linear sends no `Retry-After`.
pub const DEFAULT_RETRY_WAIT_MS: u64 = 1_000;

// ------------------------------------------------------------ replay window

/// Spec: a webhook is fresh when its signed time is at most `tol` from now.
pub open spec fn spec_fresh(now: int, sent: int, tol: int) -> bool {
    if sent <= now { now - sent <= tol } else { sent - now <= tol }
}

/// Returns `true` when the signed send time is inside the replay window.
pub fn timestamp_fresh(now_ms: u64, sent_ms: u64, tolerance_ms: u64) -> (r: bool)
    ensures
        r == spec_fresh(now_ms as int, sent_ms as int, tolerance_ms as int),
{
    if sent_ms <= now_ms {
        now_ms - sent_ms <= tolerance_ms
    } else {
        sent_ms - now_ms <= tolerance_ms
    }
}

/// The window is symmetric: the past and the future get the same limit.
proof fn lemma_fresh_symmetric(a: int, b: int, tol: int)
    ensures
        spec_fresh(a, b, tol) == spec_fresh(b, a, tol),
{
}

/// A wider window accepts all that a narrower window accepts.
proof fn lemma_fresh_monotonic(now: int, sent: int, t1: int, t2: int)
    requires
        t1 <= t2,
        spec_fresh(now, sent, t1),
    ensures
        spec_fresh(now, sent, t2),
{
}

/// A zero window accepts only the exact time.
proof fn lemma_zero_window_is_exact(now: int, sent: int)
    ensures
        spec_fresh(now, sent, 0) <==> now == sent,
{
}

/// A time outside the window is never fresh, however far away it is.
proof fn lemma_far_times_are_stale(now: int, sent: int, tol: int)
    requires
        tol >= 0,
        sent > now + tol || sent < now - tol,
    ensures
        !spec_fresh(now, sent, tol),
{
}

// ------------------------------------------------------------------ refresh

/// Spec: refresh when the token is expired or expires within the margin.
pub open spec fn spec_needs_refresh(now: int, expires_at: int, margin: int) -> bool {
    expires_at <= now || expires_at - now < margin
}

/// Returns `true` when the token must be refreshed now.
pub fn needs_refresh(now_ms: u64, expires_at_ms: u64, margin_ms: u64) -> (r: bool)
    ensures
        r == spec_needs_refresh(now_ms as int, expires_at_ms as int, margin_ms as int),
{
    if expires_at_ms <= now_ms {
        true
    } else {
        expires_at_ms - now_ms < margin_ms
    }
}

/// An expired token always needs a refresh, with any margin.
proof fn lemma_expired_needs_refresh(now: int, exp: int, margin: int)
    requires
        exp <= now,
    ensures
        spec_needs_refresh(now, exp, margin),
{
}

/// A larger margin refreshes all tokens that a smaller margin refreshes.
proof fn lemma_refresh_monotonic(now: int, exp: int, m1: int, m2: int)
    requires
        m1 <= m2,
        spec_needs_refresh(now, exp, m1),
    ensures
        spec_needs_refresh(now, exp, m2),
{
}

/// Time only moves a token toward a refresh.
proof fn lemma_refresh_monotonic_in_time(n1: int, n2: int, exp: int, margin: int)
    requires
        n1 <= n2,
        spec_needs_refresh(n1, exp, margin),
    ensures
        spec_needs_refresh(n2, exp, margin),
{
}

// -------------------------------------------------------------------- retry

/// Spec: retry only a rate-limit answer, only while retries remain, and
/// only when the wait fits the cap.
pub open spec fn spec_may_retry(limited: bool, done: int, max: int, hint: int, cap: int) -> bool {
    limited && done < max && hint <= cap
}

/// The wait before the next retry, or `None` when the caller must stop.
pub fn retry_wait_ms(
    rate_limited: bool,
    retries_done: u32,
    max_retries: u32,
    hint_ms: u64,
    cap_ms: u64,
) -> (r: Option<u64>)
    ensures
        match r {
            Some(w) => w == hint_ms && w <= cap_ms && spec_may_retry(
                rate_limited,
                retries_done as int,
                max_retries as int,
                hint_ms as int,
                cap_ms as int,
            ),
            None => !spec_may_retry(
                rate_limited,
                retries_done as int,
                max_retries as int,
                hint_ms as int,
                cap_ms as int,
            ),
        },
{
    if rate_limited && retries_done < max_retries && hint_ms <= cap_ms {
        Some(hint_ms)
    } else {
        None
    }
}

/// Only a rate-limit answer can retry. Other failures never retry.
proof fn lemma_only_rate_limits_retry(done: int, max: int, hint: int, cap: int)
    ensures
        !spec_may_retry(false, done, max, hint, cap),
{
}

/// After `max` retries, the caller stops: the retries are bounded.
proof fn lemma_retries_are_bounded(limited: bool, max: int, hint: int, cap: int)
    ensures
        !spec_may_retry(limited, max, max, hint, cap),
{
}

/// A wait longer than the cap stops the loop. The caller gets the wait and
/// can schedule the work for later.
proof fn lemma_long_waits_stop(limited: bool, done: int, max: int, hint: int, cap: int)
    requires
        hint > cap,
    ensures
        !spec_may_retry(limited, done, max, hint, cap),
{
}

// ------------------------------------------------------------- claim state

/// The stored state of one delivery id.
pub enum Slot {
    /// No claim.
    Empty,
    /// A delivery claimed the id at `at` and still runs.
    Held { at: u64 },
    /// A handler for the claim made at `at` succeeded.
    Done { at: u64 },
}

/// The answer to a claim.
pub enum Claim {
    /// The caller owns the claim. It must complete or release it.
    Fresh,
    /// Another delivery of this id still runs.
    InFlight,
    /// A handler already succeeded for this id.
    Done,
}

/// Spec: a held claim blocks a new claim while its lease runs. A clock
/// that moves back keeps the claim.
pub open spec fn spec_lease_live(at: int, now: int, lease: int) -> bool {
    now < at || now - at < lease
}

/// Spec: the slot and the answer after a claim at `now`.
pub open spec fn spec_claim(slot: Slot, now: u64, lease: u64) -> (Slot, Claim) {
    match slot {
        Slot::Empty => (Slot::Held { at: now }, Claim::Fresh),
        Slot::Done { at } => (Slot::Done { at }, Claim::Done),
        Slot::Held { at } => if spec_lease_live(at as int, now as int, lease as int) {
            (Slot::Held { at }, Claim::InFlight)
        } else {
            (Slot::Held { at: now }, Claim::Fresh)
        },
    }
}

/// Spec: the slot after the owner of `claimed_at` completes it.
pub open spec fn spec_complete(slot: Slot, claimed_at: u64) -> Slot {
    match slot {
        Slot::Held { at } => if at == claimed_at { Slot::Done { at } } else { slot },
        _ => slot,
    }
}

/// Spec: the slot after the owner of `claimed_at` releases it.
pub open spec fn spec_release(slot: Slot, claimed_at: u64) -> Slot {
    match slot {
        Slot::Held { at } => if at == claimed_at { Slot::Empty } else { slot },
        _ => slot,
    }
}

/// Claim the id at `now`. Returns the new slot and the answer.
pub fn claim(slot: Slot, now_ms: u64, lease_ms: u64) -> (r: (Slot, Claim))
    ensures
        r == spec_claim(slot, now_ms, lease_ms),
{
    match slot {
        Slot::Empty => (Slot::Held { at: now_ms }, Claim::Fresh),
        Slot::Done { at } => (Slot::Done { at }, Claim::Done),
        Slot::Held { at } => {
            if now_ms < at || now_ms - at < lease_ms {
                (Slot::Held { at }, Claim::InFlight)
            } else {
                (Slot::Held { at: now_ms }, Claim::Fresh)
            }
        }
    }
}

/// Mark the claim made at `claimed_at` as done. Another claim stays.
pub fn complete(slot: Slot, claimed_at: u64) -> (r: Slot)
    ensures
        r == spec_complete(slot, claimed_at),
{
    match slot {
        Slot::Held { at } => {
            if at == claimed_at {
                Slot::Done { at }
            } else {
                Slot::Held { at }
            }
        }
        other => other,
    }
}

/// Release the claim made at `claimed_at`, so that a retry runs again.
/// Another claim stays.
pub fn release(slot: Slot, claimed_at: u64) -> (r: Slot)
    ensures
        r == spec_release(slot, claimed_at),
{
    match slot {
        Slot::Held { at } => {
            if at == claimed_at {
                Slot::Empty
            } else {
                Slot::Held { at }
            }
        }
        other => other,
    }
}

/// Done is final: no claim, completion, or release changes it.
proof fn lemma_done_is_final(at: u64, now: u64, lease: u64, owner: u64)
    ensures
        spec_claim(Slot::Done { at }, now, lease) == (Slot::Done { at }, Claim::Done),
        spec_complete(Slot::Done { at }, owner) == (Slot::Done { at }),
        spec_release(Slot::Done { at }, owner) == (Slot::Done { at }),
{
}

/// A fresh claim always records the claim time, so its owner can settle it.
proof fn lemma_fresh_claim_is_owned(slot: Slot, now: u64, lease: u64)
    ensures
        spec_claim(slot, now, lease).1 is Fresh ==> spec_claim(slot, now, lease).0 == (Slot::Held { at: now }),
{
}

/// Two claims inside one lease cannot both be fresh.
proof fn lemma_no_double_fresh(now1: u64, now2: u64, lease: u64)
    requires
        now1 <= now2,
        now2 - now1 < lease,
    ensures
        spec_claim(spec_claim(Slot::Empty, now1, lease).0, now2, lease).1 is InFlight,
{
}

/// A late owner cannot settle a newer claim of the same id.
proof fn lemma_stale_owner_is_ignored(at: u64, stale: u64)
    requires
        at != stale,
    ensures
        spec_complete(Slot::Held { at }, stale) == (Slot::Held { at }),
        spec_release(Slot::Held { at }, stale) == (Slot::Held { at }),
{
}

/// After a release, the next claim is fresh: the retry runs again.
proof fn lemma_release_reopens(at: u64, now: u64, lease: u64)
    ensures
        spec_claim(spec_release(Slot::Held { at }, at), now, lease).1 is Fresh,
{
}

/// A completed claim answers Done to every later delivery.
proof fn lemma_complete_closes(at: u64, now: u64, lease: u64)
    ensures
        spec_claim(spec_complete(Slot::Held { at }, at), now, lease).1 is Done,
{
}

fn main() {
}

} // verus!
