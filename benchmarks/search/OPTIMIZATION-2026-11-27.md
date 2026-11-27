# Quelle optimization validation — 27 November 2026

All seven areas are implemented: failure classification, evidence preservation,
render readiness, candidate refill/deduplication, source-selection controls,
optional reranking, and bounded caching/request coalescing.

Engine order remains the default. The first ranking experiment showed why a
reranker should not be enabled solely because it is available: Qwen3-Reranker-4B
favored old PostgreSQL documentation and prerelease announcements in some cases.
The heuristic also over-weighted snippet term matches. The final implementation
keeps engine rank as a strong heuristic prior and applies explicit domain,
documentation-version and prerelease preferences after optional model ranking.

## Controls

The original 25 queries and evidence requirements remain unchanged. Five cases
each cover Swiss research, technical documentation, dated facts, comparisons and
difficult pages. This is a development regression set, not an unseen test set.
The original [Quelle vs SearXNG + Firecrawl report](RESULTS-2026-11-27.md) is a
separate experiment; no new Firecrawl timing is claimed here.

Extraction compares the preserved original executable with the changed release
on the exact same original URLs, alternating order. Ranking compares engine,
heuristic and Qwen strategies over the same archived Brave response, requested
with 20 candidates and the same user-supplied Brave credential. All ranking arms
use the changed extractor, a five-document limit, deduplication and refill.
Processes and caches are separate; arm order rotates. Preferred-domain hints
and follow-up searches are disabled in this comparison. Expected-source labels
are used only for diagnostics, never passed into ranking.

The dedicated endpoint serves `Qwen/Qwen3-Reranker-4B`; its request budget is
three seconds. Inputs are the query plus bounded titles, URLs and snippets.
There is no chat-model arm because no chat-completion endpoint was supplied;
the optional chat integration and its bounded fallback are implemented.

## Final paired extraction results

| Metric, 25 cases / 114 URL candidates | Original | Optimized |
| --- | ---: | ---: |
| Median request time | 1.309 s | 1.155 s |
| p95 request time | 7.624 s | 7.730 s |
| Returned sources | 113 | 103 |
| Explicit failures | 1 | 5 |
| Duplicate outputs removed | 0 | 6 |
| Cases matching all simple evidence signals | 25 | 25 |

The lower source count removes four known error documents and six duplicates;
it does not represent ten additional retrieval failures. The median improved
about 12% in this paired run; p95 was essentially unchanged. These are single-run
measurements with live origin variability, not confidence-bounded speed claims.
The Redis case is deliberately slower because it now retrieves actual evidence
instead of returning a navigation shell. The short quotation-page case takes
1.377 s, compared with 7.823 s in the first optimized pass and 1.553 s for the
original executable in the final paired pass.

## Final ranking ablation

All three arms returned 113 documents across the 25 cases. Some archived pools
contain fewer than five distinct readable documents, even when 20 are requested.

| Strategy | Median total | p95 total | Expected-domain matches | Median ranking time |
| --- | ---: | ---: | ---: | ---: |
| Engine order (default) | 1.286 s | 7.646 s | 76 | 0 ms |
| Guarded heuristic | 1.714 s | 7.714 s | 77 | <1 ms |
| Guarded Qwen reranker | 1.899 s | 4.866 s | 68 | 694 ms |

No Qwen call fell back in this run. Qwen's lower tail latency reflects different
selected pages; its median ranking cost is measured separately. It still selects
fewer expected domains, so this run does not justify making it the default.
The heuristic's extra expected-domain match is too small, and live-origin
failures too variable, to establish a general quality advantage.

The safeguards fix specific observed mistakes: both optional strategies now
place `/docs/current/transaction-iso.html` first for the unversioned PostgreSQL
question. The heuristic puts the actual PostgreSQL 18 release announcement
first and excludes prereleases from its top five. Qwen's top five no longer
contains prereleases, but still includes an unrelated PostgreSQL event page.
That remaining mismatch is a reason to keep the model optional.

The first pass is retained for transparency: its unguarded heuristic had 69
expected-domain matches and Qwen had 67, versus 77 for engine order. Changes
were developed against this same set; the final rerun is regression validation,
not an independent estimate of ranking quality.

## Verified changes

- **Correct failures:** the Ottawa language page, an obsolete ESTV PDF URL and
  a deleted Firmenwatch page now return `not_found`; the GitBook SQLite page
  returns `authentication_required`. Their old error-page bodies are no longer
  presented as evidence.
- **VAT table:** ESTV's hydration-only table now retains the old/new column
  dates and the 7.7→8.1%, 2.5→2.6% and 3.7→3.8% rows. The previous extractor
  returned surrounding prose without the table. Rendering waits for empty
  table components to populate, then validates the resulting document.
- **Redis shell:** the Durable Redis page previously returned 888 characters
  of navigation/footer. The first optimized pass recovered 15,475 characters,
  including the actual persistence explanation. Length is diagnostic here;
  the presence of the substantive explanation is the improvement.
- **French encoding:** the Université Laval page's 1,381 replacement characters
  became zero, with `français` and other accented words preserved.
- **Links and structure:** public link destinations accompany visible labels;
  table order, short values and code whitespace remain covered by regression
  checks. Both extraction APIs share decoding and document-quality checks.
- **Duplicates:** the final paired pass removed six duplicate outputs across
  the Fedlex, statistics, Redis, arXiv and MDN cases. Failed/duplicate candidates can be
  replaced from the larger discovery pool.
- **Bounded fallback:** optional ranking failures preserve every candidate in
  heuristic order and expose a diagnostic. An end-to-end replay check confirmed
  an absent reranker falls back and one site-restricted follow-up recovers the
  requested RFC source. Malformed domain hints return HTTP 400.

## Execution and checks

The 30 Rust tests were run successfully; new regression tests were authored
before their implementations. `cargo clippy --locked --bin quelle -- -D warnings`
passes, as do release compilation and diff checks.

Live HTTP checks returned distinct outcomes for 404, 401, 429 and 503, with zero
renderer calls. Four concurrent reads shared an extracted document; a subsequent
warm request took approximately 2 ms in that probe. A forced refresh produced
HTTP 304 using an ETag. These are mechanism checks, not a service-wide latency
estimate. Raw artifacts are in [live checks](results/2026-11-27-live-checks/).

Caches are bounded and respect origin freshness/no-store/private controls.
Extracted entries are tied to raw-response fingerprints so refreshed bytes
invalidate stale extraction. Expired raw entries may supply validators, but are
not served as fresh without revalidation. Public-address checks and DNS pinning
remain in place; connection reuse is not broadened across unchecked resolutions.

## Interpretation limits

Source counts, expected-domain matches and regex signals do not measure factual
correctness. The expected-domain list is incomplete (for example, a valid Swiss
government source can be outside a case's listed hosts). All first-pass arms
matched the simple signals in all 25 cases despite clear ranking differences.
No graded nDCG, independent blind-review score or universal winner is claimed.

The fixes were informed by this development set. A separate held-out set is
needed before promoting optional ranking to the default. A request can still
need interactive navigation, and the existing long-PDF page limit remains
explicitly reported as truncation.

## Artifacts and reproduction

- [Final summary](results/2026-11-27-optimization-final/summary.json)
- [Final manifest and binary hashes](results/2026-11-27-optimization-final/manifest.json)
- [Compact per-query results](results/2026-11-27-optimization-final/case-summary.json)
- [Full responses and source text](results/2026-11-27-optimization-final/records.json)
- [First-pass summary](results/2026-11-27-optimization/summary.json)
- [Original 25-query criteria](queries.json) and [runner instructions](README.md)

The final run reused the first pass's 20-result Brave snapshots; it made no
additional Brave requests. Raw artifacts are ignored by Git. The tests,
implementation, runners, criteria and this report are versionable. Benchmark
processes were task-owned and terminated on completion; existing services and
the remote model services were not restarted.
