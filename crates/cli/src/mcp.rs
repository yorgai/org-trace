//! Minimal MCP (Model Context Protocol) server over stdio.
//!
//! MCP is JSON-RPC 2.0 framed one message per line on stdin/stdout. This module
//! implements just enough of it — `initialize`, `tools/list`, `tools/call`, and
//! the `notifications/initialized` ack — to expose Brick's cross-tool memory as
//! agent-callable tools. All tool logic reuses the same `build_*` functions the
//! CLI uses, so MCP and CLI never drift.
//!
//! Protocol invariant: stdout carries ONLY JSON-RPC; every diagnostic goes to
//! stderr. A stray println on stdout corrupts the framing and breaks the client.

use std::io::{BufRead, Write};

use anyhow::Result;
use brick_core::{LocalStore, SourceProfileStore};
use serde_json::{json, Value};

use crate::history::{build_chunks_response, read_profile};
use crate::metadata::{build_query_response, build_recall_response};

/// MCP protocol revision this server speaks.
const PROTOCOL_VERSION: &str = "2024-11-05";
/// Default source scope for tools: search every indexed tool's history.
const DEFAULT_SOURCE: &str = "all";
/// Default recall/query result cap, kept small so tool output stays triage-sized.
const DEFAULT_LIMIT: usize = 10;
/// Default per-field truncation for `read_session`, matching the CLI default.
const DEFAULT_MAX_FIELD_BYTES: usize = 2000;

/// Runs the stdio JSON-RPC loop until stdin closes. Never returns an error to the
/// caller for per-request failures — those become JSON-RPC error responses; only
/// a fatal stdout write failure propagates.
pub fn serve(profiles: &SourceProfileStore, store: &LocalStore) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                eprintln!("brick mcp-serve: stdin read error: {error}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("brick mcp-serve: invalid JSON: {error}");
                write_message(&mut out, &parse_error())?;
                continue;
            }
        };

        // Notifications have no `id` and expect no response.
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");

        if id.is_none() {
            // e.g. notifications/initialized — acknowledge by doing nothing.
            continue;
        }
        let id = id.unwrap();

        let response = match method {
            "initialize" => success(id, initialize_result()),
            "tools/list" => success(id, tools_list_result()),
            "tools/call" => match handle_tool_call(profiles, store, &request) {
                Ok(result) => success(id, result),
                Err(error) => tool_error(id, &error.to_string()),
            },
            "ping" => success(id, json!({})),
            other => method_not_found(id, other),
        };
        write_message(&mut out, &response)?;
    }
    Ok(())
}

fn write_message(out: &mut impl Write, message: &Value) -> Result<()> {
    writeln!(out, "{}", serde_json::to_string(message)?)?;
    out.flush()?;
    Ok(())
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "brick",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": "Brick exposes cross-tool AI coding session memory. Use \
    explore_memory for an open question, recall_file before editing a file, \
    search_sessions to find past work by topic, and read_session to page through a \
    specific session's transcript."
    })
}

fn tools_list_result() -> Value {
    json!({
        "tools": [
            {
                "name": "explore_memory",
                "description": "Answer an open question about past AI coding work \
    by searching cross-tool session history and returning a synthesized summary of \
    the most relevant prior sessions (intent, tool, when, transcript pointer). Use \
    this first when you want context but don't have a specific file path.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "Natural-language question or topic, e.g. \
    'how did we fix the auth token race' or 'pagination work in the CLI'."
                        }
                    },
                    "required": ["question"]
                }
            },
            {
                "name": "recall_file",
                "description": "Recall who previously changed a file and why, across \
    every coding tool on this machine. Returns a one-line summary plus per-session \
    intent and change size. Call before editing a file.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Repo-relative or absolute file path."
                        }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "search_sessions",
                "description": "Free-text search over session metadata (title, \
    intent, touched files, repo, branch) to find past sessions by topic. Returns \
    matches newest-first, each with a transcript pointer.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Keywords to match against session metadata."
                        }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "read_session",
                "description": "Page through one session's full transcript chunks. \
    Supports offset/limit pagination and per-field truncation so large tool outputs \
    don't overflow context; set max_field_bytes to 0 to fetch one chunk untruncated.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source": {
                            "type": "string",
                            "description": "Source id (e.g. claude_code, codex_app, cursor_ide, orgii)."
                        },
                        "session_id": {
                            "type": "string",
                            "description": "External session id from a search/recall hit."
                        },
                        "offset": { "type": "integer", "description": "Chunk offset (default 0)." },
                        "limit": { "type": "integer", "description": "Max chunks (default 50)." },
                        "max_field_bytes": {
                            "type": "integer",
                            "description": "Truncate string values over this many bytes; 0 disables (default 2000)."
                        }
                    },
                    "required": ["source", "session_id"]
                }
            }
        ]
    })
}

fn handle_tool_call(
    profiles: &SourceProfileStore,
    store: &LocalStore,
    request: &Value,
) -> Result<Value> {
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing tool name"))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let payload = match name {
        "explore_memory" => {
            let question = str_arg(&args, "question")?;
            let query = build_query_response(profiles, &question, DEFAULT_SOURCE, DEFAULT_LIMIT)?;
            explore_summary(&question, &query)
        }
        "recall_file" => {
            let path = str_arg(&args, "path")?;
            let recall =
                build_recall_response(store, profiles, &path, DEFAULT_SOURCE, DEFAULT_LIMIT)?;
            serde_json::to_value(recall)?
        }
        "search_sessions" => {
            let query = str_arg(&args, "query")?;
            let response = build_query_response(profiles, &query, DEFAULT_SOURCE, DEFAULT_LIMIT)?;
            serde_json::to_value(response)?
        }
        "read_session" => {
            let source = str_arg(&args, "source")?;
            let session_id = str_arg(&args, "session_id")?;
            let offset = usize_arg(&args, "offset").unwrap_or(0);
            let limit = usize_arg(&args, "limit").unwrap_or(50);
            let max_field_bytes =
                usize_arg(&args, "max_field_bytes").unwrap_or(DEFAULT_MAX_FIELD_BYTES);
            let profile = read_profile(profiles, &source)?;
            let response = build_chunks_response(
                &profile,
                &source,
                &session_id,
                limit,
                offset,
                max_field_bytes,
            )?;
            serde_json::to_value(response)?
        }
        other => return Err(anyhow::anyhow!("unknown tool: {other}")),
    };

    // MCP tool results wrap content blocks; we hand back the JSON as text so the
    // agent gets the full structured payload it can parse.
    Ok(json!({
        "content": [
            { "type": "text", "text": serde_json::to_string_pretty(&payload)? }
        ]
    }))
}

/// Synthesizes a compact, agent-ready summary from a query result — this is the
/// "coarse-grained, agent-like" tool: it does the search-and-condense work inside
/// Brick so the caller gets conclusions, not raw rows.
fn explore_summary(question: &str, query: &crate::metadata::MetadataQueryResponse) -> Value {
    let findings: Vec<Value> = query
        .matches
        .iter()
        .take(5)
        .map(|m| {
            json!({
                "tool": m.source_id,
                "intent": m.intent,
                "when": m.last_seen_at,
                "repo": m.repo_path,
                "branch": m.branch,
                "read_session_hint": m.recall_chunks_hint,
            })
        })
        .collect();
    json!({
        "question": question,
        "summary": query.summary,
        "match_count": query.match_count,
        "top_findings": findings,
        "next_step": "Call read_session with a finding's source + session_id to read its transcript."
    })
}

fn str_arg(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing required string argument: {key}"))
}

fn usize_arg(args: &Value, key: &str) -> Option<usize> {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
}

fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn tool_error(id: Value, message: &str) -> Value {
    // Tool failures are reported as a result with isError=true (MCP convention)
    // so the agent can read the message rather than the call hard-failing.
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [ { "type": "text", "text": message } ],
            "isError": true
        }
    })
}

fn method_not_found(id: Value, method: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32601, "message": format!("method not found: {method}") }
    })
}

fn parse_error() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": { "code": -32700, "message": "parse error" }
    })
}
