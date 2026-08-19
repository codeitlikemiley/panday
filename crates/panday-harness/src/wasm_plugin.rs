//! Plugin tools and hooks in the loop (M16.4).
//!
//! The T1 tier (`panday_sandbox::t1_wasm`, `t1_hook`) runs components; this is
//! where they become things the loop already understands — a `Tool` in the
//! registry and a `Hook` in the engine. docs/16's claim is that "the loop can't
//! tell them from native tools; the *permission engine* can", and these adapters
//! are what makes the first half true: nothing in the turn loop knows a tool is
//! WASM.
//!
//! The second half is the `ToolReq` these report: `sandbox_tier: T1Wasm` and
//! `SideEffects` from the manifest, which is what the gate reads.

use crate::hooks::{Hook, HookReporter, PreTool};
use crate::tools::{Replay, SideEffects, Tool, ToolCtx, ToolOutcome, ToolReq, ToolSpec};
use panday_sandbox::t1_hook::{redact, HookCall, Verdict, WasmHook};
use panday_sandbox::t1_wasm::{T1Limits, T1Runtime, WasmTool};
use panday_sandbox::SandboxTier;
use panday_types::Json;
use std::sync::Arc;

/// A plugin-provided tool.
pub struct WasmPluginTool {
    runtime: Arc<T1Runtime>,
    component: Arc<WasmTool>,
    spec: ToolSpec,
    /// From `plugin.toml` (docs/16): the manifest declares what the tool does, and
    /// the gate reads it. A plugin cannot lower its own gating by lying here — the
    /// declaration is consented to at install time.
    side_effects: SideEffects,
    limits: T1Limits,
}

impl WasmPluginTool {
    pub fn new(
        runtime: Arc<T1Runtime>,
        component: Arc<WasmTool>,
        spec: ToolSpec,
        side_effects: SideEffects,
    ) -> Self {
        Self {
            runtime,
            component,
            spec,
            side_effects,
            limits: T1Limits::default(),
        }
    }

    pub fn with_limits(mut self, limits: T1Limits) -> Self {
        self.limits = limits;
        self
    }
}

#[async_trait::async_trait]
impl Tool for WasmPluginTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn requirements(&self) -> ToolReq {
        ToolReq {
            sandbox_tier: SandboxTier::T1Wasm,
            side_effects: self.side_effects,
            // Two plugin tools can run at once — they share nothing, since each
            // call gets its own store.
            independent: true,
            // A pure-read plugin tool is replay-safe; anything that mutates is not,
            // and the manifest is the only thing that knows which this is.
            replay: match self.side_effects {
                SideEffects::None => Replay::Safe,
                _ => Replay::Unsafe,
            },
        }
    }

    async fn call(&self, _ctx: ToolCtx, args: Json) -> ToolOutcome {
        let runtime = self.runtime.clone();
        let component = self.component.clone();
        let limits = self.limits;
        let payload = args.to_string();

        // `spawn_blocking`, because wasmtime's sync API blocks the thread it runs
        // on and a guest is allowed to use its whole fuel budget. Running it on the
        // async worker would stall every other session on that thread for as long
        // as the budget allows.
        let result =
            tokio::task::spawn_blocking(move || runtime.call(&component, &payload, limits)).await;

        match result {
            Ok(Ok((output, logs))) => {
                for (level, message) in logs.0 {
                    // The plugin's own words, attributed to it and never
                    // interpolated into a field of ours (docs/21 T5).
                    tracing::debug!(plugin = %self.spec.name, level = %level, message = %message, "plugin log");
                }
                ToolOutcome {
                    raw: output,
                    is_error: false,
                }
            }
            // A plugin's failure is a tool error, not a harness error: the model
            // reads it and can try something else, which is the same treatment a
            // native tool's failure gets.
            Ok(Err(e)) => ToolOutcome {
                raw: format!("plugin tool failed: {e}"),
                is_error: true,
            },
            Err(join) => ToolOutcome {
                raw: format!("plugin tool did not run: {join}"),
                is_error: true,
            },
        }
    }
}

/// A plugin-provided hook.
///
/// docs/16: "Fuel-metered, epoch-interrupted, 10ms default budget; a hook that
/// exceeds it is **skipped and the event logged**." The tier enforces the budget;
/// this decides what a breach means, and the answer is docs/13's rule for every
/// hook failure — log, skip, continue. A hook that ran out of time gets no say:
/// treating the breach as a veto would let a slow plugin disable tools, and
/// treating it as approval would let one bypass a policy by timing out.
pub struct WasmPluginHook {
    runtime: Arc<T1Runtime>,
    component: Arc<WasmHook>,
    name: String,
    limits: T1Limits,
    reporter: Option<Arc<dyn HookReporter>>,
}

impl WasmPluginHook {
    pub fn new(runtime: Arc<T1Runtime>, component: Arc<WasmHook>, name: impl Into<String>) -> Self {
        Self {
            runtime,
            component,
            name: name.into(),
            limits: T1Limits::hook(),
            reporter: None,
        }
    }

    pub fn with_limits(mut self, limits: T1Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Where breaches are reported. Without one a skipped hook is invisible, which
    /// is the failure mode `HookReporter` exists to prevent.
    pub fn with_reporter(mut self, reporter: Arc<dyn HookReporter>) -> Self {
        self.reporter = Some(reporter);
        self
    }

    fn failed(&self, point: &str, detail: String) {
        tracing::warn!(hook = %self.name, point, detail = %detail, "plugin hook skipped");
        if let Some(r) = &self.reporter {
            r.hook_failed(&self.name, point, &detail);
        }
    }

    fn call(&self, point: &str, call: HookCall<'_>) -> Option<Verdict> {
        match self.runtime.call_hook(&self.component, call, self.limits) {
            Ok(v) => Some(v),
            Err(e) => {
                self.failed(point, e.to_string());
                None
            }
        }
    }
}

impl Hook for WasmPluginHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn pre_tool(&self, tool: &str, args: &Json) -> PreTool {
        // Redacted before the guest sees it (docs/16). Done here rather than in the
        // tier so a *native* hook is not silently held to a different standard —
        // this is the only place a plugin receives tool arguments.
        let redacted = redact(args).to_string();
        match self.call(
            "pre_tool",
            HookCall::PreTool {
                tool,
                args: &redacted,
            },
        ) {
            Some(Verdict::Proceed) | None => PreTool::Proceed,
            Some(Verdict::Veto(reason)) => PreTool::Veto(reason),
            Some(Verdict::Rewrite(json)) => match serde_json::from_str::<Json>(&json) {
                Ok(next) => PreTool::Rewrite(next),
                Err(e) => {
                    // A rewrite we cannot parse is not a rewrite. Passing the
                    // original through is the only safe reading: substituting
                    // something the hook did not ask for would be worse.
                    self.failed("pre_tool", format!("unparseable rewrite: {e}"));
                    PreTool::Proceed
                }
            },
        }
    }

    fn post_tool(&self, tool: &str, output: &panday_types::event::ReducedOutput) {
        self.call(
            "post_tool",
            HookCall::PostTool {
                tool,
                output: &output.text,
            },
        );
    }

    fn on_stop(&self, reason: panday_types::model::StopReason) {
        self.call(
            "on_stop",
            HookCall::OnStop {
                reason: reason.as_str(),
            },
        );
    }
}
