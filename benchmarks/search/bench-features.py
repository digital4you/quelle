#!/usr/bin/env python3
"""Benchmark Quelle features #1-#6:
  1. Persistent Lightpanda CDP pool + asset blocking vs one-shot fetch
  2. Adaptive render wait on 10s delayed JS page (quotes.toscrape.com/js-delayed/)
  3. Token-budgeted section-anchored chunking & query-focused distillation
  4. Zero-LLM deterministic structured extraction (outline, tables, JSON-LD)
  5. Configurable PDF maxPages (>150 pages) & layout-aware PDF table extraction
  6. Delta-aware incremental crawling (unchanged detection & payload reduction)
"""

import argparse
import json
import os
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

TOKEN = "benchmark-token-1234567890abcdef1234567890"


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def start_quelle(binary: str, lightpanda: str, cdp_pool: bool) -> tuple[subprocess.Popen, str]:
    port = free_port()
    base = f"http://127.0.0.1:{port}"
    env = os.environ.copy()
    env.update(
        {
            "QUELLE_LISTEN": f"127.0.0.1:{port}",
            "QUELLE_TOKEN": TOKEN,
            "QUELLE_SEARXNG_URL": "http://127.0.0.1:1",
            "QUELLE_LIGHTPANDA": lightpanda,
            "QUELLE_CDP_POOL": "true" if cdp_pool else "false",
            "QUELLE_RENDER_CONCURRENCY": "4",
        }
    )
    proc = subprocess.Popen(
        [binary],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(f"{base}/healthz", timeout=1) as r:
                if r.status == 200:
                    return proc, base
        except Exception:
            time.sleep(0.1)
    proc.kill()
    raise RuntimeError(f"Quelle failed to start: {proc.stderr.read().decode(errors='replace')}")


def api_post(base: str, path: str, payload: dict, timeout: float = 60.0) -> tuple[int, dict, float]:
    req = urllib.request.Request(
        f"{base}{path}",
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {TOKEN}",
            "Content-Type": "application/json",
        },
        method="POST",
    )
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            body = json.loads(resp.read().decode())
            return resp.status, body, time.monotonic() - t0
    except urllib.error.HTTPError as e:
        raw = e.read().decode(errors="replace")
        try:
            body = json.loads(raw)
        except Exception:
            body = {"raw": raw}
        return e.code, body, time.monotonic() - t0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/release/quelle")
    parser.add_argument("--lightpanda", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    report = {}

    # ------------------------------------------------------------------
    # Feature #1 & #2: CDP Pool vs One-Shot Fetch + Adaptive Wait (10s JS)
    # ------------------------------------------------------------------
    js_urls = [
        "https://quotes.toscrape.com/js/",
        "https://quotes.toscrape.com/js/page/2/",
        "https://quotes.toscrape.com/js/page/3/",
        "https://quotes.toscrape.com/js-delayed/",
    ]
    cdp_ablation = {}
    for mode, enabled in [("oneshot_fetch", False), ("cdp_pool", True)]:
        proc, base = start_quelle(args.binary, args.lightpanda, cdp_pool=enabled)
        try:
            rows = []
            for url in js_urls:
                status, body, sec = api_post(
                    base,
                    "/api/scrape",
                    {"url": url, "formats": ["markdown"], "maxAge": 0},
                )
                md = body.get("data", {}).get("markdown", "") if status == 200 else ""
                rows.append(
                    {
                        "url": url,
                        "status": status,
                        "seconds": round(sec, 3),
                        "chars": len(md),
                        "has_quotes": ("Einstein" in md or "Rowling" in md or "Austen" in md or "Twain" in md or "Monroe" in md),
                        "retrieval": body.get("data", {}).get("retrieval"),
                        "error": body.get("error"),
                    }
                )
            cdp_ablation[mode] = rows
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except Exception:
                proc.kill()
    report["feature_1_2_cdp_and_adaptive"] = cdp_ablation
    print("F1/F2 CDP & Adaptive Wait:", json.dumps(cdp_ablation, indent=2), flush=True)

    # Start a single pooled Quelle instance for Features #3-#6
    proc, base = start_quelle(args.binary, args.lightpanda, cdp_pool=True)
    try:
        # ------------------------------------------------------------------
        # Feature #3: Token-Budgeted Section-Anchored Chunking & Distillation
        # ------------------------------------------------------------------
        chunk_cases = [
            {
                "name": "rfc9110_429_distill",
                "url": "https://www.rfc-editor.org/rfc/rfc9110.html",
                "focusQuery": "Retry-After 503 Service Unavailable rate limit status code",
                "maxTokens": 1200,
                "expected_term": "Retry-After",
            },
            {
                "name": "wai_aria_accordion_distill",
                "url": "https://www.w3.org/TR/wai-aria-1.2/",
                "focusQuery": "aria-expanded tabpanel disclosure state",
                "maxTokens": 1200,
                "expected_term": "aria-expanded",
            },
        ]
        f3 = []
        for case in chunk_cases:
            st_full, body_full, sec_full = api_post(
                base,
                "/api/scrape",
                {"url": case["url"], "formats": ["markdown"]},
            )
            full_md = body_full.get("data", {}).get("markdown", "")
            full_tokens = max(1, len(full_md) // 4)

            st_dist, body_dist, sec_dist = api_post(
                base,
                "/api/scrape",
                {
                    "url": case["url"],
                    "formats": ["markdown", "chunks"],
                    "focusQuery": case["focusQuery"],
                    "maxTokens": case["maxTokens"],
                    "chunkTokens": 400,
                },
            )
            data_dist = body_dist.get("data", {})
            chunks = data_dist.get("chunks", [])
            dist_md = data_dist.get("markdown", "")
            dist_tokens = sum(c.get("tokens", 0) for c in chunks)
            anchored = sum(1 for c in chunks if "#" in c.get("anchorUrl", ""))
            f3.append(
                {
                    "name": case["name"],
                    "status": st_dist,
                    "full_chars": len(full_md),
                    "est_full_tokens": full_tokens,
                    "distilled_tokens": dist_tokens,
                    "token_reduction_pct": round((1 - dist_tokens / max(1, full_tokens)) * 100, 1),
                    "chunk_count": len(chunks),
                    "anchored_chunks": anchored,
                    "sample_anchor": chunks[0]["anchorUrl"] if chunks else None,
                    "sample_breadcrumb": chunks[0]["breadcrumb"] if chunks else [],
                    "has_expected_term": case["expected_term"].lower() in dist_md.lower(),
                    "seconds_full": round(sec_full, 3),
                    "seconds_distilled": round(sec_dist, 3),
                }
            )
        report["feature_3_chunking_distillation"] = f3
        print("F3 Chunking & Distillation:", json.dumps(f3, indent=2), flush=True)

        # ------------------------------------------------------------------
        # Feature #4: Zero-LLM Deterministic Structured Extraction
        # ------------------------------------------------------------------
        struct_urls = [
            "https://www.ecb.europa.eu/stats/policy_and_exchange_rates/key_ecb_interest_rates/html/index.en.html",
            "https://en.wikipedia.org/wiki/List_of_HTTP_status_codes",
            "https://news.ycombinator.com/",
        ]
        f4 = []
        for url in struct_urls:
            st, body, sec = api_post(
                base,
                "/api/scrape",
                {"url": url, "formats": ["structured"]},
            )
            sd = body.get("data", {}).get("structured", {}) if st == 200 else {}
            tables = sd.get("tables", [])
            outline = sd.get("outline", [])
            json_ld = sd.get("jsonLd", [])
            f4.append(
                {
                    "url": url,
                    "status": st,
                    "seconds": round(sec, 3),
                    "outline_count": len(outline),
                    "anchored_outline": sum(1 for h in outline if h.get("id")),
                    "table_count": len(tables),
                    "first_table_headers": tables[0]["headers"] if tables else [],
                    "first_table_rows": len(tables[0]["rows"]) if tables else 0,
                    "json_ld_count": len(json_ld),
                }
            )
        report["feature_4_structured_extraction"] = f4
        print("F4 Structured Extraction:", json.dumps(f4, indent=2), flush=True)

        # ------------------------------------------------------------------
        # Feature #5: Configurable PDF maxPages (>150) & Layout Extraction
        # ------------------------------------------------------------------
        pdf_url = "https://www.rfc-editor.org/rfc/rfc9110.pdf"
        st_def, body_def, sec_def = api_post(
            base,
            "/api/scrape",
            {"url": pdf_url, "formats": ["markdown"]},
        )
        st_full, body_full, sec_full = api_post(
            base,
            "/api/scrape",
            {"url": pdf_url, "formats": ["markdown"], "maxPages": 250},
        )
        f5 = {
            "url": pdf_url,
            "default_150": {
                "status": st_def,
                "seconds": round(sec_def, 3),
                "pages_read": body_def.get("data", {}).get("metadata", {}).get("pagesRead"),
                "truncated": body_def.get("data", {}).get("truncated"),
                "chars": len(body_def.get("data", {}).get("markdown", "")),
                "retrieval": body_def.get("data", {}).get("retrieval"),
            },
            "max_pages_250": {
                "status": st_full,
                "seconds": round(sec_full, 3),
                "pages_read": body_full.get("data", {}).get("metadata", {}).get("pagesRead"),
                "truncated": body_full.get("data", {}).get("truncated"),
                "chars": len(body_full.get("data", {}).get("markdown", "")),
                "retrieval": body_full.get("data", {}).get("retrieval"),
            },
        }
        report["feature_5_pdf_max_pages"] = f5
        print("F5 PDF Configurable maxPages:", json.dumps(f5, indent=2), flush=True)

        # ------------------------------------------------------------------
        # Feature #6: Delta-Aware Incremental Crawling
        # ------------------------------------------------------------------
        crawl_seed = "https://books.toscrape.com/"
        st_c1, body_c1, sec_c1 = api_post(
            base,
            "/api/crawl",
            {
                "url": crawl_seed,
                "maxDepth": 1,
                "limit": 5,
                "useSitemap": False,
                "incremental": True,
                "sync": True,
            },
        )
        crawl1_id = body_c1.get("id")
        c1_bytes = len(json.dumps(body_c1).encode())

        # Pass 2: incremental since crawl1_id with omitUnchangedContent: true
        st_c2, body_c2, sec_c2 = api_post(
            base,
            "/api/crawl",
            {
                "url": crawl_seed,
                "maxDepth": 1,
                "limit": 5,
                "useSitemap": False,
                "sinceCrawlId": crawl1_id,
                "omitUnchangedContent": True,
                "sync": True,
            },
        )
        c2_bytes = len(json.dumps(body_c2).encode())

        # Pass 3: stateless previousHashes (simulating a mutated hash for 1 URL)
        hashes = {
            p["url"]: p["contentHash"]
            for p in body_c1.get("data", [])
            if p.get("contentHash")
        }
        if hashes:
            first_key = next(iter(hashes))
            hashes[first_key] = "sha256:" + "0" * 64
        st_c3, body_c3, sec_c3 = api_post(
            base,
            "/api/crawl",
            {
                "url": crawl_seed,
                "maxDepth": 1,
                "limit": 5,
                "useSitemap": False,
                "previousHashes": hashes,
                "omitUnchangedContent": True,
                "sync": True,
            },
        )
        f6 = {
            "seed": crawl_seed,
            "pass1_initial": {
                "status": st_c1,
                "id": crawl1_id,
                "seconds": round(sec_c1, 3),
                "completed": body_c1.get("completed"),
                "changes": body_c1.get("changes"),
                "payload_bytes": c1_bytes,
            },
            "pass2_since_crawl_id_omit_unchanged": {
                "status": st_c2,
                "seconds": round(sec_c2, 3),
                "completed": body_c2.get("completed"),
                "changes": body_c2.get("changes"),
                "payload_bytes": c2_bytes,
                "payload_reduction_pct": round((1 - c2_bytes / max(1, c1_bytes)) * 100, 1),
            },
            "pass3_stateless_previous_hashes_with_1_changed": {
                "status": st_c3,
                "seconds": round(sec_c3, 3),
                "completed": body_c3.get("completed"),
                "changes": body_c3.get("changes"),
            },
        }
        report["feature_6_incremental_crawl"] = f6
        print("F6 Incremental Crawl:", json.dumps(f6, indent=2), flush=True)

    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except Exception:
            proc.kill()

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(f"Saved feature benchmark to {args.output}")


if __name__ == "__main__":
    main()
