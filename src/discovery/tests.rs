use super::*;
use std::sync::Arc;

struct Fake {
    id: &'static str,
    calls: Arc<Mutex<Vec<&'static str>>>,
    result: Result<bool, DiscoveryError>,
}

impl SearchEngine for Fake {
    fn name(&self) -> &'static str {
        self.id
    }

    fn search<'a>(&'a self, _: &'a Query, _: SearchOptions) -> DiscoveryFuture<'a> {
        self.calls.lock().unwrap().push(self.id);
        Box::pin(async move {
            Ok(if self.result? {
                vec![Candidate {
                    url: Url::parse("https://example.org/").unwrap(),
                    title: self.id.to_owned(),
                    description: String::new(),
                }]
            } else {
                vec![]
            })
        })
    }
}

fn pool(
    outcomes: &[(&'static str, Result<bool, DiscoveryError>)],
) -> (EnginePool, Arc<Mutex<Vec<&'static str>>>) {
    let calls = Arc::new(Mutex::new(vec![]));
    let engines = outcomes
        .iter()
        .map(|&(id, result)| {
            Box::new(Fake {
                id,
                result,
                calls: calls.clone(),
            }) as Box<dyn SearchEngine>
        })
        .collect();
    (EnginePool::new(engines), calls)
}

async fn lookup(
    pool: &EnginePool,
    text: &str,
    retries: usize,
) -> Result<Vec<Candidate>, DiscoveryError> {
    pool.lookup(
        &Query {
            text: text.into(),
            language: Language::En,
        },
        SearchOptions {
            count: 3,
            spellcheck: false,
        },
        &AtomicUsize::new(retries),
    )
    .await
}

#[tokio::test]
async fn rotates_on_cache_miss_and_stops_after_success() {
    let (engines, calls) = pool(&[("one", Ok(true)), ("two", Ok(true))]);
    for (q, name) in [("a", "one"), ("a", "one"), ("b", "two"), ("c", "one")] {
        assert_eq!(lookup(&engines, q, 5).await.unwrap()[0].title, name);
    }
    assert_eq!(*calls.lock().unwrap(), ["one", "two", "one"]);
}

#[tokio::test]
async fn falls_back_on_errors_and_empty_results_and_skips_cooling_engine() {
    let (engines, calls) = pool(&[
        (
            "limited",
            Err(DiscoveryError::RateLimited(Duration::from_secs(60))),
        ),
        ("down", Err(DiscoveryError::Unavailable)),
        ("empty", Ok(false)),
        ("working", Ok(true)),
    ]);
    for q in ["a", "b", "c", "d", "e"] {
        assert_eq!(lookup(&engines, q, 5).await.unwrap()[0].title, "working");
    }
    let calls = calls.lock().unwrap();
    assert_eq!(&calls[..4], ["limited", "down", "empty", "working"]);
    assert_eq!(calls.iter().filter(|&&id| id == "limited").count(), 1);
}

#[tokio::test]
async fn preserves_empty_and_failure_outcomes_and_limits_attempts() {
    let (engines, calls) = pool(&[
        ("down", Err(DiscoveryError::Unavailable)),
        ("empty", Ok(false)),
    ]);
    assert!(matches!(
        lookup(&engines, "first", 0).await,
        Err(DiscoveryError::Unavailable)
    ));
    assert_eq!(*calls.lock().unwrap(), ["down"]);
    assert!(lookup(&engines, "next", 5).await.unwrap().is_empty());
    let (limited, _) = pool(&[(
        "a",
        Err(DiscoveryError::RateLimited(Duration::from_secs(30))),
    )]);
    for q in ["a", "b"] {
        assert!(matches!(
            lookup(&limited, q, 5).await,
            Err(DiscoveryError::RateLimited(_))
        ));
    }
}

#[test]
fn html_results_unwrap_destinations_and_reject_challenges() {
    let bing = r#"<ol id="b_results"><li class="b_algo"><h2><a href="https://www.bing.com/ck/a?u=a1aHR0cHM6Ly9leGFtcGxlLm9yZy8">Example</a></h2><div class="b_caption"><p>A summary.</p></div></li></ol>"#;
    let ddg = r#"<div class="result"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2F">Example</a><span class="result__snippet">A summary.</span></div>"#;
    for (kind, html) in [(Kind::Bing, bing), (Kind::DuckDuckGo, ddg)] {
        let hits = parse_search_html(kind, html, 3).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url.as_str(), "https://example.org/");
        assert_eq!(hits[0].description, "A summary.");
        assert!(parse_search_html(kind, "<title>Please verify</title>", 3).is_err());
    }
}
