//! The `expand_artifact` tool (docs/15 M15.1).
//!
//! The counterpart to reduction: it is what makes an aggressive default safe,
//! because anything elided is one tool call away. Lives here rather than in
//! `panday-reducer` because it implements [`Tool`], and the tool vocabulary
//! belongs to the harness.

use crate::tools::{SideEffects, Tool, ToolCtx, ToolOutcome, ToolReq, ToolSpec};
use panday_reducer::{expand, ArtifactStore, LineRange};
use panday_sandbox::SandboxTier;
use panday_types::id::ArtifactRef;
use panday_types::Json;
use std::sync::Arc;

pub struct ExpandArtifact {
    store: Arc<dyn ArtifactStore>,
}

impl ExpandArtifact {
    pub const NAME: &'static str = "expand_artifact";

    pub fn new(store: Arc<dyn ArtifactStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for ExpandArtifact {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::NAME.into(),
            description: "Retrieve an exact line range from a tool result that was elided. \
                          Use the ref and range printed in the elision marker."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "hash": {
                        "type": "string",
                        "description": "The artifact hash from the result's raw_ref."
                    },
                    "range": {
                        "type": "string",
                        "description": "Half-open line range, e.g. \"120..180\" (0-indexed)."
                    }
                },
                "required": ["hash", "range"]
            }),
        }
    }

    fn requirements(&self) -> ToolReq {
        ToolReq {
            // Reading back something already produced: no process, no
            // mutation, and safe to replay after a crash.
            sandbox_tier: SandboxTier::T0InProcess,
            side_effects: SideEffects::None,
            independent: true,
        }
    }

    async fn call(&self, _ctx: ToolCtx, args: Json) -> ToolOutcome {
        let Some(hash) = args.get("hash").and_then(|v| v.as_str()) else {
            return err("expand_artifact needs a `hash` (from the result's raw_ref)");
        };
        let Some(raw_range) = args.get("range").and_then(|v| v.as_str()) else {
            return err("expand_artifact needs a `range`, e.g. \"120..180\"");
        };
        let Some(range) = LineRange::parse(raw_range) else {
            return err(&format!(
                "could not parse range `{raw_range}` — expected start..end, e.g. \"120..180\""
            ));
        };

        // Size and media type are recorded on the event; only the hash
        // identifies the blob, so a model quoting just the hash is enough.
        let handle = ArtifactRef {
            hash: hash.to_string(),
            size: 0,
            media_type: None,
        };

        match expand(self.store.as_ref(), &handle, range) {
            Ok(text) => ToolOutcome {
                raw: text,
                is_error: false,
            },
            Err(e) => err(&format!("{e}")),
        }
    }
}

/// Tool-level failures are observations the model can act on, never panics.
fn err(message: &str) -> ToolOutcome {
    ToolOutcome {
        raw: message.to_string(),
        is_error: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use panday_reducer::MemoryArtifactStore;
    use panday_types::{AccountId, SessionId, TurnId};

    fn ctx() -> ToolCtx {
        ToolCtx {
            account: AccountId::new(),
            session: SessionId::new(),
            turn: TurnId::new(),
        }
    }

    async fn call(store: Arc<MemoryArtifactStore>, args: Json) -> ToolOutcome {
        ExpandArtifact::new(store).call(ctx(), args).await
    }

    fn stored(text: &str) -> (Arc<MemoryArtifactStore>, String) {
        let s = Arc::new(MemoryArtifactStore::new());
        let r = s.put(text.as_bytes(), None).unwrap();
        (s, r.hash)
    }

    #[tokio::test]
    async fn returns_the_requested_lines() {
        let text = (0..50)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (store, hash) = stored(&text);

        let out = call(store, serde_json::json!({"hash": hash, "range": "10..13"})).await;
        assert!(!out.is_error, "{}", out.raw);
        assert!(out.raw.contains("l10") && out.raw.contains("l12"));
        assert!(!out.raw.contains("l13"), "end is exclusive");
    }

    #[tokio::test]
    async fn accepts_a_range_copied_out_of_the_elision_marker() {
        let text = (0..50)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (store, hash) = stored(&text);
        // Exactly the substring the marker prints.
        let out = call(store, serde_json::json!({"hash": hash, "range": "10..13)"})).await;
        assert!(!out.is_error, "{}", out.raw);
        assert!(out.raw.contains("l10"));
    }

    #[tokio::test]
    async fn missing_or_malformed_arguments_are_errors_the_model_can_read() {
        let (store, hash) = stored("a\nb");

        for (args, expect) in [
            (serde_json::json!({"range": "0..1"}), "hash"),
            (serde_json::json!({"hash": hash.clone()}), "range"),
            (
                serde_json::json!({"hash": hash.clone(), "range": "nonsense"}),
                "could not parse range",
            ),
        ] {
            let out = call(store.clone(), args).await;
            assert!(out.is_error);
            assert!(out.raw.contains(expect), "unhelpful message: {}", out.raw);
        }
    }

    #[tokio::test]
    async fn an_unknown_hash_reports_not_found_rather_than_empty_output() {
        // Empty output would read as "the range was blank", which is a
        // different and misleading fact.
        let (store, _) = stored("a\nb");
        let out = call(
            store,
            serde_json::json!({"hash": "0".repeat(64), "range": "0..5"}),
        )
        .await;
        assert!(out.is_error);
        assert!(out.raw.contains("no such artifact"), "{}", out.raw);
    }

    #[test]
    fn is_replay_safe_and_needs_no_sandbox() {
        let t = ExpandArtifact::new(Arc::new(MemoryArtifactStore::new()));
        let req = t.requirements();
        assert_eq!(req.side_effects, SideEffects::None);
        assert_eq!(req.sandbox_tier, SandboxTier::T0InProcess);
    }
}
