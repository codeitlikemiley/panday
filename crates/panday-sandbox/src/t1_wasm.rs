//! T1 — wasmtime component tier for plugin tools and hooks (M14.4, docs/14 §tiers).
//!
//! > "**T1** | wasmtime component (WASI 0.3) | plugin-provided tools & hooks |
//! > ~ms | capability: only WIT imports we grant"
//!
//! ## Why T1 is not a `Sandbox`
//!
//! The `Sandbox` trait models a session you exec commands in — create, exec, put,
//! get, destroy. A T1 guest has no filesystem to put a file into and no process to
//! exec; its unit of work is a function call. Implementing the trait would mean
//! four methods returning `Unsupported` and one that lies about what `exec` means,
//! so the tier gets its own type and `SandboxTier::T1Wasm` stays the label the
//! permission engine and the metering use.
//!
//! ## The capability story, and the part std forces
//!
//! `wit/tool.wit` is the whole world: one import (`host.log`), one export (`run`).
//! But a Rust guest links WASI into its binary whether it uses it or not — the
//! demo component imports `wasi:filesystem`, `wasi:cli/environment` and friends
//! purely by having `std`. Refusing to link those would fail instantiation on
//! every real guest, so they are linked with an **empty** `WasiCtx`: no preopens,
//! no environment, no stdio, no sockets. The guest can call `wasi:filesystem` and
//! find nothing to open, which is the same guarantee by a different route — and it
//! is tested, because "we didn't grant it" is a claim about behaviour, not
//! configuration.
//!
//! ## Two limits, because they stop different things
//!
//! **Fuel** counts instructions, so it bounds work deterministically: the same
//! guest on the same input always stops at the same place, which is what makes a
//! reproducible test possible at all. **Epoch interruption** bounds wall-clock,
//! which is what an operator actually cares about, and it is the only one that can
//! stop a guest blocked in a host call rather than burning instructions. A tier
//! with only fuel lets a slow host import hang a turn; a tier with only epochs
//! makes every limit test a race against a timer.

use crate::SandboxTier;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    world: "tool",
    path: "wit",
});

/// What a plugin tool is allowed to consume.
#[derive(Debug, Clone, Copy)]
pub struct T1Limits {
    /// Instructions. ~200M is a few hundred milliseconds of real work; a linter
    /// that needs more is not a plugin tool.
    pub fuel: u64,
    /// Wall-clock ceiling. docs/16 gives hooks 10ms; a tool is allowed longer
    /// because it is doing the work rather than deciding about it.
    pub wall: Duration,
    /// Linear-memory ceiling, bytes.
    pub memory: usize,
}

impl Default for T1Limits {
    fn default() -> Self {
        Self {
            fuel: 200_000_000,
            wall: Duration::from_secs(2),
            memory: 64 * 1024 * 1024,
        }
    }
}

impl T1Limits {
    /// docs/16 §hooks: "Fuel-metered, epoch-interrupted, 10ms default budget".
    pub fn hook() -> Self {
        Self {
            fuel: 10_000_000,
            wall: Duration::from_millis(10),
            memory: 16 * 1024 * 1024,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum T1Error {
    #[error("plugin tool exceeded its instruction budget ({0} fuel)")]
    OutOfFuel(u64),
    #[error("plugin tool exceeded its {0:?} wall-clock budget")]
    Deadline(Duration),
    #[error("plugin tool trapped: {0}")]
    Trap(String),
    #[error("plugin tool returned an error: {0}")]
    ToolError(String),
    /// An import the world does not grant. This is the tier working, so it is its
    /// own variant rather than a `Trap`: an operator seeing this should read
    /// "the plugin asked for something it was not given", not "wasm broke".
    #[error("plugin requires a capability this world does not grant: {0}")]
    CapabilityNotGranted(String),
    #[error("not a valid wasm component: {0}")]
    Invalid(String),
}

/// Log lines the guest emitted, in order.
///
/// Collected rather than written straight to `tracing` so the caller decides
/// where an untrusted string goes — docs/21's scrub rules apply to plugin output
/// as much as to ours, and a guest that logged 10k lines should not be able to
/// flood our log through a host import.
#[derive(Debug, Default, Clone)]
pub struct GuestLogs(pub Vec<(String, String)>);

const MAX_GUEST_LOGS: usize = 64;

struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
    logs: Vec<(String, String)>,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl panday::plugin::host::Host for HostState {
    fn log(&mut self, level: String, message: String) {
        if self.logs.len() < MAX_GUEST_LOGS {
            // Truncated: a host import is an unbounded channel from untrusted
            // code into our process, and the only safe unbounded thing is
            // nothing.
            self.logs.push((
                level.chars().take(16).collect(),
                message.chars().take(1_000).collect(),
            ));
        }
    }
}

/// A compiled plugin tool. Compilation is the expensive part, so it is done once
/// and the component is shared; each call gets a fresh store, which is what makes
/// calls independent (a guest cannot leave state behind for the next caller).
pub struct WasmTool {
    component: Component,
    name: String,
}

impl WasmTool {
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl std::fmt::Debug for WasmTool {
    // A compiled component has no useful debug form; the name is what a caller
    // needs when a `Result<WasmTool, _>` is unwrapped in a test.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmTool")
            .field("name", &self.name)
            .finish()
    }
}

/// The T1 runtime: one engine, many tools.
pub struct T1Runtime {
    engine: Engine,
    /// Ticks the epoch clock. Held for the runtime's life; dropping it stops the
    /// deadline enforcement, which is why it is not a detached thread.
    _ticker: Arc<Ticker>,
}

struct Ticker {
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Epoch tick. Every deadline is expressed in whole ticks, so it also sets the
/// granularity of `T1Limits::wall` — 1ms, which is finer than any budget docs/16
/// gives (10ms for hooks) and coarse enough that the ticker thread costs nothing.
const TICK: Duration = Duration::from_millis(1);

impl T1Runtime {
    pub fn new() -> Result<Self, T1Error> {
        let mut config = wasmtime::Config::new();
        config
            .consume_fuel(true)
            .epoch_interruption(true)
            // Untrusted code, so the strictest settings the tier can afford.
            .wasm_component_model(true);
        let engine = Engine::new(&config).map_err(|e| T1Error::Invalid(e.to_string()))?;

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ticker = Arc::new(Ticker { stop: stop.clone() });
        {
            let engine = engine.clone();
            std::thread::Builder::new()
                .name("panday-t1-epoch".into())
                .spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        std::thread::sleep(TICK);
                        engine.increment_epoch();
                    }
                })
                .map_err(|e| T1Error::Invalid(format!("epoch ticker: {e}")))?;
        }

        Ok(Self {
            engine,
            _ticker: ticker,
        })
    }

    pub fn tier(&self) -> SandboxTier {
        SandboxTier::T1Wasm
    }

    /// Compile a component. Rejects core modules and anything malformed *here*,
    /// at install time, rather than on the first call in front of a user.
    pub fn compile(&self, name: &str, bytes: &[u8]) -> Result<WasmTool, T1Error> {
        let component = Component::from_binary(&self.engine, bytes)
            .map_err(|e| T1Error::Invalid(format!("{name}: {e}")))?;
        Ok(WasmTool {
            component,
            name: name.to_string(),
        })
    }

    pub fn compile_file(&self, name: &str, path: impl AsRef<Path>) -> Result<WasmTool, T1Error> {
        let bytes = std::fs::read(path.as_ref())
            .map_err(|e| T1Error::Invalid(format!("{}: {e}", path.as_ref().display())))?;
        self.compile(name, &bytes)
    }

    /// Run a tool. Synchronous by design: wasmtime's sync API cannot be
    /// re-entered from a host call, so there is no way for a guest to reach back
    /// into the loop. The harness calls this on a blocking pool.
    pub fn call(
        &self,
        tool: &WasmTool,
        args: &str,
        limits: T1Limits,
    ) -> Result<(String, GuestLogs), T1Error> {
        let mut linker: Linker<HostState> = Linker::new(&self.engine);
        // WASI with nothing granted — see the module note on what std forces.
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| T1Error::Invalid(format!("link wasi: {e}")))?;
        Tool::add_to_linker::<HostState, wasmtime::component::HasSelf<HostState>>(
            &mut linker,
            |s| s,
        )
        .map_err(|e| T1Error::Invalid(format!("link host: {e}")))?;

        let mut store = Store::new(
            &self.engine,
            HostState {
                // No preopens, no env, no stdio, no sockets. The builder's
                // default is already deny-everything; it is spelled out because
                // a future edit that adds `.inherit_stdio()` for debugging should
                // read as the security change it is.
                wasi: WasiCtxBuilder::new().build(),
                table: ResourceTable::new(),
                limits: StoreLimitsBuilder::new()
                    .memory_size(limits.memory)
                    // A *component* is several core instances — the guest module
                    // plus the WASI adapter — so the cap is not "one". These are
                    // set to catch a guest that instantiates in a loop, not to
                    // describe the shape of a normal one; the first draft used 1
                    // and every real component failed to instantiate.
                    .instances(64)
                    .memories(16)
                    .tables(16)
                    .build(),
                logs: Vec::new(),
            },
        );
        store.limiter(|s| &mut s.limits);
        store
            .set_fuel(limits.fuel)
            .map_err(|e| T1Error::Invalid(e.to_string()))?;

        // Ticks, not milliseconds: `increment_epoch` fires every TICK.
        let ticks = (limits.wall.as_millis() / TICK.as_millis().max(1)).max(1) as u64;
        store.set_epoch_deadline(ticks);

        let instance = Tool::instantiate(&mut store, &tool.component, &linker)
            .map_err(|e| classify(e, &store, limits))?;

        let result = instance.call_run(&mut store, args);
        let logs = GuestLogs(std::mem::take(&mut store.data_mut().logs));

        match result {
            Ok(Ok(output)) => Ok((output, logs)),
            Ok(Err(message)) => Err(T1Error::ToolError(message)),
            Err(e) => Err(classify(e, &store, limits)),
        }
    }
}

/// Turn a wasmtime error into the reason an operator needs.
///
/// The distinction that matters is between "the guest hit a limit" and "the guest
/// asked for something it was not granted": the first is a plugin to tune, the
/// second is a plugin to reject, and one `Trap(String)` for both would make that
/// call a grep through error text.
fn classify(e: wasmtime::Error, store: &Store<HostState>, limits: T1Limits) -> T1Error {
    let text = format!("{e:#}");
    if e.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::OutOfFuel)
        || text.contains("all fuel consumed")
    {
        return T1Error::OutOfFuel(limits.fuel);
    }
    if e.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::Interrupt)
        || text.contains("epoch deadline")
    {
        return T1Error::Deadline(limits.wall);
    }
    // Instantiation failures name the missing import.
    if text.contains("import") && (text.contains("not found") || text.contains("unknown")) {
        return T1Error::CapabilityNotGranted(text);
    }
    let _ = store;
    T1Error::Trap(text)
}
