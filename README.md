# Quelle

Quelle is a small web search service that answers a question with cited sources. It is written in Rust and serves the search route of the Mate assistant. It needs a Brave Search API key and an OpenAI-compatible model endpoint (for example vLLM).

## How it works

1. **Plan.** The model turns the request into one to three short search queries in the likely source language (de, fr, it or en). Conversation history only resolves references.
2. **Filter.** Before anything leaves the service, each query passes a privacy filter that removes email addresses, phone numbers, AHV numbers, IBANs, card numbers, API keys, tokens and passwords. A query reduced to fragments is dropped.
3. **Discover.** Each query goes to Brave once (Swiss locale, strict safe search, 20 results). Brave bills per request, so results are cached in memory for 15 minutes.
4. **Rank.** The model orders the results by title and snippet so the page budget goes to likely primary sources. Snippets only choose pages; they are never cited.
5. **Read.** Pages are fetched with SSRF protection (public addresses only, checked on every redirect), size and time limits, and a desktop browser user agent (Wikimedia sites get an honest one, as their policy asks). HTML is reduced to its largest content region, or the whole page when that region is only a sidebar or teaser list; cookie and JavaScript notices are dropped. PDFs are read through their text layer with `pdftotext` (first 150 pages), which covers born-digital documents and OCR'd scans. Word, Excel, PowerPoint, OpenDocument, RTF, EPUB and CSV files are converted with [anydoc](https://github.com/firecrawl/anydoc) (MIT), which limits archive entries, XML depth and table expansion; table rows are labelled with their column headers and the nearest heading, so a figure keeps its meaning ("2020 — Kanton: Zürich; Bevölkerungsstand am 31. Dezember: 1 129 722"). Passages are ranked by query terms weighted by rarity on the page, with a boost for figures and dates when the question asks for one, and the best ones are kept up to 6,500 characters per page. Pages that only render with JavaScript (script shells, "JavaScript required" notices, unrendered AngularJS or Vue templates, or almost no readable text) are rendered with [Lightpanda](https://github.com/lightpanda-io/browser) and read the same way. Lightpanda runs as a separate process per page, at most two at a time, with private and reserved networks blocked for every request the page makes, telemetry off, and memory, size and time limits. It identifies itself honestly as Lightpanda with a Quelle suffix. Without Lightpanda such pages are reported as `needs_js` and skipped.
6. **Write.** The model answers from the passages with numbered citations. Citations that point at no source are removed, and an answer without a valid citation is rejected. Planner and writer each get one corrected retry, since model calls are cheap compared with search calls.

Logs and diagnostics hold timings and counts, never query text, answers or URLs.

## API

- `GET /healthz`: `{"ok":true}`.
- `POST /api/search` with `Authorization: Bearer $QUELLE_TOKEN`:

```sh
curl -sS http://127.0.0.1:3000/api/search \
  -H "Authorization: Bearer $QUELLE_TOKEN" -H 'Content-Type: application/json' \
  --data '{"query":"Where does IANA describe example domains?","optimizationMode":"speed"}'
```

The response holds `message` and `sources` (each with `content` and `metadata.title`, `metadata.url`). With `"stream": true` the same arrives as server-sent `sources`, `response` and `done` or `error` events. `optimizationMode` is `speed`, `balanced` or `quality`.

## Configuration

| Variable | Purpose |
| --- | --- |
| `QUELLE_TOKEN` | Bearer token callers must send, at least 32 characters |
| `BRAVE_SEARCH_API_KEY` | Brave Search API key |
| `QUELLE_MODEL_BASE_URL` | OpenAI-compatible base URL ending in `/v1` (HTTPS, or HTTP on loopback) |
| `QUELLE_MODEL_API_KEY` | Model API key |
| `QUELLE_MODEL_NAME` | Model name at that endpoint |
| `QUELLE_LISTEN` | Listen address, default `127.0.0.1:3000` (`0.0.0.0:3000` in the image) |
| `QUELLE_CONCURRENCY` | Maximum parallel searches, default 8 |
| `QUELLE_MODEL_CONTEXT_TOKENS` | Model context ceiling, default 64000 |
| `QUELLE_USER_AGENT` | User agent for page fetches, default a current desktop Chrome |
| `QUELLE_LIGHTPANDA` | Path to the Lightpanda binary, default `lightpanda` on the `PATH` |

The three credentials must differ.

## Run

```sh
cargo run --release      # pdftotext (poppler-utils) for PDFs, lightpanda for script-only pages
docker run -p 3000:3000 --env-file .env ghcr.io/digital4you/quelle:master
```

The image is built by GitHub Actions on every push to `master` and on `v*` tags. It is a static musl binary on Debian slim with `pdftotext` and Lightpanda.

## Third-party software in the image

- [Lightpanda](https://github.com/lightpanda-io/browser) 1.0.5, AGPL-3.0, shipped unmodified as a separate program. Its licence is at `/usr/share/doc/lightpanda/LICENSE` in the image and its source at the linked repository. Quelle runs it as a child process and does not link it.
- Poppler (`pdftotext`), GPL, installed from Debian packages.
