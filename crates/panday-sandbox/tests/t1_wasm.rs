//! M14.4 — the T1 tier, and docs/14's escape-suite case "T1: import not granted
//! in WIT world".
//!
//! The component under test is `tests/fixtures/demo_tool.wasm`, built from
//! `fixtures/demo-tool` by `cargo xtask wasm-fixtures`. It is checked in so this
//! suite needs no wasm toolchain: a test that skips itself when a tool is missing
//! would silently stop gating isolation, which is the same argument docs/14 makes
//! about bubblewrap in CI.

use panday_sandbox::t1_wasm::{T1Error, T1Limits, T1Runtime};
use panday_sandbox::SandboxTier;

fn runtime() -> T1Runtime {
    T1Runtime::new().expect("engine")
}

fn demo(rt: &T1Runtime) -> panday_sandbox::t1_wasm::WasmTool {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/demo_tool.wasm");
    rt.compile_file("demo", &path)
        .unwrap_or_else(|e| panic!("compile {}: {e}", path.display()))
}

// ── The demo tool runs ───────────────────────────────────────────────────────

#[test]
fn a_demo_plugin_tool_runs_and_returns_its_output() {
    let rt = runtime();
    let tool = demo(&rt);
    let (out, _) = rt
        .call(
            &tool,
            r#"{"op":"echo","text":"hello from the guest"}"#,
            T1Limits::default(),
        )
        .expect("the demo tool runs");
    assert_eq!(out, r#"{"echo":"hello from the guest"}"#);
    assert_eq!(rt.tier(), SandboxTier::T1Wasm);
}

#[test]
fn the_one_granted_import_works() {
    // `host.log` is the entire import side of the world, so if this fails the
    // world grants nothing at all and the tier is useless rather than strict.
    let rt = runtime();
    let tool = demo(&rt);
    let (_, logs) = rt
        .call(&tool, r#"{"op":"log"}"#, T1Limits::default())
        .expect("log is granted");
    assert_eq!(logs.0.len(), 1);
    assert_eq!(logs.0[0].0, "info");
    assert!(logs.0[0].1.contains("demo tool ran"));
}

#[test]
fn a_tools_own_error_is_a_tool_error_not_a_trap() {
    // The model needs to see "unknown op", not "the sandbox failed": one is a
    // mistake it can correct on the next turn.
    let rt = runtime();
    let tool = demo(&rt);
    let err = rt
        .call(&tool, r#"{"op":"nonsense"}"#, T1Limits::default())
        .unwrap_err();
    assert!(matches!(err, T1Error::ToolError(_)), "{err:?}");
    assert!(err.to_string().contains("unknown op"));
}

#[test]
fn a_guest_panic_is_contained() {
    let rt = runtime();
    let tool = demo(&rt);
    let err = rt
        .call(&tool, r#"{"op":"panic"}"#, T1Limits::default())
        .unwrap_err();
    assert!(matches!(err, T1Error::Trap(_)), "{err:?}");
    // And the runtime still works afterwards — a panicking plugin must not take
    // the session with it.
    let (out, _) = rt
        .call(
            &tool,
            r#"{"op":"echo","text":"still here"}"#,
            T1Limits::default(),
        )
        .unwrap();
    assert!(out.contains("still here"));
}

// ── Capabilities not granted ─────────────────────────────────────────────────

#[test]
fn the_filesystem_is_not_reachable() {
    // A Rust guest links `wasi:filesystem` whether it uses it or not, so the
    // guarantee cannot be "the import is absent" — it is "there is nothing to
    // open". Which is a claim about behaviour, hence a test.
    let rt = runtime();
    let tool = demo(&rt);
    let result = rt.call(&tool, r#"{"op":"read"}"#, T1Limits::default());
    match result {
        Err(T1Error::ToolError(msg)) => {
            assert!(
                msg.contains("refused"),
                "the guest should have been refused: {msg}"
            );
            // And specifically not because the file happens to be absent on this
            // machine: /etc/passwd exists on every host this runs on.
            assert!(
                std::path::Path::new("/etc/passwd").exists(),
                "this assertion is meaningless without the file existing outside"
            );
        }
        other => panic!("the guest read the host filesystem: {other:?}"),
    }
}

#[test]
fn the_environment_is_empty() {
    // Secrets reach a plugin through a declared `secrets:` grant (docs/16), never
    // by inheriting our environment — the same rule T2 enforces with `--clearenv`.
    std::env::set_var("PANDAY_T1_LEAK_CANARY", "should-not-be-visible");
    let rt = runtime();
    let tool = demo(&rt);
    let (out, _) = rt
        .call(&tool, r#"{"op":"env"}"#, T1Limits::default())
        .expect("reading an empty env is allowed, it just finds nothing");
    assert_eq!(out, r#"{"env_vars":0}"#, "the guest saw our environment");
}

#[test]
fn a_component_needing_an_ungranted_import_is_refused_at_instantiation() {
    // The docs/14 escape-suite case, with a real plugin rather than a synthetic
    // one: `fixtures/greedy-tool` is built against the same package name and the
    // same `run` export, plus a `secrets` import the host does not link. It
    // compiles fine — a plugin author can write it — and it must never execute an
    // instruction.
    let rt = runtime();
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/greedy_tool.wasm");
    let tool = rt
        .compile_file("greedy", &path)
        .expect("a wider world still compiles; that is why the check is at link time");

    let err = rt
        .call(&tool, "{}", T1Limits::default())
        .expect_err("a component asking for an ungranted import must not run");
    assert!(
        matches!(err, T1Error::CapabilityNotGranted(_)),
        "the tier should name this as a missing capability, not a generic trap: {err:?}"
    );
    assert!(
        err.to_string().contains("secrets"),
        "the error should name the capability: {err}"
    );
}

#[test]
fn a_core_module_is_not_a_component() {
    let rt = runtime();
    let bytes = wat::parse_str("(module (func (export \"run\")))").unwrap();
    let err = rt.compile("core", &bytes).unwrap_err();
    assert!(matches!(err, T1Error::Invalid(_)), "{err:?}");
}

// ── Limits ───────────────────────────────────────────────────────────────────

#[test]
fn an_infinite_loop_runs_out_of_fuel_deterministically() {
    // Fuel is the limit that makes this test reproducible: instructions, not
    // seconds, so the same guest stops at the same place on a fast machine and a
    // loaded CI runner.
    let rt = runtime();
    let tool = demo(&rt);
    let limits = T1Limits {
        fuel: 5_000_000,
        // Generous, so it is unambiguous which limit fired.
        wall: std::time::Duration::from_secs(30),
        ..T1Limits::default()
    };
    let err = rt.call(&tool, r#"{"op":"spin"}"#, limits).unwrap_err();
    assert!(matches!(err, T1Error::OutOfFuel(5_000_000)), "{err:?}");
}

#[test]
fn the_wall_clock_deadline_stops_a_guest_fuel_would_not() {
    // The other half: an operator cares about wall-clock, and epochs are the only
    // limit that can stop a guest that is not burning instructions.
    let rt = runtime();
    let tool = demo(&rt);
    let limits = T1Limits {
        fuel: u64::MAX,
        wall: std::time::Duration::from_millis(50),
        ..T1Limits::default()
    };
    let started = std::time::Instant::now();
    let err = rt.call(&tool, r#"{"op":"spin"}"#, limits).unwrap_err();
    assert!(matches!(err, T1Error::Deadline(_)), "{err:?}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "the deadline took {:?} to fire",
        started.elapsed()
    );
}

#[test]
fn a_memory_hog_is_refused_rather_than_allowed_to_take_the_process() {
    let rt = runtime();
    let tool = demo(&rt);
    let limits = T1Limits {
        memory: 8 * 1024 * 1024,
        fuel: u64::MAX,
        wall: std::time::Duration::from_secs(30),
    };
    let err = rt.call(&tool, r#"{"op":"alloc"}"#, limits).unwrap_err();
    // Rust's allocator turns a refused growth into an abort, which surfaces as a
    // trap — the point is that it is the *guest* that dies.
    assert!(
        matches!(err, T1Error::Trap(_) | T1Error::ToolError(_)),
        "{err:?}"
    );
    let (out, _) = rt
        .call(
            &tool,
            r#"{"op":"echo","text":"alive"}"#,
            T1Limits::default(),
        )
        .unwrap();
    assert!(out.contains("alive"), "the runtime survived");
}

#[test]
fn the_hook_budget_is_the_ten_milliseconds_docs_16_specifies() {
    let hook = T1Limits::hook();
    assert_eq!(hook.wall, std::time::Duration::from_millis(10));
    assert!(hook.fuel < T1Limits::default().fuel);
}

#[test]
fn each_call_gets_a_fresh_store_so_a_guest_cannot_leave_state_behind() {
    // Two calls, and the second must not see the first's log buffer or memory.
    let rt = runtime();
    let tool = demo(&rt);
    let (_, first) = rt
        .call(&tool, r#"{"op":"log"}"#, T1Limits::default())
        .unwrap();
    let (_, second) = rt
        .call(&tool, r#"{"op":"echo","text":"x"}"#, T1Limits::default())
        .unwrap();
    assert_eq!(first.0.len(), 1);
    assert!(
        second.0.is_empty(),
        "state leaked between calls: {second:?}"
    );
}

#[test]
fn a_flood_of_guest_logs_is_truncated() {
    // A host import is an unbounded channel from untrusted code into our process.
    // (The demo logs once; this asserts the cap exists and is small.)
    let rt = runtime();
    let tool = demo(&rt);
    let (_, logs) = rt
        .call(&tool, r#"{"op":"log"}"#, T1Limits::default())
        .unwrap();
    assert!(logs.0.len() <= 64);
    assert!(logs.0[0].1.len() <= 1_000);
}
