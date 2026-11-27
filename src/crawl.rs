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
    problem, robots,
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

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CrawlJobSnapshot {
    pub id: String,
    pub status: CrawlStatus,
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
    pub data: Vec<ScrapedPage>,
    pub errors: Vec<CrawlErrorEntry>,
    pub elapsed_ms: u64,
}

struct CrawlJobState {
    id: String,
    status: CrawlStatus,
    started: StdInstant,
    finished_elapsed_ms: Option<u64>,
    total: usize,
    completed: usize,
    failed: usize,
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
            data: self.data.clone(),
            errors: self.errors.clone(),
            elapsed_ms,
        }
    }
}

pub struct CrawlManager {
    jobs: Mutex<HashMap<String, Arc<Mutex<CrawlJobState>>>>,
    slots: Arc<Semaphore>,
}

impl CrawlManager {
    pub fn new() -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT_CRAWLS)),
        }
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
    max_age: Option<u64>,
    concurrency: Option<usize>,
    #[serde(default)]
    sync: bool,
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
            cache_max_age: Duration::from_secs(input.max_age.unwrap_or(300)),
        },
        keep_links_in_output,
        concurrency: input.concurrency.unwrap_or(4).min(8),
        timeout: if input.sync {
            SYNC_CRAWL_TIMEOUT
        } else {
            ASYNC_CRAWL_TIMEOUT
        },
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
                if let Ok(mut g) = job.lock() {
                    g.completed += 1;
                    g.total = seen.len();
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

    if let Ok(mut g) = job.lock() {
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
    }
}

// ---------------------------------------------------------------------------
// URL / Path Matching Helpers
// ---------------------------------------------------------------------------

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
}
