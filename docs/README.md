# Panday — a Rust AI platform, built from first principles

> **Panday** — Tagalog for *blacksmith*: the one who works iron into tools.
> Rust is what iron does on its own; a panday is what someone does with it
> deliberately. Every crate is prefixed `panday-`.

This repository is the blueprint and the seed of a full AI infrastructure
platform: an agent harness, an LLM gateway with a model router, a tiered
sandbox, a plugin system (skills + MCP + ACP), a token-economy layer, a
subscription platform with metered billing, an offline/local tier, and a
path to training your own task models.

**Two things live here:**

| Path | What it is |
|---|---|
| `docs/` | The documentation set — 21 specs, ADRs, threat model, roadmap. The source of truth. Renders with `mdbook serve docs`. |
| `crates/` | A compiling cargo workspace seeded with the core types and traits the specs define. `cargo check` is green. |

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
