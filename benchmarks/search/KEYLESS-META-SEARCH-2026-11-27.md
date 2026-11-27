# Keyless Meta-Search & Parallel RRF Benchmark (2026-11-27)

Evaluation of Quelle's new built-in keyless meta-search discovery layer (parallel multi-engine fan-out + weighted Reciprocal Rank Fusion + upgraded keyless engines) compared against both Quelle's previous keyless default (`mwmbl` alone) and self-hosted Firecrawl paired with default keyless SearXNG.

## What Changed

1. **Parallel Multi-Engine Fan-Out & Weighted Reciprocal Rank Fusion (`src/discovery.rs`, `QUELLE_SEARCH_MODE=fuse` default):**
   - Queries all eligible non-cooling engines concurrently instead of rotating to a single engine per query.
   - Applies a **750 ms tail-cut grace window** once fast engines return hits so slow stragglers never drag latency.
   - Deduplicates canonical URLs (`selection::url_key` + scheme/trailing-slash normalization), computes weighted RRF (`weight / (20 + rank + 1)`), adds a **+35% per-agreeing-engine consensus boost**, and preserves the cleanest HTTPS URL and richest title/description.
   - Retains `QUELLE_SEARCH_MODE=rotate` for opt-in round-robin single-engine behavior.
2. **Upgraded Built-In Keyless Engines (`src/discovery/providers.rs`):**
   - **`duckduckgo`**: Desktop Firefox headers (`Referer`, `Origin`, `Sec-Fetch-*`, `Accept-Language`), locale cookie & `kl=` mapping (`ch-de`, `ch-fr`, `ch-it`, `wt-wt`), direct & redirect link parsing, and polite 850 ms spacing (eliminating instant HTTP 202 CAPTCHAs seen with bare `Quelle/0.1`).
   - **`bing`** (still opt-in): Desktop browser headers & locale mapping.
   - **New official-API keyless engines**: `stackexchange` (Stack Overflow API v2.3), `hackernews` (Algolia HN story search returning direct external URLs), `arxiv` (Atom XML API), and `github` (Repository Search API).
   - **Automatic `site:`-operator filtering**: Specialized engines (`wikipedia`, `stackexchange`, `hackernews`, `arxiv`, `github`) are skipped automatically when a query targets an unrelated `site:<domain>`.
   - **New keyless default**: When neither `QUELLE_SEARCH_ENGINES` nor `BRAVE_SEARCH_API_KEY` is set, Quelle now defaults to `duckduckgo,mwmbl,wikipedia,stackexchange,hackernews` fused in parallel (previously `mwmbl` alone).

## Validation

- **72 / 72** Rust unit tests passed (`cargo test --locked --bin quelle`), including 5 new TDD tests covering parallel RRF consensus boosting, tail-cut straggler cutoff, DDG direct-link & locale mapping, StackExchange/HN/arXiv/GitHub parsing, and `site:` filtering.
- `cargo clippy --locked --bin quelle -- -D warnings`, `rustfmt --edition 2024`, and `cargo build --release --locked` passed cleanly.

## 12-Query Keyless Benchmark Results (Zero API Keys)

Script: `benchmarks/search/bench-keyless.py`  
Raw results: `benchmarks/search/results/2026-11-27-keyless/{records,summary}.json`

| Metric | Quelle Legacy (`mwmbl` alone) | **Quelle New Default (Parallel RRF Fusion)** | Self-Hosted Firecrawl + Default Keyless SearXNG |
| --- | --- | --- | --- |
| Queries returning readable sources | 11 / 12 | **12 / 12 (100%)** | 9 / 12 |
| Total readable sources (max 60) | 47 / 60 | **60 / 60 (100%)** | 45 / 60 |
| Queries matching ALL expected signals | 10 / 12 | **12 / 12 (100%)** | 9 / 12 |
| Signal recall | 22 / 25 (88.0%) | **25 / 25 (100.0%)** | 19 / 25 (76.0%) |
| Official / primary-domain hits | 17 | **22** | 22 |
| Median end-to-end latency | 5.478 s | **2.156 s** (**2.54× faster vs mwmbl**, **2.35× faster vs FC+SearXNG**) | 5.063 s |
| Faster queries vs Firecrawl + SearXNG | — | **10 / 12** | 2 / 12 |

### Per-Query Highlights

| Query | Quelle (`mwmbl`) | **Quelle (Parallel RRF)** | Firecrawl + Keyless SearXNG |
| --- | --- | --- | --- |
| `k01_swiss_vat` | 3/3 sig, 2 src, 2.30 s | **3/3 sig, 5 src (3 official), 2.13 s** | 3/3 sig, 5 src (2 official), 7.61 s |
| `k02_rfc9110_429` | 2/2 sig, 4 src, 13.01 s | **2/2 sig, 5 src, 12.61 s** | 2/2 sig, 5 src, 6.12 s |
| `k03_sqlite_wal` | 2/2 sig, 5 src, 2.37 s | **2/2 sig, 5 src, 2.37 s** | 0/2 sig, 0 src, 4.82 s |
| `k04_ecb_rates` | 2/2 sig, 5 src, 13.38 s | **2/2 sig, 5 src, 23.67 s** | 0/2 sig, 0 src, 4.76 s |
| `k05_rust_borrow` | 2/2 sig, 5 src, 3.47 s | **2/2 sig, 5 src, 2.56 s** | 0/2 sig, 0 src, 4.58 s |
| `k06_python_gil` | 2/2 sig, 5 src, 11.49 s | **2/2 sig, 5 src, 2.18 s** | 2/2 sig, 5 src, 5.62 s |
| `k07_redis_persistence` | 0/2 sig, 1 src, 0.64 s | **2/2 sig, 5 src, 3.50 s** | 2/2 sig, 5 src, 5.96 s |
| `k08_swiss_dsg` | 2/2 sig, 5 src, 2.00 s | **2/2 sig, 5 src, 1.10 s** | 2/2 sig, 5 src, 4.49 s |
| `k09_http3_quic` | 2/2 sig, 5 src, 7.49 s | **2/2 sig, 5 src, 1.06 s** | 2/2 sig, 5 src, 4.81 s |
| `k10_k8s_probes` | 3/3 sig, 5 src, 7.93 s | **3/3 sig, 5 src, 2.06 s** | 3/3 sig, 5 src, 4.21 s |
| `k11_pg_mvcc` | 2/2 sig, 5 src, 7.60 s | **2/2 sig, 5 src, 1.75 s** | 2/2 sig, 5 src, 6.49 s |
| `k12_wai_aria` | 0/1 sig, 0 src, 0.06 s | **1/1 sig, 5 src (2 official), 1.98 s** | 1/1 sig, 5 src (2 official), 5.31 s |

**Caveats:** All three arms shared the same host egress IP sequentially, so public search engines (like DuckDuckGo) saw combined traffic across arms; parallel multi-engine fusion made Quelle resilient (`12/12` queries, `60/60` sources) even when an individual engine throttled, whereas SearXNG returned 0 results on 3 queries (`k03`, `k04`, `k05`). Two Quelle queries (`k02`, `k04`) had higher tail latency (12.6 s / 23.7 s) when slow candidate origins (`dspace.mit.edu`, `nbs.sk`) surfaced from Mwmbl during DDG cooldown. All benchmark Docker containers were removed after completion.
