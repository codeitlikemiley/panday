//! Hooks — synchronous extension points on the turn loop (docs/13 §hooks).
//!
//! > `pre_turn`, `pre_model`, `post_model`, `pre_tool(call)` (may rewrite args
//! > or veto — this is where policy DLP and the reducer's command-rewrites
//! > hang), `post_tool(result)`, `on_compaction`, `on_stop`. Hook misbehavior
//! > (timeout, panic) is contained: log, skip, continue.
//!
//! ## Containment is the whole design
//!
//! A hook is other people's code running inside our loop. The loop must
//! survive anything it does, because the alternative is that one bad hook can
//! kill a session mid-turn and lose work. So a panicking hook is caught,
//! reported, and **skipped** — the turn continues without it.
//!
//! Containment stops at `Veto`, deliberately: a hook that says "do not run
//! this" is not misbehaving, it is doing its job, and honouring that is the
//! point of `pre_tool`.

use panday_types::event::ReducedOutput;
use panday_types::model::{ChatRequest, StopReason};
use panday_types::Json;
use std::panic::AssertUnwindSafe;

/// What a `pre_tool` hook decides about a call.
#[derive(Debug, Clone, PartialEq)]
pub enum PreTool {
    /// Run it unchanged.
    Proceed,
    /// Run it with these arguments instead — the DLP / command-rewrite path.
    Rewrite(Json),
    /// Do not run it. The reason reaches the model as a tool error.
    Veto(String),
}

/// Extension points. Every method defaults to doing nothing, so a hook
/// implements only what it cares about.
///
/// Synchronous by design (docs/13 calls them "sync extension points"): a hook
/// that can await is a hook that can stall a turn indefinitely, and the loop
/// has no way to tell that from slow-but-working.
pub trait Hook: Send + Sync {
    /// Named so a failure can be attributed to a culprit.
    fn name(&self) -> &str;

    fn pre_turn(&self) {}
    /// May mutate the request — this is where prompt-level policy applies.
    fn pre_model(&self, _req: &mut ChatRequest) {}
    fn post_model(&self, _text: &str) {}
    fn pre_tool(&self, _tool: &str, _args: &Json) -> PreTool {
        PreTool::Proceed
    }
    fn post_tool(&self, _tool: &str, _output: &ReducedOutput) {}
    fn on_compaction(&self, _tokens_before: u32, _tokens_after: u32) {}
    fn on_stop(&self, _reason: StopReason) {}
}

/// Something to report a contained hook failure to.
///
/// A swallowed panic is indistinguishable from a hook that chose to do
/// nothing, so containment without reporting would hide broken hooks forever.
pub trait HookReporter: Send + Sync {
    fn hook_failed(&self, hook: &str, point: &str, detail: &str);
}

/// Collects failures — what the tests assert on, and a sane default for the
/// CLI, which can print them at the end of a turn.
#[derive(Default)]
pub struct CollectFailures {
    failures: std::sync::Mutex<Vec<(String, String, String)>>,
}

impl CollectFailures {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn failures(&self) -> Vec<(String, String, String)> {
        self.failures.lock().unwrap().clone()
    }
}

impl HookReporter for CollectFailures {
    fn hook_failed(&self, hook: &str, point: &str, detail: &str) {
        self.failures
            .lock()
            .unwrap()
            .push((hook.into(), point.into(), detail.into()));
    }
}

/// Runs hooks in registration order, containing their failures.
#[derive(Default)]
pub struct HookEngine {
    hooks: Vec<Box<dyn Hook>>,
    reporter: Option<std::sync::Arc<dyn HookReporter>>,
}

impl HookEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, hook: Box<dyn Hook>) {
        self.hooks.push(hook);
    }

    pub fn with_reporter(mut self, reporter: std::sync::Arc<dyn HookReporter>) -> Self {
        self.reporter = Some(reporter);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Run `f` for one hook, containing a panic.
    ///
    /// The panic payload is turned into a message rather than resumed: a hook
    /// author's `unwrap()` must not take a user's session with it.
    fn guarded<T>(&self, hook: &dyn Hook, point: &str, f: impl FnOnce() -> T) -> Option<T> {
        match std::panic::catch_unwind(AssertUnwindSafe(f)) {
            Ok(v) => Some(v),
            Err(payload) => {
                let detail = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panicked".to_string());
                if let Some(r) = &self.reporter {
                    r.hook_failed(hook.name(), point, &detail);
                }
                None
            }
        }
    }

    pub fn pre_turn(&self) {
        for h in &self.hooks {
            self.guarded(h.as_ref(), "pre_turn", || h.pre_turn());
        }
    }

    pub fn pre_model(&self, req: &mut ChatRequest) {
        for h in &self.hooks {
            // A hook that panics mid-mutation may have changed part of the
            // request. That is unavoidable without cloning per hook; what is
            // avoidable is losing the turn, so the loop continues with
            // whatever state exists and the failure is reported.
            self.guarded(h.as_ref(), "pre_model", || h.pre_model(req));
        }
    }

    pub fn post_model(&self, text: &str) {
        for h in &self.hooks {
            self.guarded(h.as_ref(), "post_model", || h.post_model(text));
        }
    }

    /// Decide about a call.
    ///
    /// **First veto wins and stops the chain** — once one hook has said no,
    /// asking the rest is pointless and lets a later `Rewrite` quietly
    /// override a refusal. Rewrites compose in order, each seeing the previous
    /// one's output, so two hooks can each redact a different thing.
    pub fn pre_tool(&self, tool: &str, args: &Json) -> PreTool {
        let mut current = args.clone();
        let mut rewritten = false;

        for h in &self.hooks {
            let decision = self
                .guarded(h.as_ref(), "pre_tool", || h.pre_tool(tool, &current))
                // A hook that panicked gets no say. Treating a crash as a veto
                // would let a bug silently disable tools; treating it as
                // approval is equally wrong, so it is simply skipped and
                // reported.
                .unwrap_or(PreTool::Proceed);

            match decision {
                PreTool::Proceed => {}
                PreTool::Rewrite(next) => {
                    current = next;
                    rewritten = true;
                }
                PreTool::Veto(reason) => return PreTool::Veto(reason),
            }
        }

        if rewritten {
            PreTool::Rewrite(current)
        } else {
            PreTool::Proceed
        }
    }

    pub fn post_tool(&self, tool: &str, output: &ReducedOutput) {
        for h in &self.hooks {
            self.guarded(h.as_ref(), "post_tool", || h.post_tool(tool, output));
        }
    }

    pub fn on_compaction(&self, before: u32, after: u32) {
        for h in &self.hooks {
            self.guarded(h.as_ref(), "on_compaction", || {
                h.on_compaction(before, after)
            });
        }
    }

    pub fn on_stop(&self, reason: StopReason) {
        for h in &self.hooks {
            self.guarded(h.as_ref(), "on_stop", || h.on_stop(reason));
        }
    }
}
