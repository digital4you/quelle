//! Firecrawl/Crawl4AI-style `/api/map` and `/api/crawl` endpoints.
//!
//! - `POST /api/map`: fast URL discovery via robots.txt + sitemap.xml + page links.
//! - `POST /api/crawl`: recursive crawler starting from a single `url` OR a set
//!   of `urls`, supporting both async jobs (`GET/DELETE /api/crawl/{id}`) and
//!   synchronous mode (`sync: true`).

use std::{
    collections::{HashMap, HashSet, VecDeque, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant as StdInstant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::{Request, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use subtle::ConstantTimeEq;
use tokio::{sync::Semaphore, time::Instant};
use url::Url;

use crate::{
    App, admit,
    pages::{
        self, ExtractOptions, OutputFormat, PageError, RenderMode, ScrapeOptions, ScrapedPage,
    },
    problem, robots, selection,
};

const MAX_MAP_LIMIT: usize = 5000;
const DEFAULT_MAP_LIMIT: usize = 200;
const MAX_CRAWL_LIMIT: usize = 1000;
const DEFAULT_CRAWL_LIMIT: usize = 50;
const MAX_CRAWL_DEPTH: usize = 10;
const DEFAULT_CRAWL_DEPTH: usize = 2;
const MAX_SEED_URLS: usize = 100;
const MAX_STORED_JOBS: usize = 64;
const MAX_CONCURRENT_CRAWLS: usize = 4;
const ASYNC_CRAWL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const SYNC_CRAWL_TIMEOUT: Duration = Duration::from_secs(110);

static JOB_SEQ: AtomicU64 = AtomicU64::new(1);

// ---------------------------------------------------------------------------
// Crawl Job Store
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CrawlStatus {
    Running,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct CrawlErrorEntry {
    pub url: Url,
    pub error: &'static str,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChangeSummary {
    pub added: usize,
    pub updated: usize,
    pub unchanged: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CrawlJobSnapshot {
    pub id: String,
    pub status: CrawlStatus,
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<ChangeSummary>,
    pub data: Vec<ScrapedPage>,
    pub errors: Vec<CrawlErrorEntry>,
    pub elapsed_ms: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredPageSnapshot {
    hash: String,
    markdown: Option<String>,
}

struct CrawlJobState {
    id: String,
    status: CrawlStatus,
    started: StdInstant,
    finished_elapsed_ms: Option<u64>,
    total: usize,
    completed: usize,
    failed: usize,
    changes: Option<ChangeSummary>,
    page_snapshots: HashMap<String, StoredPageSnapshot>,
    data: Vec<ScrapedPage>,
    errors: Vec<CrawlErrorEntry>,
    cancel: Arc<AtomicBool>,
}

impl CrawlJobState {
    fn snapshot(&self) -> CrawlJobSnapshot {
        let elapsed_ms = self.finished_elapsed_ms.unwrap_or_else(|| {
            u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
        });
        CrawlJobSnapshot {
            id: self.id.clone(),
            status: self.status,
            total: self.total,
            completed: self.completed,
            failed: self.failed,
            changes: self.changes,
            data: self.data.clone(),
            errors: self.errors.clone(),
            elapsed_ms,
        }
    }
}

pub struct CrawlManager {
    jobs: Mutex<HashMap<String, Arc<Mutex<CrawlJobState>>>>,
    slots: Arc<Semaphore>,
    state_dir: Option<std::path::PathBuf>,
}

fn valid_crawl_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("crawl-") else {
        return false;
    };
    let Some((ts, seq)) = rest.split_once('-') else {
        return false;
    };
    !ts.is_empty()
        && !seq.is_empty()
        && ts.len() <= 24
        && seq.len() <= 16
        && ts.bytes().all(|b| b.is_ascii_hexdigit())
        && seq.bytes().all(|b| b.is_ascii_hexdigit())
}

impl CrawlManager {
    pub fn with_state_dir(state_dir: Option<std::path::PathBuf>) -> Self {
        if let Some(ref dir) = state_dir {
            let _ = std::fs::create_dir_all(dir);
        }
        Self {
            jobs: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT_CRAWLS)),
            state_dir,
        }
    }

    fn persist_snapshots(&self, id: &str, snapshots: &HashMap<String, StoredPageSnapshot>) {
        let Some(ref dir) = self.state_dir else {
            return;
        };
        if !valid_crawl_id(id) || snapshots.is_empty() {
            return;
        }
        let Ok(bytes) = serde_json::to_vec(snapshots) else {
            return;
        };
        let target = dir.join(format!("{id}.json"));
        let tmp = dir.join(format!("{id}.json.tmp"));
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &target);
        }
        // Keep at most MAX_STORED_JOBS persisted files on disk.
        if let Ok(entries) = std::fs::read_dir(dir) {
            let mut files: Vec<_> = entries
                .filter_map(Result::ok)
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.starts_with("crawl-") && n.ends_with(".json"))
                })
                .filter_map(|e| {
                    let mtime = e.metadata().ok()?.modified().ok()?;
                    Some((e.path(), mtime))
                })
                .collect();
            if files.len() > MAX_STORED_JOBS {
                files.sort_by_key(|(_, m)| *m);
                for (path, _) in files.into_iter().take(MAX_STORED_JOBS / 4) {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }

    fn load_persisted_snapshots(&self, id: &str) -> Option<HashMap<String, StoredPageSnapshot>> {
        let dir = self.state_dir.as_ref()?;
        if !valid_crawl_id(id) {
            return None;
        }
        let path = dir.join(format!("{id}.json"));
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    #[cfg(test)]
    fn save_test_snapshot(&self, id: &str, url_key: &str, hash: &str, md: &str) {
        let mut map = HashMap::new();
        map.insert(
            url_key.to_owned(),
            StoredPageSnapshot {
                hash: hash.to_owned(),
                markdown: Some(md.to_owned()),
            },
        );
        self.persist_snapshots(id, &map);
    }

    fn next_id() -> String {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let seq = JOB_SEQ.fetch_add(1, Ordering::Relaxed);
        format!("crawl-{ts:x}-{seq:04x}")
    }

    fn insert(&self, job: Arc<Mutex<CrawlJobState>>) -> String {
        let id = job.lock().expect("job lock").id.clone();
        let mut map = self.jobs.lock().expect("jobs map");
        if map.len() >= MAX_STORED_JOBS {
            // Evict oldest non-running job.
            if let Some(oldest) = map
                .iter()
                .filter_map(|(k, v)| {
                    let g = v.lock().ok()?;
                    (g.status != CrawlStatus::Running).then(|| (k.clone(), g.started))
                })
                .min_by_key(|(_, started)| *started)
                .map(|(k, _)| k)
            {
                map.remove(&oldest);
            }
        }
        map.insert(id.clone(), job);
        id
    }

    fn get(&self, id: &str) -> Option<Arc<Mutex<CrawlJobState>>> {
        self.jobs.lock().ok()?.get(id).cloned()
    }
}

// ---------------------------------------------------------------------------
// POST /api/map
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MapInput {
    url: Option<Url>,
    urls: Option<Vec<Url>>,
    search: Option<String>,
    #[serde(default)]
    include_subdomains: bool,
    #[serde(default = "default_true")]
    use_sitemap: bool,
    #[serde(default)]
    include_paths: Vec<String>,
    #[serde(default)]
    exclude_paths: Vec<String>,
    limit: Option<usize>,
}

fn default_true() -> bool {
    true
}

impl MapInput {
    fn seeds(&self) -> Vec<Url> {
        let mut out = Vec::new();
        if let Some(u) = &self.url {
            out.push(u.clone());
        }
        if let Some(list) = &self.urls {
            out.extend(list.iter().cloned());
        }
        out
    }

    fn valid(&self) -> bool {
        let seeds = self.seeds();
        if seeds.is_empty() || seeds.len() > MAX_SEED_URLS {
            return false;
        }
        if !seeds.iter().all(|u| pages::public_url(u.clone()).is_ok()) {
            return false;
        }
        if let Some(limit) = self.limit
            && !(1..=MAX_MAP_LIMIT).contains(&limit)
        {
            return false;
        }
        true
    }
}

#[derive(Serialize)]
struct MapOutput {
    links: Vec<Url>,
    total: usize,
}

pub async fn map_handle(State(app): State<Arc<App>>, req: Request<Body>) -> Response {
    let deadline = Instant::now() + Duration::from_secs(45);
    let (_permit, input) = match admit(&app, req, MapInput::valid).await {
        Ok(admitted) => admitted,
        Err(response) => return response,
    };

    let limit = input.limit.unwrap_or(DEFAULT_MAP_LIMIT);
    let seeds: Vec<Url> = input
        .seeds()
        .into_iter()
        .filter_map(|u| pages::public_url(u).ok())
        .collect();
    let allowed_hosts = seed_hosts(&seeds);
    let search_lower = input
        .search
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase);

    let mut discovered = Vec::new();
    let mut seen = HashSet::new();

    let accept_url = |u: &Url| -> bool {
        if !host_matches(u, &allowed_hosts, input.include_subdomains, false) {
            return false;
        }
        if !path_matches(u.path(), &input.include_paths, &input.exclude_paths) {
            return false;
        }
        if let Some(ref needle) = search_lower
            && !u.as_str().to_ascii_lowercase().contains(needle)
        {
            return false;
        }
        true
    };

    for seed in &seeds {
        if discovered.len() < limit && accept_url(seed) && seen.insert(seed.clone()) {
            discovered.push(seed.clone());
        }
    }

    // 1. Sitemaps per unique seed host
    if input.use_sitemap {
        let mut probed_origins = HashSet::new();
        for seed in &seeds {
            if discovered.len() >= limit || Instant::now() >= deadline {
                break;
            }
            let origin = origin_key(seed);
            if !probed_origins.insert(origin) {
                continue;
            }
            let policy = robots::fetch_robots(seed, deadline).await;
            let sm_urls = robots::discover_sitemap_urls(
                seed,
                &policy,
                limit.saturating_sub(discovered.len()),
                deadline,
            )
            .await;
            for u in sm_urls {
                if discovered.len() >= limit {
                    break;
                }
                if accept_url(&u) && seen.insert(u.clone()) {
                    discovered.push(u);
                }
            }
        }
    }

    // 2. Also extract links directly from seed pages
    let scrape_opts = ScrapeOptions {
        formats: vec![OutputFormat::Links],
        extract: ExtractOptions::default(),
        render: RenderMode::Auto,
        wait_for_ms: None,
        cache_max_age: Duration::from_secs(5 * 60),
        ..ScrapeOptions::default()
    };
    for seed in &seeds {
        if discovered.len() >= limit || Instant::now() >= deadline {
            break;
        }
        let Ok(permit) = app.search.page_slots.clone().acquire_owned().await else {
            break;
        };
        if let Ok(page) = pages::scrape(seed.clone(), &scrape_opts, permit, deadline).await
            && let Some(links) = page.links
        {
            for link in links {
                if discovered.len() >= limit {
                    break;
                }
                if accept_url(&link) && seen.insert(link.clone()) {
                    discovered.push(link);
                }
            }
        }
    }

    let total = discovered.len();
    Json(MapOutput {
        links: discovered,
        total,
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// POST /api/crawl, GET /api/crawl/{id}, DELETE /api/crawl/{id}
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CrawlInput {
    url: Option<Url>,
    urls: Option<Vec<Url>>,
    max_depth: Option<usize>,
    limit: Option<usize>,
    #[serde(default)]
    include_paths: Vec<String>,
    #[serde(default)]
    exclude_paths: Vec<String>,
    #[serde(default)]
    allow_subdomains: bool,
    #[serde(default)]
    allow_external_links: bool,
    #[serde(default = "default_true")]
    use_sitemap: bool,
    #[serde(default = "default_true")]
    respect_robots: bool,
    formats: Option<Vec<OutputFormat>>,
    #[serde(default = "default_true")]
    only_main_content: bool,
    #[serde(default)]
    include_tags: Vec<String>,
    #[serde(default)]
    exclude_tags: Vec<String>,
    #[serde(default)]
    render: RenderMode,
    wait_for: Option<u64>,
    wait_for_selector: Option<String>,
    scroll_steps: Option<u8>,
    max_age: Option<u64>,
    concurrency: Option<usize>,
    #[serde(default)]
    sync: bool,
    #[serde(default)]
    incremental: bool,
    since_crawl_id: Option<String>,
    previous_hashes: Option<HashMap<String, String>>,
    #[serde(default)]
    omit_unchanged_content: bool,
    max_tokens: Option<usize>,
    chunk_tokens: Option<usize>,
    focus_query: Option<String>,
    max_pages: Option<usize>,
}

impl CrawlInput {
    fn seeds(&self) -> Vec<Url> {
        let mut out = Vec::new();
        if let Some(u) = &self.url {
            out.push(u.clone());
        }
        if let Some(list) = &self.urls {
            out.extend(list.iter().cloned());
        }
        out
    }

    fn valid(&self) -> bool {
        let seeds = self.seeds();
        if seeds.is_empty() || seeds.len() > MAX_SEED_URLS {
            return false;
        }
        if !seeds.iter().all(|u| pages::public_url(u.clone()).is_ok()) {
            return false;
        }
        if let Some(d) = self.max_depth
            && d > MAX_CRAWL_DEPTH
        {
            return false;
        }
        if let Some(l) = self.limit
            && !(1..=MAX_CRAWL_LIMIT).contains(&l)
        {
            return false;
        }
        if let Some(c) = self.concurrency
            && !(1..=16).contains(&c)
        {
            return false;
        }
        if let Some(f) = &self.formats
            && f.is_empty()
        {
            return false;
        }
        if let Some(hashes) = &self.previous_hashes
            && hashes.len() > 2000
        {
            return false;
        }
        if let Some(t) = self.max_tokens
            && !(32..=200_000).contains(&t)
        {
            return false;
        }
        if let Some(t) = self.chunk_tokens
            && !(32..=4096).contains(&t)
        {
            return false;
        }
        if let Some(q) = &self.focus_query
            && q.chars().count() > 600
        {
            return false;
        }
        if let Some(p) = self.max_pages
            && !(1..=pages::MAX_PDF_MAX_PAGES).contains(&p)
        {
            return false;
        }
        if let Some(sel) = &self.wait_for_selector
            && !pages::valid_wait_selector(sel)
        {
            return false;
        }
        if let Some(steps) = self.scroll_steps
            && steps > 10
        {
            return false;
        }
        true
    }
}

struct CrawlConfig {
    seeds: Vec<Url>,
    max_depth: usize,
    limit: usize,
    include_paths: Vec<String>,
    exclude_paths: Vec<String>,
    allow_subdomains: bool,
    allow_external_links: bool,
    use_sitemap: bool,
    respect_robots: bool,
    scrape_options: ScrapeOptions,
    keep_links_in_output: bool,
    concurrency: usize,
    timeout: Duration,
    incremental: bool,
    omit_unchanged_content: bool,
    previous: HashMap<String, StoredPageSnapshot>,
}

pub async fn start_handle(State(app): State<Arc<App>>, req: Request<Body>) -> Response {
    let (api_permit, input) = match admit(&app, req, CrawlInput::valid).await {
        Ok(admitted) => admitted,
        Err(response) => return response,
    };

    let Ok(crawl_permit) = app.crawls.slots.clone().try_acquire_owned() else {
        return problem(StatusCode::SERVICE_UNAVAILABLE, "crawl_capacity");
    };

    let requested_formats = input
        .formats
        .clone()
        .unwrap_or_else(|| vec![OutputFormat::Markdown]);
    let keep_links_in_output = requested_formats.contains(&OutputFormat::Links);
    let mut fetch_formats = requested_formats;
    // Always extract links during crawl so we can expand the frontier.
    if !fetch_formats.contains(&OutputFormat::Links) {
        fetch_formats.push(OutputFormat::Links);
    }

    let incremental =
        input.incremental || input.since_crawl_id.is_some() || input.previous_hashes.is_some();

    let mut previous: HashMap<String, StoredPageSnapshot> = HashMap::new();
    if let Some(ref prev_id) = input.since_crawl_id {
        if let Some(prev_job) = app.crawls.get(prev_id) {
            if let Ok(guard) = prev_job.lock() {
                previous.extend(guard.page_snapshots.clone());
            }
        } else if let Some(persisted) = app.crawls.load_persisted_snapshots(prev_id) {
            previous.extend(persisted);
        } else {
            return problem(StatusCode::NOT_FOUND, "crawl_not_found");
        }
    }
    if let Some(ref hashes) = input.previous_hashes {
        for (raw_url, hash) in hashes {
            let key = Url::parse(raw_url)
                .ok()
                .map_or_else(|| raw_url.clone(), |u| selection::url_key(&u));
            previous.insert(
                key,
                StoredPageSnapshot {
                    hash: hash.clone(),
                    markdown: None,
                },
            );
        }
    }

    // Incremental crawls default to conditional revalidation (max_age = 0) so
    // ETag / Last-Modified 304 responses verify freshness without stale cache hits.
    let cache_max_age = match input.max_age {
        Some(secs) => Duration::from_secs(secs),
        None if incremental => Duration::ZERO,
        None => Duration::from_secs(300),
    };

    let config = CrawlConfig {
        seeds: input
            .seeds()
            .into_iter()
            .filter_map(|u| pages::public_url(u).ok())
            .collect(),
        max_depth: input.max_depth.unwrap_or(DEFAULT_CRAWL_DEPTH),
        limit: input.limit.unwrap_or(DEFAULT_CRAWL_LIMIT),
        include_paths: input.include_paths,
        exclude_paths: input.exclude_paths,
        allow_subdomains: input.allow_subdomains,
        allow_external_links: input.allow_external_links,
        use_sitemap: input.use_sitemap,
        respect_robots: input.respect_robots,
        scrape_options: ScrapeOptions {
            formats: fetch_formats,
            extract: ExtractOptions {
                only_main_content: input.only_main_content,
                include_tags: input.include_tags,
                exclude_tags: input.exclude_tags,
            },
            render: input.render,
            wait_for_ms: input.wait_for,
            wait_for_selector: input
                .wait_for_selector
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            scroll_steps: input.scroll_steps.unwrap_or(0),
            cache_max_age,
            max_tokens: input.max_tokens,
            chunk_tokens: input.chunk_tokens,
            focus_query: input
                .focus_query
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            max_pages: input.max_pages,
        },
        keep_links_in_output,
        concurrency: input.concurrency.unwrap_or(4).min(8),
        timeout: if input.sync {
            SYNC_CRAWL_TIMEOUT
        } else {
            ASYNC_CRAWL_TIMEOUT
        },
        incremental,
        omit_unchanged_content: input.omit_unchanged_content,
        previous,
    };

    let id = CrawlManager::next_id();
    let job = Arc::new(Mutex::new(CrawlJobState {
        id: id.clone(),
        status: CrawlStatus::Running,
        started: StdInstant::now(),
        finished_elapsed_ms: None,
        total: config.seeds.len(),
        completed: 0,
        failed: 0,
        changes: config.incremental.then(ChangeSummary::default),
        page_snapshots: HashMap::new(),
        data: Vec::new(),
        errors: Vec::new(),
        cancel: Arc::new(AtomicBool::new(false)),
    }));
    app.crawls.insert(Arc::clone(&job));

    if input.sync {
        let _api_permit = api_permit;
        let _crawl_permit = crawl_permit;
        run_crawl(Arc::clone(&app), Arc::clone(&job), config).await;
        let snapshot = job.lock().expect("job lock").snapshot();
        return Json(snapshot).into_response();
    }

    // Release the short-lived HTTP request slot immediately for async crawls;
    // the background task holds `crawl_permit` until completion.
    drop(api_permit);
    let app_bg = Arc::clone(&app);
    let job_bg = Arc::clone(&job);
    tokio::spawn(async move {
        let _crawl_permit = crawl_permit;
        run_crawl(app_bg, job_bg, config).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "id": id,
            "status": "running",
            "url": format!("/api/crawl/{id}"),
        })),
    )
        .into_response()
}

pub async fn status_handle(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    req: Request<Body>,
) -> Response {
    if !authorized(&app, &req) {
        return problem(StatusCode::UNAUTHORIZED, "backend_authentication_required");
    }
    let Some(job) = app.crawls.get(&id) else {
        return problem(StatusCode::NOT_FOUND, "crawl_not_found");
    };
    let snapshot = job.lock().expect("job lock").snapshot();
    Json(snapshot).into_response()
}

pub async fn cancel_handle(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    req: Request<Body>,
) -> Response {
    if !authorized(&app, &req) {
        return problem(StatusCode::UNAUTHORIZED, "backend_authentication_required");
    }
    let Some(job) = app.crawls.get(&id) else {
        return problem(StatusCode::NOT_FOUND, "crawl_not_found");
    };
    let mut guard = job.lock().expect("job lock");
    guard.cancel.store(true, Ordering::Relaxed);
    if guard.status == CrawlStatus::Running {
        guard.status = CrawlStatus::Cancelled;
        guard.finished_elapsed_ms =
            Some(u64::try_from(guard.started.elapsed().as_millis()).unwrap_or(u64::MAX));
    }
    let snapshot = guard.snapshot();
    Json(snapshot).into_response()
}

fn authorized(app: &App, req: &Request<Body>) -> bool {
    let expected = app.authorization.as_bytes();
    req.headers()
        .get(header::AUTHORIZATION)
        .is_some_and(|value| bool::from(value.as_bytes().ct_eq(expected)))
}

async fn run_crawl(app: Arc<App>, job: Arc<Mutex<CrawlJobState>>, config: CrawlConfig) {
    let deadline = Instant::now() + config.timeout;
    let cancel = job.lock().expect("job lock").cancel.clone();
    let allowed_hosts = seed_hosts(&config.seeds);

    let mut frontier: VecDeque<(Url, usize)> = VecDeque::new();
    let mut seen: HashSet<Url> = HashSet::new();
    let mut seen_content_hashes: HashSet<u64> = HashSet::new();
    let mut robots_by_origin: HashMap<String, robots::RobotsPolicy> = HashMap::new();

    // Seed initial URLs at depth 0
    for seed in &config.seeds {
        if seen.len() >= config.limit {
            break;
        }
        if seen.insert(seed.clone()) {
            frontier.push_back((seed.clone(), 0));
        }
    }

    // Fetch robots.txt & optional sitemaps for seed origins
    for seed in &config.seeds {
        if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
            break;
        }
        let origin = origin_key(seed);
        if robots_by_origin.contains_key(&origin) {
            continue;
        }
        let policy = if config.respect_robots || config.use_sitemap {
            robots::fetch_robots(seed, deadline).await
        } else {
            robots::RobotsPolicy::default()
        };
        if config.use_sitemap && config.max_depth > 0 && seen.len() < config.limit {
            let sm_urls = robots::discover_sitemap_urls(
                seed,
                &policy,
                config.limit.saturating_sub(seen.len()),
                deadline,
            )
            .await;
            for u in sm_urls {
                if seen.len() >= config.limit {
                    break;
                }
                if !host_matches(
                    &u,
                    &allowed_hosts,
                    config.allow_subdomains,
                    config.allow_external_links,
                ) {
                    continue;
                }
                if !path_matches(u.path(), &config.include_paths, &config.exclude_paths) {
                    continue;
                }
                if config.respect_robots && !policy.is_allowed(&u) {
                    continue;
                }
                if seen.insert(u.clone()) {
                    frontier.push_back((u, 1));
                }
            }
        }
        robots_by_origin.insert(origin, policy);
    }

    if let Ok(mut g) = job.lock() {
        g.total = seen.len();
    }

    let mut in_flight: FuturesUnordered<_> = FuturesUnordered::new();
    let mut finished_count = 0usize;

    loop {
        if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
            break;
        }

        // Fill up to `config.concurrency` workers from the frontier.
        while in_flight.len() < config.concurrency
            && finished_count + in_flight.len() < config.limit
        {
            let Some((url, depth)) = frontier.pop_front() else {
                break;
            };
            let origin = origin_key(&url);
            if config.respect_robots {
                let policy = if let Some(p) = robots_by_origin.get(&origin) {
                    p.clone()
                } else {
                    let p = robots::fetch_robots(&url, deadline).await;
                    robots_by_origin.insert(origin, p.clone());
                    p
                };
                if !policy.is_allowed(&url) {
                    if let Ok(mut g) = job.lock() {
                        g.failed += 1;
                        g.errors.push(CrawlErrorEntry {
                            url,
                            error: "robots_disallowed",
                        });
                    }
                    finished_count += 1;
                    continue;
                }
            }

            let slots = app.search.page_slots.clone();
            let opts = config.scrape_options.clone();
            in_flight.push(async move {
                let result = tokio::time::timeout_at(deadline, async {
                    let permit = slots
                        .acquire_owned()
                        .await
                        .map_err(|_| PageError::Timeout)?;
                    pages::scrape(url.clone(), &opts, permit, deadline).await
                })
                .await
                .unwrap_or(Err(PageError::Timeout));
                (url, depth, result)
            });
        }

        let Some((url, depth, result)) = in_flight.next().await else {
            break;
        };
        finished_count += 1;

        match result {
            Ok(mut page) => {
                // Expand frontier with discovered links if within max_depth
                if depth < config.max_depth
                    && seen.len() < config.limit
                    && let Some(links) = &page.links
                {
                    // Output links retain evidence fragments; a crawl visits
                    // each fetched document only once regardless of section.
                    for link in links
                        .iter()
                        .filter_map(|link| pages::public_url(link.clone()).ok())
                    {
                        if seen.len() >= config.limit {
                            break;
                        }
                        if !host_matches(
                            &link,
                            &allowed_hosts,
                            config.allow_subdomains,
                            config.allow_external_links,
                        ) {
                            continue;
                        }
                        if !path_matches(link.path(), &config.include_paths, &config.exclude_paths)
                        {
                            continue;
                        }
                        if seen.insert(link.clone()) {
                            frontier.push_back((link.clone(), depth + 1));
                        }
                    }
                }
                if !config.keep_links_in_output {
                    page.links = None;
                }
                // Deduplicate identical page content across URL aliases (?utm_*, print views, etc.)
                let primary = page
                    .markdown
                    .as_deref()
                    .or(page.text.as_deref())
                    .or(page.html.as_deref())
                    .unwrap_or("");
                let content_hash = fingerprint_text(primary);
                if !primary.is_empty() && !seen_content_hashes.insert(content_hash) {
                    if let Ok(mut g) = job.lock() {
                        g.total = seen.len();
                    }
                    continue;
                }
                let stable_hash = stable_content_hash(primary);
                let url_key = selection::url_key(&page.url);
                let snapshot_md = page.markdown.clone().or_else(|| page.text.clone());

                let mut status_for_summary = None;
                if config.incremental {
                    let prev = config.previous.get(&url_key).or_else(|| {
                        page.requested_url
                            .as_ref()
                            .and_then(|u| config.previous.get(&selection::url_key(u)))
                    });
                    let change_status = match prev {
                        None => ChangeStatus::Added,
                        Some(prev_snap) if prev_snap.hash == stable_hash => {
                            if config.omit_unchanged_content {
                                page.markdown = None;
                                page.text = None;
                                page.html = None;
                                page.chunks = None;
                            }
                            ChangeStatus::Unchanged
                        }
                        Some(prev_snap) => {
                            if let (Some(old_md), Some(new_md)) =
                                (prev_snap.markdown.as_deref(), snapshot_md.as_deref())
                            {
                                page.diff = Some(compute_section_diff(old_md, new_md));
                            }
                            ChangeStatus::Updated
                        }
                    };
                    page.content_hash = Some(stable_hash.clone());
                    page.change_status = Some(change_status);
                    status_for_summary = Some(change_status);
                }

                if let Ok(mut g) = job.lock() {
                    g.completed += 1;
                    g.total = seen.len();
                    if let (Some(summary), Some(st)) = (g.changes.as_mut(), status_for_summary) {
                        match st {
                            ChangeStatus::Added => summary.added += 1,
                            ChangeStatus::Updated => summary.updated += 1,
                            ChangeStatus::Unchanged => summary.unchanged += 1,
                        }
                    }
                    g.page_snapshots.insert(
                        url_key,
                        StoredPageSnapshot {
                            hash: stable_hash,
                            markdown: snapshot_md,
                        },
                    );
                    g.data.push(page);
                }
            }
            Err(err) => {
                if let Ok(mut g) = job.lock() {
                    g.failed += 1;
                    g.total = seen.len();
                    g.errors.push(CrawlErrorEntry {
                        url,
                        error: err.code(),
                    });
                }
            }
        }
    }

    let persist_data = if let Ok(mut g) = job.lock() {
        if g.status == CrawlStatus::Running {
            g.status = if cancel.load(Ordering::Relaxed) {
                CrawlStatus::Cancelled
            } else if g.completed == 0 && g.failed > 0 {
                CrawlStatus::Failed
            } else {
                CrawlStatus::Completed
            };
        }
        g.finished_elapsed_ms =
            Some(u64::try_from(g.started.elapsed().as_millis()).unwrap_or(u64::MAX));
        (!g.page_snapshots.is_empty()).then(|| (g.id.clone(), g.page_snapshots.clone()))
    } else {
        None
    };
    if let Some((job_id, snapshots)) = persist_data {
        app.crawls.persist_snapshots(&job_id, &snapshots);
    }
}

// ---------------------------------------------------------------------------
// URL / Path Matching Helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChangeStatus {
    Added,
    Updated,
    Unchanged,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SectionDiff {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub added_sections: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub removed_sections: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub changed_sections: Vec<String>,
}

/// Computes a stable deterministic SHA-256 hex digest (`sha256:<hex>`) over
/// whitespace-normalized page content for cross-run incremental change tracking.
pub(crate) fn stable_content_hash(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !normalized.is_empty() {
            normalized.push(' ');
        }
        normalized.push_str(word);
    }
    format!("sha256:{}", sha256_hex(normalized.as_bytes()))
}

/// Computes heading-level section diffs between previous and current Markdown.
pub(crate) fn compute_section_diff(previous: &str, current: &str) -> SectionDiff {
    let prev_sections = split_markdown_sections(previous);
    let curr_sections = split_markdown_sections(current);

    let prev_map: HashMap<&str, &str> = prev_sections
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let curr_map: HashMap<&str, &str> = curr_sections
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let mut diff = SectionDiff::default();
    for (heading, body) in &curr_sections {
        match prev_map.get(heading.as_str()) {
            None => diff.added_sections.push(heading.clone()),
            Some(old_body) if stable_content_hash(old_body) != stable_content_hash(body) => {
                diff.changed_sections.push(heading.clone());
            }
            Some(_) => {}
        }
    }
    for (heading, _) in &prev_sections {
        if !curr_map.contains_key(heading.as_str()) {
            diff.removed_sections.push(heading.clone());
        }
    }
    diff
}

fn split_markdown_sections(md: &str) -> Vec<(String, String)> {
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut current_heading = "(intro)".to_owned();
    let mut current_lines: Vec<&str> = Vec::new();
    let mut in_fence = false;

    let flush = |sections: &mut Vec<(String, String)>, heading: &str, lines: &mut Vec<&str>| {
        let body = lines.join("\n").trim().to_owned();
        if !body.is_empty() || heading != "(intro)" {
            sections.push((heading.to_owned(), body));
        }
        lines.clear();
    };

    for line in md.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            current_lines.push(line);
            continue;
        }
        if !in_fence && trimmed.starts_with('#') {
            let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
            if (1..=6).contains(&hashes)
                && let Some(rest) = trimmed[hashes..].strip_prefix(' ')
            {
                let heading = rest.trim().trim_end_matches('#').trim();
                if !heading.is_empty() {
                    flush(&mut sections, &current_heading, &mut current_lines);
                    current_heading = heading.to_owned();
                    continue;
                }
            }
        }
        current_lines.push(line);
    }
    flush(&mut sections, &current_heading, &mut current_lines);
    sections
}

/// Pure-Rust FIPS 180-4 SHA-256 hex digest (zero external crates).
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity((data.len() + 9).div_ceil(64) * 64);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for block in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, chunk) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = String::with_capacity(64);
    for word in h {
        use std::fmt::Write;
        let _ = write!(&mut out, "{word:08x}");
    }
    out
}

fn fingerprint_text(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    for word in text.split_whitespace() {
        word.hash(&mut hasher);
    }
    hasher.finish()
}

fn origin_key(url: &Url) -> String {
    format!(
        "{}://{}",
        url.scheme(),
        url.host_str().unwrap_or_default().to_ascii_lowercase()
    )
}

fn seed_hosts(seeds: &[Url]) -> HashSet<String> {
    seeds
        .iter()
        .filter_map(|u| u.host_str().map(str::to_ascii_lowercase))
        .collect()
}

fn host_matches(
    url: &Url,
    allowed_hosts: &HashSet<String>,
    allow_subdomains: bool,
    allow_external: bool,
) -> bool {
    if allow_external {
        return true;
    }
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    if allowed_hosts.contains(&host) {
        return true;
    }
    if allow_subdomains {
        for base in allowed_hosts {
            let root = base.strip_prefix("www.").unwrap_or(base);
            if host == root || host.ends_with(&format!(".{root}")) {
                return true;
            }
        }
    }
    false
}

pub(crate) fn path_matches(path: &str, include: &[String], exclude: &[String]) -> bool {
    if !include.is_empty() && !include.iter().any(|p| glob_match(p, path)) {
        return false;
    }
    if exclude.iter().any(|p| glob_match(p, path)) {
        return false;
    }
    true
}

/// Simple wildcard (`*`) or prefix path matcher (e.g. `/docs/*`, `*.pdf`, `/blog`).
fn glob_match(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return text.starts_with(pattern);
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut pos = 0usize;
    for (idx, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if idx == 0 {
            if !text.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else if let Some(found) = text[pos..].find(part) {
            pos += found + part.len();
        } else {
            return false;
        }
    }
    if let Some(last) = parts.last()
        && !last.is_empty()
    {
        return text.ends_with(last);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_glob_matching_filters_include_and_exclude() {
        let include = vec!["/docs/*".to_owned(), "/blog".to_owned()];
        let exclude = vec!["/docs/internal/*".to_owned(), "*.pdf".to_owned()];

        assert!(path_matches("/docs/getting-started", &include, &exclude));
        assert!(path_matches("/blog/post-1", &include, &exclude));
        assert!(!path_matches("/pricing", &include, &exclude));
        assert!(!path_matches("/docs/internal/secret", &include, &exclude));
        assert!(!path_matches("/docs/manual.pdf", &include, &exclude));
    }

    #[test]
    fn host_matching_respects_subdomains_and_external_flags() {
        let seeds = vec![Url::parse("https://www.example.org/start").unwrap()];
        let hosts = seed_hosts(&seeds);

        let same = Url::parse("https://www.example.org/other").unwrap();
        let sub = Url::parse("https://docs.example.org/guide").unwrap();
        let ext = Url::parse("https://other.net/page").unwrap();

        assert!(host_matches(&same, &hosts, false, false));
        assert!(!host_matches(&sub, &hosts, false, false));
        assert!(host_matches(&sub, &hosts, true, false));
        assert!(!host_matches(&ext, &hosts, true, false));
        assert!(host_matches(&ext, &hosts, false, true));
    }

    #[test]
    fn stable_content_hash_is_deterministic_sha256_and_whitespace_insensitive() {
        let h1 = stable_content_hash("Hello   world\n\nSection 1");
        let h2 = stable_content_hash("Hello world Section 1");
        let h3 = stable_content_hash("Hello world Section 2");
        assert!(h1.starts_with("sha256:"));
        assert_eq!(h1.len(), "sha256:".len() + 64);
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
    }

    #[test]
    fn section_diff_identifies_added_removed_and_changed_markdown_sections() {
        let old = "\
# Docs

Welcome intro.

## Authentication

Use bearer tokens.

## Deprecated Endpoints

Legacy v1 endpoints.";

        let new = "\
# Docs

Welcome intro.

## Authentication

Use bearer tokens with 32+ ASCII chars.

## Incremental Crawls

Pass previousHashes or sinceCrawlId.";

        let diff = compute_section_diff(old, new);
        assert_eq!(diff.added_sections, vec!["Incremental Crawls"]);
        assert_eq!(diff.removed_sections, vec!["Deprecated Endpoints"]);
        assert_eq!(diff.changed_sections, vec!["Authentication"]);
    }

    #[test]
    fn persistent_crawl_state_survives_manager_restart_and_rejects_traversal_ids() {
        let dir =
            std::env::temp_dir().join(format!("quelle-crawl-state-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let m1 = CrawlManager::with_state_dir(Some(dir.clone()));
        m1.save_test_snapshot(
            "crawl-1a2b3c-0001",
            "https://example.org/docs",
            "sha256:abc",
            "# Docs\n\nBody v1",
        );

        let m2 = CrawlManager::with_state_dir(Some(dir.clone()));
        let loaded = m2
            .load_persisted_snapshots("crawl-1a2b3c-0001")
            .expect("reloaded persisted snapshots after restart");
        assert_eq!(loaded["https://example.org/docs"].hash, "sha256:abc");
        assert_eq!(
            loaded["https://example.org/docs"].markdown.as_deref(),
            Some("# Docs\n\nBody v1")
        );
        // Path traversal IDs must never load anything.
        assert!(m2.load_persisted_snapshots("../etc/passwd").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
