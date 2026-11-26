use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::Mutex,
    time::{Duration, Instant},
};

use reqwest::{Client, StatusCode, header};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{read_json, truncate};

/// Brave bills per request and returns up to 20 results for the same price.
pub const RESULTS_PER_QUERY: usize = 20;
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const CACHE_ENTRIES: usize = 512;

#[derive(Clone)]
pub struct Candidate {
    pub url: Url,
    /// Discovery data only: used to choose which pages to read, never cited.
    pub title: String,
    pub description: String,
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    De,
    Fr,
    It,
    En,
}

#[derive(Deserialize, Serialize)]
pub struct Query {
    pub text: String,
    pub language: Language,
}

#[derive(Debug)]
pub enum DiscoveryError {
    RateLimited,
    Unavailable,
}

pub type DiscoveryFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<Candidate>, DiscoveryError>> + Send + 'a>>;

#[derive(Clone, Copy, Hash, Eq, PartialEq)]
pub struct SearchOptions {
    pub count: usize,
    pub spellcheck: bool,
}

pub trait SearchEngine: Send + Sync {
    fn name(&self) -> &'static str;
    fn search<'a>(&'a self, query: &'a Query, options: SearchOptions) -> DiscoveryFuture<'a>;
}

type CacheKey = (String, Language, SearchOptions);

pub struct Brave {
    pub client: Client,
    pub endpoint: Url,
    pub token: header::HeaderValue,
    pub cache: Mutex<HashMap<CacheKey, (Instant, Vec<Candidate>)>>,
}

impl Brave {
    pub fn new(client: Client, endpoint: Url, token: header::HeaderValue) -> Self {
        Self {
            client,
            endpoint,
            token,
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn cached(&self, key: &CacheKey) -> Option<Vec<Candidate>> {
        let cache = self.cache.lock().ok()?;
        cache
            .get(key)
            .filter(|(stored, _)| stored.elapsed() < CACHE_TTL)
            .map(|(_, candidates)| candidates.clone())
    }

    fn store(&self, key: CacheKey, candidates: &[Candidate]) {
        let Ok(mut cache) = self.cache.lock() else {
            return;
        };
        cache.retain(|_, (stored, _)| stored.elapsed() < CACHE_TTL);
        if cache.len() >= CACHE_ENTRIES
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (stored, _))| *stored)
                .map(|(key, _)| key.clone())
        {
            cache.remove(&oldest);
        }
        cache.insert(key, (Instant::now(), candidates.to_vec()));
    }
}

#[derive(Deserialize)]
struct BraveResponse {
    web: Option<BraveWeb>,
}

#[derive(Deserialize)]
struct BraveWeb {
    results: Vec<BraveResult>,
}

#[derive(Deserialize)]
struct BraveResult {
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
}

impl SearchEngine for Brave {
    fn name(&self) -> &'static str {
        "brave"
    }

    fn search<'a>(&'a self, query: &'a Query, options: SearchOptions) -> DiscoveryFuture<'a> {
        Box::pin(async move {
            let key = (query.text.clone(), query.language, options);
            if let Some(candidates) = self.cached(&key) {
                tracing::info!(stage = "brave_cache", hit = true);
                return Ok(candidates);
            }
            let (language, locale) = match query.language {
                Language::De => ("de", "de-CH"),
                Language::Fr => ("fr", "fr-CH"),
                Language::It => ("it", "de-CH"),
                Language::En => ("en", "de-CH"),
            };
            let count = options.count.to_string();
            let response = self
                .client
                .get(self.endpoint.clone())
                .timeout(Duration::from_secs(5))
                .header(header::ACCEPT, "application/json")
                .header(header::CACHE_CONTROL, "no-cache")
                .header("X-Loc-Country", "CH")
                .header("X-Subscription-Token", self.token.clone())
                .query(&[
                    ("q", query.text.as_str()),
                    ("country", "CH"),
                    ("search_lang", language),
                    ("ui_lang", locale),
                    ("count", count.as_str()),
                    (
                        "spellcheck",
                        if options.spellcheck { "true" } else { "false" },
                    ),
                    ("safesearch", "strict"),
                    ("result_filter", "web"),
                    ("text_decorations", "false"),
                ])
                .send()
                .await
                .map_err(|_| DiscoveryError::Unavailable)?;
            tracing::info!(
                stage = "brave_cache",
                hit = false,
                status = response.status().as_u16()
            );
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                return Err(DiscoveryError::RateLimited);
            }
            if !response.status().is_success() {
                return Err(DiscoveryError::Unavailable);
            }
            let body: BraveResponse = read_json(response, 2 * 1024 * 1024)
                .await
                .map_err(|_| DiscoveryError::Unavailable)?;
            let candidates = body
                .web
                .into_iter()
                .flat_map(|web| web.results)
                .filter_map(|result| {
                    Some(Candidate {
                        url: Url::parse(&result.url).ok()?,
                        title: truncate(result.title.trim(), 200),
                        description: truncate(result.description.trim(), 400),
                    })
                })
                .take(options.count)
                .collect::<Vec<_>>();
            // Only successful, non-empty answers are cached so outages retry.
            if !candidates.is_empty() {
                self.store(key, &candidates);
            }
            Ok(candidates)
        })
    }
}
