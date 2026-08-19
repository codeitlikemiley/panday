//! A plugin tool that does exactly what a T1 guest can, and tries several things
//! it cannot — so one component covers the whole escape suite.
//!
//! Dispatch is on `op` in the JSON arguments:
//!
//! - `echo`     — returns its input. The "a demo plugin tool runs" case.
//! - `log`      — calls the one granted import.
//! - `spin`     — never returns. Fuel and epoch limits must stop it.
//! - `alloc`    — allocates until it is refused. Memory limit.
//! - `read`     — tries the filesystem, which the world does not grant.
//! - `env`      — tries the environment, which the host does not populate.
//! - `panic`    — traps, to prove a guest panic is a tool error and not ours.

wit_bindgen::generate!({
    world: "tool",
    path: "../../crates/panday-sandbox/wit",
});

struct Component;

fn field(args: &str, key: &str) -> Option<String> {
    // A hand-rolled scan rather than serde_json: this fixture exists to be
    // small, and pulling a JSON parser into it would put most of the wasm binary
    // outside the thing under test.
    let needle = format!("\"{key}\"");
    let start = args.find(&needle)? + needle.len();
    let rest = &args[start..];
    let colon = rest.find(':')? + 1;
    let rest = rest[colon..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

impl Guest for Component {
    fn run(args: String) -> Result<String, String> {
        let op = field(&args, "op").unwrap_or_else(|| "echo".into());
        match op.as_str() {
            "echo" => Ok(format!(
                "{{\"echo\":\"{}\"}}",
                field(&args, "text").unwrap_or_default()
            )),
            "log" => {
                panday::plugin::host::log("info", "demo tool ran");
                Ok("{\"logged\":true}".into())
            }
            "spin" => {
                // Truly unbounded, and `black_box`ed so it stays that way. The
                // first version counted to `u64::MAX` and returned; LLVM proved
                // that terminates and folded the whole loop away, so the fuel and
                // deadline tests passed with `Ok("{}")` — a limit test that never
                // reached the limit.
                let mut n: u64 = 0;
                loop {
                    n = n.wrapping_add(std::hint::black_box(1));
                    std::hint::black_box(n);
                }
            }
            "alloc" => {
                let mut held: Vec<Vec<u8>> = Vec::new();
                loop {
                    held.push(vec![7u8; 1 << 20]);
                    if held.len() > 4096 {
                        return Ok(format!("{{\"mib\":{}}}", held.len()));
                    }
                }
            }
            "read" => match std::fs::read_to_string("/etc/passwd") {
                Ok(s) => Ok(format!("{{\"read\":{}}}", s.len())),
                Err(e) => Err(format!("read refused: {e}")),
            },
            "env" => Ok(format!("{{\"env_vars\":{}}}", std::env::vars().count())),
            "panic" => panic!("the guest panicked on purpose"),
            other => Err(format!("unknown op: {other}")),
        }
    }
}

export!(Component);
