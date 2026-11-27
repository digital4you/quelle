#!/usr/bin/env python3
"""Exercise the production readiness expression in Lightpanda on delayed pages."""

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import subprocess
import threading
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lightpanda", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    script = (Path(__file__).resolve().parents[2] / "src/pages/readiness.js").read_text()
    activity_path = Path(__file__).resolve().parents[2] / "src/pages/readiness-init.js"
    activity = activity_path.read_text() if activity_path.exists() else ""
    prose = "Substantive article evidence about persistence and durability. " * 20
    cases = {
        "synchronous-document-write": (
            f"<script>document.write('<main><p>{prose}SYNC_READY</p></main>');</script>",
            "SYNC_READY", 0, 0.32,
        ),
        "cleared-timers": (
            f"<main><p>{prose}CLEARED_READY</p></main>"
            "<script>const t = setTimeout(() => {}, 5000); clearTimeout(t); const i = setInterval(() => {}, 5000); clearInterval(i);</script>",
            "CLEARED_READY", 0, 0.32,
        ),
        "pending-timer-content": (
            f"<main><p>{prose}</p></main>"
            "<script>setTimeout(function(value) { document.querySelector('main').insertAdjacentHTML('beforeend', '<p>' + value + '</p>'); }, 200, 'TIMER_EVIDENCE');</script>",
            "TIMER_EVIDENCE", 0.18, 2.5,
        ),
        "pending-fetch-body": (
            f"<main><p>{prose}</p></main>"
            "<script>fetch('/fetch-evidence').then(r => r.text()).then(t => document.querySelector('main').insertAdjacentHTML('beforeend', '<p>' + t + '</p>'));</script>",
            "FETCH_EVIDENCE", 0.18, 2.5,
        ),
        "pending-xhr-content": (
            f"<main><p>{prose}</p></main>"
            "<script>const r = new XMLHttpRequest(); r.open('GET', '/xhr-evidence'); r.onload = function() { document.querySelector('main').insertAdjacentHTML('beforeend', '<p>' + this.responseText + '</p>'); }; r.send();</script>",
            "XHR_EVIDENCE", 0.18, 2.5,
        ),
        "ready-with-background-request": (
            f"<main><h1>Article</h1><p>{prose}</p></main>"
            "<script>fetch('/background');</script>",
            "Substantive article evidence", 0, 2.5,
        ),
        "delayed-application": (
            "<app-root><main><div role='status'>Laden der Anwendung...</div></main>"
            f"<footer><p>{'Browser support notice. ' * 250}</p></footer></app-root>"
            f"<script>setTimeout(() => document.querySelector('main').innerHTML = '<h1>Law</h1><p>{prose}LOADED_LAW</p>', 900);</script>",
            "LOADED_LAW", 0.85, 3.0,
        ),
        "delayed-table": (
            f"<main><h1>Rates</h1><p>{prose}</p><div data-component='table'></div></main>"
            "<script>setTimeout(() => document.querySelector('[data-component=table]').innerHTML = '<table><tr><td>COMPLETE_RATES</td><td>8.1%</td></tr></table>', 900);</script>",
            "COMPLETE_RATES", 0.85, 3.0,
        ),
        "hidden-placeholder": (
            f"<main><p>{prose}</p><div hidden data-component='table'></div>"
            "<div hidden role='status'>Loading application</div></main>",
            "Substantive article evidence", 0, 2.5,
        ),
    }

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            delay_body = 0
            if self.path == "/background":
                time.sleep(5)
                body = b"done"
            elif self.path in ("/fetch-evidence", "/xhr-evidence"):
                body = b"FETCH_EVIDENCE" if self.path == "/fetch-evidence" else b"XHR_EVIDENCE"
                delay_body = 0.2
            else:
                body = cases[self.path.lstrip("/")][0].encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.end_headers()
            if delay_body:
                self.wfile.flush()
                time.sleep(delay_body)
            try:
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    results = []
    try:
        for name, (_, expected, minimum, maximum) in cases.items():
            started = time.monotonic()
            result = subprocess.run([
                args.lightpanda, "fetch", "--dump", "html", "--wait-until", "domcontentloaded",
                "--inject-script", activity,
                "--wait-script", f"({script})(300)", "--wait-ms", "4000", "--terminate-ms", "5000",
                "--block-private-networks", "--block-cidrs", "-127.0.0.1/32",
                "--v8-max-heap-mb", "128", "--log-level", "fatal",
                f"http://127.0.0.1:{server.server_port}/{name}",
            ], capture_output=True, timeout=7, env={"LIGHTPANDA_DISABLE_TELEMETRY": "true"})
            elapsed = time.monotonic() - started
            # Inspect the DOM without script text, so the delayed assignment's
            # source string cannot make a premature dump look complete.
            from html.parser import HTMLParser

            class VisibleText(HTMLParser):
                def __init__(self):
                    super().__init__()
                    self.script = False
                    self.parts = []

                def handle_starttag(self, tag, attrs):
                    if tag == "script":
                        self.script = True

                def handle_endtag(self, tag):
                    if tag == "script":
                        self.script = False

                def handle_data(self, data):
                    if not self.script:
                        self.parts.append(data)

            dom = VisibleText()
            dom.feed(result.stdout.decode(errors="replace"))
            passed = result.returncode == 0 and expected in " ".join(dom.parts) and minimum <= elapsed < maximum
            row = {"case": name, "seconds": round(elapsed, 3), "exit": result.returncode, "passed": passed}
            results.append(row)
            print(json.dumps(row), flush=True)
    finally:
        server.shutdown()
        server.server_close()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(results, indent=2) + "\n")
    if not all(row["passed"] for row in results):
        raise SystemExit("Readiness regression failed")


if __name__ == "__main__":
    main()
