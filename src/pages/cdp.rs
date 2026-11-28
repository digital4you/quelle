//! Persistent Lightpanda CDP server pool, minimal RFC 6455 WebSocket client,
//! non-content asset blocking, and adaptive render-wait policy.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    process::{Child, Command},
    sync::Mutex,
    time::Instant,
};
use url::Url;

use super::{
    BLOCKED_RANGES, Network, PAGE_BYTES, PageError, blocked_networks, public_url,
    resolve_public_addrs,
};

/// Maximum incoming CDP WebSocket message size (16 MiB) so large rendered DOMs
/// such as statutory collections never fail serialization.
const MAX_CDP_MESSAGE: usize = 16 * 1024 * 1024;
const NAVIGATE_TIMEOUT: Duration = Duration::from_secs(12);
const CLEANUP_TIMEOUT: Duration = Duration::from_millis(400);
const POLL_INTERVAL: Duration = Duration::from_millis(40);

/// URL wildcard patterns blocked during browser rendering so images, fonts,
/// media streams, and ad/analytics beacons never waste bandwidth or stall load.
pub(crate) const BLOCKED_ASSET_PATTERNS: &[&str] = &[
    // Raster images & favicons
    "*://*/*.png",
    "*://*/*.png?*",
    "*://*/*.jpg",
    "*://*/*.jpg?*",
    "*://*/*.jpeg",
    "*://*/*.jpeg?*",
    "*://*/*.gif",
    "*://*/*.gif?*",
    "*://*/*.webp",
    "*://*/*.webp?*",
    "*://*/*.avif",
    "*://*/*.avif?*",
    "*://*/*.ico",
    "*://*/*.ico?*",
    // Web fonts
    "*://*/*.woff",
    "*://*/*.woff?*",
    "*://*/*.woff2",
    "*://*/*.woff2?*",
    "*://*/*.ttf",
    "*://*/*.ttf?*",
    "*://*/*.eot",
    "*://*/*.eot?*",
    "*://*/*.otf",
    "*://*/*.otf?*",
    // Audio / video streams
    "*://*/*.mp4",
    "*://*/*.mp4?*",
    "*://*/*.webm",
    "*://*/*.webm?*",
    "*://*/*.mp3",
    "*://*/*.mp3?*",
    "*://*/*.wav",
    "*://*/*.wav?*",
    // Common ad & analytics beacons
    "*doubleclick.net*",
    "*googlesyndication.com*",
    "*google-analytics.com*",
    "*googletagmanager.com*",
    "*connect.facebook.net*",
    "*hotjar.com*",
    "*segment.io*",
    "*mixpanel.com*",
];

static CDP_ENABLED: AtomicBool = AtomicBool::new(true);
static CDP_SUBRESOURCE_BLOCKS: AtomicU64 = AtomicU64::new(0);

pub fn set_cdp_pool_enabled(enabled: bool) {
    CDP_ENABLED.store(enabled, Ordering::Relaxed);
}

pub(crate) fn cdp_pool_enabled() -> bool {
    cfg!(unix) && CDP_ENABLED.load(Ordering::Relaxed)
}

pub fn cdp_subresource_blocks() -> u64 {
    CDP_SUBRESOURCE_BLOCKS.load(Ordering::Relaxed)
}

/// Validates whether a Chromium subresource or redirect URL is permitted by the
/// SSRF policy: allows inline `about:blank` / `about:srcdoc` / `data:` / `blob:`
/// URIs and public `http(s)://` URLs; rejects private IPs, loopback, metadata,
/// blocked CIDRs, credentials, and local schemes (`file://`, `chrome://`, etc.).
pub(crate) fn cdp_subresource_allowed(raw_url: &str) -> bool {
    let trimmed = raw_url.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.eq_ignore_ascii_case("about:blank") || trimmed.eq_ignore_ascii_case("about:srcdoc") {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("data:") {
        return true;
    }
    if let Some(rest) = lower.strip_prefix("blob:") {
        if rest.starts_with("https://") || rest.starts_with("http://") {
            return Url::parse(&trimmed[5..])
                .ok()
                .and_then(|u| public_url(u).ok())
                .is_some();
        }
        return false;
    }
    let Ok(parsed) = Url::parse(trimmed) else {
        return false;
    };
    public_url(parsed).is_ok()
}

/// Performs full URL + DNS resolution SSRF verification for an intercepted
/// Chromium subresource or redirect URL.
async fn cdp_subresource_is_public(raw_url: &str) -> bool {
    if !cdp_subresource_allowed(raw_url) {
        return false;
    }
    let trimmed = raw_url.trim();
    let lower = trimmed.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return true;
    }
    let Ok(parsed) = Url::parse(trimmed) else {
        return false;
    };
    let Ok(clean) = public_url(parsed) else {
        return false;
    };
    tokio::time::timeout(Duration::from_secs(2), resolve_public_addrs(&clean))
        .await
        .ok()
        .is_some_and(|res| res.is_ok())
}

/// Validates a caller-supplied CSS selector for `waitForSelector` or actions.
pub fn valid_wait_selector(selector: &str) -> bool {
    let trimmed = selector.trim();
    !trimmed.is_empty()
        && trimmed.len() <= 256
        && !trimmed.chars().any(char::is_control)
        && scraper::Selector::parse(trimmed).is_ok()
}

/// Scroll direction for safe page actions.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ScrollDirection {
    #[default]
    Down,
    Up,
}

/// Safe, scoped browser actions executed after initial page readiness.
/// Arbitrary JavaScript execution is intentionally excluded.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum PageAction {
    Click {
        selector: String,
    },
    Wait {
        milliseconds: u64,
    },
    #[serde(rename_all = "camelCase")]
    WaitForSelector {
        selector: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    Scroll {
        #[serde(default)]
        direction: ScrollDirection,
        #[serde(default)]
        pixels: Option<i32>,
        #[serde(default)]
        steps: Option<u8>,
    },
    Press {
        key: String,
    },
}

const ALLOWED_PRESS_KEYS: &[&str] = &[
    "Enter",
    "Escape",
    "Tab",
    "Space",
    "ArrowDown",
    "ArrowUp",
    "ArrowLeft",
    "ArrowRight",
    "PageDown",
    "PageUp",
    "Home",
    "End",
];

/// Validates a sequence of safe page actions (bounded count, CSS selectors, wait caps,
/// and allowlisted navigation keys).
pub fn valid_actions(actions: &[PageAction]) -> bool {
    if actions.len() > 16 {
        return false;
    }
    let mut total_wait_ms = 0u64;
    for action in actions {
        match action {
            PageAction::Click { selector } => {
                if !valid_wait_selector(selector) {
                    return false;
                }
            }
            PageAction::Wait { milliseconds } => {
                if !(1..=5_000).contains(milliseconds) {
                    return false;
                }
                total_wait_ms = total_wait_ms.saturating_add(*milliseconds);
                if total_wait_ms > 10_000 {
                    return false;
                }
            }
            PageAction::WaitForSelector {
                selector,
                timeout_ms,
            } => {
                if !valid_wait_selector(selector) {
                    return false;
                }
                if let Some(ms) = timeout_ms
                    && !(100..=10_000).contains(ms)
                {
                    return false;
                }
            }
            PageAction::Scroll { pixels, steps, .. } => {
                if let Some(px) = pixels
                    && !(1..=20_000).contains(px)
                {
                    return false;
                }
                if let Some(st) = steps
                    && !(1..=10).contains(st)
                {
                    return false;
                }
            }
            PageAction::Press { key } => {
                if !ALLOWED_PRESS_KEYS.contains(&key.as_str()) {
                    return false;
                }
            }
        }
    }
    true
}

/// Builds a JSON-escaped, injection-safe JS expression for a single `PageAction`.
pub(crate) fn build_action_expression(action: &PageAction) -> Option<String> {
    match action {
        PageAction::Click { selector } => {
            let encoded = serde_json::to_string(selector.trim()).ok()?;
            Some(format!(
                "(() => {{ \
                    const el = document.querySelector({encoded}); \
                    if (!el) return false; \
                    if (typeof el.scrollIntoView === 'function') el.scrollIntoView({{ block: 'center' }}); \
                    if (typeof el.click === 'function') {{ el.click(); return true; }} \
                    el.dispatchEvent(new MouseEvent('click', {{ bubbles: true, cancelable: true }})); \
                    return true; \
                 }})()"
            ))
        }
        PageAction::Scroll {
            direction, pixels, ..
        } => {
            let delta = pixels.unwrap_or(800).clamp(1, 20_000);
            let signed = match direction {
                ScrollDirection::Down => delta,
                ScrollDirection::Up => -delta,
            };
            Some(format!(
                "(() => {{ window.scrollBy(0, {signed}); return true; }})()"
            ))
        }
        PageAction::Press { key } => {
            let encoded = serde_json::to_string(key.as_str()).ok()?;
            Some(format!(
                "(() => {{ \
                    const target = document.activeElement || document.body || document.documentElement; \
                    if (!target) return false; \
                    const ev = new KeyboardEvent('keydown', {{ key: {encoded}, bubbles: true, cancelable: true }}); \
                    target.dispatchEvent(ev); \
                    const evUp = new KeyboardEvent('keyup', {{ key: {encoded}, bubbles: true, cancelable: true }}); \
                    target.dispatchEvent(evUp); \
                    return true; \
                 }})()"
            ))
        }
        PageAction::Wait { .. } | PageAction::WaitForSelector { .. } => None,
    }
}

/// Output returned by a CDP render session (DOM HTML plus optional PNG screenshot).
pub(crate) struct RenderOutput {
    pub html: Vec<u8>,
    pub truncated: bool,
    pub screenshot: Option<String>,
}

/// Normalizes a Chromium `webSocketDebuggerUrl` so it keeps the configured
/// endpoint's host and port (handling Chrome returning `localhost` inside containers).
pub(crate) fn normalize_chromium_ws_url(endpoint: &Url, raw_ws: &str) -> Option<Url> {
    let mut ws = Url::parse(raw_ws).ok()?;
    if !matches!(ws.scheme(), "ws" | "wss") {
        return None;
    }
    if let Some(host) = endpoint.host_str() {
        ws.set_host(Some(host)).ok()?;
    }
    if let Some(port) = endpoint.port_or_known_default() {
        let _ = ws.set_port(Some(port));
    }
    Some(ws)
}

/// Builds the CDP readiness polling expression with optional safe JSON-encoded
/// CSS selector waiting.
pub(crate) fn build_poll_expression(threshold: usize, wait_for_selector: Option<&str>) -> String {
    let selector_check = match wait_for_selector.map(str::trim).filter(|s| !s.is_empty()) {
        Some(sel) => {
            let encoded = serde_json::to_string(sel).unwrap_or_else(|_| "\"\"".to_owned());
            format!(
                "const selEl = document.querySelector({encoded}); \
                 if (!selEl) {{ const a = globalThis.__quelleActivity; return (a && typeof a.busy === 'function' && a.busy()) ? 1 : 0; }} "
            )
        }
        None => String::new(),
    };
    format!(
        "(() => {{ \
            const t = (document.title || '').trim().toLowerCase(); \
            if (t === 'just a moment...' || t === 'attention required! | cloudflare' || t === 'access denied' || t === 'robot check' || document.querySelector('#challenge-form, #cf-challenge-running')) return 3; \
            {selector_check}\
            const ready = ({readiness})({threshold}); \
            if (ready) return 2; \
            const a = globalThis.__quelleActivity; \
            return (a && typeof a.busy === 'function' && a.busy()) ? 1 : 0; \
         }})()",
        readiness = include_str!("readiness.js")
    )
}

/// Decides whether a URL matches Lightpanda's case-insensitive `*` wildcard pattern.
#[cfg(test)]
pub(crate) fn wildcard_url_match(pattern: &str, url: &str) -> bool {
    let pat = pattern.to_ascii_lowercase();
    let text = url.to_ascii_lowercase();
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return text == pat;
    }
    let mut pos = 0usize;
    for (idx, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if idx == 0 {
            if !text.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else if idx == parts.len() - 1 {
            return text[pos..].ends_with(part);
        } else if let Some(found) = text[pos..].find(part) {
            pos += found + part.len();
        } else {
            return false;
        }
    }
    true
}

/// Encodes a client-to-server RFC 6455 WebSocket frame with the given opcode,
/// FIN bit, payload, and 4-byte masking key.
pub(crate) fn encode_ws_frame(opcode: u8, fin: bool, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(14 + payload.len());
    out.push((if fin { 0x80 } else { 0x00 }) | (opcode & 0x0f));
    let len = payload.len();
    if len < 126 {
        out.push(0x80 | (len as u8));
    } else if len <= u16::MAX as usize {
        out.push(0x80 | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    for (i, &b) in payload.iter().enumerate() {
        out.push(b ^ mask[i & 3]);
    }
    out
}

/// Decoded single WebSocket frame.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WsFrame {
    pub fin: bool,
    pub opcode: u8,
    pub payload: Vec<u8>,
}

/// Decodes a single WebSocket frame from `buf`, returning `(frame, bytes_consumed)`.
pub(crate) fn decode_ws_frame(
    buf: &[u8],
    max_payload: usize,
) -> Result<Option<(WsFrame, usize)>, &'static str> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let fin = (buf[0] & 0x80) != 0;
    let opcode = buf[0] & 0x0f;
    let masked = (buf[1] & 0x80) != 0;
    let len_byte = buf[1] & 0x7f;
    let (payload_len, mut offset) = match len_byte {
        0..=125 => (usize::from(len_byte), 2usize),
        126 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            (usize::from(u16::from_be_bytes([buf[2], buf[3]])), 4usize)
        }
        127 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            let raw = u64::from_be_bytes([
                buf[2], buf[3], buf[4], buf[5], buf[6], buf[7], buf[8], buf[9],
            ]);
            let len = usize::try_from(raw).map_err(|_| "ws_payload_overflow")?;
            (len, 10usize)
        }
        _ => unreachable!(),
    };
    if payload_len > max_payload {
        return Err("ws_payload_too_large");
    }
    let mask = if masked {
        if buf.len() < offset + 4 {
            return Ok(None);
        }
        let m = [
            buf[offset],
            buf[offset + 1],
            buf[offset + 2],
            buf[offset + 3],
        ];
        offset += 4;
        Some(m)
    } else {
        None
    };
    let end = offset
        .checked_add(payload_len)
        .ok_or("ws_payload_overflow")?;
    if buf.len() < end {
        return Ok(None);
    }
    let mut payload = buf[offset..end].to_vec();
    if let Some(m) = mask {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= m[i & 3];
        }
    }
    Ok(Some((
        WsFrame {
            fin,
            opcode,
            payload,
        },
        end,
    )))
}

/// Adaptive wait decision for post-navigation readiness polling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitDecision {
    /// Page content is ready and stable; extract DOM now.
    Ready,
    /// Keep polling; either within base wait or actively loading within adaptive cap.
    Continue,
    /// Wait budget exhausted; dump current DOM immediately.
    Stop,
}

/// Tracks post-navigation readiness budget with adaptive extension when timers,
/// network activity, or DOM mutations are still in progress.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AdaptiveWait {
    pub base_wait: Duration,
    pub max_wait: Duration,
}

impl AdaptiveWait {
    pub(crate) fn new(base_wait_ms: u64, max_wait_ms: u64) -> Self {
        let base = Duration::from_millis(base_wait_ms);
        let max = Duration::from_millis(max_wait_ms.max(base_wait_ms));
        Self {
            base_wait: base,
            max_wait: max,
        }
    }

    pub(crate) fn decide(&self, elapsed: Duration, ready: bool, busy: bool) -> WaitDecision {
        if ready {
            return WaitDecision::Ready;
        }
        if elapsed >= self.max_wait {
            return WaitDecision::Stop;
        }
        if elapsed < self.base_wait || busy {
            WaitDecision::Continue
        } else {
            WaitDecision::Stop
        }
    }
}

static NONCE_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_mask() -> [u8; 4] {
    let seq = NONCE_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos() as u64);
    let mixed = seq
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .xor_mix(nanos.rotate_left(17));
    (mixed as u32).to_be_bytes()
}

trait Mix {
    fn xor_mix(self, other: Self) -> Self;
}
impl Mix for u64 {
    fn xor_mix(self, other: Self) -> Self {
        let mut x = self ^ other;
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
        x ^= x >> 33;
        x
    }
}

struct WsClient<S> {
    stream: S,
    rx: Vec<u8>,
    rx_fragment: Option<Vec<u8>>,
    next_id: u64,
    fetch_guard: bool,
}

impl<S: AsyncRead + AsyncWrite + Unpin> WsClient<S> {
    async fn handshake(
        mut stream: S,
        host_header: &str,
        path_and_query: &str,
    ) -> Result<Self, &'static str> {
        let mut key_bytes = [0u8; 16];
        for chunk in key_bytes.chunks_exact_mut(4) {
            chunk.copy_from_slice(&next_mask());
        }
        let key = BASE64.encode(key_bytes);
        let req = format!(
            "GET {path_and_query} HTTP/1.1\r\n\
             Host: {host_header}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        stream
            .write_all(req.as_bytes())
            .await
            .map_err(|_| "ws_handshake_write")?;

        let mut rx = Vec::with_capacity(1024);
        let mut tmp = [0u8; 1024];
        let header_end = loop {
            if let Some(pos) = rx.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            if rx.len() > 16 * 1024 {
                return Err("ws_handshake_too_large");
            }
            let n = stream
                .read(&mut tmp)
                .await
                .map_err(|_| "ws_handshake_read")?;
            if n == 0 {
                return Err("ws_handshake_eof");
            }
            rx.extend_from_slice(&tmp[..n]);
        };
        let head = std::str::from_utf8(&rx[..header_end]).map_err(|_| "ws_handshake_utf8")?;
        if !head.starts_with("HTTP/1.1 101") && !head.starts_with("HTTP/1.0 101") {
            return Err("ws_handshake_status");
        }
        rx.drain(..header_end);
        Ok(Self {
            stream,
            rx,
            rx_fragment: None,
            next_id: 0,
            fetch_guard: false,
        })
    }

    fn set_fetch_guard(&mut self, enabled: bool) {
        self.fetch_guard = enabled;
    }

    async fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), &'static str> {
        let frame = encode_ws_frame(opcode, true, payload, next_mask());
        self.stream
            .write_all(&frame)
            .await
            .map_err(|_| "ws_write")?;
        Ok(())
    }

    async fn send_command_no_wait(
        &mut self,
        method: &str,
        params: Option<Value>,
        session_id: Option<&str>,
    ) -> Result<u64, &'static str> {
        self.next_id += 1;
        let id = self.next_id;
        let mut req = json!({ "id": id, "method": method });
        if let Some(p) = params {
            req["params"] = p;
        }
        if let Some(sid) = session_id {
            req["sessionId"] = Value::String(sid.to_owned());
        }
        let payload = serde_json::to_string(&req).map_err(|_| "cdp_encode")?;
        self.send_frame(0x1, payload.as_bytes()).await?;
        Ok(id)
    }

    async fn handle_event(&mut self, msg: &Value, default_session: Option<&str>) {
        if !self.fetch_guard {
            return;
        }
        if msg.get("method").and_then(Value::as_str) != Some("Fetch.requestPaused") {
            return;
        }
        let Some(request_id) = msg.pointer("/params/requestId").and_then(Value::as_str) else {
            return;
        };
        let req_url = msg
            .pointer("/params/request/url")
            .and_then(Value::as_str)
            .unwrap_or("");
        let ev_session = msg
            .get("sessionId")
            .and_then(Value::as_str)
            .or(default_session);
        if cdp_subresource_is_public(req_url).await {
            let _ = self
                .send_command_no_wait(
                    "Fetch.continueRequest",
                    Some(json!({ "requestId": request_id })),
                    ev_session,
                )
                .await;
        } else {
            CDP_SUBRESOURCE_BLOCKS.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(stage = "cdp_ssrf_blocked", url = req_url);
            let _ = self
                .send_command_no_wait(
                    "Fetch.failRequest",
                    Some(json!({
                        "requestId": request_id,
                        "errorReason": "BlockedByClient",
                    })),
                    ev_session,
                )
                .await;
        }
    }

    /// Waits for `duration` while actively servicing `Fetch.requestPaused`
    /// interception events when `fetch_guard` is active so subresources never stall.
    async fn sleep_pumping(&mut self, duration: Duration, session_id: Option<&str>) {
        if !self.fetch_guard || duration.is_zero() {
            tokio::time::sleep(duration).await;
            return;
        }
        let until = Instant::now() + duration;
        while Instant::now() < until {
            match tokio::time::timeout_at(until, self.recv_text()).await {
                Ok(Ok(raw)) => {
                    if let Ok(msg) = serde_json::from_str::<Value>(&raw) {
                        self.handle_event(&msg, session_id).await;
                    }
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }
    }

    async fn recv_text(&mut self) -> Result<String, &'static str> {
        let mut tmp = [0u8; 65_536];
        loop {
            while let Some((frame, consumed)) = decode_ws_frame(&self.rx, MAX_CDP_MESSAGE)? {
                self.rx.drain(..consumed);
                match frame.opcode {
                    0x8 => return Err("ws_closed"),
                    0x9 => {
                        let _ = self.send_frame(0xA, &frame.payload).await;
                    }
                    0xA => {}
                    0x1 => {
                        if frame.fin {
                            return String::from_utf8(frame.payload).map_err(|_| "ws_utf8");
                        }
                        self.rx_fragment = Some(frame.payload);
                    }
                    0x0 => {
                        let buf = self
                            .rx_fragment
                            .as_mut()
                            .ok_or("ws_unexpected_continuation")?;
                        if buf.len() + frame.payload.len() > MAX_CDP_MESSAGE {
                            return Err("ws_payload_too_large");
                        }
                        buf.extend_from_slice(&frame.payload);
                        if frame.fin {
                            let payload = self.rx_fragment.take().unwrap_or_default();
                            return String::from_utf8(payload).map_err(|_| "ws_utf8");
                        }
                    }
                    _ => {}
                }
            }
            let n = self.stream.read(&mut tmp).await.map_err(|_| "ws_read")?;
            if n == 0 {
                return Err("ws_eof");
            }
            self.rx.extend_from_slice(&tmp[..n]);
        }
    }

    async fn call(
        &mut self,
        method: &str,
        params: Option<Value>,
        session_id: Option<&str>,
    ) -> Result<Value, &'static str> {
        let id = self
            .send_command_no_wait(method, params, session_id)
            .await?;
        loop {
            let raw = self.recv_text().await?;
            let msg: Value = serde_json::from_str(&raw).map_err(|_| "cdp_decode")?;
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                if msg.get("error").is_some() {
                    return Err("cdp_error");
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
            self.handle_event(&msg, session_id).await;
        }
    }
}

impl WsClient<TcpStream> {
    async fn connect_ws(ws_url: &Url) -> Result<Self, &'static str> {
        if ws_url.scheme() != "ws" {
            return Err("ws_scheme_unsupported");
        }
        let host = ws_url.host_str().ok_or("ws_missing_host")?;
        let port = ws_url.port().unwrap_or(80);
        let stream = TcpStream::connect((host, port))
            .await
            .map_err(|_| "ws_connect")?;
        let host_header = format!("{host}:{port}");
        let path_and_query = match ws_url.query() {
            Some(q) => format!("{}?{q}", ws_url.path()),
            None => ws_url.path().to_owned(),
        };
        Self::handshake(stream, &host_header, &path_and_query).await
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_cdp_session<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WsClient<S>,
    target_id: &str,
    url: &Url,
    threshold: usize,
    wait_for_ms: Option<u64>,
    wait_for_selector: Option<&str>,
    scroll_steps: u8,
    actions: &[PageAction],
    chromium_mode: bool,
    capture_screenshot: Option<bool>,
    deadline: Instant,
) -> Result<RenderOutput, PageError> {
    let session_id = tokio::time::timeout_at(
        deadline,
        ws.call(
            "Target.attachToTarget",
            Some(json!({ "targetId": target_id, "flatten": true })),
            None,
        ),
    )
    .await
    .map_err(|_| PageError::Timeout)?
    .map_err(|_| PageError::NeedsJs)?
    .get("sessionId")
    .and_then(Value::as_str)
    .ok_or(PageError::NeedsJs)?
    .to_owned();

    tokio::time::timeout_at(
        deadline,
        ws.call("Page.enable", Some(json!({})), Some(&session_id)),
    )
    .await
    .map_err(|_| PageError::Timeout)?
    .map_err(|_| PageError::NeedsJs)?;

    if chromium_mode {
        let _ = tokio::time::timeout_at(
            deadline,
            ws.call("Network.enable", Some(json!({})), Some(&session_id)),
        )
        .await;
        // Enable CDP Fetch request interception so every redirected navigation
        // and subresource URL is checked against Quelle's SSRF + DNS policy.
        let fetch_ok = tokio::time::timeout_at(
            deadline,
            ws.call(
                "Fetch.enable",
                Some(json!({ "patterns": [{ "urlPattern": "*" }] })),
                Some(&session_id),
            ),
        )
        .await
        .ok()
        .is_some_and(|r| r.is_ok());
        if fetch_ok {
            ws.set_fetch_guard(true);
        }
        if capture_screenshot.is_some() {
            let _ = tokio::time::timeout_at(
                deadline,
                ws.call(
                    "Emulation.setDeviceMetricsOverride",
                    Some(json!({
                        "width": 1280,
                        "height": 800,
                        "deviceScaleFactor": 1,
                        "mobile": false,
                    })),
                    Some(&session_id),
                ),
            )
            .await;
        } else {
            let _ = tokio::time::timeout_at(
                deadline,
                ws.call(
                    "Network.setBlockedURLs",
                    Some(json!({ "urls": BLOCKED_ASSET_PATTERNS })),
                    Some(&session_id),
                ),
            )
            .await;
        }
    }

    tokio::time::timeout_at(
        deadline,
        ws.call(
            "Page.addScriptToEvaluateOnNewDocument",
            Some(json!({ "source": include_str!("readiness-init.js") })),
            Some(&session_id),
        ),
    )
    .await
    .map_err(|_| PageError::Timeout)?
    .map_err(|_| PageError::NeedsJs)?;

    let nav_deadline = deadline.min(Instant::now() + NAVIGATE_TIMEOUT);
    let nav = tokio::time::timeout_at(
        nav_deadline,
        ws.call(
            "Page.navigate",
            Some(json!({ "url": url.as_str() })),
            Some(&session_id),
        ),
    )
    .await
    .map_err(|_| PageError::Timeout)?
    .map_err(|_| PageError::NeedsJs)?;
    if nav.get("errorText").and_then(Value::as_str).is_some() {
        return Err(PageError::NeedsJs);
    }

    // Start adaptive readiness timer AFTER navigation completes so slow
    // origin TTFB does not starve client-side hydration.
    let nav_done = Instant::now();
    let base_wait_ms = wait_for_ms.unwrap_or(7_000).clamp(500, 15_000);
    let max_wait_ms = wait_for_ms.map_or(13_000, |ms| ms.saturating_add(6_000).min(16_000));
    let policy = AdaptiveWait::new(base_wait_ms, max_wait_ms);

    let poll_expr = build_poll_expression(threshold, wait_for_selector);

    let poll_stop = deadline
        .checked_sub(Duration::from_millis(500))
        .unwrap_or(deadline);
    loop {
        if Instant::now() >= poll_stop {
            break;
        }
        let eval = tokio::time::timeout_at(
            poll_stop.min(Instant::now() + Duration::from_secs(2)),
            ws.call(
                "Runtime.evaluate",
                Some(json!({ "expression": poll_expr, "returnByValue": true })),
                Some(&session_id),
            ),
        )
        .await;
        let state = match eval {
            Ok(Ok(val)) => val
                .pointer("/result/value")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            _ => 0,
        };
        if state == 3 {
            return Err(PageError::Blocked);
        }
        let decision = policy.decide(nav_done.elapsed(), state == 2, state == 1);
        match decision {
            WaitDecision::Ready | WaitDecision::Stop => break,
            WaitDecision::Continue => {
                ws.sleep_pumping(POLL_INTERVAL, Some(&session_id)).await;
            }
        }
    }

    // Optional read-only viewport scrolling to hydrate lazy-loaded sections.
    let steps = scroll_steps.min(10);
    for step in 1..=steps {
        if Instant::now() >= poll_stop {
            break;
        }
        let scroll_expr = format!(
            "(() => {{ \
                const h = Math.max(document.body ? document.body.scrollHeight : 0, 1200); \
                window.scrollTo(0, Math.round((h * {step}) / {steps})); \
                return true; \
             }})()"
        );
        let _ = tokio::time::timeout_at(
            poll_stop.min(Instant::now() + Duration::from_secs(1)),
            ws.call(
                "Runtime.evaluate",
                Some(json!({ "expression": scroll_expr, "returnByValue": true })),
                Some(&session_id),
            ),
        )
        .await;
        ws.sleep_pumping(Duration::from_millis(220), Some(&session_id))
            .await;
    }

    // Optional safe scoped page actions (click, wait, waitForSelector, scroll, press).
    for action in actions {
        if Instant::now() >= poll_stop {
            break;
        }
        match action {
            PageAction::Wait { milliseconds } => {
                let dur = Duration::from_millis((*milliseconds).min(5_000));
                let rem = poll_stop.saturating_duration_since(Instant::now());
                ws.sleep_pumping(dur.min(rem), Some(&session_id)).await;
            }
            PageAction::WaitForSelector {
                selector,
                timeout_ms,
            } => {
                let wait_until = poll_stop.min(
                    Instant::now() + Duration::from_millis(timeout_ms.unwrap_or(4_000).min(10_000)),
                );
                if let Ok(encoded) = serde_json::to_string(selector.trim()) {
                    let expr = format!("Boolean(document.querySelector({encoded}))");
                    while Instant::now() < wait_until {
                        let found = tokio::time::timeout_at(
                            wait_until.min(Instant::now() + Duration::from_secs(1)),
                            ws.call(
                                "Runtime.evaluate",
                                Some(json!({ "expression": expr, "returnByValue": true })),
                                Some(&session_id),
                            ),
                        )
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .and_then(|v| v.pointer("/result/value").and_then(Value::as_bool))
                        .unwrap_or(false);
                        if found {
                            break;
                        }
                        ws.sleep_pumping(POLL_INTERVAL, Some(&session_id)).await;
                    }
                }
            }
            PageAction::Scroll {
                direction,
                pixels,
                steps: Some(action_steps),
            } => {
                let count = (*action_steps).clamp(1, 10);
                let step_action = PageAction::Scroll {
                    direction: *direction,
                    pixels: *pixels,
                    steps: None,
                };
                if let Some(expr) = build_action_expression(&step_action) {
                    for _ in 0..count {
                        if Instant::now() >= poll_stop {
                            break;
                        }
                        let _ = tokio::time::timeout_at(
                            poll_stop.min(Instant::now() + Duration::from_secs(1)),
                            ws.call(
                                "Runtime.evaluate",
                                Some(json!({ "expression": expr, "returnByValue": true })),
                                Some(&session_id),
                            ),
                        )
                        .await;
                        ws.sleep_pumping(Duration::from_millis(200), Some(&session_id))
                            .await;
                    }
                }
            }
            other => {
                if let Some(expr) = build_action_expression(other) {
                    let _ = tokio::time::timeout_at(
                        poll_stop.min(Instant::now() + Duration::from_secs(2)),
                        ws.call(
                            "Runtime.evaluate",
                            Some(json!({ "expression": expr, "returnByValue": true })),
                            Some(&session_id),
                        ),
                    )
                    .await;
                    // Brief settle after clicks/keypresses for synchronous or microtask DOM updates.
                    let rem = poll_stop.saturating_duration_since(Instant::now());
                    ws.sleep_pumping(Duration::from_millis(180).min(rem), Some(&session_id))
                        .await;
                }
            }
        }
    }

    let dump = tokio::time::timeout_at(
        deadline,
        ws.call(
            "Runtime.evaluate",
            Some(json!({
                "expression": "document.documentElement ? document.documentElement.outerHTML : ''",
                "returnByValue": true
            })),
            Some(&session_id),
        ),
    )
    .await
    .map_err(|_| PageError::Timeout)?
    .map_err(|_| PageError::NeedsJs)?;

    let html_str = dump
        .pointer("/result/value")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if html_str.is_empty() {
        return Err(PageError::NeedsJs);
    }
    let mut html = html_str.as_bytes().to_vec();
    let cut = html.len() > PAGE_BYTES;
    html.truncate(PAGE_BYTES);

    let screenshot = if let Some(full_page) = capture_screenshot {
        let shot = tokio::time::timeout_at(
            deadline,
            ws.call(
                "Page.captureScreenshot",
                Some(json!({
                    "format": "png",
                    "captureBeyondViewport": full_page,
                })),
                Some(&session_id),
            ),
        )
        .await
        .map_err(|_| PageError::Timeout)?
        .map_err(|_| PageError::NeedsJs)?;
        shot.get("data")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
            .map(|d| format!("data:image/png;base64,{d}"))
    } else {
        None
    };

    Ok(RenderOutput {
        html,
        truncated: cut,
        screenshot,
    })
}

async fn resolve_chromium_ws_url(endpoint: &Url, deadline: Instant) -> Result<Url, PageError> {
    if matches!(endpoint.scheme(), "ws" | "wss") {
        return Ok(endpoint.clone());
    }
    let mut version_url = endpoint.clone();
    let base_path = version_url.path().trim_end_matches('/');
    version_url.set_path(&format!("{base_path}/json/version"));
    let client = crate::client_builder()
        .build()
        .map_err(|_| PageError::Transport)?;
    let resp = tokio::time::timeout_at(
        deadline.min(Instant::now() + Duration::from_secs(3)),
        client.get(version_url).send(),
    )
    .await
    .map_err(|_| PageError::Timeout)?
    .map_err(|_| PageError::Transport)?;
    if !resp.status().is_success() {
        return Err(PageError::NeedsJs);
    }
    let body: Value = crate::read_json(resp, 64 * 1024)
        .await
        .map_err(|_| PageError::NeedsJs)?;
    let raw_ws = body
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or(PageError::NeedsJs)?;
    normalize_chromium_ws_url(endpoint, raw_ws).ok_or(PageError::NeedsJs)
}

/// Renders a page using an optional startup-configured Chromium CDP endpoint
/// (`QUELLE_CHROMIUM_CDP_URL`), supporting full Chromium fidelity, safe actions,
/// and viewport / full-page PNG screenshots. Validates public DNS first.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn render_chromium_cdp(
    endpoint: &Url,
    url: &Url,
    threshold: usize,
    wait_for_ms: Option<u64>,
    wait_for_selector: Option<&str>,
    scroll_steps: u8,
    actions: &[PageAction],
    capture_screenshot: Option<bool>,
    deadline: Instant,
) -> Result<RenderOutput, PageError> {
    let clean = public_url(url.clone())?;
    let _ = resolve_public_addrs(&clean).await?;
    let ws_url = resolve_chromium_ws_url(endpoint, deadline).await?;
    let mut ws = tokio::time::timeout_at(
        deadline.min(Instant::now() + Duration::from_secs(4)),
        WsClient::connect_ws(&ws_url),
    )
    .await
    .map_err(|_| PageError::Timeout)?
    .map_err(|_| PageError::NeedsJs)?;

    let ctx_id =
        tokio::time::timeout_at(deadline, ws.call("Target.createBrowserContext", None, None))
            .await
            .map_err(|_| PageError::Timeout)?
            .map_err(|_| PageError::NeedsJs)?
            .get("browserContextId")
            .and_then(Value::as_str)
            .ok_or(PageError::NeedsJs)?
            .to_owned();

    let target_res = tokio::time::timeout_at(
        deadline,
        ws.call(
            "Target.createTarget",
            Some(json!({ "url": "about:blank", "browserContextId": ctx_id })),
            None,
        ),
    )
    .await;
    let target_id = match target_res {
        Ok(Ok(val)) => match val.get("targetId").and_then(Value::as_str) {
            Some(id) => id.to_owned(),
            None => {
                let _ = tokio::time::timeout(
                    CLEANUP_TIMEOUT,
                    ws.call(
                        "Target.disposeBrowserContext",
                        Some(json!({ "browserContextId": ctx_id })),
                        None,
                    ),
                )
                .await;
                return Err(PageError::NeedsJs);
            }
        },
        Ok(Err(_)) => {
            let _ = tokio::time::timeout(
                CLEANUP_TIMEOUT,
                ws.call(
                    "Target.disposeBrowserContext",
                    Some(json!({ "browserContextId": ctx_id })),
                    None,
                ),
            )
            .await;
            return Err(PageError::NeedsJs);
        }
        Err(_) => return Err(PageError::Timeout),
    };

    let result = run_cdp_session(
        &mut ws,
        &target_id,
        &clean,
        threshold,
        wait_for_ms,
        wait_for_selector,
        scroll_steps,
        actions,
        true,
        capture_screenshot,
        deadline,
    )
    .await;

    ws.set_fetch_guard(false);
    let _ = tokio::time::timeout(CLEANUP_TIMEOUT, async {
        let _ = ws
            .call(
                "Target.closeTarget",
                Some(json!({ "targetId": target_id })),
                None,
            )
            .await;
        let _ = ws
            .call(
                "Target.disposeBrowserContext",
                Some(json!({ "browserContextId": ctx_id })),
                None,
            )
            .await;
    })
    .await;

    result
}

#[cfg(unix)]
mod unix_pool {
    use super::*;
    use tokio::net::UnixStream;

    impl WsClient<UnixStream> {
        async fn connect_unix(path: &Path) -> Result<Self, &'static str> {
            let stream = UnixStream::connect(path).await.map_err(|_| "ws_connect")?;
            Self::handshake(stream, "localhost", "/").await
        }
    }

    struct ServerHandle {
        child: Child,
        socket_dir: PathBuf,
        socket_path: PathBuf,
    }

    impl Drop for ServerHandle {
        fn drop(&mut self) {
            let _ = self.child.start_kill();
            let _ = std::fs::remove_dir_all(&self.socket_dir);
        }
    }

    /// Creates an unpredictable, owner-only (`0700`) directory for the CDP Unix
    /// socket so other local users on a shared host cannot traverse or pre-create
    /// the socket path.
    pub(super) fn create_private_socket_dir() -> Result<(PathBuf, PathBuf), PageError> {
        use std::os::unix::fs::DirBuilderExt;
        let base = std::env::temp_dir();
        let pid = std::process::id();
        for _ in 0..8 {
            let mut entropy = [0u8; 16];
            for chunk in entropy.chunks_exact_mut(4) {
                chunk.copy_from_slice(&next_mask());
            }
            let suffix: String = entropy.iter().map(|b| format!("{b:02x}")).collect();
            let dir = base.join(format!("quelle-cdp-{pid}-{suffix}"));
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            if builder.create(&dir).is_ok() {
                let sock = dir.join("cdp.sock");
                return Ok((dir, sock));
            }
        }
        Err(PageError::NeedsJs)
    }

    struct CdpPool {
        server: Mutex<Option<ServerHandle>>,
    }

    static POOL: OnceLock<CdpPool> = OnceLock::new();

    fn pool() -> &'static CdpPool {
        POOL.get_or_init(|| CdpPool {
            server: Mutex::new(None),
        })
    }

    pub(super) async fn shutdown() {
        if let Some(p) = POOL.get() {
            let mut guard = p.server.lock().await;
            *guard = None;
        }
    }

    async fn ensure_server(binary: &Path, blocked: &[Network]) -> Result<PathBuf, PageError> {
        let mut guard = pool().server.lock().await;
        if let Some(handle) = guard.as_mut() {
            let alive = handle.child.try_wait().ok().flatten().is_none();
            if alive && handle.socket_path.exists() {
                return Ok(handle.socket_path.clone());
            }
        }
        *guard = None;

        let (socket_dir, socket_path) = create_private_socket_dir()?;

        let mut command = Command::new(binary);
        command
            .env_clear()
            .env("LIGHTPANDA_DISABLE_TELEMETRY", "true")
            .args([
                "serve",
                "--unix-socket",
                socket_path.to_str().ok_or(PageError::NeedsJs)?,
                "--protocol",
                "cdp",
                "--cdp-max-connections",
                "16",
                "--cdp-max-message-size",
                &MAX_CDP_MESSAGE.to_string(),
                "--disable-metrics",
                "--http-timeout",
                "12000",
                "--http-max-response-size",
                "10485760",
                "--v8-max-heap-mb",
                "512",
                "--v8-gc-request-min-mb",
                "1",
                "--block-private-networks",
                "--locale",
                "de-CH",
                "--log-level",
                "fatal",
            ]);
        for range in BLOCKED_RANGES {
            command.args(["--block-cidrs", range]);
        }
        for network in blocked {
            command.args(["--block-cidrs", &network.to_string()]);
        }
        for pattern in BLOCKED_ASSET_PATTERNS {
            command.args(["--block-urls", pattern]);
        }
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| PageError::NeedsJs)?;

        let ready_deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(Some(_)) = child.try_wait() {
                let _ = std::fs::remove_dir_all(&socket_dir);
                return Err(PageError::NeedsJs);
            }
            if socket_path.exists() && UnixStream::connect(&socket_path).await.is_ok() {
                break;
            }
            if Instant::now() >= ready_deadline {
                let _ = child.start_kill();
                let _ = std::fs::remove_dir_all(&socket_dir);
                return Err(PageError::NeedsJs);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        tracing::info!(stage = "cdp_pool_started", socket = %socket_path.display());
        *guard = Some(ServerHandle {
            child,
            socket_dir,
            socket_path: socket_path.clone(),
        });
        Ok(socket_path)
    }

    async fn invalidate_server(failed_path: &Path) {
        let mut guard = pool().server.lock().await;
        if guard
            .as_ref()
            .is_some_and(|handle| handle.socket_path == failed_path)
        {
            *guard = None;
        }
    }

    async fn connect_client(binary: &Path) -> Result<WsClient<UnixStream>, PageError> {
        let blocked = blocked_networks();
        let path = ensure_server(binary, blocked).await?;
        match WsClient::connect_unix(&path).await {
            Ok(client) => Ok(client),
            Err(_) => {
                invalidate_server(&path).await;
                let fresh = ensure_server(binary, blocked).await?;
                WsClient::connect_unix(&fresh)
                    .await
                    .map_err(|_| PageError::NeedsJs)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn render(
        binary: &Path,
        url: &Url,
        threshold: usize,
        wait_for_ms: Option<u64>,
        wait_for_selector: Option<&str>,
        scroll_steps: u8,
        actions: &[PageAction],
        deadline: Instant,
    ) -> Result<(Vec<u8>, bool), PageError> {
        let mut ws = tokio::time::timeout_at(deadline, connect_client(binary))
            .await
            .map_err(|_| PageError::Timeout)??;

        let ctx_id =
            tokio::time::timeout_at(deadline, ws.call("Target.createBrowserContext", None, None))
                .await
                .map_err(|_| PageError::Timeout)?
                .map_err(|_| PageError::NeedsJs)?
                .get("browserContextId")
                .and_then(Value::as_str)
                .ok_or(PageError::NeedsJs)?
                .to_owned();

        let target_res = tokio::time::timeout_at(
            deadline,
            ws.call(
                "Target.createTarget",
                Some(json!({ "url": "about:blank", "browserContextId": ctx_id })),
                None,
            ),
        )
        .await;
        let target_id = match target_res {
            Ok(Ok(val)) => match val.get("targetId").and_then(Value::as_str) {
                Some(id) => id.to_owned(),
                None => {
                    let _ = tokio::time::timeout(
                        CLEANUP_TIMEOUT,
                        ws.call(
                            "Target.disposeBrowserContext",
                            Some(json!({ "browserContextId": ctx_id })),
                            None,
                        ),
                    )
                    .await;
                    return Err(PageError::NeedsJs);
                }
            },
            Ok(Err(_)) => {
                let _ = tokio::time::timeout(
                    CLEANUP_TIMEOUT,
                    ws.call(
                        "Target.disposeBrowserContext",
                        Some(json!({ "browserContextId": ctx_id })),
                        None,
                    ),
                )
                .await;
                return Err(PageError::NeedsJs);
            }
            Err(_) => return Err(PageError::Timeout),
        };

        let result = run_cdp_session(
            &mut ws,
            &target_id,
            url,
            threshold,
            wait_for_ms,
            wait_for_selector,
            scroll_steps,
            actions,
            false,
            None,
            deadline,
        )
        .await;

        let _ = tokio::time::timeout(CLEANUP_TIMEOUT, async {
            let _ = ws
                .call(
                    "Target.closeTarget",
                    Some(json!({ "targetId": target_id })),
                    None,
                )
                .await;
            let _ = ws
                .call(
                    "Target.disposeBrowserContext",
                    Some(json!({ "browserContextId": ctx_id })),
                    None,
                )
                .await;
        })
        .await;

        result.map(|out| (out.html, out.truncated))
    }
}

/// Renders a page through the persistent Lightpanda CDP server pool when enabled.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn render_pooled(
    binary: &Path,
    url: &Url,
    threshold: usize,
    wait_for_ms: Option<u64>,
    wait_for_selector: Option<&str>,
    scroll_steps: u8,
    actions: &[PageAction],
    deadline: Instant,
) -> Result<(Vec<u8>, bool), PageError> {
    #[cfg(unix)]
    {
        unix_pool::render(
            binary,
            url,
            threshold,
            wait_for_ms,
            wait_for_selector,
            scroll_steps,
            actions,
            deadline,
        )
        .await
    }
    #[cfg(not(unix))]
    {
        let _ = (
            binary,
            url,
            threshold,
            wait_for_ms,
            wait_for_selector,
            scroll_steps,
            actions,
            deadline,
        );
        Err(PageError::NeedsJs)
    }
}

/// Shuts down the persistent Lightpanda CDP daemon and cleans up its socket file.
pub async fn shutdown_pool() {
    #[cfg(unix)]
    unix_pool::shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_frame_roundtrip_small_medium_and_large_payloads() {
        for len in [0usize, 17, 125, 126, 4096, 70_000] {
            let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let mask = [0x37, 0xfa, 0x21, 0x3d];
            let encoded = encode_ws_frame(0x1, true, &payload, mask);
            let (frame, consumed) = decode_ws_frame(&encoded, 16 * 1024 * 1024)
                .expect("decode valid frame")
                .expect("complete frame");
            assert_eq!(consumed, encoded.len());
            assert!(frame.fin);
            assert_eq!(frame.opcode, 0x1);
            assert_eq!(frame.payload, payload);
        }
    }

    #[test]
    fn ws_frame_decoder_handles_partial_buffers_and_rejects_oversized() {
        let payload = b"{\"id\":1,\"result\":{}}";
        let encoded = encode_ws_frame(0x1, true, payload, [1, 2, 3, 4]);
        // Truncated buffer should return Ok(None) (need more bytes).
        assert_eq!(
            decode_ws_frame(&encoded[..encoded.len() - 1], 1024).unwrap(),
            None
        );
        // Oversized payload must be rejected before allocating.
        assert!(decode_ws_frame(&encoded, 4).is_err());
    }

    #[test]
    fn ws_frame_decoder_preserves_fragmentation_and_control_opcodes() {
        let part1 = encode_ws_frame(0x1, false, b"hello ", [9, 8, 7, 6]);
        let part2 = encode_ws_frame(0x0, true, b"world", [5, 4, 3, 2]);
        let (f1, n1) = decode_ws_frame(&part1, 1024).unwrap().unwrap();
        let (f2, n2) = decode_ws_frame(&part2, 1024).unwrap().unwrap();
        assert_eq!(n1, part1.len());
        assert_eq!(n2, part2.len());
        assert!(!f1.fin);
        assert_eq!(f1.opcode, 0x1);
        assert!(f2.fin);
        assert_eq!(f2.opcode, 0x0);
        assert_eq!([f1.payload, f2.payload].concat(), b"hello world");
    }

    #[test]
    fn blocked_asset_patterns_block_media_and_trackers_but_allow_content_and_scripts() {
        let is_blocked = |url: &str| {
            BLOCKED_ASSET_PATTERNS
                .iter()
                .any(|pat| wildcard_url_match(pat, url))
        };
        assert!(is_blocked("https://cdn.example.com/hero.PNG?w=1200"));
        assert!(is_blocked("https://cdn.example.com/photo.jpg"));
        assert!(is_blocked("https://fonts.example.com/inter.woff2"));
        assert!(is_blocked("https://video.example.com/intro.mp4"));
        assert!(is_blocked(
            "https://www.googletagmanager.com/gtm.js?id=GTM-123"
        ));
        assert!(is_blocked("https://stats.g.doubleclick.net/r/collect"));

        assert!(!is_blocked("https://example.com/article"));
        assert!(!is_blocked("https://cdn.example.com/app.bundle.js"));
        assert!(!is_blocked("https://api.example.com/v1/rates.json"));
        assert!(!is_blocked("https://cdn.example.com/styles.css"));
    }

    #[test]
    fn adaptive_wait_finishes_early_stops_when_idle_and_extends_while_busy() {
        let wait = AdaptiveWait::new(7_000, 13_000);
        // Ready always completes immediately.
        assert_eq!(
            wait.decide(Duration::from_millis(120), true, true),
            WaitDecision::Ready
        );
        // Before base wait, keep waiting even if idle (script might just be starting).
        assert_eq!(
            wait.decide(Duration::from_millis(2_000), false, false),
            WaitDecision::Continue
        );
        // Past base wait and idle -> stop immediately, do not waste time.
        assert_eq!(
            wait.decide(Duration::from_millis(7_100), false, false),
            WaitDecision::Stop
        );
        // Past base wait but still busy (e.g. 10s setTimeout on js-delayed) -> extend!
        assert_eq!(
            wait.decide(Duration::from_millis(9_500), false, true),
            WaitDecision::Continue
        );
        // Ready during adaptive extension -> succeed!
        assert_eq!(
            wait.decide(Duration::from_millis(10_400), true, false),
            WaitDecision::Ready
        );
        // Hard cap always stops even if still busy.
        assert_eq!(
            wait.decide(Duration::from_millis(13_000), false, true),
            WaitDecision::Stop
        );
    }

    #[cfg(unix)]
    #[test]
    fn cdp_socket_dir_is_private_0700_and_unpredictable() {
        use std::os::unix::fs::PermissionsExt;
        let (dir1, sock1) = unix_pool::create_private_socket_dir().expect("dir1");
        let (dir2, sock2) = unix_pool::create_private_socket_dir().expect("dir2");
        assert_ne!(dir1, dir2);
        assert_eq!(sock1.parent(), Some(dir1.as_path()));
        assert_eq!(sock2.parent(), Some(dir2.as_path()));
        let mode1 = std::fs::metadata(&dir1).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode1, 0o700, "socket directory must be mode 0700");
        let _ = std::fs::remove_dir_all(&dir1);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn wait_selector_validates_css_and_escapes_safely_in_poll_expression() {
        assert!(valid_wait_selector("main .quote"));
        assert!(valid_wait_selector("table[data-loaded='true']"));
        assert!(!valid_wait_selector(""));
        assert!(!valid_wait_selector("div >>> invalid((("));
        assert!(!valid_wait_selector("main\nalert(1)"));

        let expr = build_poll_expression(300, Some("div[data-id=\"a'b\"]"));
        assert!(expr.contains("document.querySelector("));
        assert!(expr.contains(r#"\"a'b\""#));
        // Must detect bot/Cloudflare challenge pages early (return state 3) instead of
        // burning the 13s adaptive wait budget on an infinite challenge timer loop.
        assert!(expr.contains("just a moment..."));
        assert!(expr.contains("#cf-challenge-running"));
    }

    #[test]
    fn page_actions_validate_safely_and_escape_selectors() {
        let valid = vec![
            PageAction::Click {
                selector: "button[data-tab=\"pricing\"]".to_owned(),
            },
            PageAction::Wait { milliseconds: 500 },
            PageAction::WaitForSelector {
                selector: ".pricing-table".to_owned(),
                timeout_ms: Some(3000),
            },
            PageAction::Scroll {
                direction: ScrollDirection::Down,
                pixels: Some(800),
                steps: None,
            },
            PageAction::Press {
                key: "Escape".to_owned(),
            },
        ];
        assert!(valid_actions(&valid));

        // Disallowed key or invalid CSS selector must fail.
        assert!(!valid_actions(&[PageAction::Press {
            key: "F12".to_owned()
        }]));
        assert!(!valid_actions(&[PageAction::Click {
            selector: "div >>> broken(((".to_owned()
        }]));
        // Excessive single wait or cumulative wait > 10s must fail.
        assert!(!valid_actions(&[PageAction::Wait { milliseconds: 9000 }]));
        assert!(!valid_actions(&[
            PageAction::Wait { milliseconds: 4000 },
            PageAction::Wait { milliseconds: 4000 },
            PageAction::Wait { milliseconds: 4000 },
        ]));
        // More than 16 actions must fail.
        let too_many = vec![PageAction::Wait { milliseconds: 100 }; 17];
        assert!(!valid_actions(&too_many));

        // Click expression must JSON-escape selector quotes.
        let click_js = build_action_expression(&valid[0]).expect("click js");
        assert!(click_js.contains("document.querySelector("));
        assert!(click_js.contains(r#"\"pricing\""#));
        assert!(click_js.contains(".click()"));
    }

    #[test]
    fn chromium_ws_url_normalizes_host_and_port_from_configured_endpoint() {
        let endpoint = Url::parse("http://127.0.0.1:9222").unwrap();
        let raw = "ws://localhost/devtools/browser/abc-123";
        let normalized = normalize_chromium_ws_url(&endpoint, raw).expect("normalized ws");
        assert_eq!(normalized.scheme(), "ws");
        assert_eq!(normalized.host_str(), Some("127.0.0.1"));
        assert_eq!(normalized.port(), Some(9222));
        assert_eq!(normalized.path(), "/devtools/browser/abc-123");

        // Rejects non-ws schemes.
        assert!(normalize_chromium_ws_url(&endpoint, "http://127.0.0.1:9222/foo").is_none());
    }

    #[test]
    fn cdp_subresource_guard_blocks_private_loopback_metadata_and_local_schemes() {
        // Allowed non-network / public URLs
        assert!(cdp_subresource_allowed("about:blank"));
        assert!(cdp_subresource_allowed("about:srcdoc"));
        assert!(cdp_subresource_allowed(
            "data:image/png;base64,iVBORw0KGgo="
        ));
        assert!(cdp_subresource_allowed("blob:https://example.org/uuid-1"));
        assert!(cdp_subresource_allowed("https://example.org/app.js"));
        assert!(cdp_subresource_allowed("http://93.184.216.34/style.css"));

        // Blocked private, loopback, metadata, credentials, and non-http schemes
        assert!(!cdp_subresource_allowed("http://127.0.0.1/admin"));
        assert!(!cdp_subresource_allowed("http://localhost:8080/api"));
        assert!(!cdp_subresource_allowed(
            "http://169.254.169.254/latest/meta-data/"
        ));
        assert!(!cdp_subresource_allowed("http://10.0.0.1/internal"));
        assert!(!cdp_subresource_allowed("http://[::1]:3000/secret"));
        assert!(!cdp_subresource_allowed("https://user:pass@example.org/"));
        assert!(!cdp_subresource_allowed("file:///etc/passwd"));
        assert!(!cdp_subresource_allowed("chrome://settings"));
        assert!(!cdp_subresource_allowed("ftp://example.org/file"));
        assert!(!cdp_subresource_allowed(""));
    }

    #[tokio::test]
    async fn ws_client_fetch_guard_fails_private_subresource_and_continues_safe_data_uri() {
        let (client_stream, mut server_stream) = tokio::io::duplex(64 * 1024);

        let server = tokio::spawn(async move {
            // Read client WebSocket handshake
            let mut req_buf = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                let n = server_stream.read(&mut tmp).await.unwrap();
                assert!(n > 0);
                req_buf.extend_from_slice(&tmp[..n]);
                if req_buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            server_stream
                .write_all(
                    b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
                )
                .await
                .unwrap();

            // Helper to read one masked client frame
            async fn read_client_json(
                srv: &mut tokio::io::DuplexStream,
                rx: &mut Vec<u8>,
            ) -> Value {
                let mut buf = [0u8; 4096];
                loop {
                    if let Some((frame, consumed)) = decode_ws_frame(rx, 65536).unwrap() {
                        rx.drain(..consumed);
                        return serde_json::from_slice::<Value>(&frame.payload).unwrap();
                    }
                    let n = srv.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    rx.extend_from_slice(&buf[..n]);
                }
            }
            // Helper to send unmasked server text frame
            async fn send_server_json(srv: &mut tokio::io::DuplexStream, val: Value) {
                let payload = serde_json::to_vec(&val).unwrap();
                let mut frame = Vec::new();
                frame.push(0x81);
                if payload.len() < 126 {
                    frame.push(payload.len() as u8);
                } else {
                    frame.push(126);
                    frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
                }
                frame.extend_from_slice(&payload);
                srv.write_all(&frame).await.unwrap();
            }

            let mut srv_rx = Vec::new();

            // 1. Expect Page.navigate
            let nav = read_client_json(&mut server_stream, &mut srv_rx).await;
            assert_eq!(nav["method"], "Page.navigate");
            let nav_id = nav["id"].as_u64().unwrap();

            // 2. Emit two Fetch.requestPaused events before answering Page.navigate:
            //    one SSRF attempt to 169.254.169.254 and one safe data: URI.
            send_server_json(
                &mut server_stream,
                json!({
                    "method": "Fetch.requestPaused",
                    "sessionId": "sess-1",
                    "params": {
                        "requestId": "req-ssrf",
                        "request": { "url": "http://169.254.169.254/latest/meta-data/" }
                    }
                }),
            )
            .await;
            send_server_json(
                &mut server_stream,
                json!({
                    "method": "Fetch.requestPaused",
                    "sessionId": "sess-1",
                    "params": {
                        "requestId": "req-data",
                        "request": { "url": "data:text/plain,hello" }
                    }
                }),
            )
            .await;

            // 3. Verify client immediately fails the SSRF request and continues the safe one
            let cmd1 = read_client_json(&mut server_stream, &mut srv_rx).await;
            assert_eq!(cmd1["method"], "Fetch.failRequest");
            assert_eq!(cmd1["params"]["requestId"], "req-ssrf");
            assert_eq!(cmd1["params"]["errorReason"], "BlockedByClient");
            assert_eq!(cmd1["sessionId"], "sess-1");

            let cmd2 = read_client_json(&mut server_stream, &mut srv_rx).await;
            assert_eq!(cmd2["method"], "Fetch.continueRequest");
            assert_eq!(cmd2["params"]["requestId"], "req-data");
            assert_eq!(cmd2["sessionId"], "sess-1");

            // 4. Complete Page.navigate
            send_server_json(
                &mut server_stream,
                json!({ "id": nav_id, "result": { "frameId": "f1" } }),
            )
            .await;
        });

        let mut ws = WsClient::handshake(client_stream, "localhost", "/")
            .await
            .unwrap();
        ws.set_fetch_guard(true);
        let res = ws
            .call(
                "Page.navigate",
                Some(json!({ "url": "https://example.org" })),
                Some("sess-1"),
            )
            .await
            .unwrap();
        assert_eq!(res["frameId"], "f1");
        server.await.unwrap();
    }
}
