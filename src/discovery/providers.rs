use std::{
    collections::HashSet,
    env,
    sync::LazyLock,
    time::{Duration, SystemTime},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use regex::Regex;
use reqwest::{Client, Response, StatusCode, header};
use scraper::{Html, Selector};
use serde::Deserialize;
use url::Url;

use super::{
    Candidate, DiscoveryError, DiscoveryFuture, DiscoveryMode, EnginePool, Language, Query,
    SearchEngine, SearchOptions,
};
use crate::{
    endpoint, env_text, pages::public_url, read_body, read_json, secret_header, sidecar_endpoint,
    truncate,
};

const BODY_LIMIT: usize = 2 * 1024 * 1024;
const HONEST_USER_AGENT: &str = "Quelle/0.1 (+https://github.com/digital4you/quelle)";
const FIREFOX_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:138.0) Gecko/20100101 Firefox/138.0";
const CHROME_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36";
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(60);
/// Default keyless meta-search engine mix fused via parallel RRF.
const DEFAULT_KEYLESS_ENGINES: &str = "duckduckgo,mwmbl,wikipedia,stackexchange,hackernews";

#[derive(Clone, Copy)]
pub(super) enum HtmlEngine {
    Bing,
    DuckDuckGo,
}

enum Provider {
    Brave {
        endpoint: Url,
        key: header::HeaderValue,
    },
    Html(HtmlEngine),
    Mwmbl {
        key: Option<String>,
    },
    Mojeek {
        endpoint: Url,
        key: String,
    },
    Wikipedia,
    StackExchange,
    HackerNews,
    Arxiv,
    GitHub,
    Searxng {
        endpoint: Url,
        engines: Option<String>,
        categories: String,
    },
}

struct NativeEngine {
    client: Client,
    provider: Provider,
}

fn api_endpoint(name: &'static str, default: &str) -> Result<Url, &'static str> {
    match env::var(name) {
        Ok(value) => endpoint(&value, name),
        Err(env::VarError::NotPresent) => endpoint(default, name),
        Err(_) => Err(name),
    }
}

pub fn configure_engines(client: &Client, service_token: &str) -> Result<EnginePool, &'static str> {
    let setting = match env::var("QUELLE_SEARCH_ENGINES") {
        Ok(list) => list,
        Err(env::VarError::NotPresent)
            if env::var_os("BRAVE_SEARCH_API_KEY").is_some_and(|v| !v.is_empty()) =>
        {
            "brave".into()
        }
        Err(env::VarError::NotPresent) => DEFAULT_KEYLESS_ENGINES.into(),
        Err(_) => return Err("QUELLE_SEARCH_ENGINES"),
    };
    let mode = match env::var("QUELLE_SEARCH_MODE") {
        Ok(val) => match val.trim().to_ascii_lowercase().as_str() {
            "fuse" | "parallel" => DiscoveryMode::Fuse,
            "rotate" | "sequential" => DiscoveryMode::Rotate,
            _ => return Err("QUELLE_SEARCH_MODE"),
        },
        Err(env::VarError::NotPresent) => DiscoveryMode::Fuse,
        Err(_) => return Err("QUELLE_SEARCH_MODE"),
    };
    if setting.trim() == "none" {
        return Ok(EnginePool::with_mode(vec![], mode));
    }
    let mut keys = HashSet::from([service_token.to_owned()]);
    if let Ok(key) = env::var("QUELLE_MODEL_API_KEY") {
        keys.insert(key);
    }
    let mut load_key = |name| {
        let value = env_text(name)?;
        if !keys.insert(value.clone()) {
            return Err("service, model and search credentials must be distinct");
        }
        Ok(value)
    };
    let mut names = HashSet::new();
    let mut engines: Vec<Box<dyn SearchEngine>> = vec![];
    for name in setting.split(',').map(str::trim) {
        if !names.insert(name) {
            return Err("duplicate engine in QUELLE_SEARCH_ENGINES");
        }
        let provider = match name {
            "brave" => Provider::Brave {
                endpoint: api_endpoint(
                    "BRAVE_SEARCH_URL",
                    "https://api.search.brave.com/res/v1/web/search",
                )?,
                key: secret_header(load_key("BRAVE_SEARCH_API_KEY")?, "BRAVE_SEARCH_API_KEY")?,
            },
            "bing" => Provider::Html(HtmlEngine::Bing),
            "duckduckgo" => Provider::Html(HtmlEngine::DuckDuckGo),
            "mwmbl" => Provider::Mwmbl {
                key: env::var_os("MWMBL_API_KEY")
                    .filter(|k| !k.is_empty())
                    .map(|_| load_key("MWMBL_API_KEY"))
                    .transpose()?,
            },
            "mojeek" => Provider::Mojeek {
                endpoint: api_endpoint("MOJEEK_SEARCH_URL", "https://api.mojeek.com/search")?,
                key: load_key("MOJEEK_API_KEY")?,
            },
            "wikipedia" => Provider::Wikipedia,
            "stackexchange" | "stackoverflow" => Provider::StackExchange,
            "hackernews" | "hn" => Provider::HackerNews,
            "arxiv" => Provider::Arxiv,
            "github" => Provider::GitHub,
            "searxng" => {
                let raw = env_text("QUELLE_SEARXNG_URL")?;
                let mut ep = sidecar_endpoint(&raw, "QUELLE_SEARXNG_URL")?;
                let path = ep.path().trim_end_matches('/');
                if !path.ends_with("/search") {
                    ep.set_path(&format!("{path}/search"));
                }
                let engines = env::var("QUELLE_SEARXNG_ENGINES")
                    .ok()
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty());
                let categories = env::var("QUELLE_SEARXNG_CATEGORIES")
                    .ok()
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "general".to_owned());
                Provider::Searxng {
                    endpoint: ep,
                    engines,
                    categories,
                }
            }
            _ => return Err("QUELLE_SEARCH_ENGINES"),
        };
        engines.push(Box::new(NativeEngine {
            client: client.clone(),
            provider,
        }));
    }
    Ok(EnginePool::with_mode(engines, mode))
}

impl SearchEngine for NativeEngine {
    fn spacing(&self) -> Duration {
        match &self.provider {
            Provider::Mwmbl { key: Some(_) } => Duration::from_millis(200),
            Provider::Mwmbl { key: None } => Duration::from_secs(1),
            Provider::Html(HtmlEngine::DuckDuckGo) => Duration::from_millis(850),
            Provider::GitHub => Duration::from_millis(1200),
            _ => Duration::ZERO,
        }
    }

    fn weight(&self) -> f64 {
        match &self.provider {
            Provider::Brave { .. } => 1.6,
            Provider::Html(HtmlEngine::DuckDuckGo) => 1.5,
            Provider::Searxng { .. } => 1.5,
            Provider::Mojeek { .. } => 1.2,
            Provider::Html(HtmlEngine::Bing) => 1.0,
            Provider::Mwmbl { .. } => 0.9,
            Provider::Wikipedia => 0.85,
            Provider::StackExchange => 0.75,
            Provider::HackerNews => 0.75,
            Provider::GitHub => 0.65,
            Provider::Arxiv => 0.65,
        }
    }

    fn query_weight(&self, query: &Query) -> f64 {
        match &self.provider {
            Provider::Arxiv if query_has_academic_intent(&query.text) => 1.15,
            Provider::GitHub if query_has_repo_intent(&query.text) => 1.15,
            Provider::StackExchange if query_has_tech_intent(&query.text) => 0.95,
            _ => self.weight(),
        }
    }

    fn supports_query(&self, query: &Query) -> bool {
        match &self.provider {
            Provider::Wikipedia => site_filter_allows(&query.text, &["wikipedia.org"]),
            Provider::StackExchange => {
                site_filter_allows(&query.text, &["stackoverflow.com", "stackexchange.com"])
                    && (query.language == Language::En || query_has_tech_intent(&query.text))
            }
            Provider::HackerNews => {
                site_filter_allows(&query.text, &["news.ycombinator.com"])
                    && (query.language == Language::En || query_has_tech_intent(&query.text))
            }
            Provider::Arxiv => site_filter_allows(&query.text, &["arxiv.org"]),
            Provider::GitHub => site_filter_allows(&query.text, &["github.com"]),
            _ => true,
        }
    }

    fn name(&self) -> &'static str {
        match self.provider {
            Provider::Brave { .. } => "brave",
            Provider::Html(HtmlEngine::Bing) => "bing",
            Provider::Html(HtmlEngine::DuckDuckGo) => "duckduckgo",
            Provider::Mwmbl { .. } => "mwmbl",
            Provider::Mojeek { .. } => "mojeek",
            Provider::Wikipedia => "wikipedia",
            Provider::StackExchange => "stackexchange",
            Provider::HackerNews => "hackernews",
            Provider::Arxiv => "arxiv",
            Provider::GitHub => "github",
            Provider::Searxng { .. } => "searxng",
        }
    }

    fn search<'a>(&'a self, query: &'a Query, options: SearchOptions) -> DiscoveryFuture<'a> {
        Box::pin(self.fetch(query, options))
    }
}

pub(super) fn ddg_locale(language: Language) -> &'static str {
    match language {
        Language::De => "ch-de",
        Language::Fr => "ch-fr",
        Language::It => "ch-it",
        Language::En => "wt-wt",
    }
}

/// Specialized engines (Wikipedia, StackExchange, HackerNews, arXiv, GitHub) do
/// not execute general web `site:<domain>` operators. Skip them whenever the
/// query explicitly targets an unrelated `site:` domain.
pub(super) fn site_filter_allows(query: &str, allowed_domains: &[&str]) -> bool {
    static SITE_OP: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\bsite:([^\s)]+)").expect("site operator regex"));
    let mut saw_site = false;
    for cap in SITE_OP.captures_iter(query) {
        saw_site = true;
        let domain = cap[1].trim_matches('"').to_ascii_lowercase();
        if allowed_domains
            .iter()
            .any(|allowed| domain == *allowed || domain.ends_with(&format!(".{allowed}")))
        {
            return true;
        }
    }
    !saw_site
}

fn accept_language_for(language: Language) -> &'static str {
    match language {
        Language::De => "de-CH,de;q=0.9,en-US;q=0.5,en;q=0.3",
        Language::Fr => "fr-CH,fr;q=0.9,en-US;q=0.5,en;q=0.3",
        Language::It => "it-CH,it;q=0.9,en-US;q=0.5,en;q=0.3",
        Language::En => "en-US,en;q=0.9,de-CH;q=0.4",
    }
}

impl NativeEngine {
    fn request(
        &self,
        q: &str,
        language: Language,
        options: SearchOptions,
    ) -> reqwest::RequestBuilder {
        let lang = match language {
            Language::De => "de",
            Language::Fr => "fr",
            Language::It => "it",
            Language::En => "en",
        };
        let count = options.count.to_string();
        match &self.provider {
            Provider::Brave { endpoint, key } => self
                .client
                .get(endpoint.clone())
                .header(header::USER_AGENT, HONEST_USER_AGENT)
                .header(header::ACCEPT, "application/json")
                .header("X-Subscription-Token", key.clone())
                .header("X-Loc-Country", "CH")
                .header(header::CACHE_CONTROL, "no-cache")
                .query(&[
                    ("q", q),
                    ("country", "CH"),
                    ("search_lang", lang),
                    ("ui_lang", if lang == "fr" { "fr-CH" } else { "de-CH" }),
                    ("count", &count),
                    (
                        "spellcheck",
                        if options.spellcheck { "true" } else { "false" },
                    ),
                    ("safesearch", "strict"),
                    ("result_filter", "web"),
                    ("text_decorations", "false"),
                ]),
            Provider::Html(HtmlEngine::Bing) => self
                .client
                .get("https://www.bing.com/search")
                .header(header::USER_AGENT, CHROME_USER_AGENT)
                .header(
                    header::ACCEPT,
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                )
                .header(header::ACCEPT_LANGUAGE, accept_language_for(language))
                .header(header::REFERER, "https://www.bing.com/")
                .header("Sec-Fetch-Dest", "document")
                .header("Sec-Fetch-Mode", "navigate")
                .header("Sec-Fetch-Site", "same-origin")
                .header("Sec-Fetch-User", "?1")
                .query(&[
                    ("q", q),
                    ("count", &count),
                    ("cc", "ch"),
                    ("setlang", lang),
                    ("adlt", "strict"),
                ]),
            Provider::Html(HtmlEngine::DuckDuckGo) => {
                let kl = ddg_locale(language);
                self.client
                    .post("https://html.duckduckgo.com/html/")
                    .header(header::USER_AGENT, FIREFOX_USER_AGENT)
                    .header(
                        header::ACCEPT,
                        "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                    )
                    .header(header::ACCEPT_LANGUAGE, accept_language_for(language))
                    .header(header::REFERER, "https://html.duckduckgo.com/")
                    .header(header::ORIGIN, "https://html.duckduckgo.com")
                    .header(header::COOKIE, format!("kl={kl}"))
                    .header("Sec-Fetch-Dest", "document")
                    .header("Sec-Fetch-Mode", "navigate")
                    .header("Sec-Fetch-Site", "same-origin")
                    .header("Sec-Fetch-User", "?1")
                    .form(&[("q", q), ("b", ""), ("kl", kl), ("kp", "1")])
            }
            Provider::Mwmbl { key } => {
                let request = self
                    .client
                    .get("https://api.mwmbl.org/api/v2/search/")
                    .header(header::USER_AGENT, HONEST_USER_AGENT)
                    .header(header::ACCEPT, "application/json")
                    .query(&[("q", q)]);
                if let Some(key) = key {
                    request.query(&[("api_key", key)])
                } else {
                    request
                }
            }
            Provider::Mojeek { endpoint, key } => self
                .client
                .get(endpoint.clone())
                .header(header::USER_AGENT, HONEST_USER_AGENT)
                .header(header::ACCEPT, "application/json")
                .query(&[
                    ("q", q),
                    ("api_key", key),
                    ("fmt", "json"),
                    ("t", &count),
                    ("lb", &lang.to_ascii_uppercase()),
                    ("rb", "CH"),
                    ("safe", "1"),
                ]),
            Provider::Wikipedia => self
                .client
                .get(format!("https://{lang}.wikipedia.org/w/api.php"))
                .header(header::USER_AGENT, HONEST_USER_AGENT)
                .header(header::ACCEPT, "application/json")
                .query(&[
                    ("action", "query"),
                    ("list", "search"),
                    ("srsearch", q),
                    ("srlimit", &count),
                    ("srprop", "snippet"),
                    ("format", "json"),
                    ("formatversion", "2"),
                ]),
            Provider::StackExchange => {
                let pagesize = options.count.min(10).to_string();
                self.client
                    .get("https://api.stackexchange.com/2.3/search/advanced")
                    .header(header::USER_AGENT, HONEST_USER_AGENT)
                    .header(header::ACCEPT, "application/json")
                    .query(&[
                        ("order", "desc"),
                        ("sort", "relevance"),
                        ("site", "stackoverflow"),
                        ("pagesize", pagesize.as_str()),
                        ("q", q),
                    ])
            }
            Provider::HackerNews => {
                let hits = options.count.min(10).to_string();
                self.client
                    .get("https://hn.algolia.com/api/v1/search")
                    .header(header::USER_AGENT, HONEST_USER_AGENT)
                    .header(header::ACCEPT, "application/json")
                    .query(&[
                        ("query", q),
                        ("hitsPerPage", hits.as_str()),
                        ("tags", "story"),
                    ])
            }
            Provider::Arxiv => {
                let max_results = options.count.min(10).to_string();
                let search_query = format!("all:{q}");
                self.client
                    .get("https://export.arxiv.org/api/query")
                    .header(header::USER_AGENT, HONEST_USER_AGENT)
                    .header(header::ACCEPT, "application/atom+xml,application/xml")
                    .query(&[
                        ("search_query", search_query.as_str()),
                        ("max_results", max_results.as_str()),
                    ])
            }
            Provider::GitHub => {
                let per_page = options.count.min(10).to_string();
                self.client
                    .get("https://api.github.com/search/repositories")
                    .header(header::USER_AGENT, HONEST_USER_AGENT)
                    .header(header::ACCEPT, "application/vnd.github+json")
                    .query(&[("q", q), ("per_page", per_page.as_str())])
            }
            Provider::Searxng {
                endpoint,
                engines,
                categories,
            } => {
                let mut req = self
                    .client
                    .get(endpoint.clone())
                    .header(header::USER_AGENT, HONEST_USER_AGENT)
                    .header(header::ACCEPT, "application/json")
                    .query(&[
                        ("q", q),
                        ("format", "json"),
                        ("language", lang),
                        ("categories", categories.as_str()),
                    ]);
                if let Some(eng) = engines {
                    req = req.query(&[("engines", eng.as_str())]);
                }
                req
            }
        }
    }

    async fn fetch(
        &self,
        query: &Query,
        options: SearchOptions,
    ) -> Result<Vec<Candidate>, DiscoveryError> {
        let lang = match query.language {
            Language::De => "de",
            Language::Fr => "fr",
            Language::It => "it",
            Language::En => "en",
        };
        let html = matches!(self.provider, Provider::Html(_));
        let response = self
            .request(&query.text, query.language, options)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|_| DiscoveryError::Unavailable)?;
        if response.status() == StatusCode::TOO_MANY_REQUESTS
            || (html
                && matches!(
                    response.status(),
                    StatusCode::ACCEPTED | StatusCode::FORBIDDEN
                ))
            || (matches!(self.provider, Provider::GitHub)
                && response.status() == StatusCode::FORBIDDEN)
        {
            return Err(DiscoveryError::RateLimited(retry_after(&response)));
        }
        if !response.status().is_success() {
            return Err(DiscoveryError::Unavailable);
        }
        let rows = match self.provider {
            Provider::Html(kind) => {
                let bytes = read_body(response, BODY_LIMIT)
                    .await
                    .map_err(|_| DiscoveryError::Unavailable)?;
                return parse_search_html(kind, &String::from_utf8_lossy(&bytes), options.count);
            }
            Provider::Brave { .. } => {
                let data: BraveResponse = decode(response).await?;
                data.web.map(|page| page.results).unwrap_or_default()
            }
            Provider::Mwmbl { .. } => decode::<Results>(response).await?.results,
            Provider::Mojeek { .. } => {
                let page = decode::<MojeekResponse>(response).await?.response;
                if page.status.to_ascii_lowercase().contains("limit") {
                    return Err(DiscoveryError::RateLimited(DEFAULT_COOLDOWN));
                }
                if page.status != "OK" {
                    return Err(DiscoveryError::Unavailable);
                }
                page.results.ok_or(DiscoveryError::Unavailable)?
            }
            Provider::Wikipedia => {
                let page = decode::<WikiResponse>(response).await?.query;
                let base = Url::parse(&format!("https://{lang}.wikipedia.org/wiki/"))
                    .expect("Wikipedia origin");
                page.search
                    .into_iter()
                    .map(|item| {
                        let mut url = base.clone();
                        url.path_segments_mut()
                            .expect("Wikipedia URL")
                            .pop_if_empty()
                            .push(&item.title.replace(' ', "_"));
                        Row {
                            url: url.into(),
                            title: item.title,
                            description: Html::parse_fragment(&item.snippet)
                                .root_element()
                                .text()
                                .collect(),
                        }
                    })
                    .collect()
            }
            Provider::StackExchange => {
                let bytes = read_body(response, BODY_LIMIT)
                    .await
                    .map_err(|_| DiscoveryError::Unavailable)?;
                return parse_stackexchange_json(&bytes, options.count);
            }
            Provider::HackerNews => {
                let bytes = read_body(response, BODY_LIMIT)
                    .await
                    .map_err(|_| DiscoveryError::Unavailable)?;
                return parse_hackernews_json(&bytes, options.count);
            }
            Provider::Arxiv => {
                let bytes = read_body(response, BODY_LIMIT)
                    .await
                    .map_err(|_| DiscoveryError::Unavailable)?;
                return parse_arxiv_xml(&String::from_utf8_lossy(&bytes), options.count);
            }
            Provider::GitHub => {
                let bytes = read_body(response, BODY_LIMIT)
                    .await
                    .map_err(|_| DiscoveryError::Unavailable)?;
                return parse_github_json(&bytes, options.count);
            }
            Provider::Searxng { .. } => {
                let bytes = read_body(response, BODY_LIMIT)
                    .await
                    .map_err(|_| DiscoveryError::Unavailable)?;
                return parse_searxng_json(&bytes, options.count);
            }
        };
        Ok(candidates(rows, options.count))
    }
}

fn retry_after(response: &Response) -> Duration {
    let value = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok());
    value
        .and_then(|s| {
            s.parse::<u64>().ok().map(Duration::from_secs).or_else(|| {
                httpdate::parse_http_date(s)
                    .ok()?
                    .duration_since(SystemTime::now())
                    .ok()
            })
        })
        .unwrap_or(DEFAULT_COOLDOWN)
        .clamp(Duration::from_secs(1), Duration::from_secs(86400))
}

async fn decode<T: serde::de::DeserializeOwned>(response: Response) -> Result<T, DiscoveryError> {
    read_json(response, BODY_LIMIT)
        .await
        .map_err(|_| DiscoveryError::Unavailable)
}

#[derive(Deserialize)]
struct Row {
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default, alias = "desc", alias = "content")]
    description: String,
}
#[derive(Deserialize)]
struct Results {
    results: Vec<Row>,
}
#[derive(Deserialize)]
struct BraveResponse {
    web: Option<Results>,
}
#[derive(Deserialize)]
struct MojeekResponse {
    response: MojeekPage,
}
#[derive(Deserialize)]
struct MojeekPage {
    status: String,
    results: Option<Vec<Row>>,
}
#[derive(Deserialize)]
struct WikiResponse {
    query: WikiPage,
}
#[derive(Deserialize)]
struct WikiPage {
    search: Vec<WikiHit>,
}
#[derive(Deserialize)]
struct WikiHit {
    title: String,
    snippet: String,
}

fn decode_html_text(raw: &str) -> String {
    Html::parse_fragment(raw)
        .root_element()
        .text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn parse_stackexchange_json(
    bytes: &[u8],
    count: usize,
) -> Result<Vec<Candidate>, DiscoveryError> {
    #[derive(Deserialize)]
    struct SeResponse {
        #[serde(default)]
        items: Vec<SeItem>,
    }
    #[derive(Deserialize)]
    struct SeItem {
        link: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        score: i64,
        #[serde(default)]
        answer_count: u64,
    }
    let parsed: SeResponse =
        serde_json::from_slice(bytes).map_err(|_| DiscoveryError::Unavailable)?;
    let rows = parsed.items.into_iter().map(|item| {
        let tags = if item.tags.is_empty() {
            String::new()
        } else {
            format!("Tags: {}. ", item.tags.join(", "))
        };
        Row {
            url: item.link,
            title: decode_html_text(&item.title),
            description: format!(
                "{tags}Stack Overflow ({} answers, score {})",
                item.answer_count, item.score
            ),
        }
    });
    Ok(candidates(rows, count))
}

pub(super) fn parse_hackernews_json(
    bytes: &[u8],
    count: usize,
) -> Result<Vec<Candidate>, DiscoveryError> {
    #[derive(Deserialize)]
    struct HnResponse {
        #[serde(default)]
        hits: Vec<HnHit>,
    }
    #[derive(Deserialize)]
    struct HnHit {
        url: Option<String>,
        #[serde(default)]
        title: String,
        #[serde(default)]
        points: Option<i64>,
        #[serde(default)]
        num_comments: Option<i64>,
    }
    let parsed: HnResponse =
        serde_json::from_slice(bytes).map_err(|_| DiscoveryError::Unavailable)?;
    let rows = parsed.hits.into_iter().filter_map(|hit| {
        let url = hit.url.filter(|u| !u.trim().is_empty())?;
        let points = hit.points.unwrap_or(0);
        let comments = hit.num_comments.unwrap_or(0);
        Some(Row {
            url,
            title: decode_html_text(&hit.title),
            description: format!("Hacker News discussion ({points} points, {comments} comments)"),
        })
    });
    Ok(candidates(rows, count))
}

pub(super) fn parse_arxiv_xml(xml: &str, count: usize) -> Result<Vec<Candidate>, DiscoveryError> {
    static ENTRY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<entry\b[^>]*>(.*?)</entry>").unwrap());
    static ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<id>(.*?)</id>").unwrap());
    static TITLE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<title\b[^>]*>(.*?)</title>").unwrap());
    static SUMMARY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<summary\b[^>]*>(.*?)</summary>").unwrap());

    let rows = ENTRY.captures_iter(xml).filter_map(|cap| {
        let block = &cap[1];
        let id_cap = ID.captures(block)?;
        let raw_id = id_cap[1].trim();
        let url = raw_id
            .strip_prefix("http://arxiv.org/")
            .map(|rest| format!("https://arxiv.org/{rest}"))
            .unwrap_or_else(|| raw_id.to_owned());
        let title = TITLE
            .captures(block)
            .map(|c| decode_html_text(&c[1]))
            .unwrap_or_default();
        let description = SUMMARY
            .captures(block)
            .map(|c| decode_html_text(&c[1]))
            .unwrap_or_default();
        Some(Row {
            url,
            title,
            description,
        })
    });
    Ok(candidates(rows, count))
}

pub(super) fn parse_github_json(
    bytes: &[u8],
    count: usize,
) -> Result<Vec<Candidate>, DiscoveryError> {
    #[derive(Deserialize)]
    struct GhResponse {
        #[serde(default)]
        items: Vec<GhRepo>,
    }
    #[derive(Deserialize)]
    struct GhRepo {
        html_url: String,
        #[serde(default)]
        full_name: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        stargazers_count: u64,
    }
    let parsed: GhResponse =
        serde_json::from_slice(bytes).map_err(|_| DiscoveryError::Unavailable)?;
    let rows = parsed.items.into_iter().map(|repo| {
        let desc = repo.description.unwrap_or_default();
        let description = if desc.trim().is_empty() {
            format!("GitHub repository ({} stars)", repo.stargazers_count)
        } else {
            format!("{} ({} stars)", desc.trim(), repo.stargazers_count)
        };
        Row {
            url: repo.html_url,
            title: repo.full_name,
            description,
        }
    });
    Ok(candidates(rows, count))
}

pub(super) fn parse_searxng_json(
    bytes: &[u8],
    count: usize,
) -> Result<Vec<Candidate>, DiscoveryError> {
    let parsed: Results = serde_json::from_slice(bytes).map_err(|_| DiscoveryError::Unavailable)?;
    Ok(candidates(parsed.results, count))
}

pub(super) fn query_has_tech_intent(query: &str) -> bool {
    static TECH_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"(?i)\b(rust|python|javascript|typescript|golang|java|kotlin|swift|cpp|c\+\+|sql|sqlite|postgres|postgresql|mysql|redis|mongodb|",
            r"docker|kubernetes|k8s|linux|unix|bash|git|github|http|https|quic|tls|ssl|dns|tcp|udp|grpc|graphql|rest|api|sdk|cli|json|yaml|html|css|wasm|",
            r"react|vue|nuxt|next\.?js|node\.?js|tokio|async|mutex|regex|rfc|wal|mvcc|aria|oauth|jwt|csrf|ssrf|cve|compiler|error|exception|stacktrace)\b"
        ))
        .expect("tech intent regex")
    });
    TECH_RE.is_match(query) || query.contains("::") || query.contains("--") || query.contains('_')
}

pub(super) fn query_has_academic_intent(query: &str) -> bool {
    static ACAD_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(arxiv|paper|preprint|doi|theorem|proof|citation|et al|benchmark|transformer|attention|neural|diffusion|llm|empirical|dataset|ablation)\b")
            .expect("academic intent regex")
    });
    ACAD_RE.is_match(query)
}

pub(super) fn query_has_repo_intent(query: &str) -> bool {
    static REPO_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(github|repo|repository|crate|cargo|npm|pypi|pip|package|library|sdk|cli|open[- ]source|release|changelog)\b")
            .expect("repo intent regex")
    });
    REPO_RE.is_match(query)
}

fn candidates(rows: impl IntoIterator<Item = Row>, count: usize) -> Vec<Candidate> {
    let mut seen = HashSet::new();
    rows.into_iter()
        .filter_map(|row| {
            let url = public_url(Url::parse(&row.url).ok()?).ok()?;
            seen.insert(url.clone()).then(|| Candidate {
                url,
                title: truncate(row.title.trim(), 200),
                description: truncate(row.description.trim(), 400),
            })
        })
        .take(count)
        .collect()
}

pub(super) fn parse_search_html(
    kind: HtmlEngine,
    html: &str,
    count: usize,
) -> Result<Vec<Candidate>, DiscoveryError> {
    let (row, link, snippet, empty, base) = match kind {
        HtmlEngine::Bing => (
            "li.b_algo",
            "h2 a[href]",
            ".b_caption p",
            "li.b_no",
            "https://www.bing.com/",
        ),
        HtmlEngine::DuckDuckGo => (
            ".result:not(.result--ad)",
            ".result__a[href]",
            ".result__snippet",
            ".no-results, .result--no-result",
            "https://html.duckduckgo.com/",
        ),
    };
    let doc = Html::parse_document(html);
    let selector = |s| Selector::parse(s).expect("search result selector");
    if doc
        .select(&selector(
            "#b_captcha, #challenge-form, .anomaly-modal, #anomaly-modal",
        ))
        .next()
        .is_some()
    {
        return Err(DiscoveryError::RateLimited(DEFAULT_COOLDOWN));
    }
    let (row, link, snippet) = (selector(row), selector(link), selector(snippet));
    let base = Url::parse(base).expect("search origin");
    let rows = doc.select(&row).filter_map(|item| {
        let anchor = item.select(&link).next()?;
        let href = base.join(anchor.attr("href")?).ok()?;
        let url = result_destination(kind, href)?;
        Some(Row {
            url: url.into(),
            title: anchor.text().collect(),
            description: item
                .select(&snippet)
                .next()
                .map(|s| s.text().collect())
                .unwrap_or_default(),
        })
    });
    let hits = candidates(rows, count);
    if hits.is_empty() && doc.select(&selector(empty)).next().is_none() {
        return Err(DiscoveryError::Unavailable);
    }
    Ok(hits)
}

fn result_destination(kind: HtmlEngine, url: Url) -> Option<Url> {
    let host = url.host_str()?;
    match kind {
        HtmlEngine::Bing if host == "bing.com" || host.ends_with(".bing.com") => {
            if url.path() != "/ck/a" {
                return None;
            }
            let (_, value) = url.query_pairs().find(|(k, _)| k == "u")?;
            let bytes = URL_SAFE_NO_PAD
                .decode(value.strip_prefix("a1")?.trim_end_matches('='))
                .ok()?;
            Url::parse(std::str::from_utf8(&bytes).ok()?).ok()
        }
        HtmlEngine::DuckDuckGo if host == "duckduckgo.com" || host.ends_with(".duckduckgo.com") => {
            if url.path().trim_end_matches('/') != "/l" {
                return None;
            }
            let (_, target) = url.query_pairs().find(|(k, _)| k == "uddg")?;
            Url::parse(&target).ok()
        }
        _ => Some(url),
    }
}
