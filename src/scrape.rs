//! Firecrawl/Crawl4AI-style `/api/scrape` and `/api/extract` endpoints.
//!
//! - `POST /api/scrape`: scrapes a single `url`, a batch of `urls`, OR runs a
//!   search `query` -> scrapes the discovered pages in Markdown/text/HTML/links.
//! - `POST /api/extract`: scrapes `url`/`urls`/`query` and extracts structured
//!   JSON matching a user-supplied `prompt` and/or `schema` via the configured model.

use std::{collections::HashSet, sync::Arc, time::Duration};

use axum::{
    Json,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::Instant;
use url::Url;

use crate::{
    App, Failure, REQUEST_TIMEOUT, admit,
    discovery::{Language, Query, RESULTS_PER_QUERY, SearchOptions},
    model::Message,
    pages::{self, ExtractOptions, OutputFormat, RenderMode, ScrapeOptions, ScrapedPage},
    privacy, problem,
    search::query_language,
    truncate,
};

const MAX_BATCH_URLS: usize = 50;

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScrapeInput {
    url: Option<Url>,
    urls: Option<Vec<Url>>,
    query: Option<String>,
    language: Option<Language>,
    limit: Option<usize>,
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
    max_tokens: Option<usize>,
    chunk_tokens: Option<usize>,
    focus_query: Option<String>,
    max_pages: Option<usize>,
}

impl ScrapeInput {
    fn valid(&self) -> bool {
        let mode_count = usize::from(self.url.is_some())
            + usize::from(self.urls.is_some())
            + usize::from(self.query.is_some());
        if mode_count != 1 {
            return false;
        }
        if let Some(f) = &self.formats
            && f.is_empty()
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
        if let Some(u) = &self.url {
            return pages::public_url(u.clone()).is_ok();
        }
        if let Some(urls) = &self.urls {
            if urls.is_empty() || urls.len() > MAX_BATCH_URLS {
                return false;
            }
            if let Some(limit) = self.limit
                && !(1..=MAX_BATCH_URLS).contains(&limit)
            {
                return false;
            }
            return urls.iter().all(|u| pages::public_url(u.clone()).is_ok());
        }
        if let Some(q) = &self.query {
            if let Some(limit) = self.limit
                && !(1..=RESULTS_PER_QUERY).contains(&limit)
            {
                return false;
            }
            return !q.trim().is_empty()
                && q.chars().count() <= 600
                && q.split_whitespace().count() <= 75;
        }
        false
    }
}

#[derive(Serialize)]
struct ScrapeErrorEntry {
    url: Url,
    error: &'static str,
}

#[derive(Serialize)]
struct ScrapeBatchOutput {
    data: Vec<ScrapedPage>,
    errors: Vec<ScrapeErrorEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
}

pub async fn scrape_handle(State(app): State<Arc<App>>, req: Request<Body>) -> Response {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let (_permit, input) = match admit(&app, req, ScrapeInput::valid).await {
        Ok(admitted) => admitted,
        Err(response) => return response,
    };

    let options = ScrapeOptions {
        formats: input
            .formats
            .clone()
            .unwrap_or_else(|| vec![OutputFormat::Markdown]),
        extract: ExtractOptions {
            only_main_content: input.only_main_content,
            include_tags: input.include_tags.clone(),
            exclude_tags: input.exclude_tags.clone(),
        },
        render: input.render,
        wait_for_ms: input.wait_for,
        cache_max_age: Duration::from_secs(input.max_age.unwrap_or(300)),
        max_tokens: input.max_tokens,
        chunk_tokens: input.chunk_tokens,
        focus_query: input
            .focus_query
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        max_pages: input.max_pages,
    };

    // Single URL mode -> returns `{ "data": ScrapedPage }` directly (Firecrawl style).
    if let Some(url) = input.url {
        let Ok(permit) = app.search.page_slots.clone().acquire_owned().await else {
            return problem(StatusCode::SERVICE_UNAVAILABLE, "search_capacity");
        };
        return match pages::scrape(url.clone(), &options, permit, deadline).await {
            Ok(page) => Json(json!({ "data": page })).into_response(),
            Err(err) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "url": url, "error": err.code() })),
            )
                .into_response(),
        };
    }

    // Batch URLs or Search -> Scrape mode
    let urls = match resolve_urls(&app, input.urls, input.query, input.language, input.limit).await
    {
        Ok(u) => u,
        Err(resp) => return resp,
    };

    let (data, errors) = scrape_many(&app, urls, &options, deadline).await;
    let (status, error) = if data.is_empty() {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("no_readable_sources"),
        )
    } else {
        (StatusCode::OK, None)
    };
    (
        status,
        Json(ScrapeBatchOutput {
            data,
            errors,
            error,
        }),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// POST /api/extract
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExtractInput {
    url: Option<Url>,
    urls: Option<Vec<Url>>,
    query: Option<String>,
    language: Option<Language>,
    limit: Option<usize>,
    prompt: Option<String>,
    schema: Option<Value>,
    #[serde(default = "default_true")]
    only_main_content: bool,
    #[serde(default)]
    render: RenderMode,
    wait_for: Option<u64>,
}

impl ExtractInput {
    fn valid(&self) -> bool {
        let mode_count = usize::from(self.url.is_some())
            + usize::from(self.urls.is_some())
            + usize::from(self.query.is_some());
        if mode_count != 1 {
            return false;
        }
        if self.prompt.as_deref().is_none_or(|p| p.trim().is_empty()) && self.schema.is_none() {
            return false;
        }
        if let Some(p) = &self.prompt
            && p.chars().count() > 8000
        {
            return false;
        }
        if let Some(u) = &self.url {
            return pages::public_url(u.clone()).is_ok();
        }
        if let Some(urls) = &self.urls {
            return !urls.is_empty()
                && urls.len() <= 20
                && urls.iter().all(|u| pages::public_url(u.clone()).is_ok());
        }
        if let Some(q) = &self.query {
            return !q.trim().is_empty()
                && q.chars().count() <= 600
                && q.split_whitespace().count() <= 75;
        }
        false
    }
}

pub async fn extract_handle(State(app): State<Arc<App>>, req: Request<Body>) -> Response {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let (_permit, input) = match admit(&app, req, ExtractInput::valid).await {
        Ok(admitted) => admitted,
        Err(response) => return response,
    };

    let Some(model) = app.search.model.as_ref() else {
        return problem(
            Failure::ModelNotConfigured.status(),
            Failure::ModelNotConfigured.code(),
        );
    };

    let urls = if let Some(u) = input.url {
        vec![u]
    } else {
        match resolve_urls(
            &app,
            input.urls,
            input.query,
            input.language,
            input.limit.or(Some(5)),
        )
        .await
        {
            Ok(u) => u,
            Err(resp) => return resp,
        }
    };

    let options = ScrapeOptions {
        formats: vec![OutputFormat::Markdown],
        extract: ExtractOptions {
            only_main_content: input.only_main_content,
            include_tags: Vec::new(),
            exclude_tags: Vec::new(),
        },
        render: input.render,
        wait_for_ms: input.wait_for,
        cache_max_age: Duration::from_secs(5 * 60),
        ..ScrapeOptions::default()
    };

    let (pages_data, errors) = scrape_many(&app, urls, &options, deadline).await;
    if pages_data.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": "no_readable_sources",
                "errors": errors,
            })),
        )
            .into_response();
    }

    let sources_payload: Vec<Value> = pages_data
        .iter()
        .map(|p| {
            json!({
                "url": p.url,
                "title": p.title,
                "markdown": truncate(p.markdown.as_deref().unwrap_or(""), 12_000),
            })
        })
        .collect();

    let system = format!(
        "You are a precise web data extractor. Extract ONLY facts grounded in the provided pages. \
         Return valid JSON matching the requested schema or instructions. Do not invent values; use null when absent.{}",
        input
            .prompt
            .as_deref()
            .map(|p| format!("\n\nExtraction instructions:\n{p}"))
            .unwrap_or_default()
    );
    let messages = Message::exchange(system, json!({ "pages": sources_payload }));
    let schema_arg = input.schema.map(|s| ("extraction", s));

    match tokio::time::timeout_at(deadline, model.complete(&messages, 2048, schema_arg))
        .await
        .unwrap_or(Err(Failure::Deadline))
    {
        Ok(raw) => {
            let data: Value = serde_json::from_str(&raw).unwrap_or(Value::String(raw));
            let sources: Vec<Url> = pages_data.into_iter().map(|p| p.url).collect();
            Json(json!({
                "data": data,
                "sources": sources,
                "errors": errors,
            }))
            .into_response()
        }
        Err(failure) => problem(failure.status(), failure.code()),
    }
}

// ---------------------------------------------------------------------------
// Shared Helpers
// ---------------------------------------------------------------------------

async fn resolve_urls(
    app: &App,
    urls: Option<Vec<Url>>,
    query: Option<String>,
    language: Option<Language>,
    limit: Option<usize>,
) -> Result<Vec<Url>, Response> {
    let raw = if let Some(urls) = urls {
        urls
    } else if let Some(q) = query {
        let count = limit.unwrap_or(RESULTS_PER_QUERY).min(RESULTS_PER_QUERY);
        let (text, removed) = privacy::scrub_verbatim(&q);
        if !removed.is_empty()
            && !text
                .split_whitespace()
                .any(|w| w.chars().filter(|c| c.is_alphanumeric()).count() >= 3)
        {
            return Err(problem(
                Failure::QueryWithheld.status(),
                Failure::QueryWithheld.code(),
            ));
        }
        let query = Query {
            language: language.unwrap_or_else(|| query_language(&text)),
            text,
        };
        let (hits, failure, _) = app
            .search
            .discover(
                &[query],
                SearchOptions {
                    count,
                    spellcheck: false,
                },
            )
            .await;
        if let Some(failure) = failure {
            return Err(problem(failure.status(), failure.code()));
        }
        hits.into_iter().map(|hit| hit.url).collect()
    } else {
        vec![]
    };

    let cap = limit.unwrap_or(MAX_BATCH_URLS);
    let mut seen = HashSet::new();
    Ok(raw
        .into_iter()
        .filter_map(|u| pages::public_url(u).ok())
        .filter(|u| seen.insert(u.clone()))
        .take(cap)
        .collect())
}

async fn scrape_many(
    app: &App,
    urls: Vec<Url>,
    options: &ScrapeOptions,
    deadline: Instant,
) -> (Vec<ScrapedPage>, Vec<ScrapeErrorEntry>) {
    let mut jobs = stream::iter(urls)
        .map(|url| {
            let slots = app.search.page_slots.clone();
            let opts = options.clone();
            async move {
                let result = tokio::time::timeout_at(deadline, async {
                    let permit = slots
                        .acquire_owned()
                        .await
                        .expect("page semaphore remains open");
                    pages::scrape(url.clone(), &opts, permit, deadline).await
                })
                .await
                .unwrap_or(Err(pages::PageError::Timeout));
                (url, result)
            }
        })
        .buffered(6);

    let mut data = Vec::new();
    let mut errors = Vec::new();
    while let Some((url, result)) = jobs.next().await {
        match result {
            Ok(page) => data.push(page),
            Err(err) => errors.push(ScrapeErrorEntry {
                url,
                error: err.code(),
            }),
        }
    }
    (data, errors)
}
