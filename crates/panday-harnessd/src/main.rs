//! panday-harnessd — the cloud session service (docs/22 shape 2).
//! Hosts session actors, serves the AEP WebSocket endpoint with
//! resume-after-seq (docs/03 M3.3). Session-actor affinity via consistent
//! hashing on session_id at the LB; failover = fold the log on another node.

fn main() {
    println!("panday-harnessd: see docs/03-protocol.md M3.3 and docs/13-harness.md");
}
