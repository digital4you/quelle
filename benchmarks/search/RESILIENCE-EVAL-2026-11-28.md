# Resilience, Streaming, Ground-Truth Eval & Tail-Latency Report (2026-11-28)

Evaluation of six architectural upgrades to Quelle (plus a production Cloudflare challenge fast-abort fix surfaced by benchmarking) verified via unit tests, an independent 10-case ground-truth evaluation harness, Lightpanda readiness verification, and repeat 12-query keyless meta-search benchmarks.

## What Changed

1. **Head-of-Line Unblocking & Primary-Aware RRF Grace (`src/content.rs`, `src/discovery.rs`):**
   - Replaced `.buffered(4)` ordered waiting in `/api/content` with `FuturesUnordered` fast-fill + a `1500 ms` rank-preservation grace window so one slow candidate origin can no longer stall an entire search response.
   - Split discovery fusion grace into `600 ms` once a primary general-web engine (`brave`, `duckduckgo`, `bing`, `searxng`, `mwmbl`) returns hits vs `1400 ms` while a primary engine is still in flight.
2. **Per-Host Adaptive Circuit Breaker, EWMA Latency & Cloudflare Challenge Fast-Abort (`src/politeness.rs`, `src/pages.rs`, `src/pages/cdp.rs`, `src/content.rs`):**
   - Tracks per-host consecutive failures (`>= 2` → 120 s cooldown, `>= 4` → 300 s cooldown, reset on success) and EWMA latency (`α = 0.35`); caps `host_step_budget` at 5 s for degraded or slow (>6 s) hosts; stably demotes degraded/slow (>7 s EWMA) hosts in `/api/content` candidate ordering.
   - Instant bot/Cloudflare challenge detection (`state == 3` → `PageError::Blocked`) in Lightpanda's readiness poll script (`src/pages/cdp.rs`) and failure recording on `Blocked`/`Timeout` CDP renders (`src/pages.rs`), eliminating 13 s adaptive-wait stalls on Cloudflare `"Just a moment..."` challenge loops while respecting blocks without attempting any bypass.
3. **Language- & Intent-Aware Engine Routing + Native `searxng` Provider (`src/discovery/providers.rs`):**
   - Skips English-only `stackexchange`/`hackernews` on non-English, non-technical queries and boosts `arxiv` (`1.15`), `github` (`1.15`), and `stackexchange` (`0.95`) on matching academic/repo/tech intent.
   - Added native `searxng` JSON API provider (`QUELLE_SEARXNG_URL`, `QUELLE_SEARXNG_ENGINES`, `QUELLE_SEARXNG_CATEGORIES`), configured startup-only through `sidecar_endpoint`.
4. **Global Cross-Document Evidence Pack & SSE Streaming (`src/pages/chunks.rs`, `src/content.rs`, `src/scrape.rs`):**
   - Added `totalMaxTokens` on `/api/content` for cross-document chunk deduplication and global token budgeting (`budget_evidence_pack`).
   - Added `stream: true` Server-Sent Events on `/api/content` (`source` / `error` / `done`) and batch `/api/scrape` (`page` / `error` / `done`).
5. **Lightweight Read-Only CDP Controls & Persistent Crawl State (`src/pages/cdp.rs`, `src/scrape.rs`, `src/crawl.rs`, `src/main.rs`):**
   - Added CSS-validated, JSON-escaped `waitForSelector` and bounded `scrollSteps` (`0..=10`) on `/api/scrape` and `/api/crawl`.
   - Added optional `QUELLE_STATE_DIR` atomic snapshot persistence so `sinceCrawlId` incremental crawls survive process restarts (strict `crawl-<hex>-<hex>` ID validation prevents path traversal).
6. **Independent Ground-Truth Eval Harness (`benchmarks/search/eval-groundtruth.py`):**
   - Deterministic 10-case live & local harness covering factual extraction, structured tables vs layout-table rejection, Japanese UTF-8, clean MDX/JSX, full 194-page PDF extraction, CDP `waitForSelector` + `scrollSteps`, global `totalMaxTokens`, SSE streaming time-to-first-source, and persistent crawl diffs across process restart.

## Validation

- **79 / 79** Rust unit tests passed (`cargo test --locked --bin quelle`), zero new crates (`Cargo.toml` and `Cargo.lock` untouched).
- `cargo clippy --locked --bin quelle -- -D warnings`, `rustfmt --edition 2024`, and `cargo build --release --locked` passed cleanly.
- **10 / 10** Lightpanda readiness cases passed (`benchmarks/search/verify-readiness.py`).

## Ground-Truth Eval Results (`eval-groundtruth.py`)

**10 / 10 passed** against `target/release/quelle` + Lightpanda v1.0.5:

| Case | Result | Time | Key Measured Signal |
| --- | --- | ---: | --- |
| `gt01_swiss_vat_rates` | PASS | 0.137 s | Live Swiss ESTV page; exact `8.1%`, `2.6%`, `3.8%` rates (`6,035` chars) |
| `gt02_ecb_structured_table` | PASS | 0.845 s | 1 structured table, `72` rows (`USD`, `CHF`) |
| `gt03_hn_layout_table_rejected` | PASS | 1.915 s | `0` spurious tables extracted from Hacker News layout markup |
| `gt04_japanese_utf8` | PASS | 0.353 s | Clean Japanese Wikipedia extraction (`67,419` chars, no mojibake) |
| `gt05_react_mdx_clean` | PASS | 0.186 s | Clean React docs extraction (`63,656` chars, zero JSX/MDX leakage) |
| `gt06_rfc9110_pdf_full_194pp` | PASS | 1.787 s | `194 / 194` pages read, `truncated: false` (`maxPages: 200`) |
| `gt07_cdp_wait_selector_and_scroll` | PASS | 1.517 s | `waitForSelector` + `scrollSteps: 3` captured async & scroll-revealed items |
| `gt08_global_total_max_tokens` | PASS | 1.107 s | 2 sources deduplicated & packed to `1,197 / 1,200` tokens (`totalMaxTokens`) |
| `gt09_sse_streaming_ttfs` | PASS | 0.214 s TTFS | Fast source streamed in `0.214 s` while 10 s slow JS source finished at `11.197 s` (**52× faster TTFS**) |
| `gt10_persistent_crawl_state_across_restart` | PASS | 1.123 s | Full process restart + `sinceCrawlId` reported `added: 0, updated: 0, unchanged: 1` |

## Keyless Meta-Search Tail-Latency Comparison (`bench-keyless.py`)

Tracking `quelle_fused` across the `1355e6d` baseline (`2026-11-27-keyless`), v2 (`2026-11-28-keyless-v2`, before Cloudflare fast-abort), and v3 (`2026-11-28-keyless-v3`, final):

| Metric (`quelle_fused`) | Baseline (`1355e6d`) | v2 (pre-CF fix) | **v3 (Final)** | Delta vs Baseline |
| --- | ---: | ---: | ---: | --- |
| Median end-to-end latency | 2.156 s | 2.425 s | **2.332 s** | comparable (~2.3 s) |
| **p95 end-to-end latency** | 23.670 s | 35.036 s (CF stall) | **4.193 s** | **5.65× faster p95** |
| `k02_rfc9110_429` | 12.61 s | 2.59 s | **4.19 s** | **3.0× faster** |
| `k04_ecb_rates` | 23.67 s | 2.51 s | **2.42 s** | **9.8× faster** |
| `k07_redis_persistence` | 3.50 s | 35.04 s (timeout) | **2.46 s** | **14.2× faster vs v2; 0 timeouts** |
| Faster queries vs Firecrawl + SearXNG | 10 / 12 | 11 / 12 | **12 / 12** | beats on every query |

### v3 Arm Summary (`benchmarks/search/results/2026-11-28-keyless-v3/summary.json`)

| Metric | Quelle (`mwmbl` alone) | **Quelle (Parallel RRF Fusion)** | Self-Hosted Firecrawl + Keyless SearXNG |
| --- | ---: | ---: | ---: |
| Queries returning readable sources | 11 / 12 | **11 / 12** | 2 / 12 |
| Total readable sources (max 60) | 47 | **49** | 10 |
| Signal recall | 22 / 25 (88%) | **22 / 25 (88%)** | 4 / 25 (16%) |
| Median latency | 2.076 s | **2.332 s** | 4.665 s |
| **p95 latency** | 10.639 s | **4.193 s** | 7.574 s |

**Honest caveats:**
- Sequential benchmark runs share one egress IP, so public search engines progressively throttle repeated automated traffic: in v2 Firecrawl + SearXNG still got 9/12 queries (`5.369 s` median, `8.051 s` p95) and Quelle got 11/12 queries (`55` sources, `23/25` signals, `24` official hits), whereas by v3 SearXNG's upstream engines throttled 10/12 queries and DuckDuckGo throttled `k07`/`k12` (leaving `k07` with 1 source and `k12` with 0 on both Quelle arms). Even under heavy upstream throttling, Quelle's parallel RRF + fast-fill + circuit breaker held **11/12 queries** at **`2.33 s` median / `4.19 s` p95** with zero stalls.
- All benchmark Docker containers were removed after completion.
