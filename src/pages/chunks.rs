//! Section-anchored, token-budgeted Markdown chunking and query-focused distillation.

use std::collections::HashSet;

use serde::Serialize;
use url::Url;

/// Default chunk target size in approximate LLM tokens.
pub const DEFAULT_CHUNK_TOKENS: usize = 512;

/// A heading-scoped, atomic Markdown chunk suitable for RAG / LLM ingestion.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Chunk {
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heading: Option<String>,
    pub breadcrumb: Vec<String>,
    pub anchor_url: Url,
    pub markdown: String,
    pub tokens: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<u32>,
}

/// Fast deterministic token count estimate for English/European text and code.
pub fn estimate_tokens(text: &str) -> usize {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return 0;
    }
    let chars = trimmed.chars().count();
    let word_count = trimmed.split_whitespace().count();
    let by_chars = chars.div_ceil(4);
    let by_words = (word_count * 4).div_ceil(3);
    by_chars.max(by_words).max(1)
}

#[derive(Debug)]
enum Block {
    Heading {
        level: u8,
        title: String,
        raw: String,
    },
    Atom(String),
}

fn parse_atx_heading(line: &str) -> Option<(u8, String)> {
    let trimmed = line.trim_start();
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = trimmed[hashes..].strip_prefix(' ')?.trim();
    if rest.is_empty() {
        return None;
    }
    let cleaned = rest.trim_end_matches('#').trim();
    if cleaned.is_empty() {
        return None;
    }
    Some((hashes as u8, strip_inline_md(cleaned)))
}

fn strip_inline_md(text: &str) -> String {
    // Strip `[label](url)` -> `label`, bold/italic/code markers for clean heading matching.
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'['
            && let Some(close_bracket) = text[i + 1..].find(']')
        {
            let after = i + 1 + close_bracket + 1;
            if text[after..].starts_with('(')
                && let Some(close_paren) = text[after + 1..].find(')')
            {
                out.push_str(&text[i + 1..i + 1 + close_bracket]);
                i = after + 1 + close_paren + 1;
                continue;
            }
        }
        let ch = text[i..].chars().next().unwrap();
        if !matches!(ch, '*' | '`' | '_') {
            out.push(ch);
        }
        i += ch.len_utf8();
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn fence_delimiter(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let first = trimmed.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let count = trimmed.chars().take_while(|&c| c == first).count();
    (count >= 3).then_some((first, count))
}

fn is_pipe_table_line(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.len() >= 2 && t.contains('|')
}

/// Parses Markdown into atomic blocks without splitting inside fenced code
/// blocks or Markdown pipe tables.
fn parse_blocks(markdown: &str) -> Vec<Block> {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        // Fenced code block (atomic)
        if let Some((delim, count)) = fence_delimiter(line) {
            let start = i;
            i += 1;
            while i < lines.len() {
                if let Some((c, n)) = fence_delimiter(lines[i])
                    && c == delim
                    && n >= count
                    && lines[i].trim().chars().all(|ch| ch == delim)
                {
                    i += 1;
                    break;
                }
                i += 1;
            }
            blocks.push(Block::Atom(lines[start..i].join("\n")));
            continue;
        }

        // ATX heading
        if let Some((level, title)) = parse_atx_heading(line) {
            blocks.push(Block::Heading {
                level,
                title,
                raw: line.trim_end().to_owned(),
            });
            i += 1;
            continue;
        }

        // Pipe table (atomic)
        if is_pipe_table_line(line) {
            let start = i;
            i += 1;
            while i < lines.len() && is_pipe_table_line(lines[i]) {
                i += 1;
            }
            blocks.push(Block::Atom(lines[start..i].join("\n")));
            continue;
        }

        // Paragraph / list / blockquote up to blank line, heading, code fence, or table
        let start = i;
        i += 1;
        while i < lines.len() {
            let next = lines[i];
            if next.trim().is_empty()
                || fence_delimiter(next).is_some()
                || parse_atx_heading(next).is_some()
                || is_pipe_table_line(next)
            {
                break;
            }
            i += 1;
        }
        blocks.push(Block::Atom(lines[start..i].join("\n")));
    }

    blocks
}

fn normalize_heading_key(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn resolve_anchor_url(base_url: &Url, id: Option<&str>) -> Url {
    let mut url = base_url.clone();
    url.set_fragment(id.filter(|s| !s.is_empty()));
    url
}

/// Splits Markdown into heading-anchored chunks without breaking inside code
/// fences or pipe tables. `heading_anchors` maps `(level, heading_text, optional_dom_id)`.
pub fn chunk_markdown(
    markdown: &str,
    base_url: &Url,
    heading_anchors: &[(u8, String, Option<String>)],
    target_tokens: usize,
) -> Vec<Chunk> {
    let trimmed = markdown.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let target = target_tokens.clamp(32, 4096);
    let blocks = parse_blocks(trimmed);
    if blocks.is_empty() {
        return Vec::new();
    }

    let mut anchor_cursor = 0usize;
    let mut find_anchor = |level: u8, title: &str| -> Option<String> {
        let key = normalize_heading_key(title);
        for (idx, (lvl, text, id)) in heading_anchors.iter().enumerate().skip(anchor_cursor) {
            if *lvl == level && normalize_heading_key(text) == key {
                anchor_cursor = idx + 1;
                return id.clone();
            }
        }
        // Fallback: match by normalized heading text anywhere in list
        heading_anchors
            .iter()
            .find(|(_, text, _)| normalize_heading_key(text) == key)
            .and_then(|(_, _, id)| id.clone())
    };

    let mut chunks: Vec<Chunk> = Vec::new();
    let mut stack: Vec<(u8, String, Option<String>)> = Vec::new();
    let mut current_parts: Vec<String> = Vec::new();
    let mut current_tokens: usize = 0;

    let flush = |chunks: &mut Vec<Chunk>,
                 parts: &mut Vec<String>,
                 tokens: &mut usize,
                 stack: &[(u8, String, Option<String>)]| {
        if parts.is_empty() {
            return;
        }
        let body = parts.join("\n\n");
        let tk = estimate_tokens(&body);
        let heading = stack.last().map(|(_, t, _)| t.clone());
        let breadcrumb: Vec<String> = stack.iter().map(|(_, t, _)| t.clone()).collect();
        let anchor_id = stack.iter().rev().find_map(|(_, _, id)| id.as_deref());
        let anchor_url = resolve_anchor_url(base_url, anchor_id);
        let index = chunks.len();
        chunks.push(Chunk {
            index,
            heading,
            breadcrumb,
            anchor_url,
            markdown: body,
            tokens: tk,
            score: None,
        });
        parts.clear();
        *tokens = 0;
    };

    for block in blocks {
        match block {
            Block::Heading { level, title, raw } => {
                // Always start a new chunk at every heading boundary so each
                // section carries its exact heading, breadcrumb, and #anchor.
                flush(&mut chunks, &mut current_parts, &mut current_tokens, &stack);
                while stack.last().is_some_and(|(lvl, _, _)| *lvl >= level) {
                    stack.pop();
                }
                let id = find_anchor(level, &title);
                stack.push((level, title, id));
                current_tokens = estimate_tokens(&raw);
                current_parts.push(raw);
            }
            Block::Atom(atom) => {
                let atom_tokens = estimate_tokens(&atom);
                // Keep a heading attached to its first content block rather than
                // emitting a lone heading chunk, then split cleanly at target budget.
                let only_heading =
                    current_parts.len() == 1 && parse_atx_heading(&current_parts[0]).is_some();
                if !current_parts.is_empty()
                    && !only_heading
                    && current_tokens + atom_tokens > target
                {
                    flush(&mut chunks, &mut current_parts, &mut current_tokens, &stack);
                }
                current_tokens += atom_tokens;
                current_parts.push(atom);
            }
        }
    }
    flush(&mut chunks, &mut current_parts, &mut current_tokens, &stack);
    chunks
}

/// Scores chunks against `focus_query` (if provided) and packs the highest-value
/// chunks within `max_tokens`, restoring original document order.
pub fn distill_chunks(
    mut chunks: Vec<Chunk>,
    focus_query: Option<&str>,
    max_tokens: Option<usize>,
) -> Vec<Chunk> {
    if chunks.is_empty() {
        return chunks;
    }

    let query_terms: Vec<String> = focus_query
        .map(|q| {
            let mut seen = HashSet::new();
            // Include short alphanumeric tokens (e.g. "429", "ECB", "VAT", "TLS")
            // alongside >=4-char normalized stems so status codes and acronyms match.
            q.split(|c: char| !c.is_alphanumeric())
                .filter(|t| {
                    let len = t.chars().count();
                    len >= 3 || (len >= 2 && t.chars().any(|c| c.is_ascii_digit()))
                })
                .map(str::to_lowercase)
                .filter(|t| seen.insert(t.clone()))
                .take(48)
                .collect()
        })
        .unwrap_or_default();

    let numeric_query = focus_query.is_some_and(|q| {
        q.chars().any(|c| c.is_ascii_digit())
            || q.to_ascii_lowercase().split_whitespace().any(|w| {
                matches!(
                    w,
                    "rate" | "rates" | "price" | "limit" | "cost" | "when" | "how" | "percent"
                )
            })
    });

    if !query_terms.is_empty() {
        let headings: Vec<String> = chunks.iter().map(|c| c.breadcrumb.join(" ")).collect();
        let field_docs: Vec<Vec<(&str, f64)>> = chunks
            .iter()
            .zip(&headings)
            .map(|(c, h)| vec![(h.as_str(), 2.5), (c.markdown.as_str(), 1.0)])
            .collect();
        let bm25 = crate::selection::bm25f_scores(&query_terms, &field_docs);
        let fq = focus_query.unwrap_or("");

        for ((chunk, head), bm25_score) in chunks.iter_mut().zip(&headings).zip(bm25) {
            let trigram =
                crate::selection::char_trigram_cosine(fq, &format!("{} {}", head, chunk.markdown));
            let mut score = bm25_score + trigram * 1.5;
            if numeric_query && score > 0.0 && chunk.markdown.chars().any(|c| c.is_ascii_digit()) {
                score *= 1.25;
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let scaled = (score * 100.0).round() as u32;
            chunk.score = Some(scaled);
        }
    }

    let Some(budget) = max_tokens else {
        return chunks;
    };
    let budget = budget.max(32);

    if !query_terms.is_empty() {
        // Rank by score descending (tie-break by document order), pack within budget,
        // then restore original document order.
        let mut order: Vec<usize> = (0..chunks.len()).collect();
        order.sort_by(|&a, &b| {
            chunks[b]
                .score
                .unwrap_or(0)
                .cmp(&chunks[a].score.unwrap_or(0))
                .then_with(|| chunks[a].index.cmp(&chunks[b].index))
        });

        let mut selected = vec![false; chunks.len()];
        let mut used = 0usize;
        for idx in order {
            let tk = chunks[idx].tokens;
            if used + tk <= budget {
                selected[idx] = true;
                used += tk;
            }
        }
        // If even the single best chunk exceeded budget, include a safely truncated slice.
        if !selected.iter().any(|&s| s)
            && let Some(best_idx) = chunks
                .iter()
                .enumerate()
                .max_by_key(|(_, c)| (c.score.unwrap_or(0), std::cmp::Reverse(c.index)))
                .map(|(i, _)| i)
        {
            trim_chunk_to_budget(&mut chunks[best_idx], budget);
            selected[best_idx] = true;
        }
        chunks
            .into_iter()
            .enumerate()
            .filter_map(|(i, c)| selected[i].then_some(c))
            .collect()
    } else {
        // No focus query: pack opening chunks in document order up to budget.
        let mut kept = Vec::new();
        let mut used = 0usize;
        for mut chunk in chunks {
            if used + chunk.tokens <= budget {
                used += chunk.tokens;
                kept.push(chunk);
            } else if kept.is_empty() {
                trim_chunk_to_budget(&mut chunk, budget);
                kept.push(chunk);
                break;
            } else {
                break;
            }
        }
        kept
    }
}

fn trim_chunk_to_budget(chunk: &mut Chunk, budget_tokens: usize) {
    let max_chars = budget_tokens.saturating_mul(4);
    if chunk.markdown.chars().count() > max_chars {
        let cut = crate::truncate(&chunk.markdown, max_chars);
        let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(h, _)| h);
        chunk.markdown = format!("{}…", cut.trim_end());
        chunk.tokens = estimate_tokens(&chunk.markdown).min(budget_tokens);
    }
}

/// Joins selected chunks back into clean Markdown text.
pub fn join_chunks_markdown(chunks: &[Chunk]) -> String {
    chunks
        .iter()
        .map(|c| c.markdown.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Deduplicates identical/syndicated chunks across multiple documents and packs
/// the highest-value chunks globally within `total_max_tokens`, restoring each
/// document's original chunk order.
pub fn budget_evidence_pack(
    doc_chunks: Vec<Vec<Chunk>>,
    focus_query: Option<&str>,
    total_max_tokens: usize,
) -> Vec<Vec<Chunk>> {
    let doc_count = doc_chunks.len();
    if doc_count == 0 {
        return Vec::new();
    }
    let budget = total_max_tokens.max(32);

    // Flatten with (doc_idx, chunk), deduplicating identical normalized bodies
    // across documents while keeping the earliest (highest-ranked document) copy.
    let mut seen_bodies: HashSet<String> = HashSet::new();
    let mut flat: Vec<(usize, Chunk)> = Vec::new();
    for (doc_idx, chunks) in doc_chunks.into_iter().enumerate() {
        let scored = if focus_query.is_some() {
            distill_chunks(chunks, focus_query, None)
        } else {
            chunks
        };
        for chunk in scored {
            let norm: String = chunk
                .markdown
                .split_whitespace()
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>()
                .join(" ");
            if !norm.is_empty() && seen_bodies.insert(norm) {
                flat.push((doc_idx, chunk));
            }
        }
    }

    // Rank globally: query relevance score descending (with gentle document-rank
    // prior so ties favor earlier sources), then pack within `budget`.
    let mut order: Vec<usize> = (0..flat.len()).collect();
    order.sort_by(|&a, &b| {
        let (doc_a, ca) = &flat[a];
        let (doc_b, cb) = &flat[b];
        if focus_query.is_some() {
            cb.score
                .unwrap_or(0)
                .cmp(&ca.score.unwrap_or(0))
                .then_with(|| doc_a.cmp(doc_b))
                .then_with(|| ca.index.cmp(&cb.index))
        } else {
            doc_a.cmp(doc_b).then_with(|| ca.index.cmp(&cb.index))
        }
    });

    let mut selected = vec![false; flat.len()];
    let mut used = 0usize;
    for idx in order {
        let tk = flat[idx].1.tokens;
        if used + tk <= budget {
            selected[idx] = true;
            used += tk;
        }
    }
    if !selected.iter().any(|&s| s)
        && let Some(&first_idx) = (0..flat.len())
            .max_by_key(|&i| {
                (
                    flat[i].1.score.unwrap_or(0),
                    std::cmp::Reverse((flat[i].0, flat[i].1.index)),
                )
            })
            .as_ref()
    {
        trim_chunk_to_budget(&mut flat[first_idx].1, budget);
        selected[first_idx] = true;
    }

    let mut out: Vec<Vec<Chunk>> = vec![Vec::new(); doc_count];
    for (idx, (doc_idx, chunk)) in flat.into_iter().enumerate() {
        if selected[idx] {
            out[doc_idx].push(chunk);
        }
    }
    for chunks in &mut out {
        chunks.sort_by_key(|c| c.index);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_preserve_heading_breadcrumbs_and_exact_dom_anchors() {
        let base = Url::parse("https://example.org/spec").unwrap();
        let md = "\
# HTTP Semantics

Introductory overview of HTTP semantics.

## Status Codes

Overview of response status codes.

### 429 Too Many Requests

The 429 status code indicates the user has sent too many requests in a given amount of time.

### 503 Service Unavailable

The server is currently unable to handle the request due to overload.";

        let anchors = vec![
            (1, "HTTP Semantics".to_owned(), Some("top".to_owned())),
            (
                2,
                "Status Codes".to_owned(),
                Some("status-codes".to_owned()),
            ),
            (
                3,
                "429 Too Many Requests".to_owned(),
                Some("status.429".to_owned()),
            ),
            (
                3,
                "503 Service Unavailable".to_owned(),
                Some("status.503".to_owned()),
            ),
        ];

        let chunks = chunk_markdown(md, &base, &anchors, 256);
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[2].heading.as_deref(), Some("429 Too Many Requests"));
        assert_eq!(
            chunks[2].breadcrumb,
            vec!["HTTP Semantics", "Status Codes", "429 Too Many Requests"]
        );
        assert_eq!(
            chunks[2].anchor_url.as_str(),
            "https://example.org/spec#status.429"
        );
        assert!(chunks[2].tokens > 10);
    }

    #[test]
    fn chunks_never_split_fenced_code_blocks_or_markdown_tables() {
        let base = Url::parse("https://example.org/docs").unwrap();
        let md = "\
# API Reference

Lead paragraph with enough words to fill part of the first chunk cleanly.

```rust
fn compute_rate(principal: f64, rate: f64) -> f64 {
    let adjusted = principal * (1.0 + rate);
    adjusted.round()
}
```

| Tier | Limit | Window |
| --- | --- | --- |
| Free | 60 | 1m |
| Pro | 600 | 1m |
| Enterprise | 6000 | 1m |

Closing note about retry headers and exponential backoff.";

        let chunks = chunk_markdown(md, &base, &[], 40);
        assert!(
            chunks.len() >= 2,
            "expected multiple chunks under small target"
        );
        // Every chunk with a code fence must have balanced fences.
        for c in &chunks {
            let fences = c
                .markdown
                .lines()
                .filter(|l| l.trim_start().starts_with("```"))
                .count();
            assert_eq!(
                fences % 2,
                0,
                "unbalanced code fence in chunk: {}",
                c.markdown
            );
        }
        // The entire table (header + separator + 3 rows) must stay together in one chunk.
        let table_chunk = chunks
            .iter()
            .find(|c| c.markdown.contains("| Tier | Limit | Window |"))
            .expect("table chunk present");
        assert!(table_chunk.markdown.contains("| Enterprise | 6000 | 1m |"));
    }

    #[test]
    fn distill_chunks_selects_query_relevant_sections_within_budget_in_document_order() {
        let base = Url::parse("https://example.org/guide").unwrap();
        let md = "\
# Guide

## Installation

Install the package with cargo install quelle and configure environment variables for production deployments.

## Rate Limiting

The API enforces a rate limit of 120 requests per minute with Retry-After headers when HTTP 429 occurs.

## Billing

Invoices are issued monthly on the first business day in Swiss francs or euros.

## Retry-After Header

Clients must honor the Retry-After delta-seconds value before retrying HTTP 429 responses.";

        let chunks = chunk_markdown(md, &base, &[], 128);
        let distilled = distill_chunks(chunks, Some("HTTP 429 Retry-After rate limit"), Some(70));
        let total_tokens: usize = distilled.iter().map(|c| c.tokens).sum();
        assert!(total_tokens <= 70, "exceeded token budget: {total_tokens}");
        assert!(
            distilled
                .iter()
                .any(|c| c.markdown.contains("120 requests per minute")),
            "missed Rate Limiting section"
        );
        assert!(
            distilled
                .iter()
                .any(|c| c.markdown.contains("delta-seconds")),
            "missed Retry-After section"
        );
        assert!(
            !distilled
                .iter()
                .any(|c| c.markdown.contains("Invoices are issued monthly")),
            "irrelevant Billing section should be dropped under tight budget"
        );
        // Must preserve original document order.
        let indices: Vec<usize> = distilled.iter().map(|c| c.index).collect();
        let mut sorted = indices.clone();
        sorted.sort_unstable();
        assert_eq!(indices, sorted);
    }

    #[test]
    fn budget_evidence_pack_deduplicates_across_docs_and_respects_global_token_cap() {
        let u1 = Url::parse("https://doc1.example.org/a").unwrap();
        let u2 = Url::parse("https://doc2.example.org/b").unwrap();
        let dup_text =
            "The 429 status code indicates the client sent too many requests in a given window.";
        let unique1 = "Retry-After header specifies exact delta-seconds before retrying HTTP 429.";
        let unique2 =
            "Unrelated marketing footer boilerplate about quarterly webinars and newsletters.";

        let c1 = chunk_markdown(
            &format!("# Rate Limits\n\n{dup_text}\n\n## Retry-After\n\n{unique1}"),
            &u1,
            &[],
            64,
        );
        let c2 = chunk_markdown(
            &format!("# Rate Limits\n\n{dup_text}\n\n## Webinars\n\n{unique2}"),
            &u2,
            &[],
            64,
        );

        let packed = budget_evidence_pack(vec![c1, c2], Some("HTTP 429 Retry-After"), 60);
        assert_eq!(packed.len(), 2);
        let all: Vec<&Chunk> = packed.iter().flat_map(|v| v.iter()).collect();
        let total_tokens: usize = all.iter().map(|c| c.tokens).sum();
        assert!(total_tokens <= 60, "global budget exceeded: {total_tokens}");
        // Duplicate chunk must appear at most once across all documents.
        let dup_count = all
            .iter()
            .filter(|c| c.markdown.contains("too many requests"))
            .count();
        assert_eq!(
            dup_count, 1,
            "expected cross-doc duplicate chunk to be deduplicated"
        );
        // High-relevance Retry-After chunk must survive while unrelated webinar chunk is dropped.
        assert!(all.iter().any(|c| c.markdown.contains("delta-seconds")));
        assert!(
            !all.iter()
                .any(|c| c.markdown.contains("quarterly webinars"))
        );
    }

    #[test]
    fn distill_chunks_bm25f_prefers_dense_rare_term_section_over_lengthy_filler() {
        let base = Url::parse("https://example.org/sqlite").unwrap();
        let filler = "This long operational overview discusses general database storage patterns and configuration basics. ".repeat(12);
        let md = format!(
            "# SQLite Overview\n\n## General Storage\n\n{filler} checkpoint.\n\n## WAL Checkpointing\n\nSQLite WAL mode uses wal_autocheckpoint and PRAGMA wal_checkpoint(TRUNCATE) to merge frames back into the main database file."
        );
        let chunks = chunk_markdown(&md, &base, &[], 200);
        let scored = distill_chunks(chunks, Some("SQLite WAL wal_autocheckpoint"), None);
        let general = scored
            .iter()
            .find(|c| c.heading.as_deref() == Some("General Storage"))
            .unwrap();
        let wal = scored
            .iter()
            .find(|c| c.heading.as_deref() == Some("WAL Checkpointing"))
            .unwrap();
        assert!(
            wal.score.unwrap_or(0) > general.score.unwrap_or(0) * 2,
            "BM25F should strongly prefer dense heading+body WAL chunk ({:?}) over long filler ({:?})",
            wal.score,
            general.score
        );
    }
}
