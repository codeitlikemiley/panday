# 23 — Roadmap

Honest sequencing for a solo builder working with AI agents (which changes
the constant factor, not the ordering). Each phase has an **exit criterion**
— a falsifiable statement, not a feeling. Milestones referenced (M13.2 etc.)
live in the component specs; each is sized for one focused agent session to
one week of evenings.

**The one rule: do not start phase N+1 to avoid finishing phase N.**

## Phase 0 — Spine (~weeks 1–4)

The vocabulary and the door. `panday-types` events + model IR (✅ seeded in
this repo), workspace CI (M2.1–2.2), gateway with openai_compat + Anthropic
adapters streaming end to end (M11.1–11.2), SDK transport (M10.1–10.2),
router policy file v1 (M12.1), golden protocol fixtures (M3.1–3.2).

**Exit:** `panday-cli chat` streams through your gateway from two providers
and a local llama-server, with usage recorded per call. *(Yes — a chat CLI
before the harness. It forces the whole spine.)*

## Phase 1 — The agent (~weeks 5–14)

The product wedge. Harness state machine on fake client (M13.1), native
tools + T2 sandbox Linux (M14.1–14.2), reducer generic + cargo/git/test
compressors (M15.1–15.2), real-model loop (M13.2), permissions + Ask flow
(M13.3), cache-aligned assembly + compaction (M13.4), crash-resume (M13.5),
event store PG + WS resume (M3.3), macOS T2 (M14.3).

**Exit:** the agent fixes a real failing test in one of *your* repos,
unattended, under `dev` profile — and you reach for it by preference the
next day. Dogfood begins; everything after this is built *with* it.

## Phase 2 — Extension & polish (~weeks 15–22)

Skills + plugin manifests (M16.1–16.2), MCP host with Ask-gating (M16.3),
ACP bridge → Zed/JetBrains (M16.5), subagents + parallel tools (M13.6),
`panday replay` (M21.3), `panday local` v1 with model supervisor
(M18.1–18.3), reduce-then-solve eval + dollar accounting (M15.4–15.5),
observability spine (M21.1–21.2).

**Exit:** a stranger installs the CLI, connects their editor via ACP, ports
an existing SKILL.md unmodified, and completes a task offline on a laptop.

## Phase 3 — Money (~weeks 23–32)

Platform service: accounts/keys/entitlements (M17.1–17.3), ledger from
gateway+sandbox with property-tested reconciliation (M17.2, M3.5), Stripe
test-mode → live (M17.4–17.5), OpenAI-compat ingress as the API product
(M11.5), deploy shape 2 with status page (M22.2–22.3), abuse guardrails
(M20.4), minimal web dashboard (usage, keys, billing).

**Exit:** a stranger pays; the month's Stripe invoices reconcile with the
ledger to the cent; killing a provider mid-day degrades sessions to fallback
pools without a support ticket.

## Phase 4 — Scale surfaces (~weeks 33–44)

T3 Firecracker pool + snapshots (M14.5–14.6, M22.4), WASM plugin tools/hooks
(M16.4), registry + `plugin install` (M16.6), router scorecards + generated
policy PRs (M12.4), eval spine v1 (M19.1–19.2), enterprise entitlement
tokens + air-gap kit (M17.6, M18.7), SOC2-shaped controls (M20 all).

**Exit:** untrusted user code runs in your cloud with the escape suite green
in CI; one enterprise pilot installs the air-gap kit from its README alone.

## Phase 5 — Own models (~weeks 45–56, overlaps 4)

Strictly the 19 ladder: eval suites (M19.1) → **Model 1** router classifier
shipped ($20–100) (M19.3) → transcript mining with consent (M19.4) →
**Model 2** summarizer in the reducer + local catalog ($150–400) (M19.5) →
agent-bench-as-RL-environment (M19.6) → **Model 3** coding specialist SFT,
then a go/no-go on the GRPO spend ($1–5k) (M19.7).

**Exit (the vision's bar):** a model you trained handles ≥30% of routed
traffic at equal-or-better evals and lower cost than the pool it displaced —
measured by the router's counterfactual logs, not enthusiasm.

## Phase 6 — Consumer & marketplace (when 3–5 say so)

Web/desktop chat on the public API (built like a third party — if the API
can't support your own frontend, it isn't ready), marketplace storefront over
the registry, learned routing proposals (M12.5 → bandit), speculative-decode
drafts for the local tier.

## Standing weekly rhythm (from phase 1)

Dogfood daily · triage the reducer's worst savings-vs-retention case ·
review router counterfactuals · check ledger drift (must be ~0) · one
escape-suite/chaos case added · docs updated with reality (a spec that
diverges from code is a bug in the spec).

## What kills projects like this (pre-mortem)

1. **Building phase 4–6 infrastructure with phase-1 users (n=1).** The exits
   exist to stop this.
2. **The gateway becoming the product.** TensorZero warning: the agent is the
   product; the gateway is its engine room.
3. **Training models before evals** — burns the exact money and morale that
   phase 5 needs. The ladder is the discipline.
4. **Protocol invention.** Every place we conform (MCP/ACP/SKILL.md/
   openai-compat) is distribution; every place we invent is maintenance.
   Invent only AEP.
5. **Scope creep in Rust purity.** Python trains models (ADR-001). Fighting
   this wastes the exact months that ship phases 3–5.
