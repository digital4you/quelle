# Quelle vs SearXNG + Firecrawl retrieval benchmark

The 25 queries and their evidence requirements are fixed in `queries.json` before
retrieval. Five cases each cover Swiss research, technical documentation, dated
facts, comparisons and difficult pages. This is a small diagnostic benchmark,
not a representative estimate of all web searches.

Run `python3 benchmarks/search/run.py --output /absolute/output/directory`.
Provide `BRAVE_SEARCH_API_KEY` in the environment or at the hidden prompt. The
runner builds Quelle, creates a private temporary SearXNG configuration, launches
a task-specific Docker Compose project and Quelle process, and removes them on
exit. Existing services are not changed. Docker and Cargo are required. Set
`QUELLE_LIGHTPANDA` to the installed Lightpanda executable. The baseline uses
Quelle's native renderer; optional Chromium and OCR sidecars are not enabled.

The [post-optimization rematch](REMATCH-2026-11-27.md) reruns both pipelines on
all 25 queries and investigates the remaining quality and speed losses.
The [quality-fix rerun](QUALITY-2026-11-27.md) implements those extraction fixes,
optimizes browser readiness and repeats the full comparison plus the original
failing candidate URLs.
The [speed rerun](SPEED-2026-11-27.md) removes unnecessary Nuxt rendering and adds
a guarded synchronous-page fast path, with three paired old/new rounds, the full
25-query comparison and separate diagnostic repeats.

To repeat selected cases with archived search results and no new Brave calls:

```sh
QUELLE_LIGHTPANDA=/path/to/lightpanda python3 benchmarks/search/run.py \
  --output benchmarks/search/results/separate-check \
  --snapshots benchmarks/search/results/2026-11-27-rematch \
  --only hard04,hard01,cmp05,tech05
```

Use a new output directory. Omit `--only` to repeat all 25 cases. These runs start
fresh services and retrieve live pages; only the search response is frozen.
Archived Brave latency is labeled separately and is not a new measurement.

The browser readiness regression checks require no Brave key or running Quelle
service. They launch a temporary local fixture server and exercise the production
activity observer and readiness expression in Lightpanda, checking both the
synchronous fast path and delayed content:

```sh
python3 benchmarks/search/verify-readiness.py --lightpanda /path/to/lightpanda \
  --output /tmp/quelle-readiness-results.json
```

## Controls

- One live Brave request per query, same supplied credential, Switzerland,
  explicit query language, strict SafeSearch, spellcheck off, five results.
- Both adapters receive the exact same archived Brave JSON through a temporary
  authenticated replay endpoint. This removes search drift and differing locale
  defaults. The endpoint does not rewrite queries or silently substitute results.
  Failed searches remain recorded failures.
- Quelle `/api/content` and SearXNG `/search` followed by Firecrawl `/v2/scrape`,
  with up to four page requests concurrently per pipeline. Query order is seeded
  and pipeline order alternates. Pipelines run sequentially to avoid competing
  for local CPU and memory. Quelle explicitly uses engine order, five candidates
  and no follow-up searches. No model, query expansion or learned reranking.
- Fresh services, 25-second Firecrawl scrape timeout
  (Quelle's built-in page fetch budget is 25 seconds), no runner retries.
  Internal pipeline retry/render policies remain their own. These are distinct
  implementations, not identically configured fetchers. This self-hosted Firecrawl
  build rejects the cloud-only `maxAge` option; its absent-value cache age is zero.
- Record raw responses, failures, URL lists/order, upstream search latency,
  retrieval latency, source lengths, official-domain hits and evidence-term
  coverage. Results are checkpointed after each query.

Search latency and replay-backed retrieval latency are reported separately.
Adding them estimates one cold query; replay latency itself is not a live-search
performance claim. Shared upstream responses do not guarantee that adapters
preserve the same URL order, so the report checks the supplied candidate URL
sets against SearXNG's selected URLs. Quelle's returned sources/errors omit
suppressed duplicates; their URLs are observations, not a complete fetch trace.
Its completed-fetch and duplicate diagnostics are recorded separately.

## Quality review

HTTP success, character counts and evidence-term matches are diagnostics, **not
quality judgments**. Signals are deliberately simple and can match irrelevant
content. They do not verify dates, numeric values, relationships or truth.
Do not count a long page or a matching snippet as a correct answer. The raw
responses retain full content so each difference can be inspected.

`review.json` presents deterministic, randomly swapped A/B outputs for each query.
Grade each against the case criteria: relevance (0–2), evidence completeness
(0–2), primary-source quality (0–2), temporal fit (0–2, or N/A), extraction
cleanliness (0–2). Note exact supporting passages and missing evidence. Compare
paired wins/ties/losses and category patterns; disclose that a single unblinded
reviewer is not an independent ground-truth panel. `blind-key.json` decodes A/B.
All-zero retrievals remain failures rather than disappearing from averages.

## Reranking

The original November 27 baseline preserved engine order in `/api/content`.
`/api/answer` used the configured chat model to rank up to 40 candidates from
titles and snippets. The tested SearXNG + Firecrawl composition had no learned
reranker. The comparison runner now explicitly requests `selection: "engine"`
and `candidateLimit: 5` to keep those discovery controls as defaults evolve.

Test a reranker only as a separate paired experiment: obtain a larger frozen
candidate pool, apply the same model and prompt/budget to both arms, and compare
graded nDCG@5 or evidence coverage plus added latency. A five-result baseline
cannot establish whether a model selects better pages from a larger pool.

## Optimization ablations

`optimize.py` runs two separate experiments across the same 25 cases:

1. Read the original archived five URLs with a preserved baseline executable
   and the current release executable, alternating order. No new search calls.
2. Freeze one new Brave response with 20 candidates per query. Compare current
   engine order, heuristic ordering and optional Qwen3-Reranker-4B ordering.
   Each arm has a fresh process and its own caches; arm order rotates. All three
   share extraction, deduplication, refill, a five-document limit and the exact
   candidate snapshot. Preferred-domain hints and follow-up searches are off,
   so expected-source labels cannot leak into ranking inputs.

```sh
QUELLE_LIGHTPANDA=/path/to/lightpanda python3 benchmarks/search/optimize.py \
  --output benchmarks/search/results/optimization \
  --baseline /path/to/preserved/quelle \
  --snapshots benchmarks/search/results/2026-11-27-run3 \
  --reranker http://model-host:8004/rerank
```

Provide the same Brave credential via the environment or hidden prompt.
`--ranking-snapshots` reuses an earlier optimization archive without live Brave
calls. `--skip-extraction` limits a run to the ranking arms. Engine order remains
the default; optional heuristic/model ranking applies version and prerelease
preferences. See [optimization results](OPTIMIZATION-2026-11-27.md) for the paired
extraction results and final ranking ablation. Raw artifacts include binary hashes, controls, full source text, errors,
ranking latency and fallback diagnostics. These ranking ablations isolate
Quelle's strategies; they are not a new Firecrawl measurement. No chat arm is
claimed without a configured chat-model endpoint. Domain hits, source counts
and regex coverage remain diagnostics, not nDCG or proof of answer quality.

References: [SearXNG Brave API engine](https://docs.searxng.org/dev/engines/online/brave.html),
[Firecrawl scrape API](https://docs.firecrawl.dev/api-reference/endpoint/scrape).
