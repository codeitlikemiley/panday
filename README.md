# Panday — a Rust AI platform, built from first principles

> **Panday** is Tagalog for *blacksmith*. The name is settled, not a placeholder
> (`docs/00-vision.md` §Naming): crates are prefixed `panday-`, the CLI binary is
> `panday`, and API keys are `pnd_live_` / `pnd_test_`.

This repository is the specification *and* the implementation of a full AI
infrastructure platform: an agent harness, an LLM gateway with a model router, a tiered
sandbox, a plugin system (skills + MCP + ACP), a token-economy layer, a
subscription platform with metered billing, an offline/local tier, and a
path to training your own task models.

**Two things live here:**

| Path | What it is |
|---|---|
| `docs/` | The documentation set — 21 specs, ADRs, threat model, roadmap. The source of truth. Renders with `mdbook serve docs`. |
| `crates/` | The implementation. `cargo nextest run --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` are green on every commit; `docs/` is updated in the same commit whenever the code diverges from a spec. |

## Try it

Nothing below needs an account, a server, or a network connection.

```bash
# Rust 1.95, pinned in rust-toolchain.toml.
cargo build --release

# 1. One-shot chat through your own gateway: routed by policy, metered per call.
#    Set ANTHROPIC_API_KEY, or point at anything OpenAI-compatible with
#    PANDAY_COMPAT_BASE_URL=http://127.0.0.1:8080
./target/release/panday chat "why is this test failing?"

# 2. Offline: a local model, a jailed workspace, nothing leaving the machine.
#    Needs an OpenAI-compatible server on loopback (llama-server, mistral.rs, …).
#    A remote base URL is refused rather than honoured.
./target/release/panday-local --workspace . "read src/lib.rs and explain it"

# 3. Your editor, over ACP. Point Zed / JetBrains / nvim at this command:
./target/release/panday acp --workspace .

# 4. Read any session back, exactly as the CLI showed it — including one that
#    crashed halfway. The log is the state (ADR-002).
./target/release/panday replay .panday/session.jsonl --costs
```

**Skills port unmodified.** Drop a directory containing a `SKILL.md`: frontmatter
keys other runtimes use (`allowed-tools`, `license`, nested `metadata`) are
ignored rather than rejected, and `references/` loads on demand
(`docs/16-plugins.md` §Skills).

**MCP servers mount as tools.** They run as child processes with a cleared
environment, appear as `mcp:{server}:{tool}`, and ask before every call until you
grant them per tool (`docs/16` §MCP host).

## Status

Phases 0 and 1 are complete and phase 2 is most of the way there;
`docs/23-roadmap.md` carries the honest sequencing, and every shipped milestone is
marked ✅ in its own spec together with what was learned building it — including
the bugs. What is not built says so: anything needing Postgres, Stripe, GPUs, KVM
or a running llama-server sits behind a trait with an `#[ignore]`d test rather
than a fake.

## How to use this repo

The docs are written to be **executable by agents**. Each component spec ends
with numbered milestones that carry acceptance criteria — each milestone is
sized to be one focused session for a coding agent (or a week of your own
evenings). Hand a spec + the workspace to an agent, point at a milestone, and
review the diff.

Read in this order:

1. `docs/00-vision.md` — what we are building and in what order
2. `docs/01-architecture.md` — the system, on one page of diagrams
3. `docs/04-decisions.md` — the ADRs; why each contested choice went the way it did
4. `docs/23-roadmap.md` — the honest sequencing
5. The component spec for whatever you are building this week

## The short pitch

One binary-per-service Rust platform where:

- Every model call goes through **your** gateway (`panday-gateway`) — cost
  attribution, caching, failover, and the router live there.
- The agent loop (`panday-harness`) is an event-sourced state machine — every
  session is an append-only log you can replay, resume, branch, and audit.
- Tool output passes through a **reducer** before it touches context — the
  token bill is a first-class engineering target, measured against real
  prompt-cache economics, not vibes.
- Untrusted execution is **tiered**: in-process pure tools → WASM plugins →
  namespaced processes → Firecracker microVMs.
- Skills are markdown, tools are MCP, editors connect over ACP — we adopt the
  open protocols and compete on the runtime.
- Subscriptions and the API are the same metering pipeline; offline mode is a
  first-class deployment target, not an afterthought.
- Model training starts with evals, proceeds by LoRA on rented GPUs, and every
  artifact exports to GGUF so the local tier benefits too.
