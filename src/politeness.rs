//! Per-host politeness, adaptive rate limiting, and bounded response caching.
//!
//! Parallelizing across different hosts is fast and safe, while hitting the
//! same host with bursts trips WAF/load-balancer token buckets. This module
//! caps concurrent requests per host, enforces minimum spacing + adaptive
//! cooldowns on 429/503 responses, and caches recent successful downloads.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use reqwest::header;
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use url::Url;

const DEFAULT_HOST_CONCURRENCY: usize = 2;
const DEFAULT_HOST_SPACING: Duration = Duration::from_millis(250);
const MAX_HOST_ENTRIES: usize = 2048;
const CACHE_ENTRIES: usize = 256;
const DISK_CACHE_MAX_PAGES: usize = 512;
const CACHE_RETENTION: Duration = Duration::from_secs(30 * 60);
const MAX_CACHED_BODY: usize = 512 * 1024;

static HOST_CONCURRENCY: OnceLock<usize> = OnceLock::new();
static HOST_SPACING: OnceLock<Duration> = OnceLock::new();

pub fn configure(host_concurrency: usize, host_spacing_ms: u64) {
    let _ = HOST_CONCURRENCY.set(host_concurrency.clamp(1, 16));
    let _ = HOST_SPACING.set(Duration::from_millis(host_spacing_ms.min(10_000)));
}

fn host_concurrency() -> usize {
    HOST_CONCURRENCY
        .get()
        .copied()
        .unwrap_or(DEFAULT_HOST_CONCURRENCY)
}

fn host_spacing() -> Duration {
    HOST_SPACING.get().copied().unwrap_or(DEFAULT_HOST_SPACING)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchTier {
    HonestHttp,
    Rendered,
    Sidecar,
}

#[derive(Clone)]
pub struct CachedResponse {
    pub final_url: Url,
    pub status: u16,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
    pub truncated: bool,
    pub etag: Option<header::HeaderValue>,
    pub last_modified: Option<header::HeaderValue>,
    pub freshness: Duration,
}

impl CachedResponse {
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        (
            &self.final_url,
            &self.content_type,
            &self.bytes,
            self.truncated,
        )
            .hash(&mut hash);
        hash.finish()
    }
}

struct HostState {
    semaphore: Arc<Semaphore>,
    next_allowed: tokio::sync::Mutex<tokio::time::Instant>,
    cooldown_until: Mutex<Option<Instant>>,
    crawl_delay: Mutex<Option<Duration>>,
    preferred_tier: Mutex<Option<(Instant, FetchTier)>>,
    consecutive_failures: Mutex<u32>,
    circuit_open_until: Mutex<Option<Instant>>,
    ewma_latency_ms: Mutex<Option<f64>>,
    last_used: Mutex<Instant>,
}

impl HostState {
    fn new() -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(host_concurrency())),
            next_allowed: tokio::sync::Mutex::new(tokio::time::Instant::now()),
            cooldown_until: Mutex::new(None),
            crawl_delay: Mutex::new(None),
            preferred_tier: Mutex::new(None),
            consecutive_failures: Mutex::new(0),
            circuit_open_until: Mutex::new(None),
            ewma_latency_ms: Mutex::new(None),
            last_used: Mutex::new(Instant::now()),
        }
    }

    fn touch(&self) {
        if let Ok(mut last) = self.last_used.lock() {
            *last = Instant::now();
        }
    }

    fn effective_spacing(&self) -> Duration {
        let base = host_spacing();
        let robot = self.crawl_delay.lock().ok().and_then(|d| *d);
        match robot {
            Some(delay) => base.max(delay.min(Duration::from_secs(10))),
            None => base,
        }
    }
}

pub struct HostPermit {
    _permit: OwnedSemaphorePermit,
}

#[derive(Serialize, Deserialize)]
struct DiskCachedResponse {
    key: String,
    stored_unix_secs: u64,
    final_url: Url,
    status: u16,
    content_type: Option<String>,
    bytes_b64: String,
    truncated: bool,
    etag: Option<String>,
    last_modified: Option<String>,
    freshness_secs: u64,
}

/// Deterministic 128-bit FNV-1a hex digest for safe, flat cache filenames.
pub(crate) fn cache_filename_hash(key: &str) -> String {
    let mut h1: u64 = 0xcbf2_9ce4_8422_2325;
    let mut h2: u64 = 0xaf63_dc4c_8601_ec8c;
    for &b in key.as_bytes() {
        h1 ^= u64::from(b);
        h1 = h1.wrapping_mul(0x0000_0100_0000_01b3);
        h2 ^= u64::from(b.wrapping_add(0x5a));
        h2 = h2.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h1:016x}{h2:016x}")
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn prune_disk_dir(dir: &Path, max_files: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.ends_with(".json") && !n.ends_with(".tmp"))
        })
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((e.path(), mtime))
        })
        .collect();
    if files.len() > max_files {
        files.sort_by_key(|(_, m)| *m);
        let remove_n = files.len().saturating_sub(max_files) + (max_files / 8).max(1);
        for (path, _) in files.into_iter().take(remove_n) {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub struct Politeness {
    hosts: Mutex<HashMap<String, Arc<HostState>>>,
    cache: Mutex<HashMap<String, (Instant, CachedResponse)>>,
    disk_dir: Mutex<Option<PathBuf>>,
    cache_hits_mem: AtomicU64,
    cache_hits_disk: AtomicU64,
    cache_misses: AtomicU64,
}

static GLOBAL: OnceLock<Politeness> = OnceLock::new();

pub fn global() -> &'static Politeness {
    GLOBAL.get_or_init(Politeness::new)
}

impl Politeness {
    pub fn new() -> Self {
        Self {
            hosts: Mutex::new(HashMap::new()),
            cache: Mutex::new(HashMap::new()),
            disk_dir: Mutex::new(None),
            cache_hits_mem: AtomicU64::new(0),
            cache_hits_disk: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
        }
    }

    /// Configures an optional persistent state directory (`QUELLE_STATE_DIR`)
    /// so cached HTTP bodies and ETag/Last-Modified validators survive restarts.
    pub fn set_state_dir(&self, state_dir: Option<PathBuf>) {
        let dir = state_dir.and_then(|root| {
            let sub = root.join("cache").join("pages");
            std::fs::create_dir_all(&sub).ok().map(|()| sub)
        });
        if let Ok(mut guard) = self.disk_dir.lock() {
            *guard = dir;
        }
    }

    /// Returns `(mem_hits, disk_hits, misses)` for page-cache observability.
    pub fn cache_stats(&self) -> (u64, u64, u64) {
        (
            self.cache_hits_mem.load(Ordering::Relaxed),
            self.cache_hits_disk.load(Ordering::Relaxed),
            self.cache_misses.load(Ordering::Relaxed),
        )
    }

    /// Returns `(tracked_hosts, open_circuits, slow_hosts, cached_entries)` for observability.
    pub fn host_summary(&self) -> (usize, usize, usize, usize) {
        let now = Instant::now();
        let (tracked, open, slow) = self.hosts.lock().map_or((0, 0, 0), |map| {
            let mut open_cnt = 0usize;
            let mut slow_cnt = 0usize;
            for state in map.values() {
                if state
                    .circuit_open_until
                    .lock()
                    .ok()
                    .and_then(|g| *g)
                    .is_some_and(|until| until > now)
                {
                    open_cnt += 1;
                }
                if state
                    .ewma_latency_ms
                    .lock()
                    .ok()
                    .and_then(|g| *g)
                    .is_some_and(|ms| ms >= 6_000.0)
                {
                    slow_cnt += 1;
                }
            }
            (map.len(), open_cnt, slow_cnt)
        });
        let cached = self.cache.lock().map_or(0, |c| c.len());
        (tracked, open, slow, cached)
    }

    fn host_state(&self, host: &str) -> Arc<HostState> {
        let key = host.to_ascii_lowercase();
        let mut map = self.hosts.lock().expect("host map");
        if let Some(state) = map.get(&key) {
            state.touch();
            return Arc::clone(state);
        }
        if map.len() >= MAX_HOST_ENTRIES {
            // Evict entries that haven't been touched recently.
            let cutoff = Instant::now() - Duration::from_secs(300);
            map.retain(|_, v| v.last_used.lock().map(|t| *t >= cutoff).unwrap_or(false));
            if map.len() >= MAX_HOST_ENTRIES
                && let Some(oldest) = map
                    .iter()
                    .min_by_key(|(_, v)| {
                        v.last_used
                            .lock()
                            .map(|t| *t)
                            .unwrap_or_else(|_| Instant::now())
                    })
                    .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        let state = Arc::new(HostState::new());
        map.insert(key, Arc::clone(&state));
        state
    }

    /// Records a robots.txt Crawl-delay for a host so subsequent requests
    /// respect it automatically.
    pub fn set_crawl_delay(&self, host: &str, delay: Duration) {
        let state = self.host_state(host);
        if let Ok(mut slot) = state.crawl_delay.lock() {
            *slot = Some(delay.min(Duration::from_secs(10)));
        }
    }

    /// Returns the last winning fetch tier for `host` (cached for 30 minutes)
    /// so escalation can reuse a previously successful renderer.
    pub fn preferred_tier(&self, host: &str) -> Option<FetchTier> {
        let state = self.host_state(host);
        let guard = state.preferred_tier.lock().ok()?;
        guard
            .filter(|(stored, _)| stored.elapsed() <= Duration::from_secs(30 * 60))
            .map(|(_, tier)| tier)
    }

    pub fn record_tier(&self, host: &str, tier: FetchTier) {
        let state = self.host_state(host);
        if let Ok(mut guard) = state.preferred_tier.lock() {
            *guard = Some((Instant::now(), tier));
        }
    }

    /// Acquires a per-host concurrency slot and waits for any active spacing
    /// or rate-limit cooldown, bounded by `deadline`.
    pub async fn acquire(&self, host: &str, deadline: tokio::time::Instant) -> Option<HostPermit> {
        let state = self.host_state(host);
        let permit =
            tokio::time::timeout_at(deadline, Arc::clone(&state.semaphore).acquire_owned())
                .await
                .ok()?
                .ok()?;

        // Honor any active cooldown from a previous 429/503.
        let cooldown_wait = state
            .cooldown_until
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .and_then(|until| until.checked_duration_since(Instant::now()));
        if let Some(wait) = cooldown_wait {
            let target = tokio::time::Instant::now() + wait;
            if target >= deadline {
                return None;
            }
            tokio::time::sleep_until(target).await;
        }

        // Enforce per-host request spacing.
        let spacing = state.effective_spacing();
        if !spacing.is_zero() {
            let wait_result = tokio::time::timeout_at(deadline, async {
                let mut next = state.next_allowed.lock().await;
                let now = tokio::time::Instant::now();
                if *next > now {
                    tokio::time::sleep_until(*next).await;
                }
                *next = tokio::time::Instant::now() + spacing;
            })
            .await;
            if wait_result.is_err() {
                return None;
            }
        }

        Some(HostPermit { _permit: permit })
    }

    /// Records a throttle event (429 / 503) for a host so other workers back
    /// off cleanly.
    pub fn record_throttle(&self, host: &str, wait: Duration) {
        let state = self.host_state(host);
        let bounded = wait.clamp(Duration::from_millis(500), Duration::from_secs(60));
        let until = Instant::now() + bounded;
        if let Ok(mut guard) = state.cooldown_until.lock() {
            *guard = Some(guard.map_or(until, |prior| prior.max(until)));
        }
    }

    pub fn record_host_success(&self, host: &str, elapsed: Duration) {
        let state = self.host_state(host);
        if let Ok(mut fails) = state.consecutive_failures.lock() {
            *fails = 0;
        }
        if let Ok(mut open) = state.circuit_open_until.lock() {
            *open = None;
        }
        #[allow(clippy::cast_precision_loss)]
        let sample_ms = elapsed.as_millis() as f64;
        if let Ok(mut ewma) = state.ewma_latency_ms.lock() {
            *ewma = Some(match *ewma {
                Some(prev) => prev * 0.65 + sample_ms * 0.35,
                None => sample_ms,
            });
        }
    }

    pub fn record_host_failure(&self, host: &str) {
        let state = self.host_state(host);
        let count = if let Ok(mut fails) = state.consecutive_failures.lock() {
            *fails = fails.saturating_add(1);
            *fails
        } else {
            1
        };
        if count >= 2 {
            let cooldown = if count >= 4 {
                Duration::from_secs(300)
            } else {
                Duration::from_secs(120)
            };
            if let Ok(mut open) = state.circuit_open_until.lock() {
                *open = Some(Instant::now() + cooldown);
            }
        }
    }

    pub fn is_circuit_open(&self, host: &str) -> bool {
        let key = host.to_ascii_lowercase();
        let state = {
            let Ok(map) = self.hosts.lock() else {
                return false;
            };
            let Some(s) = map.get(&key) else {
                return false;
            };
            Arc::clone(s)
        };
        state
            .circuit_open_until
            .lock()
            .ok()
            .and_then(|g| *g)
            .is_some_and(|until| until > Instant::now())
    }

    pub fn host_latency_ms(&self, host: &str) -> Option<u64> {
        let key = host.to_ascii_lowercase();
        let state = {
            let map = self.hosts.lock().ok()?;
            Arc::clone(map.get(&key)?)
        };
        let ewma = state.ewma_latency_ms.lock().ok().and_then(|g| *g)?;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some(ewma.round() as u64)
    }

    /// Tightens the per-step HTTP timeout for hosts that recently failed or
    /// exhibit chronically slow EWMA response times (>6 s).
    pub fn host_step_budget(&self, host: &str, base: Duration) -> Duration {
        let key = host.to_ascii_lowercase();
        let state = {
            let Ok(map) = self.hosts.lock() else {
                return base;
            };
            let Some(s) = map.get(&key) else {
                return base;
            };
            Arc::clone(s)
        };
        let failures = state.consecutive_failures.lock().ok().map_or(0, |g| *g);
        let slow = state
            .ewma_latency_ms
            .lock()
            .ok()
            .and_then(|g| *g)
            .is_some_and(|ms| ms >= 6_000.0);
        if failures >= 1 || slow {
            base.min(Duration::from_secs(5))
        } else {
            base
        }
    }

    fn disk_load(&self, url: &Url) -> Option<(Duration, CachedResponse)> {
        let dir = self.disk_dir.lock().ok()?.clone()?;
        let path = dir.join(format!("{}.json", cache_filename_hash(url.as_str())));
        let raw = std::fs::read(&path).ok()?;
        let disk: DiskCachedResponse = serde_json::from_slice(&raw).ok()?;
        if disk.key != url.as_str() {
            return None;
        }
        let age = Duration::from_secs(unix_now_secs().saturating_sub(disk.stored_unix_secs));
        if age >= CACHE_RETENTION {
            let _ = std::fs::remove_file(&path);
            return None;
        }
        let bytes = BASE64.decode(&disk.bytes_b64).ok()?;
        if bytes.len() > MAX_CACHED_BODY {
            return None;
        }
        let entry = CachedResponse {
            final_url: disk.final_url,
            status: disk.status,
            content_type: disk.content_type,
            bytes,
            truncated: disk.truncated,
            etag: disk
                .etag
                .and_then(|v| header::HeaderValue::from_str(&v).ok()),
            last_modified: disk
                .last_modified
                .and_then(|v| header::HeaderValue::from_str(&v).ok()),
            freshness: Duration::from_secs(disk.freshness_secs),
        };
        Some((age, entry))
    }

    fn disk_store(&self, url: &Url, entry: &CachedResponse) {
        let Some(dir) = self.disk_dir.lock().ok().and_then(|g| g.clone()) else {
            return;
        };
        let disk = DiskCachedResponse {
            key: url.as_str().to_owned(),
            stored_unix_secs: unix_now_secs(),
            final_url: entry.final_url.clone(),
            status: entry.status,
            content_type: entry.content_type.clone(),
            bytes_b64: BASE64.encode(&entry.bytes),
            truncated: entry.truncated,
            etag: entry
                .etag
                .as_ref()
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
            last_modified: entry
                .last_modified
                .as_ref()
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
            freshness_secs: entry.freshness.as_secs(),
        };
        let Ok(bytes) = serde_json::to_vec(&disk) else {
            return;
        };
        let hash = cache_filename_hash(url.as_str());
        let target = dir.join(format!("{hash}.json"));
        let tmp = dir.join(format!("{hash}.json.tmp"));
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &target);
            prune_disk_dir(&dir, DISK_CACHE_MAX_PAGES);
        }
    }

    pub fn cache_get(&self, url: &Url, max_age: Duration) -> Option<CachedResponse> {
        if max_age.is_zero() {
            return None;
        }
        if let Ok(cache) = self.cache.lock()
            && let Some((stored, entry)) = cache.get(url.as_str())
            && stored.elapsed() < max_age.min(entry.freshness)
        {
            self.cache_hits_mem.fetch_add(1, Ordering::Relaxed);
            return Some(entry.clone());
        }
        if let Some((age, entry)) = self.disk_load(url)
            && age < max_age.min(entry.freshness)
        {
            if let Ok(mut cache) = self.cache.lock() {
                let stored = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
                cache.insert(url.as_str().to_owned(), (stored, entry.clone()));
            }
            self.cache_hits_disk.fetch_add(1, Ordering::Relaxed);
            return Some(entry);
        }
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// Validators may outlive freshness, but never allow serving stale bodies
    /// without a fresh upstream 304 response.
    pub fn cache_stale(&self, url: &Url) -> Option<CachedResponse> {
        if let Ok(cache) = self.cache.lock()
            && let Some((stored, entry)) = cache.get(url.as_str())
            && stored.elapsed() < CACHE_RETENTION
        {
            return Some(entry.clone());
        }
        if let Some((age, entry)) = self.disk_load(url) {
            if let Ok(mut cache) = self.cache.lock() {
                let stored = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
                cache.insert(url.as_str().to_owned(), (stored, entry.clone()));
            }
            return Some(entry);
        }
        None
    }

    pub fn cache_remove(&self, url: &Url) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.remove(url.as_str());
        }
        if let Some(dir) = self.disk_dir.lock().ok().and_then(|g| g.clone()) {
            let path = dir.join(format!("{}.json", cache_filename_hash(url.as_str())));
            let _ = std::fs::remove_file(path);
        }
    }

    pub fn cache_put(&self, url: &Url, entry: CachedResponse) {
        if entry.bytes.len() > MAX_CACHED_BODY {
            return;
        }
        self.disk_store(url, &entry);
        let Ok(mut cache) = self.cache.lock() else {
            return;
        };
        cache.retain(|_, (stored, _)| stored.elapsed() <= CACHE_RETENTION);
        if cache.len() >= CACHE_ENTRIES
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (stored, _))| *stored)
                .map(|(k, _)| k.clone())
        {
            cache.remove(&oldest);
        }
        cache.insert(url.as_str().to_owned(), (Instant::now(), entry));
    }
}

/// Parses an HTTP `Retry-After` header (delta-seconds or HTTP-date), bounded
/// to a sensible retry window.
pub fn parse_retry_after(headers: &header::HeaderMap, default: Duration) -> Duration {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.trim()
                .parse::<u64>()
                .ok()
                .map(Duration::from_secs)
                .or_else(|| {
                    httpdate::parse_http_date(s.trim())
                        .ok()?
                        .duration_since(SystemTime::now())
                        .ok()
                })
        })
        .unwrap_or(default)
        .clamp(Duration::from_millis(500), Duration::from_secs(30))
}

/// Deterministic small jitter derived from the current subsec nanos to avoid
/// thundering herds without pulling in an RNG crate.
pub fn jitter(base: Duration) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let extra_ms = u64::from(nanos % 350);
    base + Duration::from_millis(extra_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circuit_breaker_opens_after_consecutive_failures_and_resets_on_success() {
        let p = Politeness::new();
        assert!(!p.is_circuit_open("slow.example.org"));
        p.record_host_failure("slow.example.org");
        assert!(!p.is_circuit_open("slow.example.org"));
        // Tightens budget after first failure
        assert!(
            p.host_step_budget("slow.example.org", Duration::from_secs(10))
                < Duration::from_secs(10)
        );
        p.record_host_failure("slow.example.org");
        assert!(p.is_circuit_open("slow.example.org"));

        p.record_host_success("slow.example.org", Duration::from_millis(250));
        assert!(!p.is_circuit_open("slow.example.org"));
        assert_eq!(p.host_latency_ms("slow.example.org"), Some(250));
    }

    #[test]
    fn ewma_latency_tracks_slow_origins_and_caps_step_budget() {
        let p = Politeness::new();
        p.record_host_success("tarpit.example.org", Duration::from_secs(9));
        let ewma = p.host_latency_ms("tarpit.example.org").unwrap();
        assert!(ewma >= 8000);
        assert!(
            p.host_step_budget("tarpit.example.org", Duration::from_secs(10))
                <= Duration::from_secs(6)
        );
    }

    #[test]
    fn disk_cache_persists_bodies_and_etag_validators_across_restarts() {
        let dir = std::env::temp_dir().join(format!(
            "quelle-politeness-disk-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let url = Url::parse("https://docs.example.org/rfc9110").unwrap();

        // Process 1 writes cached response with ETag and Last-Modified
        let p1 = Politeness::new();
        p1.set_state_dir(Some(dir.clone()));
        p1.cache_put(
            &url,
            CachedResponse {
                final_url: url.clone(),
                status: 200,
                content_type: Some("text/html; charset=utf-8".to_owned()),
                bytes: b"<html><body>HTTP Semantics</body></html>".to_vec(),
                truncated: false,
                etag: Some(header::HeaderValue::from_static("\"v1-etag\"")),
                last_modified: Some(header::HeaderValue::from_static(
                    "Wed, 21 Oct 2026 07:28:00 GMT",
                )),
                freshness: Duration::from_secs(600),
            },
        );

        // Process 2 (fresh in-memory map) loads directly from disk
        let p2 = Politeness::new();
        p2.set_state_dir(Some(dir.clone()));
        let hit = p2
            .cache_get(&url, Duration::from_secs(300))
            .expect("disk cache hit across restart");
        assert_eq!(hit.bytes, b"<html><body>HTTP Semantics</body></html>");
        assert_eq!(
            hit.etag.as_ref().and_then(|v| v.to_str().ok()),
            Some("\"v1-etag\"")
        );
        assert_eq!(p2.cache_stats().1, 1, "disk hit counter must increment");

        // Even when max_age is 0 (revalidation), cache_stale returns validators from disk!
        let p3 = Politeness::new();
        p3.set_state_dir(Some(dir.clone()));
        assert!(p3.cache_get(&url, Duration::ZERO).is_none());
        let stale = p3
            .cache_stale(&url)
            .expect("disk stale validator must survive restart");
        assert_eq!(
            stale.last_modified.as_ref().and_then(|v| v.to_str().ok()),
            Some("Wed, 21 Oct 2026 07:28:00 GMT")
        );

        // Removing clears both memory and disk
        p3.cache_remove(&url);
        let p4 = Politeness::new();
        p4.set_state_dir(Some(dir.clone()));
        assert!(p4.cache_stale(&url).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
