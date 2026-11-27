/**
 * Optional OCRia-nano-1 (LiteParse) CPU OCR sidecar for Quelle.
 *
 * Called automatically by Quelle when a PDF has no embedded text layer
 * (scanned PDF) and `QUELLE_OCR_URL` is set.
 *
 * Contract:
 *   GET  /healthz -> 200 {"ok":true,"backend":"ocria-nano"}
 *   POST /ocr     <- raw PDF bytes (Content-Type: application/pdf)
 *                 -> 200 {"markdown":"...","pages":N}
 */

import http from "node:http";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import crypto from "node:crypto";

const execFileAsync = promisify(execFile);

const PORT = Number(process.env.PORT || 3004);
const HOST = process.env.HOST || "0.0.0.0";
const MAX_PDF_BYTES = 12 * 1024 * 1024;
const MAX_CONCURRENT = Math.max(1, Math.min(8, Number(process.env.MAX_CONCURRENT_OCR || 2)));
const OCR_TIMEOUT_MS = Number(process.env.OCR_TIMEOUT_MS || 45_000);
const OCR_BACKEND = process.env.OCR_BACKEND || "ocria-nano";
const LIT_BIN = process.env.LIT_BIN || "lit";

let active = 0;
const waiters = [];

async function acquireSlot() {
  if (active < MAX_CONCURRENT) {
    active += 1;
    return;
  }
  await new Promise((resolve) => waiters.push(resolve));
  active += 1;
}

function releaseSlot() {
  active = Math.max(0, active - 1);
  const next = waiters.shift();
  if (next) next();
}

function readRawBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    req.on("data", (chunk) => {
      size += chunk.length;
      if (size > MAX_PDF_BYTES) {
        reject(new Error("pdf_too_large"));
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on("end", () => resolve(Buffer.concat(chunks)));
    req.on("error", reject);
  });
}

function sendJson(res, status, payload) {
  const body = JSON.stringify(payload);
  res.writeHead(status, {
    "Content-Type": "application/json; charset=utf-8",
    "Content-Length": Buffer.byteLength(body),
    "Cache-Control": "no-store",
  });
  res.end(body);
}

async function runOcr(pdfBytes) {
  if (!pdfBytes.subarray(0, 5).equals(Buffer.from("%PDF-"))) {
    throw new Error("not_a_pdf");
  }
  await acquireSlot();
  const tmpFile = path.join(os.tmpdir(), `quelle-ocr-${crypto.randomUUID()}.pdf`);
  try {
    await fs.writeFile(tmpFile, pdfBytes);
    const { stdout } = await execFileAsync(
      LIT_BIN,
      ["parse", tmpFile, "--format", "markdown", "--ocr-backend", OCR_BACKEND],
      { timeout: OCR_TIMEOUT_MS, maxBuffer: 8 * 1024 * 1024 }
    );
    const markdown = stdout.trim();
    const pages = Math.max(1, (markdown.match(/^---$/gm) || []).length + 1);
    return { markdown, pages };
  } finally {
    await fs.unlink(tmpFile).catch(() => {});
    releaseSlot();
  }
}

const server = http.createServer(async (req, res) => {
  if (req.method === "GET" && req.url === "/healthz") {
    return sendJson(res, 200, {
      ok: true,
      backend: OCR_BACKEND,
      active,
      maxConcurrent: MAX_CONCURRENT,
    });
  }
  if (req.method === "POST" && req.url === "/ocr") {
    let bytes;
    try {
      bytes = await readRawBody(req);
    } catch (err) {
      return sendJson(res, err.message === "pdf_too_large" ? 413 : 400, {
        error: err.message,
      });
    }
    try {
      const result = await runOcr(bytes);
      return sendJson(res, 200, result);
    } catch (err) {
      const msg = err?.message || "ocr_failed";
      return sendJson(res, msg === "not_a_pdf" ? 400 : 502, { error: msg });
    }
  }
  return sendJson(res, 404, { error: "not_found" });
});

server.listen(PORT, HOST, () => {
  console.log(
    JSON.stringify({
      msg: "quelle ocr sidecar listening",
      address: `${HOST}:${PORT}`,
      backend: OCR_BACKEND,
      maxConcurrent: MAX_CONCURRENT,
    })
  );
});

function shutdown() {
  server.close();
  process.exit(0);
}
process.on("SIGTERM", shutdown);
process.on("SIGINT", shutdown);
