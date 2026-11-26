# Quelle

Quelle is a web search service written in Rust. Choose the endpoint by the output your caller needs.

| Mode | Endpoint | Returns | Needs a model |
| --- | --- | --- | --- |
| Answer | `POST /api/search/answer` | An answer with numbered citations and supporting passages | Yes |
| Content | `POST /api/search/content` | Page text in document order for the caller's exact search query or supplied URLs | No |

Configure a Brave Search API key at startup. Answer mode also needs a model with chat completions and a vLLM-compatible tokenizer. Without a model, content retrieval works and answer requests return `503 model_not_configured` before making a search call.

When upgrading, move answer calls from `/api/search` to `/api/search/answer`. Replace `/api/sources` calls with `/api/search/content`, sending the finished search query and reading page text from `sources[].content`.

## How it works

Answer mode runs the following six steps. Content mode uses the same search provider, fetch protections and document readers. Its request and output are described below.

1. **Plan.** The model turns the request into one to three short search queries in the likely source language (de, fr, it or en), plus evidence terms such as "Erstbesteigung" for "erstmals bestiegen". These terms help rank passages within a page. Conversation history resolves references.
2. **Filter.** Before Quelle sends a query to Brave, a privacy filter removes email addresses, phone numbers, AHV numbers, IBANs, card numbers, API keys, tokens and passwords. A query reduced to fragments is dropped.
3. **Discover.** Each query goes to Brave once (Swiss locale, strict safe search, 20 results). Brave bills per request, so results are cached in memory for 15 minutes.
4. **Rank.** The model orders the results by title and snippet so the page budget goes to likely primary sources. If reranking fails, Quelle uses a host-diverse order. The writer receives passages from the fetched pages.
5. **Read.** Pages are fetched with SSRF protection (public addresses only, checked on every redirect), size and time limits, and a desktop browser user agent (Wikimedia sites get an honest one, as their policy asks).
   - HTML is reduced to its largest content region, or the whole page when that region is only a sidebar or teaser list; cookie and JavaScript notices are dropped.
   - PDFs are read through their text layer with `pdftotext` (first 150 pages), which covers born-digital documents and OCR'd scans.
   - Word, Excel, PowerPoint, OpenDocument, RTF, EPUB and CSV files are converted with [anydoc](https://github.com/firecrawl/anydoc) (MIT), which limits archive entries, XML depth and table expansion. Table rows are labelled with their column headers and the nearest heading, so a figure keeps its meaning ("2020 — Kanton: Zürich; Bevölkerungsstand am 31. Dezember: 1 129 722").
   - Pages that only render with JavaScript (script shells, "JavaScript required" notices, unrendered AngularJS or Vue templates, or almost no readable text) are rendered with [Lightpanda](https://github.com/lightpanda-io/browser) and read the same way. Lightpanda runs as a separate process per page, at most two at a time, with private and reserved networks blocked for every request the page makes, telemetry off, and memory, size and time limits. It identifies itself honestly as Lightpanda with a Quelle suffix. Without Lightpanda such pages are reported as `needs_js` and skipped.
   - Passages are ranked by query and evidence terms weighted by rarity on the page, with a boost for figures and dates when the question asks for one. The best ones are kept up to 6,500 characters per page.
6. **Write** (answer mode). The model answers from the passages with numbered citations. Citations that point at no source are removed, and an answer without a valid citation is rejected. Planner and writer each get one corrected retry, since model calls are cheap compared with search calls.

Logs and diagnostics hold timings and counts, never query text, answers or URLs. The one exception is the host name (no path) of pages that needed JavaScript rendering, logged as `stage="render"` or `stage="needs_js"`, so it is visible which sites would profit from a site adapter.

## API

Both search endpoints require `Authorization: Bearer $QUELLE_TOKEN`. Answer requests accept the following JSON fields.

| Field | Meaning |
| --- | --- |
| `query` | The request (required) |
| `history` | Earlier turns as `[["human" or "assistant", "text"], …]`, at most 10 |
| `optimizationMode` | `speed` (default), `balanced` or `quality`: more queries, pages and time |
| `sources` | Focus: `web`, `academic` or `discussions` |
| `systemInstructions` | Instructions for the writer |
| `stream` | Answer mode only: server-sent events instead of one response |
| `mateContext` | Structured context from Mate's backend (`version: "mate-search-v1"`, original request, server and client instructions, answer language, public affairs, identity) |

### Answer mode · `POST /api/search/answer`

```sh
curl -sS http://127.0.0.1:3000/api/search/answer \
  -H "Authorization: Bearer $QUELLE_TOKEN" -H 'Content-Type: application/json' \
  --data '{"query":"Where does IANA describe example domains?","optimizationMode":"speed"}'
```

The response holds `message` and `sources` (each with `content` and `metadata.title`, `metadata.url`, and `metadata.retrieval` for `pdf`, `document` or `rendered` pages). With `"stream": true` the same arrives as server-sent `sources`, `response` and `done` or `error` events. Requests with `mateContext` also receive `diagnostics`.

### Content mode · `POST /api/search/content`

Use this endpoint when the calling agent should choose what goes into its own context. Pass a finished `query` (up to 600 characters and 75 words) or `urls` (1–20 public HTTP(S) URLs). An optional integer `limit` from 1 to 20 sets the number of results to request and read. It defaults to one full results page (20). These are the [Brave Web Search limits](https://api-dashboard.search.brave.com/api-reference/web/search/get).

```sh
curl -sS http://127.0.0.1:3000/api/search/content \
  -H "Authorization: Bearer $QUELLE_TOKEN" -H 'Content-Type: application/json' \
  --data '{"query":"site:iana.org \"example domains\"","limit":3}'
```

Quelle sends the supplied query to Brave with spellcheck disabled, preserving case, whitespace, quoted phrases and search operators outside privacy redactions. Each cache miss uses one lookup. Quelle attempts the distinct public URLs in Brave's result order. With `{"urls":["https://www.rfc-editor.org/rfc/rfc2606.txt"]}`, it reads the supplied URLs directly and spends zero Brave requests. Both forms work without a model. Reads run four at a time.

The response contains `sources` and `errors`. Each source has `content`, `metadata.url`, `metadata.title`, and `metadata.truncated`. Redirected or exported sources also carry the original `requestedUrl`. Sources follow input or search-result order. Errors identify the individual URL and reason. Partial success returns HTTP 200. If every read fails or discovery finds nothing, the endpoint returns HTTP 422 with `no_readable_sources`. An upstream search outage or rate limit returns the corresponding discovery error.

HTML becomes document-order text, including short headings, repeated values and footer contact details. Direct `mailto:` and `tel:` targets, literal concatenations in navigation handlers, simple page-defined contact functions and contact links present in a rendered DOM can be recovered. A handler that depends on a click, an API request or arbitrary application state may yield only its visible label. Plain text retains line breaks, PDFs return their text layer, and office documents return the converter's Markdown.

The output limit is 2 MiB of UTF-8 text per source, matching the HTML download limit. `metadata.truncated` indicates when Quelle cuts text, input HTML or plain text, a rendered DOM, or a PDF at its byte or 150-page limit. It describes Quelle's limits. A site can still require scrolling, pagination, a click or more loading time to reveal additional content.

### Health and errors

- `GET /healthz` returns `{"ok":true}` without spending search or model quota.
- Errors are JSON `{"error": "<code>"}` (plus `diagnostics` where available), for example `401 backend_authentication_required`, `503 search_capacity`, `503 model_not_configured`, `503 discovery_rate_limited`, `422 no_readable_sources`, `422 query_withheld` (the request consisted only of personal identifiers or secrets) and `504 deadline_exceeded`.

## Configuration

| Variable | Purpose |
| --- | --- |
| `QUELLE_TOKEN` | Bearer token callers must send, at least 32 characters |
| `BRAVE_SEARCH_API_KEY` | Brave Search API key |
| `QUELLE_MODEL_BASE_URL` | OpenAI-compatible base URL ending in `/v1` (HTTPS, or HTTP on loopback) |
| `QUELLE_MODEL_API_KEY` | Model API key |
| `QUELLE_MODEL_NAME` | Model name at that endpoint |
| `QUELLE_MODEL_CONTEXT_TOKENS` | Model context ceiling, default 64000 |
| `QUELLE_LISTEN` | Listen address, default `127.0.0.1:3000` (`0.0.0.0:3000` in the image) |
| `QUELLE_CONCURRENCY` | Maximum parallel requests, default 8 |
| `QUELLE_USER_AGENT` | User agent for page fetches, default a current desktop Chrome |
| `QUELLE_LIGHTPANDA` | Path to the Lightpanda binary, default `lightpanda` on the `PATH` |

The three `QUELLE_MODEL_*` connection variables are set together or not at all; setting only some of them stops startup. All credentials must differ.

## Run

```sh
cargo run --release      # pdftotext (poppler-utils) for PDFs, lightpanda for script-only pages
docker run -p 3000:3000 --env-file .env ghcr.io/digital4you/quelle:master
```

The image is built by GitHub Actions on every push to `master` and on `v*` tags. It is a static musl binary on Debian slim with `pdftotext` and Lightpanda.

## Third-party software in the image

- [Lightpanda](https://github.com/lightpanda-io/browser) 1.0.5, AGPL-3.0, shipped unmodified as a separate program. Its licence is at `/usr/share/doc/lightpanda/LICENSE` in the image and its source at the linked repository. Quelle runs it as a child process and does not link it.
- Poppler (`pdftotext`), GPL, installed from Debian packages.
