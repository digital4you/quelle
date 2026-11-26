# Quelle

Quelle is a web search service written in Rust. Choose the endpoint by the output your caller needs.

| Mode | Endpoint | Returns | Needs a model |
| --- | --- | --- | --- |
| Answer | `POST /answer` | An answer with numbered citations and supporting passages | Yes |
| Content | `POST /content` | Page text in document order for the caller's exact search query or supplied URLs | No |

Configure a Brave Search API key at startup. Answer mode also needs a model with chat completions and a vLLM-compatible tokenizer. Without a model, content retrieval works and answer requests return `503 model_not_configured` before making a search call.

When upgrading, move calls from `/api/search/answer` (formerly `/api/search`) to `/answer`, and from `/api/search/content` to `/content`. Clients migrating from the older `/api/sources` route should send a finished query to `/content` and read page text from `sources[].content`.

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

Both endpoints accept JSON and require `Authorization: Bearer $QUELLE_TOKEN`.

### Answer mode · `POST /answer`

| Request field | Required | Type and behavior |
| --- | --- | --- |
| `query` | Yes | Nonblank string, up to 12,000 characters. The question or research request. |
| `history` | No | Array of up to 10 `[role, text]` pairs. Roles are `user` (alias `human`) and `assistant`. Limit of 4,000 characters per turn and 12,000 across all turns. |
| `optimizationMode` | No | String. Choose `speed` (default), `balanced`, or `quality` to set the search and answer budgets. |
| `sources` | No | Array of up to three search focuses drawn from `web`, `academic`, and `discussions`. |
| `systemInstructions` | No | String, up to 64,000 characters. Supplies writer instructions when `mateContext` is absent. |
| `stream` | No | Boolean, default `false`. Set to `true` for server-sent events. |
| `mateContext` | No | Object from Mate's backend. Requires `version` set to `"mate-search-v1"` and a nonblank `originalRequest`. Optional fields are `serverInstructions`, `clientInstructions`, `answerLanguage`, `publicAffairs`, `contextual`, and `identity`. |

```sh
curl -sS http://127.0.0.1:3000/answer \
  -H "Authorization: Bearer $QUELLE_TOKEN" -H 'Content-Type: application/json' \
  --data '{"query":"Where does IANA describe example domains?","optimizationMode":"speed"}'
```

The response holds `message` and `sources` (each with `content` and `metadata.title`, `metadata.url`, and `metadata.retrieval` for `pdf`, `document` or `rendered` pages). With `"stream": true` the same arrives as server-sent `sources`, `response` and `done` or `error` events. Non-streaming requests with `mateContext` also receive `diagnostics`.

### Content mode · `POST /content`

Supply exactly one of `query` and `urls`. The endpoint rejects unknown request fields with HTTP 400.

| Request field | Required | Type and behavior |
| --- | --- | --- |
| `query` | One of `query` or `urls` | Nonblank string containing the finished Brave search query, up to 600 characters and 75 words. |
| `urls` | One of `query` or `urls` | Array of 1–20 public HTTP(S) URL strings to read directly. |
| `limit` | No | Integer from 1 to 20, default 20. Caps the results requested from Brave or the distinct supplied URLs to read. |

```sh
curl -sS http://127.0.0.1:3000/content \
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

For a content-only deployment, set `QUELLE_TOKEN` and `BRAVE_SEARCH_API_KEY`. To use Docker's `--env-file .env`, copy [.env.example](.env.example) to `.env` and fill in those two values. For a native process, export the variables in its environment.

### Required at startup

| Variable | Purpose |
| --- | --- |
| `QUELLE_TOKEN` | Bearer token clients send to Quelle. Use at least 32 printable, non-space ASCII characters. |
| `BRAVE_SEARCH_API_KEY` | Key Quelle sends to the Brave Search API. |

### Required only for `/answer`

Set all three connection variables together to enable answer generation. Omit all three for content-only use. A partial configuration stops startup. Use separate credentials for Quelle, Brave, and the model provider.

| Variable | Purpose |
| --- | --- |
| `QUELLE_MODEL_BASE_URL` | OpenAI-compatible model API base URL, such as `https://model.example.org/v1`. The server must support chat completions and a vLLM-compatible tokenizer. Plain HTTP is accepted on loopback. |
| `QUELLE_MODEL_API_KEY` | Bearer key for the model provider. |
| `QUELLE_MODEL_NAME` | Model ID served by that endpoint. |

### Optional settings

| Variable | Default | Purpose |
| --- | --- | --- |
| `QUELLE_MODEL_CONTEXT_TOKENS` | `64000` | Context ceiling for answer mode, from 2,048 to 64,000 tokens. |
| `BRAVE_SEARCH_URL` | `https://api.search.brave.com/res/v1/web/search` | Override the Brave-compatible web search endpoint. Plain HTTP is accepted on loopback. |
| `QUELLE_LISTEN` | `127.0.0.1:3000` natively, `0.0.0.0:3000` in Docker | Address and port Quelle binds to. |
| `QUELLE_CONCURRENCY` | `8` | Maximum simultaneous search requests, from 1 to 64. |
| `QUELLE_USER_AGENT` | Built-in desktop Chrome user agent | User agent for ordinary page fetches. |
| `QUELLE_LIGHTPANDA` | `lightpanda` on `PATH` | Path to the browser executable for JavaScript rendering. |

## Run

```sh
cargo run --release      # pdftotext (poppler-utils) for PDFs, lightpanda for script-only pages
docker run -p 3000:3000 --env-file .env ghcr.io/digital4you/quelle:master
```

The image is built by GitHub Actions on every push to `master` and on `v*` tags. It is a static musl binary on Debian slim with `pdftotext` and Lightpanda.

## Third-party software in the image

- [Lightpanda](https://github.com/lightpanda-io/browser) 1.0.5, AGPL-3.0, shipped unmodified as a separate program. Its licence is at `/usr/share/doc/lightpanda/LICENSE` in the image and its source at the linked repository. Quelle runs it as a child process and does not link it.
- Poppler (`pdftotext`), GPL, installed from Debian packages.
