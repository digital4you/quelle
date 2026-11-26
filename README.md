# Quelle

Quelle is a small web search service written in Rust. It serves the search route of the Mate assistant and has two modes:

| Mode | Endpoint | Returns | Needs a model |
| --- | --- | --- | --- |
| Answer | `POST /api/search` | An answer with numbered citations and the sources behind it | Yes |
| Results | `POST /api/sources` | Every page that was read, with its best passages, plus every other search hit | No (better with one) |

It needs a Brave Search API key. The answer mode also needs an OpenAI-compatible model endpoint, for example vLLM. Without a model Quelle still starts; `/api/search` then answers `503` with `model_not_configured` before making any search call, and `/api/sources` keeps working.

## How it works

Both modes share steps 1 to 5. The answer mode adds step 6.

1. **Plan.** The model turns the request into one to three short search queries in the likely source language (de, fr, it or en), plus evidence terms: synonyms and word forms an answering passage is likely to contain ("Erstbesteigung" for "erstmals bestiegen"). Evidence terms only rank passages and are never searched for. Conversation history only resolves references. Without a model, the request text itself is the query.
2. **Filter.** Before anything leaves the service, each query passes a privacy filter that removes email addresses, phone numbers, AHV numbers, IBANs, card numbers, API keys, tokens and passwords. A query reduced to fragments is dropped.
3. **Discover.** Each query goes to Brave once (Swiss locale, strict safe search, 20 results). Brave bills per request, so results are cached in memory for 15 minutes.
4. **Rank.** The model orders the results by title and snippet so the page budget goes to likely primary sources. Without a model the results keep a host-diverse order. Snippets only choose pages; they are never cited.
5. **Read.** Pages are fetched with SSRF protection (public addresses only, checked on every redirect), size and time limits, and a desktop browser user agent (Wikimedia sites get an honest one, as their policy asks).
   - HTML is reduced to its largest content region, or the whole page when that region is only a sidebar or teaser list; cookie and JavaScript notices are dropped.
   - PDFs are read through their text layer with `pdftotext` (first 150 pages), which covers born-digital documents and OCR'd scans.
   - Word, Excel, PowerPoint, OpenDocument, RTF, EPUB and CSV files are converted with [anydoc](https://github.com/firecrawl/anydoc) (MIT), which limits archive entries, XML depth and table expansion. Table rows are labelled with their column headers and the nearest heading, so a figure keeps its meaning ("2020 — Kanton: Zürich; Bevölkerungsstand am 31. Dezember: 1 129 722").
   - Pages that only render with JavaScript (script shells, "JavaScript required" notices, unrendered AngularJS or Vue templates, or almost no readable text) are rendered with [Lightpanda](https://github.com/lightpanda-io/browser) and read the same way. Lightpanda runs as a separate process per page, at most two at a time, with private and reserved networks blocked for every request the page makes, telemetry off, and memory, size and time limits. It identifies itself honestly as Lightpanda with a Quelle suffix. Without Lightpanda such pages are reported as `needs_js` and skipped.
   - Passages are ranked by query and evidence terms weighted by rarity on the page, with a boost for figures and dates when the question asks for one. The best ones are kept up to 6,500 characters per page.
6. **Write** (answer mode). The model answers from the passages with numbered citations. Citations that point at no source are removed, and an answer without a valid citation is rejected. Planner and writer each get one corrected retry, since model calls are cheap compared with search calls.

Logs and diagnostics hold timings and counts, never query text, answers or URLs. The one exception is the host name (no path) of pages that needed JavaScript rendering, logged as `stage="render"` or `stage="needs_js"`, so it is visible which sites would profit from a site adapter.

## API

All `POST` endpoints require `Authorization: Bearer $QUELLE_TOKEN` and take the same JSON body:

| Field | Meaning |
| --- | --- |
| `query` | The request (required) |
| `history` | Earlier turns as `[["human" or "assistant", "text"], …]`, at most 10 |
| `optimizationMode` | `speed` (default), `balanced` or `quality`: more queries, pages and time |
| `sources` | Focus: `web`, `academic` or `discussions` |
| `systemInstructions` | Instructions for the writer |
| `stream` | Answer mode only: server-sent events instead of one response |
| `mateContext` | Structured context from Mate's backend (`version: "mate-search-v1"`, original request, server and client instructions, answer language, public affairs, identity) |

### Answer mode: `POST /api/search`

```sh
curl -sS http://127.0.0.1:3000/api/search \
  -H "Authorization: Bearer $QUELLE_TOKEN" -H 'Content-Type: application/json' \
  --data '{"query":"Where does IANA describe example domains?","optimizationMode":"speed"}'
```

The response holds `message` and `sources` (each with `content` and `metadata.title`, `metadata.url`, and `metadata.retrieval` for `pdf`, `document` or `rendered` pages). With `"stream": true` the same arrives as server-sent `sources`, `response` and `done` or `error` events. Requests with `mateContext` also receive `diagnostics`.

### Results mode: `POST /api/sources`

```sh
curl -sS http://127.0.0.1:3000/api/sources \
  -H "Authorization: Bearer $QUELLE_TOKEN" -H 'Content-Type: application/json' \
  --data '{"query":"Wann wurde das Matterhorn erstmals bestiegen?"}'
```

```json
{
  "queries": ["Matterhorn Erstbesteigung"],
  "sources": [{ "content": "…best passages…", "metadata": { "title": "Matterhorn – Wikipedia", "url": "https://de.wikipedia.org/wiki/Matterhorn" } }],
  "hits": [{ "title": "…", "url": "…", "snippet": "…" }],
  "diagnostics": { "pages": 6, "candidates": 15, "totalMs": 1138 }
}
```

- `queries`: the search queries used, after the privacy filter.
- `sources`: every page that was read, with its selected passages, as in the answer mode.
- `hits`: every other search result in ranked order, with the engine's `title`, `url` and `snippet`. They were not read or verified.

No answer is written, so this mode is faster and costs no writer call. If no page could be read but there are hits, the response is still `200` with empty `sources`. `stream` is ignored.

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
