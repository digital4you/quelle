use std::{
    collections::{HashMap, HashSet},
    env,
    sync::{LazyLock, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::stream::{FuturesUnordered, StreamExt};
use regex::Regex;
use reqwest::{Client, Response, StatusCode, header};
use scraper::{Html, Selector};
use serde::Deserialize;
use url::Url;

use super::{
    Candidate, DiscoveryError, DiscoveryFuture, DiscoveryMode, EnginePool, Language, Query,
    SearchEngine, SearchOptions, router,
};
use crate::{
    endpoint, env_text, pages::public_url, read_body, read_json, secret_header, sidecar_endpoint,
    truncate,
};

const BODY_LIMIT: usize = 2 * 1024 * 1024;
const RSS_BODY_LIMIT: usize = 512 * 1024;
const RSS_FEED_TTL: Duration = Duration::from_secs(5 * 60);
const HONEST_USER_AGENT: &str = "Quelle/0.1 (+https://github.com/digital4you/quelle)";
const FIREFOX_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:138.0) Gecko/20100101 Firefox/138.0";
const CHROME_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36";
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(60);
/// Default keyless meta-search engine mix fused via parallel RRF:
/// 6 independent general-web engines (duckduckgo, yahoo_jp, seznam, marginalia, naver, mwmbl)
/// plus 9 intent-routed official JSON/RSS APIs (wikipedia, mdn, ietf, crossref, pubmed, gdelt,
/// rss_news, stackexchange, hackernews).
const DEFAULT_KEYLESS_ENGINES: &str = "duckduckgo,yahoo_jp,seznam,marginalia,naver,mwmbl,wikipedia,mdn,ietf,crossref,pubmed,gdelt,rss_news,stackexchange,hackernews";

#[derive(Clone, Copy)]
pub(super) enum HtmlEngine {
    Bing,
    DuckDuckGo,
    YahooJp,
    Seznam,
    Marginalia,
    Naver,
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
    Mdn,
    Ietf,
    Crossref,
    PubMed,
    Gdelt,
    RssNews,
    OpenAlex,
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
            "yahoo_jp" | "yahoojp" => Provider::Html(HtmlEngine::YahooJp),
            "seznam" => Provider::Html(HtmlEngine::Seznam),
            "marginalia" => Provider::Html(HtmlEngine::Marginalia),
            "naver" => Provider::Html(HtmlEngine::Naver),
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
            "mdn" => Provider::Mdn,
            "ietf" | "rfc" => Provider::Ietf,
            "crossref" => Provider::Crossref,
            "pubmed" | "nih" => Provider::PubMed,
            "gdelt" => Provider::Gdelt,
            "rss_news" | "news" => Provider::RssNews,
            "openalex" => Provider::OpenAlex,
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
            Provider::Html(HtmlEngine::Marginalia) => Duration::from_millis(900),
            Provider::Html(HtmlEngine::YahooJp | HtmlEngine::Seznam | HtmlEngine::Naver) => {
                Duration::from_millis(700)
            }
            Provider::PubMed => Duration::from_millis(400),
            Provider::GitHub => Duration::from_millis(1200),
            _ => Duration::ZERO,
        }
    }

    fn weight(&self) -> f64 {
        match &self.provider {
            Provider::Brave { .. } => 1.6,
            Provider::Html(HtmlEngine::YahooJp) => 1.55,
            Provider::Html(HtmlEngine::DuckDuckGo) => 1.5,
            Provider::Searxng { .. } => 1.5,
            Provider::Html(HtmlEngine::Seznam) => 1.35,
            Provider::Mojeek { .. } => 1.2,
            Provider::Html(HtmlEngine::Bing) => 1.0,
            Provider::Html(HtmlEngine::Marginalia) => 0.95,
            Provider::Html(HtmlEngine::Naver) => 0.95,
            Provider::Mwmbl { .. } => 0.9,
            Provider::Mdn => 0.9,
            Provider::Ietf => 0.9,
            Provider::Wikipedia => 0.85,
            Provider::PubMed => 0.85,
            Provider::Gdelt => 0.85,
            Provider::RssNews => 0.9,
            Provider::Crossref => 0.8,
            Provider::StackExchange => 0.75,
            Provider::HackerNews => 0.75,
            Provider::OpenAlex => 0.7,
            Provider::GitHub => 0.65,
            Provider::Arxiv => 0.65,
        }
    }

    fn query_weight(&self, query: &Query) -> f64 {
        match &self.provider {
            Provider::Ietf if query_has_ietf_intent(&query.text) => 1.45,
            Provider::Mdn if query_has_web_intent(&query.text) => 1.35,
            Provider::PubMed if query_has_biomed_intent(&query.text) => 1.3,
            Provider::Gdelt if query_has_news_intent(&query.text) => 1.15,
            Provider::RssNews if query_has_news_intent(&query.text) => 1.25,
            Provider::Arxiv | Provider::OpenAlex | Provider::Crossref
                if query_has_academic_intent(&query.text) =>
            {
                1.15
            }
            Provider::GitHub if query_has_repo_intent(&query.text) => 1.15,
            Provider::StackExchange if query_has_tech_intent(&query.text) => 0.95,
            _ => self.weight(),
        }
    }

    fn supports_query(&self, query: &Query) -> bool {
        match &self.provider {
            Provider::Wikipedia => site_filter_allows(&query.text, &["wikipedia.org"]),
            Provider::Mdn => {
                site_filter_allows(&query.text, &["developer.mozilla.org", "mozilla.org"])
                    && query_has_web_intent(&query.text)
            }
            Provider::Ietf => {
                site_filter_allows(&query.text, &["rfc-editor.org", "ietf.org"])
                    && query_has_ietf_intent(&query.text)
            }
            Provider::Crossref => {
                site_filter_allows(&query.text, &["doi.org", "crossref.org"])
                    && query_has_academic_intent(&query.text)
            }
            Provider::PubMed => {
                site_filter_allows(&query.text, &["pubmed.ncbi.nlm.nih.gov", "nih.gov"])
                    && query_has_biomed_intent(&query.text)
            }
            Provider::Gdelt | Provider::RssNews => {
                query_has_news_intent(&query.text) && !query.text.contains("site:")
            }
            Provider::OpenAlex => {
                site_filter_allows(&query.text, &["openalex.org", "doi.org"])
                    && query_has_academic_intent(&query.text)
            }
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
            Provider::Html(HtmlEngine::YahooJp) => "yahoo_jp",
            Provider::Html(HtmlEngine::Seznam) => "seznam",
            Provider::Html(HtmlEngine::Marginalia) => "marginalia",
            Provider::Html(HtmlEngine::Naver) => "naver",
            Provider::Mwmbl { .. } => "mwmbl",
            Provider::Mojeek { .. } => "mojeek",
            Provider::Wikipedia => "wikipedia",
            Provider::Mdn => "mdn",
            Provider::Ietf => "ietf",
            Provider::Crossref => "crossref",
            Provider::PubMed => "pubmed",
            Provider::Gdelt => "gdelt",
            Provider::RssNews => "rss_news",
            Provider::OpenAlex => "openalex",
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
            Provider::Html(HtmlEngine::YahooJp) => self
                .client
                .get("https://search.yahoo.co.jp/search")
                .header(header::USER_AGENT, FIREFOX_USER_AGENT)
                .header(
                    header::ACCEPT,
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                )
                .header(header::ACCEPT_LANGUAGE, accept_language_for(language))
                .query(&[("p", q), ("ei", "UTF-8")]),
            Provider::Html(HtmlEngine::Seznam) => self
                .client
                .get("https://search.seznam.cz/")
                .header(header::USER_AGENT, FIREFOX_USER_AGENT)
                .header(
                    header::ACCEPT,
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                )
                .header(header::ACCEPT_LANGUAGE, accept_language_for(language))
                .query(&[("q", q)]),
            Provider::Html(HtmlEngine::Marginalia) => self
                .client
                .get("https://search.marginalia.nu/search")
                .header(header::USER_AGENT, FIREFOX_USER_AGENT)
                .header(
                    header::ACCEPT,
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                )
                .header(header::ACCEPT_LANGUAGE, accept_language_for(language))
                .query(&[("query", q)]),
            Provider::Html(HtmlEngine::Naver) => self
                .client
                .get("https://search.naver.com/search.naver")
                .header(header::USER_AGENT, FIREFOX_USER_AGENT)
                .header(
                    header::ACCEPT,
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                )
                .header(header::ACCEPT_LANGUAGE, accept_language_for(language))
                .query(&[("where", "webkr"), ("query", q)]),
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
            Provider::Mdn => self
                .client
                .get("https://developer.mozilla.org/api/v1/search")
                .header(header::USER_AGENT, HONEST_USER_AGENT)
                .header(header::ACCEPT, "application/json")
                .query(&[("q", q), ("locale", "en-US")]),
            Provider::Ietf => {
                let limit = options.count.min(8).to_string();
                let req = self
                    .client
                    .get("https://datatracker.ietf.org/api/v1/doc/document/")
                    .header(header::USER_AGENT, HONEST_USER_AGENT)
                    .header(header::ACCEPT, "application/json")
                    .query(&[
                        ("format", "json"),
                        ("type", "rfc"),
                        ("limit", limit.as_str()),
                    ]);
                if let Some(rfc_name) = extract_rfc_number(q) {
                    req.query(&[("name__icontains", rfc_name.as_str())])
                } else {
                    let kw = ietf_search_term(q);
                    req.query(&[("title__icontains", kw.as_str())])
                }
            }
            Provider::Crossref => {
                let rows = options.count.min(10).to_string();
                self.client
                    .get("https://api.crossref.org/works")
                    .header(header::USER_AGENT, HONEST_USER_AGENT)
                    .header(header::ACCEPT, "application/json")
                    .query(&[
                        ("query", q),
                        ("rows", rows.as_str()),
                        ("select", "DOI,title,URL,published,container-title"),
                    ])
            }
            Provider::Gdelt => {
                let max = options.count.min(10).to_string();
                self.client
                    .get("https://api.gdeltproject.org/api/v2/doc/doc")
                    .header(header::USER_AGENT, HONEST_USER_AGENT)
                    .header(header::ACCEPT, "application/json")
                    .query(&[
                        ("query", q),
                        ("mode", "ArtList"),
                        ("maxrecords", max.as_str()),
                        ("format", "json"),
                    ])
            }
            Provider::RssNews => {
                // Handled directly in `fetch_rss_news` via multi-feed cache.
                self.client
                    .get("https://feeds.bbci.co.uk/news/world/rss.xml")
            }
            Provider::PubMed => {
                let retmax = options.count.min(10).to_string();
                self.client
                    .get("https://eutils.ncbi.nlm.nih.gov/entrez/eutils/esearch.fcgi")
                    .header(header::USER_AGENT, HONEST_USER_AGENT)
                    .header(header::ACCEPT, "application/json")
                    .query(&[
                        ("db", "pubmed"),
                        ("retmode", "json"),
                        ("retmax", retmax.as_str()),
                        ("term", q),
                    ])
            }
            Provider::OpenAlex => {
                let per_page = options.count.min(10).to_string();
                self.client
                    .get("https://api.openalex.org/works")
                    .header(header::USER_AGENT, HONEST_USER_AGENT)
                    .header(header::ACCEPT, "application/json")
                    .query(&[("search", q), ("per-page", per_page.as_str())])
            }
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
        if matches!(self.provider, Provider::RssNews) {
            return self.fetch_rss_news(query, options.count).await;
        }
        let lang = match query.language {
            Language::De => "de",
            Language::Fr => "fr",
            Language::It => "it",
            Language::En => "en",
        };
        let effective_query = match self.provider {
            Provider::PubMed | Provider::Mdn | Provider::Ietf | Provider::StackExchange => {
                translate_specialist_query(&query.text, query.language)
            }
            _ => query.text.clone(),
        };
        let html = matches!(self.provider, Provider::Html(_));
        let response = self
            .request(&effective_query, query.language, options)
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
            Provider::Mdn => {
                let bytes = read_body(response, BODY_LIMIT)
                    .await
                    .map_err(|_| DiscoveryError::Unavailable)?;
                return parse_mdn_json(&bytes, options.count);
            }
            Provider::Ietf => {
                let bytes = read_body(response, BODY_LIMIT)
                    .await
                    .map_err(|_| DiscoveryError::Unavailable)?;
                return parse_ietf_json(&bytes, options.count);
            }
            Provider::Crossref => {
                let bytes = read_body(response, BODY_LIMIT)
                    .await
                    .map_err(|_| DiscoveryError::Unavailable)?;
                return parse_crossref_json(&bytes, options.count);
            }
            Provider::Gdelt => {
                let bytes = read_body(response, BODY_LIMIT)
                    .await
                    .map_err(|_| DiscoveryError::Unavailable)?;
                return parse_gdelt_json(&bytes, options.count);
            }
            Provider::RssNews => unreachable!("handled in fetch_rss_news"),
            Provider::PubMed => {
                let bytes = read_body(response, BODY_LIMIT)
                    .await
                    .map_err(|_| DiscoveryError::Unavailable)?;
                let ids = parse_pubmed_esearch_json(&bytes, options.count)?;
                if ids.is_empty() {
                    return Ok(vec![]);
                }
                let id_list = ids.join(",");
                let sum_resp = self
                    .client
                    .get("https://eutils.ncbi.nlm.nih.gov/entrez/eutils/esummary.fcgi")
                    .header(header::USER_AGENT, HONEST_USER_AGENT)
                    .header(header::ACCEPT, "application/json")
                    .query(&[
                        ("db", "pubmed"),
                        ("retmode", "json"),
                        ("id", id_list.as_str()),
                    ])
                    .timeout(Duration::from_secs(5))
                    .send()
                    .await
                    .map_err(|_| DiscoveryError::Unavailable)?;
                if sum_resp.status() == StatusCode::TOO_MANY_REQUESTS {
                    return Err(DiscoveryError::RateLimited(retry_after(&sum_resp)));
                }
                if !sum_resp.status().is_success() {
                    return Err(DiscoveryError::Unavailable);
                }
                let sum_bytes = read_body(sum_resp, BODY_LIMIT)
                    .await
                    .map_err(|_| DiscoveryError::Unavailable)?;
                return parse_pubmed_esummary_json(&sum_bytes, options.count);
            }
            Provider::OpenAlex => {
                let bytes = read_body(response, BODY_LIMIT)
                    .await
                    .map_err(|_| DiscoveryError::Unavailable)?;
                return parse_openalex_json(&bytes, options.count);
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

    async fn fetch_rss_news(
        &self,
        query: &Query,
        count: usize,
    ) -> Result<Vec<Candidate>, DiscoveryError> {
        type FeedCacheMap = HashMap<&'static str, (Instant, Vec<FeedItem>)>;
        static FEED_CACHE: OnceLock<Mutex<FeedCacheMap>> = OnceLock::new();
        let cache = FEED_CACHE.get_or_init(|| Mutex::new(HashMap::new()));

        let feeds = curated_news_feeds(query.language);
        let mut pooled: Vec<FeedItem> = Vec::new();
        let mut to_fetch: Vec<(&'static str, &'static str)> = Vec::new();
        {
            if let Ok(guard) = cache.lock() {
                for &(source, url) in feeds {
                    if let Some((stored, items)) = guard.get(url)
                        && stored.elapsed() < RSS_FEED_TTL
                    {
                        pooled.extend(items.iter().cloned());
                    } else {
                        to_fetch.push((source, url));
                    }
                }
            }
        }

        if !to_fetch.is_empty() {
            let mut tasks = FuturesUnordered::new();
            for (source, url) in to_fetch {
                let client = self.client.clone();
                tasks.push(async move {
                    let resp = client
                        .get(url)
                        .header(header::USER_AGENT, HONEST_USER_AGENT)
                        .header(
                            header::ACCEPT,
                            "application/rss+xml, application/atom+xml, application/xml, text/xml",
                        )
                        .timeout(Duration::from_millis(3500))
                        .send()
                        .await
                        .ok()?;
                    if !resp.status().is_success() {
                        return None;
                    }
                    let bytes = read_body(resp, RSS_BODY_LIMIT).await.ok()?;
                    let xml = String::from_utf8_lossy(&bytes);
                    let parsed = parse_rss_or_atom(&xml, source);
                    (!parsed.is_empty()).then_some((url, parsed))
                });
            }
            while let Some(res) = tasks.next().await {
                if let Some((url, items)) = res {
                    if let Ok(mut guard) = cache.lock() {
                        guard.insert(url, (Instant::now(), items.clone()));
                    }
                    pooled.extend(items);
                }
            }
        }

        if pooled.is_empty() {
            return Err(DiscoveryError::Unavailable);
        }
        Ok(rank_feed_items(&query.text, &pooled, count))
    }
}

#[derive(Clone, Debug)]
pub(super) struct FeedItem {
    pub url: String,
    pub title: String,
    pub description: String,
    pub source: String,
    pub published: String,
}

fn curated_news_feeds(language: Language) -> &'static [(&'static str, &'static str)] {
    match language {
        Language::De => &[
            ("SRF News", "https://www.srf.ch/news/bnf/rss/1646"),
            (
                "Tagesschau",
                "https://www.tagesschau.de/infoservices/alle-meldungen-100~rss2.xml",
            ),
            ("Heise", "https://www.heise.de/rss/heise-atom.xml"),
            ("BBC World", "https://feeds.bbci.co.uk/news/world/rss.xml"),
        ],
        Language::Fr => &[
            ("France 24", "https://www.france24.com/fr/rss"),
            ("Le Monde", "https://www.lemonde.fr/rss/une.xml"),
            ("BBC World", "https://feeds.bbci.co.uk/news/world/rss.xml"),
        ],
        Language::It => &[
            ("ANSA", "https://www.ansa.it/sito/ansait_rss.xml"),
            (
                "la Repubblica",
                "https://www.repubblica.it/rss/homepage/rss2.0.xml",
            ),
            ("RaiNews", "https://www.rainews.it/rss/tutti"),
            ("BBC World", "https://feeds.bbci.co.uk/news/world/rss.xml"),
        ],
        Language::En => &[
            ("BBC World", "https://feeds.bbci.co.uk/news/world/rss.xml"),
            ("NPR News", "https://feeds.npr.org/1001/rss.xml"),
            ("The Guardian", "https://www.theguardian.com/world/rss"),
            (
                "Ars Technica",
                "https://feeds.arstechnica.com/arstechnica/index",
            ),
            ("LWN", "https://lwn.net/headlines/rss"),
        ],
    }
}

fn strip_cdata(raw: &str) -> &str {
    let trimmed = raw.trim();
    trimmed
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
        .unwrap_or(trimmed)
}

pub(super) fn parse_rss_or_atom(xml: &str, source_name: &str) -> Vec<FeedItem> {
    static RSS_ITEM: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<item\b[^>]*>(.*?)</item>").unwrap());
    static ATOM_ENTRY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<entry\b[^>]*>(.*?)</entry>").unwrap());
    static TITLE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<title\b[^>]*>(.*?)</title>").unwrap());
    static LINK_TEXT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<link\b[^>]*>(.*?)</link>").unwrap());
    static LINK_HREF: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?is)<link\b[^>]*\bhref=["']([^"']+)["']"#).unwrap());
    static DESC: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?is)<(?:description|summary|content)\b[^>]*>(.*?)</(?:description|summary|content)>",
        )
        .unwrap()
    });
    static PUBDATE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?is)<(?:pubDate|updated|published)\b[^>]*>(.*?)</(?:pubDate|updated|published)>",
        )
        .unwrap()
    });

    let extract_block = |block: &str| -> Option<FeedItem> {
        let url = LINK_TEXT
            .captures(block)
            .map(|c| strip_cdata(&c[1]).trim().to_owned())
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
            .or_else(|| {
                LINK_HREF
                    .captures(block)
                    .map(|c| c[1].trim().to_owned())
                    .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
            })?;
        let title = TITLE
            .captures(block)
            .map(|c| decode_html_text(strip_cdata(&c[1])))
            .unwrap_or_default();
        if title.is_empty() {
            return None;
        }
        let desc_raw = DESC
            .captures(block)
            .map(|c| decode_html_text(strip_cdata(&c[1])))
            .unwrap_or_default();
        let published = PUBDATE
            .captures(block)
            .map(|c| strip_cdata(&c[1]).trim().to_owned())
            .unwrap_or_default();
        Some(FeedItem {
            url,
            title,
            description: truncate(&desc_raw, 280),
            source: source_name.to_owned(),
            published,
        })
    };

    let mut out: Vec<FeedItem> = RSS_ITEM
        .captures_iter(xml)
        .filter_map(|cap| extract_block(&cap[1]))
        .collect();
    if out.is_empty() {
        out = ATOM_ENTRY
            .captures_iter(xml)
            .filter_map(|cap| extract_block(&cap[1]))
            .collect();
    }
    out
}

pub(super) fn rank_feed_items(query: &str, items: &[FeedItem], count: usize) -> Vec<Candidate> {
    if items.is_empty() || count == 0 {
        return Vec::new();
    }
    let mut seen_terms = HashSet::new();
    let query_terms: Vec<String> = crate::selection::bm25_tokens(query)
        .into_iter()
        .filter(|t| {
            !matches!(
                t.as_str(),
                "latest"
                    | "news"
                    | "today"
                    | "breaking"
                    | "aktuell"
                    | "nachrichten"
                    | "actualités"
                    | "notizie"
                    | "2025"
                    | "2026"
            )
        })
        .filter(|t| seen_terms.insert(t.clone()))
        .take(32)
        .collect();

    let field_docs: Vec<Vec<(&str, f64)>> = items
        .iter()
        .map(|it| vec![(it.title.as_str(), 3.0), (it.description.as_str(), 1.0)])
        .collect();
    let bm25 = crate::selection::bm25f_scores(&query_terms, &field_docs);

    let mut scored: Vec<(f64, usize, &FeedItem)> = items
        .iter()
        .enumerate()
        .zip(bm25)
        .filter_map(|((idx, item), bm)| {
            let trigram = crate::selection::char_trigram_cosine(
                query,
                &format!("{} {}", item.title, item.description),
            );
            // Require genuine topical relevance so unrelated breaking headlines
            // are never injected into specific news queries.
            if bm <= 0.0 && trigram < 0.16 {
                return None;
            }
            Some((bm + trigram * 2.0, idx, item))
        })
        .collect();

    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let rows = scored.into_iter().map(|(_, _, item)| {
        let mut meta = vec![item.source.clone()];
        if !item.published.is_empty() {
            meta.push(item.published.clone());
        }
        let description = if item.description.is_empty() {
            meta.join(" · ")
        } else {
            format!("{} — {}", meta.join(" · "), item.description)
        };
        Row {
            url: item.url.clone(),
            title: item.title.clone(),
            description,
        }
    });
    candidates(rows, count)
}

/// Translates common German, French, and Italian biomedical and technical terms
/// into English equivalents for English-only specialist APIs (`pubmed`, `mdn`,
/// `ietf`, `stackexchange`), preserving drug names, RFC numbers, and proper nouns.
pub(super) fn translate_specialist_query(query: &str, _language: Language) -> String {
    // Multi-word phrases first, then single-word stems (all lowercase keys).
    const PHRASES: &[(&str, &str)] = &[
        ("klinische studie", "clinical trial"),
        ("klinischen studie", "clinical trial"),
        ("étude clinique", "clinical trial"),
        ("essai clinique", "clinical trial"),
        ("studio clinico", "clinical trial"),
        ("effets secondaires", "side effects"),
        ("effet secondaire", "side effect"),
        ("effetti collaterali", "side effects"),
        ("effetto collaterale", "side effect"),
        ("hypertension artérielle", "hypertension"),
        ("pressione alta", "hypertension"),
        ("interactions médicamenteuses", "drug interactions"),
        ("base de données", "database"),
        ("base di dati", "database"),
    ];
    const WORDS: &[(&str, &str)] = &[
        // German biomedical
        ("nebenwirkungen", "side effects"),
        ("nebenwirkung", "side effect"),
        ("dosierung", "dosage"),
        ("überdosierung", "overdose"),
        ("wechselwirkungen", "drug interactions"),
        ("leber", "liver"),
        ("leberschaden", "liver damage"),
        ("lebertoxizität", "hepatotoxicity"),
        ("niere", "kidney"),
        ("nieren", "kidney"),
        ("bluthochdruck", "hypertension"),
        ("blutdruck", "blood pressure"),
        ("impfstoff", "vaccine"),
        ("impfung", "vaccination"),
        ("wirksamkeit", "efficacy"),
        ("krebs", "cancer"),
        ("behandlung", "treatment"),
        ("therapie", "therapy"),
        ("studie", "clinical study"),
        ("schmerzmittel", "analgesic"),
        ("fieber", "fever"),
        ("entzündung", "inflammation"),
        ("infektion", "infection"),
        ("antibiotika", "antibiotics"),
        ("antibiotikum", "antibiotic"),
        ("herzinfarkt", "myocardial infarction"),
        ("schlaganfall", "stroke"),
        ("schwangerschaft", "pregnancy"),
        ("stillzeit", "lactation"),
        ("symptome", "symptoms"),
        ("diagnose", "diagnosis"),
        ("vorbeugung", "prevention"),
        ("blutzucker", "blood glucose"),
        ("stoffwechsel", "metabolism"),
        ("immunsystem", "immune system"),
        ("kopfschmerzen", "headache"),
        ("übelkeit", "nausea"),
        ("schwindel", "dizziness"),
        ("allergie", "allergy"),
        ("magen", "gastric"),
        ("darm", "intestinal"),
        ("lunge", "lung"),
        ("gehirn", "brain"),
        ("erkrankung", "disease"),
        ("krankheit", "disease"),
        ("medikament", "drug"),
        ("wirkstoff", "active ingredient"),
        // French biomedical
        ("posologie", "dosage"),
        ("surdosage", "overdose"),
        ("foie", "liver"),
        ("rein", "kidney"),
        ("vaccin", "vaccine"),
        ("efficacité", "efficacy"),
        ("traitement", "treatment"),
        ("douleur", "pain"),
        ("fièvre", "fever"),
        ("grossesse", "pregnancy"),
        ("maladie", "disease"),
        ("médicament", "drug"),
        ("symptômes", "symptoms"),
        ("prévention", "prevention"),
        ("glycémie", "blood glucose"),
        // Italian biomedical
        ("dosaggio", "dosage"),
        ("sovradosaggio", "overdose"),
        ("fegato", "liver"),
        ("rene", "kidney"),
        ("vaccino", "vaccine"),
        ("efficacia", "efficacy"),
        ("trattamento", "treatment"),
        ("dolore", "pain"),
        ("febbre", "fever"),
        ("gravidanza", "pregnancy"),
        ("malattia", "disease"),
        ("farmaco", "drug"),
        ("sintomi", "symptoms"),
        ("prevenzione", "prevention"),
        // DE/FR/IT web & tech
        ("barrierefreiheit", "accessibility"),
        ("speicher", "memory"),
        ("datenbank", "database"),
        ("verschlüsselung", "encryption"),
        ("authentifizierung", "authentication"),
        ("schnittstelle", "interface"),
        ("fehlerbehandlung", "error handling"),
        ("abfrage", "query"),
        ("netzwerk", "network"),
        ("protokoll", "protocol"),
        ("sicherheit", "security"),
        ("accessibilité", "accessibility"),
        ("chiffrement", "encryption"),
        ("mémoire", "memory"),
        ("accessibilità", "accessibility"),
        ("crittografia", "encryption"),
    ];
    const STOPWORDS: &[&str] = &[
        "und", "oder", "bei", "für", "mit", "über", "von", "der", "die", "das", "den", "dem",
        "des", "ein", "eine", "les", "pour", "avec", "dans", "sur", "une", "chez", "degli",
        "della", "delle", "con", "per", "nel", "nella",
    ];

    let mut working = query.to_lowercase();
    let mut translated_any = false;
    for &(src, dst) in PHRASES {
        if working.contains(src) {
            working = working.replace(src, dst);
            translated_any = true;
        }
    }

    let mut out = Vec::new();
    for raw_tok in working.split_whitespace() {
        let clean = raw_tok.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '/');
        if clean.is_empty() {
            continue;
        }
        if let Some(&(_, dst)) = WORDS.iter().find(|&&(src, _)| src == clean) {
            out.push(dst.to_owned());
            translated_any = true;
        } else if translated_any && STOPWORDS.contains(&clean) {
            continue;
        } else {
            out.push(raw_tok.to_owned());
        }
    }

    if translated_any {
        out.join(" ")
    } else {
        query.to_owned()
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

pub(super) fn parse_mdn_json(bytes: &[u8], count: usize) -> Result<Vec<Candidate>, DiscoveryError> {
    #[derive(Deserialize)]
    struct MdnResponse {
        #[serde(default)]
        documents: Vec<MdnDoc>,
    }
    #[derive(Deserialize)]
    struct MdnDoc {
        mdn_url: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        summary: String,
    }
    let parsed: MdnResponse =
        serde_json::from_slice(bytes).map_err(|_| DiscoveryError::Unavailable)?;
    let rows = parsed.documents.into_iter().filter_map(|doc| {
        let path = doc.mdn_url.trim();
        if path.is_empty() {
            return None;
        }
        let url = if path.starts_with("http://") || path.starts_with("https://") {
            path.to_owned()
        } else {
            format!("https://developer.mozilla.org{path}")
        };
        Some(Row {
            url,
            title: decode_html_text(&doc.title),
            description: decode_html_text(&doc.summary),
        })
    });
    Ok(candidates(rows, count))
}

pub(super) fn parse_openalex_json(
    bytes: &[u8],
    count: usize,
) -> Result<Vec<Candidate>, DiscoveryError> {
    #[derive(Deserialize)]
    struct OaResponse {
        #[serde(default)]
        results: Vec<OaWork>,
    }
    #[derive(Deserialize)]
    struct OaLocation {
        landing_page_url: Option<String>,
    }
    #[derive(Deserialize)]
    struct OaWork {
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        publication_year: Option<u32>,
        #[serde(default)]
        doi: Option<String>,
        #[serde(default)]
        primary_location: Option<OaLocation>,
    }
    let parsed: OaResponse =
        serde_json::from_slice(bytes).map_err(|_| DiscoveryError::Unavailable)?;
    let rows = parsed.results.into_iter().filter_map(|work| {
        let url = work
            .primary_location
            .and_then(|loc| loc.landing_page_url)
            .filter(|u| !u.trim().is_empty())
            .or_else(|| work.doi.filter(|u| !u.trim().is_empty()))?;
        let title = work.title.unwrap_or_default();
        let description = match work.publication_year {
            Some(year) => format!("OpenAlex scholarly work ({year})"),
            None => "OpenAlex scholarly work".to_owned(),
        };
        Some(Row {
            url,
            title: decode_html_text(&title),
            description,
        })
    });
    Ok(candidates(rows, count))
}

pub(super) fn query_has_web_intent(query: &str) -> bool {
    router::classify(query).has_web()
}

pub(super) fn extract_rfc_number(query: &str) -> Option<String> {
    static RFC_NUM: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\brfc[\s-]*(\d{3,5})\b").expect("rfc number regex"));
    RFC_NUM.captures(query).map(|c| format!("rfc{}", &c[1]))
}

fn ietf_search_term(query: &str) -> String {
    static PROTO_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(quic|http/3|http/2|http|tls|oauth|dns|bgp|smtp|imap|jwt|jws|jwe|websocket|sip|rtp|sctp|ipv6|acmes?)\b")
            .expect("ietf protocol regex")
    });
    if let Some(m) = PROTO_RE.find(query) {
        return m.as_str().to_owned();
    }
    query
        .split_whitespace()
        .find(|w| w.chars().filter(|c| c.is_alphanumeric()).count() >= 4)
        .unwrap_or(query)
        .to_owned()
}

pub(super) fn query_has_ietf_intent(query: &str) -> bool {
    router::classify(query).has_ietf()
}

pub(super) fn query_has_biomed_intent(query: &str) -> bool {
    router::classify(query).has_biomed()
}

pub(super) fn query_has_news_intent(query: &str) -> bool {
    router::classify(query).has_news()
}

pub(super) fn parse_gdelt_json(
    bytes: &[u8],
    count: usize,
) -> Result<Vec<Candidate>, DiscoveryError> {
    #[derive(Deserialize)]
    struct GdeltResponse {
        #[serde(default)]
        articles: Vec<GdeltArticle>,
    }
    #[derive(Deserialize)]
    struct GdeltArticle {
        url: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        domain: String,
        #[serde(default)]
        seendate: String,
        #[serde(default)]
        sourcecountry: String,
    }
    let parsed: GdeltResponse =
        serde_json::from_slice(bytes).map_err(|_| DiscoveryError::Unavailable)?;
    let rows = parsed.articles.into_iter().map(|art| {
        let mut parts = Vec::new();
        if !art.domain.trim().is_empty() {
            parts.push(art.domain.trim().to_owned());
        }
        if !art.sourcecountry.trim().is_empty() {
            parts.push(art.sourcecountry.trim().to_owned());
        }
        if !art.seendate.trim().is_empty() {
            parts.push(art.seendate.trim().to_owned());
        }
        Row {
            url: art.url,
            title: decode_html_text(&art.title),
            description: parts.join(" · "),
        }
    });
    Ok(candidates(rows, count))
}

pub(super) fn parse_crossref_json(
    bytes: &[u8],
    count: usize,
) -> Result<Vec<Candidate>, DiscoveryError> {
    #[derive(Deserialize)]
    struct CrResponse {
        message: CrMessage,
    }
    #[derive(Deserialize)]
    struct CrMessage {
        #[serde(default)]
        items: Vec<CrItem>,
    }
    #[derive(Deserialize)]
    struct CrPublished {
        #[serde(rename = "date-parts", default)]
        date_parts: Vec<Vec<u32>>,
    }
    #[derive(Deserialize)]
    struct CrItem {
        #[serde(rename = "DOI", default)]
        doi: Option<String>,
        #[serde(rename = "URL", default)]
        url: Option<String>,
        #[serde(default)]
        title: Vec<String>,
        #[serde(rename = "container-title", default)]
        container_title: Vec<String>,
        #[serde(default)]
        published: Option<CrPublished>,
    }
    let parsed: CrResponse =
        serde_json::from_slice(bytes).map_err(|_| DiscoveryError::Unavailable)?;
    let rows = parsed.message.items.into_iter().filter_map(|item| {
        let url = item
            .url
            .filter(|u| !u.trim().is_empty())
            .or_else(|| item.doi.map(|d| format!("https://doi.org/{}", d.trim())))?;
        let title = item
            .title
            .into_iter()
            .next()
            .map(|t| decode_html_text(&t))
            .unwrap_or_default();
        let venue = item
            .container_title
            .into_iter()
            .next()
            .map(|v| decode_html_text(&v))
            .unwrap_or_default();
        let year = item
            .published
            .and_then(|p| p.date_parts.into_iter().next())
            .and_then(|parts| parts.into_iter().next());
        let description = match (venue.is_empty(), year) {
            (false, Some(y)) => format!("{venue} ({y})"),
            (false, None) => venue,
            (true, Some(y)) => format!("Crossref DOI publication ({y})"),
            (true, None) => "Crossref DOI publication".to_owned(),
        };
        Some(Row {
            url,
            title,
            description,
        })
    });
    Ok(candidates(rows, count))
}

pub(super) fn parse_pubmed_esearch_json(
    bytes: &[u8],
    count: usize,
) -> Result<Vec<String>, DiscoveryError> {
    #[derive(Deserialize)]
    struct ESearchResp {
        esearchresult: ESearchBody,
    }
    #[derive(Deserialize)]
    struct ESearchBody {
        #[serde(default)]
        idlist: Vec<String>,
    }
    let parsed: ESearchResp =
        serde_json::from_slice(bytes).map_err(|_| DiscoveryError::Unavailable)?;
    Ok(parsed
        .esearchresult
        .idlist
        .into_iter()
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
        .take(count.min(10))
        .collect())
}

pub(super) fn parse_pubmed_esummary_json(
    bytes: &[u8],
    count: usize,
) -> Result<Vec<Candidate>, DiscoveryError> {
    let val: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| DiscoveryError::Unavailable)?;
    let Some(result) = val.get("result").and_then(serde_json::Value::as_object) else {
        return Err(DiscoveryError::Unavailable);
    };
    let Some(uids) = result.get("uids").and_then(serde_json::Value::as_array) else {
        return Ok(vec![]);
    };
    let rows = uids.iter().filter_map(|uid_val| {
        let uid = uid_val.as_str()?;
        if !uid.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let item = result.get(uid)?.as_object()?;
        let title = item
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map(decode_html_text)
            .unwrap_or_default();
        let source = item
            .get("source")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim();
        let pubdate = item
            .get("pubdate")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim();
        let description = match (source.is_empty(), pubdate.is_empty()) {
            (false, false) => format!("PubMed · {source} ({pubdate})"),
            (false, true) => format!("PubMed · {source}"),
            (true, false) => format!("PubMed ({pubdate})"),
            (true, true) => "PubMed biomedical article".to_owned(),
        };
        Some(Row {
            url: format!("https://pubmed.ncbi.nlm.nih.gov/{uid}/"),
            title,
            description,
        })
    });
    Ok(candidates(rows, count))
}

pub(super) fn parse_ietf_json(
    bytes: &[u8],
    count: usize,
) -> Result<Vec<Candidate>, DiscoveryError> {
    #[derive(Deserialize)]
    struct IetfResponse {
        #[serde(default)]
        objects: Vec<IetfDoc>,
    }
    #[derive(Deserialize)]
    struct IetfDoc {
        #[serde(default)]
        name: String,
        #[serde(default)]
        rfc_number: Option<u32>,
        #[serde(default)]
        title: String,
        #[serde(default)]
        abstract_text: Option<String>,
        #[serde(rename = "abstract", default)]
        r#abstract: Option<String>,
    }
    let parsed: IetfResponse =
        serde_json::from_slice(bytes).map_err(|_| DiscoveryError::Unavailable)?;
    let rows = parsed.objects.into_iter().filter_map(|doc| {
        let num = doc.rfc_number.or_else(|| {
            doc.name
                .strip_prefix("rfc")
                .and_then(|rest| rest.parse::<u32>().ok())
        })?;
        let url = format!("https://www.rfc-editor.org/rfc/rfc{num}.html");
        let title_clean = decode_html_text(&doc.title);
        let title = if title_clean.is_empty() {
            format!("RFC {num}")
        } else {
            format!("RFC {num}: {title_clean}")
        };
        let description = doc
            .r#abstract
            .or(doc.abstract_text)
            .map(|a| decode_html_text(&a))
            .unwrap_or_default();
        Some(Row {
            url,
            title,
            description,
        })
    });
    Ok(candidates(rows, count))
}

pub(super) fn query_has_tech_intent(query: &str) -> bool {
    router::classify(query).has_tech(query)
}

pub(super) fn query_has_academic_intent(query: &str) -> bool {
    router::classify(query).has_academic()
}

pub(super) fn query_has_repo_intent(query: &str) -> bool {
    router::classify(query).has_repo()
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
    let (row, link, title_sel, snippet, empty, base) = match kind {
        HtmlEngine::Bing => (
            "li.b_algo",
            "h2 a[href]",
            None,
            ".b_caption p",
            "li.b_no",
            "https://www.bing.com/",
        ),
        HtmlEngine::DuckDuckGo => (
            ".result:not(.result--ad)",
            ".result__a[href]",
            None,
            ".result__snippet",
            ".no-results, .result--no-result",
            "https://html.duckduckgo.com/",
        ),
        HtmlEngine::YahooJp => (
            "div.sw-Card.Algo",
            "a.sw-Card__titleInner[href]",
            Some("h3.sw-Card__titleMain"),
            ".sw-Card__summary, .Algo__summary, p",
            ".Empty, .sw-NoResult",
            "https://search.yahoo.co.jp/",
        ),
        HtmlEngine::Seznam => (
            "div[data-n-id][data-e-b-n]",
            "h3 a[data-e-a='heading'][href]",
            None,
            "div[data-n-id] div span",
            ".no-results",
            "https://search.seznam.cz/",
        ),
        HtmlEngine::Marginalia => (
            "main section, main article",
            "h2 a[href]",
            None,
            "p.description, p",
            ".no-results",
            "https://search.marginalia.nu/",
        ),
        HtmlEngine::Naver => (
            "a[href]",
            "span.sds-comps-text-type-headline1",
            None,
            "span.sds-comps-text-type-body1",
            ".not_found02",
            "https://search.naver.com/",
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
    let base = Url::parse(base).expect("search origin");
    if matches!(kind, HtmlEngine::Naver) {
        let a_sel = selector("a[href]");
        let headline_sel = selector("span.sds-comps-text-type-headline1");
        let rows = doc.select(&a_sel).filter_map(|anchor| {
            let headline = anchor.select(&headline_sel).next()?;
            let href = base.join(anchor.attr("href")?).ok()?;
            let url = result_destination(kind, href)?;
            let title = headline
                .text()
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            Some(Row {
                url: url.into(),
                title,
                description: String::new(),
            })
        });
        let hits = candidates(rows, count);
        if hits.is_empty() && doc.select(&selector(empty)).next().is_none() {
            return Err(DiscoveryError::Unavailable);
        }
        return Ok(hits);
    }
    let (row, link, snippet) = (selector(row), selector(link), selector(snippet));
    let title_selector = title_sel.map(selector);
    let rows = doc.select(&row).filter_map(|item| {
        let anchor = item.select(&link).next()?;
        let href = base.join(anchor.attr("href")?).ok()?;
        let url = result_destination(kind, href)?;
        let title_raw: String = title_selector
            .as_ref()
            .and_then(|sel| item.select(sel).next())
            .map(|el| el.text().collect())
            .unwrap_or_else(|| anchor.text().collect());
        let title = title_raw.split_whitespace().collect::<Vec<_>>().join(" ");
        let description_raw: String = item
            .select(&snippet)
            .next()
            .map(|s| s.text().collect())
            .unwrap_or_default();
        let description = description_raw
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        Some(Row {
            url: url.into(),
            title,
            description,
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
        HtmlEngine::YahooJp
            if host == "yahoo.co.jp"
                || host.ends_with(".yahoo.co.jp")
                || host.ends_with(".yimg.jp") =>
        {
            None
        }
        HtmlEngine::Seznam
            if host == "seznam.cz"
                || host.ends_with(".seznam.cz")
                || host == "szn.cz"
                || host.ends_with(".szn.cz") =>
        {
            None
        }
        HtmlEngine::Marginalia if host == "marginalia.nu" || host.ends_with(".marginalia.nu") => {
            None
        }
        HtmlEngine::Naver
            if host == "naver.com"
                || host.ends_with(".naver.com")
                || host == "pstatic.net"
                || host.ends_with(".pstatic.net") =>
        {
            None
        }
        _ => Some(url),
    }
}
