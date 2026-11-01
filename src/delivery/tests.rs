#![allow(clippy::expect_used)]

use super::*;

const NOW: u64 = 1_793_000_000_000;

async fn claim(store: &InMemoryDeliveryStore, id: &str, now: u64) -> Claim {
    store.claim(id, now).await.expect("claims")
}

#[tokio::test]
async fn a_new_id_is_fresh_then_in_flight_then_done() {
    let store = InMemoryDeliveryStore::default();
    assert_eq!(claim(&store, "d-1", NOW).await, Claim::Fresh);
    assert_eq!(claim(&store, "d-1", NOW + 1).await, Claim::InFlight);
    store.complete("d-1", NOW).await.expect("completes");
    assert_eq!(claim(&store, "d-1", NOW + 2).await, Claim::Done);
    assert_eq!(store.len().expect("len"), 1);
    assert!(!store.is_empty().expect("empty"));
}

#[tokio::test]
async fn a_release_lets_the_retry_run_again() {
    let store = InMemoryDeliveryStore::default();
    assert_eq!(claim(&store, "d-1", NOW).await, Claim::Fresh);
    store.release("d-1", NOW).await.expect("releases");
    assert_eq!(claim(&store, "d-1", NOW + 60_000).await, Claim::Fresh);
}

#[tokio::test]
async fn an_expired_lease_is_claimed_again_and_the_late_owner_is_ignored() {
    let store = InMemoryDeliveryStore::default().with_lease_ms(1_000);
    assert_eq!(claim(&store, "d-1", NOW).await, Claim::Fresh);
    assert_eq!(claim(&store, "d-1", NOW + 999).await, Claim::InFlight);
    assert_eq!(claim(&store, "d-1", NOW + 1_000).await, Claim::Fresh);
    // The first owner finishes late. Its complete must not close the new claim.
    store.complete("d-1", NOW).await.expect("ignored");
    assert_eq!(claim(&store, "d-1", NOW + 1_001).await, Claim::InFlight);
    store.release("d-1", NOW).await.expect("ignored");
    assert_eq!(claim(&store, "d-1", NOW + 1_002).await, Claim::InFlight);
    store.complete("d-1", NOW + 1_000).await.expect("completes");
    assert_eq!(claim(&store, "d-1", NOW + 1_003).await, Claim::Done);
}

#[tokio::test]
async fn claims_leave_the_window() {
    let store = InMemoryDeliveryStore::new(10_000, 100);
    assert_eq!(claim(&store, "d-1", NOW).await, Claim::Fresh);
    store.complete("d-1", NOW).await.expect("completes");
    assert_eq!(claim(&store, "d-1", NOW + 9_999).await, Claim::Done);
    assert_eq!(claim(&store, "d-2", NOW + 10_000).await, Claim::Fresh);
    assert_eq!(
        claim(&store, "d-1", NOW + 10_001).await,
        Claim::Fresh,
        "d-1 left the window"
    );
}

#[tokio::test]
async fn a_full_store_drops_the_oldest_claim() {
    let store = InMemoryDeliveryStore::new(DEFAULT_WINDOW_MS, 2);
    for (i, id) in ["a", "b", "c"].iter().enumerate() {
        assert_eq!(claim(&store, id, NOW + i as u64).await, Claim::Fresh);
        store.complete(id, NOW + i as u64).await.expect("completes");
    }
    assert_eq!(store.len().expect("len"), 2);
    assert_eq!(
        claim(&store, "a", NOW + 10).await,
        Claim::Fresh,
        "a was dropped"
    );
    assert_eq!(claim(&store, "c", NOW + 11).await, Claim::Done);
}

#[tokio::test]
async fn released_ids_do_not_grow_the_order_without_a_limit() {
    let store = InMemoryDeliveryStore::new(DEFAULT_WINDOW_MS, 4);
    for i in 0..1_000_u64 {
        assert_eq!(claim(&store, "same", NOW + i).await, Claim::Fresh);
        store.release("same", NOW + i).await.expect("releases");
    }
    assert!(store.order_len().expect("len") <= 8);
    assert!(store.is_empty().expect("empty"));
}

#[test]
fn zero_capacity_counts_as_one() {
    let store = InMemoryDeliveryStore::new(DEFAULT_WINDOW_MS, 0);
    assert_eq!(store.capacity, 1);
}
