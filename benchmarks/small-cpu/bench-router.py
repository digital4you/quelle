#!/usr/bin/env python3
"""Small-CPU (2 cores) benchmark for Query Intent Routers replacing regexes.

Compares on 48 realistic multilingual (EN/DE/FR/IT) queries:
  1. Regex baseline (exact patterns from Quelle src/discovery/providers.rs)
  2. Hashed Char/Word N-Gram Linear Classifier (<1 MB pure-math, zero neural runtime)
  3. Model2Vec `minishlab/potion-base-8M` (MIT, ~15 MB static embeddings)
  4. Model2Vec `minishlab/potion-multilingual-128M` (MIT, 100+ langs static embeddings)

Evaluates both Zero-Shot Prototype Cosine and Ridge/Logistic Linear Head on top of
Model2Vec, measuring Macro-F1, Multilingual Recall, General False-Positive Rate,
p50/p95 CPU latency (ms), Peak RSS (MiB), and Disk Size (MB).
"""

import json
import math
import os
import re
import resource
import statistics
import time
from pathlib import Path

import numpy as np

INTENTS = ["biomed", "ietf", "web", "academic", "news", "tech", "repo"]

# ---------------------------------------------------------------------------
# 1. Exact current Quelle regex baseline
# ---------------------------------------------------------------------------
REGEXES = {
    "biomed": re.compile(
        r"(?i)\b(pubmed|clinical\s+trial|placebo|vaccine|mrna|cancer|tumor|tumour|carcinoma|oncology|cardiology|diabetes|"
        r"hypertension|alzheimer|parkinson|antibody|antigen|genome|genomic|protein|enzyme|ribosome|pharmacology|dosage|"
        r"syndrome|pathology|epidemiology|meta-analysis|randomized|randomised|cohort|diagnosis|prognosis|therapy|therapeutic|medicine|medical)\b"
    ),
    "ietf": re.compile(
        r"(?i)\b(rfc\s*\d{3,5}|ietf|internet[- ]draft|quic|http/3|http/2|tls\s*1\.\d|oauth\s*2|jwt|dnssec|bgp|sctp)\b"
    ),
    "web": re.compile(
        r"(?i)\b(mdn|wai-?aria|aria-[a-z-]+|wcag|accessibility|a11y|html|html5|css|dom|svg|canvas|webgl|wasm|webassembly|"
        r"javascript|ecmascript|typescript|fetch|xhr|websocket|cors|csp|cookie|headers?|http|http/2|http/3|https|quic|rfc\s*\d+|"
        r"status\s+code|retry-after|etag|cache-control|content-security-policy|same-?site|service\s+worker|indexeddb|shadow\s+dom)\b"
    ),
    "academic": re.compile(
        r"(?i)\b(arxiv|paper|preprint|doi|theorem|proof|citation|et al|benchmark|transformer|attention|neural|diffusion|llm|empirical|dataset|ablation)\b"
    ),
    "news": re.compile(
        r"(?i)\b(news|breaking|headline|headlines|today|yesterday|latest|announced|announcement|election|referendum|"
        r"summit|ceasefire|tariff|tariffs|sanctions|earnings|quarterly\s+results|rate\s+decision|central\s+bank\s+decision|"
        r"aktuell|nachrichten|meldungen|nouvelles|actualités|notizie)\b"
    ),
    "tech": re.compile(
        r"(?i)\b(rust|python|javascript|typescript|golang|java|kotlin|swift|cpp|c\+\+|sql|sqlite|postgres|postgresql|mysql|redis|mongodb|"
        r"docker|kubernetes|k8s|linux|unix|bash|git|github|http|https|quic|tls|ssl|dns|tcp|udp|grpc|graphql|rest|api|sdk|cli|json|yaml|html|css|wasm|"
        r"react|vue|nuxt|next\.?js|node\.?js|tokio|async|mutex|regex|rfc|wal|mvcc|aria|oauth|jwt|csrf|ssrf|cve|compiler|error|exception|stacktrace)\b"
    ),
    "repo": re.compile(
        r"(?i)\b(github|repo|repository|crate|cargo|npm|pypi|pip|package|library|sdk|cli|open[- ]source|release|changelog)\b"
    ),
}

# ---------------------------------------------------------------------------
# 2. Small labeled training prototypes (distinct from test set!)
# ---------------------------------------------------------------------------
TRAIN_EXAMPLES: dict[str, list[str]] = {
    "biomed": [
        "clinical study randomized placebo treatment efficacy",
        "drug side effects dosage interactions prescription",
        "disease symptoms treatment diagnosis medical review",
        "vaccine immune response antibody clinical outcomes",
        "Nebenwirkungen Medikament Dosierung Behandlung Studie",
        "Krankheit Symptome Therapie Arzt medizinische Forschung",
        "effets secondaires médicament posologie essai clinique",
        "maladie symptômes traitement diagnostic santé publique",
        "effetti collaterali farmaco dosaggio studio clinico terapia",
    ],
    "ietf": [
        "RFC 9110 HTTP semantics standard specification IETF",
        "RFC 9114 QUIC transport protocol handshake TLS 1.3",
        "IETF internet draft OAuth 2.0 JWT bearer token spec",
        "DNSSEC BGP routing protocol specification RFC standard",
        "RFC Protokoll Spezifikation TLS Handshake Standard",
    ],
    "web": [
        "HTML CSS flexbox grid layout browser DOM manipulation",
        "WAI-ARIA accessibility accordion button keyboard focus",
        "JavaScript Fetch API CORS headers cookies same-site",
        "HTTP status code 429 caching ETag Cache-Control header",
        "WebAssembly service worker Shadow DOM SVG canvas MDN",
        "CSS Layout Flexbox Browser Barrierefreiheit HTML",
        "requête HTTP en-tête CORS cache navigateur JavaScript",
    ],
    "academic": [
        "research paper arXiv preprint empirical study benchmark",
        "scientific literature citation DOI peer reviewed findings",
        "machine learning scaling laws neural architecture ablation",
        "mathematical theorem proof complexity analysis paper",
        "wissenschaftliche Studie Publikation Forschung Ergebnisse",
        "article scientifique étude empirique recherche universitaire",
        "ricerca scientifica studio empirico pubblicazione accademica",
    ],
    "news": [
        "breaking news latest announcement press release today",
        "central bank interest rate decision inflation update",
        "election results parliament vote referendum outcome",
        "company quarterly earnings merger acquisition report",
        "aktuelle Nachrichten Medienmitteilung Zinsentscheid heute",
        "Abstimmung Wahl Resultate Bundesrat Mitteilung neu",
        "dernières nouvelles communiqué de presse décision taux",
        "ultime notizie comunicato stampa elezioni decisione oggi",
    ],
    "tech": [
        "database transaction isolation MVCC WAL checkpoint",
        "systems programming memory safety concurrency async",
        "container orchestration pods probes deployment YAML",
        "SQL query optimization indexing vacuum replication",
        "Linux kernel networking firewall systemd debugging",
        "Datenbank Replikation Transaktion Speicher Optimierung",
        "programmation base de données serveur conteneur Linux",
    ],
    "repo": [
        "open source GitHub repository library package crate",
        "install pip npm cargo package manager release notes",
        "CLI tool SDK framework changelog version download",
        "best open source parser library for Rust or Python",
        "Open-Source Bibliothek Paket Repository Release",
    ],
    "general": [
        "Switzerland value added tax rate official government",
        "opening hours Zurich museum train timetable ticket",
        "history of the Roman Empire geography capital city",
        "recipe for homemade sourdough bread baking temperature",
        "Schweiz Mehrwertsteuer Steuersatz Steuerverwaltung ESTV",
        "Datenschutzgesetz Bundesgesetz Artikel Verordnung",
        "taux TVA Suisse administration fédérale impôts",
        "orari treni Svizzera turismo meteo montagna",
    ],
}

# ---------------------------------------------------------------------------
# 3. Held-out 48-query multilingual test set (hard paraphrases + non-English!)
# ---------------------------------------------------------------------------
TEST_SET: list[tuple[str, str, set[str]]] = [
    # (lang, query, expected_intents)
    # Biomed (EN + DE + FR + IT — intentionally testing drugs/conditions NOT in regex!)
    ("en", "mRNA vaccine clinical trial efficacy", {"biomed"}),
    ("en", "ibuprofen side effects stomach ulcer risk", {"biomed"}),
    ("en", "metformin mechanism of action blood glucose", {"biomed"}),
    ("de", "Nebenwirkungen Paracetamol Leber Dosierung", {"biomed"}),
    ("de", "Bluthochdruck Behandlung Medikamente Leitlinie", {"biomed"}),
    ("fr", "effets secondaires aspirine estomac posologie", {"biomed"}),
    ("it", "effetti collaterali antibiotico amoxicillina dosaggio", {"biomed"}),
    # IETF / RFC
    ("en", "RFC 9110 HTTP 429 Too Many Requests Retry-After", {"ietf", "web"}),
    ("en", "RFC 9114 HTTP/3 QUIC transport", {"ietf", "web"}),
    ("en", "TLS 1.3 handshake key exchange specification", {"ietf"}),
    ("en", "OAuth 2.0 PKCE authorization code flow RFC 7636", {"ietf"}),
    ("de", "RFC 9111 HTTP Caching Spezifikation Standard", {"ietf", "web"}),
    # Web / MDN
    ("en", "WAI-ARIA aria-expanded accordion disclosure pattern", {"web"}),
    ("en", "how to center a div vertically with CSS flexbox", {"web"}),
    ("en", "browser CORS preflight OPTIONS header blocked", {"web"}),
    ("en", "JavaScript IntersectionObserver lazy loading images", {"web"}),
    ("de", "HTML Formular Validierung Barrierefreiheit Fokus", {"web"}),
    ("fr", "en-tête HTTP Cache-Control navigateur mise en cache", {"web"}),
    # Academic
    ("en", "Attention Is All You Need transformer paper", {"academic"}),
    ("en", "Chinchilla neural scaling laws compute optimal study", {"academic"}),
    ("en", "peer reviewed systematic review climate sensitivity DOI", {"academic"}),
    ("de", "wissenschaftliche Studie Auswirkungen Schlafmangel Forschung", {"academic"}),
    ("fr", "étude empirique impact intelligence artificielle productivité", {"academic"}),
    ("it", "ricerca scientifica pubblicazione impatto microplastiche", {"academic"}),
    # News
    ("en", "latest ECB interest rate decision press release", {"news"}),
    ("en", "breaking news US tariff announcement trade talks", {"news"}),
    ("en", "Nvidia quarterly earnings revenue beat guidance", {"news"}),
    ("de", "neueste Zinsentscheidung Schweizerische Nationalbank heute", {"news"}),
    ("de", "aktuelle Abstimmung Resultate Schweiz Bundesrat", {"news"}),
    ("fr", "communiqué de presse banque centrale décision taux aujourd'hui", {"news"}),
    ("it", "ultime notizie elezioni risultati oggi governo", {"news"}),
    # Tech (non-web systems/infra)
    ("en", "SQLite WAL mode checkpoint durability", {"tech"}),
    ("en", "Rust ownership borrowing lifetime rules", {"tech"}),
    ("en", "PostgreSQL MVCC transaction isolation levels", {"tech"}),
    ("en", "Kubernetes liveness readiness startup probes", {"tech"}),
    ("en", "Redis persistence RDB AOF fsync", {"tech"}),
    ("de", "Linux Kernel Speicherverwaltung Swap Optimierung", {"tech"}),
    # Repo / Package
    ("en", "best open source Rust JSON parser crate", {"repo", "tech"}),
    ("en", "lightpanda github repository release changelog", {"repo"}),
    ("en", "Python pip package for fast PDF parsing library", {"repo", "tech"}),
    ("de", "Open-Source CLI Tool Paket Installation GitHub", {"repo"}),
    # General (MUST NOT trigger specialists — false positive check!)
    ("de", "Schweiz MWST Steuersätze 2024 ESTV", set()),
    ("de", "Schweiz Datenschutzgesetz DSG 2023 Fedlex", set()),
    ("en", "European Central Bank key ECB interest rates deposit facility", set()),
    ("en", "best time of year to visit Zermatt Matterhorn", set()),
    ("fr", "horaires CFF train Genève Lausanne billet", set()),
    ("it", "ricetta risotto ai funghi porcini tradizionale", set()),
    ("de", "Öffnungszeiten Kunsthaus Zürich Eintrittspreise", set()),
]


def eval_predictions(preds: list[set[str]]) -> dict:
    # Per-intent F1 + Macro F1 + Multilingual recall + General FP rate
    f1s = []
    per_intent = {}
    for intent in INTENTS:
        tp = sum(1 for (_, _, y), p in zip(TEST_SET, preds) if intent in y and intent in p)
        fp = sum(1 for (_, _, y), p in zip(TEST_SET, preds) if intent not in y and intent in p)
        fn = sum(1 for (_, _, y), p in zip(TEST_SET, preds) if intent in y and intent not in p)
        prec = tp / (tp + fp) if (tp + fp) else 1.0
        rec = tp / (tp + fn) if (tp + fn) else 1.0
        f1 = (2 * prec * rec / (prec + rec)) if (prec + rec) else 0.0
        f1s.append(f1)
        per_intent[intent] = {"tp": tp, "fp": fp, "fn": fn, "f1": round(f1, 3)}

    non_en_tp = non_en_tot = 0
    for (lang, _, y), p in zip(TEST_SET, preds):
        if lang != "en" and y:
            non_en_tot += len(y)
            non_en_tp += len(y & p)

    gen_queries = [(q, p) for (_, q, y), p in zip(TEST_SET, preds) if not y]
    gen_fp = sum(1 for _, p in gen_queries if len(p) > 0)

    return {
        "macro_f1": round(statistics.mean(f1s), 3),
        "multilingual_recall": round(non_en_tp / max(non_en_tot, 1), 3),
        "general_fp_rate": round(gen_fp / max(len(gen_queries), 1), 3),
        "per_intent": per_intent,
    }


def bench_regex() -> dict:
    def predict(q: str) -> set[str]:
        return {k for k, rx in REGEXES.items() if rx.search(q)}

    times = []
    preds = []
    for _ in range(5):
        preds = []
        for _, q, _ in TEST_SET:
            t0 = time.perf_counter()
            preds.append(predict(q))
            times.append((time.perf_counter() - t0) * 1000.0)
    return {
        "name": "Regex Baseline (current)",
        "license": "Built-in",
        "disk_mb": 0.0,
        "p50_ms": round(statistics.median(times), 4),
        "p95_ms": round(sorted(times)[int(len(times) * 0.95)], 4),
        **eval_predictions(preds),
    }


# ---------------------------------------------------------------------------
# Pure-math Hashed Char/Word N-Gram Linear Classifier (fastText-style, <1 MB)
# ---------------------------------------------------------------------------
def hash_features(text: str, dim: int = 8192) -> dict[int, float]:
    norm = " " + re.sub(r"\s+", " ", text.lower().strip()) + " "
    counts: dict[int, float] = {}
    # Word tokens
    for tok in re.findall(r"[a-z0-9äöüßéèêàçïîôù]+(?:[-/.][a-z0-9]+)*", norm):
        h = hash(("w", tok)) % dim
        counts[h] = counts.get(h, 0.0) + 1.5
    # Character 3..5-grams (captures German/French/Italian stems & typos!)
    for n in (3, 4, 5):
        for i in range(max(0, len(norm) - n + 1)):
            g = norm[i : i + n]
            h = hash(("c", g)) % dim
            counts[h] = counts.get(h, 0.0) + 0.5
    norm_l2 = math.sqrt(sum(v * v for v in counts.values())) or 1.0
    return {k: v / norm_l2 for k, v in counts.items()}


def bench_hashed_ngram() -> dict:
    dim = 8192
    classes = INTENTS + ["general"]
    # Build normalized TF-IDF-like centroid matrix per class + ridge logistic weights
    X_rows = []
    y_rows = []
    for cls_idx, cls in enumerate(classes):
        for text in TRAIN_EXAMPLES[cls]:
            vec = np.zeros(dim, dtype=np.float32)
            for k, v in hash_features(text, dim).items():
                vec[k] = v
            X_rows.append(vec)
            y_rows.append(cls_idx)
    X = np.stack(X_rows)
    # One-vs-rest ridge regression: W = (X^T X + lambda I)^-1 X^T Y -> solved in dual space!
    # Dual: alpha = (X X^T + lam I)^-1 Y, W = X^T alpha (fast: N x N where N ~ 50!)
    Y = np.zeros((len(y_rows), len(classes)), dtype=np.float32)
    for i, c in enumerate(y_rows):
        Y[i, c] = 1.0
    K = X @ X.T + 0.35 * np.eye(len(y_rows), dtype=np.float32)
    alpha = np.linalg.solve(K, Y)
    W = X.T @ alpha  # (dim, num_classes) -> int8 quantized would be 8192 * 8 = 64 KB!

    def predict(q: str) -> set[str]:
        feats = hash_features(q, dim)
        scores = np.zeros(len(classes), dtype=np.float32)
        for k, v in feats.items():
            scores += v * W[k]
        gen_score = float(scores[-1])
        out = set()
        for idx, intent in enumerate(INTENTS):
            if scores[idx] >= 0.24 and scores[idx] > gen_score + 0.03:
                out.add(intent)
        return out

    times = []
    preds = []
    for _ in range(5):
        preds = []
        for _, q, _ in TEST_SET:
            t0 = time.perf_counter()
            preds.append(predict(q))
            times.append((time.perf_counter() - t0) * 1000.0)
    return {
        "name": "Hashed Char/Word N-Gram Linear (pure Rust ready)",
        "license": "MIT / Owned",
        "disk_mb": round((dim * len(classes)) / (1024 * 1024), 3),  # int8 weights (~64 KB)
        "p50_ms": round(statistics.median(times), 4),
        "p95_ms": round(sorted(times)[int(len(times) * 0.95)], 4),
        **eval_predictions(preds),
    }


def bench_model2vec(model_id: str, label: str) -> dict:
    from model2vec import StaticModel

    t_load = time.perf_counter()
    model = StaticModel.from_pretrained(model_id)
    load_s = round(time.perf_counter() - t_load, 2)

    # Approximate raw parameter footprint (int8 vs float32)
    emb = model.embedding
    fp32_mb = round(emb.nbytes / (1024 * 1024), 1)
    int8_mb = round((emb.size) / (1024 * 1024), 1)

    classes = INTENTS + ["general"]
    # 1. Zero-shot prototype cosine
    proto_vecs = []
    for cls in classes:
        vecs = model.encode(TRAIN_EXAMPLES[cls])
        mean = vecs.mean(axis=0)
        mean /= np.linalg.norm(mean) + 1e-8
        proto_vecs.append(mean)
    P = np.stack(proto_vecs)  # (C, D)

    # 2. Ridge linear head on top of static embeddings (trained in <2ms on TRAIN_EXAMPLES!)
    X_list, y_list = [], []
    for c_idx, cls in enumerate(classes):
        vecs = model.encode(TRAIN_EXAMPLES[cls])
        norms = np.linalg.norm(vecs, axis=1, keepdims=True) + 1e-8
        vecs = vecs / norms
        for v in vecs:
            X_list.append(v)
            y_list.append(c_idx)
    X = np.stack(X_list)
    Y = np.zeros((len(y_list), len(classes)), dtype=np.float32)
    for i, c in enumerate(y_list):
        Y[i, c] = 1.0
    K = X @ X.T + 0.5 * np.eye(len(y_list), dtype=np.float32)
    W = X.T @ np.linalg.solve(K, Y)  # (D, C)

    def predict_linear(q: str) -> set[str]:
        v = model.encode([q])[0]
        v = v / (np.linalg.norm(v) + 1e-8)
        scores = v @ W
        gen = float(scores[-1])
        out = set()
        for idx, intent in enumerate(INTENTS):
            if scores[idx] >= 0.25 and scores[idx] > gen + 0.04:
                out.add(intent)
        return out

    def predict_cosine(q: str) -> set[str]:
        v = model.encode([q])[0]
        v = v / (np.linalg.norm(v) + 1e-8)
        sims = P @ v
        gen = float(sims[-1])
        out = set()
        for idx, intent in enumerate(INTENTS):
            if sims[idx] >= 0.36 and sims[idx] > gen + 0.04:
                out.add(intent)
        return out

    times_lin, preds_lin = [], []
    for _ in range(5):
        preds_lin = []
        for _, q, _ in TEST_SET:
            t0 = time.perf_counter()
            preds_lin.append(predict_linear(q))
            times_lin.append((time.perf_counter() - t0) * 1000.0)

    preds_cos = [predict_cosine(q) for _, q, _ in TEST_SET]

    rss_mib = round(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024.0, 1)
    return {
        "name": label,
        "model_id": model_id,
        "license": "MIT",
        "load_seconds": load_s,
        "fp32_mb": fp32_mb,
        "int8_mb": int8_mb,
        "peak_rss_mib": rss_mib,
        "p50_ms": round(statistics.median(times_lin), 4),
        "p95_ms": round(sorted(times_lin)[int(len(times_lin) * 0.95)], 4),
        "linear_head": eval_predictions(preds_lin),
        "cosine_proto": eval_predictions(preds_cos),
    }


def main():
    results = [
        bench_regex(),
        bench_hashed_ngram(),
        bench_model2vec("minishlab/potion-base-8M", "Model2Vec potion-base-8M"),
        bench_model2vec("minishlab/potion-multilingual-128M", "Model2Vec potion-multilingual-128M"),
    ]
    out_path = Path(os.environ.get("ROUTER_BENCH_OUT", "/tmp/bench-router.json"))
    out_path.write_text(json.dumps(results, indent=2))
    print("\n=== SMALL-CPU (2 CORES) INTENT ROUTER BENCHMARK ===")
    for r in results:
        if "linear_head" in r:
            lh = r["linear_head"]
            cp = r["cosine_proto"]
            print(
                f"{r['name']:36s} | {r['license']:5s} | int8={r['int8_mb']:5.1f}MB | "
                f"p50={r['p50_ms']:.3f}ms p95={r['p95_ms']:.3f}ms | "
                f"Linear F1={lh['macro_f1']:.3f} multiRec={lh['multilingual_recall']:.3f} genFP={lh['general_fp_rate']:.2f} | "
                f"Cosine F1={cp['macro_f1']:.3f}"
            )
        else:
            print(
                f"{r['name']:36s} | {r['license']:11s} | size={r['disk_mb']:5.3f}MB | "
                f"p50={r['p50_ms']:.3f}ms p95={r['p95_ms']:.3f}ms | "
                f"F1={r['macro_f1']:.3f} multiRec={r['multilingual_recall']:.3f} genFP={r['general_fp_rate']:.2f}"
            )


if __name__ == "__main__":
    main()
