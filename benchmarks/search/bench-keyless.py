#!/usr/bin/env python3
"""Keyless meta-search benchmark (zero API keys):
  1. Quelle (legacy default: mwmbl alone)
  2. Quelle (new default: parallel RRF fusion across duckduckgo,mwmbl,wikipedia,stackexchange,hackernews)
  3. Self-hosted Firecrawl + default keyless SearXNG (general engines)

Runs the same 12 real-world queries across all 3 pipelines (Swiss gov, technical
docs, standards, comparisons, current knowledge) and measures end-to-end latency,
sources retrieved, official/expected signal recall, and candidate relevance.
"""

import argparse
import json
import os
import secrets
import socket
import statistics
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

from run import HERE, REPO, free_port, ready, request, save

QUERIES = [
    {
        "id": "k01_swiss_vat",
        "query": "Schweiz MWST Steuersätze 2024 ESTV",
        "language": "de",
        "signals": ["8.1", "2.6", "3.8"],
        "official": ["admin.ch"],
    },
    {
        "id": "k02_rfc9110_429",
        "query": "RFC 9110 HTTP 429 Too Many Requests Retry-After",
        "language": "en",
        "signals": ["429", "retry-after"],
        "official": ["rfc-editor.org", "mozilla.org"],
    },
    {
        "id": "k03_sqlite_wal",
        "query": "SQLite WAL mode checkpoint durability",
        "language": "en",
        "signals": ["wal", "checkpoint"],
        "official": ["sqlite.org"],
    },
    {
        "id": "k04_ecb_rates",
        "query": "European Central Bank key ECB interest rates deposit facility",
        "language": "en",
        "signals": ["deposit facility", "main refinancing"],
        "official": ["ecb.europa.eu"],
    },
    {
        "id": "k05_rust_borrow",
        "query": "Rust ownership borrowing lifetime rules",
        "language": "en",
        "signals": ["ownership", "borrow"],
        "official": ["rust-lang.org"],
    },
    {
        "id": "k06_python_gil",
        "query": "Python 3.13 free-threaded PEP 703 GIL",
        "language": "en",
        "signals": ["703", "gil"],
        "official": ["python.org"],
    },
    {
        "id": "k07_redis_persistence",
        "query": "Redis persistence RDB AOF fsync",
        "language": "en",
        "signals": ["rdb", "aof"],
        "official": ["redis.io"],
    },
    {
        "id": "k08_swiss_dsg",
        "query": "Schweiz Datenschutzgesetz DSG 2023 Fedlex",
        "language": "de",
        "signals": ["datenschutz", "dsg"],
        "official": ["admin.ch"],
    },
    {
        "id": "k09_http3_quic",
        "query": "RFC 9114 HTTP/3 QUIC transport",
        "language": "en",
        "signals": ["quic", "9114"],
        "official": ["rfc-editor.org", "mozilla.org", "wikipedia.org"],
    },
    {
        "id": "k10_k8s_probes",
        "query": "Kubernetes liveness readiness startup probes",
        "language": "en",
        "signals": ["liveness", "readiness", "startup"],
        "official": ["kubernetes.io"],
    },
    {
        "id": "k11_pg_mvcc",
        "query": "PostgreSQL MVCC transaction isolation levels",
        "language": "en",
        "signals": ["mvcc", "isolation"],
        "official": ["postgresql.org"],
    },
    {
        "id": "k12_wai_aria",
        "query": "WAI-ARIA aria-expanded accordion disclosure pattern",
        "language": "en",
        "signals": ["aria-expanded"],
        "official": ["w3.org", "mozilla.org"],
    },
]


def start_quelle(binary: str, lightpanda: str, engines: str, mode: str = "fuse") -> tuple[subprocess.Popen, str, str]:
    port = free_port()
    token = secrets.token_hex(24)
    base = f"http://127.0.0.1:{port}"
    qenv = {k: v for k, v in os.environ.items() if not k.startswith("QUELLE_") and k != "BRAVE_SEARCH_API_KEY"}
    qenv.update(
        {
            "QUELLE_LISTEN": f"127.0.0.1:{port}",
            "QUELLE_TOKEN": token,
            "QUELLE_SEARCH_ENGINES": engines,
            "QUELLE_SEARCH_MODE": mode,
            "QUELLE_LIGHTPANDA": lightpanda,
        }
    )
    proc = subprocess.Popen(
        [binary],
        cwd=REPO,
        env=qenv,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    ready(base + "/healthz", proc)
    return proc, base, token


def score_sources(sources: list[dict], item: dict) -> dict:
    combined = "\n".join(s.get("content", "") for s in sources).lower()
    urls = [s.get("url", "").lower() for s in sources]
    matched_signals = [sig for sig in item["signals"] if sig.lower() in combined]
    official_hits = sum(
        1 for u in urls if any(dom in u for dom in item["official"])
    )
    return {
        "sources": len(sources),
        "signal_matches": len(matched_signals),
        "signal_total": len(item["signals"]),
        "all_signals": len(matched_signals) == len(item["signals"]),
        "official_hits": official_hits,
        "chars": sum(len(s.get("content", "")) for s in sources),
        "urls": [s.get("url", "") for s in sources],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default=str(REPO / "target/release/quelle"))
    parser.add_argument("--lightpanda", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    out_dir = args.output.resolve()
    out_dir.mkdir(parents=True, exist_ok=True)

    # Start Firecrawl + default keyless SearXNG (no Brave key, default keyless general engines!)
    searx_port, fire_port = free_port(), free_port()
    project = "quelle-keyless-" + secrets.token_hex(4)
    env = dict(os.environ)
    env.pop("BRAVE_SEARCH_API_KEY", None)
    env.update(BENCH_SEARX_PORT=str(searx_port), BENCH_FIRECRAWL_PORT=str(fire_port))

    # Write a keyless compose override so Firecrawl uses default keyless SearXNG engines
    # (duckduckgo, wikipedia, startpage, mojeek) instead of braveapi!
    compose_base = HERE / "compose.yml"

    records = []
    with tempfile.TemporaryDirectory(prefix=project + "-") as runtime:
        env["BENCH_RUNTIME"] = runtime
        save(
            Path(runtime) / "settings.yml",
            {
                "use_default_settings": True,
                "server": {
                    "secret_key": secrets.token_hex(32),
                    "limiter": False,
                },
                "search": {
                    "formats": ["html", "json"],
                },
            },
        )
        override = Path(runtime) / "override.yml"
        override.write_text(
            "services:\n"
            "  firecrawl:\n"
            "    environment:\n"
            "      SEARXNG_ENGINES: \"\"\n"
            "      SEARXNG_CATEGORIES: \"general\"\n"
        )
        compose = [
            "docker",
            "compose",
            "-p",
            project,
            "-f",
            str(compose_base),
            "-f",
            str(override),
        ]
        log_path = out_dir / "services.log"
        qm_proc = qf_proc = None
        try:
            with log_path.open("w") as log:
                subprocess.run(compose + ["up", "-d"], env=env, stdout=log, stderr=log, check=True)
            fbase = f"http://127.0.0.1:{fire_port}"
            ready(fbase + "/")
            ready(f"http://127.0.0.1:{searx_port}/healthz")

            qm_proc, qm_base, qm_tok = start_quelle(args.binary, args.lightpanda, "mwmbl", "rotate")
            qf_proc, qf_base, qf_tok = start_quelle(
                args.binary,
                args.lightpanda,
                "duckduckgo,mwmbl,wikipedia,stackexchange,hackernews",
                "fuse",
            )

            for idx, item in enumerate(QUERIES, 1):
                row = {"id": item["id"], "query": item["query"]}

                # 1. Quelle legacy (mwmbl alone)
                res_m = request(
                    qm_base + "/api/content",
                    {"query": item["query"], "language": item["language"], "limit": 5, "selection": "heuristic"},
                    {"Authorization": f"Bearer {qm_tok}"},
                    timeout=35,
                )
                srcs_m = [
                    {"url": s["metadata"]["url"], "content": s.get("content", "")}
                    for s in res_m["data"].get("sources", [])
                ]
                row["quelle_mwmbl"] = {
                    "seconds": round(res_m["seconds"], 3),
                    **score_sources(srcs_m, item),
                }

                # 2. Quelle new keyless default (parallel RRF meta-search)
                res_f = request(
                    qf_base + "/api/content",
                    {"query": item["query"], "language": item["language"], "limit": 5, "selection": "heuristic"},
                    {"Authorization": f"Bearer {qf_tok}"},
                    timeout=35,
                )
                srcs_f = [
                    {"url": s["metadata"]["url"], "content": s.get("content", "")}
                    for s in res_f["data"].get("sources", [])
                ]
                row["quelle_fused"] = {
                    "seconds": round(res_f["seconds"], 3),
                    **score_sources(srcs_f, item),
                }

                # 3. Firecrawl + keyless SearXNG
                res_fc = request(
                    fbase + "/v2/search",
                    {
                        "query": item["query"],
                        "limit": 5,
                        "scrapeOptions": {"formats": ["markdown"], "onlyMainContent": True},
                    },
                    timeout=45,
                )
                fc_items = (res_fc["data"].get("data") or {}).get("web") or []
                if isinstance(res_fc["data"].get("data"), list):
                    fc_items = res_fc["data"]["data"]
                srcs_fc = [
                    {
                        "url": it.get("url") or (it.get("metadata") or {}).get("sourceURL", ""),
                        "content": it.get("markdown", ""),
                    }
                    for it in fc_items
                    if (it.get("markdown") or "").strip()
                ]
                row["firecrawl_searxng"] = {
                    "seconds": round(res_fc["seconds"], 3),
                    **score_sources(srcs_fc, item),
                }

                records.append(row)
                print(
                    f"{idx:2d}/{len(QUERIES)} {item['id']:20s} | "
                    f"Mwmbl: {row['quelle_mwmbl']['signal_matches']}/{item['signals'].__len__()} sig, {row['quelle_mwmbl']['sources']} src, {row['quelle_mwmbl']['seconds']:.2f}s | "
                    f"Q-Fused: {row['quelle_fused']['signal_matches']}/{item['signals'].__len__()} sig, {row['quelle_fused']['sources']} src, {row['quelle_fused']['seconds']:.2f}s | "
                    f"FC+SearX: {row['firecrawl_searxng']['signal_matches']}/{item['signals'].__len__()} sig, {row['firecrawl_searxng']['sources']} src, {row['firecrawl_searxng']['seconds']:.2f}s",
                    flush=True,
                )
        finally:
            for p in (qm_proc, qf_proc):
                if p:
                    p.terminate()
                    try:
                        p.wait(timeout=5)
                    except Exception:
                        p.kill()
            subprocess.run(compose + ["down", "-v", "--remove-orphans"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def summarize(arm: str) -> dict:
        secs = [r[arm]["seconds"] for r in records]
        return {
            "queries_with_sources": sum(1 for r in records if r[arm]["sources"] > 0),
            "total_sources": sum(r[arm]["sources"] for r in records),
            "all_signals_queries": sum(1 for r in records if r[arm]["all_signals"]),
            "signal_matches": sum(r[arm]["signal_matches"] for r in records),
            "signal_total": sum(r[arm]["signal_total"] for r in records),
            "official_hits": sum(r[arm]["official_hits"] for r in records),
            "median_seconds": round(statistics.median(secs), 3),
            "p95_seconds": round(sorted(secs)[min(len(secs) - 1, int(len(secs) * 0.95))], 3),
        }

    summary = {
        "queries": len(records),
        "quelle_mwmbl": summarize("quelle_mwmbl"),
        "quelle_fused": summarize("quelle_fused"),
        "firecrawl_searxng": summarize("firecrawl_searxng"),
    }
    save(out_dir / "records.json", records)
    save(out_dir / "summary.json", summary)
    print("\n=== SUMMARY ===")
    print(json.dumps(summary, indent=2), flush=True)


if __name__ == "__main__":
    main()
