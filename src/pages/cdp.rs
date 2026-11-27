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
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::Mutex,
    time::Instant,
};
use url::Url;

use super::{BLOCKED_RANGES, Network, PAGE_BYTES, PageError, blocked_networks};

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

pub fn set_cdp_pool_enabled(enabled: bool) {
    CDP_ENABLED.store(enabled, Ordering::Relaxed);
}

pub(crate) fn cdp_pool_enabled() -> bool {
    cfg!(unix) && CDP_ENABLED.load(Ordering::Relaxed)
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

#[cfg(unix)]
mod unix_pool {
    use super::*;
    use tokio::net::UnixStream;

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

    struct WsClient {
        stream: UnixStream,
        rx: Vec<u8>,
        next_id: u64,
    }

    impl WsClient {
        async fn connect(path: &Path) -> Result<Self, &'static str> {
            let mut stream = UnixStream::connect(path).await.map_err(|_| "ws_connect")?;
            let mut key_bytes = [0u8; 16];
            for chunk in key_bytes.chunks_exact_mut(4) {
                chunk.copy_from_slice(&next_mask());
            }
            let key = BASE64.encode(key_bytes);
            let req = format!(
                "GET / HTTP/1.1\r\n\
                 Host: localhost\r\n\
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
                next_id: 0,
            })
        }

        async fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), &'static str> {
            let frame = encode_ws_frame(opcode, true, payload, next_mask());
            self.stream
                .write_all(&frame)
                .await
                .map_err(|_| "ws_write")?;
            Ok(())
        }

        async fn recv_text(&mut self) -> Result<String, &'static str> {
            let mut assembled: Option<Vec<u8>> = None;
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
                            assembled = Some(frame.payload);
                        }
                        0x0 => {
                            let buf = assembled.as_mut().ok_or("ws_unexpected_continuation")?;
                            if buf.len() + frame.payload.len() > MAX_CDP_MESSAGE {
                                return Err("ws_payload_too_large");
                            }
                            buf.extend_from_slice(&frame.payload);
                            if frame.fin {
                                let payload = assembled.take().unwrap_or_default();
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
            loop {
                let raw = self.recv_text().await?;
                let msg: Value = serde_json::from_str(&raw).map_err(|_| "cdp_decode")?;
                if msg.get("id").and_then(Value::as_u64) == Some(id) {
                    if msg.get("error").is_some() {
                        return Err("cdp_error");
                    }
                    return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
                }
            }
        }
    }

    struct ServerHandle {
        child: Child,
        socket_path: PathBuf,
    }

    impl Drop for ServerHandle {
        fn drop(&mut self) {
            let _ = self.child.start_kill();
            let _ = std::fs::remove_file(&self.socket_path);
        }
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

        let seq = NONCE_SEQ.fetch_add(1, Ordering::Relaxed);
        let socket_path =
            std::env::temp_dir().join(format!("quelle-lp-{}-{seq}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket_path);

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
                let _ = std::fs::remove_file(&socket_path);
                return Err(PageError::NeedsJs);
            }
            if socket_path.exists() && UnixStream::connect(&socket_path).await.is_ok() {
                break;
            }
            if Instant::now() >= ready_deadline {
                let _ = child.start_kill();
                let _ = std::fs::remove_file(&socket_path);
                return Err(PageError::NeedsJs);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        tracing::info!(stage = "cdp_pool_started", socket = %socket_path.display());
        *guard = Some(ServerHandle {
            child,
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

    async fn connect_client(binary: &Path) -> Result<WsClient, PageError> {
        let blocked = blocked_networks();
        let path = ensure_server(binary, blocked).await?;
        match WsClient::connect(&path).await {
            Ok(client) => Ok(client),
            Err(_) => {
                invalidate_server(&path).await;
                let fresh = ensure_server(binary, blocked).await?;
                WsClient::connect(&fresh)
                    .await
                    .map_err(|_| PageError::NeedsJs)
            }
        }
    }

    pub(super) async fn render(
        binary: &Path,
        url: &Url,
        threshold: usize,
        wait_for_ms: Option<u64>,
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

        let result = run_session(&mut ws, &target_id, url, threshold, wait_for_ms, deadline).await;

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

    async fn run_session(
        ws: &mut WsClient,
        target_id: &str,
        url: &Url,
        threshold: usize,
        wait_for_ms: Option<u64>,
        deadline: Instant,
    ) -> Result<(Vec<u8>, bool), PageError> {
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

        let poll_expr = format!(
            "(() => {{ \
                const ready = ({readiness})({threshold}); \
                if (ready) return 2; \
                const a = globalThis.__quelleActivity; \
                return (a && typeof a.busy === 'function' && a.busy()) ? 1 : 0; \
             }})()",
            readiness = include_str!("readiness.js")
        );

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
            let decision = policy.decide(nav_done.elapsed(), state == 2, state == 1);
            match decision {
                WaitDecision::Ready | WaitDecision::Stop => break,
                WaitDecision::Continue => {
                    tokio::time::sleep(POLL_INTERVAL).await;
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
        Ok((html, cut))
    }
}

/// Renders a page through the persistent Lightpanda CDP server pool when enabled.
pub(crate) async fn render_pooled(
    binary: &Path,
    url: &Url,
    threshold: usize,
    wait_for_ms: Option<u64>,
    deadline: Instant,
) -> Result<(Vec<u8>, bool), PageError> {
    #[cfg(unix)]
    {
        unix_pool::render(binary, url, threshold, wait_for_ms, deadline).await
    }
    #[cfg(not(unix))]
    {
        let _ = (binary, url, threshold, wait_for_ms, deadline);
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
}
