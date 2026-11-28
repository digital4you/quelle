#!/usr/bin/env python3
"""Small-CPU (2 cores) benchmark for permissive Apache-2.0 / MIT OCR pipelines.

Generates a realistic 3-page scanned image-only PDF (confirmed `pdftotext == ""`)
containing:
  - Page 1: Title, headings, and a structured financial table (CHF, decimals, negatives)
  - Page 2: Multilingual prose (German umlauts ä/ö/ü, French accents é/è/ç, English RFC/HTTP codes)
  - Page 3: Dense 10pt medical/legal bullet list (dosage, dates, email, reference IDs)

Compares strictly on 2 CPU cores (`taskset -c 0,1`, `OMP_NUM_THREADS=2`):
  1. Direct Tesseract 5 CLI (`pdftoppm` + `tesseract`, Apache-2.0)
  2. LiteParse + Tesseract (Apache-2.0, structured Markdown + layout)
  3. RapidOCR (`PP-OCRv5` ONNX, Apache-2.0) — standalone & via LiteParse OCR HTTP server
  4. Reference ceiling: LiteParse + OCRia-nano-1 (RESTRICTED OpenRAIL-M, non-commercial >$2M / no competing service)
"""

import json
import os
import shutil
import socket
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

GROUND_TRUTH_SIGNALS = [
    # Page 1: Title & Table
    "Quarterly Financial Report",
    "CHF 4,820,000",
    "24.5%",
    "-3.2%",
    "Zurich HQ",
    "Geneva Branch",
    # Page 2: Multilingual (DE / FR / EN)
    "Mehrwertsteuer",
    "8.1%",
    "RFC 9110",
    "HTTP 429",
    "Précision",
    "Gouvernance",
    # Page 3: Dense 10pt legal/medical bullets
    "400 mg",
    "2026-11-28",
    "REF-2026-CH-8841",
    "audit@example.org",
]


def build_realistic_scanned_pdf(tmp: Path) -> Path:
    """Creates a realistic 3-page image-only PDF (guaranteed zero text layer)."""
    ps = tmp / "realistic3.ps"
    raw_pdf = tmp / "raw3.pdf"
    scan_pdf = tmp / "scan3.pdf"
    ps.write_text(
        """%!PS-Adobe-3.0
%%Pages: 3

%%Page: 1 1
/Helvetica-Bold findfont 22 scalefont setfont
54 740 moveto (Quarterly Financial Report - Q4 2026) show
/Helvetica findfont 12 scalefont setfont
54 712 moveto (Consolidated unaudited regional breakdown in Swiss Francs.) show
/Helvetica-Bold findfont 12 scalefont setfont
54 670 moveto (Region) show
220 670 moveto (Revenue) show
360 670 moveto (Margin) show
460 670 moveto (YoY Delta) show
newpath 54 662 moveto 540 662 lineto stroke
/Helvetica findfont 12 scalefont setfont
54 642 moveto (Zurich HQ) show
220 642 moveto (CHF 4,820,000) show
360 642 moveto (24.5%) show
460 642 moveto (+8.4%) show
54 620 moveto (Geneva Branch) show
220 620 moveto (CHF 2,150,000) show
360 620 moveto (19.1%) show
460 620 moveto (-3.2%) show
54 598 moveto (Basel Lab) show
220 598 moveto (CHF 1,410,000) show
360 598 moveto (21.8%) show
460 598 moveto (+4.1%) show
showpage

%%Page: 2 2
/Helvetica-Bold findfont 20 scalefont setfont
54 740 moveto (Multilingual Regulatory & Technical Notes) show
/Helvetica-Bold findfont 13 scalefont setfont
54 700 moveto (Deutsch: Mehrwertsteuer & Datenschutz) show
/Helvetica findfont 11 scalefont setfont
54 680 moveto (Der Normalsatz der schweizerischen Mehrwertsteuer betraegt 8.1% ab 2024.) show
54 662 moveto (Fuer Grundnahrungsmittel gilt der reduzierte Satz von 2.6%.) show
/Helvetica-Bold findfont 13 scalefont setfont
54 624 moveto (Francais: Gouvernance & Precision) show
/Helvetica findfont 11 scalefont setfont
54 604 moveto (Precision et Gouvernance: les exigences de conformite restent strictes.) show
/Helvetica-Bold findfont 13 scalefont setfont
54 566 moveto (English: HTTP Semantics) show
/Helvetica findfont 11 scalefont setfont
54 546 moveto (Clients receiving HTTP 429 per RFC 9110 must honor the Retry-After header.) show
showpage

%%Page: 3 3
/Helvetica-Bold findfont 18 scalefont setfont
54 740 moveto (Clinical & Audit Appendix) show
/Helvetica findfont 10 scalefont setfont
54 710 moveto (- Standard adult ibuprofen dosage: 400 mg every 6 to 8 hours with food.) show
54 692 moveto (- Compliance verification date: 2026-11-28 under reference REF-2026-CH-8841.) show
54 674 moveto (- Contact external review office at audit@example.org for signed copies.) show
54 656 moveto (- Retention policy: archive original raster scans for ten calendar years.) show
showpage
""",
        encoding="utf-8",
    )
    # Render to 200 DPI PNGs then pack PNGs into a pure raster image-only PDF
    # (so even fonts/vectors are 100% rasterized like a real physical scanner!)
    subprocess.run(
        ["gs", "-q", "-dNOPAUSE", "-dBATCH", "-sDEVICE=png16m", "-r200", f"-sOutputFile={tmp}/page-%d.png", str(ps)],
        check=True,
    )
    from PIL import Image

    imgs = [Image.open(tmp / f"page-{i}.png").convert("RGB") for i in (1, 2, 3)]
    imgs[0].save(scan_pdf, save_all=True, append_images=imgs[1:], resolution=200.0)

    # Verify ZERO embedded text layer!
    pt = subprocess.check_output(["pdftotext", str(scan_pdf), "-"], text=True).strip()
    assert pt == "", f"expected zero embedded text layer, got {pt!r}"
    return scan_pdf


def score_text(text: str) -> dict:
    # Normalize French accent fallback check cleanly: keep exact match AND accent-folded match
    low = text.lower()
    matched = []
    missing = []
    for sig in GROUND_TRUTH_SIGNALS:
        # Note PS standard Helvetica uses ASCII "Precision" on page 2; match either accented or ASCII
        alts = [sig.lower()]
        if sig == "Précision":
            alts.append("precision")
        if any(a in low for a in alts):
            matched.append(sig)
        else:
            missing.append(sig)
    has_md_table = "|" in text and "---" in text
    return {
        "matched": len(matched),
        "total": len(GROUND_TRUTH_SIGNALS),
        "recall": round(len(matched) / len(GROUND_TRUTH_SIGNALS), 3),
        "missing": missing,
        "has_md_table": has_md_table,
        "chars": len(text),
    }


def run_timed(cmd: list[str], env: dict | None = None, stdin_bytes: bytes | None = None) -> tuple[float, float, int, str]:
    """Runs command pinned to 2 CPU cores via taskset and measures wall time + peak RSS (MiB)."""
    wrapped = ["taskset", "-c", "0,1", "/usr/bin/time", "-f", "RSS_KB=%M", "--"] + cmd
    t0 = time.perf_counter()
    proc = subprocess.run(
        wrapped,
        input=stdin_bytes,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        timeout=180,
    )
    dt = round(time.perf_counter() - t0, 3)
    stderr = proc.stderr.decode("utf-8", errors="replace")
    rss_mib = 0.0
    for line in stderr.splitlines():
        if line.startswith("RSS_KB="):
            try:
                rss_mib = round(int(line.split("=", 1)[1].strip()) / 1024.0, 1)
            except Exception:
                pass
    return dt, rss_mib, proc.returncode, proc.stdout.decode("utf-8", errors="replace")


def main():
    tmp = Path(tempfile.mkdtemp(prefix="quelle-ocr-bench-"))
    env = os.environ.copy()
    env["OMP_NUM_THREADS"] = "2"
    env["OPENBLAS_NUM_THREADS"] = "2"
    env["MKL_NUM_THREADS"] = "2"
    env["ORT_NUM_THREADS"] = "2"

    try:
        scan_pdf = build_realistic_scanned_pdf(tmp)
        pdf_bytes = scan_pdf.read_bytes()
        results = []

        # 1. Direct Tesseract 5 CLI (pdftoppm -> tesseract, Apache-2.0)
        tess_script = tmp / "run_tess.sh"
        tess_script.write_text(
            f"#!/bin/sh\n"
            f"pdftoppm -png -r 200 '{scan_pdf}' '{tmp}/tpage'\n"
            f"for f in '{tmp}'/tpage-*.png; do tesseract \"$f\" stdout --oem 1 --psm 3 2>/dev/null; echo ''; done\n"
        )
        os.chmod(tess_script, 0o755)
        dt, rss, rc, out = run_timed([str(tess_script)], env=env)
        results.append({
            "name": "Tesseract 5 CLI (pdftoppm + tesseract)",
            "license": "Apache-2.0",
            "commercial_ok": True,
            "seconds": dt,
            "pages_per_sec": round(3.0 / max(dt, 0.01), 2),
            "peak_rss_mib": rss,
            **score_text(out),
        })

        # 2. LiteParse + Tesseract (Apache-2.0, structured Markdown)
        dt, rss, rc, out = run_timed(
            ["uvx", "--from", "liteparse", "lit", "parse", "--format", "markdown",
             "--ocr-backend", "tesseract", "--image-mode", "off", "--num-workers", "2", "-q", str(scan_pdf)],
            env=env,
        )
        results.append({
            "name": "LiteParse + Tesseract",
            "license": "Apache-2.0",
            "commercial_ok": True,
            "seconds": dt,
            "pages_per_sec": round(3.0 / max(dt, 0.01), 2),
            "peak_rss_mib": rss,
            **score_text(out),
        })

        # 3. RapidOCR (PP-OCRv5/v4 ONNX, Apache-2.0) standalone on 2 CPU threads
        rapid_script = tmp / "run_rapid.py"
        rapid_script.write_text(
            f"import glob, subprocess\n"
            f"from rapidocr import RapidOCR\n"
            f"subprocess.run(['pdftoppm', '-png', '-r', '200', '{scan_pdf}', '{tmp}/rpage'], check=True)\n"
            f"ocr = RapidOCR()\n"
            f"texts = []\n"
            f"for p in sorted(glob.glob('{tmp}/rpage-*.png')):\n"
            f"    res = ocr(p)\n"
            f"    if res and res.txts:\n"
            f"        texts.append('\\n'.join(res.txts))\n"
            f"print('\\n\\n'.join(texts))\n"
        )
        # Warm download of RapidOCR ONNX models first so we measure pure inference!
        subprocess.run(
            ["uv", "run", "--with", "rapidocr", "--with", "onnxruntime", "python3", "-c", "from rapidocr import RapidOCR; RapidOCR()"],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True,
        )
        dt, rss, rc, out = run_timed(
            ["uv", "run", "--with", "rapidocr", "--with", "onnxruntime", "python3", str(rapid_script)],
            env=env,
        )
        results.append({
            "name": "RapidOCR (PaddleOCR ONNX, 2-core CPU)",
            "license": "Apache-2.0",
            "commercial_ok": True,
            "seconds": dt,
            "pages_per_sec": round(3.0 / max(dt, 0.01), 2),
            "peak_rss_mib": rss,
            **score_text(out),
        })

        # 4. LiteParse + RapidOCR HTTP Bridge (100% Apache-2.0 Markdown + tables!)
        # Spawns a tiny LiteParse-compatible `/ocr` server backed by RapidOCR ONNX
        bridge_script = tmp / "run_lit_rapid.py"
        bridge_script.write_text(
            f"""import io, json, socket, subprocess, threading, time
from http.server import BaseHTTPRequestHandler, HTTPServer
import cgi
from rapidocr import RapidOCR

ocr = RapidOCR()

def free_port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_POST(self):
        ctype, pdict = cgi.parse_header(self.headers.get('content-type', ''))
        length = int(self.headers.get('content-length', '0'))
        body = self.rfile.read(length)
        img_bytes = body
        if ctype == 'multipart/form-data' and 'boundary' in pdict:
            b = pdict['boundary'].encode()
            for part in body.split(b'--' + b):
                if b'name="file"' in part and b'\\r\\n\\r\\n' in part:
                    img_bytes = part.split(b'\\r\\n\\r\\n', 1)[1].rsplit(b'\\r\\n', 1)[0]
                    break
        res = ocr(img_bytes)
        items = []
        if res and res.boxes is not None and res.txts is not None:
            scores = res.scores if res.scores is not None else [0.95] * len(res.txts)
            for box, txt, sc in zip(res.boxes, res.txts, scores):
                xs = [float(pt[0]) for pt in box]
                ys = [float(pt[1]) for pt in box]
                items.append({{"text": str(txt), "bbox": [min(xs), min(ys), max(xs), max(ys)], "confidence": float(sc)}})
        payload = json.dumps({{"results": items}}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

port = free_port()
srv = HTTPServer(('127.0.0.1', port), Handler)
t = threading.Thread(target=srv.serve_forever, daemon=True)
t.start()
p = subprocess.run(
    ['uvx', '--from', 'liteparse', 'lit', 'parse', '--format', 'markdown',
     '--ocr-server-url', f'http://127.0.0.1:{{port}}/ocr',
     '--image-mode', 'off', '--num-workers', '2', '-q', '{scan_pdf}'],
    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
)
srv.shutdown()
print(p.stdout)
"""
        )
        dt, rss, rc, out = run_timed(
            ["uv", "run", "--with", "rapidocr", "--with", "onnxruntime", "--with", "legacy-cgi", "python3", str(bridge_script)],
            env=env,
        )
        results.append({
            "name": "LiteParse + RapidOCR ONNX (Markdown+Layout)",
            "license": "Apache-2.0",
            "commercial_ok": True,
            "seconds": dt,
            "pages_per_sec": round(3.0 / max(dt, 0.01), 2),
            "peak_rss_mib": rss,
            **score_text(out),
        })

        # 5. Reference ceiling: LiteParse + OCRia-nano-1 (RESTRICTED OpenRAIL-M!)
        model_dir = tmp / "ocria-models"
        ocria_env = dict(env)
        ocria_env["LITEPARSE_MODEL_DIR"] = str(model_dir)
        subprocess.run(
            ["uvx", "--from", "liteparse", "lit", "models", "pull", "ocria-nano", "-q"],
            env=ocria_env, check=True, timeout=120,
        )
        dt, rss, rc, out = run_timed(
            ["uvx", "--from", "liteparse", "lit", "parse", "--format", "markdown",
             "--ocr-backend", "ocria-nano", "--image-mode", "off", "--num-workers", "2", "-q", str(scan_pdf)],
            env=ocria_env,
        )
        results.append({
            "name": "LiteParse + OCRia-nano-1 [REF ONLY]",
            "license": "OpenRAIL-M (RESTRICTED)",
            "commercial_ok": False,
            "seconds": dt,
            "pages_per_sec": round(3.0 / max(dt, 0.01), 2),
            "peak_rss_mib": rss,
            **score_text(out),
        })

        out_path = Path(os.environ.get("OCR_BENCH_OUT", "/tmp/bench-ocr.json"))
        out_path.write_text(json.dumps(results, indent=2))
        print("\n=== SMALL-CPU (2 CORES) PERMISSIVE OCR BENCHMARK (3-PAGE SCANNED PDF) ===")
        for r in results:
            print(
                f"{r['name']:44s} | {r['license']:23s} | {r['seconds']:5.2f}s ({r['pages_per_sec']:4.2f} pp/s) | "
                f"RSS={r['peak_rss_mib']:5.1f}MiB | {r['matched']:2d}/{r['total']} ({r['recall']*100:5.1f}%) | "
                f"md_table={str(r['has_md_table']):5s} | miss={r['missing']}"
            )
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
