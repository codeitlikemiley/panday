//! The OpenAPI description of the ingress (M10.6).
//!
//! Written here rather than in a YAML file next to it, because a hand-maintained description of a
//! handler is wrong the first week nobody notices. This module is the single place the paths are
//! named: `ingress::router` builds its routes from `ROUTES`, a test walks `ROUTES` against a live
//! server and fails if a documented path 404s, and `cargo xtask ts-sdk` generates the client from
//! this document. Three consumers, one list.
//!
//! Deliberately *not* a full description of OpenAI's dialect. It describes what this ingress
//! accepts and returns, which is a subset — documenting fields we ignore would promise behaviour
//! the code does not have.

use serde_json::{json, Value};

/// Every route the ingress serves: method, path, operation id.
pub const ROUTES: &[(&str, &str, &str)] = &[
    ("post", "/v1/chat/completions", "createChatCompletion"),
    ("get", "/metrics", "getMetrics"),
    ("get", "/v1/models", "listModels"),
    ("post", "/v1/messages", "createMessage"),
    ("get", "/v1beta/models", "listGeminiModels"),
    ("post", "/v1beta/models/{*tail}", "generateContent"),
];

/// The OpenAPI 3.1 document.
pub fn document() -> Value {
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Panday gateway",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "OpenAI-compatible inference, routed by policy and metered per call \
    (docs/11). A client is redirected by changing a base URL and nothing else.",
            "license": { "name": "MIT OR Apache-2.0" },
        },
        "servers": [{ "url": "http://127.0.0.1:8088", "description": "the dev stack (`just dev`)" }],
        "security": [{ "bearerAuth": [] }],
        "paths": {
            "/v1/chat/completions": {
                "post": {
                    "operationId": "createChatCompletion",
                    "summary": "Create a completion. `model: \"auto\"` lets the router choose.",
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": {
                            "schema": { "$ref": "#/components/schemas/ChatCompletionRequest" }
                        }},
                    },
                    "responses": {
                        "200": {
                            "description": "A completion, or an SSE stream when `stream` is true. \
    The stream ends with a literal `data: [DONE]` sentinel.",
                            "content": {
                                "application/json": { "schema": {
                                    "$ref": "#/components/schemas/ChatCompletion"
                                }},
                                "text/event-stream": { "schema": { "type": "string" } },
                            },
                        },
                        "401": { "$ref": "#/components/responses/Error" },
                        "402": { "$ref": "#/components/responses/Error" },
                        "404": { "$ref": "#/components/responses/Error" },
                        "429": { "$ref": "#/components/responses/RateLimited" },
                        "503": { "$ref": "#/components/responses/Error" },
                    },
                },
            },
            "/metrics": {
                "get": {
                    "operationId": "getMetrics",
                    "summary": "Prometheus text exposition. Unauthenticated by design (docs/21).",
                    "security": [],
                    "responses": { "200": {
                        "description": "Counts and decisions. Never content, never an id.",
                        "content": { "text/plain": { "schema": { "type": "string" } } },
                    }},
                },
            },
            "/v1/models": {
                "get": {
                    "operationId": "listModels",
                    "summary": "Models the signed-in providers list for this account, not the YAML catalog.",
                    "responses": {
                        "200": {
                            "description": "OpenAI-shaped list. `id` is `provider/model`.",
                            "content": { "application/json": { "schema": {
                                "$ref": "#/components/schemas/ModelList"
                            }}},
                        },
                        "401": { "$ref": "#/components/responses/Error" },
                    },
                },
            },
            "/v1/messages": {
                "post": {
                    "operationId": "createMessage",
                    "summary": "Anthropic Messages API. Claude Code: ANTHROPIC_BASE_URL=http://127.0.0.1:8088",
                    "responses": {
                        "200": { "description": "A message, or Anthropic SSE when stream is true." },
                        "401": { "$ref": "#/components/responses/Error" },
                        "402": { "$ref": "#/components/responses/Error" },
                        "429": { "$ref": "#/components/responses/RateLimited" },
                    },
                },
            },
            "/v1beta/models": {
                "get": {
                    "operationId": "listGeminiModels",
                    "summary": "Gemini/Antigravity model list (GOOGLE_GEMINI_BASE_URL).",
                    "responses": { "200": { "description": "models[] with name models/{id}" } },
                },
            },
            "/v1beta/models/{*tail}": {
                "post": {
                    "operationId": "generateContent",
                    "summary": "Gemini generateContent / streamGenerateContent. Tail is `{model}:generateContent`.",
                    "responses": {
                        "200": { "description": "GenerateContentResponse or SSE." },
                        "429": { "$ref": "#/components/responses/RateLimited" },
                    },
                },
            },
        },
        "components": {
            "securitySchemes": { "bearerAuth": {
                "type": "http",
                "scheme": "bearer",
                "description": "An API key: `pnd_live_…` or `pnd_test_…` (docs/17). Every refusal \
    is a 401 with the same body — missing, malformed, unknown and revoked are not distinguished.",
            }},
            "responses": {
                "Error": {
                    "description": "The standard error envelope, so a client that only understands \
    OpenAI's shape can read our failures too.",
                    "content": { "application/json": { "schema": {
                        "$ref": "#/components/schemas/ErrorResponse"
                    }}},
                },
                // Its own component rather than a `headers` block on `Error`:
                // that one is shared with 401/402/503, none of which carry a
                // wait (docs/11 M11.7).
                "RateLimited": {
                    "description": "Rate limited. Carries `Retry-After` when an upstream stated a \
    wait; the header is absent — never zero — when none did.",
                    "headers": { "Retry-After": {
                        "description": "Delay-seconds (RFC 9110 §10.2.3), rounded up. Absent when \
    no upstream stated a wait: 0 would mean \"retry now\".",
                        "schema": { "type": "integer", "minimum": 1 },
                    }},
                    "content": { "application/json": { "schema": {
                        "$ref": "#/components/schemas/ErrorResponse"
                    }}},
                },
            },
            "schemas": schemas(),
        },
    })
}

fn schemas() -> Value {
    json!({
        "ModelList": {
            "type": "object",
            "required": ["object", "data"],
            "properties": {
                "object": { "const": "list" },
                "data": {
                    "type": "array",
                    "items": { "$ref": "#/components/schemas/Model" },
                },
            },
        },
        "Model": {
            "type": "object",
            "required": ["id", "object"],
            "properties": {
                "id": { "type": "string", "description": "`provider/model`" },
                "object": { "const": "model" },
                "owned_by": { "type": "string" },
            },
        },
        "ChatCompletionRequest": {
            "type": "object",
            "description": "The subset of the standard dialect this ingress reads. Fields not \
    listed are accepted and ignored rather than rejected — a client should not have to strip its \
    request to be redirected here.",
            "required": ["model", "messages"],
            "properties": {
                "model": {
                    "type": "string",
                    "description": "`provider/model`, or `auto` to route by policy (docs/12).",
                },
                "messages": {
                    "type": "array",
                    "items": { "$ref": "#/components/schemas/Message" },
                },
                "stream": {
                    "type": "boolean",
                    "default": false,
                    "description": "Server-sent events, ending with `data: [DONE]`.",
                },
                "temperature": { "type": "number" },
                "top_p": { "type": "number" },
                "max_tokens": { "type": "integer" },
                "stop": {
                    "description": "A string or an array; both shapes appear in the wild.",
                    "anyOf": [
                        { "type": "string" },
                        { "type": "array", "items": { "type": "string" } },
                    ],
                },
            },
        },
        "Message": {
            "type": "object",
            "required": ["role"],
            "properties": {
                "role": { "enum": ["system", "user", "assistant", "tool"] },
                "content": {
                    "description": "Absent on an assistant message that carried only tool calls.",
                    "anyOf": [{ "type": "string" }, { "type": "null" }],
                },
            },
        },
        "ChatCompletion": {
            "type": "object",
            "required": ["id", "object", "created", "model", "choices"],
            "properties": {
                "id": { "type": "string" },
                "object": { "const": "chat.completion" },
                "created": { "type": "integer" },
                "model": {
                    "type": "string",
                    "description": "The model that actually answered, which is not necessarily the \
    one requested: `auto` resolves through policy, and a failover walks the chain.",
                },
                "choices": { "type": "array", "items": { "$ref": "#/components/schemas/Choice" } },
                "usage": { "$ref": "#/components/schemas/Usage" },
            },
        },
        "Choice": {
            "type": "object",
            "required": ["index", "message", "finish_reason"],
            "properties": {
                "index": { "type": "integer" },
                "message": { "$ref": "#/components/schemas/Message" },
                "finish_reason": { "enum": ["stop", "length", "tool_calls", "content_filter"] },
            },
        },
        "Usage": {
            "type": "object",
            "required": ["prompt_tokens", "completion_tokens", "total_tokens"],
            "properties": {
                "prompt_tokens": { "type": "integer" },
                "completion_tokens": { "type": "integer" },
                "total_tokens": { "type": "integer" },
            },
        },
        "ErrorResponse": {
            "type": "object",
            "required": ["error"],
            "properties": { "error": {
                "type": "object",
                "required": ["message", "type"],
                "properties": {
                    "message": { "type": "string" },
                    "type": {
                        "enum": [
                            "authentication_error",
                            "rate_limit_error",
                            "invalid_request_error",
                            "model_not_found",
                            "api_error",
                        ],
                    },
                },
            }},
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_is_described() {
        // The list the router is built from and the list the document describes are the same list.
        let doc = document();
        for (method, path, operation) in ROUTES {
            let op = &doc["paths"][path][method];
            assert!(!op.is_null(), "{method} {path} is not in the document");
            assert_eq!(op["operationId"], *operation);
        }
    }

    #[test]
    fn nothing_is_described_that_is_not_routed() {
        // The other direction, which is the one that rots: a path removed from the router but left
        // in the document is an SDK method that 404s.
        let doc = document();
        let described = doc["paths"].as_object().unwrap();
        assert_eq!(
            described.len(),
            ROUTES.len(),
            "the document describes {} paths and the router serves {}",
            described.len(),
            ROUTES.len()
        );
    }

    #[test]
    fn every_ref_resolves() {
        // A dangling `$ref` produces a generated client that does not compile, discovered by
        // whoever runs the generator rather than by whoever wrote the document.
        let doc = document();
        let text = doc.to_string();
        let schemas = doc["components"]["schemas"].as_object().unwrap();
        let responses = doc["components"]["responses"].as_object().unwrap();
        for part in text.split("#/components/") {
            let Some(reference) = part.split('"').next() else {
                continue;
            };
            let Some((kind, name)) = reference.split_once('/') else {
                continue;
            };
            match kind {
                "schemas" => assert!(schemas.contains_key(name), "dangling schema ref: {name}"),
                "responses" => assert!(responses.contains_key(name), "dangling response: {name}"),
                _ => {}
            }
        }
    }

    #[test]
    fn the_metrics_endpoint_declares_that_it_needs_no_key() {
        // A metrics endpoint that needs a key is a metrics endpoint nobody scrapes — and an SDK
        // that sends one to it would be documenting a requirement that does not exist.
        let doc = document();
        assert_eq!(doc["paths"]["/metrics"]["get"]["security"], json!([]));
    }
}
