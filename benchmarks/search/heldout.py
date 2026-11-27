#!/usr/bin/env python3
"""Held-out 20-URL paired extraction quality & latency benchmark: Quelle vs Firecrawl."""

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import random
import secrets
import statistics
import subprocess
import tempfile
import time

from run import HERE, REPO, free_port, ready, request, save


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--skip-build", action="store_true")
    args = parser.parse_args()

    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    cases_bytes = (HERE / "heldout-urls.json").read_bytes()
    cases = json.loads(cases_bytes)
    if not args.skip_build:
        subprocess.run(["cargo", "build", "--release", "--locked"], cwd=REPO, check=True)

    token = secrets.token_hex(24)
    searx_port, fire_port, quelle_port = free_port(), free_port(), free_port()
    project = "quelle-heldout-" + secrets.token_hex(4)
    env = dict(os.environ)
    env.pop("BRAVE_SEARCH_API_KEY", None)
    env.update(BENCH_SEARX_PORT=str(searx_port), BENCH_FIRECRAWL_PORT=str(fire_port))
    compose = ["docker", "compose", "-p", project, "-f", str(HERE / "compose.yml")]

    manifest = {
        "started_at": datetime.now(timezone.utc).isoformat(),
        "project": project,
        "git_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=REPO, text=True).strip(),
        "urls_sha256": hashlib.sha256(cases_bytes).hexdigest(),
        "binary_sha256": hashlib.sha256((REPO / "target/release/quelle").read_bytes()).hexdigest(),
        "lightpanda": os.environ.get("QUELLE_LIGHTPANDA", "lightpanda"),
        "controls": {"mode": "direct_url", "onlyMainContent": True, "timeout_ms": 25000},
    }
    save(output / "manifest.json", manifest)

    records = []
    with tempfile.TemporaryDirectory(prefix=project + "-") as runtime:
        env["BENCH_RUNTIME"] = runtime
        save(Path(runtime) / "settings.yml", {"server": {"secret_key": secrets.token_hex(32), "limiter": False}})
        log = (output / "services.log").open("w")
        process = None
        try:
            subprocess.run(compose + ["up", "-d"], env=env, stdout=log, stderr=log, check=True)
            qenv = {k: v for k, v in os.environ.items() if not k.startswith("QUELLE_")}
            qenv.update(
                QUELLE_TOKEN=token,
                QUELLE_LISTEN=f"127.0.0.1:{quelle_port}",
                QUELLE_SEARCH_ENGINES="none",
                QUELLE_LIGHTPANDA=manifest["lightpanda"],
            )
            with (output / "quelle.log").open("w") as qlog:
                process = subprocess.Popen([str(REPO / "target/release/quelle")], cwd=REPO, env=qenv, stdout=qlog, stderr=qlog)
                qbase, fbase = f"http://127.0.0.1:{quelle_port}", f"http://127.0.0.1:{fire_port}"
                ready(qbase + "/healthz", process)
                ready(fbase + "/")
                smoke = request(fbase + "/v2/scrape", {"url": "https://example.com", "formats": ["markdown"]})
                if not smoke["data"].get("success"):
                    raise RuntimeError("Firecrawl smoke check failed")

                for idx, case in enumerate(cases):
                    order = ["quelle", "firecrawl"] if idx % 2 == 0 else ["firecrawl", "quelle"]
                    row = {"case": case, "order": order}
                    for arm in order:
                        if arm == "quelle":
                            res = request(
                                qbase + "/api/content",
                                {"urls": [case["url"]], "limit": 1},
                                {"Authorization": "Bearer " + token},
                                timeout=40,
                            )
                            data = res["data"]
                            srcs = data.get("sources", [])
                            content = srcs[0]["content"] if srcs else ""
                            err = None if srcs else (data.get("errors") or [{"error": data.get("error", "no_content")}])[0].get("error")
                            row["quelle"] = {
                                "seconds": round(res["seconds"], 3),
                                "ok": bool(srcs and content.strip()),
                                "chars": len(content),
                                "error": err,
                                "url": srcs[0]["metadata"]["url"] if srcs else case["url"],
                                "title": srcs[0]["metadata"].get("title", "") if srcs else "",
                                "content": content,
                            }
                        else:
                            res = request(
                                fbase + "/v2/scrape",
                                {"url": case["url"], "formats": ["markdown"], "onlyMainContent": True, "timeout": 25000},
                                timeout=40,
                            )
                            body = res["data"]
                            data = body.get("data") or {}
                            content = data.get("markdown", "")
                            ok = bool(res["status"] == 200 and body.get("success") and content.strip())
                            row["firecrawl"] = {
                                "seconds": round(res["seconds"], 3),
                                "ok": ok,
                                "chars": len(content),
                                "error": None if ok else body.get("error", "no_content"),
                                "url": (data.get("metadata") or {}).get("sourceURL", case["url"]),
                                "title": (data.get("metadata") or {}).get("title", ""),
                                "content": content,
                            }
                    print(
                        f"{idx+1}/{len(cases)} {case['id']} ({case['category']}): "
                        f"Q={'OK' if row['quelle']['ok'] else row['quelle']['error']} {row['quelle']['chars']}c {row['quelle']['seconds']}s | "
                        f"F={'OK' if row['firecrawl']['ok'] else row['firecrawl']['error']} {row['firecrawl']['chars']}c {row['firecrawl']['seconds']}s",
                        flush=True,
                    )
                    records.append(row)
                    save(output / f"{case['id']}.json", row)
        finally:
            if process is not None and process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            subprocess.run(compose + ["logs", "--no-color"], env=env, stdout=log, stderr=log)
            subprocess.run(compose + ["down", "--volumes", "--remove-orphans"], env=env, stdout=log, stderr=log)
            log.close()

    rng = random.Random(20261127)
    review, key = [], {}
    for row in records:
        names = ["quelle", "firecrawl"]
        rng.shuffle(names)
        cid = row["case"]["id"]
        key[cid] = dict(zip("AB", names))
        review.append({
            "id": cid,
            "category": row["case"]["category"],
            "url": row["case"]["url"],
            "criteria": row["case"]["criteria"],
            **{label: {"ok": row[name]["ok"], "chars": row[name]["chars"], "error": row[name]["error"], "content": row[name]["content"]} for label, name in zip("AB", names)},
        })
    save(output / "review.json", review)
    save(output / "blind-key.json", key)

    summary = {"urls": len(records)}
    for arm in ("quelle", "firecrawl"):
        times = sorted(r[arm]["seconds"] for r in records)
        summary[arm] = {
            "ok": sum(r[arm]["ok"] for r in records),
            "errors": sum(not r[arm]["ok"] for r in records),
            "median_seconds": statistics.median(times),
            "p95_seconds": times[max(0, int(len(times) * 0.95 + 0.999) - 1)],
        }
    summary["quelle_faster"] = sum(r["quelle"]["seconds"] < r["firecrawl"]["seconds"] for r in records)
    manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
    save(output / "manifest.json", manifest)
    save(output / "summary.json", summary)
    print(json.dumps(summary, indent=2), flush=True)


if __name__ == "__main__":
    main()
