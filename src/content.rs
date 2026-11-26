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
    discovery::{Query, RESULTS_PER_QUERY, SearchOptions},
    pages, privacy, problem,
    search::query_language,
};

#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum Input {
    Urls {
        urls: Vec<Url>,
        limit: Option<usize>,
    },
    Search {
        query: String,
        limit: Option<usize>,
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

    fn valid(&self) -> bool {
        if !(1..=RESULTS_PER_QUERY).contains(&self.limit()) {
            return false;
        }
        match self {
            Self::Urls { urls, .. } => {
                (1..=RESULTS_PER_QUERY).contains(&urls.len())
                    && urls.iter().all(|u| pages::public_url(u.clone()).is_ok())
            }
            Self::Search { query, .. } => {
                !query.trim().is_empty()
                    && query.chars().count() <= 600
                    && query.split_whitespace().count() <= 75
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
}

#[derive(Serialize)]
struct ReadError {
    url: Url,
    error: &'static str,
}

#[derive(Serialize)]
struct Extraction {
    sources: Vec<Document>,
    errors: Vec<ReadError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
}

pub async fn handle(State(app): State<Arc<App>>, req: Request<Body>) -> Response {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let (_permit, input) = match admit(&app, req, Input::valid).await {
        Ok(admitted) => admitted,
        Err(response) => return response,
    };
    let limit = input.limit();
    let urls = match input {
        Input::Urls { urls, .. } => urls,
        Input::Search { query, .. } => {
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
                language: query_language(&text),
                text,
            };
            let (hits, failure, _) = app
                .search
                .discover(
                    &[query],
                    SearchOptions {
                        count: limit,
                        spellcheck: false,
                    },
                )
                .await;
            if let Some(failure) = failure {
                return problem(failure.status(), failure.code());
            }
            hits.into_iter().map(|hit| hit.url).collect()
        }
    };
    let mut seen = HashSet::new();
    let urls = urls
        .into_iter()
        .filter_map(|u| pages::public_url(u).ok())
        .filter(|u| seen.insert(u.clone()))
        .take(limit);
    let mut jobs = stream::iter(urls)
        .map(|url| {
            let slots = app.search.page_slots.clone();
            async move {
                let result = tokio::time::timeout_at(deadline, async {
                    let permit = slots
                        .acquire_owned()
                        .await
                        .expect("page semaphore remains open");
                    pages::fetch(url.clone(), None, permit, deadline).await
                })
                .await
                .unwrap_or(Err(pages::PageError::Timeout));
                (url, result)
            }
        })
        .buffered(4);
    let mut sources = Vec::new();
    let mut errors = Vec::new();
    while let Some((url, result)) = jobs.next().await {
        match result {
            Ok(source) => sources.push(Document {
                requested_url: (source.metadata.url != url).then_some(url),
                source,
            }),
            Err(error) => errors.push(ReadError {
                url,
                error: error.code(),
            }),
        }
    }
    let (status, error) = if sources.is_empty() {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("no_readable_sources"),
        )
    } else {
        (StatusCode::OK, None)
    };
    (
        status,
        Json(Extraction {
            sources,
            errors,
            error,
        }),
    )
        .into_response()
}
