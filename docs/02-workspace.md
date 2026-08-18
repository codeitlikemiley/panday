# 02 — Workspace & Toolchain

## Monorepo layout

```
ai-infra/
├── Cargo.toml              # [workspace] — one lockfile, one clippy config
├── rust-toolchain.toml     # pinned stable
├── docs/                   # this documentation set (mdBook)
├── crates/
│   ├── panday-types/       # THE shared vocabulary: events, model IR, ids, errors
│   ├── panday-sdk/         # client library: providers, middleware, agent builder
│   ├── panday-gateway/     # bin+lib: the model-plane service
│   ├── panday-router/      # lib: routing policy engine (used by gateway)
│   ├── panday-harness/     # lib: session actor, turn loop, tools, permissions
│   ├── panday-harnessd/    # bin: the hosted session service (AEP WS endpoint)
│   ├── panday-sandbox/     # lib+bin: tiered execution (spawns sandboxd)
│   ├── panday-reducer/     # lib: token-economy strategies
│   ├── panday-plugins/     # lib: manifests, skills, MCP host, hooks
│   ├── panday-platform/    # bin: auth, orgs, billing, public REST API
│   ├── panday-local/       # bin: the offline single-binary composition
│   └── panday-cli/         # bin: the terminal client (ratatui) + ACP server
├── proto/                  # JSON Schemas for the event protocol (generated from types)
├── xtask/                  # repo automation: `cargo xtask schemas` (M3.2)
├── training/               # Python: the training pipeline (uv project) — see 19
└── deploy/                 # compose files, infra configs, k8s later
```

Rules that keep a workspace this size sane:

- **`panday-types` has near-zero dependencies** (serde, thiserror, uuid, time).
  Everything depends on it; it depends on nothing of ours. Breaking it is a
  platform-wide event — treat its PRs accordingly.
- **Libraries don't know about wire formats or databases.** `panday-harness`
  takes traits (`ModelClient`, `Sandbox`, `EventSink`); binaries wire them to
  HTTP/PG. This is what makes `panday-local` possible — same harness, different
  wiring.
- **One binary per service, `lib.rs` + thin `main.rs`** so every service is
  testable in-process.
- **No `unsafe` outside `panday-sandbox`** (which needs syscall work), and
  there it is reviewed line-by-line.

## Dependency policy (the boring, load-bearing choices)

| Concern | Crate | Note |
|---|---|---|
| async runtime | tokio | full features in bins, minimal in libs |
| HTTP server | axum | + tower middleware everywhere |
| HTTP client | reqwest (rustls) | no openssl linkage |
| serialization | serde / serde_json | events are JSON on the wire v1 |
| errors | thiserror (libs), anyhow (bins) | |
| DB | sqlx (postgres, runtime-tokio, rustls) | compile-time checked queries |
| ids | uuid v7 | time-ordered; sortable in PG |
| time | time or chrono | pick ONE (we pick `time`), enforce with clippy |
| tracing | tracing + opentelemetry | span per event, see 21 |
| hashing | sha2 | content-addressed artifact refs; `docs/03` specifies sha256 |
| regex | regex | `grep` native tool, reducer error-line matching |
| config | serde_yaml_ng | YAML policy files (docs/12). Upstream `serde_yaml` is deprecated; this is its maintained continuation |
| schemas | schemars | derive JSON Schema for tool params; in `panday-types` it is **optional** behind the `schema` feature so the near-zero-dependency rule above still holds — only `cargo xtask schemas` enables it |
| MCP | rmcp (official) | client + server features |
| ACP | agent-client-protocol | official Rust crate |
| WASM | wasmtime | plugins tier, WASI 0.3 |
| TUI | ratatui + crossterm | |

Version-pin in the workspace `[workspace.dependencies]` table; crates inherit.
`cargo deny` in CI for licenses/advisories from day one — you are building a
commercial product; know your license graph early.

## Toolchain

- **Pinned stable** via `rust-toolchain.toml`; ratchet monthly, never mid-crunch.
- `cargo clippy --workspace --all-targets -- -D warnings` gates CI.
- `cargo fmt --check`, `cargo deny check`, `cargo nextest run` (faster, better
  output than `cargo test`).
- `cargo xtask` pattern for codegen (event-schema export, OpenAPI generation)
  instead of build.rs cleverness.

## CI shape (GitHub Actions or equivalent)

1. `fmt` + `clippy` + `deny` (fast fail)
2. `nextest` unit suite (no network, no docker) — must stay under 3 minutes
3. Integration lane: spins Postgres + MinIO via compose; runs `#[ignore]`d
   integration tests
4. `cargo build --release` for linux-x86_64 + aarch64 (cross), macOS on tag
5. Docs lane: `mdbook build docs` + link check — docs that don't build are
   broken builds

## Testing philosophy

- The event protocol gets **golden-file tests**: serialized fixtures checked
  in; any diff is a reviewed protocol change (see 03 versioning).
- The harness state machine is tested **without any model**: a scripted
  `ModelClient` fake drives loops deterministically.
- The reducer is tested on **recorded real outputs** (fixtures from cargo,
  git, pytest runs) with assertions on both token count AND information
  retention (the failure line must survive compression).
- Sandbox tiers get an **escape-attempt suite** that must fail: read
  /etc/shadow, connect to a non-allowlisted host, exceed memory, fork-bomb.

## Milestones

- **M2.1** Workspace compiles with all crates stubbed (✅ shipped: `cargo test --workspace` green, clippy clean); CI workflow file shipped — first green *run* happens on your remote.
- **M2.2** nextest + cargo-deny wired; golden-file harness for `panday-types` fixtures. ✅ *(shipped: `.config/nextest.toml`, `deny.toml`, `crates/panday-types/tests/golden.rs`; CI runs nextest + a cargo-deny lane.)*
- **M2.3** Integration lane with PG+MinIO compose; first sqlx query compiles against a real schema.
- **M2.4** Release builds for linux x86_64/aarch64 + macOS arm64; binaries under 25MB. ✅ *(shipped: `.github/workflows/release.yml`, `scripts/check-binary-sizes.sh`.)*

  Linux x86_64 and aarch64 build on every push; macOS is tag-only, per the CI
  shape above. aarch64 needs both a cross linker *and* a cross C compiler,
  because `ring` (via rustls) compiles C — `--target` alone is not enough.

  The 25MB budget is **enforced, not printed**: size creeps one dependency at a
  time and nobody notices until a release is 80MB. The script fails the build,
  and it also fails when it finds *no* binaries — a passing size check over
  zero files is not a pass. Current sizes (macOS arm64): `panday` 6.2MB,
  `panday-gateway` 6.9MB, `panday-harnessd` 2.1MB.

Acceptance for all: a fresh `git clone` + `cargo check` succeeds on stable
with no system deps beyond a C linker.
