/**
 * Optional Playwright/Chromium sidecar for Quelle (Tier 3 fallback).
 *
 * Contract:
 *   GET  /healthz -> 200 {"ok":true}
 *   POST /scrape  <- {"url":"https://...","wait_after_load":1000}
 *                 -> 200 {"content":"<html>...","pageStatusCode":200,"url":"https://..."}
 *
 * Security hardening:
 *   - Blocks all private, loopback, link-local, CGNAT, multicast, reserved IPv4/IPv6
 *     ranges (127/8, 10/8, 172.16/12, 192.168/16, 100.64/10, 169.254/16, fc00::/7,
 *     fe80::/10, etc.) PLUS operator-supplied BLOCKED_NETWORKS CIDRs.
 *   - Routes Chromium through an embedded DNS-pinning HTTP/CONNECT proxy on loopback
 *     (with `--proxy-bypass-list=<-loopback>` and WebRTC non-proxied UDP disabled),
 *     eliminating DNS-rebinding (TOCTOU) and subresource SSRF at the socket layer.
 */

import http from "node:http";
import dns from "node:dns/promises";
import net from "node:net";
import { chromium } from "playwright";

const PORT = Number(process.env.PORT || 3003);
const HOST = process.env.HOST || "0.0.0.0";
const MAX_CONCURRENT = Math.max(1, Math.min(16, Number(process.env.MAX_CONCURRENT_PAGES || 4)));
const BLOCK_MEDIA = (process.env.BLOCK_MEDIA || "true").toLowerCase() !== "false";
const DEFAULT_LOCALE = process.env.BROWSER_LOCALE || "de-CH";
const DEFAULT_TIMEZONE = process.env.BROWSER_TIMEZONE || "Europe/Zurich";
const PROXY_SERVER = (process.env.PROXY_SERVER || "").trim();
const USER_AGENT =
  process.env.BROWSER_USER_AGENT ||
  "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36";

const MAX_BODY_BYTES = 64 * 1024;
const NAV_TIMEOUT_MS = 15_000;

// ---------------------------------------------------------------------------
// IP & CIDR Filtering (built-in reserved ranges + BLOCKED_NETWORKS)
// ---------------------------------------------------------------------------

function ipv4ToInt(ip) {
  const parts = ip.split(".").map(Number);
  if (parts.length !== 4 || parts.some((n) => !Number.isInteger(n) || n < 0 || n > 255)) {
    return null;
  }
  return (((parts[0] << 24) >>> 0) | (parts[1] << 16) | (parts[2] << 8) | parts[3]) >>> 0;
}

function ipv6ToBigInt(ip) {
  const lower = ip.toLowerCase();
  if (lower.includes(".")) {
    // IPv4-embedded IPv6 such as ::ffff:127.0.0.1
    const lastColon = lower.lastIndexOf(":");
    const v4 = ipv4ToInt(lower.slice(lastColon + 1));
    if (v4 === null) return null;
    const prefix = lower.slice(0, lastColon);
    const expanded = `${prefix}:${((v4 >>> 16) & 0xffff).toString(16)}:${(v4 & 0xffff).toString(16)}`;
    return ipv6ToBigInt(expanded);
  }
  const halves = lower.split("::");
  if (halves.length > 2) return null;
  const left = halves[0] ? halves[0].split(":") : [];
  const right = halves.length === 2 && halves[1] ? halves[1].split(":") : [];
  const missing = 8 - (left.length + right.length);
  if (halves.length === 1 && missing !== 0) return null;
  if (halves.length === 2 && missing < 0) return null;
  const groups = [...left, ...Array(missing).fill("0"), ...right];
  if (groups.length !== 8) return null;
  let value = 0n;
  for (const g of groups) {
    if (!/^[0-9a-f]{1,4}$/.test(g)) return null;
    value = (value << 16n) | BigInt(parseInt(g, 16));
  }
  return value;
}

function parseCidr(raw) {
  const text = raw.trim();
  const [addr, prefixStr] = text.includes("/") ? text.split("/", 2) : [text, null];
  if (net.isIPv4(addr)) {
    const prefix = prefixStr === null ? 32 : Number(prefixStr);
    if (!Number.isInteger(prefix) || prefix < 0 || prefix > 32) {
      throw new Error(`Invalid IPv4 CIDR: ${raw}`);
    }
    const mask = prefix === 0 ? 0 : (0xffffffff << (32 - prefix)) >>> 0;
    const base = (ipv4ToInt(addr) & mask) >>> 0;
    return { family: 4, base, mask };
  }
  if (net.isIPv6(addr)) {
    const prefix = prefixStr === null ? 128 : Number(prefixStr);
    if (!Number.isInteger(prefix) || prefix < 0 || prefix > 128) {
      throw new Error(`Invalid IPv6 CIDR: ${raw}`);
    }
    const mask = prefix === 0 ? 0n : ((1n << 128n) - 1n) ^ ((1n << BigInt(128 - prefix)) - 1n);
    const val = ipv6ToBigInt(addr);
    if (val === null) throw new Error(`Invalid IPv6 CIDR: ${raw}`);
    return { family: 6, base: val & mask, mask };
  }
  throw new Error(`Invalid BLOCKED_NETWORKS entry: ${raw}`);
}

const BLOCKED_NETWORKS = (process.env.BLOCKED_NETWORKS || process.env.QUELLE_BLOCKED_NETWORKS || "")
  .split(",")
  .map((s) => s.trim())
  .filter(Boolean)
  .map(parseCidr);

function matchesBlockedCidr(ip) {
  if (net.isIPv4(ip)) {
    const v = ipv4ToInt(ip);
    if (v === null) return true;
    return BLOCKED_NETWORKS.some((c) => c.family === 4 && ((v & c.mask) >>> 0) === c.base);
  }
  if (net.isIPv6(ip)) {
    const v = ipv6ToBigInt(ip);
    if (v === null) return true;
    return BLOCKED_NETWORKS.some((c) => c.family === 6 && (v & c.mask) === c.base);
  }
  return true;
}

function isGloballyRoutableIpv4(ip) {
  const parts = ip.split(".").map(Number);
  if (parts.length !== 4 || parts.some((n) => Number.isNaN(n) || n < 0 || n > 255)) return false;
  const [a, b, c] = parts;
  if (a === 0 || a === 10 || a === 127 || a >= 224) return false;
  if (a === 100 && b >= 64 && b <= 127) return false;
  if (a === 169 && b === 254) return false;
  if (a === 172 && b >= 16 && b <= 31) return false;
  if (a === 192 && b === 0 && (c === 0 || c === 2)) return false;
  if (a === 192 && b === 88 && c === 99) return false;
  if (a === 192 && b === 168) return false;
  if (a === 198 && (b === 18 || b === 19)) return false;
  if (a === 198 && b === 51 && c === 100) return false;
  if (a === 203 && b === 0 && c === 113) return false;
  return true;
}

function isAllowedIp(ip) {
  if (net.isIPv4(ip)) {
    return isGloballyRoutableIpv4(ip) && !matchesBlockedCidr(ip);
  }
  if (net.isIPv6(ip)) {
    const lower = ip.toLowerCase();
    if (lower.startsWith("::ffff:")) {
      const v4 = lower.slice(7);
      return net.isIPv4(v4) && isAllowedIp(v4);
    }
    const firstHex = parseInt(lower.split(":")[0] || "0", 16);
    if (Number.isNaN(firstHex) || firstHex < 0x2000 || firstHex > 0x3fff) return false;
    if (lower.startsWith("2001:db8:") || lower.startsWith("2002:") || lower.startsWith("3fff:")) {
      return false;
    }
    return !matchesBlockedCidr(ip);
  }
  return false;
}

function isBlockedHostname(host) {
  const h = host.toLowerCase().replace(/\.$/, "");
  return (
    !h ||
    h === "localhost" ||
    h.endsWith(".localhost") ||
    h.endsWith(".local") ||
    h.endsWith(".internal")
  );
}

/**
 * Resolves `host` as an absolute DNS name (trailing dot prevents cluster
 * search-domain hijacking), validates every returned IP against private/reserved
 * ranges and BLOCKED_NETWORKS, and returns the first verified IP.
 */
async function resolveAllowedIp(host) {
  const cleanHost = host.replace(/^\[|\]$/g, "");
  if (isBlockedHostname(cleanHost)) {
    throw new Error("blocked_host");
  }
  if (net.isIP(cleanHost)) {
    if (!isAllowedIp(cleanHost)) throw new Error("blocked_ip");
    return cleanHost;
  }
  const absolute = cleanHost.endsWith(".") ? cleanHost : `${cleanHost}.`;
  let records;
  try {
    records = await dns.lookup(absolute, { all: true });
  } catch {
    records = await dns.lookup(cleanHost, { all: true });
  }
  if (!records.length || !records.every((r) => isAllowedIp(r.address))) {
    throw new Error("blocked_ip");
  }
  return records[0].address;
}

async function assertPublicUrl(raw) {
  let parsed;
  try {
    parsed = new URL(raw);
  } catch {
    throw new Error("invalid_url");
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw new Error("unsupported_scheme");
  }
  if (parsed.port && parsed.port !== "80" && parsed.port !== "443") {
    throw new Error("blocked_port");
  }
  await resolveAllowedIp(parsed.hostname);
  return parsed.toString();
}

// ---------------------------------------------------------------------------
// Embedded DNS-Pinning HTTP/CONNECT Forward Proxy (defeats DNS rebinding)
// ---------------------------------------------------------------------------

function startPinningProxy() {
  return new Promise((resolve, reject) => {
    const proxy = http.createServer(async (req, res) => {
      // Plain HTTP proxy requests
      let target;
      try {
        target = new URL(req.url);
        if (target.protocol !== "http:") throw new Error("unsupported_scheme");
        const port = Number(target.port || 80);
        if (port !== 80 && port !== 443) throw new Error("blocked_port");
        const ip = await resolveAllowedIp(target.hostname);
        const upstream = http.request(
          {
            host: ip,
            port,
            method: req.method,
            path: `${target.pathname}${target.search}`,
            headers: { ...req.headers, host: target.host },
          },
          (upRes) => {
            res.writeHead(upRes.statusCode || 502, upRes.headers);
            upRes.pipe(res);
          }
        );
        upstream.on("error", () => {
          if (!res.headersSent) res.writeHead(502);
          res.end();
        });
        req.pipe(upstream);
      } catch {
        res.writeHead(403, { "Content-Type": "text/plain" });
        res.end("blocked_by_ssrf_policy");
      }
    });

    // HTTPS / WSS tunneling via CONNECT
    proxy.on("connect", async (req, clientSocket, head) => {
      try {
        const lastColon = req.url.lastIndexOf(":");
        if (lastColon <= 0) throw new Error("invalid_connect");
        const host = req.url.slice(0, lastColon);
        const port = Number(req.url.slice(lastColon + 1));
        if (port !== 80 && port !== 443) throw new Error("blocked_port");
        const ip = await resolveAllowedIp(host);

        const upstream = net.connect({ host: ip, port }, () => {
          clientSocket.write("HTTP/1.1 200 Connection Established\r\n\r\n");
          if (head && head.length) upstream.write(head);
          upstream.pipe(clientSocket);
          clientSocket.pipe(upstream);
        });
        upstream.setTimeout(20_000, () => upstream.destroy());
        upstream.on("error", () => clientSocket.destroy());
        clientSocket.on("error", () => upstream.destroy());
      } catch {
        clientSocket.write("HTTP/1.1 403 Forbidden\r\n\r\n");
        clientSocket.destroy();
      }
    });

    proxy.on("error", reject);
    proxy.listen(0, "127.0.0.1", () => {
      const addr = proxy.address();
      resolve({ server: proxy, url: `http://127.0.0.1:${addr.port}` });
    });
  });
}

// ---------------------------------------------------------------------------
// Concurrency Limiter & Shared Browser Lifecycle
// ---------------------------------------------------------------------------

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

let pinningProxy = null;
let browserPromise = null;

async function getBrowser() {
  if (browserPromise) {
    const b = await browserPromise;
    if (b.isConnected()) return b;
  }
  const effectiveProxy = PROXY_SERVER || pinningProxy.url;
  const launchOpts = {
    headless: true,
    proxy: {
      server: effectiveProxy,
      // `<-loopback>` removes Chromium's implicit localhost/127.0.0.1 proxy bypass!
      bypass: "<-loopback>",
    },
    args: [
      "--disable-blink-features=AutomationControlled",
      "--disable-dev-shm-usage",
      "--no-first-run",
      "--no-default-browser-check",
      "--disable-background-networking",
      "--disable-renderer-backgrounding",
      "--disable-features=IsolateOrigins,site-per-process",
      "--webrtc-ip-handling-policy=disable_non_proxied_udp",
      "--force-webrtc-ip-handling-policy",
    ],
  };
  browserPromise = chromium.launch(launchOpts);
  return browserPromise;
}

// ---------------------------------------------------------------------------
// Realistic User-Like Scraping
// ---------------------------------------------------------------------------

const STEALTH_INIT_SCRIPT = () => {
  Object.defineProperty(navigator, "webdriver", { get: () => undefined });
  Object.defineProperty(navigator, "languages", {
    get: () => ["de-CH", "de", "fr-CH", "en-US", "en"],
  });
  Object.defineProperty(navigator, "plugins", {
    get: () => [
      { name: "Chrome PDF Plugin", filename: "internal-pdf-viewer" },
      { name: "Chrome PDF Viewer", filename: "mhjfbmdgcfjbbpaeojofohoefgiehjai" },
      { name: "Native Client", filename: "internal-nacl-plugin" },
    ],
  });
  if (!window.chrome) {
    window.chrome = { runtime: {} };
  }
};

async function humanScroll(page) {
  try {
    const height = await page.evaluate(() => document.body?.scrollHeight || 0);
    if (height <= 900) return;
    const steps = Math.min(4, Math.ceil(height / 850));
    for (let i = 1; i <= steps; i++) {
      const y = Math.min(height, Math.round((height * i) / steps));
      await page.evaluate((pos) => window.scrollTo({ top: pos, behavior: "smooth" }), y);
      await page.waitForTimeout(160 + ((i * 45) % 140));
    }
    await page.evaluate(() => window.scrollTo(0, 0));
  } catch {
    // Ignore scroll errors on detached/navigating frames
  }
}

async function scrapeUrl(rawUrl, waitAfterLoadMs) {
  const targetUrl = await assertPublicUrl(rawUrl);
  await acquireSlot();
  let context;
  try {
    const browser = await getBrowser();
    const width = 1366 + Math.floor(Math.random() * 6) * 24;
    const height = 860 + Math.floor(Math.random() * 5) * 20;

    context = await browser.newContext({
      userAgent: USER_AGENT,
      viewport: { width, height },
      locale: DEFAULT_LOCALE,
      timezoneId: DEFAULT_TIMEZONE,
      deviceScaleFactor: 1,
      colorScheme: "light",
      extraHTTPHeaders: {
        "Accept-Language": "de-CH,de;q=0.9,fr-CH;q=0.8,fr;q=0.7,it;q=0.6,en;q=0.5",
      },
    });
    await context.addInitScript(STEALTH_INIT_SCRIPT);

    const page = await context.newPage();

    // Fast media blocking + defense-in-depth URL check (socket-level pinning proxy
    // enforces the real SSRF boundary even against DNS rebinding).
    await page.route("**/*", async (route) => {
      const req = route.request();
      const type = req.resourceType();
      if (BLOCK_MEDIA && (type === "image" || type === "media" || type === "font")) {
        return route.abort();
      }
      const reqUrl = req.url();
      if (reqUrl.startsWith("http://") || reqUrl.startsWith("https://")) {
        try {
          await assertPublicUrl(reqUrl);
        } catch {
          return route.abort();
        }
      }
      return route.continue();
    });

    let pageStatusCode = 200;
    const response = await page.goto(targetUrl, {
      waitUntil: "domcontentloaded",
      timeout: NAV_TIMEOUT_MS,
    });
    if (response) {
      pageStatusCode = response.status();
    }

    await page
      .waitForLoadState("networkidle", { timeout: 4500 })
      .catch(() => {});

    await humanScroll(page);

    const extraWait = Math.max(0, Math.min(10_000, Number(waitAfterLoadMs || 600)));
    if (extraWait > 0) {
      await page.waitForTimeout(extraWait);
    }

    const content = await page.content();
    const finalUrl = page.url();
    return { content, pageStatusCode, url: finalUrl };
  } finally {
    if (context) {
      await context.close().catch(() => {});
    }
    releaseSlot();
  }
}

// ---------------------------------------------------------------------------
// HTTP Server
// ---------------------------------------------------------------------------

function readJsonBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    req.on("data", (chunk) => {
      size += chunk.length;
      if (size > MAX_BODY_BYTES) {
        reject(new Error("body_too_large"));
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on("end", () => {
      try {
        resolve(JSON.parse(Buffer.concat(chunks).toString("utf8")));
      } catch {
        reject(new Error("invalid_json"));
      }
    });
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

const server = http.createServer(async (req, res) => {
  if (req.method === "GET" && req.url === "/healthz") {
    return sendJson(res, 200, {
      ok: true,
      active,
      maxConcurrent: MAX_CONCURRENT,
      blockedNetworks: BLOCKED_NETWORKS.length,
    });
  }
  if (req.method === "POST" && req.url === "/scrape") {
    let body;
    try {
      body = await readJsonBody(req);
    } catch (err) {
      return sendJson(res, 400, { error: err.message });
    }
    if (!body || typeof body.url !== "string" || !body.url.trim()) {
      return sendJson(res, 400, { error: "missing_url" });
    }
    try {
      const result = await scrapeUrl(body.url.trim(), body.wait_after_load);
      return sendJson(res, 200, result);
    } catch (err) {
      const msg = err?.message || "scrape_failed";
      const code = msg.startsWith("blocked_") || msg === "invalid_url" ? 400 : 502;
      return sendJson(res, code, { error: msg });
    }
  }
  return sendJson(res, 404, { error: "not_found" });
});

async function main() {
  if (!PROXY_SERVER) {
    pinningProxy = await startPinningProxy();
  }
  server.listen(PORT, HOST, () => {
    console.log(
      JSON.stringify({
        msg: "quelle browser sidecar listening",
        address: `${HOST}:${PORT}`,
        maxConcurrent: MAX_CONCURRENT,
        blockMedia: BLOCK_MEDIA,
        pinningProxy: Boolean(pinningProxy),
        blockedNetworks: BLOCKED_NETWORKS.length,
      })
    );
  });
}

async function shutdown() {
  server.close();
  if (pinningProxy) pinningProxy.server.close();
  if (browserPromise) {
    const b = await browserPromise.catch(() => null);
    if (b) await b.close().catch(() => {});
  }
  process.exit(0);
}

process.on("SIGTERM", shutdown);
process.on("SIGINT", shutdown);

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
