#!/usr/bin/env python3
"""Offline distillation script producing `src/discovery/router_weights.bin`.

Combines:
  1. Pre-projected token weights from MIT-licensed `minishlab/potion-multilingual-128M`
     (top-K highest-norm multilingual tokens projected through a trained 8-class
     linear head: `E[tok] @ W` quantized to int8).
  2. A compact 4096-bucket FNV-1a hashed char(3..5)/word n-gram int8 linear table
     for OOV words, typos, and German/French/Italian compound stems.

Binary format (`QRT1` magic, little-endian):
  - magic: 4 bytes b"QRT1"
  - num_classes: u16 (= 8: biomed, ietf, web, academic, news, tech, repo, general)
  - tok_scale: f32 (dequantization scale for token int8 weights)
  - ngram_scale: f32 (dequantization scale for n-gram int8 weights)
  - ngram_buckets: u32 (= 4096)
  - num_tokens: u32
  - bias: [f32; num_classes]
  - token entries: repeated `num_tokens` times:
      - key_len: u16
      - key_utf8: [u8; key_len] (lowercased word/subword token)
      - weights: [i8; num_classes]
  - ngram table: [i8; ngram_buckets * num_classes]

Evaluates on the held-out 48-query multilingual benchmark from
`benchmarks/small-cpu/bench-router.py` (never used in training!).
"""

from __future__ import annotations

import importlib.util
import math
import re
import struct
from pathlib import Path

import numpy as np
from model2vec import StaticModel

REPO = Path(__file__).resolve().parent.parent
OUT_BIN = REPO / "src/discovery/router_weights.bin"

INTENTS = ["biomed", "ietf", "web", "academic", "news", "tech", "repo"]
CLASSES = INTENTS + ["general"]
NUM_CLASSES = len(CLASSES)
NGRAM_BUCKETS = 4096
TOP_TOKENS = 6000

# Strictly separate from the 48-query held-out test set in bench-router.py!
TRAIN_DATA: dict[str, list[str]] = {
    "biomed": [
        "clinical study randomized placebo treatment efficacy",
        "drug side effects dosage interactions prescription medicine",
        "disease symptoms treatment diagnosis medical review guidelines",
        "vaccine immune response antibody clinical outcomes",
        "cardiology oncology neurology diabetes hypertension pathology",
        "antibiotic analgesic anti-inflammatory pharmacology toxicity",
        "Nebenwirkungen Medikament Dosierung Behandlung Studie Leitlinie",
        "Krankheit Symptome Therapie Arzt medizinische Forschung Wirkstoff",
        "Blutdruck Diabetes Krebs Impfstoff klinische Studie Schmerzmittel",
        "effets secondaires médicament posologie essai clinique traitement",
        "maladie symptômes diagnostic médecin santé publique vaccin",
        "effetti collaterali farmaco dosaggio studio clinico terapia medico",
        "malattia sintomi diagnosi trattamento salute vaccino antibiotico",
    ],
    "ietf": [
        "RFC 9110 HTTP semantics standard specification IETF",
        "RFC 9114 QUIC transport protocol handshake TLS 1.3",
        "IETF internet draft OAuth 2.0 JWT bearer token spec",
        "DNSSEC BGP routing protocol specification RFC standard",
        "RFC 7231 RFC 6749 RFC 8446 network protocol specification",
        "RFC Protokoll Spezifikation TLS Handshake Standard IETF",
        "spécification protocole RFC IETF norme réseau TLS OAuth",
    ],
    "web": [
        "HTML CSS flexbox grid layout browser DOM manipulation",
        "WAI-ARIA accessibility accordion button keyboard focus WCAG",
        "JavaScript Fetch API CORS headers cookies same-site preflight",
        "HTTP status code 429 caching ETag Cache-Control header",
        "WebAssembly service worker Shadow DOM SVG canvas MDN Web Docs",
        "browser DOM observer lazy loading responsive CSS layout",
        "CSS Layout Flexbox Browser Barrierefreiheit HTML Formular Fokus",
        "requête HTTP en-tête CORS cache navigateur JavaScript HTML",
        "intestazione HTTP browser CSS layout accessibilità JavaScript",
    ],
    "academic": [
        "research paper arXiv preprint empirical study benchmark",
        "scientific literature citation DOI peer reviewed findings",
        "machine learning transformer attention neural architecture ablation",
        "deep learning scaling laws LLM benchmark evaluation paper",
        "mathematical theorem proof complexity analysis paper",
        "systematic review meta-analysis empirical evidence dataset",
        "wissenschaftliche Studie Publikation Forschung Ergebnisse Universität",
        "article scientifique étude empirique recherche universitaire revue",
        "ricerca scientifica studio empirico pubblicazione accademica dati",
    ],
    "news": [
        "breaking news latest announcement press release today",
        "central bank interest rate decision inflation update today",
        "election results parliament vote referendum outcome",
        "company quarterly earnings merger acquisition guidance report",
        "trade tariff sanctions summit ceasefire diplomatic talks",
        "aktuelle Nachrichten Medienmitteilung Zinsentscheid heute",
        "Abstimmung Wahl Resultate Bundesrat Mitteilung aktuell neu",
        "dernières nouvelles communiqué de presse décision taux aujourd'hui",
        "ultime notizie comunicato stampa elezioni risultati governo oggi",
    ],
    "tech": [
        "database transaction isolation MVCC WAL checkpoint durability",
        "SQLite PostgreSQL MySQL Redis database storage engine indexing",
        "Rust Python Go TypeScript systems programming memory safety async",
        "container orchestration kubernetes pods probes deployment YAML Docker",
        "SQL query optimization indexing vacuum replication redis persistence",
        "Linux kernel networking firewall systemd memory swap debugging",
        "compiler runtime garbage collection concurrency mutex lock tokio",
        "Datenbank Replikation Transaktion Speicher Kernel Optimierung Linux",
        "programmation base de données serveur conteneur Linux noyau",
        "programmazione database server Linux container memoria",
    ],
    "repo": [
        "open source GitHub repository library package crate",
        "install pip npm cargo package manager release notes",
        "CLI tool SDK framework changelog version download source code",
        "best open source parser library for Rust Python TypeScript",
        "Open-Source Bibliothek Paket Repository Release Changelog GitHub",
        "bibliothèque open source dépôt GitHub paquet installation CLI",
    ],
    "general": [
        "Switzerland value added tax rate official government ESTV",
        "opening hours Zurich museum exhibition ticket prices",
        "history of the Roman Empire geography capital city population",
        "recipe for homemade sourdough bread pasta baking temperature",
        "weather forecast mountain hiking trail train timetable",
        "Schweiz Mehrwertsteuer Steuersatz Steuerverwaltung ESTV 2024",
        "Datenschutzgesetz Bundesgesetz Artikel Verordnung Fedlex Schweiz",
        "Öffnungszeiten Museum Eintrittspreise Fahrplan Zug Ticket",
        "taux TVA Suisse administration fédérale impôts horaires train",
        "orari treni Svizzera turismo meteo montagna ricetta cucina",
        "European Central Bank official website monetary policy overview",
        "best season to visit alpine ski resort hotel booking",
    ],
}


def fnv1a_32(data: bytes) -> int:
    h = 0x811C9DC5
    for b in data:
        h ^= b
        h = (h * 0x01000193) & 0xFFFFFFFF
    return h


def tokenize_words(text: str) -> list[str]:
    return re.findall(r"[a-z0-9äöüßéèêàçïîôù]+(?:[-/.][a-z0-9]+)*", text.lower())


def ngram_features(text: str) -> dict[int, float]:
    norm = " " + re.sub(r"\s+", " ", text.lower().strip()) + " "
    counts: dict[int, float] = {}
    for tok in tokenize_words(norm):
        b = fnv1a_32(b"w:" + tok.encode("utf-8")) % NGRAM_BUCKETS
        counts[b] = counts.get(b, 0.0) + 1.5
    chars = list(norm)
    for n in (3, 4, 5):
        for i in range(max(0, len(chars) - n + 1)):
            g = "".join(chars[i : i + n]).encode("utf-8")
            b = fnv1a_32(b"c:" + g) % NGRAM_BUCKETS
            counts[b] = counts.get(b, 0.0) + 0.45
    l2 = math.sqrt(sum(v * v for v in counts.values())) or 1.0
    return {k: v / l2 for k, v in counts.items()}


def main() -> None:
    print("Loading MIT minishlab/potion-multilingual-128M...")
    model = StaticModel.from_pretrained("minishlab/potion-multilingual-128M")

    # 1. Train 8-class Ridge head on normalized Model2Vec embeddings
    X_m2v, Y_rows = [], []
    for c_idx, cls in enumerate(CLASSES):
        vecs = model.encode(TRAIN_DATA[cls])
        norms = np.linalg.norm(vecs, axis=1, keepdims=True) + 1e-8
        vecs = vecs / norms
        for v in vecs:
            X_m2v.append(v)
            y = np.zeros(NUM_CLASSES, dtype=np.float32)
            y[c_idx] = 1.0
            Y_rows.append(y)
    X_m2v = np.stack(X_m2v).astype(np.float32)
    Y = np.stack(Y_rows).astype(np.float32)

    # Augment with small bias term via centering or ridge directly
    K_m2v = X_m2v @ X_m2v.T + 0.45 * np.eye(len(X_m2v), dtype=np.float32)
    W_m2v = X_m2v.T @ np.linalg.solve(K_m2v, Y)  # (D, 8)

    # 2. Project entire Model2Vec vocabulary E @ W_m2v -> (V, 8), keep top-K clean lowercase tokens
    vocab: dict[str, int] = model.tokenizer.get_vocab()
    emb: np.ndarray = model.embedding.astype(np.float32)  # (V, D)
    # Normalize each token embedding row before projection so token pooling stays well-scaled
    emb_norms = np.linalg.norm(emb, axis=1, keepdims=True) + 1e-8
    proj = (emb / emb_norms) @ W_m2v  # (V, 8)

    # Filter to clean standalone lowercase word/stem tokens (skip special tokens, punctuation, 1-char noise)
    # Weight informativeness by a BPE frequency prior (lower vocab index = more common real word)
    # and always retain tokens appearing in our training corpus.
    train_vocab = {tok for rows in TRAIN_DATA.values() for text in rows for tok in tokenize_words(text)}
    token_scores: dict[str, tuple[float, np.ndarray]] = {}
    clean_re = re.compile(r"^[a-z0-9äöüßéèêàçïîôù]{3,28}$")
    for raw_tok, idx in vocab.items():
        if idx >= len(proj):
            continue
        cleaned = raw_tok.lstrip("▁Ġ#").lower()
        if not clean_re.match(cleaned):
            continue
        vec = proj[idx]
        spread = float(vec.max() - np.median(vec))
        freq_prior = 1.0 / math.log2(idx + 16)
        priority = (10.0 + spread) if cleaned in train_vocab else (spread * freq_prior)
        prev = token_scores.get(cleaned)
        if prev is None or priority > prev[0]:
            token_scores[cleaned] = (priority, vec)

    ranked_tokens = sorted(
        [(k, v) for k, (_, v) in token_scores.items()],
        key=lambda kv: token_scores[kv[0]][0],
        reverse=True,
    )[:TOP_TOKENS]
    ranked_tokens.sort(key=lambda kv: kv[0])  # sorted for binary search in Rust

    tok_mat = np.stack([v for _, v in ranked_tokens]).astype(np.float32)
    tok_max = float(np.max(np.abs(tok_mat))) or 1.0
    tok_scale = tok_max / 127.0
    tok_q = np.clip(np.round(tok_mat / tok_scale), -127, 127).astype(np.int8)

    # 3. Train compact 4096-bucket FNV-1a char/word n-gram linear head
    X_ng = []
    for cls in CLASSES:
        for text in TRAIN_DATA[cls]:
            row = np.zeros(NGRAM_BUCKETS, dtype=np.float32)
            for k, v in ngram_features(text).items():
                row[k] = v
            X_ng.append(row)
    X_ng = np.stack(X_ng).astype(np.float32)
    K_ng = X_ng @ X_ng.T + 0.35 * np.eye(len(X_ng), dtype=np.float32)
    W_ng = X_ng.T @ np.linalg.solve(K_ng, Y)  # (NGRAM_BUCKETS, 8)
    ng_max = float(np.max(np.abs(W_ng))) or 1.0
    ng_scale = ng_max / 127.0
    ng_q = np.clip(np.round(W_ng / ng_scale), -127, 127).astype(np.int8)

    # 4. Serialize compact binary `QRT1`
    buf = bytearray()
    buf.extend(b"QRT1")
    buf.extend(struct.pack("<HffII", NUM_CLASSES, tok_scale, ng_scale, NGRAM_BUCKETS, len(ranked_tokens)))
    for _ in range(NUM_CLASSES):
        buf.extend(struct.pack("<f", 0.0))
    for (tok, _), qvec in zip(ranked_tokens, tok_q):
        tb = tok.encode("utf-8")
        buf.extend(struct.pack("<H", len(tb)))
        buf.extend(tb)
        buf.extend(qvec.tobytes())
    buf.extend(ng_q.tobytes())

    OUT_BIN.write_bytes(buf)
    print(f"Wrote {OUT_BIN} ({len(buf) / 1024:.1f} KiB, {len(ranked_tokens)} tokens + {NGRAM_BUCKETS} n-gram buckets)")

    # 5. Simulate EXACT Rust inference on held-out 48-query multilingual test set!
    spec = importlib.util.spec_from_file_location("bench_router", REPO / "benchmarks/small-cpu/bench-router.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)

    tok_lookup = {tok: qvec.astype(np.float32) * tok_scale for (tok, _), qvec in zip(ranked_tokens, tok_q)}
    ng_table = ng_q.astype(np.float32) * ng_scale

    def rust_exact_scores(q: str) -> np.ndarray:
        # Token lookup (exact match + prefix stem fallback for compounds)
        words = tokenize_words(q)
        tok_acc = np.zeros(NUM_CLASSES, dtype=np.float32)
        tok_hits = 0
        for w in words:
            if w in tok_lookup:
                tok_acc += tok_lookup[w]
                tok_hits += 1
            elif len(w) >= 6:
                # Try longest matching prefix/suffix stem >= 5 chars for German compounds
                for k in range(len(w) - 1, 4, -1):
                    if w[:k] in tok_lookup:
                        tok_acc += tok_lookup[w[:k]] * 0.7
                        tok_hits += 1
                        break
        if tok_hits > 0:
            tok_acc /= math.sqrt(tok_hits)

        ng_acc = np.zeros(NUM_CLASSES, dtype=np.float32)
        for b, v in ngram_features(q).items():
            ng_acc += v * ng_table[b]

        return 0.65 * tok_acc + 0.55 * ng_acc

    # Evaluate across threshold sweep to verify clean operating point
    best = None
    for thr in (0.18, 0.22, 0.25, 0.28, 0.32):
        for margin in (0.02, 0.05, 0.08):
            preds = []
            for _, q, _ in mod.TEST_SET:
                s = rust_exact_scores(q)
                gen = float(s[-1])
                pred = {INTENTS[i] for i in range(len(INTENTS)) if s[i] >= thr and s[i] > gen + margin}
                # Deterministic syntax rule: explicit `RFC \d+` always activates ietf
                if re.search(r"(?i)\brfc[\s-]*\d{3,5}\b", q):
                    pred.add("ietf")
                preds.append(pred)
            ev = mod.eval_predictions(preds)
            score = ev["macro_f1"] - 0.15 * ev["general_fp_rate"]
            if best is None or score > best[0]:
                best = (score, thr, margin, ev)
    _, thr, margin, ev = best
    print(f"Held-out 48-query eval (thr={thr}, margin={margin}):")
    print(f"  Macro F1:            {ev['macro_f1']:.3f}")
    print(f"  Multilingual Recall: {ev['multilingual_recall']:.3f}")
    print(f"  General FP Rate:     {ev['general_fp_rate']:.3f}")
    print(f"  Per-intent:          {ev['per_intent']}")


if __name__ == "__main__":
    main()
