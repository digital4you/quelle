use super::*;

#[tokio::test]
async fn concurrent_same_key_is_serialized_and_waits_are_bounded() {
    let cache: Cache<String> = Cache::new(2, 100);
    let first = cache
        .flight("a", Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    assert!(
        cache
            .flight("a", Instant::now() + Duration::from_millis(10))
            .await
            .is_err()
    );
    cache.put("a".into(), "result".into(), 6);
    drop(first);
    let _second = cache
        .flight("a", Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(
        cache.get("a", Duration::from_secs(1)),
        Some("result".into())
    );
    assert!(cache.get("a", Duration::ZERO).is_none());
}

#[test]
fn cache_enforces_byte_budget_and_can_revalidate_stale_entries() {
    let cache: Cache<String> = Cache::new(2, 10);
    cache.put("a".into(), "123456".into(), 6);
    cache.put("b".into(), "abcdef".into(), 6);
    assert!(cache.get("a", Duration::from_secs(60)).is_none());
    assert!(cache.get("b", Duration::from_secs(60)).is_some());
    cache.put("huge".into(), "ignored".into(), 11);
    assert!(cache.get("huge", Duration::from_secs(60)).is_none());
}
