//! A policy hook, of the kind docs/13 and docs/20 describe: it vetoes a
//! destructive command, rewrites an unsafe one, and passes everything else.
//!
//! It also carries the misbehaviours the escape suite needs — a hook that spins
//! forever, and one that asks whether it can see a secret.

wit_bindgen::generate!({
    world: "hook",
    path: "wit",
});


struct Component;

impl Guest for Component {
    fn pre_tool(tool: String, args: String) -> Verdict {
        // A hook that never returns. The 10ms budget must stop it.
        if args.contains("\"spin\":true") {
            let mut n: u64 = 0;
            loop {
                n = n.wrapping_add(std::hint::black_box(1));
                std::hint::black_box(n);
            }
        }

        // What a real pre_tool filter pack does (docs/20 M20.2).
        if tool == "bash" && args.contains("rm -rf /") {
            return Verdict::Veto("refusing to delete the filesystem".into());
        }
        if tool == "bash" && args.contains("curl") {
            return Verdict::Rewrite("{\"cmd\":\"echo egress is not allowed\"}".into());
        }
        // Proves what the guest was actually handed: if a secret value reaches
        // here, redaction is not working, and the hook says so out loud rather
        // than silently.
        if args.contains("sk-live-") {
            return Verdict::Veto("a secret reached the hook".into());
        }
        Verdict::Proceed
    }

    fn post_tool(_tool: String, output: String) {
        panday::plugin::host::log("info", &format!("observed {} bytes", output.len()));
    }

    fn on_stop(reason: String) {
        panday::plugin::host::log("info", &format!("turn stopped: {reason}"));
    }
}

export!(Component);
