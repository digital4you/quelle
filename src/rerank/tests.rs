use super::*;

#[test]
fn invalid_remote_rankings_fail_atomically() {
    for value in [
        serde_json::json!({"results":[{"index":9,"relevance_score":1.0}]}),
        serde_json::json!({"results":[{"index":0,"relevance_score":1.0},{"index":0,"relevance_score":0.5}]}),
        serde_json::json!({"results":[]}),
    ] {
        assert!(ranking_indices(value, 3).is_err());
    }
    assert_eq!(ranking_indices(serde_json::json!({"results":[{"index":2,"relevance_score":0.9},{"index":0,"relevance_score":0.1}]}), 3).unwrap(), vec![2,0,1]);
}

#[tokio::test]
async fn unavailable_reranker_preserves_all_candidates() {
    let server = Reranker::new(
        reqwest::Client::new(),
        Url::parse("http://127.0.0.1:1/rerank").unwrap(),
        None,
        "test".into(),
        Duration::from_millis(100),
    );
    let hits = vec![crate::discovery::Candidate {
        url: Url::parse("https://example.org/").unwrap(),
        title: "Evidence".into(),
        description: "Facts".into(),
    }];
    assert!(server.rank("evidence", &hits).await.is_err());
    assert_eq!(hits.len(), 1);
}
