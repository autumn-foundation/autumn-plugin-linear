//! Duplicate-delivery guard for webhooks.
//!
//! Linear can send one payload more than once. The webhook route claims
//! the `Linear-Delivery` id before it runs the handler:
//!
//! | Claim result | Route answer |
//! |---|---|
//! | [`Claim::Fresh`] | The handler runs. |
//! | [`Claim::InFlight`] | 409. Another delivery of this id still runs. |
//! | [`Claim::Done`] | 200. A handler already succeeded. |
//!
//! After success the route calls [`DeliveryStore::complete`]. After a
//! failure or a panic it calls [`DeliveryStore::release`], so that the next
//! retry runs again. Both calls name the claim by its time, so a late owner
//! cannot change a newer claim of the same id.
//!
//! [`InMemoryDeliveryStore`] runs the verified state machine in
//! [`crate::policy`] for each id. It keeps claims for a time window and
//! drops the oldest claim when it is full. It does not share state across
//! replicas. See ADR 0002: the guard is a fast path, not exactly-once.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

pub use crate::policy::Claim;
use crate::policy::Slot;

use crate::error::{Error, Result};
use crate::tokens::StoreFuture;

/// Default claim window: 24 hours. Linear retries for about 7 hours.
pub const DEFAULT_WINDOW_MS: u64 = 24 * 60 * 60 * 1000;
/// Default number of claims the in-memory store keeps.
pub const DEFAULT_CAPACITY: usize = 10_000;

/// Storage for delivery-id claims. Times are Unix ms.
pub trait DeliveryStore: Send + Sync + 'static {
    /// Claim `delivery_id` at `now_ms`.
    fn claim<'a>(&'a self, delivery_id: &'a str, now_ms: u64) -> StoreFuture<'a, Claim>;
    /// Mark the claim made at `claimed_at_ms` as done. Do nothing when the
    /// store holds a newer claim of the id.
    fn complete<'a>(&'a self, delivery_id: &'a str, claimed_at_ms: u64) -> StoreFuture<'a, ()>;
    /// Release the claim made at `claimed_at_ms`, so that a retry runs
    /// again. Do nothing when the store holds a newer claim of the id.
    fn release<'a>(&'a self, delivery_id: &'a str, claimed_at_ms: u64) -> StoreFuture<'a, ()>;
}

/// A process-local [`DeliveryStore`] with a time window, a lease, and a
/// size limit.
#[derive(Debug)]
pub struct InMemoryDeliveryStore {
    window_ms: u64,
    lease_ms: u64,
    pub(crate) capacity: usize,
    inner: Mutex<Claims>,
}

#[derive(Debug, Default)]
struct Claims {
    by_id: HashMap<String, Slot>,
    order: VecDeque<(String, u64)>,
}

impl Default for InMemoryDeliveryStore {
    fn default() -> Self {
        Self::new(DEFAULT_WINDOW_MS, DEFAULT_CAPACITY)
    }
}

impl InMemoryDeliveryStore {
    /// Make a store with a window (ms) and a capacity (claims). The store
    /// changes a capacity of zero to one.
    #[must_use]
    pub fn new(window_ms: u64, capacity: usize) -> Self {
        Self {
            window_ms,
            lease_ms: crate::policy::DEFAULT_LEASE_MS,
            capacity: capacity.max(1),
            inner: Mutex::new(Claims::default()),
        }
    }

    /// Set the lease of an in-flight claim, in ms.
    #[must_use]
    pub const fn with_lease_ms(mut self, lease_ms: u64) -> Self {
        self.lease_ms = lease_ms;
        self
    }

    /// The number of live claims.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Store`] when the lock is poisoned.
    pub fn len(&self) -> Result<usize> {
        self.lock().map(|claims| claims.by_id.len())
    }

    /// True when there are no claims.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Store`] when the lock is poisoned.
    pub fn is_empty(&self) -> Result<bool> {
        self.len().map(|len| len == 0)
    }

    /// The length of the age queue, for tests.
    #[cfg(test)]
    pub(crate) fn order_len(&self) -> Result<usize> {
        self.lock().map(|claims| claims.order.len())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Claims>> {
        self.inner
            .lock()
            .map_err(|_| Error::Store("delivery store lock is poisoned".to_owned()))
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "one lock covers the check and the insert"
    )]
    fn claim_now(&self, delivery_id: &str, now_ms: u64) -> Result<Claim> {
        let mut claims = self.lock()?;
        claims.prune(now_ms.saturating_sub(self.window_ms));
        let slot = claims
            .by_id
            .get(delivery_id)
            .copied()
            .unwrap_or(Slot::Empty);
        let (next, answer) = crate::policy::claim(slot, now_ms, self.lease_ms);
        if answer != Claim::Fresh {
            return Ok(answer);
        }
        while claims.by_id.len() >= self.capacity && !claims.by_id.contains_key(delivery_id) {
            if !claims.pop_oldest() {
                break;
            }
        }
        claims.by_id.insert(delivery_id.to_owned(), next);
        claims.order.push_back((delivery_id.to_owned(), now_ms));
        claims.compact(self.capacity);
        Ok(answer)
    }

    /// Complete (`done`) or release the claim made at `claimed_at_ms`.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "one lock covers the read and the write"
    )]
    fn settle(&self, delivery_id: &str, claimed_at_ms: u64, done: bool) -> Result<()> {
        let mut claims = self.lock()?;
        let Some(slot) = claims.by_id.get(delivery_id).copied() else {
            return Ok(());
        };
        let next = if done {
            crate::policy::complete(slot, claimed_at_ms)
        } else {
            crate::policy::release(slot, claimed_at_ms)
        };
        if next == Slot::Empty {
            // The `order` entry stays. `pop_oldest` skips stale entries.
            claims.by_id.remove(delivery_id);
        } else {
            claims.by_id.insert(delivery_id.to_owned(), next);
        }
        Ok(())
    }
}

impl DeliveryStore for InMemoryDeliveryStore {
    fn claim<'a>(&'a self, delivery_id: &'a str, now_ms: u64) -> StoreFuture<'a, Claim> {
        let result = self.claim_now(delivery_id, now_ms);
        Box::pin(async move { result })
    }

    fn complete<'a>(&'a self, delivery_id: &'a str, claimed_at_ms: u64) -> StoreFuture<'a, ()> {
        let result = self.settle(delivery_id, claimed_at_ms, true);
        Box::pin(async move { result })
    }

    fn release<'a>(&'a self, delivery_id: &'a str, claimed_at_ms: u64) -> StoreFuture<'a, ()> {
        let result = self.settle(delivery_id, claimed_at_ms, false);
        Box::pin(async move { result })
    }
}

/// The claim time of a slot. `None` for an empty slot.
const fn claimed_at(slot: Slot) -> Option<u64> {
    match slot {
        Slot::Held { at } | Slot::Done { at } => Some(at),
        Slot::Empty => None,
    }
}

impl Claims {
    /// True when the `order` entry `(id, at)` names the live claim.
    fn is_live(&self, id: &str, at: u64) -> bool {
        self.by_id
            .get(id)
            .is_some_and(|slot| claimed_at(*slot) == Some(at))
    }

    /// Drop claims made at or before `cutoff`.
    fn prune(&mut self, cutoff: u64) {
        while self.order.front().is_some_and(|(_, at)| *at <= cutoff) {
            self.pop_oldest();
        }
    }

    /// Drop stale entries when they outnumber the live claims, so that
    /// released ids do not grow `order` without a limit.
    fn compact(&mut self, capacity: usize) {
        if self.order.len() > capacity.saturating_mul(2) {
            let live: Vec<(String, u64)> = self
                .order
                .iter()
                .filter(|(id, at)| self.is_live(id, *at))
                .cloned()
                .collect();
            self.order = live.into();
        }
    }

    /// Drop the oldest entry. Returns `false` when there is none.
    ///
    /// An entry is stale when its id was released or claimed again later.
    /// A stale entry does not remove the live claim.
    fn pop_oldest(&mut self) -> bool {
        let Some((id, at)) = self.order.pop_front() else {
            return false;
        };
        if self.is_live(&id, at) {
            self.by_id.remove(&id);
        }
        true
    }
}

#[cfg(test)]
mod tests;
