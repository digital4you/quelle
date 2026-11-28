//! Cheap candidate selection and evidence deduplication, without model calls.
use crate::discovery::{Candidate, Language};
use regex::Regex;
use std::{
    collections::{HashMap, HashSet},
    sync::LazyLock,
};
use url::Url;

pub fn url_key(url: &Url) -> String {
    let mut url = url.clone();
    url.set_fragment(None);
    let pairs: Vec<_> = url
        .query_pairs()
        .filter(|(k, _)| {
            !k.starts_with("utm_") && !matches!(k.as_ref(), "fbclid" | "gclid" | "msclkid")
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    url.set_query(None);
    if !pairs.is_empty() {
        url.query_pairs_mut().extend_pairs(pairs);
    }
    url.into()
}

pub fn matches_domain(url: &Url, domains: &[String]) -> bool {
    url.host_str().is_some_and(|host| {
        domains
            .iter()
            .any(|d| host == d || host.ends_with(&format!(".{d}")))
    })
}

pub fn valid_domain(domain: &str) -> bool {
    domain.len() <= 253
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && domain.split('.').all(|s| {
            !s.is_empty()
                && s.len() <= 63
                && !s.starts_with('-')
                && !s.ends_with('-')
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

pub fn followup(query: &str, domains: &[String]) -> Option<String> {
    if domains.is_empty() || query.contains("site:") {
        return None;
    }
    let sites = domains
        .iter()
        .take(3)
        .map(|d| format!("site:{d}"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let result = format!("{query} ({sites})");
    (result.chars().count() <= 600).then_some(result)
}

fn doc_version(url: &Url) -> Option<&str> {
    let mut segments = url.path_segments()?;
    while let Some(segment) = segments.next() {
        if segment == "docs" {
            return segments.next().filter(|v| {
                !v.is_empty()
                    && (*v == "current"
                        || *v == "devel"
                        || v.chars().all(|c| c.is_ascii_digit() || c == '.'))
            });
        }
    }
    None
}

/// Metadata preferences remain ahead of semantic scores. They never filter
/// candidates: sparse searches and fallback paths retain every result.
pub fn constrain(query: &str, mut hits: Vec<Candidate>, domains: &[String]) -> Vec<Candidate> {
    static NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\d+(?:\.\d+)*\b").unwrap());
    static PREVIEW: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:alpha|beta|preview|release candidate|rc[ -]?\d+)\b").unwrap()
    });
    let numbers: HashSet<_> = NUMBER.find_iter(query).map(|m| m.as_str()).collect();
    let explicit_version = hits
        .iter()
        .any(|h| doc_version(&h.url).is_some_and(|v| numbers.contains(v)));
    let current_available =
        numbers.is_empty() && hits.iter().any(|h| doc_version(&h.url) == Some("current"));
    let release_query = query.to_ascii_lowercase().contains("release");
    let wants_preview = PREVIEW.is_match(query) || query.contains("devel");
    hits.sort_by_key(|h| {
        let version = doc_version(&h.url);
        let old_version = version.is_some_and(|v| {
            (explicit_version && !numbers.contains(v)) || (current_available && v != "current")
        });
        let preview = !wants_preview
            && (version == Some("devel")
                || (release_query && PREVIEW.is_match(&format!("{} {}", h.title, h.url.path()))));
        (
            !domains.is_empty() && !matches_domain(&h.url, domains),
            preview,
            old_version,
        )
    });
    hits
}

/// Tokenizes text into normalized lowercase terms suitable for BM25F scoring
/// (includes >=3-char alphanumeric tokens and >=2-char tokens containing a digit,
/// preserving status codes like "429" or protocols like "h3").
pub(crate) fn bm25_tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| {
            let len = t.chars().count();
            len >= 3 || (len >= 2 && t.chars().any(|c| c.is_ascii_digit()))
        })
        .map(str::to_lowercase)
        .collect()
}

fn term_matches(token: &str, query_term: &str) -> bool {
    if token == query_term {
        return true;
    }
    if query_term.len() >= 4 && token.len() >= 4 {
        return token.starts_with(query_term) || query_term.starts_with(token);
    }
    false
}

/// Computes pure-Rust BM25F relevance scores across a pool of multi-field documents.
/// Each document is represented as a slice of `(field_text, field_weight)` pairs.
pub(crate) fn bm25f_scores(query_terms: &[String], docs: &[Vec<(&str, f64)>]) -> Vec<f64> {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;

    let n_docs = docs.len();
    if n_docs == 0 || query_terms.is_empty() {
        return vec![0.0; n_docs];
    }

    let max_fields = docs.iter().map(Vec::len).max().unwrap_or(0);
    if max_fields == 0 {
        return vec![0.0; n_docs];
    }

    // Tokenize each field once per document and compute per-field average lengths.
    let mut field_len_sums = vec![0usize; max_fields];
    let mut field_counts = vec![0usize; max_fields];
    let tokenized: Vec<Vec<(Vec<String>, f64)>> = docs
        .iter()
        .map(|fields| {
            fields
                .iter()
                .enumerate()
                .map(|(f_idx, (text, weight))| {
                    let toks = bm25_tokens(text);
                    field_len_sums[f_idx] += toks.len();
                    field_counts[f_idx] += 1;
                    (toks, *weight)
                })
                .collect()
        })
        .collect();

    #[allow(clippy::cast_precision_loss)]
    let avg_lens: Vec<f64> = (0..max_fields)
        .map(|f| {
            let denom = field_counts[f].max(1) as f64;
            (field_len_sums[f] as f64 / denom).max(1.0)
        })
        .collect();

    #[allow(clippy::cast_precision_loss)]
    let n_f = n_docs as f64;
    let idfs: Vec<f64> = query_terms
        .iter()
        .map(|qt| {
            let df = tokenized
                .iter()
                .filter(|doc_fields| {
                    doc_fields
                        .iter()
                        .any(|(toks, _)| toks.iter().any(|tok| term_matches(tok, qt)))
                })
                .count();
            if df == 0 {
                0.0
            } else {
                #[allow(clippy::cast_precision_loss)]
                let df_f = df as f64;
                (((n_f - df_f + 0.5) / (df_f + 0.5)) + 1.0).ln().max(0.05)
            }
        })
        .collect();

    tokenized
        .iter()
        .map(|doc_fields| {
            let mut score = 0.0f64;
            for (qt, &idf) in query_terms.iter().zip(&idfs) {
                if idf <= 0.0 {
                    continue;
                }
                let mut tf_weighted = 0.0f64;
                for (f_idx, (toks, weight)) in doc_fields.iter().enumerate() {
                    if toks.is_empty() || *weight <= 0.0 {
                        continue;
                    }
                    let raw_tf = toks.iter().filter(|tok| term_matches(tok, qt)).count();
                    if raw_tf == 0 {
                        continue;
                    }
                    #[allow(clippy::cast_precision_loss)]
                    let dl = toks.len() as f64;
                    let norm = 1.0 - B + B * (dl / avg_lens[f_idx]);
                    #[allow(clippy::cast_precision_loss)]
                    let tf_f = raw_tf as f64;
                    tf_weighted += *weight * (tf_f / norm.max(0.25));
                }
                if tf_weighted > 0.0 {
                    score += idf * (tf_weighted * (K1 + 1.0)) / (tf_weighted + K1);
                }
            }
            score
        })
        .collect()
}

/// Computes normalized character-trigram cosine similarity in `[0.0, 1.0]`
/// for lightweight morphological, compound-word, and paraphrase matching
/// across EN/DE/FR/IT without any ML runtime or external crate.
pub(crate) fn char_trigram_cosine(a: &str, b: &str) -> f64 {
    fn trigram_counts(s: &str) -> HashMap<[char; 3], u32> {
        let chars: Vec<char> = s
            .chars()
            .take(3000)
            .flat_map(char::to_lowercase)
            .map(|c| if c.is_alphanumeric() { c } else { ' ' })
            .collect();
        let mut counts = HashMap::new();
        for w in chars.windows(3) {
            if w[0] == ' ' && w[1] == ' ' {
                continue;
            }
            *counts.entry([w[0], w[1], w[2]]).or_insert(0) += 1;
        }
        counts
    }
    let va = trigram_counts(a);
    let vb = trigram_counts(b);
    if va.is_empty() || vb.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (k, &ca) in &va {
        let fa = f64::from(ca);
        norm_a += fa * fa;
        if let Some(&cb) = vb.get(k) {
            dot += fa * f64::from(cb);
        }
    }
    for &cb in vb.values() {
        let fb = f64::from(cb);
        norm_b += fb * fb;
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom > 0.0 { dot / denom } else { 0.0 }
}

pub fn order(
    query: &str,
    language: Language,
    hits: Vec<Candidate>,
    domains: &[String],
) -> Vec<Candidate> {
    static NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\d+(?:\.\d+)*\b").unwrap());
    let mut seen_q = HashSet::new();
    let query_terms: Vec<String> = bm25_tokens(query)
        .into_iter()
        .filter(|t| seen_q.insert(t.clone()))
        .take(48)
        .collect();
    let numbers: HashSet<_> = NUMBER.find_iter(query).map(|m| m.as_str()).collect();
    let lang = match language {
        Language::De => "de",
        Language::Fr => "fr",
        Language::It => "it",
        Language::En => "en",
    };
    let mut seen = HashSet::new();
    let deduped: Vec<(usize, Candidate)> = hits
        .into_iter()
        .enumerate()
        .filter(|(_, h)| seen.insert(url_key(&h.url)))
        .collect();

    let field_docs: Vec<Vec<(&str, f64)>> = deduped
        .iter()
        .map(|(_, h)| {
            vec![
                (h.title.as_str(), 3.0),
                (h.url.path(), 1.5),
                (h.description.as_str(), 1.0),
            ]
        })
        .collect();
    let bm25 = bm25f_scores(&query_terms, &field_docs);

    let mut scored: Vec<(f64, usize, Candidate)> = deduped
        .into_iter()
        .zip(bm25)
        .map(|((position, hit), bm25_score)| {
            let text =
                format!("{} {} {}", hit.title, hit.description, hit.url.path()).to_lowercase();
            let trigram = char_trigram_cosine(query, &format!("{} {}", hit.title, hit.description));
            // Keep engine rank as a strong prior alongside BM25F + char-trigram similarity.
            #[allow(clippy::cast_precision_loss)]
            let rank_prior = (20_usize.saturating_sub(position) * 5) as f64;
            let mut score = rank_prior + bm25_score * 8.0 + trigram * 16.0;
            if matches_domain(&hit.url, domains) {
                score += 40.0;
            }
            for n in &numbers {
                if NUMBER.find_iter(&text).any(|m| m.as_str() == *n) {
                    score += 14.0;
                }
            }
            let segments: Vec<_> = hit.url.path_segments().into_iter().flatten().collect();
            if segments
                .iter()
                .any(|p| *p == lang || p.starts_with(&format!("{lang}-")))
            {
                score += 5.0;
            }
            if segments
                .iter()
                .any(|p| ["de", "fr", "it", "en", "ru", "ja", "zh"].contains(p) && *p != lang)
            {
                score -= 12.0;
            }
            if numbers.is_empty()
                && segments
                    .iter()
                    .any(|p| p.starts_with(|c: char| c.is_ascii_digit()) && p.contains('.'))
            {
                score -= 12.0;
            }
            if numbers.is_empty() && segments.contains(&"current") {
                score += 8.0;
            }
            (score, position, hit)
        })
        .collect();
    let mut counts = HashMap::<String, i32>::new();
    let mut result = Vec::new();
    while !scored.is_empty() {
        let best = (0..scored.len())
            .max_by(|&a, &b| {
                let (sa, pos_a, hit_a) = &scored[a];
                let (sb, pos_b, hit_b) = &scored[b];
                let pen_a = f64::from(
                    counts
                        .get(hit_a.url.host_str().unwrap_or(""))
                        .copied()
                        .unwrap_or(0)
                        * 3,
                );
                let pen_b = f64::from(
                    counts
                        .get(hit_b.url.host_str().unwrap_or(""))
                        .copied()
                        .unwrap_or(0)
                        * 3,
                );
                (sa - pen_a)
                    .total_cmp(&(sb - pen_b))
                    .then_with(|| pos_b.cmp(pos_a))
            })
            .unwrap();
        let (_, _, hit) = scored.remove(best);
        *counts
            .entry(hit.url.host_str().unwrap_or("").into())
            .or_default() += 1;
        result.push(hit);
    }
    constrain(query, result, domains)
}

#[derive(Default)]
pub struct EvidenceSet {
    urls: HashSet<String>,
    fingerprints: Vec<HashSet<u64>>,
}

impl EvidenceSet {
    pub fn insert(&mut self, url: &Url, text: &str) -> bool {
        use std::hash::{Hash, Hasher};
        let key = url_key(url);
        if self.urls.contains(&key) {
            return false;
        }
        let words: Vec<_> = text
            .split_whitespace()
            .take(50_000)
            .map(str::to_lowercase)
            .collect();
        let hashes: HashSet<_> = words
            .chunks(5)
            .map(|chunk| {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                chunk.hash(&mut h);
                h.finish()
            })
            .collect();
        if !hashes.is_empty()
            && self.fingerprints.iter().any(|prior| {
                let intersection = prior.intersection(&hashes).count();
                let union = prior.len() + hashes.len() - intersection;
                union > 0 && intersection as f64 / union as f64 >= 0.94
            })
        {
            return false;
        }
        self.urls.insert(key);
        self.fingerprints.push(hashes);
        true
    }
}

#[cfg(test)]
mod tests;
