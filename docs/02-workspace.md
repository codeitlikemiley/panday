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
| config | toml | `plugin.toml` (docs/16); the spec names the file, so the format is product surface |
| signing | ed25519-dalek | `.plugin` archive signatures and registry verification (docs/16) |
| config | serde_yaml_ng | YAML policy files (docs/12). Upstream `serde_yaml` is deprecated; this is its maintained continuation |
| schemas | schemars | derive JSON Schema for tool params; in `panday-types` it is **optional** behind the `schema` feature so the near-zero-dependency rule above still holds — only `cargo xtask schemas` enables it |
| MCP | rmcp (official) | **client + stdio transport only** (M16.3); no socket transports, no OAuth. Pulls `chrono` transitively — allowed with `wrappers = ["rmcp"]` in `deny.toml`, banned everywhere else |
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

**The isolation suite is required nightly, not per-PR.** T2 Linux jails through `bubblewrap`, which
means `apt`, which means an Ubuntu mirror. One mirror stall hung a job for 3.5 hours (bounded and
retried since); then the Azure mirror went dark for an afternoon and every retry hit the same dead
host, failing every PR on this repo for reasons unrelated to the code. A mirror outage is not an
isolation regression, and blocking merges on one teaches people to ignore a red tick. So PR CI
installs best-effort and emits a **warning annotation** when it could not — including when the
runner's kernel refuses the user namespace bwrap is built on, which Ubuntu 24.04 does by default
(`kernel.apparmor_restrict_unprivileged_userns=1`; CI lifts it with sudo, and a deployment either
allows userns or uses T3) — while
`.github/workflows/nightly.yml` requires `bwrap` and fails without it — a regression is caught
within a day, and somebody else's outage is not our merge queue's problem.

**One test has its own timeout** (`.config/nextest.toml`): the `#[tool]` compile-fail
suite (docs/10 M10.4) builds a scratch crate, so on a cold cache it compiles the
dependency tree a second time. The default 2-minute kill ended the run before rustc
finished, which showed up as "(test timed out)" with nothing else failing. It is
overridden rather than `#[ignore]`d — a macro's error messages are its quality, and a
compile-fail suite that runs only when someone remembers stops matching the macro.


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
- **M2.3** Integration lane with PG+MinIO compose; first sqlx query compiles against a real schema. ✅ *(shipped: `deploy/integration-compose.yml`, `crates/panday-platform/migrations/0001_init.sql`, `panday_platform::pg`, `crates/panday-platform/tests/pg_integration.rs`, and the `integration` job in `ci.yml`.)*

  **Two bugs that only a real database could find**, which is the whole argument for this lane:

  - `CREATE TABLE IF NOT EXISTS` is **not atomic** against a concurrent create. Six tests each
    migrating on entry produced "duplicate key value violates unique constraint
    `pg_type_typname_nsp_index`" — and it is the same race a rolling deploy has when several pods
    boot at once, so the fix is a `pg_advisory_lock` around the migration rather than
    serialised tests.
  - `SUM(bigint)` is **NUMERIC**, not BIGINT. Reading the balance as `i64` failed with a type
    mismatch; the query casts now, and the cast is safe rather than convenient — i64
    micro-credits is ~9.2e12 dollars, so a balance that overflows it is a reconciliation problem
    long before it is a decoding problem.

  A third came from the tests themselves: the lane's database outlives a single `cargo test`, so
  a fixed `idempotency_key` is a test that passes exactly once. Every test now owns its account
  and its keys, which also means they run concurrently — and that concurrency *is* the
  tenant-scoping property under test, because a query that leaked across accounts would make
  them interfere and say so.

  **Migrations are files, not `sqlx::migrate!`.** The macro embeds them at compile time, so a
  schema change rebuilds everything that links the crate, and it hides the SQL from M20.3's
  tenant-scoping lint, which reads `.sql` files. The loop is ten lines and keeps both properties
  — and the lint duly passed on the first real SQL in the repo, which is what it was armed for.

  **MinIO is in the compose file and not in the CI job.** docs/02's step 3 names PG *and* MinIO, but no
  test reads a bucket yet — artifacts spill to memory (docs/15) — and a service container nothing uses
  is a failure mode with no benefit. It proved that immediately: the job went red because
  `bitnami/minio:latest` stopped existing, for a service the suite never connected to. It returns to CI
  with the first test that needs object storage.

  The compose file uses **non-default ports** (5433, 9100) and `tmpfs` for the data directory: a
  developer's own Postgres on 5432 is a coin flip between "the tests passed against the wrong
  database" and "the tests wiped something", and the lane's database is disposable by
  definition. CI uses service containers instead, because Actions health-checks them for free.

  Every test in the lane is `#[ignore]`d so the unit lane stays "no network, no docker"; the CI
  job selects them with `--run-ignored all -E 'binary(pg_integration)'` rather than a blanket
  `--ignored`, because the other ignored tests are a wall-clock benchmark and a live
  llama-server leg that cannot pass on a shared runner.
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
