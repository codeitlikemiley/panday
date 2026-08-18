//! Deterministic test doubles for the loop.
//!
//! docs/02 §Testing philosophy: "The harness state machine is tested
//! **without any model**: a scripted `ModelClient` fake drives loops
//! deterministically." Public rather than `#[cfg(test)]` because
//! `panday-cli`, `panday-local` and the eval harness all need to drive a
//! session without a provider.

use crate::tools::{SideEffects, Tool, ToolCtx, ToolOutcome, ToolReq, ToolSpec};
use panday_sandbox::SandboxTier;
use panday_sdk::{ItemStream, ModelClient, PandayError};
use panday_types::model::{ChatRequest, StopReason, StreamItem};
use panday_types::{CallId, Json};
use std::sync::Mutex;

/// One scripted model turn.
#[derive(Debug, Clone)]
pub struct ScriptedTurn {
    pub text: String,
    /// `(tool, args)` — the fake mints a `CallId` per call, as a real
    /// adapter does.
    pub calls: Vec<(String, Json)>,
    pub stop: StopReason,
    pub usage: panday_types::model::Usage,
}

impl ScriptedTurn {
    /// A turn that only talks.
    pub fn text(s: &str) -> Self {
        Self {
            text: s.into(),
            calls: Vec::new(),
            stop: StopReason::EndTurn,
            usage: panday_types::model::Usage {
                input_tokens: 100,
                output_tokens: 10,
                ..Default::default()
            },
        }
    }

    /// A turn that calls tools.
    pub fn calling(s: &str, calls: Vec<(&str, Json)>) -> Self {
        Self {
            text: s.into(),
            calls: calls.into_iter().map(|(n, a)| (n.to_string(), a)).collect(),
            stop: StopReason::ToolUse,
            usage: panday_types::model::Usage {
                input_tokens: 100,
                output_tokens: 10,
                ..Default::default()
            },
        }
    }
}

/// Replays scripted turns in order. Running past the end is a loud error
/// rather than a silent empty turn — an over-running loop is exactly the bug
/// these tests exist to catch.
pub struct ScriptedClient {
    turns: Mutex<std::collections::VecDeque<ScriptedTurn>>,
    seen: Mutex<Vec<ChatRequest>>,
}

impl ScriptedClient {
    pub fn new(turns: Vec<ScriptedTurn>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// Every request the loop issued — lets tests assert on context assembly.
    pub fn requests(&self) -> Vec<ChatRequest> {
        self.seen.lock().unwrap().clone()
    }

    pub fn remaining(&self) -> usize {
        self.turns.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl ModelClient for ScriptedClient {
    async fn chat(&self, req: ChatRequest) -> Result<ItemStream, PandayError> {
        self.seen.lock().unwrap().push(req);

        let turn = self.turns.lock().unwrap().pop_front().ok_or_else(|| {
            PandayError::Protocol(
                "ScriptedClient exhausted: the loop asked for more turns than the script \
                 provides, which usually means it failed to stop"
                    .into(),
            )
        })?;

        let mut items: Vec<Result<StreamItem, PandayError>> = Vec::new();
        if !turn.text.is_empty() {
            // Split into two deltas so tests exercise accumulation.
            let mid = turn.text.len() / 2;
            let (a, b) = turn.text.split_at(turn.text.floor_char_boundary(mid));
            if !a.is_empty() {
                items.push(Ok(StreamItem::Delta { text: a.into() }));
            }
            if !b.is_empty() {
                items.push(Ok(StreamItem::Delta { text: b.into() }));
            }
        }
        for (i, (name, args)) in turn.calls.iter().enumerate() {
            let id = CallId::new();
            items.push(Ok(StreamItem::ToolCallStart {
                id,
                name: name.clone(),
                provider_id: Some(format!("call_scripted_{i}")),
            }));
            items.push(Ok(StreamItem::ToolCallDelta {
                id,
                args_fragment: args.to_string(),
            }));
        }
        items.push(Ok(StreamItem::Usage { usage: turn.usage }));
        items.push(Ok(StreamItem::Done { reason: turn.stop }));

        Ok(Box::pin(futures_util::stream::iter(items)))
    }
}

/// A tool that returns whatever it is told to, for driving the loop.
///
/// The real native toolset (`read_file`, `bash`, …) is M13.2; this exists so
/// the state machine can be tested before any of it lands.
pub struct EchoTool {
    pub name: String,
    pub reply: String,
    pub is_error: bool,
    pub side_effects: SideEffects,
}

impl EchoTool {
    pub fn ok(name: &str, reply: &str) -> Box<dyn Tool> {
        Box::new(Self {
            name: name.into(),
            reply: reply.into(),
            is_error: false,
            side_effects: SideEffects::None,
        })
    }

    pub fn failing(name: &str, reply: &str) -> Box<dyn Tool> {
        Box::new(Self {
            name: name.into(),
            reply: reply.into(),
            is_error: true,
            side_effects: SideEffects::None,
        })
    }

    /// A mutating-but-replayable tool: denied under `read_only`, `Ask` under
    /// `dev`, allowed under `unleashed`.
    pub fn mutating(name: &str, reply: &str) -> Box<dyn Tool> {
        Box::new(Self {
            name: name.into(),
            reply: reply.into(),
            is_error: false,
            side_effects: SideEffects::Idempotent,
        })
    }

    /// An irreversible tool — forces `Ask` regardless of profile (docs/13).
    pub fn irreversible(name: &str, reply: &str) -> Box<dyn Tool> {
        Box::new(Self {
            name: name.into(),
            reply: reply.into(),
            is_error: false,
            side_effects: SideEffects::Irreversible,
        })
    }
}

#[async_trait::async_trait]
impl Tool for EchoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: format!("test double: {}", self.name),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    fn requirements(&self) -> ToolReq {
        ToolReq {
            sandbox_tier: SandboxTier::T0InProcess,
            side_effects: self.side_effects,
            independent: true,
        }
    }

    async fn call(&self, _ctx: ToolCtx, _args: Json) -> ToolOutcome {
        ToolOutcome {
            raw: self.reply.clone(),
            is_error: self.is_error,
        }
    }
}

/// Render a log deterministically for golden comparison.
///
/// Ids are UUIDv7 and timestamps are wall-clock, so a raw log can never be
/// byte-compared. This substitutes stable labels in first-appearance order
/// (`sess_1`, `turn_1`, `call_1`) and drops `at`, leaving exactly the part of
/// the log that is a protocol decision rather than an accident of when it ran.
pub fn render_log(events: &[panday_types::event::Envelope]) -> String {
    use std::collections::HashMap;
    let mut labels: HashMap<String, String> = HashMap::new();
    let mut counters: HashMap<&str, usize> = HashMap::new();

    let mut label = |raw: &str, kind: &'static str| -> String {
        labels
            .entry(raw.to_string())
            .or_insert_with(|| {
                let n = counters.entry(kind).or_insert(0);
                *n += 1;
                format!("{kind}_{n}")
            })
            .clone()
    };

    let mut out = String::new();
    for env in events {
        let mut value = serde_json::to_value(env).expect("envelope serializes");
        let obj = value.as_object_mut().expect("envelope is an object");
        obj.remove("at");

        for key in ["session_id", "turn_id", "call_id", "child"] {
            if let Some(v) = obj.get(key).and_then(|v| v.as_str()).map(str::to_string) {
                let kind = match key {
                    "session_id" | "child" => "sess",
                    "turn_id" => "turn",
                    _ => "call",
                };
                obj.insert(key.into(), serde_json::Value::String(label(&v, kind)));
            }
        }

        out.push_str(&canonical(&serde_json::Value::Object(obj.clone())));
        out.push('\n');
    }
    out
}

/// Serialize with keys in sorted order, always.
///
/// `serde_json` orders object keys by `BTreeMap` normally but by insertion
/// order when the `preserve_order` feature is on — and features unify across
/// a build, so this workspace gets insertion order only when `schemars`
/// (which enables it) is in the graph. That made a golden file's bytes depend
/// on WHICH crates were being compiled: `cargo test -p panday-harness` and
/// `cargo test --workspace` disagreed. Canonicalising here makes the fixture
/// a property of the log alone.
fn canonical(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let body: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    format!(
                        "{}:{}",
                        serde_json::Value::String(k.clone()),
                        canonical(&map[k])
                    )
                })
                .collect();
            format!("{{{}}}", body.join(","))
        }
        serde_json::Value::Array(items) => {
            let body: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", body.join(","))
        }
        other => other.to_string(),
    }
}
