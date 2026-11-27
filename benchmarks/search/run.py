#!/usr/bin/env python3
"""Paired live retrieval with one frozen Brave response per query; stdlib only."""

import argparse
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
import getpass
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import random
import re
import secrets
import socket
import statistics
import subprocess
import tempfile
import threading
import time
from urllib.error import HTTPError, URLError
from urllib.parse import parse_qs, urlencode, urlsplit
from urllib.request import Request, urlopen

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent


def save(path, data):
    """Write computed benchmark artifacts, never source files or credentials."""
    path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n")


def request(url, payload=None, headers=None, timeout=115):
    start = time.monotonic()
    req = Request(url, data=None if payload is None else json.dumps(payload).encode(),
                  headers={"Content-Type": "application/json", **(headers or {})})
    try:
        try:
            response = urlopen(req, timeout=timeout)
        except HTTPError as error:
            response = error
        with response:
            raw = response.read().decode("utf-8", errors="replace")
            try:
                data = json.loads(raw)
            except ValueError:
                data = {"error": "non_json_response", "body": raw[:2000]}
            return {"status": response.status, "seconds": time.monotonic() - start, "data": data}
    except (URLError, TimeoutError, OSError) as error:
        return {"status": 0, "seconds": time.monotonic() - start,
                "data": {"error": type(error).__name__}}


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def ready(url, process=None, seconds=100):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if process is not None and process.poll() is not None:
            raise RuntimeError("Quelle exited during startup; inspect quelle.log")
        result = request(url, timeout=2)
        if 200 <= result["status"] < 300:
            return
        time.sleep(0.5)
    raise RuntimeError(f"Service did not become ready: {url}")


def official(url, hosts):
    host = (urlsplit(url).hostname or "").lower()
    return any(host == expected or host.endswith("." + expected) for expected in hosts)


def metrics(case, sources, elapsed, errors):
    texts = [source.get("content", "") for source in sources]
    return {
        "seconds": round(elapsed, 3), "sources": len(sources), "errors": len(errors),
        "characters": sum(map(len, texts)),
        "official_sources": sum(official(s["url"], case["hosts"]) for s in sources),
        "signal_matches": [any(re.search(pattern, text, re.I) for text in texts)
                           for pattern in case["signals"]],
    }


def quelle(case, base, token):
    result = request(base + "/api/content", {"query": case["query"], "language": case["language"], "limit": 5,
                     "candidateLimit": 5, "selection": "engine", "followUp": False},
                     {"Authorization": "Bearer " + token})
    data = result["data"]
    sources = [{"url": s["metadata"]["url"], "requested_url": s.get("requestedUrl", s["metadata"]["url"]),
                "title": s["metadata"].get("title", ""), "content": s["content"]}
               for s in data.get("sources", [])]
    errors = data.get("errors", [])
    if data.get("error") and not errors:
        errors = [{"error": data["error"]}]
    measured = metrics(case, sources, result["seconds"], errors)
    measured.update(duplicates_removed=data.get("diagnostics", {}).get("duplicates", 0),
                    completed_fetches=data.get("diagnostics", {}).get("fetchAttempts"))
    # Suppressed duplicates have no source/error row. This is an observation
    # list, not a complete fetch trace; compare supplied candidate sets below.
    return {"raw": result, "sources": sources, "errors": errors,
            "metrics": measured,
            "observed_urls": [s["requested_url"] for s in sources] + [e["url"] for e in errors if "url" in e]}


def firecrawl(case, searx, fire):
    start = time.monotonic()
    search = request(searx + "/search?" + urlencode({"q": case["query"], "language": case["language"],
                     "engines": "braveapi", "categories": "general", "safesearch": 2, "format": "json"}))
    hits = search["data"].get("results", [])[:5]
    def scrape(hit):
        return {"url": hit["url"], "response": request(fire + "/v2/scrape", {
            "url": hit["url"], "formats": ["markdown"], "onlyMainContent": True,
            "timeout": 25000,
        }, timeout=40)}
    with ThreadPoolExecutor(max_workers=4) as pool:
        pages = list(pool.map(scrape, hits))
    elapsed = time.monotonic() - start
    sources, errors = [], []
    for page in pages:
        result = page["response"]
        body = result["data"]
        data = body.get("data") or {}
        content = data.get("markdown", "")
        if result["status"] == 200 and body.get("success") and content.strip():
            metadata = data.get("metadata") or {}
            sources.append({"url": metadata.get("sourceURL", page["url"]), "requested_url": page["url"],
                            "title": metadata.get("title", ""), "content": content})
        else:
            errors.append({"url": page["url"], "status": result["status"], "error": body.get("error", "no_content")})
    if not hits:
        errors.append({"error": "no_search_results", "status": search["status"],
                       "unresponsive_engines": search["data"].get("unresponsive_engines", [])})
    return {"raw": {"search": search, "pages": pages}, "sources": sources, "errors": errors,
            "metrics": metrics(case, sources, elapsed, errors), "attempted_urls": [hit["url"] for hit in hits]}


def report(output, records, *, live_brave=True):
    rng = random.Random(270127)
    review, key = [], {}
    for record in records:
        names = ["quelle", "firecrawl"]
        rng.shuffle(names)
        case = record["case"]
        key[case["id"]] = dict(zip("AB", names))
        review.append({"id": case["id"], "query": case["query"], "criteria": case["criteria"],
                       **{label: {"sources": record[name]["sources"], "scores": {
                           "relevance": None, "completeness": None, "authority": None,
                           "temporal_fit": None, "cleanliness": None}, "notes": ""}
                          for label, name in zip("AB", names)}})
    save(output / "review.json", review)
    save(output / "blind-key.json", key)
    summary = {"queries": len(records)}
    for name in ("quelle", "firecrawl"):
        ms = [r[name]["metrics"] for r in records]
        times = sorted(m["seconds"] for m in ms)
        summary[name] = {
            "queries_with_sources": sum(m["sources"] > 0 for m in ms),
            "sources": sum(m["sources"] for m in ms), "errors": sum(m["errors"] for m in ms),
            "official_sources": sum(m["official_sources"] for m in ms),
            "all_signals_queries": sum(all(m["signal_matches"]) for m in ms),
            "signal_matches": sum(sum(m["signal_matches"]) for m in ms),
            "signal_total": sum(len(m["signal_matches"]) for m in ms),
            "median_retrieval_seconds": statistics.median(times),
            "p95_retrieval_seconds": times[max(0, int(len(times) * .95 + .999) - 1)],
            "duplicates_removed": sum(m.get("duplicates_removed", 0) for m in ms),
        }
    summary["same_supplied_candidate_url_sets"] = sum(r["same_supplied_candidate_url_sets"] for r in records)
    latency_field = "median_live_brave_seconds" if live_brave else "median_archived_brave_seconds"
    summary[latency_field] = statistics.median(r["brave"]["seconds"] for r in records)
    save(output / "summary.json", summary)
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--cases", type=int, default=25, help="Use fewer only for setup validation")
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--snapshots", type=Path, help="Replay existing Brave responses without new API calls")
    parser.add_argument("--only", help="Comma-separated case IDs for an explicitly separate diagnostic rerun")
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    key = None if args.snapshots else os.environ.get("BRAVE_SEARCH_API_KEY") or getpass.getpass("Brave API key: ")
    cases_bytes = (HERE / "queries.json").read_bytes()
    cases = json.loads(cases_bytes)
    assert len(cases) == 25 and len({c["query"] for c in cases}) == 25
    for case in cases:
        for signal in case["signals"]:
            re.compile(signal)
    random.Random(20261127).shuffle(cases)
    if args.only:
        wanted = set(args.only.split(","))
        unknown = wanted - {case["id"] for case in cases}
        if unknown:
            raise ValueError(f"Unknown case IDs: {sorted(unknown)}")
        cases = [case for case in cases if case["id"] in wanted]
    cases = cases[:args.cases]
    if not 1 <= len(cases) <= 25:
        raise ValueError("--cases must be between 1 and 25")
    if not args.skip_build:
        subprocess.run(["cargo", "build", "--release", "--locked"], cwd=REPO, check=True)
    token = secrets.token_hex(24)
    replay_token = secrets.token_hex(24)
    cache, replay_events = {}, []

    class Replay(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            query = parse_qs(urlsplit(self.path).query).get("q", [""])[0]
            authorized = self.headers.get("X-Subscription-Token") == replay_token
            result = cache.get(query) if authorized else None
            status = result["status"] if result else (404 if authorized else 401)
            body = json.dumps(result["data"] if result else {"error": "unrecorded_query"}).encode()
            replay_events.append({"query": query, "status": status, "monotonic": time.monotonic()})
            self.send_response(status or 502)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    # Bound to all interfaces for the isolated Docker bridge; requires random auth.
    server = ThreadingHTTPServer(("0.0.0.0", 0), Replay)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    port = server.server_port
    searx_port, fire_port, quelle_port = free_port(), free_port(), free_port()
    project = "quelle-bench-" + secrets.token_hex(4)
    env = dict(os.environ)
    env.pop("BRAVE_SEARCH_API_KEY", None)
    env.update(BENCH_SEARX_PORT=str(searx_port), BENCH_FIRECRAWL_PORT=str(fire_port))
    compose = ["docker", "compose", "-p", project, "-f", str(HERE / "compose.yml")]
    records = []
    manifest = {
        "started_at": datetime.now(timezone.utc).isoformat(), "project": project,
        "git_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=REPO, text=True).strip(),
        "query_sha256": hashlib.sha256(cases_bytes).hexdigest(), "queries": [c["id"] for c in cases],
        "binary_sha256": hashlib.sha256((REPO / "target/release/quelle").read_bytes()).hexdigest(),
        "lightpanda": os.environ.get("QUELLE_LIGHTPANDA", "lightpanda"),
        "controls": {"count": 5, "country": "CH", "spellcheck": False, "safesearch": "strict",
                     "shared_serp": True, "page_concurrency": 4, "reranker": None,
                     "quelle_selection": "engine", "quelle_candidate_limit": 5, "quelle_follow_up": False,
                     "live_brave": args.snapshots is None, "snapshot_directory": str(args.snapshots) if args.snapshots else None,
                     "quelle_optional_sidecars": False, "firecrawl_timeout_ms": 25000},
    }
    images = ["searxng/searxng:latest", "ghcr.io/firecrawl/firecrawl:latest",
              "ghcr.io/firecrawl/playwright-service:latest", "ghcr.io/firecrawl/nuq-postgres:latest"]
    manifest["images"] = json.loads(subprocess.check_output(["docker", "image", "inspect", *images], text=True))
    # Save only image identity, not arbitrary image configuration/environment.
    manifest["images"] = [{"id": im["Id"], "tags": im.get("RepoTags"), "digests": im.get("RepoDigests")}
                          for im in manifest["images"]]
    save(output / "manifest.json", manifest)
    save(output / "queries.json", cases)
    try:
        with tempfile.TemporaryDirectory(prefix=project + "-") as runtime:
            env["BENCH_RUNTIME"] = runtime
            # Generated private configuration contains an ephemeral replay credential,
            # never the real Brave key. JSON is valid YAML for SearXNG's YAML loader.
            settings = {
                "use_default_settings": {"engines": {"keep_only": []}},
                "engines": [{"name": "braveapi", "engine": "braveapi", "disabled": False, "enable_http": True,
                             "api_key": replay_token, "results_per_page": 5,
                             "base_url": f"http://host.docker.internal:{port}/search"}],
                "server": {"secret_key": secrets.token_hex(32), "limiter": False, "image_proxy": False},
                "search": {"formats": ["json"], "safe_search": 2},
                "outgoing": {"request_timeout": 15.0},
            }
            save(Path(runtime) / "settings.yml", settings)
            log = (output / "services.log").open("w")
            process = None
            try:
                subprocess.run(compose + ["up", "-d"], env=env, stdout=log, stderr=log, check=True)
                qenv = {k: v for k, v in os.environ.items() if not k.startswith("QUELLE_")}
                qenv.update(QUELLE_TOKEN=token, QUELLE_LISTEN=f"127.0.0.1:{quelle_port}",
                            QUELLE_SEARCH_ENGINES="brave", BRAVE_SEARCH_API_KEY=replay_token,
                            BRAVE_SEARCH_URL=f"http://127.0.0.1:{port}/search",
                            QUELLE_LIGHTPANDA=manifest["lightpanda"])
                with (output / "quelle.log").open("w") as qlog:
                    process = subprocess.Popen([str(REPO / "target/release/quelle")], cwd=REPO,
                                               env=qenv, stdout=qlog, stderr=qlog)
                    qbase, sbase, fbase = [f"http://127.0.0.1:{p}" for p in (quelle_port, searx_port, fire_port)]
                    ready(qbase + "/healthz", process)
                    ready(sbase + "/healthz")
                    ready(fbase + "/")
                    smoke = request(fbase + "/v2/scrape", {"url": "https://example.com", "formats": ["markdown"]})
                    save(output / "firecrawl-smoke.json", smoke)
                    if not smoke["data"].get("success"):
                        raise RuntimeError("Firecrawl scrape smoke check failed")
                    probe_query = "quelle benchmark replay readiness"
                    cache[probe_query] = {"status": 200, "data": {"web": {"results": [{
                        "url": "https://example.com/", "title": "Example Domain", "description": "Replay readiness probe"
                    }]}}}
                    probe = request(sbase + "/search?" + urlencode({"q": probe_query, "format": "json", "engines": "braveapi"}))
                    save(output / "searxng-smoke.json", probe)
                    if not probe["data"].get("results"):
                        raise RuntimeError("SearXNG replay smoke check failed")
                    del cache[probe_query]
                    for index, case in enumerate(cases):
                        params = {"q": case["query"], "country": "CH", "search_lang": case["language"],
                                  "ui_lang": "fr-CH" if case["language"] == "fr" else "de-CH", "count": 5,
                                  "spellcheck": "false", "safesearch": "strict", "result_filter": "web",
                                  "text_decorations": "false"}
                        if args.snapshots:
                            brave = json.loads((args.snapshots / (case["id"] + "-brave.json")).read_text())
                        else:
                            brave = request("https://api.search.brave.com/res/v1/web/search?" + urlencode(params),
                                            headers={"X-Subscription-Token": key, "X-Loc-Country": "CH",
                                                     "Cache-Control": "no-cache"}, timeout=30)
                        cache[case["query"]] = brave
                        save(output / (case["id"] + "-brave.json"), brave)
                        record = {"case": case, "brave": brave, "pipeline_order":
                                  ["quelle", "firecrawl"] if index % 2 == 0 else ["firecrawl", "quelle"]}
                        print(f"{index + 1}/{len(cases)} {case['id']} Brave={brave['status']}", flush=True)
                        for name in record["pipeline_order"]:
                            record[name] = quelle(case, qbase, token) if name == "quelle" else firecrawl(case, sbase, fbase)
                            m = record[name]["metrics"]
                            print(f"  {name}: {m['sources']} sources, {m['errors']} errors, {m['seconds']}s", flush=True)
                        record["supplied_brave_urls"] = [h["url"] for h in brave["data"].get("web", {}).get("results", [])][:5]
                        record["same_supplied_candidate_url_sets"] = set(record["supplied_brave_urls"]) == set(record["firecrawl"]["attempted_urls"])
                        records.append(record)
                        save(output / (case["id"] + ".json"), record)
                        report(output, records, live_brave=args.snapshots is None)
                        if not args.snapshots:
                            time.sleep(1.1)  # Respect a one-search-per-second Brave plan.
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
    finally:
        server.shutdown()
        server.server_close()
        save(output / "replay-events.json", replay_events)
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        manifest["completed_queries"] = len(records)
        save(output / "manifest.json", manifest)
    print(json.dumps(report(output, records, live_brave=args.snapshots is None), indent=2), flush=True)


if __name__ == "__main__":
    main()
