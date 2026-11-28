//! Search a caller's finished query or read supplied URLs into full-text sources.
use std::{collections::HashSet, convert::Infallible, sync::Arc, time::Duration};

use axum::{
    Json,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
};
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::time::Instant;
use url::Url;

use crate::{
    App, Failure, REQUEST_TIMEOUT, admit,
    discovery::{Language, Query, RESULTS_PER_QUERY, SearchOptions},
    pages, politeness, privacy, problem,
    rerank::Strategy,
    search::query_language,
    selection,
};

/// Once `limit` unique sources have finished, wait at most this long for any
/// higher-ranked in-flight URL before returning, eliminating head-of-line stalls.
const RANK_GRACE_WINDOW: Duration = Duration::from_millis(1500);

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
        #[serde(rename = "totalMaxTokens")]
        total_max_tokens: Option<usize>,
        #[serde(rename = "chunkTokens")]
        chunk_tokens: Option<usize>,
        #[serde(rename = "focusQuery")]
        focus_query: Option<String>,
        #[serde(default)]
        structured: bool,
        #[serde(rename = "maxPages")]
        max_pages: Option<usize>,
        #[serde(default)]
        stream: bool,
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
        #[serde(rename = "totalMaxTokens")]
        total_max_tokens: Option<usize>,
        #[serde(rename = "chunkTokens")]
        chunk_tokens: Option<usize>,
        #[serde(rename = "focusQuery")]
        focus_query: Option<String>,
        #[serde(default)]
        structured: bool,
        #[serde(rename = "maxPages")]
        max_pages: Option<usize>,
        #[serde(default)]
        stream: bool,
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

    fn total_max_tokens(&self) -> Option<usize> {
        match self {
            Self::Urls {
                total_max_tokens, ..
            }
            | Self::Search {
                total_max_tokens, ..
            } => *total_max_tokens,
        }
    }

    fn stream(&self) -> bool {
        match self {
            Self::Urls { stream, .. } | Self::Search { stream, .. } => *stream,
        }
    }

    fn wants_chunks_output(&self) -> bool {
        match self {
            Self::Urls { chunks, .. } | Self::Search { chunks, .. } => *chunks,
        }
    }

    fn extras(&self) -> pages::FetchExtras {
        match self {
            Self::Urls {
                chunks,
                max_tokens,
                total_max_tokens,
                chunk_tokens,
                focus_query,
                structured,
                max_pages,
                ..
            } => pages::FetchExtras {
                chunks: *chunks || total_max_tokens.is_some(),
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
                total_max_tokens,
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
                let effective_focus = explicit_focus.or_else(|| {
                    (*chunks || max_tokens.is_some() || total_max_tokens.is_some())
                        .then(|| query.clone())
                });
                pages::FetchExtras {
                    chunks: *chunks || total_max_tokens.is_some(),
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
        let (max_tokens, total_max_tokens, chunk_tokens, focus_query, max_pages) = match self {
            Self::Urls {
                max_tokens,
                total_max_tokens,
                chunk_tokens,
                focus_query,
                max_pages,
                ..
            }
            | Self::Search {
                max_tokens,
                total_max_tokens,
                chunk_tokens,
                focus_query,
                max_pages,
                ..
            } => (
                *max_tokens,
                *total_max_tokens,
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
        if let Some(t) = total_max_tokens
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
        demote_slow_hosts: bool,
        attempted: &mut HashSet<String>,
        evidence: &mut selection::EvidenceSet,
    ) {
        let mut queued = HashSet::new();
        let mut urls: Vec<_> = urls
            .into_iter()
            .filter_map(|u| pages::public_url(u).ok())
            .filter(|u| !attempted.contains(&selection::url_key(u)))
            .filter(|u| queued.insert(selection::url_key(u)))
            .collect();
        if demote_slow_hosts && urls.len() > limit {
            // Stable-demote hosts with an open circuit breaker or >7 s EWMA
            // behind healthy candidates so zombie origins never stall search batches.
            let (healthy, degraded): (Vec<_>, Vec<_>) = urls.into_iter().partition(|u| {
                let host = u.host_str().unwrap_or_default();
                !politeness::global().is_circuit_open(host)
                    && politeness::global()
                        .host_latency_ms(host)
                        .is_none_or(|ms| ms < 7_000)
            });
            urls = healthy;
            urls.extend(degraded);
        }
        self.diagnostics.candidates += urls.len();

        let needed = limit.saturating_sub(self.sources.len());
        if needed == 0 || urls.is_empty() {
            return;
        }

        // Unordered completion eliminates head-of-line blocking on stalled hosts:
        // once `needed` unique sources succeed, wait at most `RANK_GRACE_WINDOW`
        // for any higher-ranked in-flight candidate, then restore candidate rank order.
        let concurrency = if urls.len() > needed {
            (needed + 2).clamp(4, 8)
        } else {
            needed.clamp(1, 6)
        };
        let mut iter = urls.into_iter().enumerate();
        let mut in_flight = FuturesUnordered::new();
        let mut pending_ranks: HashSet<usize> = HashSet::new();

        let spawn_next = |iter: &mut std::iter::Enumerate<std::vec::IntoIter<Url>>,
                          in_flight: &mut FuturesUnordered<_>,
                          pending_ranks: &mut HashSet<usize>| {
            if let Some((rank, url)) = iter.next() {
                pending_ranks.insert(rank);
                let slots = app.search.page_slots.clone();
                let extras = extras.clone();
                in_flight.push(async move {
                    let result = tokio::time::timeout_at(deadline, async {
                        let permit = slots
                            .acquire_owned()
                            .await
                            .expect("page semaphore remains open");
                        pages::fetch_rich(url.clone(), &extras, permit, deadline).await
                    })
                    .await
                    .unwrap_or(Err(pages::PageError::Timeout));
                    (rank, url, result)
                });
                true
            } else {
                false
            }
        };

        for _ in 0..concurrency {
            if !spawn_next(&mut iter, &mut in_flight, &mut pending_ranks) {
                break;
            }
        }

        let mut accepted: Vec<(usize, Document)> = Vec::new();
        let mut grace_deadline: Option<Instant> = None;

        while !in_flight.is_empty() {
            // If we already have `needed` sources and no higher-ranked candidate is
            // still in flight, finish immediately!
            if accepted.len() >= needed {
                accepted.sort_by_key(|(r, _)| *r);
                let cutoff_rank = accepted[needed - 1].0;
                let has_higher_pending = pending_ranks.iter().any(|&r| r < cutoff_rank);
                if !has_higher_pending {
                    break;
                }
                if grace_deadline.is_none() {
                    grace_deadline = Some((Instant::now() + RANK_GRACE_WINDOW).min(deadline));
                }
            }

            let next_item = match grace_deadline {
                Some(stop) => match tokio::time::timeout_at(stop, in_flight.next()).await {
                    Ok(item) => item,
                    Err(_) => break,
                },
                None => in_flight.next().await,
            };
            let Some((rank, url, result)) = next_item else {
                break;
            };
            pending_ranks.remove(&rank);
            attempted.insert(selection::url_key(&url));
            self.diagnostics.fetch_attempts += 1;
            match result {
                Ok(rich) if evidence.insert(&rich.source.metadata.url, &rich.source.content) => {
                    accepted.push((
                        rank,
                        Document {
                            requested_url: (rich.source.metadata.url != url).then_some(url),
                            source: rich.source,
                            chunks: rich.chunks,
                            structured: rich.structured,
                        },
                    ));
                }
                Ok(_) => self.diagnostics.duplicates += 1,
                Err(error) => self.errors.push(ReadError {
                    url,
                    error: error.code(),
                }),
            }
            if accepted.len() < needed {
                spawn_next(&mut iter, &mut in_flight, &mut pending_ranks);
            }
        }

        accepted.sort_by_key(|(rank, _)| *rank);
        for (_, doc) in accepted.into_iter().take(needed) {
            self.sources.push(doc);
        }
    }

    fn apply_global_budget(
        &mut self,
        total_max_tokens: usize,
        focus_query: Option<&str>,
        keep_chunks_field: bool,
    ) {
        if self.sources.is_empty() {
            return;
        }
        let doc_chunks: Vec<Vec<pages::chunks::Chunk>> = self
            .sources
            .iter()
            .map(|doc| {
                doc.chunks.clone().unwrap_or_else(|| {
                    pages::chunks::chunk_markdown(
                        &doc.source.content,
                        &doc.source.metadata.url,
                        &[],
                        pages::chunks::DEFAULT_CHUNK_TOKENS,
                    )
                })
            })
            .collect();
        let packed = pages::chunks::budget_evidence_pack(doc_chunks, focus_query, total_max_tokens);
        for (doc, kept) in self.sources.iter_mut().zip(packed) {
            doc.source.content = pages::chunks::join_chunks_markdown(&kept);
            doc.chunks = keep_chunks_field.then_some(kept);
        }
        self.sources.retain(|d| !d.source.content.trim().is_empty());
    }
}

fn sse_event(payload: serde_json::Value) -> Result<Event, Infallible> {
    Ok(Event::default().data(payload.to_string()))
}

async fn fetch_stream_one(
    slots: Arc<tokio::sync::Semaphore>,
    url: Url,
    extras: pages::FetchExtras,
    deadline: Instant,
) -> (Url, Result<pages::RichSource, pages::PageError>) {
    let res = tokio::time::timeout_at(deadline, async {
        let p = slots.acquire_owned().await.expect("page semaphore");
        pages::fetch_rich(url.clone(), &extras, p, deadline).await
    })
    .await
    .unwrap_or(Err(pages::PageError::Timeout));
    (url, res)
}

pub async fn handle(State(app): State<Arc<App>>, req: Request<Body>) -> Response {
    let started = Instant::now();
    let deadline = started + REQUEST_TIMEOUT;
    let (permit, input) = match admit(&app, req, Input::valid).await {
        Ok(admitted) => admitted,
        Err(response) => return response,
    };
    let limit = input.limit();
    let extras = input.extras();
    let total_max_tokens = input.total_max_tokens();
    let keep_chunks = input.wants_chunks_output();
    let streaming = input.stream();
    let is_search = matches!(input, Input::Search { .. });
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

    if streaming {
        let stream = async_stream::stream! {
            let _permit = permit;
            let mut diag = output.diagnostics;
            let mut queued = HashSet::new();
            let clean_urls: Vec<Url> = urls
                .into_iter()
                .filter_map(|u| pages::public_url(u).ok())
                .filter(|u| queued.insert(selection::url_key(u)))
                .collect();
            diag.candidates = clean_urls.len();
            let concurrency = (limit + 2).clamp(4, 8);
            let mut iter = clean_urls.into_iter();
            let mut in_flight = FuturesUnordered::new();
            let mut evidence = selection::EvidenceSet::default();
            let mut emitted = 0usize;

            for _ in 0..concurrency {
                if let Some(url) = iter.next() {
                    in_flight.push(fetch_stream_one(
                        app.search.page_slots.clone(),
                        url,
                        extras.clone(),
                        deadline,
                    ));
                }
            }

            while emitted < limit {
                let Some((url, result)) = in_flight.next().await else {
                    break;
                };
                diag.fetch_attempts += 1;
                match result {
                    Ok(rich) if evidence.insert(&rich.source.metadata.url, &rich.source.content) => {
                        emitted += 1;
                        let doc = Document {
                            requested_url: (rich.source.metadata.url != url).then_some(url),
                            source: rich.source,
                            chunks: keep_chunks.then_some(rich.chunks).flatten(),
                            structured: rich.structured,
                        };
                        yield sse_event(json!({ "type": "source", "data": doc }));
                    }
                    Ok(_) => diag.duplicates += 1,
                    Err(err) => {
                        yield sse_event(json!({
                            "type": "error",
                            "data": ReadError { url, error: err.code() }
                        }));
                    }
                }
                if emitted < limit
                    && let Some(url) = iter.next()
                {
                    in_flight.push(fetch_stream_one(
                        app.search.page_slots.clone(),
                        url,
                        extras.clone(),
                        deadline,
                    ));
                }
            }
            diag.total_ms = started.elapsed().as_millis() as u64;
            yield sse_event(json!({ "type": "done", "diagnostics": diag }));
        };
        return Sse::new(stream)
            .keep_alive(KeepAlive::new().interval(Duration::from_secs(10)))
            .into_response();
    }

    let _permit = permit;
    let mut attempted = HashSet::new();
    let mut evidence = selection::EvidenceSet::default();
    output
        .read(
            &app,
            urls,
            limit,
            deadline,
            &extras,
            is_search,
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
                    true,
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
    if let Some(budget) = total_max_tokens {
        output.apply_global_budget(budget, extras.focus_query.as_deref(), keep_chunks);
    } else if !keep_chunks {
        for doc in &mut output.sources {
            doc.chunks = None;
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
