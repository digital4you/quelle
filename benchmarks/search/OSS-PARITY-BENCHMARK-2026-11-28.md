# Gently-Paced Free-Engines OSS Parity Benchmark (2026-11-28)

Head-to-head evaluation of **Quelle** (100% standalone, zero API keys, zero SearXNG) vs **Self-Hosted OSS Firecrawl + Default Keyless SearXNG** (6-container stack).

- **Script:** [bench-oss-parity.py](bench-oss-parity.py)
- **Raw results:**
  - Before new built-in providers: `benchmarks/search/results/2026-11-28-oss-parity/{records,summary}.json`
  - After adding built-in `yahoo_jp`, `seznam`, `mdn`, `openalex`: `benchmarks/search/results/2026-11-28-oss-parity-v2/{records,summary}.json`
- **Pacing & fairness:** Only 2 search arms, **`8.0 s` delay** after every search call, and **alternating first-arm order** per query (odd: Quelle first; even: Firecrawl first) to avoid IP throttling and warm-cache bias. All Docker benchmark containers were removed after completion.

---

## Built-In Free Discovery Upgrade (No SearXNG)

Added 4 zero-key providers directly into `src/discovery/providers.rs`:
- **`yahoo_jp`** (`search.yahoo.co.jp`, Google-backed global web index, weight `1.55`, 700 ms polite spacing)
- **`seznam`** (`search.seznam.cz`, independent European full-web crawler, weight `1.35`, 700 ms polite spacing)
- **`mdn`** (official MDN Web Docs JSON API, intent-routed for web/HTTP/ARIA/CSS/DOM queries, weight `1.35` on web intent)
- **`openalex`** (official open scholarly JSON API, intent-routed for academic queries)

New standalone keyless default: `duckduckgo,yahoo_jp,seznam,mwmbl,wikipedia,mdn,stackexchange,hackernews` (4 independent general-web engines + 4 intent-routed official JSON APIs fused via parallel weighted RRF).

---

## Part A: OSS-Parity Capabilities & Resource Footprint (`v2`)

| Capability / Metric | **Quelle (Standalone)** | Self-Hosted OSS Firecrawl + SearXNG | Notes |
| --- | --- | --- | --- |
| Cold boot time | **0.51 s** | 16.20 s | **31.8× faster boot** |
| Live memory footprint | **97.1 MiB** (1 process) | 1,418.0 MiB (6 containers) | **14.6× less RAM** (`firecrawl` 512 + `playwright` 355 + `searxng` 184 + `postgres` 156 + `rabbitmq` 142 + `redis` 69 MiB) |
| PNG Screenshot (`example.org`) | **PASS · 0.45 s** (`86,570` B PNG via Chromium CDP) | FAIL · HTTP 500 | Self-hosted OSS Firecrawl fails screenshots without cloud/storage wiring |
| Click Pagination (`quotes.toscrape.com/js/`) | **PASS · 1.93 s** (`3,394` chars, Page 2 loaded via Lightpanda) | FAIL · HTTP 400 | Self-hosted OSS Firecrawl rejected `actions` on `/v2/scrape` (`playwright-service` does not implement full Firecrawl Cloud actions) |
| Async Crawl (`example.org`, limit 1) | **PASS · 0.52 s** (live SSE `/api/crawl/{id}/stream`) | PASS · 12.96 s (polling `/v2/crawl/{id}`) | **24.8× faster** |
| Scanned 3-Page PDF OCR (zero text layer) | **PASS · 1.01 s** (`2.96` pages/s, **5/5** signals via `llamaindex/OCRia-nano-1` on CPU) | Not supported locally | Confirmed `pdftotext == ""` prior to OCR |

---

## Part B: Free Keyless Meta-Search (12 Queries, 8.0 s Delay, Alternating Order)

| Metric | Quelle v1 (pre-upgrade) | **Quelle v2 (Standalone Free Default)** | Self-Hosted Firecrawl + Keyless SearXNG (`v2`) | Winner |
| --- | ---: | ---: | ---: | --- |
| Queries returning sources | 11 / 12 | **12 / 12 (100%)** | 12 / 12 (100%) | Tied at 100% |
| Total readable sources (max 60) | 51 / 60 | **60 / 60 (100%)** | 57 / 60 (95.0%) | **Quelle (+3)** |
| **Signal recall** | 22 / 25 (88.0%) | **25 / 25 (100.0%)** | 25 / 25 (100.0%) | **Tied at 100%** |
| **Official-domain hits** | 17 | **23** | 21 | **Quelle (+2)** |
| **Median latency** | 2.522 s | **2.399 s** | 4.191 s | **Quelle (1.75× faster)** |
| **p95 tail latency** | 3.206 s | **2.881 s** | 32.358 s | **Quelle (11.2× lower p95)** |
| **Faster queries** | 11 / 12 | **12 / 12 (100%)** | 0 / 12 | **Quelle (12–0 sweep)** |

### Per-Query Results (`v2`)

| # | Query (`first`) | **Quelle (Standalone Free Default)** | Firecrawl + Keyless SearXNG |
| ---: | --- | --- | --- |
| 1 | `k01_swiss_vat` (Q) | **3/3 sig, 5 src (4 off), 2.69 s** | 3/3 sig, 5 src (2 off), 5.10 s |
| 2 | `k02_rfc9110_429` (F) | **2/2 sig, 5 src (2 off), 2.13 s** | 2/2 sig, 5 src (0 off), 3.73 s |
| 3 | `k03_sqlite_wal` (Q) | **2/2 sig, 5 src (1 off), 1.74 s** | 2/2 sig, 3 src (1 off), 10.79 s |
| 4 | `k04_ecb_rates` (F) | **2/2 sig, 5 src (1 off), 2.04 s** | 2/2 sig, 5 src (3 off), 2.29 s |
| 5 | `k05_rust_borrow` (Q) | **2/2 sig, 5 src (1 off), 2.88 s** | 2/2 sig, 5 src (1 off), 4.71 s |
| 6 | `k06_python_gil` (F) | **2/2 sig, 5 src (2 off), 2.53 s** | 2/2 sig, 5 src (2 off), 2.81 s |
| 7 | `k07_redis_persistence` (Q) | **2/2 sig, 5 src (3 off), 1.76 s** | 2/2 sig, 4 src (3 off), 32.36 s |
| 8 | `k08_swiss_dsg` (F) | **2/2 sig, 5 src (1 off), 2.59 s** | 2/2 sig, 5 src (2 off), 2.75 s |
| 9 | `k09_http3_quic` (Q) | **2/2 sig, 5 src (3 off), 2.33 s** | 2/2 sig, 5 src (1 off), 4.65 s |
| 10 | `k10_k8s_probes` (F) | **3/3 sig, 5 src (2 off), 2.76 s** | 3/3 sig, 5 src (2 off), 5.31 s |
| 11 | `k11_pg_mvcc` (Q) | **2/2 sig, 5 src (0 off), 1.43 s** | 2/2 sig, 5 src (2 off), 2.18 s |
| 12 | `k12_wai_aria` (F) | **1/1 sig, 5 src (3 off), 2.46 s** | 1/1 sig, 5 src (2 off), 2.92 s |
