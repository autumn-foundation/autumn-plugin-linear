//! The verified core: pure decisions with no IO and no clock.
//!
//! `verus/policy.rs` holds the Verus spec and the proofs for these
//! functions. Keep both files in step. The test `verus_bodies_match`
//! compares the function bodies and the constants.
//!
//! | Function | Decides |
//! |---|---|
//! | [`timestamp_fresh`] | Is the signed webhook time inside the replay window? |
//! | [`needs_refresh`] | Must the token be refreshed now? |
//! | [`retry_wait_ms`] | Retry a rate-limited call, and after which wait? |
//! | [`claim`], [`complete`], [`release`] | The delivery-claim state machine |
//!
//! All times are Unix milliseconds.

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

/// Returns `true` when the signed send time is inside the replay window.
#[must_use]
pub const fn timestamp_fresh(now_ms: u64, sent_ms: u64, tolerance_ms: u64) -> bool {
    if sent_ms <= now_ms {
        now_ms - sent_ms <= tolerance_ms
    } else {
        sent_ms - now_ms <= tolerance_ms
    }
}

/// Returns `true` when the token must be refreshed now.
#[must_use]
pub const fn needs_refresh(now_ms: u64, expires_at_ms: u64, margin_ms: u64) -> bool {
    if expires_at_ms <= now_ms {
        true
    } else {
        expires_at_ms - now_ms < margin_ms
    }
}

/// The wait before the next retry, or `None` when the caller must stop.
///
/// Only a rate-limit answer retries: Linear did not run the call. A wait
/// longer than `cap_ms` stops the loop, so that the caller can schedule
/// the work for later.
#[must_use]
pub const fn retry_wait_ms(
    rate_limited: bool,
    retries_done: u32,
    max_retries: u32,
    hint_ms: u64,
    cap_ms: u64,
) -> Option<u64> {
    if rate_limited && retries_done < max_retries && hint_ms <= cap_ms {
        Some(hint_ms)
    } else {
        None
    }
}

/// The stored state of one delivery id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// No claim.
    Empty,
    /// A delivery claimed the id at `at` and still runs.
    Held {
        /// The claim time.
        at: u64,
    },
    /// A handler for the claim made at `at` succeeded.
    Done {
        /// The claim time.
        at: u64,
    },
}

/// The answer to a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Claim {
    /// The caller owns the claim. It must complete or release it.
    Fresh,
    /// Another delivery of this id still runs.
    InFlight,
    /// A handler already succeeded for this id.
    Done,
}

/// Claim the id at `now`. Returns the new slot and the answer.
#[must_use]
pub const fn claim(slot: Slot, now_ms: u64, lease_ms: u64) -> (Slot, Claim) {
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
#[must_use]
pub const fn complete(slot: Slot, claimed_at: u64) -> Slot {
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
#[must_use]
pub const fn release(slot: Slot, claimed_at: u64) -> Slot {
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

#[cfg(test)]
mod tests;
