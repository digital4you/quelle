//! Search a caller's finished query or read supplied URLs into full-text sources.
use std::{collections::HashSet, sync::Arc};

use axum::{
    Json,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use url::Url;

use crate::{
    App, Failure, REQUEST_TIMEOUT, admit,
    discovery::{Language, Query, RESULTS_PER_QUERY, SearchOptions},
    pages, privacy, problem,
    rerank::Strategy,
    search::query_language,
    selection,
};

#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum Input {
    Urls {
        urls: Vec<Url>,
        limit: Option<usize>,
        #[serde(default)]
        chunks: bool,
        #[serde(rename = "maxTokens")]
        max_tokens: Option<usize>,
        #[serde(rename = "chunkTokens")]
        chunk_tokens: Option<usize>,
        #[serde(rename = "focusQuery")]
        focus_query: Option<String>,
        #[serde(default)]
        structured: bool,
        #[serde(rename = "maxPages")]
        max_pages: Option<usize>,
    },
    Search {
        query: String,
        limit: Option<usize>,
        /// Language hint for engines that take one; guessed when absent.
        language: Option<Language>,
        #[serde(rename = "candidateLimit")]
        candidate_limit: Option<usize>,
        #[serde(default)]
        selection: Strategy,
        #[serde(default, rename = "preferredDomains")]
        preferred_domains: Vec<String>,
        #[serde(rename = "followUp")]
        follow_up: Option<bool>,
        #[serde(default)]
        chunks: bool,
        #[serde(rename = "maxTokens")]
        max_tokens: Option<usize>,
        #[serde(rename = "chunkTokens")]
        chunk_tokens: Option<usize>,
        #[serde(rename = "focusQuery")]
        focus_query: Option<String>,
        #[serde(default)]
        structured: bool,
        #[serde(rename = "maxPages")]
        max_pages: Option<usize>,
    },
}

impl Input {
    fn limit(&self) -> usize {
        match self {
            Self::Urls { limit, .. } | Self::Search { limit, .. } => {
                limit.unwrap_or(RESULTS_PER_QUERY)
            }
        }
    }

    fn extras(&self) -> pages::FetchExtras {
        match self {
            Self::Urls {
                chunks,
                max_tokens,
                chunk_tokens,
                focus_query,
                structured,
                max_pages,
                ..
            } => pages::FetchExtras {
                chunks: *chunks,
                max_tokens: *max_tokens,
                chunk_tokens: *chunk_tokens,
                focus_query: focus_query
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
                structured: *structured,
                max_pages: *max_pages,
            },
            Self::Search {
                query,
                chunks,
                max_tokens,
                chunk_tokens,
                focus_query,
                structured,
                max_pages,
                ..
            } => {
                let explicit_focus = focus_query
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                // When searching with a token budget or chunking enabled and no
                // explicit focusQuery, default focusQuery to the search query!
                let effective_focus = explicit_focus
                    .or_else(|| (*chunks || max_tokens.is_some()).then(|| query.clone()));
                pages::FetchExtras {
                    chunks: *chunks,
                    max_tokens: *max_tokens,
                    chunk_tokens: *chunk_tokens,
                    focus_query: effective_focus,
                    structured: *structured,
                    max_pages: *max_pages,
                }
            }
        }
    }

    fn valid(&self) -> bool {
        if !(1..=RESULTS_PER_QUERY).contains(&self.limit()) {
            return false;
        }
        let (max_tokens, chunk_tokens, focus_query, max_pages) = match self {
            Self::Urls {
                max_tokens,
                chunk_tokens,
                focus_query,
                max_pages,
                ..
            }
            | Self::Search {
                max_tokens,
                chunk_tokens,
                focus_query,
                max_pages,
                ..
            } => (
                *max_tokens,
                *chunk_tokens,
                focus_query.as_deref(),
                *max_pages,
            ),
        };
        if let Some(t) = max_tokens
            && !(32..=200_000).contains(&t)
        {
            return false;
        }
        if let Some(t) = chunk_tokens
            && !(32..=4096).contains(&t)
        {
            return false;
        }
        if let Some(q) = focus_query
            && q.chars().count() > 600
        {
            return false;
        }
        if let Some(p) = max_pages
            && !(1..=pages::MAX_PDF_MAX_PAGES).contains(&p)
        {
            return false;
        }
        match self {
            Self::Urls { urls, .. } => {
                (1..=RESULTS_PER_QUERY).contains(&urls.len())
                    && urls.iter().all(|u| pages::public_url(u.clone()).is_ok())
            }
            Self::Search {
                query,
                candidate_limit,
                preferred_domains,
                ..
            } => {
                !query.trim().is_empty()
                    && query.chars().count() <= 600
                    && query.split_whitespace().count() <= 75
                    && (self.limit()..=RESULTS_PER_QUERY)
                        .contains(&candidate_limit.unwrap_or(RESULTS_PER_QUERY))
                    && preferred_domains.len() <= 8
                    && preferred_domains.iter().all(|d| selection::valid_domain(d))
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Document {
    #[serde(skip_serializing_if = "Option::is_none")]
    requested_url: Option<Url>,
    #[serde(flatten)]
    source: pages::Source,
    #[serde(skip_serializing_if = "Option::is_none")]
    chunks: Option<Vec<pages::chunks::Chunk>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    structured: Option<pages::structured::StructuredData>,
}

#[derive(Serialize)]
struct ReadError {
    url: Url,
    error: &'static str,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Diagnostics {
    candidates: usize,
    fetch_attempts: usize,
    duplicates: usize,
    follow_up: bool,
    ranking_ms: u64,
    total_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    ranking_fallback: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    follow_up_error: Option<&'static str>,
}

#[derive(Default, Serialize)]
struct Extraction {
    sources: Vec<Document>,
    errors: Vec<ReadError>,
    diagnostics: Diagnostics,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
}

impl Extraction {
    #[allow(clippy::too_many_arguments)]
    async fn read(
        &mut self,
        app: &App,
        urls: Vec<Url>,
        limit: usize,
        deadline: Instant,
        extras: &pages::FetchExtras,
        attempted: &mut HashSet<String>,
        evidence: &mut selection::EvidenceSet,
    ) {
        let mut queued = HashSet::new();
        let urls: Vec<_> = urls
            .into_iter()
            .filter_map(|u| pages::public_url(u).ok())
            .filter(|u| !attempted.contains(&selection::url_key(u)))
            .filter(|u| queued.insert(selection::url_key(u)))
            .collect();
        self.diagnostics.candidates += urls.len();
        // Preserve ranking despite different host speeds, refilling every failed
        // or duplicate result from the remaining candidate pool.
        let mut jobs = stream::iter(urls)
            .map(|url| {
                let slots = app.search.page_slots.clone();
                let extras = extras.clone();
                async move {
                    let result = tokio::time::timeout_at(deadline, async {
                        let permit = slots
                            .acquire_owned()
                            .await
                            .expect("page semaphore remains open");
                        pages::fetch_rich(url.clone(), &extras, permit, deadline).await
                    })
                    .await
                    .unwrap_or(Err(pages::PageError::Timeout));
                    (url, result)
                }
            })
            .buffered(4);
        while self.sources.len() < limit {
            let Some((url, result)) = jobs.next().await else {
                break;
            };
            attempted.insert(selection::url_key(&url));
            self.diagnostics.fetch_attempts += 1;
            match result {
                Ok(rich) if evidence.insert(&rich.source.metadata.url, &rich.source.content) => {
                    self.sources.push(Document {
                        requested_url: (rich.source.metadata.url != url).then_some(url),
                        source: rich.source,
                        chunks: rich.chunks,
                        structured: rich.structured,
                    })
                }
                Ok(_) => self.diagnostics.duplicates += 1,
                Err(error) => self.errors.push(ReadError {
                    url,
                    error: error.code(),
                }),
            }
        }
    }
}

pub async fn handle(State(app): State<Arc<App>>, req: Request<Body>) -> Response {
    let started = Instant::now();
    let deadline = started + REQUEST_TIMEOUT;
    let (_permit, input) = match admit(&app, req, Input::valid).await {
        Ok(admitted) => admitted,
        Err(response) => return response,
    };
    let limit = input.limit();
    let extras = input.extras();
    let mut output = Extraction::default();
    let mut followup = None;
    let mut preferred = Vec::new();
    let mut strategy = Strategy::Heuristic;
    let urls = match input {
        Input::Urls { urls, .. } => urls,
        Input::Search {
            query,
            language,
            candidate_limit,
            selection,
            preferred_domains,
            follow_up,
            ..
        } => {
            let (text, removed) = privacy::scrub_verbatim(&query);
            if !removed.is_empty()
                && !text
                    .split_whitespace()
                    .any(|w| w.chars().filter(|c| c.is_alphanumeric()).count() >= 3)
            {
                return problem(
                    Failure::QueryWithheld.status(),
                    Failure::QueryWithheld.code(),
                );
            }
            let query = Query {
                language: language.unwrap_or_else(|| query_language(&text)),
                text,
            };
            preferred = preferred_domains
                .into_iter()
                .map(|d| d.to_ascii_lowercase())
                .collect();
            strategy = selection;
            if follow_up.unwrap_or(true) {
                followup = Some(Query {
                    text: query.text.clone(),
                    language: query.language,
                });
            }
            let (hits, failure, _) = app
                .search
                .discover(
                    std::slice::from_ref(&query),
                    SearchOptions {
                        count: candidate_limit.unwrap_or(RESULTS_PER_QUERY),
                        spellcheck: false,
                    },
                )
                .await;
            if let Some(failure) = failure {
                return problem(failure.status(), failure.code());
            }
            let ranked_at = Instant::now();
            let (hits, fallback) = app
                .search
                .rank_candidates(&query.text, query.language, hits, &preferred, strategy)
                .await;
            output.diagnostics.ranking_ms = ranked_at.elapsed().as_millis() as u64;
            output.diagnostics.ranking_fallback = fallback;
            hits.into_iter().map(|hit| hit.url).collect()
        }
    };
    let mut attempted = HashSet::new();
    let mut evidence = selection::EvidenceSet::default();
    output
        .read(
            &app,
            urls,
            limit,
            deadline,
            &extras,
            &mut attempted,
            &mut evidence,
        )
        .await;
    let missing: Vec<_> = preferred
        .iter()
        .filter(|domain| {
            !output.sources.iter().any(|d| {
                selection::matches_domain(&d.source.metadata.url, std::slice::from_ref(domain))
            })
        })
        .cloned()
        .collect();
    if let Some(original) = followup
        && let Some(text) = selection::followup(&original.text, &missing)
        && Instant::now() < deadline
    {
        let query = Query {
            text,
            language: original.language,
        };
        output.diagnostics.follow_up = true;
        let follow = async {
            let (hits, failure, _) = app
                .search
                .discover(
                    std::slice::from_ref(&query),
                    SearchOptions {
                        count: RESULTS_PER_QUERY,
                        spellcheck: false,
                    },
                )
                .await;
            output.diagnostics.follow_up_error = failure.map(|f| f.code());
            let ranked_at = Instant::now();
            let (hits, fallback) = app
                .search
                .rank_candidates(&query.text, query.language, hits, &preferred, strategy)
                .await;
            output.diagnostics.ranking_ms += ranked_at.elapsed().as_millis() as u64;
            output.diagnostics.ranking_fallback = output.diagnostics.ranking_fallback.or(fallback);
            let urls = hits
                .into_iter()
                .filter(|h| selection::matches_domain(&h.url, &missing))
                .map(|h| h.url)
                .collect();
            let before = output.sources.len();
            output
                .read(
                    &app,
                    urls,
                    before + 1,
                    deadline,
                    &extras,
                    &mut attempted,
                    &mut evidence,
                )
                .await;
            if output.sources.len() > limit {
                let primary = output.sources.pop().unwrap();
                output.sources.pop();
                output.sources.push(primary);
            }
        };
        if tokio::time::timeout_at(deadline, follow).await.is_err() {
            output.diagnostics.follow_up_error = Some("timeout");
        }
    }
    output.diagnostics.total_ms = started.elapsed().as_millis() as u64;
    let status = if output.sources.is_empty() {
        output.error = Some("no_readable_sources");
        StatusCode::UNPROCESSABLE_ENTITY
    } else {
        StatusCode::OK
    };
    (status, Json(output)).into_response()
}
