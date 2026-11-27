# Extraction Quality, Held-Out Evaluation, and Ranking Ablation (Round 2)

This round closes the remaining extraction-quality gaps identified against
self-hosted Firecrawl across the 25-query development set, validates generalization
on a new 20-URL held-out benchmark, evaluates 20-candidate retrieval/ranking with
the updated extractor, and eliminates same-host redirect/negotiation-retry sleep
and render-slot bottlenecks.

## Summary of Changes

1. **Collapsed disclosure content recovery** ([`src/pages/clean.rs`](../../src/pages/clean.rs), [`src/pages/markdown.rs`](../../src/pages/markdown.rs)):
   Preserve ARIA accordions and tab panels (`role="tabpanel"`, `role="region"` with
   `aria-labelledby`, non-chrome `aria-controls` targets, and explicit
   accordion/tab-pane content wrappers) that use `style="visibility:hidden;"`,
   `aria-hidden="true"`, or `hidden`, while keeping genuine hidden traps,
   decorative `aria-hidden` icons, dialogs, loading indicators, and hidden
   navigation excluded.
2. **Structured Markdown in `/api/content` & shared cleanup** ([`src/pages.rs`](../../src/pages.rs), [`src/pages/markdown.rs`](../../src/pages/markdown.rs), [`src/pages/clean.rs`](../../src/pages/clean.rs)):
   `/api/content` now emits clean structured Markdown (headings, lists, nested-fence-safe
   code blocks with preserved indentation, data tables with spans/captions,
   flattened layout tables without pipe escaping, single-line links, JS/direct
   contact extraction and footer contact supplementation). Strips soft hyphens
   (`\u{00AD}`) and zero-width spaces (`\u{200B}`, `\u{FEFF}`) across both HTML
   and PDF extraction, fixes image-inside-link spacing, and removes duplicated
   phone/email suffixes in plain-text mode.
3. **Sidebar, menu, and skip-link chrome stripping** ([`src/pages/clean.rs`](../../src/pages/clean.rs), [`src/pages/markdown.rs`](../../src/pages/markdown.rs)):
   Expands multilingual skip-link stripping (`Direkt zum Footer springen`,
   `Überspringe den Hauptinhalt`, `Skip to footer`, etc.) and strips non-semantic
   sidebars/menus (`#sidebar`, `.sidebar`, `#pgSideNav`, `#pgNavbar`, `.mainmenu`,
   `.submenu`, `.searchmenu`, etc.), guarded so elements containing `<h1>`,
   `<main>`, or `<article>` are never dropped.
4. **Static catalog false-positive fix** ([`src/pages/quality.rs`](../../src/pages/quality.rs)):
   Caught by the held-out benchmark on `books.toscrape.com`: sums visible
   `<main>`/`<article>` content lengths when checking for link-only JS shells so
   product catalogs with many short `<article>` cards are not falsely rejected as
   `needs_js`.
5. **Same-host redirect/retry reuse & configurable render concurrency** ([`src/pages.rs`](../../src/pages.rs), [`src/main.rs`](../../src/main.rs)):
   Reuses the per-host politeness permit and DNS-pinned HTTP client across
   same-host/same-origin redirect and HTML-negotiation retry hops (still checking
   `public_url` on every hop and releasing before 429/503 backoff), avoids parsing
   rendered Lightpanda HTML twice, and raises default `QUELLE_RENDER_CONCURRENCY`
   from 2 to 4 (configurable `1–16`).

## 25-Query Development Benchmark (Frozen Brave Snapshots)

Replayed from `2026-11-27-speed-final` snapshots (zero new Brave calls, identical
candidate URLs, fresh services, alternating order):

| Metric | Previous Quelle (`speed-final`) | Updated Quelle (`quality3-full`) | SearXNG + Firecrawl |
| --- | ---: | ---: | ---: |
| Median retrieval time | 1.228 s | **1.176 s** | 3.342 s |
| p95 retrieval time (nearest-rank) | 2.663 s | **2.225 s** | 7.909 s |
| Sources with Markdown headings | 16 / 102 | **94 / 98** | 97 / 111 |
| Sources with fenced code blocks | 11 / 102 | **38 / 98** | 1 / 111 |
| Orphan-newline / glued / dup-phone defects | 22 / 13 / 7 | **0 / 0 / 0** | — |
| Soft hyphens / ZWSP / HTML skip leaks | 3 / 1 / 6 | **0 / 0 / 0** | — |

Targeted Swiss federal accordion & sidebar recoveries:
- `estv.admin.ch/de/mehrwertsteuer`: 4,362 → **9,125 chars** (VAT FAQs recovered)
- `bj.admin.ch/de/handelsregister-zefix-und-regix`: 3,469 → **10,672 chars** (Dossier & Press recovered)
- `bak.admin.ch/fr/langues`: 1,860 → **5,517 chars** (RS 441.1 & official PDFs recovered)
- `bfs.admin.ch/.../landesindex-konsumentenpreise.html`: 10,939 → **24,250 chars** (opening hours & links recovered)
- `doc.rust-lang.org/1.18.0/.../borrowing.html`: 19,690 → **9,833 chars** (`#sidebar` TOC stripped)

**Blinded A/B audit (`quality3-full/review.json`, decoded after scoring):**
- **Quelle 22 wins, Firecrawl 0 wins, 3 ties** (200 vs. 177 rubric points).
- Note on `hard01` (Fedlex): during the full run, `fedlex.admin.ch` had a cold
  origin stall (~7.4 s raw HTTP TTFB verified independently via `urllib`), causing
  both Quelle (24.1 s, 0 sources) and Firecrawl (21.2 s, 5 JS-required notices)
  to fail. In the separate repeat (`quality2-check`) and the 20-candidate ranking
  run (`2026-11-27-ranking-ablation`), Quelle retrieved 4–5 complete legal-text
  documents (257k–325k chars each, Art. 4 intact, 0 loading screens) in 5.7–7.5 s.
- **Original unversioned Nuxt fallback (`quality2-check`, `tech05`):** dropped
  from 3.340 s to **1.448 s** (vs. Firecrawl **3.481 s**), returning the complete
  36,244-char Nuxt 4 article with zero browser launches.

*Disclosure:* A/B grades were assigned by the assistant before reading
`blind-key.json`, though Markdown formatting cues (e.g., ATX vs. Setext headings)
remain visible; this is not an independent human blind panel.

## 20-URL Held-Out Benchmark (`heldout.py` / `heldout-urls.json`)

Evaluates 20 unseen live URLs (ECB rates table, Swiss gov, Python/Go/Docker/Git/TypeScript/React/Tailwind docs, Wikipedia, SemVer, Hacker News, Lobsters, Martin Fowler, arXiv RAG PDF, Books to Scrape, 10 s delayed JS quotes, W3C WAI-ARIA 1.2) using direct URL retrieval vs. self-hosted Firecrawl (`onlyMainContent: true`, 25 s timeout):

| Metric | Quelle (`2026-11-27-heldout2`) | Firecrawl |
| --- | ---: | ---: |
| Median retrieval time | **0.193 s** | 1.282 s |
| p95 retrieval time | **1.414 s** | 2.449 s |
| Faster URL pairs | **19 / 20** | 1 / 20 |
| Substantive extractions | **19 / 20** | 19 / 20 |
| Blinded A/B audit | **16 wins, 4 ties, 0 losses** | 0 wins |

On `ho19` (`quotes.toscrape.com/js-delayed/`, 10 s script delay), Firecrawl
returned a 143-char empty shell without quotes, while Quelle returned `needs_js`
under its 7 s default render wait.

## 20-Candidate Retrieval & Ranking Ablation (`2026-11-27-ranking-ablation`)

Replayed from `2026-11-27-optimization-final` frozen 20-candidate Brave responses:

| Strategy | Median | p95 | Sources | Errors | Official sources | All signals | Fallbacks |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `engine` (default) | **1.218 s** | **2.850 s** | 113 | 7 | 77 | 25/25 | 0 |
| `heuristic` | 1.430 s | 3.410 s | 113 | **6** | 77 | 25/25 | 0 |
| `rerank` (offline endpoint test) | 1.237 s | 4.552 s | 113 | 6 | 77 | 25/25 | 24 (graceful) |

Compared with the 5-candidate pool, 20-candidate refill raises unique retrieved
sources from 102 to 113 and matches all signals on 25/25 queries; p95 drops from
7.65 s (in `OPTIMIZATION-2026-11-27.md`) to **2.85 s**. `heuristic` reduces
fetch errors (6 vs. 7) and eliminates noisy legacy/prerelease slots on `tech01`
and `date04` while preserving all 77 official-domain hits.

## Validation

- All regression tests were authored before implementation and observed failing
  first.
- **46 / 46 Rust tests**, `cargo clippy --locked --bin quelle -- -D warnings`,
  `rustfmt --check`, `cargo build --release --locked`, and **9 / 9 Lightpanda
  readiness checks** passed.
- All benchmark Docker containers and volumes were cleaned up.
