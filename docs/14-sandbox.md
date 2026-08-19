# 14 — panday-sandbox

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

- **M14.1** Trait ✅ + T0 (path-policied native tools) + policy types; unit-tested FS scoping. ✅ *(shipped: `panday_sandbox::t0::T0Sandbox`; 17-case scoping suite in `crates/panday-sandbox/tests/t0_scoping.rs`.)*

  **What T0 does and does not defend against.** It constrains *our own* code
  acting on behalf of a model — a model asking for `../../etc/shadow` is
  stopped here. It is not a defence against hostile native code, which would
  never route through this API; that is T2/T3's job (ADR-004).

  Paths are canonicalised *before* the root check, which is what defeats the
  symlink bypass (a path textually inside the workspace whose link resolves
  outside). Both the file-symlink and directory-symlink cases are in the
  suite, each asserting the target really was reachable otherwise — an escape
  test that would pass against a no-op is worthless.

  Known, documented gap: a TOCTOU window remains between resolving a path and
  opening it. Closing it needs `openat2(RESOLVE_BENEATH)` or a real jail —
  M14.2.
- **M14.2** T2 Linux: namespaces + seccomp + egress proxy; escape suite green; `bash` tool runs through it. ✅ *(shipped: `panday_sandbox::t2_linux::T2LinuxSandbox` via bubblewrap; suite in `crates/panday-sandbox/tests/t2_linux_escape.rs`, gated to Linux and run by CI.)*

  **Reads here are a real allowlist**, which is the parity gap M14.3 recorded
  from the macOS side: nothing is visible inside the jail unless it was bound
  in, so `/etc/shadow` is not *denied* — it does not exist. Mount namespaces
  express what Seatbelt could not.

  | Guarantee | Mechanism | Status |
  |---|---|---|
  | No read outside the bound set | mount namespace | **strict** |
  | No write outside the workspace | only the workspace bound `rw` | **strict** |
  | No network egress | `--unshare-net` | **strict** |
  | Wall-clock ceiling | us (SIGKILL) | **strict** |
  | No parent env inherited | `--clearenv` | **strict** |
  | pid ceiling | pid namespace + `--die-with-parent` | *partial* |
  | Memory ceiling | needs cgroup v2 delegation | **not enforced** |

  `bwrap` is invoked rather than reimplemented — docs/14 lists it first, and
  it keeps `unsafe` out of the crate whose whole job is containment. **Verified
  in CI, not locally**: this was written on macOS, so ubuntu-latest is the only
  machine that has ever run the suite. CI installs bubblewrap explicitly,
  because a suite that skips itself when the tool is missing would silently
  stop gating isolation.

  The egress *proxy* (for a non-empty allowlist) is not built: with
  `--unshare-net` there is no network to filter, and default-deny is the
  stronger guarantee. A per-domain allowlist needs the proxy component and is
  deferred with it.
- **M14.3** T2 macOS via Seatbelt profile generation; parity subset of escape suite. ✅ *(shipped: `panday_sandbox::t2_macos` — `SeatbeltProfile`, `T2MacosSandbox`; 15-case suite in `crates/panday-sandbox/tests/t2_macos_escape.rs`.)*

  Taken **out of roadmap order**, ahead of M14.2: the roadmap assumes a Linux
  host, and a Linux escape suite cannot be run on the macOS machine this is
  being built on. docs/14 is explicit that the suite gates every sandbox
  change, so shipping the tier we can actually verify first is the honest
  order. M14.2 follows, verified in CI (ubuntu).

  **What this tier guarantees on macOS**, each covered by the suite:

  | Guarantee | Enforced by | Status |
  |---|---|---|
  | No write outside the workspace | Seatbelt `file-write*` allowlist | **strict** |
  | No network egress | Seatbelt `(deny network*)` | **strict** |
  | Wall-clock ceiling | us (SIGKILL at the deadline) | **strict** |
  | No parent env inherited | `env_clear()` | **strict** |
  | No read of sensitive paths | Seatbelt `file-read*` **deny**list | *partial* |
  | Memory / pid ceilings | — | **not enforced** |

  **Reads are a denylist here, unlike Linux.** A strict read allowlist is not
  achievable through Seatbelt in practice: the dynamic loader and shared cache
  touch paths that vary by macOS version and APFS firmlink layout. Every
  profile that enumerated top-level directories aborted `/bin/echo` with
  SIGABRT before `main`; only `(subpath "/")` — i.e. no scoping — reliably
  lets a binary start. So the tier allows broad reads and denies what matters
  (SSH/AWS/GPG keys, keychains, shell history, `/etc/master.passwd`). This is
  weaker than Linux T2's mount-namespace scoping and is stated rather than
  implied; read confinement is a T3 property.

  Every must-fail case is paired with a **positive control** proving the same
  operation succeeds unsandboxed — the network test skips itself when the host
  has no egress, because a denial proves nothing on an offline machine.
- **M14.4** T1 wasmtime: WIT world for plugin tools (`panday:plugin/tool`), fuel + epoch limits; a demo plugin tool runs. ✅ *(shipped: `crates/panday-sandbox/wit/tool.wit`, `panday_sandbox::t1_wasm`, 14-case suite in `crates/panday-sandbox/tests/t1_wasm.rs`, demo guests in `fixtures/{demo,greedy}-tool` built by `cargo xtask wasm-fixtures`.)*

  **T1 is not a `Sandbox`.** That trait models a session you exec commands in —
  create, exec, put, get, destroy. A T1 guest has no filesystem to put a file into
  and no process to exec; its unit of work is a function call. Implementing the
  trait would mean four `Unsupported` methods and one that lies about what `exec`
  means, so the tier has its own type and `SandboxTier::T1Wasm` stays the label the
  permission engine and the metering use.

  **The capability story, and the part `std` forces.** `wit/tool.wit` is the whole
  world: one import (`host.log`), one export (`run`). But a Rust guest links WASI
  into its binary whether it uses it or not — the demo component imports
  `wasi:filesystem`, `wasi:cli/environment` and eleven more purely by having `std`.
  Refusing to link those would fail instantiation on every real guest, so they are
  linked with an **empty** `WasiCtx`: no preopens, no environment, no stdio, no
  sockets. The guest can call `wasi:filesystem` and find nothing to open. That is
  the same guarantee by a different route, and it is a claim about behaviour rather
  than configuration, so the suite tests it — including a canary environment
  variable the guest must not see.

  | Guarantee | Mechanism | Status |
  |---|---|---|
  | Only granted imports reachable | WIT world + linker | **strict** (refused at instantiation) |
  | No filesystem | empty `WasiCtx` (no preopens) | **strict** |
  | No environment inherited | empty `WasiCtx` | **strict** |
  | No network egress | sockets never linked | **strict** |
  | Instruction ceiling | wasmtime fuel | **strict** |
  | Wall-clock ceiling | epoch interruption (1ms tick) | **strict** |
  | Memory ceiling | `StoreLimits` | **strict** |
  | No state across calls | fresh `Store` per call | **strict** |

  **Two limits, because they stop different things.** Fuel counts instructions, so
  it bounds work deterministically — the same guest stops at the same place on a
  fast laptop and a loaded CI runner, which is what makes a reproducible limit test
  possible. Epochs bound wall-clock, which is what an operator actually cares
  about, and are the only limit that can stop a guest blocked in a host call rather
  than burning instructions. Fuel alone lets a slow import hang a turn; epochs
  alone make every limit test a race against a timer.

  **The escape-suite case docs/14 names** ("T1: import not granted in WIT world")
  is a real plugin, not a synthetic one: `fixtures/greedy-tool` is built against
  the same package name with the same `run` export plus a `secrets` import the host
  does not link. It compiles fine — a plugin author can write it — and it is
  refused at instantiation, before it executes an instruction. The error is its own
  variant (`CapabilityNotGranted`) rather than a generic trap, because "the plugin
  asked for something it was not given" and "wasm broke" call for different
  responses.

  **Two bugs the tests caught.** The store limits started at `instances(1)` — a
  *component* is several core instances (guest module plus WASI adapter), so every
  real component failed to instantiate. And the demo guest's `spin` op counted to
  `u64::MAX` and returned: LLVM proved that terminates and folded the loop away, so
  the fuel and deadline tests both passed with `Ok("{}")` — limit tests that never
  reached a limit. It is `black_box`ed now.

  The components are **checked in** (`tests/fixtures/*.wasm`) and rebuilt by `cargo
  xtask wasm-fixtures`. A suite that needed `wasm32-wasip2` and a component
  toolchain installed would skip itself on most machines, and a sandbox suite that
  skips itself is a sandbox nobody is testing — the same argument docs/14 already
  makes about bubblewrap in CI. Cost of the dependency: wasmtime is a large build,
  which adds a few minutes to a cold CI compile.
- **M14.5** T3 Firecracker client (UDS REST) + golden rootfs build + jailer; cold exec under 300ms p95. ✅ *(shipped: `panday_sandbox::t3` — `api` (the client), `jailer` (argv + cgroup caps), the boot and snapshot sequences, `scripts/build-rootfs.sh` / `just rootfs`. **The p95 number is not measured** — see below.)*

  **The client is hand-rolled HTTP/1.1 over a Unix socket, as docs/14 chose.** Six endpoints, no
  chunked encoding, no keep-alive negotiation, JSON shapes pinned by Firecracker's own spec. One
  connection per request, which removes the entire class of bug where a half-read response
  desynchronises the next call — the bug a hand-rolled client would otherwise have. It half-closes
  after writing, which Firecracker does not need (it frames by `Content-Length`) and anything that
  frames by EOF does: leaving it out deadlocks against the second kind, which is exactly how the
  stub in the suite caught it.

  **Tested against a socket that answers like Firecracker.** That establishes what can be
  established without KVM: that we form the documented requests, in the order the VMM requires
  (configuration before `InstanceStart`, which it rejects afterwards), and read its answers —
  including `fault_message`, because "400" alone turns a typo in a drive path into an afternoon.

  **The jailer's arguments are the host-side security story**, so they are built by a function with
  tests rather than assembled in a string. uid or gid 0 is refused outright: a Firecracker escape
  has to land somewhere powerless, and running the VMM as root makes the chroot the only barrier —
  chroots are not a security boundary. A VM id that is a path traversal is refused for the same
  reason, since the id becomes a path component under the chroot base. Every cap docs/14 names is
  applied: without `pids` a fork bomb in the guest takes the host's process table, without `io` one
  sandbox's thrash is everyone's latency.

  **SMT off, dirty-page tracking on.** Two sandboxes sharing a core share a timing side channel,
  which is the entire reason a stranger's code is in a VM rather than a jail; dirty-page tracking is
  what makes M14.6's cross-host restore possible at all.

  **A snapshot pauses and stays paused.** Snapshotting a running VM and then resuming it produces
  two futures of one machine — same entropy, same connections, same clock — and the caller has to
  choose which continues.

  **Without `/dev/kvm` the tier refuses by name and does not degrade.** A sandbox tier that quietly
  became a weaker one would be the worst failure available here, since T3 exists precisely because
  T2 is not enough for a stranger's code.

  **What is not here, and why:** the measured cold-boot p95, and a guest that has run code. Both
  need KVM, and this tree is developed on macOS. The remaining pieces are the guest agent (a static
  `panday-guest` that is pid 1 in the VM) and the pool manager, which is M14.6 — the rootfs script
  already has the slot for the agent and says so rather than producing an image that boots to
  nothing.
- **M14.6** T3 snapshot/restore pools; warm exec under 50ms p95; session-resume-with-state demo. ✅ *(shipped: `panday_sandbox::t3::pool` — `WarmPool`, the `VmBackend` seam, and `FirecrackerBackend` behind it. **The 50ms number and the resume demo are not measured** — both need KVM.)*

  **Two rules the design follows, and neither is about speed.**

  - *A VM never serves two sessions.* On return it is destroyed and the pool refills from the golden
    snapshot. Reuse would be faster and would mean one stranger's code inherits another's memory,
    page cache and open descriptors — the thing T3 exists to prevent. `checkin` therefore takes the
    VM **by value**: there is no API for handing one back, because an API that made reuse possible
    would make it eventually happen.
  - *An empty pool is a slow request, never a failed one.* Under a burst it falls back to a cold
    boot rather than blocking on a refill. A queue would turn a traffic spike into a timeout for
    everybody instead of latency for the unlucky.

  **Restored paused, resumed on checkout.** A warm VM that started executing while it waited would
  drift from the snapshot every other VM in the pool was restored from — same entropy, same clock,
  now diverged in a way nothing observes.

  **A refill failure is counted, not propagated.** A pool that cannot refill still serves from cold
  boots; turning a transient backend hiccup into a failed *user request* would be worse than being
  slow, and `PoolStats::refill_failures` is what an operator alarms on before anyone notices the
  latency.

  **Tested against a fake backend**, which establishes what a pool actually gets wrong: handing the
  same VM to two callers (asserted under concurrency), leaking one on shutdown, wedging when the
  backend fails. What it cannot establish is how fast a real restore is — that is the 50ms clause,
  and it needs a KVM host.
- **M14.7** sandbox-seconds metering events → ledger (17). ✅ *(shipped: `panday_harness::SandboxUsageSink` + the call site in `execute()`, `panday_platform::ledger::SandboxLedger`; suite in `crates/panday-platform/tests/sandbox_ledger.rs`.)*

  **Priced per tier, because that is the only honest unit.** A T0 call is a Rust function in our own
  process and is billed at **zero** — charging for it would be charging for CPU already paid for in
  the request. T1 is a wasmtime instantiation, T2 a jailed process, T3 a slice of a machine we rent.
  One price for "a sandbox second" would either overcharge for T0 or give T3 away.

  **Billed per millisecond, not rounded up to a second.** A loop of forty 50ms tool calls would
  otherwise be charged forty seconds — not a rounding error but a different price. The arithmetic is
  a pure function on `TierPrices` so it is testable without a database; the first draft put it behind
  a method that needed a connection pool, and the test had to build one to check a multiplication.

  **A free execution is still recorded.** A ledger that omitted zero-cost work could not answer "what
  did this session do", which is the question a dispute starts from.

  **The call id is the idempotency unit.** docs/13's resume may re-run a replay-safe call; the work is
  the same work, so the bill is the same bill.

  The sink is a trait with a discarding default, for the same reason `UsageSink` is: `panday local`
  has no account, and a loop that needed a billing backend to run a tool would make the offline tier
  impossible.
