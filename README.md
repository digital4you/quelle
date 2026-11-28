# Quelle

Quelle is a Rust service that retrieves web pages for agents. It can answer a question with citations or return readable page text for your own model.

| Mode | Endpoint | Returns | Needs a model |
| --- | --- | --- | --- |
| Answer | `POST /api/answer` | An answer with numbered citations and supporting passages | Yes |
| Content | `POST /api/content` | Page text in document order for the caller's exact search query or supplied URLs | No |
| Scrape | `POST /api/scrape` | Clean Markdown, text, HTML, links, and metadata for a `url`, batch `urls`, or search `query` | No |
| Map | `POST /api/map` | Fast site URL discovery from `robots.txt`, `sitemap.xml`, and page links | No |
| Crawl | `POST /api/crawl` (`GET`/`DELETE /api/crawl/{id}`, `GET /api/crawl/{id}/stream`) | Async, live-streamed SSE, or sync recursive crawl with mid-crawl resume checkpoints | No |
| Extract | `POST /api/extract` | Structured JSON extracted from `url`, `urls`, or `query` with schema validation and corrective retry | Yes |
| MCP | `quelle mcp` (stdio JSON-RPC 2.0) | Built-in Model Context Protocol server exposing `quelle_search`, `quelle_scrape`, `quelle_extract`, `quelle_map`, and `quelle_crawl` | No |

## Quick start

Copy [.env.example](.env.example) to `.env` and set `QUELLE_TOKEN` to a token of at least 32 characters. You can generate one with `openssl rand -hex 32`. This enables `/api/content` with keyless search through Mwmbl. Set `QUELLE_SEARCH_ENGINES` to choose other engines. To enable `/api/answer`, also fill in the three model variables before starting Quelle.

```sh
docker run -d --name quelle --env-file .env \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  -p 127.0.0.1:3000:3000 ghcr.io/digital4you/quelle:master
curl -sS http://127.0.0.1:3000/healthz
```

The health check returns `{"ok":true}`. For the API examples below, set `QUELLE_TOKEN` in your client shell to the same value you put in `.env`.

To run from source, export the configuration in your shell and run `cargo run --locked --release` with Rust 1.95 or later. Install `pdftotext` (from `poppler-utils`) for PDFs and `lightpanda` for JavaScript rendering. The Docker image includes both.

## API

Both endpoints accept JSON and require `Authorization: Bearer $QUELLE_TOKEN`.

### Answer mode · `POST /api/answer`

Send a question. The model plans searches and ranks results. Quelle selects up to 6,500 characters of relevant passages per page, then the model writes an answer citing those passages. If the model is unconfigured, this endpoint returns HTTP 503 with `model_not_configured` before searching.

| Request field | Required | Type and behavior |
| --- | --- | --- |
| `query` | Yes | Nonblank string, up to 12,000 characters. The question or research request. |
| `history` | No | Array of up to 10 `[role, text]` pairs. Roles are `user` (alias `human`) and `assistant`. Limit of 4,000 characters per turn and 12,000 across all turns. |
| `optimizationMode` | No | String. Choose `speed` (default), `balanced`, or `quality` to set the search and answer budgets. |
| `sources` | No | Array of up to three search focuses drawn from `web`, `academic`, and `discussions`. |
| `systemInstructions` | No | String, up to 64,000 characters. Supplies writer instructions when `context` is absent. |
| `stream` | No | Boolean, default `false`. Set to `true` for server-sent events. |
| `context` | No | Object for callers that supply the user's original request separately from the research query. See the example below. |

```sh
curl -sS http://127.0.0.1:3000/api/answer \
  -H "Authorization: Bearer $QUELLE_TOKEN" -H 'Content-Type: application/json' \
  --data '{"query":"Where does IANA describe example domains?","optimizationMode":"speed"}'
```

The JSON response contains `message` and `sources`. A citation such as `[1]` refers to the first source, with its supporting passages in `content` and its title and URL in `metadata`. If Quelle finds no readable evidence and the request omitted `context`, it returns HTTP 200 with `{"message":"","sources":[]}`.

A caller can pass structured context with the question. The object requires `version` and a nonblank `originalRequest`. Optional fields are `serverInstructions`, `clientInstructions`, `answerLanguage`, `publicAffairs`, `contextual`, and `identity`.

```json
{
  "query": "IANA policy on registering example.org",
  "context": {
    "version": "search-v1",
    "originalRequest": "Can I register example.org for my business?",
    "answerLanguage": "en"
  }
}
```

With `context`, a non-streaming search returns timing and count diagnostics with `diagnostics.version` set to `"evidence-v1"`. Missing evidence returns HTTP 422 with `no_readable_sources`.

With `"stream": true`, read the JSON object in each server-sent event's `data:` line. A successful stream starts with the sources, sends an empty `response` when writing begins, then sends the complete cited answer in a second `response` followed by `done`. Failures after streaming starts use `{"type":"error","data":"<code>"}`. A basic request with no readable evidence ends with `done`.

### Content mode · `POST /api/content`

Supply a finished search query or a list of known URLs. Both forms return page text for the caller to use directly. Sending both fields or an unknown field returns HTTP 400.

| Request field | Required | Type and behavior |
| --- | --- | --- |
| `query` | One of `query` or `urls` | Nonblank string containing the finished search query, up to 600 characters and 75 words. |
| `urls` | One of `query` or `urls` | Array of 1–20 public HTTP(S) URL strings on standard ports to read directly. |
| `limit` | No | Integer from 1 to 20, default 20. Caps returned unique documents. Failed and duplicate candidates are replaced from the remaining pool. |
| `language` | No | With `query` only: `de`, `fr`, `it` or `en`. The language hint for engines that take one, such as Brave or Wikipedia. Without it, Quelle guesses from common words and spelling and falls back to `de`. |
| `candidateLimit` | No | With `query`: discovery pool size, from `limit` to 20, default 20. |
| `selection` | No | With `query`: `engine` (default), `heuristic`, `rerank`, or `chat`. Model failures fall back to heuristic order and are reported in diagnostics. |
| `preferredDomains` | No | With `query`: up to eight domain names to prioritize, including their subdomains. An explicit preference also overrides engine order. |
| `followUp` | No | With `query`: default true. At most one extra site-restricted search when a preferred domain has no retrieved evidence. No expansion without preferred domains, or for an existing `site:` query. |
| `chunks` | No | Boolean, default `false`. Return heading-scoped, section-anchored (`#id`) chunks with breadcrumbs and token counts on each source without splitting code blocks or tables. |
| `maxTokens` | No | Integer (32–200,000). Distills each page's Markdown and chunks to fit the per-page token budget while preserving document order. |
| `totalMaxTokens` | No | Integer (32–200,000). Deduplicates syndicated chunks across all returned sources and packs a single global evidence bundle within the total token cap. |
| `chunkTokens` | No | Integer (32–4,096, default 512). Target token size per chunk. |
| `focusQuery` | No | String (up to 600 chars). Scores and selects the highest-relevance chunks for `maxTokens` / `totalMaxTokens` distillation (defaults to `query` in search mode when chunking or budgeting is set). |
| `structured` | No | Boolean, default `false`. Return deterministic single-pass `outline` (with `#id` URLs), real data `tables` (excluding layout tables), and `jsonLd` payloads alongside content. |
| `maxPages` | No | Integer (1–1,000, default `QUELLE_PDF_MAX_PAGES` = 150). Override the PDF page cap for this request. |
| `stream` | No | Boolean, default `false`. Stream SSE events (`source`, `error`, `done`) as each page completes instead of buffering the full batch. |

To fetch known pages directly, pass `urls`. This makes zero search or model calls.

```sh
curl -sS http://127.0.0.1:3000/api/content \
  -H "Authorization: Bearer $QUELLE_TOKEN" -H 'Content-Type: application/json' \
  --data '{"urls":["https://www.rfc-editor.org/rfc/rfc2606.txt"]}'
```

To discover pages, pass the exact query you want the search engine to run.

```sh
curl -sS http://127.0.0.1:3000/api/content \
  -H "Authorization: Bearer $QUELLE_TOKEN" -H 'Content-Type: application/json' \
  --data '{"query":"site:iana.org \"example domains\"","limit":3}'
```

Quelle preserves case, whitespace, quoted phrases and search operators outside privacy redactions. Each engine interprets its own query syntax. For Brave, Quelle also disables spellcheck. The default `selection: "engine"` preserves discovery order. Optional heuristic ordering uses engine rank as a prior, then query terms, requested dates/versions, matching language paths and preferred domains, with a modest host-diversity adjustment. Optional model and heuristic ranking both prioritize preferred domains, requested documentation versions, current documentation for unversioned requests and stable releases over unsolicited previews. These metadata preferences do not discard candidates or prove freshness. Quelle reads up to four candidates at a time and deduplicates tracking variants, redirect targets and near-identical extracted text.

Read `sources[].content` for the page text and `sources[].metadata.url` for the final URL. Each source also has `metadata.title` and `metadata.truncated`. When the final URL differs, `requestedUrl` records the original. Successful sources follow input or selected candidate order; a recovered preferred source may replace the last result. Individual failures appear in `errors` as objects with `url` and `error`. `diagnostics` reports pool size, completed fetch attempts, duplicates, ranking time/fallback, follow-up and total time. Up to three speculative fetches may be cancelled once the result limit is reached. Partial success returns HTTP 200. If every read fails or discovery finds nothing, HTTP 422 includes `error: "no_readable_sources"` alongside both arrays.

Hard failures have distinct codes: `not_found` (404/410), `authentication_required` (401/407), `rate_limited` (429) and `upstream_error` (5xx). They do not trigger browser escalation. A failed negotiated Markdown variant gets one HTML-only retry of the original URL; explicit Markdown URLs and ordinary HTML 404s remain failures. Renderable blocks can escalate through Lightpanda and the optional Chromium sidecar. Both extraction APIs reject recognized error documents, empty app shells, loading screens and unpopulated table components; short text alone is not proof of failure.

For dedicated ranking, configure `QUELLE_RERANK_URL` with the complete `/rerank` endpoint and `QUELLE_RERANK_MODEL`. `QUELLE_RERANK_API_KEY` is optional; `QUELLE_RERANK_TIMEOUT_MS` defaults to 3000 (100–30000). The endpoint accepts public query text and bounded title/URL/snippet documents in the vLLM/Jina-style rerank contract. Content requests opt in using `selection: "rerank"`; answer requests use it instead of chat ranking when configured. `selection: "chat"` uses the configured answer model, bounded to five seconds. Malformed indices, duplicate indices, network failures and timeouts preserve every candidate in heuristic order. Rank scores are selection signals, not evidence.

Identical concurrent discovery and page reads are coalesced. Full-text extraction has a bounded 16 MiB cache, tied to the raw response fingerprint and freshness. HTTP caches honor `no-store`, `private`, `no-cache` and `max-age`; expired bodies with ETag/Last-Modified can be conditionally refreshed. Redirects and fresh requests still undergo public-address validation and DNS pinning.

### Scrape mode · `POST /api/scrape`

Scrape a single `url`, a batch of `urls` (1–50), or discover-and-scrape via a search `query`. Returns clean LLM-ready Markdown, plain text, raw HTML, extracted links, and metadata (`description`, `language`, `canonicalUrl`, `og:*`, `statusCode`, `contentType`).

```sh
curl -sS http://127.0.0.1:3000/api/scrape \
  -H "Authorization: Bearer $QUELLE_TOKEN" -H 'Content-Type: application/json' \
  --data '{"url":"https://example.org","formats":["markdown","links"],"onlyMainContent":true}'
```

Optional fields: `formats` (`["markdown","text","html","links","chunks","structured","screenshot","fullPageScreenshot"]`, default `["markdown"]`; screenshots require `QUELLE_CHROMIUM_CDP_URL`), `onlyMainContent` (default `true`), `includeTags` / `excludeTags` (CSS selectors), `render` (`"auto"`, `"always"`, `"chromium"`, `"never"`), `waitFor` (ms), `waitForSelector` (CSS selector awaited in CDP), `scrollSteps` (0–10 smooth viewport scrolls for lazy-loaded pages), `actions` (up to 16 safe scoped actions: `click`, `wait`, `waitForSelector`, `scroll`, `press` — no arbitrary JavaScript execution), `maxAge` (cache TTL in seconds, default `300`; set `0` to bypass cache), `maxTokens` (token-budgeted distillation), `chunkTokens` (default `512`), `focusQuery` (query-focused chunk scoring), `maxPages` (PDF page cap, 1–1,000), `stream` (SSE streaming for batch `urls`/`query`), `limit`, `language`.

### Map mode · `POST /api/map`

Discover URLs on a site (or set of seed `urls`) via `robots.txt`, `sitemap.xml` (including sitemap indexes), and link extraction.

```sh
curl -sS http://127.0.0.1:3000/api/map \
  -H "Authorization: Bearer $QUELLE_TOKEN" -H 'Content-Type: application/json' \
  --data '{"url":"https://example.org","limit":100}'
```

Optional fields: `urls` (multiple seeds), `search` (URL substring filter), `includeSubdomains`, `useSitemap` (default `true`), `includePaths`, `excludePaths`, `limit` (1–5000, default 200).

### Crawl mode · `POST /api/crawl`

Recursively crawl starting from a single `url` **or** a list of `urls` (1–100 seeds). By default returns HTTP 202 with `{"id":"...","status":"running","url":"/api/crawl/<id>","streamUrl":"/api/crawl/<id>/stream"}`; poll `GET /api/crawl/{id}`, subscribe to live SSE events at `GET /api/crawl/{id}/stream`, or cancel with `DELETE /api/crawl/{id}`. When `QUELLE_STATE_DIR` is configured, periodic checkpoints allow interrupted crawls to resume automatically on restart. Set `"sync": true` for small crawls that should return results directly in one request.

```sh
curl -sS http://127.0.0.1:3000/api/crawl \
  -H "Authorization: Bearer $QUELLE_TOKEN" -H 'Content-Type: application/json' \
  --data '{"urls":["https://example.org/docs","https://example.org/blog"],"maxDepth":2,"limit":30,"formats":["markdown"]}'
```

Optional fields: `maxDepth` (0–10, default 2), `limit` (1–1000, default 50), `includePaths` / `excludePaths` (prefix or `*` globs), `allowSubdomains`, `allowExternalLinks`, `useSitemap` (default `true`), `respectRobots` (default `true`), `formats`, `onlyMainContent`, `includeTags`, `excludeTags`, `render`, `waitFor`, `waitForSelector`, `scrollSteps` (0–10), `actions`, `maxAge`, `maxTokens`, `chunkTokens`, `focusQuery`, `maxPages`, `concurrency` (1–16, default 4; per-host politeness is always enforced), `sync` (default `false`), `incremental` (emit deterministic `sha256:` `contentHash`, `changeStatus`, and crawl `changes` summary; defaults `maxAge` to `0` for conditional HTTP revalidation), `sinceCrawlId` (compare against a prior crawl job — persisted across restarts when `QUELLE_STATE_DIR` is set — and attach heading-level `diff`s for updated pages), `previousHashes` (stateless `{url: contentHash}` map), `omitUnchangedContent` (omit page bodies when `changeStatus == "unchanged"`).

### Extract mode · `POST /api/extract`

Scrape `url`, `urls` (1–20), or a search `query` (with optional `render`, `waitFor`, `waitForSelector`, `scrollSteps`, and `actions`) and extract structured JSON matching `prompt` and/or JSON `schema` using the configured model, with automatic schema validation and 1-shot corrective retry.

```sh
curl -sS http://127.0.0.1:3000/api/extract \
  -H "Authorization: Bearer $QUELLE_TOKEN" -H 'Content-Type: application/json' \
  --data '{"url":"https://example.org","prompt":"Extract the page title and main topics"}'
```

### Built-in MCP Server & Thin Clients

Run `quelle mcp` to expose Quelle over Model Context Protocol stdio JSON-RPC 2.0 (`QUELLE_URL` + `QUELLE_TOKEN` / `QUELLE_API_KEY`):

```json
{
  "mcpServers": {
    "quelle": {
      "command": "quelle",
      "args": ["mcp"],
      "env": {
        "QUELLE_URL": "http://127.0.0.1:3000",
        "QUELLE_TOKEN": "<your-token>"
      }
    }
  }
}
```

Zero-dependency clients are included in `clients/python/quelle.py` (stdlib `urllib`) and `clients/typescript/quelle.ts` (`fetch` + SSE async generators).

### Page extraction

- **HTML.** `/api/content` emits clean structured Markdown in document order, including headings, lists, fenced code blocks with preserved indentation, Markdown data tables with captions and expanded row/column spans, flattened layout tables, link destinations with section anchors, collapsed ARIA accordion and tab-panel content, repeated values and footer contacts. It prefers a substantial main/article region and supplements public contact targets outside it. Both extraction APIs share charset decoding, invisible-character cleanup and document-quality checks. It leaves out site navigation, non-semantic sidebars and menus, search forms, the site banner, consent dialogs, language switchers and skip links. A static page that consists only of navigation keeps it. It extracts `mailto:` and `tel:` targets from direct links, simple JavaScript contact functions and links in the rendered page. A handler that needs a click or a follow-up API request may yield only its visible label.
- **JavaScript.** When static HTML looks like a script shell or an unrendered template, Quelle renders via a supervised persistent Lightpanda CDP pool over a private 0600 Unix socket (`QUELLE_CDP_POOL=true` by default, with automatic restart and one-shot `lightpanda fetch` fallback). Non-content assets (images, fonts, audio/video streams, ad/analytics beacons) are blocked at the browser layer. Readiness timing starts after navigation finishes so slow origin TTFB does not starve hydration; the browser waits for populated content and tables, followed by 350 ms of main-content stability (or 80 ms when the activity observer reports a fully loaded synchronous page with no pending timers or async network work). When one-shot timers, in-flight requests, or DOM mutations are still active past the 7 s base wait, adaptive wait extends up to 13 s so delayed JS apps complete cleanly while idle pages stop immediately at 7 s. If the page needs rendering and the browser is unavailable, its error code is `needs_js`.
- **Documents.** Plain text retains line breaks. PDFs return their embedded text layer up to `QUELLE_PDF_MAX_PAGES` (default 150, configurable per request via `maxPages` up to 1,000). Fragmented multi-column/table PDF output automatically retries with `pdftotext -layout` and formats aligned columns as Markdown tables (`retrieval: "pdf-layout"`); scanned or garbled PDFs without a usable text layer escalate automatically to LlamaIndex [OCRia-nano-1](https://huggingface.co/llamaindex/OCRia-nano-1) via LiteParse (`lit`, `retrieval: "pdf-ocria"`) when installed, or to `QUELLE_OCR_URL` when configured. Word, Excel, PowerPoint, OpenDocument, RTF, EPUB and CSV files are converted to Markdown with [anydoc](https://github.com/firecrawl/anydoc).

The output limit for `/api/content` is 2 MiB of UTF-8 text per source. `metadata.truncated` flags a byte or PDF page limit reached during extraction. A site can still require scrolling, pagination or a click to reveal additional content.

### Search privacy and errors

Before sending a query to any engine, Quelle filters identifiers such as email addresses, phone numbers, IBANs and credentials. A query reduced to fragments can return `422 query_withheld`. The same filtered query goes to each fallback. Region and safe-search controls depend on the engine. In answer mode the configured model receives the question and supplied conversation context.

Quelle checks page destinations on every redirect and restricts page and browser requests to public addresses: private, loopback, link-local, shared (CGNAT), documentation, multicast and other reserved IPv4 and IPv6 ranges are refused. Host names are resolved as absolute names, so resolver search domains such as Kubernetes' `<namespace>.svc.cluster.local` do not apply, and the checked addresses are the ones connected to. Diagnostics contain stage timings and counts. Rendering logs include the host name of the page.

Common HTTP errors include `400 invalid_search_request` for invalid input, `401 backend_authentication_required` for a missing or incorrect bearer token, `503 search_capacity` when all request slots are busy, and `503 discovery_rate_limited` when every attempted engine is rate-limited or cooling down. Upstream outages can return `502 discovery_unavailable`. The JSON response includes a machine-readable `error` field.

## Configuration

### Required at startup

| Variable | Purpose |
| --- | --- |
| `QUELLE_TOKEN` | Bearer token clients send to Quelle. Use at least 32 printable, non-space ASCII characters. |

### Search engines

Set `QUELLE_SEARCH_ENGINES` to a comma-separated list of engines. With the variable unset, Quelle uses `brave` when `BRAVE_SEARCH_API_KEY` is present, and otherwise defaults to keyless parallel meta-search (`duckduckgo,mwmbl,wikipedia,stackexchange,hackernews`). Use `none` to disable discovery and read supplied URLs directly. Requests requiring discovery then return `503 discovery_not_configured`.

| Engine | What it searches | Credential |
| --- | --- | --- |
| `brave` | Brave Web Search API | `BRAVE_SEARCH_API_KEY` required |
| `duckduckgo` | DuckDuckGo HTML web results (with locale `kl=` and desktop headers) | None |
| `bing` | Bing web results, parsed from the result page. Not enabled by default; check Bing's terms of use before enabling it. | None |
| `mwmbl` | Mwmbl open web index (JSON API) | Anonymous or optional `MWMBL_API_KEY` |
| `mojeek` | Mojeek web index (JSON API) | `MOJEEK_API_KEY` required |
| `wikipedia` | Wikipedia articles in the query language (`de`, `fr`, `it`, or `en`) | None |
| `stackexchange` | Stack Overflow Q&A (official Stack Exchange v2.3 API; alias `stackoverflow`) | None |
| `hackernews` | Primary tech articles & discussions via Hacker News Algolia API (alias `hn`) | None |
| `arxiv` | Academic preprints via arXiv Atom API (auto-boosted on academic/paper queries) | None |
| `github` | Open-source repositories via GitHub Search API (auto-boosted on repo/library queries) | None |
| `searxng` | Self-hosted SearXNG JSON API (`QUELLE_SEARXNG_URL`, optional `QUELLE_SEARXNG_ENGINES` / `QUELLE_SEARXNG_CATEGORIES`) | None |

By default (`QUELLE_SEARCH_MODE=fuse`), when multiple engines are configured, Quelle queries eligible engines **in parallel**, uses a **primary-aware tail-cut grace window** (600 ms after a primary web engine answers; up to 1,400 ms if only secondary engines have answered while a primary engine is still in flight), deduplicates canonical URLs, and merges results using **weighted Reciprocal Rank Fusion (RRF)** with a cross-engine consensus bonus. Specialized engines (`wikipedia`, `stackexchange`, `hackernews`, `arxiv`, `github`) are skipped automatically on unrelated `site:` domains, and English-centric `stackexchange`/`hackernews` are skipped on non-English non-technical queries. Set `QUELLE_SEARCH_MODE=rotate` to restore single-engine round-robin fallback. Nonempty results are cached for 15 minutes; rate-limited engines enter cooldown automatically. Page fetches use unordered fast-fill completion with a 1.5 s rank-preservation grace window and per-host circuit breakers so stalled origins cannot block healthy results.

Discovery allows up to six upstream attempts per API request, shared across the planned queries in answer mode. Rate-limited engines enter a cooldown. Mwmbl requests are also spaced to match its anonymous or keyed rate. Invalid engine names, duplicate entries, and missing keys for enabled engines stop startup.

### Required only for `/api/answer`

Set all three connection variables together to enable answer generation. Omit all three for content-only use. A partial configuration stops startup. Use separate credentials for Quelle, each search provider, and the model provider.

| Variable | Purpose |
| --- | --- |
| `QUELLE_MODEL_BASE_URL` | OpenAI-compatible model API base URL, such as `https://model.example.org/v1`. For that base path, the server must support `POST /v1/chat/completions` and vLLM-compatible `POST /tokenize`. Plain HTTP is accepted on loopback. |
| `QUELLE_MODEL_API_KEY` | Bearer key for the model provider. |
| `QUELLE_MODEL_NAME` | Model ID served by that endpoint. |

### Optional settings

| Variable | Default | Purpose |
| --- | --- | --- |
| `QUELLE_MODEL_CONTEXT_TOKENS` | `64000` | Context ceiling for answer mode, from 2,048 to 64,000 tokens. |
| `BRAVE_SEARCH_URL` | `https://api.search.brave.com/res/v1/web/search` | Override the Brave-compatible web search endpoint. Plain HTTP is accepted on loopback. |
| `MOJEEK_SEARCH_URL` | `https://api.mojeek.com/search` | Override the Mojeek-compatible endpoint. Plain HTTP is accepted on loopback. |
| `QUELLE_LISTEN` | `127.0.0.1:3000` natively, `0.0.0.0:3000` in Docker | Address and port Quelle binds to. |
| `QUELLE_CONCURRENCY` | `8` | Maximum simultaneous search requests, from 1 to 64. |
| `QUELLE_USER_AGENT` | Built-in desktop Chrome user agent | User agent for ordinary page fetches. |
| `QUELLE_LIGHTPANDA` | `lightpanda` on `PATH` | Path to the browser executable for JavaScript rendering. |
| `QUELLE_BLOCKED_NETWORKS` | None | Comma-separated CIDR ranges or addresses, such as `34.118.224.0/20,2600:1f14::/56`, that page fetches and the browser must not reach, in addition to the built-in reserved ranges. Invalid entries stop startup. |
| `QUELLE_PROXY_URL` | None | Optional HTTP/HTTPS/SOCKS proxy URL for outbound page fetches. |
| `QUELLE_BROWSER_URL` | None | Optional external Playwright/Chromium sidecar endpoint (e.g. `http://127.0.0.1:3003/scrape`) used as Tier 3 fallback when Lightpanda fails or is unavailable. |
| `QUELLE_OCR_URL` | None | Optional OCRia-nano-1 (LiteParse) CPU OCR sidecar endpoint (e.g. `http://127.0.0.1:3004/ocr`) used automatically for scanned PDFs that have no embedded text layer. |
| `QUELLE_HOST_CONCURRENCY` | `2` | Maximum simultaneous requests per target host (1–16). |
| `QUELLE_HOST_SPACING_MS` | `250` | Minimum delay in milliseconds between requests to the same host (0–10000). |
| `QUELLE_RENDER_CONCURRENCY` | `4` | Maximum simultaneous browser renders (Lightpanda or sidecar, 1–16). |
| `QUELLE_CDP_POOL` | `true` | Keep a supervised persistent Lightpanda CDP server over a private Unix socket with asset blocking (`false` falls back to one-shot `lightpanda fetch`). |
| `QUELLE_PDF_MAX_PAGES` | `150` | Default maximum pages extracted per PDF document (1–1,000; overridable per request via `maxPages`). |
| `QUELLE_STATE_DIR` | None | Optional directory to persist incremental crawl snapshots (`sinceCrawlId`) across restarts. |
| `QUELLE_SEARXNG_URL` | None | Self-hosted SearXNG base URL when `searxng` is enabled in `QUELLE_SEARCH_ENGINES`. |

### Optional sidecars (`QUELLE_BROWSER_URL` & `QUELLE_OCR_URL`)

- **Playwright/Chromium sidecar (`browser-sidecar/`):** For heavy SPAs that Lightpanda cannot render, using a realistic desktop Chrome profile, smooth scrolling, WebRTC lockdown, and an embedded DNS-pinning proxy that blocks private/reserved ranges and `QUELLE_BLOCKED_NETWORKS`.
- **OCRia-nano-1 sidecar (`ocr-sidecar/`):** Runs LlamaIndex's 8.1M-parameter `ocria-nano` CPU model via LiteParse (`@llamaindex/liteparse`) for scanned PDFs without an embedded text layer.

```sh
# Playwright sidecar only:
QUELLE_BROWSER_URL=http://browser-sidecar:3003/scrape \
  docker compose --profile browser up -d --build

# OCRia-nano-1 sidecar only:
QUELLE_OCR_URL=http://ocr-sidecar:3004/ocr \
  docker compose --profile ocr up -d --build

# Full stack (Quelle + Playwright + OCRia-nano-1):
QUELLE_BROWSER_URL=http://browser-sidecar:3003/scrape \
QUELLE_OCR_URL=http://ocr-sidecar:3004/ocr \
  docker compose --profile full up -d --build
```

## Deployment hardening

Quelle needs no writable file system, no Linux capabilities and no privilege escalation. The Docker example above sets these restrictions. In Kubernetes, use the equivalent container `securityContext`:

```yaml
securityContext:
  runAsNonRoot: true
  readOnlyRootFilesystem: true
  allowPrivilegeEscalation: false
  capabilities:
    drop: ["ALL"]
```

Quelle fetches URLs chosen by callers and search results, so treat its network position as part of its security. Private ranges such as `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `100.64.0.0/10` and IPv6 unique local addresses are always refused. If a cluster's pod, service or internal load-balancer ranges use other addresses, for example publicly routable IPv6 pod networks or reused public IPv4 ranges, list them in `QUELLE_BLOCKED_NETWORKS`. As a second layer, give the pod an egress NetworkPolicy that allows DNS, the model endpoint and the internet but excludes the cluster's own ranges.

## Third-party software in the image

- [Lightpanda](https://github.com/lightpanda-io/browser) 1.0.5, AGPL-3.0, shipped unmodified as a separate program. Its licence is at `/usr/share/doc/lightpanda/LICENSE` in the image and its source at the linked repository. Quelle runs it as a child process and does not link it.
- Poppler (`pdftotext`), GPL, installed from Debian packages.
