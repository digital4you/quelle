mod discovery;
mod model;
mod pages;
mod privacy;
mod search;

use std::{
    convert::Infallible, env, net::SocketAddr, process::ExitCode, sync::Arc, time::Duration,
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, StatusCode, header},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use reqwest::header::HeaderValue;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use subtle::ConstantTimeEq;
use tokio::{net::TcpListener, sync::Semaphore, time::Instant};
use url::Url;

use crate::{
    discovery::Brave,
    model::Model,
    pages::Source,
    search::{Diagnostics, Search},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(110);
const MAX_REQUEST_BODY: usize = 256 * 1024;

#[derive(Clone, Copy, Debug)]
enum Failure {
    InvalidPlan,
    QueryWithheld,
    ModelNotConfigured,
    UnresolvedContext,
    DiscoveryRateLimited,
    DiscoveryUnavailable,
    NoReadableSources,
    ModelUnavailable,
    IncompleteModelResponse,
    TokenizerUnavailable,
    ContextTooLarge,
    Deadline,
}

impl Failure {
    fn code(self) -> &'static str {
        match self {
            Self::InvalidPlan => "invalid_plan",
            Self::QueryWithheld => "query_withheld",
            Self::ModelNotConfigured => "model_not_configured",
            Self::UnresolvedContext => "context_resolution_unavailable",
            Self::DiscoveryRateLimited => "discovery_rate_limited",
            Self::DiscoveryUnavailable => "discovery_unavailable",
            Self::NoReadableSources => "no_readable_sources",
            Self::ModelUnavailable => "model_unavailable",
            Self::IncompleteModelResponse => "incomplete_model_response",
            Self::TokenizerUnavailable => "tokenizer_unavailable",
            Self::ContextTooLarge => "context_too_large",
            Self::Deadline => "deadline_exceeded",
        }
    }

    fn status(self) -> StatusCode {
        match self {
            Self::NoReadableSources
            | Self::UnresolvedContext
            | Self::ContextTooLarge
            | Self::QueryWithheld => StatusCode::UNPROCESSABLE_ENTITY,
            Self::DiscoveryRateLimited | Self::ModelNotConfigured => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::Deadline => StatusCode::GATEWAY_TIMEOUT,
            _ => StatusCode::BAD_GATEWAY,
        }
    }
}

#[derive(Debug)]
enum BodyError {
    TooLarge,
    Read,
    Data,
}

async fn read_body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, BodyError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(BodyError::TooLarge);
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| BodyError::Read)? {
        if chunk.len() > limit.saturating_sub(data.len()) {
            return Err(BodyError::TooLarge);
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

async fn read_json<T: DeserializeOwned>(
    response: reqwest::Response,
    limit: usize,
) -> Result<T, BodyError> {
    serde_json::from_slice(&read_body(response, limit).await?).map_err(|_| BodyError::Data)
}

fn truncate(text: &str, characters: usize) -> String {
    text.char_indices()
        .nth(characters)
        .map_or_else(|| text.to_owned(), |(end, _)| text[..end].to_owned())
}

fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(3))
}

struct App {
    authorization: HeaderValue,
    slots: Arc<Semaphore>,
    search: Search,
}

#[derive(Serialize)]
struct Answer {
    message: String,
    sources: Vec<Source>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostics: Option<Diagnostics>,
}

fn problem(status: StatusCode, code: &'static str) -> Response {
    (status, Json(json!({"error": code}))).into_response()
}

/// Authenticates, takes a search slot and parses the request body. Shared by
/// both endpoints; the permit is held until the response is complete.
async fn admit(
    app: &App,
    req: Request<Body>,
) -> Result<(tokio::sync::OwnedSemaphorePermit, search::Request), Response> {
    let expected = app.authorization.as_bytes();
    if !req
        .headers()
        .get(header::AUTHORIZATION)
        .is_some_and(|value| bool::from(value.as_bytes().ct_eq(expected)))
    {
        return Err(problem(
            StatusCode::UNAUTHORIZED,
            "backend_authentication_required",
        ));
    }
    let Ok(permit) = app.slots.clone().try_acquire_owned() else {
        return Err(problem(StatusCode::SERVICE_UNAVAILABLE, "search_capacity"));
    };
    let body = match tokio::time::timeout(
        Duration::from_secs(10),
        to_bytes(req.into_body(), MAX_REQUEST_BODY),
    )
    .await
    {
        Ok(Ok(body)) => body,
        Ok(Err(error))
            if std::error::Error::source(&error)
                .is_some_and(|source| source.is::<http_body_util::LengthLimitError>()) =>
        {
            return Err(problem(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_body_too_large",
            ));
        }
        Ok(Err(_)) => return Err(problem(StatusCode::BAD_REQUEST, "invalid_search_request")),
        Err(_) => return Err(problem(StatusCode::REQUEST_TIMEOUT, "request_body_timeout")),
    };
    let Ok(request) = serde_json::from_slice::<search::Request>(&body) else {
        return Err(problem(StatusCode::BAD_REQUEST, "invalid_search_request"));
    };
    if !request.validate() {
        return Err(problem(StatusCode::BAD_REQUEST, "invalid_search_request"));
    }
    Ok((permit, request))
}

#[derive(Serialize)]
struct Results {
    queries: Vec<String>,
    sources: Vec<Source>,
    hits: Vec<search::Hit>,
    diagnostics: Diagnostics,
}

/// Results without an answer: every page that was read, with its selected
/// passages, and every other search hit with the engine's title and
/// snippet. Same request body as /api/search; `stream` is ignored.
async fn sources(State(app): State<Arc<App>>, req: Request<Body>) -> Response {
    let started = Instant::now();
    let (permit, request) = match admit(&app, req).await {
        Ok(admitted) => admitted,
        Err(response) => return response,
    };
    let _permit = permit;
    let mut diagnostics = Diagnostics::default();
    let result = tokio::time::timeout_at(
        started + REQUEST_TIMEOUT,
        app.search.gather(&request, &mut diagnostics),
    )
    .await
    .unwrap_or(Err(Failure::Deadline));
    // An empty read set with hits is still a useful result.
    let result = match result {
        Ok(gathered) if gathered.sources.is_empty() && gathered.hits.is_empty() => {
            Err(gathered.failure.unwrap_or(Failure::NoReadableSources))
        }
        other => other,
    };
    record(started, &result);
    diagnostics.total_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    diagnostics.error = result.as_ref().err().map(|failure| failure.code());
    match result {
        Ok(gathered) => Json(Results {
            queries: gathered.queries,
            sources: gathered.sources,
            hits: gathered.hits,
            diagnostics,
        })
        .into_response(),
        Err(failure) => (
            failure.status(),
            Json(json!({"error": failure.code(), "diagnostics": diagnostics})),
        )
            .into_response(),
    }
}

async fn handle(State(app): State<Arc<App>>, req: Request<Body>) -> Response {
    let started = Instant::now();
    let deadline = started + REQUEST_TIMEOUT;
    let (permit, request) = match admit(&app, req).await {
        Ok(admitted) => admitted,
        Err(response) => return response,
    };
    // Answers need the model; refuse after authentication and before any
    // search call.
    if app.search.model.is_none() {
        drop(permit);
        return problem(
            Failure::ModelNotConfigured.status(),
            Failure::ModelNotConfigured.code(),
        );
    }
    let legacy = request.legacy();
    if request.stream {
        // The generator owns the permit and all upstream futures. Closing the
        // response drops them, including pending discovery and model requests.
        let stream = async_stream::stream! {
            let _permit = permit;
            let mut diagnostics = Diagnostics::default();
            let ready = tokio::time::timeout_at(deadline, app.search.prepare(&request, &mut diagnostics)).await.unwrap_or(Err(Failure::Deadline));
            let outcome = match ready {
                Ok(ready) => {
                    yield event(json!({"type": "sources", "data": ready.sources}));
                    // Mate's legacy parser marks the first response event as the
                    // start of writing. No answer text is sent until validation.
                    yield event(json!({"type": "response", "data": ""}));
                    tokio::time::timeout_at(deadline, app.search.write(&ready, &mut diagnostics)).await.unwrap_or(Err(Failure::Deadline))
                }
                Err(failure) => Err(failure),
            };
            record(started, &outcome);
            match outcome {
                Ok(message) => {
                    yield event(json!({"type": "response", "data": message}));
                    yield event(json!({"type": "done"}));
                }
                Err(Failure::NoReadableSources) if legacy => yield event(json!({"type": "done"})),
                Err(failure) => yield event(json!({"type": "error", "data": failure.code()})),
            }
        };
        return Sse::new(stream)
            .keep_alive(KeepAlive::new().interval(Duration::from_secs(10)))
            .into_response();
    }
    let _permit = permit;
    let mut diagnostics = Diagnostics::default();
    let result = tokio::time::timeout_at(deadline, async {
        let ready = app.search.prepare(&request, &mut diagnostics).await?;
        let message = app.search.write(&ready, &mut diagnostics).await?;
        Ok((message, ready.sources))
    })
    .await
    .unwrap_or(Err(Failure::Deadline));
    record(started, &result);
    diagnostics.total_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    diagnostics.error = result.as_ref().err().map(|failure| failure.code());
    // Only structured callers receive diagnostics; the legacy shape stays as is.
    let diagnostics = (!legacy).then_some(diagnostics);
    match result {
        Ok((message, sources)) => Json(Answer {
            message,
            sources,
            diagnostics,
        })
        .into_response(),
        // The Go legacy adapter maps an empty answer to its public no-sources error.
        Err(Failure::NoReadableSources) if legacy => Json(Answer {
            message: String::new(),
            sources: vec![],
            diagnostics: None,
        })
        .into_response(),
        Err(failure) => (
            failure.status(),
            Json(json!({
                "error": failure.code(),
                "diagnostics": diagnostics,
            })),
        )
            .into_response(),
    }
}

fn record<T>(started: Instant, outcome: &Result<T, Failure>) {
    let error = outcome.as_ref().err().map(|failure| failure.code());
    tracing::info!(
        stage = "search_complete",
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        error
    );
}

fn event(payload: serde_json::Value) -> Result<Event, Infallible> {
    Ok(Event::default().data(payload.to_string()))
}

fn env_text(name: &'static str) -> Result<String, &'static str> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty() && value.trim() == value)
        .ok_or(name)
}

fn secret_header(value: String, name: &'static str) -> Result<HeaderValue, &'static str> {
    let mut header = HeaderValue::try_from(value).map_err(|_| name)?;
    header.set_sensitive(true);
    Ok(header)
}

fn endpoint(value: &str, name: &'static str) -> Result<Url, &'static str> {
    let mut url = Url::parse(value).map_err(|_| name)?;
    let loopback = matches!(url.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback())
        || matches!(url.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback())
        || url.host_str() == Some("localhost");
    if url.host().is_none()
        || (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(name);
    }
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&path);
    Ok(url)
}

fn positive_env(
    name: &'static str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, &'static str> {
    match env::var(name) {
        Ok(value) => value
            .parse()
            .ok()
            .filter(|value| (min..=max).contains(value))
            .ok_or(name),
        Err(env::VarError::NotPresent) => Ok(default),
        _ => Err(name),
    }
}

/// The answer model is optional: with none of `QUELLE_MODEL_BASE_URL`,
/// `QUELLE_MODEL_API_KEY` and `QUELLE_MODEL_NAME` set, Quelle runs without
/// one. Setting only some of them is a configuration error.
fn configure_model(
    client: &reqwest::Client,
    token: &str,
    brave_token: &HeaderValue,
) -> Result<Option<Model>, &'static str> {
    let names = [
        "QUELLE_MODEL_BASE_URL",
        "QUELLE_MODEL_API_KEY",
        "QUELLE_MODEL_NAME",
    ];
    let present = names
        .iter()
        .filter(|name| env::var_os(name).is_some_and(|value| !value.is_empty()))
        .count();
    if present == 0 {
        return Ok(None);
    }
    if present < names.len() {
        return Err(
            "QUELLE_MODEL_BASE_URL, QUELLE_MODEL_API_KEY and QUELLE_MODEL_NAME must be set together",
        );
    }
    let provider_key = env_text("QUELLE_MODEL_API_KEY")?;
    let provider_auth = secret_header(format!("Bearer {provider_key}"), "QUELLE_MODEL_API_KEY")?;
    if brave_token.as_bytes() == provider_key.as_bytes() || provider_key == token {
        return Err("service, model and search credentials must be distinct");
    }
    let base = endpoint(&env_text("QUELLE_MODEL_BASE_URL")?, "QUELLE_MODEL_BASE_URL")?;
    let mut completions = base.clone();
    completions.set_path(&format!(
        "{}/chat/completions",
        base.path().trim_end_matches('/')
    ));
    let mut tokenize = base.clone();
    let root = base.path().trim_end_matches('/');
    tokenize.set_path(&format!(
        "{}/tokenize",
        root.strip_suffix("/v1").unwrap_or(root)
    ));
    Ok(Some(Model {
        client: client.clone(),
        completions,
        tokenize,
        name: env_text("QUELLE_MODEL_NAME")?,
        authorization: provider_auth,
        context_limit: positive_env("QUELLE_MODEL_CONTEXT_TOKENS", 64_000, 2048, 64_000)?,
    }))
}

fn configure() -> Result<(SocketAddr, Arc<App>), &'static str> {
    let listen = env::var("QUELLE_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:3000".to_owned())
        .parse()
        .map_err(|_| "QUELLE_LISTEN")?;
    let token = env_text("QUELLE_TOKEN")?;
    if token.len() < 32 || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err("QUELLE_TOKEN");
    }
    let authorization = secret_header(format!("Bearer {token}"), "QUELLE_TOKEN")?;
    let brave_token = secret_header(env_text("BRAVE_SEARCH_API_KEY")?, "BRAVE_SEARCH_API_KEY")?;
    if brave_token.as_bytes() == token.as_bytes() {
        return Err("service, model and search credentials must be distinct");
    }
    let client = client_builder().build().map_err(|_| "HTTP client")?;
    let model = configure_model(&client, &token, &brave_token)?;
    let br = env::var("BRAVE_SEARCH_URL")
        .unwrap_or_else(|_| "https://api.search.brave.com/res/v1/web/search".to_owned());
    let user_agent = env::var("QUELLE_USER_AGENT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| pages::DEFAULT_USER_AGENT.to_owned());
    pages::set_user_agent(HeaderValue::try_from(user_agent).map_err(|_| "QUELLE_USER_AGENT")?);
    let concurrent = positive_env("QUELLE_CONCURRENCY", 8, 1, 64)?;
    let app = App {
        authorization,
        slots: Arc::new(Semaphore::new(concurrent)),
        search: Search {
            model,
            engines: vec![Box::new(Brave::new(
                client,
                endpoint(&br, "BRAVE_SEARCH_URL")?,
                brave_token,
            ))],
            page_slots: Arc::new(Semaphore::new(concurrent * 4)),
        },
    };
    Ok((listen, Arc::new(app)))
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"ok": true}))
}

async fn shutdown() {
    #[cfg(unix)]
    if let Ok(mut stop) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = stop.recv() => {} }
        return;
    }
    let _ = tokio::signal::ctrl_c().await;
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt().with_target(false).init();
    let (listen, app) = match configure() {
        Ok(config) => config,
        Err(field) => {
            tracing::error!(field, "invalid search service configuration");
            return ExitCode::FAILURE;
        }
    };
    let Ok(listener) = TcpListener::bind(listen).await else {
        tracing::error!(%listen, "could not bind search service");
        return ExitCode::FAILURE;
    };
    tracing::info!(address = %listener.local_addr().unwrap_or(listen), "search service listening");
    if pages::detect_pdf_support() {
        tracing::info!("PDF text extraction enabled");
    } else {
        tracing::warn!("pdftotext not found; PDF sources are skipped");
    }
    if app.search.model.is_none() {
        tracing::warn!(
            "no answer model configured; /api/search answers 503, /api/sources searches the request text directly"
        );
    }
    if pages::detect_renderer() {
        tracing::info!("Lightpanda rendering enabled for script-only pages");
    } else {
        tracing::warn!("Lightpanda not found; script-only pages are skipped");
    }
    let router = Router::new()
        .route("/api/search", post(handle))
        .route("/api/sources", post(sources))
        .route("/healthz", get(health))
        .route("/api/providers", get(health))
        .with_state(app)
        .layer(axum::middleware::map_response(
            |mut response: Response| async move {
                response
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                response
            },
        ));
    match axum::serve(listener, router)
        .with_graceful_shutdown(shutdown())
        .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
