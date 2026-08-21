# 23 — Roadmap

Honest sequencing for a solo builder working with AI agents (which changes
the constant factor, not the ordering). Each phase has an **exit criterion**
— a falsifiable statement, not a feeling. Milestones referenced (M13.2 etc.)
live in the component specs; each is sized for one focused agent session to
one week of evenings.

**The one rule: do not start phase N+1 to avoid finishing phase N.**

**Where numbered work stands (2026-08-20):** 89 milestones, 83 shipped, 3 partial
(M22.3 host, M22.4 KVM timing, M22.5 air-gapped install), 3 not started (M19.3 /
M19.5 / M19.7 training). Laptop-provable leftovers are closed: subscription
OAuth, live M13.2, json-bench 200/200 on `xai/grok-4.6`, that model's catalog
row `provenance: measured`, M20.1 canaries in agent-bench (41 tasks). What
remains is named in each phase's status, not guessed.

## Phase 0 — Spine (~weeks 1–4)

The vocabulary and the door. `panday-types` events + model IR (✅ seeded in
this repo), workspace CI (M2.1–2.2), gateway with openai_compat + Anthropic
adapters streaming end to end (M11.1–11.2), SDK transport (M10.1–10.2),
router policy file v1 (M12.1), golden protocol fixtures (M3.1–3.2).

**Exit:** `panday-cli chat` streams through your gateway from two providers
and a local llama-server, with usage recorded per call. *(Yes — a chat CLI
before the harness. It forces the whole spine.)*

- **M0.1** **The spine: `panday chat` end to end.** ✅ *(shipped:
  `panday_gateway::Gateway` — adapter registry keyed by `ModelRef` provider
  prefix, router integration, `UsageSink`; `panday-cli` with the `chat`
  subcommand; `crates/panday-router/policy/dev.yaml`.)*

  This milestone exists because the phase's exit criterion was not covered by
  any numbered milestone. M2.1–2.2, M3.1–3.2, M10.1–10.2, M11.1–11.2 and
  M12.1 could all be green — and were — while the three binaries were still
  `println!` stubs and nothing was wired together. A future session reading
  only the milestone lists would have concluded Phase 0 was finished. Phases
  exit on their criteria, not on their checklists; where the two diverge, the
  criterion wins and gets a milestone of its own.

  Scope deliberately excluded, each already owned elsewhere: chain failover
  (M11.3), the Postgres ledger behind `UsageSink` (M11.4), HTTP ingress so the
  CLI can talk to a *remote* gateway (M11.5), accounts and entitlements
  (M17.x), and glob→model catalog resolution (M12.2). *All since shipped.*

## Phase 1 — The agent (~weeks 5–14)

The product wedge. Harness state machine on fake client (M13.1), native
tools + T2 sandbox Linux (M14.1–14.2), reducer generic + cargo/git/test
compressors (M15.1–15.2), real-model loop (M13.2), permissions + Ask flow
(M13.3), cache-aligned assembly + compaction (M13.4), crash-resume (M13.5),
event store PG + WS resume (M3.3), macOS T2 (M14.3).

**Exit:** the agent fixes a real failing test in one of *your* repos,
unattended, under `dev` profile — and you reach for it by preference the
next day. Dogfood begins; everything after this is built *with* it.

**Status: milestones complete; the exit is half-met.** All eleven Phase 1
milestones are shipped (M13.1–13.5, M14.1–14.3, M15.1–15.2, M3.3).

The mechanical half of the exit *is* demonstrated: the agent repairs a
genuinely broken crate — through the real native tools, inside a real T2 jail,
**unattended under `dev`** — and the test verifies it by running `cargo test`
on the repo afterwards, checking the fix landed in the implementation and the
test was not deleted (`crates/panday-harness/tests/fix_a_failing_test.rs`).

What is *not* met, and cannot be met by this repo alone:

- the model in the default suite is **scripted**, not live. The script chooses
  the plan; every tool call, edit, jail and test run beneath it is real. The
  live `#[ignore]`d leg ran on 2026-08-20 with Grok CLI OAuth (`xai/grok-4.6`
  against `api.x.ai`, no Anthropic key):
  `a_live_model_fixes_it_unattended` ok, and `panday chat -m xai/grok-4.6`
  replied `pong`. It stays ignored in CI because CI has no subscription.
- "in one of *your* repos" and "you reach for it by preference the next day"
  are judgements only the builder can make. They are the dogfood clause, and
  they are the point of the phase.

Do not treat Phase 1 as exited until both hold.

## Phase 2 — Extension & polish (~weeks 15–22)

Skills + plugin manifests (M16.1–16.2), MCP host with Ask-gating (M16.3),
ACP bridge → Zed/JetBrains (M16.5), subagents + parallel tools (M13.6),
`panday replay` (M21.3), `panday local` v1 with model supervisor
(M18.1–18.3), reduce-then-solve eval + dollar accounting (M15.4–15.5),
observability spine (M21.1–21.2).

**Exit:** a stranger installs the CLI, connects their editor via ACP, ports
an existing SKILL.md unmodified, and completes a task offline on a laptop.

**Where that stands (M0.2).** Like Phase 0's exit, this criterion has work in it
that no component milestone owns — so it is written down here rather than left
invisible:

| Clause | State |
|---|---|
| installs the CLI | Binaries build for four targets in `release.yml`; SBOM + the `sign` job shipped (M20.5). The README's "Try it" is the stranger's path. No package-manager recipe yet. |
| connects their editor via ACP | `panday acp` (M16.5), verified against the official crate's own client over a real ACP conversation. **Zed itself is unverified** — CI cannot run an editor, so that is one manual check by whoever has it installed. |
| ports an existing SKILL.md unmodified | Covered by a fixture in the published shape (`allowed-tools`, `license`, nested `metadata`, `references/`) that loads with no edits (M16.1, test at `crates/panday-harness/tests/skills_in_context.rs`). |
| completes a task offline on a laptop | `panday local` (M18.1) with the SQLite store (M18.3), against an OpenAI-compatible server on loopback. The suite uses a fake one because CI has no GGUF; the llama-server leg is `#[ignore]`d and runnable by anyone with one. |

So three clauses hold as far as CI can hold them, and two things remain that only
a human can do: run it in Zed, and run it against a real local model.

## Phase 3 — Money (~weeks 23–32)

Platform service: accounts/keys/entitlements (M17.1–17.3), ledger from
gateway+sandbox with property-tested reconciliation (M17.2, M3.5), Stripe
test-mode → live (M17.4–17.5), OpenAI-compat ingress as the API product
(M11.5; Anthropic Messages and Gemini generateContent join it), deploy shape 2 with status page (M22.2–22.3), abuse guardrails
(M20.4), minimal web dashboard (usage, keys, billing).

**Exit:** a stranger pays; the month's Stripe invoices reconcile with the
ledger to the cent; killing a provider mid-day degrades sessions to fallback
pools without a support ticket.

**Status: everything that does not need a third party is shipped.** Accounts, keys and rate
limiting (M17.3), the ledger with property-tested reconciliation (M17.2, M3.5), the OpenAI /
Anthropic / Gemini ingress as the product (M11.5), the webhook inbox and meter export (M17.4–17.5), abuse guardrails
and the admin surface (M17.7, M20.4), the dev shape and the service binary that is the composition
root (M22.1).

Three clauses of the exit criterion cannot be closed from here, and each names what it needs:

- **"a stranger pays"** needs a Stripe account. The projection is built and tested against a
  trait — what is missing is the signature check and the HTTP client, and a live key to exercise
  them.
- **"invoices reconcile to the cent"** is testable in both directions today: `check_invoice`
  compares what we reported against the ledger, and `panday-platform drift` compares recorded COGS
  against a provider's usage report (M21.4). Neither has been run against a real invoice, because
  there is no real invoice.
- **deploy shape 2 with a status page (M22.2–22.3)** needs somewhere to deploy. The image, the
  migrations-on-boot and the compose file exist; nothing has been pointed at a host.

## Phase 4 — Scale surfaces (~weeks 33–44)

T3 Firecracker pool + snapshots (M14.5–14.6, M22.4), WASM plugin tools/hooks
(M16.4), registry + `plugin install` (M16.6), router scorecards + generated
policy PRs (M12.4), eval spine v1 (M19.1–19.2), enterprise entitlement
tokens + air-gap kit (M17.6, M18.7), SOC2-shaped controls (M20 all).

**Exit:** untrusted user code runs in your cloud with the escape suite green
in CI; one enterprise pilot installs the air-gap kit from its README alone.

**Status: T0/T1/T2 are shipped with their escape suites; T3 is not, and cannot be from here.**
Firecracker needs KVM, and this tree is developed on macOS — M14.5, M14.6 and M22.4 are blocked on
hardware rather than on design. Everything else in the phase is done: WASM plugin tools and hooks
(M16.4), the registry and `plugin install` (M16.6), scorecards and generated policy PRs (M12.4),
the eval spine (M19.1), entitlement tokens (M17.6), the air-gap kit (M18.7), and the M20 controls
including the injection canaries (M20.1) and drill #1 (M20.4). The kit installs and runs from a
clean prefix here; what M22.5 asks for is an *air-gapped machine*, and the machine is the missing
part.

## Phase 5 — Own models (~weeks 45–56, overlaps 4)

Strictly the 19 ladder: eval suites (M19.1) → **Model 1** router classifier
shipped ($20–100) (M19.3) → transcript mining with consent (M19.4) →
**Model 2** summarizer in the reducer + local catalog ($150–400) (M19.5) →
agent-bench-as-RL-environment (M19.6) → **Model 3** coding specialist SFT,
then a go/no-go on the GRPO spend ($1–5k) (M19.7).

**Exit (the vision's bar):** a model you trained handles ≥30% of routed
traffic at equal-or-better evals and lower cost than the pool it displaced —
measured by the router's counterfactual logs, not enthusiasm.

**Status: the infrastructure ahead of every model is shipped; the models are not.** The eval spine
with its scorecard artifact and json-bench's 200 fixtures (M19.1 — `xai/grok-4.6` measured 200/200
on 2026-08-20; local GGUFs still unmeasured), the capability-profile generator
(M19.2 — `xai/grok-4.6` measured 2026-08-20; local GGUF rows stay `declared`), the shadow-mode harness that
compares a candidate classifier without letting it route (M12.5), the consent-first mining pipeline
(M19.4 — no transcripts to mine), agent-bench as a GRPO environment (M19.6 — 41 tasks in T2, not 50
in T3), and the signed model catalog that a tuned GGUF would enter through (M18.2). What is left is
the training itself: M19.3, M19.5 and M19.7. None of them can be faked from a laptop with no corpus
and no GPU. Doing so would produce numbers rather than evidence, which is the exact failure docs/19
opens by warning about.

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
