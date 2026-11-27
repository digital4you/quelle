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

struct RichFake {
    id: &'static str,
    weight: f64,
    delay: Duration,
    hits: Result<Vec<(&'static str, &'static str, &'static str)>, DiscoveryError>,
}

impl SearchEngine for RichFake {
    fn name(&self) -> &'static str {
        self.id
    }

    fn weight(&self) -> f64 {
        self.weight
    }

    fn search<'a>(&'a self, _: &'a Query, _: SearchOptions) -> DiscoveryFuture<'a> {
        let delay = self.delay;
        let hits = self.hits.clone();
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let rows = hits?;
            Ok(rows
                .into_iter()
                .map(|(u, t, d)| Candidate {
                    url: Url::parse(u).unwrap(),
                    title: t.to_owned(),
                    description: d.to_owned(),
                })
                .collect())
        })
    }
}

#[tokio::test]
async fn fuse_mode_fans_out_in_parallel_and_boosts_consensus_results_via_rrf() {
    let engines: Vec<Box<dyn SearchEngine>> = vec![
        Box::new(RichFake {
            id: "alpha",
            weight: 1.4,
            delay: Duration::from_millis(10),
            hits: Ok(vec![
                (
                    "https://alpha-only.example.org/a",
                    "Alpha Solo",
                    "Solo result",
                ),
                ("https://consensus.example.org/doc", "Consensus", ""),
            ]),
        }),
        Box::new(RichFake {
            id: "beta",
            weight: 1.0,
            delay: Duration::from_millis(20),
            hits: Ok(vec![
                ("https://beta-only.example.org/b", "Beta Solo", "Beta"),
                (
                    "https://consensus.example.org/doc?utm_source=test",
                    "Consensus Guide",
                    "Authoritative description from beta",
                ),
            ]),
        }),
        Box::new(RichFake {
            id: "broken",
            weight: 1.0,
            delay: Duration::ZERO,
            hits: Err(DiscoveryError::RateLimited(Duration::from_secs(60))),
        }),
    ];
    let pool = EnginePool::with_mode(engines, DiscoveryMode::Fuse);
    let hits = pool
        .lookup(
            &Query {
                text: "consensus test".into(),
                language: Language::En,
            },
            SearchOptions {
                count: 5,
                spellcheck: false,
            },
            &AtomicUsize::new(6),
        )
        .await
        .unwrap();

    // Consensus URL appeared #2 on both alpha & beta (with tracking param on beta),
    // so RRF + consensus boost must rank it #1 and keep beta's non-empty description!
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].url.as_str(), "https://consensus.example.org/doc");
    assert_eq!(hits[0].description, "Authoritative description from beta");
}

#[tokio::test]
async fn fuse_mode_cuts_off_slow_straggler_after_grace_window() {
    let engines: Vec<Box<dyn SearchEngine>> = vec![
        Box::new(RichFake {
            id: "fast",
            weight: 1.4,
            delay: Duration::from_millis(15),
            hits: Ok(vec![("https://fast.example.org/1", "Fast Hit", "Quick")]),
        }),
        Box::new(RichFake {
            id: "straggler",
            weight: 1.0,
            delay: Duration::from_secs(4),
            hits: Ok(vec![("https://slow.example.org/1", "Slow Hit", "Late")]),
        }),
    ];
    let pool = EnginePool::with_mode(engines, DiscoveryMode::Fuse);
    let started = Instant::now();
    let hits = pool
        .lookup(
            &Query {
                text: "straggler test".into(),
                language: Language::En,
            },
            SearchOptions {
                count: 5,
                spellcheck: false,
            },
            &AtomicUsize::new(6),
        )
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].url.as_str(), "https://fast.example.org/1");
    assert!(
        elapsed < Duration::from_millis(1500),
        "fuse mode waited too long for straggler: {elapsed:?}"
    );
}

#[test]
fn ddg_handles_direct_links_and_locale_mapping() {
    assert_eq!(ddg_locale(Language::De), "ch-de");
    assert_eq!(ddg_locale(Language::Fr), "ch-fr");
    assert_eq!(ddg_locale(Language::It), "ch-it");
    assert_eq!(ddg_locale(Language::En), "wt-wt");

    let direct = r#"<div class="result"><a class="result__a" href="https://www.estv.admin.ch/de/mwst">MWST</a><a class="result__snippet">8.1% Normalsatz</a></div>"#;
    let hits = parse_search_html(Kind::DuckDuckGo, direct, 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].url.as_str(), "https://www.estv.admin.ch/de/mwst");
    assert_eq!(hits[0].description, "8.1% Normalsatz");
}

#[test]
fn parses_stackexchange_hackernews_arxiv_and_github_keyless_apis() {
    let se = br#"{"items":[{"title":"Handling HTTP 429 &amp; Retry-After","link":"https://stackoverflow.com/questions/123/http-429","tags":["http","rust"],"answer_count":5,"score":42}]}"#;
    let se_hits = parse_stackexchange_json(se, 5).unwrap();
    assert_eq!(se_hits.len(), 1);
    assert_eq!(
        se_hits[0].url.as_str(),
        "https://stackoverflow.com/questions/123/http-429"
    );
    assert_eq!(se_hits[0].title, "Handling HTTP 429 & Retry-After");
    assert!(se_hits[0].description.contains("rust"));

    let hn = br#"{"hits":[{"title":"SQLite WAL Internals","url":"https://sqlite.org/wal.html","points":310,"num_comments":84,"objectID":"999"},{"title":"Ask HN: No external URL","url":null,"points":10,"objectID":"1000"}]}"#;
    let hn_hits = parse_hackernews_json(hn, 5).unwrap();
    assert_eq!(hn_hits.len(), 1);
    assert_eq!(hn_hits[0].url.as_str(), "https://sqlite.org/wal.html");
    assert!(hn_hits[0].description.contains("310"));

    let arxiv = r#"<?xml version="1.0" encoding="UTF-8"?>
    <feed xmlns="http://www.w3.org/2005/Atom">
      <entry>
        <id>http://arxiv.org/abs/1706.03762v7</id>
        <title>Attention Is All You Need</title>
        <summary>The dominant sequence transduction models are based on complex recurrent networks.</summary>
      </entry>
    </feed>"#;
    let ax_hits = parse_arxiv_xml(arxiv, 5).unwrap();
    assert_eq!(ax_hits.len(), 1);
    assert_eq!(
        ax_hits[0].url.as_str(),
        "https://arxiv.org/abs/1706.03762v7"
    );
    assert_eq!(ax_hits[0].title, "Attention Is All You Need");
    assert!(ax_hits[0].description.contains("sequence transduction"));

    let gh = br#"{"items":[{"full_name":"lightpanda-io/browser","html_url":"https://github.com/lightpanda-io/browser","description":"Headless browser designed for AI and automation","stargazers_count":9000}]}"#;
    let gh_hits = parse_github_json(gh, 5).unwrap();
    assert_eq!(gh_hits.len(), 1);
    assert_eq!(
        gh_hits[0].url.as_str(),
        "https://github.com/lightpanda-io/browser"
    );
    assert!(gh_hits[0].description.contains("Headless browser"));
}

#[test]
fn site_filter_skips_specialized_engines_for_unrelated_site_queries() {
    assert!(site_filter_allows(
        "SQLite WAL mode",
        &["stackoverflow.com"]
    ));
    assert!(site_filter_allows(
        "site:stackoverflow.com rust async",
        &["stackoverflow.com"]
    ));
    assert!(!site_filter_allows(
        "site:estv.admin.ch MWST 2024",
        &["stackoverflow.com"]
    ));
    assert!(!site_filter_allows(
        "site:fedlex.admin.ch DSG",
        &["wikipedia.org"]
    ));
    assert!(site_filter_allows(
        "site:en.wikipedia.org HTTP 429",
        &["wikipedia.org"]
    ));
}
