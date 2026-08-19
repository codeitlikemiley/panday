//! A hand-written MCP server, used as a test fixture (M16.3).
//!
//! Hand-written rather than built with `rmcp`'s server half on purpose: mounting "a
//! public MCP server" means talking to somebody else's implementation, and a test
//! where both ends come from the same crate would prove only that the crate agrees
//! with itself. This speaks the wire — JSON-RPC 2.0, line-delimited over stdio —
//! the way a Python or TypeScript server does.
//!
//! Built as a test binary so the suite can spawn it via `CARGO_BIN_EXE_`.

use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(request): Result<serde_json::Value, _> = serde_json::from_str(&line) else {
            continue;
        };
        let method = request["method"].as_str().unwrap_or_default();
        let id = request.get("id").cloned();

        let result = match method {
            "initialize" => Some(serde_json::json!({
                "protocolVersion": request["params"]["protocolVersion"]
                    .as_str()
                    .unwrap_or("2025-06-18"),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "fixture-server", "version": "0.0.1" }
            })),
            "tools/list" => Some(serde_json::json!({
                "tools": [
                    {
                        "name": "list_issues",
                        "description": "List issues in a repository.\nA second line that the brief form must drop.",
                        "inputSchema": {
                            "type": "object",
                            "properties": { "repo": { "type": "string" } },
                            "required": ["repo"]
                        }
                    },
                    {
                        "name": "create_issue",
                        "description": "Open a new issue.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "repo": { "type": "string" },
                                "title": { "type": "string" }
                            }
                        }
                    },
                    {
                        "name": "leak_env",
                        "description": "Reports how many environment variables this server can see.",
                        "inputSchema": { "type": "object" }
                    },
                    {
                        "name": "explode",
                        "description": "Always reports an error.",
                        "inputSchema": { "type": "object" }
                    },
                    {
                        "name": "picture",
                        "description": "Returns non-text content.",
                        "inputSchema": { "type": "object" }
                    }
                ]
            })),
            "tools/call" => {
                let name = request["params"]["name"].as_str().unwrap_or_default();
                let args = &request["params"]["arguments"];
                Some(match name {
                    "list_issues" => serde_json::json!({
                        "content": [{
                            "type": "text",
                            "text": format!("2 open issues in {}", args["repo"].as_str().unwrap_or("?"))
                        }],
                        "isError": false
                    }),
                    "create_issue" => serde_json::json!({
                        "content": [{
                            "type": "text",
                            "text": format!("opened `{}`", args["title"].as_str().unwrap_or("?"))
                        }],
                        "isError": false
                    }),
                    "leak_env" => serde_json::json!({
                        "content": [{
                            "type": "text",
                            "text": format!("env_vars={}", std::env::vars().count())
                        }],
                        "isError": false
                    }),
                    "explode" => serde_json::json!({
                        "content": [{ "type": "text", "text": "the repository does not exist" }],
                        "isError": true
                    }),
                    "picture" => serde_json::json!({
                        "content": [{
                            "type": "image",
                            "data": "aGVsbG8=",
                            "mimeType": "image/png"
                        }],
                        "isError": false
                    }),
                    other => serde_json::json!({
                        "content": [{ "type": "text", "text": format!("no such tool: {other}") }],
                        "isError": true
                    }),
                })
            }
            // Notifications (no id) get no response, which is the part of JSON-RPC a
            // naive server gets wrong and then hangs a client.
            _ => None,
        };

        if let (Some(id), Some(result)) = (id, result) {
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result
            });
            let _ = writeln!(stdout, "{response}");
            let _ = stdout.flush();
        }
    }
}
