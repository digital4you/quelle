use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use url::Url;

mod providers;
pub use providers::configure_engines;
#[cfg(test)]
use providers::{HtmlEngine as Kind, parse_search_html};

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

#[derive(Clone, Copy, Debug)]
pub enum DiscoveryError {
    RateLimited(Duration),
    Unavailable,
    NotConfigured,
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
    fn spacing(&self) -> Duration {
        Duration::ZERO
    }
    fn search<'a>(&'a self, query: &'a Query, options: SearchOptions) -> DiscoveryFuture<'a>;
}

type CacheKey = (String, Language, SearchOptions);

struct Slot {
    engine: Box<dyn SearchEngine>,
    cooldown: Mutex<Option<Instant>>,
    next_request: tokio::sync::Mutex<tokio::time::Instant>,
}

impl Slot {
    fn check_cooldown(&self) -> Result<(), DiscoveryError> {
        match self
            .cooldown
            .lock()
            .expect("engine cooldown")
            .and_then(|until| until.checked_duration_since(Instant::now()))
        {
            Some(wait) => Err(DiscoveryError::RateLimited(wait)),
            None => Ok(()),
        }
    }

    async fn ready(&self) -> Result<(), DiscoveryError> {
        self.check_cooldown()?;
        let spacing = self.engine.spacing();
        if spacing.is_zero() {
            return Ok(());
        }
        // A short queue allows one planner's queries to share a paced engine.
        // Recheck after waiting: an earlier request may have received a 429.
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut next = self.next_request.lock().await;
            tokio::time::sleep_until(*next).await;
            self.check_cooldown()?;
            *next = tokio::time::Instant::now() + spacing;
            Ok(())
        })
        .await
        .unwrap_or(Err(DiscoveryError::Unavailable))
    }
}

pub struct EnginePool {
    slots: Vec<Slot>,
    next: AtomicUsize,
    cache: Mutex<HashMap<CacheKey, (Instant, Vec<Candidate>)>>,
}

impl EnginePool {
    fn new(engines: Vec<Box<dyn SearchEngine>>) -> Self {
        Self {
            slots: engines
                .into_iter()
                .map(|engine| Slot {
                    engine,
                    cooldown: Mutex::new(None),
                    next_request: tokio::sync::Mutex::new(tokio::time::Instant::now()),
                })
                .collect(),
            next: AtomicUsize::new(0),
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

    /// One initial attempt per query; extra attempts share the request's retry budget.
    pub async fn lookup(
        &self,
        query: &Query,
        options: SearchOptions,
        retries: &AtomicUsize,
    ) -> Result<Vec<Candidate>, DiscoveryError> {
        let key = (query.text.clone(), query.language, options);
        static FLIGHTS: std::sync::OnceLock<crate::cache::Cache<()>> = std::sync::OnceLock::new();
        let flight_key = serde_json::to_string(&(
            &query.text,
            query.language,
            options.count,
            options.spellcheck,
        ))
        .map_err(|_| DiscoveryError::Unavailable)?;
        let _flight = FLIGHTS
            .get_or_init(|| crate::cache::Cache::new(1, 1))
            .flight(
                &flight_key,
                tokio::time::Instant::now() + Duration::from_secs(20),
            )
            .await
            .map_err(|_| DiscoveryError::Unavailable)?;
        if let Some(hits) = self.cached(&key) {
            tracing::info!(stage = "discovery_cache", hit = true);
            return Ok(hits);
        }
        if self.slots.is_empty() {
            return Err(DiscoveryError::NotConfigured);
        }
        let start = self.next.fetch_add(1, Ordering::Relaxed) % self.slots.len();
        let mut attempted = false;
        let mut empty = false;
        let mut unavailable = false;
        let mut limited = Duration::ZERO;
        for slot in self.slots[start..].iter().chain(&self.slots[..start]) {
            if let Err(error) = slot.ready().await {
                match error {
                    DiscoveryError::RateLimited(wait) => limited = limited.max(wait),
                    _ => unavailable = true,
                }
                continue;
            }
            if attempted
                && retries
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                    .is_err()
            {
                break;
            }
            attempted = true;
            let now = Instant::now();
            let result = slot.engine.search(query, options).await;
            tracing::info!(stage = "search_engine", engine = slot.engine.name(), elapsed_ms = now.elapsed().as_millis() as u64,
                candidates = result.as_ref().map_or(0, Vec::len), error = ?result.as_ref().err());
            match result {
                Ok(hits) if hits.is_empty() => empty = true,
                Ok(hits) => {
                    self.store(key, &hits);
                    return Ok(hits);
                }
                Err(DiscoveryError::RateLimited(wait)) => {
                    let mut cooldown = slot.cooldown.lock().expect("engine cooldown");
                    let until = Instant::now() + wait;
                    *cooldown = Some(cooldown.map_or(until, |prior| prior.max(until)));
                    limited = limited.max(wait);
                }
                Err(_) => unavailable = true,
            }
        }
        if empty {
            Ok(vec![])
        } else if unavailable {
            Err(DiscoveryError::Unavailable)
        } else {
            Err(DiscoveryError::RateLimited(limited))
        }
    }
}

#[cfg(test)]
mod tests;
