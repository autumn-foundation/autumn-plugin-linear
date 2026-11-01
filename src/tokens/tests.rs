#![allow(clippy::expect_used)]

use super::*;

const NOW: u64 = 1_793_000_000_000;

fn org(id: &str) -> OrganizationId {
    OrganizationId::parse(id).expect("valid")
}

fn token(expires_in_ms: Option<u64>) -> WorkspaceToken {
    WorkspaceToken {
        access_token: "at-secret".into(),
        refresh_token: Some("rt-secret".into()),
        expires_at_ms: expires_in_ms.map(|ms| NOW + ms),
        scope: "read,write".into(),
        installed_at_ms: Some(NOW),
    }
}

#[tokio::test]
async fn the_in_memory_store_puts_gets_and_deletes() {
    let store = InMemoryTokenStore::new();
    assert!(store.get(&org("o1")).await.expect("get").is_none());
    store.put(&org("o1"), token(None)).await.expect("put");
    assert_eq!(store.get(&org("o1")).await.expect("get"), Some(token(None)));
    assert!(store.get(&org("o2")).await.expect("get").is_none());
    store.delete(&org("o1")).await.expect("delete");
    assert!(store.get(&org("o1")).await.expect("get").is_none());
    store
        .delete(&org("o1"))
        .await
        .expect("a second delete is fine");
}

#[test]
fn expiry_rules_use_the_policy_core() {
    assert!(
        !token(None).needs_refresh(NOW, 300_000),
        "no expiry, no refresh"
    );
    assert!(!token(None).is_expired(NOW));
    assert!(token(Some(299_999)).needs_refresh(NOW, 300_000));
    assert!(!token(Some(300_000)).needs_refresh(NOW, 300_000));
    assert!(token(Some(0)).is_expired(NOW));
    assert!(!token(Some(1)).is_expired(NOW));
}

#[test]
fn debug_redacts_both_tokens() {
    let text = format!("{:?}", token(Some(1)));
    assert!(
        !text.contains("at-secret") && !text.contains("rt-secret"),
        "{text}"
    );
    assert!(text.contains("<redacted>"));
}

#[test]
fn scopes_split_on_commas_and_spaces() {
    let mut t = token(None);
    t.scope = "read write,comments:create".into();
    assert_eq!(t.scopes(), vec!["read", "write", "comments:create"]);
}

#[tokio::test]
async fn delete_if_removes_only_the_seen_token() {
    let store = InMemoryTokenStore::new();
    let seen = token(None);
    store.put(&org("o1"), seen.clone()).await.expect("put");
    let mut newer = token(None);
    newer.access_token = "at-new".into();
    assert!(!store.delete_if(&org("o1"), &newer).await.expect("compare"));
    assert!(store.get(&org("o1")).await.expect("get").is_some());
    assert!(store.delete_if(&org("o1"), &seen).await.expect("compare"));
    assert!(store.get(&org("o1")).await.expect("get").is_none());
    assert!(
        !store
            .delete_if(&org("o1"), &seen)
            .await
            .expect("missing is false")
    );
}
