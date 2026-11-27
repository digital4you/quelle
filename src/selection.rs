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

pub fn order(
    query: &str,
    language: Language,
    hits: Vec<Candidate>,
    domains: &[String],
) -> Vec<Candidate> {
    static NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\d+(?:\.\d+)*\b").unwrap());
    let terms: HashSet<_> = crate::pages::words(query).collect();
    let numbers: HashSet<_> = NUMBER.find_iter(query).map(|m| m.as_str()).collect();
    let lang = match language {
        Language::De => "de",
        Language::Fr => "fr",
        Language::It => "it",
        Language::En => "en",
    };
    let mut seen = HashSet::new();
    let mut scored: Vec<_> = hits
        .into_iter()
        .enumerate()
        .filter(|(_, h)| seen.insert(url_key(&h.url)))
        .map(|(position, hit)| {
            let text =
                format!("{} {} {}", hit.title, hit.description, hit.url.path()).to_lowercase();
            // Snippet term counts alone reward repetition and can displace the
            // actual release with a preview. Keep engine rank as a strong prior.
            let mut score = (20_usize.saturating_sub(position) * 6) as i32
                + terms.iter().filter(|t| text.contains(t.as_str())).count() as i32 * 4;
            if matches_domain(&hit.url, domains) {
                score += 40;
            }
            for n in &numbers {
                if NUMBER.find_iter(&text).any(|m| m.as_str() == *n) {
                    score += 14;
                }
            }
            let segments: Vec<_> = hit.url.path_segments().into_iter().flatten().collect();
            if segments
                .iter()
                .any(|p| *p == lang || p.starts_with(&format!("{lang}-")))
            {
                score += 5;
            }
            if segments
                .iter()
                .any(|p| ["de", "fr", "it", "en", "ru", "ja", "zh"].contains(p) && *p != lang)
            {
                score -= 12;
            }
            if numbers.is_empty()
                && segments
                    .iter()
                    .any(|p| p.starts_with(|c: char| c.is_ascii_digit()) && p.contains('.'))
            {
                score -= 12;
            }
            if numbers.is_empty() && segments.contains(&"current") {
                score += 8;
            }
            (score, position, hit)
        })
        .collect();
    let mut counts = HashMap::<String, i32>::new();
    let mut result = Vec::new();
    while !scored.is_empty() {
        let best = (0..scored.len())
            .max_by_key(|i| {
                let (score, position, hit) = &scored[*i];
                let penalty = counts
                    .get(hit.url.host_str().unwrap_or(""))
                    .copied()
                    .unwrap_or(0)
                    * 3;
                (score - penalty, std::cmp::Reverse(*position))
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
