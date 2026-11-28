#!/usr/bin/env python3
"""Independent ground-truth QA & capability regression harness for Quelle.

Verifies objective factual extraction, structured parsing, multilingual UTF-8,
delayed JS + CDP controls (waitForSelector & scrollSteps), global cross-document
token budgeting (totalMaxTokens), SSE streaming TTFS (time-to-first-source),
and persistent crawl state recovery across process restarts (QUELLE_STATE_DIR).
"""

import argparse
import json
import os
import secrets
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

TOKEN = "groundtruth-eval-token-0123456789abcdef01234567"


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def start_quelle(binary: str, lightpanda: str, state_dir: str | None = None) -> tuple[subprocess.Popen, str]:
    port = free_port()
    base = f"http://127.0.0.1:{port}"
    env = {k: v for k, v in os.environ.items() if not k.startswith("QUELLE_") and k != "BRAVE_SEARCH_API_KEY"}
    env.update(
        {
            "QUELLE_LISTEN": f"127.0.0.1:{port}",
            "QUELLE_TOKEN": TOKEN,
            "QUELLE_SEARCH_ENGINES": "none",
            "QUELLE_LIGHTPANDA": lightpanda,
        }
    )
    if state_dir:
        env["QUELLE_STATE_DIR"] = state_dir
    proc = subprocess.Popen([binary], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
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


def api_post(base: str, path: str, payload: dict, timeout: float = 45.0) -> tuple[int, dict, float]:
    req = urllib.request.Request(
        f"{base}{path}",
        data=json.dumps(payload).encode(),
        headers={"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"},
        method="POST",
    )
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, json.loads(r.read().decode()), time.monotonic() - t0
    except urllib.error.HTTPError as e:
        raw = e.read().decode(errors="replace")
        try:
            body = json.loads(raw)
        except Exception:
            body = {"raw": raw}
        return e.code, body, time.monotonic() - t0


def api_post_sse(base: str, path: str, payload: dict, timeout: float = 45.0) -> tuple[int, list[dict], float, float]:
    req = urllib.request.Request(
        f"{base}{path}",
        data=json.dumps(payload).encode(),
        headers={"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"},
        method="POST",
    )
    t0 = time.monotonic()
    first_source_s = None
    events = []
    with urllib.request.urlopen(req, timeout=timeout) as r:
        for raw_line in r:
            line = raw_line.decode("utf-8", errors="ignore").strip()
            if line.startswith("data:"):
                ev = json.loads(line[5:].strip())
                if first_source_s is None and ev.get("type") in ("source", "page"):
                    first_source_s = time.monotonic() - t0
                events.append(ev)
        total_s = time.monotonic() - t0
        return r.status, events, first_source_s or total_s, total_s


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/release/quelle")
    parser.add_argument("--lightpanda", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    state_dir = tempfile.mkdtemp(prefix="quelle-eval-state-")
    results = []

    def record(name: str, passed: bool, seconds: float, detail: dict):
        row = {"case": name, "passed": passed, "seconds": round(seconds, 3), **detail}
        results.append(row)
        print(json.dumps(row), flush=True)

    proc, base = start_quelle(args.binary, args.lightpanda, state_dir=state_dir)
    try:
        # 1. Ground-truth factual extraction: Swiss VAT 2024 rates (8.1%, 2.6%, 3.8%)
        st, body, sec = api_post(
            base,
            "/api/scrape",
            {"url": "https://www.estv.admin.ch/de/mwst-steuersaetze-schweiz", "formats": ["markdown", "structured"]},
        )
        md = body.get("data", {}).get("markdown", "")
        passed = st == 200 and all(rate in md for rate in ("8,1", "2,6", "3,8")) or all(rate in md for rate in ("8.1", "2.6", "3.8"))
        record("gt01_swiss_vat_rates", passed, sec, {"status": st, "chars": len(md)})

        # 2. Ground-truth structured table extraction: ECB key rates
        st, body, sec = api_post(
            base,
            "/api/scrape",
            {
                "url": "https://www.ecb.europa.eu/stats/policy_and_exchange_rates/key_ecb_interest_rates/html/index.en.html",
                "formats": ["structured"],
            },
        )
        tables = body.get("data", {}).get("structured", {}).get("tables", [])
        passed = st == 200 and len(tables) >= 1 and len(tables[0].get("rows", [])) >= 50
        record("gt02_ecb_structured_table", passed, sec, {"status": st, "tables": len(tables), "rows": len(tables[0]["rows"]) if tables else 0})

        # 3. Ground-truth layout-table rejection: Hacker News (0 fake tables)
        st, body, sec = api_post(base, "/api/scrape", {"url": "https://news.ycombinator.com/", "formats": ["structured"]})
        tables = body.get("data", {}).get("structured", {}).get("tables", [])
        record("gt03_hn_layout_table_rejected", st == 200 and len(tables) == 0, sec, {"status": st, "tables": len(tables)})

        # 4. Ground-truth non-Latin UTF-8: Japanese Wikipedia (HTTP)
        st, body, sec = api_post(
            base,
            "/api/scrape",
            {"url": "https://ja.wikipedia.org/wiki/HTTP", "formats": ["markdown", "structured"]},
        )
        md = body.get("data", {}).get("markdown", "")
        passed = st == 200 and "Hypertext Transfer Protocol" in md and "プロトコル" in md
        record("gt04_japanese_utf8", passed, sec, {"status": st, "chars": len(md)})

        # 5. Ground-truth MDX normalization: react.dev useEffect
        st, body, sec = api_post(
            base,
            "/api/scrape",
            {"url": "https://react.dev/reference/react/useEffect", "formats": ["markdown"]},
        )
        md = body.get("data", {}).get("markdown", "")
        passed = st == 200 and "useEffect" in md and "<Intro>" not in md and "{/*" not in md
        record("gt05_react_mdx_clean", passed, sec, {"status": st, "chars": len(md)})

        # 6. Ground-truth full 194-page PDF extraction (maxPages: 250)
        st, body, sec = api_post(
            base,
            "/api/scrape",
            {"url": "https://www.rfc-editor.org/rfc/rfc9110.pdf", "formats": ["markdown"], "maxPages": 250},
        )
        meta = body.get("data", {}).get("metadata", {})
        passed = st == 200 and meta.get("pagesRead") == 194 and body.get("data", {}).get("truncated") is False
        record("gt06_rfc9110_pdf_full_194pp", passed, sec, {"status": st, "pagesRead": meta.get("pagesRead"), "truncated": body.get("data", {}).get("truncated")})

        # 7. Ground-truth CDP waitForSelector + scrollSteps on JS quotes page
        st, body, sec = api_post(
            base,
            "/api/scrape",
            {
                "url": "https://quotes.toscrape.com/js/",
                "formats": ["markdown"],
                "waitForSelector": ".quote .text",
                "scrollSteps": 2,
                "maxAge": 0,
            },
        )
        md = body.get("data", {}).get("markdown", "")
        passed = st == 200 and "Albert Einstein" in md and "J.K. Rowling" in md
        record("gt07_cdp_wait_selector_and_scroll", passed, sec, {"status": st, "chars": len(md)})

        # 8. Ground-truth global totalMaxTokens cross-document evidence pack
        st, body, sec = api_post(
            base,
            "/api/content",
            {
                "urls": [
                    "https://www.rfc-editor.org/rfc/rfc9110.html",
                    "https://www.w3.org/TR/wai-aria-1.2/",
                ],
                "chunks": True,
                "totalMaxTokens": 1200,
                "focusQuery": "Retry-After 429 aria-expanded",
            },
        )
        sources = body.get("sources", [])
        total_tokens = sum(c["tokens"] for s in sources for c in (s.get("chunks") or []))
        combined = "\n".join(s.get("content", "") for s in sources)
        passed = st == 200 and len(sources) >= 1 and total_tokens <= 1200 and "Retry-After" in combined
        record("gt08_global_total_max_tokens", passed, sec, {"status": st, "sources": len(sources), "total_tokens": total_tokens})

        # 9. Ground-truth SSE streaming TTFS (static page arrives long before 10s delayed JS page!)
        st, events, ttfs, total_s = api_post_sse(
            base,
            "/api/content",
            {
                "urls": [
                    "https://quotes.toscrape.com/js-delayed/",
                    "https://www.rfc-editor.org/rfc/rfc2606.txt",
                ],
                "stream": True,
            },
        )
        src_events = [e for e in events if e.get("type") == "source"]
        done_events = [e for e in events if e.get("type") == "done"]
        # Even though js-delayed is URL #1 (10s+), rfc2606.txt must stream FIRST in <2s!
        passed = st == 200 and len(src_events) == 2 and len(done_events) == 1 and ttfs < 2.5 and total_s > 9.0
        record("gt09_sse_streaming_ttfs", passed, total_s, {"status": st, "sources": len(src_events), "ttfs_seconds": round(ttfs, 3)})

        # 10. Ground-truth persistent crawl state surviving a full process restart!
        st1, body1, sec1 = api_post(
            base,
            "/api/crawl",
            {
                "url": "https://books.toscrape.com/",
                "maxDepth": 0,
                "limit": 1,
                "useSitemap": False,
                "incremental": True,
                "sync": True,
            },
        )
        crawl1_id = body1.get("id")
        assert st1 == 200 and crawl1_id
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except Exception:
            proc.kill()

    # Restart a BRAND-NEW Quelle process pointing at the same QUELLE_STATE_DIR!
    proc2, base2 = start_quelle(args.binary, args.lightpanda, state_dir=state_dir)
    try:
        st2, body2, sec2 = api_post(
            base2,
            "/api/crawl",
            {
                "url": "https://books.toscrape.com/",
                "maxDepth": 0,
                "limit": 1,
                "useSitemap": False,
                "sinceCrawlId": crawl1_id,
                "omitUnchangedContent": True,
                "sync": True,
            },
        )
        changes = body2.get("changes") or {}
        passed = st2 == 200 and changes.get("unchanged") == 1 and changes.get("added") == 0
        record("gt10_persistent_crawl_state_across_restart", passed, sec2, {"status": st2, "crawl1_id": crawl1_id, "changes": changes})
    finally:
        proc2.terminate()
        try:
            proc2.wait(timeout=5)
        except Exception:
            proc2.kill()
        shutil.rmtree(state_dir, ignore_errors=True)

    summary = {
        "total": len(results),
        "passed": sum(1 for r in results if r["passed"]),
        "failed": [r["case"] for r in results if not r["passed"]],
        "results": results,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(summary, indent=2) + "\n")
    print(f"\nGround-truth eval: {summary['passed']}/{summary['total']} passed", flush=True)
    if summary["failed"]:
        raise SystemExit(f"Failed ground-truth cases: {summary['failed']}")


if __name__ == "__main__":
    main()
