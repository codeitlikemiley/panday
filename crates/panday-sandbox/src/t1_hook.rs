//! T1 hook runtime (M16.4, docs/16 §hooks).
//!
//! > "Plugin hooks are WASM components implementing `panday:plugin/hook` — same
//! > lifecycle points as in-process hooks (13) with vetoes limited to `pre_tool`.
//! > Fuel-metered, epoch-interrupted, 10ms default budget; a hook that exceeds it
//! > is skipped and the event logged. Hooks see *redacted* views (no secrets in
//! > args)."
//!
//! Same engine, same empty `WasiCtx`, same limits machinery as `t1_wasm` — a hook
//! that ran with a more generous context than a tool would be a hole in the tier,
//! so the store is built by one function for both.
//!
//! The one behavioural difference is what a failure means. A tool that traps
//! produces a tool error the model can read and correct. A hook that traps has no
//! say: docs/13 is explicit that a hook failure is "log, skip, continue", because
//! treating a crash as a veto lets a bug silently disable tools, and treating it as
//! approval is equally wrong.

use crate::t1_wasm::{classify, HostState, T1Error, T1Limits, T1Runtime};
use wasmtime::component::{Component, Linker};

wasmtime::component::bindgen!({
    world: "hook",
    path: "wit",
    // Reuse the tool world's generated `host` bindings so one `HostState`
    // implements one `Host` trait. Two generated copies of the same interface
    // would mean two implementations to keep in step, and the log cap in one of
    // them only.
    with: {
        "panday:plugin/host": crate::t1_wasm::panday::plugin::host,
    },
});

/// A type-only interface still gets a `Host` trait; there is nothing to implement.
impl panday::plugin::hook_types::Host for HostState {}

pub struct WasmHook {
    component: Component,
    name: String,
}

impl WasmHook {
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl std::fmt::Debug for WasmHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmHook")
            .field("name", &self.name)
            .finish()
    }
}

/// Which lifecycle point is being called. Only `pre-tool` can answer.
#[derive(Debug, Clone)]
pub enum HookCall<'a> {
    PreTool { tool: &'a str, args: &'a str },
    PostTool { tool: &'a str, output: &'a str },
    OnStop { reason: &'a str },
}

impl T1Runtime {
    pub fn compile_hook(&self, name: &str, bytes: &[u8]) -> Result<WasmHook, T1Error> {
        let component = Component::from_binary(self.engine(), bytes)
            .map_err(|e| T1Error::Invalid(format!("{name}: {e}")))?;
        Ok(WasmHook {
            component,
            name: name.to_string(),
        })
    }

    pub fn compile_hook_file(
        &self,
        name: &str,
        path: impl AsRef<std::path::Path>,
    ) -> Result<WasmHook, T1Error> {
        let bytes = std::fs::read(path.as_ref())
            .map_err(|e| T1Error::Invalid(format!("{}: {e}", path.as_ref().display())))?;
        self.compile_hook(name, &bytes)
    }

    /// Call one lifecycle point.
    ///
    /// `Verdict` comes back only for `pre-tool`; the notification points return
    /// `Verdict::Proceed` so a caller cannot accidentally give a `post-tool` hook a
    /// vote by reading its return value.
    pub fn call_hook(
        &self,
        hook: &WasmHook,
        call: HookCall<'_>,
        limits: T1Limits,
    ) -> Result<Verdict, T1Error> {
        let mut linker: Linker<HostState> = Linker::new(self.engine());
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| T1Error::Invalid(format!("link wasi: {e}")))?;
        Hook::add_to_linker::<HostState, wasmtime::component::HasSelf<HostState>>(
            &mut linker,
            |s| s,
        )
        .map_err(|e| T1Error::Invalid(format!("link host: {e}")))?;

        let mut store = self.store(limits)?;
        let instance = Hook::instantiate(&mut store, &hook.component, &linker)
            .map_err(|e| classify(e, &store, limits))?;

        let result = match call {
            HookCall::PreTool { tool, args } => instance.call_pre_tool(&mut store, tool, args),
            HookCall::PostTool { tool, output } => instance
                .call_post_tool(&mut store, tool, output)
                .map(|()| Verdict::Proceed),
            HookCall::OnStop { reason } => instance
                .call_on_stop(&mut store, reason)
                .map(|()| Verdict::Proceed),
        };
        result.map_err(|e| classify(e, &store, limits))
    }
}

/// Keys whose values never reach a hook.
///
/// A denylist of *key shapes* rather than a scan for secret-looking values: a
/// value-based filter has to guess (and a token that does not match the guess is
/// the one that leaks), while a key-based one is decidable from the schema the
/// plugin already declared. docs/16 puts secrets behind an explicit `secrets:`
/// grant, so a hook seeing one in tool arguments is always an accident.
const SECRET_KEY_MARKERS: &[&str] = &[
    "secret",
    "token",
    "password",
    "passwd",
    "api_key",
    "apikey",
    "key",
    "credential",
    "auth",
    "session_id",
    "cookie",
    "bearer",
];

/// Redact secret-shaped values from a JSON object before a hook sees it.
///
/// Structure is preserved — a hook that filters on `cmd` still works — and every
/// redacted value is replaced with a fixed marker rather than removed, so a hook
/// can tell "there was a token here" from "there was no token", which is a
/// distinction a DLP filter needs.
pub fn redact(args: &serde_json::Value) -> serde_json::Value {
    const MARKER: &str = "[redacted]";
    match args {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let lower = k.to_lowercase();
                    if SECRET_KEY_MARKERS.iter().any(|m| lower.contains(m)) {
                        (k.clone(), serde_json::Value::String(MARKER.into()))
                    } else {
                        (k.clone(), redact(v))
                    }
                })
                .collect(),
        ),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(redact).collect())
        }
        other => other.clone(),
    }
}
