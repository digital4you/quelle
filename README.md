# Quelle

Quelle is a Rust service that retrieves web pages for agents. It can answer a question with citations or return readable page text for your own model.

| Mode | Endpoint | Returns | Needs a model |
| --- | --- | --- | --- |
| Answer | `POST /api/answer` | An answer with numbered citations and supporting passages | Yes |
| Content | `POST /api/content` | Page text in document order for the caller's exact search query or supplied URLs | No |

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
| `limit` | No | Integer from 1 to 20, default 20. Caps the number of distinct search results or supplied URLs to read. |
| `language` | No | With `query` only: `de`, `fr`, `it` or `en`. The language hint for engines that take one, such as Brave or Wikipedia. Without it, Quelle guesses from common words and spelling and falls back to `de`. |

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

Quelle preserves case, whitespace, quoted phrases and search operators outside privacy redactions. Each engine interprets its own query syntax. For Brave, Quelle also disables spellcheck. Quelle reads distinct public URLs in the successful engine's result order, up to four at a time.

Read `sources[].content` for the page text and `sources[].metadata.url` for the final URL. Each source also has `metadata.title` and `metadata.truncated`. When the final URL differs, `requestedUrl` records the original. Successful sources follow input or search-result order. Individual failures appear in `errors` as objects with `url` and `error`. Partial success returns HTTP 200. If every read fails or discovery finds nothing, HTTP 422 includes `error: "no_readable_sources"` alongside both arrays.

### Page extraction

- **HTML.** `/api/content` keeps readable text in document order, including headings, repeated values and footer contacts. It leaves out site navigation, search forms, the site banner, language switchers and skip links. A page that consists only of navigation keeps it. It extracts `mailto:` and `tel:` targets from direct links, simple JavaScript contact functions and links in the rendered page. A handler that needs a click or a follow-up API request may yield only its visible label.
- **JavaScript.** When static HTML looks like a script shell or an unrendered template, Quelle tries Lightpanda and reads the resulting DOM. If the page needs rendering and the browser is unavailable, its error code is `needs_js`.
- **Documents.** Plain text retains line breaks. PDFs return their embedded text layer, up to 150 pages. Word, Excel, PowerPoint, OpenDocument, RTF, EPUB and CSV files are converted to Markdown with [anydoc](https://github.com/firecrawl/anydoc).

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

Set `QUELLE_SEARCH_ENGINES` to an ordered, comma-separated list. For example, `QUELLE_SEARCH_ENGINES=mwmbl,wikipedia,brave` enables two keyless providers and Brave. With the variable unset, Quelle uses `brave` alone when a Brave key is present and `mwmbl` otherwise. Anonymous Mwmbl access allows about one search per second, so add a keyed engine for more traffic. Use `none` to disable discovery and read supplied URLs directly. Requests requiring discovery then return `503 discovery_not_configured`.

| Engine | What it searches | Credential |
| --- | --- | --- |
| `brave` | Brave Web Search API | `BRAVE_SEARCH_API_KEY` required |
| `bing` | Bing web results, parsed from the result page. Not enabled by default; check Bing's terms of use before enabling it. | None |
| `mwmbl` | Mwmbl web index (JSON API) | Anonymous or optional `MWMBL_API_KEY` |
| `mojeek` | Mojeek web index (JSON API) | `MOJEEK_API_KEY` required |
| `duckduckgo` | DuckDuckGo HTML web results | None |
| `wikipedia` | Wikipedia articles in the query language (German, French, Italian or English), from the planner or the `language` field, or detected | None |

On each uncached query, Quelle rotates the starting engine by one position. It tries the next configured engine on an empty result, timeout, rate limit, challenge, or upstream error, and stops at the first usable result list. For example, with `mwmbl,wikipedia,brave`, a query starting at Wikipedia can fall back to Brave, then Mwmbl. Every listed engine can take a turn as the first choice. Nonempty results are cached for 15 minutes. Page fetch failures are reported separately.

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
