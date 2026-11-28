use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use futures_util::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use url::Url;

mod providers;
mod router;
pub use providers::configure_engines;
#[cfg(test)]
use providers::{
    HtmlEngine as Kind, ddg_locale, extract_rfc_number, parse_arxiv_xml, parse_crossref_json,
    parse_gdelt_json, parse_github_json, parse_hackernews_json, parse_ietf_json, parse_mdn_json,
    parse_openalex_json, parse_pubmed_esearch_json, parse_pubmed_esummary_json, parse_search_html,
    parse_searxng_json, parse_stackexchange_json, query_has_academic_intent,
    query_has_biomed_intent, query_has_ietf_intent, query_has_news_intent, query_has_repo_intent,
    query_has_tech_intent, query_has_web_intent, site_filter_allows,
};

pub const RESULTS_PER_QUERY: usize = 20;
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const CACHE_ENTRIES: usize = 512;
/// Once a primary web engine returns hits in parallel fuse mode, give slower
/// engines a short grace window to contribute consensus votes before fusing.
const FUSE_GRACE_WINDOW: Duration = Duration::from_millis(600);
/// If only secondary/specialized engines have replied while a primary web
/// engine is still in flight, wait a bit longer so primary results aren't cut off.
const FUSE_PRIMARY_GRACE_WINDOW: Duration = Duration::from_millis(1400);
/// Reciprocal Rank Fusion smoothing constant tuned for top-20 web results.
const RRF_K: f64 = 20.0;

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
    fn weight(&self) -> f64 {
        1.0
    }
    fn query_weight(&self, _query: &Query) -> f64 {
        self.weight()
    }
    fn is_primary(&self) -> bool {
        self.weight() >= 1.0
    }
    fn supports_query(&self, _query: &Query) -> bool {
        true
    }
    fn spacing(&self) -> Duration {
        Duration::ZERO
    }
    fn search<'a>(&'a self, query: &'a Query, options: SearchOptions) -> DiscoveryFuture<'a>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscoveryMode {
    Rotate,
    Fuse,
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

    fn record_rate_limit(&self, wait: Duration) {
        let mut cooldown = self.cooldown.lock().expect("engine cooldown");
        let until = Instant::now() + wait;
        *cooldown = Some(cooldown.map_or(until, |prior| prior.max(until)));
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
    mode: DiscoveryMode,
    next: AtomicUsize,
    cache: Mutex<HashMap<CacheKey, (Instant, Vec<Candidate>)>>,
}

impl EnginePool {
    #[cfg(test)]
    fn new(engines: Vec<Box<dyn SearchEngine>>) -> Self {
        Self::with_mode(engines, DiscoveryMode::Rotate)
    }

    fn with_mode(engines: Vec<Box<dyn SearchEngine>>, mode: DiscoveryMode) -> Self {
        Self {
            slots: engines
                .into_iter()
                .map(|engine| Slot {
                    engine,
                    cooldown: Mutex::new(None),
                    next_request: tokio::sync::Mutex::new(tokio::time::Instant::now()),
                })
                .collect(),
            mode,
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

    /// Looks up candidates using either parallel multi-engine fan-out with RRF
    /// consensus fusion (`DiscoveryMode::Fuse`) or single-engine rotation
    /// (`DiscoveryMode::Rotate`).
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
        if self.mode == DiscoveryMode::Fuse && self.slots.len() > 1 {
            return self.lookup_fused(key, query, options).await;
        }
        self.lookup_rotating(key, query, options, retries).await
    }

    async fn lookup_rotating(
        &self,
        key: CacheKey,
        query: &Query,
        options: SearchOptions,
        retries: &AtomicUsize,
    ) -> Result<Vec<Candidate>, DiscoveryError> {
        let start = self.next.fetch_add(1, Ordering::Relaxed) % self.slots.len();
        let mut attempted = false;
        let mut empty = false;
        let mut unavailable = false;
        let mut limited = Duration::ZERO;
        for slot in self.slots[start..].iter().chain(&self.slots[..start]) {
            if !slot.engine.supports_query(query) {
                continue;
            }
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
            tracing::info!(
                stage = "search_engine",
                engine = slot.engine.name(),
                elapsed_ms = now.elapsed().as_millis() as u64,
                candidates = result.as_ref().map_or(0, Vec::len),
                error = ?result.as_ref().err()
            );
            match result {
                Ok(hits) if hits.is_empty() => empty = true,
                Ok(hits) => {
                    self.store(key, &hits);
                    return Ok(hits);
                }
                Err(DiscoveryError::RateLimited(wait)) => {
                    slot.record_rate_limit(wait);
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

    async fn lookup_fused(
        &self,
        key: CacheKey,
        query: &Query,
        options: SearchOptions,
    ) -> Result<Vec<Candidate>, DiscoveryError> {
        let mut eligible: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.engine.supports_query(query))
            .map(|(idx, _)| idx)
            .collect();
        if eligible.is_empty() {
            eligible = (0..self.slots.len()).collect();
        }

        let mut empty = false;
        let mut unavailable = false;
        let mut limited = Duration::ZERO;
        let mut primary_in_flight = 0usize;
        let mut in_flight = FuturesUnordered::new();

        for idx in eligible {
            let slot = &self.slots[idx];
            if let Err(DiscoveryError::RateLimited(wait)) = slot.check_cooldown() {
                limited = limited.max(wait);
                continue;
            }
            if slot.engine.is_primary() {
                primary_in_flight += 1;
            }
            in_flight.push(async move {
                if let Err(err) = slot.ready().await {
                    return (idx, Err(err));
                }
                let now = Instant::now();
                let result = slot.engine.search(query, options).await;
                tracing::info!(
                    stage = "search_engine",
                    engine = slot.engine.name(),
                    elapsed_ms = now.elapsed().as_millis() as u64,
                    candidates = result.as_ref().map_or(0, Vec::len),
                    error = ?result.as_ref().err()
                );
                (idx, result)
            });
        }

        let mut batches: Vec<(f64, Vec<Candidate>)> = Vec::new();
        let mut grace_deadline: Option<tokio::time::Instant> = None;

        while !in_flight.is_empty() {
            let next_item = match grace_deadline {
                Some(deadline) => match tokio::time::timeout_at(deadline, in_flight.next()).await {
                    Ok(item) => item,
                    Err(_) => break,
                },
                None => in_flight.next().await,
            };
            let Some((idx, result)) = next_item else {
                break;
            };
            let slot = &self.slots[idx];
            let was_primary = slot.engine.is_primary();
            if was_primary {
                primary_in_flight = primary_in_flight.saturating_sub(1);
            }
            match result {
                Ok(hits) if hits.is_empty() => empty = true,
                Ok(hits) => {
                    batches.push((slot.engine.query_weight(query), hits));
                    let now = tokio::time::Instant::now();
                    let target = if primary_in_flight > 0 && !was_primary {
                        now + FUSE_PRIMARY_GRACE_WINDOW
                    } else {
                        now + FUSE_GRACE_WINDOW
                    };
                    grace_deadline = Some(match grace_deadline {
                        Some(existing) if was_primary || primary_in_flight == 0 => {
                            existing.min(now + FUSE_GRACE_WINDOW)
                        }
                        Some(existing) => existing,
                        None => target,
                    });
                }
                Err(DiscoveryError::RateLimited(wait)) => {
                    slot.record_rate_limit(wait);
                    limited = limited.max(wait);
                }
                Err(_) => unavailable = true,
            }
            // If all remaining primary engines failed/finished after a secondary
            // already supplied hits, tighten the grace window immediately.
            if primary_in_flight == 0 && !batches.is_empty() {
                let tight = tokio::time::Instant::now() + FUSE_GRACE_WINDOW;
                grace_deadline = Some(grace_deadline.map_or(tight, |d| d.min(tight)));
            }
        }

        if !batches.is_empty() {
            let fused = fuse_candidates(batches, options.count);
            self.store(key, &fused);
            return Ok(fused);
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

/// Merges candidate lists from multiple engines using weighted Reciprocal Rank
/// Fusion with a cross-engine consensus bonus, preserving the cleanest URL and
/// richest title/description across agreeing engines.
fn fuse_candidates(batches: Vec<(f64, Vec<Candidate>)>, count: usize) -> Vec<Candidate> {
    struct Entry {
        first_seen: usize,
        rrf_score: f64,
        engine_count: usize,
        candidate: Candidate,
    }

    let mut merged: HashMap<String, Entry> = HashMap::new();
    let mut seq = 0usize;

    for (weight, hits) in batches {
        let mut seen_in_engine = HashSet::new();
        for (rank, cand) in hits.into_iter().enumerate() {
            let key = canonical_fusion_key(&cand.url);
            if !seen_in_engine.insert(key.clone()) {
                continue;
            }
            #[allow(clippy::cast_precision_loss)]
            let contrib = weight / (RRF_K + (rank as f64) + 1.0);
            match merged.get_mut(&key) {
                Some(entry) => {
                    entry.rrf_score += contrib;
                    entry.engine_count += 1;
                    // Prefer the cleaner URL (e.g. without tracking params or HTTP -> HTTPS).
                    if cand.url.as_str().len() < entry.candidate.url.as_str().len()
                        || (cand.url.scheme() == "https" && entry.candidate.url.scheme() != "https")
                    {
                        entry.candidate.url = cand.url;
                    }
                    if cand.title.len() > entry.candidate.title.len() {
                        entry.candidate.title = cand.title;
                    }
                    if cand.description.len() > entry.candidate.description.len() {
                        entry.candidate.description = cand.description;
                    }
                }
                None => {
                    let first_seen = seq;
                    seq += 1;
                    merged.insert(
                        key,
                        Entry {
                            first_seen,
                            rrf_score: contrib,
                            engine_count: 1,
                            candidate: cand,
                        },
                    );
                }
            }
        }
    }

    let mut ranked: Vec<(f64, usize, Candidate)> = merged
        .into_values()
        .map(|entry| {
            #[allow(clippy::cast_precision_loss)]
            let consensus = 1.0 + 0.35 * (entry.engine_count.saturating_sub(1) as f64);
            (
                entry.rrf_score * consensus,
                entry.first_seen,
                entry.candidate,
            )
        })
        .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    ranked.into_iter().take(count).map(|(_, _, c)| c).collect()
}

fn canonical_fusion_key(url: &Url) -> String {
    let key = crate::selection::url_key(url);
    let key = key
        .strip_prefix("https://")
        .or_else(|| key.strip_prefix("http://"))
        .unwrap_or(&key);
    key.trim_end_matches('/').to_ascii_lowercase()
}

#[cfg(test)]
mod tests;
