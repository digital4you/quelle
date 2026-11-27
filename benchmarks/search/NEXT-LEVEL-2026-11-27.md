# Quelle Next-Level Capabilities Benchmark (2026-11-27)

Validation and paired benchmarks covering six new retrieval capabilities added to Quelle:

1. **Persistent Lightpanda CDP Pool + Asset Blocking** (`src/pages/cdp.rs`, `QUELLE_CDP_POOL=true` default): supervised `lightpanda serve` over a private `0600` Unix socket, minimal zero-crate RFC 6455 WebSocket client, per-render isolated `BrowserContext`/`Target`, and `--block-urls` filtering for raster images, fonts, audio/video streams, and ad/analytics beacons (with automatic restart and one-shot `lightpanda fetch` fallback).
2. **Adaptive Render Wait** (`src/pages/cdp.rs`, `src/pages/readiness-init.js`): post-navigation readiness timer (slow origin TTFB no longer starves hydration) with activity-aware extension from the 7 s base wait up to 13 s while one-shot timers, in-flight `fetch`/XHR requests, or DOM mutations remain active; idle pages still stop at 7 s and fast pages still settle in 80 ms / 350 ms.
3. **Token-Budgeted, Section-Anchored Chunking & Query-Focused Distillation** (`src/pages/chunks.rs`): opt-in `chunks` format, `maxTokens`, `chunkTokens`, and `focusQuery` across `/api/scrape`, `/api/content`, and `/api/crawl`. Preserves heading breadcrumbs, exact DOM `#id` deep-link URLs, whole fenced code blocks, and whole Markdown pipe tables, restoring document order after IDF/heading-boosted selection.
4. **Zero-LLM Deterministic Structured Extraction** (`src/pages/structured.rs`): opt-in `structured` format on `/api/scrape` / `/api/crawl` and `structured: true` on `/api/content`, returning single-pass `outline` (with `#id` URLs), real data `tables` (with `colspan`/`rowspan` expansion and layout-table rejection), and `jsonLd` payloads.
5. **Layout-Aware PDF & Configurable Page Cap** (`src/pages.rs`, `QUELLE_PDF_MAX_PAGES`, per-request `maxPages` 1–1,000): detects fragmented tabular/multi-column `pdftotext` output and automatically retries with `pdftotext -layout` (`retrieval: "pdf-layout"`), formatting aligned columns into Markdown tables; flags garbled CID/replacement text layers for automatic `QUELLE_OCR_URL` escalation.
6. **Delta-Aware Incremental Crawling & Section-Level Change Diffs** (`src/crawl.rs`): opt-in `incremental`, `sinceCrawlId`, `previousHashes`, and `omitUnchangedContent` on `/api/crawl`, emitting deterministic `sha256:` `contentHash`, `changeStatus` (`added`/`updated`/`unchanged`), crawl `changes` summary, heading-level `diff`s (`addedSections`, `removedSections`, `changedSections`), and defaulting `maxAge` to `0` for conditional HTTP `304` revalidation.

## Validation

- **Unit tests:** `65 / 65` passed (`cargo test --locked --bin quelle`), written TDD-first before implementation.
- **Browser readiness suite:** `10 / 10` passed (`benchmarks/search/verify-readiness.py` with Lightpanda 1.0.5).
- **Static checks & release build:** `cargo clippy --locked --bin quelle -- -D warnings`, `rustfmt --edition 2024 --check` on edited files, and `cargo build --release --locked` passed cleanly.

## 1. Feature #1 & #2: Persistent CDP Pool + Adaptive Wait Ablation

Raw results: `benchmarks/search/results/2026-11-27-nextlevel-features.json`.

| Target URL | One-Shot `lightpanda fetch` (`QUELLE_CDP_POOL=false`) | Persistent CDP Pool + Adaptive Wait (`QUELLE_CDP_POOL=true`) | Notes |
| --- | --- | --- | --- |
| `https://quotes.toscrape.com/js/` | 1.938 s (1,624 c, OK) | **1.233 s** (1,624 c, OK) | **36.4% faster**, identical text |
| `https://quotes.toscrape.com/js/page/2/` | 1.189 s (3,394 c, OK) | **1.158 s** (3,394 c, OK) | Identical text |
| `https://quotes.toscrape.com/js/page/3/` | 1.215 s (1,798 c, OK) | **1.167 s** (1,798 c, OK) | Identical text |
| `https://quotes.toscrape.com/js-delayed/` (10 s delay) | 7.497 s (`422 needs_js`, 0 c) | **11.156 s** (**200 OK**, 1,624 c, all 10 quotes) | Adaptive wait extends for pending 10 s `setTimeout` |

During the 25-query paired benchmark, all **15 / 15** browser renders ran through the persistent CDP pool with zero fallbacks and zero timeouts (`quotes.toscrape.com`: 672–732 ms, `discuss.kubernetes.io`: 640–714 ms, `redis.io`: 865 ms, `www.estv.admin.ch`: 873–879 ms, `www.zefix.admin.ch`: 1,171 ms, `www.fedlex.admin.ch` 5/5 Angular pages: 2,120–3,678 ms).

## 2. Feature #3: Token-Budgeted Chunking & Query Distillation

| Document | Full Size | Distilled (`maxTokens: 1200`) | Token Reduction | Anchored Chunks | Target Evidence Retained | Latency |
| --- | --- | --- | --- | --- | --- | --- |
| `rfc9110.html` (`Retry-After 503 ...`) | 748,092 c (~187,023 tok) | **1,199 tok** (6 chunks) | **99.4%** | **6 / 6** (`#rfcnum`, etc.) | Yes (`Retry-After`) | 0.468 s |
| `wai-aria-1.2` (`aria-expanded tabpanel ...`) | 725,673 c (~181,418 tok) | **1,198 tok** (8 chunks) | **99.3%** | **8 / 8** (`#x1-5-authoring-practices`, etc.) | Yes (`aria-expanded`) | 0.768 s |

## 3. Feature #4: Zero-LLM Deterministic Structured Extraction

| Target URL | Latency | Outline Headings (Anchored) | Data Tables Extracted | JSON-LD Blocks |
| --- | --- | --- | --- | --- |
| ECB Key Interest Rates | **0.796 s** | 3 (1 anchored) | **1 table (72 rows, 6 expanded headers)** | 0 |
| Wikipedia `List_of_HTTP_status_codes` | **0.999 s** | **26 (26/26 anchored)** | 0 | **1** |
| Hacker News (`news.ycombinator.com`) | **1.683 s** | 0 | **0** (layout table cleanly rejected) | 0 |

## 4. Feature #5: Layout-Aware PDF & Configurable `maxPages`

Target: `https://www.rfc-editor.org/rfc/rfc9110.pdf` (194 pages).

| Mode | Pages Read | Truncated | Chars | Retrieval | Latency |
| --- | --- | --- | --- | --- | --- |
| Default (`maxPages` unset, 150 cap) | 150 | `true` | 374,731 | `pdf-layout` | **1.112 s** |
| `maxPages: 250` | **194 / 194** | **`false`** | **462,331** | `pdf-layout` | **1.121 s** |
| Self-hosted Firecrawl (`hard03`, 25 s timeout) | 0 (HTTP 500) | — | 0 | Failed ("needs >= 35 s timeout") | 7.621 s |

## 5. Feature #6: Delta-Aware Incremental Crawling

Target: `https://books.toscrape.com/` (`maxDepth: 1`, `limit: 5`, `sync: true`).

| Pass | Mode | Latency | Changes (`added / updated / unchanged`) | Payload Bytes | Payload Reduction |
| --- | --- | --- | --- | --- | --- |
| Pass 1 | Initial (`incremental: true`) | 2.787 s | `4 / 0 / 0` | 30,249 B | — |
| Pass 2 | `sinceCrawlId` + `omitUnchangedContent: true` | **1.698 s** (-39%) | **`0 / 0 / 4`** | **1,558 B** | **94.8%** |
| Pass 3 | Stateless `previousHashes` (1 hash mutated) | **1.690 s** | **`0 / 1 / 3`** | — | Detects single updated page |

## 6. Paired Head-to-Head Benchmarks vs Self-Hosted Firecrawl

### Held-Out 20-URL Benchmark (`benchmarks/search/results/2026-11-27-nextlevel-heldout`)

| Metric | Quelle (with #1–#6) | Self-Hosted Firecrawl |
| --- | --- | --- |
| HTTP 200 & usable content | **20 / 20** (0 errors) | 19 / 20 usable + 1 empty shell (`ho19`: 143 c, 0 quotes) |
| Faster URLs | **19 / 20** | 1 / 20 (`ho19` returned empty shell in 1.344 s vs Quelle waiting 11.182 s for all 10 quotes) |
| Median latency | **0.341 s** (**3.77× faster**) | 1.285 s |
| p95 latency | **0.853 s** (**3.71× faster**) | 3.165 s |

On `ho19` (`https://quotes.toscrape.com/js-delayed/`, 10-second delayed JS), adaptive render wait flipped what was previously a dual failure into a decisive Quelle win: Quelle extracted **1,624 chars** containing all 10 quotes (`Albert Einstein`, `J.K. Rowling`, etc.) in 11.182 s, while Firecrawl returned a 143-char empty login header.

### 25-Query Paired Benchmark (`benchmarks/search/results/2026-11-27-nextlevel-25q`, shared Brave snapshots)

| Metric | Quelle (with #1–#6) | Self-Hosted Firecrawl |
| --- | --- | --- |
| Queries with all expected signals | **25 / 25** (**78 / 78**, 100%) | 24 / 25 (75 / 78, 96.2%) |
| Faster queries | **21 / 25** | 4 / 25 |
| Median retrieval latency | **1.181 s** (**2.45× faster**) | 2.890 s |
| p95 retrieval latency | **5.083 s** (**1.73× faster**) | 8.812 s |
| Deduplicated sources | 6 | 0 |

All benchmark Docker containers were removed cleanly after completion (`docker ps` verified empty).
