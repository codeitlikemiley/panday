# 04 — Decisions (ADRs)

Short-form architecture decision records. Format: context → decision →
consequences. Status is **Accepted** unless noted. Revisit triggers are named
so these stay decisions, not dogma.

---

## ADR-001 — Rust end to end; Python only inside training jobs

**Context.** The platform is a long-lived, latency-sensitive, multi-tenant
system; the builder is Rust-native. But LLM *training* in Rust (burn,
candle-lora) is confirmed non-viable for production LoRA in 2026.

**Decision.** All services, clients, and tooling are Rust. The `training/`
directory is a self-contained Python (uv) project whose jobs run in
containers, orchestrated by Rust, producing artifacts (safetensors→GGUF) that
Rust serves. No Python in any serving path.

**Consequences.** Two toolchains, but with a hard boundary and one direction
of data flow. Revisit if burn grows a real LLM fine-tuning ecosystem (check
yearly, expect 2027+).

---

## ADR-002 — The session is an append-only event log (event sourcing)

**Context.** We need streaming UX, resume across devices, audit, replay
debugging, billing reconciliation, and training-data mining — five features
that are all views over "what happened, in order."

**Decision.** AEP events (03) are the source of truth. State is a fold.
Deltas are ephemeral; everything else is durable. Artifacts spill to object
storage by content hash.

**Consequences.** Slightly more ceremony per feature (define the event first).
In exchange: resume/fork/audit are free, and the ledger is provable from the
log. The classic event-sourcing trap (schema churn) is mitigated by the
versioning rules in 03.

---

## ADR-003 — One Postgres for OLTP, queues, and vectors (v1)

**Context.** Solo-team platform. Every extra stateful system is an on-call
burden. Postgres does LISTEN/NOTIFY + `FOR UPDATE SKIP LOCKED` queues,
pgvector 0.8.x similarity, JSONB, and boring OLTP.

**Decision.** Postgres 16+ is the only database. Queues are tables. Vectors
are pgvector (upgrade path: VectorChord/pgvectorscale, both drop-in-adjacent).
Object storage (S3/MinIO) for blobs. No Redis until a measured need (the
gateway's exact-match cache starts as a PG unlogged table).

**Consequences.** Simpler ops, one backup story. Ceilings exist — written
down in 22 with the trigger metrics (queue >1k msg/s sustained → NATS;
analytics queries hurting OLTP → ClickHouse; vector set > RAM → VectorChord).

---

## ADR-004 — Tiered sandboxing, not one mechanism

**Context.** "Sandbox" spans four different trust problems: our own pure
functions, third-party plugins, the user's own shell commands on their
machine, and *strangers' code on our cloud*.

**Decision.** Four tiers (14): T0 in-process Rust; T1 wasmtime components
(WASI 0.3, deny-by-default imports) for plugins; T2 OS-namespace jail
(bubblewrap/seccomp on Linux, Seatbelt on macOS — the mechanism Anthropic's
sandbox-runtime validated, reimplemented in Rust) for local shell; T3
Firecracker microVMs for cloud multi-tenant, driven over its socket API
directly (the community Rust SDKs are too thin to depend on).

**Consequences.** Four integration surfaces behind one `Sandbox` trait.
Firecracker is Linux/KVM-only — cloud pools are Linux; macOS cloud sandboxing
is out of scope (local T2 covers Macs).

---

## ADR-005 — Adopt MCP, ACP, SKILL.md, and OpenAI-compat; own AEP

**Context.** 2026 settled the protocol wars: MCP for tools (official rmcp,
v3.x), ACP for editor⇄agent (13 editors), SKILL.md for skills, OpenAI-compat
for model servers. Inventing parallel protocols buys nothing and costs the
ecosystem.

**Decision.** Conform at every boundary; innovate only in AEP (our internal
event protocol, which none of the above covers) and map AEP⇄ACP at the edge.

**Consequences.** Free ecosystem: every MCP server is a tool, every ACP
editor is a client, existing skills port. Constraint: protocol changes
upstream land on their schedule, not ours — pin versions, test upgrades.

---

## ADR-006 — Build the gateway; study, don't adopt, the existing ones

**Context.** Candidates: LiteLLM (Python — wrong runtime), Plano (Envoy-based,
active), Helicone ai-gateway (Rust, beta), Traceloop Hub (Rust, small),
TensorZero (archived Jun 2026 — cautionary). Our gateway must integrate the
router, the credit ledger, entitlements, and per-tenant budget stops — the
commercial core.

**Decision.** Build `ferrum-gateway` (11). Provider adapters are a small,
well-understood surface (~4 dialects); the value is in what wraps them, which
is precisely what we can't outsource. Crib test suites and edge-case handling
from the open Rust gateways.

**Consequences.** We own streaming quirks, provider flakiness, and dialect
drift forever. Mitigation: conformance fixtures per provider, recorded-replay
tests, and the OpenAI-compat ingress so we can A/B our gateway against any
other by flipping a base URL.

---

## ADR-007 — Reducer sits at the harness boundary and is cache-aware

**Context.** rtk proved tool-output compression works (60-90% on bash
output) — and JetBrains' independent benchmark proved the trap: compressing
only Bash while Read/Grep dominate, ignoring cache-read pricing (~0.1x),
yields ~0% net savings.

**Decision.** The reducer (15) intercepts **every** tool result before it
enters context, not just shell output. Its accounting model prices marginal
tokens at *actual* rates: fresh input 1x, cache reads ~0.1x, and cache-write
surcharges where they exist (Anthropic: ~1.25x at 5m TTL, ~2x at 1h TTL;
OpenAI-style automatic caching has no write premium) — and it never mutates
content that is part of a stable cached prefix (churn costs money).

**Consequences.** Savings claims are denominated in dollars, not raw tokens.
Compression strategies must carry information-retention tests, because a
reducer that eats the failing test's name is negative-value at any ratio.

---

## ADR-008 — Prompt-cache-aligned context layout is a hard invariant

**Context.** Both major providers price cache reads at ~0.1x, with
prefix-based caching (Anthropic: explicit breakpoints, write surcharge of
1.25x at 5m TTL / 2x at 1h TTL; OpenAI: automatic ≥1024-token prefixes, no
write surcharge). Layout churn silently ~10x's effective input cost on long
sessions.

**Decision.** Context assembly (13) is strictly stable→volatile: system +
tool schemas + skills index (stable, breakpointed) → memory/summaries
(slow-moving) → transcript window → current turn (volatile). Any feature that
would inject content into the stable region mid-session is rejected at design
time. Cache hit-rate per session is a first-class metric.

**Consequences.** Some UX ideas (e.g., dynamically reordering tools per turn)
are banned or must live in the volatile tail. Worth it: this single invariant
is the difference between 1x and ~3-6x effective input pricing on long agent
sessions.

---

## ADR-009 — Postgres-backed ledger is the billing source of truth; Stripe is a projection

**Context.** Stripe Billing Meters (v1 GA) aggregate asynchronously; the v2
credit system is preview-only. Real-time budget stops can't wait on Stripe,
and disputes need provable numbers.

**Decision.** An append-only `ledger_entries` table (17), written by gateway
and sandbox in the request path (fail-open rules defined per plan tier).
Entitlement checks read materialized balances. Stripe receives aggregated
meter events and handles money movement; it never defines truth.

**Consequences.** We own idempotency, backfill, and reconciliation jobs.
In exchange, offline/on-prem (no Stripe at all) and prepaid credits work on
the same rails.

---

## ADR-010 — Providers + local GGUF fallback for v1; no owned GPUs until phase 5

**Context.** User decision. Commercial APIs win on capability-per-ops-hour;
the local tier (llama-server / mistral.rs) covers offline, privacy, and
degraded mode. Owned/rented GPU serving becomes worthwhile only when tuned
models displace enough provider spend.

**Decision.** Router targets: provider models (primary), local GGUF
(offline/fallback/cheap-tier), tuned adapters via vLLM (phase 5+).

**Consequences.** Margin depends on routing discipline rather than
infrastructure arbitrage at first. The gateway abstraction keeps the phase-5
switch a config change.

---

## ADR-011 — Training ladder: evals → distilled SFT/QLoRA → GRPO; weights must export

**Context.** Deep-dive research (19): closed-provider fine-tuning is
disappearing (OpenAI winding down, Anthropic none, Mistral deprecated) and
never yields weights. Open-model tuning via Unsloth (self-managed), HF
Jobs/Together (managed with export), Tinker (managed RL with checkpoint
download) all satisfy the GGUF requirement.

**Decision.** Every model effort starts with an eval; first artifacts are a
ModernBERT-class router classifier and a 2-4B summarizer via QLoRA; GRPO
(ART/prime-rl or Tinker) only after SFT plateaus on a checkable reward. Any
path that can't hand us weights is rejected regardless of convenience.

**Consequences.** Slightly slower first result than clicking a provider's
tune button; in exchange the offline tier inherits every model we ever train.

---

## ADR-012 — CLI is the first client and ships as an ACP server too

**Context.** Building N clients is the classic startup death. ACP v1 is
stable with an official Rust crate and 13 editors on the client side.

**Decision.** `ferrum-cli` is a ratatui TUI *and* speaks ACP over stdio.
One codebase, `ferrum acp` subcommand, every ACP editor becomes a surface.

**Consequences.** Editor UX is bounded by what ACP models (fine for phase 1-2).
The web client waits until the platform phase, as decided.
