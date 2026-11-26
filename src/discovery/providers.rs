use std::{
    collections::HashSet,
    env,
    time::{Duration, SystemTime},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Client, Response, StatusCode, header};
use scraper::{Html, Selector};
use serde::Deserialize;
use url::Url;

use super::{
    Candidate, DiscoveryError, DiscoveryFuture, EnginePool, Language, Query, SearchEngine,
    SearchOptions,
};
use crate::{endpoint, env_text, pages::public_url, read_body, read_json, secret_header, truncate};

const BODY_LIMIT: usize = 2 * 1024 * 1024;
const USER_AGENT: &str = "Quelle/0.1 (https://github.com/digital4you/quelle)";
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(60);

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
        Err(env::VarError::NotPresent) => "mwmbl".into(),
        Err(_) => return Err("QUELLE_SEARCH_ENGINES"),
    };
    if setting.trim() == "none" {
        return Ok(EnginePool::new(vec![]));
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
            _ => return Err("QUELLE_SEARCH_ENGINES"),
        };
        engines.push(Box::new(NativeEngine {
            client: client.clone(),
            provider,
        }));
    }
    Ok(EnginePool::new(engines))
}

impl SearchEngine for NativeEngine {
    fn spacing(&self) -> Duration {
        match &self.provider {
            Provider::Mwmbl { key: Some(_) } => Duration::from_millis(200),
            Provider::Mwmbl { key: None } => Duration::from_secs(1),
            _ => Duration::ZERO,
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
        }
    }

    fn search<'a>(&'a self, query: &'a Query, options: SearchOptions) -> DiscoveryFuture<'a> {
        Box::pin(self.fetch(query, options))
    }
}

impl NativeEngine {
    fn request(&self, q: &str, lang: &str, options: SearchOptions) -> reqwest::RequestBuilder {
        let count = options.count.to_string();
        match &self.provider {
            Provider::Brave { endpoint, key } => self
                .client
                .get(endpoint.clone())
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
            Provider::Html(HtmlEngine::Bing) => {
                self.client.get("https://www.bing.com/search").query(&[
                    ("q", q),
                    ("count", &count),
                    ("cc", "ch"),
                    ("setlang", lang),
                    ("adlt", "strict"),
                ])
            }
            Provider::Html(HtmlEngine::DuckDuckGo) => self
                .client
                .post("https://html.duckduckgo.com/html/")
                .form(&[("q", q), ("kp", "1")]),
            Provider::Mwmbl { key } => {
                let request = self
                    .client
                    .get("https://api.mwmbl.org/api/v2/search/")
                    .query(&[("q", q)]);
                if let Some(key) = key {
                    request.query(&[("api_key", key)])
                } else {
                    request
                }
            }
            Provider::Mojeek { endpoint, key } => self.client.get(endpoint.clone()).query(&[
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
                .query(&[
                    ("action", "query"),
                    ("list", "search"),
                    ("srsearch", q),
                    ("srlimit", &count),
                    ("srprop", "snippet"),
                    ("format", "json"),
                    ("formatversion", "2"),
                ]),
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
            .request(&query.text, lang, options)
            .header(header::USER_AGENT, USER_AGENT)
            .header(
                header::ACCEPT,
                if html {
                    "text/html"
                } else {
                    "application/json"
                },
            )
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
