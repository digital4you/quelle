use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    process::Stdio,
    sync::{LazyLock, OnceLock},
    time::Duration,
};

use regex::Regex;
use reqwest::{StatusCode, header};
use scraper::{Html, Selector};
use serde::Serialize;
use tokio::{
    io::AsyncWriteExt, net::lookup_host, process::Command, sync::OwnedSemaphorePermit,
    time::Instant,
};
use url::{Host, Url};

use crate::{client_builder, read_body, truncate};

/// HTML beyond this is cut off and the prefix is parsed; long pages keep
/// their opening content instead of failing.
const PAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_BLOCKS: usize = 3000;
/// Candidate passages ranked per source; a spreadsheet or CSV can have
/// thousands of rows.
const MAX_PASSAGES: usize = 20_000;
const MAX_REDIRECTS: usize = 3;
const PAGE_TIMEOUT: Duration = Duration::from_secs(5);
const PASSAGE_BUDGET: usize = 6500;
/// Official PDFs are larger and slower than HTML pages but often the
/// primary source, so they get a bigger byte and time budget.
const PDF_BYTES: usize = 12 * 1024 * 1024;
const PDF_TIMEOUT: Duration = Duration::from_secs(8);
const PDF_EXTRACT_TIMEOUT: Duration = Duration::from_secs(5);
/// Search answers rarely need more than the opening pages; this bounds
/// extraction time for long reports.
const PDF_PAGES: &str = "150";
const PDF_TEXT_BYTES: usize = 4 * 1024 * 1024;

static PDF_SUPPORT: OnceLock<bool> = OnceLock::new();

/// Whole budget for one page: download, optional rendering and parsing.
const FETCH_BUDGET: Duration = Duration::from_secs(15);
const RENDER_TIMEOUT: Duration = Duration::from_secs(10);
/// Pages with less readable text than this also get a rendering attempt.
const THIN_PAGE: usize = 800;
/// Each render is a separate browser process of roughly 100 MB.
static RENDER_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
static RENDERER: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
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
     (KHTML, like Gecko) Chrome/157.0.0.0 Safari/537.36";
static USER_AGENT: OnceLock<header::HeaderValue> = OnceLock::new();

pub fn set_user_agent(value: header::HeaderValue) {
    let _ = USER_AGENT.set(value);
}

/// Wikimedia asks automated clients to identify themselves and throttles
/// browser-like ones (HTTP 429), so its sites get an honest identity.
fn user_agent_for(url: &Url) -> header::HeaderValue {
    let wikimedia = url.host_str().is_some_and(|host| {
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
    });
    if wikimedia {
        header::HeaderValue::from_static(HONEST_AGENT)
    } else {
        user_agent()
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Metadata {
    pub title: String,
    pub url: Url,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieval: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pages_read: Option<usize>,
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

fn public_address(address: IpAddr) -> bool {
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
) -> Result<Vec<u8>, crate::BodyError> {
    if !keep_prefix {
        return read_body(response, limit).await;
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| crate::BodyError::Read)? {
        let room = limit - data.len();
        if chunk.len() >= room {
            data.extend_from_slice(&chunk[..room]);
            break;
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

async fn download(mut url: Url) -> Result<(Url, Vec<u8>, Option<String>, Kind), PageError> {
    let started = Instant::now();
    for step in 0..=MAX_REDIRECTS {
        let mut client = client_builder();
        if let Some(Host::Domain(host)) = url.host() {
            let port = url.port_or_known_default().ok_or(PageError::Target)?;
            let addresses: Vec<SocketAddr> = lookup_host((host, port))
                .await
                .map_err(|_| PageError::Transport)?
                .collect();
            if addresses.is_empty() || !addresses.iter().all(|addr| public_address(addr.ip())) {
                return Err(PageError::Target);
            }
            client = client.resolve_to_addrs(host, &addresses);
        }
        let response = client
            .build()
            .map_err(|_| PageError::Transport)?
            .get(url.clone())
            .header(
                header::ACCEPT,
                if pdf_enabled() {
                    "text/html,application/xhtml+xml,application/xml;q=0.9,application/pdf;q=0.8,text/plain;q=0.5,*/*;q=0.1"
                } else {
                    "text/html,application/xhtml+xml,application/xml;q=0.9,text/plain;q=0.5,*/*;q=0.1"
                },
            )
            .header(
                header::ACCEPT_LANGUAGE,
                "de-CH,de;q=0.9,fr-CH;q=0.8,fr;q=0.7,it;q=0.6,en;q=0.5",
            )
            .header(header::USER_AGENT, user_agent_for(&url))
            .send();
        let response = tokio::time::timeout_at(started + PAGE_TIMEOUT, response)
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
                .and_then(|value| value.to_str().ok())
                .ok_or(PageError::Blocked)?;
            url = public_url(url.join(location).map_err(|_| PageError::Target)?)?;
            continue;
        }
        if !response.status().is_success()
            || response
                .headers()
                .get("cf-mitigated")
                .is_some_and(|value| value == "challenge")
        {
            tracing::info!(stage = "page_refused", status = response.status().as_u16());
            return Err(PageError::Blocked);
        }
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_ascii_lowercase);
        let pdf_path = url.path().to_ascii_lowercase().ends_with(".pdf");
        // `None` means sniff the body: missing or generic binary types.
        let declared = match content_type
            .as_deref()
            .map(|value| value.split(';').next().unwrap_or_default().trim())
        {
            Some("text/html" | "application/xhtml+xml") => Some(Kind::Html),
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
        let bytes = tokio::time::timeout_at(
            started + budget,
            read_page(response, limit, !maybe_pdf && !maybe_document),
        )
        .await
        .map_err(|_| PageError::Timeout)?
        .map_err(|error| match error {
            crate::BodyError::TooLarge => PageError::TooLarge,
            _ => PageError::Transport,
        })?;
        let is_pdf = bytes.starts_with(b"%PDF-");
        let kind = match declared {
            Some(Kind::Pdf) if !is_pdf => return Err(PageError::Unreadable),
            Some(kind) => kind,
            None if is_pdf && pdf_enabled() => Kind::Pdf,
            None if maybe_pdf => return Err(PageError::Unsupported),
            None => match anydoc::Format::from_bytes(&bytes) {
                Some(anydoc::Format::Pdf) => return Err(PageError::Unsupported),
                Some(format) => Kind::Document(Some(format)),
                None if extension == "csv" => Kind::Document(Some(anydoc::Format::Csv)),
                None if maybe_document => return Err(PageError::Unsupported),
                None => Kind::Html,
            },
        };
        return Ok((url, bytes, content_type, kind));
    }
    Err(PageError::Blocked)
}

pub async fn fetch(
    url: Url,
    relevance: &Relevance,
    permit: OwnedSemaphorePermit,
) -> Result<Source, PageError> {
    let _permit = permit;
    tokio::time::timeout(FETCH_BUDGET, async {
        let (url, bytes, content_type, kind) = download(export_url(url)).await?;
        match kind {
            Kind::Pdf => return pdf(url, &bytes, relevance).await,
            Kind::Document(format) => return document(url, bytes, format, relevance).await,
            Kind::Html | Kind::Text => {}
        }
        let rendered = |url: Url| async move {
            let html = render(&url).await?;
            let mut source = parse(
                url,
                html,
                Some("text/html; charset=utf-8".to_owned()),
                relevance,
            )
            .await?;
            source.metadata.retrieval = Some("rendered");
            Ok::<_, PageError>(source)
        };
        match parse(url.clone(), bytes, content_type, relevance).await {
            // Script-only pages are rendered. So are pages whose markup holds
            // almost no readable text; the rendered result is kept only when
            // it carries more.
            Err(PageError::NeedsJs | PageError::Unreadable) if render_enabled() => {
                rendered(url).await
            }
            Err(PageError::NeedsJs) => {
                tracing::info!(stage = "needs_js", host = url.host_str());
                Err(PageError::NeedsJs)
            }
            Ok(source) if render_enabled() && source.content.chars().count() < THIN_PAGE => {
                match rendered(url).await {
                    Ok(better)
                        if better.content.chars().count() > source.content.chars().count() =>
                    {
                        Ok(better)
                    }
                    _ => Ok(source),
                }
            }
            other => other,
        }
    })
    .await
    .map_err(|_| PageError::Timeout)?
}

async fn parse(
    url: Url,
    bytes: Vec<u8>,
    content_type: Option<String>,
    relevance: &Relevance,
) -> Result<Source, PageError> {
    let relevance = relevance.clone();
    tokio::task::spawn_blocking(move || extract(url, &bytes, content_type.as_deref(), &relevance))
        .await
        .map_err(|_| PageError::Unreadable)?
}

/// Renders a script-only page with Lightpanda and returns the resulting
/// DOM as HTML. Lightpanda checks every connection, including scripts and
/// XHR, against private and reserved ranges after DNS resolution, so page
/// scripts cannot reach internal services. Telemetry is disabled, the
/// environment is cleared, and the process is killed when the request ends.
async fn render(url: &Url) -> Result<Vec<u8>, PageError> {
    let Some(binary) = RENDERER.get().and_then(Option::as_ref) else {
        return Err(PageError::NeedsJs);
    };
    let _slot = RENDER_SLOTS
        .acquire()
        .await
        .map_err(|_| PageError::NeedsJs)?;
    let started = Instant::now();
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
            "document.body && (document.body.innerText || '').length > 3000",
            "--wait-ms",
            "6000",
            "--terminate-ms",
            "8000",
            "--http-timeout",
            "6000",
            "--http-max-response-size",
            "10485760",
            "--v8-max-heap-mb",
            "256",
            "--block-private-networks",
            "--locale",
            "de-CH",
            "--log-level",
            "fatal",
            "--user-agent-suffix",
            HONEST_AGENT,
        ]);
    for range in BLOCKED_RANGES {
        command.args(["--block-cidrs", range]);
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
    html.truncate(PAGE_BYTES);
    Ok(html)
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
/// same passage ranking as HTML and PDF.
async fn document(
    url: Url,
    bytes: Vec<u8>,
    format: Option<anydoc::Format>,
    relevance: &Relevance,
) -> Result<Source, PageError> {
    let relevance = relevance.clone();
    let task = tokio::task::spawn_blocking(move || {
        let markdown = std::panic::catch_unwind(|| anydoc::to_markdown_bytes(&bytes, format))
            .map_err(|_| PageError::Unreadable)?
            .map_err(|_| PageError::Unreadable)?;
        let passages = markdown_passages(&markdown);
        let title = passages
            .iter()
            .take(8)
            .find(|p| p.split_whitespace().count() >= 3 && p.chars().count() >= 16)
            .map_or_else(|| pdf_name(&url), |p| shorten(p, 160));
        let mut source = source(url, title, passages.into_iter(), &relevance)?;
        source.metadata.retrieval = Some("document");
        Ok(source)
    });
    tokio::time::timeout(PDF_EXTRACT_TIMEOUT, task)
        .await
        .map_err(|_| PageError::Timeout)?
        .map_err(|_| PageError::Unreadable)?
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

/// Reads the PDF text layer with poppler's `pdftotext`. This covers both
/// born-digital documents and scans that carry an OCR layer; image-only
/// scans have no text layer and are skipped. The child process is killed
/// when the request is dropped or the extraction budget runs out.
async fn pdf(url: Url, bytes: &[u8], relevance: &Relevance) -> Result<Source, PageError> {
    let mut child = Command::new("pdftotext")
        .args([
            "-q", "-l", PDF_PAGES, "-enc", "UTF-8", "-eol", "unix", "-", "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| PageError::Unreadable)?;
    let mut stdin = child.stdin.take().ok_or(PageError::Unreadable)?;
    let input = bytes.to_vec();
    // pdftotext may stop reading early on a broken file; a write error then
    // is expected and the exit status decides.
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(&input).await;
    });
    let output = tokio::time::timeout(PDF_EXTRACT_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| PageError::Timeout)?
        .map_err(|_| PageError::Unreadable)?;
    writer.abort();
    if !output.status.success() {
        return Err(PageError::Unreadable);
    }
    let mut stdout = output.stdout;
    stdout.truncate(PDF_TEXT_BYTES);
    let text = String::from_utf8_lossy(&stdout).into_owned();
    let relevance = relevance.clone();
    tokio::task::spawn_blocking(move || {
        let pages = text.matches('\u{c}').count().max(1);
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
    url::form_urlencoded::parse(format!("x={}", text.replace('+', "%2B")).as_bytes())
        .next()
        .map_or_else(|| text.to_owned(), |(_, value)| value.into_owned())
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
    relevance: &Relevance,
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

    let blocks = |region: scraper::ElementRef<'_>| {
        region
            .select(&BLOCKS)
            .take(MAX_BLOCKS)
            .filter(|element| {
                (element.value().name() != "div" || leaf_div(*element))
                    && !discarded(*element)
                    && !element
                        .ancestors()
                        .skip(1)
                        .filter_map(scraper::ElementRef::wrap)
                        .any(discarded)
            })
            .map(|element| {
                let text = normalize(visible_text(element), 2000);
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
    if needs_script(&document, &body, body_size)
        || (render_enabled() && client_template(&document, &body))
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
    let passages = region.unwrap_or(body);
    source(url, title, passages.into_iter(), relevance)
}

/// Script shells: a "JavaScript required" notice or an empty app root.
fn needs_script(document: &Html, body: &[String], body_size: usize) -> bool {
    static SHELL: LazyLock<Selector> = LazyLock::new(|| {
        Selector::parse("app-root, #root, #app, #__next, #__nuxt, [ng-version], [data-reactroot]")
            .expect("shell selector")
    });
    static NOSCRIPT: LazyLock<Selector> =
        LazyLock::new(|| Selector::parse("noscript").expect("noscript selector"));
    // Server-rendered apps keep their root element, so a root alone only
    // counts when the page carries almost no text.
    (body_size < 1500
        && (body.iter().any(|block| script_notice(block))
            || document
                .select(&NOSCRIPT)
                .any(|element| script_notice(&element.text().collect::<String>()))))
        || (body_size < 300 && document.select(&SHELL).next().is_some())
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
            "[ng-app], [data-ng-app], [ng-controller], [ng-bind], [ng-repeat], [ng-if], [v-cloak], [x-data]",
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
            r"(?i)javascript|browser (is )?(not|nicht) (supported|unterstützt)",
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
            r"(?i)javascript|cookies?\b|browser (is )?(not|nicht) (supported|unterstützt)",
            r"|navigateur (n'est pas|non) (pris en charge|supporté)",
        ))
        .expect("notice pattern")
    });
    text.chars().count() < 400 && NOTICE.is_match(text)
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
