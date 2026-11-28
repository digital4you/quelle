//! Built-in Model Context Protocol (MCP) stdio server (`quelle mcp`).
//!
//! Speaks newline-delimited JSON-RPC 2.0 on stdin/stdout (protocol version
//! `2024-11-05`) and forwards tool calls to a running Quelle HTTP service
//! configured via `QUELLE_URL` (default `http://127.0.0.1:3000`) and
//! `QUELLE_TOKEN` / `QUELLE_API_KEY`.

use std::{
    env,
    io::{BufRead, Write},
    process::ExitCode,
    time::Duration,
};

use reqwest::{
    Client,
    header::{AUTHORIZATION, HeaderValue},
};
use serde_json::{Value, json};
use url::Url;

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

pub struct McpBridge {
    client: Client,
    base_url: Url,
    authorization: HeaderValue,
}

impl McpBridge {
    pub fn from_env() -> Result<Self, &'static str> {
        let raw_url = env::var("QUELLE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "http://127.0.0.1:3000".to_owned());
        let base_url = crate::sidecar_endpoint(raw_url.trim(), "QUELLE_URL")?;
        let token = env::var("QUELLE_API_KEY")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                env::var("QUELLE_TOKEN")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
            })
            .ok_or("QUELLE_TOKEN or QUELLE_API_KEY is required for `quelle mcp`")?;
        let authorization = HeaderValue::try_from(format!("Bearer {}", token.trim()))
            .map_err(|_| "QUELLE_TOKEN")?;
        let client = crate::client_builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|_| "HTTP client")?;
        Ok(Self {
            client,
            base_url,
            authorization,
        })
    }

    async fn post_json(&self, path: &str, payload: &Value) -> Result<(u16, String), String> {
        let mut target = self.base_url.clone();
        let base = target.path().trim_end_matches('/');
        target.set_path(&format!("{base}{path}"));
        let resp = self
            .client
            .post(target)
            .header(AUTHORIZATION, self.authorization.clone())
            .json(payload)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp
            .text()
            .await
            .map_err(|e| format!("failed reading response body: {e}"))?;
        Ok((status, body))
    }
}

pub fn tool_definitions() -> Value {
    json!([
        {
            "name": "quelle_search",
            "description": "Search the web with Quelle's parallel RRF meta-search and retrieve clean, citation-ready page content.",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string", "description": "Search query" },
                    "language": { "type": "string", "enum": ["de", "fr", "it", "en"] },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 10 },
                    "maxTokens": { "type": "integer", "minimum": 32, "maximum": 200000 },
                    "totalMaxTokens": { "type": "integer", "minimum": 64, "maximum": 200000 },
                    "focusQuery": { "type": "string" },
                    "chunks": { "type": "boolean" },
                    "structured": { "type": "boolean" }
                }
            }
        },
        {
            "name": "quelle_scrape",
            "description": "Scrape one or more public URLs into clean Markdown, text, HTML, links, structured tables/JSON-LD, chunks, or screenshots.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Single public URL to scrape" },
                    "urls": { "type": "array", "items": { "type": "string" }, "description": "Batch of public URLs" },
                    "formats": {
                        "type": "array",
                        "items": {
                            "type": "string",
                            "enum": ["markdown", "text", "html", "links", "chunks", "structured", "screenshot", "fullPageScreenshot"]
                        }
                    },
                    "onlyMainContent": { "type": "boolean" },
                    "render": { "type": "string", "enum": ["auto", "always", "chromium", "never"] },
                    "waitForSelector": { "type": "string" },
                    "scrollSteps": { "type": "integer", "minimum": 0, "maximum": 10 },
                    "actions": { "type": "array", "description": "Safe scoped actions (click, wait, waitForSelector, scroll, press)" },
                    "maxTokens": { "type": "integer", "minimum": 32, "maximum": 200000 },
                    "focusQuery": { "type": "string" },
                    "maxPages": { "type": "integer", "minimum": 1, "maximum": 1000 }
                }
            }
        },
        {
            "name": "quelle_extract",
            "description": "Scrape URL(s) or a search query and extract grounded structured JSON matching a prompt and/or JSON Schema.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "urls": { "type": "array", "items": { "type": "string" } },
                    "query": { "type": "string" },
                    "prompt": { "type": "string" },
                    "schema": { "type": "object" },
                    "render": { "type": "string", "enum": ["auto", "always", "chromium", "never"] },
                    "waitForSelector": { "type": "string" },
                    "scrollSteps": { "type": "integer", "minimum": 0, "maximum": 10 },
                    "actions": { "type": "array" }
                }
            }
        },
        {
            "name": "quelle_map",
            "description": "Fast URL discovery for a site via robots.txt, sitemap.xml, and page links.",
            "inputSchema": {
                "type": "object",
                "required": ["url"],
                "properties": {
                    "url": { "type": "string" },
                    "search": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 5000 },
                    "includeSubdomains": { "type": "boolean" },
                    "useSitemap": { "type": "boolean" }
                }
            }
        },
        {
            "name": "quelle_crawl",
            "description": "Recursively crawl a public website with optional incremental change detection (defaults to sync mode for interactive tool calls).",
            "inputSchema": {
                "type": "object",
                "required": ["url"],
                "properties": {
                    "url": { "type": "string" },
                    "maxDepth": { "type": "integer", "minimum": 0, "maximum": 10 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000 },
                    "sync": { "type": "boolean", "default": true },
                    "incremental": { "type": "boolean" },
                    "sinceCrawlId": { "type": "string" },
                    "omitUnchangedContent": { "type": "boolean" },
                    "includePaths": { "type": "array", "items": { "type": "string" } },
                    "excludePaths": { "type": "array", "items": { "type": "string" } }
                }
            }
        }
    ])
}

/// Maps an MCP tool invocation `(name, arguments)` onto a Quelle HTTP endpoint and payload.
pub fn map_tool_to_request(name: &str, arguments: Value) -> Result<(&'static str, Value), String> {
    let mut obj = match arguments {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        _ => return Err("tool arguments must be a JSON object".to_owned()),
    };
    match name {
        "quelle_search" => {
            if obj
                .get("query")
                .and_then(Value::as_str)
                .is_none_or(|q| q.trim().is_empty())
            {
                return Err("quelle_search requires a non-empty 'query' string".to_owned());
            }
            obj.remove("stream");
            Ok(("/api/content", Value::Object(obj)))
        }
        "quelle_scrape" => {
            if !obj.contains_key("url") && !obj.contains_key("urls") && !obj.contains_key("query") {
                return Err("quelle_scrape requires 'url', 'urls', or 'query'".to_owned());
            }
            obj.remove("stream");
            Ok(("/api/scrape", Value::Object(obj)))
        }
        "quelle_extract" => {
            if !obj.contains_key("url") && !obj.contains_key("urls") && !obj.contains_key("query") {
                return Err("quelle_extract requires 'url', 'urls', or 'query'".to_owned());
            }
            Ok(("/api/extract", Value::Object(obj)))
        }
        "quelle_map" => {
            if !obj.contains_key("url") && !obj.contains_key("urls") {
                return Err("quelle_map requires 'url' or 'urls'".to_owned());
            }
            Ok(("/api/map", Value::Object(obj)))
        }
        "quelle_crawl" => {
            if !obj.contains_key("url") && !obj.contains_key("urls") {
                return Err("quelle_crawl requires 'url' or 'urls'".to_owned());
            }
            obj.entry("sync").or_insert(Value::Bool(true));
            obj.entry("limit").or_insert_with(|| json!(20));
            Ok(("/api/crawl", Value::Object(obj)))
        }
        other => Err(format!("unknown tool '{other}'")),
    }
}

fn rpc_ok(id: Value, result: Value) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
    .to_string()
}

fn rpc_err(id: Value, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    })
    .to_string()
}

/// Processes a single JSON-RPC 2.0 line for MCP. Returns `None` for notifications.
pub async fn handle_rpc_line(bridge: Option<&McpBridge>, line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let msg: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return Some(rpc_err(Value::Null, -32700, "parse error")),
    };
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        return msg
            .get("id")
            .cloned()
            .map(|id| rpc_err(id, -32600, "invalid request"));
    };
    let Some(id) = msg.get("id").cloned() else {
        // JSON-RPC notification (e.g. `notifications/initialized`): no reply.
        return None;
    };

    match method {
        "initialize" => Some(rpc_ok(
            id,
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "quelle",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }),
        )),
        "ping" => Some(rpc_ok(id, json!({}))),
        "tools/list" => Some(rpc_ok(id, json!({ "tools": tool_definitions() }))),
        "tools/call" => {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(rpc_err(id, -32602, "missing tool name"));
            };
            let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
            let (path, payload) = match map_tool_to_request(name, arguments) {
                Ok(mapped) => mapped,
                Err(err) => {
                    return Some(rpc_ok(
                        id,
                        json!({
                            "content": [{ "type": "text", "text": err }],
                            "isError": true,
                        }),
                    ));
                }
            };
            let Some(bridge) = bridge else {
                return Some(rpc_ok(
                    id,
                    json!({
                        "content": [{ "type": "text", "text": "MCP HTTP bridge not initialized" }],
                        "isError": true,
                    }),
                ));
            };
            match bridge.post_json(path, &payload).await {
                Ok((status, body)) => Some(rpc_ok(
                    id,
                    json!({
                        "content": [{ "type": "text", "text": body }],
                        "isError": !(200..300).contains(&status),
                    }),
                )),
                Err(err) => Some(rpc_ok(
                    id,
                    json!({
                        "content": [{ "type": "text", "text": err }],
                        "isError": true,
                    }),
                )),
            }
        }
        _ => Some(rpc_err(id, -32601, "method not found")),
    }
}

pub async fn run_stdio() -> ExitCode {
    let bridge = match McpBridge::from_env() {
        Ok(b) => b,
        Err(msg) => {
            eprintln!("quelle mcp configuration error: {msg}");
            return ExitCode::FAILURE;
        }
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(32);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines().map_while(Result::ok) {
            if tx.blocking_send(line).is_err() {
                break;
            }
        }
    });
    let mut stdout = std::io::stdout();
    while let Some(line) = rx.recv().await {
        if let Some(reply) = handle_rpc_line(Some(&bridge), &line).await
            && (stdout.write_all(reply.as_bytes()).is_err()
                || stdout.write_all(b"\n").is_err()
                || stdout.flush().is_err())
        {
            break;
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mcp_initialize_ping_and_tools_list_return_valid_jsonrpc() {
        let init = handle_rpc_line(
            None,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        )
        .await
        .expect("init reply");
        let v: Value = serde_json::from_str(&init).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(v["result"]["serverInfo"]["name"], "quelle");

        let ping = handle_rpc_line(None, r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#)
            .await
            .expect("ping reply");
        let pv: Value = serde_json::from_str(&ping).unwrap();
        assert_eq!(pv["result"], json!({}));

        let list = handle_rpc_line(None, r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#)
            .await
            .expect("tools/list reply");
        let lv: Value = serde_json::from_str(&list).unwrap();
        let names: Vec<&str> = lv["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "quelle_search",
                "quelle_scrape",
                "quelle_extract",
                "quelle_map",
                "quelle_crawl",
            ]
        );
    }

    #[tokio::test]
    async fn mcp_notifications_produce_no_response_and_bad_json_returns_parse_error() {
        assert!(
            handle_rpc_line(
                None,
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            )
            .await
            .is_none()
        );
        let bad = handle_rpc_line(None, "{not-valid-json")
            .await
            .expect("parse error");
        let bv: Value = serde_json::from_str(&bad).unwrap();
        assert_eq!(bv["error"]["code"], -32700);
    }

    #[test]
    fn mcp_tool_mapping_validates_tool_names_and_defaults_sync_crawl() {
        let (path, payload) =
            map_tool_to_request("quelle_crawl", json!({ "url": "https://example.org" })).unwrap();
        assert_eq!(path, "/api/crawl");
        assert_eq!(payload["sync"], true);
        assert_eq!(payload["limit"], 20);

        let (spath, spayload) = map_tool_to_request(
            "quelle_search",
            json!({ "query": "rust borrowing", "stream": true }),
        )
        .unwrap();
        assert_eq!(spath, "/api/content");
        assert!(spayload.get("stream").is_none());

        assert!(map_tool_to_request("quelle_search", json!({})).is_err());
        assert!(map_tool_to_request("unknown_tool", json!({})).is_err());
    }
}
