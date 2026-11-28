//! Pure-Rust, zero-dependency multilingual query intent router.
//!
//! Loads compact int8 weights (`router_weights.bin`, ~138 KiB) distilled from
//! MIT-licensed `minishlab/potion-multilingual-128M` (projected top-6000
//! multilingual token table) combined with a 4096-bucket FNV-1a hashed
//! char(3..5)/word n-gram linear head. Runs in ~20 microseconds on CPU across
//! English, German, French, and Italian with zero external ML runtime.

use std::{collections::HashMap, sync::OnceLock};

use super::providers::extract_rfc_number;

const NUM_CLASSES: usize = 8;
const THRESHOLD: f32 = 0.24;
const GENERAL_MARGIN: f32 = 0.01;

static RAW_WEIGHTS: &[u8] = include_bytes!("router_weights.bin");
static ROUTER: OnceLock<RouterModel> = OnceLock::new();

#[derive(Clone, Copy, Debug)]
pub(super) struct IntentScores {
    pub biomed: f32,
    pub ietf: f32,
    pub web: f32,
    pub academic: f32,
    pub news: f32,
    pub tech: f32,
    pub repo: f32,
    pub general: f32,
}

impl IntentScores {
    fn active(&self, score: f32) -> bool {
        score >= THRESHOLD && score > self.general + GENERAL_MARGIN
    }

    pub(super) fn has_biomed(&self) -> bool {
        self.active(self.biomed)
    }

    pub(super) fn has_ietf(&self) -> bool {
        self.active(self.ietf)
    }

    pub(super) fn has_web(&self) -> bool {
        self.active(self.web)
    }

    pub(super) fn has_academic(&self) -> bool {
        self.active(self.academic)
    }

    pub(super) fn has_news(&self) -> bool {
        self.active(self.news)
    }

    pub(super) fn has_repo(&self) -> bool {
        self.active(self.repo)
    }

    pub(super) fn has_tech(&self, query: &str) -> bool {
        self.active(self.tech)
            || self.has_ietf()
            || self.has_web()
            || self.has_repo()
            || query.contains("::")
            || query.contains("--")
            || query.contains('_')
    }
}

struct RouterModel {
    tok_scale: f32,
    ngram_scale: f32,
    ngram_buckets: usize,
    bias: [f32; NUM_CLASSES],
    /// Sorted `(utf8_token, [i8; 8])` for zero-overhead O(log N) binary search.
    tokens: Vec<(String, [i8; NUM_CLASSES])>,
    /// Flat `ngram_buckets * 8` int8 table.
    ngrams: Vec<[i8; NUM_CLASSES]>,
}

impl RouterModel {
    fn parse(bytes: &[u8]) -> Self {
        assert!(
            bytes.len() >= 22 && &bytes[0..4] == b"QRT1",
            "invalid QRT1 router header"
        );
        let mut pos = 4usize;
        let read_u16 = |p: &mut usize| -> u16 {
            let v = u16::from_le_bytes([bytes[*p], bytes[*p + 1]]);
            *p += 2;
            v
        };
        let read_u32 = |p: &mut usize| -> u32 {
            let v = u32::from_le_bytes([bytes[*p], bytes[*p + 1], bytes[*p + 2], bytes[*p + 3]]);
            *p += 4;
            v
        };
        let read_f32 = |p: &mut usize| -> f32 {
            let v = f32::from_le_bytes([bytes[*p], bytes[*p + 1], bytes[*p + 2], bytes[*p + 3]]);
            *p += 4;
            v
        };

        let num_classes = usize::from(read_u16(&mut pos));
        assert_eq!(num_classes, NUM_CLASSES, "unexpected router class count");
        let tok_scale = read_f32(&mut pos);
        let ngram_scale = read_f32(&mut pos);
        let ngram_buckets = read_u32(&mut pos) as usize;
        let num_tokens = read_u32(&mut pos) as usize;

        let mut bias = [0.0f32; NUM_CLASSES];
        for slot in &mut bias {
            *slot = read_f32(&mut pos);
        }

        let mut tokens = Vec::with_capacity(num_tokens);
        for _ in 0..num_tokens {
            let key_len = usize::from(read_u16(&mut pos));
            let key = std::str::from_utf8(&bytes[pos..pos + key_len])
                .expect("valid UTF-8 router token")
                .to_owned();
            pos += key_len;
            let mut weights = [0i8; NUM_CLASSES];
            for w in &mut weights {
                *w = bytes[pos] as i8;
                pos += 1;
            }
            tokens.push((key, weights));
        }

        let mut ngrams = Vec::with_capacity(ngram_buckets);
        for _ in 0..ngram_buckets {
            let mut weights = [0i8; NUM_CLASSES];
            for w in &mut weights {
                *w = bytes[pos] as i8;
                pos += 1;
            }
            ngrams.push(weights);
        }

        Self {
            tok_scale,
            ngram_scale,
            ngram_buckets,
            bias,
            tokens,
            ngrams,
        }
    }

    fn lookup_token(&self, token: &str) -> Option<&[i8; NUM_CLASSES]> {
        self.tokens
            .binary_search_by(|(k, _)| k.as_str().cmp(token))
            .ok()
            .map(|idx| &self.tokens[idx].1)
    }

    fn score(&self, query: &str) -> IntentScores {
        let words = tokenize_words(query);
        let mut tok_acc = [0.0f32; NUM_CLASSES];
        let mut tok_hits = 0usize;

        for word in &words {
            if let Some(w) = self.lookup_token(word) {
                for (acc, &qi) in tok_acc.iter_mut().zip(w.iter()) {
                    *acc += f32::from(qi) * self.tok_scale;
                }
                tok_hits += 1;
            } else if word.chars().count() >= 6 {
                // Compound stem prefix fallback (e.g. German compounds like Bluthochdruck/Zinsentscheidung).
                let char_indices: Vec<usize> = word.char_indices().map(|(i, _)| i).collect();
                for k in (5..char_indices.len()).rev() {
                    let prefix = &word[..char_indices[k]];
                    if let Some(w) = self.lookup_token(prefix) {
                        for (acc, &qi) in tok_acc.iter_mut().zip(w.iter()) {
                            *acc += f32::from(qi) * self.tok_scale * 0.7;
                        }
                        tok_hits += 1;
                        break;
                    }
                }
            }
        }
        if tok_hits > 0 {
            let denom = (tok_hits as f32).sqrt();
            for acc in &mut tok_acc {
                *acc /= denom;
            }
        }

        let mut ng_acc = [0.0f32; NUM_CLASSES];
        for (bucket, weight) in ngram_features(query, self.ngram_buckets) {
            let row = &self.ngrams[bucket];
            for (acc, &qi) in ng_acc.iter_mut().zip(row.iter()) {
                *acc += weight * (f32::from(qi) * self.ngram_scale);
            }
        }

        let mut combined = [0.0f32; NUM_CLASSES];
        for i in 0..NUM_CLASSES {
            combined[i] = self.bias[i] + 0.65 * tok_acc[i] + 0.55 * ng_acc[i];
        }

        // Deterministic syntax guarantee: explicit `RFC <number>` always activates IETF.
        if extract_rfc_number(query).is_some() {
            combined[1] = combined[1].max(0.85);
        }

        IntentScores {
            biomed: combined[0],
            ietf: combined[1],
            web: combined[2],
            academic: combined[3],
            news: combined[4],
            tech: combined[5],
            repo: combined[6],
            general: combined[7],
        }
    }
}

fn model() -> &'static RouterModel {
    ROUTER.get_or_init(|| RouterModel::parse(RAW_WEIGHTS))
}

pub(super) fn classify(query: &str) -> IntentScores {
    model().score(query)
}

fn fnv1a_32(prefix: &[u8], data: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in prefix.iter().chain(data.iter()) {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            'ä' | 'ö' | 'ü' | 'ß' | 'é' | 'è' | 'ê' | 'à' | 'ç' | 'ï' | 'î' | 'ô' | 'ù'
        )
}

fn is_connector(c: char) -> bool {
    matches!(c, '-' | '/' | '.')
}

fn tokenize_words(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !is_word_char(chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_word_char(chars[i]) {
            i += 1;
        }
        while i + 1 < chars.len() && is_connector(chars[i]) && chars[i + 1].is_ascii_alphanumeric()
        {
            i += 1;
            while i < chars.len() && chars[i].is_ascii_alphanumeric() {
                i += 1;
            }
        }
        out.push(chars[start..i].iter().collect());
    }
    out
}

fn ngram_features(text: &str, buckets: usize) -> HashMap<usize, f32> {
    let collapsed = text
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let norm = format!(" {collapsed} ");
    let mut counts: HashMap<usize, f32> = HashMap::new();

    for tok in tokenize_words(&norm) {
        let b = (fnv1a_32(b"w:", tok.as_bytes()) as usize) % buckets;
        *counts.entry(b).or_insert(0.0) += 1.5;
    }

    let chars: Vec<char> = norm.chars().collect();
    let mut gram = String::with_capacity(20);
    for n in [3usize, 4, 5] {
        if chars.len() < n {
            continue;
        }
        for i in 0..=chars.len() - n {
            gram.clear();
            for &ch in &chars[i..i + n] {
                gram.push(ch);
            }
            let b = (fnv1a_32(b"c:", gram.as_bytes()) as usize) % buckets;
            *counts.entry(b).or_insert(0.0) += 0.45;
        }
    }

    let l2 = counts.values().map(|v| v * v).sum::<f32>().sqrt();
    if l2 > 0.0 {
        for v in counts.values_mut() {
            *v /= l2;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_rust_router_classifies_multilingual_paraphrases_with_zero_general_false_positives() {
        // Biomedical across EN/DE/FR/IT including drugs NOT in old regexes!
        for q in [
            "mRNA vaccine clinical trial efficacy",
            "ibuprofen side effects stomach ulcer risk",
            "Nebenwirkungen Paracetamol Leber Dosierung",
            "effets secondaires aspirine estomac posologie",
            "effetti collaterali antibiotico amoxicillina dosaggio",
        ] {
            let s = classify(q);
            assert!(s.has_biomed(), "expected biomed for '{q}': {s:?}");
        }

        // News across EN/DE/FR/IT
        for q in [
            "latest ECB interest rate decision press release",
            "neueste Zinsentscheidung Schweizerische Nationalbank heute",
            "communiqué de presse banque centrale décision taux aujourd'hui",
            "ultime notizie elezioni risultati oggi governo",
        ] {
            let s = classify(q);
            assert!(s.has_news(), "expected news for '{q}': {s:?}");
        }

        // IETF / Web / Academic / Repo
        assert!(classify("RFC 9110 HTTP 429 Too Many Requests Retry-After").has_ietf());
        assert!(classify("RFC 9114 HTTP/3 QUIC transport").has_ietf());
        assert!(classify("WAI-ARIA aria-expanded accordion disclosure pattern").has_web());
        assert!(classify("how to center a div vertically with CSS flexbox").has_web());
        assert!(classify("Attention Is All You Need transformer paper").has_academic());
        assert!(
            classify("étude empirique impact intelligence artificielle productivité")
                .has_academic()
        );
        assert!(classify("lightpanda github repository release changelog").has_repo());

        // General queries across EN/DE/FR/IT MUST NOT trigger specialists!
        for q in [
            "Schweiz MWST Steuersätze 2024 ESTV",
            "Schweiz Datenschutzgesetz DSG 2023 Fedlex",
            "best time of year to visit Zermatt Matterhorn",
            "horaires CFF train Genève Lausanne billet",
            "ricetta risotto ai funghi porcini tradizionale",
            "Öffnungszeiten Kunsthaus Zürich Eintrittspreise",
        ] {
            let s = classify(q);
            assert!(
                !s.has_biomed()
                    && !s.has_ietf()
                    && !s.has_web()
                    && !s.has_academic()
                    && !s.has_news()
                    && !s.has_repo()
                    && !s.has_tech(q),
                "unexpected specialist trigger on general query '{q}': {s:?}"
            );
        }
    }
}
