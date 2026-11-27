use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    process::Stdio,
    sync::{Arc, LazyLock, OnceLock},
    time::Duration,
};

use regex::Regex;
use reqwest::{StatusCode, cookie::Jar, header};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::lookup_host,
    process::Command,
    sync::OwnedSemaphorePermit,
    time::Instant,
};
use url::{Host, Url};

use crate::{politeness, read_body, truncate};

mod clean;
pub mod markdown;
#[cfg(test)]
mod tests;

pub use markdown::{ExtractOptions, PageMeta};

/// HTML beyond this is cut off and the prefix is parsed; long pages keep
/// their opening content instead of failing.
const PAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_BLOCKS: usize = 3000;
/// Candidate passages ranked per source; a spreadsheet or CSV can have
/// thousands of rows.
const MAX_PASSAGES: usize = 20_000;
const MAX_REDIRECTS: usize = 8;
const MAX_RETRIES: usize = 2;
const PAGE_TIMEOUT: Duration = Duration::from_secs(10);
const PASSAGE_BUDGET: usize = 6500;
/// Official PDFs are larger and slower than HTML pages but often the
/// primary source, so they get a bigger byte and time budget.
const PDF_BYTES: usize = 12 * 1024 * 1024;
const PDF_TIMEOUT: Duration = Duration::from_secs(12);
const PDF_EXTRACT_TIMEOUT: Duration = Duration::from_secs(5);
/// Search answers rarely need more than the opening pages; this bounds
/// extraction time for long reports.
const PDF_PAGES: &str = "150";
const PDF_TEXT_BYTES: usize = 4 * 1024 * 1024;

static PDF_SUPPORT: OnceLock<bool> = OnceLock::new();
static PROXY_URL: OnceLock<Option<String>> = OnceLock::new();
static BROWSER_SIDECAR_URL: OnceLock<Option<Url>> = OnceLock::new();
static OCR_SIDECAR_URL: OnceLock<Option<Url>> = OnceLock::new();

/// Whole budget for one page: download, optional rendering and parsing.
const FETCH_BUDGET: Duration = Duration::from_secs(25);
const RENDER_TIMEOUT: Duration = Duration::from_secs(18);
const FULL_TEXT_BYTES: usize = PAGE_BYTES;
/// Pages with less readable text than this also get a rendering attempt.
const THIN_PAGE: usize = 800;
/// Each render is a separate browser process of roughly 100 MB.
static RENDER_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
static RENDERER: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
/// Office and CSV conversions running at once. Each is CPU-bound.
static DOCUMENT_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

pub fn set_proxy(proxy: Option<String>) {
    let _ = PROXY_URL.set(proxy);
}

fn configured_proxy() -> Option<&'static str> {
    PROXY_URL.get().and_then(Option::as_deref)
}

pub fn set_browser_sidecar(url: Option<Url>) {
    let _ = BROWSER_SIDECAR_URL.set(url);
}

fn browser_sidecar() -> Option<&'static Url> {
    BROWSER_SIDECAR_URL.get().and_then(Option::as_ref)
}

pub fn set_ocr_sidecar(url: Option<Url>) {
    let _ = OCR_SIDECAR_URL.set(url);
}

fn ocr_sidecar() -> Option<&'static Url> {
    OCR_SIDECAR_URL.get().and_then(Option::as_ref)
}

fn any_renderer_enabled() -> bool {
    render_enabled() || browser_sidecar().is_some()
}
/// Reserved ranges blocked in addition to Lightpanda's private networks.
const BLOCKED_RANGES: &[&str] = &[
    "0.0.0.0/8",
    "100.64.0.0/10",
    "192.0.0.0/24",
    "192.0.2.0/24",
    "192.88.99.0/24",
    "198.18.0.0/15",
    "198.51.100.0/24",
    "203.0.113.0/24",
    "224.0.0.0/3",
    "64:ff9b::/96",
    "100::/64",
    "2001:db8::/32",
];
const HONEST_AGENT: &str = concat!(
    "Quelle/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/digital4you/quelle)"
);

/// Finds Lightpanda once at startup: `QUELLE_LIGHTPANDA` or `lightpanda`
/// on the path. Without it, script-only pages are skipped.
pub fn detect_renderer() -> bool {
    RENDERER
        .get_or_init(|| {
            // The child runs with a cleared environment, so resolve the
            // full path here instead of relying on PATH lookup later.
            let binary = std::env::var_os("QUELLE_LIGHTPANDA")
                .filter(|value| !value.is_empty())
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var_os("PATH").and_then(|paths| {
                        std::env::split_paths(&paths)
                            .map(|dir| dir.join("lightpanda"))
                            .find(|candidate| candidate.is_file())
                    })
                })?;
            std::process::Command::new(&binary)
                .arg("version")
                .env_clear()
                .env("LIGHTPANDA_DISABLE_TELEMETRY", "true")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
                .then_some(binary)
        })
        .is_some()
}

fn render_enabled() -> bool {
    RENDERER.get().is_some_and(Option::is_some)
}

/// A current desktop browser identity. Many sites serve reduced or blocked
/// pages to unknown clients; explicit bot challenges are still respected.
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36";
const DEFAULT_SEC_CH_UA: &str =
    r#""Chromium";v="136", "Google Chrome";v="136", "Not.A/Brand";v="99""#;
static USER_AGENT: OnceLock<header::HeaderValue> = OnceLock::new();

pub fn set_user_agent(value: header::HeaderValue) {
    let _ = USER_AGENT.set(value);
}

/// Wikimedia asks automated clients to identify themselves and throttles
/// browser-like ones (HTTP 429), so its sites get an honest identity.
fn is_wikimedia(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        [
            "wikipedia.org",
            "wikimedia.org",
            "wikidata.org",
            "wiktionary.org",
            "wikisource.org",
            "wikivoyage.org",
            "wikinews.org",
            "wikiquote.org",
            "wikibooks.org",
            "wikiversity.org",
            "mediawiki.org",
        ]
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
    })
}

fn user_agent_for(url: &Url) -> (header::HeaderValue, bool) {
    if is_wikimedia(url) {
        (header::HeaderValue::from_static(HONEST_AGENT), false)
    } else {
        let ua = user_agent();
        let browser_like = ua
            .to_str()
            .is_ok_and(|s| s.contains("Chrome/") && s.contains("Mozilla/5.0"));
        (ua, browser_like)
    }
}

fn user_agent() -> header::HeaderValue {
    USER_AGENT
        .get()
        .cloned()
        .unwrap_or_else(|| header::HeaderValue::from_static(DEFAULT_USER_AGENT))
}

/// Checks once at startup whether poppler's `pdftotext` is available.
pub fn detect_pdf_support() -> bool {
    *PDF_SUPPORT.get_or_init(|| {
        std::process::Command::new("pdftotext")
            .arg("-v")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    })
}

fn pdf_enabled() -> bool {
    PDF_SUPPORT.get().copied().unwrap_or(false)
}

#[derive(Serialize)]
pub struct Source {
    pub content: String,
    pub metadata: Metadata,
}

impl Source {
    fn mark_truncated(&mut self, cut: bool) {
        if let Some(truncated) = &mut self.metadata.truncated {
            *truncated |= cut;
        }
    }

    fn full(url: Url, title: String, text: &str) -> Result<Self, PageError> {
        if text.trim().is_empty() {
            return Err(PageError::Unreadable);
        }
        let end = text.floor_char_boundary(FULL_TEXT_BYTES.min(text.len()));
        Ok(Self {
            content: text[..end].to_owned(),
            metadata: Metadata {
                url,
                title,
                retrieval: None,
                pages_read: None,
                truncated: Some(end < text.len()),
            },
        })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Metadata {
    pub title: String,
    pub url: Url,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieval: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pages_read: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
}

#[derive(Clone, Copy, Debug)]
pub enum PageError {
    Target,
    Transport,
    Blocked,
    Unsupported,
    TooLarge,
    Unreadable,
    Timeout,
    NoTextLayer,
    /// The page is a script shell or a "JavaScript required" notice.
    NeedsJs,
}

impl PageError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Target => "target",
            Self::Transport => "transport",
            Self::Blocked => "blocked",
            Self::Unsupported => "unsupported",
            Self::TooLarge => "too_large",
            Self::Unreadable => "unreadable",
            Self::Timeout => "timeout",
            Self::NoTextLayer => "no_text_layer",
            Self::NeedsJs => "needs_js",
        }
    }
}

/// What the request is looking for, used to rank passages within a page.
#[derive(Clone)]
pub struct Relevance {
    keywords: Vec<String>,
    /// The question asks for a number, amount or date.
    numeric: bool,
}

impl Relevance {
    /// Keywords from the request and the search queries first, then the
    /// planner's evidence terms (synonyms and word forms the page may use).
    pub fn new<'a>(
        request: &str,
        queries: impl Iterator<Item = &'a str>,
        evidence: impl Iterator<Item = &'a str>,
    ) -> Self {
        static NUMERIC: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(concat!(
                r"(?i)\b(wann|seit wann|wie ?viele?|wie hoch|wie gross|wie groß|wie lang|wie alt|welche[mns]? jahr",
                r"|quand|depuis quand|combien|quelle année|en quelle",
                r"|quando|quanti|quante|quanto",
                r"|when|how many|how much|how long|how old|what year)\b",
            ))
            .expect("numeric question pattern")
        });
        let mut seen = HashSet::new();
        let keywords = words(request)
            .chain(queries.flat_map(words))
            .chain(evidence.take(12).flat_map(words))
            .filter(|word| seen.insert(word.clone()))
            .take(80)
            .collect();
        Self {
            keywords,
            numeric: NUMERIC.is_match(request),
        }
    }
}

/// Exact match, or a shared stem: the shorter word is a prefix of the longer
/// one and differs by an inflection-sized ending ("Steuer"/"Steuern",
/// "Tage"/"Tagen").
fn matches_keyword(word: &str, keyword: &str) -> bool {
    if word == keyword {
        return true;
    }
    let (short, long) = if word.len() <= keyword.len() {
        (word, keyword)
    } else {
        (keyword, word)
    };
    let (short_len, long_len) = (short.chars().count(), long.chars().count());
    long.starts_with(short)
        && ((short_len >= 5 && long_len <= short_len + 3) || (short_len == 4 && long_len <= 6))
}

enum Kind {
    Html,
    Text,
    Markdown,
    Json,
    Feed,
    Pdf,
    /// Office, OpenDocument, RTF, EPUB or CSV, converted with anydoc. The
    /// format is named for CSV, which has no signature, and otherwise
    /// detected from the content.
    Document(Option<anydoc::Format>),
}

pub fn public_url(mut url: Url) -> Result<Url, PageError> {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.as_str().len() > 2048
    {
        return Err(PageError::Target);
    }
    match url.host() {
        Some(Host::Domain(host))
            if host.contains('.')
                && !host.ends_with('.')
                && !host.ends_with(".localhost")
                && !host.ends_with(".local")
                && !host.ends_with(".internal") => {}
        Some(Host::Ipv4(ip)) if public_address(ip.into()) => {}
        Some(Host::Ipv6(ip)) if public_address(ip.into()) => {}
        _ => return Err(PageError::Target),
    }
    url.set_fragment(None);
    Ok(url)
}

/// An operator-supplied network that neither Quelle nor Lightpanda may
/// reach, for clusters whose pod, service or internal load-balancer ranges
/// are not private addresses.
#[derive(Clone, Copy)]
pub struct Network {
    address: IpAddr,
    prefix: u8,
}

impl Network {
    /// Parses `address/prefix` or a single address. Host bits are cleared.
    pub fn parse(text: &str) -> Option<Self> {
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (address.parse().ok()?, Some(prefix.parse().ok()?)),
            None => (text.parse().ok()?, None),
        };
        let max = if matches!(address, IpAddr::V4(_)) {
            32
        } else {
            128
        };
        let prefix = prefix.unwrap_or(max);
        if prefix > max {
            return None;
        }
        let network = Self { address, prefix };
        Some(Self {
            address: network.masked(address)?,
            prefix,
        })
    }

    /// The address with its host bits cleared, or `None` for another family.
    fn masked(self, address: IpAddr) -> Option<IpAddr> {
        let bits = u32::from(self.prefix);
        match (self.address, address) {
            (IpAddr::V4(_), IpAddr::V4(ip)) => {
                let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
                Some(IpAddr::V4((u32::from(ip) & mask).into()))
            }
            (IpAddr::V6(_), IpAddr::V6(ip)) => {
                let mask = u128::MAX.checked_shl(128 - bits).unwrap_or(0);
                Some(IpAddr::V6((u128::from(ip) & mask).into()))
            }
            _ => None,
        }
    }

    fn contains(self, address: IpAddr) -> bool {
        self.masked(address) == Some(self.address)
    }
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.address, self.prefix)
    }
}

static BLOCKED_NETWORKS: OnceLock<Vec<Network>> = OnceLock::new();

pub fn set_blocked_networks(networks: Vec<Network>) {
    let _ = BLOCKED_NETWORKS.set(networks);
}

fn blocked_networks() -> &'static [Network] {
    BLOCKED_NETWORKS.get().map_or(&[], Vec::as_slice)
}

fn public_address(address: IpAddr) -> bool {
    globally_routable(address) && !blocked_networks().iter().any(|n| n.contains(address))
}

/// False for private, loopback, link-local, shared, documentation,
/// benchmarking, multicast and other reserved addresses.
fn globally_routable(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => !matches!(
            ip.octets(),
            [0 | 10 | 127 | 224..=255, ..]
                | [100, 64..=127, ..]
                | [169, 254, ..]
                | [172, 16..=31, ..]
                | [192, 0, 0 | 2, _]
                | [192, 88, 99, _]
                | [192, 168, ..]
                | [198, 18 | 19, ..]
                | [198, 51, 100, _]
                | [203, 0, 113, _]
        ),
        IpAddr::V6(ip) => match ip.segments() {
            [0x2001, 0..=0x01ff | 0x0db8, ..] | [0x2002 | 0x3fff, ..] => false,
            [0x2000..=0x3fff, ..] => true,
            _ => false,
        },
    }
}

pub fn requested_links(text: &str) -> Vec<Url> {
    static LINK: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)https?://[^\s<>\[\]"`]+"#).expect("static URL pattern"));
    LINK.find_iter(text)
        .filter_map(|item| {
            let raw = item
                .as_str()
                .trim_end_matches(['.', ',', ';', '!', '?', '\'']);
            let raw = if raw.ends_with(')') && raw.matches(')').count() > raw.matches('(').count() {
                raw.trim_end_matches(')')
            } else {
                raw
            };
            Url::parse(raw).ok().and_then(|url| public_url(url).ok())
        })
        .take(3)
        .collect()
}

/// Reads the body up to `limit`. With `keep_prefix` a longer body is cut off
/// instead of rejected, which suits HTML but not binary formats.
async fn read_page(
    mut response: reqwest::Response,
    limit: usize,
    keep_prefix: bool,
) -> Result<(Vec<u8>, bool), crate::BodyError> {
    if !keep_prefix {
        return read_body(response, limit).await.map(|bytes| (bytes, false));
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| crate::BodyError::Read)? {
        let room = limit - data.len();
        if chunk.len() > room {
            data.extend_from_slice(&chunk[..room]);
            return Ok((data, true));
        }
        data.extend_from_slice(&chunk);
    }
    Ok((data, false))
}

fn page_client_builder(jar: Arc<Jar>) -> Result<reqwest::ClientBuilder, PageError> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(5))
        .cookie_provider(jar);
    if let Some(proxy_url) = configured_proxy() {
        let proxy = reqwest::Proxy::all(proxy_url).map_err(|_| PageError::Transport)?;
        builder = builder.proxy(proxy);
    } else {
        builder = builder.no_proxy();
    }
    Ok(builder)
}

fn apply_page_headers(
    mut req: reqwest::RequestBuilder,
    url: &Url,
    accept: &'static str,
    force_honest: bool,
) -> reqwest::RequestBuilder {
    let (ua, browser_like) = if force_honest {
        (header::HeaderValue::from_static(HONEST_AGENT), false)
    } else {
        user_agent_for(url)
    };
    req = req
        .header(header::ACCEPT, accept)
        .header(
            header::ACCEPT_LANGUAGE,
            "de-CH,de;q=0.9,fr-CH;q=0.8,fr;q=0.7,it;q=0.6,en;q=0.5",
        )
        .header(header::USER_AGENT, ua);
    if browser_like {
        req = req
            .header("sec-ch-ua", DEFAULT_SEC_CH_UA)
            .header("sec-ch-ua-mobile", "?0")
            .header("sec-ch-ua-platform", r#""Windows""#)
            .header("sec-fetch-dest", "document")
            .header("sec-fetch-mode", "navigate")
            .header("sec-fetch-site", "none")
            .header("sec-fetch-user", "?1")
            .header("upgrade-insecure-requests", "1");
    }
    req
}

async fn resolve_public_addrs(url: &Url) -> Result<Option<(String, Vec<SocketAddr>)>, PageError> {
    let Some(Host::Domain(host)) = url.host() else {
        return Ok(None);
    };
    let port = url.port_or_known_default().ok_or(PageError::Target)?;
    // The trailing dot makes the name absolute, so resolver search
    // domains (Kubernetes adds `<namespace>.svc.cluster.local` and
    // others) cannot turn a public-looking name into a cluster one.
    let absolute = format!("{}.", host.trim_end_matches('.'));
    let addresses: Vec<SocketAddr> = lookup_host((absolute.as_str(), port))
        .await
        .map_err(|_| PageError::Transport)?
        .collect();
    if addresses.is_empty() || !addresses.iter().all(|addr| public_address(addr.ip())) {
        return Err(PageError::Target);
    }
    Ok(Some((host.to_owned(), addresses)))
}

/// Lightweight raw download (used by robots.txt and sitemap.xml discovery)
/// using the same SSRF checks, HTTP/2, cookie jar, and per-host politeness.
pub async fn download_raw(
    mut url: Url,
    max_bytes: usize,
    deadline: Instant,
) -> Result<(Url, u16, Option<String>, Vec<u8>), PageError> {
    url = public_url(url)?;
    let jar = Arc::new(Jar::default());
    for step in 0..=MAX_REDIRECTS {
        let host_key = url.host_str().unwrap_or_default().to_owned();
        let _host_permit = politeness::global()
            .acquire(&host_key, deadline)
            .await
            .ok_or(PageError::Timeout)?;
        let resolved = resolve_public_addrs(&url).await?;
        let mut builder = page_client_builder(Arc::clone(&jar))?;
        if let Some((host, addrs)) = &resolved
            && configured_proxy().is_none()
        {
            builder = builder.resolve_to_addrs(host, addrs);
        }
        let client = builder.build().map_err(|_| PageError::Transport)?;
        let req = apply_page_headers(
            client.get(url.clone()),
            &url,
            "text/plain,application/xml,text/xml,*/*;q=0.5",
            true,
        );
        let response = tokio::time::timeout_at(deadline.min(Instant::now() + PAGE_TIMEOUT), req.send())
            .await
            .map_err(|_| PageError::Timeout)?
            .map_err(|_| PageError::Transport)?;
        if matches!(
            response.status(),
            StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::SEE_OTHER
                | StatusCode::TEMPORARY_REDIRECT
                | StatusCode::PERMANENT_REDIRECT
        ) {
            if step == MAX_REDIRECTS {
                return Err(PageError::Blocked);
            }
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or(PageError::Blocked)?;
            url = public_url(url.join(location).map_err(|_| PageError::Target)?)?;
            continue;
        }
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_ascii_lowercase);
        if !response.status().is_success() {
            return Err(if status == 404 {
                PageError::Unreadable
            } else {
                PageError::Blocked
            });
        }
        let (bytes, _) = tokio::time::timeout_at(
            deadline.min(Instant::now() + PAGE_TIMEOUT),
            read_page(response, max_bytes, false),
        )
        .await
        .map_err(|_| PageError::Timeout)?
        .map_err(|e| match e {
            crate::BodyError::TooLarge => PageError::TooLarge,
            _ => PageError::Transport,
        })?;
        return Ok((url, status, content_type, bytes));
    }
    Err(PageError::Blocked)
}

struct Downloaded {
    url: Url,
    status: u16,
    bytes: Vec<u8>,
    content_type: Option<String>,
    kind: Kind,
    truncated: bool,
}

async fn download(
    initial_url: Url,
    deadline: Instant,
    cache_max_age: Duration,
) -> Result<Downloaded, PageError> {
    if let Some(cached) = politeness::global().cache_get(&initial_url, cache_max_age) {
        return classify_download(
            cached.final_url,
            cached.status,
            cached.content_type,
            cached.bytes,
            cached.truncated,
        );
    }

    let jar = Arc::new(Jar::default());
    let mut url = initial_url.clone();
    let mut retries = 0usize;
    let initial_host = initial_url.host_str().unwrap_or_default().to_owned();
    let mut force_honest =
        politeness::global().preferred_tier(&initial_host) == Some(politeness::FetchTier::HonestHttp);
    let mut tried_honest = force_honest;

    for step in 0..=(MAX_REDIRECTS + MAX_RETRIES + 1) {
        let host_key = url.host_str().unwrap_or_default().to_owned();
        let _host_permit = politeness::global()
            .acquire(&host_key, deadline)
            .await
            .ok_or(PageError::Timeout)?;

        let resolved = resolve_public_addrs(&url).await?;
        let mut builder = page_client_builder(Arc::clone(&jar))?;
        if let Some((host, addrs)) = &resolved
            && configured_proxy().is_none()
        {
            builder = builder.resolve_to_addrs(host, addrs);
        }
        let client = builder.build().map_err(|_| PageError::Transport)?;
        let accept = if pdf_enabled() {
            "text/html,application/xhtml+xml,text/markdown;q=0.95,application/xml;q=0.9,application/rss+xml;q=0.8,application/atom+xml;q=0.8,application/pdf;q=0.8,application/json;q=0.7,text/plain;q=0.5,*/*;q=0.1"
        } else {
            "text/html,application/xhtml+xml,text/markdown;q=0.95,application/xml;q=0.9,application/rss+xml;q=0.8,application/atom+xml;q=0.8,application/json;q=0.7,text/plain;q=0.5,*/*;q=0.1"
        };
        let response = apply_page_headers(client.get(url.clone()), &url, accept, force_honest).send();
        let step_deadline = deadline.min(Instant::now() + PAGE_TIMEOUT);
        let response = match tokio::time::timeout_at(step_deadline, response).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(_)) => return Err(PageError::Transport),
            Err(_) => return Err(PageError::Timeout),
        };
        if matches!(
            response.status(),
            StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::SEE_OTHER
                | StatusCode::TEMPORARY_REDIRECT
                | StatusCode::PERMANENT_REDIRECT
        ) {
            if step > MAX_REDIRECTS {
                return Err(PageError::Blocked);
            }
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or(PageError::Blocked)?;
            url = public_url(url.join(location).map_err(|_| PageError::Target)?)?;
            continue;
        }
        if matches!(
            response.status(),
            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
        ) && retries < MAX_RETRIES
        {
            let wait = politeness::jitter(politeness::parse_retry_after(
                response.headers(),
                Duration::from_secs(2u64 << retries),
            ));
            politeness::global().record_throttle(&host_key, wait);
            retries += 1;
            let resume_at = Instant::now() + wait;
            if resume_at < deadline {
                drop(_host_permit);
                tokio::time::sleep_until(resume_at).await;
                continue;
            }
        }
        let is_cf_challenge = response
            .headers()
            .get("cf-mitigated")
            .is_some_and(|value| value == "challenge");
        // Escalation step 1: Akamai/Fastly/gov WAFs often reject non-Chrome TLS
        // claiming to be Chrome (`sec-ch-ua` + `rustls`), while allowing honest
        // HTTP clients. Retry once with an honest identity before escalating.
        if response.status() == StatusCode::FORBIDDEN && !is_cf_challenge && !tried_honest {
            tracing::info!(
                stage = "escalate_honest_http",
                host = url.host_str(),
                status = 403
            );
            force_honest = true;
            tried_honest = true;
            continue;
        }
        if !response.status().is_success() || is_cf_challenge {
            tracing::info!(
                stage = "page_refused",
                host = url.host_str(),
                status = response.status().as_u16()
            );
            return Err(PageError::Blocked);
        }
        if force_honest && !is_wikimedia(&url) {
            politeness::global().record_tier(&host_key, politeness::FetchTier::HonestHttp);
        }
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_ascii_lowercase);
        let pdf_path = url.path().to_ascii_lowercase().ends_with(".pdf");
        let declared = match content_type
            .as_deref()
            .map(|value| value.split(';').next().unwrap_or_default().trim())
        {
            Some("text/html" | "application/xhtml+xml") => Some(Kind::Html),
            Some("text/markdown" | "text/x-markdown") => Some(Kind::Markdown),
            Some("application/json") | Some("application/ld+json") => Some(Kind::Json),
            Some("application/rss+xml" | "application/atom+xml" | "text/xml" | "application/xml") => {
                Some(Kind::Feed)
            }
            Some("text/plain") => Some(Kind::Text),
            Some("application/pdf" | "application/x-pdf") if pdf_enabled() => Some(Kind::Pdf),
            Some("text/csv") => Some(Kind::Document(Some(anydoc::Format::Csv))),
            Some(media) if document_media(media) => Some(Kind::Document(None)),
            None | Some("application/octet-stream" | "binary/octet-stream") => None,
            _ => return Err(PageError::Unsupported),
        };
        let extension = url
            .path()
            .rsplit_once('.')
            .map(|(_, extension)| extension.to_ascii_lowercase())
            .unwrap_or_default();
        let document_path = anydoc::Format::from_extension(&extension)
            .is_some_and(|format| format != anydoc::Format::Pdf);
        let maybe_pdf = matches!(declared, Some(Kind::Pdf))
            || (declared.is_none() && pdf_enabled() && pdf_path);
        let maybe_document =
            matches!(declared, Some(Kind::Document(_))) || (declared.is_none() && document_path);
        let (limit, budget) = if maybe_pdf || maybe_document {
            (PDF_BYTES, PDF_TIMEOUT)
        } else {
            (PAGE_BYTES, PAGE_TIMEOUT)
        };
        let (bytes, truncated) = tokio::time::timeout_at(
            deadline.min(Instant::now() + budget),
            read_page(response, limit, !maybe_pdf && !maybe_document),
        )
        .await
        .map_err(|_| PageError::Timeout)?
        .map_err(|error| match error {
            crate::BodyError::TooLarge => PageError::TooLarge,
            _ => PageError::Transport,
        })?;

        let result = classify_download(url.clone(), status, content_type.clone(), bytes.clone(), truncated)?;
        politeness::global().cache_put(
            &initial_url,
            politeness::CachedResponse {
                final_url: url,
                status,
                content_type,
                bytes,
                truncated,
            },
        );
        return Ok(result);
    }
    Err(PageError::Blocked)
}

fn classify_download(
    url: Url,
    status: u16,
    content_type: Option<String>,
    bytes: Vec<u8>,
    truncated: bool,
) -> Result<Downloaded, PageError> {
    let pdf_path = url.path().to_ascii_lowercase().ends_with(".pdf");
    let declared = match content_type
        .as_deref()
        .map(|value| value.split(';').next().unwrap_or_default().trim())
    {
        Some("text/html" | "application/xhtml+xml") => Some(Kind::Html),
        Some("text/markdown" | "text/x-markdown") => Some(Kind::Markdown),
        Some("application/json") | Some("application/ld+json") => Some(Kind::Json),
        Some("application/rss+xml" | "application/atom+xml" | "text/xml" | "application/xml") => {
            Some(Kind::Feed)
        }
        Some("text/plain") => Some(Kind::Text),
        Some("application/pdf" | "application/x-pdf") if pdf_enabled() => Some(Kind::Pdf),
        Some("text/csv") => Some(Kind::Document(Some(anydoc::Format::Csv))),
        Some(media) if document_media(media) => Some(Kind::Document(None)),
        None | Some("application/octet-stream" | "binary/octet-stream") => None,
        _ => return Err(PageError::Unsupported),
    };
    let extension = url
        .path()
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    let document_path = anydoc::Format::from_extension(&extension)
        .is_some_and(|format| format != anydoc::Format::Pdf);
    let maybe_pdf =
        matches!(declared, Some(Kind::Pdf)) || (declared.is_none() && pdf_enabled() && pdf_path);
    let maybe_document =
        matches!(declared, Some(Kind::Document(_))) || (declared.is_none() && document_path);
    let is_pdf = bytes.starts_with(b"%PDF-");
    let kind = match declared {
        Some(Kind::Pdf) if !is_pdf => return Err(PageError::Unreadable),
        Some(Kind::Text) if matches!(extension.as_str(), "md" | "markdown") => Kind::Markdown,
        Some(Kind::Feed) => {
            // Distinguish RSS/Atom feeds from generic XHTML/HTML served as text/xml.
            let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).to_ascii_lowercase();
            if head.contains("<rss") || head.contains("<feed") || head.contains("<rdf:rdf") {
                Kind::Feed
            } else if head.contains("<html") {
                Kind::Html
            } else {
                Kind::Text
            }
        }
        Some(kind) => kind,
        None if is_pdf && pdf_enabled() => Kind::Pdf,
        None if maybe_pdf => return Err(PageError::Unsupported),
        None => match anydoc::Format::from_bytes(&bytes) {
            Some(anydoc::Format::Pdf) => return Err(PageError::Unsupported),
            Some(format) => Kind::Document(Some(format)),
            None if extension == "csv" => Kind::Document(Some(anydoc::Format::Csv)),
            None if matches!(extension.as_str(), "md" | "markdown") => Kind::Markdown,
            None if extension == "json" => Kind::Json,
            None if maybe_document => return Err(PageError::Unsupported),
            None => Kind::Html,
        },
    };
    Ok(Downloaded {
        url,
        status,
        bytes,
        content_type,
        kind,
        truncated,
    })
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RenderMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Markdown,
    Text,
    Html,
    Links,
}

#[derive(Clone, Debug)]
pub struct ScrapeOptions {
    pub formats: Vec<OutputFormat>,
    pub extract: ExtractOptions,
    pub render: RenderMode,
    pub wait_for_ms: Option<u64>,
    pub cache_max_age: Duration,
}

impl Default for ScrapeOptions {
    fn default() -> Self {
        Self {
            formats: vec![OutputFormat::Markdown],
            extract: ExtractOptions::default(),
            render: RenderMode::Auto,
            wait_for_ms: None,
            cache_max_age: Duration::from_secs(5 * 60),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrapedPage {
    pub url: Url,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_url: Option<Url>,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<Url>>,
    pub metadata: PageMeta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieval: Option<&'static str>,
    pub truncated: bool,
}

pub async fn fetch(
    url: Url,
    relevance: Option<&Relevance>,
    permit: OwnedSemaphorePermit,
    deadline: Instant,
) -> Result<Source, PageError> {
    let _permit = permit;
    let deadline = deadline.min(Instant::now() + FETCH_BUDGET);
    tokio::time::timeout_at(deadline, async {
        let target = export_url(url);
        let host_key = target.host_str().unwrap_or_default().to_owned();
        let preferred = politeness::global().preferred_tier(&host_key);

        let rendered = |url: Url, threshold| async move {
            let (html, cut, tier) = render_with_fallback(&url, threshold, None, deadline).await?;
            let mut source = parse(
                url,
                html,
                Some("text/html; charset=utf-8".to_owned()),
                relevance,
                true,
            )
            .await?;
            source.metadata.retrieval = Some(match tier {
                politeness::FetchTier::Sidecar => "sidecar",
                _ => "rendered",
            });
            source.mark_truncated(cut);
            Ok::<_, PageError>((source, tier))
        };

        // If this host already required rendering earlier, skip the wasted 403.
        if matches!(
            preferred,
            Some(politeness::FetchTier::Rendered | politeness::FetchTier::Sidecar)
        ) && any_renderer_enabled()
            && let Ok((source, _)) = rendered(target.clone(), 300).await
        {
            return Ok(source);
        }

        let downloaded = match download(target.clone(), deadline, Duration::from_secs(5 * 60)).await
        {
            Ok(d) => d,
            Err(PageError::Blocked) if any_renderer_enabled() => {
                tracing::info!(
                    stage = "escalate_browser_on_block",
                    host = target.host_str()
                );
                let (source, tier) = rendered(target, 300).await?;
                politeness::global().record_tier(&host_key, tier);
                return Ok(source);
            }
            Err(e) => return Err(e),
        };
        let Downloaded {
            url,
            bytes,
            content_type,
            kind,
            truncated: cut,
            ..
        } = downloaded;
        match kind {
            Kind::Pdf => return pdf_or_ocr(url, &bytes, relevance, deadline).await,
            Kind::Document(format) => return document(url, bytes, format, relevance).await,
            Kind::Markdown => {
                let decoded = decode_bytes(&bytes, content_type.as_deref());
                return text_source(url, decoded, "markdown", relevance);
            }
            Kind::Json => {
                let decoded = pretty_json_bytes(&bytes, content_type.as_deref());
                return text_source(url, decoded, "json", relevance);
            }
            Kind::Feed => {
                let decoded = decode_bytes(&bytes, content_type.as_deref());
                let md = feed_to_markdown(&decoded, &url);
                return text_source(url, md, "feed", relevance);
            }
            Kind::Html | Kind::Text => {}
        }
        let is_html = matches!(kind, Kind::Html);
        static SCRIPT: LazyLock<regex::bytes::Regex> =
            LazyLock::new(|| regex::bytes::Regex::new(r"(?i)<script[\s>]").expect("script tag"));
        let has_script = is_html && SCRIPT.is_match(&bytes);
        match parse(url.clone(), bytes, content_type, relevance, false).await {
            // Script-only pages are rendered. So are pages whose markup holds
            // almost no readable text; the rendered result is kept only when
            // it carries more.
            Err(error @ (PageError::NeedsJs | PageError::Unreadable))
                if has_script && any_renderer_enabled() =>
            {
                let (source, _) = rendered(
                    url,
                    if matches!(error, PageError::NeedsJs) {
                        3000
                    } else {
                        300
                    },
                )
                .await?;
                Ok(source)
            }
            Err(PageError::NeedsJs) => {
                tracing::info!(stage = "needs_js", host = url.host_str());
                Err(PageError::NeedsJs)
            }
            Ok(mut source) => {
                source.mark_truncated(cut);
                let length = source.content.chars().count();
                if has_script && any_renderer_enabled() && length < THIN_PAGE {
                    // Give back the static text even if the speculative render stalls.
                    let mut stop = deadline - Duration::from_millis(150);
                    if length >= 300 {
                        stop = stop.min(Instant::now() + Duration::from_secs(2));
                    }
                    if let Ok(Ok((better, _))) =
                        tokio::time::timeout_at(stop, rendered(url, length + 120)).await
                        && better.content.chars().count() > length
                    {
                        return Ok(better);
                    }
                }
                Ok(source)
            }
            other => other,
        }
    })
    .await
    .map_err(|_| PageError::Timeout)?
}

/// Firecrawl/Crawl4AI-style rich scrape: returns clean Markdown, plain text,
/// raw HTML, extracted links, and metadata in a single pass.
pub async fn scrape(
    requested: Url,
    options: &ScrapeOptions,
    permit: OwnedSemaphorePermit,
    deadline: Instant,
) -> Result<ScrapedPage, PageError> {
    let _permit = permit;
    let deadline = deadline.min(Instant::now() + FETCH_BUDGET);
    let options = options.clone();
    tokio::time::timeout_at(deadline, async move {
        let want_md = options.formats.contains(&OutputFormat::Markdown);
        let want_text = options.formats.contains(&OutputFormat::Text);
        let want_html = options.formats.contains(&OutputFormat::Html);
        let want_links = options.formats.contains(&OutputFormat::Links);

        let target = export_url(requested.clone());
        let host_key = target.host_str().unwrap_or_default().to_owned();
        let preferred = politeness::global().preferred_tier(&host_key);

        let render_to_page = |u: Url, threshold: usize| {
            let req = requested.clone();
            let opts = options.clone();
            async move {
                let (html_bytes, cut, tier) =
                    render_with_fallback(&u, threshold, opts.wait_for_ms, deadline).await?;
                let retrieval = match tier {
                    politeness::FetchTier::Sidecar => Some("sidecar"),
                    _ => Some("rendered"),
                };
                let page = build_scraped_html(
                    u,
                    req,
                    200,
                    Some("text/html; charset=utf-8".to_owned()),
                    html_bytes,
                    cut,
                    retrieval,
                    &opts,
                    want_md,
                    want_text,
                    want_html,
                    want_links,
                )
                .await?;
                Ok::<_, PageError>((page, tier))
            }
        };

        if options.render == RenderMode::Always {
            let clean_target = public_url(requested.clone())?;
            let (page, _) = render_to_page(clean_target, 300).await?;
            return Ok(page);
        }

        if options.render == RenderMode::Auto
            && any_renderer_enabled()
            && matches!(
                preferred,
                Some(politeness::FetchTier::Rendered | politeness::FetchTier::Sidecar)
            )
            && let Ok((page, _)) = render_to_page(target.clone(), 300).await
        {
            return Ok(page);
        }

        let downloaded = match download(target.clone(), deadline, options.cache_max_age).await {
            Ok(d) => d,
            Err(PageError::Blocked)
                if options.render != RenderMode::Never && any_renderer_enabled() =>
            {
                tracing::info!(
                    stage = "escalate_browser_on_block",
                    host = target.host_str()
                );
                let (page, tier) = render_to_page(target, 300).await?;
                politeness::global().record_tier(&host_key, tier);
                return Ok(page);
            }
            Err(e) => return Err(e),
        };
        let Downloaded {
            url,
            status,
            bytes,
            content_type,
            kind,
            truncated: cut,
        } = downloaded;

        match kind {
            Kind::Pdf => {
                let src = pdf_or_ocr(url.clone(), &bytes, None, deadline).await?;
                let meta = PageMeta {
                    status_code: Some(status),
                    content_type,
                    ..Default::default()
                };
                return Ok(ScrapedPage {
                    requested_url: (url != requested).then_some(requested),
                    url,
                    title: src.metadata.title,
                    markdown: want_md.then(|| src.content.clone()),
                    text: want_text.then_some(src.content),
                    html: None,
                    links: want_links.then(Vec::new),
                    metadata: meta,
                    retrieval: src.metadata.retrieval.or(Some("pdf")),
                    truncated: src.metadata.truncated.unwrap_or(false),
                });
            }
            Kind::Document(format) => {
                let src = document(url.clone(), bytes, format, None).await?;
                let meta = PageMeta {
                    status_code: Some(status),
                    content_type,
                    ..Default::default()
                };
                return Ok(ScrapedPage {
                    requested_url: (url != requested).then_some(requested),
                    url,
                    title: src.metadata.title,
                    markdown: want_md.then(|| src.content.clone()),
                    text: want_text.then_some(src.content),
                    html: None,
                    links: want_links.then(Vec::new),
                    metadata: meta,
                    retrieval: Some("document"),
                    truncated: src.metadata.truncated.unwrap_or(false),
                });
            }
            Kind::Markdown => {
                let decoded = decode_bytes(&bytes, content_type.as_deref());
                let title = first_markdown_heading(&decoded)
                    .unwrap_or_else(|| pdf_name(&url));
                let meta = PageMeta {
                    status_code: Some(status),
                    content_type,
                    ..Default::default()
                };
                return Ok(ScrapedPage {
                    requested_url: (url != requested).then_some(requested),
                    url,
                    title,
                    markdown: want_md.then(|| decoded.clone()),
                    text: want_text.then_some(decoded),
                    html: None,
                    links: want_links.then(Vec::new),
                    metadata: meta,
                    retrieval: Some("markdown"),
                    truncated: cut,
                });
            }
            Kind::Json => {
                let decoded = pretty_json_bytes(&bytes, content_type.as_deref());
                let title = url.host_str().unwrap_or("JSON document").to_owned();
                let meta = PageMeta {
                    status_code: Some(status),
                    content_type,
                    ..Default::default()
                };
                return Ok(ScrapedPage {
                    requested_url: (url != requested).then_some(requested),
                    url,
                    title,
                    markdown: want_md.then(|| format!("```json\n{decoded}\n```")),
                    text: want_text.then_some(decoded),
                    html: None,
                    links: want_links.then(Vec::new),
                    metadata: meta,
                    retrieval: Some("json"),
                    truncated: cut,
                });
            }
            Kind::Feed => {
                let decoded = decode_bytes(&bytes, content_type.as_deref());
                let (title, md, feed_links) = parse_feed(&decoded, &url);
                let meta = PageMeta {
                    status_code: Some(status),
                    content_type,
                    ..Default::default()
                };
                return Ok(ScrapedPage {
                    requested_url: (url != requested).then_some(requested),
                    url,
                    title,
                    markdown: want_md.then(|| md.clone()),
                    text: want_text.then_some(md),
                    html: None,
                    links: want_links.then_some(feed_links),
                    metadata: meta,
                    retrieval: Some("feed"),
                    truncated: cut,
                });
            }
            Kind::Text => {
                let decoded = decode_bytes(&bytes, content_type.as_deref());
                let title = url.host_str().unwrap_or("Text document").to_owned();
                let meta = PageMeta {
                    status_code: Some(status),
                    content_type,
                    ..Default::default()
                };
                return Ok(ScrapedPage {
                    requested_url: (url != requested).then_some(requested),
                    url,
                    title,
                    markdown: want_md.then(|| decoded.clone()),
                    text: want_text.then_some(decoded),
                    html: None,
                    links: want_links.then(Vec::new),
                    metadata: meta,
                    retrieval: Some("text"),
                    truncated: cut,
                });
            }
            Kind::Html => {}
        }

        static SCRIPT: LazyLock<regex::bytes::Regex> =
            LazyLock::new(|| regex::bytes::Regex::new(r"(?i)<script[\s>]").expect("script tag"));
        let has_script = SCRIPT.is_match(&bytes);

        let static_res = build_scraped_html(
            url.clone(),
            requested.clone(),
            status,
            content_type.clone(),
            bytes,
            cut,
            None,
            &options,
            want_md,
            want_text,
            want_html,
            want_links,
        )
        .await;

        if options.render == RenderMode::Never || !any_renderer_enabled() {
            return static_res;
        }

        match static_res {
            Err(error @ (PageError::NeedsJs | PageError::Unreadable | PageError::Blocked))
                if has_script || matches!(error, PageError::Blocked) =>
            {
                let threshold = if matches!(error, PageError::NeedsJs) {
                    3000
                } else {
                    300
                };
                let (page, tier) = render_to_page(url, threshold).await?;
                if matches!(error, PageError::Blocked) {
                    politeness::global().record_tier(&host_key, tier);
                }
                Ok(page)
            }
            Ok(page) if has_script => {
                let primary_len = page
                    .markdown
                    .as_deref()
                    .or(page.text.as_deref())
                    .map_or(0, |s| s.chars().count());
                if primary_len < THIN_PAGE {
                    let mut stop = deadline - Duration::from_millis(150);
                    if primary_len >= 300 {
                        stop = stop.min(Instant::now() + Duration::from_secs(3));
                    }
                    if let Ok(Ok((better, _))) =
                        tokio::time::timeout_at(stop, render_to_page(url, primary_len + 120)).await
                    {
                        let better_len = better
                            .markdown
                            .as_deref()
                            .or(better.text.as_deref())
                            .map_or(0, |s| s.chars().count());
                        if better_len > primary_len {
                            return Ok(better);
                        }
                    }
                }
                Ok(page)
            }
            other => other,
        }
    })
    .await
    .map_err(|_| PageError::Timeout)?
}

#[allow(clippy::too_many_arguments)]
async fn build_scraped_html(
    url: Url,
    requested: Url,
    status: u16,
    content_type: Option<String>,
    bytes: Vec<u8>,
    cut: bool,
    retrieval: Option<&'static str>,
    options: &ScrapeOptions,
    want_md: bool,
    want_text: bool,
    want_html: bool,
    want_links: bool,
) -> Result<ScrapedPage, PageError> {
    let extract_opts = options.extract.clone();
    let rendered = retrieval == Some("rendered");
    tokio::task::spawn_blocking(move || {
        let decoded = decode_bytes(&bytes, content_type.as_deref());
        let document = Html::parse_document(&decoded);
        static TITLE: LazyLock<Selector> =
            LazyLock::new(|| Selector::parse("title").expect("title selector"));
        static CHALLENGE: LazyLock<Selector> = LazyLock::new(|| {
            Selector::parse("#challenge-form, #cf-challenge-running").expect("challenge selector")
        });
        let title = document
            .select(&TITLE)
            .next()
            .map(|el| normalize(el.text(), 300))
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| url.host_str().unwrap_or("Web source").to_owned());
        if document.select(&CHALLENGE).next().is_some()
            || matches!(
                title.to_lowercase().as_str(),
                "just a moment..."
                    | "access denied"
                    | "attention required! | cloudflare"
                    | "robot check"
            )
        {
            return Err(PageError::Blocked);
        }

        let mut plain = clean::page_text(document.root_element(), !extract_opts.only_main_content);
        if extract_opts.only_main_content && plain.trim().is_empty() {
            plain = clean::page_text(document.root_element(), true);
        }
        let body = std::slice::from_ref(&plain);
        if needs_script(&document, body, plain.len(), rendered)
            || (!rendered && client_template(&document, body))
        {
            return Err(PageError::NeedsJs);
        }

        let md = want_md.then(|| {
            markdown::html_to_markdown(&document, &url, &extract_opts, FULL_TEXT_BYTES)
        });
        let links = want_links.then(|| markdown::extract_links(&document, &url, 500));
        let mut meta = markdown::extract_meta(&document, &url);
        meta.status_code = Some(status);
        meta.content_type = content_type;

        let text = want_text.then(|| {
            let trimmed = plain.trim_matches(['\n', '\r']);
            let end = trimmed.floor_char_boundary(FULL_TEXT_BYTES.min(trimmed.len()));
            trimmed[..end].to_owned()
        });
        let html = want_html.then(|| {
            let end = decoded.floor_char_boundary(FULL_TEXT_BYTES.min(decoded.len()));
            decoded[..end].to_owned()
        });

        let has_content = md.as_deref().is_some_and(|s| !s.trim().is_empty())
            || text.as_deref().is_some_and(|s| !s.trim().is_empty())
            || html.as_deref().is_some_and(|s| !s.trim().is_empty())
            || links.as_ref().is_some_and(|l| !l.is_empty());
        if !has_content && plain.trim().is_empty() {
            return Err(PageError::Unreadable);
        }

        Ok(ScrapedPage {
            requested_url: (url != requested).then_some(requested),
            url,
            title,
            markdown: md,
            text,
            html,
            links,
            metadata: meta,
            retrieval,
            truncated: cut,
        })
    })
    .await
    .map_err(|_| PageError::Unreadable)?
}

fn decode_bytes(bytes: &[u8], media_type: Option<&str>) -> String {
    let charset = media_type.and_then(|media_type| {
        media_type.split(';').skip(1).find_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            (name.trim() == "charset").then(|| value.trim().trim_matches(['\'', '"']))
        })
    });
    let encoding = charset
        .and_then(|charset| encoding_rs::Encoding::for_label(charset.as_bytes()))
        .or_else(|| sniff_encoding(bytes))
        .unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = encoding.decode(bytes);
    text.into_owned()
}

async fn parse(
    url: Url,
    bytes: Vec<u8>,
    content_type: Option<String>,
    relevance: Option<&Relevance>,
    rendered: bool,
) -> Result<Source, PageError> {
    let relevance = relevance.cloned();
    tokio::task::spawn_blocking(move || {
        extract(
            url,
            &bytes,
            content_type.as_deref(),
            relevance.as_ref(),
            rendered,
        )
    })
    .await
    .map_err(|_| PageError::Unreadable)?
}

/// Escalates from Tier 2 (Lightpanda) to Tier 3 (optional Playwright/Chromium
/// sidecar configured via `QUELLE_BROWSER_URL`).
async fn render_with_fallback(
    url: &Url,
    threshold: usize,
    wait_for_ms: Option<u64>,
    deadline: Instant,
) -> Result<(Vec<u8>, bool, politeness::FetchTier), PageError> {
    let host_key = url.host_str().unwrap_or_default();
    let skip_lightpanda =
        politeness::global().preferred_tier(host_key) == Some(politeness::FetchTier::Sidecar)
            && browser_sidecar().is_some();

    if !skip_lightpanda && render_enabled() {
        match render(url, threshold, wait_for_ms).await {
            Ok((html, cut)) => return Ok((html, cut, politeness::FetchTier::Rendered)),
            Err(err) if browser_sidecar().is_none() => return Err(err),
            Err(_) => {
                tracing::info!(
                    stage = "escalate_browser_sidecar",
                    host = url.host_str()
                );
            }
        }
    }

    if let Some(sidecar) = browser_sidecar() {
        let (html, cut) = render_sidecar(sidecar, url, wait_for_ms, deadline).await?;
        return Ok((html, cut, politeness::FetchTier::Sidecar));
    }

    Err(PageError::NeedsJs)
}

/// Calls an external Playwright/Chromium sidecar (compatible with Firecrawl's
/// `playwright-service` JSON contract: `POST { url, wait_after_load }` ->
/// `{ content, pageStatusCode }`, or raw HTML). Validates public DNS first.
async fn render_sidecar(
    sidecar: &Url,
    url: &Url,
    wait_for_ms: Option<u64>,
    deadline: Instant,
) -> Result<(Vec<u8>, bool), PageError> {
    let clean = public_url(url.clone())?;
    let _ = resolve_public_addrs(&clean).await?;
    let _slot = RENDER_SLOTS
        .acquire()
        .await
        .map_err(|_| PageError::NeedsJs)?;
    let started = Instant::now();
    let client = crate::client_builder()
        .build()
        .map_err(|_| PageError::Transport)?;
    let payload = serde_json::json!({
        "url": clean.as_str(),
        "wait_after_load": wait_for_ms.unwrap_or(1000).min(15_000),
    });
    let step_deadline = deadline.min(Instant::now() + RENDER_TIMEOUT);
    let response = tokio::time::timeout_at(
        step_deadline,
        client.post(sidecar.clone()).json(&payload).send(),
    )
    .await
    .map_err(|_| PageError::Timeout)?
    .map_err(|_| PageError::Transport)?;
    if !response.status().is_success() {
        return Err(PageError::Blocked);
    }
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().contains("application/json"));
    let (body, cut) = tokio::time::timeout_at(
        step_deadline,
        read_page(response, PAGE_BYTES + 64 * 1024, !is_json),
    )
    .await
    .map_err(|_| PageError::Timeout)?
    .map_err(|_| PageError::Transport)?;

    let mut html = if is_json {
        #[derive(Deserialize)]
        struct SidecarResponse {
            content: Option<String>,
            html: Option<String>,
            #[serde(rename = "pageStatusCode")]
            page_status_code: Option<u16>,
        }
        let parsed: SidecarResponse =
            serde_json::from_slice(&body).map_err(|_| PageError::Unreadable)?;
        if parsed.page_status_code.is_some_and(|code| code >= 400) {
            return Err(PageError::Blocked);
        }
        parsed
            .content
            .or(parsed.html)
            .ok_or(PageError::Unreadable)?
            .into_bytes()
    } else {
        body
    };
    let truncated = cut || html.len() > PAGE_BYTES;
    html.truncate(PAGE_BYTES);
    tracing::info!(
        stage = "sidecar_render",
        host = url.host_str(),
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        ok = true
    );
    Ok((html, truncated))
}

/// Renders a script-only page with Lightpanda and returns the resulting
/// DOM as HTML. Lightpanda checks every connection, including scripts and
/// XHR, against private and reserved ranges after DNS resolution, so page
/// scripts cannot reach internal services. Telemetry is disabled, the
/// environment is cleared, and the process is killed when the request ends.
async fn render(
    url: &Url,
    threshold: usize,
    wait_for_ms: Option<u64>,
) -> Result<(Vec<u8>, bool), PageError> {
    let Some(binary) = RENDERER.get().and_then(Option::as_ref) else {
        return Err(PageError::NeedsJs);
    };
    let _slot = RENDER_SLOTS
        .acquire()
        .await
        .map_err(|_| PageError::NeedsJs)?;
    let started = Instant::now();
    let wait_ms = wait_for_ms.unwrap_or(7_000).clamp(500, 15_000);
    let terminate_ms = (wait_ms + 1500).min(17_000);
    let mut command = Command::new(binary);
    command
        .env_clear()
        .env("LIGHTPANDA_DISABLE_TELEMETRY", "true")
        .args([
            "fetch",
            "--dump",
            "html",
            // Network quiet, then enough visible text: script-heavy sites
            // can go quiet before their content has been inserted.
            "--wait-until",
            "networkalmostidle",
            "--wait-script",
            &format!("document.body && (document.body.innerText || '').length > {threshold}"),
            "--wait-ms",
            &wait_ms.to_string(),
            "--terminate-ms",
            &terminate_ms.to_string(),
            "--http-timeout",
            "8000",
            "--http-max-response-size",
            "10485760",
            "--v8-max-heap-mb",
            "256",
            "--block-private-networks",
            "--locale",
            "de-CH",
            "--log-level",
            "fatal",
        ]);
    for range in BLOCKED_RANGES {
        command.args(["--block-cidrs", range]);
    }
    // Parsed addresses never start with '-', which Lightpanda reads as an
    // exemption.
    for network in blocked_networks() {
        command.args(["--block-cidrs", &network.to_string()]);
    }
    let output = command
        .args(["--dump-max-bytes", &PAGE_BYTES.to_string(), url.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(RENDER_TIMEOUT, output).await;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let output = match output {
        // A wait that runs out still writes the page and exits non-zero;
        // what was rendered by then is used.
        Ok(Ok(output)) if !output.stdout.is_empty() => output,
        Ok(_) => {
            tracing::info!(
                stage = "render",
                host = url.host_str(),
                elapsed_ms,
                ok = false
            );
            return Err(PageError::NeedsJs);
        }
        Err(_) => {
            tracing::info!(
                stage = "render",
                host = url.host_str(),
                elapsed_ms,
                ok = false,
                timeout = true
            );
            return Err(PageError::Timeout);
        }
    };
    tracing::info!(
        stage = "render",
        host = url.host_str(),
        elapsed_ms,
        ok = true
    );
    let mut html = output.stdout;
    let cut = html.len() > PAGE_BYTES
        || html.ends_with(b"[truncated]\n")
        || html.ends_with(b"[truncated]");
    html.truncate(PAGE_BYTES);
    Ok((html, cut))
}

/// Media types anydoc converts: Word, Excel, PowerPoint (binary and XML),
/// OpenDocument, RTF and EPUB.
fn document_media(media: &str) -> bool {
    media.starts_with("application/vnd.openxmlformats-officedocument.")
        || media.starts_with("application/vnd.oasis.opendocument.")
        || media.starts_with("application/vnd.ms-excel")
        || media.starts_with("application/vnd.ms-powerpoint")
        || media.starts_with("application/vnd.ms-word")
        || matches!(
            media,
            "application/msword" | "application/rtf" | "text/rtf" | "application/epub+zip"
        )
}

/// Converts an office document with anydoc, which enforces its own limits on
/// archive entries, XML depth and table expansion. It runs off the async
/// threads, a parser panic is contained, and the result goes through the
/// same passage ranking as HTML and PDF. A blocking thread cannot be
/// cancelled, so a conversion that outlives its timeout keeps its slot until
/// it finishes; crafted documents cannot pile up converter threads.
async fn document(
    url: Url,
    bytes: Vec<u8>,
    format: Option<anydoc::Format>,
    relevance: Option<&Relevance>,
) -> Result<Source, PageError> {
    let relevance = relevance.cloned();
    let task = async {
        let slot = DOCUMENT_SLOTS
            .acquire()
            .await
            .map_err(|_| PageError::Unreadable)?;
        tokio::task::spawn_blocking(move || convert(url, &bytes, format, relevance, slot))
            .await
            .map_err(|_| PageError::Unreadable)?
    };
    tokio::time::timeout(PDF_EXTRACT_TIMEOUT, task)
        .await
        .map_err(|_| PageError::Timeout)?
}

fn convert(
    url: Url,
    bytes: &[u8],
    format: Option<anydoc::Format>,
    relevance: Option<Relevance>,
    _slot: tokio::sync::SemaphorePermit<'static>,
) -> Result<Source, PageError> {
    let markdown = std::panic::catch_unwind(|| anydoc::to_markdown_bytes(bytes, format))
        .map_err(|_| PageError::Unreadable)?
        .map_err(|_| PageError::Unreadable)?;
    let Some(relevance) = relevance else {
        let title = pdf_name(&url);
        let mut source = Source::full(url, title, &markdown)?;
        source.metadata.retrieval = Some("document");
        return Ok(source);
    };
    let passages = markdown_passages(&markdown);
    let title = passages
        .iter()
        .take(8)
        .find(|p| p.split_whitespace().count() >= 3 && p.chars().count() >= 16)
        .map_or_else(|| pdf_name(&url), |p| shorten(p, 160));
    let mut source = source(url, title, passages.into_iter(), &relevance)?;
    source.metadata.retrieval = Some("document");
    Ok(source)
}

/// Splits anydoc Markdown into passages: paragraphs and list items as they
/// are, headings without their marks, and tables via [`table_passages`].
fn markdown_passages(markdown: &str) -> Vec<String> {
    fn cells(line: &str) -> Vec<String> {
        line.trim()
            .trim_matches('|')
            .split('|')
            .map(|cell| {
                cell.replace("**", "")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    }
    let mut passages = Vec::new();
    let mut paragraph = String::new();
    let mut table: Vec<Vec<String>> = Vec::new();
    let mut heading = String::new();
    let flush = |paragraph: &mut String, passages: &mut Vec<String>| {
        if !paragraph.trim().is_empty() {
            passages.push(paragraph.trim().replace("**", ""));
        }
        paragraph.clear();
    };
    for line in markdown.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('|') {
            flush(&mut paragraph, &mut passages);
            let row = cells(trimmed);
            if !row
                .iter()
                .all(|cell| cell.chars().all(|c| matches!(c, '-' | ':' | ' ')))
            {
                table.push(row);
            }
            continue;
        }
        if !table.is_empty() {
            passages.extend(table_passages(&std::mem::take(&mut table), &heading));
        }
        if trimmed.is_empty() {
            flush(&mut paragraph, &mut passages);
            continue;
        }
        let text = trimmed.trim_start_matches('#').trim_start();
        let item = text
            .strip_prefix("- ")
            .or_else(|| text.strip_prefix("* "))
            .or_else(|| text.strip_prefix("+ "));
        if trimmed.starts_with('#') {
            heading = text.replace("**", "");
        }
        if trimmed.starts_with('#') || item.is_some() {
            flush(&mut paragraph, &mut passages);
            passages.push(item.unwrap_or(text).replace("**", ""));
            continue;
        }
        if !paragraph.is_empty() {
            paragraph.push(' ');
        }
        paragraph.push_str(text);
    }
    flush(&mut paragraph, &mut passages);
    if !table.is_empty() {
        passages.extend(table_passages(&table, &heading));
    }
    passages
}

/// A cell that holds a figure ("1 129 722", "- 6 837", "0.4", "12%").
fn numeric(cell: &str) -> bool {
    cell.chars().any(|c| c.is_ascii_digit())
        && cell.chars().all(|c| {
            c.is_ascii_digit() || matches!(c, ' ' | '-' | '.' | ',' | '\'' | '’' | '%' | '+')
        })
}

/// Turns a table into passages. Data rows (mostly figures) are labelled with
/// the column headers, which spreadsheets often spread over several short
/// text rows above the data ("Bevölkerungs- / stand am / 31. Dezember").
/// Title rows and layout tables (Word documents often set text in two
/// columns) become plain text. Data rows carry the nearest heading, such as
/// a sheet name, so rows from different years or regions stay apart.
fn table_passages(rows: &[Vec<String>], context: &str) -> Vec<String> {
    let filled = |row: &Vec<String>| row.iter().filter(|cell| !cell.is_empty()).count();
    let figures = |row: &Vec<String>| row.iter().filter(|cell| numeric(cell)).count();
    let is_data = |row: &Vec<String>| figures(row) >= 2 && figures(row) * 3 >= filled(row);
    let first_data = rows.iter().position(is_data);
    let mut labels: Vec<String> = Vec::new();
    let mut passages = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let plain = || {
            row.iter()
                .filter(|cell| !cell.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(" ")
        };
        let before_data = first_data.is_some_and(|first| index < first);
        let header = before_data
            && filled(row) >= 3
            && figures(row) == 0
            && row.iter().all(|cell| cell.chars().count() <= 60);
        if header {
            if labels.len() < row.len() {
                labels.resize(row.len(), String::new());
            }
            for (label, cell) in labels.iter_mut().zip(row) {
                if cell.is_empty() {
                    continue;
                }
                if label.ends_with('-') && cell.chars().next().is_some_and(char::is_lowercase) {
                    label.pop();
                } else if !label.is_empty() {
                    label.push(' ');
                }
                label.push_str(cell);
            }
            continue;
        }
        if first_data.is_some() && !before_data && is_data(row) && !labels.is_empty() {
            let text = row
                .iter()
                .enumerate()
                .filter(|(_, value)| !value.is_empty())
                .map(
                    |(column, value)| match labels.get(column).filter(|l| !l.is_empty()) {
                        Some(label) => format!("{label}: {value}"),
                        None => value.clone(),
                    },
                )
                .collect::<Vec<_>>()
                .join("; ");
            passages.push(if context.is_empty() {
                text
            } else {
                format!("{context} — {text}")
            });
            continue;
        }
        let text = plain();
        if !text.is_empty() {
            passages.push(text);
        }
    }
    passages
}

/// Extracts PDF text via `pdftotext` first; when the PDF has no embedded text
/// layer (scanned PDF) and `QUELLE_OCR_URL` (OCRia-nano-1 / LiteParse sidecar)
/// is configured, escalates to OCR automatically.
async fn pdf_or_ocr(
    url: Url,
    bytes: &[u8],
    relevance: Option<&Relevance>,
    deadline: Instant,
) -> Result<Source, PageError> {
    match pdf(url.clone(), bytes, relevance).await {
        Ok(src) => Ok(src),
        Err(PageError::NoTextLayer) if ocr_sidecar().is_some() => {
            ocr_pdf(url, bytes, relevance, deadline).await
        }
        Err(err) => Err(err),
    }
}

async fn ocr_pdf(
    url: Url,
    bytes: &[u8],
    relevance: Option<&Relevance>,
    deadline: Instant,
) -> Result<Source, PageError> {
    let Some(endpoint) = ocr_sidecar() else {
        return Err(PageError::NoTextLayer);
    };
    let _slot = DOCUMENT_SLOTS
        .acquire()
        .await
        .map_err(|_| PageError::Unreadable)?;
    let started = Instant::now();
    let client = crate::client_builder()
        .build()
        .map_err(|_| PageError::Transport)?;
    let step_deadline = deadline.min(Instant::now() + Duration::from_secs(45));
    let response = tokio::time::timeout_at(
        step_deadline,
        client
            .post(endpoint.clone())
            .header(header::CONTENT_TYPE, "application/pdf")
            .body(bytes.to_vec())
            .send(),
    )
    .await
    .map_err(|_| PageError::Timeout)?
    .map_err(|_| PageError::Transport)?;
    if !response.status().is_success() {
        return Err(PageError::NoTextLayer);
    }
    #[derive(Deserialize)]
    struct OcrResponse {
        markdown: Option<String>,
        text: Option<String>,
        pages: Option<usize>,
    }
    let body = tokio::time::timeout_at(step_deadline, read_body(response, PDF_TEXT_BYTES))
        .await
        .map_err(|_| PageError::Timeout)?
        .map_err(|_| PageError::Unreadable)?;
    let parsed: OcrResponse =
        serde_json::from_slice(&body).map_err(|_| PageError::Unreadable)?;
    let text = parsed
        .markdown
        .or(parsed.text)
        .filter(|t| !t.trim().is_empty())
        .ok_or(PageError::NoTextLayer)?;
    tracing::info!(
        stage = "pdf_ocr",
        host = url.host_str(),
        pages = parsed.pages.unwrap_or(0),
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        ok = true
    );
    let mut src = text_source(url, text, "pdf-ocr", relevance)?;
    src.metadata.pages_read = parsed.pages;
    Ok(src)
}

fn text_source(
    url: Url,
    text: String,
    retrieval: &'static str,
    relevance: Option<&Relevance>,
) -> Result<Source, PageError> {
    let title = first_markdown_heading(&text)
        .unwrap_or_else(|| pdf_name(&url));
    let Some(relevance) = relevance else {
        let mut src = Source::full(url, title, &text)?;
        src.metadata.retrieval = Some(retrieval);
        return Ok(src);
    };
    let passages = markdown_passages(&text);
    let mut src = source(url, title, passages.into_iter(), relevance)?;
    src.metadata.retrieval = Some(retrieval);
    Ok(src)
}

fn first_markdown_heading(md: &str) -> Option<String> {
    md.lines().take(20).find_map(|line| {
        let t = line.trim();
        t.starts_with('#')
            .then(|| t.trim_start_matches('#').trim())
            .filter(|s| !s.is_empty())
            .map(|s| shorten(s, 160))
    })
}

fn pretty_json_bytes(bytes: &[u8], content_type: Option<&str>) -> String {
    if let Ok(val) = serde_json::from_slice::<serde_json::Value>(bytes)
        && let Ok(pretty) = serde_json::to_string_pretty(&val)
    {
        return pretty;
    }
    decode_bytes(bytes, content_type)
}

fn feed_to_markdown(xml: &str, base_url: &Url) -> String {
    parse_feed(xml, base_url).1
}

/// Lightweight RSS 2.0 / Atom 1.0 feed parser -> clean Markdown + links.
fn parse_feed(xml: &str, base_url: &Url) -> (String, String, Vec<Url>) {
    static ITEM_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<(?:item|entry)\b[^>]*>(.*?)</(?:item|entry)>").unwrap());
    static TITLE_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<title\b[^>]*>(.*?)</title>").unwrap());
    static LINK_TAG_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?is)<link\b[^>]*href=["']([^"']+)["'][^>]*/?>|<link\b[^>]*>(.*?)</link>"#).unwrap());
    static DATE_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?is)<(?:pubDate|published|updated)\b[^>]*>(.*?)</(?:pubDate|published|updated)>")
            .unwrap()
    });
    static DESC_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?is)<(?:description|summary|content)\b[^>]*>(.*?)</(?:description|summary|content)>")
            .unwrap()
    });

    let strip_cdata = |raw: &str| -> String {
        let s = raw.trim();
        let s = s
            .strip_prefix("<![CDATA[")
            .and_then(|v| v.strip_suffix("]]>"))
            .unwrap_or(s);
        // Strip basic HTML tags inside feed descriptions
        let frag = Html::parse_fragment(s);
        let text: String = frag.root_element().text().collect();
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    };

    // Feed title is the first <title> before the first <item>/<entry>
    let head_end = ITEM_RE.find(xml).map_or(xml.len(), |m| m.start());
    let feed_title = TITLE_RE
        .captures(&xml[..head_end])
        .map(|c| strip_cdata(&c[1]))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| base_url.host_str().unwrap_or("Feed").to_owned());

    let mut md = format!("# {feed_title}\n\n");
    let mut links = Vec::new();
    let mut seen = HashSet::new();

    for cap in ITEM_RE.captures_iter(xml).take(100) {
        let block = &cap[1];
        let title = TITLE_RE
            .captures(block)
            .map(|c| strip_cdata(&c[1]))
            .unwrap_or_default();
        let link = LINK_TAG_RE.captures(block).and_then(|c| {
            let raw = c.get(1).or_else(|| c.get(2))?.as_str().trim();
            let raw = raw
                .strip_prefix("<![CDATA[")
                .and_then(|v| v.strip_suffix("]]>"))
                .unwrap_or(raw)
                .trim();
            base_url
                .join(raw)
                .ok()
                .and_then(|u| public_url(u).ok())
        });
        let date = DATE_RE
            .captures(block)
            .map(|c| strip_cdata(&c[1]))
            .filter(|s| !s.is_empty());
        let summary = DESC_RE
            .captures(block)
            .map(|c| strip_cdata(&c[1]))
            .filter(|s| !s.is_empty());

        if let Some(ref u) = link
            && seen.insert(u.clone())
        {
            links.push(u.clone());
        }

        if !title.is_empty() {
            if let Some(ref u) = link {
                md.push_str(&format!("## [{title}]({u})\n"));
            } else {
                md.push_str(&format!("## {title}\n"));
            }
        }
        if let Some(d) = date {
            md.push_str(&format!("*{d}*\n\n"));
        }
        if let Some(s) = summary {
            md.push_str(&shorten(&s, 600));
            md.push_str("\n\n");
        } else {
            md.push('\n');
        }
    }

    (feed_title, md.trim().to_owned(), links)
}

/// Reads the PDF text layer with poppler's `pdftotext`. This covers both
/// born-digital documents and scans that carry an OCR layer; image-only
/// scans have no text layer and are skipped. The child process is killed
/// when the request is dropped or the extraction budget runs out.
async fn pdf(url: Url, bytes: &[u8], relevance: Option<&Relevance>) -> Result<Source, PageError> {
    // Read one extra page in full-text mode to detect a truncated document.
    let last = if relevance.is_none() {
        "151"
    } else {
        PDF_PAGES
    };
    let mut child = Command::new("pdftotext")
        .args(["-q", "-l", last, "-enc", "UTF-8", "-eol", "unix", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| PageError::Unreadable)?;
    let mut stdin = child.stdin.take().ok_or(PageError::Unreadable)?;
    let reader = child.stdout.take().ok_or(PageError::Unreadable)?;
    let mut stdout = Vec::new();
    let success = tokio::time::timeout(PDF_EXTRACT_TIMEOUT, async {
        let write = async move {
            // Poppler can stop reading early. Its exit status decides.
            let _ = stdin.write_all(bytes).await;
        };
        let read = async {
            reader
                .take(PDF_TEXT_BYTES as u64 + 1)
                .read_to_end(&mut stdout)
                .await?;
            if stdout.len() > PDF_TEXT_BYTES {
                // Do not buffer an arbitrarily large text layer.
                child.kill().await?;
                Ok(true)
            } else {
                child.wait().await.map(|status| status.success())
            }
        };
        let ((), result) = tokio::join!(write, read);
        result
    })
    .await
    .map_err(|_| PageError::Timeout)?
    .map_err(|_| PageError::Unreadable)?;
    if !success {
        return Err(PageError::Unreadable);
    }
    let cut = stdout.len() > PDF_TEXT_BYTES;
    stdout.truncate(PDF_TEXT_BYTES);
    let text = String::from_utf8_lossy(&stdout).into_owned();
    let relevance = relevance.cloned();
    tokio::task::spawn_blocking(move || {
        let pages = text.matches('\u{c}').count().max(1);
        let Some(relevance) = relevance else {
            let mut page_text = text.split_terminator('\u{c}');
            let prefix = page_text
                .by_ref()
                .take(150)
                .collect::<Vec<_>>()
                .join("\n\n");
            let title = pdf_name(&url);
            let mut source =
                Source::full(url, title, &prefix).map_err(|_| PageError::NoTextLayer)?;
            source.mark_truncated(cut || page_text.next().is_some());
            source.metadata.retrieval = Some("pdf");
            source.metadata.pages_read = Some(pages.min(150));
            return Ok(source);
        };
        let passages = pdf_passages(&text);
        if passages.iter().map(|p| p.chars().count()).sum::<usize>() < 80 {
            return Err(PageError::NoTextLayer);
        }
        // The opening lines usually name the document; single-word labels
        // and running headers are skipped.
        let title = passages
            .iter()
            .take(8)
            .find(|p| p.split_whitespace().count() >= 3 && p.chars().count() >= 16)
            .map_or_else(|| pdf_name(&url), |p| shorten(p, 160));
        let mut source = source(url, title, passages.into_iter(), &relevance)?;
        source.metadata.retrieval = Some("pdf");
        source.metadata.pages_read = Some(pages);
        Ok(source)
    })
    .await
    .map_err(|_| PageError::Unreadable)?
}

/// Rebuilds paragraphs from pdftotext lines: blank lines and page breaks end
/// a paragraph, and words hyphenated across a line break are joined.
fn pdf_passages(text: &str) -> Vec<String> {
    // Table-of-contents dot leaders carry no content.
    static LEADER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:\s*[.·…]){4,}\s*").expect("leader pattern"));
    let mut passages = Vec::new();
    let mut current = String::new();
    let mut flush = |current: &mut String| {
        if !current.is_empty() {
            passages.push(std::mem::take(current));
        }
    };
    for raw in text.split('\n') {
        let page_break = raw.contains('\u{c}');
        let line = raw.replace('\u{c}', "");
        let line = LEADER.replace_all(&line, " … ");
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if page_break || line.is_empty() {
            flush(&mut current);
        }
        if line.is_empty() {
            continue;
        }
        if current.ends_with('-')
            && current
                .chars()
                .rev()
                .nth(1)
                .is_some_and(char::is_alphabetic)
            && line.chars().next().is_some_and(char::is_lowercase)
        {
            current.pop();
        } else if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(&line);
    }
    flush(&mut current);
    passages
}

/// Cuts at a word boundary and marks the cut.
fn shorten(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let cut = truncate(text, limit);
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    format!("{}…", cut.trim_end_matches([',', ';', ':']))
}

fn pdf_name(url: &Url) -> String {
    url.path_segments()
        .and_then(|mut segments| segments.next_back())
        .map(|name| {
            percent_decode(name)
                .trim_end_matches(".pdf")
                .replace(['_', '-'], " ")
        })
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| url.host_str().unwrap_or("PDF document").to_owned())
}

fn percent_decode(text: &str) -> String {
    percent_encoding::percent_decode_str(text)
        .decode_utf8_lossy()
        .into_owned()
}

pub fn words(input: &str) -> impl Iterator<Item = String> + '_ {
    input
        .split(|value: char| !value.is_alphanumeric())
        .filter(|value| value.chars().take(4).count() >= 4)
        .map(str::to_lowercase)
}

fn extract(
    url: Url,
    bytes: &[u8],
    media_type: Option<&str>,
    relevance: Option<&Relevance>,
    rendered: bool,
) -> Result<Source, PageError> {
    let charset = media_type.and_then(|media_type| {
        media_type.split(';').skip(1).find_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            (name.trim() == "charset").then(|| value.trim().trim_matches(['\'', '"']))
        })
    });
    let encoding = charset
        .and_then(|charset| encoding_rs::Encoding::for_label(charset.as_bytes()))
        .or_else(|| sniff_encoding(bytes))
        .unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = encoding.decode(bytes);
    let fallback_title = || url.host_str().unwrap_or("Web source").to_owned();
    if media_type.is_some_and(|media_type| media_type.starts_with("text/plain")) {
        let Some(relevance) = relevance else {
            let title = fallback_title();
            return Source::full(url, title, &text);
        };
        return source(
            url.clone(),
            fallback_title(),
            text.lines().map(str::to_owned),
            relevance,
        );
    }

    static TITLE: LazyLock<Selector> =
        LazyLock::new(|| Selector::parse("title").expect("title selector"));
    static REGIONS: LazyLock<Selector> =
        LazyLock::new(|| Selector::parse("main, article, [role=main]").expect("article selector"));
    static BLOCKS: LazyLock<Selector> = LazyLock::new(|| {
        Selector::parse(
            "h1, h2, h3, h4, p, li, td, th, dd, dt, blockquote, pre, figcaption, summary, div",
        )
        .expect("passage selector")
    });
    static CHALLENGE: LazyLock<Selector> = LazyLock::new(|| {
        Selector::parse("#challenge-form, #cf-challenge-running").expect("challenge selector")
    });
    let document = Html::parse_document(&text);
    let title = document
        .select(&TITLE)
        .next()
        .map(|element| normalize(element.text(), 300))
        .filter(|title| !title.is_empty())
        .unwrap_or_else(fallback_title);
    if document.select(&CHALLENGE).next().is_some()
        || matches!(
            title.to_lowercase().as_str(),
            "just a moment..."
                | "access denied"
                | "attention required! | cloudflare"
                | "robot check"
        )
    {
        return Err(PageError::Blocked);
    }

    let Some(relevance) = relevance else {
        let mut text = clean::page_text(document.root_element(), false);
        // A page made only of navigation, such as a sitemap, keeps it.
        if text.trim().is_empty() {
            text = clean::page_text(document.root_element(), true);
        }
        let body = std::slice::from_ref(&text);
        if needs_script(&document, body, text.len(), rendered)
            || (!rendered && client_template(&document, body))
        {
            return Err(PageError::NeedsJs);
        }
        return Source::full(url, title, text.trim_matches(['\n', '\r']));
    };
    let blocks = |region: scraper::ElementRef<'_>| {
        region
            .select(&BLOCKS)
            .take(MAX_BLOCKS)
            .filter(|element| {
                !discarded(*element)
                    && !element
                        .ancestors()
                        .filter_map(scraper::ElementRef::wrap)
                        .any(discarded)
            })
            .map(|element| {
                let text = if element.value().name() == "div" && !leaf_div(element) {
                    // Inline siblings of nested blocks can carry the actual
                    // article text (e.g. a quotation beside a list of tags).
                    normalize(
                        element.children().flat_map(|node| {
                            if let Some(child) = scraper::ElementRef::wrap(node)
                                && clean::inline(child.value().name())
                                && !discarded(child)
                            {
                                return visible_text(child).collect::<Vec<_>>();
                            }
                            node.value().as_text().map(|t| &**t).into_iter().collect()
                        }),
                        2000,
                    )
                } else {
                    normalize(visible_text(element), 2000)
                };
                match cell_label(element) {
                    Some(label) if !label.is_empty() && label != text => format!("{label}: {text}"),
                    _ => text,
                }
            })
            .collect::<Vec<_>>()
    };
    let size = |blocks: &[String]| blocks.iter().map(String::len).sum::<usize>();
    let body = blocks(document.root_element());
    let body_size = size(&body);
    if needs_script(&document, &body, body_size, rendered)
        || (!rendered && client_template(&document, &body))
    {
        return Err(PageError::NeedsJs);
    }
    // Use the largest content region only when it holds a fair share of the
    // page; some sites mark a sidebar as <main> or list teasers as <article>.
    let region = document
        .select(&REGIONS)
        .take(64)
        .map(blocks)
        .max_by_key(|region| size(region))
        .filter(|region| size(region) >= 500 && size(region) * 10 >= body_size * 3);
    let mut passages = region.unwrap_or(body);
    passages.extend(clean::contact_passages(document.root_element(), &title));
    source(url, title, passages.into_iter(), relevance)
}

/// Script shells: a "JavaScript required" notice or an empty app root.
fn needs_script(document: &Html, body: &[String], body_size: usize, rendered: bool) -> bool {
    static SHELL: LazyLock<Selector> = LazyLock::new(|| {
        Selector::parse("app-root, #root, #app, #__next, #__nuxt, [ng-version], [data-reactroot]")
            .expect("shell selector")
    });
    static NOSCRIPT: LazyLock<Selector> =
        LazyLock::new(|| Selector::parse("noscript").expect("noscript selector"));
    // Server-rendered apps keep their root element, so a root alone only
    // counts when the page carries almost no text.
    let empty_root = body_size < 3000
        && document
            .select(&SHELL)
            .any(|root| !visible_text(root).any(|part| !part.trim().is_empty()));
    empty_root
        || (body_size < 1500
            && (body
                .iter()
                .flat_map(|block| block.lines())
                .any(script_notice)
                || (!rendered
                    && body_size < 300
                    && document
                        .select(&NOSCRIPT)
                        .any(|element| script_notice(&element.text().collect::<String>())))))
}

/// Google Docs, Sheets and Drive links point at script applications; their
/// export endpoints serve the content as HTML or the original file.
fn export_url(url: Url) -> Url {
    let segments = url
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();
    let rewritten = match (url.host_str(), segments.as_slice()) {
        // Published documents (/d/e/...) are already plain HTML.
        (Some("docs.google.com"), ["document", "d", id, ..]) if *id != "e" => {
            format!("https://docs.google.com/document/d/{id}/export?format=html")
        }
        (Some("docs.google.com"), ["spreadsheets", "d", id, ..]) if *id != "e" => {
            // Fragments are stripped earlier, so only a `gid` query survives.
            let gid = url
                .query_pairs()
                .find(|(key, _)| key == "gid")
                .map(|(_, value)| format!("&gid={value}"))
                .unwrap_or_default();
            format!("https://docs.google.com/spreadsheets/d/{id}/gviz/tq?tqx=out:html{gid}")
        }
        (Some("drive.google.com"), ["file", "d", id, ..]) => {
            format!("https://drive.google.com/uc?export=download&id={id}")
        }
        _ => return url,
    };
    Url::parse(&rewritten)
        .ok()
        .and_then(|url| public_url(url).ok())
        .unwrap_or(url)
}

/// Splits a long block into groups of whole sentences of about 500
/// characters, so one matching sentence does not spend the whole budget.
fn sentence_groups(text: &str) -> Vec<String> {
    const TARGET: usize = 500;
    if text.chars().count() <= TARGET + 200 {
        return vec![text.to_owned()];
    }
    let mut groups = Vec::new();
    let mut current = String::new();
    let mut chars = text.char_indices().peekable();
    let mut start = 0;
    while let Some((index, c)) = chars.next() {
        let boundary = matches!(c, '.' | '!' | '?')
            && chars.peek().is_some_and(|(_, next)| next.is_whitespace());
        if boundary {
            let end = index + c.len_utf8();
            current.push_str(&text[start..end]);
            start = end;
            if current.chars().count() >= TARGET {
                groups.push(std::mem::take(&mut current).trim().to_owned());
            }
        }
    }
    current.push_str(&text[start..]);
    if !current.trim().is_empty() {
        groups.push(current.trim().to_owned());
    }
    groups
}

/// Row and column headers of a table cell, so a figure keeps its meaning
/// ("Erstbesteigung: 14. Juli 1865") when it is selected on its own.
fn cell_label(cell: scraper::ElementRef<'_>) -> Option<String> {
    static ROWS: LazyLock<Selector> =
        LazyLock::new(|| Selector::parse("tr").expect("row selector"));
    if cell.value().name() != "td" {
        return None;
    }
    let is_cell = |element: &scraper::ElementRef<'_>| matches!(element.value().name(), "td" | "th");
    let row = cell
        .parent()
        .and_then(scraper::ElementRef::wrap)
        .filter(|row| row.value().name() == "tr")?;
    let cells = row
        .children()
        .filter_map(scraper::ElementRef::wrap)
        .filter(is_cell)
        .collect::<Vec<_>>();
    let mut parts = Vec::new();
    if let Some(header) = cells.iter().find(|c| c.value().name() == "th") {
        parts.push(normalize(visible_text(*header), 120));
    }
    let index = cells.iter().position(|c| c.id() == cell.id())?;
    let table = row
        .ancestors()
        .filter_map(scraper::ElementRef::wrap)
        .find(|element| element.value().name() == "table")?;
    let column = table
        .select(&ROWS)
        .take(5)
        .find(|candidate| {
            candidate.id() != row.id()
                && candidate
                    .children()
                    .filter_map(scraper::ElementRef::wrap)
                    .filter(is_cell)
                    .all(|c| c.value().name() == "th")
        })
        .and_then(|header| {
            header
                .children()
                .filter_map(scraper::ElementRef::wrap)
                .filter(is_cell)
                .nth(index)
        })
        .map(|header| normalize(visible_text(header), 120));
    if let Some(column) = column.filter(|text| !text.is_empty() && !parts.contains(text)) {
        parts.push(column);
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Encoding from a byte-order mark or a `<meta charset>` / `http-equiv`
/// declaration near the start, for servers that send no charset header.
fn sniff_encoding(bytes: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    if let Some((encoding, _)) = encoding_rs::Encoding::for_bom(bytes) {
        return Some(encoding);
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]).to_ascii_lowercase();
    let start = head.find("charset=")? + "charset=".len();
    let label = head[start..]
        .trim_start_matches(['"', '\''])
        .split(|c: char| matches!(c, '"' | '\'' | ';' | '>' | '/') || c.is_whitespace())
        .next()?;
    encoding_rs::Encoding::for_label(label.as_bytes())
}

/// An unrendered client-side template (AngularJS, Vue, Alpine): framework
/// binding attributes or visible `{{ placeholder }}` expressions. The server
/// HTML of such pages holds labels but not the content.
fn client_template(document: &Html, body: &[String]) -> bool {
    static BINDINGS: LazyLock<Selector> = LazyLock::new(|| {
        Selector::parse(
            "[ng-app], [data-ng-app], [ng-controller], [data-ng-controller], \
             [ng-bind], [data-ng-bind], [ng-bind-html], [data-ng-bind-html], \
             [ng-repeat], [data-ng-repeat], [ng-if], [data-ng-if], [v-cloak], [x-data]",
        )
        .expect("template selector")
    });
    static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\{\{\s*[A-Za-z_][\w.]*\s*\}\}").expect("placeholder pattern")
    });
    document.select(&BINDINGS).take(5).count() >= 5
        || body
            .iter()
            .map(|block| PLACEHOLDER.find_iter(block).count())
            .sum::<usize>()
            >= 3
}

/// A short "enable JavaScript" or unsupported-browser message.
fn script_notice(text: &str) -> bool {
    static NOTICE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"(?i)(?:enable|activate|allow|requires?)\s+javascript|javascript (?:is (?:required|disabled)|required|aktivieren|einschalten)",
            r"|(?:activer|activez|attivare|attiva)\s+javascript|javascript[- ](?:capable|fähigen) browser",
            r"|(?:navigateur supportant|browser che supporta) javascript|browser (is )?(not|nicht) (supported|unterstützt)",
            r"|navigateur (n'est pas|non) (pris en charge|supporté)",
        ))
        .expect("script notice pattern")
    });
    text.chars().count() < 400 && NOTICE.is_match(text)
}

/// Short boilerplate about JavaScript, cookies or browser support.
fn notice(text: &str) -> bool {
    static NOTICE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"(?i)(?:accept|allow|enable|accepter|akzeptieren)\s+(?:all |alle |les |tous les )?cookies",
            r"|cookies (?:akzeptieren|zulassen)",
        ))
        .expect("notice pattern")
    });
    script_notice(text) || (text.chars().count() < 400 && NOTICE.is_match(text))
}

/// Text of an element without inline script, style or template content.
fn visible_text(element: scraper::ElementRef<'_>) -> impl Iterator<Item = &str> {
    element.descendants().filter_map(|node| {
        let text = node.value().as_text()?;
        let hidden = node
            .parent()
            .and_then(|parent| parent.value().as_element())
            .is_some_and(|parent| {
                matches!(parent.name(), "script" | "style" | "noscript" | "template")
            });
        (!hidden).then_some(&**text)
    })
}

/// Navigation and page chrome. Forms are kept: ASP.NET and SharePoint
/// sites wrap the whole page in one.
fn discarded(element: scraper::ElementRef<'_>) -> bool {
    matches!(
        element.value().name(),
        "nav" | "footer" | "aside" | "template" | "script" | "style" | "noscript"
    ) || element.attr("hidden").is_some()
}

/// A <div> used as a paragraph: it holds text and inline markup but no
/// block-level children, so its text is not counted twice.
fn leaf_div(element: scraper::ElementRef<'_>) -> bool {
    !element
        .children()
        .filter_map(scraper::ElementRef::wrap)
        .any(|child| {
            matches!(
                child.value().name(),
                "div"
                    | "p"
                    | "ul"
                    | "ol"
                    | "li"
                    | "table"
                    | "section"
                    | "article"
                    | "main"
                    | "header"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "blockquote"
                    | "pre"
                    | "dl"
                    | "figure"
                    | "details"
                    | "form"
                    | "nav"
                    | "aside"
                    | "footer"
            )
        })
}

fn normalize<'a>(parts: impl Iterator<Item = &'a str>, limit: usize) -> String {
    let mut text = String::new();
    let mut remaining = limit;
    for word in parts.flat_map(str::split_whitespace) {
        if !text.is_empty() {
            text.push(' ');
            remaining -= 1;
        }
        if let Some((end, _)) = word.char_indices().nth(remaining) {
            text.push_str(&word[..end]);
            break;
        }
        text.push_str(word);
        remaining -= word.chars().count();
        if remaining <= 1 {
            break;
        }
    }
    text
}

fn source(
    url: Url,
    title: String,
    passages: impl Iterator<Item = String>,
    relevance: &Relevance,
) -> Result<Source, PageError> {
    let mut seen = HashSet::new();
    let blocks = passages
        .take(MAX_PASSAGES)
        .map(|part| truncate(&part, 2000))
        .flat_map(|part| sentence_groups(&part))
        .filter(|part| {
            part.trim().chars().count() >= 24 && !notice(part) && seen.insert(part.clone())
        })
        .collect::<Vec<_>>();
    // Which keywords each block contains, then an IDF weight per keyword so
    // a term on every block (the page topic) counts less than the asked-for
    // detail.
    let present = blocks
        .iter()
        .map(|text| {
            let words = words(text).collect::<HashSet<_>>();
            relevance
                .keywords
                .iter()
                .map(|keyword| {
                    words.contains(keyword)
                        || words.iter().any(|word| matches_keyword(word, keyword))
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    #[allow(clippy::cast_precision_loss)]
    let total = blocks.len().max(1) as f64;
    let weights = (0..relevance.keywords.len())
        .map(|index| {
            let frequency = present.iter().filter(|row| row[index]).count();
            #[allow(clippy::cast_precision_loss)]
            let frequency = frequency as f64;
            if frequency == 0.0 {
                0.0
            } else {
                (1.0 + total / frequency).ln()
            }
        })
        .collect::<Vec<_>>();
    let scores = blocks
        .iter()
        .zip(&present)
        .map(|(text, row)| {
            let mut score = row
                .iter()
                .zip(&weights)
                .filter(|(present, _)| **present)
                .map(|(_, weight)| weight)
                .sum::<f64>();
            if relevance.numeric && score > 0.0 && text.chars().any(|c| c.is_ascii_digit()) {
                score *= 1.3;
            }
            score
        })
        .collect::<Vec<_>>();
    let mut passages = blocks
        .into_iter()
        .enumerate()
        .map(|(index, text)| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let score = (scores[index] * 1000.0) as u64;
            (index, score, text)
        })
        .collect::<Vec<_>>();
    // Fill the passage budget with the most relevant blocks, then restore
    // document order so the excerpt reads naturally.
    passages.sort_unstable_by_key(|&(index, score, _)| (std::cmp::Reverse(score), index));
    let mut used = 0;
    passages.retain(|(_, _, text)| {
        let length = text.chars().count() + 2;
        if used + length > PASSAGE_BUDGET && used > 0 {
            return false;
        }
        used += length;
        true
    });
    passages.sort_unstable_by_key(|&(index, _, _)| index);
    let content = truncate(
        &passages
            .into_iter()
            .map(|(_, _, text)| text)
            .collect::<Vec<_>>()
            .join("\n\n"),
        PASSAGE_BUDGET,
    );
    if content.chars().count() < 80 {
        return Err(PageError::Unreadable);
    }
    Ok(Source {
        content,
        metadata: Metadata {
            title: truncate(&title, 300),
            url,
            retrieval: None,
            pages_read: None,
            truncated: None,
        },
    })
}

pub fn prioritize(requested: Vec<Url>, discovered: Vec<Url>) -> Vec<Url> {
    let mut seen_urls = HashSet::new();
    let mut seen_hosts = HashSet::new();
    let mut first = Vec::new();
    let mut rest = Vec::new();
    for url in requested {
        if seen_urls.insert(url.clone()) {
            seen_hosts.insert(url.host_str().unwrap_or_default().to_owned());
            first.push(url);
        }
    }
    for url in discovered
        .into_iter()
        .filter_map(|url| public_url(url).ok())
    {
        if seen_urls.insert(url.clone()) {
            if seen_hosts.insert(url.host_str().unwrap_or_default().to_owned()) {
                first.push(url);
            } else {
                rest.push(url);
            }
        }
    }
    first.extend(rest);
    first
}
