# Quelle

Quelle is a Rust service that retrieves web pages for agents. It can answer a question with citations or return readable page text for your own model.

| Mode | Endpoint | Returns | Needs a model |
| --- | --- | --- | --- |
| Answer | `POST /api/answer` | An answer with numbered citations and supporting passages | Yes |
| Content | `POST /api/content` | Page text in document order for the caller's exact search query or supplied URLs | No |

## Quick start

Copy [.env.example](.env.example) to `.env`. Set `BRAVE_SEARCH_API_KEY` and a separate `QUELLE_TOKEN` of at least 32 characters. You can generate the token with `openssl rand -hex 32`. These two settings enable `/api/content`. To enable `/api/answer`, also fill in the three model variables in the same file before starting Quelle.

```sh
docker run -d --name quelle --env-file .env \
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
| `query` | One of `query` or `urls` | Nonblank string containing the finished Brave search query, up to 600 characters and 75 words. |
| `urls` | One of `query` or `urls` | Array of 1–20 public HTTP(S) URL strings on standard ports to read directly. |
| `limit` | No | Integer from 1 to 20, default 20. Caps the results requested from Brave or the distinct supplied URLs to read. |

To fetch known pages directly, pass `urls`. This makes zero Brave or model calls.

```sh
curl -sS http://127.0.0.1:3000/api/content \
  -H "Authorization: Bearer $QUELLE_TOKEN" -H 'Content-Type: application/json' \
  --data '{"urls":["https://www.rfc-editor.org/rfc/rfc2606.txt"]}'
```

To discover pages, pass the exact query you want Brave to run.

```sh
curl -sS http://127.0.0.1:3000/api/content \
  -H "Authorization: Bearer $QUELLE_TOKEN" -H 'Content-Type: application/json' \
  --data '{"query":"site:iana.org \"example domains\"","limit":3}'
```

Quelle disables spellcheck and preserves case, whitespace, quoted phrases and search operators outside privacy redactions. Each cache miss uses one Brave lookup. Quelle reads distinct public URLs in result order, up to four at a time.

Read `sources[].content` for the page text and `sources[].metadata.url` for the final URL. Each source also has `metadata.title` and `metadata.truncated`. When the final URL differs, `requestedUrl` records the original. Successful sources follow input or search-result order. Individual failures appear in `errors` as objects with `url` and `error`. Partial success returns HTTP 200. If every read fails or discovery finds nothing, HTTP 422 includes `error: "no_readable_sources"` alongside both arrays.

### Page extraction

- **HTML.** `/api/content` keeps readable text in document order, including headings, repeated values and footer contacts. It extracts `mailto:` and `tel:` targets from direct links, simple JavaScript contact functions and links in the rendered page. A handler that needs a click or a follow-up API request may yield only its visible label.
- **JavaScript.** When static HTML looks like a script shell or an unrendered template, Quelle tries Lightpanda and reads the resulting DOM. If the page needs rendering and the browser is unavailable, its error code is `needs_js`.
- **Documents.** Plain text retains line breaks. PDFs return their embedded text layer, up to 150 pages. Word, Excel, PowerPoint, OpenDocument, RTF, EPUB and CSV files are converted to Markdown with [anydoc](https://github.com/firecrawl/anydoc).

The output limit for `/api/content` is 2 MiB of UTF-8 text per source. `metadata.truncated` flags a byte or PDF page limit reached during extraction. A site can still require scrolling, pagination or a click to reveal additional content.

### Search privacy and errors

Before sending a query to Brave, Quelle filters identifiers such as email addresses, phone numbers, IBANs and credentials. A query reduced to fragments can return `422 query_withheld`. Searches use the Swiss country setting and strict Safe Search. Successful nonempty results are cached in memory for 15 minutes. In answer mode the configured model receives the question and supplied conversation context.

Quelle checks page destinations on every redirect and restricts browser requests to public addresses. Diagnostics contain stage timings and counts. Rendering logs include the host name of the page.

Common HTTP errors include `400 invalid_search_request` for invalid input, `401 backend_authentication_required` for a missing or incorrect bearer token, `503 search_capacity` when all request slots are busy, and `503 discovery_rate_limited` when Brave rate-limits discovery. The JSON response includes a machine-readable `error` field.

## Configuration

### Required at startup

| Variable | Purpose |
| --- | --- |
| `QUELLE_TOKEN` | Bearer token clients send to Quelle. Use at least 32 printable, non-space ASCII characters. |
| `BRAVE_SEARCH_API_KEY` | Brave Search API key. Required at startup even for deployments that only read supplied URLs. |

### Required only for `/api/answer`

Set all three connection variables together to enable answer generation. Omit all three for content-only use. A partial configuration stops startup. Use separate credentials for Quelle, Brave, and the model provider.

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
| `QUELLE_LISTEN` | `127.0.0.1:3000` natively, `0.0.0.0:3000` in Docker | Address and port Quelle binds to. |
| `QUELLE_CONCURRENCY` | `8` | Maximum simultaneous search requests, from 1 to 64. |
| `QUELLE_USER_AGENT` | Built-in desktop Chrome user agent | User agent for ordinary page fetches. |
| `QUELLE_LIGHTPANDA` | `lightpanda` on `PATH` | Path to the browser executable for JavaScript rendering. |

## Third-party software in the image

- [Lightpanda](https://github.com/lightpanda-io/browser) 1.0.5, AGPL-3.0, shipped unmodified as a separate program. Its licence is at `/usr/share/doc/lightpanda/LICENSE` in the image and its source at the linked repository. Quelle runs it as a child process and does not link it.
- Poppler (`pdftotext`), GPL, installed from Debian packages.
