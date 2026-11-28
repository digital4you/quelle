#!/usr/bin/env python3
"""Gently-paced, zero-API-key (free engines only) OSS head-to-head benchmark:
  - Quelle (parallel RRF keyless meta-search, Lightpanda + Chromium CDP,
    safe page actions, live crawl SSE, built-in MCP, OCRia-nano-1)
  - Self-Hosted OSS Firecrawl + default keyless SearXNG (6-container stack)

Fairness & IP-politeness safeguards:
  1. Only 2 search arms (Quelle keyless default vs Firecrawl + keyless SearXNG).
  2. Sleeps `--delay` seconds (default 8.0s) after every search call.
  3. Alternates which arm runs first on each query (odd: Quelle first, even:
     Firecrawl first) so neither arm gets systematic rate-limit or warm-cache bias.
  4. Benchmarks OSS-parity capabilities head-to-head without search throttling:
     - Screenshot capture (PNG)
     - Interactive page actions (click pagination on quotes.toscrape.com/js/)
     - Async crawl (Quelle live SSE stream vs Firecrawl poll)
     - Real OCRia-nano-1 CPU OCR on a zero-text-layer scanned PDF
     - Live RSS / container memory footprint
"""

import argparse
import base64
import importlib.util
import json
import os
import secrets
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

from run import HERE, REPO, free_port, ready, request, save

sys.path.insert(0, str(REPO / "clients/python"))
from quelle import QuelleClient


def load_keyless_queries() -> list[dict]:
    spec = importlib.util.spec_from_file_location("bench_keyless", HERE / "bench-keyless.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod.QUERIES, mod.score_sources


def wait_http(url: str, timeout: float = 20.0) -> bool:
    end = time.time() + timeout
    while time.time() < end:
        try:
            with urllib.request.urlopen(url, timeout=1.5) as r:
                if r.status == 200:
                    return True
        except Exception:
            time.sleep(0.2)
    return False


def rss_mib(pid: int) -> float:
    try:
        out = subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)], text=True).strip()
        return round(int(out) / 1024.0, 1)
    except Exception:
        return 0.0


def parse_mem_mib(raw: str) -> float:
    raw = raw.split("/")[0].strip()
    if raw.endswith("GiB"):
        return float(raw[:-3]) * 1024.0
    if raw.endswith("MiB"):
        return float(raw[:-3])
    if raw.endswith("KiB"):
        return float(raw[:-3]) / 1024.0
    return 0.0


def docker_stack_mem(project: str) -> dict:
    try:
        names = subprocess.check_output(
            ["docker", "ps", "--filter", f"label=com.docker.compose.project={project}", "--format", "{{.Names}}"],
            text=True,
        ).strip().splitlines()
        if not names:
            return {"containers": {}, "total_mib": 0.0}
        out = subprocess.check_output(
            ["docker", "stats", "--no-stream", "--format", "{{.Name}}\t{{.MemUsage}}"] + names,
            text=True,
        ).strip().splitlines()
        per = {}
        for line in out:
            if "\t" in line:
                name, mem = line.split("\t", 1)
                short = name.replace(project + "-", "").rsplit("-", 1)[0]
                per[short] = round(parse_mem_mib(mem), 1)
        return {"containers": per, "total_mib": round(sum(per.values()), 1)}
    except Exception:
        return {"containers": {}, "total_mib": 0.0}


def run_quelle_search(qbase: str, qtok: str, item: dict, score_sources) -> dict:
    res = request(
        qbase + "/api/content",
        {
            "query": item["query"],
            "language": item["language"],
            "limit": 5,
            "selection": "heuristic",
        },
        {"Authorization": f"Bearer {qtok}"},
        timeout=35,
    )
    srcs = [
        {"url": s["metadata"]["url"], "content": s.get("content", "")}
        for s in res["data"].get("sources", [])
    ]
    return {"seconds": round(res["seconds"], 3), **score_sources(srcs, item)}


def run_firecrawl_search(fbase: str, item: dict, score_sources) -> dict:
    res = request(
        fbase + "/v2/search",
        {
            "query": item["query"],
            "limit": 5,
            "scrapeOptions": {"formats": ["markdown"], "onlyMainContent": True},
        },
        timeout=45,
    )
    fc_items = (res["data"].get("data") or {}).get("web") or []
    if isinstance(res["data"].get("data"), list):
        fc_items = res["data"]["data"]
    srcs = [
        {
            "url": it.get("url") or (it.get("metadata") or {}).get("sourceURL", ""),
            "content": it.get("markdown", ""),
        }
        for it in fc_items
        if (it.get("markdown") or "").strip()
    ]
    return {"seconds": round(res["seconds"], 3), **score_sources(srcs, item)}


def bench_ocria_scanned_pdf() -> dict:
    """Measures real llamaindex/OCRia-nano-1 CPU OCR on a 3-page zero-text-layer scanned PDF."""
    tmp = tempfile.mkdtemp(prefix="quelle-bench-ocria-")
    model_dir = os.path.join(tmp, "models")
    env = os.environ.copy()
    env["LITEPARSE_MODEL_DIR"] = model_dir
    try:
        t_pull = time.time()
        subprocess.run(
            ["uvx", "--from", "liteparse", "lit", "models", "pull", "ocria-nano", "-q"],
            env=env,
            check=True,
            timeout=120,
        )
        pull_s = round(time.time() - t_pull, 2)

        ps_path = os.path.join(tmp, "scan3.ps")
        scan_pdf = os.path.join(tmp, "scan3.pdf")
        with open(ps_path, "w") as f:
            f.write(
                "%!PS-Adobe-3.0\n"
                "/Helvetica-Bold findfont 24 scalefont setfont\n"
                "72 720 moveto (Page 1: Quarterly Financial Summary) show\n"
                "/Helvetica findfont 14 scalefont setfont\n"
                "72 670 moveto (Revenue Q4: CHF 4,820,000) show\n"
                "72 645 moveto (Operating Margin: 24.5 percent) show\n"
                "showpage\n"
                "/Helvetica-Bold findfont 24 scalefont setfont\n"
                "72 720 moveto (Page 2: Regional Breakdown) show\n"
                "/Helvetica findfont 14 scalefont setfont\n"
                "72 670 moveto (Zurich HQ: 62 percent of volume) show\n"
                "72 645 moveto (Geneva Office: 38 percent of volume) show\n"
                "showpage\n"
                "/Helvetica-Bold findfont 24 scalefont setfont\n"
                "72 720 moveto (Page 3: Compliance Attestation) show\n"
                "/Helvetica findfont 14 scalefont setfont\n"
                "72 670 moveto (Audit Status: Clean Opinion 2026) show\n"
                "showpage\n"
            )
        subprocess.run(
            ["gs", "-q", "-dNOPAUSE", "-dBATCH", "-sDEVICE=pdfwrite", "-dNoOutputFonts", f"-sOutputFile={scan_pdf}", ps_path],
            check=True,
        )
        pt = subprocess.check_output(["pdftotext", scan_pdf, "-"], text=True).strip()
        assert pt == "", f"expected zero text layer, got {pt!r}"

        with open(scan_pdf, "rb") as f:
            pdf_bytes = f.read()

        t0 = time.time()
        proc = subprocess.run(
            [
                "uvx", "--from", "liteparse", "lit", "parse",
                "--format", "markdown",
                "--ocr-backend", "ocria-nano",
                "--image-mode", "off",
                "--max-pages", "10",
                "-q", "-",
            ],
            input=pdf_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            timeout=90,
        )
        elapsed = round(time.time() - t0, 3)
        md = proc.stdout.decode("utf-8", errors="replace").strip()
        expected = ["4,820,000", "24.5", "Zurich", "Geneva", "Clean Opinion 2026"]
        matched = [s for s in expected if s.lower() in md.lower()]
        return {
            "passed": proc.returncode == 0 and len(matched) == len(expected),
            "pages": 3,
            "seconds": elapsed,
            "pages_per_sec": round(3.0 / max(elapsed, 0.01), 2),
            "model_pull_seconds": pull_s,
            "matched_signals": len(matched),
            "total_signals": len(expected),
            "chars": len(md),
        }
    except Exception as exc:
        return {"passed": False, "error": str(exc)}
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default=str(REPO / "target/release/quelle"))
    parser.add_argument("--lightpanda", required=True)
    parser.add_argument("--chrome", default="/usr/bin/google-chrome")
    parser.add_argument("--delay", type=float, default=8.0, help="Seconds to pause between search calls")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    out_dir = args.output.resolve()
    out_dir.mkdir(parents=True, exist_ok=True)
    queries, score_sources = load_keyless_queries()

    searx_port, fire_port = free_port(), free_port()
    q_port, chrome_port = free_port(), free_port()
    q_tok = secrets.token_hex(24)
    project = "quelle-ossparity-" + secrets.token_hex(4)

    env = dict(os.environ)
    env.pop("BRAVE_SEARCH_API_KEY", None)
    env.update(BENCH_SEARX_PORT=str(searx_port), BENCH_FIRECRAWL_PORT=str(fire_port))

    chrome_dir = tempfile.mkdtemp(prefix="quelle-bench-chrome-")
    state_dir = tempfile.mkdtemp(prefix="quelle-bench-state-")
    chrome_proc = q_proc = None

    with tempfile.TemporaryDirectory(prefix=project + "-") as runtime:
        env["BENCH_RUNTIME"] = runtime
        save(
            Path(runtime) / "settings.yml",
            {
                "use_default_settings": True,
                "server": {"secret_key": secrets.token_hex(32), "limiter": False},
                "search": {"formats": ["html", "json"]},
            },
        )
        override = Path(runtime) / "override.yml"
        override.write_text(
            "services:\n"
            "  firecrawl:\n"
            "    environment:\n"
            '      SEARXNG_ENGINES: ""\n'
            '      SEARXNG_CATEGORIES: "general"\n'
        )
        compose = [
            "docker", "compose", "-p", project,
            "-f", str(HERE / "compose.yml"),
            "-f", str(override),
        ]
        log_path = out_dir / "services.log"
        try:
            print("Starting Firecrawl + keyless SearXNG stack and Quelle + Lightpanda + Chromium CDP...", flush=True)
            t_fc_boot = time.time()
            with log_path.open("w") as log:
                subprocess.run(compose + ["up", "-d"], env=env, stdout=log, stderr=log, check=True)
            fbase = f"http://127.0.0.1:{fire_port}"
            ready(fbase + "/")
            ready(f"http://127.0.0.1:{searx_port}/healthz")
            fc_boot_s = round(time.time() - t_fc_boot, 2)

            chrome_proc = subprocess.Popen(
                [
                    args.chrome,
                    "--headless=new",
                    "--no-sandbox",
                    "--disable-gpu",
                    "--disable-dev-shm-usage",
                    f"--remote-debugging-port={chrome_port}",
                    f"--user-data-dir={chrome_dir}",
                ],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            assert wait_http(f"http://127.0.0.1:{chrome_port}/json/version"), "Chromium CDP failed to start"

            t_q_boot = time.time()
            qenv = {k: v for k, v in os.environ.items() if not k.startswith("QUELLE_") and k != "BRAVE_SEARCH_API_KEY"}
            qenv.update(
                {
                    "QUELLE_LISTEN": f"127.0.0.1:{q_port}",
                    "QUELLE_TOKEN": q_tok,
                    "QUELLE_SEARCH_MODE": "fuse",
                    "QUELLE_LIGHTPANDA": args.lightpanda,
                    "QUELLE_CHROMIUM_CDP_URL": f"http://127.0.0.1:{chrome_port}",
                    "QUELLE_STATE_DIR": state_dir,
                }
            )
            q_proc = subprocess.Popen(
                [args.binary],
                cwd=REPO,
                env=qenv,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )
            qbase = f"http://127.0.0.1:{q_port}"
            ready(qbase + "/healthz", q_proc)
            q_boot_s = round(time.time() - t_q_boot, 2)
            qclient = QuelleClient(base_url=qbase, token=q_tok)
            print(f"Boot times: Quelle={q_boot_s}s | Firecrawl+SearXNG stack={fc_boot_s}s\n", flush=True)

            # -----------------------------------------------------------------
            # PART A: OSS-Parity Feature Benchmarks (zero search rate-limit)
            # -----------------------------------------------------------------
            print("=== PART A: OSS-PARITY CAPABILITY HEAD-TO-HEAD ===", flush=True)
            features = {}

            # A1. Screenshot capture
            t0 = time.time()
            q_shot = qclient.scrape("https://example.org", formats=["markdown", "screenshot"])
            q_shot_s = round(time.time() - t0, 3)
            q_png = base64.b64decode(q_shot["data"]["screenshot"].split(",", 1)[1])
            q_shot_ok = q_png[:8] == b"\x89PNG\r\n\x1a\n" and q_shot["data"].get("retrieval") == "chromium"

            fc_shot_res = request(
                fbase + "/v2/scrape",
                {"url": "https://example.org", "formats": ["markdown", "screenshot"]},
                timeout=35,
            )
            fc_shot_data = fc_shot_res["data"].get("data") or {}
            fc_shot_val = fc_shot_data.get("screenshot") or ""
            fc_shot_ok = bool(fc_shot_val) and (
                fc_shot_val.startswith("data:image/") or fc_shot_val.startswith("http") or len(fc_shot_val) > 500
            )
            features["screenshot"] = {
                "quelle": {"passed": q_shot_ok, "seconds": q_shot_s, "png_bytes": len(q_png)},
                "firecrawl": {
                    "passed": fc_shot_ok,
                    "seconds": round(fc_shot_res["seconds"], 3),
                    "status": fc_shot_res["status"],
                    "len": len(fc_shot_val),
                },
            }
            print(
                f"A1 Screenshot  | Quelle (Chromium CDP): {q_shot_s:.2f}s (ok={q_shot_ok}, {len(q_png)}B) | "
                f"Firecrawl: {fc_shot_res['seconds']:.2f}s (ok={fc_shot_ok}, status={fc_shot_res['status']})",
                flush=True,
            )

            # A2. Safe Page Actions (click pagination on quotes.toscrape.com/js/)
            t0 = time.time()
            q_act = qclient.scrape(
                "https://quotes.toscrape.com/js/",
                formats=["markdown"],
                actions=[
                    {"type": "waitForSelector", "selector": "li.next a", "timeoutMs": 4000},
                    {"type": "click", "selector": "li.next a"},
                    {"type": "wait", "milliseconds": 600},
                    {"type": "waitForSelector", "selector": "li.previous a", "timeoutMs": 4000},
                ],
            )
            q_act_s = round(time.time() - t0, 3)
            q_act_md = q_act["data"].get("markdown") or ""
            q_act_ok = "Previous" in q_act_md or "Marilyn Monroe" in q_act_md

            fc_act_res = request(
                fbase + "/v2/scrape",
                {
                    "url": "https://quotes.toscrape.com/js/",
                    "formats": ["markdown"],
                    "actions": [
                        {"type": "wait", "milliseconds": 1500},
                        {"type": "click", "selector": "li.next a"},
                        {"type": "wait", "milliseconds": 1500},
                    ],
                },
                timeout=35,
            )
            fc_act_md = (fc_act_res["data"].get("data") or {}).get("markdown") or ""
            fc_act_ok = "Previous" in fc_act_md or "Marilyn Monroe" in fc_act_md
            features["page_actions_click"] = {
                "quelle": {"passed": q_act_ok, "seconds": q_act_s, "chars": len(q_act_md)},
                "firecrawl": {
                    "passed": fc_act_ok,
                    "seconds": round(fc_act_res["seconds"], 3),
                    "chars": len(fc_act_md),
                    "status": fc_act_res["status"],
                },
            }
            print(
                f"A2 ClickAction | Quelle (Lightpanda):   {q_act_s:.2f}s (ok={q_act_ok}, {len(q_act_md)} chars) | "
                f"Firecrawl: {fc_act_res['seconds']:.2f}s (ok={fc_act_ok}, {len(fc_act_md)} chars)",
                flush=True,
            )

            # A3. Async Crawl (Quelle live SSE stream vs Firecrawl poll)
            t0 = time.time()
            q_started = qclient.crawl("https://example.org", maxDepth=0, limit=1, sync=False)
            q_events = list(qclient.crawl_stream(q_started["id"]))
            q_crawl_s = round(time.time() - t0, 3)
            q_crawl_ok = any(e.get("type") == "page" for e in q_events) and q_events[-1].get("type") == "done"

            t0 = time.time()
            fc_start = request(fbase + "/v2/crawl", {"url": "https://example.org", "limit": 1}, timeout=20)
            fc_id = fc_start["data"].get("id")
            fc_crawl_ok = False
            fc_pages = 0
            if fc_id:
                poll_end = time.time() + 30
                while time.time() < poll_end:
                    st = request(fbase + f"/v2/crawl/{fc_id}", None, timeout=15)
                    status_str = st["data"].get("status")
                    if status_str == "completed":
                        fc_pages = len(st["data"].get("data") or [])
                        fc_crawl_ok = fc_pages >= 1
                        break
                    if status_str in ("failed", "cancelled"):
                        break
                    time.sleep(0.35)
            fc_crawl_s = round(time.time() - t0, 3)
            features["async_crawl"] = {
                "quelle_sse": {"passed": q_crawl_ok, "seconds": q_crawl_s, "events": [e.get("type") for e in q_events]},
                "firecrawl_poll": {"passed": fc_crawl_ok, "seconds": fc_crawl_s, "pages": fc_pages},
            }
            print(
                f"A3 AsyncCrawl  | Quelle (live SSE):     {q_crawl_s:.2f}s (ok={q_crawl_ok}) | "
                f"Firecrawl (poll): {fc_crawl_s:.2f}s (ok={fc_crawl_ok}, pages={fc_pages})",
                flush=True,
            )

            # A4. Real OCRia-nano-1 CPU OCR on 3-page scanned PDF
            ocria = bench_ocria_scanned_pdf()
            features["ocria_nano_scanned_pdf"] = ocria
            print(
                f"A4 OCRia-nano  | 3-page zero-text-layer scanned PDF: "
                f"{ocria.get('seconds')}s ({ocria.get('pages_per_sec')} pages/s, "
                f"{ocria.get('matched_signals')}/{ocria.get('total_signals')} signals, ok={ocria.get('passed')})",
                flush=True,
            )

            # -----------------------------------------------------------------
            # PART B: Gently-Paced Keyless Meta-Search (Free Engines Only!)
            # -----------------------------------------------------------------
            print(f"\n=== PART B: FREE KEYLESS META-SEARCH (12 queries, {args.delay:.1f}s delay, alternating order) ===", flush=True)
            records = []
            for idx, item in enumerate(queries, 1):
                row = {"id": item["id"], "query": item["query"], "first_arm": "quelle" if idx % 2 == 1 else "firecrawl"}
                if idx % 2 == 1:
                    row["quelle_fused"] = run_quelle_search(qbase, q_tok, item, score_sources)
                    time.sleep(args.delay)
                    row["firecrawl_searxng"] = run_firecrawl_search(fbase, item, score_sources)
                else:
                    row["firecrawl_searxng"] = run_firecrawl_search(fbase, item, score_sources)
                    time.sleep(args.delay)
                    row["quelle_fused"] = run_quelle_search(qbase, q_tok, item, score_sources)

                records.append(row)
                qf = row["quelle_fused"]
                fc = row["firecrawl_searxng"]
                sig_n = len(item["signals"])
                print(
                    f"{idx:2d}/{len(queries)} {item['id']:20s} ({row['first_arm'][0].upper()}) | "
                    f"Quelle: {qf['signal_matches']}/{sig_n} sig, {qf['sources']} src, {qf['official_hits']} off, {qf['seconds']:.2f}s | "
                    f"FC+SearX: {fc['signal_matches']}/{sig_n} sig, {fc['sources']} src, {fc['official_hits']} off, {fc['seconds']:.2f}s",
                    flush=True,
                )
                if idx < len(queries):
                    time.sleep(args.delay)

            # Live footprint measurement while both are warm
            q_mem = rss_mib(q_proc.pid)
            fc_mem = docker_stack_mem(project)
            footprint = {
                "quelle_boot_seconds": q_boot_s,
                "firecrawl_stack_boot_seconds": fc_boot_s,
                "quelle_rss_mib": q_mem,
                "firecrawl_stack_mem": fc_mem,
            }
            print(
                f"\nFootprint: Quelle RSS={q_mem} MiB (boot {q_boot_s}s) vs "
                f"Firecrawl+SearXNG 6 containers={fc_mem['total_mib']} MiB (boot {fc_boot_s}s)",
                flush=True,
            )

        finally:
            for p in (q_proc, chrome_proc):
                if p:
                    p.terminate()
                    try:
                        p.wait(timeout=5)
                    except Exception:
                        p.kill()
            subprocess.run(compose + ["down", "-v", "--remove-orphans"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            shutil.rmtree(chrome_dir, ignore_errors=True)
            shutil.rmtree(state_dir, ignore_errors=True)

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
        "delay_seconds": args.delay,
        "queries": len(records),
        "quelle_fused": summarize("quelle_fused"),
        "firecrawl_searxng": summarize("firecrawl_searxng"),
        "quelle_faster_count": sum(
            1 for r in records if r["quelle_fused"]["seconds"] < r["firecrawl_searxng"]["seconds"]
        ),
        "features": features,
        "footprint": footprint,
    }
    save(out_dir / "records.json", records)
    save(out_dir / "summary.json", summary)
    print("\n=== FINAL SUMMARY ===")
    print(json.dumps(summary, indent=2), flush=True)


if __name__ == "__main__":
    main()
