# 14 — ferrum-sandbox

Four different trust problems wear the same trait. Tier is chosen per tool
call by `ToolReq.sandbox_tier` × deployment shape (ADR-004).

## The trait

```rust
#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn create(&self, spec: SessionSpec) -> Result<SandboxHandle>;
    async fn exec(&self, h: &SandboxHandle, cmd: ExecSpec) -> Result<ExecStream>; // pty or pipes
    async fn put(&self, h: &SandboxHandle, path: &Path, data: ByteStream) -> Result<()>;
    async fn get(&self, h: &SandboxHandle, path: &Path) -> Result<ByteStream>;
    async fn snapshot(&self, h: &SandboxHandle) -> Result<SnapshotRef>;   // T3 only (Err(Unsupported) elsewhere)
    async fn destroy(&self, h: SandboxHandle) -> Result<()>;
}
```

`ExecStream` yields stdout/stderr chunks + exit status; the reducer taps this
stream *before* the harness folds it into context.

## Tiers

| Tier | Mechanism | For | Startup | Isolation |
|---|---|---|---|---|
| **T0** | in-process Rust fn | our native pure tools (read/grep/glob) | 0 | none needed (no exec, path-policy enforced in code) |
| **T1** | wasmtime component (WASI 0.3) | plugin-provided tools & hooks | ~ms | capability: only WIT imports we grant |
| **T2** | OS jail: bubblewrap-style namespaces + seccomp (Linux), Seatbelt profile (macOS) | user's own shell/tools on THEIR machine | ~10ms | FS scoping + egress deny; user is the trust anchor |
| **T3** | Firecracker microVM | strangers' code on OUR cloud | ~125ms cold, ~snapshot-warm | hardware virtualization |

Mechanism notes, from validated prior art:

- **T2 is Anthropic's sandbox-runtime design, reimplemented in Rust** (theirs
  is TypeScript): Linux = bubblewrap + seccomp BPF; macOS = `sandbox-exec`
  Seatbelt profiles; network = default-deny with a local HTTP/SOCKS5 proxy
  enforcing a domain allowlist. We implement the same shape natively —
  `bwrap` vendored/invoked or direct `clone3`+namespaces, proxy in-process.
- **T3 drives Firecracker's REST-over-UDS API directly.** The community Rust
  SDK situation is thin (fctools "semi-stable", others stale) — the API is
  small and stable; we own a 500-line client. Rootfs = overlay over a golden
  image per toolchain; jailer + cgroup v2 caps (cpu, mem, pids, io);
  snapshot/restore gives warm-start pools and *session resume with running
  processes* — a product feature, not just an optimization.
- **KVM required for T3** → cloud pools are Linux/metal-or-nested-virt. macOS
  users get T2 locally; cloud execution is always Linux.

## Policy (uniform across tiers)

```rust
pub struct SandboxPolicy {
    pub fs:      FsPolicy      { workspace_rw, staged_ro, deny_all_else },
    pub net:     NetPolicy     { default_deny, allow: Vec<DomainPattern>, via_proxy: true },
    pub limits:  Limits        { cpu_ms, mem_bytes, pids, disk_bytes, wall_clock },
    pub secrets: SecretPolicy  { none_by_default },  // env is scrubbed; injection is explicit
}
```

The egress proxy is one Rust component reused by T1/T2/T3; every allowed
request is logged with (session, tool, domain, bytes) — exfiltration attempts
are *visible*, and package-registry access is an allowlist entry, not a hole.

## Workspace lifecycle

Per session: an ephemeral workspace volume (T2: a temp dir bind; T3: an ext4
sparse file), staged inputs mounted RO, results pulled via `get` before
destroy. `snapshot` on idle (T3) → restore on next message; a resumed session
finds its `node_modules` warm.

## The escape suite (CI, per tier)

Must-fail tests: read `/etc/shadow` · write outside workspace · connect to
non-allowlisted IP + DNS-rebind attempt · exceed mem (OOM-killed, harness gets
typed error) · fork bomb (pids cap) · clock-burn (wall-clock kill) · T1:
import not granted in WIT world · T3: /proc surface minimal. Suite runs in CI
on every sandbox PR — an isolation regression is a broken build, same as a
type error.

## Milestones

- **M14.1** Trait ✅ + T0 (path-policied native tools) + policy types; unit-tested FS scoping.
- **M14.2** T2 Linux: namespaces + seccomp + egress proxy; escape suite green; `bash` tool runs through it.
- **M14.3** T2 macOS via Seatbelt profile generation; parity subset of escape suite.
- **M14.4** T1 wasmtime: WIT world for plugin tools (`ferrum:plugin/tool`), fuel + epoch limits; a demo plugin tool runs.
- **M14.5** T3 Firecracker client (UDS REST) + golden rootfs build + jailer; cold exec under 300ms p95.
- **M14.6** T3 snapshot/restore pools; warm exec under 50ms p95; session-resume-with-state demo.
- **M14.7** sandbox-seconds metering events → ledger (17).
